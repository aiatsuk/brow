# Task 18 — Benchmarks, evals, compatibility, and proof corpus

## Objective

Define a reproducible evaluation suite that proves `brow`'s claimed correctness, efficiency, and bounded failure behavior against itself over time and, where fair, against alternatives.

Work in two explicit phases: Phase A defines the harness, metadata schema, fairness/denominator rules, and current shipped baseline in parallel. Phase B defines feature-specific fixtures and regression budgets only as candidate contracts to be canonicalized after Tasks 05–24.

## Required local inputs

- `README.md`
- `Cargo.toml`
- `src/cdp/`
- `src/page/`
- `src/daemon.rs`
- `src/jobs.rs`
- `src/redact.rs`
- all files under `tests/`
- `docs/research/00-README.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Official benchmark/evaluation documentation from Playwright, Browser Use benchmark materials, WebArena/BrowserGym or other primary browser-agent benchmark sources where applicable. Use tool-neutral tasks; do not copy unreviewed benchmark data.

## Required questions

1. Define separate suites for protocol correctness, action fidelity, snapshot fidelity, security boundaries, concurrency, performance, and agent task success.
2. Define fixtures for overlay, moving target, stale refs, closed shadow, same-process iframe, OOPIF, SPA navigation, secrets, egress, crash, and long pages.
3. Define metrics: cold/warm latency, output bytes/tokens, success, false action, refusal quality, memory, CPU, dropped events/frames, drift.
4. Define Chrome stable/next compatibility matrix and metadata.
5. Define fair comparison rules and prohibited cherry-picking: same fixture, initial state, browser/version where supported, timeout, retry count, output budget, and safety mode; unsupported capabilities remain `UNSUPPORTED` in the denominator and non-default flags are disclosed.
6. Define regression budgets and CI cadence.
7. Identify where no benchmark can prove the security claim and adversarial tests are required.

## Task-specific deliverables

- Eval corpus specification.
- Metrics dictionary and result schema.
- CI matrix and regression thresholds.
- Public benchmark-report template.
- Separate Phase A and Phase B implementation packages and dependencies.

## Task-specific acceptance criteria

- Every result records commit, OS, browser product/version, headed/headless, hardware class, and config.
- Failures and unsupported cases remain in published denominators.
- Performance and correctness are not collapsed into one score.
- External comparisons run equivalent tasks and state all capability differences.
- The specification is detailed enough for another engineer to implement the commands, result schema, and fixture corpus without making new product decisions.
