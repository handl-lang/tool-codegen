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
pub struct FsStore {
    root: PathBuf,
    tmp_counter: AtomicUsize,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            tmp_counter: AtomicUsize::new(0),
        }
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
        self.atomic_write(&path, content)
    }

    fn delete(&self, r: &ArtifactRef) -> Result<(), StoreError> {
        let path = self.path_for(r)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::io("delete", path.display().to_string(), e)),
        }
    }

    fn hash(&self, r: &ArtifactRef) -> Result<Option<ContentHash>, StoreError> {
        let path = self.path_for(r)?;
        match std::fs::File::open(&path) {
            Ok(f) => ContentHash::of_reader(std::io::BufReader::new(f))
                .map(Some)
                .map_err(|e| StoreError::io("hash", path.display().to_string(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io("hash", path.display().to_string(), e)),
        }
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
