# Task 10 — Capability policy, approvals, and auditability

## Objective

Turn the current closed IPC surface into an explicit enforceable capability model. Strengthen human approval so permission is bound to the exact observed target state, including DOM mutations without navigation.

## Required local inputs

- `src/ipc.rs`
- `src/cli.rs`
- `src/daemon.rs`
- `src/jobs.rs`
- `src/paths.rs`
- `src/page/input.rs`
- `src/page/mod.rs`
- `src/page/capture.rs`
- `tests/jobs_e2e.rs`
- `docs/research/80-security-capabilities-and-policy.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Primary specifications or official docs for at least two comparable capability/approval systems. Use competitors only for patterns, not proof of safety.

## Required questions

1. Map every current `Request` and CLI variant. Separately map only these future groups: recording, crawler, policy, artifact export, durable-job control, and adapter control; their exact verbs remain `PROPOSAL`. A verb may require multiple capabilities; record the minimal conjunctive set and deny if any required capability is denied.
2. Define policy layers, precedence, deny-wins semantics, and project/user/managed ownership.
3. Define per-job capability tokens in addition to peer UID.
4. Define approval binding to document generation, target fingerprint, accessible name/role, bounds, action, screenshot evidence, TTL, and fresh pre-action verification.
5. Define agent-decision versus human-approval authorization at the protocol layer.
6. Define audit events without leaking secrets.
7. Enumerate hard-forbidden flags, verbs, and escape hatches.

## Task-specific deliverables

- Verb-to-capability table.
- Policy merge algorithm and sample policies.
- Approval lifecycle and invalidation state machine.
- Adversarial test matrix.

## Task-specific acceptance criteria

- Page mutation without navigation invalidates a changed destructive target.
- An agent-facing request cannot satisfy a human-only gate.
- Policy is enforced below CLI parsing in the daemon/application layer.
- No policy can enable raw CDP, cookie theft from everyday Chrome, or a disabled browser sandbox. Permitted navigation, form submission, download/upload, clipboard write, external-protocol launch, storage mutation, permission grant, and harness-initiated network request require explicit capability and emit an audit event; strict egress is delegated to Task 11.
