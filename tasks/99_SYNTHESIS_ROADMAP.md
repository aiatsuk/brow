# Task 99 — Synthesize the evidence-weighted development roadmap

## Start condition

Do not start until the coordinator confirms Wave A is closed. Task 99 is not independent.

## Objective

Merge available `results/01__*.md` through `results/24__*.md` into one coherent roadmap. Resolve conflicts by source quality and current repository evidence. Do not conduct a new broad research pass.

## Required local inputs

- `00_MASTER.md`
- every available result `01` through `24` under `results/`
- `shared/` conflict/source ledgers if the coordinator created them
- `README.md`
- `docs/research/01-SUMMARY-RU.md`

## Required questions

1. What is the single recommended product direction and primary user?
2. Which claims are publishable now, after which milestones, or prohibited?
3. What must ship in Wave 0 correctness, Wave 1 evidence/reliability, Wave 2 differentiation, and Wave 3 adoption?
4. What are the dependency-critical paths and which packages can run concurrently?
5. Which researched features should be WONT-NOW?
6. Define exactly five portfolio-level metrics. For each specify measurement method, baseline, continue threshold, pivot threshold, stop threshold, review cadence, and governed roadmap directions.
7. Which conflicts remain unresolved and what is the smallest experiment to resolve each?

## Task-specific deliverables

- Final one-page strategy.
- Current-state truth table.
- Dependency graph for all accepted work packages.
- Four-wave roadmap with outcomes, deliverables, acceptance criteria, sizes, owners-by-role, and exit gates.
- 30/60/90-day plan for one engineer and parallel plan for two engineers.
- Public positioning/landing brief.
- Risk register and WONT-NOW list.
- Source index deduplicated across workers.

## Merge rules

1. Current code/tests outrank README prose.
2. Reproduced local experiments outrank external analogy.
3. Official primary sources outrank summaries.
4. A newer verified source outranks an older one unless the environments differ.
5. Unresolved conflict remains `CONFLICT`; do not average claims.
6. Do not count duplicate recommendations from several workers as independent evidence.

## Task-specific acceptance criteria

- Every accepted milestone has a measurable user outcome and technical exit gate.
- No task is scheduled before its prerequisites.
- Maximize safe Wave 0 parallelism. If fewer than 50% of packages can run concurrently, preserve evidence-backed dependencies and explain the blocking chain rather than restructuring work to hit a quota.
- The critical path is explicit.
- The final roadmap contains no unsupported security, uniqueness, persistence, or compatibility claim.
- Result maximum: 6,000 words.
