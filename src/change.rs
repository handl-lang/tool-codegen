//! `Change` — one desired artifact write (or, implicitly, one entry
//! in the desired set; deletes are derived from manifest-vs-desired
//! diffing, not from `Change` itself).

use crate::types::{ArtifactRef, ContentHash};
use serde::{Deserialize, Serialize};

/// A single desired artifact: where it goes, the store hash observed
/// when the desired state was computed (drift tripwire), and the
/// bytes to write. `content` is `Vec<u8>` — text emitters return
/// strings, but binary artifacts (`.safetensors`, `.onnx`) exist
/// downstream and must not round-trip through `String`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub artifact: ArtifactRef,
    /// Store hash observed at plan/materialize time, if any. `plan`
    /// validates this against the store and fails closed on mismatch;
    /// `apply` re-checks it before mutating.
    pub before_hash: Option<ContentHash>,
    pub content: Vec<u8>,
}

impl Change {
    /// UTF-8 text convenience constructor.
    pub fn text(artifact: impl Into<ArtifactRef>, content: impl Into<String>) -> Self {
        Self {
            artifact: artifact.into(),
            before_hash: None,
            content: content.into().into_bytes(),
        }
    }

    /// Binary content constructor.
    pub fn binary(artifact: impl Into<ArtifactRef>, content: Vec<u8>) -> Self {
        Self {
            artifact: artifact.into(),
            before_hash: None,
            content,
        }
    }

    /// Content hash of the desired bytes.
    pub fn after_hash(&self) -> ContentHash {
        ContentHash::of(&self.content)
    }
}

/// Wire form for `Change` — `content` as a byte array (serde's
/// default `Vec<u8>` encoding). Kept for the future stage-protocol
/// boundary; the kernel itself never serializes `Change`s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRepr {
    pub artifact: ArtifactRef,
    pub before_hash: Option<ContentHash>,
    pub content: Vec<u8>,
}

impl From<&Change> for ChangeRepr {
    fn from(c: &Change) -> Self {
        Self {
            artifact: c.artifact.clone(),
            before_hash: c.before_hash,
            content: c.content.clone(),
        }
    }
}
