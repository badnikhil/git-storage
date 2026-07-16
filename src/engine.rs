//! The storage engine: sealed segments + the manifest transaction log
//! (DESIGN.md Sections 3, 4, 8, 11, 13), running over the [`Backend`] trait
//! (Section 2.1) so the same engine drives local bare repos or remote git
//! servers unchanged (Milestone 4).
//!
//! Store layout (inside the `--repo` directory):
//! ```text
//! config.json          # chunker params, zstd level, checkpoint interval,
//!                      # AND the fixed volume set (M4)
//! index.git/           # the transaction log (local bare OR a remote mirror)
//! volumes/<id>.git/    # sealed segments per volume (local bare OR mirror)
//! ```
//!
//! Write protocol (two-phase, DESIGN.md Section 8.3):
//!   Phase 1 — data first: new chunks are staged into ONE new sealed segment
//!     (a commit whose tree holds ciphertext blobs at fanout paths), pinned by
//!     refs/segments/<id> in a SELECTED volume (Section 9.3). Idempotent; a
//!     crash here leaves only orphans.
//!   Phase 2 — the commit point: one transaction commit is appended to the
//!     log via atomic CAS on refs/heads/log. Losers rebase and retry.
//!
//! Volume selection (Section 9.3 / 15): pick the volume with the most free
//! headroom below its volume-full threshold; when N ≥ 3 volumes are declared,
//! reserve one as the compaction spare (Section 15.5) and never place ordinary
//! writes there. If no volume can accept the segment, REFUSE the write — the
//! budget wall (Section 15.3). The fleet never grows automatically.
//!
//! Reads never consult anything but the log (single source of truth): a
//! reader loads the latest checkpoint plus the delta tail (Section 8.6). A
//! reader that pins a log commit OID gets an immutable snapshot (Section 13.3).
//!
//! Crash-injection hooks (GITSTORAGE_CRASH env) exist so tests can kill the
//! process at each phase boundary and verify the crash matrix (Section 11).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, LocalBackend, RemoteBackend};
use crate::chunker::{self, ChunkerParams};
use crate::crypto::Keys;
use crate::manifest::{ChunkRef, Manifest};

const LOG_REF: &str = "refs/heads/log";
/// The default single volume of a back-compat (M3-style) local store.
const VOL0: &str = "v0";
/// Give up after this many consecutive CAS rejections (would indicate a bug
/// or a pathological writer storm, not normal contention).
const MAX_CAS_RETRIES: u32 = 32;
/// Default per-volume full threshold when config omits it (DESIGN §15.2).
pub const DEFAULT_VOLUME_FULL_THRESHOLD: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB
/// Spare slot becomes mandatory at N ≥ this many volumes (DESIGN §15.5).
const SPARE_SLOT_MIN_VOLUMES: usize = 3;

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

/// One declared volume in the fixed volume set (DESIGN §15.1). `url` absent =
/// a local bare repo under `volumes/<id>.git` (the M3 transport); present =
/// a remote git URL (file://, https://, ssh://).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub push_interval_ms: u64,
    #[serde(default = "default_threshold")]
    pub volume_full_threshold: u64,
}

fn default_threshold() -> u64 {
    DEFAULT_VOLUME_FULL_THRESHOLD
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoreConfig {
    pub version: u32,
    pub chunker: ChunkerParams,
    pub zstd_level: i32,
    /// Emit a checkpoint when the delta tail reaches this length.
    pub checkpoint_interval: u32,
    /// The fixed volume set (M4). Absent in an M3 store; back-compat synthesizes
    /// a single local `v0` (see [`load_or_init_config`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<VolumeConfig>>,
    /// Optional URL for the index/log repo (M4). Absent = local `index.git`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_url: Option<String>,
}

pub struct PutStats {
    pub total_chunks: usize,
    pub new_chunks: usize,
    pub ciphertext_bytes: u64,
    pub size: u64,
    pub committed: bool,
    /// Which volume the new segment landed in (None if no new segment).
    pub volume: Option<String>,
}

/// A live volume: its backend transport plus its declared metadata.
struct VolumeHandle {
    id: String,
    backend: Box<dyn Backend>,
    threshold: u64,
    /// True if reserved as the compaction spare (DESIGN §15.5): no ordinary
    /// writes land here.
    spare: bool,
}

impl VolumeHandle {
    /// Current live bytes: sum of blob sizes across all segment trees. This is
    /// an upper bound on stored bytes (git may dedup identical blobs), which is
    /// the safe side for a budget wall.
    fn used_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        for (refname, _oid) in self.backend.list_refs("refs/segments/")? {
            for (_path, size) in self.backend.ls_tree_sizes(&refname)? {
                total += size;
            }
        }
        Ok(total)
    }
}

pub struct Engine {
    index: Box<dyn Backend>,
    volumes: Vec<VolumeHandle>,
    keys: Keys,
    config: StoreConfig,
}

impl Engine {
    /// Open (or initialize) the store at `root`.
    pub fn open(root: &Path, keys: Keys, requested_avg: Option<usize>) -> Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        let config = load_or_init_config(root, requested_avg)?;
        let index = open_index(root, &config)?;
        let volumes = open_volumes(root, &config)?;
        Ok(Self {
            index,
            volumes,
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

    /// Per-volume (id, used_bytes, threshold, spare) for `stats`/tests.
    pub fn volume_usage(&self) -> Result<Vec<(String, u64, u64, bool)>> {
        self.volumes
            .iter()
            .map(|v| Ok((v.id.clone(), v.used_bytes()?, v.threshold, v.spare)))
            .collect()
    }

    /// A note about how the volume backends serve reads (promisor verdict etc).
    pub fn read_path_notes(&self) -> Vec<(String, String)> {
        self.volumes
            .iter()
            .filter_map(|v| v.backend.read_path_note().map(|n| (v.id.clone(), n)))
            .collect()
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

        // Chunk + seal, BUFFERING new ciphertext in memory. We must know the
        // segment's total size before choosing a volume (budget wall needs the
        // projected size), and where the segment lands before writing blobs.
        // Segments are bounded (≈512 MiB target), so buffering is acceptable.
        let seg_id = random_id();
        struct StagedChunk {
            chunk_id: String,
            ciphertext: Vec<u8>,
        }
        let mut staged: Vec<StagedChunk> = Vec::new();
        // Chunk-ids already staged in THIS put, so a chunk repeated within the
        // same file is buffered/counted once (matches M3's in-closure dedup).
        let mut staged_ids: BTreeSet<String> = BTreeSet::new();
        // (chunk metadata, placement-to-fill-later flag). vol/seg for existing
        // chunks is known; for new ones we fill after volume selection.
        let mut chunk_meta: Vec<(ChunkRef, bool)> = Vec::new();
        let mut new_chunks = 0usize;
        let mut ciphertext_bytes = 0u64;
        let mut size = 0u64;

        let gear_seed = self.keys.gear_seed;
        let file_hash = chunker::stream_chunks(reader, &self.config.chunker, gear_seed, |chunk| {
            size += chunk.data.len() as u64;
            let sealed = self.keys.seal_chunk(&chunk.data, self.config.zstd_level)?;
            let is_new = !known.contains_key(&sealed.chunk_id);
            let (vol, seg) = match known.get(&sealed.chunk_id) {
                Some(placement) => placement.clone(),
                None => {
                    // Placeholder placement, resolved after volume selection.
                    // Buffer + count each distinct new chunk once per put.
                    if staged_ids.insert(sealed.chunk_id.clone()) {
                        ciphertext_bytes += sealed.ciphertext.len() as u64;
                        new_chunks += 1;
                        staged.push(StagedChunk {
                            chunk_id: sealed.chunk_id.clone(),
                            ciphertext: sealed.ciphertext.clone(),
                        });
                    }
                    (String::new(), seg_id.clone())
                }
            };
            chunk_meta.push((
                ChunkRef {
                    id: sealed.chunk_id,
                    plaintext_hash: sealed.plaintext_hash_hex,
                    len: chunk.data.len() as u64,
                    vol,
                    seg,
                },
                is_new,
            ));
            Ok(())
        })?;

        // Select the destination volume for the NEW segment (if any).
        let target_vol = if staged.is_empty() {
            None
        } else {
            Some(self.select_volume(ciphertext_bytes)?)
        };
        if let Some(vi) = target_vol {
            let vid = self.volumes[vi].id.clone();
            // Fill the placeholder placements for new chunks now that we know
            // the destination volume.
            for (c, is_new) in chunk_meta.iter_mut() {
                if *is_new {
                    c.vol = vid.clone();
                }
            }
        }

        let chunks: Vec<ChunkRef> = chunk_meta.into_iter().map(|(c, _)| c).collect();
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
        if staged.is_empty() && namespace.get(name) == Some(&manifest) {
            return Ok(PutStats {
                total_chunks: manifest.chunks.len(),
                new_chunks: 0,
                ciphertext_bytes: 0,
                size,
                committed: false,
                volume: None,
            });
        }

        // ----- Phase 1: data first (idempotent) -----
        maybe_crash("before-segment"); // C1: blobs written, nothing reachable

        let landed_volume = if let Some(vi) = target_vol {
            let vol = &self.volumes[vi];
            // `staged` is already de-duplicated by chunk_id (see chunking loop).
            let mut seg_entries: Vec<(String, String)> = Vec::new();
            for sc in &staged {
                let oid = vol.backend.write_blob(&sc.ciphertext)?;
                seg_entries.push((fanout_path(&sc.chunk_id), oid));
            }
            let tree = vol.backend.write_tree(&seg_entries)?;
            let commit = vol
                .backend
                .commit_tree(&tree, &[], &format!("segment {seg_id}"))?;
            vol.backend
                .set_ref(&format!("refs/segments/{seg_id}"), &commit)?;
            Some(vol.id.clone())
        } else {
            None
        };

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
                    volume: landed_volume,
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
                    volume: landed_volume,
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
            let vol = self
                .volumes
                .iter()
                .find(|v| v.id == c.vol)
                .with_context(|| format!("unknown volume {:?} for chunk {}", c.vol, c.id))?;
            let ciphertext = vol
                .backend
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

    /// Choose the destination volume for a new segment of `projected` bytes
    /// (DESIGN §9.3): among the WRITABLE volumes (spare excluded when N ≥ 3),
    /// the one with the most free headroom below its threshold whose projected
    /// post-write size still fits. Ties broken by lowest volume ID. If none can
    /// accept the segment, the budget wall refuses (DESIGN §15.3).
    fn select_volume(&self, projected: u64) -> Result<usize> {
        let mut best: Option<(usize, u64)> = None; // (index, headroom)
        for (i, v) in self.volumes.iter().enumerate() {
            if v.spare {
                continue; // reserved for compaction (DESIGN §15.5)
            }
            let used = v.used_bytes()?;
            let after = used.saturating_add(projected);
            if after > v.threshold {
                continue; // would breach the volume-full threshold
            }
            let headroom = v.threshold - used;
            match best {
                Some((_, best_hr)) if headroom <= best_hr => {}
                _ => best = Some((i, headroom)),
            }
        }
        match best {
            Some((i, _)) => Ok(i),
            None => bail!(
                "budget exhausted: no volume can accept a {}-byte segment \
                 without exceeding its volume-full threshold. The fleet does \
                 NOT grow automatically — free space (compaction/removal) or \
                 declare a larger volume set, then retry.",
                projected
            ),
        }
    }

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

/// Open the index/log backend (local bare, or a remote mirror).
fn open_index(root: &Path, config: &StoreConfig) -> Result<Box<dyn Backend>> {
    match &config.index_url {
        None => Ok(Box::new(LocalBackend::open(&root.join("index.git"))?)),
        Some(url) => {
            let mirror = root.join("index.git"); // local staging/cache mirror
            Ok(Box::new(RemoteBackend::open(url, &mirror, 0)?))
        }
    }
}

/// Open every declared volume, marking the spare (highest ID) when N ≥ 3.
fn open_volumes(root: &Path, config: &StoreConfig) -> Result<Vec<VolumeHandle>> {
    let declared = config
        .volumes
        .clone()
        .unwrap_or_else(|| vec![back_compat_volume()]);
    let n = declared.len();
    let spare_idx = if n >= SPARE_SLOT_MIN_VOLUMES {
        Some(n - 1) // reserve the last-declared volume as the spare
    } else {
        None
    };
    let mut handles = Vec::with_capacity(n);
    for (i, vc) in declared.into_iter().enumerate() {
        let backend: Box<dyn Backend> = match &vc.url {
            None => {
                let dir = root.join("volumes").join(format!("{}.git", vc.id));
                Box::new(LocalBackend::open(&dir)?)
            }
            Some(url) => {
                let mirror = root.join("volumes").join(format!("{}.git", vc.id));
                Box::new(RemoteBackend::open(url, &mirror, vc.push_interval_ms)?)
            }
        };
        handles.push(VolumeHandle {
            id: vc.id,
            backend,
            threshold: vc.volume_full_threshold,
            spare: Some(i) == spare_idx,
        });
    }
    Ok(handles)
}

/// The synthesized single volume for an M3 (pre-M4) store with no volume set.
fn back_compat_volume() -> VolumeConfig {
    VolumeConfig {
        id: VOL0.to_string(),
        url: None,
        push_interval_ms: 0,
        volume_full_threshold: DEFAULT_VOLUME_FULL_THRESHOLD,
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
        volumes: None, // back-compat single local v0 by default
        index_url: None,
    };
    let json = serde_json::to_string_pretty(&config).context("serializing store config")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(config)
}

/// Write a store config that declares a fixed volume set (M4 `init`). Fails if
/// a config already exists (init is create-only).
pub fn init_config_with_volumes(
    root: &Path,
    avg: Option<usize>,
    volumes: Vec<VolumeConfig>,
    index_url: Option<String>,
) -> Result<StoreConfig> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let path = root.join("config.json");
    if path.exists() {
        bail!(
            "store already initialized at {} — refusing to overwrite config.json",
            root.display()
        );
    }
    let avg = avg.unwrap_or(chunker::DEFAULT_AVG_SIZE);
    let config = StoreConfig {
        version: crate::crypto::FORMAT_VERSION,
        chunker: ChunkerParams::from_avg(avg)?,
        zstd_level: 3,
        checkpoint_interval: 128,
        volumes: Some(volumes),
        index_url,
    };
    let json = serde_json::to_string_pretty(&config).context("serializing store config")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(config)
}
