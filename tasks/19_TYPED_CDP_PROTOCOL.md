# Task 19 — Typed generated CDP protocol and provenance

## Objective

Specify pinned Chromium PDL ingestion and generated typed Rust protocol crates while preserving direct pipe transport and keeping raw CDP unrepresentable in agent-facing IPC.

## Required local inputs

- `Cargo.toml`
- `src/cdp/`
- `src/page/`
- `src/browser/launch.rs`
- `src/ipc.rs`
- `docs/research/20-rust-implementation-stack.md`

## Required external research

Official Chromium PDL sources/license headers and current `chromiumoxide` generator architecture/license. Inspect code-generation patterns, not high-level WebSocket runtime behavior.

## Required questions

1. Define workspace boundaries for `cdp-codegen`, `cdp-protocol`, and `xtask`.
2. Define pinned Chromium revision, source retrieval, offline regeneration, and provenance manifest.
3. Define command/event allowlist plus transitive type closure.
4. Define forward compatibility for unknown response fields/events and protocol-version drift.
5. Define incremental migration from `serde_json::Value` without blocking feature work.
6. Define generated-code review, formatting, license headers, and zero-diff CI.

## Task-specific deliverables

- Candidate crate/API layout.
- Generator pipeline and provenance format.
- Current CDP string inventory and migration slices.
- CI and compatibility test plan.

## Task-specific acceptance criteria

- Generated code records exact source revision and license.
- Regeneration is deterministic and zero-diff in CI.
- Protocol errors retain method, code, message, and data.
- Unknown non-breaking fields remain forward-compatible.
- No IPC variant accepts a CDP method string.
