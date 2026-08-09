# Task 07 — Daemon restart, browser supervisor, and resume semantics

## Objective

Make an explicit product and architecture decision: keep honest interruption on daemon restart, or build a per-browser supervisor and resynchronization protocol. Do not assume “persistent” means crash persistence.

## Required local inputs

- `src/browser/launch.rs`
- `src/cdp/transport.rs`
- `src/daemon.rs`
- `src/jobs.rs`
- `tests/daemon_e2e.rs`
- `tests/jobs_e2e.rs`
- `docs/research/10-cdp-transport-and-process.md`
- `docs/research/70-daemon-jobs-and-service.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Official macOS launchd and Linux systemd process-lifecycle documentation relevant to descendants, cgroups, and user services.

## Required questions

1. Enumerate hypothesized user outcomes and complexity. Label user value `GAP` unless cited primary user evidence supports it; specify the smallest interview or usage-measurement experiment needed to quantify it.
2. Compare options: current interruption, replay-from-root, detached supervisor, OS service per browser.
3. If supervisor is chosen, specify ownership of pipe FDs, authentication, reconnection, target resync, event gaps, profile locks, and shutdown.
4. State the recovery capabilities and evidence required from the durable job store; do not design its schema or leases.
5. Define platform-specific lifecycle behavior and unsupported combinations.
6. Identify evidence needed before implementation.

## Task-specific deliverables

- ADR with one recommendation and rejected alternatives.
- Resync protocol sketch if applicable.
- Failure matrix for daemon/browser/supervisor/host crash.
- Phased proof-of-concept plan.

## Task-specific acceptance criteria

- No state is called resumable unless an end-to-end recovery invariant is defined.
- On the next authoritative read after a specified bounded failure-detection/recovery interval, a dead executor is not reported as `running`.
- Event loss and stale DOM/ref state are surfaced explicitly.
- Recommendation includes a WONT-NOW option if cost exceeds validated demand.
