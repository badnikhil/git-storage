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

This is design-stage: the diagram below describes the *intended* shape, not a
built system.

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
