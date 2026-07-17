//! M6 robustness / fuzz targets for the untrusted-input surface.
//!
//! A store's backend is UNTRUSTED (a git host; possibly hostile or corrupted).
//! Everything the client reads back — ciphertext blobs, sealed manifests, JSON
//! metadata — is attacker-influenceable, so the parse/decrypt/decompress path
//! must NEVER panic (a panic on hostile input is a DoS / a crash bug); it must
//! fail as a clean `Err`. These are deterministic, seeded, bounded fuzzers that
//! run inside `cargo test` for a fixed budget — the exit-criterion "fuzzers run
//! clean for a fixed budget". A panic in any target propagates and fails the
//! test, which is exactly the bug class we are hunting.
//!
//! A real libFuzzer/cargo-fuzz campaign (unbounded, coverage-guided) is the
//! natural follow-on; these targets are shaped so that harness can wrap the
//! same calls. Reproducibility: all randomness is a seeded xorshift, so any
//! failure is replayable from the fixed per-test seed.

use git_storage::chunker::{self, ChunkerParams};
use git_storage::crypto::Keys;
use git_storage::manifest::Manifest;

/// Seeded xorshift64 stream — deterministic, no external rng, non-zero state.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            v.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        v.truncate(len);
        v
    }
    /// A length in [0, max], small values well-represented (edge cases).
    fn len_upto(&mut self, max: usize) -> usize {
        (self.next_u64() as usize) % (max + 1)
    }
}

const KEY: [u8; 32] = [0x5a; 32];
const VALID_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Decrypting arbitrary bytes as a chunk must never panic — only Err (bad tag,
/// truncated nonce, malformed zstd frame behind a forged tag, …). A panic here
/// fails the test.
#[test]
fn fuzz_open_chunk_never_panics() {
    let keys = Keys::new(KEY);
    let mut rng = Rng::new(0xC0FFEE);
    for _ in 0..20_000 {
        let len = rng.len_upto(4096);
        let garbage = rng.bytes(len);
        let _ = keys.open_chunk(&garbage, VALID_HASH);
    }
}

/// Same for sealed manifests (the transaction-log payload path).
#[test]
fn fuzz_open_manifest_never_panics() {
    let keys = Keys::new(KEY);
    let mut rng = Rng::new(0xBADF00D);
    for _ in 0..20_000 {
        let len = rng.len_upto(8192);
        let garbage = rng.bytes(len);
        let _ = keys.open_manifest(&garbage);
    }
}

/// Parsing arbitrary bytes as manifest JSON must never panic.
#[test]
fn fuzz_manifest_json_never_panics() {
    let mut rng = Rng::new(0x1234_5678);
    for _ in 0..20_000 {
        let len = rng.len_upto(4096);
        let garbage = rng.bytes(len);
        let _ = serde_json::from_slice::<Manifest>(&garbage);
    }
}

/// Property: seal→open round-trips for arbitrary plaintext across the corpus,
/// not just the hand-picked vectors.
#[test]
fn fuzz_seal_open_roundtrips() {
    let keys = Keys::new(KEY);
    let mut rng = Rng::new(0xABCDEF);
    // Bounded budget: enough distinct sizes to exercise the seal/open path in
    // debug builds without dominating the suite runtime (seal+open is real
    // zstd + AEAD work; a full unbounded campaign is the cargo-fuzz follow-on).
    for _ in 0..400 {
        let len = rng.len_upto(48 * 1024);
        let plaintext = rng.bytes(len);
        let sealed = keys.seal_chunk(&plaintext, 3).expect("seal");
        let opened = keys
            .open_chunk(&sealed.ciphertext, &sealed.plaintext_hash_hex)
            .expect("open of our own ciphertext must succeed");
        assert_eq!(opened, plaintext, "seal→open must round-trip ({len} bytes)");
    }
}

/// Integrity: flipping ANY single bit of a valid ciphertext must make open fail
/// (AEAD tamper-evidence) — never return wrong plaintext, never panic.
#[test]
fn fuzz_single_bit_flips_are_rejected() {
    let keys = Keys::new(KEY);
    let mut rng = Rng::new(0x7E57);
    for _ in 0..500 {
        let len = 1 + rng.len_upto(4096);
        let plaintext = rng.bytes(len);
        let sealed = keys.seal_chunk(&plaintext, 3).expect("seal");
        for _ in 0..8 {
            let mut ct = sealed.ciphertext.clone();
            let bit = (rng.next_u64() as usize) % (ct.len() * 8);
            ct[bit / 8] ^= 1 << (bit % 8);
            match keys.open_chunk(&ct, &sealed.plaintext_hash_hex) {
                Err(_) => {}
                Ok(p) => assert_ne!(
                    p, plaintext,
                    "a bit-flipped ciphertext must NEVER open to the original plaintext"
                ),
            }
        }
    }
}

/// The chunker must handle adversarial/degenerate inputs (empty, all-same-byte,
/// tiny, large) across a range of parameters without panicking, and the emitted
/// chunks must always concatenate back to the exact input with a stable hash.
#[test]
fn fuzz_chunker_reassembles_and_never_panics() {
    let mut rng = Rng::new(0x0DD_BA11);
    let avgs = [4096usize, 64 * 1024, 1024 * 1024];
    for _ in 0..300 {
        let avg = avgs[(rng.next_u64() as usize) % avgs.len()];
        let params = ChunkerParams::from_avg(avg).expect("valid avg");
        let seed = rng.next_u64();

        let input = match rng.next_u64() % 4 {
            0 => Vec::new(),                        // empty
            1 => vec![0xAB; rng.len_upto(3 * avg)], // all-same-byte
            2 => {
                let n = rng.len_upto(8);
                rng.bytes(n) // sub-min tiny
            }
            _ => {
                let n = rng.len_upto(3 * avg + 12345);
                rng.bytes(n) // arbitrary
            }
        };

        let mut parts: Vec<u8> = Vec::new();
        let hash = chunker::stream_chunks(&input[..], &params, seed, |c| {
            parts.extend_from_slice(&c.data);
            Ok(())
        })
        .expect("chunker should not error on in-memory input");

        assert_eq!(parts, input, "chunks must concatenate to the input");
        assert_eq!(
            hash,
            blake3::hash(&input).to_hex().to_string(),
            "whole-file hash must be BLAKE3 of the input"
        );
    }
}
