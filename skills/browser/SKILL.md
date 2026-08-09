---
name: browser
description: Drive a real browser — open pages, read their structure, click, type, screenshot, and inspect what a page is actually doing. Use whenever the task involves a web page: checking that a UI change works, walking a signup or checkout flow, reproducing a bug on a site, reading a page that needs JavaScript to render, or verifying a deploy. Requires the `brow` CLI.
---

# Browser

`brow` drives a persistent Chromium. The browser stays open between commands, so
you build up state across a session instead of starting over each time.

## Check it is there

```bash
brow daemon status
```

If `brow` is not installed, say so and stop — do not try to substitute `curl`,
`wget` or a headless-browser library. A page that needs JavaScript will not come
back correctly from any of them.

## The loop

Always: **open → snapshot → act → snapshot again.**

```bash
brow open example.com
brow snapshot
```

`snapshot` returns the interactive elements with refs:

```text
http://example.com/  "Example"
generation 2

@node-2-12 button "Create account" id=go [420,610 220x48]
@node-2-14 input type=text placeholder="Email address" [420,540 220x40]
@node-2-19 a "Pricing" href=/pricing [120,40 60x20]
@node-2-23 input role=checkbox "Toggle Todo" transparent [360,250 40x40]
```

`transparent` means the page intentionally made a real pointer target visually
transparent, a common styled-checkbox pattern. Use its ref normally; brow still
rechecks live geometry and compositor hit testing before delivering input.

Act on refs, never on coordinates unless you have no alternative:

```bash
brow fill @node-2-14 'someone@example.com'
brow click @node-2-12
brow press Enter
```

Then **snapshot again**. Refs belong to one document generation. Any navigation,
including an in-page route change, invalidates them all. Reusing a stale ref is an
error, not a wrong click — that is deliberate, and the error tells you what to do.

## Commands

| Need | Command |
|---|---|
| Go to a page | `brow open <url>` |
| See what is on it | `brow snapshot` (add `--all` for visible non-interactive nodes too) |
| Click | `brow click @node-2-12` (`--button right`, `--double`, `--force`) |
| Hover | `brow hover @node-2-12` |
| Fill a field | `brow fill @node-2-14 'text'` |
| Type into focus | `brow type 'text'` (`--by-key` if the app listens to keydown) |
| Keys | `brow press Enter` · `brow press Ctrl+A` · `brow press Escape` |
| Scroll | `brow scroll down 800` |
| Screenshot | `brow screenshot` · `--full-page` · `--node @node-2-12` · `-o path.png` |
| Read page state | `brow eval "document.querySelector('#status').textContent"` |
| Console + exceptions | `brow console` (`--errors` for problems only) |
| Network | `brow network` (`--failed` for failures and 4xx/5xx) |
| Touch | `brow tap @node-2-12` · `brow long-press @node-2-12` · `brow swipe --from 320,700 --to 320,160` |
| Pinch zoom | `brow pinch --center 400,400 --scale 1.8` |
| Drag | `brow drag --from @node-2-31 --to @node-2-40` |
| Several browsers | `--session qa` on any command |
| Finish | `brow close` |

## Diagnosing, not guessing

When something on the page did not work, look before theorising:

```bash
brow console --errors     # did the app throw?
brow network --failed     # did a request 404 or blow up CORS?
```

This is usually faster and far more conclusive than screenshotting and
speculating. Known credential headers, query keys, URL userinfo, Bearer values
and JWT shapes are stripped before storage, but arbitrary PII and unknown secret
formats are not. Inspect output before quoting it back.

Two things to know:

- A request shows a status as soon as headers arrive, but only counts as
  *finished* once its body has fully transferred. A `fetch` whose body the app
  never reads stays unfinished forever — that is normal, not a hang.
- Capture keeps the most recent 2000 entries per session and says how many it
  dropped.
- If output reports `event_stream_gaps` or `complete: false`, say that the view is
  incomplete; never turn a missing event into “no errors” or “no requests”.

## Touch

Touch gestures turn on touch emulation the first time you use one. That updates
`navigator.maxTouchPoints` right away, but `'ontouchstart' in window` stays false
until the page reloads. Gestures work regardless — but if you are testing a
responsive layout that branches on touch support, tap once and then `brow open`
the same URL again so the site re-renders in its touch layout.

Add `--json` to any command when you need to parse the result rather than read it.

## Reading a page cheaply

`brow snapshot` gives interactive elements only. That is usually what you want and
it is small. Reach further only when you need to:

- `brow snapshot --all` — every visible node, plus any transparent actionable
  pointer target already present in the default snapshot. Large; use on a small
  page or when the thing you need has no interactive affordance.
- `brow eval "..."` — read specific state directly. Cheapest way to check a
  result: `brow eval "document.querySelector('.toast').textContent"`.
- `brow screenshot` — when layout or a visual bug is the actual question. Do not
  screenshot to *find an element*; snapshot is faster and more precise.

## Evaluation is read-only by default

`brow eval` refuses to change the page. V8 enforces this, so a mutating
expression is aborted before it takes effect.

The check is conservative and sometimes refuses harmless reads:

- works: `document.querySelector('#x').textContent`, `window.appState`,
  `document.body.innerHTML`, `arr.map(...)`
- refused: `document.getElementById('x').textContent`,
  `el.getBoundingClientRect()`

If you get a side-effect error on something that only reads, rewrite it with
`querySelector`. Use `--mutate` only when changing the page is the actual intent,
and say so in your message to the user.

## When a click is refused

```text
brow: node is not the topmost element at (200,420) — something is covering it
  → something is covering the element; dismiss it, or pass --force to click anyway
```

This is real information: a cookie banner, modal or overlay is in the way. Deal
with the overlay — that is what a user would have to do too. Reach for `--force`
only when you have confirmed the overlay is irrelevant (a transparent wrapper, a
decorative layer).

## Long work: background jobs

For anything that would take many turns of clicking — walking a signup flow,
re-checking a deploy, exercising a long form — hand it to a job instead of
driving it step by step. A job runs in its own browser and keeps going after your
command returns.

```bash
brow job start --intent "check signup still works after the deploy" \
  --step "open staging.example.com/signup" \
  --step 'fill "Email" = qa@example.com' \
  --step 'click "Create account"' \
  --step "screenshot" \
  --step "check-errors"
```

Steps: `open <url>` · `click <text>` · `fill <field>=<value>` · `press <chord>` ·
`wait <ms>` · `screenshot` · `check-errors`.

Then `brow job logs <id> --follow`. It stops following when the job finishes
**or when the job needs you** — which is the part to pay attention to.

A job never guesses and never calls a model. It stops in one of two ways:

- **`needs_decision`** — several elements matched your text. It lists them; you
  pick: `brow job answer <id> 1`. This one is yours to answer.
- **`waiting_for_approval`** — the next click looks irreversible ("Delete
  workspace", "Send", "Pay"). **This one is not yours to answer.** Show the user
  the action and the screenshot the job captured, and let them run
  `brow job approve <id>` or `--reject`. `brow job answer` is refused here on
  purpose. The daemon does not yet authenticate human presence, so this rule is
  an agent operating constraint as well as a product workflow. The pending
  action is bound to a fresh exact-node fingerprint and screenshot and is checked
  again immediately before the press, but same-uid IPC is not proof that a human
  invoked the approval command.

Other useful commands: `brow job list`, `brow job status <id>`,
`brow job stop <id>`.

If the daemon restarts, running jobs come back as `interrupted` and cannot be
resumed — start them again rather than assuming they continued.

## Multiple flows at once

Sessions are separate browsers with independent profiles. Different sessions run
concurrently; commands within one session are serialized to preserve browser
state ordering:

```bash
brow --session admin open app.example.com/admin
brow --session user  open app.example.com
brow sessions
```

Use this for anything involving two accounts, or to keep a logged-in session
while you poke at something else.

Cross-origin out-of-process iframes are included recursively. If snapshot prints
`coverage_gap`, do not act as if the missing frame was inspected; retry once, then
report the exact gap if it remains.

## What to hand back to the human

- OS-level dialogs, Keychain, Touch ID, CAPTCHA, and browser chrome. `brow`
  cannot drive these and should not pretend to.
- Anything irreversible or outward-facing — publishing, deleting, purchasing,
  sending a message, entering an OTP. Describe exactly what you are about to
  click and get confirmation first.
- Credentials. Do not type passwords you were not explicitly given for this task.

## Habits that keep this reliable

- Snapshot after every action that could change the page. It is cheap.
- Prefer refs over coordinates; coordinates break on any layout change.
- Read results with `eval` or `snapshot`, not by screenshotting and guessing.
- If something fails twice the same way, stop and report what you saw — including
  the exact error — rather than trying a third variation.
- `brow close` when the task is done, so a browser is not left running.
