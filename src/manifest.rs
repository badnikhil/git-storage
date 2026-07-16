//! File manifests: the milestone-0 metadata layer.
//!
//! MILESTONE 0: one pretty-printed JSON manifest per stored file, committed to
//! the store repo alongside the chunk objects. This is deliberately naive — the
//! target design replaces per-file JSON with an encrypted, CAS-serialized
//! transaction log (DESIGN.md Section 8). The manifest records everything `get`
//! needs to reconstruct and verify a file: the ordered chunk list and hashes.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One chunk reference: BLAKE3 hash (hex) of the chunk and its length in bytes.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkRef {
    pub hash: String,
    pub len: u64,
}

/// Manifest for one stored file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Logical name (the input file's name).
    pub name: String,
    /// Total file size in bytes.
    pub size: u64,
    /// Target average chunk size (content-defined chunking; actual chunk
    /// sizes vary between the store's min and max).
    pub avg_chunk_size: u64,
    /// BLAKE3 (hex) of the whole file, verified end-to-end on `get`.
    pub file_hash: String,
    /// Ordered chunk list; concatenating these reconstructs the file.
    pub chunks: Vec<ChunkRef>,
}

impl Manifest {
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serializing manifest")?;
        fs::write(path, json).with_context(|| format!("writing manifest {}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing manifest {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let m = Manifest {
            name: "example.bin".into(),
            size: 1234,
            avg_chunk_size: 1024,
            file_hash: "f".repeat(64),
            chunks: vec![
                ChunkRef {
                    hash: "a".repeat(64),
                    len: 1024,
                },
                ChunkRef {
                    hash: "b".repeat(64),
                    len: 210,
                },
            ],
        };
        let json = serde_json::to_string_pretty(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
