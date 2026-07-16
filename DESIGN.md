# git-storage — v1 Design Document

> Status: **design-stage**. This document specifies the v1 architecture. No code
> exists yet. Where a value is a proposal rather than a derived hard constraint it
> is labelled **PROPOSED** with rationale. Every load-bearing platform number is
> cited to primary documentation.
>
> This document is a project artifact and is intended to be committed. It is
> distinct from the `agent-docs/` scratch notes, which are local-only.

Spec-language convention: **MUST**, **MUST NOT**, **SHOULD**, **MAY** carry
RFC-2119 force. "The client" is the local process performing chunking, encryption,
and git protocol operations. "The backend" is a git hosting server reached over
the git wire protocol (self-hosted Gitea/Forgejo, or GitHub).

---

## 0. Table of contents

1. Scope, goals, and non-goals
2. System model and backend abstraction
3. Storage hierarchy: chunk, segment, volume
4. Naming, refs, and reachability
5. Chunking specification
6. Compression and encryption specification
7. Deduplication
8. The manifest transaction log
9. Write path
10. Read path
11. Crash consistency
12. Compaction and garbage collection
13. Multi-writer concurrency and consistency model
14. Erasure coding and replication
15. Repository budget enforcement
16. Control plane vs data plane
17. Failure and abuse-signature avoidance
18. Open problems
19. Appendix A — Platform constraints as design inputs
20. Appendix B — Implementation language analysis
21. Appendix C — Prior-art delta

---

## 1. Scope, goals, and non-goals

### 1.1 Goal

Compose the two primitives that every git host reliably exposes — immutable,
content-addressed object storage reachable from refs, and atomic compare-and-swap
ref update — into a small, honest, log-structured object store with client-side
encryption, deduplication, snapshot-consistent reads, and repository-granularity
garbage collection.

### 1.2 Primary scale target

The primary scale target is a **self-hosted Gitea/Forgejo server**, where the
operator sets the limits. GitHub is **one supported backend among several**,
usable only at modest scale strictly within its documented limits (Appendix A).
The design MUST be correct on any backend providing the two primitives of Section 2;
GitHub-specific numbers constrain only the GitHub backend's operating envelope.

### 1.3 Non-goals (out of scope BY DESIGN)

These mirror the README's "What this is NOT" commitments. The design MUST NOT
contradict them.

- **Not unlimited storage.** There is a fixed, user-declared **repository budget**
  (Section 15). The system refuses writes that would exceed it. It does not silently grow.
- **Not automated repo-fleet expansion.** The system MUST NOT create new
  repositories on the fly to dodge per-repo size limits. The set of volume
  repositories is declared up front by the operator and stays fixed for the life
  of the store (except operator-initiated reconfiguration).
- **Not a circumvention tool.** Any feature whose *purpose* is to evade a provider's
  quotas, rate limits, bandwidth limits, or file-size limits is out of scope. The
  design deliberately chooses *lazy* behavior (Section 12) precisely because
  high-frequency evasion patterns are both bad engineering and abuse-detection
  signatures (Section 17).
- **Not a backup product.** GitHub's own guidance states "Git is not designed to
  serve as a backup tool."[^aup-largefiles] The primary target is self-hosted
  infrastructure the operator owns.

### 1.4 What v1 delivers

- Content-defined chunking, client-side compress-then-encrypt, content addressing.
- Sealed immutable segments as the write unit; volumes as the delete unit.
- A serialized transaction log on a single CAS-guarded ref, with periodic
  checkpoints and snapshot-consistent reads.
- Single-logical-writer consistency, with concurrent writers *safe* (no lost
  updates) but contended.
- Repository-budget enforcement and lazy, hysteresis-gated compaction.
- Single backend plus optional whole-store mirror.

Explicitly deferred to future work: Delta-Lake-style optimistic multi-writer
conflict resolution (Section 13.4), Reed–Solomon erasure coding across independent
backends (Section 14.3), FUSE as anything more than a cached demo.

---

## 2. System model and backend abstraction

### 2.1 The two primitives

The backend abstraction is deliberately minimal. A backend MUST provide exactly
two operations; everything else in the system is built from them.

**P1 — Immutable content-addressed blob storage reachable from refs.** The client
can write an object (a git blob/tree/commit), and as long as some ref transitively
reaches that object, the server MUST retain it. Objects are addressed by a
collision-resistant hash. This is ordinary git object storage.

**P2 — Atomic compare-and-swap ref update.** The client can request "set ref R
from old-oid O_old to new-oid O_new"; the server MUST apply it atomically and MUST
reject the update if R's current value is not O_old (git's non-fast-forward /
stale-oid rejection, i.e. `git push` with the receiving side enforcing
`--force`-free fast-forward or an exact old-oid). This is the *only* atomic
mutation the system relies on.

### 2.2 Reachability and GC (why P1 needs refs)

Git servers garbage-collect objects that no ref reaches. Therefore every object
the store depends on MUST remain reachable from a ref the client controls. The
consequence pervades the design: data chunks are not "just uploaded"; they are
sealed under a ref (Section 4) so the server's own GC will not reclaim them.

### 2.3 What the abstraction does NOT assume

The abstraction MUST NOT assume: per-object deletion (git offers no reliable
per-blob delete — the only true delete is dropping a whole repo, Section 12); server-side
byte-range reads of blobs (unverified on GitHub raw, Section 10.4); server-side dedup
or delta compression of *our* objects (we deliver already-compressed ciphertext,
Section 6); or transactions spanning more than one ref (there are none — the log is a
single ref, Section 8).

### 2.4 Latency model

Backend operations cost **hundreds of milliseconds per round trip** (TLS + git
negotiation + server processing), and hosted backends additionally impose
rate limits measured in operations per minute (Appendix A). The dominant design
pressure is therefore **amortization**: minimize the number of backend operations
per byte stored. This is the direct justification for large chunks (Section 5.3) and for
batching many chunks into one push per segment (Section 3.2).

---

## 3. Storage hierarchy: chunk, segment, volume

Three levels. Each maps to a distinct backend concern.

```
  VOLUME (= one git repository)          unit of DELETION / GC / budget
   ├── SEGMENT (= one commit + tree)      unit of WRITE (one push)
   │    ├── CHUNK  (= one git blob)        unit of DEDUP / addressing
   │    ├── CHUNK
   │    └── ...
   ├── SEGMENT
   └── ...
```

### 3.1 CHUNK — unit of deduplication and content addressing

A chunk is a FastCDC content-defined slice of a file's byte stream, after
compression and encryption (Sections 5 and 6). A chunk is stored as a single git **blob**.
Its identity — the **chunk ID** — is the hash of the **ciphertext as stored**
(Section 6.6). Content addressing on stored ciphertext is what gives deduplication (Section 7).

Target average chunk size is large (Section 5.3, PROPOSED 1 MiB avg) because sub-100 KB
objects would multiply the per-object backend latency and rate-limit cost of Section 2.4
without benefit.

### 3.2 SEGMENT — unit of write

A segment is **one sealed batch of chunks**, materialized as **one git commit
whose tree contains the batch's chunk blobs**, delivered to a volume in **one git
push**. Segments are the LSM-tree "sorted run" analogue: written once, immutable
forever, never updated in place.

Rationale for the segment = one-push mapping:

- GitHub enforces a **2 GB push size limit**[^repo-limits] and recommends **no more
  than 6 pushes/minute per repository**.[^repo-limits] Batching many chunks into
  one push is the only way to stay under the push-rate ceiling while moving useful
  volume. One push per segment makes the write unit and the rate-limited unit the
  same object, which makes rate budgeting tractable.
- A sealed segment is a single atomic unit of reachability: once the segment's
  ref (Section 4.2) points at the commit, every chunk in it is retained by the server.

Segment sealing is triggered when (Section 9.4): the staged segment reaches a size
threshold (**PROPOSED** target 512 MiB packed, hard ceiling well under the 2 GB
push cap to leave headroom, Section 18 open-problem 5), OR the client is asked to `sync`
explicitly, OR a staging-age timeout fires. Once sealed, a segment MUST NOT be
mutated.

### 3.3 VOLUME — unit of deletion, GC, and budget

A volume is **one git repository**. It holds many segments. It is the unit of:

- **Deletion.** Per-object delete is not a backend primitive (Section 2.3). The only
  reliable "delete" is dropping an entire repository. Compaction (Section 12) therefore
  works at volume granularity: rewrite the still-live chunks of a mostly-dead
  volume into a fresh volume, commit the manifest change, then delete the old
  repository whole.
- **Budget.** The operator declares a fixed set of volume repositories and a size
  budget (Section 15). The system refuses writes beyond it.

A volume's on-disk `.git` size MUST be kept below the backend's recommended
ceiling. On GitHub that is **< 5 GB strongly recommended, < 1 GB ideal**, with a
**10 GB on-disk `.git` recommended maximum**.[^repo-limits][^aup-largefiles] The
volume-full threshold (Section 15.2) is set below that ceiling.

---

## 4. Naming, refs, and reachability

### 4.1 Store identity

A *store* is the top-level logical object: one manifest transaction log (Section 8) plus
a fixed set of volume repositories. A store has a master key (Section 6.2) and a
randomized chunker seed (Section 5.4). One index repository holds the log; N volume
repositories hold segments.

### 4.2 Segment refs

Each sealed segment is pinned by a ref in its volume repository:

```
refs/segments/<segment-id>   ->  <commit-oid of the sealed segment>
```

`<segment-id>` is a store-unique identifier (**PROPOSED**: a random 128-bit value
rendered hex, not derived from content, so segment identity is decoupled from any
single chunk). Pinning each segment under its own ref guarantees the server retains
the segment's chunks (Section 2.2) and lets compaction retire one segment's ref without
disturbing others. Segment commits do not form a chain; they are independent
roots, each reachable via its own ref. This avoids a single mutable branch tip
whose history GitHub would treat as a growing linear branch.

### 4.3 Chunk placement inside a segment tree

Chunks are laid out in the segment's git tree under **fanout directories keyed by a
hex prefix of the chunk ID**:

```
<aa>/<bb>/<full-chunk-id>
```

where `<aa>` and `<bb>` are the first and second hex bytes of the chunk ID.
Two-level, two-hex-char fanout yields up to 256 top-level and 256 second-level
directories. This respects the backend's **3,000-entries-per-directory** limit and
**50-directory-depth** limit[^repo-limits]: with 256×256 = 65,536 leaf directories,
a segment can hold on the order of 65,536 × 3,000 ≈ 196M chunk entries before any
single directory overflows — far beyond a single segment's chunk count. Depth is 3,
well under 50.

**PROPOSED** fanout width: 2 hex chars (1 byte) per level, 2 levels. Rationale:
one byte per level keeps directory names short and the tree shallow; two levels
suffice for realistic per-segment chunk counts (a 512 MiB segment at 1 MiB avg
chunk ≈ 512 chunks, so even one level of 256 dirs keeps each dir ~2 entries — two
levels is future headroom, not immediate need). If a segment ever approached the
3,000-entry width limit at one level, the second level absorbs it.

### 4.4 The log ref

The manifest transaction log lives in the dedicated index repository under a single
ref:

```
refs/heads/log   ->  <commit-oid of the latest transaction>
```

This is the one ref updated by CAS as the system's atomic commit point (Section 8.3).

---

## 5. Chunking specification

### 5.1 Algorithm

The client MUST chunk file byte streams with **FastCDC** (content-defined chunking
via a rolling gear hash with normalized chunk-size distribution), the same family
used by restic and borg. Content-defined boundaries mean an insertion or deletion
shifts only the chunks local to the edit, preserving deduplication of unchanged
regions (Section 7).

### 5.2 Parameters

FastCDC is parameterized by minimum, average (normal), and maximum chunk size.

| Parameter | **PROPOSED** value | Rationale |
|---|---|---|
| min | 512 KiB | Floor on object size; below this the per-object backend latency/rate cost (Section 2.4) dominates. |
| avg (normal) | 1 MiB | Target average; see Section 5.3. |
| max | 4 MiB | Ceiling to bound worst-case blob size and memory; still an order of magnitude under the 100 MiB per-file push block.[^repo-limits] |

These mirror restic's tunable min/avg/max shape but shifted an order of magnitude
larger than restic's defaults (restic centers near ~1 MiB avg with 512 KiB/8 MiB
bounds); the shift up is deliberate given Section 5.3.

### 5.3 Why large chunks — the latency arithmetic

Backend operations cost hundreds of ms each and are rate-limited per minute
(Section 2.4, Appendix A). Consider two average chunk sizes for storing 1 GiB:

- **64 KiB avg** → ~16,384 chunks. Even at a favorable 5 chunks packed per object
  this is thousands of objects; and on GitHub the **content-creation cap of
  80/min and 500/hr**[^rate-limits] would make even the control-plane accounting
  infeasible, while per-push object counts balloon.
- **1 MiB avg** → ~1,024 chunks per GiB. At 512 chunks per 512 MiB segment, one
  GiB is ~2 pushes. Under the **6 pushes/min/repo**[^repo-limits] ceiling that is
  trivially within budget.

The ~16× reduction in object count from 64 KiB → 1 MiB directly buys ~16× fewer
backend round trips per byte. Because the data plane is network-bound at tens of
MB/s (Section 20.2) while FastCDC itself runs at GB/s (Appendix B), making chunks large
costs nothing in CPU and saves everything in backend operations. Large chunks do
mildly reduce deduplication granularity (a change touches a larger minimum unit),
which is the accepted tradeoff.

### 5.4 Randomized chunker seed — a security requirement (CRITICAL)

**The chunker's gear table / seed MUST be randomized per store, derived from the
master key.** Concretely: `gear_seed = HKDF(master_key, "git-storage/chunker-gear")`,
and the FastCDC gear table is generated deterministically from `gear_seed`.

**The attack this defends against.** Content-defined chunk *boundaries* — and hence
the *sequence of chunk sizes* a file produces — are a function of the plaintext
content and the (normally fixed, public) gear table. Chunk sizes are visible to
anyone who can see the stored objects, because encryption hides chunk *contents*
but not chunk *lengths*. A fixed, publicly-known gear table therefore leaks a
content fingerprint that **survives encryption**: an adversary who knows a
candidate file can chunk it with the public gear table, compute its chunk-size
sequence, and match that sequence against the observed ciphertext object sizes to
confirm the file's presence — a fingerprinting / confirmation-of-file side channel.
Randomizing the gear table per store from the secret master key makes the chunk-size
sequence unpredictable to anyone without the key, closing the channel. **borg does
exactly this** (per-repo randomized chunker seed); this design adopts it as a hard
requirement.

Residual leakage that this does NOT fix: total stored size, number of chunks, and
coarse timing remain observable. This is acknowledged, not solved, in v1.

---

## 6. Compression and encryption specification

### 6.1 Order: compress THEN encrypt

The client MUST **compress before encrypting**. Two reasons:

1. Ciphertext is incompressible, so compression MUST precede encryption to have any
   effect.
2. The backend cannot delta-compress or dedup our ciphertext anyway (Section 2.3), so the
   client owns compression end-to-end; there is no server-side compression to defer
   to. We deliver already-small, opaque objects.

Compression: **zstd**. zstd is a linked C library (never hand-rolled, Appendix B Section 20.3),
language-neutral, with a good ratio/speed frontier. **PROPOSED** default level 3,
tunable per store.

The compress-then-encrypt order carries the standard CRIME/BREACH-class caveat:
compressing then encrypting attacker-influenced-plus-secret data in the same unit
can leak via ciphertext length. Here each chunk is an independent unit with no
mixing of attacker-chosen and secret plaintext within a chunk, and chunk lengths
are already assumed observable and defended at the chunker-seed layer (Section 5.4); the
caveat is noted for completeness.

### 6.2 Key hierarchy

```
master_key (256-bit, user-held; never leaves client)
   ├── chunk-key derivation:      chunk_key = HMAC(master_key, H(plaintext_chunk))   [Section 6.3]
   ├── manifest_key = HKDF(master_key, "git-storage/manifest")                        [Section 8.7]
   └── chunker_gear_seed = HKDF(master_key, "git-storage/chunker-gear")               [Section 5.4]
```

The master key is the single root secret. Loss of the master key means loss of the
store (no recovery by design — the backend sees only ciphertext). Master-key
storage/rotation is a control-plane concern (Section 16); rotation is future work because
convergent chunk keys (Section 6.3) are content-derived and re-keying implies rewriting.

### 6.3 Keyed convergent encryption

Per-chunk keys are **keyed-convergent**:

```
chunk_key = HMAC(master_key, H(plaintext_chunk))
```

where `H` is a collision-resistant hash of the *plaintext* chunk. Properties:

- **Dedup preserved within the store.** Two identical plaintext chunks produce the
  same `H(plaintext)`, hence the same `chunk_key`, hence — with a deterministic
  nonce derived from the same material (Section 6.5) — the same ciphertext and the same
  chunk ID. Identical plaintext deduplicates (Section 7).
- **Convergent-encryption confirmation attack defeated.** Plain (unkeyed)
  convergent encryption sets the key to `H(plaintext)`, which anyone can compute;
  an attacker can then encrypt a *guessed* plaintext and test whether the resulting
  ciphertext already exists in storage — confirming the victim stores that exact
  file. Mixing the secret `master_key` into the key via HMAC means an attacker
  **without** the master key cannot compute `chunk_key`, cannot produce the matching
  ciphertext, and therefore **cannot test guessed plaintext against stored
  ciphertext**. This is the honest, specific security win.
- **What it does NOT protect against (stated honestly).** An attacker *with* the
  master key (or an insider) can still run the confirmation attack — keyed
  convergence protects against outsiders, not key-holders. It also does not hide
  dedup *within* the store from someone who holds the key. And because dedup is
  deterministic within a store, an on-backend observer can still see *that* two
  stored objects are byte-identical (they share a chunk ID); keyed convergence hides
  *which plaintext* that is from a non-key-holder, not the fact of internal
  repetition. Cross-user dedup is deliberately absent (Section 7) precisely so that this
  observability never spans trust boundaries.

### 6.4 AEAD cipher

Each chunk is sealed with an **AEAD**. **PROPOSED: XChaCha20-Poly1305.**

Rationale for XChaCha20-Poly1305 over AES-256-GCM:

- **192-bit nonce** removes nonce-collision anxiety. GCM's 96-bit nonce makes
  random-nonce reuse a real risk at scale (birthday bound ~2^48 messages); with a
  content-addressed store writing potentially billions of chunks over its life, the
  large nonce space is worth having.
- No dependence on hardware AES for competitive software performance; the data
  plane is network-bound (Section 20.2) so cipher throughput is not the constraint, and
  ChaCha is fast and constant-time in pure software (matters for the Rust/no-AES-NI
  and mobile cases).
- AES-256-GCM remains an acceptable alternative where a validated/hardware
  implementation is mandated; the AEAD is an interface, not a hardcoded choice, but
  v1 defaults to XChaCha20-Poly1305.

The AEAD's associated-data field MUST bind the chunk ID and a store/format version
so a chunk cannot be silently substituted or replayed across format versions.

### 6.5 Nonce derivation

To keep dedup deterministic (Section 6.3) the nonce MUST be a deterministic function of
the plaintext and key material, not random:

```
nonce = truncate_192( HMAC(master_key, "git-storage/nonce" || H(plaintext_chunk)) )
```

Because the nonce is derived from the same plaintext+key inputs as the chunk key,
identical plaintext yields identical (key, nonce, ciphertext), preserving dedup,
while distinct plaintext yields distinct nonces with overwhelming probability. The
large XChaCha nonce space makes derived-nonce collision negligible.

### 6.6 Chunk ID

```
chunk_id = H(ciphertext_as_stored)
```

The chunk ID is the hash of the exact bytes written to the backend (the AEAD
output). Content addressing is therefore on stored ciphertext: the git blob's own
object hash and our logical chunk ID coincide in *what they cover* (the stored
bytes), which lets the read path fetch by OID (Section 10) and lets integrity be checked
by rehashing fetched bytes. **PROPOSED** hash: BLAKE3 (fast, 256-bit,
tree-hash-parallelizable). Any collision-resistant 256-bit hash is acceptable.

---

## 7. Deduplication

Deduplication is a direct consequence of content addressing on keyed-convergent
ciphertext (Section 6): identical plaintext chunks map to identical chunk IDs and are
stored once. The manifest (Section 8) records a chunk once; subsequent references are
manifest pointers, not new blobs.

**No cross-user / cross-store deduplication, by design.** Each store has its own
master key, hence its own `chunk_key` and nonce derivation, hence *different*
ciphertext for the *same* plaintext across stores. Two users storing the identical
file produce different chunk IDs and do not share storage. This forecloses
cross-user dedup — a deliberate privacy choice: cross-user dedup would reintroduce
exactly the confirmation-attack surface Section 6.3 closes, across trust boundaries. Dedup
scope is **intra-store only**.

---

## 8. The manifest transaction log

This is the core of the system.

### 8.1 Structure

A **dedicated index repository** holds a single ref, `refs/heads/log` (Section 4.4). The
log is a **serialized transaction log**: a chain of commits where each commit is one
transaction. This is the Delta Lake / Iceberg pattern — a transaction log over
immutable object storage — mapped onto git's CAS ref (the README's stated
architectural family).

### 8.2 What a transaction records

Each transaction commit's tree/payload records the delta applied by that
transaction:

- **Files added / removed / modified** (logical namespace entries).
- **File → chunk-list mappings** (ordered list of chunk IDs reconstructing each
  file).
- **Chunk → placement mappings**: `chunk_id → (volume_id, segment_id)`.
- **Segment seal records**: segment ID, volume, its `refs/segments/...` OID, chunk
  count, packed size.
- **Volume state changes**: volume created/retired, size accounting, budget state.

The transaction payload is encrypted with `manifest_key` (Section 8.7) — the backend must
not read the namespace or placement metadata in cleartext.

### 8.3 Commit protocol (the two-phase discipline)

A writer commits a transaction in two ordered phases:

1. **Phase 1 — write data first (idempotent).** Push all new sealed segments to
   their volumes (Section 9). Segments are content-addressed and immutable, so this phase
   is **idempotent and safe to redo**: re-pushing the same segment is a no-op or a
   redundant write of identical objects; a partially-completed phase-1 leaves only
   *unreferenced* (orphaned) objects, which corrupt nothing (Section 11).

2. **Phase 2 — CAS the log ref (the commit point).** Build the transaction commit
   whose parent is the log tip the writer read, then perform **P2**: set
   `refs/heads/log` from `old = observed_tip` to `new = transaction_commit`. The
   CAS success **is** the atomic commit point. Nothing is "committed" until this
   succeeds.

Ordering rationale: data must be durable and reachable *before* the manifest points
at it, so that at every instant the manifest only ever references data that already
exists. The reverse order could publish a manifest referencing not-yet-pushed
chunks.

### 8.4 CAS race handling — rebase and retry

If Phase 2's CAS is rejected (another writer advanced the tip), the loser MUST:

1. Fetch the new log tip and load the winner's transaction(s).
2. **Rebase its transaction record onto the winner**: re-parent its commit onto the
   new tip and re-validate. Because Phase 1 already made the loser's data durable
   and content-addressed, no data movement is needed — only the manifest delta is
   replayed. In v1's single-logical-writer model (Section 13) a rebase is a
   straightforward re-parent (there is no semantic conflict to resolve because
   writers are serialized in intent); the mechanism is nonetheless correct under
   concurrent writers because the CAS guarantees no lost update.
3. Retry Phase 2. Repeat until the CAS succeeds or a retry ceiling is hit.

### 8.5 Checkpoints

Replaying the log from genesis on every read does not scale. Periodically the
writer emits a **CHECKPOINT** commit: a **full-manifest snapshot** (the complete
current file namespace, chunk lists, and placement map) rather than a delta. This is
the Delta Lake checkpoint pattern (Delta writes periodic Parquet checkpoints of the
log; readers load the latest checkpoint plus the tail of newer commits rather than
the whole log).

- **PROPOSED checkpoint interval: every 128 transactions, OR when the
  since-checkpoint delta bytes exceed the last checkpoint's size, whichever comes
  first.** Rationale: 128 bounds reader tail-replay to at most 127 delta commits;
  the byte-ratio trigger prevents a burst of large transactions from making the tail
  expensive even within 128 commits. Tunable per store.
- A checkpoint is itself a commit on the log; it does not require a separate ref.
- Old pre-checkpoint log commits remain reachable (they are ancestors) until log
  compaction (future work) prunes them; readers simply never need to walk past the
  latest checkpoint.
- **Checkpoint size caveat.** A full-manifest checkpoint grows with namespace
  size: at millions of files it becomes a large single encrypted payload,
  re-serialized at every checkpoint interval. The designed escape hatch is
  **sharded (multi-part) checkpoints** — the snapshot split across multiple
  objects, Delta Lake's multi-part checkpoint pattern — so a reader can load
  shards in parallel and a writer can bound per-object size. Sharding is
  deferred; v1 accepts monolithic checkpoints and SHOULD document a practical
  namespace-size ceiling once measured.

### 8.6 Reader model

A reader:

1. Fetches `refs/heads/log` (a small, single-ref fetch).
2. Loads the **latest checkpoint** it can find at or below the tip.
3. Replays the **tail** of transactions from that checkpoint to the tip.
4. Now holds the full current manifest: file namespace + chunk placement.

Snapshot reads fall out of this for free (Section 13.3): a reader that pins a specific log
commit OID sees an immutable, internally-consistent manifest snapshot, because that
commit and all its ancestors (segments, checkpoints) are immutable.

### 8.7 Manifest encryption

The manifest payload is encrypted with `manifest_key = HKDF(master_key,
"git-storage/manifest")` (Section 6.2) using the same AEAD as chunks (Section 6.4). The backend
stores the log as opaque encrypted commits; it learns commit *shape and cadence*
(number and size of transactions) but not the namespace or placement. This residual
metadata leakage (transaction count/timing) is acknowledged, not eliminated, in v1.

---

## 9. Write path

### 9.1 Overview

All bulk data movement is **git protocol only**. The REST API is used solely for
control-plane operations (Section 16). This is forced by the numbers: GitHub's
content-creation cap of **80 requests/min and 500/hr**[^rate-limits] makes an
API-based data plane infeasible — ~500 blob-creation operations per hour would cap
the entire store's write throughput regardless of chunk size. The git push path is
not subject to the content-creation cap (it is bounded instead by the **6
pushes/min/repo** and **2 GB/push** limits[^repo-limits], which are far more
generous per byte).

### 9.2 Steps

For each file (or byte stream) written:

1. **Chunk.** FastCDC with the store's randomized gear seed (Section 5) splits the stream
   into plaintext chunks.
2. **Compress.** zstd each chunk (Section 6.1).
4. **Encrypt.** Derive `chunk_key` and `nonce` (Sections 6.3 and 6.5); AEAD-seal each
   compressed chunk (Section 6.4); compute `chunk_id = H(ciphertext)` (Section 6.6).
4. **Dedup check.** If `chunk_id` is already present in the current manifest (Section 8),
   skip the byte write; record only a manifest reference.
5. **Stage.** Append the new ciphertext chunk to the **local staging segment** (a
   local packfile/tree being assembled), and record its intended tree path (Section 4.3).
6. **Seal & push** when a seal trigger fires (Section 9.4): finalize the staging segment
   into one commit + tree, and perform **one git push** to a target volume,
   creating `refs/segments/<segment-id>` (Section 4.2).
7. **Commit manifest transaction** per the two-phase protocol (Section 8.3): the push in
   step 6 is Phase 1; the log CAS is Phase 2.

### 9.3 Volume selection (placement)

The writer places a new segment into a volume chosen by a placement policy over the
fixed volume set (Section 15):

- **PROPOSED policy:** choose the volume with the most free headroom below its
  volume-full threshold (Section 15.2) that is not currently under compaction. Ties broken
  by lowest volume ID for determinism.
- The **spare slot** (Section 15.5) is excluded from normal placement: ordinary
  writes MUST NOT target it. It is reserved as the compaction destination.
- The writer MUST refuse to seal into a volume whose projected post-push size would
  exceed the volume-full threshold, and MUST refuse the write entirely if no volume
  in the fixed budget can accept the segment (Section 15.3) — this is the budget wall.

### 9.4 Seal triggers

A staging segment is sealed and pushed when **any** of:

- **Size:** packed staging size ≥ segment target (**PROPOSED** 512 MiB), OR
- **Explicit sync:** the caller invokes `sync`, OR
- **Age:** the staging segment has been open longer than a timeout (**PROPOSED**
  default 5 minutes) — bounds data-loss window for the local staging buffer and
  bounds how stale a not-yet-pushed write can be.

### 9.5 Rate-limit governance

On rate-limited backends the writer MUST throttle pushes to stay under the per-repo
push-rate ceiling (GitHub: 6/min[^repo-limits]) and MUST back off on HTTP 429 /
secondary-rate-limit responses with exponential backoff plus jitter. The writer
maintains a token bucket per volume repository reflecting the configured backend's
push-rate limit.

---

## 10. Read path

### 10.1 Order of attempts

To read a file the client resolves file → chunk list from the manifest (Section 8.6), then
for each chunk:

1. **Local content-addressed cache** (Section 10.2). Hit → done.
2. **Partial-clone / promisor blob fetch by OID** (Section 10.3) from the chunk's volume.
3. **Fallback: Git Data Blobs API** (Section 10.4).

`raw.githubusercontent.com` is **explicitly rejected** as a read channel (Section 10.5).

### 10.2 Local cache

The client maintains a local content-addressed cache keyed by chunk ID. Because
chunk IDs are hashes of stored ciphertext, cache integrity is self-verifying:
re-hash on read. Cache eviction is LRU with a configurable size cap. Decryption
happens after cache read (the cache stores ciphertext, matching the backend, so a
compromised cache leaks no plaintext).

### 10.3 Promisor-remote blob fetch (primary miss path)

On cache miss the client fetches the specific chunk blob **by OID** from the volume
using **partial clone / promisor remote** semantics. **GitHub.com supports partial
clone**: `git clone --filter=blob:none` (blobless) demand-fetches blobs from the
promisor remote on access.[^partial-clone] The client keeps a blobless clone (or
treeless, `--filter=tree:0`) of each volume and lets git fetch individual chunk
blobs on demand by OID. This fetches only needed chunks without downloading full
history or all blobs — the essential capability for a store far larger than any one
working set.

**Chunk-ID → git-OID resolution.** The fetch-by-OID step needs the chunk's *git
blob OID*, but the manifest addresses chunks by *chunk ID* (BLAKE3 of ciphertext,
Section 6.6) — these are different hashes. Resolution goes through the segment's
tree: the manifest maps `chunk_id → (volume, segment)` (Section 8.2), and the
segment's tree contains the chunk at the deterministic fanout path
`<aa>/<bb>/<chunk_id>` (Section 4.3). In a blob-filtered clone, **trees and
commits are present locally; only blobs are demand-fetched** — so the client
walks the segment tree locally (no network) to the chunk's path, reads the blob
OID from the tree entry, then fetches exactly that blob from the promisor
remote. The client SHOULD cache `chunk_id → blob-OID` mappings alongside the
chunk cache to skip the tree walk on repeat access.

Caveat (documented): promisor fetch requires the client to be online and the
promisor remote reachable.[^partial-clone] Behavior on non-GitHub backends
(Gitea/Forgejo) is **unverified** and must be tested (Section 18 open-problem 3).

### 10.4 Fallback: Git Data Blobs API

If partial clone is unavailable on a backend, the client MAY fetch a chunk via the
**Git Data / Blobs API**, which serves blobs up to **100 MB**.[^contents-api] Costs:
base64 JSON encoding overhead (~33% inflation) and consumption of the authenticated
REST budget (5,000 req/hr[^rate-limits]). This is a fallback, not the primary path,
precisely because of those costs. The Contents API's 1 MB inline path[^contents-api]
is unsuitable and MUST NOT be relied on for chunks above 1 MB.

### 10.5 Rejected: raw.githubusercontent.com

`raw.githubusercontent.com` MUST NOT be used as a read channel. It is rate-limited
aggressively per IP at roughly **60 requests/hour unauthenticated**, returning HTTP
429 when tripped.[^raw-behavior] Additionally, its **HTTP Range / byte-range
support is unverified** (Section 18 open-problem 4), so range reads of large chunks cannot
be relied upon. The 60/hr/IP ceiling alone disqualifies it for any real workload.

---

## 11. Crash consistency

The two-phase protocol (Section 8.3) plus content addressing (Section 6.6) means **no crash can
corrupt the store**; the worst outcome is orphaned (unreferenced) objects, swept
lazily by compaction. The following table enumerates every crash point.

| # | Crash point | Resulting on-backend state | Why no corruption | What cleans it up |
|---|---|---|---|---|
| C1 | During chunk upload (mid-push, Phase 1) | Partial or zero objects landed in the volume; **no** `refs/segments/...` ref created, so any landed objects are **unreachable** | The manifest (Section 8) references nothing new; readers never see these objects; content addressing means a redo pushes identical bytes | Server-side GC reclaims unreferenced objects; a redo of the segment push is idempotent (Section 8.3 Phase 1) |
| C2 | After segment push, before manifest CAS (between Phase 1 and Phase 2) | Segment fully pushed and **reachable** via `refs/segments/...`, but the log ref does **not** reference it → an **orphaned segment** | The log is the sole source of truth for what is "live"; an unreferenced segment is invisible to readers and harmless | Compaction sweep (Section 12.5) detects segments not referenced by the manifest and retires them; alternatively the writer's redo re-attempts Phase 2 and adopts the already-pushed segment (idempotent) |
| C3 | During manifest CAS (Phase 2 in flight) | Either the CAS **applied atomically** (P2 is atomic) or it did **not**; there is no partial state | P2 is an atomic compare-and-swap by definition (Section 2.1); the ref is old value or new value, never in between | Nothing to clean: if applied, the transaction is committed; if not, the writer sees rejection and retries (Section 8.4). The segment from C2's Phase 1 is at worst orphaned and handled as C2 |
| C4 | After CAS succeeds, before local-cache update | Backend is fully consistent and committed; only the **client's local cache/index** is stale | The authoritative state lives on the backend (log ref); local cache is a performance optimization, never a source of truth | On restart the client re-fetches the log ref (Section 8.6) and rebuilds/refreshes its cache; the committed transaction is already durable |

Invariants that make the table hold:

- **INV-1: The manifest never references data that does not exist.** Guaranteed by
  data-before-manifest ordering (Section 8.3). So a reader replaying the log never
  dereferences a missing chunk.
- **INV-2: Unreferenced data is always safe to delete.** Guaranteed because the
  manifest is the sole liveness authority. So orphans (C1, C2) are collectible with
  no analysis beyond "is it referenced by the current manifest?"
- **INV-3: The commit point is atomic and singular.** Guaranteed by P2 (Section 2.1). So
  there is exactly one linearization point per transaction (C3).

---

## 12. Compaction and garbage collection

### 12.1 Why repo-granularity

Per-object deletion is not a backend primitive (Section 2.3). The only reliable delete is
dropping an entire repository. Therefore GC = **compaction at volume granularity**:
identify a mostly-dead volume, rewrite its still-live chunks into fresh segments in
another volume, commit the manifest change, then delete the old repository whole.

### 12.2 Liveness

A chunk is **live** iff the current manifest (latest committed transaction, Section 8)
references it from some file's chunk list. Deletes and modifications in the logical
namespace drop references; a chunk with zero references is **dead**. A volume's
**dead-ratio** = dead bytes / total bytes in that volume.

### 12.3 Compaction procedure

To compact volume V:

1. Mark V as under-compaction (excluded from placement, Section 9.3).
2. Enumerate V's live chunks from the manifest.
3. Read each live chunk (via read path Section 10) and **re-stage** it into new segments
   targeting the **spare slot** (Section 15.5), or other volumes with headroom if any
   exist. The spare slot guarantees a destination exists even at full budget
   pressure — without it, compaction would need free space at exactly the moment
   the store has none (the chicken-and-egg this design closes).
4. Push the new segments (Phase 1, Section 8.3) — content-addressed, so if a live chunk
   already exists elsewhere it deduplicates and is skipped.
5. **CAS the manifest** (Phase 2) to repoint the moved chunks' placements to their
   new (volume, segment) and to record V's retirement.
6. Only **after** the manifest CAS commits, **delete repository V** whole.
7. Unmark; V's now-empty slot becomes the **new spare slot** (spare designation
   rotates; the volume count stays fixed — this is **not** fleet expansion,
   Section 15.4).

Ordering rationale: the repository is deleted only after the manifest no longer
references any chunk in it, so INV-1 holds throughout — a crash between steps 5 and
6 leaves a retired-in-manifest-but-still-present repository, harmlessly re-deletable
on retry.

### 12.4 Compaction policy — hysteresis (CRITICAL)

Compaction MUST be **lazy** and gated by hysteresis so the system does not churn
repositories. A volume is eligible for compaction only when **all** hold:

- **Dead-ratio > 50%** (**PROPOSED**): more than half the volume is dead. Below
  this, rewriting live data costs more than it reclaims.
- **AND budget pressure exists** (**PROPOSED**): total store utilization ≥ 80% of
  the declared repo budget, OR a write is currently blocked on lack of headroom.
  Absent pressure, dead space is simply tolerated — reclaiming it eagerly buys
  nothing and costs pushes.
- **AND a minimum interval since this volume's last compaction has passed**
  (**PROPOSED**: 24 hours). This is the anti-churn floor.

Rationale for laziness being both correct and polite: high-frequency repository
create/delete churn (a) resembles the **automated bulk-activity / rapid-repo-creation
signatures that trigger abuse detection**[^abuse] (Section 17), and (b) is bad engineering
— every compaction rewrites live bytes, consuming push budget and bandwidth. Lazy
compaction minimizes both. The three-gate hysteresis prevents oscillation (compact →
frees space → falls below pressure → next write repopulates → compact again).

### 12.5 Orphan sweep

Compaction also sweeps **orphaned segments** (C1/C2 in Section 11): segments whose
`refs/segments/...` ref exists but which the manifest does not reference. An orphan
older than a safety window (**PROPOSED** 1 hour, comfortably beyond the max staging
age Section 9.4 so in-flight writes are never misclassified) MAY have its segment ref
deleted, making its objects unreferenced and server-GC-collectible. The manifest
remains the sole liveness authority (INV-2), so this is always safe.

---

## 13. Multi-writer concurrency and consistency model

### 13.1 v1 model: single logical writer, CAS-serialized

The v1 consistency model is **a single logical writer per store**, serialized by
the log-ref CAS (Section 8.3). "Single logical writer" is an *intended* operating mode, not
a lock: the system does not prevent multiple physical processes from writing.

### 13.2 Concurrent writers are SAFE but contended

If two processes write concurrently, the log-ref CAS (P2) guarantees **no lost
updates**: at most one CAS succeeds per tip; the loser rebases and retries (Section 8.4).
So concurrency is **safe** — the store never corrupts and no committed transaction
is silently overwritten. It is merely **contended**: under sustained concurrent
writing, losers repeatedly rebase, wasting work. v1 does not attempt to make
concurrent writers *efficient*, only *correct*.

### 13.3 Read consistency — snapshot reads via pinned log commit

A reader that **pins a specific log commit OID** obtains an **immutable, internally
consistent snapshot for free**. Because that commit and everything it transitively
references (checkpoints, segments, chunks) are immutable, the pinned view cannot
change under the reader; concurrent writers only advance the tip, never rewrite
history. This gives:

- **Snapshot isolation** for any reader that pins a commit at the start of its work
  and reads exclusively against it.
- **Read-committed** semantics for a reader that re-fetches the tip between
  operations (it sees only committed transactions, since only committed transactions
  are on the log).

This elegance is a direct dividend of building on immutable content-addressed
objects plus a single monotonically-advancing ref — the same property Delta Lake's
snapshot reads rely on. v1 exposes pinned-commit reads as the mechanism for
consistent multi-object reads and point-in-time views.

### 13.4 Future work: optimistic conflict resolution

v1 rebases blindly (re-parent + retry). A future version SHOULD adopt **Delta
Lake-style optimistic concurrency**: on rebase, perform a **semantic conflict check**
— two transactions that touch **non-overlapping** file sets (and non-overlapping
volume placements) can **both commit**, the loser simply re-parenting its
independent delta onto the winner without redoing data work. Only genuine conflicts
(same file mutated concurrently) would fail. This turns "safe but contended" into
"safe and mostly concurrent" for disjoint workloads. Deferred because it requires a
carefully specified conflict predicate and testing; v1's blind rebase is correct in
the meantime.

---

## 14. Erasure coding and replication

### 14.1 The correlated-failure-domain principle

Erasure coding (or replication) provides durability only when shards land in
**independently failing** domains. Multiple repositories **within one account /
provider** share a failure domain: one account suspension (Section 17), one provider
outage, or one AUP enforcement action (Section 9 of GitHub's AUP reserves the right to
delete straining repositories[^aup-bandwidth]) takes them **all** down together.

### 14.2 Intra-provider EC is theater — stated honestly

Spreading Reed–Solomon shards across several repositories in the **same** GitHub
account provides **no meaningful durability** against the dominant failure mode
(account/provider-level action). It is theater. v1 MUST NOT market or imply
otherwise. The only honest use of erasure coding is **across independent backends**
(different providers, or a provider plus self-hosted).

### 14.3 v1 vs future

- **v1: single backend + optional whole-store mirror.** The optional mirror is a
  full second copy of the store on an **independent** backend (e.g. self-hosted
  Forgejo mirroring a GitHub store, or vice versa). Mirroring is coarse and honest:
  it survives loss of one entire backend.
- **Future: RS-coding across independent backends.** k-of-n Reed–Solomon with
  shards on n independent backends, tolerating loss of (n−k) whole backends. This is
  the only configuration where erasure coding earns its complexity. Deferred to
  future work.

---

## 15. Repository budget enforcement

### 15.1 Fixed, user-declared budget

The operator declares, **up front**, the fixed set of volume repositories and a
total size budget for the store. This set does **not** grow automatically (Section 1.3,
README commitment). Reconfiguration (adding volumes) is an explicit,
operator-initiated action, never an automatic response to running out of space.

### 15.2 Volume-full threshold

Each volume has a **volume-full threshold** set below the backend's recommended
repo-size ceiling. **PROPOSED** default **4 GiB** on GitHub backends, chosen under
the **< 5 GB strongly recommended**[^aup-largefiles] guidance and the **10 GB
on-disk `.git`**[^repo-limits] recommended max, with headroom for git overhead. On
self-hosted backends the operator sets it.

### 15.3 The budget wall

When a write would require a segment that no volume in the fixed set can accept
without exceeding its volume-full threshold, **and** compaction (Section 12) cannot free
sufficient space under its hysteresis gates, the system **MUST refuse the write**
with a clear "budget exhausted" error. It MUST NOT create a new repository to absorb
the overflow. This refusal is the mechanism that makes "not unlimited storage" true.

### 15.4 Slot reuse is not fleet expansion

When compaction retires a volume (Section 12.3 step 6), its **declared slot** may be
refilled by a fresh empty repository in the *same* slot. This keeps the *count* of
volumes fixed at the declared budget and is therefore **not** fleet expansion: the
number of live repositories never exceeds the declared budget, and no new slot is
ever conjured to dodge a size limit.

### 15.5 The spare slot — reserved compaction headroom

Of the declared volume set, **one slot is reserved as the spare**: it receives no
ordinary writes (Section 9.3) and exists solely as the destination for compaction
re-staging (Section 12.3 step 3). This is the SSD over-provisioning pattern applied
at volume granularity, and it resolves a chicken-and-egg in the naive design:
compaction triggers under budget pressure (Section 12.4), which is precisely when
no ordinary volume has headroom to receive the live chunks being rescued.

- The spare counts **within** the declared budget (a store declared with N volumes
  has N−1 writable volumes plus 1 spare) — reserving it never increases repo count.
- After a compaction completes, the spare designation **rotates**: the just-emptied
  slot becomes the new spare (Section 12.3 step 7).
- Effective capacity is therefore (N−1) × volume-full threshold; sizing guidance
  MUST state this so operators are not surprised.
- **PROPOSED minimum:** stores with N < 3 volumes run without a spare (compaction
  simply requires organic headroom); the spare becomes mandatory at N ≥ 3.

---

## 16. Control plane vs data plane

- **Data plane = git protocol only.** All chunk/segment bytes move via `git push`
  (write) and partial-clone promisor fetch (read). This avoids the content-creation
  API caps entirely (Section 9.1).
- **Control plane = REST API (sparingly).** Repository lifecycle (create a volume
  repo in a declared slot, delete a retired volume, set repo visibility) uses the
  REST API. These are rare, operator-driven, and each is one request; they fit
  comfortably within the 5,000 req/hr authenticated budget[^rate-limits] and, being
  content-generating (repo creation), are counted against the 80/min–500/hr
  content-creation cap[^rate-limits] — which is fine precisely because they are
  rare (a fixed budget of volumes is created once, not continuously).

The strict data/control separation is what keeps the design inside the API rate
limits: the high-volume path (data) never touches the API, and the API path (control)
is low-volume by construction.

---

## 17. Failure and abuse-signature avoidance

This section is a design constraint, not an evasion strategy. The system's behavior
MUST NOT resemble abuse signatures, because (a) doing so risks automated suspension
that would take the whole store down (Section 14.1), and (b) the patterns that trigger
abuse detection are the same patterns that are bad engineering.

- **No high-frequency repo create/delete.** Automated rapid/bulk repository creation
  is a documented trigger for automated spam/abuse flagging.[^abuse] The fixed
  volume budget (Section 15) and lazy, hysteresis-gated compaction (Section 12.4) together ensure
  repository lifecycle events are rare.
- **No excessive automated bulk pushing.** The writer throttles under the per-repo
  push-rate ceiling (Section 9.5). GitHub's AUP Section 4 targets "excessive automated bulk
  activity" and "undue burden on our servers";[^aup-spam] the design stays well
  under the documented per-repo push rate.
- **No scraping channel.** The read path never scrapes `raw.githubusercontent.com`
  (Section 10.5); it uses git protocol / the sanctioned API.
- **Bandwidth restraint.** GitHub's AUP Section 9 reserves the right to throttle or delete
  repositories placing "undue strain."[^aup-bandwidth] The design's amortization
  (large chunks, batched pushes, local cache) minimizes bandwidth per useful byte.

The honest framing: on hosted backends the design operates at **modest scale within
documented limits**. Its correctness does not depend on exceeding them, and it
refuses to add features whose purpose is to.

---

## 18. Open problems

These are genuinely unresolved in v1 and are called out for future work.

1. **Manifest-log contention under multi-writer.** v1's blind-rebase (Sections 8.4 and 13.2)
   is correct but degrades under sustained concurrent writing (losers repeatedly
   redo manifest work). The optimistic-conflict-check design (Section 13.4) is sketched but
   not specified in detail; the conflict predicate needs formalization and testing.

2. **Compaction policy tuning under a fixed repo budget — the budget-wall-mid-write
   hazard.** The hysteresis thresholds (Section 12.4: 50% dead-ratio, 80% pressure, 24 h
   interval) are proposed, not validated. The spare slot (Section 15.5) guarantees
   compaction always has a *destination*, but a pathological sequence — store near
   budget, write arrives, compaction blocked by its own interval gate — can still
   wall a write that "should" have been serviceable. Tuning the three gates (and
   the interval gate's interaction with blocked writes) against real workloads is
   unresolved.

3. **Partial-clone / promisor blob-fetch behavior on non-GitHub backends is
   unverified.** GitHub.com partial clone is verified.[^partial-clone] Gitea/Forgejo
   promisor-remote blob-on-demand behavior is **unverified** and must be tested; the
   read path's primary miss strategy (Section 10.3) may need a backend-specific fallback.

4. **Raw byte-range request support is unverified.** Whether any backend's raw
   endpoint honors HTTP `Range:`/206 is **unverified**[^raw-behavior]; this
   forecloses an otherwise-attractive "fetch a sub-range of a large chunk" read
   optimization. v1 does not rely on it (Section 10.5), but confirming/refuting it would
   inform future read-path design.

5. **Segment size vs push-cap headroom tradeoff.** The 2 GB push cap[^repo-limits]
   bounds segment size, but the safe operating point is unclear: larger segments
   amortize push overhead better but risk hitting the cap (or a transient near-cap
   failure) and increase the blast radius of a failed push. The proposed 512 MiB
   target (Sections 3.2 and 9.4) is conservative; the optimal point under real latency/failure
   distributions is unresolved.

6. **Index/log repository growth — the forced-update problem.** The log repo grows
   monotonically: every transaction and every checkpoint is a new commit, and
   checkpoints do not reclaim history (pre-checkpoint commits remain ancestors,
   Section 8.5). Over a long-lived store the index repo's size becomes a liability of
   its own. The only way to *reclaim* log history is to re-root `refs/heads/log`
   onto a fresh checkpoint commit with no ancestry — but that is a
   **non-fast-forward forced ref update**, which violates the CAS/fast-forward
   model this entire design rests on (Section 2.1): a concurrent writer's CAS could
   race the re-root, and any reader pinned to a pre-re-root commit (Section 13.3)
   would find its snapshot's ancestry unreachable and subject to server GC. A safe
   log-pruning protocol (epoch/generation scheme, grace periods for pinned
   readers, or index-repo rotation treated like volume compaction) is unspecified
   and is required future spec work before long-lived stores are viable.

---

## 19. Appendix A — Platform constraints as design inputs

Every row is a **[V]**-verified limit from the research doc (verbatim official
GitHub documentation). Unverified items are excluded from load-bearing use.

| Platform constraint | Value | Source | How it shaped the design |
|---|---|---|---|
| Per-file push hard block | **100 MiB** (warns at 50 MiB) | [About large files][^aup-largefiles] | Max chunk size 4 MiB (Section 5.2) sits far under this; chunks never approach the block. |
| Repository size guidance | **< 5 GB strongly recommended, < 1 GB ideal**; 10 GB `.git` on-disk max | [About large files][^aup-largefiles], [Repo limits][^repo-limits] | Volume-full threshold 4 GiB (Section 15.2); volume = unit of GC so a volume stays small. |
| Entries per directory | **3,000** | [Repo limits][^repo-limits] | Two-level hex fanout (Section 4.3) keeps every tree directory far under 3,000 entries. |
| Directory depth | **50** | [Repo limits][^repo-limits] | Fanout depth is 3 (Section 4.3), trivially within 50. |
| Push size | **2 GB enforced** | [Repo limits][^repo-limits] | Segment = one push; segment target 512 MiB with headroom under 2 GB (Section 3.2; Section 18 problem 5). |
| Push rate | **6 pushes/min/repo (recommended max)** | [Repo limits][^repo-limits] | Per-volume push token bucket (Section 9.5); large chunks/segments keep pushes/GiB low (Section 5.3). |
| Git read ops | **15 ops/sec/repo (recommended max)** | [Repo limits][^repo-limits] | Read path prefers local cache + batched promisor fetch (Section 10) to stay under read rate. |
| Repos per account/org | **100,000 max** (banner at 50,000) | [Repo limits][^repo-limits] | Fixed volume budget (Section 15); no fleet expansion means repo count is tiny and bounded. |
| REST content-creation cap | **80/min and 500/hr** | [REST rate limits][^rate-limits] | Data plane is git-only (Sections 9.1 and 16); API never carries chunk bytes. |
| Authenticated REST rate | **5,000 req/hr** | [REST rate limits][^rate-limits] | Control plane (Section 16) is low-volume and fits comfortably; blobs-API fallback (Section 10.4) is budgeted against this. |
| Git Data Blobs API object size | **up to 100 MB** | [Contents API changelog][^contents-api] | Fallback read path (Section 10.4) works for any chunk (≤4 MiB); Contents API 1 MB inline path avoided. |
| Partial clone supported on GitHub.com | **yes** (`--filter=blob:none`) | [Partial clone][^partial-clone] | Primary read miss path fetches chunks by OID on demand (Section 10.3). |
| `raw.githubusercontent.com` | **~60 req/hr/IP**, 429 on trip | [raw behavior][^raw-behavior] | Rejected as a read channel (Section 10.5). |
| AUP Section 9 excessive bandwidth | reserves right to throttle/**delete repos** | [AUP][^aup-bandwidth] | Bandwidth amortization + modest-scale posture (Section 17); mirror across independent backends (Section 14). |
| AUP Section 4 bulk-activity | prohibits "excessive automated bulk activity" | [AUP][^aup-spam] | Lazy compaction + throttled pushes avoid abuse signatures (Sections 12.4 and 17). |
| "Git is not a backup tool" | official guidance | [About large files][^aup-largefiles] | Primary target is self-hosted; "not a backup product" non-goal (Section 1.3). |

---

## 20. Appendix B — Implementation language analysis

**The implementation language is decided: Rust** (locked in 2026-07-16). The
analysis below is the basis for that decision and is retained as its record.

### 20.1 The workload's real shape

The security- and correctness-critical surface of this system is:

- **Parsing untrusted remote bytes** — packfiles fetched from the backend, encrypted
  manifests, and segment trees. These arrive from a network peer and MUST be parsed
  defensively.
- **Concurrency** — staging, pushing, promisor fetches, and CAS retries run
  concurrently.
- **Crypto composition** — HMAC key derivation, AEAD sealing, hashing, nonce
  derivation composed correctly (Section 6).

This is a **memory-safety-critical** surface: a parser bug or a mistake in key
handling is a CVE, exactly where it hurts most.

### 20.2 Chunking is NOT the bottleneck (the throughput math)

FastCDC runs at **~1–3 GB/s single-threaded** in C, C++, or Rust alike. The data
plane is **network-bound at tens of MB/s** (backend latency + rate limits, Section 2.4).
That is roughly a **100× headroom** between chunker speed and network speed. The
consequence: **a faster-language chunker buys nothing measurable** — the chunker is
never the constraint. C/C++'s traditional performance argument therefore purchases
no observable throughput here, while adding memory-unsafe surface precisely on the
untrusted-parser + key-handling path (Section 20.1).

### 20.3 Compression is a linked C library, language-neutral

Compression (zstd) is a **linked, mature C library** (Section 6.1), never hand-written. Its
performance and safety are identical regardless of the host language; it is not a
differentiator.

### 20.4 Decision: Rust

**Rust.**

- **No GC**, explicit memory layout, predictable performance for the data plane.
- **Memory safety** on the untrusted-parser + crypto surface — the property that
  actually matters here (Section 20.1).
- **SIMD available** for hashing/chunking if ever needed (it will not be the
  bottleneck, Section 20.2, but it is there).
- **Ecosystem fit:** `gitoxide` for in-process packfile/tree construction (no
  shelling out to `git`), `RustCrypto` (HMAC, AEAD, HKDF), `blake3`, `zstd`
  bindings, `clap` for CLI, producing a **static single-binary CLI**.

### 20.5 Alternatives considered

- **Go + git CLI** — fastest path to a prototype; GC pauses are irrelevant at
  network speed; but shelling out to `git` weakens low-level control over packfile
  construction and error handling. Viable for a prototype, weaker for the shipping
  data plane.
- **C / C++** — **rejected.** Puts an unsafe-by-default language on the
  untrusted-parsing surface, buys no measurable performance (Section 20.2), and forces
  hand-rolling ecosystem pieces Rust provides safely.
- **Chunker-in-C hybrid** (Rust/Go orchestration calling a C chunker) —
  **unwarranted.** The throughput math (Section 20.2) shows the chunker is not the
  bottleneck, so a C chunker adds FFI complexity and unsafe surface for zero gain.

**Decision: Rust.** Locked in as of 2026-07-16.

---

## 21. Appendix C — Prior-art delta

What THIS design does differently, tool by tool.

- **bup.** bup stores deduplicated content in its own git-packfile-format backend,
  typically on storage you control, and is not built around a hosted git provider's
  limits or a transactional manifest. This design takes bup's git-packfile-shaped,
  content-hashed storage idea but adds a **CAS-serialized transaction log with
  snapshot reads**, **client-side keyed-convergent encryption**, and
  **repo-granularity GC under a fixed multi-volume budget over any git host**.

- **git-annex.** git-annex keeps large content out of the repo and points to
  pluggable special remotes, but its metadata model is git-branch-based
  bookkeeping, not a serialized transaction log with checkpoints, and it does not
  provide snapshot-isolated reads or repo-granularity compaction under a budget.
  This design takes git-annex's multi-backend/pluggable-remote abstraction and
  replaces the bookkeeping with a **Delta-Lake-style transaction log**. *(The
  one-line answer to "why not git-annex with a GitHub remote?": a transactional
  manifest log with snapshot reads, plus repo-granularity GC and fixed-budget
  multi-volume placement, over any git host.)*

- **restic / borg.** restic and borg do content-defined chunking and dedup
  excellently, and borg's per-repo randomized chunker seed directly inspires Section 5.4.
  But their repository format targets a filesystem/object-store backend and a single
  repo; they do not model **git-host limits**, **CAS-serialized multi-writer safety
  on a git ref**, or **repo-granularity GC across a fixed volume fleet**. This design
  takes their chunking/dedup/encryption discipline and re-hosts it on **git's two
  primitives** with a git-native transaction log.

- **Delta Lake / Apache Iceberg.** Delta/Iceberg build a transaction log with
  periodic checkpoints over immutable object storage (S3), giving snapshot reads and
  optimistic concurrency. This design takes that exact pattern but maps it onto
  **git's compare-and-swap ref** instead of S3 conditional writes, and adds the
  **chunking + client-side encryption + repo-granularity GC** layers that a table
  format does not have. The manifest log (Section 8), checkpoints (Section 8.5), snapshot reads
  (Section 13.3), and future optimistic concurrency (Section 13.4) are the direct inheritance.

---

## Footnotes / citations

[^aup-largefiles]: About large files on GitHub — "Git is not designed to serve as a
backup tool"; repo-size guidance (< 1 GB ideal, < 5 GB strongly recommended); file
size 50 MiB warn / 100 MiB block.
https://docs.github.com/en/repositories/working-with-files/managing-large-files/about-large-files-on-github

[^repo-limits]: Repository limits — on-disk `.git` 10 GB; 3,000 entries/dir; depth
50; 5,000 branches; push size 2 GB enforced; single object 1 MB recommended /
100 MB enforced; git read 15 ops/sec/repo; push 6/min/repo; 100,000 repos/account.
https://docs.github.com/en/repositories/creating-and-managing-repositories/repository-limits

[^rate-limits]: REST API rate limits — unauth 60/hr, authenticated 5,000/hr;
secondary limits incl. 100 concurrent, 900 points/min/endpoint, and content
creation **80/min and 500/hr**.
https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api

[^contents-api]: Contents API 1 MB inline limit (raised-behavior changelog);
Git Data Blobs API serves blobs up to 100 MB.
https://github.blog/changelog/2022-05-03-increased-file-size-limit-when-retrieving-file-contents-via-rest-api/

[^partial-clone]: Partial clone and shallow clone — GitHub.com supports partial
clone; `--filter=blob:none` demand-fetches blobs from the promisor remote.
https://github.blog/open-source/git/get-up-to-speed-with-partial-clone-and-shallow-clone/

[^raw-behavior]: `raw.githubusercontent.com` served via CDN, rate-limited per IP
(~60 req/hr unauthenticated, HTTP 429 on trip); HTTP Range support unverified.
(Research doc section 2g — [P]/[U].)

[^aup-bandwidth]: GitHub Acceptable Use Policies Section 9 "Excessive Bandwidth Use" —
reserves right to suspend, throttle, limit, and (after advance notice) delete
straining repositories.
https://docs.github.com/en/site-policy/acceptable-use-policies/github-acceptable-use-policies

[^aup-spam]: GitHub Acceptable Use Policies Section 4 "Spam and Inauthentic Activity" —
prohibits excessive automated bulk activity / undue burden on servers.
https://docs.github.com/en/site-policy/acceptable-use-policies/github-acceptable-use-policies

[^abuse]: Enforcement vector — automated spam/abuse detection flags rapid/bulk
repository creation and high-volume automation; realistic suspension path is
automated flagging, not hand-written notice. (Research doc section 3c — [P].)
