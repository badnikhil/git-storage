//! git-storage CLI: milestone-3 — sealed segments + CAS transaction log.
//!
//! Thin binary over the library crate (src/lib.rs). See DESIGN.md for the
//! architecture and IMPLEMENTATION-PLAN.md for the milestone ladder.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use git_storage::backend::provision::{ensure_repo, Host, Provisioned, RepoSpec};
use git_storage::chunker;
use git_storage::crypto::{keyfile, Keys};
use git_storage::engine::{self, Engine, VolumeConfig, DEFAULT_VOLUME_FULL_THRESHOLD};
use git_storage::gitrepo::Bare;

#[derive(Parser)]
#[command(
    name = "git-storage",
    version,
    about = "Encrypted, deduplicating, transactional file storage on git repositories (milestone 3)"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a store with a FIXED, operator-declared volume set (M4).
    /// Provisions each declared volume: file:// URLs are inited as bare repos
    /// directly; https:// GitHub/Gitea repos are created via the control-plane
    /// REST API (requires GITSTORAGE_TOKEN) — and ONLY the declared set, never
    /// automatically. The data plane can never create repos.
    Init {
        /// Store directory (created if missing).
        #[arg(long)]
        repo: PathBuf,
        /// Master keyfile (created if missing for a brand-new store).
        #[arg(long)]
        keyfile: PathBuf,
        /// A declared volume, `id=url`. Repeatable. url may be file://, https://
        /// (GitHub/Gitea), or ssh://. Example: `--volume v0=file:///srv/v0.git`.
        #[arg(long = "volume", value_name = "ID=URL")]
        volumes: Vec<String>,
        /// URL for the index/log repo (defaults to a local index.git).
        #[arg(long)]
        index_url: Option<String>,
        /// Per-volume full threshold in bytes (default 4 GiB).
        #[arg(long)]
        threshold: Option<u64>,
        /// Min interval between pushes per volume, ms (rate governance).
        #[arg(long, default_value_t = 0)]
        push_interval_ms: u64,
        /// For https:// GitHub/Gitea provisioning: which API to speak.
        #[arg(long, value_enum, default_value_t = HostArg::Gitea)]
        host: HostArg,
        /// Gitea web base URL (e.g. https://gitea.example.com) for API base.
        #[arg(long)]
        gitea_base: Option<String>,
        #[arg(long, value_parser = chunker::parse_chunk_size)]
        chunk_size: Option<usize>,
    },
    /// Chunk, compress, encrypt a file into the store; commit atomically.
    Put {
        /// File to store.
        path: PathBuf,
        /// Store directory (created if missing; holds index.git + volumes/).
        #[arg(long)]
        repo: PathBuf,
        /// Master keyfile (64 hex chars). Created with the store if missing;
        /// REQUIRED (and never regenerated) for existing stores. Losing it
        /// means losing the store.
        #[arg(long)]
        keyfile: PathBuf,
        /// Target AVERAGE chunk size: bytes or k/m suffix (e.g. "512k", "1m").
        /// Pinned into the store on first put (default 1 MiB); later puts must
        /// match or omit it.
        #[arg(long, value_parser = chunker::parse_chunk_size)]
        chunk_size: Option<usize>,
    },
    /// Decrypt and reconstruct a stored file, verifying everything.
    Get {
        /// Stored file name (as shown by `ls`).
        name: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
        /// Where to write the reconstructed file.
        #[arg(long)]
        output: PathBuf,
        /// Read from a pinned log commit (snapshot read) instead of the tip.
        #[arg(long)]
        at: Option<String>,
    },
    /// List stored files (requires the keyfile — all metadata is encrypted).
    Ls {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
        /// List at a pinned log commit (snapshot read) instead of the tip.
        #[arg(long)]
        at: Option<String>,
    },
    /// Print the current log tip commit (for snapshot reads with --at).
    Tip {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
    },
    /// Show per-volume usage vs threshold and the read-path/promisor verdict.
    Stats {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
    },
    /// Logically delete a stored file (DESIGN §12.1). The bytes are reclaimed
    /// later by compaction; deleting an unknown name fails cleanly.
    Rm {
        /// Stored file name (as shown by `ls`).
        name: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
    },
    /// Reclaim dead bytes by compacting eligible volumes (DESIGN §12.3/12.4).
    /// Hysteresis-gated: runs only when the dead-ratio, budget-pressure and
    /// min-interval gates all pass (tunable via GITSTORAGE_* env for tests).
    Compact {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
        /// Ignore the min-interval and pressure gates; compact any volume whose
        /// dead-ratio gate passes. (Never bypasses delete-after-CAS ordering.)
        #[arg(long)]
        force: bool,
    },
    /// Mirror the whole store (index + every volume) to an INDEPENDENT second
    /// backend for durability (DESIGN §14.3). Push-only + idempotent; the mirror
    /// is ciphertext only. Targets are provisioned exactly like `init` (file://
    /// inited as bare repos; https:// created via control-plane REST with
    /// GITSTORAGE_TOKEN; ssh:// assumed to exist). Needs a --to-volume for every
    /// declared volume.
    Mirror {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        keyfile: PathBuf,
        /// Target URL for the index/log repo.
        #[arg(long)]
        to_index: String,
        /// A volume's mirror target, `id=url`. Repeatable; one per volume.
        #[arg(long = "to-volume", value_name = "ID=URL")]
        to_volumes: Vec<String>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum HostArg {
    Github,
    Gitea,
}

impl From<HostArg> for Host {
    fn from(h: HostArg) -> Self {
        match h {
            HostArg::Github => Host::GitHub,
            HostArg::Gitea => Host::Gitea,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Init {
            repo,
            keyfile,
            volumes,
            index_url,
            threshold,
            push_interval_ms,
            host,
            gitea_base,
            chunk_size,
        } => init(InitArgs {
            repo,
            keyfile,
            volumes,
            index_url,
            threshold,
            push_interval_ms,
            host: host.into(),
            gitea_base,
            chunk_size,
        }),
        Cmd::Put {
            path,
            repo,
            keyfile,
            chunk_size,
        } => put(&path, &repo, &keyfile, chunk_size),
        Cmd::Get {
            name,
            repo,
            keyfile,
            output,
            at,
        } => get(&name, &repo, &keyfile, &output, at.as_deref()),
        Cmd::Ls { repo, keyfile, at } => ls(&repo, &keyfile, at.as_deref()),
        Cmd::Tip { repo, keyfile } => tip(&repo, &keyfile),
        Cmd::Stats { repo, keyfile } => stats(&repo, &keyfile),
        Cmd::Rm {
            name,
            repo,
            keyfile,
        } => rm(&name, &repo, &keyfile),
        Cmd::Compact {
            repo,
            keyfile,
            force,
        } => compact(&repo, &keyfile, force),
        Cmd::Mirror {
            repo,
            keyfile,
            to_index,
            to_volumes,
        } => mirror(&repo, &keyfile, &to_index, &to_volumes),
    }
}

/// Open the engine; the keyfile may be created only when the store itself is
/// brand new — never silently regenerated for existing data.
fn open_engine(repo: &Path, keyfile_path: &Path, chunk_size: Option<usize>) -> Result<Engine> {
    let store_is_new = !Engine::store_exists(repo);
    let master = keyfile::load_or_create(keyfile_path, store_is_new)?;
    Engine::open(repo, Keys::new(master), chunk_size)
}

struct InitArgs {
    repo: PathBuf,
    keyfile: PathBuf,
    volumes: Vec<String>,
    index_url: Option<String>,
    threshold: Option<u64>,
    push_interval_ms: u64,
    host: Host,
    gitea_base: Option<String>,
    chunk_size: Option<usize>,
}

/// Parse `id=url` into (id, url).
fn parse_volume_arg(s: &str) -> Result<(String, String)> {
    let (id, url) = s
        .split_once('=')
        .with_context(|| format!("--volume must be ID=URL, got {s:?}"))?;
    if id.is_empty() || url.is_empty() {
        anyhow::bail!("--volume must be ID=URL with non-empty parts, got {s:?}");
    }
    Ok((id.to_string(), url.to_string()))
}

/// Best-effort owner/name extraction from a hosted repo URL for provisioning.
/// e.g. https://github.com/owner/name(.git) -> (owner, name).
fn owner_name_from_url(url: &str) -> Option<(String, String)> {
    let after_scheme = url.split("://").nth(1)?;
    let path = after_scheme.split_once('/')?.1; // strip host
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, name) = path.rsplit_once('/')?;
    // owner may itself contain a leading path segment on some hosts; take the
    // last two path components.
    let owner = owner.rsplit('/').next().unwrap_or(owner);
    Some((owner.to_string(), name.to_string()))
}

/// Provision one volume repo. file:// = init a bare repo directly (data plane
/// is allowed to create LOCAL repos). https:// = control-plane REST create
/// (requires GITSTORAGE_TOKEN), gated + idempotent-safe. ssh:// = must exist.
fn provision_volume(url: &str, host: Host, gitea_base: Option<&str>) -> Result<()> {
    if let Some(dir) = url.strip_prefix("file://") {
        Bare::open(Path::new(dir))
            .with_context(|| format!("initializing local bare repo {dir}"))?;
        println!("  {url}: local bare repo ready");
        return Ok(());
    }
    if url.starts_with("https://") {
        let (owner, name) = owner_name_from_url(url)
            .with_context(|| format!("cannot parse owner/name from {url}"))?;
        let web_base = gitea_base.map(|s| s.to_string()).unwrap_or_else(|| {
            url.split_once("://")
                .map(|(_, r)| {
                    r.split_once('/')
                        .map(|(h, _)| format!("https://{h}"))
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        });
        let spec = RepoSpec {
            host,
            owner: owner.clone(),
            name: name.clone(),
            web_base,
        };
        match ensure_repo(&spec)? {
            Provisioned::Created => println!("  {url}: created private repo {owner}/{name}"),
            Provisioned::AdoptedEmpty => {
                println!("  {url}: adopted existing empty repo {owner}/{name}")
            }
        }
        return Ok(());
    }
    // ssh:// or anything else: we do not create it; it must already exist.
    println!("  {url}: assumed pre-provisioned (no control-plane create for this scheme)");
    Ok(())
}

fn init(a: InitArgs) -> Result<()> {
    if a.volumes.is_empty() {
        anyhow::bail!("init requires at least one --volume ID=URL");
    }
    let threshold = a.threshold.unwrap_or(DEFAULT_VOLUME_FULL_THRESHOLD);
    let mut vol_configs = Vec::new();
    println!("provisioning {} volume(s):", a.volumes.len());
    for v in &a.volumes {
        let (id, url) = parse_volume_arg(v)?;
        provision_volume(&url, a.host, a.gitea_base.as_deref())?;
        vol_configs.push(VolumeConfig {
            id,
            url: Some(url),
            push_interval_ms: a.push_interval_ms,
            volume_full_threshold: threshold,
        });
    }
    // Provision the index repo too if it's a file:// URL.
    if let Some(iu) = &a.index_url {
        provision_volume(iu, a.host, a.gitea_base.as_deref()).context("provisioning index repo")?;
    }

    engine::init_config_with_volumes(&a.repo, a.chunk_size, vol_configs, a.index_url)?;
    // Create the keyfile for the new store (0600) if absent.
    let _ = keyfile::load_or_create(&a.keyfile, true)?;
    println!("store initialized at {}", a.repo.display());
    Ok(())
}

fn put(path: &Path, repo: &Path, keyfile_path: &Path, chunk_size: Option<usize>) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("input path {} has no usable file name", path.display()))?
        .to_string();
    let engine = open_engine(repo, keyfile_path, chunk_size)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let stats = engine.put(&name, BufReader::new(file))?;

    println!(
        "{name}: {} chunks, {} new, {} deduped, {} ciphertext bytes written",
        stats.total_chunks,
        stats.new_chunks,
        stats.total_chunks - stats.new_chunks,
        stats.ciphertext_bytes
    );
    if stats.committed {
        match &stats.volume {
            Some(v) => println!("committed to {} (segment → volume {v})", repo.display()),
            None => println!("committed to {}", repo.display()),
        }
    } else {
        println!("no changes (identical content already stored)");
    }
    Ok(())
}

fn get(
    name: &str,
    repo: &Path,
    keyfile_path: &Path,
    output: &Path,
    at: Option<&str>,
) -> Result<()> {
    let engine = open_engine(repo, keyfile_path, None)?;
    let out = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let size = engine.get(name, BufWriter::new(out), at)?;
    println!(
        "{name}: {size} bytes reconstructed to {}, all hashes verified",
        output.display()
    );
    Ok(())
}

fn ls(repo: &Path, keyfile_path: &Path, at: Option<&str>) -> Result<()> {
    let engine = open_engine(repo, keyfile_path, None)?;
    let namespace = engine.namespace_at(at)?;
    if namespace.is_empty() {
        println!("store is empty");
        return Ok(());
    }
    for m in namespace.values() {
        println!("{}\t{} bytes\t{} chunks", m.name, m.size, m.chunks.len());
    }
    Ok(())
}

fn tip(repo: &Path, keyfile_path: &Path) -> Result<()> {
    let engine = open_engine(repo, keyfile_path, None)?;
    match engine.log_tip()? {
        Some(oid) => println!("{oid}"),
        None => println!("(empty log)"),
    }
    Ok(())
}

fn stats(repo: &Path, keyfile_path: &Path) -> Result<()> {
    let engine = open_engine(repo, keyfile_path, None)?;
    println!("volumes:");
    for v in engine.stats()? {
        let tag = if v.spare { " [spare]" } else { "" };
        let util = (v.utilization() * 100.0).round() as u64;
        let dead = (v.dead_ratio() * 100.0).round() as u64;
        println!(
            "  {id}{tag}: {total} / {threshold} bytes ({util}% full) \
             — live {live}, dead {deadb} ({dead}% dead)",
            id = v.id,
            total = v.total,
            threshold = v.threshold,
            live = v.live,
            deadb = v.dead,
        );
    }
    let notes = engine.read_path_notes();
    if notes.is_empty() {
        println!("read path: local (no promisor probe needed)");
    } else {
        println!("read path:");
        for (id, note) in notes {
            println!("  {id}: {note}");
        }
    }
    Ok(())
}

fn rm(name: &str, repo: &Path, keyfile_path: &Path) -> Result<()> {
    let mut engine = open_engine(repo, keyfile_path, None)?;
    engine.remove(name)?;
    println!("removed {name} (bytes reclaimed later by compaction)");
    Ok(())
}

fn mirror(repo: &Path, keyfile_path: &Path, to_index: &str, to_volumes: &[String]) -> Result<()> {
    let engine = open_engine(repo, keyfile_path, None)?;
    // Provision file:// targets (init bare repos); https:// must pre-exist.
    // This is the same control-plane discipline as `init` — no data-plane
    // repo creation. `provision_volume` only creates local/file repos.
    provision_volume(to_index, Host::Gitea, None).context("provisioning mirror index")?;
    let mut targets = std::collections::BTreeMap::new();
    for v in to_volumes {
        let (id, url) = parse_volume_arg(v)?;
        provision_volume(&url, Host::Gitea, None)
            .with_context(|| format!("provisioning mirror volume {id}"))?;
        targets.insert(id, url);
    }
    engine.mirror(to_index, &targets)?;
    println!(
        "mirrored store to {} volume target(s) + index {}",
        targets.len(),
        to_index
    );
    Ok(())
}

fn compact(repo: &Path, keyfile_path: &Path, force: bool) -> Result<()> {
    let mut engine = open_engine(repo, keyfile_path, None)?;
    let report = engine.compact(force)?;
    if report.compacted.is_empty() {
        println!("no volume eligible for compaction (gates not met)");
    } else {
        for c in &report.compacted {
            println!(
                "compacted {vol}: {before} -> {after} bytes ({moved} live chunks moved to {dest})",
                vol = c.volume,
                before = c.bytes_before,
                after = c.bytes_after,
                moved = c.chunks_moved,
                dest = c.dest_volume,
            );
        }
    }
    if report.orphans_swept > 0 {
        println!("swept {} orphan segment(s)", report.orphans_swept);
    }
    Ok(())
}
