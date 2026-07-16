//! The store: a local git repository holding content-addressed ciphertext
//! chunk objects, sealed manifests, and a store config.
//!
//! MILESTONE 2 layout (inside the `--repo` directory):
//! ```text
//! config.json                      # chunker size params + format version
//! objects/<aa>/<chunk-id>          # AEAD-sealed zstd chunks; id = BLAKE3(ciphertext)
//! manifests/<name-tag>.sealed      # encrypted manifests; tag = keyed hash of name
//! ```
//! Everything the backend sees is ciphertext or a content address; the store
//! config holds only size parameters (the gear seed is key-derived and never
//! written to disk).
//!
//! Git operations shell out to the `git` CLI — LOCAL subcommands only (init,
//! add, diff, commit). No network git command (push/fetch/clone/pull) exists
//! anywhere in this codebase, and every invocation sets GIT_TERMINAL_PROMPT=0
//! so git can never prompt for credentials. Commits use a fixed tool identity
//! rather than the user's global git config. The target design moves to
//! in-process packfile construction via gitoxide (DESIGN.md Appendix B).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::chunker::ChunkerParams;
use crate::crypto::Keys;
use crate::manifest::Manifest;

/// Store-level configuration, written once at store creation and immutable
/// thereafter: a store MUST chunk identically for its whole lifetime, or new
/// puts of unchanged data stop dedup-ing against old chunks.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Config/format version (bound into every AEAD as associated data).
    pub version: u32,
    pub chunker: ChunkerParams,
    /// zstd level used at seal time (informational; decompression is level-
    /// agnostic).
    pub zstd_level: i32,
}

pub struct Store {
    root: PathBuf,
    keys: Keys,
}

impl Store {
    /// Open the store at `root` with the given keys, initializing directory
    /// layout and `git init` on first use.
    pub fn open(root: &Path, keys: Keys) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))
            .with_context(|| format!("creating {}/objects", root.display()))?;
        fs::create_dir_all(root.join("manifests"))
            .with_context(|| format!("creating {}/manifests", root.display()))?;
        if !root.join(".git").exists() {
            run_git(root, &["init", "--quiet"])?;
        }
        Ok(Self {
            root: root.to_path_buf(),
            keys,
        })
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Load the store config, or create-and-pin it on first use.
    pub fn config_or_init(
        &self,
        requested_avg: Option<usize>,
        zstd_level: i32,
    ) -> Result<StoreConfig> {
        let path = self.config_path();
        if path.exists() {
            let data =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let config: StoreConfig = serde_json::from_str(&data)
                .with_context(|| format!("parsing {}", path.display()))?;
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
        let avg = requested_avg.unwrap_or(crate::chunker::DEFAULT_AVG_SIZE);
        let config = StoreConfig {
            version: crate::crypto::FORMAT_VERSION,
            chunker: ChunkerParams::from_avg(avg)?,
            zstd_level,
        };
        let json = serde_json::to_string_pretty(&config).context("serializing store config")?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(config)
    }

    fn object_path(&self, chunk_id: &str) -> PathBuf {
        self.root
            .join("objects")
            .join(&chunk_id[..2])
            .join(chunk_id)
    }

    fn manifest_path(&self, name: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{}.sealed", self.keys.name_tag(name)))
    }

    /// Write a ciphertext chunk object if absent. Returns `true` if newly
    /// written, `false` if it already existed (dedup hit).
    pub fn put_object(&self, chunk_id: &str, ciphertext: &[u8]) -> Result<bool> {
        let path = self.object_path(chunk_id);
        if path.exists() {
            return Ok(false);
        }
        let dir = path.parent().expect("object path has parent");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        // Write via temp file + rename so a crash never leaves a truncated
        // object under its content address.
        let tmp = dir.join(format!(".tmp-{chunk_id}"));
        fs::write(&tmp, ciphertext).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(true)
    }

    /// Read a ciphertext chunk object and verify its content address before
    /// returning it. (AEAD verification happens at decryption, in crypto.rs.)
    pub fn get_object(&self, chunk_id: &str) -> Result<Vec<u8>> {
        let path = self.object_path(chunk_id);
        let data = fs::read(&path)
            .with_context(|| format!("missing chunk object {chunk_id} ({})", path.display()))?;
        let actual = blake3::hash(&data).to_hex().to_string();
        if actual != chunk_id {
            bail!(
                "chunk hash mismatch: manifest expects {chunk_id}, object file hashes to {actual} — store is corrupt"
            );
        }
        Ok(data)
    }

    /// Seal and write a manifest under its keyed name tag.
    ///
    /// Manifest sealing uses a random nonce, so identical plaintext produces
    /// different ciphertext each time. To keep identical re-puts commit-free
    /// (and history quiet), skip the write when an existing manifest already
    /// decrypts to the same plaintext.
    pub fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        let json = manifest.to_json()?;
        let path = self.manifest_path(&manifest.name);
        if path.exists() {
            if let Ok(existing) = fs::read(&path) {
                if let Ok(plaintext) = self.keys.open_manifest(&existing) {
                    if plaintext == json {
                        return Ok(()); // unchanged; don't churn ciphertext
                    }
                }
            }
        }
        let sealed = self.keys.seal_manifest(&json)?;
        fs::write(&path, sealed).with_context(|| format!("writing manifest {}", path.display()))
    }

    /// Load and open the manifest for `name`.
    pub fn load_manifest(&self, name: &str) -> Result<Manifest> {
        let path = self.manifest_path(name);
        if !path.exists() {
            bail!("no manifest for {name:?} in this store (see `ls`)");
        }
        let sealed =
            fs::read(&path).with_context(|| format!("reading manifest {}", path.display()))?;
        Manifest::from_json(&self.keys.open_manifest(&sealed)?)
    }

    /// All manifests in the store, sorted by logical name. (Requires the keys:
    /// names are not recoverable from the directory listing alone — that is
    /// the point.)
    pub fn list_manifests(&self) -> Result<Vec<Manifest>> {
        let dir = self.root.join("manifests");
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry.context("reading manifests dir entry")?.path();
            if path.extension().is_some_and(|e| e == "sealed") {
                let sealed = fs::read(&path)
                    .with_context(|| format!("reading manifest {}", path.display()))?;
                out.push(Manifest::from_json(&self.keys.open_manifest(&sealed)?)?);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// `git add -A && git commit` in the store repo. Returns `false` (without
    /// error) when there is nothing to commit, e.g. an identical re-put.
    /// Commit messages are generic on purpose: file names are encrypted
    /// everywhere else, so they must not leak via git history.
    pub fn commit(&self, message: &str) -> Result<bool> {
        run_git(&self.root, &["add", "-A"])?;
        // `git diff --cached --quiet` exits 1 when there ARE staged changes.
        let staged = git_command(&self.root)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .context("running git diff --cached")?;
        if staged.success() {
            return Ok(false); // nothing staged
        }
        run_git(&self.root, &["commit", "--quiet", "-m", message])?;
        Ok(true)
    }
}

/// Base git command: local-only use, credential prompts hard-disabled, and a
/// fixed tool identity so commits never depend on (or read) the user's global
/// identity configuration.
fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        // Never prompt for credentials (there are no network ops, but this
        // makes the guarantee structural rather than incidental).
        .env("GIT_TERMINAL_PROMPT", "0")
        // Store commits are machine-made; use the tool's identity.
        .args([
            "-c",
            "user.name=git-storage",
            "-c",
            "user.email=git-storage@localhost",
            "-c",
            "commit.gpgsign=false",
        ]);
    cmd
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = git_command(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {:?} failed in {}: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
