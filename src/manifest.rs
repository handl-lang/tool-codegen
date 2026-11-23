//! `Manifest` — the record of what a generator owns. Stored as a
//! store artifact itself (`.handl/codegen/manifest/<target>.json`),
//! so every store gets lifecycle tracking for free.

use crate::types::{ArtifactRef, ContentHash, RESERVED_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One artifact the generator owns: ref + content hash at apply time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub artifact: ArtifactRef,
    pub hash: ContentHash,
}

/// Ownership + provenance record written at the end of a successful
/// `apply`. `destroy` deletes exactly `artifacts` — never anything
/// unrecorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Generator/target name (owner).
    pub target: String,
    /// Generator version — plan caches key on this.
    pub generator_version: String,
    /// Source hashes the run consumed (spec files, headers, …).
    pub source_hashes: BTreeMap<String, ContentHash>,
    /// Owned artifacts, sorted by ref.
    pub artifacts: Vec<ManifestEntry>,
    /// Journal run id that produced this manifest.
    pub run_id: String,
}

/// Map a free-form name (target, run id) to a ref-safe path segment.
pub fn ref_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Manifest {
    /// The store ref this target's manifest lives at.
    pub fn artifact_ref(target: &str) -> ArtifactRef {
        ArtifactRef::new(format!(
            "{RESERVED_PREFIX}manifest/{}.json",
            ref_safe(target)
        ))
    }
}
