//! File manifests: the metadata layer.
//!
//! MILESTONE 2: manifests are stored ENCRYPTED (sealed under the manifest key,
//! see crypto.rs) — the backend never sees names, sizes, or chunk lists in
//! cleartext. This module defines the plaintext shape; sealing happens in
//! store.rs. The target design replaces per-file manifests with an encrypted,
//! CAS-serialized transaction log (DESIGN.md Section 8).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One chunk reference.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkRef {
    /// Chunk ID = BLAKE3 (hex) of the ciphertext as stored — the content
    /// address in the object store (DESIGN.md Section 6.6).
    pub id: String,
    /// BLAKE3 (hex) of the chunk plaintext. Needed on read to re-derive the
    /// chunk key and nonce (keyed convergent encryption, Section 6.3/6.5).
    pub plaintext_hash: String,
    /// Plaintext length in bytes.
    pub len: u64,
}

/// Manifest for one stored file (plaintext form; sealed before hitting disk).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Logical name (the input file's name).
    pub name: String,
    /// Total plaintext file size in bytes.
    pub size: u64,
    /// Target average chunk size (content-defined chunking; actual chunk
    /// sizes vary between the store's min and max).
    pub avg_chunk_size: u64,
    /// BLAKE3 (hex) of the whole plaintext file, verified end-to-end on `get`.
    pub file_hash: String,
    /// Ordered chunk list; decrypt-and-concatenate reconstructs the file.
    pub chunks: Vec<ChunkRef>,
}

impl Manifest {
    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).context("serializing manifest")
    }

    pub fn from_json(data: &[u8]) -> Result<Self> {
        serde_json::from_slice(data).context("parsing manifest")
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
                    id: "a".repeat(64),
                    plaintext_hash: "c".repeat(64),
                    len: 1024,
                },
                ChunkRef {
                    id: "b".repeat(64),
                    plaintext_hash: "d".repeat(64),
                    len: 210,
                },
            ],
        };
        let back = Manifest::from_json(&m.to_json().unwrap()).unwrap();
        assert_eq!(m, back);
    }
}
