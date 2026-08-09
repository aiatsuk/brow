# Research seed for coordinator

Checked: 2026-08-09. This file is a merge aid, not a substitute for worker verification.

## Current repository corrections

- Baseline commit `2a63892` passed 95 tests. On the coordinator worktree dated 2026-08-09, `BROW_REQUIRE_CHROME=1 cargo test --all-targets` passed 137 tests: 109 unit-test-binary tests and 28 integration-test-binary tests. Re-measure after any worktree change.
- Older README counts (`49`, `95`, or `96`) are stale.
- The coordinator worktree now contains recursive OOPIF routing, V8 evaluation timeout, collision-free session paths, exact-target approval revalidation, per-session concurrency, bounded CDP transport, tiled full-page PNG, atomic job manifests, and lifecycle cleanup. Treat the old gaps as historical and inspect current symbols/tests.
- Still-open boundaries include daemon-restart survival/resume, authenticated human presence, strict egress, transformed inline-frame informational bounds, giant-capture peak memory, transactional multi-record storage, and the product features named in the master contract.

## Primary external sources worth reusing

- [Playwright MCP](https://github.com/microsoft/playwright-mcp)
- [Playwright CLI capabilities](https://playwright.dev/agent-cli/capabilities)
- [Playwright Trace Viewer](https://playwright.dev/docs/trace-viewer)
- [Chrome DevTools for agents](https://developer.chrome.com/docs/devtools/agents)
- [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp)
- [Vercel agent-browser](https://github.com/vercel-labs/agent-browser)
- [gsd-browser product page](https://opengsd.net/products/gsd-browser)
- [gstack](https://github.com/garrytan/gstack)
- [Stagehand](https://github.com/browserbase/stagehand)
- [Browser Use](https://github.com/browser-use/browser-use)
- [Crawlee](https://crawlee.dev/js/docs/introduction)
- [CDP Target domain](https://chromedevtools.github.io/devtools-protocol/tot/Target/)
- [chromiumoxide](https://github.com/mattsse/chromiumoxide)
- [Chrome for Testing](https://github.com/GoogleChromeLabs/chrome-for-testing)
- [MCP release candidate 2026-07-28](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)
- Current MCP specification must be resolved from the official spec index at execution time; do not reuse a dated URL as the current specification without verification.

## Seed synthesis

`INFERENCE` — The cited competitor sources show that the market already has native/Rust CLIs, persistent daemons, refs, sessions, MCP, viewers, policies, recordings, assertions, and cloud backends. `brow` should not compete on command count or “CLI instead of MCP.” The most defensible direction is a constrained local capability runtime with real hit-tested input, a unified DOM/AX/layout identity, explicit agent-decision versus human-approval gates, no raw protocol escape hatch, deterministic work below the model, and causal evidence/site-state provenance.
