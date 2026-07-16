//! Milestone-0 integration tests: roundtrip, dedup, corruption detection.
//! These are the M0 exit criteria from IMPLEMENTATION-PLAN.md.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_git-storage"))
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

/// ~3 MiB of varied, non-repeating bytes (xorshift PRNG) so fixed-size chunks
/// all differ and dedup within one file cannot occur by accident.
fn varied_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x243F6A8885A308D3u64; // seed: pi digits
    let mut out = Vec::with_capacity(len);
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
fn roundtrip_is_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("store");
    let input = tmp.path().join("input.bin");
    let output = tmp.path().join("restored.bin");
    let data = varied_bytes(3 * 1024 * 1024 + 137); // ~3 MiB, non-aligned tail

    fs::write(&input, &data).unwrap();
    run_ok(
        bin()
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(&repo)
            .args(["--chunk-size", "256k"]),
    );
    run_ok(
        bin()
            .args(["get", "input.bin", "--output"])
            .arg(&output)
            .args(["--repo"])
            .arg(&repo),
    );

    let restored = fs::read(&output).unwrap();
    assert_eq!(data, restored, "roundtrip must be byte-identical");
}

#[test]
fn second_put_dedups_everything() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("store");
    let input = tmp.path().join("input.bin");
    fs::write(&input, varied_bytes(1024 * 1024)).unwrap();

    run_ok(
        bin()
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(&repo)
            .args(["--chunk-size", "256k"]),
    );
    let second = run_ok(
        bin()
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(&repo)
            .args(["--chunk-size", "256k"]),
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
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("store");
    let input = tmp.path().join("input.bin");
    let output = tmp.path().join("restored.bin");
    fs::write(&input, varied_bytes(600 * 1024)).unwrap();

    run_ok(
        bin()
            .args(["put"])
            .arg(&input)
            .args(["--repo"])
            .arg(&repo)
            .args(["--chunk-size", "256k"]),
    );

    // Flip one byte in the first chunk object found.
    let object = find_first_object(&repo.join("objects"));
    let mut bytes = fs::read(&object).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&object, &bytes).unwrap();

    let out = bin()
        .args(["get", "input.bin", "--output"])
        .arg(&output)
        .args(["--repo"])
        .arg(&repo)
        .output()
        .expect("spawning git-storage");
    assert!(!out.status.success(), "get must fail on corrupted object");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hash mismatch"),
        "error must name the hash mismatch, got: {stderr}"
    );
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
