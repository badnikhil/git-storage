//! Integration tests: M0 exit criteria (roundtrip, dedup, corruption) plus
//! M1 exit criteria (insert-in-middle dedup, per-store boundary divergence,
//! pinned chunk-size enforcement).
//!
//! GIT ISOLATION: every CLI invocation runs with the user's git configuration
//! blanked out (GIT_CONFIG_GLOBAL/SYSTEM pointed at /dev/null, HOME at the
//! tempdir, credential prompts disabled). Tests must never read the user's
//! gitconfig or credentials, and there is nothing to push to even if they did
//! (the CLI has no network git operations).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin(sandbox_home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git-storage"));
    cmd.env("HOME", sandbox_home) // never the real home
        .env("GIT_CONFIG_GLOBAL", "/dev/null") // never the user's ~/.gitconfig
        .env("GIT_CONFIG_SYSTEM", "/dev/null") // never /etc/gitconfig
        .env("GIT_TERMINAL_PROMPT", "0") // never prompt for credentials
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS");
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

/// Deterministic varied bytes (xorshift PRNG) so chunks differ and dedup
/// within one file cannot occur by accident.
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

struct StoreFixture {
    tmp: TempDir,
}

impl StoreFixture {
    fn new() -> Self {
        Self {
            tmp: TempDir::new().unwrap(),
        }
    }
    fn home(&self) -> &Path {
        self.tmp.path()
    }
    fn repo(&self) -> std::path::PathBuf {
        self.tmp.path().join("store")
    }
    fn file(&self, name: &str, data: &[u8]) -> std::path::PathBuf {
        let p = self.tmp.path().join(name);
        fs::write(&p, data).unwrap();
        p
    }
}

#[test]
fn roundtrip_is_byte_identical() {
    let fx = StoreFixture::new();
    let data = varied_bytes(3 * 1024 * 1024 + 137, 0x243F6A88); // non-aligned tail
    let input = fx.file("input.bin", &data);
    let output = fx.tmp.path().join("restored.bin");

    run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--chunk-size", "256k"]),
    );
    run_ok(
        bin(fx.home())
            .args(["get", "input.bin", "--output"])
            .arg(&output)
            .args(["--repo"])
            .arg(fx.repo()),
    );

    assert_eq!(
        data,
        fs::read(&output).unwrap(),
        "roundtrip must be byte-identical"
    );
}

#[test]
fn second_put_dedups_everything() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(1024 * 1024, 0x1234));

    run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--chunk-size", "256k"]),
    );
    let second = run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(fx.repo()),
    );

    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("0 new"),
        "second identical put must report 0 new chunks, got: {stdout}"
    );
    assert!(
        stdout.contains("no changes"),
        "second identical put must not create a commit, got: {stdout}"
    );
}

#[test]
fn corrupted_object_fails_get_loudly() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(600 * 1024, 0x77));
    let output = fx.tmp.path().join("restored.bin");

    run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--chunk-size", "64k"]),
    );

    // Flip one byte in the first chunk object found.
    let object = find_first_object(&fx.repo().join("objects"));
    let mut bytes = fs::read(&object).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&object, &bytes).unwrap();

    let out = bin(fx.home())
        .args(["get", "input.bin", "--output"])
        .arg(&output)
        .args(["--repo"])
        .arg(fx.repo())
        .output()
        .expect("spawning git-storage");
    assert!(!out.status.success(), "get must fail on corrupted object");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hash mismatch"),
        "error must name the hash mismatch, got: {stderr}"
    );
}

/// M1 exit criterion 1: after a mid-file insert, most chunks must dedup
/// against the original version. Fixed-size chunking dedups nothing past the
/// edit point; FastCDC re-synchronizes.
#[test]
fn insert_in_middle_mostly_dedups() {
    let fx = StoreFixture::new();
    let original = varied_bytes(16 * 1024 * 1024, 0xBEEF);
    let mut edited = original.clone();
    edited.splice(8 * 1024 * 1024..8 * 1024 * 1024, [0xAAu8; 1024]);

    let orig_file = fx.file("data.bin", &original);
    run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&orig_file)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--chunk-size", "256k"]),
    );

    // Re-put under the same name after the edit; count dedup from the output.
    fs::write(&orig_file, &edited).unwrap();
    let out = run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&orig_file)
            .args(["--repo"])
            .arg(fx.repo()),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // "data.bin: <total> chunks, <new> new, <deduped> deduped, ..."
    let (total, new) = parse_counts(&stdout);
    let deduped = total - new;
    assert!(
        deduped * 100 / total >= 80,
        "expected >=80% chunk dedup after 1 KiB mid-file insert in 16 MiB, got {deduped}/{total}\noutput: {stdout}"
    );

    // And the edited file must still roundtrip.
    let restored = fx.tmp.path().join("restored.bin");
    run_ok(
        bin(fx.home())
            .args(["get", "data.bin", "--output"])
            .arg(&restored)
            .args(["--repo"])
            .arg(fx.repo()),
    );
    assert_eq!(edited, fs::read(&restored).unwrap());
}

/// M1 exit criterion 2: two stores (different gear seeds) chunk the same file
/// differently — the anti-fingerprinting property.
#[test]
fn different_stores_produce_different_boundaries() {
    let fx = StoreFixture::new();
    let data = varied_bytes(4 * 1024 * 1024, 0xCAFE);
    let input = fx.file("input.bin", &data);
    let repo_a = fx.tmp.path().join("store-a");
    let repo_b = fx.tmp.path().join("store-b");

    for repo in [&repo_a, &repo_b] {
        run_ok(
            bin(fx.home())
                .args(["put"])
                .arg(&input)
                .args(["--repo"])
                .arg(repo)
                .args(["--chunk-size", "256k"]),
        );
    }

    let hashes_a = object_names(&repo_a.join("objects"));
    let hashes_b = object_names(&repo_b.join("objects"));
    assert_ne!(
        hashes_a, hashes_b,
        "two stores with independent gear seeds must not produce identical chunk sets"
    );
}

/// M1: a store pins its chunk size; a conflicting --chunk-size must be
/// refused (silently accepting it would break dedup for all later puts).
#[test]
fn conflicting_chunk_size_is_refused() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(512 * 1024, 0x5EED));

    run_ok(
        bin(fx.home())
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(fx.repo())
            .args(["--chunk-size", "256k"]),
    );
    let out = bin(fx.home())
        .args(["put"])
        .arg(&input)
        .args(["--repo"])
        .arg(fx.repo())
        .args(["--chunk-size", "512k"])
        .output()
        .expect("spawning git-storage");
    assert!(
        !out.status.success(),
        "conflicting --chunk-size on an existing store must be refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pinned"),
        "error should explain the store is pinned, got: {stderr}"
    );
}

fn parse_counts(stdout: &str) -> (usize, usize) {
    // "<name>: <total> chunks, <new> new, ..."
    let after_colon = stdout.split(": ").nth(1).expect("summary line");
    let mut nums = after_colon
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<usize>().unwrap());
    let total = nums.next().expect("total chunk count");
    let new = nums.next().expect("new chunk count");
    (total, new)
}

fn object_names(objects_dir: &Path) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    for prefix in fs::read_dir(objects_dir).unwrap() {
        let prefix = prefix.unwrap().path();
        if prefix.is_dir() {
            for obj in fs::read_dir(&prefix).unwrap() {
                names.insert(obj.unwrap().file_name().to_string_lossy().into_owned());
            }
        }
    }
    names
}

fn find_first_object(objects_dir: &Path) -> std::path::PathBuf {
    for prefix in fs::read_dir(objects_dir).unwrap() {
        let prefix = prefix.unwrap().path();
        if prefix.is_dir() {
            if let Some(obj) = fs::read_dir(&prefix).unwrap().next() {
                return obj.unwrap().path();
            }
        }
    }
    panic!("no chunk objects found in {}", objects_dir.display());
}
