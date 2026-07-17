//! Control-plane repo provisioning (DESIGN.md Section 16).
//!
//! This is the ONLY place that creates repositories, and it does so ONLY at
//! `init` time for the operator-declared fixed volume set. It is deliberately
//! separated from the data-plane [`Backend`](super::Backend): the data plane
//! has NO repo-creation capability whatsoever, so a full volume set can never
//! trigger fleet growth (README policy: no auto repo-fleet expansion).
//!
//! Transport: we shell out to `curl` for the single REST call per repo rather
//! than pull a full async HTTP + TLS stack into the crate. Rationale: control
//! calls are rare and operator-driven (DESIGN Section 16 "REST sparingly"), and the
//! token stays off-disk — it is read from GITSTORAGE_TOKEN and passed as a
//! `-H "Authorization: ..."` header for that one invocation, never written to
//! a credential store or git config.
//!
//! Idempotent-safe: if the target repo already exists AND is empty (no refs),
//! we adopt it. If it exists and is NON-EMPTY in a way that is not ours, we
//! FAIL LOUDLY rather than clobber or silently adopt foreign data.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Which hosted control-plane API shape to speak. GitHub and Gitea/Forgejo
/// share the `POST /user/repos {name, private}` and `GET /repos/{o}/{r}`
/// shapes closely enough to share one adapter with a different API base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    GitHub,
    Gitea,
}

impl Host {
    /// REST API base for the host (no trailing slash).
    fn api_base(&self, web_base: &str) -> String {
        match self {
            // github.com → api.github.com; GHES → <host>/api/v3 (not handled
            // here — v1 targets github.com + self-hosted Gitea).
            Host::GitHub => "https://api.github.com".to_string(),
            // Gitea/Forgejo: <web-base>/api/v1
            Host::Gitea => format!("{}/api/v1", web_base.trim_end_matches('/')),
        }
    }
}

/// A declared volume repo to provision: owner + name, plus the web base URL
/// (only used by Gitea to derive its API base).
pub struct RepoSpec {
    pub host: Host,
    pub owner: String,
    pub name: String,
    /// Web base, e.g. "https://gitea.example.com" (Gitea) — ignored for GitHub.
    pub web_base: String,
}

/// Result of provisioning one repo.
#[derive(Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// We created it fresh.
    Created,
    /// It already existed and was empty (adopted as our slot).
    AdoptedEmpty,
}

/// Provision the declared repo via the control-plane REST API. Requires the
/// GITSTORAGE_TOKEN env var (auth). Idempotent-safe (see module docs).
pub fn ensure_repo(spec: &RepoSpec) -> Result<Provisioned> {
    let token = std::env::var("GITSTORAGE_TOKEN").context(
        "GITSTORAGE_TOKEN is required to provision remote repositories \
         (control plane, init only)",
    )?;
    let api = spec.host.api_base(&spec.web_base);

    // 1. Does it already exist?
    match get_repo(&api, spec, &token)? {
        RepoState::Absent => {}
        RepoState::Empty => return Ok(Provisioned::AdoptedEmpty),
        RepoState::NonEmpty => bail!(
            "repository {}/{} already exists and is NOT empty — refusing to \
             adopt foreign data. Point the volume at a fresh repo or delete it \
             via the host's interface.",
            spec.owner,
            spec.name
        ),
    }

    // 2. Create it (private).
    create_repo(&api, spec, &token)?;
    Ok(Provisioned::Created)
}

enum RepoState {
    Absent,
    Empty,
    NonEmpty,
}

/// Auth header value for the given host.
fn auth_header(host: Host, token: &str) -> String {
    match host {
        // GitHub accepts `Authorization: Bearer <token>` and `token <token>`.
        Host::GitHub => format!("Authorization: Bearer {token}"),
        // Gitea accepts `Authorization: token <token>`.
        Host::Gitea => format!("Authorization: token {token}"),
    }
}

fn get_repo(api: &str, spec: &RepoSpec, token: &str) -> Result<RepoState> {
    let url = format!("{api}/repos/{}/{}", spec.owner, spec.name);
    let out = curl_json(spec.host, token, "GET", &url, None)?;
    match out.http_status {
        404 => Ok(RepoState::Absent),
        200 => {
            // Distinguish empty vs non-empty: a fresh repo has no default
            // branch / no commits. GitHub returns "size":0 for empty; Gitea
            // returns "empty":true. Be conservative: treat clearly-empty as
            // Empty, everything else as NonEmpty.
            let body = &out.body;
            let looks_empty = body.contains("\"empty\":true")
                || body.contains("\"size\":0")
                || body.contains("\"size\": 0");
            if looks_empty {
                Ok(RepoState::Empty)
            } else {
                Ok(RepoState::NonEmpty)
            }
        }
        s => bail!(
            "unexpected HTTP {s} probing {}/{}: {}",
            spec.owner,
            spec.name,
            out.body.trim()
        ),
    }
}

fn create_repo(api: &str, spec: &RepoSpec, token: &str) -> Result<()> {
    // Both APIs: POST /user/repos {name, private:true}. (Org repos would use
    // POST /orgs/{org}/repos — out of scope for v1's user-owned test fleet.)
    let url = format!("{api}/user/repos");
    let payload = format!(r#"{{"name":"{}","private":true}}"#, spec.name);
    let out = curl_json(spec.host, token, "POST", &url, Some(&payload))?;
    match out.http_status {
        200 | 201 => Ok(()),
        s => bail!(
            "creating {}/{} failed (HTTP {s}): {}",
            spec.owner,
            spec.name,
            out.body.trim()
        ),
    }
}

struct CurlOut {
    http_status: u16,
    body: String,
}

/// One REST call via curl with credential isolation. The token is passed as a
/// header for this invocation only; it never lands on disk. `-sS` = quiet but
/// show errors; we append the HTTP status via `-w`.
fn curl_json(
    host: Host,
    token: &str,
    method: &str,
    url: &str,
    body: Option<&str>,
) -> Result<CurlOut> {
    const MARKER: &str = "\n__HTTP_STATUS__:";
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-X", method])
        .arg("-H")
        .arg(auth_header(host, token))
        .arg("-H")
        .arg("Accept: application/json")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-w")
        .arg(format!("{MARKER}%{{http_code}}"));
    if let Some(b) = body {
        cmd.arg("-d").arg(b);
    }
    cmd.arg(url);
    let out = cmd
        .output()
        .context("running curl for control-plane call")?;
    if !out.status.success() {
        bail!(
            "curl transport error for {method} {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let combined = String::from_utf8_lossy(&out.stdout);
    let (body, status) = match combined.rsplit_once(MARKER) {
        Some((b, s)) => (b.to_string(), s.trim().parse::<u16>().unwrap_or(0)),
        None => (combined.to_string(), 0),
    };
    Ok(CurlOut {
        http_status: status,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_shapes() {
        assert_eq!(Host::GitHub.api_base("ignored"), "https://api.github.com");
        assert_eq!(
            Host::Gitea.api_base("https://gitea.example.com/"),
            "https://gitea.example.com/api/v1"
        );
    }

    #[test]
    fn auth_header_shapes() {
        assert_eq!(auth_header(Host::GitHub, "T"), "Authorization: Bearer T");
        assert_eq!(auth_header(Host::Gitea, "T"), "Authorization: token T");
    }
}
