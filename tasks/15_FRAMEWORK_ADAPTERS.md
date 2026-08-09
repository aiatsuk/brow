# Task 15 — Framework detection and bounded route adapters

## Objective

Decide which framework-specific features are feasible, valuable, and compatible with the product boundary. Separate cheap detection from route/component extraction and reject impossible marketing claims.

## Required local inputs

- `src/page/mod.rs`
- `src/page/tree.rs`
- `docs/research/30-unified-page-tree.md`
- `docs/research/90-crawler-and-site-graph.md`
- `docs/research/95-prior-art-and-agent-integration.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Official current docs/source for React, Next.js App Router, Vue/Nuxt, Angular, Svelte/SvelteKit, and React Router only where existing measured findings may have changed.

## Required questions

1. For each framework, separate detection, component names, source locations, route declarations, runtime route state, and mutation hooks.
2. State what works in development only, production only, both, or neither.
3. Explain isolated-world versus fixed main-world probe constraints.
4. Rank adapters by achievable precision/recall and maintenance cost.
5. Define a typed fixed-probe interface; no agent-supplied JavaScript.
6. Decide whether adapters belong before or after site graph MVP.

## Task-specific deliverables

- Framework capability matrix.
- Recommend up to two adapters that meet the evidence thresholds; if fewer qualify, return `WONT-NOW` for the remainder and name the missing evidence.
- Probe schema, threat boundary, and version-drift tests.
- Benchmark fixture corpus and precision/recall thresholds.

## Task-specific acceptance criteria

- Do not claim production component/source extraction without evidence.
- Next App Router and production Angular limitations are explicit unless newly disproved.
- Every main-world probe is a fixed harness-owned string and labeled page-observable/tamperable.
- WONT-NOW is acceptable and preferred over low-confidence breadth.
