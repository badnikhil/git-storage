# AGENTS.md — git-storage

Agent instructions for this project. The full, detailed project rules live in
**CLAUDE.md** (same directory) — read it. This file front-loads the one
principle that is easiest to get wrong.

## Testing philosophy (hard rule)

**Every known issue MUST have a test that fails on it.** A green test suite with
open issues means the suite is *inadequate* — it is not exercising the defects we
already know about. If we have an issue and nothing is red, the tests are lying.

Concretely:

- For every open issue — a bug OR a documented limitation — write a test that
  asserts the **desired, correct behavior**. It will fail today. Mark it
  `#[ignore = "issue #N: <one-line>"]` so `cargo test` stays green for what
  actually works, while **`cargo test -- --ignored`** runs the known-failing
  tests that pin every open issue.
- When an issue is fixed, **delete the `#[ignore]`** — the test must then pass,
  and the issue closes. The ignored test *is* the acceptance criterion.
- When today's behavior is a *safe degradation* (e.g. "fails loudly, never
  returns wrong data"), keep **both**: a passing test that locks the safe
  behavior, and the ignored test for the fully-correct behavior.
- Never document a limitation without an `#[ignore]`d test linked to its issue,
  and never open an issue without one. Every open issue links to its test; every
  `#[ignore]`d test names its issue number in the ignore reason.
- The set from `cargo test -- --ignored` should map 1:1 to the open issues that
  are genuine defects/limitations (validation gaps and pure enhancements are
  tracked as issues but need no failing test).

## Other conventions (full list in CLAUDE.md)

- Spell out "Section N" in prose and comments — do not use the section-symbol
  shorthand character.
- Quality gates on every change, all clean: `cargo build`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
- Network git lives ONLY in `src/backend.rs`; tests never touch the user's real
  git config or credentials (use the isolated-env helper).
- `agent-docs/` is local-only — never commit, stage, or push it.
