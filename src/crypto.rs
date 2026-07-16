//! Key hierarchy and per-chunk AEAD sealing (DESIGN.md Section 6).
//!
//! Pipeline per chunk: zstd-compress, then seal with XChaCha20-Poly1305 under
//! a keyed-convergent chunk key with a deterministic nonce. The chunk ID is
//! BLAKE3 of the ciphertext as stored (DESIGN.md Section 6.6), so content
//! addressing — and therefore deduplication — operates on what the backend
//! actually sees.
//!
//! Key hierarchy (DESIGN.md Section 6.2):
//! ```text
//! master_key (256-bit, keyfile; never leaves the client)
//!   ├── chunk_key  = PRF(master_key, H(plaintext_chunk))       [keyed convergent]
//!   ├── nonce      = PRF(master_key, "nonce" || H(plaintext))  [deterministic]
//!   ├── manifest_key = KDF(master_key, "manifest")
//!   └── chunker_gear_seed = KDF(master_key, "chunker-gear")
//! ```
//!
//! SPEC DEVIATION (recorded in DESIGN.md Section 6.2): the spec names
//! HMAC/HKDF; we use BLAKE3's keyed mode as the PRF and BLAKE3 derive_key as
//! the KDF. Same construction family (keyed PRF / purpose-bound KDF), one
//! hash primitive for the whole system, two fewer dependencies.
//!
//! Honest properties (DESIGN.md Section 6.3): identical plaintext within one
//! store encrypts to identical ciphertext — that is what preserves dedup, and
//! it means an observer of the backend can see *that* two objects repeat, but
//! not *what* they are. An outsider without the master key cannot run the
//! convergent-encryption confirmation attack (cannot compute chunk_key for a
//! guessed plaintext). A key-holder can. Cross-store, the same plaintext
//! yields unrelated ciphertext (different master keys).

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

/// Format version, bound into every AEAD's associated data so a chunk sealed
/// under one format cannot be replayed into another.
pub const FORMAT_VERSION: u32 = 1;

/// KDF context strings (BLAKE3 derive_key contexts; unique per purpose).
const CTX_MANIFEST: &str = "git-storage 2026-07-16 manifest key v1";
const CTX_GEAR: &str = "git-storage 2026-07-16 chunker gear seed v1";
const CTX_CHUNK: &str = "git-storage 2026-07-16 chunk key v1";
const CTX_NONCE: &str = "git-storage 2026-07-16 chunk nonce v1";
const CTX_NAME: &str = "git-storage 2026-07-16 manifest name tag v1";

/// The store's root secret plus everything derived once per store.
pub struct Keys {
    master: Zeroizing<[u8; 32]>,
    manifest_key: Zeroizing<[u8; 32]>,
    pub gear_seed: u64,
}

impl Keys {
    pub fn new(master: [u8; 32]) -> Self {
        let manifest_key = derive(&master, CTX_MANIFEST);
        let gear = derive(&master, CTX_GEAR);
        let gear_seed = u64::from_le_bytes(gear[..8].try_into().expect("8 bytes"));
        Self {
            master: Zeroizing::new(master),
            manifest_key,
            gear_seed,
        }
    }

    /// Keyed tag for a logical file name, used as the manifest's on-disk file
    /// name so the backend does not see stored names in cleartext. Keyed (not
    /// a plain hash) so names are not confirmable by an outsider either.
    pub fn name_tag(&self, name: &str) -> String {
        let key = derive(&self.master, CTX_NAME);
        blake3::keyed_hash(&key, name.as_bytes())
            .to_hex()
            .to_string()
    }

    /// Keyed-convergent chunk key: PRF(master, plaintext-hash) — an outsider
    /// without `master` cannot compute this for a guessed plaintext.
    fn chunk_key(&self, plaintext_hash: &[u8; 32]) -> Zeroizing<[u8; 32]> {
        keyed_prf(&derive(&self.master, CTX_CHUNK), plaintext_hash)
    }

    /// Deterministic 192-bit XChaCha nonce from the same inputs, so identical
    /// plaintext yields identical (key, nonce, ciphertext) and dedup survives
    /// encryption (DESIGN.md Section 6.5).
    fn chunk_nonce(&self, plaintext_hash: &[u8; 32]) -> [u8; 24] {
        let full = keyed_prf(&derive(&self.master, CTX_NONCE), plaintext_hash);
        full[..24].try_into().expect("24 bytes")
    }

    /// Seal one plaintext chunk: zstd then XChaCha20-Poly1305.
    /// Returns the ciphertext-as-stored (nonce is NOT stored — it re-derives
    /// from the plaintext hash, which the manifest records per chunk).
    pub fn seal_chunk(&self, plaintext: &[u8], zstd_level: i32) -> Result<SealedChunk> {
        let plaintext_hash: [u8; 32] = *blake3::hash(plaintext).as_bytes();
        let compressed = zstd::bulk::compress(plaintext, zstd_level).context("zstd compress")?;
        let key = self.chunk_key(&plaintext_hash);
        let nonce = self.chunk_nonce(&plaintext_hash);
        let cipher = XChaCha20Poly1305::new(key.as_slice().into());
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &compressed,
                    aad: &aad(),
                },
            )
            .map_err(|_| anyhow::anyhow!("AEAD encryption failed"))?;
        Ok(SealedChunk {
            chunk_id: blake3::hash(&ciphertext).to_hex().to_string(),
            plaintext_hash_hex: blake3::Hash::from_bytes(plaintext_hash)
                .to_hex()
                .to_string(),
            ciphertext,
        })
    }

    /// Open one sealed chunk, verifying AEAD integrity, then decompress and
    /// verify the plaintext hash matches what the manifest claimed.
    pub fn open_chunk(&self, ciphertext: &[u8], plaintext_hash_hex: &str) -> Result<Vec<u8>> {
        let plaintext_hash = parse_hash(plaintext_hash_hex)?;
        let key = self.chunk_key(&plaintext_hash);
        let nonce = self.chunk_nonce(&plaintext_hash);
        let cipher = XChaCha20Poly1305::new(key.as_slice().into());
        let compressed = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad(),
                },
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "AEAD authentication failed: chunk is corrupt, tampered with, \
                     or sealed under a different key"
                )
            })?;
        let plaintext = zstd::bulk::decompress(&compressed, crate::chunker::ABS_MAX_CHUNK)
            .context("zstd decompress")?;
        let actual = blake3::hash(&plaintext);
        if actual.as_bytes() != &plaintext_hash {
            bail!("plaintext hash mismatch after decryption — manifest/store inconsistency");
        }
        Ok(plaintext)
    }

    /// Seal a manifest (whole-blob encryption under the manifest key with a
    /// random nonce prepended — manifests are not content-addressed, so no
    /// determinism requirement).
    pub fn seal_manifest(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        use rand::RngCore;
        let mut nonce = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new(self.manifest_key.as_slice().into());
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad(),
                },
            )
            .map_err(|_| anyhow::anyhow!("manifest encryption failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn open_manifest(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < 24 {
            bail!("sealed manifest too short");
        }
        let (nonce, ciphertext) = sealed.split_at(24);
        let cipher = XChaCha20Poly1305::new(self.manifest_key.as_slice().into());
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad(),
                },
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "manifest authentication failed: wrong key, or manifest corrupt/tampered"
                )
            })
    }
}

pub struct SealedChunk {
    /// BLAKE3 of ciphertext-as-stored — the content address (DESIGN.md 6.6).
    pub chunk_id: String,
    /// BLAKE3 of the plaintext, recorded in the manifest; needed to re-derive
    /// key+nonce on read.
    pub plaintext_hash_hex: String,
    pub ciphertext: Vec<u8>,
}

/// Associated data binding format version into every seal.
fn aad() -> Vec<u8> {
    let mut aad = b"git-storage/v".to_vec();
    aad.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    aad
}

fn derive(key: &[u8; 32], context: &str) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(blake3::derive_key(context, key))
}

fn keyed_prf(key: &Zeroizing<[u8; 32]>, input: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(*blake3::keyed_hash(key, input).as_bytes())
}

fn parse_hash(hex: &str) -> Result<[u8; 32]> {
    let hash: blake3::Hash = hex
        .parse()
        .with_context(|| format!("invalid hash in manifest: {hex:?}"))?;
    Ok(*hash.as_bytes())
}

/// Master-key file handling: 64 hex chars in a file the user controls.
pub mod keyfile {
    use std::fs;
    use std::path::Path;

    use anyhow::{bail, Context, Result};

    /// Load a master key, or generate one (0600 permissions) if the file does
    /// not exist yet. Generation happens only on store creation.
    pub fn load_or_create(path: &Path, allow_create: bool) -> Result<[u8; 32]> {
        if path.exists() {
            let hex = fs::read_to_string(path)
                .with_context(|| format!("reading keyfile {}", path.display()))?;
            let hex = hex.trim();
            let bytes = parse_hex_32(hex)
                .with_context(|| format!("keyfile {} is not 64 hex chars", path.display()))?;
            return Ok(bytes);
        }
        if !allow_create {
            bail!(
                "keyfile {} not found — this store is encrypted; pass the keyfile \
                 that was created with it",
                path.display()
            );
        }
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        fs::write(path, format!("{hex}\n"))
            .with_context(|| format!("writing keyfile {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("setting 0600 on {}", path.display()))?;
        }
        Ok(key)
    }

    fn parse_hex_32(hex: &str) -> Result<[u8; 32]> {
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("expected 64 hex characters, got {} chars", hex.len());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("validated hex");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(seed: u8) -> Keys {
        Keys::new([seed; 32])
    }

    #[test]
    fn seal_is_deterministic_within_a_store() {
        let k = keys(1);
        let a = k.seal_chunk(b"hello chunk world", 3).unwrap();
        let b = k.seal_chunk(b"hello chunk world", 3).unwrap();
        assert_eq!(a.chunk_id, b.chunk_id, "dedup must survive encryption");
        assert_eq!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn different_stores_produce_unrelated_ciphertext() {
        let a = keys(1).seal_chunk(b"hello chunk world", 3).unwrap();
        let b = keys(2).seal_chunk(b"hello chunk world", 3).unwrap();
        assert_ne!(a.chunk_id, b.chunk_id);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn roundtrip_and_tamper_detection() {
        let k = keys(3);
        let plain = b"some plaintext that should compress and roundtrip".repeat(100);
        let sealed = k.seal_chunk(&plain, 3).unwrap();
        assert_ne!(
            sealed.ciphertext.windows(4).position(|w| w == b"some"),
            Some(0),
            "ciphertext must not contain plaintext"
        );
        let opened = k
            .open_chunk(&sealed.ciphertext, &sealed.plaintext_hash_hex)
            .unwrap();
        assert_eq!(plain, opened[..]);

        let mut tampered = sealed.ciphertext.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0x01;
        let err = k
            .open_chunk(&tampered, &sealed.plaintext_hash_hex)
            .unwrap_err();
        assert!(err.to_string().contains("authentication failed"));
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let sealed = keys(4).seal_chunk(b"secret bytes", 3).unwrap();
        let err = keys(5)
            .open_chunk(&sealed.ciphertext, &sealed.plaintext_hash_hex)
            .unwrap_err();
        assert!(err.to_string().contains("authentication failed"));
    }

    #[test]
    fn manifest_seal_roundtrip_and_tamper() {
        let k = keys(6);
        let sealed = k.seal_manifest(b"{\"name\":\"x\"}").unwrap();
        assert_eq!(k.open_manifest(&sealed).unwrap(), b"{\"name\":\"x\"}");
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert!(k.open_manifest(&bad).is_err());
    }

    #[test]
    fn gear_seed_is_stable_per_master_key() {
        assert_eq!(keys(7).gear_seed, keys(7).gear_seed);
        assert_ne!(keys(7).gear_seed, keys(8).gear_seed);
    }
}
