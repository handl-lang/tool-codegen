//! Error types: `StoreError` (store impl failures) and `CodegenError`
//! (the kernel-wide error every trait/lifecycle op returns).

use crate::types::{ArtifactRef, ContentHash};
use std::fmt;

/// Store-implementation failure. The lifecycle engine never produces
/// these directly — it wraps them in `CodegenError::Store`.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying I/O failure. `path` is the store-relative ref (or the
    /// host path for `FsStore`).
    Io {
        op: &'static str,
        path: String,
        source: std::io::Error,
    },
    /// The artifact ref is not a legal key for this store.
    InvalidRef(String),
    /// Internal synchronization primitive poisoned (Mutex).
    Poisoned,
    /// Store capability not implemented (e.g. `BundleStore` stub).
    Unsupported(String),
}

impl StoreError {
    pub fn io(op: &'static str, path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "store {op} {path:?} failed: {source}")
            }
            Self::InvalidRef(m) => write!(f, "invalid artifact ref: {m}"),
            Self::Poisoned => write!(f, "store lock poisoned"),
            Self::Unsupported(m) => write!(f, "store unsupported: {m}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Kernel-wide error. `Source::parse`, `Emitter::materialize`,
/// `Emitter::verify`, and every `Engine` op return this.
#[derive(Debug)]
pub enum CodegenError {
    /// Store read/write/delete/hash/stat failure.
    Store(StoreError),
    /// `Source::parse` failure.
    Parse(String),
    /// `Emitter::materialize` failure.
    Emit(String),
    /// `Emitter::verify` gate failure — honest decline before apply.
    Verify(String),
    /// Fail-closed drift: the store content no longer matches the hash
    /// observed at plan time (or a `Change.before_hash` lied).
    Drift {
        artifact: ArtifactRef,
        expected: Option<ContentHash>,
        found: Option<ContentHash>,
    },
    /// Journal append/read/parse failure.
    Journal(String),
    /// Manifest encode/decode/store failure.
    Manifest(String),
    /// Artifact ref rejected by the kernel (reserved prefix, illegal
    /// path shape, duplicate refs in a `Change` set).
    InvalidRef(String),
    /// Operation not supported by this engine/store combination.
    Unsupported(String),
}

impl CodegenError {
    pub fn drift(
        artifact: ArtifactRef,
        expected: Option<ContentHash>,
        found: Option<ContentHash>,
    ) -> Self {
        Self::Drift {
            artifact,
            expected,
            found,
        }
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Parse(m) => write!(f, "parse: {m}"),
            Self::Emit(m) => write!(f, "emit: {m}"),
            Self::Verify(m) => write!(f, "verify: {m}"),
            Self::Drift {
                artifact,
                expected,
                found,
            } => write!(
                f,
                "drift on {artifact}: expected {}, found {}",
                expected.map_or("<absent>".to_string(), |h| h.to_string()),
                found.map_or("<absent>".to_string(), |h| h.to_string())
            ),
            Self::Journal(m) => write!(f, "journal: {m}"),
            Self::Manifest(m) => write!(f, "manifest: {m}"),
            Self::InvalidRef(m) => write!(f, "invalid artifact ref: {m}"),
            Self::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for CodegenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for CodegenError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
