# brow — Research Corpus README

**Date:** 2026-08-04 · **Target:** Google Chrome 151.0.7922.72 (V8 15.1.206.10, protocol 1.3, 57 domains) · **Host:** macOS Darwin 25.5.0

---

## 1. What this is

Seven deep-research dossiers, each written against the **live local Chrome over a real `--remote-debugging-pipe` connection**, then put through an adversarial verification pass that re-ran the load-bearing claims and corrected the ones that were wrong. Roughly 5,700 lines. Almost nothing here is reasoned-from-docs; where it is, it is labelled `UNVERIFIED` or `LIKELY`.

The verification pass matters more than the original research. It **refuted seven headline claims** and materially narrowed six more — including three that would have shipped as design bugs (the non-affine iframe guard that could not fire, the isolated-world framework adapters that cannot work, the `Fetch` mutation backstop that leaks through service workers).

### Index

| File | Dimension | One line |
|---|---|---|
| `10-cdp-transport-and-process.md` | Transport, process, targets, sessions | Pipe wire format from Chromium source + 18 probes; the pipe-holder owns the browser's life, which forces the supervisor design |
| `30-unified-page-tree.md` | DOM + Shadow + iframes + AX + CSS + listeners | The "ONE node model" is N sessions × 3 calls stitched daemon-side; closed shadow roots are visible to CDP but not JS; token economics are brutal |
| `40-input-synthesis.md` | Mouse, touch, gesture, keyboard, IME, drag, upload | Everything in the spec is achievable and `isTrusted:true`; three landmines, one permanently-wedging CDP call |
| `50-capture-screenshots-and-video.md` | Screenshots, node/frame shots, screencast, diff | Coordinate spaces are the trap; no texture ceiling; screencast timestamps are host wall-clock, making action↔video sync nearly free |
| `60-event-streams-console-network-performance.md` | Console, network, WS/SSE, HAR, vitals, tracing, mutations | Four of the brief's assumptions are wrong; console and Log are disjoint; vitals and mutations must be injected observers |
| `80-security-capabilities-and-policy.md` | Capabilities, policy, redaction, injection, sandbox | `throwOnSideEffect` is a real fail-closed read-only boundary; isolated worlds are not; prompt injection is budgetable, not solvable |
| `90-crawler-and-site-graph.md` | State-aware crawling, site graph, coverage | Crawljax/Burp got the model right in 2008; state identity is unsolved; route extraction works for React Router + Nuxt and is impossible for Next App Router / SvelteKit / prod Angular |

There is no `20-` or `70-` dossier. Two dimensions were folded into their neighbours: process/lifecycle into `10-`, and framework adapters into `30-` §8 + `90-` §3.

### State of the tree

The repo is **not** empty. Two commits exist (`7899d6b` iteration 1, `9285b29` iteration 2) carrying ~7.3K lines of a single-crate walking skeleton: pipe transport, `setsid()`-detached daemon, browser discovery, `DOMSnapshot`-backed snapshot with `@node-N` refs, the full gesture set, capture, console/network with ingest-time redaction, `throwOnSideEffect` eval, and e2e tests against a real Chromium. It does **not** have: the supervisor process, `Target.setAutoAttach` / OOPIF sessions, jobs, video, the crawler, policy, or the `crates/` split. Milestone M1 below is therefore partly banked — see §7.

---

## 2. Bottom line — can brow be built as specified?

**Yes, with four amendments to the spec. None of them is fatal; all four must be made before code, because each changes a crate boundary or a public promise.**

1. **"Persistent background Chromium" + "restartable daemon" + "no TCP port" needs a fourth process.** Chrome exits `rc=0` within **0.053 s** of the fd-3 pipe reaching EOF (measured twice). Whoever holds the pipe owns the browser's lifetime. A per-browser `setsid()`-detached supervisor that owns fds 3/4 and re-exports over a `0600` unix socket is the only way to have all three. Confirmed to survive `launchctl bootout` on macOS; on Linux `setsid()` is **not** sufficient (cgroup kill) and it must be a `systemd-run --user --scope` transient unit. The resync protocol on top is undesigned — that is the actual hard part.

2. **The "ONE node model" is a daemon-side data structure, not a CDP call.** `DOMSnapshot.captureSnapshot` returns nothing for cross-origin frames; `DOM.getDocument{pierce:true}` gives the `<iframe>` node with no `contentDocument`; `Accessibility.getFullAXTree` crosses **no** iframe, not even same-origin. Cost is O(frames) round trips and a stitching invariant that must be tested, not conventional.

3. **"Raw CDP is never exposed to the agent" survives, but `inspect.evaluate` in an isolated world cannot do framework route extraction.** Verified twice: expandos (`__reactFiber$*`, `__vue_app__`) and route globals (`__reactRouterManifest`) are invisible from an isolated world because Blink gives each world its own V8 wrapper. Framework enrichment needs a **third** evaluate tier: harness-authored fixed strings in the main world (`inspect.routes`), page-observable and page-tamperable, labelled as such. The agent still never supplies code.

4. **The crawler must be sold as best-effort read-only, never as containment.** The `Fetch.requestPaused` block on non-idempotent methods — described in the original research as "the only sound layer" — is measurably leaky: service-worker-originated `fetch()` produces **zero** pause events on a page session (POST reached the server, 200), mutating GETs sail through by construction, WebSocket handshakes are invisible, and `localStorage.clear()` / `indexedDB.deleteDatabase()` are untouchable. The fix (auto-attach + `Fetch.enable` on every `service_worker`/`worker` session) closes the biggest hole and is verified working, but the guard is still not sound.

Two harder truths that do not change the architecture but must change the copy:

- **Every page brow touches sees `navigator.webdriver === true`**, unconditionally, because `--remote-debugging-pipe` sets it (websocket mode does not). This is not opt-out-able without shipping an evasion flag. Say it in SKILL.md.
- **Route COVERAGE is only computable against *declared* routes with per-source provenance.** Coverage over parameterised routes is not merely hard, it is not meaningful. The spec's own instinct ("report coverage and provenance rather than pretending a crawl is exhaustive") is exactly right and the research supports it with numbers.

**What is genuinely out of reach and must be removed from any promise:** canvas/WebGL/Flutter-without-semantics content enumeration (pixels only), automating the user's existing logged-in Chrome profile, per-frame screenshots of OOPIFs as isolated images, hardware-faithful IME commit, WebSocket close reasons, HAR `cache` fields, memory-leak detection, deterministic/reproducible crawls, and proof that no exfiltration occurred.

---

## 3. Architecture Decision Record

Each decision below is settled by the corpus. Confidence: **confirmed** = measured or read from source; **likely** = strong secondary evidence; **open** = the research names the experiment that would settle it.

### ADR-01 — Transport is `--remote-debugging-pipe` in ASCIIZ (NUL-delimited JSON) mode, and nothing else
**Why:** no `listen()` means no other local process can attach; single client by construction; the fds die with their holder. Wire format read from `devtools_pipe_handler.cc` and verified live. CBOR mode is labelled *"Experimental (!)"* in-source and has **no** inbound size cap at all (`PipeReaderCBOR` never calls `set_max_buffer_size`), which is a worse DoS profile.
**Rejected:** `--remote-debugging-port` + WebSocket — reachable by every local process, no auth, defeats the no-raw-CDP invariant. Keep it behind `brow doctor --unsafe-open-port` at most (pipe and port were verified to coexist).
**Confidence:** confirmed · `10-` §1

### ADR-02 — Write our own CDP client; generate types in `build.rs` from vendored `browser_protocol.json` + `js_protocol.json`
**Why:** there is **no runtime protocol descriptor over a pipe**. `Schema.getDomains` 404s on the browser session, returns 35 bare domain names at a frozen version "1.2" on a page session (count varies with which agents are instantiated), and is `deprecated`. `/json/protocol` needs the HTTP port we refuse to open. `protocolVersion` has been "1.3" for years. So pin `r1672245` (2026-08-01), record it in `protocol/PINNED.toml`, feature-gate at runtime by milestone parsed from `Browser.getVersion.product`, and discover missing methods lazily from `-32601`.
**Rejected:** `chromiumoxide` / `headless_chrome` (bundle their own launcher/handler layer); runtime `Schema.getDomains`.
**Confidence:** confirmed · `10-` §9

### ADR-03 — Flat protocol everywhere; one process-wide monotonic `u64` id; one writer task
**Why:** non-flat (`Target.sendMessageToTarget`) is deprecated (crbug.com/991325). Two concurrent commands with `id:99` on different sessions both returned correctly, distinguishable only by echoed `sessionId` — so per-session id namespaces buy nothing and add correlation bugs. Interleaved writes from two tasks corrupt the stream; serialize behind one writer fed by an mpsc channel.
**Confidence:** confirmed · `10-` §2

### ADR-04 — A per-browser `brow-supervisor` process, `setsid()`-detached, owns fds 3/4 and re-exports CDP over a `0600` unix socket
**Why:** pipe EOF → Chrome exits `rc=0` in 0.053 s. Without a separate holder, every `launchd`/`systemd` restart of `browserd` kills every browser. The supervisor is also the only component that touches raw framing, so "no raw CDP to the agent" becomes a **process boundary**, not a code convention.
**Platform detail:** macOS confirmed empirically — after `launchctl bootout`, the `setsid()`'d grandchild lived and the same-pgid control died; `man 5 launchd.plist` says reaping is PGID-based and `AbandonProcessGroup` is not even required. **Linux refuted from primary source** — `systemd.kill(5)` defaults to `KillMode=control-group`, and process groups are irrelevant to cgroups; use `systemd-run --user --scope --unit=brow-sup-<id>` (or D-Bus `StartTransientUnit`). Windows job objects are the same class of risk (`CREATE_BREAKAWAY_FROM_JOB`), untested.
**Rejected:** browserd holds the pipe (browsers die on restart); TCP port (security); `SCM_RIGHTS` fd handoff (works for planned handoffs only, not crash recovery, unverified).
**Confidence:** confirmed on macOS / refuted-and-corrected on Linux / **the reconnect-and-resync protocol is open** · `10-` §8.2

### ADR-05 — Recursive `Target.setAutoAttach{autoAttach, waitForDebuggerOnStart:true, flatten:true, filter}` re-armed in exactly one `on_attached()` hook
**Why:** auto-attach reaches only *immediate* children, so A→B→C needs three calls. A cross-origin iframe is completely invisible from the parent session (`iframe.contentDocument === null`, verified). A missed re-arm silently drops an entire subtree — the worst possible failure mode for a tool selling "complete introspection". `waitForDebuggerOnStart:true` costs a round trip per target and is mandatory: without it you race the renderer and lose early network/console events, and a service worker can issue requests before `Fetch.enable` lands.
**Corollary the corpus flags as missing and cheap:** compare the count of `<iframe>`/`<frame>` elements per session against attached iframe targets + same-process `contentDocument`s, and report the delta as a coverage gap. Without this, "route COVERAGE and provenance" has no foundation at the frame level.
**Confidence:** confirmed · `10-` §3.3, `30-` §5

### ADR-06 — Per-job isolation via `Target.createBrowserContext{disposeOnDetach:true}`; durable logins live in named `--user-data-dir` profiles
**Why:** measured full cookie + localStorage + permission isolation; `Storage.*` and `Browser.setPermission` are `browserContextId`-scoped; an empty context is ~free and a live page costs ~149 MB with or without its own context, versus ~505 MB + 6 processes for a second browser. **But BrowserContexts cannot be persisted to disk** — only the default context in the user-data-dir is durable. So named profiles are a first-class concept, not an escape hatch, and they are the only mechanism behind the site mapper's auth-branch ambition.
**Confidence:** confirmed · `10-` §4

### ADR-07 — Node ref = daemon-allocated `@nN` over `(target_id, frame_id, doc_generation, backend_node_id)` + a re-resolution fallback; never trust `backend_node_id` alone
**Why:** this is the **highest-severity correctness risk in the whole corpus**. `nodeId` dies on reparent. `backendNodeId` survives reparent *and survives same-process navigation* — after which `DOM.describeNode`, `DOM.getBoxModel` and `DOM.scrollIntoViewIfNeeded` all **succeed** and return byte-identical stale geometry with no error (reproduced deterministically on two independent fixtures). Only `DOM.resolveNode` errors, with `-32000` — match on the **code**, not the message. Without a generation counter the harness will eventually click coordinates from the previous page. `backendNodeId` is also only unique per renderer process, so `(target_id, backend_node_id)` is the primary key.
**Also:** always `Runtime.releaseObjectGroup` after an inspection batch — retained `RemoteObject`s keep stale nodes resolvable.
**Confidence:** confirmed · `30-` §1

### ADR-08 — Unified tree = `DOMSnapshot.captureSnapshot` (bulk) + `DOM.getDocument{pierce:true}` (structure/identity) + per-frame `Accessibility.getFullAXTree` (enrichment), stitched via `DOM.getFrameOwner` called from the **parent** session
**Why:** 211 ms for 19,707 nodes on Wikipedia, replacing ~16K per-node round trips; struct-of-arrays with a shared string table. AX is an *enrichment layer* keyed by `backendDOMNodeId`, not the base — raw `getFullAXTree` on Wikipedia was **9.28 MB**, larger than both the snapshot (4.01 MB) and `getDocument{pierce}` (3.86 MB); stripping `name.sources`/`chromeRole`/`ignoredReasons` halves it. Useful refinement from verification: `getFullAXTree{frameId}` works for **same-origin** children from the same session, so the per-frame cost is one AX call per frame in the process — only true OOPIFs need their own session.
**Confidence:** confirmed · `30-` §2, §3

### ADR-09 — Cross-frame coordinates via an affine basis from the owner iframe's **content** quad ÷ the child's `clientWidth/Height`, guarded by a both-axes parallelogram test
**Why:** verified exact for translate/scale/rotate/skew (predicted click landed within 1 px on `rotate(25deg) scale(0.6)`); naive origin-addition missed a scaled iframe entirely. **The guard originally proposed was mathematically wrong** — it compared only x-components, so a `perspective:400px` + `rotateY(45deg)` frame passed as "affine" and the click landed 32 px off, on the document instead of the button. Correct test: `TL→TR` must equal `BL→BR` in **both** components. `perspective` + `rotateY` is the standard card-flip idiom, so `E_NONAFFINE_FRAME` will fire in the wild; a 4-point homography (~30 lines) would actually solve it and is the right follow-up.
**Confidence:** confirmed (algorithm and the corrected guard, both measured) · `30-` §5

### ADR-10 — Agent-facing output is tiered: compact line-oriented interactive view by default (viewport-scoped, paged) → fat node ≤ 900 tokens on demand → raw dumps to artifacts with server-side query
**Why:** measured. github.com: 253 elements = 22,343 bytes ≈ 6.0K tokens compacted, versus 168–246K raw. One raw fat node on github.com = ~309K tokens (`CSS.getMatchedStylesForNode` 985 KB, of which `inherited` alone was 1,062,842 bytes across 11 entries; computed style = 2,465 properties, 1,984 of them custom properties). Even the compacted view is ~39.6K tokens on a Wikipedia article, so **paging and viewport scoping are architectural, not cosmetic**, and `page find` (backed by `Accessibility.queryAXTree{role}` = 15.5 KB / 17 nodes) must be the encouraged entry point. Note `queryAXTree{accessibleName}` is **exact match** — "Star" returned 0 against "Star this repository" — so fuzzy search is harness-side.
**Confidence:** confirmed · `30-` §11

### ADR-11 — All input points resolve through `DOM.getContentQuads` in main-frame viewport CSS px; every click is `mouseMoved` → `mousePressed` → `mouseReleased`
**Why:** quads are true rotated parallelograms (not bboxes), `{quads:[]}` for `display:none`, live and viewport-relative, and already composed to main-frame coordinates for same-origin iframes. CSS px are DPR-independent (a click at 196,400 under `deviceScaleFactor:3` arrived as `clientX/Y 196,400`). Skipping `mouseMoved` still synthesizes over/enter but with an impossible `buttons:1` and **no** `mousemove` at all, which breaks hover menus, drag thresholds, tooltip timers and dnd-kit activation constraints.
**OOPIF:** compose offsets in the parent for hit-testing (only the parent can see a parent-page overlay), dispatch on the frame's **own** session with frame-local coords. Both verified.
**Confidence:** confirmed · `40-` §1, §2

### ADR-12 — `Emulation.setEmitTouchEventsForMouse` is banned from the codebase; mobile = `setTouchEmulationEnabled` + explicit `dispatchTouchEvent`
**Why:** after enabling it, `mousePressed` never resolves. Verification widened the blast radius: **every** subsequent `Input.dispatchMouseEvent` on that session hangs including `mouseReleased`, turning the emulation back off does **not** recover it, and only `Target.closeTarget` + recreate does. The browser process stays healthy and `Runtime.evaluate` keeps answering — so a naive health check reports the session fine while every gesture times out. crbug 40225266, open since 2022.
**Design requirement this forces:** a global per-command CDP timeout **plus** a per-session "input path dead" latch that fails fast and recommends target recreation.
**Also confirmed, contradicting folklore:** `dispatchTouchEvent` works *without* touch emulation. Emulation is a **feature-detection** prerequisite (sites gate handler binding on `'ontouchstart' in window`), not a delivery one.
**Confidence:** confirmed · `40-` §11

### ADR-13 — Keymap code-generated from Chromium's BSD-3-Clause `dom_us_layout_data.h` + `keyboard_codes_posix.h`, not vendored from Puppeteer
**Why:** no crate supplies `code` + `key` + `windowsVirtualKeyCode` + `text` together (`keyboard-types` 0.8.3 has everything but the VK codes — exactly the gap). Same provenance as the browser being driven, regenerable per Chrome milestone, and it satisfies the letter *and* spirit of "no Puppeteer anywhere". Vendoring the Apache-2.0 `USKeyboardLayout.ts` table is legally fine and saves ~2 days; it just reads badly.
**Confidence:** confirmed · `40-` §5

### ADR-14 — Actionability is our problem: ref-generation → enabled/editable from the tree → quads → viewport → **in-page isolated-world two-rAF stability** → clipped action point → hit test in the main session; structured failure reasons, never a bare timeout
**Why:** this is the single largest thing Playwright absorbs. Measured: the CDP-awaited double-rAF costs 3 round trips and spans 14–47 ms of wall clock; the in-page version is 1 RTT with correct ~4 px frame granularity, and running it in an isolated world means the page cannot observe or patch the probe. Failure must return `StaleRef | Disabled | ReadOnly | NotVisible | OffScreen | Unstable | Occluded{by}` plus an annotated screenshot, because that is what lets an agent self-correct instead of retrying blindly. Caveat found the hard way: a single "stable" reading proves nothing on short transitions — pair it with a minimum settle time after any action that could start an animation.
**Confidence:** confirmed · `40-` §12

### ADR-15 — Full page = `scrollTo(0,0)` + `captureBeyondViewport:true` + explicit clip from `cssContentSize`; node shot = `getContentQuads` → union bbox → **add `cssLayoutViewport.pageX/pageY`** → clip
**Why:** two coordinate spaces, and mixing them is the most likely geometry bug in the project. `getContentQuads` is **viewport**-relative (goes negative); `captureScreenshot.clip` is **page**-absolute (byte-identical PNGs at scrollY 0 vs 1000). Omitting the scroll add passes every test written at scroll 0 and corrupts silently later. Use `getBoxModel(iframe).content` — not `getContentQuads` — for the frame offset chain (quads return the **border** box; measured 10 px difference on all sides). `position:fixed` is **not** duplicated in a full-page shot (reproduced on two independent fixtures: exactly one band, at `y == scrollY`), so `scrollTo(0,0)` is the entire fix. Cap `max_capture_megapixels` (~120 MP default) — there is no protocol-level ceiling, and one oversized capture in a shared daemon can take down every session.
**Confidence:** confirmed · `50-` §1, §2

### ADR-16 — Video = screencast JPEG → frame directory first → optional ffmpeg mux; the JSONL action log is the timing source of truth
**Why:** three rungs (ffmpeg via `ffmpeg-sidecar` → pure-Rust MJPEG-in-MP4 via `mp4-atom` → frames + manifest), rung 3 written first so a crash mid-recording still leaves usable artifacts, and because **an agent can read frames directly** — arguably the most useful rung for the actual consumer. No pure-Rust rung produces a universally-playable video in 2026 (MJPEG-in-MP4 does not play in Chrome or Firefox; `rav1e` is too slow; `vpx-encode` last touched 2022). ffmpeg's obvious invocations silently drop 63% of frames — only `-fps_mode passthrough` preserves all of them, and `yuv420p` needs an even-dimension filter or libx264 fails with `-22`. Video PTS quantizes to ~20 ms in **both** MP4 and WebM (it is the concat demuxer's timebase), while the action log is accurate to ~2 ms — state both numbers, never conflate them. `screencastFrame.sessionId` is documented as "Frame number" and is **constant 1**; maintain your own `frame_index` and never claim a recording is complete.
**Clock:** screencast and Input timestamps are already host wall-clock epoch seconds (measured 1.5 ms skew); `requestWillBeSent` carries both clocks for the same instant and is the free Rosetta stone for `MonotonicTime`.
**Confidence:** confirmed · `50-` §4

### ADR-17 — Events: consume `Runtime.consoleAPICalled` **and** `Log.entryAdded` as disjoint sources; vitals and mutations come from injected isolated-world observers; bodies via `Network.streamResourceContent` subscribed at `requestWillBeSent`
**Why:** four of the brief's assumptions were wrong and each is measured. (a) 36 console events vs 6 Log entries, **zero overlap** — there is nothing to dedupe. (b) `PerformanceTimeline.enable` accepts **only** `largest-contentful-paint` and `layout-shift`; 16 of 18 probed types return `-32602`, so FCP/INP/TTFB/longtask/LoAF *require* an injected `PerformanceObserver` (15 types available; `observe()` on an unsupported type silently no-ops). (c) response bodies are evicted by **any** navigation, including same-process `about:blank` — retroactive capture is not a strategy; and `Fetch` interception guarantees bodies at the cost of corrupting the very timings the harness exists to report, so it is a per-job mode choice and you never claim both. (d) CDP `DOM.*` mutation events are 5,651 `childNodeCountUpdated` / 775 KB / 3 s carrying **counts, not content** (exactly one `childNodeInserted`), versus an isolated-world `MutationObserver` at 2,976 real records / 438 bytes — a ~1,770× reduction at higher fidelity.
**Confidence:** confirmed · `60-` §1, §4, §8, §9

### ADR-18 — Evaluate is **three** tiers, not two
| Tier | Mechanism | Boundary |
|---|---|---|
| `inspect.evaluate` | `Runtime.evaluate{throwOnSideEffect:true, silent:true, timeout:2000}` in an isolated world | **Real.** 12 targeted bypasses (`Reflect.set`, `Function()`, `eval`, `setTimeout`, `Promise.then`, dynamic `import()`, `String.replace` callback, `toString` hijack, Proxy get-trap, `structuredClone`, `forEach(e=>e.remove())`, array mutation) all threw `EvalError: Possible side-effect in debug-evaluate`, zero observable side effects, ~0.05 ms/call |
| `inspect.routes` (**new — forced by the corpus**) | Harness-authored *fixed strings* in the **main** world | None. Page-observable and page-tamperable. Agent never supplies the code |
| `mutate.evaluate` | `Runtime.evaluate`, per-origin, inside an already-closed egress allowlist, full audit record | None in-page. Equivalent to a shell for that origin |

**Why the third tier exists:** verified twice, on fixtures and on five live production sites — `Object.keys(node)` is **empty** in an isolated world, and `__reactRouterManifest` / `__vue_app__` / `__reactFiber$*` all read `undefined` there while DOM node counts are identical (reactrouter 118/118, nuxt.com 1945/1945, nextjs.org 2378/2378). Blink gives each world its own V8 wrapper. Isolated worlds are **not** a mutation boundary either — a `textContent` write from an isolated world was immediately visible in the main world.
**Caveats to ship:** `throwOnSideEffect` protects **integrity, not confidentiality** (it happily returned `input[type=password].value`); it is over-conservative (`getElementById`, `elementFromPoint`, `localStorage.getItem`, `indexedDB.databases()` are all rejected while `querySelector` is allowed); and it is EXPERIMENTAL — ship a 6-expression boot canary that refuses to expose `inspect.evaluate` if it stops blocking writes. It is a V8 correctness mechanism being used as a security boundary; brow would be among the first to treat it that way.
**Confidence:** confirmed · `80-` §4, `30-` §8, `90-` §3.4

### ADR-19 — Policy: capability lattice `observe ⊂ interact ⊂ {inspect, storage} ⊂ mutate ⊂ control`; the verb→capability table **is** the allowlist; three merged layers with deny-always-wins; default grant `observe+inspect` on an ephemeral profile
**Why:** the verb table is what makes "no raw CDP" true — if a verb is not in the table it does not exist, and there is deliberately no `cdp raw`, no `cookies import-from-chrome`, no `attach --pid`, no `launch --no-sandbox`. Merge-not-override with deny-wins is Claude Code's semantics; the managed tier gives teams a lock. Ephemeral (logged-out) default mirrors Atlas's guidance and is the single highest-leverage default: **a job with no session cannot leak one.** Approvals bind to node ref **+ document generation** and are re-verified by screenshot diff against the approval evidence immediately before execution, because an SPA can re-render between approval and click.
**Hard-blocked:** `Target.exposeDevToolsProtocol` (injects a `window.cdp` binding — a direct violation) should carry a compile-time test asserting the string never appears in any allowed capability path.
**Confidence:** likely (design) on the lattice; confirmed on every enforcement primitive · `80-` §2, §3

### ADR-20 — Egress allowlist enforced at `Fetch.requestPaused` on **every** attached session — page, OOPIF, `worker`, and `service_worker` — never browser-session-only
**Why:** browser-session `Fetch.enable` caught exactly one request (the top-level Document); every subresource escaped, and `Network.enable` does not exist on the browser session at all. Verified blocking of XHR + Image with `BlockedByClient` (the page saw `TypeError: Failed to fetch`), and — the load-bearing correction — service-worker `fetch()` is invisible to a page-session Fetch until you auto-attach the SW target and `Fetch.enable` there. `localhost` and `127.0.0.1` are **distinct hosts** to the matcher; canonicalization is mandatory.
**Note the tension:** graph-node selection wants `type=="page"` + own `browserContextId`; policy enforcement needs a strictly wider predicate. **One target filter cannot serve both.**
**Confidence:** confirmed · `80-` §6, `90-` §4.3

### ADR-21 — Storage: SQLite (WAL) for the site graph, content-addressed blob store (blake3) for evidence, `sitegraph.json` as the canonical export, JSONL+zstd segments for events
**Why:** crawls are long-running, resumable and need queries; artifacts are large binaries that dedupe well across replays; JSON is the machine contract and DOT/mermaid are throwaway views; JSONL is greppable and streams to `--follow`. `petgraph` 0.8.3 stays an in-memory analysis view only. Event segments roll at 16 MB / 60 s with a sidecar index (`first_seq`, `mono_us` range, kind counts, url bloom) so `--since`/`--filter` skip whole segments.
**Open sub-decision the corpus flags:** rusqlite defaults to rollback-journal, so a long crawl writing while `job status` reads will hit `SQLITE_BUSY` — WAL, `busy_timeout` and transaction granularity are unspecified, and **resume is claimed but not designed** (no persisted frontier table).
**Confidence:** likely · `90-` §5, `60-` §11

### ADR-22 — State identity is a composite tuple with a per-crawl match policy, and route-template induction runs **before** any DOM hashing
**Why:** the 2026 empirical study (arXiv 2606.16650, verified to exist with the cited authors and Table 5 numbers) shows Crawljax swinging **39.39% → 54.18%** coverage on abstraction choice alone, and Gestalt 17 states vs StringCmp 316 on the same app. No abstraction dominates; the right one depends on the exploration strategy. Two corrections from verification that change the implementation: **StringCmp scores 49.12%**, second-best and ~10 points *above* PDiff — state count and coverage are near-orthogonal, so "more states = worse" is wrong; and the AX skeleton **degenerates on div soup** (measured: every non-root line becomes `generic|…`/`StaticText|…`, and a div-soup modal is invisible to the skeleton entirely, taking `OverlayStack` detection with it). Ship the DOMSnapshot-based fallback skeleton in v1, auto-selected when the non-generic role ratio falls below a threshold. Also drop all `InlineTextBox` nodes (21% of AX nodes on Wikipedia; they are layout line boxes and make the signature viewport- and font-timing-dependent) and filter `state_bits` on the property **value**, not its presence (a plain `<button>` otherwise carries `invalid`).
**Confidence:** confirmed (literature + measured) · `90-` §2

### ADR-23 — Protocol drift is managed, not hoped away
**Why:** **38 of 57 domains in Chrome 151 are `experimental: true`**, including `CSS`, `CacheStorage`, `DOMSnapshot`, `Accessibility`, `Storage`, `ServiceWorker`, `Preload`, `PerformanceTimeline` — i.e. essentially the whole load-bearing surface. Member-level flags matter too (`DOM.getContentQuads`, `Input.insertText`, all three `synthesize*`, `Runtime.evaluate.throwOnSideEffect`, `Network.streamResourceContent`, `Runtime.bindingCalled` the event, `Emulation.setLocaleOverride` but *not* `setTimezoneOverride`). Policy: vendor + codegen + milestone gate + lazy `-32601` discovery + a fixture test per experimental call that fails loudly + `xtask check-protocol` diffing against upstream head + record `Browser` and `Protocol-Version` in every artifact.
**Unaddressed anywhere in the corpus (a real gap):** Chrome auto-updates silently, including *while a browser instance is running* — the binary on disk changes under a live process and a relaunch negotiates a different milestone than the session did.
**Confidence:** confirmed (the surface); **open** (the auto-update story) · `90-` §10.10, `10-` §9.4

### ADR-24 — No evasion, and say so
**Why:** `--remote-debugging-pipe` sets `navigator.webdriver === true` unconditionally (full 8-row matrix measured: no-CDP control `false`, websocket `false`, websocket+`--enable-automation` `true`, **pipe `true` in both headless and headful**, pipe+`--disable-blink-features=AutomationControlled` `false`). Do not pass `--enable-automation` (it only changes the infobar and password-save UI now) and do not pass the evasion flag. Never `--no-sandbox`, `--single-process`, `--disable-web-security`, or `--disable-features=IsolateOrigins,site-per-process` — killing site isolation would remove OOPIF targets *and* the main renderer mitigation on a machine browsing arbitrary sites for an agent. Frame the whole detection section as "why your app behaves differently under automation", with a measured signal table, not as a bypass toolkit.
**Confidence:** confirmed · `10-` §6.4, `40-` §11

---

## 4. The unsettled fork — who runs the LLM loop in a detached job?

This is the one decision the owner must make **before any code is written**, because it determines whether `crates/jobs` contains a decision queue, whether `browserd` ever needs a model client, and what `SKILL.md` actually instructs the agent to do.

### The problem, stated precisely

The spec promises `browserctl job start --detached --record-video` plus `status`/`logs --follow`/`artifacts`/`pause`/`resume`/`stop`, and dangerous actions that park the job in `waiting_for_approval`. The crawler research then says the agent is where the judgement lives — *"hand the residual to the agent as a compact list (≤40 items) and let it choose/rank… this is where the LLM earns its keep"* (`90-` §4.2 step 6). Both are reasonable. Together they are contradictory:

- A 2,000-action crawl **cannot** round-trip to an LLM per action (latency, token cost, and the calling agent's process is a one-shot turn that has already exited).
- A job that parks on `waiting_for_approval` has, by construction, no TTY to park to — the originating terminal is gone (`80-` open question 2).
- The daemon calling a model itself would violate "no cloud APIs, no telemetry" unless the model is local, which is a whole other product.

### The options

| | Model | Who decides | Pros | Cons |
|---|---|---|---|---|
| **A** | **Fully autonomous daemon.** Job runs a deterministic, policy-driven loop; the agent supplies a config up front (priorities, veto lexemes, form fixtures, budget) and reads the report afterwards | `crates/crawler` heuristics | No LLM dependency in the daemon; honours all constraints; detached jobs genuinely detached | Loses exactly the semantic judgement the crawler research says is the differentiator; icon-only destructive buttons and "which of 30 links matters" get heuristics |
| **B** | **Daemon calls a model.** `browserd` holds an API key or a local model and drives the loop | daemon | Best action selection; true autonomy | Violates "no cloud APIs / no telemetry" as written; a local model is a second product; key management in a daemon is a security regression |
| **C** | **Agent-side supervision, async.** Job runs autonomously but emits a bounded `needs_decision` queue; the calling agent answers items via `browserctl job answer <job> <q-id> …` on its next turn (or in a `/browser` follow-up); the job keeps working other frontier branches meanwhile and only blocks when the queue gates everything | agent, asynchronously | Keeps the LLM where it earns its keep; daemon stays model-free; detached jobs still make progress; maps cleanly onto how Claude Code / Codex actually run (turn-based, resumable) | Needs a queue, correlation ids, TTLs, and a "what happens if nobody ever answers" policy; the agent must be *told* to check back — a SKILL.md problem, not a daemon problem |
| **D** | **Plan-then-replay.** The agent produces a full action plan interactively; the detached job is a deterministic replay of it | agent, up front | Simplest; perfectly auditable; great for regression runs | Useless for exploration — a crawl's whole point is that you do not know the plan; and replay divergence is unmeasured (see risk R7) |

### Recommendation: **C, with A as the floor and D as a separate feature**

Concretely:

1. **The daemon never calls a model.** That constraint stays absolute and is the reason brow is worth building.
2. **Every detached job is defined by a `JobPlan`** submitted at `job start`: capability set (intersected with the caller's grant), origin allowlist, budget, priorities, veto lexemes, form fixtures, and a `decision_policy` of `auto` | `ask` | `stop`.
3. **The job always makes progress under heuristics** (option A behaviour) and never blocks on the agent for ordinary action selection. This is the floor: a job whose agent never comes back still produces a coverage report.
4. **Two queues, not one, and they must not be conflated:**
   - `needs_decision` — semantic questions (which of these 30 affordances, is "Retire this workspace" destructive, is this state genuinely new). Answerable by the **agent**. TTL-bounded; on expiry the `decision_policy` decides (`auto` → heuristic, `ask` → skip and record as `blocked{reason:"undecided"}`, `stop` → park the job).
   - `waiting_for_approval` — irreversible actions per the spec (publish, delete, purchase, send, OTP, upload, permission grant, cookie import). Answerable by a **human only**, bound to ref + doc generation, expiring in 15 minutes by default, re-verified by screenshot diff before execution. Delivery: desktop notification + `brow approve`, because the TTY is gone by construction.
5. **`brow job answer` and `brow approve` are different verbs with different auth.** `approve` is refused when stdin is not a TTY (per `80-` §2.2). `answer` is not — it is the agent's channel.
6. **D ships later as `job replay <plan.json>`**, which is genuinely valuable for regression runs and costs almost nothing once the action log exists.

**What this buys:** the daemon stays a deterministic, testable, model-free system; the agent's judgement is used where it changes outcomes and nowhere else; detached jobs are usable without a live terminal; and the "who is the human" question gets a concrete answer instead of being deferred.

**What it costs:** a decision queue with TTLs and correlation ids in `crates/jobs` (~1 engineer-week), and a SKILL.md that must teach the agent to check back — which is the part most likely to be got wrong and should be prototyped against a real Claude Code / Codex session before the protocol is frozen.

---

## 5. Risk register

Top 15, ranked by expected damage. L = likelihood, I = impact, both High/Med/Low.

| # | Risk | L | I | Mitigation | Dossier |
|---|---|---|---|---|---|
| R1 | **Supervisor reconnect/resync is undesigned.** `browserd` restarts mid-command: pending ids are lost, the supervisor already forwarded them, Chrome answers into the void, and nothing says how the reconnecting daemon rebuilds its session map | H | H | Design the resync protocol *before* freezing crate boundaries: bounded replay buffer of browser-scoped events, explicit `resync` handshake returning the full target/session inventory, all in-flight ids failed deterministically on reconnect. Prototype against real `launchd` **and** real `systemd --user` | `10-` §8.2 + GAP |
| R2 | **Concurrency is completely untested.** Every experiment in all seven dossiers is single-session. Nothing tests two jobs sharing one browserd: screencast loss under concurrent recording, whether a blocking `synthesizeScrollGesture` (measured 5.3 s at speed 100) stalls another session, whether one 31 MP capture (28.5 s of encode) starves everyone, head-of-line blocking on the single ordered pipe, or interleaved writes | H | H | Make the *first* thing after the walking skeleton a two-session soak test. Serialize writes behind one task (ADR-03), give long gestures their own in-flight slot with a watchdog, and cap captures per-daemon not per-session | GAP |
| R3 | **Stale `backendNodeId` → clicking coordinates from the previous page.** `getBoxModel` returns byte-identical stale geometry with no error after same-process navigation | M | H | ADR-07: generation counter checked before *every* geometry read and *every* action; match on error **code** not message; fixture test that navigates and asserts refusal | `30-` §1 |
| R4 | **A missed `setAutoAttach` re-arm silently drops an entire iframe subtree** — and nothing in the corpus proposes a way to *detect* it | M | H | One `on_attached()` hook, no call-site convention; plus the cheap completeness check: `<iframe>` element count per session vs attached iframe targets + same-process `contentDocument`s, reported as a coverage gap | `10-` §3.3 + GAP |
| R5 | **Protocol drift / Chrome auto-update.** 38 of 57 domains experimental, including `CSS`, `DOMSnapshot`, `Accessibility`, `CacheStorage`; Chrome updates itself silently, possibly while a browser is live | H | M | ADR-23. Plus: detect binary mtime/version change at relaunch and refuse to reuse a negotiated capability set; CI canary against Chrome beta/dev for `streamResourceContent` and `dataReceived.data` | `90-` §10.10 |
| R6 | **Token blowup.** Raw AX on Wikipedia = 9.28 MB ≈ 2.5M tokens; one fat node on github.com ≈ 309K tokens; even the compacted view is ~40K tokens on Wikipedia | H | M | ADR-10 tiering is load-bearing, not polish. Hard byte caps on every CDP call that can be forwarded; `page find` as the encouraged entry point; refuse-or-page above N interactive nodes | `30-` §11 |
| R7 | **Replay-from-root may not terminate.** The corrected cost envelope is ~62 min for a 300-state, 6-deep crawl (root load + `L·(t+settle)` + the mandatory 1.5 s volatility double-check), and **per-edge divergence rate is completely unmeasured**; the pseudocode aborts a path on signature mismatch, so 10%/edge fails ~47% of 6-deep restores | H | H | **Week-one instrument:** replay 50 known paths on one real app and report per-edge divergence. That single number decides whether replay-from-root is viable or whether the crawler must be scoped to `restorable_by_url` states only | `90-` §4.6 |
| R8 | **The destructive-action guard is not sound.** Measured leaks: service-worker `fetch()` (POST reached the server, 200), mutating GETs, WebSocket frames, `localStorage.clear()`, `indexedDB.deleteDatabase()`, Background Sync, prerender-triggered server hits | H | H | Auto-attach + `Fetch.enable` on every SW/worker session (verified fix); ship the `MUTATION CONTAINMENT` block in every crawl report; product copy says **best-effort read-only**, never "cannot mutate" | `90-` §4.3 |
| R9 | **Prompt injection.** brow is a lethal-trifecta machine by construction: private data + untrusted content + egress | H | H | Unfixable; budget it. Observe-only default, ephemeral profile default, closed per-job egress allowlist, capability decisions computed in the daemon on the *action* (a page has no channel into the policy engine), provenance envelopes, hidden-text detection that auto-downgrades the job, human diff before submit. **Never ship copy saying brow is "safe"** | `80-` §5 |
| R10 | **`mutate.evaluate` is a remote shell for the granted origin.** Any in-page restriction after the grant is advisory | M | H | Per-origin only, inside an already-closed egress allowlist, with source SHA-256 + DOMSnapshot/cookies/screenshot before-and-after. Document the browser-process controls that *do* survive (cookies, downloads, uploads, permissions, contexts, navigation) as the real boundary | `80-` §3.2 |
| R11 | **Framework adapters run in the main world** — page-observable and page-tamperable, which reopens the security argument that the isolated-world design was supposed to close | M | M | Capture pristine `Reflect.ownKeys` / `Function.prototype.call` in a document-start main-world script before page script runs; treat every value read as untrusted input (size-cap, never eval); label output `provenance:"main-world"`; consider capability-gating rather than default-on | `30-` §8 |
| R12 | **One oversized capture kills a shared daemon.** No protocol size limit (1600×200000 captured successfully ⇒ ~1.28 GB RGBA); headless Chrome observed dying twice during large-capture sequences | M | H | `max_capture_megapixels` (~120 MP) with a `--tiles` fallback; enforce the cap **per-daemon**, not per-session; a ~64 MB client-side guard on the *outbound* write path (the 100 MB pipe cap is client→Chrome only — a 125.8 MB screenshot response transits fine) | `50-` §1.4, `10-` §1.1 |
| R13 | **Politeness, rate limiting and robots compliance are entirely absent** from the crawler design. A 2,000-action budget against a small site is a self-inflicted DoS; the first real crawl gets the user IP-banned or Cloudflare-challenged | H | M | Week-one policy decision: obey robots for the crawler by default, `--ignore-robots` with a loud warning, per-origin concurrency cap, crawl-delay, backoff on 429/503, and a documented User-Agent | GAP |
| R14 | **Artifacts are credential-equivalent and there is no redaction stage for the crawler's own outputs.** Screenshots of logged-in pages, `network_json` with bearer tokens, DOM/AX blobs with post-login content, all written to disk; the vault-mode design only protects the credential file | M | H | Extend ingest-time redaction (already built for events) to every artifact writer; `0700` dirs / `0600` files; a documented threat model for shipping an artifact bundle to a colleague; pin evidence referenced by pending approvals so GC cannot delete it | `90-` GAP, `80-` §9 |
| R15 | **No universally-playable video without ffmpeg**, and the spec forbids hard-requiring it | M | M | 3-rung ladder with honest labelling; write frames first; do **not** describe rung 2 (MJPEG-in-MP4, plays in Safari/QuickTime/VLC but not Chrome or Firefox, ~40× larger) as "a video" in SKILL.md | `50-` §4.4 |

### Capabilities the research found impossible or materially limited

Every one of these needs a line in `SKILL.md`. An agent that believes a capability exists and silently gets nothing is worse than one that is told no.

| Capability the spec implies | Reality |
|---|---|
| Complete page introspection incl. canvas/WebGL | **Pixels only.** No `Canvas` domain in Chrome 151, no draw-call log, no scene graph. Flutter Web/CanvasKit exposes structure only via `<flt-semantics>` and only when semantics are forced on |
| Per-frame screenshots | `Page.captureScreenshot` is rejected on OOPIF sessions (`-32000 Command can only be executed on top-level targets`). "Per-frame" means *the region of the top-level page where that frame lives*, including whatever the parent paints over it |
| Correct full-page screenshots of any site | Lazy/IntersectionObserver content is **blank**; `captureBeyondViewport` does not drive it. Only scroll-priming works, and that mutates page state and fires analytics |
| Refs auto-invalidate on navigation | CDP will not do it. Same-process navigation leaves `describeNode`/`getBoxModel`/`scrollIntoViewIfNeeded` succeeding on the dead document |
| Attach to the user's logged-in Chrome | Chrome ≥136 ignores both remote-debugging switches against the default data dir. *(The follow-on "the encryption key is bound to the data directory" claim is **unsupported on macOS** — one app-wide Keychain item; App-Bound Encryption is Windows-only. Do not write it in SKILL.md.)* Human logs in once inside a brow profile |
| Reconnect to an already-running browser | Impossible in pipe mode. One client, no discovery, no `DevToolsActivePort`. Only *our* supervisor socket |
| Validate the protocol against the local browser at startup | No runtime descriptor over a pipe. Milestone parsing + lazy `-32601` only |
| Isolated-world framework enrichment | Impossible. Per-world V8 wrappers. Main world or nothing |
| Full Web Vitals from CDP | `PerformanceTimeline` gives LCP + layout-shift only. Everything else is an injected observer |
| Retroactive response bodies | Evicted by any navigation. Decide at `requestWillBeSent` or lose them |
| Why a WebSocket closed | `webSocketClosed` carries only `requestId` + `timestamp`. No code, no reason |
| HAR `cache` fields | Not derivable; DevTools itself emits `{}` |
| Memory-leak detection | `Memory.getDOMCountersForLeakDetection` → `-32000 Failed to run leak detection` |
| Hardware-faithful IME commit | `compositionend` from `insertText` is `isTrusted:false` — the only such event in the chain |
| Deterministic double-click timing | `clickCount` is honoured mechanically, not temporally |
| Browser accelerators (Cmd+T/L/P, F12) | Renderer-only delivery. Expose explicit verbs instead |
| Native UI: OS context menus, omnibox, print, downloads shelf, permission bubbles, `<select>` popups on macOS, Keychain, Touch ID, CAPTCHA | Genuinely unreachable, forever. Human handoff — hold the line |
| Reproducible crawls | Impossible. Only the **model diff** is comparable, and only with flakiness suppression |
| Proof that no exfiltration occurred | Impossible. `Fetch` does not see timing channels, DNS prefetch, preconnect, WebRTC ICE, cache timing. The egress log is evidence, not proof |
| Route extraction for Next.js App Router / SvelteKit / prod Angular | **Nonexistent at runtime.** Verified on nextjs.org, vercel.com, tailwindcss.com, svelte.dev, angular.dev. Works for React Router 7/8 (fog-of-war, lower bound) and Vue/Nuxt (293 routes, complete) |
| Snap/Flatpak Chromium on Linux | fd inheritance unreliable, `--user-data-dir` confined to `$HOME`. Detect and refuse rather than half-work |
| Windows | Transport path now **confirmed from the consuming code** (`AdoptPipes`/`AdoptHandle`: raw HANDLEs as decimal uint32, `<read>,<write>` order, must be `FILE_TYPE_PIPE`, near-silent failure) — but nothing has been executed there. Tier-2 until proven |

### The "no Playwright" cost list

These are the papercuts Playwright/Puppeteer absorb that brow now owns. None is hard individually; together they are most of the schedule.

1. **Actionability + retry semantics** (visible / stable / receives-events / enabled / editable, per action class) — ADR-14.
2. **Auto-waiting and navigation lifecycle** — `Page.lifecycleEvent` plumbing, `networkAlmostIdle`, settle windows.
3. **The US keymap table** — `code` + `key` + `windowsVirtualKeyCode` + `text`, code-generated from Chromium (ADR-13).
4. **OOPIF session stitching and cross-frame coordinate composition**, including the affine guard.
5. **Browser discovery** across macOS/Linux/Windows with priority weights (and never launching from `/Volumes`).
6. **The launch flag set** — which of ~40 flags are current, which were removed in 2016–2019, which are actively harmful.
7. **Protocol version skew** — pinning, codegen, milestone gating, lazy capability discovery.
8. **Locator/re-binding engine** — the stability-ranked chain (`data-testid` → non-generated `id` → role+name+nth → container+text → CSS → XPath) and reporting when it degrades.
9. **Dialog handling** — you must *always* answer `beforeunload` because `Page.navigate` blocks on it, and its message is deliberately empty.
10. **Downloads** (`Browser.setDownloadBehavior`) and **file chooser interception** (`Page.setInterceptFileChooserDialog`, incl. the `cancel:true` path so a stray chooser cannot wedge a page).
11. **Three drag strategies**, probed rather than guessed (HTML5 DnD and pointer DnD are mutually exclusive mechanisms).
12. **Screenshot mechanics** — scroll normalisation, coordinate spaces, clipping ancestors, occlusion sampling, megapixel caps, DPR pinning.
13. **The entire video pipeline** — screencast acking, frame indexing, concat manifests, the `-fps_mode passthrough` and even-dimension traps, the clock normalisation.
14. **Network plumbing** — the `*ExtraInfo` join, body-capture policy, HAR `buildTimings` (including DevTools' deliberate quirks), redaction at ingest.
15. **Known-bug workarounds** — the `setEmitTouchEventsForMouse` ban + input-dead latch, `preventFling` defaulting to true, `sessionId` not being a frame number, `queryAXTree` exact-match, `getContentQuads` returning the iframe border box.
16. **Process lifecycle** — orphan detection with start-time verification, `kill(-pgid)` not `kill(pid)`, profile lock artifacts, crash/sad-tab recovery.
17. **A fixture corpus** for every one of the above, because each is a silent-wrongness bug rather than a crash.

---

## 6. Scope reality check

The corpus contains no explicit MVP/v2/v3 breakdown from the owner, so the split below is **reconstructed from the spec's feature list** (`SHAPE` + `HARD CONSTRAINTS`). If the owner's actual plan differs, the effort numbers still apply per feature. Estimates are engineer-weeks for one competent Rust engineer already fluent in CDP, including tests; they assume the research is not redone.

### MVP — "a persistent browser an agent can see and drive"

| Item | Weeks | Verdict |
|---|---|---|
| CDP transport + codegen + `cdp-protocol` crate | 1.5 | Achievable. Partly banked in the existing skeleton |
| Browser discovery, launch flags, process lifecycle | 1 | Achievable. Partly banked |
| **Supervisor + resync protocol** (macOS + Linux) | **2.5** | Achievable but **underestimated by everyone**; the resync half is the hard half and it is undesigned |
| Daemon, unix socket + peer-uid, session model | 1.5 | Partly banked |
| OOPIF auto-attach, session stitching, frame completeness check | 1.5 | Achievable; **not** in the skeleton |
| Unified Page Tree (snapshot + getDocument + AX + listeners + tiering) | 3 | Achievable. The tiered output design is the expensive part |
| Ref model + generation invalidation + re-binding | 1 | Achievable, and non-negotiable (R3) |
| Input: mouse/keyboard/touch/gestures/drag/upload + keymap codegen | 2.5 | Achievable. Partly banked |
| Actionability | 1.5 | Achievable. The single highest-value week in the project |
| Capture: viewport/full-page/node/region + annotations + diff | 2 | Achievable |
| Events: console + network + redaction at ingest | 1.5 | Partly banked |
| Policy crate: lattice, verb table, three-layer merge, egress | 2 | Achievable |
| `SKILL.md` + CLI ergonomics + install into 3 harnesses | 1.5 | Achievable; more fiddly than it sounds |
| **MVP total** | **≈ 23 weeks** | |

**Misplaced into MVP:** nothing, if the supervisor stays. **Move *out* of MVP:** framework adapters, cascade-winner computation, video, jobs.

### v2 — "background jobs, recording, and honest event streams"

| Item | Weeks | Verdict |
|---|---|---|
| Jobs: detached lifecycle, status/logs/artifacts/pause/resume/stop | 2 | Achievable |
| **Decision + approval queues** (§4) | 1.5 | Achievable; blocked on the fork decision |
| Video: screencast → 3-rung ladder + action log + clock normalisation | 2.5 | Achievable; measured, not speculative |
| Event streams: vitals observer, mutation observer, WS/SSE, HAR, tracing | 3 | Achievable |
| Artifact store: blake3 CAS, retention, GC, manifests | 1 | Achievable |
| Multi-session/concurrency hardening (R2) | 1.5 | **Should be in MVP, not v2** — it is the daemon's entire selling point and is untested |
| Windows tier-2 | 2 | Achievable; transport path confirmed from source, nothing executed |
| **v2 total** | **≈ 13.5 weeks** | |

**Misplaced:** concurrency hardening belongs in MVP. Windows can slip to v3 without harm.

### v3 — "site graph, coverage, adapters"

| Item | Weeks | Verdict |
|---|---|---|
| Site graph schema, SQLite/WAL, resumable frontier, `sitegraph.json` | 2 | Achievable |
| State identity (composite signature + DOMSnapshot fallback skeleton + volatility masking) | 2.5 | Achievable, but **tuning it is open-ended** |
| Declared-route sources (sitemap, robots, SW precache, speculation rules, app manifest) | 1.5 | Achievable; yields are bimodal and must be reported as such |
| Crawl loop, frontier, form filling, destructive guard, coverage report | 3.5 | Achievable |
| Politeness/robots/rate limiting (R13) | 0.5 | **Must move to whenever the crawler first touches a real site** |
| Crawl diff + `--stabilise` | 1.5 | Achievable |
| Framework adapters (React, Vue/Nuxt, Svelte, Next, Angular, Flutter) | 2 | **Half of it is impossible** — see below |
| **Cascade-winner computation** (origin × importance × layers × `@scope` × shadow order × source order) | **2+** | **A research project, not a feature.** Ship "matched rules sorted by specificity + `winner_confidence`" and defer |
| **Replay-from-root viability** | ? | **Unknown until R7 is measured.** Could invalidate the crawler architecture |
| **v3 total** | **≈ 15.5 weeks**, plus unbounded tuning | |

**Blunt assessments:**

- **Framework adapters are not one feature, they are two.** *Detection* (name + version) is trivial and works everywhere — even from the isolated world, since `ng-version` is a DOM attribute. *Route extraction* works for React Router and Nuxt and is **verified impossible at runtime** for Next App Router, SvelteKit and production Angular. Budget the adapters as "React + Vue: 1 week; everything else: detection only, plus an optional repo-aware mode reading `.next/routes-manifest.json` / `.svelte-kit/` — which is a filesystem-scope policy question, not an engineering one."
- **Cascade-winner fidelity is a research project.** CDP hands you all matches, specificity (including the experimental per-simple-selector `components` breakdown), layer order integers, and source ranges — but never the winner. Getting `!important` × cascade layers × `@scope` × shadow tree order right is a week of careful work *plus* a fixture corpus, and you will still be wrong in corners. v1 answer: show matched rules ordered by specificity, no winner claim.
- **The crawler's schedule risk is not the code, it is R7.** Measure per-edge replay divergence on one real app before writing `crates/crawler`. If it is high, the honest product is "crawl the `restorable_by_url` states thoroughly and report the rest as `blocked{reason:"not-restorable"}`", which is a smaller and still-valuable feature.
- **Total, honestly: ~52 engineer-weeks** for the spec as written, excluding the open-ended tuning and excluding anything R7 forces. Roughly a year for one person, two quarters for two.

---

## 7. Revised milestone plan

### M0 — Decisions (0 weeks of code, blocking everything)

Answer §4 (the LLM-loop fork) and open questions Q1–Q4 in §8. **Exit:** written answers in this file.

### M1 — Walking skeleton (partly banked)

**The smallest end-to-end slice that proves the architecture:** launch → pipe → attach → snapshot → click → screenshot, through the daemon and the CLI.

Already in tree (iterations 1–2): pipe transport, `setsid`'d daemon, discovery, `DOMSnapshot`-backed snapshot with `@node-N`, gestures, capture, console/network with redaction, `throwOnSideEffect` eval, e2e tests against real Chromium.

**Still required for M1 to count:**
- `crates/` split with `cdp-protocol` codegen from vendored JSON (currently hand-rolled JSON in a single crate).
- Ref generation counter + refusal on stale refs (R3) — with a fixture that navigates and asserts the refusal.
- `Target.setAutoAttach` re-armed in one hook + one OOPIF fixture where a button inside a cross-origin iframe is clicked successfully via the frame's own session.
- The frame-completeness check emitting a coverage gap.

**Exit criteria:** `brow open <url-with-oopif> && brow snapshot && brow click @nN && brow screenshot --node @nN` works headful and headless; the OOPIF button is in the snapshot and clickable; a navigation invalidates refs and the next click fails with `E_REF_STALE` rather than clicking stale coordinates; all of it green in CI against a real Chrome.

### M2 — Supervisor and survival (the architectural bet)

The supervisor process, the unix-socket re-export, the **resync protocol**, orphan cleanup with start-time verification, and hang detection (browser-session heartbeat + per-session deadlines + `Target.closeTarget` escape + the per-session input-dead latch).

**Exit criteria:** a real LaunchAgent is booted out and re-bootstrapped while a page holds state; the browser survives, the daemon reconnects, the session inventory is rebuilt, in-flight commands fail deterministically, and the same page is still driveable. Repeat under `systemd-run --user --scope` on a Linux box. `Page.crash` is recovered from cleanly. **Do not proceed without the Linux run** — the corpus explicitly refutes `setsid()` sufficiency there.

### M3 — Concurrency and honest limits

Two sessions, two jobs, one browserd. The soak test that R2 says nobody has run.

**Exit criteria:** two concurrent sessions each doing snapshot + click + screenshot for 30 minutes with no cross-talk; a 5 s blocking gesture on session A does not stall session B; a capture that would exceed the megapixel cap is refused with a structured error rather than killing the browser; an oversized outbound command is refused client-side at ~64 MB; measured and documented head-of-line behaviour on the shared pipe.

### M4 — The introspection product

Tiered tree output, listener delegation map, AX enrichment, per-frame stitching, CSS (matched rules by specificity — **no winner claim**), `page find`, artifacts + server-side query.

**Exit criteria:** github.com's interactive view is ≤ 8K tokens; a Wikipedia article either pages or refuses with a useful instruction; a closed-shadow-root button and a same-origin-iframe button both appear with correct paths and geometry; one fat node stays under 900 tokens.

### M5 — Input completeness and actionability

Full gesture set, keymap codegen, IME, drag strategies, upload paths, dialogs, device emulation, and the actionability algorithm with structured failures.

**Exit criteria:** the fixture corpus from `40-` passes; every actionability failure returns a typed reason plus an annotated screenshot; `setEmitTouchEventsForMouse` is unreachable from any code path (compile-time test).

### M6 — Policy and capabilities

Lattice, verb table, three-layer merge, egress allowlist on **all** session types, redaction pipeline covering every artifact writer, the boot canary, approvals bound to ref + generation.

**Exit criteria:** the SW-mutation fixture from `90-` §4.3 is blocked; the boot canary fails closed on a stubbed regression; a job started with `--cap observe` cannot reach any `interact` verb; `Target.exposeDevToolsProtocol` fails a compile-time assertion; an artifact bundle contains no unredacted `Set-Cookie` or `Authorization`.

### M7 — Jobs, video, events

Detached job lifecycle, the two queues from §4, screencast → action log → 3-rung ladder, injected vitals/mutation observers, HAR, retention and GC.

**Exit criteria:** a detached job survives a daemon restart, records video, parks on approval with a desktop notification, is approved from a fresh terminal, and resumes; the action log and the video agree within the stated 2 ms / 20 ms budgets; the ffmpeg-absent path still yields readable frames + manifest.

### M8 — Crawler, gated on a measurement

**Before writing `crates/crawler`:** the R7 instrument — replay 50 known paths on one real app, report per-edge divergence. Then politeness/robots, declared-route sources, state signature with the div-soup fallback, frontier, coverage report with the `PARTIAL`, `UNBOUNDED` and `MUTATION CONTAINMENT` blocks, crawl diff.

**Exit criteria:** a real app crawled inside budget with a coverage report whose every number is provenance-tagged; the report states its own containment leaks; a re-run diff is readable (not 40% noise) with `--stabilise`.

### M9 — Adapters, Windows, polish

Detection everywhere, route extraction where it exists, optional repo-aware mode, Windows tier-2, codesigning/notarization.

---

## 8. Open questions for the owner

Deduplicated from ~60 across the seven dossiers, prioritised. Each names the decision it blocks.

### Blocking — answer before any code

| # | Question | Blocks | Recommendation |
|---|---|---|---|
| **Q1** | **Who runs the LLM loop in a detached job?** (§4) | `crates/jobs` shape; whether browserd ever needs a model client; `SKILL.md` structure | Option **C**: autonomous heuristic floor + async agent `needs_decision` queue + human-only `waiting_for_approval` |
| **Q2** | **Supervisor process: yes or no?** If no, which of {persistent Chromium, restartable daemon, no TCP port} do we drop? | Crate boundaries (`browser-supervisor` exists or `browser-process` owns the pipe); packaging for launchd/systemd | **Yes.** It is the only way to have all three, and it makes "no raw CDP" a process boundary |
| **Q3** | **Headful by default?** Off-screen positioning breaks screenshots on macOS, so there is no invisible-headful trick | Capture fidelity claims; whether a window appears on the user's desktop | **Yes**, headful default (~80 ms more startup, ground-truth compositing), headless for detached jobs with no visual-fidelity claim |
| **Q4** | **Minimum Chrome milestone: 136 or 128?** | Whether the non-default-user-data-dir rule is unconditional; how much feature-gating and how many `blocked` coverage entries | **136.** Simpler story, and the user-data-dir rule is then unconditional |

### High — answer before the relevant milestone

| # | Question | Blocks | Recommendation |
|---|---|---|---|
| Q5 | **Do framework adapters ship enabled-by-default with a `provenance: main-world` label, or capability-gated?** They execute page-observable code in the page's own world | M9 adapters; the security posture in `SKILL.md` | Capability-gated (`inspect.routes`), off by default, with the pristine-reference hardening |
| Q6 | **Closed shadow DOM policy** — always pierce, pierce only under `inspect`, or `--pierce-closed` with an audit entry? The site author explicitly opted out | M4 tree defaults | Pierce under `inspect`, audit-logged; the alternative is a worse product for accessibility and testing work |
| Q7 | **Ref stability across snapshots** — is it a hard requirement? If yes, refs must be re-keyed by fallback identity on every snapshot | ADR-07 implementation; whether `@nN` is dense-allocated or backend-derived | Daemon-allocated dense sequence, re-keyed by fallback; stability across snapshots is a *best-effort* documented property, not a guarantee |
| Q8 | **Snapshot cadence** — re-snapshot per command (211 ms / 19.7K nodes) with a coalescing window, or maintain an incremental tree? | M4 architecture; ~5× code difference | Re-snapshot with a 300 ms coalescing window. Incremental is a whole class of drift bugs for a latency win nobody asked for |
| Q9 | **`inspect.evaluate` ergonomics** — given `throwOnSideEffect` rejects `getElementById` and `localStorage.getItem`, do we auto-rewrite, or drop free-form read-only JS for typed read verbs (`read attr` / `read prop` / `read storage`)? | The `inspect` verb surface | Typed read verbs as the primary path, free-form as the escape hatch. Safer, and probably better for an LLM |
| Q10 | **Does the crawler obey robots.txt by default?** (R13) | `crates/policy`; the first real crawl | Yes, with `--ignore-robots` behind a loud warning, plus per-origin concurrency caps, crawl-delay, 429/503 backoff, and a documented UA |
| Q11 | **Do we attach to `tab` targets as well as `page`?** Needed to model prerender/bfcache swaps as continuous entities | Site-graph identity model; a second identity layer in the tree | Defer to M8. Note that prerendered documents were verified to appear as **no target at all** — the risk is invisibility, not noise |
| Q12 | **bfcache on or off?** Playwright disables it for deterministic navigation interception; a site mapper arguably wants the restore as a real edge | Crawler determinism vs fidelity | On. A bfcache restore is a genuine state transition and this is a site mapper, not a test runner |

### Medium — can be decided at implementation time

| # | Question | Recommendation |
|---|---|---|
| Q13 | Default capability set — `observe+inspect`, or `interact` auto-allowed for `localhost`/`127.0.0.1`/`*.local`? | `observe+inspect` globally; `interact` auto for loopback is a defensible ergonomics carve-out |
| Q14 | Body-capture default — all bodies under N KB, or an allow-list of content types? | Content-type allow-list (json/text/xml/html/js) with a size cap; `--all-bodies` opt-in |
| Q15 | Default DPR for artifacts — pin `deviceScaleFactor:1` for reproducibility, or native retina? | Pin at 1, `--dpr` to override |
| Q16 | Video codec — H.264/MP4 or VP9/WebM? | WebM fits the local-first ethos; measured ~7% smaller; make it configurable |
| Q17 | Screencast fps — full rate (~530 KB/s) or `everyNthFrame:2`? | Default `everyNthFrame:2`; report observed fps per recording rather than assuming any rate |
| Q18 | Is `--prime-scroll` the default for `--full-page`? It mutates page state and fires analytics | No. Opt-in, with `lazy_primed:true|false` in every manifest so a blank region is diagnosable |
| Q19 | Async stack depth — 32 (useful traces, real V8 cost) or 0? Different for interactive vs background jobs? | 32 interactive, 0 for long background jobs; never `Debugger.enable` by default |
| Q20 | Reveal cache — is a 64 MB / 10-min memory-only pre-redaction cache acceptable, or must redaction be irreversible? | Bounded cache, `storage`-gated, audit-logged. Irreversible redaction makes auth debugging impossible |
| Q21 | Humanised input (Bézier paths, log-normal keystroke intervals) — ship as ergonomics, or omit so nobody mistakes brow for evasion tooling? | Ship as `PathStyle::Human`, documented explicitly as an ergonomics feature for hover/gesture UIs |
| Q22 | Non-US keyboard layouts? | US-only + `insertText` for everything else in v1. A real layout engine (AZERTY/QWERTZ/JIS, dead keys, AltGr) is weeks |
| Q23 | Repo-aware mode (reading `.next/routes-manifest.json`, `.svelte-kit/`, globbing `src/app/**/page.tsx`)? | Yes, opt-in — it is the *only* way to get complete routes for the three frameworks that expose nothing at runtime. It is a filesystem-scope policy question |
| Q24 | Do we ever expose `--remote-debugging-port` behind `brow doctor --unsafe-open-port`? | Yes, TTY-gated and loudly warned. It is genuinely useful for debugging the harness with real DevTools, and pipe+port were verified to coexist |
| Q25 | Developer ID cert for macOS codesigning/notarization? | Needed before v1 ships — an unsigned persistent launchd agent is a bad look for a security-sensitive tool |
| Q26 | Do we file the Chrome bugs upstream? | Only crbug 40225266 (fresh repro in hand). **Not** the `char` storm — it did not reproduce across six conditions and both transports; do not file without a new minimal repro |

---

## 9. How to read the corpus

Three conventions matter:

1. **Every dossier ends with a "Verification pass" table.** Where the body and the verification table disagree, **the verification table wins** — the bodies carry inline `> Corrected 2026-08-04` blocks, but a skim will miss them.
2. **Confidence tags are load-bearing.** `confirmed` means measured or read from Chromium source. `likely` means secondary sources agree. `UNVERIFIED` means reasoning — and the corpus is honest about how much of the supervisor design, all of Windows, and all of Linux falls in that bucket.
3. **The "Limits and impossibilities" section of each dossier is the most valuable part.** It is what stops brow from promising things CDP cannot do, which is the difference between a tool an agent can trust and one it silently gets wrong answers from.
