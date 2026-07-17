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
/// Default per-volume full threshold when config omits it (DESIGN Section 15.2).
pub const DEFAULT_VOLUME_FULL_THRESHOLD: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB
/// Spare slot becomes mandatory at N ≥ this many volumes (DESIGN Section 15.5).
const SPARE_SLOT_MIN_VOLUMES: usize = 3;

/// Compaction hysteresis gate defaults (DESIGN Section 12.4). All three must hold for a
/// volume to be eligible. Overridable at runtime (env, below) so tests can tune.
const DEFAULT_DEAD_RATIO_GATE: f64 = 0.50; // dead bytes / total > 50%
const DEFAULT_PRESSURE_UTIL_GATE: f64 = 0.80; // store util >= 80% of budget
const DEFAULT_MIN_COMPACT_INTERVAL_SECS: i64 = 24 * 3600; // 24h anti-churn floor
/// Orphan-sweep safety window (DESIGN Section 12.5): a segment ref unreferenced by the
/// manifest is collectible only once older than this (comfortably beyond max
/// staging age). Overridable via GITSTORAGE_ORPHAN_WINDOW_SECS for tests.
const DEFAULT_ORPHAN_WINDOW_SECS: i64 = 3600; // 1 hour

pub type Namespace = BTreeMap<String, Manifest>;

/// The compaction hysteresis gates (DESIGN Section 12.4), resolved from defaults and
/// per-invocation env overrides so tests can drive compaction deterministically.
/// These are the ONLY knobs that make compaction fire; there is no background
/// thread — compaction runs only when [`Engine::compact`] is invoked and every
/// gate passes (unless `force` bypasses them, which the `compact` CLI exposes
/// for operators and tests).
#[derive(Debug, Clone, Copy)]
pub struct CompactionGates {
    pub dead_ratio: f64,
    pub pressure_util: f64,
    pub min_interval_secs: i64,
    pub orphan_window_secs: i64,
}

impl Default for CompactionGates {
    fn default() -> Self {
        Self {
            dead_ratio: DEFAULT_DEAD_RATIO_GATE,
            pressure_util: DEFAULT_PRESSURE_UTIL_GATE,
            min_interval_secs: DEFAULT_MIN_COMPACT_INTERVAL_SECS,
            orphan_window_secs: DEFAULT_ORPHAN_WINDOW_SECS,
        }
    }
}

impl CompactionGates {
    /// Resolve gates from defaults, overlaying any GITSTORAGE_* env overrides.
    /// The env hooks exist so tests can tune the three gates (the brief requires
    /// all three configurable); operators normally leave them at DESIGN defaults.
    fn from_env() -> Self {
        let mut g = Self::default();
        if let Ok(v) = std::env::var("GITSTORAGE_DEAD_RATIO_GATE") {
            if let Ok(f) = v.parse() {
                g.dead_ratio = f;
            }
        }
        if let Ok(v) = std::env::var("GITSTORAGE_PRESSURE_UTIL_GATE") {
            if let Ok(f) = v.parse() {
                g.pressure_util = f;
            }
        }
        if let Ok(v) = std::env::var("GITSTORAGE_MIN_COMPACT_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                g.min_interval_secs = n;
            }
        }
        if let Ok(v) = std::env::var("GITSTORAGE_ORPHAN_WINDOW_SECS") {
            if let Ok(n) = v.parse() {
                g.orphan_window_secs = n;
            }
        }
        g
    }
}

/// A segment seal record (DESIGN.md Section 8.2): where a sealed segment landed
/// and how many ciphertext bytes it holds. Carried in the transaction payload so
/// liveness/stats/volume-selection can size a volume WITHOUT full-fetching every
/// segment (the M4 read-amplification fix — see agent-docs/milestone-5.md). The
/// log is thus the single authority for byte accounting as well as liveness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegRec {
    pub vol: String,
    pub seg: String,
    pub bytes: u64,
}

/// One transaction record — the (encrypted) payload of one log commit.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Txn {
    /// Incremental change: a file put, and/or a removal (M5).
    Delta {
        #[serde(skip_serializing_if = "Option::is_none")]
        put: Option<Manifest>,
        #[serde(skip_serializing_if = "Option::is_none")]
        remove: Option<String>,
        /// Segments sealed by this transaction (DESIGN Section 8.2 seal records).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        seals: Vec<SegRec>,
    },
    /// Full-namespace snapshot (DESIGN.md Section 8.5). Carries the complete
    /// segment-size index so a reader that starts at a checkpoint knows every
    /// volume's byte accounting without walking pre-checkpoint history.
    Checkpoint {
        namespace: Namespace,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        seg_index: Vec<SegRec>,
    },
    /// Compaction commit point (DESIGN Section 12.3 step 5): live chunks of `retired`
    /// volumes were rewritten into `seals` on the spare/headroom volumes; the
    /// listed files are repointed to their new placements. Applying this txn
    /// makes the retired volumes unreferenced, so they can be deleted AFTER this
    /// commit lands (delete-only-after-CAS ordering).
    Compact {
        /// Volume IDs whose repositories are retired by this compaction.
        retired: Vec<String>,
        /// New segment seal records produced by the rewrite.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        seals: Vec<SegRec>,
        /// Files whose chunk placements moved (full replacement manifests).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        repoint: Vec<Manifest>,
    },
}

/// One declared volume in the fixed volume set (DESIGN Section 15.1). `url` absent =
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

#[derive(Debug)]
pub struct PutStats {
    pub total_chunks: usize,
    pub new_chunks: usize,
    pub ciphertext_bytes: u64,
    pub size: u64,
    pub committed: bool,
    /// Which volume the new segment landed in (None if no new segment).
    pub volume: Option<String>,
}

/// Per-volume liveness + budget accounting for `stats` (DESIGN Section 12.2, Section 15).
/// All figures are derived from the log alone (namespace + segment index); no
/// segment is fetched (the M4 read-amplification fix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeStats {
    pub id: String,
    /// Total stored ciphertext bytes in this volume (sum of its seal records).
    pub total: u64,
    /// Live bytes: ciphertext of DISTINCT chunks still referenced by the
    /// current namespace (a shared chunk counts once).
    pub live: u64,
    /// Dead bytes = total − live (garbage awaiting compaction).
    pub dead: u64,
    /// The volume-full threshold (budget) for this volume.
    pub threshold: u64,
    /// True if this is the reserved compaction spare (no ordinary writes).
    pub spare: bool,
}

impl VolumeStats {
    /// Dead / total, in [0, 1]. Zero when the volume is empty.
    pub fn dead_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.dead as f64 / self.total as f64
        }
    }

    /// total / threshold, in [0, ∞). Budget utilization (DESIGN Section 15.2).
    pub fn utilization(&self) -> f64 {
        if self.threshold == 0 {
            0.0
        } else {
            self.total as f64 / self.threshold as f64
        }
    }
}

/// What one `compact` invocation did (DESIGN Section 12.3). Empty `compacted` means the
/// hysteresis gates blocked every volume — a normal, expected outcome (churn
/// guard, Section 12.4). `orphans_swept` counts C2-style crash orphans reclaimed by the
/// safety-windowed sweep (Section 12.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport {
    pub compacted: Vec<CompactedVolume>,
    pub orphans_swept: usize,
}

/// One retired volume's before/after in a compaction pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedVolume {
    pub volume: String,
    pub dest_volume: String,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub chunks_moved: usize,
}

/// A live volume: its backend transport plus its declared metadata.
struct VolumeHandle {
    id: String,
    backend: Box<dyn Backend>,
    threshold: u64,
    /// True if reserved as the compaction spare (DESIGN Section 15.5): no ordinary
    /// writes land here.
    spare: bool,
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

    /// Per-volume liveness + budget accounting (DESIGN Section 12.2, Section 15) for `stats`
    /// and compaction gating. All figures come from the log (namespace + seg
    /// index) with ZERO segment fetches.
    pub fn stats(&self) -> Result<Vec<VolumeStats>> {
        let state = self.load_full_state(None)?;
        Ok(self.volume_stats(&state))
    }

    /// Compute per-volume stats from an already-loaded store state.
    fn volume_stats(&self, state: &StoreState) -> Vec<VolumeStats> {
        // Live bytes per volume: sum of ciphertext sizes of DISTINCT live chunks
        // (a chunk shared by several files counts once). Liveness = referenced
        // by the current namespace (DESIGN Section 12.2).
        let mut live_chunks: BTreeSet<&str> = BTreeSet::new();
        let mut live_by_vol: BTreeMap<&str, u64> = BTreeMap::new();
        for m in state.namespace.values() {
            for c in &m.chunks {
                if live_chunks.insert(c.id.as_str()) {
                    *live_by_vol.entry(c.vol.as_str()).or_default() += c.clen;
                }
            }
        }
        self.volumes
            .iter()
            .map(|v| {
                let total = volume_used(&v.id, &state.seg_index);
                let live = live_by_vol.get(v.id.as_str()).copied().unwrap_or(0);
                // Live can exceed the seg-index total only for pre-M5 manifests
                // with clen=0 or mixed data; clamp dead at 0 for safety.
                let dead = total.saturating_sub(live);
                VolumeStats {
                    id: v.id.clone(),
                    total,
                    live,
                    dead,
                    threshold: v.threshold,
                    spare: v.spare,
                }
            })
            .collect()
    }

    /// A note about how the volume backends serve reads (promisor verdict etc).
    pub fn read_path_notes(&self) -> Vec<(String, String)> {
        self.volumes
            .iter()
            .filter_map(|v| v.backend.read_path_note().map(|n| (v.id.clone(), n)))
            .collect()
    }

    /// Whole-store mirror to an independent backend (DESIGN Section 14.3): push the
    /// index/log and every volume to a second, independent set of git repos, so
    /// the store survives the loss of the primary backend. Push-only and
    /// idempotent (re-running mirrors only new objects). `volume_targets` must
    /// name a target URL for EVERY declared volume — a partial mirror is not a
    /// durable copy, so a missing target is refused. The mirror is ciphertext
    /// only (same opacity as the primary); the keyfile is NOT part of the store.
    pub fn mirror(
        &self,
        index_target: &str,
        volume_targets: &BTreeMap<String, String>,
    ) -> Result<()> {
        for v in &self.volumes {
            if !volume_targets.contains_key(&v.id) {
                bail!(
                    "no mirror target for volume {:?} — a whole-store mirror needs \
                     a target for every volume (got {} of {})",
                    v.id,
                    volume_targets.len(),
                    self.volumes.len()
                );
            }
        }
        // Volumes (data) BEFORE the index (log) — the same data-before-manifest
        // ordering as the write path (Section 8.3): at every instant the mirror's
        // log only ever references segments already present on the mirror's
        // volumes. If the mirror push is interrupted, the mirror's log tip is
        // simply older than the primary's, never dangling.
        for v in &self.volumes {
            let target = &volume_targets[&v.id];
            v.backend
                .mirror_to(target)
                .with_context(|| format!("mirroring volume {}", v.id))?;
        }
        self.index.mirror_to(index_target)?;
        Ok(())
    }

    /// Store a byte stream under `name`. Returns stats. Safe under concurrent
    /// writers: the log CAS serializes commits; losers rebase and retry.
    pub fn put<R: Read>(&self, name: &str, reader: R) -> Result<PutStats> {
        let state = self.load_full_state(None)?;
        let namespace = &state.namespace;

        // Known placements: chunk_id -> (vol, seg, ciphertext_len) across the
        // whole namespace. The clen lets a deduped chunk re-carry its stored
        // size into the new manifest for live/dead accounting (DESIGN Section 12.2).
        let mut known: BTreeMap<String, (String, String, u64)> = BTreeMap::new();
        for m in namespace.values() {
            for c in &m.chunks {
                known.insert(c.id.clone(), (c.vol.clone(), c.seg.clone(), c.clen));
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
            let clen = sealed.ciphertext.len() as u64;
            let is_new = !known.contains_key(&sealed.chunk_id);
            let (vol, seg) = match known.get(&sealed.chunk_id) {
                Some((vol, seg, _clen)) => (vol.clone(), seg.clone()),
                None => {
                    // Placeholder placement, resolved after volume selection.
                    // Buffer + count each distinct new chunk once per put.
                    if staged_ids.insert(sealed.chunk_id.clone()) {
                        ciphertext_bytes += clen;
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
                    clen,
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
            Some(self.select_volume(ciphertext_bytes, &state.seg_index)?)
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

        // The seal record for the new segment (DESIGN Section 8.2), carried in the txn
        // so byte accounting needs no segment fetch. None if this put stored no
        // new data (pure dedup / metadata-only manifest change).
        let new_seal = landed_volume.as_ref().map(|vid| SegRec {
            vol: vid.clone(),
            seg: seg_id.clone(),
            bytes: ciphertext_bytes,
        });

        // ----- Phase 2: the commit point (CAS, rebase-and-retry) -----
        let total_chunks = manifest.chunks.len();
        for _attempt in 0..MAX_CAS_RETRIES {
            let tip = self.log_tip()?;
            let mut state = self.load_full_state(tip.as_deref())?;

            // Rebase check: if the winning writer already stored an identical
            // manifest for this name, our transaction is a no-op.
            if state.namespace.get(name) == Some(&manifest) {
                return Ok(PutStats {
                    total_chunks,
                    new_chunks,
                    ciphertext_bytes,
                    size,
                    committed: false,
                    volume: landed_volume,
                });
            }

            let seals: Vec<SegRec> = new_seal.iter().cloned().collect();
            let checkpoint_due =
                state.since_checkpoint + 1 >= self.config.checkpoint_interval as usize;
            let txn = if checkpoint_due {
                // Fold this put into the snapshot and emit the full seg index so
                // readers starting here get complete byte accounting.
                state.namespace.insert(name.to_string(), manifest.clone());
                for s in &seals {
                    state
                        .seg_index
                        .insert((s.vol.clone(), s.seg.clone()), s.bytes);
                }
                Txn::Checkpoint {
                    namespace: state.namespace,
                    seg_index: seg_index_records(&state.seg_index),
                }
            } else {
                Txn::Delta {
                    put: Some(manifest.clone()),
                    remove: None,
                    seals,
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

    /// Logically delete `name` (DESIGN Section 12.1): append a transaction that drops
    /// the namespace entry. The chunks stay on disk — their bytes become dead and
    /// are reclaimed later by compaction (Section 12.3). Removing a name that is not in
    /// the store fails cleanly (no transaction is written).
    ///
    /// This is metadata-only: no Phase-1 data write, just the Phase-2 CAS commit
    /// point, with the same rebase-and-retry loop as `put`.
    pub fn remove(&mut self, name: &str) -> Result<()> {
        // Fail-fast against the current tip so the CLI reports a clean error for
        // an unknown name without writing anything.
        {
            let state = self.load_full_state(None)?;
            if !state.namespace.contains_key(name) {
                bail!("no file {name:?} in this store (see `ls`)");
            }
        }

        for _attempt in 0..MAX_CAS_RETRIES {
            let tip = self.log_tip()?;
            let mut state = self.load_full_state(tip.as_deref())?;

            // Rebase check: a concurrent writer may have already removed it.
            if !state.namespace.contains_key(name) {
                return Ok(());
            }

            let checkpoint_due =
                state.since_checkpoint + 1 >= self.config.checkpoint_interval as usize;
            let txn = if checkpoint_due {
                state.namespace.remove(name);
                Txn::Checkpoint {
                    namespace: state.namespace,
                    seg_index: seg_index_records(&state.seg_index),
                }
            } else {
                Txn::Delta {
                    put: None,
                    remove: Some(name.to_string()),
                    seals: Vec::new(),
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

            maybe_crash("before-cas");
            if self.index.cas_ref(LOG_REF, &commit, tip.as_deref())? {
                maybe_crash("after-cas");
                return Ok(());
            }
        }
        bail!("log CAS failed {MAX_CAS_RETRIES} times — giving up (pathological contention?)");
    }

    /// Reclaim dead bytes by compacting eligible volumes (DESIGN Section 12.3/12.4).
    ///
    /// Hysteresis: a volume is a candidate only when its dead-ratio exceeds the
    /// gate; the pass runs at all only under budget pressure and after the
    /// min-interval since the last compaction. `force` bypasses the pressure and
    /// interval gates (operator/test escape hatch) but NEVER the dead-ratio gate
    /// and NEVER the delete-only-after-CAS ordering.
    ///
    /// Procedure (delete-only-after-CAS, Section 12.3):
    ///
    /// 1. Rewrite each candidate's LIVE chunks into a fresh segment on the
    ///    spare/headroom volume (Phase 1, idempotent, content-addressed).
    /// 2. CAS a `Compact` transaction repointing the affected files and
    ///    retiring the candidate volumes — THIS is the commit point.
    /// 3. Only AFTER the CAS lands, destroy+recreate the retired repos.
    ///
    /// A crash before step 2's CAS leaves the old segments live and the rewrite
    /// as orphans; a crash after the CAS but before step 3 leaves the store fully
    /// readable (new placements) with the old repos as reclaimable garbage — the
    /// next `compact` finishes the delete. The store is readable at every point.
    ///
    /// Always sweeps safety-windowed orphans (Section 12.5) regardless of gate outcome.
    pub fn compact(&mut self, force: bool) -> Result<CompactReport> {
        let gates = CompactionGates::from_env();
        let mut report = CompactReport::default();

        let state = self.load_full_state(None)?;
        let stats = self.volume_stats(&state);

        // Pressure gate (store-wide): is any writable volume near its budget?
        let under_pressure = stats
            .iter()
            .any(|s| !s.spare && s.utilization() >= gates.pressure_util);
        // Interval gate: enough time since the last compaction commit?
        let interval_ok = match self.last_compaction_time()? {
            Some(t) => now_secs().saturating_sub(t) >= gates.min_interval_secs,
            None => true, // never compacted before
        };
        let gates_open = force || (under_pressure && interval_ok);

        if gates_open {
            // Candidates: writable, non-spare volumes over the dead-ratio gate,
            // with something live worth moving (or entirely dead → just retire).
            let candidates: Vec<VolumeStats> = stats
                .iter()
                .filter(|s| !s.spare && s.total > 0 && s.dead_ratio() > gates.dead_ratio)
                .cloned()
                .collect();
            for cand in &candidates {
                if let Some(done) = self.compact_one(&cand.id, &state)? {
                    report.compacted.push(done);
                }
            }
        }

        report.orphans_swept = self.sweep_orphans(gates.orphan_window_secs)?;
        Ok(report)
    }

    /// Compact a single volume: rewrite its live chunks onto a destination
    /// volume, CAS the repoint/retire commit, then destroy+recreate the source.
    /// Returns None if the destination cannot hold the live set (caller leaves
    /// the volume for a later pass rather than breaking the budget wall).
    fn compact_one(
        &mut self,
        vid: &str,
        base_state: &StoreState,
    ) -> Result<Option<CompactedVolume>> {
        // Live chunks placed in this volume, and the files that reference them.
        // Dedup by chunk id: a chunk shared across files is rewritten once.
        let mut live_ids: BTreeSet<String> = BTreeSet::new();
        let mut live_bytes = 0u64;
        let mut affected_files: Vec<&Manifest> = Vec::new();
        for m in base_state.namespace.values() {
            let touches = m.chunks.iter().any(|c| c.vol == vid);
            if touches {
                affected_files.push(m);
            }
            for c in &m.chunks {
                if c.vol == vid && live_ids.insert(c.id.clone()) {
                    live_bytes += c.clen;
                }
            }
        }
        let bytes_before = volume_used(vid, &base_state.seg_index);

        // Pick a destination volume with room for the live bytes: prefer the
        // spare (its whole purpose, Section 15.5), then any writable volume that is not
        // the one being retired and has headroom. If none fits, refuse (the
        // budget wall still holds during compaction — never grow the fleet).
        let dest_idx = self.select_compact_dest(vid, live_bytes, &base_state.seg_index)?;
        let Some(dest_idx) = dest_idx else {
            return Ok(None);
        };
        let dest_id = self.volumes[dest_idx].id.clone();

        // ----- Phase 1: rewrite live chunks into a new segment on dest -----
        // The new segment id is content-derived (hash of the sorted live ids) so
        // a re-run after a crash reproduces the SAME segment ref (idempotent).
        // The bytes written equal the live ciphertext (`clen`), so the seal size
        // is `live_bytes` whether we write now or reuse an existing rewrite.
        let new_seg = compact_segment_id(vid, &live_ids);
        let seg_ref = format!("refs/segments/{new_seg}");
        if !live_ids.is_empty() {
            let dest = &self.volumes[dest_idx];
            // Idempotent redo: a crashed earlier attempt may already have pushed
            // this exact (content-derived) segment. Re-`set_ref` of a freshly
            // built commit would be a NON-fast-forward push (the commit OID
            // differs by timestamp), so if the ref is already present we reuse it
            // rather than rewrite — the segment's content is identical by
            // construction.
            if dest.backend.read_ref(&seg_ref)?.is_none() {
                let mut moved: BTreeSet<String> = BTreeSet::new();
                let mut seg_entries: Vec<(String, String)> = Vec::new();
                for m in &affected_files {
                    for c in &m.chunks {
                        if c.vol == vid && moved.insert(c.id.clone()) {
                            // Read ciphertext from the OLD placement, write to dest.
                            let ciphertext = self.read_chunk_ciphertext(vid, &c.seg, &c.id)?;
                            let oid = dest.backend.write_blob(&ciphertext)?;
                            seg_entries.push((fanout_path(&c.id), oid));
                        }
                    }
                }
                let tree = dest.backend.write_tree(&seg_entries)?;
                let commit =
                    dest.backend
                        .commit_tree(&tree, &[], &format!("compact segment {new_seg}"))?;
                dest.backend.set_ref(&seg_ref, &commit)?;
            }
        }

        maybe_crash("compact-after-rewrite"); // new segment live, log unaware

        let seals = if live_ids.is_empty() {
            Vec::new()
        } else {
            vec![SegRec {
                vol: dest_id.clone(),
                seg: new_seg.clone(),
                bytes: live_bytes,
            }]
        };

        // ----- Phase 2: the commit point (CAS the Compact txn) -----
        // `commit_compaction` builds the repointed manifests from the CURRENT
        // namespace on every attempt (not this stale snapshot) and aborts if a
        // concurrent writer put data on `vid` we did not rewrite — so a
        // put-during-compaction never loses data.
        let committed = self.commit_compaction(vid, &dest_id, &new_seg, &live_ids, &seals)?;
        if !committed {
            // Lost the race / could not land within retries / a concurrent
            // writer touched the volume. Our rewrite is a harmless orphan
            // (content-addressed) reclaimed by the sweep. Do NOT delete the
            // source — it may still be referenced by the winning tip.
            return Ok(None);
        }

        maybe_crash("compact-before-delete"); // committed; source not yet reclaimed

        // ----- Phase 3: reclaim the retired volume (ONLY after CAS) -----
        // Safe now: the current log tip references the NEW placements
        // exclusively, so every segment in the source is unreferenced garbage.
        // Prefer a whole-repo delete (destroy+recreate wipes it in one shot);
        // if the backend refuses — a hosted https/ssh repo, where deletion is a
        // control-plane/operator action (mirrors init-time provisioning) — fall
        // back to deleting the source's segment refs over the wire. That leaves
        // an empty, reusable slot and makes the objects server-GC-collectible;
        // the empty repository itself may be removed by an operator out of band.
        {
            let src = self
                .volumes
                .iter_mut()
                .find(|v| v.id == vid)
                .expect("retired volume present");
            match src.backend.destroy() {
                Ok(()) => src.backend.recreate()?,
                Err(_) => {
                    for (refname, _) in src.backend.list_refs("refs/segments/")? {
                        src.backend.delete_ref(&refname)?;
                    }
                }
            }
        }

        Ok(Some(CompactedVolume {
            volume: vid.to_string(),
            dest_volume: dest_id,
            bytes_before,
            bytes_after: 0,
            chunks_moved: live_ids.len(),
        }))
    }

    /// CAS a `Compact` transaction (repoint + retire) onto the log tip, with the
    /// same rebase-and-retry loop as `put`. Returns true if it landed.
    ///
    /// The repointed manifests are rebuilt from the CURRENT namespace on every
    /// attempt, so this is safe against a concurrent `put`/`rm` that advanced
    /// the log after the rewrite snapshot: `moved_ids` is the set of chunk IDs
    /// we actually rewrote onto `dest`; if any file in the current namespace
    /// still references `retired` via a chunk we did NOT rewrite (new data a
    /// concurrent put placed there), retiring the volume would destroy live
    /// bytes, so we ABORT (return false) and leave the source intact — our
    /// rewrite becomes a swept orphan and a later pass retries.
    fn commit_compaction(
        &mut self,
        retired: &str,
        dest_id: &str,
        new_seg: &str,
        moved_ids: &BTreeSet<String>,
        seals: &[SegRec],
    ) -> Result<bool> {
        for _attempt in 0..MAX_CAS_RETRIES {
            let tip = self.log_tip()?;
            let state = self.load_full_state(tip.as_deref())?;

            // Rebuild repoint from the CURRENT namespace, and check safety.
            let mut repoint: Vec<Manifest> = Vec::new();
            let mut still_referenced = false;
            for m in state.namespace.values() {
                let mut touches = false;
                for c in &m.chunks {
                    if c.vol == retired {
                        still_referenced = true;
                        touches = true;
                        if !moved_ids.contains(&c.id) {
                            // A chunk on the retiring volume we never rewrote —
                            // a concurrent put landed it after our snapshot.
                            // Retiring now would lose it. Abort this pass.
                            return Ok(false);
                        }
                    }
                }
                if touches {
                    let mut nm = m.clone();
                    for c in &mut nm.chunks {
                        if c.vol == retired {
                            c.vol = dest_id.to_string();
                            c.seg = new_seg.to_string();
                        }
                    }
                    repoint.push(nm);
                }
            }
            if !still_referenced && !state.seg_index.keys().any(|(v, _)| v == retired) {
                return Ok(true); // already reflected; treat as landed
            }

            let txn = Txn::Compact {
                retired: vec![retired.to_string()],
                seals: seals.to_vec(),
                repoint,
            };
            let payload = self
                .keys
                .seal_manifest(&serde_json::to_vec(&txn).context("serializing txn")?)?;
            let blob = self.index.write_blob(&payload)?;
            let tree = self.index.write_tree(&[("txn".to_string(), blob)])?;
            let parents: Vec<String> = tip.iter().cloned().collect();
            let commit = self.index.commit_tree(&tree, &parents, "compact")?;

            maybe_crash("compact-before-cas");
            if self.index.cas_ref(LOG_REF, &commit, tip.as_deref())? {
                maybe_crash("compact-after-cas");
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Sweep C2-style orphan segments (DESIGN Section 12.5): segment refs that no live
    /// manifest references AND that are older than the safety window (by commit
    /// time). Never touches in-flight staging (young segments stay). Returns the
    /// number of orphan segment refs deleted.
    fn sweep_orphans(&self, window_secs: i64) -> Result<usize> {
        let state = self.load_full_state(None)?;
        // Segments referenced by the current namespace are OFF-LIMITS.
        let mut referenced: BTreeSet<(String, String)> = BTreeSet::new();
        for m in state.namespace.values() {
            for c in &m.chunks {
                referenced.insert((c.vol.clone(), c.seg.clone()));
            }
        }
        let now = now_secs();
        let mut swept = 0usize;
        for v in &self.volumes {
            for (refname, _oid) in v.backend.list_refs("refs/segments/")? {
                let seg = refname
                    .strip_prefix("refs/segments/")
                    .unwrap_or(&refname)
                    .to_string();
                if referenced.contains(&(v.id.clone(), seg.clone())) {
                    continue; // live — never touch
                }
                // Safety window: only collect segments comfortably older than the
                // maximum staging age, so an in-flight write is never swept.
                let age = now.saturating_sub(v.backend.commit_time(&refname)?);
                if age < window_secs {
                    continue;
                }
                v.backend.delete_ref(&refname)?;
                swept += 1;
            }
        }
        Ok(swept)
    }

    /// Pick a destination for compacting `retiring`'s `live_bytes`: the spare
    /// first (Section 15.5), else any other writable volume with headroom. Excludes the
    /// volume being retired. None = no room (leave it for a later pass; the fleet
    /// never grows).
    fn select_compact_dest(
        &self,
        retiring: &str,
        live_bytes: u64,
        seg_index: &BTreeMap<(String, String), u64>,
    ) -> Result<Option<usize>> {
        // Prefer the spare slot: it exists precisely to give compaction a
        // guaranteed destination.
        if let Some(i) = self
            .volumes
            .iter()
            .position(|v| v.spare && v.id != retiring)
        {
            let used = volume_used(&self.volumes[i].id, seg_index);
            if used.saturating_add(live_bytes) <= self.volumes[i].threshold {
                return Ok(Some(i));
            }
        }
        // Otherwise the writable volume (non-spare) with the most headroom.
        let mut best: Option<(usize, u64)> = None;
        for (i, v) in self.volumes.iter().enumerate() {
            if v.id == retiring || v.spare {
                continue;
            }
            let used = volume_used(&v.id, seg_index);
            if used.saturating_add(live_bytes) > v.threshold {
                continue;
            }
            let headroom = v.threshold - used;
            match best {
                Some((_, hr)) if headroom <= hr => {}
                _ => best = Some((i, headroom)),
            }
        }
        Ok(best.map(|(i, _)| i))
    }

    /// Read a chunk's ciphertext from a specific volume+segment (compaction read
    /// path). Verifies the content address before returning.
    fn read_chunk_ciphertext(&self, vid: &str, seg: &str, id: &str) -> Result<Vec<u8>> {
        let vol = self
            .volumes
            .iter()
            .find(|v| v.id == vid)
            .with_context(|| format!("unknown volume {vid:?}"))?;
        let ciphertext = vol
            .backend
            .read_blob_at(&format!("refs/segments/{seg}"), &fanout_path(id))
            .with_context(|| format!("chunk {id} missing from segment {seg} during compaction"))?;
        let actual = blake3::hash(&ciphertext).to_hex().to_string();
        if actual != id {
            bail!("compaction read: chunk {id} hashes to {actual} (corruption)");
        }
        Ok(ciphertext)
    }

    /// Commit time of the most recent compaction commit, or None if the log has
    /// none — the interval gate's reference point (DESIGN Section 12.4).
    fn last_compaction_time(&self) -> Result<Option<i64>> {
        let Some(tip) = self.log_tip()? else {
            return Ok(None);
        };
        for commit in self.index.rev_list(&tip)? {
            if let Txn::Compact { .. } = self.read_txn(&commit)? {
                return Ok(Some(self.index.commit_time(&commit)?));
            }
        }
        Ok(None)
    }

    /// Debug/test: count (checkpoints, non-checkpoint txns) in the whole log.
    pub fn txn_kind_counts(&self) -> Result<(usize, usize)> {
        let Some(tip) = self.log_tip()? else {
            return Ok((0, 0));
        };
        let mut checkpoints = 0;
        let mut deltas = 0;
        for commit in self.index.rev_list(&tip)? {
            match self.read_txn(&commit)? {
                Txn::Checkpoint { .. } => checkpoints += 1,
                Txn::Delta { .. } | Txn::Compact { .. } => deltas += 1,
            }
        }
        Ok((checkpoints, deltas))
    }

    /// Count compaction commits in the whole log — the churn-guard assertion
    /// (DESIGN Section 12.4: hysteresis must prevent repeated compactions).
    pub fn compaction_count(&self) -> Result<usize> {
        let Some(tip) = self.log_tip()? else {
            return Ok(0);
        };
        let mut n = 0;
        for commit in self.index.rev_list(&tip)? {
            if let Txn::Compact { .. } = self.read_txn(&commit)? {
                n += 1;
            }
        }
        Ok(n)
    }

    // ---------- internals ----------

    /// Choose the destination volume for a new segment of `projected` bytes
    /// (DESIGN Section 9.3): among the WRITABLE volumes (spare excluded when N ≥ 3),
    /// the one with the most free headroom below its threshold whose projected
    /// post-write size still fits. Ties broken by lowest volume ID. If none can
    /// accept the segment, the budget wall refuses (DESIGN Section 15.3).
    ///
    /// Usage comes from the log-derived `seg_index` (no segment fetch — the M4
    /// read-amplification fix).
    fn select_volume(
        &self,
        projected: u64,
        seg_index: &BTreeMap<(String, String), u64>,
    ) -> Result<usize> {
        let mut best: Option<(usize, u64)> = None; // (index, headroom)
        for (i, v) in self.volumes.iter().enumerate() {
            if v.spare {
                continue; // reserved for compaction (DESIGN Section 15.5)
            }
            let used = volume_used(&v.id, seg_index);
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
        let st = self.load_full_state(at)?;
        Ok((st.namespace, st.since_checkpoint))
    }

    /// Full store state at a pinned commit or the current tip: the namespace
    /// AND the segment-size index (DESIGN Section 8.2 seal records) reconstructed from
    /// the log. The seg index lets stats/liveness/volume-selection size volumes
    /// with ZERO segment fetches (the M4 read-amplification fix). Reader model
    /// per DESIGN Section 8.6: newest checkpoint (which carries a full seg index) then
    /// the delta/compaction tail applied forward.
    fn load_full_state(&self, at: Option<&str>) -> Result<StoreState> {
        let tip = match at {
            Some(oid) => oid.to_string(),
            None => match self.log_tip()? {
                Some(t) => t,
                None => return Ok(StoreState::default()),
            },
        };
        let mut tail: Vec<Txn> = Vec::new();
        let mut base = Namespace::new();
        let mut seg_index: BTreeMap<(String, String), u64> = BTreeMap::new();
        for commit in self.index.rev_list(&tip)? {
            match self.read_txn(&commit)? {
                Txn::Checkpoint {
                    namespace,
                    seg_index: seals,
                } => {
                    base = namespace;
                    for s in seals {
                        seg_index.insert((s.vol, s.seg), s.bytes);
                    }
                    break;
                }
                other => tail.push(other),
            }
        }
        let since_checkpoint = tail.len();
        for txn in tail.into_iter().rev() {
            apply_txn(&mut base, &mut seg_index, txn);
        }
        Ok(StoreState {
            namespace: base,
            seg_index,
            since_checkpoint,
        })
    }

    fn read_txn(&self, commit: &str) -> Result<Txn> {
        let sealed = self.index.read_blob_at(commit, "txn")?;
        let plaintext = self.keys.open_manifest(&sealed)?;
        serde_json::from_slice(&plaintext).context("parsing transaction payload")
    }
}

/// Store state reconstructed from the log: the logical namespace plus the
/// authoritative per-segment byte index (keyed by (volume, segment)).
#[derive(Default)]
struct StoreState {
    namespace: Namespace,
    seg_index: BTreeMap<(String, String), u64>,
    since_checkpoint: usize,
}

/// Apply one non-checkpoint transaction forward onto the running namespace and
/// segment-size index. Kept free-standing so both the reader and the checkpoint
/// builder share exactly one interpretation of a transaction's effect.
fn apply_txn(base: &mut Namespace, seg_index: &mut BTreeMap<(String, String), u64>, txn: Txn) {
    match txn {
        Txn::Delta { put, remove, seals } => {
            for s in seals {
                seg_index.insert((s.vol, s.seg), s.bytes);
            }
            if let Some(m) = put {
                base.insert(m.name.clone(), m);
            }
            if let Some(name) = remove {
                base.remove(&name);
            }
        }
        Txn::Compact {
            retired,
            seals,
            repoint,
        } => {
            for s in seals {
                seg_index.insert((s.vol, s.seg), s.bytes);
            }
            for m in repoint {
                base.insert(m.name.clone(), m);
            }
            // The retired volumes' segments leave the accounting: their bytes are
            // no longer part of any live volume once the repos are gone.
            for vid in &retired {
                seg_index.retain(|(v, _), _| v != vid);
            }
        }
        Txn::Checkpoint { .. } => {
            // A checkpoint in the tail would have terminated the walk; never here.
        }
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

/// Flatten the (volume, segment) → bytes map into sorted seal records for a
/// checkpoint payload (deterministic order = stable, testable serialization).
fn seg_index_records(map: &BTreeMap<(String, String), u64>) -> Vec<SegRec> {
    map.iter()
        .map(|((vol, seg), bytes)| SegRec {
            vol: vol.clone(),
            seg: seg.clone(),
            bytes: *bytes,
        })
        .collect()
}

/// Bytes stored in `vol` per the log-derived segment-size index. This is the
/// authoritative accounting (DESIGN Section 8.2) and needs ZERO segment fetches.
fn volume_used(vol: &str, seg_index: &BTreeMap<(String, String), u64>) -> u64 {
    seg_index
        .iter()
        .filter(|((v, _), _)| v == vol)
        .map(|(_, b)| *b)
        .sum()
}

/// Random 128-bit hex ID for segments.
fn random_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content-derived segment ID for a compaction rewrite: BLAKE3 over the
/// retiring volume ID and the sorted live chunk IDs (a `BTreeSet` iterates in
/// order, so this is deterministic). Because the ID is a pure function of what
/// is being rewritten, a compaction that crashes after Phase 1 and re-runs
/// reproduces the SAME `refs/segments/<id>` — the rewrite is idempotent and
/// never leaves a second, divergent orphan segment.
fn compact_segment_id(retiring: &str, live_ids: &BTreeSet<String>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gitstorage-compact-seg\0");
    hasher.update(retiring.as_bytes());
    hasher.update(b"\0");
    for id in live_ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\0");
    }
    // 128-bit ID to match `random_id`'s width; the hex string is ASCII so the
    // byte slice is a valid char boundary.
    hasher.finalize().to_hex()[..32].to_string()
}

/// Current wall-clock time in unix seconds. The reference clock for the
/// compaction min-interval gate (DESIGN Section 12.4) and the orphan-sweep safety
/// window (Section 12.5). A clock before the epoch (impossible in practice) reads 0,
/// which only makes those gates more conservative.
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
