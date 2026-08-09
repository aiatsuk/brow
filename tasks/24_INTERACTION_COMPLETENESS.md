# Task 24 — Browser interaction completeness and explicit boundaries

## Objective

Define the next interaction primitives needed for real QA/workflows: tabs/popups, JavaScript dialogs, downloads, uploads/file chooser, permissions, device profiles, IME, and HTML5 drag-and-drop—while preserving explicit OS/browser-chrome boundaries.

## Required local inputs

- `src/cli.rs`
- `src/ipc.rs`
- `src/page/input.rs`
- `src/page/mod.rs`
- `src/page/events.rs`
- `src/paths.rs`
- `docs/research/40-input-synthesis.md`

## Required external research

Official current CDP Target, Page, Browser, Input, DOM, and download behavior documentation. Use Playwright/Puppeteer docs only to understand user expectations.

## Required questions

1. Rank each missing primitive by user job, feasibility, privilege, and evidence requirement.
2. Define popup/tab ownership and interaction with session isolation/OOPIF routing.
3. Define safe download paths, filename handling, hashes, limits, and artifact ownership.
4. Define upload/file chooser capability and human approval requirements.
5. Define JavaScript dialog and CDP-controlled permission semantics.
6. Define what remains impossible or out of scope: browser chrome, Keychain, Touch ID, native OS dialogs, CAPTCHA.
7. Define fixture coverage per primitive.

## Task-specific deliverables

- Capability/priority matrix.
- Candidate CLI/IPC verbs and policy requirements.
- Interaction state machines.
- E2E fixture and security test plan.

## Task-specific acceptance criteria

- Every proposed outward/file side effect has capability, audit event, and bounded path/size rules.
- Popups cannot escape session ownership or bypass policy.
- Unsupported native UI is reported as handoff, not automation success.
- Each accepted primitive has a real-Chromium fixture and observable oracle.
