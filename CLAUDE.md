# git-storage — agent instructions

## What this is
A log-structured, content-addressed object store over git hosting backends.
**DESIGN.md is the authoritative spec** (target architecture);
**IMPLEMENTATION-PLAN.md is the milestone ladder** (what gets built when).
README.md carries the project's policy commitments — never contradict them
(no unlimited storage, no auto repo-fleet expansion, no circumvention features).

## Status
- M0 complete: walking-skeleton CLI (put/get/ls, BLAKE3 content addressing,
  local git store repos). Spec cleanup + review fixes landed.
- M1 complete: FastCDC content-defined chunking; store config pins params.
- M2 complete: zstd + XChaCha20-Poly1305, keyed convergent encryption
  (dedup survives sealing), key-derived gear seed, keyed name tags, master
  keyfile (--keyfile, auto-created only for new stores). Store repos are
  fully opaque: no plaintext, no filenames, generic commit messages.
- Next: M3 (segments + CAS transaction log — the big one). See IMPLEMENTATION-PLAN.md.

## Git safety rules (hard requirements)
- The CLI performs LOCAL git ops only (init/add/diff/commit). Never add a
  network git command (push/fetch/clone/pull) outside the M4 backend layer.
- Every git invocation goes through store.rs::git_command(): terminal prompts
  disabled, fixed tool identity, gpgsign off. Keep it that way.
- Tests must NEVER touch the user's git config or credentials: spawn the CLI
  only via the isolated-env helper in tests/roundtrip.rs (HOME=tempdir,
  GIT_CONFIG_GLOBAL/SYSTEM=/dev/null, GIT_TERMINAL_PROMPT=0).

## Layout
- `src/` — Rust CLI: main.rs (clap), chunker.rs (FastCDC), crypto.rs (keys +
  AEAD), store.rs (objects/manifests/git), manifest.rs
- `tests/roundtrip.rs` — integration tests (roundtrip, dedup-through-encryption,
  corruption, opacity, wrong-key)
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
