# Task 03 — Target users, jobs, and adoption wedges

## Objective

Define who should adopt `brow` first and which jobs justify switching from existing browser automation. Convert technical strengths into falsifiable user outcomes.

## Required local inputs

- `README.md`
- `skills/browser/SKILL.md`
- `docs/research/01-SUMMARY-RU.md`

## Required external research

Use primary-source issues, discussions, docs, and product pages from Playwright, Chrome DevTools MCP, agent-browser, gsd-browser, Stagehand, and Browser Use. For each product use at least one maintainer-confirmed issue/discussion or official limitation from the last 24 months, unless explicitly labeled historical. Use at most three evidence items per pain category. Search for recurring pain around browser startup cost, login persistence, token-heavy page representation, flaky selectors, debugging artifacts, local privacy, approvals, and long-running agent work.

## Required questions

1. Define 4–6 candidate user segments.
2. For each segment, list trigger, current workaround, switching barrier, unacceptable failure, and proof required.
3. Identify the narrowest beachhead where existing shipped behavior already solves a painful job.
4. Which jobs require missing capabilities and must not be marketed yet?
5. Define adoption signals and rejection signals for each segment.
6. Propose 8–12 concrete jobs-to-be-done written as “When…, I need…, so that…”.

## Task-specific deliverables

- Segment ranking table.
- Jobs-to-be-done catalog.
- Shipped-now / next / later use-case map.
- Five customer-discovery experiments that do not require building major features.

## Task-specific acceptance criteria

- Every segment links to observed external evidence or is labeled `PROPOSAL`.
- Do not invent quotes or customer counts.
- Recommend one primary segment and one secondary segment, with explicit tradeoff.
