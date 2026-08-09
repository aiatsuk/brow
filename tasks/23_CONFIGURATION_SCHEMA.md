# Task 23 — Versioned configuration and effective-policy schema

## Objective

Propose one configuration model for scheduler limits, policy, egress, retention, recording, browser provisioning, and adapters instead of letting each feature invent flags and precedence.

## Required local inputs

- `src/cli.rs`
- `src/paths.rs`
- `src/browser/discover.rs`
- `src/browser/launch.rs`
- `README.md`

## Required external research

Official configuration documentation from two mature Rust CLI tools and two relevant browser-agent tools. Use them as patterns, not authority for `brow` semantics.

## Required questions

1. Define managed, user, project, session/job, environment, and CLI layers.
2. Integrate deny-wins security constraints without allowing lower layers to weaken them.
3. Define schema versioning, migrations, unknown/deprecated keys, and effective-config inspection.
4. Define secret references versus forbidden inline secrets.
5. Define reload/restart semantics and config snapshotting into job/trace provenance.
6. Inventory requirements from scheduler, storage, policy, egress, recording, browser provisioning, and adapters as candidate namespaces.

## Task-specific deliverables

- Candidate TOML schema and precedence algorithm.
- Effective-config output schema.
- Migration/deprecation policy.
- Conflict and security test matrix.

## Task-specific acceptance criteria

- Lower-precedence config cannot weaken a deny or managed maximum.
- Effective config explains value, source layer, and constraint for every non-default setting.
- Unknown security-sensitive keys fail closed; benign unknown keys follow an explicit compatibility rule.
- Job/evidence manifests record the effective non-secret config fingerprint.
