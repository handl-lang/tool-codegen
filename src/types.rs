//! Core value types: `ArtifactRef`, `ContentHash`, `ArtifactStat`.
//!
//! `ArtifactRef` is the opaque store key — a relative path for
//! `FsStore`, an index/member name for `MemoryStore`/`BundleStore`.
//! The kernel never interprets it; only store impls do.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Component;
use std::str::FromStr;

/// Reserved prefix for kernel-internal artifacts (manifest, journal,
/// before-image blobs). User-facing desired `Change`s must not target
/// refs under this prefix — `Engine` rejects them.
pub const RESERVED_PREFIX: &str = ".handl/codegen/";

/// Opaque artifact store key.
///
/// Ordering is the raw string order, which is the deterministic
/// ordering the lifecycle engine uses everywhere.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactRef(pub String);

impl ArtifactRef {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for kernel-internal refs (manifest/journal/blob paths).
    pub fn is_reserved(&self) -> bool {
        self.0.starts_with(RESERVED_PREFIX)
    }

    /// Validate as a relative path usable under `FsStore`: non-empty,
    /// no root/prefix, no `..` components.
    pub fn validate_fs_relative(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("empty artifact ref".to_string());
        }
        if self.0.contains('\0') {
            return Err(format!("artifact ref {self:?} contains NUL"));
        }
        let mut comps = std::path::Path::new(&self.0).components();
        for c in comps.by_ref() {
            match c {
                Component::Normal(_) | Component::CurDir => {}
                other => {
                    return Err(format!(
                        "artifact ref {self:?} has non-relative component {other:?}"
                    ));
                }
            }
        }
        // A trailing `..` or `.` segment is caught above; also reject a
        // bare "." or paths that normalize to nothing.
        if self.0 == "." {
            return Err("artifact ref must not be `.`".to_string());
        }
        Ok(())
    }
}

impl fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ArtifactRef {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for ArtifactRef {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Fixed-size content hash (BLAKE3-256). Serialized as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// Hash a complete in-memory buffer.
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// Streaming hash — stores prefer this for large artifacts.
    pub fn of_reader<R: std::io::Read>(mut r: R) -> std::io::Result<Self> {
        let mut h = blake3::Hasher::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(Self(*h.finalize().as_bytes()))
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        if s.len() != 64 {
            return Err(format!(
                "content hash must be 64 hex chars, got {}",
                s.len()
            ));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let hi = (chunk[0] as char).to_digit(16);
            let lo = (chunk[1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => out[i] = ((h << 4) | l) as u8,
                _ => return Err(format!("invalid hex in content hash {s:?}")),
            }
        }
        Ok(Self(out))
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.to_hex())
    }
}

impl FromStr for ContentHash {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for ContentHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Cheap stat pre-filter for `ArtifactStore::stat` — size + mtime let
/// callers skip a re-hash when nothing changed.
#[derive(Clone, Copy, Debug)]
pub struct ArtifactStat {
    pub size: u64,
    pub mtime: Option<std::time::SystemTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_hex_roundtrip() {
        let h = ContentHash::of(b"hello");
        let s = h.to_hex();
        assert_eq!(s.len(), 64);
        assert_eq!(ContentHash::from_hex(&s).unwrap(), h);
        assert!(ContentHash::from_hex("zz").is_err());
        assert!(ContentHash::from_hex(&s[..62]).is_err());
    }

    #[test]
    fn ref_validation() {
        assert!(
            ArtifactRef::new("src/gen/foo.hnl")
                .validate_fs_relative()
                .is_ok()
        );
        assert!(ArtifactRef::new("").validate_fs_relative().is_err());
        assert!(
            ArtifactRef::new("/abs/path")
                .validate_fs_relative()
                .is_err()
        );
        assert!(
            ArtifactRef::new("../escape")
                .validate_fs_relative()
                .is_err()
        );
        assert!(
            ArtifactRef::new("a/../../b")
                .validate_fs_relative()
                .is_err()
        );
        assert!(ArtifactRef::new(".").validate_fs_relative().is_err());
    }
}
