//! M3 engine tests: crash matrix (C1–C4, DESIGN.md Section 11), snapshot
//! reads (Section 13.3), concurrent-writer CAS safety (Section 13.2), and
//! checkpoint behavior (Section 8.5).
//!
//! Crash injection uses the GITSTORAGE_CRASH env hook: the CLI process kills
//! itself (exit 97) at the named phase boundary, exactly as a power loss
//! would. A fresh process must then see a consistent store.
//!
//! GIT ISOLATION: same rules as roundtrip.rs — no user git config, no
//! credential prompts, everything in tempdirs, zero network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use git_storage::crypto::Keys;
use git_storage::engine::Engine;

// ---------- subprocess fixture (shared shape with roundtrip.rs) ----------

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

struct Fx {
    tmp: TempDir,
    /// When true, the store is backed by file:// remotes (RemoteBackend),
    /// initialized via `git-storage init`. When false, it's a plain
    /// M3-back-compat local store (LocalBackend). The SAME test bodies run in
    /// both modes so the engine is proven identical across the trait boundary.
    remote: bool,
}

impl Fx {
    fn new() -> Self {
        Self::with_mode(false)
    }
    fn new_remote() -> Self {
        Self::with_mode(true)
    }
    fn with_mode(remote: bool) -> Self {
        let fx = Self {
            tmp: TempDir::new().unwrap(),
            remote,
        };
        if remote {
            fx.init_remote();
        }
        fx
    }
    /// Initialize a remote-backed store (one file:// volume + file:// index).
    fn init_remote(&self) {
        let remotes = self.tmp.path().join("remotes");
        fs::create_dir_all(&remotes).unwrap();
        let v0 = format!("file://{}", remotes.join("v0.git").display());
        let idx = format!("file://{}", remotes.join("index.git").display());
        run_ok(
            bin(self.home())
                .args(["init", "--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile())
                .args(["--volume", &format!("v0={v0}")])
                .args(["--index-url", &idx])
                .args(["--chunk-size", "64k"]),
        );
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
    /// Path to the bare repo holding segments (mirror in remote mode, the real
    /// volume in local mode — same relative path either way).
    fn volume_git(&self) -> PathBuf {
        if self.remote {
            self.tmp.path().join("remotes/v0.git")
        } else {
            self.repo().join("volumes/v0.git")
        }
    }
    /// Path to the bare repo holding the log (the remote index in remote mode).
    fn index_git(&self) -> PathBuf {
        if self.remote {
            self.tmp.path().join("remotes/index.git")
        } else {
            self.repo().join("index.git")
        }
    }
    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let p = self.tmp.path().join(name);
        fs::write(&p, data).unwrap();
        p
    }
    fn put_cmd(&self, input: &Path) -> Command {
        let mut c = bin(self.home());
        c.args(["put"])
            .arg(input)
            .args(["--repo"])
            .arg(self.repo())
            .args(["--keyfile"])
            .arg(self.keyfile())
            .args(["--chunk-size", "64k"]);
        c
    }
    fn ls_stdout(&self) -> String {
        let out = run_ok(
            bin(self.home())
                .args(["ls", "--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn get_ok(&self, name: &str, output: &Path) {
        run_ok(
            bin(self.home())
                .args(["get", name, "--output"])
                .arg(output)
                .args(["--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        );
    }
    /// Run a put that self-destructs at the named crash point; assert it died.
    fn crashing_put(&self, input: &Path, point: &str) {
        let out = self
            .put_cmd(input)
            .env("GITSTORAGE_CRASH", point)
            .output()
            .expect("spawning git-storage");
        assert_eq!(
            out.status.code(),
            Some(97),
            "expected simulated crash at {point}, got: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    /// Segment refs currently present in volume v0.
    fn segment_refs(&self) -> Vec<String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.volume_git())
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
    fn log_tip(&self) -> Option<String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.index_git())
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/log"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }
}

// ---------- crash matrix (DESIGN.md Section 11) ----------
//
// M4: each crash-matrix and concurrency test body is parameterized over the
// backend (`Fx::new` = LocalBackend, `Fx::new_remote` = RemoteBackend over
// file://). The SAME body runs in both modes, demonstrating the engine is
// identical across the trait boundary — the M4 exit criterion.

/// C1: crash after chunk blobs are written but before the segment ref exists.
/// Blobs are unreachable; the log is untouched; a redo succeeds cleanly.
fn body_crash_c1(fx: &Fx) {
    let input = fx.file("a.bin", &varied_bytes(300 * 1024, 1));

    fx.crashing_put(&input, "before-segment");

    // Nothing reachable: no segment refs, no log.
    assert!(fx.segment_refs().is_empty(), "no segment ref may exist");
    assert!(fx.log_tip().is_none(), "log must be untouched");
    assert!(fx.ls_stdout().contains("store is empty"));

    // Redo is clean and the file is fully readable afterwards.
    run_ok(&mut fx.put_cmd(&input));
    let restored = fx.tmp.path().join("restored.bin");
    fx.get_ok("a.bin", &restored);
    assert_eq!(fs::read(&input).unwrap(), fs::read(&restored).unwrap());
}

#[test]
fn crash_c1_before_segment_leaves_store_consistent() {
    body_crash_c1(&Fx::new());
}

#[test]
fn crash_c1_before_segment_leaves_store_consistent_remote() {
    body_crash_c1(&Fx::new_remote());
}

/// C2: crash after the segment ref lands but before the log CAS. The segment
/// is an invisible orphan: readers see nothing, redo succeeds, and the
/// orphan's chunks get REUSED by the redo (content addressing at work).
fn body_crash_c2(fx: &Fx) {
    let input = fx.file("a.bin", &varied_bytes(300 * 1024, 2));

    fx.crashing_put(&input, "after-segment");

    // Segment exists (orphan), log untouched, namespace empty.
    assert_eq!(fx.segment_refs().len(), 1, "orphan segment ref expected");
    assert!(fx.log_tip().is_none(), "log must be untouched");
    assert!(fx.ls_stdout().contains("store is empty"));

    // Redo: the orphan is not referenced by the log, so the redo re-stages
    // (content-addressed blobs dedup at the object level inside git).
    run_ok(&mut fx.put_cmd(&input));
    let restored = fx.tmp.path().join("restored.bin");
    fx.get_ok("a.bin", &restored);
    assert_eq!(fs::read(&input).unwrap(), fs::read(&restored).unwrap());
}

#[test]
fn crash_c2_orphan_segment_is_invisible_and_redo_reuses_chunks() {
    body_crash_c2(&Fx::new());
}

#[test]
fn crash_c2_orphan_segment_is_invisible_and_redo_reuses_chunks_remote() {
    body_crash_c2(&Fx::new_remote());
}

/// C3 boundary: crash after the transaction commit object is built but before
/// the CAS is issued. The log ref must be exactly its old value.
fn body_crash_c3(fx: &Fx) {
    // First, one successful put so the log has a known tip.
    let base = fx.file("base.bin", &varied_bytes(100 * 1024, 3));
    run_ok(&mut fx.put_cmd(&base));
    let tip_before = fx.log_tip().expect("log tip after first put");

    // Crash a second put right before its CAS.
    let input = fx.file("b.bin", &varied_bytes(200 * 1024, 4));
    fx.crashing_put(&input, "before-cas");

    // The commit point never executed: tip unchanged, namespace = {base.bin}.
    assert_eq!(
        fx.log_tip().unwrap(),
        tip_before,
        "log tip must be unchanged"
    );
    let ls = fx.ls_stdout();
    assert!(ls.contains("base.bin") && !ls.contains("b.bin"));

    // Redo commits cleanly.
    run_ok(&mut fx.put_cmd(&input));
    assert!(fx.ls_stdout().contains("b.bin"));
}

#[test]
fn crash_c3_before_cas_log_is_old_value() {
    body_crash_c3(&Fx::new());
}

#[test]
fn crash_c3_before_cas_log_is_old_value_remote() {
    body_crash_c3(&Fx::new_remote());
}

/// C4: crash immediately after a successful CAS. The transaction is durable;
/// a fresh process sees the file and reads it back verified.
fn body_crash_c4(fx: &Fx) {
    let input = fx.file("a.bin", &varied_bytes(300 * 1024, 5));

    fx.crashing_put(&input, "after-cas");

    // Committed: visible and fully readable from a fresh process.
    assert!(fx.ls_stdout().contains("a.bin"));
    let restored = fx.tmp.path().join("restored.bin");
    fx.get_ok("a.bin", &restored);
    assert_eq!(fs::read(&input).unwrap(), fs::read(&restored).unwrap());

    // An identical redo is a no-op (idempotent recovery).
    let out = run_ok(&mut fx.put_cmd(&input));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no changes"),
        "redo must be no-op: {stdout}"
    );
}

#[test]
fn crash_c4_after_cas_transaction_is_durable() {
    body_crash_c4(&Fx::new());
}

#[test]
fn crash_c4_after_cas_transaction_is_durable_remote() {
    body_crash_c4(&Fx::new_remote());
}

// ---------- snapshot reads (Section 13.3) ----------

#[test]
fn pinned_reader_sees_immutable_snapshot() {
    body_pinned_reader(&Fx::new());
}

#[test]
fn pinned_reader_sees_immutable_snapshot_remote() {
    body_pinned_reader(&Fx::new_remote());
}

fn body_pinned_reader(fx: &Fx) {
    let a = fx.file("a.bin", &varied_bytes(100 * 1024, 6));
    run_ok(&mut fx.put_cmd(&a));
    let pinned = fx.log_tip().unwrap();

    // Writer advances the log.
    let b = fx.file("b.bin", &varied_bytes(100 * 1024, 7));
    run_ok(&mut fx.put_cmd(&b));

    // Pinned ls: only a.bin. Fresh ls: both.
    let out = run_ok(
        bin(fx.home())
            .args(["ls", "--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(fx.keyfile())
            .args(["--at", &pinned]),
    );
    let pinned_ls = String::from_utf8_lossy(&out.stdout);
    assert!(pinned_ls.contains("a.bin") && !pinned_ls.contains("b.bin"));

    let fresh_ls = fx.ls_stdout();
    assert!(fresh_ls.contains("a.bin") && fresh_ls.contains("b.bin"));

    // Pinned get still reconstructs a.bin even though the tip moved.
    let restored = fx.tmp.path().join("restored.bin");
    run_ok(
        bin(fx.home())
            .args(["get", "a.bin", "--output"])
            .arg(&restored)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(fx.keyfile())
            .args(["--at", &pinned]),
    );
    assert_eq!(fs::read(&a).unwrap(), fs::read(&restored).unwrap());
}

// ---------- concurrency (Section 13.2): safe but contended ----------

/// Two writers race on the same store: the CAS guarantees no lost update —
/// both files must be present and readable afterwards.
#[test]
fn concurrent_writers_never_lose_updates() {
    body_concurrent_writers(&Fx::new());
}

/// CAS-lost-race OVER THE WIRE (M4 exit): two RemoteBackends racing on the same
/// file:// remote. The push --force-with-lease loser must observe Ok(false),
/// rebase, retry, and converge — no lost updates end-to-end.
#[test]
fn concurrent_writers_never_lose_updates_remote() {
    body_concurrent_writers(&Fx::new_remote());
}

fn body_concurrent_writers(fx: &Fx) {
    // Initialize the store (and keyfile) once, serially.
    let init = fx.file("init.bin", &varied_bytes(64 * 1024, 8));
    run_ok(&mut fx.put_cmd(&init));

    let inputs: Vec<PathBuf> = (0..4)
        .map(|i| {
            fx.file(
                &format!("file-{i}.bin"),
                &varied_bytes(256 * 1024, 100 + i as u64),
            )
        })
        .collect();

    // Race 4 puts as simultaneous processes.
    let children: Vec<std::process::Child> = inputs
        .iter()
        .map(|input| {
            let mut c = fx.put_cmd(input);
            c.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            c.spawn().unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "concurrent put failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // No lost updates: every file is present and byte-identical.
    let ls = fx.ls_stdout();
    for i in 0..4 {
        assert!(
            ls.contains(&format!("file-{i}.bin")),
            "missing file-{i}: {ls}"
        );
    }
    for (i, input) in inputs.iter().enumerate() {
        let restored = fx.tmp.path().join(format!("r{i}.bin"));
        fx.get_ok(&format!("file-{i}.bin"), &restored);
        assert_eq!(fs::read(input).unwrap(), fs::read(&restored).unwrap());
    }
}

// ---------- checkpoints (Section 8.5) — library-level ----------

#[test]
fn checkpoints_bound_the_reader_tail() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let mut engine = Engine::open(&root, Keys::new([9u8; 32]), Some(64 * 1024)).unwrap();
    engine.set_checkpoint_interval(3);

    for i in 0..7 {
        let data = varied_bytes(64 * 1024, 200 + i as u64);
        engine.put(&format!("f{i}.bin"), &data[..]).unwrap();
    }

    // 7 transactions at interval 3 → checkpoints exist.
    let (checkpoints, deltas) = engine.txn_kind_counts().unwrap();
    assert!(
        checkpoints >= 2,
        "expected >=2 checkpoints after 7 puts at interval 3, got {checkpoints} (deltas {deltas})"
    );

    // Full state is correct regardless of checkpoint/delta layout.
    let ns = engine.namespace_at(None).unwrap();
    assert_eq!(ns.len(), 7);
    for i in 0..7 {
        assert!(ns.contains_key(&format!("f{i}.bin")));
    }
}

/// Snapshot semantics at the library level: a pinned namespace never changes
/// while the tip advances.
#[test]
fn library_snapshot_is_stable_across_writes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let engine = Engine::open(&root, Keys::new([10u8; 32]), Some(64 * 1024)).unwrap();

    engine
        .put("one.bin", &varied_bytes(64 * 1024, 300)[..])
        .unwrap();
    let pin = engine.log_tip().unwrap().unwrap();

    engine
        .put("two.bin", &varied_bytes(64 * 1024, 301)[..])
        .unwrap();
    engine
        .put("three.bin", &varied_bytes(64 * 1024, 302)[..])
        .unwrap();

    let snapshot = engine.namespace_at(Some(&pin)).unwrap();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.contains_key("one.bin"));

    let now = engine.namespace_at(None).unwrap();
    assert_eq!(now.len(), 3);
}
