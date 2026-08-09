# Task 14 — State-aware crawler and causal site graph

## Objective

Specify a crawler that maps application states and the actions that caused transitions, not merely URLs. It must be bounded, polite, inspectable, and honest about divergence.

## Required local inputs

- `src/jobs.rs`
- `src/page/mod.rs`
- `src/page/tree.rs`
- `src/page/events.rs`
- `src/daemon.rs`
- `src/paths.rs`
- `docs/research/90-crawler-and-site-graph.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Official Crawlee concepts/docs and primary crawler literature only for unresolved design choices. Do not repeat the existing general prior-art survey.

## Required questions

1. Define graph node identity using URL, normalized visible/interactive tree, frame completeness, storage/auth context, modal/tab state, and uncertainty.
2. Define edge provenance: DOM link, form, click, redirect, declared route, agent answer.
3. Define frontier states, leases, retry classes, dead-letter, budgets, BFS/DFS policy, per-origin concurrency, robots, and politeness.
4. Define destructive-action avoidance and capability requirements.
5. Define replay-from-root divergence measurement before claiming resumability or reproducibility.
6. Evaluate SQLite, JSON, Graphviz, HTML, coverage report, and regression plan; choose one canonical store and at most two MVP exports, then classify the rest `SHOULD/COULD/WONT-NOW`.
7. Define prerequisites and blockers, especially OOPIF and strict egress.

## Task-specific deliverables

- Graph/frontier schema.
- State signature algorithm with normalization and collision discussion.
- Crawl scheduler and budget policy.
- Authenticated CRUD convergence experiment.

## Task-specific acceptance criteria

- Frame state changes affect identity or produce an explicit coverage gap.
- Every edge has causal evidence and artifact pointers.
- Page/depth/time/byte/action budgets stop deterministically.
- Crash returns leased work after TTL; retries may execute, but a defined idempotency/unique-edge key prevents duplicate committed logical edges.
- No reproducibility claim is made before measured replay divergence.
