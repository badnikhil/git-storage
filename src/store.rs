//! The store: a local git repository holding content-addressed chunk objects
//! and per-file manifests.
//!
//! MILESTONE 0 layout (inside the `--repo` directory):
//! ```text
//! objects/<aa>/<full-blake3-hex>   # chunk files; <aa> = first 2 hex chars
//! manifests/<file-name>.json      # one manifest per stored file
//! ```
//! Git operations shell out to the `git` CLI. The target design moves to
//! in-process packfile construction via gitoxide (DESIGN.md Appendix B) and to
//! sealed segments + a transaction log (DESIGN.md Sections 3 and 8); this loose
//! layout is the milestone-0 stand-in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::manifest::Manifest;

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
        let staged = Command::new("git")
            .current_dir(&self.root)
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

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(dir)
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
