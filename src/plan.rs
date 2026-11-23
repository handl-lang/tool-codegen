//! `Plan`/`PlanEntry`/`PlanAction` — the diff between desired state
//! (`Change` set + prior `Manifest`) and the store.

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

/// Normalize a desired `Change` set: sort by `ArtifactRef`, reject
/// reserved refs and duplicates. Returns the sorted vec.
pub fn normalize_changes(changes: &[Change]) -> Result<Vec<Change>, CodegenError> {
    let mut out: Vec<Change> = changes.to_vec();
    for c in &out {
        if c.artifact.is_reserved() {
            return Err(CodegenError::InvalidRef(format!(
                "{}: reserved kernel prefix {RESERVED_PREFIX}",
                c.artifact
            )));
        }
        c.artifact
            .validate_fs_relative()
            .map_err(|m| CodegenError::InvalidRef(format!("{}: {m}", c.artifact)))?;
    }
    out.sort_by(|a, b| a.artifact.cmp(&b.artifact));
    for w in out.windows(2) {
        if w[0].artifact == w[1].artifact {
            return Err(CodegenError::InvalidRef(format!(
                "duplicate artifact ref {}",
                w[0].artifact
            )));
        }
    }
    Ok(out)
}

/// Compute the plan: diff `desired` (normalized) against the store,
/// plus deletes for prior-manifest artifacts missing from desired.
///
/// Fail-closed: a `Change.before_hash` that disagrees with the store
/// is `CodegenError::Drift`.
pub fn compute_plan<S: ArtifactStore + ?Sized>(
    store: &S,
    desired: &[Change],
    prior: Option<&Manifest>,
) -> Result<Plan, CodegenError> {
    let desired = normalize_changes(desired)?;
    let desired_refs: std::collections::BTreeSet<&ArtifactRef> =
        desired.iter().map(|c| &c.artifact).collect();

    let mut entries = Vec::with_capacity(desired.len());
    for c in &desired {
        let cur = store.hash(&c.artifact)?;
        if let Some(claimed) = c.before_hash
            && cur != Some(claimed)
        {
            return Err(CodegenError::drift(c.artifact.clone(), Some(claimed), cur));
        }
        let after = c.after_hash();
        let action = match cur {
            None => PlanAction::Create,
            Some(h) if h == after => PlanAction::Unchanged,
            Some(_) => PlanAction::Update,
        };
        entries.push(PlanEntry {
            artifact: c.artifact.clone(),
            action,
            before_hash: cur,
            after_hash: Some(after),
        });
    }

    if let Some(m) = prior {
        for me in &m.artifacts {
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
