//! Reproducible benchmark harness (Milestone 6).
//!
//! One command:  `cargo run --release --example bench`
//!
//! Measures, against a local bare-repo backend (no network, so the numbers
//! isolate the chunk→compress→encrypt→git pipeline from wire latency):
//!   1. put / get throughput (MB/s) at the default 1 MiB average chunk size;
//!   2. dedup ratio on an edited-file workload (insert bytes mid-file, re-put);
//!   3. a chunk-size sweep (throughput + dedup vs average chunk size).
//!
//! All inputs are deterministic (seeded xorshift), so the workload is
//! reproducible run to run; only wall-clock timings vary. Numbers are printed,
//! not asserted — this is a measurement tool, not a test. Representative
//! results are recorded in agent-docs/milestone-6.md.

use std::io::sink;
use std::time::Instant;

use git_storage::crypto::Keys;
use git_storage::engine::Engine;
use tempfile::TempDir;

/// Deterministic incompressible-ish bytes (xorshift; seed must be non-zero).
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

fn mb_per_s(bytes: usize, secs: f64) -> f64 {
    (bytes as f64 / (1024.0 * 1024.0)) / secs
}

fn fresh_engine(avg: usize) -> (TempDir, Engine) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("store");
    let engine = Engine::open(&root, Keys::new([42u8; 32]), Some(avg)).unwrap();
    (tmp, engine)
}

fn main() {
    println!("git-storage benchmark (local bare backend, deterministic input)\n");

    // ---- 1. put / get throughput at the default 1 MiB average ----
    {
        const SIZE: usize = 64 * 1024 * 1024; // 64 MiB
        let (_tmp, engine) = fresh_engine(1024 * 1024);
        let data = varied_bytes(SIZE, 0x9E3779B97F4A7C15);

        let t = Instant::now();
        let stats = engine.put("bench.bin", &data[..]).unwrap();
        let put_s = t.elapsed().as_secs_f64();

        let t = Instant::now();
        let got = engine.get("bench.bin", sink(), None).unwrap();
        let get_s = t.elapsed().as_secs_f64();
        assert_eq!(got, SIZE as u64);

        println!("== throughput (64 MiB, avg 1 MiB chunks) ==");
        println!(
            "  put : {:7.1} MB/s   ({} chunks, {} new)",
            mb_per_s(SIZE, put_s),
            stats.total_chunks,
            stats.new_chunks
        );
        println!(
            "  get : {:7.1} MB/s   (all hashes + AEAD verified)",
            mb_per_s(SIZE, get_s)
        );
        println!();
    }

    // ---- 2. dedup on an edited-file workload ----
    {
        const SIZE: usize = 32 * 1024 * 1024; // 32 MiB
        let (_tmp, engine) = fresh_engine(1024 * 1024);
        let base = varied_bytes(SIZE, 0xD1B54A32D192ED03);
        let first = engine.put("edit.bin", &base[..]).unwrap();

        // Insert 1 KiB in the middle and re-put.
        let mut edited = base.clone();
        let ins = varied_bytes(1024, 0x100);
        edited.splice(SIZE / 2..SIZE / 2, ins);
        let second = engine.put("edit.bin", &edited[..]).unwrap();

        let deduped = second.total_chunks - second.new_chunks;
        let dedup_pct = 100.0 * deduped as f64 / second.total_chunks as f64;
        println!("== dedup on mid-file 1 KiB insert (32 MiB, avg 1 MiB) ==");
        println!(
            "  first put : {} chunks ({} new)",
            first.total_chunks, first.new_chunks
        );
        println!(
            "  re-put    : {} chunks, {} new, {} deduped ({:.1}% by count)",
            second.total_chunks, second.new_chunks, deduped, dedup_pct
        );
        println!("  (fixed-size chunking would re-write almost everything after the edit)");
        println!();
    }

    // ---- 3. chunk-size sweep ----
    {
        const SIZE: usize = 32 * 1024 * 1024; // 32 MiB
        println!("== chunk-size sweep (32 MiB; put MB/s + edit dedup) ==");
        println!(
            "  {:>8}  {:>9}  {:>7}  {:>12}",
            "avg", "put MB/s", "chunks", "edit dedup%"
        );
        for &avg in &[256 * 1024, 512 * 1024, 1024 * 1024, 2 * 1024 * 1024] {
            let (_tmp, engine) = fresh_engine(avg);
            let base = varied_bytes(SIZE, 0xA0761D6478BD642F);

            let t = Instant::now();
            let first = engine.put("sweep.bin", &base[..]).unwrap();
            let put_s = t.elapsed().as_secs_f64();

            let mut edited = base.clone();
            edited.splice(SIZE / 2..SIZE / 2, varied_bytes(1024, 0x200));
            let second = engine.put("sweep.bin", &edited[..]).unwrap();
            let dedup_pct = 100.0 * (second.total_chunks - second.new_chunks) as f64
                / second.total_chunks as f64;

            let label = if avg >= 1024 * 1024 {
                format!("{} MiB", avg / (1024 * 1024))
            } else {
                format!("{} KiB", avg / 1024)
            };
            println!(
                "  {:>8}  {:>9.1}  {:>7}  {:>11.1}%",
                label,
                mb_per_s(SIZE, put_s),
                first.total_chunks,
                dedup_pct
            );
        }
        println!();
    }

    println!("done. (numbers vary with hardware; workload is fixed/reproducible)");
}
