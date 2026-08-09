# Task 08 — Durable jobs, event log, and artifact storage

## Objective

Specify crash-consistent job persistence without changing the rule that `browd` never calls a model. Cover job state, decisions, approvals, logs, artifacts, retention, and recovery. The candidate schema must support both current honest-interruption mode and supervisor mode left conditional or selected by Task 07.

## Required local inputs

- `src/jobs.rs`
- `src/daemon.rs`
- `src/paths.rs`
- `tests/jobs_e2e.rs`
- `docs/research/70-daemon-jobs-and-service.md`
- `docs/research/90-crawler-and-site-graph.md`

## Required external research

Official SQLite WAL/transaction documentation and current `rusqlite` documentation. Research another store only if proposing it instead.

## Required questions

1. Define persisted entities and a normalized schema.
2. Define state transitions and atomicity between current state and event append.
3. Define boot IDs, leases, idempotency keys, retry semantics, and recovery.
4. Persist the candidate approval identity, TTL, evidence binding, and replay fields owned semantically by Task 10; do not redefine authorization semantics.
5. Define artifact content addressing, manifests, retention, disk quota, and garbage collection.
6. Define migration/versioning and operator inspection/repair.

## Task-specific deliverables

- Schema with keys, indexes, foreign keys, and transaction boundaries.
- State-transition table including invalid transitions.
- Crash/restart test matrix.
- Retention and privacy policy proposal.

## Task-specific acceptance criteria

- Database state and event history cannot disagree after a committed transition.
- Startup deterministically classifies work owned by a dead boot.
- Repeated control messages are idempotent.
- Database/API references cannot expose another job's artifact without an explicit ownership/share relation; same-UID filesystem access is out of scope.
- A torn log or partial artifact remains detectable and inspectable.
