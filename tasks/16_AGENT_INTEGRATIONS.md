# Task 16 — Agent integrations: Skill, CLI, MCP, and workflow recipes

## Objective

Define adoption surfaces without duplicating browser semantics or widening privileges. Keep CLI + Skill primary unless evidence supports another order.

## Required local inputs

- `src/cli.rs`
- `src/ipc.rs`
- `src/main.rs`
- `src/client.rs`
- `src/daemon.rs`
- `tests/daemon_e2e.rs`
- `tests/jobs_e2e.rs`
- `skills/browser/SKILL.md`
- `README.md`
- `docs/research/95-prior-art-and-agent-integration.md`

## Required external research

- Official Playwright MCP/CLI guidance.
- Current MCP core/tools/tasks specifications and release notes.
- Official integration docs for at least three coding agents where a reusable skill or command can be installed.
- Official agent-browser and gsd-browser integration docs.

## Required questions

1. Which surfaces serve which user: CLI, Skill, MCP stdio, SDK, GitHub Action?
2. What is the minimum installable Skill package and update mechanism?
3. Should MCP expose compact composite tools or one tool per CLI verb?
4. How does each adapter map Task 09's candidate IPC outcomes, Task 10's auth/approval context, and Task 08's artifact handles without changing their semantics?
5. How is semantic parity tested across CLI and MCP?
6. How are schema/token budgets measured?

## Task-specific deliverables

- Surface decision matrix and recommended rollout order.
- Thin MCP adapter contract, if recommended.
- Skill packaging/install plan.

## Task-specific acceptance criteria

- Browser logic remains in one internal application layer.
- MCP never accepts a CDP method name and never invokes the MCP Sampling capability or any embedded/external LLM to execute a `brow` job.
- Destructive action remains human-approved regardless of adapter.
- CLI and adapter return semantically equivalent machine-readable outcomes.
