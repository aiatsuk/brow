# Task 22 — Deterministic workflow capture, replay, and divergence

## Objective

Design a declarative flow format that lets successful browser work replay without a model and parks with a bounded divergence packet when the page changes. The daemon remains model-free.

## Required local inputs

- `src/jobs.rs`
- `src/ipc.rs`
- `src/page/tree.rs`
- `skills/browser/SKILL.md`
- `docs/research/70-daemon-jobs-and-service.md`
- `docs/research/95-prior-art-and-agent-integration.md`

## Required external research

Official Stagehand caching/self-healing materials, gstack skillification documentation, and browser-harness workflow documentation. Extract patterns only; record licenses and provenance limits.

## Required questions

1. Define a versioned workflow schema with typed actions, preconditions, postconditions, variables, and no JS/CDP.
2. Define capture/normalization from an interactive session.
3. Define deterministic replay and structured divergence categories.
4. Define agent-mediated repair outside the daemon and atomic versioned updates.
5. Define capability inheritance so a workflow cannot exceed its caller.
6. Define secret references without embedding plaintext credentials.
7. Define provenance and tests for repaired versions.

## Task-specific deliverables

- Candidate workflow schema.
- Replay/divergence/repair state machine.
- Storage and capability requirements for Tasks 08/10/13.
- Three end-to-end recipe examples.

## Task-specific acceptance criteria

- Workflow contains no arbitrary JavaScript, CDP, shell, or hidden LLM call.
- Known-page replay invokes no model.
- Drift produces a bounded evidence packet and never guesses a replacement action.
- Repair is atomic, versioned, auditable, and preserves prior provenance.
