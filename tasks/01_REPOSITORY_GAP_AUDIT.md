# Task 01 — Repository claim-to-code gap audit

## Objective

Produce the authoritative current-state ledger for `brow`. Determine what is implemented, partially implemented, researched-only, missing, or contradicted. This task prevents the roadmap from scheduling already-finished work or treating research conclusions as shipped behavior.

## Required local inputs

- `README.md`
- `Cargo.toml`
- all files under `src/` and `tests/`
- `docs/research/00-README.md`
- `docs/research/01-SUMMARY-RU.md`
- `git log --oneline -20`

## Required questions

1. Map every README capability and known limit to exact implementation symbols and tests.
2. Identify stale claims, including test counts and risks already fixed after the research snapshot.
3. Find correctness bugs not highlighted in README. Explicitly inspect global daemon locking across `.await`, V8 timeout for mutating eval, session-name/path collisions, approval revalidation, frame coverage reporting, and capture truncation.
4. Record all stringly typed CDP boundaries, unbounded queues, and global mutable state.
5. Produce a gap ledger with `SHIPPED`, `PARTIAL`, `RESEARCHED`, `MISSING`, `CONFLICT` states.
6. Rank the ten highest-leverage next actions by severity and dependency, not novelty.

## External research

Not required. Do not broaden this into competitor research.

## Task-specific deliverables

- Claim-to-code matrix.
- Architecture hotspot list with paths/symbols.
- Test inventory and missing-test matrix.
- Minimal Wave 0 correctness backlog.

## Task-specific acceptance criteria

- Run now: record `git rev-parse --short HEAD` and `git status --short`, then run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib -- --list`, `cargo test --bin brow -- --list`, and `cargo test --tests -- --list`; record exact outcomes.
- Every gap cites code or a missing symbol search.
- Explicitly distinguish old research TODOs already closed in current HEAD.
- Result maximum: 3,500 words.
