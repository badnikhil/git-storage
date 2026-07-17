# git-storage — Implementation Plan

> Companion to [DESIGN.md](DESIGN.md) (the target architecture) and
> [README.md](README.md) (the commitments). This plan is the ladder between
> "break a file into pieces and commit it" and the full design. One naive piece
> is replaced per milestone; the tool works at every rung.
>
> Language: **Rust** (decided). Early milestones shell out to the `git` CLI;
> gitoxide arrives when the git interaction gets deep enough to earn it (M3/M6).

## Method

Walking skeleton. M0 is deliberately dumb: fixed-size chunks, JSON manifests,
local repo, `git` CLI. Every later milestone swaps exactly one naive component
for its DESIGN.md counterpart, keeping `put`/`get`/`ls` working end-to-end the
whole way. No milestone starts until the previous one's exit criteria pass
(exceptions listed in Sequencing).

Current state (2026-07-16): two earlier agent runs were interrupted, leaving
partial artifacts that M0 and M0.5 absorb:

- `Cargo.toml`, `.gitignore`, and a partial `src/chunker.rs` exist → reconciled/finished in M0.
- `DESIGN.md` still contains ~138 `Section ` characters from an interrupted notation
  cleanup, and its Appendix B may still say the language decision is deferred →
  M0 (spec half).
- The project root is **not yet a git repository** → step 0 below.

## Step 0 — repo init

**What this adds:** version control for the project itself — nothing else.

The project root is a git repository with `origin` =
https://github.com/badnikhil/git-storage. The initial commit lands together
with M0 completion (docs + working skeleton as one coherent starting point).
`agent-docs/` stays untracked (gitignored) — it is the local-only agent
knowledge base, never committed.

**Nothing is ever pushed without explicit user confirmation.** Size: S.

---

## M0 — Walking skeleton + spec hygiene (finish interrupted work)

*(Formerly M0 and M0.5 — merged into one milestone with a code half and a
spec half.)*

**Goal:** `put` a file into a local git repo as fixed-size, content-addressed
chunks and `get` it back bit-identical — and bring DESIGN.md fully up to date
with every decision and review finding so far.

**What this adds:** the first working tool. Three commands appear — `put`,
`get`, `ls` — plus the store repo format they operate on: a local git repo
holding `objects/<aa>/<hash>` chunk files and one JSON manifest per stored
file. Dedup and integrity verification exist from day one (both fall out of
content addressing). Everything is plaintext and local; chunk boundaries are
dumb fixed-size splits.

**In scope**
- Reconcile/finish the partial scaffold (`Cargo.toml`, `src/chunker.rs`).
- Fixed-size chunking, default 1 MiB, `--chunk-size` with k/m suffixes
  (bounds: 1 KiB – 90 MiB, staying under git hosts' 100 MiB blob block).
- BLAKE3 content addressing: `objects/<aa>/<full-hash>`; write-if-absent = dedup.
- One JSON manifest per file: name, size, chunk size, whole-file hash, ordered
  chunk list `{hash, len}`.
- `put` / `get` / `ls` subcommands (clap); store repo auto-`git init`ed;
  `git add -A && git commit` per put.
- Streaming I/O both directions (never load the whole file); verify per-chunk
  hash and whole-file hash on `get`.
- Deps: clap, blake3, serde/serde_json, anyhow. No async.

**Explicitly NOT in scope:** CDC, compression, encryption, segments,
transaction log, remotes, gitoxide.

**Deliverables:** working CLI; integration tests (`tests/roundtrip.rs`).

**Exit criteria**
1. Roundtrip test: ~3 MiB varied-content file, 256k chunks, byte-identical after get.
2. Dedup test: second `put` of the same file → 0 new objects, no error.
3. Corruption test: flip one byte in an object file → `get` fails loudly with hash mismatch.
4. `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` all pass.

**DESIGN.md sections (code half):** foreshadows 3.1 (chunk), 6.6 (content
addressing), 7 (dedup) in naive form.

### Spec-hygiene half (no code — runs in parallel with the code half)

After this half lands, DESIGN.md is clean of `Section ` notation, records Rust as the
decided language, and contains the four review-round1 fixes (spare compaction
slot, log-growth open problem, chunk-ID→OID lookup step, checkpoint sharding
note). Later milestones implement against the amended spec.

**In scope**
- Finish `Section ` → "Section N" cleanup (all ~138 occurrences; keep sentences grammatical).
- Verify/complete the Appendix B edit: Rust is **decided**, not deferred.
- Apply the four design-review-round1 findings (agent-docs/design-review-round1.md):
  1. Spare compaction slot: reserve one volume slot as compaction headroom
     (fixes the compact-under-pressure chicken-and-egg; Sections 12/15).
  2. Log-repo growth + forced-update wrinkle → new open problem 6 (pruning old
     log history is a non-fast-forward update, which violates the CAS model).
  3. Spell out chunk-ID → git-OID resolution in the read path (segment tree
     path lookup in the blobless clone; Section 10.3).
  4. Checkpoint size: note sharded checkpoints as the escape hatch (Section 8.5).

**Explicitly NOT in scope (spec half):** any new design work beyond the four
findings.

**Exit criteria (spec half):** zero `Section ` in DESIGN.md; Appendix B says Rust
decided; all four findings visible in the spec; review-round1 doc marked
resolved.

**Combined size: M.**

---

## M1 — Content-defined chunking

**Goal:** replace fixed-size splitting with FastCDC so edits dedup.

**What this adds:** smart chunk boundaries. The chunker becomes
content-defined (FastCDC), so inserting bytes mid-file no longer shifts every
subsequent chunk — edited files now dedup against their previous versions,
which is the property that makes versioned storage cheap. Also adds the
per-store randomized gear seed (privacy defense) and a store config file
pinning chunker parameters for the store's lifetime. The CLI surface doesn't
change.

**In scope**
- FastCDC with min/avg/max = 512 KiB / 1 MiB / 4 MiB (DESIGN Section 5.2).
- Randomized gear table derived from a per-store seed (the fingerprinting
  side-channel defense, Section 5.4). Seed lives in store config for now;
  becomes key-derived in M2.
- Store config file recording chunker params (a store must chunk consistently
  for its lifetime).
- Dedup-ratio measurement on an edited-file workload.

**Explicitly NOT in scope:** encryption/compression; manifest format changes
beyond recording chunker params.

**Exit criteria**
1. Insert-in-middle test: store file, insert bytes mid-file, re-put → most
   chunks dedup (fixed-size would dedup none after the edit point). Assert a
   concrete ratio (e.g. ≥ 80% of bytes deduped for a 16 MiB file with a 1 KiB
   mid-file insert).
2. Two stores with different seeds produce different chunk boundaries for the
   same file.
3. All M0 tests still pass.

**DESIGN.md sections:** 5. **Size: M.**

---

## M2 — Compress + encrypt

**Goal:** chunks leave the chunker as zstd-compressed, AEAD-sealed ciphertext;
dedup still works.

**What this adds:** privacy and smaller storage. From this milestone on, the
store repo contains only ciphertext — chunks are zstd-compressed then sealed
with XChaCha20-Poly1305, manifests are encrypted, and a master keyfile appears
(lose it, lose the store). The clever part being added is keyed convergent
encryption with deterministic nonces: encryption that still dedups identical
plaintext within a store, while outsiders can't test guessed files against it.
Chunk IDs switch from plaintext hashes to ciphertext hashes.

**In scope**
- Order: compress (zstd level 3) then encrypt (DESIGN Section 6.1).
- Key hierarchy (Section 6.2): master key in a keyfile (0600 perms) for now;
  chunker gear seed becomes HKDF-derived from master key (replacing M1's
  config-file seed).
- Keyed convergent chunk keys (Section 6.3) + deterministic nonces
  (Section 6.5); XChaCha20-Poly1305; AD binds chunk ID + format version.
- Chunk ID becomes BLAKE3 of **ciphertext** (Section 6.6).
- Manifests encrypted with the manifest key (Section 8.7's key, applied to the
  JSON manifests for now).

**Explicitly NOT in scope:** key rotation; keychain/agent integration; any
cross-store dedup (never happens, by design — Section 7).

**Exit criteria**
1. Determinism test: same plaintext, same store → identical ciphertext and
   chunk ID (dedup preserved through encryption).
2. Different-store test: same plaintext, different master keys → different
   ciphertext/IDs.
3. Backend-opacity check: nothing plaintext-derived is readable in the store
   repo (spot-check manifests and objects are ciphertext).
4. Tamper test: modify ciphertext → AEAD open fails loudly.
5. All prior tests still pass (roundtrip now through
   decrypt→decompress→verify).

**DESIGN.md sections:** 6, 7. **Size: M.**

---

## M3 — Segments + manifest transaction log

**Goal:** replace loose objects + JSON files with sealed segments and the
CAS-serialized transaction log — the core of the design.

**What this adds:** the actual storage engine. Loose chunk files are replaced
by sealed, immutable segments (batched chunk trees under `refs/segments/<id>`),
and the per-file JSON manifests are replaced by a single transaction log on
one CAS-guarded ref in a dedicated index repo — every write becomes an atomic
transaction with a crash-safe two-phase commit. This buys: crash consistency
(provable via the C1–C4 test matrix), safe concurrent writers (no lost
updates), point-in-time snapshot reads by pinning a log commit, and
checkpoints so readers don't replay history. After M3 the system is
architecturally the DESIGN.md system, just local-only.

**In scope**
- Sealed segment trees under `refs/segments/<id>` with 2-hex fanout
  (Sections 3.2, 4.2, 4.3); staging + seal triggers (size / explicit sync).
- Dedicated index repo; single `refs/heads/log` transaction log; transaction
  payload per Section 8.2 (encrypted).
- Two-phase commit: data pushed first, log-ref CAS as the commit point
  (Section 8.3); rebase-and-retry on CAS rejection (Section 8.4).
- Checkpoints every 128 transactions (Section 8.5); reader = checkpoint + tail
  (Section 8.6).
- Backend = **local bare repos** (volume repos + index repo), pushed to over
  the file protocol — real push/CAS semantics, no network. This is where
  git-CLI-based CAS gets probed (see Risks).
- Crash-matrix tests C1–C4 (Section 11): inject a kill/abort at each phase
  boundary, restart, verify invariants INV-1/2/3 (no dangling manifest refs;
  orphans invisible; log tip is old-or-new, never partial).

**Explicitly NOT in scope:** network backends; compaction (orphans just
accumulate); multi-writer efficiency (blind rebase only, correctness ensured
by CAS).

**Exit criteria**
1. put/get/ls work end-to-end against local bare volume+index repos.
2. C1–C4 crash tests pass deterministically.
3. Two concurrent `put` processes: both eventually commit, no lost update,
   store consistent (Section 13.2's "safe but contended").
4. Pinned-commit read: reader pinned to an old log OID sees the old namespace
   while a writer advances the tip (Section 13.3 snapshot semantics).

**DESIGN.md sections:** 3, 4, 8, 11, 13.1–13.3. **Size: L** (the big one).

---

## M4 — Remote backends

**Goal:** the same engine runs against real git servers; backend becomes a trait.

**What this adds:** the network. A backend trait (exactly the two primitives:
reachable-blob push + CAS ref update) with three implementations — local bare
repos, Gitea, GitHub. Data starts moving over the git wire protocol: segments
push to remote volume repos, reads come back via partial-clone blob fetch.
The GitHub adapter arrives last and ships with its safety rails built in:
push-rate token bucket, volume-full threshold, budget wall, and no
repo-creation capability at all. After M4, `--repo <dir>` grows into store
configs pointing at real remote volume sets.

**In scope**
- Backend trait = exactly the two primitives (Section 2.1): reachable-blob
  write (push) + CAS ref update. Local-bare, Gitea, GitHub implementations.
- **Gitea in Docker as the primary integration target** (docker-compose in
  `dev/`); full test suite runs against it in CI-able form.
- Read path: partial clone / promisor blob fetch by OID (Section 10.3);
  **probe open problem 3** — verify Gitea/Forgejo promisor behavior early,
  fall back to full segment fetch if unsupported (record findings in
  DESIGN.md).
- GitHub adapter **last**: token auth; push-rate token bucket (6/min/repo,
  Section 9.5); volume-full threshold 4 GiB + budget wall refusal
  (Sections 15.2–15.3); backoff on 429.
- Store config declares the fixed volume set up front (Section 15.1).

**Explicitly NOT in scope:** compaction; mirroring; erasure coding; any
automatic repo creation beyond the operator-declared fixed set (README
commitment — the GitHub adapter must be incapable of fleet expansion).

**Exit criteria**
1. Full M3 test suite green against Gitea-in-Docker.
2. Promisor-fetch verdict for Gitea recorded (works / fallback engaged).
3. GitHub smoke test on a real private test repo pair (2 volumes + 1 index,
   tiny data, user-initiated): put/get roundtrip, rate governor observably
   throttles, budget wall refuses when a volume is (artificially) marked full.
4. Backend swap requires zero changes above the trait boundary.

**DESIGN.md sections:** 2, 9, 10, 15, 16, 17. **Size: L.**

---

## M5 — Compaction + budget lifecycle

**Goal:** dead space gets reclaimed at volume granularity without churn.

**What this adds:** the full data lifecycle — deletion becomes real. Two new
commands (`rm` for logical delete, `stats` for live/dead/budget accounting)
and the machinery behind them: liveness tracking from the manifest,
hysteresis-gated volume compaction (rewrite live chunks, then delete the
mostly-dead repo whole), orphan sweeping, and the spare-slot headroom that
guarantees compaction always has somewhere to write. Also the budget wall in
its final form: when the declared volumes are full and compaction can't help,
writes refuse — the mechanism that keeps "not unlimited storage" true.

**In scope**
- Liveness from manifest (Section 12.2); dead-ratio accounting per volume.
- Hysteresis-gated compaction (Section 12.4: dead-ratio > 50% AND budget
  pressure AND ≥ 24 h since last — configurable for tests).
- Spare-slot headroom from M0's spec fix (compaction always has a
  destination even at budget pressure).
- Compaction procedure with delete-only-after-CAS ordering (Section 12.3);
  slot reuse, not fleet expansion (Section 15.4).
- Orphan sweep with safety window (Section 12.5).
- `rm` command (logical delete: manifest drops references; bytes die later via
  compaction) and a `stats` command (per-volume live/dead, budget utilization).

**Explicitly NOT in scope:** log compaction (open problem 6 stays open);
policy auto-tuning (open problem 2 — record measurements, don't solve).

**Exit criteria**
1. Fill volumes, delete most files, trigger compaction (test-tuned gates):
   live data survives, old repo deleted only after manifest CAS, store
   consistent throughout (crash-inject during compaction too).
2. Churn guard: a workload oscillating around the pressure threshold does NOT
   cause repeated compactions (hysteresis works).
3. Orphan sweep collects C2-style orphans; never touches in-flight staging.
4. Budget wall: with all volumes full and compaction gated off, writes refuse
   with a clear error (never creates a repo).

**DESIGN.md sections:** 12, 15. **Size: M–L.**

---

## M6 — Hardening + polish

**Goal:** trustworthy tool, measured claims, clean internals.

**What this adds:** proof and polish, not new architecture. A reproducible
benchmark suite (throughput, latency, dedup ratios, segment-size sweep) so the
project's claims are measured numbers; fuzzers on the manifest/segment parsers
(the untrusted-input surface); an optional whole-store mirror to a second,
independent backend — the honest durability story; the gitoxide migration if
the git-CLI shell-outs earned replacement; and user-facing docs including a
threat-model writeup of what the encryption does and does not protect against.

**In scope**
- Optional whole-store mirror to an independent backend (Section 14.3).
- Benchmark suite: throughput (put/get MB/s vs backend), latency per op,
  dedup ratio on realistic workloads, segment-size sweep (informs open
  problem 5). Published in docs as measured numbers, not claims.
- gitoxide migration for in-process packfile/tree construction if the git-CLI
  shell-outs have become the bottleneck or fragility source (decide on
  evidence from M3–M5).
- Docs: user guide, threat-model summary (what the encryption does and does
  NOT protect against — mirrors Section 6.3 honesty), updated README.
- Fuzz targets for the manifest/segment parsers (untrusted-input surface,
  Appendix B rationale).

**Explicitly NOT in scope:** FUSE (post-v1 demo at most); optimistic
multi-writer (Section 13.4, future); RS erasure coding (Section 14.3, future).

**Exit criteria:** benchmarks reproducible via one command; fuzzers run clean
for a fixed budget; docs current; all suites green on local-bare + Gitea.

**DESIGN.md sections:** 14.3, open problems 3/5 evidence. **Size: M.**

---

## M7 (future goal) — large files that span repos

**Goal:** a file larger than one volume can be stored, spread across the volume
set, and — within a user-declared ceiling — across repos the CLI provisions on
demand. Removes today's "one put = one segment = must fit one volume" limit
(GitHub issue #6; DESIGN.md Section 15.3 "Planned evolution").

**In scope**
- **Segment splitting.** Seal a file into multiple bounded (~512 MiB) segments
  (DESIGN Section 3.2 / 9.4 seal triggers) and place each independently via volume
  selection (Section 9.3). Stream the file — never buffer the whole thing in RAM.
- **Bounded, authorized auto-provisioning.** The CLI is authorized with the
  user's token; when the declared volumes fill, it MAY control-plane-provision a
  new volume repo and place the remaining chunks there — but only within a
  **user-declared ceiling** (max repos / total budget) and **rate-limited**
  (respect host push/create limits; no high-frequency repo churn; substantial use
  per repo before the next). At the ceiling, the budget wall (Section 15.3) still
  refuses.

**Explicitly NOT in scope / invariants preserved:** unlimited storage (the
ceiling is hard); high-frequency create/delete churn (Section 17); data-plane repo
creation (provisioning stays control-plane, Section 16 — the CLI just may invoke it
mid-life under the ceiling, not only at `init`); circumvention of a provider's
documented limits.

**Exit criteria**
1. A file larger than any single volume stores + reads back byte-identical,
   spread across multiple volumes (segment splitting), with bounded memory.
2. With auto-provisioning enabled and a ceiling of N repos, a store that fills
   its declared volumes provisions new ones up to N, then refuses at N (budget
   wall) — provisioning observably rate-limited.
3. The failing test from issue #6 (large file across small volumes) passes; the
   `#[ignore]` is removed.

**DESIGN.md sections:** 3.2, 9.3/9.4, 15.1/15.3, 16, 17. **Size: L.**

---

## Sequencing and parallelism

```
Step 0 ──► M0 (code half ∥ spec half) ──► M1 ──► M2 ──► M3 ──► M4 ──► M5 ──► M6
```

- M0's two halves are independent (code vs DESIGN.md) and run in parallel;
  M0 is done only when both land.
- The strict chain M0→M1→M2→M3 exists because each swaps a layer the next
  builds on (chunk boundaries → chunk bytes → container format).
- Within M4, the Gitea promisor probe (risk R1) should run FIRST — it's the
  finding most likely to force read-path rework.
- Documentation duty applies continuously: every milestone updates agent-docs/
  as it works, not at the end.

## Risks and probe points

| # | Risk | Impact | Probe |
|---|------|--------|-------|
| R1 | Gitea/Forgejo promisor/partial-clone blob-fetch unverified (open problem 3) | Primary read path falls back to whole-segment fetch → read amplification | First task of M4: standalone spike against Gitea-in-Docker before building the adapter |
| R2 | Index/log repo grows without bound; pruning = forced ref update, breaking the CAS/fast-forward model (open problem 6, from review round 1) | Long-lived stores degrade; no safe prune protocol yet | Measure log growth in M3 tests; design prune protocol as spec work before M6; M0 (spec half) records it as an open problem |
| R3 | CAS-over-git-CLI awkwardness: exact compare-and-swap needs `git push --force-with-lease=<ref>:<expected-oid>` semantics; CLI error reporting for "stale expected" is stringly | Flaky race handling in M3's core protocol | M3 spike: prove reliable CAS + rejection detection through the CLI against a local bare repo; if brittle, pull gitoxide forward from M6 into M3 |
| R4 | GitHub adapter misuse surface (accidental limit violations) | Policy exposure the project explicitly rejects | M4 exit criterion 3 tests the governor and budget wall; adapter ships with conservative defaults and no repo-creation capability |

## Definition of done (every milestone)

1. All tests pass: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
2. Exit criteria demonstrated with real command output, not assertions of success.
3. `agent-docs/` updated: what changed, decisions taken and why, findings.
4. Any deviation from DESIGN.md recorded in the spec itself (delta or new open
   problem) — the spec stays authoritative.
5. Nothing pushed to any remote without explicit user confirmation.
