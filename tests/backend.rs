//! M4 backend tests: rate governor, volume selection (§9.3/§15), budget wall
//! (§15.3), promisor probe + fallback (§10.3), and env-gated live-host suites
//! (Gitea, GitHub). Everything unit/file:// runs in CI; the live suites skip
//! cleanly when their env vars are unset.
//!
//! GIT ISOLATION: every git/CLI invocation runs with the user's config blanked
//! (GIT_CONFIG_GLOBAL/SYSTEM=/dev/null, HOME=tempdir, prompts disabled). Tests
//! never touch the user's real git config or credentials.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use tempfile::TempDir;

use git_storage::backend::{Backend, RemoteBackend};

// ---------- shared isolated-env CLI harness ----------

fn bin(sandbox_home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git-storage"));
    cmd.env("HOME", sandbox_home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GITSTORAGE_CRASH")
        .env_remove("GITSTORAGE_FORCE_FULL_FETCH");
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

/// A multi-volume file:// store fixture built via `git-storage init`.
struct MultiVol {
    tmp: TempDir,
}

impl MultiVol {
    /// `specs`: (id, threshold_bytes, push_interval_ms) per volume.
    fn new(specs: &[(&str, u64, u64)], chunk_size: &str) -> Self {
        let tmp = TempDir::new().unwrap();
        let remotes = tmp.path().join("remotes");
        fs::create_dir_all(&remotes).unwrap();
        let mut args: Vec<String> = vec![
            "init".into(),
            "--repo".into(),
            tmp.path().join("store").display().to_string(),
            "--keyfile".into(),
            tmp.path().join("master.key").display().to_string(),
            "--index-url".into(),
            format!("file://{}", remotes.join("index.git").display()),
            "--chunk-size".into(),
            chunk_size.into(),
        ];
        // NOTE: init applies one --threshold to all volumes; to give volumes
        // distinct thresholds we post-edit config.json below.
        for (id, _thr, interval) in specs {
            let url = format!("file://{}", remotes.join(format!("{id}.git")).display());
            args.push("--volume".into());
            args.push(format!("{id}={url}"));
            if *interval > 0 {
                // push-interval is global in the CLI; last one wins. For the
                // rate test we use a single volume so this is unambiguous.
                args.push("--push-interval-ms".into());
                args.push(interval.to_string());
            }
        }
        let fx = Self { tmp };
        run_ok(bin(fx.home()).args(&args));
        // Patch per-volume thresholds into config.json.
        fx.set_thresholds(specs);
        fx
    }

    fn set_thresholds(&self, specs: &[(&str, u64, u64)]) {
        let cfg_path = self.repo().join("config.json");
        let mut cfg: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&cfg_path).unwrap()).unwrap();
        if let Some(vols) = cfg.get_mut("volumes").and_then(|v| v.as_array_mut()) {
            for v in vols.iter_mut() {
                let id = v.get("id").and_then(|s| s.as_str()).unwrap().to_string();
                if let Some((_, thr, _)) = specs.iter().find(|(sid, _, _)| *sid == id) {
                    v["volume_full_threshold"] = serde_json::json!(thr);
                }
            }
        }
        fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
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
    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let p = self.tmp.path().join(name);
        fs::write(&p, data).unwrap();
        p
    }
    fn put(&self, input: &Path) -> Output {
        run(bin(self.home())
            .args(["put"])
            .arg(input)
            .args(["--repo"])
            .arg(self.repo())
            .args(["--keyfile"])
            .arg(self.keyfile()))
    }
    fn put_ok(&self, input: &Path) -> Output {
        run_ok(
            bin(self.home())
                .args(["put"])
                .arg(input)
                .args(["--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        )
    }
    fn get_ok(&self, name: &str, output: &Path) -> Output {
        run_ok(
            bin(self.home())
                .args(["get", name, "--output"])
                .arg(output)
                .args(["--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile()),
        )
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
    /// Which volume holds a segment ref (by checking each remote v*.git).
    fn segment_count(&self, id: &str) -> usize {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.tmp.path().join(format!("remotes/{id}.git")))
            .args(["for-each-ref", "--format=%(refname)", "refs/segments/"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }
}

// ---------- 3. rate governor ----------

/// With a non-trivial push_interval, consecutive segment pushes must be spaced
/// at least the interval apart (DESIGN §9.5). Uses a generous margin.
#[test]
fn rate_governor_spaces_pushes() {
    let interval_ms = 400u64;
    let fx = MultiVol::new(&[("v0", 1 << 30, interval_ms)], "16k");
    // Two DISTINCT files → two segments → two throttled pushes in one process.
    // Do them in a single put each; measure wall time across three puts. The
    // throttle is per-RemoteBackend-instance (one process), so we drive three
    // segment pushes inside ONE process by putting a file with 3 unique bodies
    // is awkward across CLI calls; instead assert the pair spacing directly on
    // the library backend.
    let _ = fx;

    let tmp = TempDir::new().unwrap();
    let remote = tmp.path().join("remote.git");
    Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&remote)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    let url = format!("file://{}", remote.display());
    let mirror = tmp.path().join("mirror.git");
    let be = RemoteBackend::open(&url, &mirror, interval_ms).unwrap();

    // Three pushes; measure total elapsed. Expect >= 2 * interval (the first
    // push is immediate, the next two each wait a full interval).
    let start = Instant::now();
    for i in 0..3 {
        let blob = be.write_blob(format!("obj-{i}").as_bytes()).unwrap();
        let tree = be.write_tree(&[(format!("aa/bb/{i}"), blob)]).unwrap();
        let commit = be.commit_tree(&tree, &[], &format!("s{i}")).unwrap();
        be.set_ref(&format!("refs/segments/seg{i}"), &commit)
            .unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() as u64 >= 2 * interval_ms - 50,
        "3 throttled pushes at {interval_ms}ms should take >= {}ms, took {}ms",
        2 * interval_ms,
        elapsed.as_millis()
    );
}

// ---------- 4. budget wall (§15.3) ----------

/// With an artificially tiny volume-full threshold, writes must be REFUSED
/// once the volume can't hold the next segment. The refusal must be clean and
/// the store must stay consistent + readable (already-committed data intact).
#[test]
fn budget_wall_refuses_and_store_stays_consistent() {
    // 64 KiB threshold; 16k chunks. One ~200 KiB file exceeds it.
    let fx = MultiVol::new(&[("v0", 64 * 1024, 0)], "16k");

    // First small file fits.
    let small = fx.file("small.bin", &varied_bytes(20 * 1024, 1));
    fx.put_ok(&small);

    // Now a file whose new segment would breach the threshold: refused.
    let big = fx.file("big.bin", &varied_bytes(400 * 1024, 2));
    let out = fx.put(&big);
    assert!(
        !out.status.success(),
        "budget wall must refuse the oversized write"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("budget exhausted") || stderr.contains("volume-full"),
        "refusal must name the budget wall, got: {stderr}"
    );

    // Store still consistent: small.bin present and readable; big.bin absent.
    let ls = fx.ls_stdout();
    assert!(
        ls.contains("small.bin") && !ls.contains("big.bin"),
        "ls: {ls}"
    );
    let restored = fx.tmp.path().join("r.bin");
    fx.get_ok("small.bin", &restored);
    assert_eq!(fs::read(&small).unwrap(), fs::read(&restored).unwrap());

    // The fleet did NOT grow: still exactly one volume repo.
    assert!(fx.tmp.path().join("remotes/v0.git").exists());
    assert!(!fx.tmp.path().join("remotes/v1.git").exists());
}

// ---------- 5. volume selection (§9.3 / §15.5) ----------

/// Placement goes to the volume with the MOST free headroom below threshold;
/// with N >= 3 volumes the last-declared one is the reserved spare and never
/// receives ordinary writes.
#[test]
fn volume_selection_prefers_headroom_and_reserves_spare() {
    // 3 volumes: v0 big, v1 medium, v2 spare (reserved at N>=3). All generous
    // thresholds so the budget wall doesn't interfere; we test PLACEMENT.
    let fx = MultiVol::new(
        &[
            ("v0", 100 * 1024 * 1024, 0),
            ("v1", 100 * 1024 * 1024, 0),
            ("v2", 100 * 1024 * 1024, 0),
        ],
        "16k",
    );

    // Several distinct files. Each new segment should land in a WRITABLE
    // volume (v0 or v1), never the spare (v2).
    for i in 0..4 {
        let f = fx.file(&format!("f{i}.bin"), &varied_bytes(64 * 1024, 10 + i));
        fx.put_ok(&f);
    }

    let spare = fx.segment_count("v2");
    assert_eq!(spare, 0, "spare volume v2 must receive no ordinary writes");
    let writable = fx.segment_count("v0") + fx.segment_count("v1");
    assert_eq!(writable, 4, "all 4 segments must land in writable volumes");

    // With both writable volumes starting empty and equal thresholds, the
    // first placement goes to the most-headroom (tie → lowest id = v0). After
    // v0 has data, the next placement should prefer the now-emptier v1.
    assert!(
        fx.segment_count("v0") >= 1,
        "v0 should receive at least one"
    );
    assert!(
        fx.segment_count("v1") >= 1,
        "v1 should receive writes once it has more headroom than v0"
    );
}

// ---------- 6. promisor probe + fallback (§10.3) ----------

/// The read path works both with promisor blob-by-OID fetch AND with the
/// forced full-segment fallback (GITSTORAGE_FORCE_FULL_FETCH=1). Both must
/// reconstruct byte-identical data over a file:// remote.
#[test]
fn promisor_read_and_forced_fallback_both_roundtrip() {
    let fx = MultiVol::new(&[("v0", 1 << 30, 0)], "16k");
    let data = varied_bytes(200 * 1024, 42);
    let input = fx.file("p.bin", &data);
    fx.put_ok(&input);

    // Promisor path (default).
    let r1 = fx.tmp.path().join("r1.bin");
    fx.get_ok("p.bin", &r1);
    assert_eq!(data, fs::read(&r1).unwrap(), "promisor read must roundtrip");

    // Wipe the mirror so the next read must fetch from the remote again.
    let _ = fs::remove_dir_all(fx.repo().join("volumes/v0.git"));

    // Forced full-segment fallback path.
    let r2 = fx.tmp.path().join("r2.bin");
    run_ok(
        bin(fx.home())
            .env("GITSTORAGE_FORCE_FULL_FETCH", "1")
            .args(["get", "p.bin", "--output"])
            .arg(&r2)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--keyfile"])
            .arg(fx.keyfile()),
    );
    assert_eq!(data, fs::read(&r2).unwrap(), "fallback read must roundtrip");
}

/// Library-level: after a promisor read the verdict is observable, and a
/// force-full-fetch backend reports the fallback.
#[test]
fn promisor_verdict_is_observable() {
    let tmp = TempDir::new().unwrap();
    let remote = tmp.path().join("remote.git");
    Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&remote)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    let url = format!("file://{}", remote.display());

    // Stage + push a segment via one backend.
    let m1 = tmp.path().join("m1.git");
    let writer = RemoteBackend::open(&url, &m1, 0).unwrap();
    let blob = writer.write_blob(b"ciphertext-here").unwrap();
    let tree = writer.write_tree(&[("aa/bb/cid".into(), blob)]).unwrap();
    let commit = writer.commit_tree(&tree, &[], "seg").unwrap();
    writer.set_ref("refs/segments/s", &commit).unwrap();

    // Fresh reader with a fresh mirror → promisor path.
    let m2 = tmp.path().join("m2.git");
    let reader = RemoteBackend::open(&url, &m2, 0).unwrap();
    assert_eq!(reader.promisor_supported(), None, "not probed yet");
    let got = reader.read_blob_at("refs/segments/s", "aa/bb/cid").unwrap();
    assert_eq!(got, b"ciphertext-here");
    assert_eq!(
        reader.promisor_supported(),
        Some(true),
        "file:// supports promisor in modern git"
    );

    // Forced-fallback backend reports unsupported after a read.
    std::env::set_var("GITSTORAGE_FORCE_FULL_FETCH", "1");
    let m3 = tmp.path().join("m3.git");
    let fb = RemoteBackend::open(&url, &m3, 0).unwrap();
    let got = fb.read_blob_at("refs/segments/s", "aa/bb/cid").unwrap();
    std::env::remove_var("GITSTORAGE_FORCE_FULL_FETCH");
    assert_eq!(got, b"ciphertext-here");
    assert_eq!(fb.promisor_supported(), Some(false), "forced fallback");
}

// ---------- 8. Gitea integration suite (env-gated) ----------

/// Full put/get roundtrip against a LIVE Gitea, gated on GITSTORAGE_GITEA_URL
/// (a base URL like https://gitea.example.com) + GITSTORAGE_TOKEN +
/// GITSTORAGE_GITEA_OWNER. Skips with a clear message when unset. Docker is not
/// available in this environment, so it skips here — but it is written to work
/// against a real instance.
#[test]
fn gitea_live_roundtrip() {
    let (base, owner, _token) = match (
        std::env::var("GITSTORAGE_GITEA_URL"),
        std::env::var("GITSTORAGE_GITEA_OWNER"),
        std::env::var("GITSTORAGE_TOKEN"),
    ) {
        (Ok(b), Ok(o), Ok(t)) => (b, o, t),
        _ => {
            eprintln!(
                "SKIP gitea_live_roundtrip: set GITSTORAGE_GITEA_URL, \
                 GITSTORAGE_GITEA_OWNER, GITSTORAGE_TOKEN to run against a live \
                 Gitea instance."
            );
            return;
        }
    };
    live_roundtrip_https(&base, &owner, "gitea");
}

// ---------- 9. GitHub smoke test (env-gated) ----------

/// Tiny put/get roundtrip against LIVE GitHub, gated on GITSTORAGE_TOKEN +
/// GITSTORAGE_GITHUB_OWNER. Also exercises the budget wall on an artificial
/// full mark. Skips cleanly when unset.
#[test]
fn github_live_smoke() {
    let (owner, _token) = match (
        std::env::var("GITSTORAGE_GITHUB_OWNER"),
        std::env::var("GITSTORAGE_TOKEN"),
    ) {
        (Ok(o), Ok(t)) => (o, t),
        _ => {
            eprintln!(
                "SKIP github_live_smoke: set GITSTORAGE_TOKEN and \
                 GITSTORAGE_GITHUB_OWNER to run a real GitHub roundtrip."
            );
            return;
        }
    };
    live_roundtrip_https("https://github.com", &owner, "github");
}

/// Shared live-host roundtrip: init a store with two remote volumes + index,
/// put a tiny file, read it back verified, then assert the budget wall refuses
/// once a volume is artificially marked full. Uses unique repo names per run.
fn live_roundtrip_https(web_base: &str, owner: &str, host_arg: &str) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let v0 = format!("gitstorage-test-v0-{suffix}");
    let v1 = format!("gitstorage-test-v1-{suffix}");
    let idx = format!("gitstorage-test-idx-{suffix}");
    let repo_url = |name: &str| format!("{web_base}/{owner}/{name}.git");

    let store = home.join("store");
    let keyfile = home.join("master.key");
    let out = run(bin(home)
        .args(["init", "--repo"])
        .arg(&store)
        .args(["--keyfile"])
        .arg(&keyfile)
        .args(["--host", host_arg])
        .args(["--gitea-base", web_base])
        .args(["--volume", &format!("v0={}", repo_url(&v0))])
        .args(["--volume", &format!("v1={}", repo_url(&v1))])
        .args(["--index-url", &repo_url(&idx)])
        .args(["--chunk-size", "64k"])
        .args(["--threshold", &(4u64 * 1024 * 1024 * 1024).to_string()]));
    assert!(
        out.status.success(),
        "live init failed (provisioning): {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let data = varied_bytes(128 * 1024, 7);
    let input = home.join("live.bin");
    fs::write(&input, &data).unwrap();
    run_ok(
        bin(home)
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(&store)
            .args(["--keyfile"])
            .arg(&keyfile),
    );

    let restored = home.join("restored.bin");
    run_ok(
        bin(home)
            .args(["get", "live.bin", "--output"])
            .arg(&restored)
            .args(["--repo"])
            .arg(&store)
            .args(["--keyfile"])
            .arg(&keyfile),
    );
    assert_eq!(
        data,
        fs::read(&restored).unwrap(),
        "live roundtrip byte-identical"
    );

    // Budget wall: mark all volumes tiny-full by rewriting thresholds to 1 byte.
    let cfg_path = store.join("config.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg_path).unwrap()).unwrap();
    for v in cfg["volumes"].as_array_mut().unwrap() {
        v["volume_full_threshold"] = serde_json::json!(1u64);
    }
    fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    let big = home.join("big.bin");
    fs::write(&big, varied_bytes(256 * 1024, 8)).unwrap();
    let out = run(bin(home)
        .args(["put"])
        .arg(&big)
        .args(["--repo"])
        .arg(&store)
        .args(["--keyfile"])
        .arg(&keyfile));
    assert!(
        !out.status.success(),
        "budget wall must refuse once volumes are marked full"
    );
    eprintln!(
        "NOTE: live test created private repos {v0}, {v1}, {idx} under {owner} — \
         delete them via the host interface when done."
    );
}
