//! Store impl tests: `MemoryStore` unit tests + `FsStore`/`tempfile`
//! integration tests (atomic write, on-disk layout, ref validation).

use hnl_engine_codegen::types::{ArtifactRef, ArtifactStat, ContentHash};
use hnl_engine_codegen::{ArtifactStore, FsStore, MemoryStore, StoreError};

fn aref(s: impl Into<String>) -> ArtifactRef {
    ArtifactRef::new(s)
}

// ------------------------------------------------------------------
// MemoryStore
// ------------------------------------------------------------------

#[test]
fn memory_store_roundtrip() {
    let s = MemoryStore::new();
    let r = aref("a/b.txt");
    assert!(s.read(&r).unwrap().is_none());
    assert!(s.hash(&r).unwrap().is_none());
    assert!(s.stat(&r).unwrap().is_none());

    s.write(&r, b"hello").unwrap();
    assert_eq!(s.read(&r).unwrap().unwrap(), b"hello");
    assert_eq!(s.hash(&r).unwrap().unwrap(), ContentHash::of(b"hello"));
    let st = s.stat(&r).unwrap().unwrap();
    assert_eq!(st.size, 5);

    // Overwrite.
    s.write(&r, b"world!!").unwrap();
    assert_eq!(s.read(&r).unwrap().unwrap(), b"world!!");

    // Delete is idempotent.
    s.delete(&r).unwrap();
    s.delete(&r).unwrap();
    assert!(s.read(&r).unwrap().is_none());
}

#[test]
fn memory_store_snapshot_sorted() {
    let s = MemoryStore::new();
    s.write(&aref("z"), b"1").unwrap();
    s.write(&aref("a"), b"2").unwrap();
    s.write(&aref("m"), b"3").unwrap();
    let snap = s.snapshot();
    let keys: Vec<&ArtifactRef> = snap.keys().collect();
    assert_eq!(keys, vec![&aref("a"), &aref("m"), &aref("z")]);
    assert_eq!(s.len(), 3);
}

#[test]
fn memory_store_binary() {
    let s = MemoryStore::new();
    let bytes: Vec<u8> = (0u8..=255).collect();
    s.write(&aref("bin"), &bytes).unwrap();
    assert_eq!(s.read(&aref("bin")).unwrap().unwrap(), bytes);
}

// ------------------------------------------------------------------
// FsStore
// ------------------------------------------------------------------

#[test]
fn fs_store_roundtrip_and_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let s = FsStore::new(tmp.path());
    let r = aref("nested/deep/gen/file.hnl");

    s.write(&r, b"generated").unwrap();
    // File exists on disk at the expected path.
    assert_eq!(
        std::fs::read(tmp.path().join("nested/deep/gen/file.hnl")).unwrap(),
        b"generated"
    );
    assert_eq!(s.read(&r).unwrap().unwrap(), b"generated");
    assert_eq!(s.hash(&r).unwrap().unwrap(), ContentHash::of(b"generated"));

    let st = s.stat(&r).unwrap().unwrap();
    assert_eq!(st.size, 9);
    assert!(st.mtime.is_some());

    s.delete(&r).unwrap();
    s.delete(&r).unwrap(); // idempotent
    assert!(s.read(&r).unwrap().is_none());
    assert!(s.hash(&r).unwrap().is_none());
    assert!(s.stat(&r).unwrap().is_none());
}

#[test]
fn fs_store_atomic_write_no_tmp_leftovers() {
    let tmp = tempfile::tempdir().unwrap();
    let s = FsStore::new(tmp.path());
    for i in 0..20 {
        s.write(&aref(format!("f{i}.txt")), format!("v{i}").as_bytes())
            .unwrap();
    }
    // No temp files remain.
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("codegen-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    assert_eq!(s.read(&aref("f7.txt")).unwrap().unwrap(), b"v7");
}

#[test]
fn fs_store_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let s = FsStore::new(tmp.path());
    let r = aref("x.txt");
    s.write(&r, b"one").unwrap();
    s.write(&r, b"two-three").unwrap();
    assert_eq!(s.read(&r).unwrap().unwrap(), b"two-three");
}

#[test]
fn fs_store_rejects_bad_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let s = FsStore::new(tmp.path());
    for bad in ["../escape", "a/../../b", "/absolute", "", "."] {
        let r = aref(bad);
        let res = s.write(&r, b"x");
        assert!(res.is_err(), "expected error for {bad:?}");
        match res.unwrap_err() {
            StoreError::InvalidRef(_) => {}
            e => panic!("expected InvalidRef for {bad:?}, got {e}"),
        }
    }
    // Nothing escaped the root.
    assert!(!tmp.path().parent().unwrap().join("escape").exists());
}

#[test]
fn fs_store_hash_streams_large_file() {
    let tmp = tempfile::tempdir().unwrap();
    let s = FsStore::new(tmp.path());
    let big = vec![0xABu8; 2 * 1024 * 1024];
    s.write(&aref("big.bin"), &big).unwrap();
    assert_eq!(
        s.hash(&aref("big.bin")).unwrap().unwrap(),
        ContentHash::of(&big)
    );
}

#[test]
fn fs_store_stat_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let s = FsStore::new(tmp.path());
    s.write(&aref("a"), b"12345").unwrap();
    let st: ArtifactStat = s.stat(&aref("a")).unwrap().unwrap();
    assert_eq!(st.size, 5);
    // Missing artifact → None.
    assert!(s.stat(&aref("nope")).unwrap().is_none());
}

#[test]
fn memory_store_error_paths() {
    // Poisoned-lock path is hard to trigger without unsafe; just
    // confirm the trait surface works through &dyn.
    let s: Box<dyn ArtifactStore> = Box::new(MemoryStore::new());
    s.write(&aref("k"), b"v").unwrap();
    let mut buf = Vec::new();
    // read through trait object
    let bytes = s.read(&aref("k")).unwrap().unwrap();
    buf.extend_from_slice(&bytes);
    assert_eq!(buf, b"v");
}

#[test]
fn content_hash_of_reader_matches_of() {
    let data = b"stream me".repeat(1000);
    let a = ContentHash::of(&data);
    let b = ContentHash::of_reader(&data[..]).unwrap();
    assert_eq!(a, b);
    // And through an actual file.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("f");
    std::fs::write(&p, &data).unwrap();
    let f = std::fs::File::open(&p).unwrap();
    let c = ContentHash::of_reader(std::io::BufReader::new(f)).unwrap();
    assert_eq!(a, c);
}
