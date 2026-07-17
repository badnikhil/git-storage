# git-storage — agent instructions

## What this is
A log-structured, content-addressed object store over git hosting backends.
**DESIGN.md is the authoritative spec** (target architecture);
**IMPLEMENTATION-PLAN.md is the milestone ladder** (what gets built when).
README.md carries the project's policy commitments — never contradict them
(no unlimited storage, no auto repo-fleet expansion, no circumvention features).

## Testing philosophy (HARD RULE — see AGENTS.md)
**Every known issue MUST have a test that fails on it.** A green suite with open
issues means the suite is inadequate. For each open issue (bug or documented
limitation), write a test asserting the DESIRED behavior and mark it
`#[ignore = "issue #N: ..."]` — so `cargo test` stays green for what works and
`cargo test -- --ignored` runs the known-failing tests that pin every issue.
Fixing an issue means deleting its `#[ignore]` (the test becomes the acceptance
criterion). Keep a passing test for any safe-degradation behavior AND the ignored
test for the correct behavior. Every issue links to its test; every ignored test
names its issue. Spell out "Section N" in text — never the section-symbol char.

## Status
- M0–M2 complete: FastCDC chunking, keyed convergent encryption (dedup
  survives sealing), fully opaque store repos.
- M3 complete: THE ENGINE. Sealed segments (refs/segments/<id> in bare volume
  repos), encrypted CAS transaction log (refs/heads/log in index.git),
  two-phase commit, checkpoints, snapshot reads (--at), crash matrix C1–C4
  tested via GITSTORAGE_CRASH injection. Crate = lib + thin bin.
- M4 complete (file:// + unit; live host verdicts pending): Backend trait
  (src/backend.rs) = the two DESIGN Section 2.1 primitives + read/lifecycle. Two impls:
  LocalBackend (wraps Bare) and RemoteBackend (git wire over file:///https://
  /ssh://, mirror-then-push, --force-with-lease CAS, promisor blob-by-OID read
  + probe + full-fetch fallback, per-repo push throttle + 429 backoff, token
  ONLY from GITSTORAGE_TOKEN as a header). Engine is Box<dyn Backend>, config
  declares a fixed volume set (volumes[] + index_url), volume selection Section 9.3 +
  budget wall Section 15.3 + spare slot at N≥3. Init-time provisioning
  (src/backend/provision.rs, CONTROL PLANE ONLY, GitHub/Gitea, idempotent-safe).
  M3 stores keep working with zero migration (absent volumes[] → local v0).
  Whole M3 correctness surface (crash C1–C4, snapshots, CAS race) reruns green
  against RemoteBackend over file://. See agent-docs/milestone-4.md.
  PENDING: live Gitea promisor verdict (DESIGN Open Problem 3) + live GitHub run
  — both env-gated tests, skip cleanly (GITSTORAGE_GITEA_URL / GITSTORAGE_TOKEN).
- M5 complete: THE LIFECYCLE. `rm` (logical delete) + `stats` (per-volume
  live/dead/util). Byte accounting now comes wholly from the log — each txn
  carries SegRec{vol,seg,bytes}, each ChunkRef carries clen — so stats/selection/
  budget need ZERO segment fetches (fixes the M4 read-amp residual). Hysteresis-
  gated compaction (Section 12.4: dead>50% AND util≥80% AND ≥24h; all env-tunable,
  `compact --force` bypasses pressure+interval) with delete-only-after-CAS
  (Section 12.3): content-derived rewrite segment id = idempotent crash redo; Compact
  txn is the commit point; source repo destroyed ONLY after the CAS. Concurrency
  guard: repoint rebuilt from current namespace each attempt, aborts if a racing
  put left un-rewritten data on the retiring volume (no data loss). Spare slot is
  the compaction dest (Section 15.5); slot reuse, never fleet growth. Orphan sweep with
  safety window (Section 12.5, default 1h). Compaction crash matrix + churn guard +
  budget wall + volume selection tested in tests/compaction.rs (11 tests, incl.
  crash injection over file:// RemoteBackend). 61 tests total. See
  agent-docs/milestone-5.md.
- M6 complete: HARDENING + POLISH. Whole-store mirror to an INDEPENDENT backend
  (Engine::mirror, CLI `mirror`; backend.rs::mirror_repo does the isolated
  refs/*:refs/* push, volumes-before-index ordering; ciphertext-only; idempotent;
  tests/mirror.rs opens a fresh store on the mirror alone and reads byte-identical).
  Reproducible benchmark harness (examples/bench.rs, one command: `cargo run
  --release --example bench` — put/get throughput, edit-dedup, chunk-size sweep;
  numbers in agent-docs/milestone-6.md). Fuzz/robustness suite (tests/fuzz.rs, 6
  targets: open_chunk/open_manifest/manifest-JSON never panic on garbage, seal↔open
  roundtrip, single-bit-flip rejection, chunker reassembly on degenerate inputs).
  gitoxide DEFERRED with recorded evidence (put ~30 MB/s is git-CLI-spawn bound;
  stderr-scraping is fragile — but migration is large/high-risk, prime post-v1
  target). README gained a user guide + threat-model section. 69 tests total.
  See agent-docs/milestone-6.md.
- v1 core COMPLETE (M0–M6). Remaining before a real v1 ship: live-host
  validation (Gitea promisor verdict + GitHub smoke, both env-gated) and the
  deferred items (gitoxide, log compaction / Open Problem 6, RS erasure coding).

## Store layout (M4)
config.json (declares volumes[] + index_url; both optional — absent = M3 local
mode) · index.git (bare; the log) · volumes/v0.git … (local bare mirrors of
each volume; remotes reached via the URL in config)

## M4 safety invariants (hard requirements)
- Network git lives ONLY in src/backend.rs — RemoteBackend AND the M6
  `mirror_repo` primitive (the whole-store mirror push). Same isolation as
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
  promisor probe/fallback, env-gated Gitea/GitHub live suites;
  `tests/compaction.rs` — M5: rm, compaction correctness + crash matrix (over
  file://), churn guard (min-interval), orphan-sweep window, budget wall,
  volume selection / spare exclusion;
  `tests/mirror.rs` — M6: whole-store mirror is a complete independent replica +
  incremental re-mirror; `tests/fuzz.rs` — M6: bounded seeded fuzz of the
  untrusted-input parsers (never-panic + integrity); `examples/bench.rs` — M6:
  one-command benchmark harness;
  `tests/cli.rs` — CLI surface + error paths (edge-size files, missing/bad
  inputs, init/keyfile validation, tip/stats, resurrection);
  `tests/property.rs` — boundary-size roundtrips, deterministic dedup,
  many-files+checkpoint stress. Backend primitives have unit tests in
  src/backend.rs (cas_ref/read_ref/list_refs/scheme rejection).
- Test suite: 97 tests (lib units + roundtrip/engine/backend/compaction/mirror/
  fuzz/cli/property). Two known limitations are documented as DESIGN Open
  Problems 7 (compaction not snapshot-aware — old snapshots fail LOUDLY after
  compaction) and 8 (concurrent write during same-volume compaction is outside
  the Section 13.1 single-writer model; M5's guard covers committed data, not in-flight).
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
