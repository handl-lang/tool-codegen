//! The pipeline traits: `IrModule`, `Source<I>`, `Emitter<I>`, and the
//! kernel-side `EmitContext`.
//!
//! # `EmitContext`, not `CodegenRequest`
//!
//! The proposal names `CodegenRequest` as the parse/materialize
//! input, but that type lives in the host plugin layer
//! (`handl-plugin-codegen`). This kernel stays **host-free** — it
//! defines `EmitContext`, a minimal kernel-side request shape
//! (output-dir hint, config map, language name), and the plugin
//! adapts `CodegenRequest` into it at the boundary. Keeping the
//! kernel free of host deps is what lets `hnl-tool-codemod` and
//! other engine crates depend on it (engine→engine deps are legal;
//! engine→host-plugin deps are not).

use crate::change::Change;
use crate::error::CodegenError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// IR family tag — a string newtype so registries can pair sources
/// and emitters by kind without a god-enum.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IrKind(pub String);

impl IrKind {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for IrKind {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Any typed intermediate representation the pipeline lifts to.
/// Deliberately minimal — `kind()` only; diagnostics stay per-family.
/// IR ranges from declarative data (`SdkgenModule`) to costed,
/// semantics-bearing terms (`lib-kernel-ir`'s `Term`).
pub trait IrModule {
    fn kind(&self) -> IrKind;
}

/// Kernel-side request passed to `Source::parse` /
/// `Emitter::materialize` / `Emitter::verify`. The host plugin's
/// `CodegenRequest` is adapted into this before dispatch.
#[derive(Clone, Debug, Default)]
pub struct EmitContext {
    /// Where generated artifacts are headed (a hint for emitters that
    /// compute relative paths or embed output locations).
    pub output_dir: Option<PathBuf>,
    /// Free-form emitter/source configuration (`arm=`, `leaves=`, …).
    pub config: BTreeMap<String, String>,
    /// Source language name when known (canonical kit name).
    pub language: Option<String>,
    /// Input spec/header path the `Source` parses (the plugin's
    /// `CodegenRequest::spec`, already resolved to a local path).
    pub spec_path: Option<PathBuf>,
    /// Provenance URL (`--from-url`/`--source-url`) — sources stamp it
    /// into the IR so emitters can render `--- source:` headers.
    pub source_url: Option<String>,
    /// Cache directory for doc/spec caches (e.g. webidl's MDN cache) —
    /// request-provided, deliberately outside the output tree.
    pub cache_dir: Option<PathBuf>,
}

impl EmitContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    pub fn with_language(mut self, lang: impl Into<String>) -> Self {
        self.language = Some(lang.into());
        self
    }

    pub fn with_config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.insert(key.into(), value.into());
        self
    }

    pub fn with_spec_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.spec_path = Some(path.into());
        self
    }

    pub fn with_source_url(mut self, url: impl Into<String>) -> Self {
        self.source_url = Some(url.into());
        self
    }

    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }
}

/// Lift: anything → typed IR. `parse` is the only required method;
/// a source may be a file header, a spec path, or an in-memory blob —
/// `req` carries whatever the source needs.
pub trait Source<I: IrModule> {
    fn parse(&self, req: &EmitContext) -> Result<I, CodegenError>;
}

/// Materialize: IR → desired `Change` set, **no side effects**.
/// `verify` is the optional honest-decline gate (mlopt's
/// `lower(verify=True)` / kernelopt's `KernelSink` precedent) and
/// sees both IR and desired state.
pub trait Emitter<I: IrModule> {
    fn materialize(&self, ir: &I, req: &EmitContext) -> Result<Vec<Change>, CodegenError>;

    /// Optional verify gate — default accepts everything.
    fn verify(&self, ir: &I, changes: &[Change]) -> Result<(), CodegenError> {
        let _ = ir;
        let _ = changes;
        Ok(())
    }
}
