//! Integration tests: M0 (roundtrip, dedup, corruption) + M1 (insert-in-middle
//! dedup, per-store divergence, pinned chunk size) + M2 (encryption: dedup
//! survives sealing, backend opacity, wrong/missing key, tamper detection).
//!
//! GIT ISOLATION: every CLI invocation runs with the user's git configuration
//! blanked out (GIT_CONFIG_GLOBAL/SYSTEM pointed at /dev/null, HOME at the
//! tempdir, credential prompts disabled). Tests must never read the user's
//! gitconfig or credentials, and there is nothing to push to even if they did
//! (the CLI has no network git operations).

use std::fs;
use std::path::{Path, PathBuf};
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
    fn put(&self, input: &Path, extra: &[&str]) -> Output {
        run_ok(
            bin(self.home())
                .args(["put"])
                .arg(input)
                .args(["--repo"])
                .arg(self.repo())
                .args(["--keyfile"])
                .arg(self.keyfile())
                .args(extra),
        )
    }
    fn get(&self, name: &str, output: &Path) -> Output {
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
}

#[test]
fn roundtrip_is_byte_identical() {
    let fx = StoreFixture::new();
    let data = varied_bytes(3 * 1024 * 1024 + 137, 0x243F6A88); // non-aligned tail
    let input = fx.file("input.bin", &data);
    let output = fx.tmp.path().join("restored.bin");

    fx.put(&input, &["--chunk-size", "256k"]);
    fx.get("input.bin", &output);

    assert_eq!(
        data,
        fs::read(&output).unwrap(),
        "roundtrip must be byte-identical"
    );
}

/// M2 exit criterion 1: dedup survives encryption — identical plaintext,
/// same store → 0 new chunks.
#[test]
fn second_put_dedups_everything_through_encryption() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(1024 * 1024, 0x1234));

    fx.put(&input, &["--chunk-size", "256k"]);
    let second = fx.put(&input, &[]);

    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("0 new"),
        "second identical put must report 0 new chunks (deterministic encryption), got: {stdout}"
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

    fx.put(&input, &["--chunk-size", "64k"]);

    // Flip one byte in a git loose object inside the volume (M3 layout:
    // chunks are blobs in volumes/v0.git). Any single-bit corruption must
    // surface as a loud failure, never silent garbage.
    let object = find_first_loose_object(&fx.repo().join("volumes/v0.git/objects"));
    let mut bytes = fs::read(&object).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    // Git writes loose objects read-only; make writable to simulate bit rot.
    let mut perms = fs::metadata(&object).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&object, perms).unwrap();
    fs::write(&object, &bytes).unwrap();

    let out = bin(fx.home())
        .args(["get", "input.bin", "--output"])
        .arg(&output)
        .args(["--repo"])
        .arg(fx.repo())
        .args(["--keyfile"])
        .arg(fx.keyfile())
        .output()
        .expect("spawning git-storage");
    assert!(!out.status.success(), "get must fail on corrupted object");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("chunk") || stderr.contains("hash mismatch") || stderr.contains("txn"),
        "error must point at the corrupted data, got: {stderr}"
    );
}

/// M1 exit criterion 1 (still holding under encryption): after a mid-file
/// insert, most chunks must dedup against the original version.
#[test]
fn insert_in_middle_mostly_dedups() {
    let fx = StoreFixture::new();
    let original = varied_bytes(16 * 1024 * 1024, 0xBEEF);
    let mut edited = original.clone();
    edited.splice(8 * 1024 * 1024..8 * 1024 * 1024, [0xAAu8; 1024]);

    let orig_file = fx.file("data.bin", &original);
    fx.put(&orig_file, &["--chunk-size", "256k"]);

    fs::write(&orig_file, &edited).unwrap();
    let out = fx.put(&orig_file, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);

    let (total, new) = parse_counts(&stdout);
    let deduped = total - new;
    assert!(
        deduped * 100 / total >= 80,
        "expected >=80% chunk dedup after 1 KiB mid-file insert in 16 MiB, got {deduped}/{total}\noutput: {stdout}"
    );

    let restored = fx.tmp.path().join("restored.bin");
    fx.get("data.bin", &restored);
    assert_eq!(edited, fs::read(&restored).unwrap());
}

/// M1/M2: two stores with different master keys chunk AND encrypt differently
/// — no shared boundaries, no shared ciphertext.
#[test]
fn different_stores_share_nothing() {
    let fx = StoreFixture::new();
    let data = varied_bytes(4 * 1024 * 1024, 0xCAFE);
    let input = fx.file("input.bin", &data);

    let mut object_sets = Vec::new();
    for label in ["a", "b"] {
        let repo = fx.tmp.path().join(format!("store-{label}"));
        let keyfile = fx.tmp.path().join(format!("master-{label}.key"));
        run_ok(
            bin(fx.home())
                .args(["put"])
                .arg(&input)
                .args(["--repo"])
                .arg(&repo)
                .args(["--keyfile"])
                .arg(&keyfile)
                .args(["--chunk-size", "256k"]),
        );
        // M3 layout: chunk ciphertext lives as git blobs in the volume;
        // compare the sets of loose-object names (derived from ciphertext).
        object_sets.push(object_names(&repo.join("volumes/v0.git/objects")));
    }

    assert!(
        object_sets[0].is_disjoint(&object_sets[1]),
        "two stores with different master keys must share zero chunk IDs"
    );
}

/// M2 exit criterion 3: backend opacity — nothing plaintext-derived is
/// readable anywhere in the store repo.
#[test]
fn store_repo_contains_no_plaintext() {
    let fx = StoreFixture::new();
    // Highly recognizable plaintext, repeated so zstd would love it.
    let needle = b"TOP-SECRET-NEEDLE-0xDEADBEEF";
    let data: Vec<u8> = needle.repeat(200_000); // ~5.4 MiB
    let input = fx.file("secrets.txt", &data);
    fx.put(&input, &["--chunk-size", "256k"]);

    // Scan every file in the store repo (including .git) for the needle and
    // for the logical file name.
    let mut scanned = 0;
    for path in walk(&fx.repo()) {
        let bytes = fs::read(&path).unwrap();
        assert!(
            find_subslice(&bytes, needle).is_none(),
            "plaintext needle found in {}",
            path.display()
        );
        if path.file_name().is_some_and(|n| n != "config.json") {
            assert!(
                find_subslice(&bytes, b"secrets.txt").is_none(),
                "file name leaked in {}",
                path.display()
            );
        }
        scanned += 1;
    }
    assert!(scanned > 5, "expected to scan several files, got {scanned}");
}

/// M2 exit criterion: wrong key must fail loudly, not produce garbage.
#[test]
fn wrong_key_fails_cleanly() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(512 * 1024, 0x5EED));
    fx.put(&input, &["--chunk-size", "256k"]);

    // Attempt get with a DIFFERENT keyfile.
    let wrong_key = fx.tmp.path().join("wrong.key");
    fs::write(&wrong_key, format!("{}\n", "ab".repeat(32))).unwrap();
    let out = bin(fx.home())
        .args(["get", "input.bin", "--output"])
        .arg(fx.tmp.path().join("out.bin"))
        .args(["--repo"])
        .arg(fx.repo())
        .args(["--keyfile"])
        .arg(&wrong_key)
        .output()
        .expect("spawning git-storage");
    assert!(!out.status.success(), "wrong key must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("authentication failed") || stderr.contains("no file"),
        "wrong-key failure must be an auth/lookup error, got: {stderr}"
    );
}

/// M2: a missing keyfile on an EXISTING store must refuse (not regenerate and
/// silently orphan the data).
#[test]
fn missing_keyfile_on_existing_store_is_refused() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(128 * 1024, 0xF00D));
    fx.put(&input, &["--chunk-size", "64k"]);

    fs::remove_file(fx.keyfile()).unwrap();
    let out = bin(fx.home())
        .args(["ls", "--repo"])
        .arg(fx.repo())
        .args(["--keyfile"])
        .arg(fx.keyfile())
        .output()
        .expect("spawning git-storage");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("keyfile") && stderr.contains("not found"),
        "must explain the keyfile is missing, got: {stderr}"
    );
}

/// M1: a store pins its chunk size; a conflicting --chunk-size must be
/// refused (silently accepting it would break dedup for all later puts).
#[test]
fn conflicting_chunk_size_is_refused() {
    let fx = StoreFixture::new();
    let input = fx.file("input.bin", &varied_bytes(512 * 1024, 0x5EED));

    fx.put(&input, &["--chunk-size", "256k"]);
    let out = bin(fx.home())
        .args(["put"])
        .arg(&input)
        .args(["--repo"])
        .arg(fx.repo())
        .args(["--keyfile"])
        .arg(fx.keyfile())
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

// ---------- helpers ----------

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

/// Names of git loose objects under a bare repo's objects/ dir (two-hex-char
/// fanout; skips info/ and pack/). Full OID = prefix + filename.
fn object_names(objects_dir: &Path) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    for prefix in fs::read_dir(objects_dir).unwrap() {
        let prefix = prefix.unwrap().path();
        let dirname = prefix.file_name().unwrap().to_string_lossy().into_owned();
        if prefix.is_dir() && dirname.len() == 2 {
            for obj in fs::read_dir(&prefix).unwrap() {
                let name = obj.unwrap().file_name().to_string_lossy().into_owned();
                names.insert(format!("{dirname}{name}"));
            }
        }
    }
    names
}

/// First git loose object file under a bare repo's objects/ dir.
fn find_first_loose_object(objects_dir: &Path) -> PathBuf {
    for prefix in fs::read_dir(objects_dir).unwrap() {
        let prefix = prefix.unwrap().path();
        let dirname = prefix.file_name().unwrap().to_string_lossy().into_owned();
        if prefix.is_dir() && dirname.len() == 2 {
            if let Some(obj) = fs::read_dir(&prefix).unwrap().next() {
                return obj.unwrap().path();
            }
        }
    }
    panic!("no loose objects found in {}", objects_dir.display());
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
