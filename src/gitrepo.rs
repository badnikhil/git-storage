//! Git bare-repository plumbing: the backend's two primitives, realized on
//! local bare repos (DESIGN.md Section 2.1).
//!
//! - **P1 (reachable blob storage):** `write_blob` / `write_tree` /
//!   `commit_tree` + `set_ref` make objects durable and reachable.
//! - **P2 (atomic compare-and-swap ref update):** `cas_ref` uses
//!   `git update-ref <ref> <new> <old>`, which atomically rejects the update
//!   if the ref's current value is not `<old>`. This is a true CAS with zero
//!   network involvement.
//!
//! MILESTONE 3 backend = local bare repos on the filesystem. M4 swaps this
//! for remote backends where P2 becomes a fast-forward `git push` (rejected
//! when stale) — same semantics, different transport.
//!
//! CREDENTIAL SAFETY: all invocations run with GIT_TERMINAL_PROMPT=0, the
//! user's global/system git config masked out, and a fixed tool identity.
//! There are still no network git commands anywhere in this codebase.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};

/// All-zeros OID: as `<old>` in update-ref it means "ref must not exist yet".
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

static INDEX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A local bare git repository.
pub struct Bare {
    dir: PathBuf,
}

impl Bare {
    /// Open the bare repo at `dir`, creating it (`git init --bare`) if absent.
    pub fn open(dir: &Path) -> Result<Self> {
        if !dir.join("HEAD").exists() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let out = Self::base_command(dir)
                .args(["init", "--bare", "--quiet"])
                .output()
                .context("running git init --bare")?;
            if !out.status.success() {
                bail!(
                    "git init --bare failed in {}: {}",
                    dir.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write a blob object; returns its OID. (P1, object half.)
    pub fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        let out = self.run_stdin(&["hash-object", "-w", "--stdin"], bytes, &[])?;
        Ok(String::from_utf8(out)
            .context("oid utf8")?
            .trim()
            .to_string())
    }

    /// Build a tree from (path, blob-oid) entries via a throwaway index file;
    /// returns the tree OID. Paths may contain '/' — git builds the subtrees.
    pub fn write_tree(&self, entries: &[(String, String)]) -> Result<String> {
        let idx = self.dir.join(format!(
            "gs-index-{}-{}",
            std::process::id(),
            INDEX_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let idx_str = idx.to_str().context("index path utf8")?.to_string();
        let mut input = String::new();
        for (path, oid) in entries {
            input.push_str(&format!("100644 {oid}\t{path}\n"));
        }
        let env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &idx_str)];
        let result = (|| {
            self.run_stdin(&["update-index", "--index-info"], input.as_bytes(), env)?;
            let out = self.run(&["write-tree"], env)?;
            Ok::<String, anyhow::Error>(
                String::from_utf8(out)
                    .context("tree oid utf8")?
                    .trim()
                    .to_string(),
            )
        })();
        let _ = std::fs::remove_file(&idx); // best-effort cleanup
        result
    }

    /// Create a commit of `tree` with the given parents; returns the commit OID.
    pub fn commit_tree(&self, tree: &str, parents: &[String], message: &str) -> Result<String> {
        let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
        for p in parents {
            args.push("-p".into());
            args.push(p.clone());
        }
        args.push("-m".into());
        args.push(message.into());
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = self.run(&arg_refs, &[])?;
        Ok(String::from_utf8(out)
            .context("commit oid utf8")?
            .trim()
            .to_string())
    }

    /// Read a ref's current OID, or None if the ref does not exist.
    pub fn read_ref(&self, refname: &str) -> Result<Option<String>> {
        let out = Self::base_command(&self.dir)
            .args(["rev-parse", "--verify", "--quiet", refname])
            .output()
            .context("running git rev-parse")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8(out.stdout)
                .context("oid utf8")?
                .trim()
                .to_string(),
        ))
    }

    /// P2: atomic compare-and-swap. Sets `refname` to `new` iff its current
    /// value is `old` (None = ref must not exist). Returns false if the CAS
    /// was rejected because the ref moved; errors on anything else.
    pub fn cas_ref(&self, refname: &str, new: &str, old: Option<&str>) -> Result<bool> {
        let old = old.unwrap_or(ZERO_OID);
        let out = Self::base_command(&self.dir)
            .args(["update-ref", refname, new, old])
            .output()
            .context("running git update-ref (CAS)")?;
        if out.status.success() {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        // update-ref reports a value mismatch via these phrasings.
        if stderr.contains("but expected")
            || stderr.contains("cannot lock ref")
            || stderr.contains("reference already exists")
        {
            return Ok(false); // CAS lost the race — caller rebases and retries
        }
        bail!("git update-ref failed unexpectedly: {}", stderr.trim());
    }

    /// Unconditional ref set (used for sealed segment refs, which are
    /// create-once under a unique name — no contention by construction).
    pub fn set_ref(&self, refname: &str, oid: &str) -> Result<()> {
        let out = Self::base_command(&self.dir)
            .args(["update-ref", refname, oid])
            .output()
            .context("running git update-ref")?;
        if !out.status.success() {
            bail!(
                "git update-ref {refname} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Delete a ref (used by tests and, later, orphan sweep).
    pub fn delete_ref(&self, refname: &str) -> Result<()> {
        let out = Self::base_command(&self.dir)
            .args(["update-ref", "-d", refname])
            .output()
            .context("running git update-ref -d")?;
        if !out.status.success() {
            bail!(
                "git update-ref -d {refname} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Read the blob at `<rev>:<path>` (rev may be a ref name or commit OID).
    pub fn read_blob_at(&self, rev: &str, path: &str) -> Result<Vec<u8>> {
        self.run(&["cat-file", "blob", &format!("{rev}:{path}")], &[])
            .with_context(|| format!("reading blob {rev}:{path}"))
    }

    /// Commits reachable from `tip`, newest first (linear log expected).
    pub fn rev_list(&self, tip: &str) -> Result<Vec<String>> {
        let out = self.run(&["rev-list", tip], &[])?;
        Ok(String::from_utf8(out)
            .context("rev-list utf8")?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// All refs under a prefix, e.g. "refs/segments/".
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let out = self.run(
            &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
            &[],
        )?;
        Ok(String::from_utf8(out)
            .context("for-each-ref utf8")?
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.to_string(), it.next()?.to_string()))
            })
            .collect())
    }

    // ---------- internals ----------

    /// Base git command with credential/config isolation and tool identity.
    fn base_command(dir: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(dir);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "git-storage")
            .env("GIT_AUTHOR_EMAIL", "git-storage@localhost")
            .env("GIT_COMMITTER_NAME", "git-storage")
            .env("GIT_COMMITTER_EMAIL", "git-storage@localhost")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS");
        cmd
    }

    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut cmd = Self::base_command(&self.dir);
        cmd.args(args);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd
            .output()
            .with_context(|| format!("running git {args:?}"))?;
        if !out.status.success() {
            bail!(
                "git {:?} failed in {}: {}",
                args,
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    fn run_stdin(
        &self,
        args: &[&str],
        stdin_bytes: &[u8],
        extra_env: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        use std::io::Write;
        let mut cmd = Self::base_command(&self.dir);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning git {args:?}"))?;
        child
            .stdin
            .take()
            .context("child stdin")?
            .write_all(stdin_bytes)
            .context("writing to git stdin")?;
        let out = child
            .wait_with_output()
            .with_context(|| format!("waiting for git {args:?}"))?;
        if !out.status.success() {
            bail!(
                "git {:?} failed in {}: {}",
                args,
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, Bare) {
        let tmp = tempfile::TempDir::new().unwrap();
        let bare = Bare::open(&tmp.path().join("r.git")).unwrap();
        (tmp, bare)
    }

    #[test]
    fn blob_tree_commit_ref_roundtrip() {
        let (_t, r) = repo();
        let b1 = r.write_blob(b"chunk one").unwrap();
        let b2 = r.write_blob(b"chunk two").unwrap();
        let tree = r
            .write_tree(&[("aa/bb/one".into(), b1.clone()), ("aa/cc/two".into(), b2)])
            .unwrap();
        let commit = r.commit_tree(&tree, &[], "segment test").unwrap();
        r.set_ref("refs/segments/s1", &commit).unwrap();

        assert_eq!(r.read_ref("refs/segments/s1").unwrap().unwrap(), commit);
        assert_eq!(
            r.read_blob_at("refs/segments/s1", "aa/bb/one").unwrap(),
            b"chunk one"
        );
    }

    #[test]
    fn cas_semantics() {
        let (_t, r) = repo();
        let blob = r.write_blob(b"x").unwrap();
        let tree = r.write_tree(&[("txn".into(), blob)]).unwrap();
        let c1 = r.commit_tree(&tree, &[], "t1").unwrap();

        // Create: succeeds only when ref absent.
        assert!(r.cas_ref("refs/heads/log", &c1, None).unwrap());
        assert!(!r.cas_ref("refs/heads/log", &c1, None).unwrap()); // exists now

        // Advance with correct old value: ok.
        let c2 = r
            .commit_tree(&tree, std::slice::from_ref(&c1), "t2")
            .unwrap();
        assert!(r.cas_ref("refs/heads/log", &c2, Some(&c1)).unwrap());

        // Advance with STALE old value: rejected, ref unchanged.
        let c3 = r
            .commit_tree(&tree, std::slice::from_ref(&c1), "t3-stale")
            .unwrap();
        assert!(!r.cas_ref("refs/heads/log", &c3, Some(&c1)).unwrap());
        assert_eq!(r.read_ref("refs/heads/log").unwrap().unwrap(), c2);
    }

    #[test]
    fn rev_list_is_newest_first() {
        let (_t, r) = repo();
        let blob = r.write_blob(b"p").unwrap();
        let tree = r.write_tree(&[("txn".into(), blob)]).unwrap();
        let c1 = r.commit_tree(&tree, &[], "1").unwrap();
        let c2 = r
            .commit_tree(&tree, std::slice::from_ref(&c1), "2")
            .unwrap();
        let c3 = r
            .commit_tree(&tree, std::slice::from_ref(&c2), "3")
            .unwrap();
        assert_eq!(r.rev_list(&c3).unwrap(), vec![c3, c2, c1]);
    }
}
