# Task 11 — Strict egress, privacy, and artifact redaction

## Objective

Define an enforceable network boundary and end-to-end secret-handling model. Replace marketing-level “no telemetry” assumptions with testable guarantees and precisely scoped claims.

## Required local inputs

- `src/browser/launch.rs`
- `src/ipc.rs`
- `src/daemon.rs`
- `src/jobs.rs`
- `src/page/events.rs`
- `src/redact.rs`
- `src/page/capture.rs`
- `src/paths.rs`
- `tests/observe_e2e.rs`
- `README.md`
- `docs/research/80-security-capabilities-and-policy.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Official Chromium proxy, Fetch, service-worker, WebSocket, DNS, and Speculation Rules documentation where needed. Verify relevant Rust proxy/TLS libraries from official repos and licenses if recommending one.

## Required questions

1. Define threat actors and protected assets: page, local process, browser background service, agent, stored artifact.
2. Specify per-context proxy allowlisting, CONNECT handling, redirects, IP literals, DNS rebinding, WebSocket, speculation, workers, and Chrome background traffic.
3. Define failure semantics so a denied request cannot look like successful opaque response.
4. Define capture-time redaction for headers, URLs, bodies, console, DOM text, screenshots/video, approval evidence, and exported bundles.
5. State what cannot be reliably redacted and what user controls are required.
6. Define auditable claims replacing ambiguous “no telemetry.”

## Task-specific deliverables

- Network enforcement architecture.
- Exfiltration adversarial suite.
- Data classification and redaction matrix.
- Public claim wording and non-goals.

## Task-specific acceptance criteria

- Denied HTTP, HTTPS, WS/WSS, speculation, worker, and redirect traffic produces zero fixture-server hits.
- Every denial is recorded without secret material.
- Visual evidence has an explicit PII policy; no blanket “safe screenshot” claim.
- Default and strict modes are named so users cannot confuse best-effort flags with enforced egress.
