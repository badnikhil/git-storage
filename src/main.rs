//! git-storage: milestone-2 walking skeleton.
//!
//! Stores files in a local git repository as content-defined (FastCDC),
//! zstd-compressed, XChaCha20-Poly1305-sealed, content-addressed chunks.
//! Everything the store repo contains is ciphertext; a keyfile holds the
//! master key. See DESIGN.md for the target architecture.

mod chunker;
mod crypto;
mod manifest;
mod store;

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crypto::{keyfile, Keys};
use manifest::{ChunkRef, Manifest};
use store::Store;

/// Default zstd compression level (DESIGN.md Section 6.1 PROPOSED).
const ZSTD_LEVEL: i32 = 3;

#[derive(Parser)]
#[command(
    name = "git-storage",
    version,
    about = "Encrypted, deduplicating file storage in a git repo (milestone 2)"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Chunk, compress, encrypt a file into the store and commit it.
    Put {
        /// File to store.
        path: PathBuf,
        /// Store repository directory (created and `git init`ed if missing).
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
    },
    /// List stored files (requires the keyfile — names are encrypted).
    Ls {
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
        } => get(&name, &repo, &keyfile, &output),
        Cmd::Ls { repo, keyfile } => ls(&repo, &keyfile),
    }
}

/// Open the store; the keyfile may be created only when the store itself is
/// brand new (no config yet) — never silently regenerated for existing data.
fn open_store(repo: &Path, keyfile_path: &Path) -> Result<Store> {
    let store_is_new = !repo.join("config.json").exists();
    let master = keyfile::load_or_create(keyfile_path, store_is_new)?;
    Store::open(repo, Keys::new(master))
}

fn put(path: &Path, repo: &Path, keyfile_path: &Path, chunk_size: Option<usize>) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("input path {} has no usable file name", path.display()))?
        .to_string();
    let store = open_store(repo, keyfile_path)?;
    let config = store.config_or_init(chunk_size, ZSTD_LEVEL)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut chunks: Vec<ChunkRef> = Vec::new();
    let mut new_chunks = 0usize;
    let mut new_bytes = 0u64;
    let mut total_size = 0u64;

    let gear_seed = store.keys().gear_seed;
    let file_hash = chunker::stream_chunks(reader, &config.chunker, gear_seed, |chunk| {
        total_size += chunk.data.len() as u64;
        let sealed = store.keys().seal_chunk(&chunk.data, config.zstd_level)?;
        if store.put_object(&sealed.chunk_id, &sealed.ciphertext)? {
            new_chunks += 1;
            new_bytes += sealed.ciphertext.len() as u64;
        }
        chunks.push(ChunkRef {
            id: sealed.chunk_id,
            plaintext_hash: sealed.plaintext_hash_hex,
            len: chunk.data.len() as u64,
        });
        Ok(())
    })?;

    let total = chunks.len();
    let manifest = Manifest {
        name: name.clone(),
        size: total_size,
        avg_chunk_size: config.chunker.avg_size as u64,
        file_hash,
        chunks,
    };
    store.save_manifest(&manifest)?;

    // Generic commit message: names are encrypted everywhere else in the
    // store; they must not leak through git history.
    let committed = store.commit(&format!("put: {total} chunks ({new_chunks} new)"))?;
    println!(
        "{name}: {total} chunks, {new_chunks} new, {} deduped, {new_bytes} ciphertext bytes written",
        total - new_chunks
    );
    if committed {
        println!("committed to {}", repo.display());
    } else {
        println!("no changes (identical content already stored)");
    }
    Ok(())
}

fn get(name: &str, repo: &Path, keyfile_path: &Path, output: &Path) -> Result<()> {
    let store = open_store(repo, keyfile_path)?;
    let manifest = store.load_manifest(name)?;

    let out = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut writer = BufWriter::new(out);
    let mut file_hasher = blake3::Hasher::new();

    for chunk_ref in &manifest.chunks {
        // get_object verifies the content address; open_chunk verifies the
        // AEAD tag and the plaintext hash.
        let ciphertext = store.get_object(&chunk_ref.id)?;
        let plaintext = store
            .keys()
            .open_chunk(&ciphertext, &chunk_ref.plaintext_hash)?;
        if plaintext.len() as u64 != chunk_ref.len {
            bail!(
                "chunk {} length mismatch: manifest says {}, plaintext is {}",
                chunk_ref.id,
                chunk_ref.len,
                plaintext.len()
            );
        }
        chunker::write_and_hash(&mut writer, &mut file_hasher, &plaintext)?;
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

fn ls(repo: &Path, keyfile_path: &Path) -> Result<()> {
    let store = open_store(repo, keyfile_path)?;
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
