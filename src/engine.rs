//! `Engine` — the lifecycle: `plan`, `apply`, `destroy`, `revert`,
//! `resume`. Store-agnostic; deterministic (sorted `ArtifactRef`);
//! write-ahead journaled; fail-closed on hash drift.
//!
//! Semantics (proposal P-001):
//! - **plan** diffs desired `Change`s + prior manifest against the
//!   store → create/update/delete/unchanged.
//! - **apply** journals each before-image *before* each mutation,
//!   writes, verifies the post-write hash, then stamps a manifest.
//!   Fail-closed on `before_hash` drift.
//! - **destroy** deletes exactly the manifest's recorded refs
//!   (owned artifacts only) — the inverse of apply for generators.
//! - **revert** restores journal before-images hash-checked — the
//!   inverse for non-owned artifacts (codemod).
//! - **resume** = apply with `opts.resume`: re-derive the plan and
//!   skip already-converged artifacts via hash check.

use crate::change::Change;
use crate::error::CodegenError;
use crate::journal::{JOURNAL_VERSION, Journal, JournalOp, JournalRecord, capture_before};
use crate::manifest::{Manifest, ManifestEntry, ref_safe};
use crate::plan::{Plan, PlanAction, compute_plan, normalize_changes};
use crate::store::ArtifactStore;
use crate::types::{ArtifactRef, ContentHash};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Provenance passed to `apply` — becomes the manifest.
#[derive(Clone, Debug, Default)]
pub struct RunMeta {
    /// Generator/target name (manifest owner).
    pub target: String,
    /// Generator version.
    pub generator_version: String,
    /// Source hashes consumed by the run.
    pub source_hashes: BTreeMap<String, ContentHash>,
    /// Journal run id. Default: derived from target + a counter-free
    /// content hash of the desired set is *not* used — callers that
    /// want resumable runs pass a stable id; `new_run_id` makes a
    /// unique one.
    pub run_id: String,
}

impl RunMeta {
    pub fn new(target: impl Into<String>, generator_version: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            generator_version: generator_version.into(),
            source_hashes: BTreeMap::new(),
            run_id: String::new(),
        }
    }

    /// A unique run id: `<target>-<blake3-prefix>` over target +
    /// version + a monotonic-ish entropy source (time + pid).
    pub fn new_run_id(&mut self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!(
            "{}|{}|{}|{}",
            self.target,
            self.generator_version,
            nanos,
            std::process::id()
        );
        let h = ContentHash::of(seed.as_bytes()).to_hex();
        self.run_id = format!("{}-{}", ref_safe(&self.target), &h[..16]);
        self.run_id.clone()
    }
}

/// Per-artifact error policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    /// Stop at the first per-artifact failure (default).
    #[default]
    Fail,
    /// Record the failure and continue with remaining artifacts.
    Skip,
}

/// Apply/destroy/revert options.
#[derive(Clone, Debug)]
pub struct ApplyOptions {
    /// Re-derive-and-skip: artifacts already converged (store hash ==
    /// desired, or absent for deletes) are skipped via hash check.
    pub resume: bool,
    /// Continue past per-artifact failures, collecting diagnostics.
    pub keep_going: bool,
    /// Error policy — `Skip` implies keep-going for that artifact.
    pub on_error: OnError,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            resume: false,
            keep_going: false,
            on_error: OnError::Fail,
        }
    }
}

impl ApplyOptions {
    /// `--keep-going` CLI shape.
    pub fn keep_going() -> Self {
        Self {
            keep_going: true,
            ..Self::default()
        }
    }

    /// `--resume` shape.
    pub fn resume() -> Self {
        Self {
            resume: true,
            ..Self::default()
        }
    }

    fn continue_on_error(&self) -> bool {
        self.keep_going || self.on_error == OnError::Skip
    }
}

/// Outcome of an apply/destroy run.
#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    pub run_id: String,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped: usize,
    /// Artifacts that failed, with the error text.
    pub failures: Vec<(ArtifactRef, String)>,
    /// Stopped early on first failure (`!keep_going`).
    pub aborted: bool,
    /// The error that aborted the run, if any.
    pub fatal: Option<String>,
    /// Manifest artifact written at the end (only when zero failures).
    pub manifest_written: bool,
}

impl ApplyReport {
    fn record_ok(&mut self, action: PlanAction) {
        match action {
            PlanAction::Create => self.created += 1,
            PlanAction::Update => self.updated += 1,
            PlanAction::Delete => self.deleted += 1,
            PlanAction::Unchanged => self.unchanged += 1,
        }
    }

    /// True when every planned mutation converged.
    pub fn clean(&self) -> bool {
        self.failures.is_empty() && !self.aborted
    }
}

/// Outcome of a revert run.
#[derive(Clone, Debug, Default)]
pub struct RevertReport {
    pub run_id: String,
    pub restored: usize,
    pub removed: usize,
    pub skipped: usize,
    pub failures: Vec<(ArtifactRef, String)>,
    pub aborted: bool,
    pub fatal: Option<String>,
}

/// The lifecycle engine over any `ArtifactStore`.
pub struct Engine<'s, S: ArtifactStore + ?Sized> {
    store: &'s S,
}

impl<'s, S: ArtifactStore + ?Sized> Engine<'s, S> {
    pub fn new(store: &'s S) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &'s S {
        self.store
    }

    /// Diff desired state (+ prior manifest for deletes) against the
    /// store. Fail-closed on `Change.before_hash` drift.
    pub fn plan(&self, desired: &[Change], prior: Option<&Manifest>) -> Result<Plan, CodegenError> {
        compute_plan(self.store, desired, prior)
    }

    /// Converge the store to `desired`. Journals before-images,
    /// mutates in sorted order, verifies post-write hashes, stamps the
    /// manifest (only on a clean run).
    ///
    /// `prior` supplies manifest-owned deletes; `meta` supplies
    /// manifest provenance. `opts.resume` skips converged artifacts.
    pub fn apply(
        &self,
        desired: &[Change],
        prior: Option<&Manifest>,
        meta: &RunMeta,
        opts: &ApplyOptions,
    ) -> Result<ApplyReport, CodegenError> {
        let desired = normalize_changes(desired)?;
        let plan = compute_plan(self.store, &desired, prior)?;
        let run_id = if meta.run_id.is_empty() {
            meta.target.clone()
        } else {
            meta.run_id.clone()
        };
        let journal = Journal::open(self.store, &run_id);
        let mut report = ApplyReport {
            run_id: run_id.clone(),
            ..ApplyReport::default()
        };
        let desired_by_ref: BTreeMap<&ArtifactRef, &Change> =
            desired.iter().map(|c| (&c.artifact, c)).collect();

        let mut seq = journal.next_seq()?;
        for entry in &plan.entries {
            if entry.action == PlanAction::Unchanged {
                report.unchanged += 1;
                continue;
            }
            // Resume: skip already-converged artifacts via hash check.
            if opts.resume && self.converged(entry)? {
                report.skipped += 1;
                continue;
            }
            // Fail-closed drift check: store must still match the
            // hash observed at plan time (or be converged already).
            if let Err(e) = self.check_drift(entry) {
                if self.fail_or_continue(&mut report, opts, entry.artifact.clone(), e)? {
                    continue;
                } else {
                    return Ok(report);
                }
            }
            let result = match entry.action {
                PlanAction::Delete => self.journaled_delete(&journal, &mut seq, entry),
                _ => {
                    let change = desired_by_ref
                        .get(&entry.artifact)
                        .expect("plan entry from desired set");
                    self.journaled_write(&journal, &mut seq, entry, change)
                }
            };
            match result {
                Ok(()) => report.record_ok(entry.action),
                Err(e) => {
                    if self.fail_or_continue(&mut report, opts, entry.artifact.clone(), e)? {
                        continue;
                    } else {
                        return Ok(report);
                    }
                }
            }
        }

        // Stamp the manifest — only when the run converged cleanly;
        // a partial apply must not claim ownership of unwritten
        // artifacts. The manifest write is itself journaled so
        // `revert` restores the previous manifest.
        if report.failures.is_empty() && !report.aborted {
            let manifest = Manifest {
                target: meta.target.clone(),
                generator_version: meta.generator_version.clone(),
                source_hashes: meta.source_hashes.clone(),
                artifacts: desired
                    .iter()
                    .map(|c| ManifestEntry {
                        artifact: c.artifact.clone(),
                        hash: c.after_hash(),
                    })
                    .collect(),
                run_id: run_id.clone(),
            };
            let bytes = serde_json::to_vec_pretty(&manifest)
                .map_err(|e| CodegenError::Manifest(format!("serialize: {e}")))?;
            let mref = Manifest::artifact_ref(&meta.target);
            // Journal the manifest write like any other mutation.
            let mentry = crate::plan::PlanEntry {
                artifact: mref.clone(),
                action: if self.store.hash(&mref)?.is_some() {
                    PlanAction::Update
                } else {
                    PlanAction::Create
                },
                before_hash: self.store.hash(&mref)?,
                after_hash: Some(ContentHash::of(&bytes)),
            };
            self.journaled_write_bytes(&journal, &mut seq, &mentry, &bytes)?;
            report.manifest_written = true;
        }
        Ok(report)
    }

    /// Delete exactly the artifacts the manifest records (plus the
    /// manifest itself). Never touches unrecorded artifacts. No drift
    /// check — the manifest is the authority on what the tool owns.
    pub fn destroy(
        &self,
        manifest: &Manifest,
        opts: &ApplyOptions,
    ) -> Result<ApplyReport, CodegenError> {
        let run_id = if manifest.run_id.is_empty() {
            format!("destroy-{}", ref_safe(&manifest.target))
        } else {
            format!("destroy-{}", manifest.run_id)
        };
        let journal = Journal::open(self.store, &run_id);
        let mut report = ApplyReport {
            run_id: run_id.clone(),
            ..ApplyReport::default()
        };
        let mut seq = journal.next_seq()?;

        // Manifest-owned artifacts, sorted (manifest is stored sorted,
        // but re-sort defensively).
        let mut artifacts: Vec<&ManifestEntry> = manifest.artifacts.iter().collect();
        artifacts.sort_by(|a, b| a.artifact.cmp(&b.artifact));
        for me in artifacts {
            let cur = self.store.hash(&me.artifact)?;
            if opts.resume && cur.is_none() {
                report.skipped += 1;
                continue;
            }
            let entry = crate::plan::PlanEntry {
                artifact: me.artifact.clone(),
                action: PlanAction::Delete,
                before_hash: cur,
                after_hash: None,
            };
            match self.journaled_delete(&journal, &mut seq, &entry) {
                Ok(()) => report.deleted += 1,
                Err(e) => {
                    if self.fail_or_continue(&mut report, opts, me.artifact.clone(), e)? {
                        continue;
                    } else {
                        return Ok(report);
                    }
                }
            }
        }

        // Remove the manifest artifact itself (journaled, so revert
        // can restore it).
        let mref = Manifest::artifact_ref(&manifest.target);
        let cur = self.store.hash(&mref)?;
        let mentry = crate::plan::PlanEntry {
            artifact: mref.clone(),
            action: PlanAction::Delete,
            before_hash: cur,
            after_hash: None,
        };
        match self.journaled_delete(&journal, &mut seq, &mentry) {
            Ok(()) => report.deleted += 1,
            Err(e) => {
                self.fail_or_continue(&mut report, opts, mref.clone(), e)?;
            }
        }
        Ok(report)
    }

    /// Restore before-images from a run's journal, in reverse order,
    /// hash-checked: an artifact whose current hash no longer matches
    /// the journaled after-hash has drifted since apply and is
    /// refused (per `opts` policy).
    pub fn revert(&self, run_id: &str, opts: &ApplyOptions) -> Result<RevertReport, CodegenError> {
        let journal = Journal::open(self.store, run_id);
        // Strict: a corrupt journal refuses rather than silently
        // skipping records during undo.
        let records = journal.records_strict()?;
        let mut report = RevertReport {
            run_id: run_id.to_string(),
            ..RevertReport::default()
        };
        let mut seq = journal.next_seq()?;

        for rec in records.iter().rev() {
            let current = self.store.hash(&rec.artifact)?;
            // Hash-check: current must equal the journaled after-state.
            let expected_after = match rec.op {
                JournalOp::Write => rec.after_hash,
                JournalOp::Delete => None,
            };
            if current != expected_after {
                let e = CodegenError::drift(rec.artifact.clone(), expected_after, current);
                if self.fail_or_continue_rev(&mut report, opts, rec.artifact.clone(), e)? {
                    continue;
                } else {
                    return Ok(report);
                }
            }
            // Write-ahead journal the revert mutation itself, so a
            // crashed revert is recoverable: before = the state we're
            // undoing (== expected_after, just verified), after = the
            // before-image we're restoring.
            let (rb_before_hash, rb_before_blob) =
                capture_before(self.store, &journal, &rec.artifact)?;
            let (revert_op, revert_after) = match rec.before_hash {
                Some(h) => (JournalOp::Write, Some(h)),
                None => (JournalOp::Delete, None),
            };
            journal.append(JournalRecord {
                v: JOURNAL_VERSION,
                run_id: journal.run_id().to_string(),
                seq,
                artifact: rec.artifact.clone(),
                op: revert_op,
                before_hash: rb_before_hash,
                before_blob: rb_before_blob,
                after_hash: revert_after,
            })?;
            seq += 1;
            // Restore the before-image.
            let result = match (&rec.before_hash, &rec.before_blob) {
                (Some(_), Some(blob)) => match self.store.read(blob)? {
                    Some(bytes) => self
                        .store
                        .write(&rec.artifact, &bytes)
                        .map_err(CodegenError::from),
                    None => Err(CodegenError::Journal(format!(
                        "before-image blob {blob} missing"
                    ))),
                },
                (None, None) => self.store.delete(&rec.artifact).map_err(CodegenError::from),
                _ => Err(CodegenError::Journal(format!(
                    "record {} has inconsistent before fields",
                    rec.artifact
                ))),
            };
            match result {
                Ok(()) => match rec.before_hash {
                    Some(_) => report.restored += 1,
                    None => report.removed += 1,
                },
                Err(e) => {
                    if self.fail_or_continue_rev(&mut report, opts, rec.artifact.clone(), e)? {
                        continue;
                    } else {
                        return Ok(report);
                    }
                }
            }
        }
        Ok(report)
    }

    // -- internals ----------------------------------------------------

    /// True when the store already reflects the entry's end state.
    fn converged(&self, entry: &crate::plan::PlanEntry) -> Result<bool, CodegenError> {
        let cur = self.store.hash(&entry.artifact)?;
        Ok(match entry.action {
            PlanAction::Delete => cur.is_none(),
            _ => cur.is_some() && cur == entry.after_hash,
        })
    }

    /// Fail-closed drift check for apply/destroy plan entries.
    fn check_drift(&self, entry: &crate::plan::PlanEntry) -> Result<(), CodegenError> {
        let cur = self.store.hash(&entry.artifact)?;
        if cur == entry.before_hash {
            return Ok(());
        }
        // Already converged end-state is acceptable (idempotent
        // re-apply); anything else is drift.
        let end_state = match entry.action {
            PlanAction::Delete => cur.is_none(),
            _ => cur.is_some() && cur == entry.after_hash,
        };
        if end_state {
            return Ok(());
        }
        Err(CodegenError::drift(
            entry.artifact.clone(),
            entry.before_hash,
            cur,
        ))
    }

    /// Journal write-ahead + store write + post-write hash verify.
    fn journaled_write(
        &self,
        journal: &Journal<'_, S>,
        seq: &mut u64,
        entry: &crate::plan::PlanEntry,
        change: &Change,
    ) -> Result<(), CodegenError> {
        self.journaled_write_bytes(journal, seq, entry, &change.content)
    }

    fn journaled_write_bytes(
        &self,
        journal: &Journal<'_, S>,
        seq: &mut u64,
        entry: &crate::plan::PlanEntry,
        content: &[u8],
    ) -> Result<(), CodegenError> {
        // 1. Capture before-image (journal blob) — BEFORE mutating.
        let (before_hash, before_blob) = capture_before(self.store, journal, &entry.artifact)?;
        // 2. Write-ahead journal record.
        let rec = JournalRecord {
            v: JOURNAL_VERSION,
            run_id: journal.run_id().to_string(),
            seq: *seq,
            artifact: entry.artifact.clone(),
            op: JournalOp::Write,
            before_hash,
            before_blob,
            after_hash: entry.after_hash,
        };
        journal.append(rec)?;
        *seq += 1;
        // 3. Mutate.
        self.store.write(&entry.artifact, content)?;
        // 4. Verify post-write hash.
        let now = self.store.hash(&entry.artifact)?;
        if now != entry.after_hash {
            return Err(CodegenError::drift(
                entry.artifact.clone(),
                entry.after_hash,
                now,
            ));
        }
        Ok(())
    }

    /// Journal write-ahead + store delete + verify absent.
    fn journaled_delete(
        &self,
        journal: &Journal<'_, S>,
        seq: &mut u64,
        entry: &crate::plan::PlanEntry,
    ) -> Result<(), CodegenError> {
        let (before_hash, before_blob) = capture_before(self.store, journal, &entry.artifact)?;
        let rec = JournalRecord {
            v: JOURNAL_VERSION,
            run_id: journal.run_id().to_string(),
            seq: *seq,
            artifact: entry.artifact.clone(),
            op: JournalOp::Delete,
            before_hash,
            before_blob,
            after_hash: None,
        };
        journal.append(rec)?;
        *seq += 1;
        self.store.delete(&entry.artifact)?;
        if self.store.hash(&entry.artifact)?.is_some() {
            return Err(CodegenError::Journal(format!(
                "delete of {} did not converge",
                entry.artifact
            )));
        }
        Ok(())
    }

    /// Apply policy: record failure; return `true` to continue,
    /// `false` to abort (report already carries the failure).
    fn fail_or_continue(
        &self,
        report: &mut ApplyReport,
        opts: &ApplyOptions,
        artifact: ArtifactRef,
        e: CodegenError,
    ) -> Result<bool, CodegenError> {
        let msg = e.to_string();
        report.failures.push((artifact.clone(), msg.clone()));
        if opts.continue_on_error() {
            Ok(true)
        } else {
            report.aborted = true;
            report.fatal = Some(msg);
            Ok(false)
        }
    }

    fn fail_or_continue_rev(
        &self,
        report: &mut RevertReport,
        opts: &ApplyOptions,
        artifact: ArtifactRef,
        e: CodegenError,
    ) -> Result<bool, CodegenError> {
        let msg = e.to_string();
        report.failures.push((artifact.clone(), msg.clone()));
        if opts.continue_on_error() {
            Ok(true)
        } else {
            report.aborted = true;
            report.fatal = Some(msg);
            Ok(false)
        }
    }
}
