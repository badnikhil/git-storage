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
- Next: M4 (backend trait + remote backends) / M5 (compaction + budget).
  See IMPLEMENTATION-PLAN.md.

## Store layout (M3)
config.json · index.git (bare; the log) · volumes/v0.git (bare; segments)

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
  snapshots, concurrency, checkpoints
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
