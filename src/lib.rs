//! `hnl-engine-codegen` — the desired-state codegen kernel.
//!
//! The reconciliation half of the org-wide pipeline
//! `Lift → Transform → Materialize → [Verify] → Reconcile`
//! (proposal P-001, plan PL-004 Phase 0):
//!
//! ```text
//! Source<I> ──parse──► I: IrModule ──[domain Transform]──► I'
//!                           │
//!                      Emitter<I>::materialize ──► Vec<Change>  (desired)
//!                           │
//!                      [Emitter::verify]  (optional honest-decline gate)
//!                           │
//!                      ArtifactStore ── Plan / Apply / Destroy / Revert
//!                      (FsStore | MemoryStore | BundleStore TODO)
//! ```
//!
//! # Host-free by design
//!
//! This crate has **no handl host/plugin dependencies**. The proposal's
//! `CodegenRequest` lives in `handl-plugin-codegen`; the kernel
//! defines [`EmitContext`] — a minimal request shape (output-dir
//! hint, config map, language name) — and the plugin adapts
//! `CodegenRequest` into it at the boundary. Engine crates
//! (`hnl-engine-codemod`, compile backends, …) can depend on this
//! kernel; nothing here depends back on a plugin.
//!
//! # Guarantees
//!
//! - **Deterministic**: all mutation order is sorted `ArtifactRef`.
//! - **Write-ahead**: every mutation is journaled (before-image,
//!   content-addressed blob) *before* it happens; a crash mid-apply
//!   leaves a complete recovery record.
//! - **Fail-closed**: apply re-checks `before_hash` drift and
//!   post-write hashes; revert refuses artifacts changed since apply.
//! - **Owned vs borrowed**: `destroy` deletes only manifest-recorded
//!   artifacts (generators own output); `revert` restores journal
//!   before-images (mutating tools don't own their inputs).
//! - **Atomic per artifact**: `FsStore` writes are temp+rename;
//!   `MemoryStore` inserts are atomic by construction.
//!
//! # Layout
//!
//! - [`types`]: `ArtifactRef` (opaque store key), `ContentHash`
//!   (BLAKE3-256), `ArtifactStat`.
//! - [`store`]: `ArtifactStore` trait + `FsStore` + `MemoryStore`.
//!   `BundleStore` is a TODO.
//! - [`traits`]: `IrModule`, `Source<I>`, `Emitter<I>`, `EmitContext`.
//! - [`change`]: `Change` — desired artifact (binary-capable).
//! - [`plan`]: `Plan`/`PlanEntry`/`PlanAction` diff computation.
//! - [`manifest`]: `Manifest` — ownership/provenance record.
//! - [`journal`]: append-only JSONL write-ahead `Journal`.
//! - [`engine`]: `Engine` — `plan`/`apply`/`destroy`/`revert`/`resume`.
//! - [`error`][]: `CodegenError`/`StoreError`.

pub mod change;
pub mod engine;
pub mod error;
#[cfg(feature = "handl-term")]
pub mod handl_term;
pub mod journal;
pub mod manifest;
pub mod plan;
pub mod store;
pub mod traits;
pub mod types;

pub use change::Change;
pub use engine::{ApplyOptions, ApplyReport, Engine, OnError, RevertReport, RunMeta};
pub use error::{CodegenError, StoreError};
pub use journal::{JOURNAL_VERSION, Journal, JournalOp, JournalRecord, blob_ref, journal_ref};
pub use manifest::{Manifest, ManifestEntry, ref_safe};
pub use plan::{Plan, PlanAction, PlanEntry, normalize_changes};
pub use store::{ArtifactStore, FsStore, MemoryStore};
pub use traits::{EmitContext, Emitter, IrKind, IrModule, Source};
pub use types::{ArtifactRef, ArtifactStat, ContentHash, RESERVED_PREFIX};
