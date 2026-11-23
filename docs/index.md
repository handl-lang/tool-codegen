# engine-codegen

`hnl-engine-codegen` is the pure-Rust kernel for the desired-state
codegen pipeline: lift anything to a typed IR, materialize the IR to
a desired `Change` set, optionally verify, then reconcile an
`ArtifactStore` to that desired state via `Plan`/`Apply`/`Destroy`/
`Revert`.

- Traits: `IrModule`, `Source<I>`, `Emitter<I>` — generic over the IR
  type, no god-enum.
- Stores: `ArtifactStore` trait with `FsStore` (atomic temp+rename)
  and `MemoryStore` impls.
- Lifecycle: `Engine` — deterministic plan diffs, write-ahead
  journal, manifest-owned destroy, hash-checked revert, resume.
