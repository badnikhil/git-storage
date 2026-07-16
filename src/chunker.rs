//! Content-defined chunking (FastCDC).
//!
//! MILESTONE 1: replaces milestone-0's fixed-size splitting. Chunk boundaries
//! are chosen by content (a rolling gear hash hitting a mask), so inserting or
//! deleting bytes shifts only the chunks local to the edit — everything after
//! re-synchronizes and dedups against the previous version. This is DESIGN.md
//! Section 5.
//!
//! Two deliberate details:
//! - The 256-entry gear table is generated from a per-store random seed
//!   (DESIGN.md Section 5.4): with a public, fixed gear table, the sequence of
//!   chunk sizes is a fingerprint of the content that survives encryption.
//!   A secret per-store gear closes that side channel. In milestone 2 the seed
//!   becomes HKDF-derived from the master key; for now it lives in the store
//!   config.
//! - We use contiguous bit-masks rather than the paper's spread masks. The
//!   expected cut probability is identical; only the size distribution shape
//!   differs slightly. Simpler, and revisitable if measurements ever care.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Smallest permitted *average* chunk size: 1 KiB.
pub const MIN_AVG_SIZE: usize = 1024;
/// Largest permitted *average* chunk size: 16 MiB (max chunk = 4x average =
/// 64 MiB, comfortably under git hosts' 100 MiB per-blob hard block).
pub const MAX_AVG_SIZE: usize = 16 * 1024 * 1024;
/// Default average chunk size: 1 MiB (DESIGN.md Section 5.2).
pub const DEFAULT_AVG_SIZE: usize = 1024 * 1024;

/// FastCDC parameters, pinned per store: a store MUST chunk identically for
/// its whole lifetime or dedup breaks (see StoreConfig).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkerParams {
    pub min_size: usize,
    pub avg_size: usize,
    pub max_size: usize,
    /// Seed for the gear table, hex-encoded u64.
    pub gear_seed: String,
}

impl ChunkerParams {
    /// Derive min/max from an average using the conventional FastCDC ratios
    /// (min = avg/2, max = avg*4) and a fresh random gear seed.
    pub fn from_avg(avg: usize, gear_seed: u64) -> Result<Self> {
        validate_avg_size(avg)?;
        Ok(Self {
            min_size: avg / 2,
            avg_size: avg,
            max_size: avg * 4,
            gear_seed: format!("{gear_seed:016x}"),
        })
    }

    fn seed(&self) -> Result<u64> {
        u64::from_str_radix(&self.gear_seed, 16)
            .with_context(|| format!("invalid gear_seed in store config: {:?}", self.gear_seed))
    }
}

/// Parse a `--chunk-size` argument (the target *average*): plain bytes or a
/// `k`/`m` suffix, e.g. `"512k"`, `"1m"`, `"1048576"`.
pub fn parse_chunk_size(s: &str) -> Result<usize> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024usize),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        _ => (s, 1usize),
    };
    let value: usize = num
        .trim()
        .parse()
        .with_context(|| format!("invalid chunk size: {s:?}"))?;
    let bytes = value
        .checked_mul(mult)
        .with_context(|| format!("chunk size overflow: {s:?}"))?;
    validate_avg_size(bytes)?;
    Ok(bytes)
}

fn validate_avg_size(bytes: usize) -> Result<()> {
    anyhow::ensure!(
        bytes >= MIN_AVG_SIZE,
        "average chunk size {bytes} bytes is below the {MIN_AVG_SIZE}-byte minimum (1 KiB)"
    );
    anyhow::ensure!(
        bytes <= MAX_AVG_SIZE,
        "average chunk size {bytes} bytes exceeds the {MAX_AVG_SIZE}-byte maximum (16 MiB)"
    );
    Ok(())
}

/// One produced chunk: its content and BLAKE3 hash (hex).
pub struct Chunk {
    pub hash: String,
    pub data: Vec<u8>,
}

/// splitmix64: tiny deterministic PRNG used only to expand the gear seed into
/// the 256-entry gear table. Not used for anything security-sensitive beyond
/// making the table unpredictable without the seed.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn gear_table(seed: u64) -> [u64; 256] {
    let mut state = seed;
    let mut table = [0u64; 256];
    for entry in table.iter_mut() {
        *entry = splitmix64(&mut state);
    }
    table
}

/// Mask with `bits` low bits set.
fn mask(bits: u32) -> u64 {
    (1u64 << bits) - 1
}

struct FastCdc {
    gear: [u64; 256],
    min_size: usize,
    avg_size: usize,
    max_size: usize,
    /// Stricter mask before the average point (cuts less likely)...
    mask_hard: u64,
    /// ...easier mask after it (cuts more likely). FastCDC normalization.
    mask_easy: u64,
}

impl FastCdc {
    fn new(params: &ChunkerParams) -> Result<Self> {
        let bits = (params.avg_size as f64).log2().round() as u32;
        Ok(Self {
            gear: gear_table(params.seed()?),
            min_size: params.min_size,
            avg_size: params.avg_size,
            max_size: params.max_size,
            mask_hard: mask(bits + 2),
            mask_easy: mask(bits.saturating_sub(2).max(1)),
        })
    }

    /// Find the cut point for the front of `buf`. `buf` is either at least
    /// `max_size` long or ends at EOF; the returned length is the next chunk.
    fn cut(&self, buf: &[u8]) -> usize {
        let len = buf.len();
        if len <= self.min_size {
            return len;
        }
        let limit = len.min(self.max_size);
        let normal = self.avg_size.min(limit);
        let mut hash: u64 = 0;
        // The first min_size bytes are skipped (never cut inside them); the
        // rolling hash warms up from there — standard FastCDC.
        for (i, &byte) in buf[self.min_size..normal].iter().enumerate() {
            hash = (hash << 1).wrapping_add(self.gear[byte as usize]);
            if hash & self.mask_hard == 0 {
                return self.min_size + i + 1;
            }
        }
        for (i, &byte) in buf[normal..limit].iter().enumerate() {
            hash = (hash << 1).wrapping_add(self.gear[byte as usize]);
            if hash & self.mask_easy == 0 {
                return normal + i + 1;
            }
        }
        limit
    }
}

/// Stream `reader` through FastCDC, invoking `on_chunk` per chunk. Feeds every
/// byte into a whole-file BLAKE3 hasher and returns the file hash (hex).
///
/// Memory bound: at most `max_size` bytes buffered at once — the whole file is
/// never resident.
pub fn stream_chunks<R, F>(mut reader: R, params: &ChunkerParams, mut on_chunk: F) -> Result<String>
where
    R: Read,
    F: FnMut(Chunk) -> Result<()>,
{
    let cdc = FastCdc::new(params)?;
    let mut file_hasher = blake3::Hasher::new();
    let mut buf: Vec<u8> = Vec::with_capacity(cdc.max_size);
    let mut read_buf = vec![0u8; 64 * 1024];
    let mut eof = false;

    loop {
        // Fill the window to max_size (or EOF) so cut() sees a full horizon.
        while !eof && buf.len() < cdc.max_size {
            let n = reader.read(&mut read_buf).context("reading input file")?;
            if n == 0 {
                eof = true;
                break;
            }
            file_hasher.update(&read_buf[..n]);
            buf.extend_from_slice(&read_buf[..n]);
        }
        if buf.is_empty() {
            break;
        }
        let end = cdc.cut(&buf);
        let data = buf[..end].to_vec();
        let hash = blake3::hash(&data).to_hex().to_string();
        on_chunk(Chunk { hash, data })?;
        buf.drain(..end);
    }

    Ok(file_hasher.finalize().to_hex().to_string())
}

/// Copy `data` into `writer` while feeding `hasher`; used on the read path so
/// the caller can verify the whole-file hash as chunks stream out.
pub fn write_and_hash<W: Write>(
    writer: &mut W,
    hasher: &mut blake3::Hasher,
    data: &[u8],
) -> Result<()> {
    hasher.update(data);
    writer.write_all(data).context("writing output chunk")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(avg: usize, seed: u64) -> ChunkerParams {
        ChunkerParams::from_avg(avg, seed).unwrap()
    }

    /// Deterministic varied bytes (xorshift) for boundary tests.
    fn varied(len: usize, mut state: u64) -> Vec<u8> {
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

    fn boundaries(data: &[u8], p: &ChunkerParams) -> Vec<usize> {
        let mut lens = Vec::new();
        stream_chunks(data, p, |c| {
            lens.push(c.data.len());
            Ok(())
        })
        .unwrap();
        lens
    }

    #[test]
    fn parse_suffixes_and_bounds() {
        assert_eq!(parse_chunk_size("1048576").unwrap(), 1024 * 1024);
        assert_eq!(parse_chunk_size("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_chunk_size("1M").unwrap(), 1024 * 1024);
        assert!(parse_chunk_size("1023").is_err()); // below 1 KiB avg
        assert!(parse_chunk_size("17m").is_err()); // above 16 MiB avg
        assert!(parse_chunk_size("bogus").is_err());
    }

    #[test]
    fn chunks_respect_min_max_and_cover_all_bytes() {
        let p = params(4096, 42);
        let data = varied(256 * 1024, 7);
        let lens = boundaries(&data, &p);
        let total: usize = lens.iter().sum();
        assert_eq!(total, data.len(), "chunks must cover every byte");
        // Every chunk except the last obeys min/max.
        for &len in &lens[..lens.len() - 1] {
            assert!(len >= p.min_size, "chunk {len} below min {}", p.min_size);
            assert!(len <= p.max_size, "chunk {len} above max {}", p.max_size);
        }
        assert!(*lens.last().unwrap() <= p.max_size);
        // With 256 KiB at 4 KiB average we expect tens of chunks.
        assert!(lens.len() > 16, "suspiciously few chunks: {}", lens.len());
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        let p = params(4096, 42);
        let lens = boundaries(&[], &p);
        assert!(lens.is_empty());
    }

    #[test]
    fn chunking_is_deterministic_for_same_seed() {
        let p = params(4096, 1234);
        let data = varied(128 * 1024, 99);
        assert_eq!(boundaries(&data, &p), boundaries(&data, &p));
    }

    #[test]
    fn different_seeds_give_different_boundaries() {
        let a = params(4096, 1);
        let b = params(4096, 2);
        let data = varied(256 * 1024, 99);
        assert_ne!(
            boundaries(&data, &a),
            boundaries(&data, &b),
            "different gear seeds must produce different chunk boundaries"
        );
    }

    #[test]
    fn insert_in_middle_preserves_most_chunks() {
        let p = params(4096, 77);
        let original = varied(512 * 1024, 5);
        let mut edited = original.clone();
        let mid = edited.len() / 2;
        edited.splice(mid..mid, [0xAAu8; 100]);

        let mut orig_hashes = std::collections::HashSet::new();
        stream_chunks(&original[..], &p, |c| {
            orig_hashes.insert(c.hash);
            Ok(())
        })
        .unwrap();

        let mut reused = 0usize;
        let mut reused_bytes = 0usize;
        let mut total = 0usize;
        let mut total_bytes = 0usize;
        stream_chunks(&edited[..], &p, |c| {
            total += 1;
            total_bytes += c.data.len();
            if orig_hashes.contains(&c.hash) {
                reused += 1;
                reused_bytes += c.data.len();
            }
            Ok(())
        })
        .unwrap();

        assert!(
            reused_bytes * 100 / total_bytes >= 80,
            "expected >=80% of bytes to dedup after a mid-file insert, got {}% ({reused}/{total} chunks)",
            reused_bytes * 100 / total_bytes
        );
    }
}
