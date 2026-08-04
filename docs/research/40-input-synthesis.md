# 40 — Input Synthesis: browser-level mouse, touch, gesture, keyboard, IME, drag, upload

> **Bottom line.** Everything the brief asks for is achievable with `Input.*` + `Emulation.*` + `DOM.*` on Chrome 151, and CDP-synthesised events are `isTrusted === true` (verified on every event type below) — that is a real, defensible advantage over `element.dispatchEvent`. The three things that will actually cost you engineering time are (1) **coordinates**: every input point must be resolved through `DOM.getContentQuads` in *main-frame viewport CSS pixels*, and OOPIFs break `pierce:true`, so you need per-frame sessions plus offset composition; (2) **the keymap**: there is no crate that gives you `code` + `key` + `windowsVirtualKeyCode` + `text` together, so you must code-generate one from Chromium's own BSD-licensed tables (do **not** vendor Puppeteer's Apache-2.0 `USKeyboardLayout.ts` given the project's "no Puppeteer anywhere" constraint); (3) **actionability**, which is the entire difference between flaky and non-flaky and which nobody hands you. Three landmines found empirically: `Emulation.setEmitTouchEventsForMouse` **permanently kills the mouse-input path of that session** — every later `dispatchMouseEvent` hangs, and turning the emulation back off does not help (known, unfixed — crbug 40225266); `Input.dispatchKeyEvent{type:"char"}` without `code` is malformed and should never be emitted; and `Input.dispatchTouchEvent` works *without* touch emulation, which is the opposite of the folklore — but you still need `Emulation.setTouchEmulationEnabled` because sites feature-detect.

> **Two corrections to this bottom line, 2026-08-04 (see the Verification pass at the end):**
> * **`navigator.webdriver` is `true` for this project, always.** The original sentence — "`false` under plain `--remote-debugging-port`; set by `--enable-automation`" — is correct *for websocket mode* and irrelevant, because `brow` uses `--remote-debugging-pipe`, and **the pipe sets `navigator.webdriver` unconditionally** in both headless and headful. See §11.
> * **The `char` storm did not reproduce** on Chrome 151.0.7922.72 across six conditions and both transports. It is downgraded from "browser-killer" to "unreproduced". The hygiene rule (always populate `code` + `windowsVirtualKeyCode`) is retained because it costs nothing. See §5.

---

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| All input points resolved via `DOM.getContentQuads` → main-frame viewport CSS px | Handles transforms, iframe offsets, scroll, DPR automatically; returns real rotated quads not bboxes | `getBoundingClientRect` in page (axis-aligned, needs eval, wrong under transforms) | **confirmed** (observed) |
| Never dispatch mouse without a preceding `mouseMoved` at the same point | Frameworks bind `mousemove`; hover menus/tooltips need it; Chrome's synthesized over/enter on `mousePressed` carries anomalous `buttons:1` | Press/release only (works for plain buttons, breaks hover-driven UI) | **confirmed** (observed) |
| Text entry: `Input.insertText` for bulk, per-key `dispatchKeyEvent{keyDown+text, keyUp}` when semantics matter | `insertText` respects `maxlength` and fires `beforeinput`/`input`/`textInput` (React-safe) but emits **no** key events | Per-key always (10–40× slower); `insertText` always (breaks keydown-driven UIs, autocomplete, Enter-to-submit) | **confirmed** (observed) |
| **Never** send `Input.dispatchKeyEvent{type:"char"}` without `code` + `windowsVirtualKeyCode` | Malformed input; a keydown storm was once observed but **did not reproduce on 2026-08-04** (§5). Rule kept as zero-cost hygiene | Puppeteer-style `rawKeyDown`/`char`/`keyUp` triad verbatim | **downgraded** (storm NOT REPRODUCED) |
| **Ban** `Emulation.setEmitTouchEventsForMouse` from the codebase | Permanently wedges the **whole mouse-input path of that session** — all later `dispatchMouseEvent` calls hang, disabling it does not recover, only target recreation does. Browser itself survives. crbug 40225266, still open | Using it for mobile emulation convenience | **confirmed** (re-observed 2026-08-04 + crbug) |
| Mobile mode = `setTouchEmulationEnabled` + explicit `dispatchTouchEvent`, never mouse→touch conversion | Only safe path; also the only path that supports multi-touch | `emulateTouchFromMouseEvent` (EXPERIMENTAL, DIP coords, no multi-touch) | **confirmed** |
| Keymap code-generated from Chromium `ui/events/keycodes/dom_us_layout_data.h` + `keyboard_codes_posix.h` (BSD-3-Clause) | Same provenance as the browser; satisfies "no Puppeteer in the dependency graph"; regenerable per Chrome version | Vendoring Puppeteer `USKeyboardLayout.ts` (Apache-2.0, 217 entries) | **confirmed** (fetched both files) |
| `keyboard-types` 0.8.3 for `Code`/`Key`/`Modifiers`/`Location` value types only | MIT OR Apache-2.0, stable, W3C-aligned naming | Hand-rolling the enums | **confirmed** (crates.io + docs.rs) |
| Actionability stability check runs **in-page in an isolated world**, one round trip | 1 RTT vs 3; measures exactly 2 rAFs; page cannot observe or patch it | Two `getContentQuads` around a CDP-awaited double-rAF (3 RTT, ~25 ms of wall clock instead of 2 frames) | **confirmed** (observed both) |
| Drag: three explicit strategies (`native-html5`, `pointer`, `file-drop`), chosen by probing, never auto-guessed silently | HTML5 DnD and pointer DnD (dnd-kit, react-beautiful-dnd) are mutually exclusive mechanisms | One "drag" primitive | **confirmed** (both recipes observed working) |
| Do **not** pass `--enable-automation`; do **not** patch `navigator.webdriver` | Patching would be evasion tooling, out of scope. **Note (2026-08-04): the premise changed** — `--remote-debugging-pipe` sets `navigator.webdriver === true` on its own, so this decision affects the infobar and password-save UI only, not the fingerprint (§11) | Stealth patching | **decision stands, rationale corrected** |
| Fling/inertia via `Input.synthesizeScrollGesture{preventFling:false}` | Blocks until the fling settles, so no polling; overshoot is real | Hand-rolled touch streams (also flings, but velocity is uncontrolled wall-clock) | **confirmed** (observed) |

---

## 1. The coordinate model — get this right first, everything else follows

`Input.dispatchMouseEvent.x/y` and `Input.dispatchTouchEvent.touchPoints[].x/y` are ~~**CSS pixels in the main frame's visual viewport**~~ **CSS pixels in the main frame's LAYOUT viewport** *(corrected 2026-08-04: under `Emulation.setPageScaleFactor{2.0}` the visual viewport became 400×300 while the layout viewport stayed 800×600; a click at `(250,425)` still arrived as `clientX/clientY = 250,425`, and `getContentQuads` was unchanged by the page scale — so the two spaces are distinguishable and CDP uses layout)*, independent of `deviceScaleFactor`. Verified: with `Emulation.setDeviceMetricsOverride{deviceScaleFactor:3}`, a click dispatched at `(196,400)` arrived in the page as `clientX/clientY = 196,400`.

`DOM.getContentQuads` (EXPERIMENTAL, but load-bearing) returns an array of 4-corner quads `[x1,y1,x2,y2,x3,y3,x4,y4]` in the **same** space. Observed properties:

| Case | Observation |
|---|---|
| `transform: rotate(30deg)` element | Quad is the true rotated parallelogram `[269.2,378.3, 355.8,428.3, 330.8,471.7, 244.2,421.7]`, **not** an axis-aligned bbox |
| `display:none` element | `{"quads": []}` — no error. This is your "not visible" signal |
| Element scrolled out of viewport | Quad y = 2000 with `scrollY=0`; after `DOM.scrollIntoViewIfNeeded` → y = 224 with `scrollY=1776`. So: **viewport-relative, live** |
| Element inside a *same-origin* iframe, queried from the **main** session with a pierced `backendNodeId` | Quad already in **main-frame** coordinates (`70,130 → 170,170` for a button at `10,10` in an iframe at `60,120`). Clicking the centre delivered `clientX/Y = 60,30` to the iframe. **iframe offsets are applied for you** |
| Element inside an **OOPIF** | `DOM.getDocument{pierce:true}` from the main session **does not reach it at all** |

### OOPIF recipe (verified)

An out-of-process iframe (`localhost:8802` inside `127.0.0.1:8801`) produces a separate `Target` of type `iframe`. Two working strategies:

```text
Strategy A (composition):
  main:  DOM.querySelector("#the-iframe") -> DOM.getContentQuads  => F = [60,120,...]
  oopif: DOM.querySelector("#b")          -> DOM.getContentQuads  => Q = [10,10,...]  (FRAME-LOCAL)
  point  = centre(Q) + (F[0], F[1])                                => (120,150)
  main:  Input.dispatchMouseEvent(x=120,y=150)                     => OOPIF sees clientX/Y = 60,30  ✅

Strategy B (simpler, also verified):
  oopif session: Input.dispatchMouseEvent(x=60,y=30)               => OOPIF sees clientX/Y = 60,30  ✅
```

Strategy B is cheaper and avoids nested-frame offset chains. Strategy A is still required for *occlusion* checks, because `DOM.getNodeForLocation` on the OOPIF session cannot see a parent-page overlay covering the frame. **Do both:** compose in the parent for hit-testing, dispatch on the frame session for delivery.

`Page.getFrameTree` from the main session lists **no child frames** for an OOPIF — you must use `Target.setAutoAttach{autoAttach:true, flatten:true}` to discover them.

---

## 2. Mouse — `Input.dispatchMouseEvent`

Live schema from Chrome 151.0.7922.72 (`/json/protocol`), domain **not** experimental:

```
Input.dispatchMouseEvent
  type: "mousePressed" | "mouseReleased" | "mouseMoved" | "mouseWheel"   (required)
  x, y: number                                                            (required, CSS px)
  modifiers: integer            Alt=1  Ctrl=2  Meta/Command=4  Shift=8   (bitmask, OR them)
  timestamp: TimeSinceEpoch     seconds since UNIX epoch, fractional
  button: "none"|"left"|"middle"|"right"|"back"|"forward"
  buttons: integer              Left=1 Right=2 Middle=4 Back=8 Forward=16 (bitmask of HELD buttons)
  clickCount: integer
  force: number                 [EXPERIMENTAL] normalized pressure [0,1]
  tangentialPressure: number    [EXPERIMENTAL] [-1,1]
  tiltX, tiltY: number          degrees [-90,90]
  twist: integer                [EXPERIMENTAL] [0,359]
  deltaX, deltaY: number        wheel only
  pointerType: "mouse" | "pen"
```

Modifier bitmask **verified by probe** (dispatch with `modifiers=1,2,4,8,15`, read `altKey/ctrlKey/metaKey/shiftKey`): `1=Alt, 2=Ctrl, 4=Meta, 8=Shift`, and `15` sets all four. On macOS, `Meta` is Command.

### The exact event sequence a real click produces

With `mouseMoved` → `mousePressed` → `mouseReleased` at (100,80), the page observed, in order:

```
pointerover, pointerenter ×4, mouseover, mouseenter ×4,   ← enter fires once per ancestor
pointermove, mousemove,
pointerdown, mousedown (clickCount 1),
pointerup,   mouseup   (clickCount 1),
click        (isTrusted: true on every one)
```

**If you skip `mouseMoved`:** Chrome *still* synthesizes `pointerover/pointerenter/mouseover/mouseenter` at press time — but with `buttons: 1` on the over/enter events, which no real mouse ever produces — and emits **no `pointermove`/`mousemove` at all**. Consequences:

- `:hover` CSS still applies (`el.matches(':hover')` was `true` after a bare `mouseMoved`, and the over/enter chain fires on press).
- Anything driven by `mousemove` (hover menus, drag thresholds, tooltip timers, canvas cursors, analytics heatmaps, `dnd-kit` activation constraints) **gets nothing**.
- Frameworks that key on `buttons === 0` to mean "hovering, not dragging" see `buttons === 1` and can enter a bad state.

**Rule for the harness:** every click is `mouseMoved(point, buttons:0)` → settle → `mousePressed` → `mouseReleased`. For hover-revealed targets, insert an interpolated path of 5–15 `mouseMoved` points plus a settle delay before re-running actionability.

### Double / triple click

Do **not** rely on timing. Dispatch `clickCount` explicitly and Chrome synthesizes the higher-order events regardless of wall-clock spacing:

```
press/release clickCount=1  → click (detail 1)
press/release clickCount=2  → click (detail 2), dblclick (detail 2)   ← verified, back-to-back with no delay
press/release clickCount=3  → click (detail 3)   (triple-click selects the paragraph)
```

### Right-click, context menus

Verified sequence for `button:"right", buttons:2`:

```
pointerover/enter, mouseover/enter, pointermove, mousemove,
pointerdown (button 2), mousedown (button 2),
contextmenu (button 2)      ← fires BETWEEN mousedown and pointerup
pointerup, mouseup, auxclick (button 2)
```

- The page **does** get `contextmenu`, `isTrusted: true`.
- **Headful, verified:** the three CDP calls returned in **53 ms** and a subsequent `mouseMoved` returned in **0 ms**. The native OS context menu does **not** block the CDP channel, and CDP-injected input continues to be delivered to the renderer afterwards. Whether the native menu is visually painted I could not confirm (the test window was positioned off-screen) — treat "a native menu may be visible on screen and will appear in screenshots/video" as **likely**, not confirmed.
- The native menu itself is browser chrome → **out of scope for automation**, per the project constraints. Expose `contextMenu(ref)` as "fire `contextmenu` so the page's *custom* menu opens", and route "click a native menu item" to human handoff.

### Middle click

`button:"middle", buttons:4` produces `pointerdown/mousedown/pointerup/mouseup/auxclick` with `button === 1`, and **no** `click`, **no** `contextmenu` (verified). The open-in-new-tab side effect is browser-level: if the target is a link, a new `Target` appears. The harness must therefore treat middle-click as a **navigation-producing action** and subscribe to `Target.targetCreated` around it.

### Pen / stylus

`pointerType:"pen"` with `tiltX:30, tiltY:-15, twist:45` propagated verbatim to the page's `PointerEvent`. `force` mapped to `pressure` was **0** on a hovering (`buttons:0`) move — correct per spec (hovering pointers report pressure 0); send `force` on `mousePressed` to get non-zero pressure.

### Wheel

`type:"mouseWheel"` with `deltaY` is **1:1 in CSS pixels, no acceleration** (verified: one event with `deltaY:500` → `scrollY === 500`; then 20 × `deltaY:40` → `scrollY === 1300`). The page sees a single `wheel` event with `deltaMode: 0` (pixels). This is the correct primitive for deterministic scrolling; it is *not* how a real trackpad feels. For "feels real", use `Input.synthesizeScrollGesture` (§4).

---

## 3. Touch — `Input.dispatchTouchEvent`

```
Input.dispatchTouchEvent
  type: "touchStart" | "touchEnd" | "touchMove" | "touchCancel"
  touchPoints: TouchPoint[]     ← for touchEnd, this is the REMAINING points (empty for last finger up)
  modifiers, timestamp

TouchPoint { x, y, radiusX?, radiusY?, rotationAngle?, force?,
             tangentialPressure? [EXP], tiltX?, tiltY?, twist? [EXP], id? }
```

### The folklore is wrong — touch emulation is NOT a protocol prerequisite

**Verified, and this contradicts the brief's assumption.** On a brand-new target with `navigator.maxTouchPoints === 0` and `'ontouchstart' in window === false`, `Input.dispatchTouchEvent{touchStart}` + `{touchEnd}` succeeded and the page received:

```
pointerdown(pointerType:"touch"), touchstart, pointerup, touchend, click   — all isTrusted:true
```

So the protocol delivers touch regardless. **But you must still call `Emulation.setTouchEmulationEnabled` on real sites**, because sites gate on feature detection. After `setTouchEmulationEnabled{enabled:true, maxTouchPoints:5}` (non-experimental):

| Probe | before | after |
|---|---|---|
| `navigator.maxTouchPoints` | 0 | 5 |
| `'ontouchstart' in window` | false | true |
| `matchMedia('(pointer:coarse)')` | false | true |

A React app that does `if ('ontouchstart' in window) bindTouchHandlers()` has **no handlers attached** in the "before" column, so your perfectly-valid touch events land on nothing. Frame this correctly in the capability API: `setTouchEmulationEnabled` is a **feature-detection prerequisite**, not an event-delivery prerequisite.

### Multi-touch

Verified with two points. Chrome splits a multi-point `touchStart` into **one `touchstart` event per finger**, with `event.touches` growing:

```
dispatch touchStart [p1@(100,200) id1, p2@(200,200) id2]
  → touchstart (touches: 1) ... touchstart (touches: 2)     ← two events, matches real hardware
dispatch touchMove [p1@(80,200), p2@(220,200)]
  → 2× pointermove, 1× touchmove (touches: 2)
dispatch touchEnd  [p2 only]  → touchend (touches: 1)
dispatch touchEnd  []         → touchend (touches: 0)
```

`id` is your finger identifier and maps to `Touch.identifier`. `force` and `radiusX` propagate (`force:0.9` → `Touch.force: 0.8999999761581421`).

### Chrome DOES compute fling from your raw point stream

**Verified, and this is the surprising and useful result.** A hand-rolled swipe (`touchStart`, 12 × `touchMove` of −28 px, `touchEnd`) on a scroller produced `scrollTop = 335` immediately after `touchEnd` and `scrollTop = 692` 1.5 s later — the fling kept running. This held both **with** explicit `timestamp` values and **without** them.

Implication: velocity for inertia is derived from the point stream's arrival times. Without `timestamp`, that means *your wall-clock IPC jitter* sets the fling velocity — non-deterministic. Two options:
1. Set `timestamp` explicitly on every touch point and pace the sends to match (best-effort determinism; note that in the "with timestamps" run the result was still 321→682, i.e. essentially the same, so Chrome may be preferring arrival time — **unverified** which it uses).
2. Use `synthesizeScrollGesture` (below), which is deterministic and blocking.

**Recommendation:** expose `swipe()` on top of `synthesizeScrollGesture` for the "make it feel real" path, and keep the raw touch stream for gestures the synthesizers don't cover (arbitrary paths, rotation, >2 fingers, long-press-then-drag).

### Long-press, double-tap

Neither has a CDP primitive. Build them:

- **long-press**: `touchStart` → hold ≥ 500 ms (600 ms is the safe default; Android's is 500, iOS ~500) with ≤ 5 px of jitter → `touchEnd`. Moving more than the slop distance cancels it.
- **double-tap**: two tap pairs ≤ 300 ms apart at the same point. `synthesizeTapGesture{tapCount:2}` does this for you — verified to emit two full down/up/click cycles 74 ms apart.

---

## 4. The `synthesize*` gestures — present, EXPERIMENTAL, and **blocking**

All three are present in Chrome 151 and all three are marked `experimental: true`. This matters: they are the compositor-level gesture generators (`SyntheticGestureController`) used by Chrome's own telemetry harness, and they are what makes a swipe *feel* real instead of *look* real.

```
Input.synthesizeScrollGesture   x, y, xDistance, yDistance, xOverscroll, yOverscroll,
                                preventFling (default TRUE), speed (px/s), gestureSourceType,
                                repeatCount, repeatDelayMs, interactionMarkerName
Input.synthesizePinchGesture    x, y, scaleFactor, relativeSpeed, gestureSourceType
Input.synthesizeTapGesture      x, y, duration (ms), tapCount, gestureSourceType
GestureSourceType = "default" | "touch" | "mouse"     [EXPERIMENTAL]
```

### Verified behaviour

| Test | Result |
|---|---|
| `yDistance:-600, speed:800, touch, preventFling:false` | call returned after **1353 ms**; `scrollTop = 733` at return — i.e. **133 px of fling overshoot**, and no further movement 1.2 s later. **The call blocks through the fling.** |
| same with `preventFling:true` | returned after **928 ms**; `scrollTop = 601` exactly |
| `yDistance:-500` at `speed = 100 / 800 / 5000` | **5303 ms / 806 ms / 260 ms**. `speed` is **pixels per second**, and the call blocks for the whole duration |
| `synthesizeTapGesture{duration:120}` | returned after 222 ms; page saw `pointerdown@763ms … pointerup@883ms` — **exactly 120 ms** apart, then `click` |
| `synthesizeTapGesture{tapCount:2, duration:60}` | two complete tap cycles, 74 ms apart |
| `synthesizePinchGesture{scaleFactor:2.0, relativeSpeed:800}` | returned after 307 ms; two touch points placed **vertically** 125 px apart around `(x,y)`, moved apart over ~10 `touchmove` frames. Client coordinates then *compress* as page scale changes — expected, since pinch alters the visual viewport |
| `gestureSourceType:"mouse"` scroll | emits a stream of real `wheel` events with fractional, **easing-ramped** deltas (`6.30, 7.67, 12.56, …`) — genuinely smooth, unlike a single `mouseWheel` |

> **Verified 2026-08-04 — the "blocks through the fling" claim was flagged as possibly-coincidental; it is not.** Independent reproduction on a fresh fixture: `synthesizeScrollGesture{yDistance:-600, speed:800, gestureSourceType:"touch", preventFling:false}` returned after **1.292 s** with `scrollTop = 730`, and `scrollTop` was still exactly **730** measured 1.5 s later. Two independent runs (this one and the original's 1353 ms / 733) agreeing to within 3 px and 60 ms is strong evidence the call really does return only after the fling settles, not by luck. Code may rely on "call returned ⇒ scroll settled" — but keep the watchdog anyway, since `speed` scales the block time linearly (measured 5303/806/260 ms for speed 100/800/5000).

**Notes for the daemon.** Because these block for their full duration (up to seconds), they must not run on the connection's request path in a way that stalls other commands — CDP is multiplexed per-session, so issue them on their own in-flight slot and keep a watchdog. Also: `preventFling` defaults to **true** per the docs, so you must explicitly pass `false` to get inertia. `xDistance`/`yDistance` are *positive = left/up* per the docs (my `yDistance:-600` scrolled *down*, consistent).

`interactionMarkerName` emits trace markers — useful later for the video/action-log sync requirement.

---

## 5. Keyboard — `Input.dispatchKeyEvent`

```
Input.dispatchKeyEvent
  type: "keyDown" | "keyUp" | "rawKeyDown" | "char"
  modifiers, timestamp
  text            "Text as generated by processing a virtual key code with a keyboard layout"
  unmodifiedText  "Text generated without modifiers (except shift)"
  keyIdentifier   legacy, e.g. 'U+0041'
  code            physical key, e.g. 'KeyA'
  key             logical key, e.g. 'a' / 'A' / 'ArrowLeft'
  windowsVirtualKeyCode, nativeVirtualKeyCode
  autoRepeat, isKeypad, isSystemKey: boolean
  location        1=Left, 2=Right (3=Numpad)
  commands        [EXPERIMENTAL] string[] — Blink editing commands, e.g. ["selectAll"]
```

### What each `type` actually does (verified)

| Sequence | Page sees |
|---|---|
| `keyDown{key:'a', code:'KeyA', text:'a', unmodifiedText:'a', windowsVirtualKeyCode:65}` + `keyUp` | `keydown(keyCode 65)`, `keypress(keyCode 97)`, `beforeinput{inputType:"insertText", data:"a"}`, `textInput`, `input`, `keyup`. **Value updated.** |
| `keyDown` **without** `text` + `keyUp` | `keydown`, `keyup` only. No `keypress`, no `input`, **no value change**. This is the correct form for Escape/Arrows/F-keys/modifiers |
| `autoRepeat: true` | one `keydown` with `event.repeat === true` |
| Shift+A: `keyDown(Shift, vk 16, location 1, modifiers 8)` → `keyDown(key:'A', code:'KeyA', text:'A', unmodifiedText:'a', modifiers 8)` → keyUps | correct `keydown(A, kc 65)`, `keypress(kc 65)`, `input "A"` |
| Backspace `keyDown{key:'Backspace', code:'Backspace', vk 8}` (no `text`) | `beforeinput{inputType:"deleteContentBackward"}`, `input`. Deletion works with no `text` |
| Enter in an `<input>` `{key:'Enter', code:'Enter', text:'\r', vk 13}` | `keydown`, `keypress`, `beforeinput{insertLineBreak}`, **`change`**, `keyup` |
| `commands:["selectAll"]` on a `rawKeyDown` | selection became `[0,6]` on a 6-char input — **works** |

### ⚠️ The `type:"char"` storm (reproducible browser-killer)

This is a genuine, narrow-scoped defect I found today. Minimal matrix on Chrome 151.0.7922.72 headless, counting `keydown` listeners fired after the sequence:

| Sequence | keydown count | verdict |
|---|---|---|
| `keyDown(text)` + `char` + `keyUp` | 1 | fine |
| `rawKeyDown` + `keyUp` (no char) | 1 | fine |
| `char` + `keyUp` only | 0 | fine |
| `char` alone (no `code`) | 0 (waited 3 s) | fine |
| **`rawKeyDown(code:'KeyB')` + `char{text:'b'}` (no `code`) + `keyUp(code:'KeyB')`** | **2280, growing ~4000/s, unbounded** | ☠️ |
| `rawKeyDown(code:'KeyB')` + `char{text:'b', code:'KeyB', vk:66}` + `keyUp` | 1 | fine |

The storm produces `keydown` events with `key: "Unidentified"`, `keyCode: 0`, `code: ""`, forever. In one run it ran for seconds and the browser process subsequently exited. **Root cause (inferred, unverified):** a `char` event carrying no `code` corrupts Chrome's held-key state, and the matching `keyUp` then re-enters a synthesis loop.

> **NOT REPRODUCED 2026-08-04 — this claim should be downgraded from "reproducible browser-killer" to "unreproduced, cause unknown".** I tried to reproduce it on the same binary (Chrome 151.0.7922.72, headless, macOS) across **both** transports and **six** conditions, counting `keydown` on a document-level capture listener at 1 s and again at 3–4 s after the sequence:
>
> | Condition | keydown @1 s | @3–4 s | browser |
> |---|---|---|---|
> | pipe, `http://` origin, `rawKeyDown(KeyB)` + `char{text:'b'}` + `keyUp(KeyB)` | 1 | 1 | alive |
> | pipe, `data:` URL, same triple | 1 | 1 | alive |
> | pipe, `data:` URL, `rawKeyDown{code only}` + `char{text}` + `keyUp{code only}` | 1 | 1 | alive |
> | pipe, `data:` URL, `keyDown{text}` + `char{text}` + `keyUp` | 1 | 1 | alive |
> | pipe, `data:` URL, `char{text}` alone | 0 | 0 | alive |
> | **websocket** (`--remote-debugging-port`, the original document's exact transport), `data:` URL, the storming triple | **1** | **1** | alive |
> | control: `char` **with** `code`+`vk` | 1 | 1 | alive |
>
> Every variant produced exactly one `keydown`. No storm, no `key:"Unidentified"` events, no browser death, on either transport or origin.
>
> **What to conclude.** Either the defect is fixed/masked in this exact build under these conditions, or the original measurement had a harness artifact (e.g. the client's own retry/read loop re-sending, or the counter being read while a separate autorepeat was active). Either way, **nothing in the design should rest on it.** The *guidance* is unchanged and costs nothing — prefer `keyDown{text}`, and if you must emit `char`, always populate `code` + `windowsVirtualKeyCode`, because a `char` without `code` is malformed input regardless of whether it storms. But **do not file this upstream without a fresh minimal repro**, and remove "browser-killer" from any user-facing text.

**Hard rule for `crates/input`:** either never emit `type:"char"` at all (use `keyDown{text}`), or, if you must (for pasted single characters with no physical key), always populate `code` and `windowsVirtualKeyCode` to match the surrounding `rawKeyDown`/`keyUp`. Add a debug-build assertion. (Retained as cheap hygiene — see the non-reproduction note above.)

### Where to get a correct US keymap — and why not Puppeteer

Puppeteer's `packages/puppeteer-core/src/common/USKeyboardLayout.ts` is the canonical table (217 entries, `{keyCode, shiftKeyCode, key, shiftKey, code, text, shiftText, location}`), header `SPDX-License-Identifier: Apache-2.0`, © 2017 Google Inc. Apache-2.0 *permits* vendoring a derived data table with attribution and a NOTICE file — it is data, not a dependency. **But** the project's hard constraint says "no Puppeteer anywhere in the dependency graph", and a reviewer grepping for `USKeyboardLayout` will read a vendored copy as a violation of the spirit of that rule.

**Use Chromium's own tables instead** (BSD-3-Clause, same license as the browser you are driving, verified fetched today):

| File | Contents |
|---|---|
| `ui/events/keycodes/dom_us_layout_data.h` | `kPrintableCodeMap[]`: `DomCode → {normal, shifted}` char (`{DomCode::US_A, {'a','A'}}`); `kNonPrintableCodeMap[]`: `DomCode → DomKey`; and a `DomCode → VKEY_*` table (`{DomCode::US_A, VKEY_A}, // 0x070004 KeyA`) — 698 lines |
| `ui/events/keycodes/keyboard_codes_posix.h` | numeric `VKEY_*` values: `VKEY_BACK = 0x08`, `VKEY_TAB = 0x09`, `VKEY_RETURN = 0x0D`, `VKEY_SHIFT = 0x10`, `VKEY_A = 0x41`, `VKEY_NUMPAD0 = 0x60`, `VKEY_OEM_1 = 0xBA` — 300 lines |
| `ui/events/keycodes/dom/keycode_converter_data.inc` | USB HID usage ↔ `DomCode` ↔ native scancodes, with the W3C `code` string |
| `third_party/blink/renderer/core/editing/commands/editor_command_names.h` | 140 `V(Name)` entries — the exact legal values for `dispatchKeyEvent.commands` (`SelectAll`, `Copy`, `Cut`, `DeleteBackward`, `MoveToBeginningOfLine`, …) |

Build a `build.rs` (or an offline `xtask keymap-gen`) that parses these into a static Rust table and vendors the *generated* `.rs` plus a `THIRD_PARTY_NOTICES` entry. Regenerate when you bump the supported Chrome floor.

Rust support crates (versions checked on crates.io **2026-08-04**):

| Crate | Version | License | Role |
|---|---|---|---|
| `keyboard-types` | **0.8.3** (2025-10-02) | MIT OR Apache-2.0 | `Code`, `Key`, `Location`, `Modifiers`, `KeyState`, `CompositionEvent`; W3C-aligned `FromStr`/`Display`. **Does not** carry `windowsVirtualKeyCode` — that is exactly the gap the generated table fills |
| `unicode-segmentation` | **1.13.3** | MIT/Apache-2.0 | grapheme-cluster iteration so emoji/ZWJ/combining marks are typed as one unit |
| `serde_json` | **1.0.151** | MIT/Apache-2.0 | CDP wire |
| `tokio` | **1.53.1** | MIT | daemon runtime |
| `rand` | **0.10.2** | MIT/Apache-2.0 | movement jitter (see §10 caveat) |
| `simple-easing` | **1.0.2** | MIT | cubic/quad easing for interpolated paths |

### Shortcuts handled by the **browser**, not the page

`Input.dispatchKeyEvent` delivers to the *renderer*. Accelerators consumed by the browser process are **not reachable**: Cmd/Ctrl+T/N/W/Q, Cmd+Shift+T, Cmd+L (omnibox), Cmd+R (partially — the page can `preventDefault` but the browser handles it), Cmd+`+`/`-` zoom, F12/Cmd+Opt+I, Cmd+P (print — opens browser chrome). Two consequences:

1. `browserctl` must expose these as **explicit capability verbs** (`page.reload`, `page.zoom`, `tab.new`, `tab.close`) implemented via `Page.reload`, `Emulation.setPageScaleFactor`, `Target.createTarget`, `Target.closeTarget` — never as key combos.
2. Combos the *page* handles (Cmd+S in a web IDE, Cmd+K command palettes, Cmd+Enter submit) work fine via `dispatchKeyEvent` **as long as you send the modifier keydown first and set `modifiers` on the letter key**. On macOS also set `isSystemKey: true` for Cmd-combos so Blink routes them as accelerators rather than text.

---

## 6. Text entry — `insertText` vs per-key

`Input.insertText{text}` (EXPERIMENTAL) — verified behaviour:

```
Input.insertText{text: "hello wörld"}   into <input maxlength=10>
  → beforeinput{inputType:"insertText", data:"hello wörld"}
  → textInput
  → input{data:"hello wörl"}            ← TRUNCATED by maxlength
  final value: "hello wörl"             ← identical to typing 12 chars key-by-key
  NO keydown / keypress / keyup at all
```

Two findings that contradict common belief:
1. **`insertText` respects `maxlength`.** Per-key typing of `"abcdefghijkl"` also gave `"abcdefghij"`. Identical results.
2. **`insertText` fires the full `beforeinput`/`input` pair with `isTrusted: true`**, which is precisely what React's controlled-input `onChange` (a synthetic wrapper over `input`) needs. React controlled inputs are safe with `insertText`.

> **Verified 2026-08-04 — confirmed, with one detail the original run missed and one scope limit made explicit.** Reproduced on `<input maxlength=10>`: final value `"hello wörl"`. But the event payloads are **not** uniformly truncated:
> ```
> beforeinput  isTrusted:true  data:"hello wörld"   <-- FULL, untruncated
> textInput    isTrusted:true  data:"hello wörld"   <-- FULL
> input        isTrusted:true  data:"hello wörl"    <-- truncated
> ```
> Any code (ours or the page's) that reads `e.data` on `beforeinput` sees the *pre-clamp* string. React's `onBeforeInput` and input-mask libraries that key on `beforeinput.data` will therefore see a different value than the field ends up holding — worth a note in the action log so a "typed 11 chars, field has 10" discrepancy is diagnosable rather than mysterious.
>
> **Scope limit, tested:** on a `contenteditable` div, `Input.insertText("0123456789ABCDEF")` inserted **all 16 characters** — `maxlength` is an `<input>`/`<textarea>` attribute and does not exist there. The equivalence claim is therefore exactly "for `<input maxlength=N>`", not "for text entry generally". Still untested: React controlled inputs that reject values in `onChange`, and per-keystroke input masks — the `mode: "keys"` escape hatch exists precisely for those.

| Use | Why |
|---|---|
| `insertText` | Bulk text (paragraphs, long form fields), non-ASCII/emoji, pasted content, speed. One round trip vs 2N |
| Per-key `keyDown{text}` + `keyUp` | Autocomplete/typeahead that reacts to each `keydown`; input masks (phone/card formatters) that reformat per keystroke; anything binding `keypress`; Enter-to-submit; character-limit counters driven by `keydown`; when you must reproduce a user-visible typing cadence for the recording |

**Default policy:** `type(ref, text, {mode: "auto"})` where `auto` = per-key if the element has any `keydown`/`keypress`/`beforeinput` listener discovered via `DOMDebugger.getEventListeners` **or** matches a mask heuristic (`inputmode`, `pattern`, `data-mask`), else `insertText`. Expose `mode: "keys" | "insert"` for override.

### IME / CJK — `Input.imeSetComposition` (EXPERIMENTAL)

```
Input.imeSetComposition { text, selectionStart, selectionEnd, replacementStart?, replacementEnd? }
```

Verified pinyin flow (`ni` → `nihao` → commit `你好`):

```
imeSetComposition("ni",  2,2)   → compositionstart, compositionupdate("ni"),
                                  beforeinput{insertCompositionText, isComposing:true}, input  (value "ni")
imeSetComposition("nihao",5,5)  → compositionupdate("nihao"), beforeinput, input             (value "nihao")
insertText("你好")               → compositionupdate("你好"), beforeinput, textInput, input,
                                  compositionend(data:"你好")                                (value "你好")
```

Cancelling a composition: `imeSetComposition{text:"", selectionStart:0, selectionEnd:0}` cleanly rewinds to empty with a final `compositionend`.

**⚠️ Quirk:** the terminating `compositionend` had **`isTrusted: false`** in every run, while every other event in the sequence was `isTrusted: true`. A page that gates on `compositionend.isTrusted` would reject the commit. This is a real deviation from a hardware IME. Document it; there is no workaround from CDP.

The API shape should be `type_ime(ref, [{composing:"ni"},{composing:"nihao"},{commit:"你好"}])` so the agent expresses candidate-selection steps, which is what CJK/Korean/Vietnamese testing actually needs.

---

## 7. Drag and drop — three different mechanisms, three recipes

### 7a. Native HTML5 DnD, intercepted (`draggable=true`, `dataTransfer`)

```
Input.setInterceptDrags { enabled: true }          [EXPERIMENTAL]
mouseMoved(src) → mousePressed(src) → mouseMoved ×N past the drag threshold
  ⇒ page fires dragstart; Chrome then emits Input.dragIntercepted { data: DragData }
     and STOPS driving the drag itself
Input.dispatchDragEvent { type:"dragEnter", x,y, data }
Input.dispatchDragEvent { type:"dragOver",  x,y, data }   ← repeat while "moving"
Input.dispatchDragEvent { type:"drop",      x,y, data }
(or "dragCancel"; Input.cancelDragging — note: NOT experimental — clears a stuck drag)
```

Verified: the page saw `dragstart, drag, dragenter(dst), dragover, drop, dragend`, all `isTrusted: true`.
`DragData = { items: DragDataItem[], files?: string[], dragOperationsMask: integer }`,
`DragDataItem = { mimeType, data, title?, baseURL? }`.

### 7b. Fabricated drop with no source (paste-a-payload-onto-a-dropzone)

You can skip the source entirely. Verified:

```json
{"items":[{"mimeType":"text/plain","data":"hello-from-harness"},
          {"mimeType":"text/uri-list","data":"https://example.com"}],
 "dragOperationsMask":1}
```
→ page saw `dragenter/dragover/drop` with `dataTransfer.types === ["text/plain","text/uri-list"]` and `getData('text/plain') === "hello-from-harness"`. This is the fastest and most reliable HTML5-DnD recipe and needs no `setInterceptDrags`.

### 7c. Pointer-based DnD (dnd-kit, react-beautiful-dnd, SortableJS, interact.js)

These libraries **never use HTML5 DnD**. They use `pointerdown` + `setPointerCapture` + `pointermove`. `dispatchDragEvent` does nothing for them. Recipe (verified working against a `setPointerCapture` implementation):

```
mouseMoved(src, buttons:0)
mousePressed(src, buttons:1)
mouseMoved × 8–20 interpolated points with buttons:1   ← MUST exceed the library's activation
                                                          distance (dnd-kit default 8px) and,
                                                          for delay-based sensors, take ≥250ms
mouseReleased(dst, buttons:0)
```
Observed: `pd@x=70`, then `pm` at 100,130,…,310 with pointer capture held, then `pu@310`. Interpolation granularity matters — libraries with a `PointerSensor` `activationConstraint: {delay, tolerance}` need the *timing* too, so pace the moves.

### How to decide (implement as a probe, not a guess)

```rust
enum DragStrategy { NativeHtml5, Pointer, FileDrop }

// inspection-capability probe, isolated world:
//   src.draggable == true  || src.closest('[draggable="true"]')  -> candidate NativeHtml5
//   DOMDebugger.getEventListeners(src) contains "dragstart"       -> NativeHtml5 (strong)
//   listeners contain "pointerdown"/"mousedown" and target or an
//     ancestor has style.touchAction === "none"                   -> Pointer (strong)
//   dst listeners contain "drop" and "dragover"                   -> NativeHtml5 or FileDrop
// If both look plausible: try Pointer first (it is a strict superset of a mouse gesture and
// is harmless if ignored), verify the DOM changed, else fall back to NativeHtml5.
```
Record the chosen strategy in the action log so a failed drag is diagnosable.

---

## 8. File upload

### 8a. `<input type=file>` — `DOM.setFileInputFiles` (not experimental)

```
DOM.setFileInputFiles { files: ["/abs/path.txt"], nodeId | backendNodeId | objectId }
```
Verified: `change` fired with `files.length === 1`, `name === "upl.txt"`, `size === 16`. Paths must be absolute and readable by the **browser** process (matters if the daemon ever sandboxes differently).

### 8b. Buttons that are not file inputs — `Page.setInterceptFileChooserDialog`

```
Page.setInterceptFileChooserDialog { enabled: true, cancel?: boolean [EXPERIMENTAL] }
  → event Page.fileChooserOpened { frameId, mode: "selectSingle"|"selectMultiple",
                                   backendNodeId? [EXPERIMENTAL] }
  → DOM.setFileInputFiles { files, backendNodeId }
```
Verified end to end: a `<button>` that creates a detached `input[type=file]` and calls `.click()` produced `{"frameId":"CB72…","mode":"selectSingle","backendNodeId":13}`, and `setFileInputFiles` with that `backendNodeId` succeeded. Note `backendNodeId` is optional in the schema — handle its absence by falling back to a DOM search for the most recently focused/created file input.

The newer `cancel: true` parameter lets you dismiss a chooser you did not intend to open — use it in the `observe` capability mode so an accidental chooser never wedges the page.

### 8c. Drag-drop upload — `DragData.files`

**Verified**, and this is the cleanest recipe: `Input.dispatchDragEvent{type:"drop", data:{items:[], files:["/private/tmp/upl.txt"], dragOperationsMask:1}}`. The page's `drop` handler received a real `File`: `[{"name":"upl.txt","size":16,"type":"text/plain"}]`, and `FileReader.readAsText` returned `"BROW-UPLOAD-TEST"`. MIME type is inferred by Chrome from the extension.

**Policy:** every upload path is a `mutate`-class action, and per the brief `file upload` parks a background job in `waiting_for_approval`. The approval payload should carry the absolute paths, sizes and SHA-256 of the files.

---

## 9. Dialogs

```
Page.javascriptDialogOpening { url, frameId, message, type, hasBrowserHandler, defaultPrompt? }
Page.handleJavaScriptDialog  { accept, promptText? }
Page.javascriptDialogClosed  { frameId, result, userInput }
```
Verified: `prompt("say?","def")` produced `{"message":"say?","type":"prompt","hasBrowserHandler":true,"defaultPrompt":"def"}`, and `handleJavaScriptDialog{accept:true, promptText:"BROW"}` resolved it.

`beforeunload` verified: navigating away from a page with a `beforeunload` handler produced `{"type":"beforeunload","message":"","hasBrowserHandler":true}` — Chrome deliberately supplies an **empty message** (custom beforeunload text has been ignored by browsers since 2016). The harness must therefore surface "the page is asking to confirm before leaving" without a message, and must **always** answer it, because `Page.navigate` blocks until you do. Default in `observe`/`interact` modes: `accept:true` for `beforeunload`, and **park** for `confirm`/`prompt` in detached jobs.

`hasBrowserHandler: true` means CDP owns the dialog and no native UI is shown — that is what you want, and it is automatic once `Page.enable` is on.

---

## 10. Device emulation — enough for a real phone profile

Verified full profile applied to one target (all read back correctly after `Page.reload`):

```jsonc
Emulation.setDeviceMetricsOverride {
  "width":393, "height":852, "deviceScaleFactor":3, "mobile":true,
  "screenWidth":393, "screenHeight":852,                    // EXPERIMENTAL
  "screenOrientation":{"type":"portraitPrimary","angle":0}
  // also: scale, positionX/Y, dontSetVisibleSize, viewport, scrollbarType,
  //       screenOrientationLockEmulation  (all EXPERIMENTAL)
  // displayFeature / devicePosture are EXPERIMENTAL *and* DEPRECATED in Chrome 151
}
Emulation.setTouchEmulationEnabled { "enabled":true, "maxTouchPoints":5 }
Emulation.setUserAgentOverride     { "userAgent":"…iPhone…", "acceptLanguage":"ja-JP,ja",
                                     "platform":"iPhone",
                                     "userAgentMetadata": { … }  }   // EXPERIMENTAL
Emulation.setTimezoneOverride      { "timezoneId":"Asia/Tokyo" }
Emulation.setLocaleOverride        { "locale":"ja-JP" }               // EXPERIMENTAL
Emulation.setGeolocationOverride   { "latitude":35.6812,"longitude":139.7671,"accuracy":10 }
Emulation.setEmulatedMedia         { "features":[{"name":"prefers-color-scheme","value":"dark"},
                                                 {"name":"prefers-reduced-motion","value":"reduce"}] }
Emulation.setCPUThrottlingRate     { "rate":4 }
Network.emulateNetworkConditions   { "offline":false,"latency":150,
                                     "downloadThroughput":200000,"uploadThroughput":93750,
                                     "connectionType":"cellular3g",
                                     "packetLoss":0 }                 // packetLoss/packetQueueLength/
                                                                      // packetReordering EXPERIMENTAL
Emulation.setIdleOverride          { "isUserActive":true,"isScreenUnlocked":true }
```

Read-back: `innerWidth×innerHeight = 393×852`, `screen = 393×852`, `devicePixelRatio = 3`, `navigator.platform = "iPhone"`, `maxTouchPoints = 5`, `(pointer:coarse) = true`, `(prefers-color-scheme:dark) = true`, `(prefers-reduced-motion:reduce) = true`, `Intl…timeZone = "Asia/Tokyo"`, `Intl.NumberFormat().resolvedOptions().locale = "ja-JP"`, `screen.orientation = "portrait-primary"/0`.

**Two knobs people conflate — verified separate:**
- `navigator.languages` comes from `setUserAgentOverride.acceptLanguage` (my run left it `"en-US,en"` because I passed that, despite `setLocaleOverride("ja-JP")`).
- `Intl.*` resolution comes from `Emulation.setLocaleOverride`.
Set **both**, consistently, or you will produce a browser that no human has.

**Client hints:** `userAgentMetadata` (EXPERIMENTAL) carries `brands`, `fullVersionList`, `platform`, `platformVersion`, `architecture`, `model`, `mobile`, `bitness`, `wow64`, `formFactors`. Caveat observed: `navigator.userAgentData` was `undefined` on a `data:` URL — UA-CH is a **secure-context-only** API. Test fixtures must be served over `http://localhost` or `https://`, not `data:`, or you will chase a phantom.

**Also relevant, mostly EXPERIMENTAL, worth exposing:** `setHardwareConcurrencyOverride`, `setScrollbarsHidden`, `setAutoDarkModeOverride`, `setFocusEmulationEnabled` (keeps the page "focused" while headless/backgrounded — important for background jobs), `setSensorOverrideEnabled` (accelerometer/gyro), `setPressureSourceOverrideEnabled`.

**Banned:** `Emulation.setEmitTouchEventsForMouse` — see §11.

---

## 11. Trust, detection, and what this harness deliberately does *not* do

### `isTrusted`

**Confirmed on every single event type tested**: mouse (over/enter/move/down/up/click/dblclick/auxclick/contextmenu/wheel), touch (start/move/end), pointer, key (down/press/up), `beforeinput`, `input`, `textInput`, `change`, `compositionstart`/`update`, drag (`dragstart`/`drag`/`dragenter`/`dragover`/`drop`/`dragend`) — all `isTrusted: true`.

**The one exception found:** `compositionend` produced by `Input.insertText` terminating a composition had `isTrusted: false`.

This is the core value proposition versus `element.dispatchEvent`, which produces `isTrusted: false` and cannot trigger user-activation-gated APIs (clipboard write, fullscreen, autoplay-with-sound, `window.open`, WebAuthn, file pickers). Say this plainly in `SKILL.md`.

### Detection surface — what we measured

> **⚠️ Corrected 2026-08-04 — the row below is right for `--remote-debugging-port` and WRONG for the transport `brow` actually uses.** `10-cdp-transport-and-process.md` measured the opposite; both dossiers over-generalised from one transport. Full matrix, Chrome 151.0.7922.72, macOS 26.5.1:
>
> | Launch | transport | `navigator.webdriver` |
> |---|---|---|
> | headless, **no** debugging switch (`--dump-dom` control) | none | `false` |
> | headless / headful `--remote-debugging-port` | websocket | `false` |
> | headless `--remote-debugging-port --enable-automation` | websocket | **`true`** |
> | headless **`--remote-debugging-pipe`** | pipe | **`true`** |
> | headful **`--remote-debugging-pipe`** | pipe | **`true`** |
> | pipe `+ --enable-automation` | pipe | `true` (unchanged) |
> | pipe `+ --disable-blink-features=AutomationControlled` | pipe | `false` |
>
> **Corrected again 2026-08-04 (second pass): the `--remote-debugging-port` row is only true for a *fixed, non-zero* port.** Primary source [`content/child/runtime_features.cc`](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/child/runtime_features.cc): lines 387–389 map `switches::kEnableAutomation`, `switches::kHeadless` and `switches::kRemoteDebuggingPipe` to `wrf::EnableAutomationControlled`, and lines 438–445 additionally enable it when `--remote-debugging-port` parses to **`0`** ("*the caller has requested an ephemeral port which is how ChromeDriver launches the browser by default*"). Measured on Chrome 151: `--headless=new --remote-debugging-port=0` → `navigator.webdriver` **`true`**; fixed ports 39471/39472 → `false` (with and without `--no-startup-window`). Add this row to the matrix:
>
> | headless `--remote-debugging-port=0` | websocket | **`true`** |
>
> Note also that `switches::kHeadless` is in the source table yet `--headless=new` + a fixed port measured `false` — most likely because new headless does not put `--headless` on the *renderer's* command line, which is what `runtime_features.cc` reads. Trust the measurements, not the table.
>
> **`--remote-debugging-pipe` sets `navigator.webdriver` unconditionally.** Since the transport decision (`10-…` §1) is pipe-only, **every page `brow` touches will see `navigator.webdriver === true`**, headful included. Correction #1 below ("CDP attachment does not set `navigator.webdriver`") is therefore false for this project, and the "don't pass `--enable-automation`" decision becomes cosmetic — it still suppresses the infobar and the password-save UI, but it does not change the fingerprint. Say so plainly in `SKILL.md`: *sites that gate behaviour on `navigator.webdriver` will treat every `brow` session as automation, and the harness will not lie about it.*

| Probe | Chrome 151 headless, plain `--remote-debugging-port` | headful, plain | headful `--enable-automation` |
|---|---|---|---|
| `navigator.webdriver` | **false** | **false** | **true** |
| `navigator.userAgent` | contains **`HeadlessChrome/151.0.0.0`** | `Chrome/151.0.0.0` | `Chrome/151.0.0.0` |
| `navigator.plugins.length` | 5 | — | — |
| `window.chrome` keys | `loadTimes,csi,app` (no `runtime`) | — | — |
| classic `Runtime.enable` console-serialization probe (getter on `Error.stack` / on an object property, then `console.debug`/`console.log`) | **did not fire**, with or without `Runtime.enable` | — | — |

Two corrections to the common lore, both empirically grounded:
1. ~~**CDP attachment does not set `navigator.webdriver`.**~~ **REFUTED for this project 2026-08-04 (see the matrix above).** Websocket CDP attachment does not set it; **`--remote-debugging-pipe` does**, and that is our transport. `--enable-automation` also sets it (in port mode). `browserd` should still **not** pass `--enable-automation` — for the infobar/password-UI reasons — but must not claim the flag is what determines `navigator.webdriver`.
2. The historically famous `Runtime.enable` side-channel did not reproduce in Chrome 151. Treat that as a moving target, not a guarantee — it is not a claim to build on.

### Stated policy (put this verbatim in the docs)

> `brow` is for authorised testing and inspection of applications **you control**. It performs **no** anti-detection work: it does not patch `navigator.webdriver`, does not spoof plugin/canvas/WebGL fingerprints, does not rotate proxies, and does not solve CAPTCHAs (CAPTCHA is explicit human handoff). What it *does* give you is an honest answer to "why does my app behave differently under automation?": headless UA string, missing `chrome.runtime`, `--enable-automation` if set, `Runtime.enable` artefacts, and — most of all — the input physics below.

**Input physics is the thing you cannot fake away, and should not try to.** Synthesised input differs from human input in: perfectly straight or perfectly eased mouse paths, zero micro-tremor, uniform inter-keystroke intervals, no typo-and-correct, sub-millisecond dwell times, and fling velocities that match a formula. The harness *may* add optional humanisation (Bézier paths, log-normal inter-key delays, `rand` 0.10.2 jitter) purely so hover/gesture UIs behave naturally — that is an **ergonomics** feature, not an evasion feature, and should be documented as such.

### The `setEmitTouchEventsForMouse` landmine

```
Emulation.setEmitTouchEventsForMouse { enabled: true }        [EXPERIMENTAL]
then Input.dispatchMouseEvent { type: "mousePressed", … }
  → mouseMoved returns fine
  → mousePressed NEVER RESOLVES (observed >30s, then browser process gone)
```
Reproduced three times, minimal repro is two calls. This is **crbug 40225266** ("CDP command for mouse click doesn't resolve in emulation mode"), also filed as puppeteer#10513: *"the callback on the input_handler is not executed if the event type is not one of the mouse events when touch emulation is enabled"* — the event **is** delivered to the page, but the CDP response is never sent. Still reproducible in Chrome 151.0.7922.72 in August 2026.

> **Verified 2026-08-04 — hang CONFIRMED, "browser dies" REFUTED, and the blast radius is worse than described in one way and better in another.** Independent reproduction over the pipe:
> ```
> Emulation.setEmitTouchEventsForMouse{enabled:true}   -> {} (0.0 s)
> Input.dispatchMouseEvent{mouseMoved}                 -> {} (0.0 s)   <- still fine
> Input.dispatchMouseEvent{mousePressed}               -> NEVER RESOLVES (15 s timeout)
> ```
> Then, on the *same* session, after the hang:
> ```
> Runtime.evaluate{"1+1"}                              -> OK        <- session is NOT wedged
> Input.dispatchMouseEvent{mousePressed}  (again)      -> hangs
> Input.dispatchMouseEvent{mouseReleased}              -> hangs
> Emulation.setEmitTouchEventsForMouse{enabled:false}
> Input.dispatchMouseEvent{mousePressed}               -> STILL hangs
> Browser.getVersion                                   -> OK        <- browser ALIVE
> ```
> Corrections: (a) **the browser process does not die** — it stayed healthy in every run; the original "browser process gone" observation was probably the harness's own teardown. (b) **The damage is broader than "the next `mousePressed`"**: *every* subsequent `Input.dispatchMouseEvent` on that session hangs, including `mouseReleased`. (c) **Turning the emulation back off does not recover it** — the session's mouse-input path is permanently dead. Recovery requires `Target.closeTarget` + recreate. (d) Non-Input domains keep working, so a naive health check will report the session healthy while every gesture silently times out.
>
> This strengthens the ban, and adds a requirement: the per-command timeout must be paired with a **per-session "input path is dead" latch** that fails fast and recommends target recreation, rather than timing out every gesture for the life of the session.

**Mitigations in `browserd`:** (a) never call it; (b) add a **global per-command timeout** on the CDP transport regardless — a hung command must not wedge a session; (c) `Input.emulateTouchFromMouseEvent` is the explicit alternative if you ever need mouse→touch, but it is EXPERIMENTAL, takes **DIP** (not CSS px) coordinates, supports only `none`/`left`/`right`, and cannot express multi-touch. Prefer `dispatchTouchEvent`.

---

## 12. The actionability algorithm

This is the main thing Playwright gives people and the main thing you must reimplement. Playwright's checks are: **Visible** (non-empty bbox and not `visibility:hidden`; `opacity:0` still counts as visible), **Stable** (same bounding box for two consecutive animation frames), **Receives Events** (element is the hit target at the action point), **Enabled** (not `[disabled]`, not in a disabled `<fieldset>`, no `[aria-disabled=true]` ancestor), **Editable** (enabled and not `[readonly]`/`[aria-readonly=true]`). Applied per action: click/tap/check need V+S+RE+E; hover/drag need V+S+RE; fill needs V+E+Editable; `press`/`setInputFiles`/`focus` need none.

### Proposed `brow` algorithm (per attempt, retried until `timeout`)

```rust
pub struct ActionPoint { pub x: f64, pub y: f64, pub frame_session: SessionId }

async fn resolve_action_point(r: &Ref, need: Checks) -> Result<ActionPoint, NotActionable> {
    // 0. REF VALIDITY — refs are bound to a document generation; a navigation invalidates them.
    r.assert_generation_current()?;                       // else Err(StaleRef)

    // 1. ENABLED / EDITABLE — cheap, from the Unified Page Tree snapshot (no eval).
    if need.enabled  && node.is_disabled()  { return Err(Disabled) }
    if need.editable && node.is_readonly()  { return Err(ReadOnly) }

    // 2. VISIBLE — DOM.getContentQuads on the node's OWN frame session.
    let quads = dom.get_content_quads(r.backend_node_id).await?;
    let quads = quads.into_iter().filter(|q| area(q) > 1.0).collect::<Vec<_>>();
    if quads.is_empty() { return Err(NotVisible) }        // display:none => quads == []
    // visibility:hidden is NOT reflected in quads -> also check computed style from the tree.
    if node.computed("visibility") == "hidden" { return Err(NotVisible) }

    // 3. IN VIEWPORT — compose to main-frame coords, intersect with the layout viewport.
    let main_quads = compose_frame_offsets(&quads, r.frame_chain).await?;
    if !intersects_viewport(&main_quads) {
        dom.scroll_into_view_if_needed(r.backend_node_id).await?;   // may be a no-op; re-read
        continue_to_next_attempt();
    }

    // 4. STABLE — one round trip, in an ISOLATED WORLD, measuring exactly two rAFs.
    //    (isolated world so the page can neither observe nor monkey-patch the probe)
    let stable: bool = eval_isolated(r.frame, r#"
        (async (el) => {
          const raf = () => new Promise(r => requestAnimationFrame(r));
          const a = el.getBoundingClientRect();
          await raf(); await raf();
          const b = el.getBoundingClientRect();
          return a.x===b.x && a.y===b.y && a.width===b.width && a.height===b.height;
        })"#, r).await?;
    if need.stable && !stable { return Err(Unstable) }
    // ⚠ 2026-08-04: this await BLOCKS FOREVER on a backgrounded tab. See the note below.

    // 5. ACTION POINT — centre of the largest quad, CLIPPED to the viewport and to
    //    every scroll-clipping ancestor, so sticky/overflow:hidden cases pick a visible pixel.
    let p = clipped_centre(&main_quads, &r.clip_chain);

    // 6. RECEIVES EVENTS — hit test in the MAIN session (sees parent overlays; an OOPIF
    //    session cannot). Walk up from the hit node: accept if it is the target or a
    //    descendant of it, or the target is a descendant of it (label/shadow host cases).
    if need.receives_events {
        let hit = dom.get_node_for_location(p.x as i64, p.y as i64,
                    /*includeUserAgentShadowDOM=*/true).await?;
        if !hit_is_acceptable(hit.backend_node_id, r) {
            // try the other quads / a few sample points before giving up
            return Err(Occluded { by: describe(hit.backend_node_id).await? });
        }
    }

    Ok(ActionPoint { x: p.x, y: p.y, frame_session: r.frame_session })
}
```

> **⚠ Defect found 2026-08-04 in step 4 as written — the stability probe hangs forever on any page that is not the active tab of its window.** `requestAnimationFrame` does not fire on a `document.visibilityState === "hidden"` page, so `await raf(); await raf();` never resolves and the `Runtime.evaluate{awaitPromise:true}` never returns. Measured on Chrome 151:
> ```
> single page (visible):                       probe returned true in 0.02 s
> after creating a 2nd page in the same
>   BrowserContext (1st becomes "hidden"):     probe TIMED OUT at 12 s, no reply
> ```
> Only one page per browser **window** is `visible` (see `10-…` §4.2 for the full matrix — `Target.activateTarget` swaps which one, `Page.bringToFront` / `Emulation.setFocusEmulationEnabled` / `Page.setWebLifecycleState{active}` and the `--disable-*-backgrounding` flags all fail to help). Three required changes:
> 1. **One `BrowserContext` (or `Target.createTarget{newWindow:true}`) per concurrently-driven page.** Verified: pages in separate contexts are all `visible` at 60 fps.
> 2. **Never issue a bare `awaitPromise` rAF probe.** Race it against a timer inside the page — `Promise.race([twoRafs, new Promise(r => setTimeout(() => r("hidden-no-raf"), 250))])` — so the command always returns, and surface `hidden-no-raf` as a distinct actionability outcome rather than an `Unstable` or a timeout.
> 3. The daemon's per-command deadline must exist regardless (it is already required for the `setEmitTouchEventsForMouse` latch in §11), but a 5 s actionability budget made of 30 s hangs is not a budget.

Retry loop: re-run every attempt (do **not** cache), backoff ~50 ms, default budget 5 s, and on failure return a **structured** reason (`StaleRef | Disabled | ReadOnly | NotVisible | OffScreen | Unstable | Occluded{by}`) plus a screenshot with the action point annotated. That error object is what makes the agent able to self-correct instead of retrying blindly.

### Empirical notes on the stability check

Against a 6 s linear `translateX(0 → 600px)` transition:

| Method | Result mid-animation | Cost |
|---|---|---|
| `getContentQuads`, CDP-awaited double-rAF, `getContentQuads` | detected instability (`x: 1.13 → 62.95`), but the gap was **14–47 ms of wall clock**, not 2 frames | 3 round trips |
| in-page `getBoundingClientRect` across 2 rAFs, one `Runtime.evaluate` | detected instability with the correct granularity (`515.97 → 520.34`, ~4.4 px) | **1 round trip** |

Both work. Prefer the in-page version. **Caveat found the hard way:** on a *short* (1 s, `ease`) transition my first attempt reported `stable=true` at every sample — because by the time of sampling the transition had already completed. Do not conclude from a single stable reading that nothing is animating; combine with a minimum settle time after any action that could start an animation.

Occlusion check verified: with a `z-index:5` overlay covering a button, `DOM.getNodeForLocation{x:110,y:60}` returned the **overlay's** `backendNodeId`, correctly reporting `Occluded`. `ignorePointerEventsNone:true` is the flag for the "the overlay has `pointer-events:none` so it does not really block" case.

---

## 13. Suggested `crates/input` surface

```rust
pub enum Gesture {
    Click   { r: Ref, button: MouseButton, count: u8, modifiers: Modifiers },
    Hover   { r: Ref, path: PathStyle },
    Wheel   { at: Target, dx: f64, dy: f64 },
    Scroll  { at: Target, dx: f64, dy: f64, speed_px_s: u32, fling: bool },  // synthesizeScrollGesture
    Tap     { r: Ref, count: u8, duration_ms: u32 },                          // synthesizeTapGesture
    LongPress { r: Ref, hold_ms: u32 },                                       // hand-rolled touch
    Swipe   { from: Point, to: Point, speed_px_s: u32, fling: bool },
    Pinch   { at: Point, scale: f64, speed: u32 },                            // synthesizePinchGesture
    Drag    { from: Ref, to: Ref, strategy: DragStrategy },
    Type    { r: Ref, text: String, mode: TypeMode },
    Press   { keys: KeyChord },
    Ime     { r: Ref, steps: Vec<ImeStep> },
    Upload  { r: Ref, files: Vec<PathBuf>, via: UploadVia },
}
pub enum PathStyle { Direct, Interpolated { points: u8, easing: Easing }, Human { seed: u64 } }
pub enum TypeMode  { Auto, Keys { delay_ms: Range<u32> }, Insert }
pub enum UploadVia { FileInput, FileChooser, DragDrop }
```

Capability mapping per the brief: `Hover`/`Wheel`/`Scroll` → `interact`; `Click`/`Tap`/`Type`/`Press`/`Drag` → `interact`; `Upload` → `mutate` **and** parks a detached job in `waiting_for_approval`; `Emulation.*` → `control`; `Input.setIgnoreInputEvents` → `control` (verified: with `ignore:true` a full press/release produced **0** page events; `ignore:false` restored delivery — a clean global input gate for "freeze the page while I screenshot").

---

## What we verified empirically

Chrome **151.0.7922.72** (V8 15.1.206.10), macOS Darwin 25.5.0, launched by me with `--headless=new --remote-debugging-port=<port> --user-data-dir=/private/tmp/brow-c*`, plus two headful instances (`--window-position=2000,2000`, one with `--enable-automation`). Driven by a ~70-line dependency-free RFC-6455 WebSocket client in Python (no Puppeteer/Playwright anywhere). All my instances were killed and scratch dirs removed afterwards; other agents' Chrome processes were left alone.

| # | What I ran | Raw observation |
|---|---|---|
| 1 | `curl /json/protocol`, enumerate `Input` | 13 commands. EXPERIMENTAL: `dispatchDragEvent`, `insertText`, `imeSetComposition`, `emulateTouchFromMouseEvent`, `setInterceptDrags`, `synthesizePinchGesture`, `synthesizeScrollGesture`, `synthesizeTapGesture`, event `dragIntercepted`. NOT experimental: `dispatchKeyEvent`, `dispatchMouseEvent`, `dispatchTouchEvent`, `cancelDragging`, `setIgnoreInputEvents`. **Nothing deprecated.** All three `synthesize*` alive in 2026 |
| 2 | Full click vs press-only | Full: over/enter/move/down/up/click. Press-only: over/enter chain still fires but with `buttons:1`, and **no** move event |
| 3 | `modifiers` probe 1/2/4/8/15 | Alt/Ctrl/Meta/Shift respectively; 15 = all |
| 4 | Right-click | `contextmenu` fires between `mousedown` and `pointerup`; `auxclick` with `button 2`. Headful: 3 calls in 53 ms, no blocking |
| 5 | Middle click | `auxclick` `button 1`, no `click`, no `contextmenu` |
| 6 | Pen | `pointerType:"pen"`, `tiltX/tiltY/twist` propagate; `pressure` 0 while hovering |
| 7 | `dispatchTouchEvent` with **no** emulation | **Works** — full touch + click delivered, `isTrusted:true`, despite `maxTouchPoints===0` |
| 8 | `setTouchEmulationEnabled{maxTouchPoints:5}` | `maxTouchPoints 0→5`, `ontouchstart false→true`, `(pointer:coarse) false→true` |
| 9 | 2-finger multi-touch | One `touchstart` per finger; `touches` array grows; `force`/`radiusX` propagate |
| 10 | `synthesizeScrollGesture` fling | `preventFling:false` → 1353 ms, `scrollTop` **733** for a 600 px request (133 px overshoot). `preventFling:true` → 928 ms, exactly **601** |
| 11 | `synthesizeScrollGesture` speed | 500 px at speed 100/800/5000 → **5303/806/260 ms**. `speed` = px/s. Blocking |
| 12 | `synthesizeTapGesture` | `duration:120` → down@763, up@883 (exactly 120 ms). `tapCount:2` → two full cycles 74 ms apart |
| 13 | `synthesizePinchGesture` | 307 ms; two vertical touch points 125 px apart moving apart; client coords compress as page scale changes |
| 14 | `gestureSourceType:"mouse"` scroll | stream of `wheel` events with eased fractional deltas 6.30 → 7.67 → 12.56 |
| 15 | Manual touch swipe | `scrollTop` 335 → **692** over 1.5 s after `touchEnd` — **fling from a hand-rolled point stream**, with and without timestamps |
| 16 | Keyboard forms | `keyDown{text}` → keydown/keypress/beforeinput/textInput/input/keyup. Without `text` → keydown/keyup only |
| 17 | **`rawKeyDown` + `char`(no `code`) + `keyUp`** | **2280 `keydown` events and climbing at ~4 kHz**, `key:"Unidentified"`, `keyCode:0`. Adding `code`+`vk` to the `char` → exactly 1. Browser died in a later run |
| 18 | `insertText("hello wörld")` into `maxlength=10` | value `"hello wörl"` — **maxlength respected**; `beforeinput`/`textInput`/`input` fired, **no key events**. Per-key typing gave the identical truncation |
| 19 | IME pinyin `ni`→`nihao`→`你好` | Full `compositionstart/update/beforeinput/input` chain, `isComposing:true`; final `compositionend` had **`isTrusted:false`** |
| 20 | `commands:["selectAll"]` | selection became `[0,6]` on a 6-char input |
| 21 | `setInterceptDrags` + `dragIntercepted` + `dispatchDragEvent` | Full HTML5 chain delivered; `items:[]` because the fixture never called `setData` |
| 22 | Fabricated `DragData` with no source | `dataTransfer.types === ["text/plain","text/uri-list"]`, `getData` returned the payload |
| 23 | Pointer DnD via plain mouse events | `setPointerCapture` held across 8 interpolated moves |
| 24 | `DragData.files` drop | Page got a real `File{name:"upl.txt", size:16, type:"text/plain"}`; `FileReader` read `"BROW-UPLOAD-TEST"` |
| 25 | `DOM.setFileInputFiles` | `change` fired, correct name/size |
| 26 | `Page.setInterceptFileChooserDialog` on a synthetic `<button>` | `fileChooserOpened{mode:"selectSingle", backendNodeId:13}`; upload via that id succeeded |
| 27 | `prompt()` + `handleJavaScriptDialog` | `{message:"say?", type:"prompt", hasBrowserHandler:true, defaultPrompt:"def"}` |
| 28 | `beforeunload` | `{type:"beforeunload", message:"", hasBrowserHandler:true}` |
| 29 | Full phone profile | All 13 read-back probes correct (see §10) |
| 30 | **`setEmitTouchEventsForMouse` + `mousePressed`** | `mouseMoved` fine; `mousePressed` **never resolves**; browser gone afterwards. Reproduced 3× |
| 31 | `getContentQuads` edge cases | rotated → true parallelogram; `display:none` → `{quads:[]}`; offscreen → viewport-relative, updates after `scrollIntoViewIfNeeded` |
| 32 | Same-origin iframe | main-session quads already in **main-frame** coords; click delivered `clientX/Y = 60,30` to the frame |
| 33 | OOPIF (`localhost:8802` in `127.0.0.1:8801`) | `pierce:true` **cannot see it**; separate `iframe` target; OOPIF quads are frame-local; composition **and** direct OOPIF-session dispatch both worked; `Page.getFrameTree` from main showed no child frames |
| 34 | Occlusion | `getNodeForLocation` returned the covering overlay |
| 35 | Two-frame stability, 6 s linear transition | CDP double-rAF (3 RTT, 14–47 ms) and in-page double-rAF (1 RTT, ~4 px deltas) both detected instability; a 1 s `ease` transition produced false "stable" because it had already finished |
| 36 | `navigator.webdriver` | headless plain **false**; headful plain **false**; headful `--enable-automation` **true** |
| 37 | Classic `Runtime.enable` console-serialization probe | did **not** fire with or without `Runtime.enable` |
| 38 | `mouseWheel` | `deltaY:500` → `scrollY 500`; 20×40 → 1300. 1:1, `deltaMode:0` |
| 39 | `setIgnoreInputEvents` | `ignore:true` → 0 events; `ignore:false` → delivery restored |
| 40 | Chromium source | `dom_us_layout_data.h` 698 lines (BSD), `keyboard_codes_posix.h` 300 lines, `editor_command_names.h` 140 `V()` entries |

---

## Limits and impossibilities — blunt

1. **`Emulation.setEmitTouchEventsForMouse` is unusable.** Not "flaky" — it permanently kills the **whole mouse-input path of that session**: every subsequent `dispatchMouseEvent` (press *and* release) hangs forever, and disabling the emulation does not recover it; only recreating the target does. *(Corrected 2026-08-04: it does **not** take the browser with it — the browser process and all other domains on the same session stay healthy, which makes it harder to detect, not easier.)* crbug 40225266 has been open since 2022 and still reproduces.
2. ~~**`type:"char"` without `code` is a browser-killer** (§5).~~ **Downgraded 2026-08-04: NOT REPRODUCED** across six conditions and both transports on Chrome 151.0.7922.72 — every variant produced exactly 1 `keydown` and the browser survived (§5). Keep the "always populate `code` + `windowsVirtualKeyCode` on `char`" hygiene rule; drop the "browser-killer" framing and do not file upstream without a fresh repro.
3. **`compositionend` from `insertText` is `isTrusted:false`.** You cannot produce a fully hardware-faithful IME commit from CDP. Sites that check it will reject.
4. **Fling velocity from hand-rolled touch streams is not deterministic.** Chrome derives it from event arrival; you cannot fully pin it even with explicit `timestamp`. Use `synthesizeScrollGesture` when determinism matters. (Whether `timestamp` is honoured for velocity is **unverified** — both runs gave similar results, which is weak evidence either way.)
5. **All three `synthesize*` gestures are EXPERIMENTAL.** They can change or vanish in any Chrome release. Version-gate them behind a capability probe against `/json/protocol` at daemon startup and keep a hand-rolled fallback for each.
6. **`DOM.getContentQuads` is EXPERIMENTAL** and it is the single most load-bearing call in the whole input path. Same mitigation: probe at startup, fall back to isolated-world `getClientRects()` (loses nothing except a round trip, but *does* lose the guarantee that the page cannot lie to you).
7. **OOPIFs break `pierce:true`.** Any design that assumes one DOM tree per page is wrong. This affects the Unified Page Tree dimension as much as input.
8. **Native browser UI is unreachable and always will be.** OS context menus, the omnibox, print dialogs, the download shelf, permission bubbles, Keychain, Touch ID, `<select>` popups on macOS (rendered by the OS), and CAPTCHA. Per the brief these are human handoff — hold that line, do not let "just one screenshot of the print dialog" creep in.
9. **Browser-level accelerators are not deliverable via `Input.dispatchKeyEvent`.** Expose explicit verbs instead (§5).
10. **You cannot make synthesised input statistically indistinguishable from human input**, and this project explicitly does not try. Say so.
11. **Headless UA leaks `HeadlessChrome`.** Overriding it with `setUserAgentOverride` is legitimate device emulation, but be aware you are then lying about one thing while `--headless=new` still differs in others (no GPU compositing by default, different `screen` metrics, no window chrome).
12. **`data:` URLs are not secure contexts**, so `navigator.userAgentData`, clipboard, and geolocation behave differently there. Fixtures must be served over `http://localhost`.
13. **Anything time-based is dead on a backgrounded tab.** Only one page per browser window has `document.visibilityState === "visible"`; every other tab in that window gets `requestAnimationFrame` at **0 fps**. That silently breaks the actionability stability probe (§12 — verified to hang past 12 s), CSS-transition settling, `synthesize*` gesture pacing assumptions, and screencast. No flag or CDP command un-hides a background tab. The only fix is one browser window — i.e. one `BrowserContext`, or `newWindow:true` — per concurrently-driven page. Verified 2026-08-04; see `10-…` §4.2.
14. **`clickCount` is honoured mechanically, not temporally.** That is convenient, but it means the harness never exercises the *real* double-click timing path — a page with its own `dblclick` polyfill keyed on timestamps will behave differently under `brow` than under a human. Offer `Tap{count:2}`/`Click{count:2}` with an optional real inter-click delay.

---

## Open questions for the owner

1. **Puppeteer's keymap:** confirm that code-generating from Chromium BSD sources (my recommendation) is preferred over vendoring the Apache-2.0 `USKeyboardLayout.ts` table with a NOTICE file. The latter is legally fine and saves ~2 days; it just reads badly against "no Puppeteer anywhere".
2. **Non-US layouts.** The brief says "US-layout key map". Do we need AZERTY/QWERTZ/JIS at all, or is `insertText` the answer for every non-US case? A full layout engine is weeks of work.
3. **Humanised input** (Bézier paths, log-normal keystroke intervals, micro-tremor): ship it as an ergonomics option (`PathStyle::Human`), or omit it entirely so nobody can mistake the project for evasion tooling?
4. **`synthesize*` blocking calls** can occupy a session for seconds. Should long gestures be first-class **jobs** (with `status`/`stop`) rather than synchronous `browserctl` calls?
5. **Should `brow` file the two Chrome bugs upstream** (the `char` storm; a nudge on 40225266)? I have minimal repros for both.
6. **Right-click policy:** headful right-click may paint a native OS menu that will appear in screen recordings. Do we (a) accept it, (b) always `Input.setIgnoreInputEvents` around it, or (c) restrict `contextMenu` to headless sessions?
7. **Default `mode` for `type()`** — is the listener-probing `Auto` heuristic worth the extra `DOMDebugger.getEventListeners` round trip on every call, or should the agent be forced to choose?
8. **Actionability budget:** 5 s default, or shorter with an explicit `wait_for` verb so the agent reasons about waiting instead of the harness hiding it?

---

## Sources

1. https://chromedevtools.github.io/devtools-protocol/tot/Input/ — Input domain reference (tot), fetched 2026-08-04
2. Local Chrome 151.0.7922.72 `http://127.0.0.1:<port>/json/protocol` — authoritative live schema, dumped and diffed against (1)
3. Local Chrome 151.0.7922.72 `/json/version` — `Chrome/151.0.7922.72`, V8 `15.1.206.10`, protocol 1.3
4. https://issues.chromium.org/issues/40225266 — "CDP command for mouse click doesn't resolve in emulation mode" (login-walled; content obtained via search summary and puppeteer#10513)
5. https://github.com/puppeteer/puppeteer/issues/10513 — "[Bug]: use cdp mouse click causes timeout error"
6. https://raw.githubusercontent.com/puppeteer/puppeteer/main/packages/puppeteer-core/src/common/USKeyboardLayout.ts — Apache-2.0 header, `KeyDefinition`, 217 entries
7. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/ui/events/keycodes/dom_us_layout_data.h — BSD-3-Clause, `kPrintableCodeMap`, `kNonPrintableCodeMap`, DomCode→VKEY (698 lines)
8. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/ui/events/keycodes/keyboard_codes_posix.h — numeric `VKEY_*` values (300 lines)
9. https://source.chromium.org/chromium/chromium/src/+/main:ui/events/keycodes/dom/keycode_converter_data.inc — USB HID ↔ DomCode ↔ native scancode
10. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/third_party/blink/renderer/core/editing/commands/editor_command_names.h — 140 legal `dispatchKeyEvent.commands` values
11. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/LICENSE — Chromium BSD-3-Clause
12. https://playwright.dev/docs/actionability — actionability check matrix and definitions
13. https://crates.io/api/v1/crates/keyboard-types — 0.8.3, updated 2025-10-02
14. https://docs.rs/keyboard-types/0.8.3/keyboard_types/ — MIT OR Apache-2.0; `Code`/`Key`/`Location`/`Modifiers`/`KeyState`/`CompositionEvent`; no legacy VK codes
15. https://crates.io/api/v1/crates/{unicode-segmentation,serde_json,tokio,rand,simple-easing,nix} — versions 1.13.3 / 1.0.151 / 1.53.1 / 0.10.2 / 1.0.2 / 0.31.3, all checked 2026-08-04
16. https://datadome.co/threat-research/how-new-headless-chrome-the-cdp-signal-are-impacting-bot-detection/ — CDP/headless detection signals (context only; my own measurements supersede its `navigator.webdriver` claim)
17. https://scrappey.com/qa/anti-bot/what-is-cdp-detection — `Runtime.enable` console-serialization probe description (did not reproduce on Chrome 151)
18. https://blog.send.win/headless-browser-detection-methods-browser-isolation-guide-2026/ — 2026 detection landscape (secondary)

---

## Verification pass — 2026-08-04 (adversarial review)

Re-run against **Google Chrome 151.0.7922.72** on macOS 26.5.1, primarily over `--remote-debugging-pipe` (the transport this project will actually use) with `--remote-debugging-port` used as a control. Fixtures served over `http://127.0.0.1` and, where the original used them, `data:` URLs. All processes killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| `navigator.webdriver` is `false` under plain CDP; `--enable-automation` sets it | **REFUTED for our transport** | pipe → `true` (headless *and* headful); port → `false`; `--enable-automation` (port) → `true`; no-CDP control → `false`. See §11 matrix |
| `type:"char"` without `code` triggers an unbounded keydown storm that kills the browser | **NOT REPRODUCED** | 6 conditions × both transports, all gave exactly 1 `keydown`, browser alive. Guidance retained as hygiene; "browser-killer" removed (§5) |
| `Emulation.setEmitTouchEventsForMouse` hangs `mousePressed` and kills the browser | **PARTIAL — hang CONFIRMED, death REFUTED, scope WIDER** | `mousePressed` never resolves (15 s); *all* later mouse events on that session hang; disabling does not recover; `Runtime.evaluate` and `Browser.getVersion` still OK (§11) |
| `Input.dispatchTouchEvent` works without `Emulation.setTouchEmulationEnabled` | **CONFIRMED** | Fresh target with `navigator.maxTouchPoints === 0`, `'ontouchstart' in window === false`: `touchStart`/`touchEnd` delivered `pointerdown, touchstart, touchend, click`, all `isTrusted:true` (§3) |
| `insertText` respects `maxlength`, equivalent to per-key | **CONFIRMED, narrower than it reads** | `<input maxlength=10>` → `"hello wörl"`; but `beforeinput`/`textInput` carry the **full** untruncated `data`, and on `contenteditable` all 16 chars were inserted (§6) |
| `compositionend` from `insertText` has `isTrusted:false` | **CONFIRMED** | Full pinyin chain `compositionstart/update/beforeinput/input` all `isTrusted:true`; terminating `compositionend` `isTrusted:false`, `data:"你好"` (§6) |
| `synthesizeScrollGesture` blocks through the entire fling | **CONFIRMED (independent run)** | Returned at 1.292 s with `scrollTop=730`; still 730 after a further 1.5 s (§4) |
| CDP-synthesised mouse input is `isTrusted:true` | **CONFIRMED** | `pointerdown … click` all `isTrusted:true` |
| `Input` domain experimental/stable split | **CONFIRMED** | Chrome 151 `/json/protocol`: experimental = `dispatchDragEvent, insertText, imeSetComposition, emulateTouchFromMouseEvent, setInterceptDrags, synthesizePinchGesture, synthesizeScrollGesture, synthesizeTapGesture`; stable = `dispatchKeyEvent, dispatchMouseEvent, dispatchTouchEvent, cancelDragging, setIgnoreInputEvents`. `DOM.getContentQuads` experimental: `true` |

**Not re-tested:** modifier bitmask probe, right/middle/pen variants, multi-touch splitting, `synthesizeTapGesture`/`synthesizePinchGesture` timings, drag recipes, file upload paths, dialogs, the full device-emulation profile, `setIgnoreInputEvents`, the actionability algorithm, keymap codegen feasibility.

---

## Verification pass 2 — 2026-08-04 (second adversarial review)

Chrome **151.0.7922.72**, macOS Darwin 25.5.0, over `--remote-debugging-pipe` with `--remote-debugging-port` as a control, fixtures served from `http://127.0.0.1:8899`. All processes killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| The §12 actionability algorithm as written is sound | **REFUTED at step 4** | The in-page double-`rAF` stability probe returned `true` in 0.02 s on a visible page and **never returned** (12 s timeout) once a second page in the same BrowserContext made the first `hidden`. `rAF` does not fire on hidden pages. Fix: race the probe against an in-page timer, and give every concurrently-driven page its own window/BrowserContext (§12) |
| `navigator.webdriver` matrix: "port mode → `false`" | **PARTIAL — only for a fixed non-zero port** | `content/child/runtime_features.cc:438-445` also enables `AutomationControlled` for `--remote-debugging-port=0`. Measured: port 0 → `true`, ports 39471/39472 → `false`. Pipe headless/headful → `true` (reproduced) (§11) |
| Mechanism behind the pipe/webdriver coupling was only observational | **now CONFIRMED from primary source** | `runtime_features.cc:387-389`: `{EnableAutomationControlled, kEnableAutomation}`, `{…, kHeadless}`, `{…, kRemoteDebuggingPipe}` (§11) |
| `Input` domain experimental/stable split, `DOM.getContentQuads` experimental | **CONFIRMED** | Re-read from this Chrome's `/json/protocol`: `DOM.getContentQuads` `experimental:true`; `Input.synthesizeScrollGesture` `experimental:true` with params `x, y, xDistance, yDistance, xOverscroll, yOverscroll, preventFling, speed, gestureSourceType, repeatCount, repeatDelayMs, interactionMarkerName` |
| `Input.dispatchMouseEvent` coordinates are "visual viewport CSS px" (§1) | **PARTIAL — they are LAYOUT viewport CSS px** | Under `Emulation.setPageScaleFactor{2.0}` the visual viewport shrank to 400×300 (`scale:2`) while the layout viewport stayed 800×600. A click dispatched at the element's `getContentQuads` centre `(250,425)` arrived as `clientX/clientY = 250,425` — unchanged by page scale. Quads and `getBoxModel` likewise tracked `getBoundingClientRect()` exactly. Reword §1 to say **layout** viewport |
