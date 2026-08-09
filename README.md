# brow

Local-first browser harness for AI agents. A persistent Chromium driven over the
DevTools Protocol directly — no Playwright, no Puppeteer, no cloud, and no
product telemetry implemented by the `brow` binary.

```text
brow (CLI)  ──unix socket──▶  browd (daemon)  ──CDP pipe──▶  Chromium
```

An agent turn is a one-shot process. A login session, a half-filled form and a
forty-minute crawl are not. `browd` holds the browsers so they outlive whichever
command happens to be running, and `brow` is the small deterministic verb surface
an agent drives them with.

## Status

**Iteration 3.** The core browser flows below are covered by real-Chromium E2E;
supporting branches also have unit/integration coverage. Video recording, the
site graph and framework adapters are not built yet.

## Install

Needs Rust 1.82+ and a Chromium-family browser already installed. `brow` never
downloads a browser.

```bash
cargo install --path .
```

Discovery order: `$BROW_CHROME`, then the usual Chrome / Chromium / Edge / Brave
locations, then `PATH`.

## Use

```bash
brow open example.com               # starts the daemon and a browser if needed
brow snapshot                       # interactive elements, with @node-G-N refs
brow click @node-2-12
brow fill @node-2-14 'someone@example.com'
brow press Enter
brow screenshot --full-page -o page.png
brow eval "document.querySelector('#status').textContent"
brow console --errors               # console output and uncaught exceptions
brow network --failed               # requests, with credentials already stripped
brow close
```

Touch and pointer gestures:

```bash
brow tap @node-2-12
brow long-press @node-2-12 --duration-ms 800
brow swipe --from 320,720 --to 320,160 --duration-ms 450
brow pinch --center 400,400 --scale 1.8
brow drag --from @node-2-31 --to @node-2-40
```

Everything takes `--json` for machine-readable output and `--session NAME` to
keep several browsers with independent profiles. Different sessions execute
concurrently; operations within one session are serialized so navigation,
snapshot and input cannot race one another. Open/close and daemon shutdown are
linearized explicitly.

```bash
brow --session qa open staging.example.com
brow sessions
brow daemon status
brow daemon stop
```

## Background jobs

A job runs a plan in its own browser and keeps going after the command returns.

```bash
brow job start --intent "check the signup flow still works" \
  --step "open staging.example.com/signup" \
  --step 'fill "Email" = qa@example.com' \
  --step 'click "Create account"' \
  --step "screenshot" \
  --step "check-errors"

brow job logs job_0199c… --follow
brow job list
```

**The daemon never calls a language model.** A job is executed by heuristics,
treated as the lower bound on competence rather than as judgement. When the
heuristics are not enough it stops and asks, and *who* it asks depends on the
question:

| Parked as | Question | Who answers |
|---|---|---|
| `needs_decision` | three controls match `"Continue"` — which one? | the **agent**, with `brow job answer <id> <index>` |
| `waiting_for_approval` | the next click says "Delete workspace" | a **human**, with `brow job approve <id>` |

Answering through the wrong verb is refused. The approval verb is intended for a
human operator, but the local socket currently authenticates only the Unix user,
not human presence; an agent running as that user could invoke it. Treat this as
an explicit manual workflow boundary, not an authenticated security boundary.
An approval carries an atomic screenshot and a fresh fingerprint of the exact
selected node: renderer/session identity, tag, role, accessible label, raw and
effective action semantics such as destination and form method. Any change, even
without navigation, voids it. Geometry and compositor hit testing are repeated
after hover immediately before trusted input, so a newly installed overlay is
refused rather than pressed.

Jobs do not survive a daemon restart (see the limits below). Interrupted jobs
come back as `interrupted` with their logs intact rather than pretending they can
resume.

## What it does

**Real input.** Every gesture goes through the CDP `Input` domain, so the page
receives events with `isTrusted === true`. Applications that gate on trusted
events behave normally. Before any click, `brow` scrolls the node into view,
samples its content quads twice to confirm it has stopped moving, and hit-tests
the target point — a click that would land on an overlay is refused with an
explanation rather than silently delivered somewhere else.

**One page tree.** `brow snapshot` merges DOM structure, the accessibility tree,
layout boxes and computed visibility into a single node model. It pierces shadow
roots — including **closed** ones, because CDP operates below the JavaScript
boundary — folds text into its owning element, and recursively attaches
out-of-process cross-origin iframes. Target fragments are spliced beside their
DOM owner in deterministic preorder. Accessibility is fetched per frame and
action coordinates are transformed through every iframe owner to the top-level
viewport. A target that cannot attach, initialize or capture becomes an explicit
`coverage_gap`; it is never silently presented as a complete tree.

Visual painting and CSS pointer eligibility are tracked separately. A native
control hidden with `opacity: 0` beneath a styled checkbox remains in the default
interactive snapshot and is marked `transparent`; a control with
`pointer-events: none` does not. This is only candidate discovery: the live
geometry and compositor hit test immediately before input still decide whether
an action is safe to deliver.

**Refs that expire.** Emitted refs include their generation, for example
`@node-7-42`. Navigating, an SPA `pushState`, or a relevant target-tree change
invalidates outstanding refs. Repeated snapshots in one stable generation reuse
a ref only for the same browser-side node identity and never recycle a removed
node's number onto a different node.

**Read-only evaluation that is actually enforced.** `brow eval` runs with V8's
`throwOnSideEffect`, which aborts an expression the moment it tries to mutate
anything. This is a real guarantee, not a convention — see the caveat below.

**Known credential shapes are redacted before event storage.** Console output,
exception locations and network URLs are filtered at capture: credential headers
and query parameters by name, plus `Bearer` / JWT shapes in free text. This is a
narrow best-effort filter, not a guarantee for arbitrary PII, DOM text, images or
unknown secret formats.

**No listening debug port.** The browser is launched with
`--remote-debugging-pipe` and speaks only to its parent process over inherited
file descriptors. Nothing else on the machine can attach to it. The control
socket is mode `0600` inside a `0700` directory, and the daemon additionally
checks the peer's uid.

**No raw protocol for the caller.** The socket protocol is the whole capability
surface, and it has no variant that carries a CDP method name. "The agent cannot
issue arbitrary protocol commands" is a property of the type, not a filter
somebody has to remember to update.

## Known limits

Verified on Chrome, macOS; original browser-limit probes 2026-08-04, current
regression suite 2026-08-09.

- **`throwOnSideEffect` is sound but conservative.** Nothing that mutates gets
  through, but some harmless reads are refused too:
  `document.querySelector('#x').textContent` is allowed, while
  `document.getElementById('x').textContent` and `el.getBoundingClientRect()` are
  not. The error message says so and suggests the rewrite.
- **One Chromium screenshot frame is capped** at 16,384 output pixels per axis
  and 8,000,000 pixels total. Oversized full-page PNGs are tiled and stitched up
  to 64,000,000 output pixels; only one high-memory stitch runs process-wide.
  Oversized full-page JPEG/WebP is refused because lossy tiles cannot be joined
  faithfully. Explicit rect/node clips stay single-frame and are truncated with
  a warning rather than accepting Chromium's silent repeated-row corruption.
- **Only one page per browser window is `visible`.** Every other tab in the same
  window gets `requestAnimationFrame` at zero — frozen animations, no screencast
  frames, stalled `IntersectionObserver` — and no flag changes it. This is why
  each `--session` gets its own browser process rather than a tab.
- **Touch feature detection lags one navigation.** The first touch gesture turns
  on touch emulation, which updates `navigator.maxTouchPoints` immediately but
  leaves `'ontouchstart' in window` false until the page reloads — the property is
  fixed when the document is created. Gestures are delivered correctly either way,
  but a responsive site keeps its desktop layout until you reload.
- **Event capture is bounded** at 2000 console entries and 2000 requests per
  session, oldest dropped first. CDP frame, queue and retained-event byte budgets
  are bounded too. Ring evictions and upstream stream gaps are reported
  separately; console/network responses mark themselves incomplete after a gap.
- **Event timestamps are receive time**, not browser event time. CDP mixes several
  clocks and reconciling them is not done yet.
- **Transformed inline-frame bounds are approximate.** Trusted actions and node
  screenshots use live DOM quads and are checked across iframe compositors, but
  the informational `bounds` emitted for a same-process iframe nested under CSS
  rotation/scale or thick borders are still based on translation-only snapshot
  geometry. Cross-process OOPIF owner transforms do handle affine scale/rotation.
- **The browser does not survive a daemon restart.** Closing the CDP pipe is
  Chromium's shutdown signal, so killing `browd` takes its browsers with it —
  verified, including that it leaves no orphans and that a stale socket is
  recovered cleanly on the next start. "Persistent" here means across CLI
  invocations, not across a daemon restart; the latter would need a supervisor
  process per browser holding the pipe.
- **The approval command is not proof of human presence.** The Unix peer uid is
  authenticated, but any process running as that user can invoke it. The exact
  target/evidence binding is enforced; the human/operator distinction remains a
  documented workflow boundary until an external trusted approval surface exists.
- **Browser flags are not a strict egress firewall.** The `brow` binary has no
  product telemetry and Chromium background services are disabled, but page and
  browser traffic is not forced through a deny-by-default proxy.
- **`navigator.webdriver` is `true`.** The pipe transport sets it unconditionally.
  `brow` makes no attempt to hide that it is automation — it is a tool for testing
  your own applications, and the relevant consequence is that your app may take a
  bot-detection branch it would not take for a human.
- **Unix only.** The CDP pipe is wired up with `dup2` in a `pre_exec` hook; the
  Windows handle-inheritance equivalent is not written.
- **Canvas and WebGL** expose only the `<canvas>` element. Nothing inside it is
  introspectable without application cooperation.
- **Browser chrome, OS permission dialogs, Keychain, Touch ID and CAPTCHA** are
  out of scope by design and need a human.

## Development

```bash
cargo test          # 138 tests (110 unit + 28 integration in this worktree)
BROW_REQUIRE_CHROME=1 cargo test # fail instead of skipping browser e2e
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

The browser tests skip themselves with a notice when no Chromium is installed.
They serve their own fixture page from an in-process HTTP server, so the suite
needs no network.

`docs/research/` is dated protocol research behind the design, not a current-gap
ledger. Current code/tests and this README take precedence where implementation
has moved on.
