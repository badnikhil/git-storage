# git-storage — agent instructions

## What this is
A log-structured, content-addressed object store over git hosting backends.
**DESIGN.md is the authoritative spec** (target architecture);
**IMPLEMENTATION-PLAN.md is the milestone ladder** (what gets built when).
README.md carries the project's policy commitments — never contradict them
(no unlimited storage, no auto repo-fleet expansion, no circumvention features).

## Status
- M0 complete: walking-skeleton CLI (fixed-size chunks → BLAKE3 objects →
  JSON manifests → local git repo). Spec cleanup + review-round-1 fixes landed.
- Next: M1 (FastCDC content-defined chunking). See IMPLEMENTATION-PLAN.md.

## Layout
- `src/` — Rust CLI: main.rs (clap dispatch), chunker.rs, store.rs, manifest.rs
- `tests/roundtrip.rs` — integration tests (roundtrip, dedup, corruption)
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
