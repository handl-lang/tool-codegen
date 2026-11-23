<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->

# Agent guide — engine-codegen

The desired-state codegen kernel (`hnl-engine-codegen`), pure Rust —
no handl host/plugin deps. Implements the lifecycle half of
proposal P-001 / plan PL-004 Phase 0: `Source<I> → IR → Emitter<I> →
Change set → Plan/Apply/Destroy/Revert` over a generic
`ArtifactStore` (files are one impl).

## Layout

- `src/` — the whole crate (single-crate layout like
  `engine-transform`): `types.rs` (`ArtifactRef`/`ContentHash`/
  `ArtifactStat`), `store.rs` (`ArtifactStore` trait, `FsStore`,
  `MemoryStore`), `traits.rs` (`IrModule`/`Source`/`Emitter`/
  `EmitContext`), `change.rs` (`Change`), `manifest.rs`
  (`Manifest`/`ManifestEntry`), `journal.rs` (append-only JSONL
  write-ahead record), `plan.rs` (`Plan`/`PlanEntry`/`PlanAction`),
  `engine.rs` (`Engine` lifecycle: `plan`/`apply`/`destroy`/`revert`/
  `resume`), `error.rs` (`CodegenError`/`StoreError`).
- `tests/` — `store.rs` (MemoryStore unit + FsStore/tempfile
  integration), `lifecycle.rs` (plan/apply/destroy/revert/resume,
  drift refusal, crash-resume, property tests).
- `xtask/` — repo gate tasks (`fmt`, `clippy`, `test`, `doc`,
  `gate`); `ci.sh` is the bupha entry.

## Conventions

- The kernel is **host-free**: `EmitContext` is the kernel-side
  request shape; `handl-plugin-codegen` adapts its `CodegenRequest`
  into it. Do not add host/plugin deps here.
- Artifact ordering is always sorted by `ArtifactRef` — keep
  `Change` sets deterministic.
- Writes are write-ahead journaled *before* each mutation; apply is
  fail-closed on `before_hash` drift.
- `BundleStore` is a documented TODO (stub only).

## Verification

```sh
cargo xtask gate        # fmt -> clippy -D warnings -> test -> doc
# or individually: cargo xtask fmt|clippy|test|doc|check
```
