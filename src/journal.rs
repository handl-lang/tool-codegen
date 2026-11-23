//! `Journal` — the append-only JSONL write-ahead record.
//!
//! Every mutation is journaled *before* it happens: the record
//! carries the artifact's before-image (content-addressed blob under
//! `.handl/codegen/blob/<hash>` — identical priors dedup for free)
//! and the after-hash. A crash mid-apply leaves a complete recovery
//! record; `revert` restores before-images hash-checked, `resume`
//! re-derives and skips already-applied work.
//!
//! Journal files are store artifacts
//! (`.handl/codegen/journal/<run_id>.jsonl`) so every store gets them
//! for free. Reads are lenient: a truncated final line (crash
//! mid-append) is skipped, complete records still load.

use crate::error::{CodegenError, StoreError};
use crate::manifest::ref_safe;
use crate::store::ArtifactStore;
use crate::types::{ArtifactRef, ContentHash, RESERVED_PREFIX};
use serde::{Deserialize, Serialize};

/// Journal format version.
pub const JOURNAL_VERSION: u32 = 1;

/// What the mutation did to the artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JournalOp {
    /// Create or update — `after_hash` is the desired content hash.
    Write,
    /// Delete — `after_hash` is `None` (artifact absent afterwards).
    Delete,
}

/// One write-ahead record. Appended before the mutation it describes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub v: u32,
    pub run_id: String,
    /// Monotonic sequence within the journal (0-based).
    pub seq: u64,
    pub artifact: ArtifactRef,
    pub op: JournalOp,
    /// Store hash before the mutation (`None` = artifact absent).
    pub before_hash: Option<ContentHash>,
    /// Store ref of the blob holding the before-image
    /// (`.handl/codegen/blob/<hex>`), present iff `before_hash` is.
    pub before_blob: Option<ArtifactRef>,
    /// Content hash after the mutation (`None` for deletes).
    pub after_hash: Option<ContentHash>,
}

/// The store ref for a run's journal file.
pub fn journal_ref(run_id: &str) -> ArtifactRef {
    ArtifactRef::new(format!(
        "{RESERVED_PREFIX}journal/{}.jsonl",
        ref_safe(run_id)
    ))
}

/// The store ref for a content-addressed before-image blob.
pub fn blob_ref(hash: &ContentHash) -> ArtifactRef {
    ArtifactRef::new(format!("{RESERVED_PREFIX}blob/{}", hash.to_hex()))
}

/// Handle on one run's journal within a store.
pub struct Journal<'a, S: ArtifactStore + ?Sized> {
    store: &'a S,
    run_id: String,
    artifact: ArtifactRef,
}

impl<'a, S: ArtifactStore + ?Sized> Journal<'a, S> {
    pub fn open(store: &'a S, run_id: &str) -> Self {
        Self {
            store,
            run_id: run_id.to_string(),
            artifact: journal_ref(run_id),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }

    /// All complete records, in append order. A truncated/corrupt
    /// trailing line (crash mid-append) is skipped; corrupt lines in
    /// the middle are also skipped — the journal is best-effort audit,
    /// the manifest + store state are the reconciliation truth.
    pub fn records(&self) -> Result<Vec<JournalRecord>, CodegenError> {
        let Some(bytes) = self.store.read(&self.artifact)? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(line) {
                Ok(rec) => out.push(rec),
                Err(_) => continue, // truncated tail or corrupt line
            }
        }
        Ok(out)
    }

    /// Strict read — any unparseable non-empty line is a journal error.
    /// Used by `revert` so a corrupt journal refuses rather than
    /// silently skipping records.
    pub fn records_strict(&self) -> Result<Vec<JournalRecord>, CodegenError> {
        let Some(bytes) = self.store.read(&self.artifact)? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(bytes).map_err(|e| {
            CodegenError::Journal(format!("journal {} not utf-8: {e}", self.artifact))
        })?;
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rec: JournalRecord = serde_json::from_str(line).map_err(|e| {
                CodegenError::Journal(format!("journal {} line {}: {e}", self.artifact, i + 1))
            })?;
            out.push(rec);
        }
        Ok(out)
    }

    /// Append one record (write-ahead: call *before* the mutation).
    /// Read-modify-write of the whole JSONL file through the store's
    /// atomic `write`.
    pub fn append(&self, rec: JournalRecord) -> Result<(), CodegenError> {
        let mut bytes = self.store.read(&self.artifact)?.unwrap_or_default();
        let mut line = serde_json::to_vec(&rec)
            .map_err(|e| CodegenError::Journal(format!("serialize record: {e}")))?;
        line.push(b'\n');
        bytes.extend_from_slice(&line);
        self.store
            .write(&self.artifact, &bytes)
            .map_err(CodegenError::from)
    }

    /// Next sequence number = count of existing complete records.
    pub fn next_seq(&self) -> Result<u64, CodegenError> {
        Ok(self.records()?.len() as u64)
    }

    /// Persist a before-image blob (content-addressed — identical
    /// priors dedup: skip the write if the hash already exists).
    pub fn ensure_blob(
        &self,
        hash: &ContentHash,
        content: &[u8],
    ) -> Result<ArtifactRef, CodegenError> {
        let r = blob_ref(hash);
        if self.store.hash(&r)?.is_none() {
            self.store.write(&r, content)?;
        }
        Ok(r)
    }
}

/// Store a before-image for `artifact` if it exists, returning the
/// journal fields (`before_hash`, `before_blob`). Shared by apply and
/// revert paths.
pub fn capture_before<S: ArtifactStore + ?Sized>(
    store: &S,
    journal: &Journal<'_, S>,
    artifact: &ArtifactRef,
) -> Result<(Option<ContentHash>, Option<ArtifactRef>), CodegenError> {
    match store.read(artifact)? {
        Some(bytes) => {
            let hash = ContentHash::of(&bytes);
            let blob = journal.ensure_blob(&hash, &bytes)?;
            Ok((Some(hash), Some(blob)))
        }
        None => Ok((None, None)),
    }
}

/// Map a store error into the journal error channel where a journal
/// op failed for store reasons.
pub fn store_err(context: &str, e: StoreError) -> CodegenError {
    CodegenError::Journal(format!("{context}: {e}"))
}
