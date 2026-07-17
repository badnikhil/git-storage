//! M6 test: whole-store mirror to an independent backend (DESIGN §14.3).
//!
//! A store is mirrored to a SECOND, independent set of file:// repos, then a
//! fresh store is pointed at the mirror (same keyfile) and every file must read
//! back byte-identical from the copy alone — proving the mirror is a complete,
//! self-sufficient replica. Same git isolation as the rest of the suite.

use std::fs;
use std::path::Path;
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

#[test]
fn whole_store_mirror_is_a_complete_independent_replica() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let keyfile = home.join("master.key");

    // ---- primary store: two file:// volumes + file:// index ----
    let primary = home.join("primary");
    let prem = home.join("primary-remotes");
    fs::create_dir_all(&prem).unwrap();
    let pv0 = format!("file://{}", prem.join("v0.git").display());
    let pv1 = format!("file://{}", prem.join("v1.git").display());
    let pidx = format!("file://{}", prem.join("index.git").display());
    run_ok(
        bin(home)
            .args(["init", "--repo"])
            .arg(&primary)
            .args(["--keyfile"])
            .arg(&keyfile)
            .args(["--volume", &format!("v0={pv0}")])
            .args(["--volume", &format!("v1={pv1}")])
            .args(["--index-url", &pidx])
            .args(["--chunk-size", "64k"]),
    );

    // Store several files (varied seeds → no accidental cross-file dedup),
    // spread across the two writable volumes.
    let files: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("f{i}.bin"), varied_bytes(120 * 1024, 1 + i)))
        .collect();
    for (name, data) in &files {
        let p = home.join(name);
        fs::write(&p, data).unwrap();
        run_ok(
            bin(home)
                .args(["put"])
                .arg(&p)
                .args(["--repo"])
                .arg(&primary)
                .args(["--keyfile"])
                .arg(&keyfile)
                .args(["--chunk-size", "64k"]),
        );
    }

    // ---- mirror to an INDEPENDENT set of file:// repos ----
    let mrem = home.join("mirror-remotes");
    fs::create_dir_all(&mrem).unwrap();
    let mv0 = format!("file://{}", mrem.join("v0.git").display());
    let mv1 = format!("file://{}", mrem.join("v1.git").display());
    let midx = format!("file://{}", mrem.join("index.git").display());
    run_ok(
        bin(home)
            .args(["mirror", "--repo"])
            .arg(&primary)
            .args(["--keyfile"])
            .arg(&keyfile)
            .args(["--to-index", &midx])
            .args(["--to-volume", &format!("v0={mv0}")])
            .args(["--to-volume", &format!("v1={mv1}")]),
    );

    // ---- open a FRESH store against the mirror only, same keyfile ----
    // Build the replica config by copying the primary's config.json and
    // repointing the URLs to the mirror (robust against config schema drift).
    // If reads succeed here, the mirror is a self-sufficient replica — the
    // primary's repos are never consulted.
    let replica = home.join("replica");
    fs::create_dir_all(&replica).unwrap();
    let cfg = fs::read_to_string(primary.join("config.json"))
        .unwrap()
        .replace(&pv0, &mv0)
        .replace(&pv1, &mv1)
        .replace(&pidx, &midx);
    fs::write(replica.join("config.json"), cfg).unwrap();

    // ls against the replica lists every file.
    let out = run_ok(
        bin(home)
            .args(["ls", "--repo"])
            .arg(&replica)
            .args(["--keyfile"])
            .arg(&keyfile),
    );
    let ls = String::from_utf8_lossy(&out.stdout);
    for (name, _) in &files {
        assert!(
            ls.contains(name.as_str()),
            "replica ls missing {name}: {ls}"
        );
    }

    // get every file from the replica → byte-identical to the original.
    for (name, data) in &files {
        let got = home.join(format!("replica-{name}"));
        run_ok(
            bin(home)
                .args(["get", name, "--output"])
                .arg(&got)
                .args(["--repo"])
                .arg(&replica)
                .args(["--keyfile"])
                .arg(&keyfile),
        );
        assert_eq!(
            &fs::read(&got).unwrap(),
            data,
            "{name} from the mirror must be byte-identical"
        );
    }
}

/// Re-mirroring after more writes is idempotent and carries the new data:
/// the replica sees the later file too.
#[test]
fn re_mirror_is_incremental_and_current() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let keyfile = home.join("master.key");
    let primary = home.join("primary");
    let prem = home.join("primary-remotes");
    fs::create_dir_all(&prem).unwrap();
    let pv0 = format!("file://{}", prem.join("v0.git").display());
    let pidx = format!("file://{}", prem.join("index.git").display());

    let init = |extra_vol: &str| {
        run_ok(
            bin(home)
                .args(["init", "--repo"])
                .arg(&primary)
                .args(["--keyfile"])
                .arg(&keyfile)
                .args(["--volume", extra_vol])
                .args(["--index-url", &pidx])
                .args(["--chunk-size", "64k"]),
        );
    };
    init(&format!("v0={pv0}"));

    let put = |name: &str, data: &[u8]| {
        let p = home.join(name);
        fs::write(&p, data).unwrap();
        run_ok(
            bin(home)
                .args(["put"])
                .arg(&p)
                .args(["--repo"])
                .arg(&primary)
                .args(["--keyfile"])
                .arg(&keyfile)
                .args(["--chunk-size", "64k"]),
        );
    };
    put("early.bin", &varied_bytes(80 * 1024, 7));

    let mrem = home.join("mirror-remotes");
    fs::create_dir_all(&mrem).unwrap();
    let mv0 = format!("file://{}", mrem.join("v0.git").display());
    let midx = format!("file://{}", mrem.join("index.git").display());
    let do_mirror = || {
        run_ok(
            bin(home)
                .args(["mirror", "--repo"])
                .arg(&primary)
                .args(["--keyfile"])
                .arg(&keyfile)
                .args(["--to-index", &midx])
                .args(["--to-volume", &format!("v0={mv0}")]),
        );
    };
    do_mirror();
    // More data, then mirror again — must be accepted (idempotent) and current.
    let late = varied_bytes(80 * 1024, 8);
    put("late.bin", &late);
    do_mirror();

    let replica = home.join("replica");
    fs::create_dir_all(&replica).unwrap();
    let cfg = fs::read_to_string(primary.join("config.json"))
        .unwrap()
        .replace(&pv0, &mv0)
        .replace(&pidx, &midx);
    fs::write(replica.join("config.json"), cfg).unwrap();

    let got = home.join("replica-late.bin");
    run_ok(
        bin(home)
            .args(["get", "late.bin", "--output"])
            .arg(&got)
            .args(["--repo"])
            .arg(&replica)
            .args(["--keyfile"])
            .arg(&keyfile),
    );
    assert_eq!(
        fs::read(&got).unwrap(),
        late,
        "re-mirror must carry new data"
    );
}
