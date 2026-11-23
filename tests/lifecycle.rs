//! Lifecycle tests: plan/apply/destroy/revert/resume over
//! `MemoryStore` + `FsStore`, drift refusal, crash-resume via
//! truncated journal, keep-going policy, and the property tests
//! `apply∘plan == desired`, `destroy∘apply == original`.

use hnl_tool_codegen::journal::journal_ref;
use hnl_tool_codegen::manifest::Manifest;
use hnl_tool_codegen::plan::PlanAction;
use hnl_tool_codegen::store::ArtifactStore;
use hnl_tool_codegen::traits::{EmitContext, Emitter, IrKind, IrModule, Source};
use hnl_tool_codegen::types::{ArtifactRef, ContentHash};
use hnl_tool_codegen::{
    ApplyOptions, Change, CodegenError, Engine, FsStore, Journal, ManifestEntry, MemoryStore,
    OnError, RunMeta,
};
use std::collections::BTreeMap;

fn aref(s: &str) -> ArtifactRef {
    ArtifactRef::new(s)
}

fn meta(target: &str) -> RunMeta {
    let mut m = RunMeta::new(target, "test-gen-0.1");
    m.run_id = format!("run-{target}");
    m
}

/// Desired set as (path, content) pairs.
fn desired(pairs: &[(&str, &str)]) -> Vec<Change> {
    pairs.iter().map(|(p, c)| Change::text(*p, *c)).collect()
}

// ------------------------------------------------------------------
// Trait surface smoke test (Source/I -> IR -> Emitter -> Changes)
// ------------------------------------------------------------------

struct TestIr {
    files: Vec<(String, String)>,
}

impl IrModule for TestIr {
    fn kind(&self) -> IrKind {
        IrKind::new("test-ir")
    }
}

struct TestSource;

impl Source<TestIr> for TestSource {
    fn parse(&self, req: &EmitContext) -> Result<TestIr, CodegenError> {
        let n: usize = req
            .config
            .get("files")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        Ok(TestIr {
            files: (0..n)
                .map(|i| (format!("gen/{i}.txt"), format!("content-{i}")))
                .collect(),
        })
    }
}

struct TestEmitter;

impl Emitter<TestIr> for TestEmitter {
    fn materialize(&self, ir: &TestIr, _req: &EmitContext) -> Result<Vec<Change>, CodegenError> {
        Ok(ir
            .files
            .iter()
            .map(|(p, c)| Change::text(p.clone(), c.clone()))
            .collect())
    }
}

struct RejectEmitter;

impl Emitter<TestIr> for RejectEmitter {
    fn materialize(&self, ir: &TestIr, req: &EmitContext) -> Result<Vec<Change>, CodegenError> {
        TestEmitter.materialize(ir, req)
    }

    fn verify(&self, _ir: &TestIr, _changes: &[Change]) -> Result<(), CodegenError> {
        Err(CodegenError::Verify("honest decline".into()))
    }
}

#[test]
fn source_emitter_pipeline_shape() {
    let src = TestSource;
    let emitter = TestEmitter;
    let ctx = EmitContext::new().with_config("files", "3");
    let ir = src.parse(&ctx).unwrap();
    assert_eq!(ir.kind().as_str(), "test-ir");
    let changes = emitter.materialize(&ir, &ctx).unwrap();
    assert_eq!(changes.len(), 3);
    emitter.verify(&ir, &changes).unwrap();

    // Verify gate declines honestly.
    let bad = RejectEmitter;
    let err = bad.verify(&ir, &changes).unwrap_err();
    assert!(matches!(err, CodegenError::Verify(_)));
}

// ------------------------------------------------------------------
// Plan
// ------------------------------------------------------------------

#[test]
fn plan_classifies_create_update_delete_unchanged() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);

    // Pre-existing: "keep" matches desired (unchanged), "stale" differs
    // (update), "gone" is manifest-owned but not desired (delete).
    store.write(&aref("keep.txt"), b"same").unwrap();
    store.write(&aref("stale.txt"), b"old").unwrap();
    store.write(&aref("gone.txt"), b"delete me").unwrap();

    let prior = Manifest {
        target: "t".into(),
        generator_version: "v".into(),
        source_hashes: BTreeMap::new(),
        artifacts: vec![
            ManifestEntry {
                artifact: aref("keep.txt"),
                hash: ContentHash::of(b"same"),
            },
            ManifestEntry {
                artifact: aref("stale.txt"),
                hash: ContentHash::of(b"old"),
            },
            ManifestEntry {
                artifact: aref("gone.txt"),
                hash: ContentHash::of(b"delete me"),
            },
        ],
        run_id: "r0".into(),
    };

    let changes = desired(&[
        ("keep.txt", "same"),
        ("stale.txt", "new"),
        ("fresh.txt", "hi"),
    ]);
    let plan = engine.plan(&changes, Some(&prior)).unwrap();

    let action = |a: &str| {
        plan.entries
            .iter()
            .find(|e| e.artifact.as_str() == a)
            .map(|e| e.action)
            .unwrap()
    };
    assert_eq!(action("keep.txt"), PlanAction::Unchanged);
    assert_eq!(action("stale.txt"), PlanAction::Update);
    assert_eq!(action("fresh.txt"), PlanAction::Create);
    assert_eq!(action("gone.txt"), PlanAction::Delete);
    // Sorted by ArtifactRef.
    let keys: Vec<&str> = plan.entries.iter().map(|e| e.artifact.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn plan_rejects_reserved_and_duplicate_refs() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    let bad = vec![Change::text(".handl/codegen/evil", "x")];
    assert!(matches!(
        engine.plan(&bad, None),
        Err(CodegenError::InvalidRef(_))
    ));
    let dup = vec![Change::text("a", "1"), Change::text("a", "2")];
    assert!(matches!(
        engine.plan(&dup, None),
        Err(CodegenError::InvalidRef(_))
    ));
}

#[test]
fn plan_fails_closed_on_before_hash_lie() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("x"), b"real").unwrap();
    let c = Change::put_with_before(
        "x",
        Some(ContentHash::of(b"not-the-store-content")),
        b"desired".to_vec(),
    );
    let err = engine.plan(&[c], None).unwrap_err();
    assert!(matches!(err, CodegenError::Drift { .. }));
}

#[test]
fn plan_is_order_independent_and_sorted() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    // Mixed store state so every action class appears.
    store.write(&aref("same.txt"), b"S").unwrap();
    store.write(&aref("diff.txt"), b"old").unwrap();
    store.write(&aref("owned.txt"), b"owned").unwrap();
    let prior = Manifest {
        target: "t".into(),
        generator_version: "v".into(),
        source_hashes: BTreeMap::new(),
        artifacts: vec![ManifestEntry {
            artifact: aref("owned.txt"),
            hash: ContentHash::of(b"owned"),
        }],
        run_id: "r0".into(),
    };

    let base = desired(&[
        ("z-new.txt", "Z"),
        ("same.txt", "S"),
        ("diff.txt", "D"),
        ("a-new.txt", "A"),
    ]);
    let p1 = engine.plan(&base, Some(&prior)).unwrap();

    // Every permutation of the input yields the identical Plan —
    // normalization sorts by ArtifactRef before diffing.
    let orders: Vec<Vec<Change>> = vec![
        base.iter().rev().cloned().collect(),
        vec![
            base[2].clone(),
            base[0].clone(),
            base[3].clone(),
            base[1].clone(),
        ],
        vec![
            base[1].clone(),
            base[3].clone(),
            base[0].clone(),
            base[2].clone(),
        ],
    ];
    for (i, changes) in orders.iter().enumerate() {
        let p = engine.plan(changes, Some(&prior)).unwrap();
        assert_eq!(p, p1, "permutation {i} produced a different plan");
    }
    // Entries sorted by ArtifactRef.
    let keys: Vec<&str> = p1.entries.iter().map(|e| e.artifact.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

// ------------------------------------------------------------------
// Apply
// ------------------------------------------------------------------

#[test]
fn apply_writes_manifest_and_converges() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    let changes = desired(&[("b.txt", "B"), ("a.txt", "A")]);

    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();
    assert!(report.clean(), "failures: {:?}", report.failures);
    assert_eq!(report.created, 2);
    assert!(report.manifest_written);

    // Artifacts present with desired bytes.
    assert_eq!(store.read(&aref("a.txt")).unwrap().unwrap(), b"A");
    assert_eq!(store.read(&aref("b.txt")).unwrap().unwrap(), b"B");

    // Manifest recorded, sorted, with hashes.
    let mref = Manifest::artifact_ref("gen");
    let mbytes = store.read(&mref).unwrap().unwrap();
    let manifest: Manifest = serde_json::from_slice(&mbytes).unwrap();
    assert_eq!(manifest.target, "gen");
    assert_eq!(manifest.generator_version, "test-gen-0.1");
    assert_eq!(manifest.run_id, "run-gen");
    assert_eq!(manifest.artifacts.len(), 2);
    assert_eq!(manifest.artifacts[0].artifact.as_str(), "a.txt");
    assert_eq!(manifest.artifacts[0].hash, ContentHash::of(b"A"));

    // Journal exists with write-ahead records (2 artifacts + manifest).
    let journal = Journal::open(&store, "run-gen");
    let recs = journal.records().unwrap();
    assert_eq!(recs.len(), 3);
    // All before_hashes None (fresh store).
    assert!(recs.iter().all(|r| r.before_hash.is_none()));

    // Re-plan against own manifest: clean.
    let plan = engine.plan(&changes, Some(&manifest)).unwrap();
    assert!(plan.is_clean());
}

#[test]
fn apply_updates_and_deletes_via_manifest() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);

    // First run owns a.txt + old.txt.
    engine
        .apply(
            &desired(&[("a.txt", "A"), ("old.txt", "O")]),
            None,
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    let prior: Manifest =
        serde_json::from_slice(&store.read(&Manifest::artifact_ref("gen")).unwrap().unwrap())
            .unwrap();

    // Second run: a.txt changes, old.txt dropped, new.txt added.
    // Unowned pre-existing file must survive.
    store.write(&aref("unowned.txt"), b"keep me").unwrap();
    let changes = desired(&[("a.txt", "A2"), ("new.txt", "N")]);
    let report = engine
        .apply(
            &changes,
            Some(&prior),
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    assert!(report.clean(), "{:?}", report.failures);
    assert_eq!(report.updated, 1);
    assert_eq!(report.created, 1);
    assert_eq!(report.deleted, 1);

    assert_eq!(store.read(&aref("a.txt")).unwrap().unwrap(), b"A2");
    assert!(store.read(&aref("old.txt")).unwrap().is_none());
    assert_eq!(store.read(&aref("new.txt")).unwrap().unwrap(), b"N");
    assert_eq!(
        store.read(&aref("unowned.txt")).unwrap().unwrap(),
        b"keep me"
    );
}

#[test]
fn apply_fails_closed_on_drift() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("x.txt"), b"original").unwrap();

    // Plan while store says "original".
    let changes = desired(&[("x.txt", "v2")]);
    let plan = engine.plan(&changes, None).unwrap();
    assert_eq!(plan.entries[0].action, PlanAction::Update);

    // Drift: someone else writes between plan and apply.
    store.write(&aref("x.txt"), b"interloper").unwrap();

    // apply() re-plans internally, so to exercise the drift path we
    // call the internal check via a stale Change.before_hash instead:
    // simulate by planning, drifting, then applying the *stale plan*
    // is not the public API — public apply re-plans. The drift guard
    // that matters publicly is Change.before_hash (tested above) and
    // post-write verify. Here verify post-write integrity instead:
    // apply normally (fresh plan sees "interloper" → Update).
    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();
    assert!(report.clean());
    assert_eq!(store.read(&aref("x.txt")).unwrap().unwrap(), b"v2");

    // True drift refusal: a Change claiming a before_hash that the
    // store contradicts fails at plan inside apply.
    let lying = Change::put_with_before(
        "y.txt",
        Some(ContentHash::of(b"never-existed")),
        b"why".to_vec(),
    );
    let res = engine.apply(&[lying], None, &meta("gen"), &ApplyOptions::default());
    // y.txt absent → before_hash Some(...) vs cur None → drift error
    // surfaces from compute_plan as Err.
    assert!(matches!(res, Err(CodegenError::Drift { .. })));
}

// ------------------------------------------------------------------
// Resume / keep-going / on_error
// ------------------------------------------------------------------

#[test]
fn resume_skips_converged_artifacts() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    let changes = desired(&[("a.txt", "A"), ("b.txt", "B")]);

    engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();

    // Second apply: everything converged → plan is clean, no
    // mutations.
    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::resume())
        .unwrap();
    assert!(report.clean());
    assert_eq!(report.unchanged, 2);
    assert_eq!(report.created + report.updated + report.deleted, 0);
}

#[test]
fn resume_skips_already_absent_deletes() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    // Prior manifest claims "ghost.txt" which is already absent —
    // plan says Delete, resume skips it via hash check.
    let prior = Manifest {
        target: "g".into(),
        generator_version: "v".into(),
        source_hashes: BTreeMap::new(),
        artifacts: vec![ManifestEntry {
            artifact: aref("ghost.txt"),
            hash: ContentHash::of(b"was here"),
        }],
        run_id: "r".into(),
    };
    let report = engine
        .apply(&[], Some(&prior), &meta("g"), &ApplyOptions::resume())
        .unwrap();
    assert!(report.clean());
    assert_eq!(report.skipped, 1);
    assert_eq!(report.deleted, 0);
}

/// Store wrapper that fails writes to artifacts whose name contains
/// "bad" — exercises keep-going/on_error policies.
struct FailStore {
    inner: MemoryStore,
}

impl ArtifactStore for FailStore {
    fn read(&self, r: &ArtifactRef) -> Result<Option<Vec<u8>>, hnl_tool_codegen::StoreError> {
        self.inner.read(r)
    }
    fn write(&self, r: &ArtifactRef, c: &[u8]) -> Result<(), hnl_tool_codegen::StoreError> {
        if r.as_str().contains("bad") {
            return Err(hnl_tool_codegen::StoreError::Unsupported(format!(
                "injected failure for {r}"
            )));
        }
        self.inner.write(r, c)
    }
    fn delete(&self, r: &ArtifactRef) -> Result<(), hnl_tool_codegen::StoreError> {
        self.inner.delete(r)
    }
    fn hash(&self, r: &ArtifactRef) -> Result<Option<ContentHash>, hnl_tool_codegen::StoreError> {
        self.inner.hash(r)
    }
    fn stat(
        &self,
        r: &ArtifactRef,
    ) -> Result<Option<hnl_tool_codegen::ArtifactStat>, hnl_tool_codegen::StoreError> {
        self.inner.stat(r)
    }
}

#[test]
fn keep_going_collects_failures_default_aborts() {
    // Default (fail): first failure aborts — but "bad" sorts early.
    let store = FailStore {
        inner: MemoryStore::new(),
    };
    let engine = Engine::new(&store);
    let changes = desired(&[("bad-1.txt", "x"), ("good.txt", "y")]);
    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();
    assert!(report.aborted);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].0.as_str(), "bad-1.txt");
    // Manifest NOT written on failure.
    assert!(!report.manifest_written);

    // keep_going: failure recorded, good artifact still written.
    let store2 = FailStore {
        inner: MemoryStore::new(),
    };
    let engine2 = Engine::new(&store2);
    let report2 = engine2
        .apply(&changes, None, &meta("gen"), &ApplyOptions::keep_going())
        .unwrap();
    assert!(!report2.aborted);
    assert_eq!(report2.failures.len(), 1);
    assert_eq!(report2.created, 1);
    assert_eq!(store2.inner.read(&aref("good.txt")).unwrap().unwrap(), b"y");
    assert!(!report2.manifest_written);
}

#[test]
fn on_error_skip_continues() {
    let store = FailStore {
        inner: MemoryStore::new(),
    };
    let engine = Engine::new(&store);
    let changes = desired(&[("bad-1.txt", "x"), ("ok.txt", "y")]);
    let opts = ApplyOptions {
        on_error: OnError::Skip,
        ..ApplyOptions::default()
    };
    let report = engine.apply(&changes, None, &meta("gen"), &opts).unwrap();
    assert!(!report.aborted);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.created, 1);
}

// ------------------------------------------------------------------
// Destroy
// ------------------------------------------------------------------

#[test]
fn destroy_removes_only_manifest_artifacts() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    engine
        .apply(
            &desired(&[("owned.txt", "O")]),
            None,
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    // Unowned artifact appears after apply.
    store.write(&aref("foreign.txt"), b"not ours").unwrap();

    let manifest: Manifest =
        serde_json::from_slice(&store.read(&Manifest::artifact_ref("gen")).unwrap().unwrap())
            .unwrap();
    let report = engine.destroy(&manifest, &ApplyOptions::default()).unwrap();
    assert!(report.clean(), "{:?}", report.failures);
    assert_eq!(report.deleted, 2); // owned.txt + manifest itself

    assert!(store.read(&aref("owned.txt")).unwrap().is_none());
    assert!(
        store
            .read(&Manifest::artifact_ref("gen"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.read(&aref("foreign.txt")).unwrap().unwrap(),
        b"not ours"
    );
}

// ------------------------------------------------------------------
// Revert
// ------------------------------------------------------------------

#[test]
fn revert_restores_before_images() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("a.txt"), b"before-A").unwrap();
    store.write(&aref("b.txt"), b"before-B").unwrap();

    // Apply updates a.txt, creates c.txt, deletes nothing (no prior).
    let changes = desired(&[
        ("a.txt", "after-A"),
        ("b.txt", "after-B"),
        ("c.txt", "new-C"),
    ]);
    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();
    assert!(report.clean());
    assert_eq!(store.read(&aref("a.txt")).unwrap().unwrap(), b"after-A");

    let rev = engine.revert("run-gen", &ApplyOptions::default()).unwrap();
    assert!(rev.failures.is_empty(), "{:?}", rev.failures);
    // a.txt and b.txt restored; c.txt removed (was absent before).
    assert_eq!(store.read(&aref("a.txt")).unwrap().unwrap(), b"before-A");
    assert_eq!(store.read(&aref("b.txt")).unwrap().unwrap(), b"before-B");
    assert!(store.read(&aref("c.txt")).unwrap().is_none());
    // Manifest write was also reverted to its before-state (absent).
    assert!(
        store
            .read(&Manifest::artifact_ref("gen"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn revert_refuses_on_drift() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("a.txt"), b"before").unwrap();
    engine
        .apply(
            &desired(&[("a.txt", "after")]),
            None,
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    // Someone edits the artifact after apply.
    store.write(&aref("a.txt"), b"tampered").unwrap();

    let rev = engine.revert("run-gen", &ApplyOptions::default()).unwrap();
    assert!(rev.aborted);
    assert_eq!(rev.failures.len(), 1);
    // Artifact left untouched (fail-closed).
    assert_eq!(store.read(&aref("a.txt")).unwrap().unwrap(), b"tampered");
}

// ------------------------------------------------------------------
// Crash / truncated journal (FsStore)
// ------------------------------------------------------------------

#[test]
fn truncated_journal_still_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FsStore::new(tmp.path());
    let engine = Engine::new(&store);
    let changes = desired(&[("a.txt", "A"), ("b.txt", "B")]);
    engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();

    // Simulate crash mid-append: corrupt the journal tail.
    let jref = journal_ref("run-gen");
    let jpath = tmp.path().join(jref.as_str());
    let mut bytes = std::fs::read(&jpath).unwrap();
    // Truncate inside the last line + append a partial line.
    let cut = bytes.len() - 10;
    bytes.truncate(cut);
    bytes.extend_from_slice(b"{\"v\":1,\"run_id\":\"run-gen\",\"seq\":9,\"artif");
    std::fs::write(&jpath, &bytes).unwrap();

    // Lenient read skips the truncated tail; strict read fails.
    let journal = Journal::open(&store, "run-gen");
    let lenient = journal.records().unwrap();
    assert!(lenient.len() >= 2);
    assert!(journal.records_strict().is_err());

    // Resume apply converges cleanly despite the damaged tail.
    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::resume())
        .unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
}

#[test]
fn fs_store_manifest_and_journal_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FsStore::new(tmp.path());
    let engine = Engine::new(&store);
    engine
        .apply(
            &desired(&[("gen/out.txt", "O")]),
            None,
            &meta("t"),
            &ApplyOptions::default(),
        )
        .unwrap();

    // Manifest + journal + before-image blobs live under the reserved prefix.
    let manifest_path = tmp.path().join(".handl/codegen/manifest/t.json");
    assert!(manifest_path.exists());
    let journal_path = tmp.path().join(".handl/codegen/journal/run-t.jsonl");
    assert!(journal_path.exists());
    // JSONL: every complete line parses.
    let text = std::fs::read_to_string(&journal_path).unwrap();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["v"], 1);
    }
    // Atomic writes left no temp files.
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join(".handl/codegen"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("codegen-tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn fs_store_hash_memo_does_not_mask_external_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FsStore::new(tmp.path());
    let engine = Engine::new(&store);
    let changes = desired(&[("out.txt", "generated")]);
    engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();

    // Warm the memo: plan once (all Unchanged).
    let manifest: Manifest =
        serde_json::from_slice(&store.read(&Manifest::artifact_ref("gen")).unwrap().unwrap())
            .unwrap();
    let plan = engine.plan(&changes, Some(&manifest)).unwrap();
    assert!(plan.is_clean());

    // External edit (different size) — the stat pre-filter must miss
    // and the plan must see the drift as an Update, not Unchanged.
    std::fs::write(tmp.path().join("out.txt"), b"hand-edited-longer").unwrap();
    let plan = engine.plan(&changes, Some(&manifest)).unwrap();
    let entry = plan
        .entries
        .iter()
        .find(|e| e.artifact.as_str() == "out.txt")
        .unwrap();
    assert_eq!(entry.action, PlanAction::Update);

    // Re-apply converges back to desired through the same store.
    let report = engine
        .apply(
            &changes,
            Some(&manifest),
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    assert!(report.clean(), "{:?}", report.failures);
    assert_eq!(report.updated, 1);
    assert_eq!(
        std::fs::read(tmp.path().join("out.txt")).unwrap(),
        b"generated"
    );
}

// ------------------------------------------------------------------
// Property tests (deterministic LCG — no proptest dep)
// ------------------------------------------------------------------

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// apply∘plan == desired and destroy∘apply == original, over
/// generated inputs.
#[test]
fn property_apply_plan_and_destroy_roundtrip() {
    let mut rng = Lcg(0xC0DE);
    for case in 0..50 {
        let store = MemoryStore::new();
        let engine = Engine::new(&store);
        let target = format!("prop-{case}");

        // Random desired set: 0..6 artifacts, random names/contents.
        let n = rng.below(7);
        let mut pairs: Vec<(String, String)> = Vec::new();
        for i in 0..n {
            let name = format!("gen/{case}-{i}-{}.txt", rng.below(1000));
            let content = format!("payload-{}-{}", case, rng.below(1 << 20));
            pairs.push((name, content));
        }
        // Sort + dedup names for a valid Change set.
        pairs.sort();
        pairs.dedup_by(|a, b| a.0 == b.0);
        let changes: Vec<Change> = pairs
            .iter()
            .map(|(p, c)| Change::text(p.clone(), c.clone()))
            .collect();

        // Snapshot "original" user state (empty store here; also seed
        // a foreign artifact that must survive destroy).
        store.write(&aref("foreign/keep.txt"), b"foreign").unwrap();
        let original: BTreeMap<ArtifactRef, Vec<u8>> = store
            .snapshot()
            .into_iter()
            .filter(|(k, _)| !k.is_reserved())
            .collect();

        // apply∘plan == desired
        let report = engine
            .apply(
                &changes,
                None,
                &RunMeta::new(&target, "prop"),
                &ApplyOptions::default(),
            )
            .unwrap();
        assert!(report.clean(), "case {case}: {:?}", report.failures);
        for c in &changes {
            assert_eq!(
                store.read(c.artifact()).unwrap().unwrap(),
                c.content().unwrap(),
                "case {case}: {}",
                c.artifact()
            );
        }

        // destroy∘apply == original (user artifacts only)
        let manifest: Manifest = serde_json::from_slice(
            &store
                .read(&Manifest::artifact_ref(&target))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let drep = engine.destroy(&manifest, &ApplyOptions::default()).unwrap();
        assert!(drep.clean(), "case {case}: {:?}", drep.failures);
        let final_state: BTreeMap<ArtifactRef, Vec<u8>> = store
            .snapshot()
            .into_iter()
            .filter(|(k, _)| !k.is_reserved())
            .collect();
        assert_eq!(final_state, original, "case {case}");
    }
}

/// Re-apply idempotence: second apply is a no-op plan.
#[test]
fn property_reapply_is_idempotent() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    let changes = desired(&[("x", "1"), ("y", "2")]);
    engine
        .apply(&changes, None, &meta("g"), &ApplyOptions::default())
        .unwrap();
    let manifest: Manifest =
        serde_json::from_slice(&store.read(&Manifest::artifact_ref("g")).unwrap().unwrap())
            .unwrap();
    let plan = engine.plan(&changes, Some(&manifest)).unwrap();
    assert!(plan.is_clean());
    // Applying again creates/updates/deletes nothing.
    let report = engine
        .apply(
            &changes,
            Some(&manifest),
            &meta("g"),
            &ApplyOptions::default(),
        )
        .unwrap();
    assert_eq!(report.created + report.updated + report.deleted, 0);
}

// ------------------------------------------------------------------
// Change::Delete — emitter-declared stale-artifact cleanup
// ------------------------------------------------------------------

#[test]
fn plan_shows_delete_for_existing_artifact() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("stale.txt"), b"old").unwrap();

    let changes = vec![Change::delete("stale.txt")];
    let plan = engine.plan(&changes, None).unwrap();
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(plan.entries[0].action, PlanAction::Delete);
    assert_eq!(plan.entries[0].before_hash, Some(ContentHash::of(b"old")));
    assert_eq!(plan.entries[0].after_hash, None);
}

#[test]
fn delete_of_missing_artifact_is_noop() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);

    let changes = vec![Change::delete("ghost.txt")];
    let plan = engine.plan(&changes, None).unwrap();
    // Absent → Unchanged no-op, not an error and not a Delete.
    assert_eq!(plan.entries[0].action, PlanAction::Unchanged);
    assert!(plan.is_clean());

    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();
    assert!(report.clean(), "{:?}", report.failures);
    assert_eq!(report.deleted, 0);
    assert_eq!(report.unchanged, 1);
}

#[test]
fn apply_delete_then_revert_restores() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("stale.txt"), b"precious-old").unwrap();

    // Emitter declares: write fresh.txt, delete stale.txt.
    let changes = vec![
        Change::text("fresh.txt", "new"),
        Change::delete("stale.txt"),
    ];
    let report = engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();
    assert!(report.clean(), "{:?}", report.failures);
    assert_eq!(report.deleted, 1);
    assert!(store.read(&aref("stale.txt")).unwrap().is_none());
    assert_eq!(store.read(&aref("fresh.txt")).unwrap().unwrap(), b"new");

    // Journal carried the before-image: revert restores stale.txt.
    let rev = engine.revert("run-gen", &ApplyOptions::default()).unwrap();
    assert!(rev.failures.is_empty(), "{:?}", rev.failures);
    assert_eq!(
        store.read(&aref("stale.txt")).unwrap().unwrap(),
        b"precious-old"
    );
    // fresh.txt was a create → revert removes it.
    assert!(store.read(&aref("fresh.txt")).unwrap().is_none());
}

#[test]
fn delete_changes_never_enter_manifest() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("old.txt"), b"stale").unwrap();

    let changes = vec![Change::text("new.txt", "N"), Change::delete("old.txt")];
    engine
        .apply(&changes, None, &meta("gen"), &ApplyOptions::default())
        .unwrap();

    let manifest: Manifest =
        serde_json::from_slice(&store.read(&Manifest::artifact_ref("gen")).unwrap().unwrap())
            .unwrap();
    // Only the Put artifact is owned; the deleted one is absent.
    assert_eq!(manifest.artifacts.len(), 1);
    assert_eq!(manifest.artifacts[0].artifact.as_str(), "new.txt");
}

#[test]
fn emitter_delete_of_manifest_artifact_dedupes_with_manifest_delete() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);

    // Run 1 owns keep.txt + drop.txt.
    engine
        .apply(
            &desired(&[("keep.txt", "K"), ("drop.txt", "D")]),
            None,
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    let prior: Manifest =
        serde_json::from_slice(&store.read(&Manifest::artifact_ref("gen")).unwrap().unwrap())
            .unwrap();

    // Run 2: emitter explicitly deletes drop.txt (also manifest-owned)
    // and keeps keep.txt unchanged.
    let changes = vec![Change::text("keep.txt", "K"), Change::delete("drop.txt")];
    let plan = engine.plan(&changes, Some(&prior)).unwrap();
    // Exactly ONE delete entry — the manifest diff must not double it.
    assert_eq!(plan.deletes().count(), 1);
    assert_eq!(plan.deletes().next().unwrap().artifact.as_str(), "drop.txt");

    let report = engine
        .apply(
            &changes,
            Some(&prior),
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();
    assert!(report.clean(), "{:?}", report.failures);
    assert_eq!(report.deleted, 1);
    assert!(store.read(&aref("drop.txt")).unwrap().is_none());
}

#[test]
fn put_and_delete_same_artifact_is_rejected() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    let changes = vec![Change::text("x", "1"), Change::delete("x")];
    assert!(matches!(
        engine.plan(&changes, None),
        Err(CodegenError::InvalidRef(_))
    ));
}

#[test]
fn delete_change_journaled_with_before_image() {
    let store = MemoryStore::new();
    let engine = Engine::new(&store);
    store.write(&aref("victim.txt"), b"doomed").unwrap();

    engine
        .apply(
            &[Change::delete("victim.txt")],
            None,
            &meta("gen"),
            &ApplyOptions::default(),
        )
        .unwrap();

    // The delete was journaled as op=delete WITH a before-image blob.
    let journal = Journal::open(&store, "run-gen");
    let recs = journal.records().unwrap();
    let del = recs
        .iter()
        .find(|r| r.artifact.as_str() == "victim.txt")
        .expect("delete record");
    assert_eq!(del.op, hnl_tool_codegen::JournalOp::Delete);
    assert_eq!(del.before_hash, Some(ContentHash::of(b"doomed")));
    assert!(del.before_blob.is_some());
    // Blob actually stored.
    let blob = del.before_blob.clone().unwrap();
    assert_eq!(store.read(&blob).unwrap().unwrap(), b"doomed");
}
