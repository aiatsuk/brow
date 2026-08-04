# brow

Local-first browser harness for AI agents. A persistent Chromium driven over the
DevTools Protocol directly — no Playwright, no Puppeteer, no cloud, no telemetry.

```text
brow (CLI)  ──unix socket──▶  browd (daemon)  ──CDP pipe──▶  Chromium
```

An agent turn is a one-shot process. A login session, a half-filled form and a
forty-minute crawl are not. `browd` holds the browsers so they outlive whichever
command happens to be running, and `brow` is the small deterministic verb surface
an agent drives them with.

## Status

**Iteration 2.** Everything below works and is covered by tests that run against
a real Chromium. Background jobs, video recording, the site graph and framework
adapters are not built yet.

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
brow snapshot                       # interactive elements, with @node-N refs
brow click @node-12
brow fill @node-14 'someone@example.com'
brow press Enter
brow screenshot --full-page -o page.png
brow eval "document.querySelector('#status').textContent"
brow console --errors               # console output and uncaught exceptions
brow network --failed               # requests, with credentials already stripped
brow close
```

Touch and pointer gestures:

```bash
brow tap @node-12
brow long-press @node-12 --duration-ms 800
brow swipe --from 320,720 --to 320,160 --duration-ms 450
brow pinch --center 400,400 --scale 1.8
brow drag --from @node-31 --to @node-40
```

Everything takes `--json` for machine-readable output and `--session NAME` to run
several independent browsers at once.

```bash
brow --session qa open staging.example.com
brow sessions
brow daemon status
brow daemon stop
```

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
boundary — folds text into its owning element, and reads same-process iframes
inline. The accessibility tree is fetched per frame and layout boxes are
translated into top-level coordinates, because CDP gives neither of those for
free: `Accessibility.getFullAXTree` does not cross an iframe boundary even
same-origin, and `DOMSnapshot` reports each document's layout in its own frame's
coordinate space.

**Refs that expire.** `@node-42` is valid only for the document generation it was
minted in. Navigating, or an SPA `pushState`, invalidates every outstanding ref,
and using a stale one is a loud error naming the fix instead of a click on
whatever now occupies that position.

**Read-only evaluation that is actually enforced.** `brow eval` runs with V8's
`throwOnSideEffect`, which aborts an expression the moment it tries to mutate
anything. This is a real guarantee, not a convention — see the caveat below.

**Secrets never reach disk.** Console output and network URLs are redacted at
capture, not at display: credential headers by name, credential query parameters
by name, and `Bearer` / JWT token shapes in free text. Redaction is narrow on
purpose — a request id or a content hash is not a secret, and over-redaction
destroys the debuggability the capture exists for.

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

Measured on Chrome, macOS, 2026-08-04.

- **`throwOnSideEffect` is sound but conservative.** Nothing that mutates gets
  through, but some harmless reads are refused too:
  `document.querySelector('#x').textContent` is allowed, while
  `document.getElementById('x').textContent` and `el.getBoundingClientRect()` are
  not. The error message says so and suggests the rewrite.
- **Full-page screenshots cap at 16384 px** per axis; beyond that the capture is
  truncated and the result says by how much.
- **Touch feature detection lags one navigation.** The first touch gesture turns
  on touch emulation, which updates `navigator.maxTouchPoints` immediately but
  leaves `'ontouchstart' in window` false until the page reloads — the property is
  fixed when the document is created. Gestures are delivered correctly either way,
  but a responsive site keeps its desktop layout until you reload.
- **Event capture is bounded** at 2000 console entries and 2000 requests per
  session, oldest dropped first; the count of what was dropped is reported rather
  than hidden.
- **Event timestamps are receive time**, not browser event time. CDP mixes several
  clocks and reconciling them is not done yet.
- **Out-of-process iframes** are not yet traversed. Same-process iframes are fully
  supported — tree, accessible names, coordinates and clicks — but a cross-origin
  frame runs in its own process and needs its own attached session, which is not
  wired up yet.
- **The browser does not survive a daemon restart.** Closing the CDP pipe is
  Chromium's shutdown signal, so killing `browd` takes its browsers with it —
  verified, including that it leaves no orphans and that a stale socket is
  recovered cleanly on the next start. "Persistent" here means across CLI
  invocations, not across a daemon restart; the latter would need a supervisor
  process per browser holding the pipe.
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
cargo test          # 49 tests; the e2e ones drive a real Chromium
cargo clippy --all-targets
```

The browser tests skip themselves with a notice when no Chromium is installed.
They serve their own fixture page from an in-process HTTP server, so the suite
needs no network.

`docs/research/` holds the protocol research this design is built on, including
the experiments behind each claim above.
