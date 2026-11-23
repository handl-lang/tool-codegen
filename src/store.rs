//! `ArtifactStore` — the store-agnostic persistence port — plus the
//! `FsStore` and `MemoryStore` impls.
//!
//! `BundleStore` (member names in a single archive) is a documented
//! TODO — see the crate docs; it will implement this same trait.

use crate::error::StoreError;
use crate::types::{ArtifactRef, ArtifactStat, ContentHash};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Opaque artifact persistence. All methods take `&self`; impls that
/// need mutation use interior mutability (`Mutex`).
///
/// `write` must be atomic per artifact: observers never see a
/// half-written artifact (`FsStore` = temp file + rename;
/// `MemoryStore` = single map insert).
pub trait ArtifactStore {
    /// Stored bytes, or `None` if the artifact does not exist.
    fn read(&self, r: &ArtifactRef) -> Result<Option<Vec<u8>>, StoreError>;

    /// Atomically store `content` at `r` (create parents as needed).
    fn write(&self, r: &ArtifactRef, content: &[u8]) -> Result<(), StoreError>;

    /// Remove `r`. Deleting an absent artifact is `Ok(())` —
    /// idempotent delete keeps `destroy`/`revert` crash-safe.
    fn delete(&self, r: &ArtifactRef) -> Result<(), StoreError>;

    /// Content hash of the stored artifact, `None` if absent.
    /// Default impl reads + hashes; stores may override with a
    /// cheaper path (e.g. `FsStore` could stat-cache later).
    fn hash(&self, r: &ArtifactRef) -> Result<Option<ContentHash>, StoreError> {
        match self.read(r)? {
            Some(bytes) => Ok(Some(ContentHash::of(&bytes))),
            None => Ok(None),
        }
    }

    /// Size + mtime pre-filter for cache/drift checks.
    fn stat(&self, r: &ArtifactRef) -> Result<Option<ArtifactStat>, StoreError>;
}

// ------------------------------------------------------------------
// FsStore
// ------------------------------------------------------------------

/// Filesystem-backed store rooted at a directory. `ArtifactRef` is a
/// validated relative path under the root.
///
/// `hash()` carries an in-memory **stat→hash memo**: the first hash of
/// an artifact records `(size, mtime) → ContentHash`; a later `hash()`
/// whose `stat` is unchanged returns the memoized hash without
/// re-reading the file — the same mtime/size pre-filter handl-run's
/// input cache (`crates/run/src/cache.rs`) applies to its manifests.
/// `write` primes the memo (the post-write verify in `Engine` then
/// costs one `stat`), `delete`/absent-stat evict it, and an external
/// rewrite that changes size or mtime misses and re-hashes.
///
/// Caveat (inherited from the pre-filter design): a rewrite that
/// preserves both size *and* mtime is undetectable — the memo would
/// serve the stale hash. mtimes carry nanoseconds and `FsStore`
/// writes always move mtime, so this requires deliberate timestamp
/// forgery; callers needing stronger guarantees should hash through a
/// fresh `FsStore` (empty memo) per run.
pub struct FsStore {
    root: PathBuf,
    tmp_counter: AtomicUsize,
    /// `(stat fingerprint, content hash)` per artifact ref.
    hash_memo: Mutex<BTreeMap<ArtifactRef, (ArtifactStat, ContentHash)>>,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            tmp_counter: AtomicUsize::new(0),
            hash_memo: Mutex::new(BTreeMap::new()),
        }
    }

    /// The memo is a pure optimization — a poisoned lock must never
    /// fail an operation, so recover the guard and keep going.
    fn memo(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<ArtifactRef, (ArtifactStat, ContentHash)>> {
        self.hash_memo.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a ref to a host path, validating it is relative and
    /// traversal-free.
    fn path_for(&self, r: &ArtifactRef) -> Result<PathBuf, StoreError> {
        r.validate_fs_relative()
            .map_err(|m| StoreError::InvalidRef(format!("{r}: {m}")))?;
        Ok(self.root.join(&r.0))
    }

    /// Write `content` to `tmp` then rename onto `path`. The temp file
    /// lives in the same directory so the rename stays on one
    /// filesystem (atomic on POSIX; on Windows we remove the
    /// destination first — a small non-atomic window documented here).
    fn atomic_write(&self, path: &Path, content: &[u8]) -> Result<(), StoreError> {
        let dir = path.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir)
            .map_err(|e| StoreError::io("mkdir", dir.display().to_string(), e))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_string());
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.codegen-tmp-{}-{n}", std::process::id()));
        let write_result = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
            drop(f);
            match std::fs::rename(&tmp, path) {
                Ok(()) => Ok(()),
                Err(_) if path.exists() => {
                    // Windows: rename refuses an existing destination.
                    std::fs::remove_file(path)?;
                    std::fs::rename(&tmp, path)
                }
                Err(e) => Err(e),
            }
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_result.map_err(|e| StoreError::io("write", path.display().to_string(), e))
    }
}

impl ArtifactStore for FsStore {
    fn read(&self, r: &ArtifactRef) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path_for(r)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io("read", path.display().to_string(), e)),
        }
    }

    fn write(&self, r: &ArtifactRef, content: &[u8]) -> Result<(), StoreError> {
        let path = self.path_for(r)?;
        self.atomic_write(&path, content)?;
        // Prime the hash memo with the post-write stat + the hash of
        // the bytes just written — the engine's post-write verify then
        // costs one `stat` instead of a full re-read.
        match self.stat(r) {
            Ok(Some(stat)) => {
                self.memo()
                    .insert(r.clone(), (stat, ContentHash::of(content)));
            }
            // Stat failed after a successful write: evict conservatively.
            _ => {
                self.memo().remove(r);
            }
        }
        Ok(())
    }

    fn delete(&self, r: &ArtifactRef) -> Result<(), StoreError> {
        let path = self.path_for(r)?;
        let result = match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::io("delete", path.display().to_string(), e)),
        };
        // Evict whether or not the file existed — a stale memo entry
        // must never outlive the artifact.
        self.memo().remove(r);
        result
    }

    fn hash(&self, r: &ArtifactRef) -> Result<Option<ContentHash>, StoreError> {
        // Stat first: it doubles as the existence check and the
        // pre-filter fingerprint.
        let Some(stat) = self.stat(r)? else {
            self.memo().remove(r);
            return Ok(None);
        };
        if let Some((s, h)) = self.memo().get(r)
            && *s == stat
        {
            return Ok(Some(*h));
        }
        let path = self.path_for(r)?;
        let hash = match std::fs::File::open(&path) {
            Ok(f) => ContentHash::of_reader(std::io::BufReader::new(f))
                .map_err(|e| StoreError::io("hash", path.display().to_string(), e))?,
            // Deleted between stat and open — treat as absent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.memo().remove(r);
                return Ok(None);
            }
            Err(e) => return Err(StoreError::io("hash", path.display().to_string(), e)),
        };
        // Record under the stat taken *before* hashing: a mid-hash
        // rewrite moves mtime, so the next call's stat misses and
        // re-hashes — the safe direction.
        self.memo().insert(r.clone(), (stat, hash));
        Ok(Some(hash))
    }

    fn stat(&self, r: &ArtifactRef) -> Result<Option<ArtifactStat>, StoreError> {
        let path = self.path_for(r)?;
        match std::fs::metadata(&path) {
            Ok(m) => Ok(Some(ArtifactStat {
                size: m.len(),
                mtime: m.modified().ok(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io("stat", path.display().to_string(), e)),
        }
    }
}

// ------------------------------------------------------------------
// MemoryStore
// ------------------------------------------------------------------

/// In-memory store — `ArtifactRef` keys map to byte contents. The
/// default for unit tests and for `hnl compile`-style single-shot
/// sinks.
pub struct MemoryStore {
    map: Mutex<BTreeMap<ArtifactRef, Vec<u8>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(BTreeMap::new()),
        }
    }

    /// Full snapshot in deterministic (sorted) order — for tests and
    /// property checks.
    pub fn snapshot(&self) -> BTreeMap<ArtifactRef, Vec<u8>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtifactStore for MemoryStore {
    fn read(&self, r: &ArtifactRef) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .map
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .get(r)
            .cloned())
    }

    fn write(&self, r: &ArtifactRef, content: &[u8]) -> Result<(), StoreError> {
        self.map
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .insert(r.clone(), content.to_vec());
        Ok(())
    }

    fn delete(&self, r: &ArtifactRef) -> Result<(), StoreError> {
        self.map.lock().map_err(|_| StoreError::Poisoned)?.remove(r);
        Ok(())
    }

    fn hash(&self, r: &ArtifactRef) -> Result<Option<ContentHash>, StoreError> {
        Ok(self
            .map
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .get(r)
            .map(|b| ContentHash::of(b)))
    }

    fn stat(&self, r: &ArtifactRef) -> Result<Option<ArtifactStat>, StoreError> {
        Ok(self
            .map
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .get(r)
            .map(|b| ArtifactStat {
                size: b.len() as u64,
                mtime: None,
            }))
    }
}

// ------------------------------------------------------------------
// FsStore hash memo (stat→hash pre-filter)
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ArtifactRef;
    use std::time::{Duration, SystemTime};

    fn aref(s: &str) -> ArtifactRef {
        ArtifactRef::new(s)
    }

    #[test]
    fn hash_memo_primes_on_write_and_evicts_on_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsStore::new(tmp.path());
        let r = aref("f.txt");

        s.write(&r, b"v1").unwrap();
        assert_eq!(s.memo().len(), 1, "write should prime the memo");
        assert_eq!(
            s.hash(&r).unwrap().unwrap(),
            ContentHash::of(b"v1"),
            "memoized hash must match content"
        );

        s.delete(&r).unwrap();
        assert!(s.memo().is_empty(), "delete must evict the memo");
        assert!(s.hash(&r).unwrap().is_none());
    }

    #[test]
    fn hash_memo_rehashes_on_external_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsStore::new(tmp.path());
        let r = aref("f.txt");
        let path = tmp.path().join("f.txt");

        s.write(&r, b"v1").unwrap();
        assert_eq!(s.hash(&r).unwrap().unwrap(), ContentHash::of(b"v1"));

        // External rewrite, different size → stat fingerprint moves.
        std::fs::write(&path, b"v2-much-longer").unwrap();
        assert_eq!(
            s.hash(&r).unwrap().unwrap(),
            ContentHash::of(b"v2-much-longer"),
            "size change must miss the memo"
        );

        // External delete → absent, memo evicted.
        std::fs::remove_file(&path).unwrap();
        assert!(s.hash(&r).unwrap().is_none());
        assert!(s.memo().is_empty());
    }

    #[test]
    fn hash_memo_rehashes_on_same_size_rewrite_with_new_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsStore::new(tmp.path());
        let r = aref("f.txt");
        let path = tmp.path().join("f.txt");

        s.write(&r, b"aaaa").unwrap();
        assert_eq!(s.hash(&r).unwrap().unwrap(), ContentHash::of(b"aaaa"));

        // Same size, different bytes — only mtime distinguishes it.
        // Pin the mtime forward explicitly so the test is independent
        // of filesystem timestamp granularity.
        std::fs::write(&path, b"bbbb").unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(60)),
        )
        .unwrap();
        drop(f);

        assert_eq!(
            s.hash(&r).unwrap().unwrap(),
            ContentHash::of(b"bbbb"),
            "mtime change must miss the memo even at equal size"
        );
    }

    #[test]
    fn hash_memo_unknown_artifact_hashes_and_records() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsStore::new(tmp.path());
        let r = aref("external.txt");
        // File appears without going through the store.
        std::fs::write(tmp.path().join("external.txt"), b"ext").unwrap();
        assert_eq!(s.hash(&r).unwrap().unwrap(), ContentHash::of(b"ext"));
        assert_eq!(s.memo().len(), 1);
        // Repeat hash: memo hit, same answer.
        assert_eq!(s.hash(&r).unwrap().unwrap(), ContentHash::of(b"ext"));
    }
}
