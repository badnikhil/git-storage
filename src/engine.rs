//! The storage engine: sealed segments + the manifest transaction log
//! (DESIGN.md Sections 3, 4, 8, 11, 13).
//!
//! Store layout (inside the `--repo` directory):
//! ```text
//! config.json          # chunker params, zstd level, checkpoint interval
//! index.git/           # bare; refs/heads/log = the transaction log
//! volumes/v0.git/      # bare; refs/segments/<id> = sealed segments
//! ```
//!
//! Write protocol (two-phase, DESIGN.md Section 8.3):
//!   Phase 1 — data first: new chunks are staged into ONE new sealed segment
//!     (a commit whose tree holds ciphertext blobs at fanout paths), pinned by
//!     refs/segments/<id>. Idempotent; a crash here leaves only orphans.
//!   Phase 2 — the commit point: one transaction commit is appended to the
//!     log via atomic CAS on refs/heads/log. Losers rebase and retry.
//!
//! Reads never consult anything but the log (single source of truth): a
//! reader loads the latest checkpoint plus the delta tail (Section 8.6). A
//! reader that pins a log commit OID gets an immutable snapshot (Section 13.3).
//!
//! Crash-injection hooks (GITSTORAGE_CRASH env) exist so tests can kill the
//! process at each phase boundary and verify the crash matrix (Section 11).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::chunker::{self, ChunkerParams};
use crate::crypto::Keys;
use crate::gitrepo::Bare;
use crate::manifest::{ChunkRef, Manifest};

const LOG_REF: &str = "refs/heads/log";
/// The single volume of milestone 3. M5 generalizes to a fixed budget of many.
const VOL0: &str = "v0";
/// Give up after this many consecutive CAS rejections (would indicate a bug
/// or a pathological writer storm, not normal contention).
const MAX_CAS_RETRIES: u32 = 32;

pub type Namespace = BTreeMap<String, Manifest>;

/// One transaction record — the (encrypted) payload of one log commit.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Txn {
    /// Incremental change: a file put (or removal, from M5 on).
    Delta {
        #[serde(skip_serializing_if = "Option::is_none")]
        put: Option<Manifest>,
        #[serde(skip_serializing_if = "Option::is_none")]
        remove: Option<String>,
    },
    /// Full-namespace snapshot (DESIGN.md Section 8.5).
    Checkpoint { namespace: Namespace },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoreConfig {
    pub version: u32,
    pub chunker: ChunkerParams,
    pub zstd_level: i32,
    /// Emit a checkpoint when the delta tail reaches this length.
    pub checkpoint_interval: u32,
}

pub struct PutStats {
    pub total_chunks: usize,
    pub new_chunks: usize,
    pub ciphertext_bytes: u64,
    pub size: u64,
    pub committed: bool,
}

pub struct Engine {
    index: Bare,
    volume: Bare,
    keys: Keys,
    config: StoreConfig,
}

impl Engine {
    /// Open (or initialize) the store at `root`.
    pub fn open(root: &Path, keys: Keys, requested_avg: Option<usize>) -> Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        let config = load_or_init_config(root, requested_avg)?;
        let index = Bare::open(&root.join("index.git"))?;
        let volume = Bare::open(&root.join("volumes").join(format!("{VOL0}.git")))?;
        Ok(Self {
            index,
            volume,
            keys,
            config,
        })
    }

    /// True if a store already exists at `root` (used to gate keyfile creation).
    pub fn store_exists(root: &Path) -> bool {
        root.join("config.json").exists()
    }

    pub fn config(&self) -> &StoreConfig {
        &self.config
    }

    /// Test hook: shrink the checkpoint interval.
    pub fn set_checkpoint_interval(&mut self, interval: u32) {
        self.config.checkpoint_interval = interval.max(1);
    }

    /// Current log tip (None = empty store).
    pub fn log_tip(&self) -> Result<Option<String>> {
        self.index.read_ref(LOG_REF)
    }

    /// The namespace at a pinned log commit (snapshot read, Section 13.3), or
    /// at the current tip when `at` is None.
    pub fn namespace_at(&self, at: Option<&str>) -> Result<Namespace> {
        Ok(self.load_state(at)?.0)
    }

    /// Store a byte stream under `name`. Returns stats. Safe under concurrent
    /// writers: the log CAS serializes commits; losers rebase and retry.
    pub fn put<R: Read>(&self, name: &str, reader: R) -> Result<PutStats> {
        let (namespace, _) = self.load_state(None)?;

        // Known placements: chunk_id -> (vol, seg) across the whole namespace.
        let mut known: BTreeMap<String, (String, String)> = BTreeMap::new();
        for m in namespace.values() {
            for c in &m.chunks {
                known.insert(c.id.clone(), (c.vol.clone(), c.seg.clone()));
            }
        }

        // Chunk, seal, and stage: new chunk blobs are written to the volume
        // immediately (loose objects, unreachable until the segment ref lands)
        // and recorded as entries of ONE new segment tree.
        let seg_id = random_id();
        let mut seg_entries: Vec<(String, String)> = Vec::new();
        let mut chunks: Vec<ChunkRef> = Vec::new();
        let mut new_chunks = 0usize;
        let mut ciphertext_bytes = 0u64;
        let mut size = 0u64;

        let gear_seed = self.keys.gear_seed;
        let file_hash = chunker::stream_chunks(reader, &self.config.chunker, gear_seed, |chunk| {
            size += chunk.data.len() as u64;
            let sealed = self.keys.seal_chunk(&chunk.data, self.config.zstd_level)?;
            let (vol, seg) = match known.get(&sealed.chunk_id) {
                Some(placement) => placement.clone(),
                None => {
                    let oid = self.volume.write_blob(&sealed.ciphertext)?;
                    seg_entries.push((fanout_path(&sealed.chunk_id), oid));
                    new_chunks += 1;
                    ciphertext_bytes += sealed.ciphertext.len() as u64;
                    let placement = (VOL0.to_string(), seg_id.clone());
                    known.insert(sealed.chunk_id.clone(), placement.clone());
                    placement
                }
            };
            chunks.push(ChunkRef {
                id: sealed.chunk_id,
                plaintext_hash: sealed.plaintext_hash_hex,
                len: chunk.data.len() as u64,
                vol,
                seg,
            });
            Ok(())
        })?;

        let manifest = Manifest {
            name: name.to_string(),
            size,
            avg_chunk_size: self.config.chunker.avg_size as u64,
            file_hash,
            chunks,
        };

        // Identical re-put: nothing new to store, namespace unchanged — no
        // transaction, no data churn (same invariant as M2's manifest
        // short-circuit; see agent-docs/milestone-2.md).
        if seg_entries.is_empty() && namespace.get(name) == Some(&manifest) {
            return Ok(PutStats {
                total_chunks: manifest.chunks.len(),
                new_chunks: 0,
                ciphertext_bytes: 0,
                size,
                committed: false,
            });
        }

        // ----- Phase 1: data first (idempotent) -----
        maybe_crash("before-segment"); // C1: blobs written, nothing reachable

        if !seg_entries.is_empty() {
            let tree = self.volume.write_tree(&seg_entries)?;
            let commit = self
                .volume
                .commit_tree(&tree, &[], &format!("segment {seg_id}"))?;
            self.volume
                .set_ref(&format!("refs/segments/{seg_id}"), &commit)?;
        }

        maybe_crash("after-segment"); // C2: segment reachable, log unaware

        // ----- Phase 2: the commit point (CAS, rebase-and-retry) -----
        let total_chunks = manifest.chunks.len();
        for _attempt in 0..MAX_CAS_RETRIES {
            let tip = self.log_tip()?;
            let (mut ns, since_checkpoint) = self.load_state(tip.as_deref())?;

            // Rebase check: if the winning writer already stored an identical
            // manifest for this name, our transaction is a no-op.
            if ns.get(name) == Some(&manifest) {
                return Ok(PutStats {
                    total_chunks,
                    new_chunks,
                    ciphertext_bytes,
                    size,
                    committed: false,
                });
            }

            let checkpoint_due = since_checkpoint + 1 >= self.config.checkpoint_interval as usize;
            let txn = if checkpoint_due {
                ns.insert(name.to_string(), manifest.clone());
                Txn::Checkpoint { namespace: ns }
            } else {
                Txn::Delta {
                    put: Some(manifest.clone()),
                    remove: None,
                }
            };

            let payload = self
                .keys
                .seal_manifest(&serde_json::to_vec(&txn).context("serializing txn")?)?;
            let blob = self.index.write_blob(&payload)?;
            let tree = self.index.write_tree(&[("txn".to_string(), blob)])?;
            let parents: Vec<String> = tip.iter().cloned().collect();
            let kind = if checkpoint_due { "checkpoint" } else { "txn" };
            let commit = self.index.commit_tree(&tree, &parents, kind)?;

            maybe_crash("before-cas"); // C3 boundary: commit built, CAS not issued

            if self.index.cas_ref(LOG_REF, &commit, tip.as_deref())? {
                maybe_crash("after-cas"); // C4: committed on backend, client work pending
                return Ok(PutStats {
                    total_chunks,
                    new_chunks,
                    ciphertext_bytes,
                    size,
                    committed: true,
                });
            }
            // CAS rejected: another writer advanced the log. Rebase (re-read
            // state) and retry. Our Phase-1 data is content-addressed and
            // already durable; only the transaction record is rebuilt.
        }
        bail!("log CAS failed {MAX_CAS_RETRIES} times — giving up (pathological contention?)");
    }

    /// Reconstruct `name` into `writer`, verifying every chunk (content
    /// address + AEAD tag + plaintext hash) and the whole-file hash.
    pub fn get<W: Write>(&self, name: &str, mut writer: W, at: Option<&str>) -> Result<u64> {
        let (namespace, _) = self.load_state(at)?;
        let manifest = namespace
            .get(name)
            .with_context(|| format!("no file {name:?} in this store (see `ls`)"))?;

        let mut file_hasher = blake3::Hasher::new();
        for c in &manifest.chunks {
            if c.vol != VOL0 {
                bail!("unknown volume {:?} (multi-volume arrives in M5)", c.vol);
            }
            let ciphertext = self
                .volume
                .read_blob_at(&format!("refs/segments/{}", c.seg), &fanout_path(&c.id))
                .with_context(|| format!("chunk {} missing from segment {}", c.id, c.seg))?;
            // Verify content address (ciphertext hash) before decrypting.
            let actual = blake3::hash(&ciphertext).to_hex().to_string();
            if actual != c.id {
                bail!(
                    "chunk hash mismatch: manifest expects {}, stored object hashes to {actual}",
                    c.id
                );
            }
            let plaintext = self.keys.open_chunk(&ciphertext, &c.plaintext_hash)?;
            if plaintext.len() as u64 != c.len {
                bail!(
                    "chunk {} length mismatch: manifest says {}, plaintext is {}",
                    c.id,
                    c.len,
                    plaintext.len()
                );
            }
            file_hasher.update(&plaintext);
            writer.write_all(&plaintext).context("writing output")?;
        }
        writer.flush().context("flushing output")?;

        let actual = file_hasher.finalize().to_hex().to_string();
        if actual != manifest.file_hash {
            bail!(
                "whole-file hash mismatch for {name}: manifest expects {}, got {actual}",
                manifest.file_hash
            );
        }
        Ok(manifest.size)
    }

    /// Debug/test: count (checkpoints, deltas) in the whole log.
    pub fn txn_kind_counts(&self) -> Result<(usize, usize)> {
        let Some(tip) = self.log_tip()? else {
            return Ok((0, 0));
        };
        let mut checkpoints = 0;
        let mut deltas = 0;
        for commit in self.index.rev_list(&tip)? {
            match self.read_txn(&commit)? {
                Txn::Checkpoint { .. } => checkpoints += 1,
                Txn::Delta { .. } => deltas += 1,
            }
        }
        Ok((checkpoints, deltas))
    }

    // ---------- internals ----------

    /// Load (namespace, deltas-since-last-checkpoint) at a pinned commit or
    /// the current tip. Reader model per DESIGN.md Section 8.6: walk back to
    /// the newest checkpoint, then apply the delta tail forward.
    fn load_state(&self, at: Option<&str>) -> Result<(Namespace, usize)> {
        let tip = match at {
            Some(oid) => oid.to_string(),
            None => match self.log_tip()? {
                Some(t) => t,
                None => return Ok((Namespace::new(), 0)),
            },
        };
        let mut deltas: Vec<Txn> = Vec::new();
        let mut base = Namespace::new();
        for commit in self.index.rev_list(&tip)? {
            match self.read_txn(&commit)? {
                Txn::Checkpoint { namespace } => {
                    base = namespace;
                    break;
                }
                delta => deltas.push(delta),
            }
        }
        let since_checkpoint = deltas.len();
        for txn in deltas.into_iter().rev() {
            if let Txn::Delta { put, remove } = txn {
                if let Some(m) = put {
                    base.insert(m.name.clone(), m);
                }
                if let Some(name) = remove {
                    base.remove(&name);
                }
            }
        }
        Ok((base, since_checkpoint))
    }

    fn read_txn(&self, commit: &str) -> Result<Txn> {
        let sealed = self.index.read_blob_at(commit, "txn")?;
        let plaintext = self.keys.open_manifest(&sealed)?;
        serde_json::from_slice(&plaintext).context("parsing transaction payload")
    }
}

/// Fanout path for a chunk inside a segment tree (DESIGN.md Section 4.3):
/// `<aa>/<bb>/<full-id>`, two 1-byte hex levels.
fn fanout_path(chunk_id: &str) -> String {
    format!("{}/{}/{}", &chunk_id[0..2], &chunk_id[2..4], chunk_id)
}

/// Random 128-bit hex ID for segments.
fn random_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Crash-injection hook for the crash-matrix tests (DESIGN.md Section 11).
/// Real code path, test-controlled trigger: if GITSTORAGE_CRASH names this
/// point, the process dies here exactly as a power loss would.
fn maybe_crash(point: &str) {
    if std::env::var("GITSTORAGE_CRASH").as_deref() == Ok(point) {
        eprintln!("simulated crash at {point}");
        std::process::exit(97);
    }
}

fn load_or_init_config(root: &Path, requested_avg: Option<usize>) -> Result<StoreConfig> {
    let path = root.join("config.json");
    if path.exists() {
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: StoreConfig =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        if let Some(avg) = requested_avg {
            if avg != config.chunker.avg_size {
                bail!(
                    "this store is pinned to a {}-byte average chunk size; \
                     --chunk-size {} would break dedup against existing data. \
                     Omit --chunk-size, or create a new store.",
                    config.chunker.avg_size,
                    avg
                );
            }
        }
        return Ok(config);
    }
    let avg = requested_avg.unwrap_or(chunker::DEFAULT_AVG_SIZE);
    let config = StoreConfig {
        version: crate::crypto::FORMAT_VERSION,
        chunker: ChunkerParams::from_avg(avg)?,
        zstd_level: 3,
        checkpoint_interval: 128,
    };
    let json = serde_json::to_string_pretty(&config).context("serializing store config")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(config)
}
