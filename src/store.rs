//! The store: a local git repository holding content-addressed chunk objects,
//! per-file manifests, and a store config that pins chunking parameters.
//!
//! Layout (inside the `--repo` directory):
//! ```text
//! config.json                      # pinned chunker params (see StoreConfig)
//! objects/<aa>/<full-blake3-hex>   # chunk files; <aa> = first 2 hex chars
//! manifests/<file-name>.json      # one manifest per stored file
//! ```
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
use crate::manifest::Manifest;

/// Store-level configuration, written once at store creation and immutable
/// thereafter: a store MUST chunk identically for its whole lifetime, or new
/// puts of unchanged data stop dedup-ing against old chunks.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Config format version.
    pub version: u32,
    pub chunker: ChunkerParams,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open the store at `root`, initializing directory layout and `git init`
    /// on first use.
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))
            .with_context(|| format!("creating {}/objects", root.display()))?;
        fs::create_dir_all(root.join("manifests"))
            .with_context(|| format!("creating {}/manifests", root.display()))?;
        if !root.join(".git").exists() {
            run_git(root, &["init", "--quiet"])?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Load the store config, or create-and-pin it on first use.
    ///
    /// `requested_avg` is the `--chunk-size` argument if given. On an existing
    /// store it must match the pinned average (or be omitted); on a new store
    /// it seeds the config (falling back to the default).
    pub fn config_or_init(&self, requested_avg: Option<usize>) -> Result<StoreConfig> {
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
        // Gear seed from OS entropy (RandomState is OS-seeded). Milestone 2
        // replaces this with HKDF(master_key) per DESIGN.md Section 5.4.
        let seed = {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        let config = StoreConfig {
            version: 1,
            chunker: ChunkerParams::from_avg(avg, seed)?,
        };
        let json = serde_json::to_string_pretty(&config).context("serializing store config")?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(config)
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.root.join("objects").join(&hash[..2]).join(hash)
    }

    fn manifest_path(&self, name: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{name}.json"))
    }

    /// Write a chunk object if absent. Returns `true` if newly written,
    /// `false` if it already existed (dedup hit).
    pub fn put_object(&self, hash: &str, data: &[u8]) -> Result<bool> {
        let path = self.object_path(hash);
        if path.exists() {
            return Ok(false);
        }
        let dir = path.parent().expect("object path has parent");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        // Write via temp file + rename so a crash never leaves a truncated
        // object under its content address.
        let tmp = dir.join(format!(".tmp-{hash}"));
        fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(true)
    }

    /// Read a chunk object and verify its content hash before returning it.
    pub fn get_object(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.object_path(hash);
        let data = fs::read(&path)
            .with_context(|| format!("missing chunk object {hash} ({})", path.display()))?;
        let actual = blake3::hash(&data).to_hex().to_string();
        if actual != hash {
            bail!("chunk hash mismatch: manifest expects {hash}, object file hashes to {actual} — store is corrupt");
        }
        Ok(data)
    }

    pub fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        manifest.save(&self.manifest_path(&manifest.name))
    }

    pub fn load_manifest(&self, name: &str) -> Result<Manifest> {
        let path = self.manifest_path(name);
        if !path.exists() {
            bail!("no manifest for {name:?} in this store (see `ls`)");
        }
        Manifest::load(&path)
    }

    /// All manifests in the store, sorted by name.
    pub fn list_manifests(&self) -> Result<Vec<Manifest>> {
        let dir = self.root.join("manifests");
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry.context("reading manifests dir entry")?.path();
            if path.extension().is_some_and(|e| e == "json") {
                out.push(Manifest::load(&path)?);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// `git add -A && git commit` in the store repo. Returns `false` (without
    /// error) when there is nothing to commit, e.g. an identical re-put.
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
