//! M5 tests: logical delete (`rm`), hysteresis-gated compaction with
//! delete-only-after-CAS (including a crash matrix during compaction), the
//! orphan sweep with its safety window, the budget wall, and volume selection
//! (spare-slot exclusion). DESIGN.md Sections 12 and 15.
//!
//! Two styles, same isolation discipline as the rest of the suite:
//!   * Library-level tests drive the `Engine` API directly against multi-volume
//!     LOCAL stores (fast, deterministic; compaction gates left at DESIGN
//!     defaults so no process-global env is ever touched — `force` bypasses the
//!     pressure/interval gates without env overrides).
//!   * Subprocess tests spawn the CLI for the things that need a fresh process:
//!     crash injection (GITSTORAGE_CRASH) and per-invocation gate env vars set
//!     ONLY on the child. Multi-volume stores are declared over file:// remotes
//!     via `init`, so compaction is also exercised over the wire (RemoteBackend).
//!
//! GIT ISOLATION: HOME=tempdir, masked git config, no credential prompts, no
//! network — identical rules to roundtrip.rs / engine.rs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use git_storage::crypto::Keys;
use git_storage::engine::{self, Engine, VolumeConfig};

fn varied_bytes(len: usize, mut state: u64) -> Vec<u8> {
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

// ===================== library-level (multi-volume LOCAL) =====================

/// Build a fresh multi-volume LOCAL store with the given per-volume thresholds.
/// url=None makes each volume a `LocalBackend` bare repo under `volumes/<id>.git`
/// — the only way to declare a MULTI-volume store without going over the wire,
/// and exactly what exercises volume selection / compaction placement locally.
/// With N >= 3 volumes the last one is reserved as the compaction spare (Section 15.5).
fn open_multivol(root: &Path, thresholds: &[u64]) -> Engine {
    let volumes: Vec<VolumeConfig> = thresholds
        .iter()
        .enumerate()
        .map(|(i, t)| VolumeConfig {
            id: format!("v{i}"),
            url: None,
            push_interval_ms: 0,
            volume_full_threshold: *t,
        })
        .collect();
    engine::init_config_with_volumes(root, Some(64 * 1024), volumes, None).unwrap();
    Engine::open(root, Keys::new([7u8; 32]), None).unwrap()
}

const MIB: u64 = 1024 * 1024;

#[test]
fn rm_deletes_logically_and_get_fails() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    // A back-compat single-volume store is enough for rm semantics.
    let mut engine = Engine::open(&root, Keys::new([1u8; 32]), Some(64 * 1024)).unwrap();
    engine
        .put("a.bin", &varied_bytes(80 * 1024, 1)[..])
        .unwrap();
    engine
        .put("b.bin", &varied_bytes(80 * 1024, 2)[..])
        .unwrap();

    engine.remove("a.bin").unwrap();

    let ns = engine.namespace_at(None).unwrap();
    assert!(
        !ns.contains_key("a.bin"),
        "a.bin must be gone from namespace"
    );
    assert!(ns.contains_key("b.bin"), "b.bin must remain");

    // Reading the removed name fails cleanly; the survivor is byte-identical.
    let mut sink = Vec::new();
    assert!(
        engine.get("a.bin", &mut sink, None).is_err(),
        "get of a removed file must fail"
    );
    let mut out = Vec::new();
    engine.get("b.bin", &mut out, None).unwrap();
    assert_eq!(out, varied_bytes(80 * 1024, 2));
}

#[test]
fn rm_unknown_name_fails_cleanly_without_writing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let mut engine = Engine::open(&root, Keys::new([2u8; 32]), Some(64 * 1024)).unwrap();
    engine
        .put("real.bin", &varied_bytes(40 * 1024, 3)[..])
        .unwrap();
    let tip_before = engine.log_tip().unwrap();

    let err = engine.remove("ghost.bin").unwrap_err();
    assert!(
        format!("{err:#}").contains("no file"),
        "unexpected error: {err:#}"
    );
    // No transaction was appended for the failed removal.
    assert_eq!(
        engine.log_tip().unwrap(),
        tip_before,
        "a failed rm must not advance the log"
    );
}

#[test]
fn compaction_reclaims_dead_bytes_and_preserves_live() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    // v0 large (takes every write), v1 tiny (never accepts a full segment),
    // v2 = spare (compaction destination). This concentrates data on v0 so we
    // can drive its dead-ratio past the gate deterministically.
    let mut engine = open_multivol(&root, &[8 * MIB, 32 * 1024, 8 * MIB]);

    for i in 0..4u64 {
        engine
            .put(&format!("f{i}.bin"), &varied_bytes(200 * 1024, 10 + i)[..])
            .unwrap();
    }
    // Everything landed on v0, all live.
    let v0 = vol_stat(&engine, "v0");
    assert!(
        v0.total > 0 && v0.live == v0.total,
        "all data live on v0: {v0:?}"
    );

    // Remove three of four → v0 is ~75% dead.
    for i in 0..3u64 {
        engine.remove(&format!("f{i}.bin")).unwrap();
    }
    let v0 = vol_stat(&engine, "v0");
    assert!(
        v0.dead_ratio() > 0.5,
        "v0 should be >50% dead before compaction: {v0:?}"
    );

    // Force bypasses the pressure/interval gates; the dead-ratio gate (0.5)
    // still applies and v0 qualifies.
    let report = engine.compact(true).unwrap();
    assert_eq!(report.compacted.len(), 1, "exactly v0 compacts: {report:?}");
    assert_eq!(report.compacted[0].volume, "v0");
    assert_eq!(
        report.compacted[0].dest_volume, "v2",
        "compaction must target the spare"
    );

    // The one surviving file is still byte-identical, now served from v2.
    let mut out = Vec::new();
    engine.get("f3.bin", &mut out, None).unwrap();
    assert_eq!(out, varied_bytes(200 * 1024, 13));

    // v0 fully reclaimed; the spare now holds the live bytes; nothing dead.
    let v0 = vol_stat(&engine, "v0");
    let v2 = vol_stat(&engine, "v2");
    assert_eq!(v0.total, 0, "retired volume reclaimed to zero: {v0:?}");
    assert!(
        v2.total > 0 && v2.dead == 0,
        "spare holds only live bytes: {v2:?}"
    );

    let ns = engine.namespace_at(None).unwrap();
    assert_eq!(ns.len(), 1);
    assert!(ns.contains_key("f3.bin"));
}

#[test]
fn budget_wall_refuses_when_no_volume_fits() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    // A single small volume (100 KiB). N=1 → no spare.
    let engine = open_multivol(&root, &[100 * 1024]);

    // A small file fits.
    engine
        .put("small.bin", &varied_bytes(30 * 1024, 1)[..])
        .unwrap();

    // A file whose segment exceeds the threshold is refused — the budget wall.
    let err = engine
        .put("big.bin", &varied_bytes(200 * 1024, 2)[..])
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("budget exhausted"),
        "expected a budget-wall refusal, got: {msg}"
    );

    // The refusal happened before any segment was written: the store is intact
    // and the earlier file still reads back byte-identical.
    let ns = engine.namespace_at(None).unwrap();
    assert_eq!(ns.len(), 1, "only the accepted file is present");
    let mut out = Vec::new();
    engine.get("small.bin", &mut out, None).unwrap();
    assert_eq!(out, varied_bytes(30 * 1024, 1));
}

#[test]
fn volume_selection_spreads_across_writable_and_excludes_spare() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    // Three equal volumes; v2 is the spare (N >= 3).
    let engine = open_multivol(&root, &[8 * MIB, 8 * MIB, 8 * MIB]);

    engine
        .put("f0.bin", &varied_bytes(200 * 1024, 1)[..])
        .unwrap();
    engine
        .put("f1.bin", &varied_bytes(200 * 1024, 2)[..])
        .unwrap();

    // f0 → v0 (equal headroom, tie broken by lowest id); f1 → v1 (now the most
    // free writable volume). The spare is never chosen for ordinary writes.
    let ns = engine.namespace_at(None).unwrap();
    let vol_of = |name: &str| ns.get(name).unwrap().chunks[0].vol.clone();
    assert_eq!(vol_of("f0.bin"), "v0", "first write → v0");
    assert_eq!(
        vol_of("f1.bin"),
        "v1",
        "second write → most-free v1, not v0"
    );

    let v2 = vol_stat(&engine, "v2");
    assert!(v2.spare, "v2 must be the reserved spare");
    assert_eq!(v2.total, 0, "spare receives no ordinary writes");
}

#[test]
fn churn_guard_min_interval_blocks_immediate_recompaction() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    // v0 small (so a few files reach the pressure threshold), v1 tiny (unused),
    // v2 = spare with room for several compactions.
    let mut engine = open_multivol(&root, &[MIB, 32 * 1024, 8 * MIB]);

    // Round 1: fill v0 to high utilization, then delete a majority → pressured
    // AND dead. A non-force compaction fires (no prior compaction to gate it).
    fill_and_delete_majority(&mut engine, 0);
    let r1 = engine.compact(false).unwrap();
    assert_eq!(
        r1.compacted.len(),
        1,
        "round 1 should compact under pressure"
    );
    assert_eq!(engine.compaction_count().unwrap(), 1);

    // Round 2: recreate the same pressured+dead condition on the (reused) v0
    // slot. A non-force compaction must now be BLOCKED by the 24h min-interval
    // gate — the anti-churn hysteresis (DESIGN Section 12.4).
    fill_and_delete_majority(&mut engine, 100);
    let r2 = engine.compact(false).unwrap();
    assert!(
        r2.compacted.is_empty(),
        "min-interval gate must block a second immediate compaction: {r2:?}"
    );
    assert_eq!(
        engine.compaction_count().unwrap(),
        1,
        "no new compaction while the interval gate holds"
    );

    // Proving it was the interval gate (not a lack of candidates): forcing past
    // the gate compacts the still-dead volume.
    let r3 = engine.compact(true).unwrap();
    assert_eq!(r3.compacted.len(), 1, "force bypasses the interval gate");
    assert_eq!(engine.compaction_count().unwrap(), 2);
}

/// Fill v0 to high utilization with five 200 KiB files (unique seeds keyed by
/// `base` so rounds don't dedup), then remove three → >50% dead and pressured.
/// Seeds are always non-zero: `varied_bytes(_, 0)` is all-zero (xorshift of 0
/// stays 0), which zstd would crush to near-nothing and wreck the size math.
fn fill_and_delete_majority(engine: &mut Engine, base: u64) {
    for i in 0..5u64 {
        engine
            .put(
                &format!("g{base}_{i}.bin"),
                &varied_bytes(200 * 1024, base + i + 1)[..],
            )
            .unwrap();
    }
    for i in 0..3u64 {
        engine.remove(&format!("g{base}_{i}.bin")).unwrap();
    }
    let v0 = vol_stat(engine, "v0");
    assert!(
        v0.utilization() >= 0.80 && v0.dead_ratio() > 0.5,
        "setup should leave v0 pressured and mostly dead: {v0:?}"
    );
}

fn vol_stat(engine: &Engine, id: &str) -> git_storage::engine::VolumeStats {
    engine
        .stats()
        .unwrap()
        .into_iter()
        .find(|v| v.id == id)
        .unwrap_or_else(|| panic!("volume {id} not found"))
}

// ===================== subprocess (crash + gate env, over file://) ============

fn bin(sandbox_home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git-storage"));
    cmd.env("HOME", sandbox_home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GITSTORAGE_CRASH");
    cmd
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("spawning git-storage");
    assert!(
        out.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A multi-volume store declared over file:// remotes (RemoteBackend), so the
/// subprocess CLI exercises compaction over the wire.
struct RemoteStore {
    tmp: TempDir,
}

impl RemoteStore {
    fn init(n_vols: usize, threshold: u64) -> Self {
        let tmp = TempDir::new().unwrap();
        let remotes = tmp.path().join("remotes");
        fs::create_dir_all(&remotes).unwrap();
        let s = Self { tmp };
        let mut c = bin(s.home());
        c.args(["init", "--repo"])
            .arg(s.repo())
            .args(["--keyfile"])
            .arg(s.keyfile());
        for i in 0..n_vols {
            let url = format!("file://{}", remotes.join(format!("v{i}.git")).display());
            c.args(["--volume", &format!("v{i}={url}")]);
        }
        let idx = format!("file://{}", remotes.join("index.git").display());
        c.args(["--index-url", &idx])
            .args(["--threshold", &threshold.to_string()])
            .args(["--chunk-size", "64k"]);
        run_ok(&mut c);
        s
    }
    fn home(&self) -> &Path {
        self.tmp.path()
    }
    fn repo(&self) -> PathBuf {
        self.tmp.path().join("store")
    }
    fn keyfile(&self) -> PathBuf {
        self.tmp.path().join("master.key")
    }
    fn remote_git(&self, id: &str) -> PathBuf {
        self.tmp.path().join("remotes").join(format!("{id}.git"))
    }
    fn write_file(&self, name: &str, data: &[u8]) -> PathBuf {
        let p = self.tmp.path().join(name);
        fs::write(&p, data).unwrap();
        p
    }
    fn put(&self, name: &str, data: &[u8]) {
        let p = self.write_file(name, data);
        run_ok(
            bin(self.home())
                .args(["put"])
                .arg(&p)
                .args(["--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile())
                .args(["--chunk-size", "64k"]),
        );
    }
    fn rm(&self, name: &str) {
        run_ok(
            bin(self.home())
                .args(["rm", name, "--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        );
    }
    fn ls(&self) -> String {
        let out = run_ok(
            bin(self.home())
                .args(["ls", "--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    /// `get` a file and assert it is byte-identical to `expect`.
    fn assert_get(&self, name: &str, expect: &[u8]) {
        let out = self.tmp.path().join(format!("got-{name}"));
        run_ok(
            bin(self.home())
                .args(["get", name, "--output"])
                .arg(&out)
                .args(["--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        );
        assert_eq!(fs::read(&out).unwrap(), expect, "{name} must round-trip");
    }
    /// The current log tip commit (for pinning a snapshot).
    fn tip(&self) -> String {
        let out = run_ok(
            bin(self.home())
                .args(["tip", "--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    /// `get name --at <commit>`, returning the raw Output (may fail).
    fn get_at(&self, name: &str, at: &str) -> Output {
        let out = self.tmp.path().join(format!("at-{name}"));
        bin(self.home())
            .args(["get", name, "--output"])
            .arg(&out)
            .args(["--repo"])
            .arg(self.repo())
            .args(["--keyfile"])
            .arg(self.keyfile())
            .args(["--at", at])
            .output()
            .expect("spawning git-storage get --at")
    }
    /// Run `compact`, optionally forced, with per-invocation gate env overrides
    /// (set ONLY on the child process). Returns the Output (may be a crash).
    fn compact(&self, force: bool, crash: Option<&str>, env: &[(&str, &str)]) -> Output {
        let mut c = bin(self.home());
        c.args(["compact", "--repo"])
            .arg(self.repo())
            .args(["--keyfile"])
            .arg(self.keyfile());
        if force {
            c.arg("--force");
        }
        if let Some(point) = crash {
            c.env("GITSTORAGE_CRASH", point);
        }
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().expect("spawning git-storage compact")
    }
    /// Segment refs currently present on a volume's REMOTE bare repo.
    fn segment_refs(&self, id: &str) -> Vec<String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.remote_git(id))
            .args(["for-each-ref", "--format=%(refname)", "refs/segments/"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect()
    }
}

/// Exit code the crash hook uses (DESIGN Section 11).
const CRASH_EXIT: i32 = 97;

/// Delete-only-after-CAS crash matrix (DESIGN Section 12.3): killing compaction at any
/// phase boundary must leave the store fully readable, and a follow-up compact
/// converges to a clean, consistent state. Runs over file:// (RemoteBackend).
fn compaction_crash_body(point: &str) {
    let store = RemoteStore::init(3, 8 * MIB); // v0,v1 writable; v2 spare

    // 6 tiny files round-robin across v0/v1 (3 each); keep f4,f5 live so both
    // volumes end up >50% dead with live bytes worth moving.
    let payloads: Vec<Vec<u8>> = (0..6).map(|i| varied_bytes(16 * 1024, 500 + i)).collect();
    for (i, p) in payloads.iter().enumerate() {
        store.put(&format!("f{i}.bin"), p);
    }
    for i in 0..4 {
        store.rm(&format!("f{i}.bin"));
    }

    // Crash compaction at the named boundary.
    let out = store.compact(true, Some(point), &[]);
    assert_eq!(
        out.status.code(),
        Some(CRASH_EXIT),
        "expected a simulated crash at {point}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // INVARIANT: a fresh process sees a consistent store — the two live files
    // are present and byte-identical no matter where compaction died (served
    // from their old placement if the CAS hadn't landed, or the new one if it
    // had).
    let ls = store.ls();
    assert!(
        ls.contains("f4.bin") && ls.contains("f5.bin"),
        "live files must survive the crash at {point}: {ls}"
    );
    store.assert_get("f4.bin", &payloads[4]);
    store.assert_get("f5.bin", &payloads[5]);

    // Recovery: a forced compaction with a zero orphan window converges — the
    // rewrite is either completed or its orphan reclaimed, and no live data is
    // lost. Idempotent content-derived segment ids make the redo safe.
    let rec = store.compact(true, None, &[("GITSTORAGE_ORPHAN_WINDOW_SECS", "0")]);
    assert!(
        rec.status.success(),
        "recovery compaction failed after crash at {point}: {}",
        String::from_utf8_lossy(&rec.stderr)
    );

    // Still exactly {f4,f5}, still byte-identical.
    let ls = store.ls();
    assert!(ls.contains("f4.bin") && ls.contains("f5.bin"));
    assert!(
        !ls.contains("f0.bin") && !ls.contains("f3.bin"),
        "removed files must not reappear: {ls}"
    );
    store.assert_get("f4.bin", &payloads[4]);
    store.assert_get("f5.bin", &payloads[5]);
}

#[test]
fn compaction_crash_after_rewrite_is_recoverable() {
    compaction_crash_body("compact-after-rewrite");
}

#[test]
fn compaction_crash_before_cas_is_recoverable() {
    compaction_crash_body("compact-before-cas");
}

#[test]
fn compaction_crash_after_cas_is_recoverable() {
    compaction_crash_body("compact-after-cas");
}

#[test]
fn compaction_crash_before_delete_is_recoverable() {
    compaction_crash_body("compact-before-delete");
}

/// Orphan sweep safety window (DESIGN Section 12.5): a crash-orphaned segment (C2:
/// pushed but never committed to the log) is collectible only once older than
/// the window. A young orphan is left untouched; a zero window reclaims it.
#[test]
fn orphan_sweep_respects_safety_window() {
    let store = RemoteStore::init(1, 8 * MIB);

    // Manufacture a C2 orphan: a put that crashes after the segment ref is
    // pushed but before the log CAS. The segment is on the remote, unreferenced.
    let p = store.write_file("orphan.bin", &varied_bytes(32 * 1024, 900));
    let out = bin(store.home())
        .args(["put"])
        .arg(&p)
        .args(["--repo"])
        .arg(store.repo())
        .args(["--keyfile"])
        .arg(store.keyfile())
        .args(["--chunk-size", "64k"])
        .env("GITSTORAGE_CRASH", "after-segment")
        .output()
        .expect("spawning git-storage put");
    assert_eq!(out.status.code(), Some(CRASH_EXIT), "expected C2 crash");
    assert_eq!(
        store.segment_refs("v0").len(),
        1,
        "one orphan segment ref must exist on the remote"
    );

    // A huge window: the young orphan is NOT swept.
    let keep = store.compact(true, None, &[("GITSTORAGE_ORPHAN_WINDOW_SECS", "100000")]);
    assert!(keep.status.success());
    assert_eq!(
        store.segment_refs("v0").len(),
        1,
        "orphan younger than the window must be preserved (could be in-flight)"
    );

    // Zero window: the orphan is now collectible and gets swept.
    let sweep = store.compact(true, None, &[("GITSTORAGE_ORPHAN_WINDOW_SECS", "0")]);
    assert!(sweep.status.success());
    let stdout = String::from_utf8_lossy(&sweep.stdout);
    assert!(
        stdout.contains("swept 1 orphan"),
        "sweep should report reclaiming the orphan: {stdout}"
    );
    assert!(
        store.segment_refs("v0").is_empty(),
        "orphan segment must be gone after a zero-window sweep"
    );
}

// ===================== M5/M6 edge coverage (added) =============================

/// Liveness is namespace-wide: a chunk shared by several files counts ONCE and
/// stays live until the LAST referrer is removed (DESIGN Section 12.2). Two identical
/// files share every chunk; removing one leaves them all live, removing both
/// makes them all dead.
#[test]
fn shared_chunk_stays_live_until_all_referrers_removed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let mut engine = open_multivol(&root, &[8 * MIB, 32 * 1024, 8 * MIB]);
    let data = varied_bytes(200 * 1024, 55);

    engine.put("a.bin", &data[..]).unwrap();
    let after_a = vol_stat(&engine, "v0");
    assert!(after_a.total > 0 && after_a.dead == 0);

    // An identical file fully dedups: no new chunks, no new bytes, shared once.
    let s = engine.put("b.bin", &data[..]).unwrap();
    assert_eq!(s.new_chunks, 0, "identical content must fully dedup");
    let after_b = vol_stat(&engine, "v0");
    assert_eq!(
        after_b.total, after_a.total,
        "no new bytes for a shared file"
    );
    assert_eq!(after_b.live, after_a.live, "shared chunks counted once");
    assert_eq!(after_b.dead, 0);

    // Removing one referrer leaves the chunks live (b still references them).
    engine.remove("a.bin").unwrap();
    let after_rm_a = vol_stat(&engine, "v0");
    assert_eq!(after_rm_a.dead, 0, "chunks shared with b must stay live");
    assert_eq!(after_rm_a.live, after_a.live);

    // Removing the last referrer makes every shared chunk dead.
    engine.remove("b.bin").unwrap();
    let after_rm_b = vol_stat(&engine, "v0");
    assert_eq!(after_rm_b.live, 0, "no referrers left → all dead");
    assert_eq!(after_rm_b.dead, after_rm_b.total);
}

/// Forcing compaction when nothing is dead is a clean no-op: the dead-ratio gate
/// (which `force` never bypasses) blocks every volume, and the store is intact.
#[test]
fn compact_with_nothing_dead_is_a_clean_noop() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let mut engine = open_multivol(&root, &[8 * MIB, 32 * 1024, 8 * MIB]);
    for i in 0..3u64 {
        engine
            .put(&format!("f{i}.bin"), &varied_bytes(100 * 1024, 70 + i)[..])
            .unwrap();
    }
    let report = engine.compact(true).unwrap();
    assert!(
        report.compacted.is_empty(),
        "nothing is >50% dead, so force must still compact nothing: {report:?}"
    );
    assert_eq!(report.orphans_swept, 0);

    // Everything still reads back.
    let mut out = Vec::new();
    engine.get("f1.bin", &mut out, None).unwrap();
    assert_eq!(out, varied_bytes(100 * 1024, 71));
}

/// KNOWN LIMITATION, locked as a SAFETY guarantee (DESIGN Section 13.3 vs Section 12): a
/// snapshot pinned BEFORE a compaction that retired the volumes it references is
/// no longer readable — compaction reclaims old versions. The guarantee we DO
/// make is that such a read fails LOUDLY (missing segment/chunk), never returns
/// silent wrong data. See DESIGN.md open problem (compaction is not
/// snapshot-aware).
#[test]
fn snapshot_read_after_compaction_fails_loudly_never_silently() {
    let store = RemoteStore::init(3, 8 * MIB); // v0,v1 writable; v2 spare
    let payloads: Vec<Vec<u8>> = (0..4).map(|i| varied_bytes(16 * 1024, 700 + i)).collect();
    for (i, p) in payloads.iter().enumerate() {
        store.put(&format!("f{i}.bin"), p);
    }
    // Pin the tip BEFORE any deletion/compaction: a consistent snapshot of all 4.
    let pin = store.tip();
    assert!(
        store.get_at("f0.bin", &pin).status.success(),
        "snapshot read must work BEFORE compaction"
    );

    // Delete most and compact: live data moves to the spare, the source volumes
    // are retired and destroyed (window 0 sweeps any leftover).
    for i in 0..3 {
        store.rm(&format!("f{i}.bin"));
    }
    let out = store.compact(true, None, &[("GITSTORAGE_ORPHAN_WINDOW_SECS", "0")]);
    assert!(out.status.success(), "compaction itself must succeed");

    // The current tip is fine (surviving file repointed to the spare)…
    store.assert_get("f3.bin", &payloads[3]);

    // …but reading at the OLD pinned snapshot references now-destroyed segments.
    // It MUST fail loudly, never hand back wrong bytes.
    let res = store.get_at("f0.bin", &pin);
    assert!(
        !res.status.success(),
        "a snapshot read of compacted-away data must fail loudly, not silently"
    );
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("missing") || stderr.contains("couldn't find") || stderr.contains("chunk"),
        "the failure must point at the missing data, got: {stderr}"
    );
}
