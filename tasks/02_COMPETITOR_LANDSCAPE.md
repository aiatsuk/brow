# Task 02 — Competitor and adjacent-product landscape

## Objective

Build a current, source-backed comparison that identifies where `brow` can win and where parity is required. Research products, their landing pages, official docs, repositories, packaging, and proof mechanisms.

## Required local inputs

- `README.md`
- `docs/research/95-prior-art-and-agent-integration.md`

## Required external targets

At minimum verify current official sources for:

- Playwright and Playwright MCP
- Chrome DevTools MCP
- `browser-use/browser-use` and Browser Use cloud/product site
- Browserbase Stagehand
- `vercel-labs/agent-browser`
- `open-gsd/gsd-browser` and its official product page
- gstack browse
- Puppeteer
- Crawlee
- up to three newer direct analogues: run at least two explicit search queries and stop after the first three official repos/sites matching CLI or MCP browser control for agents

## Required questions

1. What user and job does each product target?
2. Is execution local, cloud, hybrid, or unclear?
3. What persists: browser, profile, job, artifact, or nothing?
4. What observation/input/recording/network/debugging/crawling surfaces exist?
5. What is the trust model, approval model, and raw-protocol exposure?
6. What installation and agent integration surfaces exist: CLI, skill, MCP, SDK, hosted API?
7. What proof appears on the landing page: demo, benchmark, trace, customer, security claim?
8. Which three differentiators for `brow` are defensible now, later, or not at all?

## Task-specific deliverables

- Comparison matrix with these columns: `Product | Target user/job | Local/cloud | Runtime/interface | Persisted state | Observation/input | Evidence | Trust/approval/raw escape | Packaging/integrations | License | Source/access date | Brow implication`.
- Feature-parity shortlist versus deliberate non-goals.
- Three positioning territories, each with supporting and contradicting evidence.
- Source-backed watchlist of fast-moving competitors.

## Task-specific acceptance criteria

- Use official sites/repos/docs only for factual rows.
- Separate repository capabilities from hosted-service capabilities.
- Record license and access date for every open-source target.
- Do not claim uniqueness from absence of evidence.
