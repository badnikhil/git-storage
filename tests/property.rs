//! Property-style coverage: byte-identical roundtrips across boundary sizes,
//! deterministic dedup, and a many-files / checkpoint stress. These drive the
//! library `Engine` directly (fast, no subprocess) and complement the hostile-
//! input fuzzers in `tests/fuzz.rs` with the "good input, every shape" side.

use git_storage::crypto::Keys;
use git_storage::engine::Engine;
use tempfile::TempDir;

fn varied_bytes(len: usize, mut state: u64) -> Vec<u8> {
    // Seed must be non-zero (xorshift of 0 stays 0 → all-zero data).
    state |= 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn roundtrip(engine: &Engine, name: &str, data: &[u8]) {
    engine.put(name, data).unwrap();
    let mut out = Vec::new();
    let n = engine.get(name, &mut out, None).unwrap();
    assert_eq!(n as usize, data.len(), "reported size for {name}");
    assert_eq!(
        out,
        data,
        "roundtrip mismatch for {name} ({} bytes)",
        data.len()
    );
}

/// Every size around the chunker boundaries must round-trip byte-identically.
/// With avg 64 KiB the chunker uses min 32 KiB / max 256 KiB, so the interesting
/// points are 0, 1, min±1, avg, max±1, and non-aligned multi-MiB tails.
#[test]
fn roundtrip_across_boundary_sizes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let engine = Engine::open(&root, Keys::new([3u8; 32]), Some(64 * 1024)).unwrap();

    let k = 1024;
    let sizes = [
        0,
        1,
        2,
        4 * k,
        32 * k - 1,
        32 * k,
        32 * k + 1,
        64 * k,
        256 * k - 1,
        256 * k,
        256 * k + 1,
        1024 * k + 123,
        5 * 1024 * k + 777,
    ];
    for (i, &sz) in sizes.iter().enumerate() {
        let data = varied_bytes(sz, 0x1000 + i as u64);
        roundtrip(&engine, &format!("f{i}.bin"), &data);
    }
}

/// Dedup is deterministic and namespace-scoped: an identical re-put under the
/// same name is a no-op; the same content under a NEW name stores zero new
/// chunks but commits a new namespace entry.
#[test]
fn identical_data_dedups_deterministically() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let engine = Engine::open(&root, Keys::new([4u8; 32]), Some(64 * 1024)).unwrap();
    let data = varied_bytes(300 * 1024, 0xDED);

    let first = engine.put("x.bin", &data[..]).unwrap();
    assert!(first.new_chunks > 0 && first.committed);

    // Same name, same bytes → nothing to do.
    let again = engine.put("x.bin", &data[..]).unwrap();
    assert_eq!(again.new_chunks, 0);
    assert!(!again.committed, "identical re-put must be a no-op");

    // New name, same bytes → all chunks dedup, but a new entry is committed.
    let other = engine.put("y.bin", &data[..]).unwrap();
    assert_eq!(other.new_chunks, 0, "same content, new name → full dedup");
    assert!(other.committed, "a new namespace entry must commit");

    // Both names reconstruct identically.
    for name in ["x.bin", "y.bin"] {
        let mut out = Vec::new();
        engine.get(name, &mut out, None).unwrap();
        assert_eq!(out, data);
    }
}

/// Many small files exercise the namespace + checkpoint machinery: every file
/// round-trips, the count is exact, and checkpoints actually get emitted.
#[test]
fn many_files_roundtrip_with_checkpoints() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let mut engine = Engine::open(&root, Keys::new([5u8; 32]), Some(64 * 1024)).unwrap();
    engine.set_checkpoint_interval(8);

    let n: u64 = 40;
    for i in 0..n {
        engine
            .put(&format!("f{i}.bin"), &varied_bytes(20 * 1024, 900 + i)[..])
            .unwrap();
    }

    let ns = engine.namespace_at(None).unwrap();
    assert_eq!(ns.len() as u64, n, "all files present");

    for i in [0u64, 7, 8, 39] {
        let mut out = Vec::new();
        engine.get(&format!("f{i}.bin"), &mut out, None).unwrap();
        assert_eq!(
            out,
            varied_bytes(20 * 1024, 900 + i),
            "f{i} must round-trip"
        );
    }

    let (checkpoints, _deltas) = engine.txn_kind_counts().unwrap();
    assert!(
        checkpoints >= 2,
        "40 puts at checkpoint interval 8 should emit several checkpoints, got {checkpoints}"
    );
}

/// A modified file (same name, new content) replaces cleanly and reads back the
/// NEW bytes, while an unrelated file is untouched.
#[test]
fn overwrite_replaces_content_cleanly() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let engine = Engine::open(&root, Keys::new([6u8; 32]), Some(64 * 1024)).unwrap();

    let v1 = varied_bytes(150 * 1024, 111);
    let v2 = varied_bytes(150 * 1024, 222);
    let other = varied_bytes(80 * 1024, 333);
    engine.put("doc.bin", &v1[..]).unwrap();
    engine.put("keep.bin", &other[..]).unwrap();
    engine.put("doc.bin", &v2[..]).unwrap(); // overwrite

    let mut got = Vec::new();
    engine.get("doc.bin", &mut got, None).unwrap();
    assert_eq!(got, v2, "overwritten file must read the new content");

    let mut keep = Vec::new();
    engine.get("keep.bin", &mut keep, None).unwrap();
    assert_eq!(keep, other, "the untouched file must be unchanged");
}
