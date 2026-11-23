//! `Plan`/`PlanEntry`/`PlanAction` — the diff between desired state
//! (`Change` set + prior `Manifest`) and the store.
//!
//! # Performance notes
//!
//! `compute_plan` costs one `store.hash` per artifact — for `FsStore`
//! that is one `stat` plus (on a cold stat→hash memo) one streaming
//! BLAKE3 pass; unchanged artifacts across repeated `plan`/`apply`
//! cycles on the same store instance cost only the `stat`. BLAKE3 is
//! already the fast path — no cheaper pre-hash is warranted.
//!
//! A *plan-result* cache (keyed by manifest hash + store fingerprint)
//! is deliberately **deferred**: fingerprinting the store is itself a
//! per-artifact stat sweep, and with the `FsStore` memo the residual
//! plan cost is already ~one syscall per artifact. Revisit only if
//! profiling shows `plan` dominating after the memo is warm.
//! `Manifest::generator_version` is the documented key component for
//! when that day comes.

use crate::change::Change;
use crate::error::CodegenError;
use crate::manifest::Manifest;
use crate::store::ArtifactStore;
use crate::types::{ArtifactRef, ContentHash, RESERVED_PREFIX};
use serde::{Deserialize, Serialize};

/// What reconcile must do for one artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanAction {
    /// Artifact absent from store, present in desired.
    Create,
    /// Artifact present, content differs from desired.
    Update,
    /// Artifact recorded in the prior manifest but absent from desired.
    Delete,
    /// Store already matches desired — no mutation.
    Unchanged,
}

/// One planned mutation (or no-op) for one artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub artifact: ArtifactRef,
    pub action: PlanAction,
    /// Store hash observed at plan time (`None` = absent).
    pub before_hash: Option<ContentHash>,
    /// Desired content hash (`None` for deletes).
    pub after_hash: Option<ContentHash>,
}

/// The full diff. Entries are sorted by `ArtifactRef` — the
/// deterministic ordering the engine mutates in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    pub fn is_clean(&self) -> bool {
        self.entries
            .iter()
            .all(|e| e.action == PlanAction::Unchanged)
    }

    pub fn count(&self, action: PlanAction) -> usize {
        self.entries.iter().filter(|e| e.action == action).count()
    }

    pub fn creates(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.action == PlanAction::Create)
    }

    pub fn updates(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.action == PlanAction::Update)
    }

    pub fn deletes(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.action == PlanAction::Delete)
    }

    pub fn mutations(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.action != PlanAction::Unchanged)
    }
}

/// Sort `changes` by `ArtifactRef` without cloning content — the
/// internal fast path `compute_plan`/`Engine::apply` use so a plan
/// doesn't deep-copy every desired byte string.
pub(crate) fn sorted_changes(changes: &[Change]) -> Result<Vec<&Change>, CodegenError> {
    for c in changes {
        if c.artifact().is_reserved() {
            return Err(CodegenError::InvalidRef(format!(
                "{}: reserved kernel prefix {RESERVED_PREFIX}",
                c.artifact()
            )));
        }
        c.artifact()
            .validate_fs_relative()
            .map_err(|m| CodegenError::InvalidRef(format!("{}: {m}", c.artifact())))?;
    }
    let mut out: Vec<&Change> = changes.iter().collect();
    out.sort_by(|a, b| a.artifact().cmp(b.artifact()));
    for w in out.windows(2) {
        if w[0].artifact() == w[1].artifact() {
            return Err(CodegenError::InvalidRef(format!(
                "duplicate artifact ref {}",
                w[0].artifact()
            )));
        }
    }
    Ok(out)
}

/// Normalize a desired `Change` set: sort by `ArtifactRef`, reject
/// reserved refs and duplicates (a `Put` and `Delete` for the same
/// artifact is ambiguous → rejected). Returns the sorted vec.
///
/// Clones the set (content included) — public-API convenience.
/// In-crate callers use `sorted_changes` to avoid the copy.
pub fn normalize_changes(changes: &[Change]) -> Result<Vec<Change>, CodegenError> {
    Ok(sorted_changes(changes)?.into_iter().cloned().collect())
}

/// Compute the plan: diff `desired` (normalized) against the store,
/// plus deletes for prior-manifest artifacts missing from desired.
///
/// `Change::Delete` semantics: artifact present → `Delete` action;
/// artifact absent → `Unchanged` (a no-op, never an error). Deletes
/// produced either way are journaled with before-images at apply, so
/// `revert` restores them; they never become manifest entries (the
/// manifest records the desired *end state* — a deleted artifact
/// simply isn't one).
///
/// Fail-closed: a `Put.before_hash` that disagrees with the store
/// is `CodegenError::Drift`.
pub fn compute_plan<S: ArtifactStore + ?Sized>(
    store: &S,
    desired: &[Change],
    prior: Option<&Manifest>,
) -> Result<Plan, CodegenError> {
    let desired = sorted_changes(desired)?;
    compute_plan_sorted(store, &desired, prior)
}

/// `compute_plan` over an already-normalized (validated + sorted)
/// change set — `Engine::apply` uses this so the set is normalized
/// once per run instead of once per stage.
pub(crate) fn compute_plan_sorted<S: ArtifactStore + ?Sized>(
    store: &S,
    desired: &[&Change],
    prior: Option<&Manifest>,
) -> Result<Plan, CodegenError> {
    let desired_refs: std::collections::BTreeSet<&ArtifactRef> =
        desired.iter().map(|c| c.artifact()).collect();

    let mut entries = Vec::with_capacity(desired.len());
    for &c in desired {
        let artifact = c.artifact().clone();
        let cur = store.hash(&artifact)?;
        if let Some(claimed) = c.before_hash()
            && cur != Some(claimed)
        {
            return Err(CodegenError::drift(artifact, Some(claimed), cur));
        }
        let (action, after) = match c {
            Change::Put { content, .. } => {
                let after = ContentHash::of(content);
                let action = match cur {
                    None => PlanAction::Create,
                    Some(h) if h == after => PlanAction::Unchanged,
                    Some(_) => PlanAction::Update,
                };
                (action, Some(after))
            }
            Change::Delete { .. } => match cur {
                // Emitter-declared cleanup of an existing artifact.
                Some(_) => (PlanAction::Delete, None),
                // Already absent — no-op, not an error.
                None => (PlanAction::Unchanged, None),
            },
        };
        entries.push(PlanEntry {
            artifact,
            action,
            before_hash: cur,
            after_hash: after,
        });
    }

    if let Some(m) = prior {
        for me in &m.artifacts {
            // desired_refs covers both Put and Delete changes — an
            // emitter-declared Delete for a manifest artifact dedupes
            // here (the Change::Delete branch above already planned it).
            if desired_refs.contains(&me.artifact) {
                continue;
            }
            let cur = store.hash(&me.artifact)?;
            entries.push(PlanEntry {
                artifact: me.artifact.clone(),
                action: PlanAction::Delete,
                before_hash: cur,
                after_hash: None,
            });
        }
    }

    entries.sort_by(|a, b| a.artifact.cmp(&b.artifact));
    Ok(Plan { entries })
}
