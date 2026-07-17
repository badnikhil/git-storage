# git-storage

A log-structured, content-addressed object store built on top of git hosting providers used as immutable blob storage.

⚠️ **status: early experimental project** · working title (`git-storage`) · milestone-0 walking-skeleton CLI (fixed-size chunking, local repos only — see [IMPLEMENTATION-PLAN.md](IMPLEMENTATION-PLAN.md)); [DESIGN.md](DESIGN.md) remains the authoritative target architecture

> [!WARNING]
> This is an **educational / experimental systems project**. It is **NOT** intended, endorsed, or supported for use as "unlimited free cloud storage". Users are **solely responsible** for complying with the terms of service and acceptable use policies of any storage backend they configure. The project **deliberately refuses** to design or ship features whose purpose is to circumvent a platform's storage quotas, rate limits, bandwidth limits, or file-size limits. If you want bulk storage, use object storage (S3 and friends) or self-hosted infrastructure you operate.

---

## What this is

`git-storage` is a design exploration of whether the primitives that git hosting
providers expose — immutable objects, content addressing, and atomic
compare-and-swap ref updates — can be composed into a small, honest object store.

The primary scale target is a **self-hosted git server** (Gitea / Forgejo), where
*you* are the operator and *you* set the limits. Hosted providers such as GitHub
are treated as *one supported backend among several*, usable only at modest scale
and strictly within their documented limits (see [Platform policy](#platform-policy)).

Core design ideas:

- **Chunking.** Files are split with content-defined chunking (FastCDC-style) so
  that edits touch only the chunks that changed.
- **Optional client-side encryption.** Encryption happens before anything leaves
  the client, so the backend only ever sees opaque ciphertext chunks.
- **Sealed segments (LSM-tree style).** Chunks are packed into immutable, sealed
  repository "segments." Once sealed, a segment is never mutated — mirroring how
  an LSM-tree writes immutable sorted runs.
- **A transaction log on git's one atomic primitive.** The metadata / manifest
  layer is a log built on the *only* atomic operation git hosts offer: the
  **compare-and-swap ref update**. This is the same architectural family as
  Delta Lake / Apache Iceberg building a transaction log on top of S3.
- **Deduplication via content addressing.** Identical chunks are stored once,
  addressed by their content hash.
- **Compaction at repository granularity.** Because per-object deletion inside a
  git host is not a reliable primitive, the only true "delete" is dropping a whole
  repository. Compaction therefore happens at repository granularity: live chunks
  are rewritten into fresh segments and the old repository is retired.
- **Multi-backend abstraction.** Backends are pluggable. Erasure coding is only
  meaningful *across independent backends* (spreading shards over providers /
  servers that fail independently), not within one.

## What this is NOT

- ❌ **Not an "unlimited free cloud drive."** There is a fixed, user-declared
  repository budget. The system will not silently grow beyond it.
- ❌ **Not automated repo-fleet expansion.** It does **not** spin up new
  repositories on the fly to dodge per-repo size limits. The repo budget is
  declared by you, up front, and stays fixed.
- ❌ **Not a circumvention tool.** Any feature whose *purpose* is to evade a
  provider's quotas, rate limits, or file-size limits is out of scope by design.
- ❌ **Not a backup product.** See GitHub's own guidance: "Git is not designed to
  serve as a backup tool."

## Architecture sketch

The core is now **implemented** (milestones M0–M6): content-defined chunking,
keyed convergent encryption, sealed segments over a CAS transaction log,
pluggable backends (local, and remote git over the wire), compaction + budget
enforcement, and a whole-store mirror. It is exercised end-to-end against local
bare repos and over `file://` remotes; validation against live hosted providers
(GitHub / a self-hosted Gitea) is the remaining, deliberately-gated step. The
diagram below is the shape that exists, minus that live-host validation.

```
   file
    │
    ▼
 ┌────────────────────┐
 │ content-defined    │   FastCDC-style; edits touch only changed chunks
 │ chunking (FastCDC) │
 └─────────┬──────────┘
           │  chunks
           ▼
 ┌────────────────────┐
 │ optional client-   │   backend only ever sees opaque ciphertext
 │ side encryption    │
 └─────────┬──────────┘
           │  content-addressed, deduplicated
           ▼
 ┌────────────────────┐        ┌──────────────────────────────┐
 │ sealed, immutable  │◀──────▶│ manifest / transaction log    │
 │ segments (LSM)     │        │ on atomic CAS ref updates      │
 └─────────┬──────────┘        │ (Delta/Iceberg-on-S3 family)   │
           │                   └──────────────────────────────┘
           ▼
 ┌───────────────────────────────────────────────────────────┐
 │ multi-backend abstraction                                  │
 │   ├─ self-hosted Gitea / Forgejo   (primary scale target)  │
 │   ├─ GitHub  (modest scale, within documented limits)      │
 │   └─ …other git backends                                   │
 │  erasure coding only meaningful ACROSS independent backends│
 └───────────────────────────────────────────────────────────┘
```

Envisioned interfaces come later: a **CLI + SDK with explicit sync semantics
first**; a **FUSE** filesystem only as a cached / write-back *demo*, never as the
primary interface.

## Using it

The tool is a single binary (`cargo build --release` → `target/release/git-storage`).
Every command needs a `--keyfile`: the master key that encrypts the store. **Lose
the keyfile and the store is unrecoverable** — there is no backdoor, by design.
The keyfile is created (mode 0600) with a brand-new store and never regenerated
for an existing one.

```sh
# Initialise a store with a fixed, operator-declared volume set.
# file:// volumes are inited as bare repos; https:// (GitHub/Gitea) are created
# via the control-plane REST API with GITSTORAGE_TOKEN, and ONLY the declared set.
git-storage init --repo ./store --keyfile ./master.key \
    --volume v0=file:///srv/git/v0.git \
    --volume v1=file:///srv/git/v1.git \
    --index-url file:///srv/git/index.git

git-storage put ./photo.raw     --repo ./store --keyfile ./master.key   # store a file
git-storage ls                  --repo ./store --keyfile ./master.key   # list (metadata is encrypted)
git-storage get photo.raw --output ./out.raw --repo ./store --keyfile ./master.key
git-storage rm  photo.raw       --repo ./store --keyfile ./master.key   # logical delete
git-storage stats               --repo ./store --keyfile ./master.key   # per-volume live/dead/budget
git-storage compact             --repo ./store --keyfile ./master.key   # reclaim dead space (gated)

# Whole-store mirror to an INDEPENDENT backend for durability (ciphertext only):
git-storage mirror --repo ./store --keyfile ./master.key \
    --to-index file:///mnt/backup/index.git \
    --to-volume v0=file:///mnt/backup/v0.git \
    --to-volume v1=file:///mnt/backup/v1.git

# Snapshot reads: pin a log commit and read the store as of that point.
git-storage tip --repo ./store --keyfile ./master.key                 # -> <commit>
git-storage ls  --repo ./store --keyfile ./master.key --at <commit>
```

A local store without a declared volume set (`git-storage put --repo ./store …`
with no prior `init`) works too — it synthesises a single local volume, which is
how the earlier milestones' stores keep working unchanged.

Reproduce the throughput / dedup / chunk-size numbers with one command:
`cargo run --release --example bench`.

## Threat model — what the encryption protects (and what it does not)

Encryption is **keyed convergent** (DESIGN.md §6.3): dedup still works within a
store, and an outsider cannot confirm-a-guessed-file against it. Being honest
about the boundaries:

**Protects (against a backend that is honest-but-curious or fully hostile):**
- **Content confidentiality.** Chunks are zstd-compressed then sealed with
  XChaCha20-Poly1305; the backend sees only opaque ciphertext blobs.
- **Metadata confidentiality.** File names, sizes, and chunk layout live in the
  transaction log, which is itself encrypted — the backend cannot read the
  namespace.
- **Integrity / tamper-evidence.** Every chunk is content-addressed (BLAKE3 of
  the ciphertext) and AEAD-tagged; a flipped bit or substituted blob fails
  loudly on read, never returns wrong plaintext. (Fuzz-tested in `tests/fuzz.rs`.)

**Does NOT protect:**
- **The keyfile.** It is the whole ballgame; its custody is out of scope. Lose
  it → data lost. Leak it → data exposed.
- **Availability.** A backend can delete or withhold your repos. The **mirror**
  (independent second backend) is the mitigation, not the encryption.
- **Access-pattern / size side-channels.** The backend still sees blob sizes,
  counts, and access timing. Chunk *boundaries* are hidden by a per-store
  key-derived gear seed (§5.4), but coarse volume/segment sizes are observable.
- **Cross-store correlation is intentionally impossible** (no cross-user dedup,
  §7) — a non-goal turned into a privacy property, not a gap.

## Status and what's left

**Done — the v1 core (milestones M0–M6).** Content-defined chunking, keyed
convergent encryption, sealed segments over a CAS transaction log, a pluggable
backend trait (local + remote git over the wire), compaction + budget
enforcement, and a whole-store mirror. 97 tests (unit + integration + fuzz),
green against local bare repos and `file://` remotes; `cargo clippy -D warnings`
and `cargo fmt --check` clean.

**Remaining before this is a real v1 you'd trust with data:**

- **Live-host validation (the biggest gap).** Everything is proven over
  `file://`, which exercises the same git send-pack/receive-pack path but not a
  real provider. Two things are written and env-gated but never run here: the
  **Gitea promisor-fetch verdict** (does partial-clone blob-by-OID work on
  Gitea/Forgejo, or does the read path fall back to full-segment fetch?) and a
  **GitHub smoke test**. Until these run, "works on real hosts" is unproven.
- **Known limitations surfaced by the test suite** (see *Open problems* below and
  the issue tracker): compaction is not snapshot-aware; concurrent writing during
  compaction of the same volume is outside the single-writer model.
- **Index/log repository grows without bound.** Checkpoints bound *reader* work
  but never reclaim log history; there is no safe prune protocol yet (pruning is
  a non-fast-forward update, which breaks the CAS model). Long-lived stores need
  this solved (DESIGN.md open problem 6).
- **`gitoxide` migration (deferred, with evidence).** Write throughput is bound
  by one `git hash-object` process per chunk (see `cargo run --release --example
  bench`), and race detection scrapes git's stderr. Both argue for in-process
  gitoxide, but the migration is large and high-risk against a green test suite,
  so it's the prime post-v1 milestone, not v1 work.

**Explicitly deferred to future work (non-goals for v1):** Reed–Solomon erasure
coding *across independent backends* (the only honest EC configuration),
optimistic multi-writer conflict resolution, and a FUSE filesystem (a cached
demo at most, never the primary interface).

### Open problems (current, tracked as GitHub issues)

The authoritative list is **DESIGN.md §18**. The ones a user should know about:

| DESIGN §18 | Problem | Current guarantee / status | Issue |
|---|---------|----------------------------|-------|
| 3 | Gitea/Forgejo promisor fetch unverified | Mechanism + fallback implemented; **live verdict pending** | [#3](https://github.com/badnikhil/git-storage/issues/3) |
| 6 | Index/log repo grows unbounded | No safe prune protocol yet; blocks long-lived stores | [#4](https://github.com/badnikhil/git-storage/issues/4) |
| 7 | Compaction is not snapshot-aware | A snapshot pinned before a compaction that retired its data becomes unreadable — **guaranteed to fail loudly, never silent wrong data** (locked by a test) | [#1](https://github.com/badnikhil/git-storage/issues/1) |
| 8 | Concurrent write during same-volume compaction | Outside the v1 single-writer model; committed data is protected, an in-flight write is not | [#2](https://github.com/badnikhil/git-storage/issues/2) |

Plus [#5](https://github.com/badnikhil/git-storage/issues/5): evaluate a `gitoxide`
migration for the object/CAS path (post-v1 performance + robustness).

## Platform policy

`git-storage`'s primary target is **self-hosted git servers, where the operator
sets the limits.** On hosted providers the project's stance is: stay small, stay
within documented limits, and never build features to evade them.

GitHub's Acceptable Use Policies are explicit about the risk. From
**§9 "Excessive Bandwidth Use"**
([GitHub Acceptable Use Policies](https://docs.github.com/en/site-policy/acceptable-use-policies/github-acceptable-use-policies)):

> "The Service's bandwidth limitations vary based on the features you use. If we determine your bandwidth usage to be significantly excessive in relation to other users of similar features, we reserve the right to suspend your Account, throttle your file hosting, or otherwise limit your activity until you can reduce your bandwidth consumption. We also reserve the right—after providing advance notice—to delete repositories that we determine to be placing undue strain on our infrastructure."

And from
**[About large files on GitHub](https://docs.github.com/en/repositories/working-with-files/managing-large-files/about-large-files-on-github)**:

> "Git is not designed to serve as a backup tool. However, there are many solutions specifically designed for performing backups, such as Arq, Carbonite, and CrashPlan."

The same page also states that GitHub recommends
"repositories remain small, ideally less than 1 GB, and less than 5 GB is
strongly recommended."

If GitHub is used as a backend, it must be used **at modest scale within the
documented limits.** The load-bearing numbers:

| Limit | Value | Source |
|---|---|---|
| Per-file hard block on push | **100 MiB** (warns at 50 MiB) | [About large files](https://docs.github.com/en/repositories/working-with-files/managing-large-files/about-large-files-on-github) |
| Repository size | **< 5 GB strongly recommended** (< 1 GB ideal) | [About large files](https://docs.github.com/en/repositories/working-with-files/managing-large-files/about-large-files-on-github) |
| Authenticated REST API | **5,000 requests / hour** | [REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api) |
| Content-creation (writes) | **80 / minute and 500 / hour** | [REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api) |

Writes are the bottleneck: the 80/min and 500/hour content-creation caps mean a
GitHub backend can sustain only on the order of ~500 blob-creation operations per
hour via the API. That is a design constraint, not a limit to be gamed.

## Prior art

- **[bup](https://bup.github.io/)** — deduplicating backup that stores content in
  its own git-packfile format. We take the idea of git-packfile-shaped,
  content-hashed storage.
- **[git-annex](https://git-annex.branchable.com/)** — keeps large *content* out
  of the repo and points to pluggable "special remotes." We take the
  multi-backend / pluggable-remote abstraction.
- **[restic](https://restic.net/) / [borg](https://www.borgbackup.org/)** —
  content-defined chunking + deduplication done well. We take FastCDC-style
  chunking and dedup by content address.
- **[Delta Lake](https://delta.io/)** (and Apache Iceberg) — a transaction log
  layered on immutable object storage (S3). We take the "atomic log over immutable
  objects" pattern, mapped onto git's compare-and-swap ref updates.

## License

Licensing here is **layered and deliberately not OSI-pure**:

- **[`LICENSE`](LICENSE)** — the base grant is **Apache License 2.0**, unmodified.
- **[`ACCEPTABLE-USE.md`](ACCEPTABLE-USE.md)** — a supplemental acceptable-use
  condition (in the style of the Commons Clause) layered on top of Apache-2.0.
  It withholds the license grant for uses that violate a provider's terms of
  service, that are designed to circumvent a provider's quotas / rate / bandwidth
  / file-size limits, or that market the software as "unlimited" storage on
  infrastructure you do not operate.
- **[`NOTICE`](NOTICE)** — the standard Apache NOTICE, restating the educational
  intent and the user's responsibility for backend terms of service.

Because of the supplemental restriction, the **combined terms are not
OSI-approved open source.** This is a deliberate choice: the Apache-2.0 base gives
a clear liability shield and patent grant, while the supplemental clause exists to
discourage misuse. Uses on infrastructure you own or operate yourself are
unrestricted — see `ACCEPTABLE-USE.md`.
