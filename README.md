# engine-codegen

`hnl-engine-codegen` — the desired-state codegen kernel.

The reconciliation half of the org-wide `Lift → Transform →
Materialize → [Verify] → Reconcile` pipeline (proposal P-001,
plan PL-004): generic `Source<I>`/`Emitter<I>` traits over any IR
type, `Change` sets as desired state, and a store-agnostic
`Plan`/`Apply`/`Destroy`/`Revert` lifecycle engine with manifest +
write-ahead journal over an `ArtifactStore` (`FsStore`,
`MemoryStore`; `BundleStore` TODO).

Pure Rust, minimal deps (serde/serde_json/blake3) — no handl
host/plugin dependencies.

## Development

```bash
cargo xtask gate    # fmt → clippy → test → doc
```

## License

MIT.
