//! Fixed-size chunking.
//!
//! MILESTONE 0 (walking skeleton): chunks are fixed-size slices of the byte
//! stream. This is deliberately NOT the target algorithm. DESIGN.md §5 specifies
//! FastCDC content-defined chunking with a per-store randomized gear seed; that
//! replaces this module later. Fixed-size chunking has no shift-resistance (an
//! insertion near the front re-chunks everything after it), which is exactly the
//! property FastCDC buys. We keep it here only because it is the simplest thing
//! that exercises the store/manifest/git plumbing.

use std::io::{Read, Write};

use anyhow::{Context, Result};

/// Minimum accepted chunk size: 1 KiB.
pub const MIN_CHUNK_SIZE: usize = 1024;
/// Maximum accepted chunk size: 90 MiB. Kept under git hosts' 100 MiB per-blob
/// hard block (DESIGN.md Appendix A / README platform table) with headroom.
pub const MAX_CHUNK_SIZE: usize = 90 * 1024 * 1024;
/// Default chunk size: 1 MiB (matches DESIGN.md §5.2 PROPOSED avg, though here it
/// is a fixed size rather than an average).
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Parse a `--chunk-size` argument accepting plain bytes or a `k`/`m` suffix
/// (case-insensitive): e.g. `"512k"`, `"1m"`, `"1048576"`.
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
    validate_chunk_size(bytes)?;
    Ok(bytes)
}

/// Reject chunk sizes outside the accepted range.
pub fn validate_chunk_size(bytes: usize) -> Result<()> {
    anyhow::ensure!(
        bytes >= MIN_CHUNK_SIZE,
        "chunk size {bytes} bytes is below the {MIN_CHUNK_SIZE}-byte minimum (1 KiB)"
    );
    anyhow::ensure!(
        bytes <= MAX_CHUNK_SIZE,
        "chunk size {bytes} bytes exceeds the {MAX_CHUNK_SIZE}-byte maximum (90 MiB)"
    );
    Ok(())
}

/// One produced chunk: its content and BLAKE3 hash (hex).
pub struct Chunk {
    pub hash: String,
    pub data: Vec<u8>,
}

/// Stream `reader` in fixed-size chunks, invoking `on_chunk` for each. Also feeds
/// every byte into a whole-file BLAKE3 hasher and returns the final file hash
/// (hex) once the stream is exhausted.
///
/// The whole file is never held in memory: at most `chunk_size` bytes plus a
/// small read buffer are resident at once.
pub fn stream_chunks<R, F>(mut reader: R, chunk_size: usize, mut on_chunk: F) -> Result<String>
where
    R: Read,
    F: FnMut(Chunk) -> Result<()>,
{
    validate_chunk_size(chunk_size)?;

    let mut file_hasher = blake3::Hasher::new();
    // Reusable buffer for the current chunk under construction.
    let mut buf = vec![0u8; chunk_size];
    let mut filled = 0usize;

    loop {
        // Fill `buf` up to `chunk_size`, tolerating short reads.
        let n = reader
            .read(&mut buf[filled..])
            .context("reading input file")?;
        if n == 0 {
            break;
        }
        file_hasher.update(&buf[filled..filled + n]);
        filled += n;
        if filled == chunk_size {
            emit(&mut on_chunk, &buf[..filled])?;
            filled = 0;
        }
    }
    // Trailing partial chunk.
    if filled > 0 {
        emit(&mut on_chunk, &buf[..filled])?;
    }

    Ok(file_hasher.finalize().to_hex().to_string())
}

fn emit<F>(on_chunk: &mut F, data: &[u8]) -> Result<()>
where
    F: FnMut(Chunk) -> Result<()>,
{
    let hash = blake3::hash(data).to_hex().to_string();
    on_chunk(Chunk {
        hash,
        data: data.to_vec(),
    })
}

/// Copy `data` into `writer` while feeding `hasher`; used on the read path so the
/// caller can verify a chunk's hash as it is streamed out.
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

    #[test]
    fn parse_suffixes() {
        assert_eq!(parse_chunk_size("1048576").unwrap(), 1024 * 1024);
        assert_eq!(parse_chunk_size("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_chunk_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_chunk_size("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_chunk_size("1M").unwrap(), 1024 * 1024);
    }

    #[test]
    fn parse_rejects_out_of_range() {
        assert!(parse_chunk_size("1023").is_err()); // < 1 KiB
        assert!(parse_chunk_size("91m").is_err()); // > 90 MiB
        assert!(parse_chunk_size("bogus").is_err());
    }

    #[test]
    fn chunks_cover_all_bytes() {
        let data = vec![7u8; 1000];
        let mut collected = Vec::new();
        let file_hash = stream_chunks(&data[..], MIN_CHUNK_SIZE, |c| {
            collected.push(c);
            Ok(())
        })
        .unwrap();
        // 1000 bytes with 1 KiB chunk => a single partial chunk.
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].data.len(), 1000);
        assert_eq!(file_hash, blake3::hash(&data).to_hex().to_string());
    }

    #[test]
    fn multiple_chunks_exact_and_partial() {
        let data = vec![3u8; MIN_CHUNK_SIZE * 2 + 5];
        let mut lens = Vec::new();
        stream_chunks(&data[..], MIN_CHUNK_SIZE, |c| {
            lens.push(c.data.len());
            Ok(())
        })
        .unwrap();
        assert_eq!(lens, vec![MIN_CHUNK_SIZE, MIN_CHUNK_SIZE, 5]);
    }
}
