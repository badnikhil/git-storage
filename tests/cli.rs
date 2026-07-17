//! CLI surface + error-path coverage.
//!
//! These lock the behaviour a user actually hits at the command line: edge-size
//! files, missing inputs, bad snapshots, `init` validation, keyfile validation,
//! `rm`/`tip`/`stats` output, and delete→re-put resurrection. The earlier suites
//! cover the happy path and the engine internals; this file covers the seams and
//! the failure messages.
//!
//! GIT ISOLATION: same rules as the rest of the suite — HOME=tempdir, git config
//! masked, prompts disabled, no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

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

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("spawning git-storage")
}

fn run_ok(cmd: &mut Command) -> Output {
    let out = run(cmd);
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

/// A back-compat local store (single synthesized volume, no `init` needed) —
/// the fast path for CLI behaviour that doesn't care about the volume set.
struct Local {
    tmp: TempDir,
}

impl Local {
    fn new() -> Self {
        Self {
            tmp: TempDir::new().unwrap(),
        }
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
    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }
    fn put(&self, input: &Path) -> Output {
        run(bin(self.home())
            .args(["put"])
            .arg(input)
            .args(["--repo"])
            .arg(self.repo())
            .args(["--keyfile"])
            .arg(self.keyfile())
            .args(["--chunk-size", "64k"]))
    }
    fn cmd(&self, args: &[&str]) -> Output {
        run(bin(self.home())
            .args(args)
            .args(["--repo"])
            .arg(self.repo())
            .args(["--keyfile"])
            .arg(self.keyfile()))
    }
}

// ---------- edge-size roundtrips ----------

/// A 0-byte file must round-trip: 0 chunks stored, an empty file reconstructed,
/// all hashes "verified" (vacuously) — never a crash or a spurious byte.
#[test]
fn empty_file_roundtrips() {
    let fx = Local::new();
    let input = fx.path("empty.bin");
    fs::write(&input, b"").unwrap();
    let put = fx.put(&input);
    assert!(put.status.success(), "put of empty file must succeed");
    assert!(String::from_utf8_lossy(&put.stdout).contains("0 chunks"));

    let out = fx.path("empty.out");
    run_ok(
        bin(fx.home())
            .args(["get", "empty.bin", "--output"])
            .arg(&out)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(fx.keyfile()),
    );
    assert_eq!(fs::read(&out).unwrap(), b"", "empty file must round-trip");
}

/// A 1-byte file exercises the sub-min-chunk path.
#[test]
fn single_byte_file_roundtrips() {
    let fx = Local::new();
    let input = fx.path("one.bin");
    fs::write(&input, b"Z").unwrap();
    assert!(fx.put(&input).status.success());
    let out = fx.path("one.out");
    run_ok(
        bin(fx.home())
            .args(["get", "one.bin", "--output"])
            .arg(&out)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(fx.keyfile()),
    );
    assert_eq!(fs::read(&out).unwrap(), b"Z");
}

// ---------- missing / bad inputs ----------

#[test]
fn put_missing_input_fails_cleanly() {
    let fx = Local::new();
    let out = fx.put(&fx.path("does-not-exist.bin"));
    assert!(!out.status.success(), "put of a missing file must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("opening") || stderr.contains("No such file"),
        "error must name the missing input, got: {stderr}"
    );
}

#[test]
fn get_unknown_name_fails_cleanly() {
    let fx = Local::new();
    let input = fx.path("real.bin");
    fs::write(&input, varied_bytes(40 * 1024, 1)).unwrap();
    fx.put(&input);

    let out = run(bin(fx.home())
        .args(["get", "ghost.bin", "--output"])
        .arg(fx.path("g.out"))
        .args(["--repo"])
        .arg(fx.repo())
        .args(["--keyfile"])
        .arg(fx.keyfile()));
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no file") && stderr.contains("ghost.bin"),
        "must name the missing file, got: {stderr}"
    );
}

/// `get --at <bad-oid>` must fail loudly, never silently read the tip.
#[test]
fn get_at_bad_commit_fails_cleanly() {
    let fx = Local::new();
    let input = fx.path("real.bin");
    fs::write(&input, varied_bytes(40 * 1024, 2)).unwrap();
    fx.put(&input);

    let out = run(bin(fx.home())
        .args(["get", "real.bin", "--output"])
        .arg(fx.path("out.bin"))
        .args(["--repo"])
        .arg(fx.repo())
        .args(["--keyfile"])
        .arg(fx.keyfile())
        .args(["--at", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"]));
    assert!(!out.status.success(), "a bad --at commit must fail");
}

// ---------- rm at the CLI ----------

#[test]
fn rm_unknown_name_fails_at_cli() {
    let fx = Local::new();
    let input = fx.path("real.bin");
    fs::write(&input, varied_bytes(20 * 1024, 3)).unwrap();
    fx.put(&input);

    let out = fx.cmd(&["rm", "nope.bin"]);
    assert!(!out.status.success(), "rm of an unknown name must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no file"));
}

/// Delete then re-put the same name: the file comes back, byte-identical.
#[test]
fn delete_then_reput_resurrects() {
    let fx = Local::new();
    let data = varied_bytes(50 * 1024, 4);
    let input = fx.path("doc.bin");
    fs::write(&input, &data).unwrap();
    fx.put(&input);

    assert!(fx.cmd(&["rm", "doc.bin"]).status.success());
    assert!(
        String::from_utf8_lossy(&fx.cmd(&["ls"]).stdout).contains("store is empty"),
        "namespace must be empty after rm"
    );

    assert!(fx.put(&input).status.success(), "re-put must succeed");
    let out = fx.path("doc.out");
    run_ok(
        bin(fx.home())
            .args(["get", "doc.bin", "--output"])
            .arg(&out)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(fx.keyfile()),
    );
    assert_eq!(fs::read(&out).unwrap(), data, "resurrected file must match");
}

// ---------- tip / stats output ----------

#[test]
fn tip_reports_empty_log_on_a_fresh_store() {
    let fx = Local::new();
    // A never-written store: `tip` opens it (creating the keyfile) and reports
    // an empty log — no commit yet.
    let out = fx.cmd(&["tip"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("empty log"));
}

#[test]
fn tip_reports_a_commit_after_a_put() {
    let fx = Local::new();
    // Put FIRST so the store is created at this chunk size (avoids the pinned
    // chunk-size conflict a prior `tip` would introduce at the 1 MiB default).
    let input = fx.path("x.bin");
    fs::write(&input, varied_bytes(20 * 1024, 5)).unwrap();
    assert!(fx.put(&input).status.success());

    let out = fx.cmd(&["tip"]);
    let oid = String::from_utf8_lossy(&out.stdout);
    let oid = oid.trim();
    assert!(
        oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit()),
        "tip must print a 40-hex commit oid, got: {oid:?}"
    );
}

#[test]
fn stats_reports_usage_after_a_put() {
    let fx = Local::new();
    let input = fx.path("s.bin");
    fs::write(&input, varied_bytes(80 * 1024, 6)).unwrap();
    fx.put(&input);

    let out = fx.cmd(&["stats"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("v0"), "stats must list the volume: {s}");
    assert!(
        s.contains("live") && s.contains("dead"),
        "stats must report live/dead accounting: {s}"
    );
    // A back-compat local store reports no promisor probe.
    assert!(
        s.contains("read path"),
        "stats must report the read path: {s}"
    );
}

// ---------- keyfile validation ----------

#[test]
fn malformed_keyfile_wrong_length_is_rejected() {
    let fx = Local::new();
    let input = fx.path("x.bin");
    fs::write(&input, varied_bytes(20 * 1024, 7)).unwrap();
    fx.put(&input); // creates a valid keyfile + store

    // Overwrite the keyfile with too-short hex.
    fs::write(fx.keyfile(), "abcd\n").unwrap();
    let out = fx.cmd(&["ls"]);
    assert!(!out.status.success(), "a short keyfile must be rejected");
    assert!(String::from_utf8_lossy(&out.stderr).contains("hex"));
}

#[test]
fn malformed_keyfile_non_hex_is_rejected() {
    let fx = Local::new();
    // 64 chars but not hex.
    fs::write(fx.keyfile(), format!("{}\n", "z".repeat(64))).unwrap();
    // Create a store first with a good key so "existing store" path is hit.
    let good = fx.path("good.key");
    fs::write(&good, format!("{}\n", "ab".repeat(32))).unwrap();
    let input = fx.path("x.bin");
    fs::write(&input, varied_bytes(20 * 1024, 8)).unwrap();
    run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(&good)
            .args(["--chunk-size", "64k"]),
    );
    // Now read with the non-hex keyfile.
    let out = fx.cmd(&["ls"]);
    assert!(!out.status.success(), "a non-hex keyfile must be rejected");
    assert!(String::from_utf8_lossy(&out.stderr).contains("hex"));
}

// ---------- init validation (needs the multi-volume path) ----------

fn init_args(home: &Path, store: &Path, key: &Path) -> Command {
    let mut c = bin(home);
    c.args(["init", "--repo"])
        .arg(store)
        .args(["--keyfile"])
        .arg(key);
    c
}

#[test]
fn init_requires_at_least_one_volume() {
    let tmp = TempDir::new().unwrap();
    let out = run(&mut init_args(
        tmp.path(),
        &tmp.path().join("store"),
        &tmp.path().join("k"),
    ));
    assert!(!out.status.success(), "init with no --volume must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("at least one --volume"));
}

#[test]
fn init_rejects_malformed_volume_spec() {
    let tmp = TempDir::new().unwrap();
    let out = run(
        init_args(tmp.path(), &tmp.path().join("store"), &tmp.path().join("k"))
            .args(["--volume", "no-equals-sign"]),
    );
    assert!(!out.status.success(), "a --volume without = must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("ID=URL"));
}

#[test]
fn init_refuses_an_existing_store() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let key = tmp.path().join("k");
    let vol = format!("v0=file://{}", tmp.path().join("rem/v0.git").display());
    let idx = format!("file://{}", tmp.path().join("rem/index.git").display());
    fs::create_dir_all(tmp.path().join("rem")).unwrap();

    run_ok(
        init_args(tmp.path(), &store, &key)
            .args(["--volume", &vol])
            .args(["--index-url", &idx])
            .args(["--chunk-size", "64k"]),
    );
    // Second init on the same store dir must refuse.
    let out = run(init_args(tmp.path(), &store, &key)
        .args(["--volume", &vol])
        .args(["--index-url", &idx]));
    assert!(!out.status.success(), "re-init must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("already initialized"));
}
