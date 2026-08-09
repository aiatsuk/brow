# Task 09 — IPC v2 streaming, cancellation, and backpressure

## Objective

Design a versioned IPC envelope for server-push progress, job following, cancellation, structured errors, and bounded flow while preserving the closed typed capability surface.

## Required local inputs

- `src/ipc.rs`
- `src/client.rs`
- `src/daemon.rs`
- `src/cdp/conn.rs`
- `src/cdp/transport.rs`
- `docs/research/70-daemon-jobs-and-service.md`

## Required external research

Primary protocol documentation for JSON-RPC/LSP progress and cancellation only as comparison evidence. Do not adopt a generic string method field.

## Required questions

1. Define frames for request, response event, terminal end, cancellation, and protocol error.
2. Define request IDs, ordering, multiplexing, late frames, disconnects, and retries.
3. Define per-stream and global bounds, backpressure, drop rules, and gap reporting.
4. Preserve typed `Request` variants and the test that raw CDP is unrepresentable.
5. Define v1/v2 negotiation and compatibility window.
6. Define CLI behavior for `job logs --follow`, Ctrl-C, and daemon shutdown.

## Task-specific deliverables

- Wire examples and state machine.
- Stable error-code catalog.
- Backpressure budget and overload behavior.
- Migration and test plan.

## Task-specific acceptance criteria

- For a live negotiated connection, each accepted request emits at most one terminal frame. After disconnect/crash, lookup returns a durable terminal outcome or explicit `outcome_unknown`; it never synthesizes success.
- A slow follower cannot grow memory without bound or block CDP response routing.
- Dropped progress is counted and reported.
- No arbitrary protocol method string is exposed to the caller.
