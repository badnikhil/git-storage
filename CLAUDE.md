# git-storage — agent instructions

## What this is
A log-structured, content-addressed object store over git hosting backends.
**DESIGN.md is the authoritative spec** (target architecture);
**IMPLEMENTATION-PLAN.md is the milestone ladder** (what gets built when).
README.md carries the project's policy commitments — never contradict them
(no unlimited storage, no auto repo-fleet expansion, no circumvention features).

## Status
- M0–M2 complete: FastCDC chunking, keyed convergent encryption (dedup
  survives sealing), fully opaque store repos.
- M3 complete: THE ENGINE. Sealed segments (refs/segments/<id> in bare volume
  repos), encrypted CAS transaction log (refs/heads/log in index.git),
  two-phase commit, checkpoints, snapshot reads (--at), crash matrix C1–C4
  tested via GITSTORAGE_CRASH injection. Crate = lib + thin bin.
- M4 complete (file:// + unit; live host verdicts pending): Backend trait
  (src/backend.rs) = the two DESIGN §2.1 primitives + read/lifecycle. Two impls:
  LocalBackend (wraps Bare) and RemoteBackend (git wire over file:///https://
  /ssh://, mirror-then-push, --force-with-lease CAS, promisor blob-by-OID read
  + probe + full-fetch fallback, per-repo push throttle + 429 backoff, token
  ONLY from GITSTORAGE_TOKEN as a header). Engine is Box<dyn Backend>, config
  declares a fixed volume set (volumes[] + index_url), volume selection §9.3 +
  budget wall §15.3 + spare slot at N≥3. Init-time provisioning
  (src/backend/provision.rs, CONTROL PLANE ONLY, GitHub/Gitea, idempotent-safe).
  M3 stores keep working with zero migration (absent volumes[] → local v0).
  Whole M3 correctness surface (crash C1–C4, snapshots, CAS race) reruns green
  against RemoteBackend over file://. See agent-docs/milestone-4.md.
  PENDING: live Gitea promisor verdict (DESIGN Open Problem 3) + live GitHub run
  — both env-gated tests, skip cleanly (GITSTORAGE_GITEA_URL / GITSTORAGE_TOKEN).
- Next: M5 (compaction + budget). See IMPLEMENTATION-PLAN.md.

## Store layout (M4)
config.json (declares volumes[] + index_url; both optional — absent = M3 local
mode) · index.git (bare; the log) · volumes/v0.git … (local bare mirrors of
each volume; remotes reached via the URL in config)

## M4 safety invariants (hard requirements)
- Network git lives ONLY in RemoteBackend (src/backend.rs). Same isolation as
  Bare: GIT_TERMINAL_PROMPT=0, `-c credential.helper=` (disabled), no askpass,
  token via `-c http.extraHeader` for https:// ONLY, sourced only from
  GITSTORAGE_TOKEN — never on-disk creds/helpers.
- The data-plane adapter has NO repo-creation capability. Repo creation is
  init-only, control-plane-only (provision.rs). Never let it into RemoteBackend.
- Budget wall: never overflow a volume, never create repos to make room — a
  write with no accepting volume is REFUSED ("budget exhausted").

## Git safety rules (hard requirements)
- The CLI performs LOCAL git ops only (init/add/diff/commit). Never add a
  network git command (push/fetch/clone/pull) outside the M4 backend layer.
- Every git invocation goes through gitrepo.rs::Bare::base_command(): terminal
  prompts disabled, user config masked, fixed tool identity. Keep it that way.
- Tests must NEVER touch the user's git config or credentials: spawn the CLI
  only via the isolated-env helper in tests/roundtrip.rs (HOME=tempdir,
  GIT_CONFIG_GLOBAL/SYSTEM=/dev/null, GIT_TERMINAL_PROMPT=0).

## Layout
- `src/` — lib (lib.rs): chunker (FastCDC), crypto (keys+AEAD), gitrepo (bare
  plumbing + CAS), engine (segments + tx log), manifest; main.rs = thin CLI
- `tests/roundtrip.rs` — CLI tests; `tests/engine.rs` — crash matrix,
  snapshots, concurrency, checkpoints (each parameterized local + remote/file://);
  `tests/backend.rs` — M4: rate governor, budget wall, volume selection,
  promisor probe/fallback, env-gated Gitea/GitHub live suites
- `agent-docs/` — **local-only shared agent knowledge base. NEVER commit, stage,
  or push anything in it.** Read it before starting work; keep it updated as you
  work (decisions + why, findings, changes).

## Build / test
```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings   # must be clean
cargo fmt --check                           # must be clean
```

## Conventions
- Language: Rust (decided — see DESIGN.md Appendix B). Milestone-0 shells out
  to the `git` CLI; gitoxide comes later per the plan.
- Every milestone: tests + clippy + fmt green before it counts as done.
- Deviations from DESIGN.md get recorded in the spec (delta or open problem).
- Never push to any remote without explicit user confirmation.
