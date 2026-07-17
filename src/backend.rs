//! Backend abstraction: the two primitives of DESIGN.md Section 2.1 as a
//! trait, with two transports.
//!
//! - [`LocalBackend`] — a bare repo on local disk; CAS = `git update-ref`.
//!   (Milestone 3's transport.)
//! - [`RemoteBackend`] — a REAL git remote spoken to over the git wire
//!   protocol: `ls-remote` (read refs), `push` (publish refs; CAS via
//!   `--force-with-lease=<ref>:<expected>`), `fetch` (read path). A local
//!   mirror bare repo stages objects before push and caches fetched ones.
//!   The transport is URL-agnostic: `file://` (tested in CI — the same
//!   send-pack/receive-pack path), `https://` Gitea/GitHub, or `ssh://`.
//!
//! CREDENTIAL SAFETY (hard rule, see CLAUDE.md): no credential helpers, no
//! prompts (GIT_TERMINAL_PROMPT=0 everywhere), no reading the user's git
//! config. HTTPS auth, when a real host is configured, comes ONLY from the
//! GITSTORAGE_TOKEN environment variable, injected as an Authorization
//! header for that single invocation. Tests use file:// URLs exclusively.
//!
//! Rate governance (DESIGN.md Section 9.5): RemoteBackend enforces a
//! min-interval between pushes per volume (token-bucket-lite), because
//! hosted backends document push-rate ceilings (GitHub: 6/min/repo), and
//! backs off with exponential delay + jitter on HTTP 429.
//!
//! Read path (DESIGN.md Section 10.3): RemoteBackend prefers a promisor
//! (blob-filtered) fetch of a single blob BY OID. It walks the segment tree
//! locally (trees are present in a blob-filtered clone) to resolve the chunk
//! fanout path → blob OID, then demand-fetches exactly that blob. A runtime
//! capability probe detects servers without partial-clone support and falls
//! back to a full segment fetch; the probe verdict is observable via
//! [`RemoteBackend::promisor_supported`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::gitrepo::Bare;

pub mod provision;

/// The two primitives (plus the plumbing needed to exercise them). Everything
/// the engine does goes through this surface.
pub trait Backend: Send {
    /// P1: durable object writes (reachability is the caller's duty via refs).
    fn write_blob(&self, bytes: &[u8]) -> Result<String>;
    fn write_tree(&self, entries: &[(String, String)]) -> Result<String>;
    fn commit_tree(&self, tree: &str, parents: &[String], message: &str) -> Result<String>;

    /// Publish a ref unconditionally (sealed segments: unique names, no races).
    fn set_ref(&self, refname: &str, oid: &str) -> Result<()>;
    /// P2: atomic compare-and-swap on a ref. None = ref must not exist.
    /// Ok(false) = lost the race (caller rebases and retries).
    fn cas_ref(&self, refname: &str, new: &str, old: Option<&str>) -> Result<bool>;
    fn delete_ref(&self, refname: &str) -> Result<()>;

    /// Read the CURRENT ref value as the backend sees it (for RemoteBackend
    /// this consults the remote, not a stale mirror).
    fn read_ref(&self, refname: &str) -> Result<Option<String>>;
    fn list_refs(&self, prefix: &str) -> Result<Vec<(String, String)>>;

    /// Read path. `rev` may be a ref name or commit OID that this backend
    /// published or fetched.
    fn read_blob_at(&self, rev: &str, path: &str) -> Result<Vec<u8>>;
    fn rev_list(&self, tip: &str) -> Result<Vec<String>>;
    /// Recursive (path, blob-oid, size) listing of a commit's tree — used for
    /// volume accounting (live/dead bytes) without extra bookkeeping.
    fn ls_tree_sizes(&self, rev: &str) -> Result<Vec<(String, u64)>>;
    /// Unix timestamp of a commit (orphan-sweep safety window).
    fn commit_time(&self, rev: &str) -> Result<i64>;

    /// Destroy the entire store this backend fronts (compaction retirement:
    /// the only true delete, DESIGN.md Section 12). Local/file only; real
    /// hosts require an operator/control-plane action instead.
    fn destroy(&mut self) -> Result<()>;
    /// Recreate empty after destroy (slot reuse, DESIGN.md Section 15.4).
    fn recreate(&mut self) -> Result<()>;

    /// Mirror EVERY ref (and all reachable objects) of this backend's repo to an
    /// independent target git URL (whole-store mirror, DESIGN.md Section 14.3).
    /// Push-only + idempotent. The network push itself lives in [`mirror_repo`]
    /// (this file), keeping ALL network git in the backend layer.
    fn mirror_to(&self, target_url: &str) -> Result<()>;

    /// Observability: a short human-readable note about how this backend last
    /// served a read (e.g. the promisor-probe verdict). Local returns None.
    fn read_path_note(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------- local ----

/// Bare repo on local disk (M3 transport).
pub struct LocalBackend {
    dir: PathBuf,
    repo: Bare,
}

impl LocalBackend {
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            dir: dir.to_path_buf(),
            repo: Bare::open(dir)?,
        })
    }
}

impl Backend for LocalBackend {
    fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        self.repo.write_blob(bytes)
    }
    fn write_tree(&self, entries: &[(String, String)]) -> Result<String> {
        self.repo.write_tree(entries)
    }
    fn commit_tree(&self, tree: &str, parents: &[String], message: &str) -> Result<String> {
        self.repo.commit_tree(tree, parents, message)
    }
    fn set_ref(&self, refname: &str, oid: &str) -> Result<()> {
        self.repo.set_ref(refname, oid)
    }
    fn cas_ref(&self, refname: &str, new: &str, old: Option<&str>) -> Result<bool> {
        self.repo.cas_ref(refname, new, old)
    }
    fn delete_ref(&self, refname: &str) -> Result<()> {
        self.repo.delete_ref(refname)
    }
    fn read_ref(&self, refname: &str) -> Result<Option<String>> {
        self.repo.read_ref(refname)
    }
    fn list_refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        self.repo.list_refs(prefix)
    }
    fn read_blob_at(&self, rev: &str, path: &str) -> Result<Vec<u8>> {
        self.repo.read_blob_at(rev, path)
    }
    fn rev_list(&self, tip: &str) -> Result<Vec<String>> {
        self.repo.rev_list(tip)
    }
    fn ls_tree_sizes(&self, rev: &str) -> Result<Vec<(String, u64)>> {
        self.repo.ls_tree_sizes(rev)
    }
    fn commit_time(&self, rev: &str) -> Result<i64> {
        self.repo.commit_time(rev)
    }
    fn destroy(&mut self) -> Result<()> {
        std::fs::remove_dir_all(&self.dir)
            .with_context(|| format!("removing {}", self.dir.display()))
    }
    fn recreate(&mut self) -> Result<()> {
        self.repo = Bare::open(&self.dir)?;
        Ok(())
    }
    fn mirror_to(&self, target_url: &str) -> Result<()> {
        // A local bare repo already holds every object, so mirror it directly.
        mirror_repo(&self.dir, target_url)
    }
}

// --------------------------------------------------------------- remote ----

/// Promisor-capability verdict, cached per RemoteBackend after the first
/// probe. Kept as an atomic so `Backend` methods stay `&self`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Promisor {
    Unknown = 0,
    Supported = 1,
    Unsupported = 2,
}

impl From<u8> for Promisor {
    fn from(v: u8) -> Self {
        match v {
            1 => Promisor::Supported,
            2 => Promisor::Unsupported,
            _ => Promisor::Unknown,
        }
    }
}

/// A git remote over the wire protocol, staged through a local mirror.
pub struct RemoteBackend {
    url: String,
    mirror: Bare,
    mirror_dir: PathBuf,
    /// Push throttle (DESIGN.md Section 9.5): min interval between pushes.
    push_interval: Duration,
    last_push: Mutex<Option<Instant>>,
    /// Cached promisor-capability verdict (0=unknown,1=yes,2=no).
    promisor: AtomicU8,
    /// Test/ops override: force-disable promisor to exercise the fallback.
    force_full_fetch: bool,
}

/// Max backoff attempts on HTTP 429 / secondary rate limit (DESIGN §9.5).
const MAX_RATE_LIMIT_RETRIES: u32 = 5;

impl RemoteBackend {
    /// `mirror_dir` holds the local staging/cache repo. `push_interval_ms`
    /// throttles pushes (0 = off; hosted defaults come from store config).
    pub fn open(url: &str, mirror_dir: &Path, push_interval_ms: u64) -> Result<Self> {
        if !(url.starts_with("file://") || url.starts_with("https://") || url.starts_with("ssh://"))
        {
            bail!("unsupported remote URL scheme: {url} (file://, https://, ssh:// only)");
        }
        // Force-full-fetch escape hatch: exercise the non-promisor read path
        // even where the server supports it (tests, or a debugging operator).
        let force_full_fetch = std::env::var("GITSTORAGE_FORCE_FULL_FETCH")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Ok(Self {
            url: url.to_string(),
            mirror: Bare::open(mirror_dir)?,
            mirror_dir: mirror_dir.to_path_buf(),
            push_interval: Duration::from_millis(push_interval_ms),
            last_push: Mutex::new(None),
            promisor: AtomicU8::new(Promisor::Unknown as u8),
            force_full_fetch,
        })
    }

    /// The remote URL this backend fronts.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Observable promisor verdict after at least one read (None = not yet
    /// probed).
    pub fn promisor_supported(&self) -> Option<bool> {
        match Promisor::from(self.promisor.load(Ordering::Relaxed)) {
            Promisor::Unknown => None,
            Promisor::Supported => Some(true),
            Promisor::Unsupported => Some(false),
        }
    }

    /// git subcommand against the MIRROR repo but talking to the remote URL,
    /// with credential isolation. HTTPS auth comes only from GITSTORAGE_TOKEN.
    fn net_git(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(self.mirror_dir.as_path());
        // Token auth (real hosts): a single extraHeader for this invocation
        // only. No credential helpers, ever.
        if self.url.starts_with("https://") {
            if let Ok(token) = std::env::var("GITSTORAGE_TOKEN") {
                cmd.arg("-c")
                    .arg(format!("http.extraHeader=Authorization: Bearer {token}"));
            }
        }
        // Belt-and-suspenders: explicitly disable any credential helper the
        // masked config might otherwise inherit.
        cmd.arg("-c").arg("credential.helper=");
        cmd.args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS");
        cmd.output()
            .with_context(|| format!("running git {args:?}"))
    }

    /// Run a network git op with exponential-backoff-plus-jitter retry on
    /// HTTP 429 / secondary-rate-limit responses (DESIGN §9.5). `is_ok`
    /// classifies the output; any non-429 failure is returned to the caller
    /// unretried.
    fn net_git_retrying(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut attempt = 0u32;
        loop {
            let out = self.net_git(args)?;
            if out.status.success() || attempt >= MAX_RATE_LIMIT_RETRIES {
                return Ok(out);
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !is_rate_limited(&stderr) {
                return Ok(out);
            }
            // Exponential backoff: base 500ms * 2^attempt, plus jitter.
            let base = 500u64 << attempt;
            let jitter = pseudo_jitter(attempt) % 250;
            std::thread::sleep(Duration::from_millis(base + jitter));
            attempt += 1;
        }
    }

    fn throttle(&self) {
        if self.push_interval.is_zero() {
            return;
        }
        let mut last = self.last_push.lock().expect("push throttle lock");
        if let Some(t) = *last {
            let elapsed = t.elapsed();
            if elapsed < self.push_interval {
                std::thread::sleep(self.push_interval - elapsed);
            }
        }
        *last = Some(Instant::now());
    }

    /// Fetch a ref (and ALL its objects) from the remote into the mirror — the
    /// full-segment-fetch path (fallback / non-promisor servers).
    fn fetch_ref_full(&self, refname: &str) -> Result<()> {
        let out = self.net_git_retrying(&[
            "fetch",
            "--quiet",
            &self.url,
            &format!("+{refname}:{refname}"),
        ])?;
        if !out.status.success() {
            bail!(
                "git fetch {refname} from {} failed: {}",
                self.url,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Blob-filtered fetch of a ref: brings trees + commits local but leaves
    /// blobs as promisor placeholders (DESIGN §10.3). Returns Err if the
    /// server rejects the filter (no partial-clone support).
    fn fetch_ref_filtered(&self, refname: &str) -> Result<()> {
        let out = self.net_git_retrying(&[
            "fetch",
            "--quiet",
            "--filter=blob:none",
            &self.url,
            &format!("+{refname}:{refname}"),
        ])?;
        if !out.status.success() {
            bail!(
                "filtered fetch of {refname} rejected: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Demand-fetch exactly one blob by OID from the promisor remote.
    fn fetch_blob_by_oid(&self, oid: &str) -> Result<()> {
        // `git fetch <url> <oid>` with allowReachableSHA1InWant-style servers;
        // modern git fetches a bare object id when the server permits it. For
        // promisor remotes this is what cat-file triggers implicitly, but we
        // do it explicitly so we control errors and backoff.
        let out =
            self.net_git_retrying(&["fetch", "--quiet", "--filter=blob:none", &self.url, oid])?;
        if !out.status.success() {
            bail!(
                "promisor blob fetch {oid} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Decide (once) whether this remote supports promisor/partial-clone by
    /// attempting a blob-filtered fetch of the target ref. Caches the verdict.
    /// Returns true if the filtered path is usable.
    fn ensure_promisor_probe(&self, refname: &str) -> bool {
        if self.force_full_fetch {
            self.promisor
                .store(Promisor::Unsupported as u8, Ordering::Relaxed);
            return false;
        }
        match Promisor::from(self.promisor.load(Ordering::Relaxed)) {
            Promisor::Supported => return true,
            Promisor::Unsupported => return false,
            Promisor::Unknown => {}
        }
        let ok = self.fetch_ref_filtered(refname).is_ok();
        self.promisor.store(
            if ok {
                Promisor::Supported as u8
            } else {
                Promisor::Unsupported as u8
            },
            Ordering::Relaxed,
        );
        ok
    }

    /// Ensure a whole ref's history/objects are present for log walking.
    fn ensure_ref_local(&self, refname: &str) -> Result<()> {
        if self.mirror.read_ref(refname)?.is_some() {
            return Ok(());
        }
        self.fetch_ref_full(refname)
    }

    /// Bring EVERY ref (and all objects) from the remote into the mirror — used
    /// before mirroring a remote-backed volume to a second backend, so the copy
    /// is complete even if reads had only promisor-fetched some blobs.
    fn fetch_all_refs(&self) -> Result<()> {
        let out =
            self.net_git_retrying(&["fetch", "--quiet", "--force", &self.url, "refs/*:refs/*"])?;
        if !out.status.success() {
            bail!(
                "git fetch refs/*:refs/* from {} failed: {}",
                self.url,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

impl Backend for RemoteBackend {
    // Object construction is local staging in the mirror; only ref
    // publication touches the network.
    fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        self.mirror.write_blob(bytes)
    }
    fn write_tree(&self, entries: &[(String, String)]) -> Result<String> {
        self.mirror.write_tree(entries)
    }
    fn commit_tree(&self, tree: &str, parents: &[String], message: &str) -> Result<String> {
        self.mirror.commit_tree(tree, parents, message)
    }

    /// Publish a segment ref: one push carries the whole sealed segment
    /// (DESIGN.md Section 3.2: segment = unit of write = one push).
    fn set_ref(&self, refname: &str, oid: &str) -> Result<()> {
        self.mirror.set_ref(refname, oid)?;
        self.throttle();
        let out =
            self.net_git_retrying(&["push", "--quiet", &self.url, &format!("{oid}:{refname}")])?;
        if !out.status.success() {
            bail!(
                "git push {refname} to {} failed: {}",
                self.url,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// P2 over the wire: push guarded by --force-with-lease=<ref>:<expected>.
    /// The receiving side atomically rejects if the remote ref is not exactly
    /// the expected value — CAS semantics end to end.
    fn cas_ref(&self, refname: &str, new: &str, old: Option<&str>) -> Result<bool> {
        self.throttle();
        let zero = "0000000000000000000000000000000000000000";
        let lease = format!("--force-with-lease={}:{}", refname, old.unwrap_or(zero));
        let out = self.net_git_retrying(&[
            "push",
            "--quiet",
            &lease,
            &self.url,
            &format!("{new}:{refname}"),
        ])?;
        if out.status.success() {
            self.mirror.set_ref(refname, new)?; // keep mirror's view current
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("stale info")
            || stderr.contains("[rejected]")
            || stderr.contains("failed to push")
            || stderr.contains("fetch first")
            || stderr.contains("non-fast-forward")
        {
            return Ok(false); // CAS lost — rebase and retry
        }
        bail!(
            "git push (CAS) to {} failed unexpectedly: {}",
            self.url,
            stderr.trim()
        );
    }

    fn delete_ref(&self, refname: &str) -> Result<()> {
        self.throttle();
        let out = self.net_git_retrying(&["push", "--quiet", &self.url, &format!(":{refname}")])?;
        if !out.status.success() {
            bail!(
                "git push (delete {refname}) failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let _ = self.mirror.delete_ref(refname);
        Ok(())
    }

    /// Authoritative ref read = ls-remote (cheap: refs only, no objects).
    fn read_ref(&self, refname: &str) -> Result<Option<String>> {
        let out = self.net_git_retrying(&["ls-remote", &self.url, refname])?;
        if !out.status.success() {
            bail!(
                "git ls-remote {} failed: {}",
                self.url,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8(out.stdout).context("ls-remote utf8")?;
        Ok(stdout
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().next())
            .map(|s| s.to_string()))
    }

    fn list_refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let out = self.net_git_retrying(&["ls-remote", &self.url, &format!("{prefix}*")])?;
        if !out.status.success() {
            bail!(
                "git ls-remote {} failed: {}",
                self.url,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8(out.stdout).context("ls-remote utf8")?;
        Ok(stdout
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let oid = it.next()?.to_string();
                let name = it.next()?.to_string();
                Some((name, oid))
            })
            .collect())
    }

    /// Read a chunk blob (DESIGN §10.3). Preferred path: blob-filtered fetch
    /// of the segment (trees local, blobs on demand), then resolve the fanout
    /// path → blob OID locally and demand-fetch just that blob. Fallback: full
    /// segment fetch. `rev` here is a `refs/segments/<id>` ref; `path` is the
    /// chunk fanout path.
    fn read_blob_at(&self, rev: &str, path: &str) -> Result<Vec<u8>> {
        // Fast path: already fully local (staged this process, or previously
        // full-fetched).
        if self.mirror.read_ref(rev)?.is_some() {
            if let Ok(bytes) = self.mirror.read_blob_at(rev, path) {
                return Ok(bytes);
            }
        }

        if rev.starts_with("refs/") && self.ensure_promisor_probe(rev) {
            // Promisor path: tree is local now; resolve chunk path → blob OID
            // without the network, then demand-fetch exactly that blob.
            if let Ok(oid) = self.mirror.tree_entry_oid(rev, path) {
                if !self.mirror.has_object(&oid) {
                    self.fetch_blob_by_oid(&oid)?;
                }
                return self.mirror.read_blob_by_oid(&oid);
            }
        }

        // Fallback: full segment fetch (non-promisor server, or resolution
        // failed). Idempotent; brings every blob local.
        if rev.starts_with("refs/") {
            self.fetch_ref_full(rev)?;
        }
        self.mirror.read_blob_at(rev, path)
    }

    fn rev_list(&self, tip: &str) -> Result<Vec<String>> {
        // The log tip's history must be local; fetch the log ref if the tip
        // commit is unknown to the mirror.
        if self.mirror.rev_list(tip).is_err() {
            self.fetch_ref_full("refs/heads/log")?;
        }
        self.mirror.rev_list(tip)
    }

    fn ls_tree_sizes(&self, rev: &str) -> Result<Vec<(String, u64)>> {
        if rev.starts_with("refs/") {
            self.ensure_ref_local(rev)?;
        }
        self.mirror.ls_tree_sizes(rev)
    }

    fn commit_time(&self, rev: &str) -> Result<i64> {
        if rev.starts_with("refs/") {
            self.ensure_ref_local(rev)?;
        }
        self.mirror.commit_time(rev)
    }

    fn destroy(&mut self) -> Result<()> {
        if let Some(dir) = self.url.strip_prefix("file://") {
            std::fs::remove_dir_all(dir).with_context(|| format!("removing {dir}"))?;
            std::fs::remove_dir_all(&self.mirror_dir)
                .with_context(|| format!("removing {}", self.mirror_dir.display()))?;
            return Ok(());
        }
        // Real hosts: repository deletion is a control-plane/operator action
        // (and deliberately NOT automated against hosted providers).
        bail!(
            "refusing to destroy remote repository {} — delete it via the \
             host's interface, then re-run",
            self.url
        );
    }

    fn recreate(&mut self) -> Result<()> {
        if let Some(dir) = self.url.strip_prefix("file://") {
            Bare::open(Path::new(dir))?; // re-init empty bare repo in the slot
            self.mirror = Bare::open(&self.mirror_dir)?;
            return Ok(());
        }
        bail!(
            "refusing to create remote repository {} — create it via the \
             host's interface first",
            self.url
        );
    }

    fn mirror_to(&self, target_url: &str) -> Result<()> {
        // Complete the local mirror first (reads may have only promisor-fetched
        // some blobs), then push the whole thing to the independent target.
        self.fetch_all_refs()?;
        mirror_repo(&self.mirror_dir, target_url)
    }

    fn read_path_note(&self) -> Option<String> {
        Some(match self.promisor_supported() {
            Some(true) => "promisor blob-by-OID fetch".to_string(),
            Some(false) => "full segment fetch (promisor unsupported/forced)".to_string(),
            None => "no read served yet".to_string(),
        })
    }
}

/// Mirror EVERY ref (and all reachable objects) of a local bare repo to an
/// independent target git URL — the whole-store mirror primitive (DESIGN.md
/// Section 14.3). Push-only and idempotent: git sends only objects the target
/// lacks, so re-mirroring is cheap. `refs/*:refs/*` with `--force` makes the
/// target converge on the source (the source is authoritative); it never
/// deletes target refs, so a stale segment ref on the mirror is at worst
/// harmless garbage (readers follow the log ref, which IS force-updated).
///
/// Credential isolation is identical to `RemoteBackend::net_git`: terminal
/// prompts off, no credential helper, HTTPS auth ONLY from `GITSTORAGE_TOKEN`
/// as a one-shot header. This is the single mirror entry point and it lives in
/// the backend layer, honoring the "network git only in src/backend.rs" rule.
pub fn mirror_repo(source_git_dir: &Path, target_url: &str) -> Result<()> {
    if !(target_url.starts_with("file://")
        || target_url.starts_with("https://")
        || target_url.starts_with("ssh://"))
    {
        bail!("unsupported mirror target scheme: {target_url} (file://, https://, ssh:// only)");
    }
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(source_git_dir);
    if target_url.starts_with("https://") {
        if let Ok(token) = std::env::var("GITSTORAGE_TOKEN") {
            cmd.arg("-c")
                .arg(format!("http.extraHeader=Authorization: Bearer {token}"));
        }
    }
    cmd.arg("-c").arg("credential.helper=");
    cmd.args(["push", "--quiet", "--force", target_url, "refs/*:refs/*"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS");
    let out = cmd
        .output()
        .with_context(|| format!("git push mirror to {target_url}"))?;
    if !out.status.success() {
        bail!(
            "mirror push to {target_url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Classify a git stderr as an HTTP 429 / secondary-rate-limit signal.
fn is_rate_limited(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("429")
        || s.contains("too many requests")
        || s.contains("secondary rate limit")
        || s.contains("rate limit")
}

/// Cheap deterministic jitter without pulling `rand` into the hot path.
fn pseudo_jitter(seed: u32) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos ^ seed.wrapping_mul(2654435761)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_classifier() {
        assert!(is_rate_limited("error: 429 Too Many Requests"));
        assert!(is_rate_limited("You have exceeded a secondary rate limit"));
        assert!(!is_rate_limited("fatal: repository not found"));
    }

    #[test]
    fn rejects_bad_scheme() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = RemoteBackend::open("http://nope", &tmp.path().join("m.git"), 0);
        assert!(err.is_err());
    }
}
