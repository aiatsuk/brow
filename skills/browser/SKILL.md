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

@node-12 button "Create account" id=go [420,610 220x48]
@node-14 input type=text placeholder="Email address" [420,540 220x40]
@node-19 a "Pricing" href=/pricing [120,40 60x20]
```

Act on refs, never on coordinates unless you have no alternative:

```bash
brow fill @node-14 'someone@example.com'
brow click @node-12
brow press Enter
```

Then **snapshot again**. Refs belong to one document generation. Any navigation,
including an in-page route change, invalidates them all. Reusing a stale ref is an
error, not a wrong click — that is deliberate, and the error tells you what to do.

## Commands

| Need | Command |
|---|---|
| Go to a page | `brow open <url>` |
| See what is on it | `brow snapshot` (add `--all` for every visible node) |
| Click | `brow click @node-12` (`--button right`, `--double`, `--force`) |
| Hover | `brow hover @node-12` |
| Fill a field | `brow fill @node-14 'text'` |
| Type into focus | `brow type 'text'` (`--by-key` if the app listens to keydown) |
| Keys | `brow press Enter` · `brow press Ctrl+A` · `brow press Escape` |
| Scroll | `brow scroll down 800` |
| Screenshot | `brow screenshot` · `--full-page` · `--node @node-12` · `-o path.png` |
| Read page state | `brow eval "document.querySelector('#status').textContent"` |
| Several browsers | `--session qa` on any command |
| Finish | `brow close` |

Add `--json` to any command when you need to parse the result rather than read it.

## Reading a page cheaply

`brow snapshot` gives interactive elements only. That is usually what you want and
it is small. Reach further only when you need to:

- `brow snapshot --all` — every visible node. Large; use on a small page or when
  the thing you need has no interactive affordance.
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

## Multiple flows at once

Sessions are independent browsers with independent profiles:

```bash
brow --session admin open app.example.com/admin
brow --session user  open app.example.com
brow sessions
```

Use this for anything involving two accounts, or to keep a logged-in session
while you poke at something else.

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
