# Task 06 — Per-session concurrency and resource scheduler

## Objective

Remove cross-session head-of-line blocking and define resource-aware concurrency that respects Chromium's one-visible-page-per-window behavior and high per-session memory cost.

## Required local inputs

- `src/daemon.rs`
- `src/jobs.rs`
- `src/cdp/conn.rs`
- `src/cdp/transport.rs`
- `tests/daemon_e2e.rs`
- `tests/jobs_e2e.rs`
- `docs/research/01-SUMMARY-RU.md`
- `docs/research/70-daemon-jobs-and-service.md`

## Required external research

Primary sources for Tokio synchronization/cancellation and Chromium background/visibility behavior only where existing research leaves a gap.

## Required questions

1. Prove whether the global `Arc<Mutex<Daemon>>` is held across `dispatch().await` and enumerate blocked operations.
2. Design registry, session handle, page operation, job handle, and shutdown lock ownership.
3. Define close-vs-command, daemon-stop-vs-job, and browser-crash transitions.
4. Replace or bound all browser-fed unbounded queues without deadlocking CDP responses.
5. Define admission control with pluggable workload cost classes for base, recording, crawl, and trace. Recording weights remain explicitly provisional pending Tasks 12 and 18.
6. Define fairness and observability: queue time, active operation, drops, saturation reason.

## Task-specific deliverables

- Lock hierarchy and state machines.
- Scheduler policy with defaults and override boundaries.
- Concurrency/soak test plan.
- Migration sequence that preserves the IPC contract.

## Task-specific acceptance criteria

- Propose a benchmark with at least 30 paired runs on recorded hardware/browser/config. Baseline is session-B `status` plus snapshot latency while A is idle; the future pass target is p95 added latency no greater than 250 ms while A runs a five-second action.
- Close and shutdown races terminate without deadlock or orphaned browser.
- Define a post-Task-12 integration test in which two concurrent recordings both receive non-empty frames; recording implementation is not part of Task 06.
- Saturation returns a structured, actionable response rather than waiting unboundedly.
