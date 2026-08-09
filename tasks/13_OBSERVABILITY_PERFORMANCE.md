# Task 13 — Core event/network envelope, clocks, and response bodies

## Objective

Design a bounded, honest core observation system for console, audits, network, WebSocket/SSE, mutations, response bodies, and the canonical logical event/clock envelope consumed by media, tracing, and crawler work.

## Required local inputs

- `src/page/events.rs`
- `src/cdp/conn.rs`
- `src/redact.rs`
- `tests/observe_e2e.rs`
- `docs/research/60-event-streams-console-network-performance.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Official current CDP docs for Runtime, Log, Audits, and Network.

## Required questions

1. Define a normalized event envelope with source clock, normalized time, receipt time, sequence, frame/target/session, and redaction status.
2. Define cross-stream dedup without dropping useful CORS/CSP explanations.
3. Define body capture modes and explicitly document timing-versus-completeness tradeoff.
4. Define WebSocket, native EventSource, fetch-based SSE, redirects, extra info, and worker coverage.
5. Define fixed injected observer boundaries for mutations and browser compatibility canaries. Performance/Vitals/Tracing belong to Task 21.
6. Define logical event-store operations, query/index requirements, bounds, and backpressure. Task 08 owns physical storage, transactions, segmentation, retention, and garbage collection.

## Task-specific deliverables

- Event schema and source matrix.
- Capture-mode matrix.
- Logical export/query requirements consumed by Task 21; do not define HAR/trace format here.
- Completeness and redaction test plan.

## Task-specific acceptance criteria

- Timestamp-less events are marked receipt-time, not falsely precisely ordered.
- Stream gaps and dropped counts are queryable.
- No response body reaches disk before redaction policy is applied.
- Unsupported browser capability degrades explicitly rather than silently returning empty data.
