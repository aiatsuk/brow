# Task 05 — OOPIF target router and unified page tree

## Objective

Specify the smallest correct architecture for cross-process iframe traversal, lifecycle tracking, coordinate translation, ref invalidation, and explicit coverage gaps.

## Required local inputs

- `src/page/mod.rs`
- `src/page/tree.rs`
- `src/cdp/conn.rs`
- `src/daemon.rs`
- `tests/browser_e2e.rs`
- `docs/research/10-cdp-transport-and-process.md`
- `docs/research/30-unified-page-tree.md`
- `docs/research/95-prior-art-and-agent-integration.md`

## Required external research

- Official current CDP `Target`, `Page`, `DOM`, `DOMSnapshot`, and `Accessibility` domain docs.
- Current Chromium protocol definitions where docs are ambiguous.

## Required questions

1. Define the target/session/frame/document-generation data model and ownership.
2. Specify `Target.setAutoAttach` setup and re-arming behavior.
3. Define attach, detach, navigation, crash, and race transitions.
4. Explain the `frameId == targetId` join and when it may be unavailable.
5. Define tree completeness reporting and failure behavior.
6. Define ref identity and invalidation across top-level and child-frame navigation.
7. Define coordinate transforms and hit testing across OOPIF boundaries.

## Task-specific deliverables

- State machine and target-router interface.
- Required CDP command/event allowlist.
- Integration plan by module.
- Two-origin fixture design and race-test matrix.

## Task-specific acceptance criteria

- Cross-origin iframe button appears with accessible name and is clickable by ref.
- Detach/navigation cannot leave a valid-looking stale ref.
- Any unattached or failed frame is an explicit `coverage_gap`, never silent omission.
- Repeat attach/navigate/detach at least 100 times in proposed stress verification.
