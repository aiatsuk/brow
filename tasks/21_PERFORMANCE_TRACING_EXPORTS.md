# Task 21 — Performance observers, tracing, and exports

## Objective

Specify performance diagnostics separately from the core event/network envelope: Web Vitals, injected observers, CDP tracing, clock joins, compatibility canaries, and user-facing exports.

## Required local inputs

- `src/page/events.rs`
- `src/cdp/conn.rs`
- `src/redact.rs`
- `docs/research/60-event-streams-console-network-performance.md`

## Required external research

Official current CDP PerformanceTimeline and Tracing docs, current Web Vitals definitions, and HAR specification/source. Verify browser support rather than inferring it.

## Required questions

1. Which metrics come directly from CDP and which require fixed injected observers?
2. Define support canaries and explicit degradation across Chrome milestones.
3. Define trace categories, compression, bounds, lifecycle, parsing, and joins to Task 13 event IDs.
4. Separate HAR semantics from trace semantics and define incomplete-export markers.
5. Define privacy/redaction requirements for URLs, bodies, stack traces, and DOM-derived metric attribution.
6. Define minimal CLI/product surfaces and avoid a general DevTools clone.

## Task-specific deliverables

- Metric/source/clock matrix.
- Candidate observer and trace lifecycle.
- Export contracts and incompleteness semantics.
- Compatibility and overhead benchmark plan.

## Task-specific acceptance criteria

- Unsupported metrics are labeled unavailable, never emitted as zero.
- Trace bounds and dropped/incomplete state are observable.
- Injected code is fixed and harness-owned, not agent-supplied.
- Exported evidence is joinable to Task 13 identities without redefining its envelope.
