//! `Change` — one entry in the desired-state set.
//!
//! Two variants:
//!
//! - [`Change::Put`] — write `content` to `artifact` (create/update).
//!   `content` is `Vec<u8>`: text emitters return strings, but binary
//!   artifacts (`.safetensors`, `.onnx`) exist downstream and must not
//!   round-trip through `String`.
//! - [`Change::Delete`] — emitter-declared stale-artifact cleanup:
//!   "this artifact should not exist" (e.g. an emitter that supersedes
//!   previously-generated files). Deletes are journaled with
//!   before-images exactly like puts, so `revert` restores them.
//!
//! Deletes never enter the manifest: the manifest records the
//! *desired end state*, and a deleted artifact simply isn't an entry.
//! Manifest-vs-desired diffing still produces deletes for artifacts
//! the tool owned but the emitter no longer mentions at all.

use crate::types::{ArtifactRef, ContentHash};
use serde::{Deserialize, Serialize};

/// One desired-state operation on one artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Write `content` to `artifact`. `before_hash` is the store hash
    /// observed when the desired state was computed (drift tripwire);
    /// `plan` validates it against the store and fails closed on
    /// mismatch, `apply` re-checks before mutating.
    Put {
        artifact: ArtifactRef,
        before_hash: Option<ContentHash>,
        content: Vec<u8>,
    },
    /// Delete `artifact` — emitter-declared cleanup of a stale
    /// artifact. Absent artifacts are a no-op, not an error.
    Delete { artifact: ArtifactRef },
}

impl Change {
    /// UTF-8 text `Put` convenience constructor.
    pub fn text(artifact: impl Into<ArtifactRef>, content: impl Into<String>) -> Self {
        Self::Put {
            artifact: artifact.into(),
            before_hash: None,
            content: content.into().into_bytes(),
        }
    }

    /// Binary `Put` constructor.
    pub fn binary(artifact: impl Into<ArtifactRef>, content: Vec<u8>) -> Self {
        Self::Put {
            artifact: artifact.into(),
            before_hash: None,
            content,
        }
    }

    /// `Put` with an explicit observed-before hash (drift tripwire).
    pub fn put_with_before(
        artifact: impl Into<ArtifactRef>,
        before_hash: Option<ContentHash>,
        content: Vec<u8>,
    ) -> Self {
        Self::Put {
            artifact: artifact.into(),
            before_hash,
            content,
        }
    }

    /// Emitter-declared delete of `artifact`.
    pub fn delete(artifact: impl Into<ArtifactRef>) -> Self {
        Self::Delete {
            artifact: artifact.into(),
        }
    }

    /// The artifact this change targets.
    pub fn artifact(&self) -> &ArtifactRef {
        match self {
            Self::Put { artifact, .. } | Self::Delete { artifact } => artifact,
        }
    }

    /// Observed-before hash claim (`Put` only).
    pub fn before_hash(&self) -> Option<ContentHash> {
        match self {
            Self::Put { before_hash, .. } => *before_hash,
            Self::Delete { .. } => None,
        }
    }

    /// Desired content hash — `Put` content hash, `None` for deletes
    /// (the desired end state is absence).
    pub fn after_hash(&self) -> Option<ContentHash> {
        match self {
            Self::Put { content, .. } => Some(ContentHash::of(content)),
            Self::Delete { .. } => None,
        }
    }

    /// `Put` content bytes, if this is a put.
    pub fn content(&self) -> Option<&[u8]> {
        match self {
            Self::Put { content, .. } => Some(content),
            Self::Delete { .. } => None,
        }
    }
}

/// Wire form for `Change` — kept for the future stage-protocol
/// boundary; the kernel itself never serializes `Change`s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ChangeRepr {
    Put {
        artifact: ArtifactRef,
        before_hash: Option<ContentHash>,
        #[serde(with = "serde_bytes_like")]
        content: Vec<u8>,
    },
    Delete {
        artifact: ArtifactRef,
    },
}

/// Minimal byte-array serde (serde_json's default `Vec<u8>` encoding).
mod serde_bytes_like {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        Vec::<u8>::deserialize(d)
    }
}

impl From<&Change> for ChangeRepr {
    fn from(c: &Change) -> Self {
        match c {
            Change::Put {
                artifact,
                before_hash,
                content,
            } => Self::Put {
                artifact: artifact.clone(),
                before_hash: *before_hash,
                content: content.clone(),
            },
            Change::Delete { artifact } => Self::Delete {
                artifact: artifact.clone(),
            },
        }
    }
}
