//! git-storage: milestone-0 walking skeleton.
//!
//! Stores files in a local git repository as fixed-size, content-addressed
//! chunks. See DESIGN.md for the target architecture and IMPLEMENTATION-PLAN.md
//! for what replaces each naive piece here.

mod chunker;
mod manifest;
mod store;

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use chunker::DEFAULT_CHUNK_SIZE;
use manifest::{ChunkRef, Manifest};
use store::Store;

#[derive(Parser)]
#[command(
    name = "git-storage",
    version,
    about = "Store files in a git repo as content-addressed chunks (milestone 0)"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Chunk a file into the store and commit it.
    Put {
        /// File to store.
        path: PathBuf,
        /// Store repository directory (created and `git init`ed if missing).
        #[arg(long)]
        repo: PathBuf,
        /// Chunk size: bytes or with k/m suffix (e.g. "512k", "1m"). Default 1 MiB.
        #[arg(long, value_parser = chunker::parse_chunk_size, default_value_t = DEFAULT_CHUNK_SIZE)]
        chunk_size: usize,
    },
    /// Reconstruct a stored file, verifying every chunk and the whole file.
    Get {
        /// Stored file name (as shown by `ls`).
        name: String,
        #[arg(long)]
        repo: PathBuf,
        /// Where to write the reconstructed file.
        #[arg(long)]
        output: PathBuf,
    },
    /// List stored files.
    Ls {
        #[arg(long)]
        repo: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Put {
            path,
            repo,
            chunk_size,
        } => put(&path, &repo, chunk_size),
        Cmd::Get { name, repo, output } => get(&name, &repo, &output),
        Cmd::Ls { repo } => ls(&repo),
    }
}

fn put(path: &Path, repo: &Path, chunk_size: usize) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("input path {} has no usable file name", path.display()))?
        .to_string();
    let store = Store::open(repo)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut chunks: Vec<ChunkRef> = Vec::new();
    let mut new_chunks = 0usize;
    let mut new_bytes = 0u64;
    let mut total_size = 0u64;

    let file_hash = chunker::stream_chunks(reader, chunk_size, |chunk| {
        total_size += chunk.data.len() as u64;
        if store.put_object(&chunk.hash, &chunk.data)? {
            new_chunks += 1;
            new_bytes += chunk.data.len() as u64;
        }
        chunks.push(ChunkRef {
            hash: chunk.hash,
            len: chunk.data.len() as u64,
        });
        Ok(())
    })?;

    let total = chunks.len();
    let manifest = Manifest {
        name: name.clone(),
        size: total_size,
        chunk_size: chunk_size as u64,
        file_hash,
        chunks,
    };
    store.save_manifest(&manifest)?;

    let committed = store.commit(&format!("put {name}: {total} chunks ({new_chunks} new)"))?;
    println!(
        "{name}: {total} chunks, {new_chunks} new, {} deduped, {new_bytes} bytes written",
        total - new_chunks
    );
    if committed {
        println!("committed to {}", repo.display());
    } else {
        println!("no changes (identical content already stored)");
    }
    Ok(())
}

fn get(name: &str, repo: &Path, output: &Path) -> Result<()> {
    let store = Store::open(repo)?;
    let manifest = store.load_manifest(name)?;

    let out = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut writer = BufWriter::new(out);
    let mut file_hasher = blake3::Hasher::new();

    for chunk_ref in &manifest.chunks {
        // get_object verifies the chunk's own hash before returning it.
        let data = store.get_object(&chunk_ref.hash)?;
        if data.len() as u64 != chunk_ref.len {
            bail!(
                "chunk {} length mismatch: manifest says {}, object is {}",
                chunk_ref.hash,
                chunk_ref.len,
                data.len()
            );
        }
        chunker::write_and_hash(&mut writer, &mut file_hasher, &data)?;
    }
    writer.flush().context("flushing output")?;

    let actual = file_hasher.finalize().to_hex().to_string();
    if actual != manifest.file_hash {
        bail!(
            "whole-file hash mismatch for {name}: manifest expects {}, reconstruction hashes to {actual}",
            manifest.file_hash
        );
    }
    println!(
        "{name}: {} bytes reconstructed to {}, all hashes verified",
        manifest.size,
        output.display()
    );
    Ok(())
}

fn ls(repo: &Path) -> Result<()> {
    let store = Store::open(repo)?;
    let manifests = store.list_manifests()?;
    if manifests.is_empty() {
        println!("store is empty");
        return Ok(());
    }
    for m in manifests {
        println!("{}\t{} bytes\t{} chunks", m.name, m.size, m.chunks.len());
    }
    Ok(())
}
