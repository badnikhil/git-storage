# Using git-storage

Encrypted, deduplicating file storage on top of git repositories. Your files are
chunked, compressed, and encrypted **on your machine** — the git host only ever
sees ciphertext.

## Install

```sh
cargo build --release
# put the binary on your PATH:
ln -sf "$PWD/target/release/git-storage" ~/.local/bin/git-storage
```

## 30 seconds

```sh
git-storage put photo.jpg  --repo ./store --keyfile ./key          # store a file
git-storage get photo.jpg  --output copy.jpg --repo ./store --keyfile ./key   # get it back
```

The first `put` creates the store folder and the keyfile. Every command takes
`--repo` (the store) and `--keyfile`.

> **Keep the keyfile safe.** Lose it and the data is gone — there is no recovery.

## Commands

| Command | What it does |
|---|---|
| `put <file>` | Store a file |
| `get <name> --output <path>` | Reconstruct a file (verifies every byte) |
| `ls` | List stored files |
| `rm <name>` | Delete a file (space reclaimed by `compact`) |
| `stats` | Per-volume usage: live / dead / budget |
| `tip` | Print the current version id (for `--at`) |
| `compact` | Reclaim deleted space |
| `mirror` | Copy the whole store to a second backend |
| `init` | Set up a store spread across multiple repos |

Every command needs `--repo` and `--keyfile`. Add `-h` for flags, e.g.
`git-storage put -h`.

## Recipes

**Read an old version (time travel):**

```sh
V=$(git-storage tip --repo ./store --keyfile ./key)             # remember "now"
# ...make more changes...
git-storage ls  --repo ./store --keyfile ./key --at $V          # list as of $V
git-storage get report.pdf --output old.pdf --repo ./store --keyfile ./key --at $V
```

**Spread across several repos:**

```sh
git-storage init --repo ./store --keyfile ./key \
  --volume v0=file:///srv/git/v0.git \
  --volume v1=file:///srv/git/v1.git \
  --index-url file:///srv/git/index.git
# then put / get / ls exactly as before
```

Volumes can be local (`file://`), a self-hosted Gitea, or GitHub (`https://…`).
For hosted repos, set a token first — it is used once to create the repos and is
never written to disk:

```sh
export GITSTORAGE_TOKEN=$(gh auth token)   # or your own token
```

**Back up to an independent copy:**

```sh
git-storage mirror --repo ./store --keyfile ./key \
  --to-index  file:///backup/index.git \
  --to-volume v0=file:///backup/v0.git
```

## Good to know

- **The keyfile is everything.** Lose it → data lost. Leak it → data exposed.
  Store it somewhere safe and separate.
- **Chunk size is fixed per store.** Set it on the first write with
  `--chunk-size 64k` (or `1m`, …); it can't change later.
- **Storage is bounded.** When your repos are full, writes are refused — it never
  silently grows or creates repos to dodge a host's limits.
- **One file can't be bigger than one repo yet** — see the issue tracker for the
  plan to split large files across repos.
