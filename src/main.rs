//! git-storage CLI: milestone-3 — sealed segments + CAS transaction log.
//!
//! Thin binary over the library crate (src/lib.rs). See DESIGN.md for the
//! architecture and IMPLEMENTATION-PLAN.md for the milestone ladder.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use git_storage::chunker;
use git_storage::crypto::{keyfile, Keys};
use git_storage::engine::Engine;

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
}

fn main() -> Result<()> {
    match Cli::parse().command {
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
    }
}

/// Open the engine; the keyfile may be created only when the store itself is
/// brand new — never silently regenerated for existing data.
fn open_engine(repo: &Path, keyfile_path: &Path, chunk_size: Option<usize>) -> Result<Engine> {
    let store_is_new = !Engine::store_exists(repo);
    let master = keyfile::load_or_create(keyfile_path, store_is_new)?;
    Engine::open(repo, Keys::new(master), chunk_size)
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
        println!("committed to {}", repo.display());
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
