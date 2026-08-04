# Prior Art, Competitive Landscape, Framework Adapters, and Agent-Side Integration

> **Bottom line.** The shape you designed already exists twice: `gsd-browser` (Rust + chromiumoxide + persistent daemon + `@v1:e1` refs + 90 commands, MIT/Apache-2.0) and `gstack browse` (TS/Bun daemon + `@e3` refs + deny-default CDP allowlist + token tiers). Neither is a reason not to build `brow` — but you are not inventing the category, you are competing on the *fat tree*, on *real input*, and on *state-aware site mapping*, and you should say so explicitly. The industry consensus for agent-facing page representation is settled: **accessibility tree + opaque refs keyed to (loaderId, backendNodeId)**, which is exactly what Google's own `chrome-devtools-mcp` does (`uid = "<snapshotId>_<counter>"`, reused while `${loaderId}_${backendNodeId}` is stable). The "no Playwright/Puppeteer" ban costs you a concrete, enumerable list of ~14 behaviours you must reimplement — that list is the risk register in §A2 and it is the single largest schedule risk in the project. On adapters: React/Vue/Angular/Svelte introspection is **mostly unavailable in production builds** and you must ship that honesty in the output schema, not discover it later. Flutter Web is *not* a stub — I verified end-to-end that you can force the semantics tree on with a **real browser-level gesture** (reposition `flt-semantics-placeholder` via `DOM.setAttributeValue`, then `Input.dispatchMouseEvent`), producing 41 `flt-semantics` nodes and a 206-node CDP AX tree on DartPad. On packaging: `SKILL.md` is now an actual cross-vendor open standard (agentskills.io) adopted by Claude Code, Codex, Cursor, OpenCode, Gemini CLI, Copilot/VS Code and ~40 others — **one skill folder + one CLI binary genuinely does install everywhere**, and MCP is not needed as the lowest common denominator.

---

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| Ref syntax `@e<N>` scoped to a snapshot generation, invalidated on loaderId change | Matches the two closest neighbours (`gstack` `@e3`, `chrome-devtools-mcp` `1_42`) and the industry's stable identity key `${loaderId}_${backendNodeId}`; agents already know the idiom | Opaque UUIDs (unreadable in transcripts); CSS selectors (fragile, token-expensive) | **Confirmed** (read chrome-devtools-mcp `TextSnapshot.ts`) |
| Copy Puppeteer's `USKeyboardLayout` *semantics*, reimplement the table in Rust from the Apache-2.0 source with attribution | 230 key definitions with `keyCode`/`key`/`code`/`shiftKey`/`shiftKeyCode`/`text`/`location` — a data table, not an algorithm; regenerating it from scratch is pure waste | Hand-rolling from the UI Events spec (misses Chrome's actual keyCode quirks) | **Confirmed** (read the file header + structure) |
| Reimplement Playwright's actionability checks as an explicit, *reportable* `readiness` object, not as invisible auto-wait | Agents need to know *why* a click was refused; a silent 30s retry loop is the worst possible agent UX | Silent auto-wait (Playwright's model); no checks at all (browser-use's model) | **Likely** |
| Framework adapters inject in the **MAIN world**, not an isolated world, and this is a declared capability | Empirically verified: React only registers with a main-world `__REACT_DEVTOOLS_GLOBAL_HOOK__`. An isolated world cannot see or be seen by the page's globals — this is definitional | Isolated-world-only adapters (would silently return nothing for React/Vue/Angular) | **Confirmed** (empirical, §Verified) |
| Every adapter output carries `build: "development" \| "production" \| "unknown"` and `nameSource: "debug" \| "displayName" \| "minified" \| "heuristic"` | Production React gives you `"eA"` as a component name and `null` for source location. Reporting `"eA"` without provenance is a lie to the agent | Silently omitting names; guessing from file paths | **Confirmed** (empirical) |
| Flutter Web adapter is a **real feature**, activated by reposition-then-real-click on `flt-semantics-placeholder` | Verified working on DartPad (Flutter 3.44.8). CDP `Accessibility.enable` does **not** trigger it; nothing auto-enables it | Declaring Flutter out of scope; using the Dart VM service (not exposed in release web builds) | **Confirmed** (empirical, controlled A/B) |
| Ship one `skills/browser/SKILL.md` conforming to the agentskills.io standard + symlink/copy recipe per harness | Claude Code, Codex, Cursor, OpenCode, Gemini CLI, Copilot, VS Code, Goose, Amp all read `SKILL.md` with `name`+`description` frontmatter; Cursor even reads `.claude/skills/` and `.codex/skills/` directly | One MCP server (burns 30–40k tokens of protocol framing per 20-command session per gstack's measurements; also re-exposes a tool surface you'd have to re-vet) | **Confirmed** (fetched all vendor docs) |
| No MCP server in v1; add a thin optional MCP shim in v2 | MCP is the *fallback* for harnesses without skills, not the primary. A CLI is cheaper in tokens, testable in a shell, and scriptable | MCP-first | **Likely** |
| Default CLI output is compact text; `--json` on every command; hard output cap with explicit truncation markers | gstack caps stdout at 1MB and exits non-zero; `gh` uses `--json <fields>`; chrome-devtools-mcp paginates console/network with `pageIdx`/`pageSize` | Dumping full DOM/AX trees | **Confirmed** |

---

## PART A — Prior art

### A1. `chrome-devtools-mcp` (Google / ChromeDevTools org)

**Architecture.** Node/TypeScript MCP server, Apache-2.0, ~48.5k stars. It **does** use Puppeteer — the README states it plainly: *"Uses puppeteer to automate actions in Chrome and automatically wait for action results."* [1][2] `src/` contains `McpContext.ts`, `McpPage.ts`, `TextSnapshot.ts`, `WaitForHelper.ts`, `PageCollector.ts`, `ServiceWorkerCollector.ts`, `HeapSnapshotManager.ts`, plus `tools/`, `formatters/`, `trace-processing/`, `daemon/`. [3]

**Tool surface — 52 tools in 10 categories** [4]:

> **Corrected 2026-08-04:** the previous draft said "64 tools", which contradicted its own table (the ten rows below sum to **52**). Re-fetched `docs/tool-reference.md` and `README.md`: both give **52**. Star count re-checked via the GitHub API: **48,522** (`api.github.com/repos/ChromeDevTools/chrome-devtools-mcp`, pushed 2026-08-04), so "~48.5k" stands.

| Category | Tools |
|---|---|
| Input (10) | `click(uid, dblClick, includeSnapshot)`, `drag(from_uid, to_uid)`, `fill(uid, value)`, `fill_form(elements)`, `handle_dialog(action, promptText)`, `hover(uid)`, `press_key(key)`, `type_text(text, submitKey)`, `upload_file(filePath, uid)`, `click_at(x, y, dblClick)` |
| Navigation (6) | `close_page`, `list_pages`, `navigate_page(url, handleBeforeUnload, ignoreCache, initScript, timeout, type)`, `new_page(url, background, isolatedContext)`, `select_page(pageId, bringToFront)`, `wait_for(text, timeout)` |
| Emulation (2) | `emulate(colorScheme, cpuThrottlingRate, extraHttpHeaders, geolocation, networkConditions, userAgent, viewport)`, `resize_page` |
| Performance (3) | `performance_start_trace(autoStop, filePath, reload)`, `performance_stop_trace`, `performance_analyze_insight` |
| Network (2) | `get_network_request(reqid, requestFilePath, responseFilePath)`, `list_network_requests(pageIdx, pageSize, resourceTypes, includePreservedRequests)` |
| Debugging (8) | `evaluate_script(function, args, dialogAction, filePath)`, `take_screenshot(filePath, format, fullPage, quality, uid)`, `take_snapshot(filePath, verbose)`, `list_console_messages`, `get_console_message`, `lighthouse_audit`, `screencast_start/stop` |
| Memory (12) | heap snapshot open/compare/dominators/retainers/duplicate-strings/… |
| Extensions (5) | `install_extension`, `list_extensions`, `reload_extension`, `trigger_extension_action`, `uninstall_extension` |
| 3rd-party (2) | `list_3p_developer_tools`, `execute_3p_developer_tool` |
| WebMCP (2) | `list_webmcp_tools`, `execute_webmcp_tool` |

**The ref mechanism — read the source, this is the single most valuable artefact in the prior art.** `TextSnapshot.create()` calls `page.pptrPage.accessibility.snapshot({includeIframes: true, interestingOnly: !verbose})`, then: [5]

```ts
const uniqueBackendId = `${node.loaderId}_${backendNodeId}`;
const existingMcpId = uniqueBackendNodeIdToMcpId.get(uniqueBackendId);
id = existingMcpId ?? `${snapshotId}_${idCounter++}`;   // e.g. "3_17"
```

That is: **refs are stable across snapshots for as long as `(loaderId, backendNodeId)` is stable, and the loaderId changes on cross-document navigation.** Your "@node-42 bound to a document generation" spec is the same design, independently arrived at. Adopt the same key; the only difference should be that you also cover non-AX nodes (see §A6).

**Auto-wait recipe — `WaitForHelper.ts`, exact constants** [6]:

| Field | Value |
|---|---|
| `#stableDomTimeout` | `3000 * cpuTimeoutMultiplier` ms |
| `#stableDomFor` | `100 * cpuTimeoutMultiplier` ms (MutationObserver quiet period, `{childList, subtree, attributes}` on `document.body`) |
| `#expectNavigationIn` | `100 * cpuTimeoutMultiplier` ms |
| `#navigationTimeout` | `3000 * networkTimeoutMultiplier` ms |

Navigation detection uses the **experimental** `Page.frameStartedNavigating` event and treats `navigationType ∈ {historySameDocument, historyDifferentDocument, sameDocument}` as *not a real navigation*. Dialogs are tracked separately with the comment *"Track all dialogs as they pause the renderer"* and a hard cap so `evaluateHandle` can't hang for the 180 s protocol timeout while a mutex is held. **Copy this state machine verbatim in spirit.**

**What it deliberately avoids:** no crawling/site mapping, no video recording synced to an action log (only `screencast_start/stop`), no capability tiers or approval gating, no persistent cross-session daemon story, no framework introspection. It also ships telemetry on by default (`--no-usage-statistics` to disable) and periodic npm update checks (`CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS`) — both of which your spec forbids, correctly.

**Cost of the Puppeteer ban here:** `accessibility.snapshot({includeIframes: true})` is *Puppeteer's* frame-stitching. CDP's own `Accessibility.getFullAXTree` (verified **EXPERIMENTAL** in local Chrome 151) takes only `depth` and `frameId` — **there is no `includeIframes`**. You must enumerate frames yourself and splice child trees at the owning iframe node.

> **Verified 2026-08-04 (this was asserted from Puppeteer/Playwright history; I have now measured it).** Fixture: a page at `http://127.0.0.1:8731/index.html` with one **same-origin** iframe (`/child.html`) and one **OOPIF** (`https://example.com/`), driven over raw CDP on Chrome 151.0.7922.72. Four findings, one of which is worse than the draft said and three of which are *better*:
>
> 1. **`getFullAXTree{}` does not cross *any* iframe boundary — not even same-origin.** It returned **11 nodes**, containing `TopButton` but **not** `SameOriginChildButton` and not `Example Domain`. Every node carried the top frame's `frameId`. The draft implied OOPIFs are the problem; in fact *every* iframe needs its own call. This is the load-bearing correction — see the new §A6.7.
> 2. **Same-process frames are one extra call.** `Accessibility.getFullAXTree{frameId}` on the **same page session** returned the child's 9 nodes (`SameOriginChildButton` present). No new session needed.
> 3. **The splice key exists and is cheap.** The top-frame AX tree contains `role: "Iframe"` nodes with `childIds: []` and **no `frameId` property**. `DOM.describeNode{backendNodeId}` on those nodes returns `frameId` for **both** the same-origin iframe (`C9335669…`, `contentDocument` present) and the OOPIF (`00FEEC12…`, `contentDocument` absent). For the OOPIF, **`frameId` is byte-identical to the iframe target's `targetId`** — that is the join.
> 4. **`Page.getFrameTree` from the page session omits OOPIFs entirely** — it listed only the top frame and the same-origin child. The OOPIF is reachable *only* via `Target.setAutoAttach{autoAttach:true, flatten:true}` → `Target.attachedToTarget{targetInfo.type == "iframe"}`; `getFullAXTree{}` on that session returned its 19 nodes.
>
> Working algorithm, ~5 call types:
> ```
> ax = Accessibility.getFullAXTree{}                       // page session, top frame only
> for n in ax where n.role == "Iframe":
>     fid = DOM.describeNode{backendNodeId: n.backendDOMNodeId}.node.frameId
>     if fid in Page.getFrameTree():  child = getFullAXTree{frameId: fid}   // same session
>     else:                           child = getFullAXTree{}               // session of target fid
>     splice child.RootWebArea under n
> ```
> **Verdict: the stitch itself is not "genuinely hard" — it is an afternoon.** What Playwright actually spent years on is the *lifecycle*: frames navigating mid-snapshot, `Page.frameDetached`/`Target.detachedFromTarget` races, and a frame→sessionId map that must be invalidated correctly. Re-scope risk item #6 from "OOPIF AX stitching" to "frame lifecycle bookkeeping".

### A2. Playwright: the papercut register (this IS the cost of "no Playwright")

Every row here is a bug you will ship unless you build it. Rows 1–5 are quoted from Playwright's actionability doc [7]; the rest are from Playwright API docs and the HN "Leaving Playwright for CDP" thread [8].

| # | Papercut Playwright absorbs | What you must build in `crates/input` + `crates/inspection` |
|---|---|---|
| 1 | **Visible** — non-empty bounding box AND not `visibility:hidden`. (`opacity:0` *passes*.) | `DOM.getBoxModel` + `CSS.getComputedStyleForNode`; do not conflate opacity with visibility |
| 2 | **Stable** — same bounding box for **two consecutive animation frames** | Poll box model across 2 rAF ticks; needs a rAF pump, not a sleep |
| 3 | **Receives events** — element is the hit target at the action point | `DOM.getNodeForLocation` at the click point, compare against target (and its shadow-including ancestors) |
| 4 | **Enabled** — not `[disabled]`, not a descendant of a disabled `<fieldset>`, not `[aria-disabled=true]` | AX node `disabled` property + DOM ancestor walk |
| 5 | **Editable** — enabled and not `[readonly]` / `[aria-readonly=true]` | Same |
| 6 | **Frame handling** — `Frame` objects, cross-origin OOPIF traversal, per-frame CDP session routing. Playwright *hides* frames; users of raw CDP report needing "hundreds of" state-tracking calls for "navigations with 30 different embedded frames" [8] | `Target.setAutoAttach{autoAttach:true, flatten:true, waitForDebuggerOnStart:true}` + a frame→sessionId map, invalidated on `Page.frameDetached`/`Target.detachedFromTarget` |
| 7 | **Event ordering / auto-wait after action** — see §A1 constants | `WaitForHelper` clone |
| 8 | **Downloads** — `Browser.setDownloadBehavior`, `Page.downloadWillBegin`/`downloadProgress`, suggested filename, saveAs | These CDP events are experimental and fire on the *browser* target; you need a download registry keyed by `guid` |
| 9 | **Dialogs** — `Page.javascriptDialogOpening` **pauses the renderer**; any pending `Runtime.evaluate` hangs until handled. Playwright queues dialogs and auto-dismisses when unhandled [9] | A dialog arbiter that pre-empts the command mutex; default policy must be explicit, not "hang" |
| 10 | **`beforeunload`** — `Page.navigate` silently blocks | `Page.close({runBeforeUnload})` + `Page.handleJavaScriptDialog` |
| 11 | **New windows / `target=_blank` / popups** — race between `Target.targetCreated` and the first navigation | Auto-attach with `waitForDebuggerOnStart:true` so you can install init scripts before first paint, then `Runtime.runIfWaitingForDebugger` |
| 12 | **File uploads** — `DOM.setFileInputFiles` needs a `backendNodeId`/`objectId`, and `Page.fileChooserOpened` needs `Page.setInterceptFileChooserDialog` | Explicit; also your `upload` is a gated capability |
| 13 | **Selector engines / text matching / `:has-text()` / shadow-piercing** | You get this for free *if* your fat tree is the selector engine — a genuine advantage |
| 14 | **Cross-browser** | You explicitly don't want it. Correctly: dropping Firefox/WebKit removes ~40% of Playwright's complexity budget |

Two more honest notes from the HN thread [8]: the counter-argument to raw CDP is that "the most difficult part is managing the lifecycle of Windows, Pages, and Frames and handling race conditions", and the pro-CDP argument that Playwright *doesn't expose* things you need (cross-origin iframe CDP, extension APIs, unique frame IDs). Browserbase, who wrote Stagehand on Playwright, published "Why we're graduating from Playwright" and cite exactly the frame-ID gap: *"Playwright doesn't expose a unique frame ID, which makes lifecycle tracking and per-frame CDP routing harder than it needs to be."* [10] So the direction of travel in 2026 favours your decision — but only if you actually build rows 1–12.

### A3. Puppeteer: what to learn, what you may copy

Puppeteer is **Apache-2.0**. Under that licence you may copy source verbatim into `brow` provided you retain the licence text, the copyright notice, and mark modified files (Apache-2.0 §4). Practically:

- **`USKeyboardLayout.ts`** — `/** @license Copyright 2017 Google Inc. SPDX-License-Identifier: Apache-2.0 */`, a `_keyDefinitions` record with **~253 entries**, fields `keyCode`, `key`, `code`, `shiftKey`, `shiftKeyCode`, `text`, `location` (1=left, 2=right, 3=numpad, 4=mobile). [11] This is *data*. Transliterate it into a Rust `phf` map, keep the licence header, note the provenance in `NOTICE`. Hand-deriving 250-odd Windows virtual-key codes from the UI Events spec is a week of avoidable bugs.

  > **Corrected 2026-08-04:** the draft said **230 entries**; that number was not measured. Re-fetched `puppeteer/main` (18,746 bytes) and counted top-level record entries with `grep -cE "^  '?[^ :]+'?: \{"` → **253**. Field frequencies in the same file: `key:` 255, `keyCode:` 250, `code:` 247, `shiftKey:` 58, `location:` 42, `shiftKeyCode:` 11, `text:` 4. The Apache-2.0 header is verbatim as quoted. Treat 253 as approximate (the grep counts object entries, some of which are aliases) — the point is the table is ~250 rows, not 230, and you should count it at transliteration time rather than trust either number.
- **Pipe transport (`--remote-debugging-pipe`)** — Chrome reads NUL-delimited JSON on **fd 3** and writes on **fd 4**. I verified this works on Chrome 151 (all empirical work in this document used a from-scratch pipe client, no port, no HTTP). This is strictly better than a TCP port for your threat model: nothing on localhost can connect to the browser, so the daemon is the only gatekeeper. **Use pipe transport, not `--remote-debugging-port`.**
- **Full-page screenshots** — historically Puppeteer resized the viewport, painted, and restored; the modern path is `Page.captureScreenshot{captureBeyondViewport: true}` plus `Page.getLayoutMetrics` for `cssContentSize`. [12] Note `captureBeyondViewport` has a long history of interacting badly with `position:fixed`/sticky elements (they repeat or float) — a known trap for your "sticky/fixed correct" requirement.

### A4. The 2026 agent-browser landscape, briefly and factually

| System | Representation | Note |
|---|---|---|
| **browser-use** | DOM+AX text, moving hybrid with screenshots in 2.0 | ~81k stars, ~89.1% WebVoyager, ~$0.07/10-step task [13] |
| **Stagehand** (Browserbase) | AX tree + DOM via CDP, `act()/extract()/observe()`, MIT | Explicitly moving *off* Playwright toward direct CDP [10] |
| **Skyvern** | **Pure vision** — "no DOM parsing, no accessibility tree, just pixels"; Planner-Actor-Validator; v1 ~45% → v2 ~85.85% WebVoyager [13] | The one genuine architectural alternative to your approach |
| **Steel.dev** | Open-source browser API; runs headless Chrome and *exposes CDP* + an Nginx UI; Puppeteer/Playwright/Selenium clients [14] | Closest to "brow as a service" |
| **Browserbase / Hyperbrowser** | Cloud Chromium, session persistence, stealth, recordings | Cloud-first; disqualified by your no-cloud constraint |
| **Claude in Chrome** | Extension over the user's real Chrome | Product, not a library; permissioned per-site |
| **ChatGPT Atlas** | Shutdown **announced 9 July 2026**; the browser **stops functioning 9 August 2026** (data-export deadline), folded into the ChatGPT desktop app + a Chrome extension + a server-side cloud browser [15] | Do not model against it |
| **Perplexity Comet** | Full replacement browser | Product |

> **Corrected 2026-08-04:** the draft's date was wrong and its source was weak. TechCrunch and 9to5Mac both carry the shutdown story dated **2026-07-09** (announcement), and OpenAI's own help-centre article *"Evolving Atlas into ChatGPT for browser-based agentic work"* gives users until **9 August 2026** — five days from today — to export data before Atlas stops working. Replace source [15] (`dualmedia.fr`, a secondary aggregator) with `help.openai.com/en/articles/20001371-evolving-atlas-into-chatgpt-for-browser-based-agentic-work`. The substantive conclusion ("don't model against it") is unaffected.

**Benchmarks.** WebVoyager is **saturated** — 97–98% for top commercial agents, Alumnium tracked at 98.5% (Mar 2026); it is 643 tasks over 15 sites and mostly read-heavy. WebArena sits at 68.7% (Claude Mythos Preview) to 74.3% (WebTactix/DeepSeek v3.2). The stated criticism is precisely your differentiator: *"the harder problems are logging in, solving 2FA, filling out forms, and downloading files, which are underrepresented or missing entirely, and WebVoyager says nothing about browser infrastructure running underneath each agent."* Web Bench (5,750 tasks / 452 sites, read vs write separated, infrastructure measured) is the 2026 replacement. [16][17] **Recommendation: do not target WebVoyager. Target Web Bench's write-task split and publish route-coverage numbers, which nobody else reports.**

### A5. `gstack` — found, and it is your nearest neighbour

The owner's reference is real and public: **`garrytan/gstack`**, documented in `BROWSER.md` (126,253 stars, MIT, TypeScript). [18] There is also **`gsd-build/gsd-browser`**, which is *even closer to brow*: **Rust**, native binaries for macOS/Linux/Windows, persistent daemon over loopback HTTP, `chromiumoxide` as the CDP backend, **92 top-level commands across 22 areas**, versioned refs `@v<N>:e<M>`, `--json` on every command, MIT-or-Apache-2.0. [19]

> **Verified 2026-08-04, with three corrections.**
> 1. **The canonical/mirror direction is backwards in the draft.** `gsd-build/gsd-browser` has **252 stars**; `open-gsd/gsd-browser` has **37**. The README's own install (`curl -fsSL https://install.gsd.build/browser | bash`) and clone URL both point at **`gsd-build`**. Treat `gsd-build` as canonical.
> 2. **`chromiumoxide` is confirmed — but from `Cargo.toml`, not the README** (which never names it). `gsd-build/gsd-browser` `Cargo.toml` on both `main` and `master` pins `chromiumoxide = "0.9"`, alongside `tokio`, `clap 4`, `nix 0.29`, `image 0.25`, `reqwest 0.12`. **No `chromey` and no stealth-backend feature flags appear** — the draft's "with `chromey` and stealth backends behind feature flags" is unsupported; drop it unless you can point at the feature table.
> 3. **Command count is 92**, per the README ("92 top-level commands"), not "90+" as a vague figure. Transport is a **loopback TCP port** (`GSD_BROWSER_DAEMON_PORT`, default 9333; `cdp_url = "http://localhost:9222"` to attach to an existing Chrome) — i.e. *not* a unix socket, which is a genuine differentiator for brow's threat model, since anything on localhost can reach a TCP port.

What to steal from `gstack BROWSER.md` [18]:

- **Deny-default CDP allowlist**: *"Only methods enumerated in `browse/src/cdp-allowlist.ts` are reachable; any other method returns 403."* Each entry declares **scope** (tab vs browser) and **output trust** (trusted vs untrusted); untrusted methods (e.g. `Network.getResponseBody`) get UNTRUSTED-envelope-wrapped output. — *This is a better model than your "raw CDP is never exposed": keep the deny-default allowlist as an internal invariant even though no raw-CDP command ships.*
- **Token tiers**: root token (daemon lifetime, local listener only), setup key (5 min, one-time), scoped token (24 h, allowlist-bound, `tabPolicy: 'own-only'`). Maps cleanly onto your capability modes.
- **Output contract**: stdout = JSON or plain text, stderr = streaming logs, exit 0/non-zero, **1 MB max stdout (truncate + exit non-zero)**, 60 s default timeout with `--timeout=Ns`.
- **Refs**: `@e<N>` for AX-tree elements, **`@c<N>` for "cursor-interactive" non-ARIA elements** (divs with `cursor:pointer` / `onclick`) — a pragmatic patch for the AX tree's biggest blind spot.
- **Staleness**: `resolveRef()` runs a `count()` check before every use and **throws immediately** rather than waiting for a timeout, forcing a fresh snapshot. Adopt this: fail fast, tell the model to re-snapshot.
- **Context cost claim**: gstack measures ~0 context tokens for CLI stdout vs ~1,500–2,000 tokens per MCP call, i.e. 30–40k tokens of protocol framing over a 20-command session. Treat as *the vendor's own number* (unverified by me) but directionally right and a strong argument for CLI-over-MCP.

  > **Verified 2026-08-04 — the quote is accurate; the fact is still unestablished.** Re-fetched `BROWSER.md`. Verbatim: *"In a 20-command browser session, MCP tools burn 30,000–40,000 tokens on protocol framing alone. gstack burns zero."*, with per-call figures *"~2000 tokens (schema + protocol)"* for Chrome MCP and *"~1500 tokens (schema + protocol)"* for Playwright MCP. So the draft **correctly reproduces gstack's wording** — but this remains **marketing arithmetic from the party being compared**, and the "zero" is definitionally false for brow: brow's stdout is tokens too. The real quantity is *total* tokens (tool schemas + framing + output), and a fat-tree CLI can easily lose to a lean MCP tool on that measure. **Do not cite this number in brow's own README.** If CLI-over-MCP needs defending, measure it: same 20-task script, count tokens end-to-end, publish both. Also re-confirmed from the same file: the deny-default allowlist (*"Only methods enumerated in `browse/src/cdp-allowlist.ts` (`CDP_ALLOWLIST` const) are reachable"*), the 1 MB stdout cap (*"Max stdout 1MB (truncate + non-zero exit if exceeded)"*), the three token tiers, and — importantly — that the snapshot is built on **`page.locator(scope).ariaSnapshot()`**, i.e. Playwright, *"mapping them back to Playwright Locators"*. The draft's caveat stands.

Caveat worth flagging: `gstack browse` builds its snapshot on **Playwright's `ariaSnapshot()`**. So `gstack`'s ref design is downstream of the layer you banned. `gsd-browser` is the proof that the same design works on raw CDP in Rust.

### A6. Where the "unified fat tree" genuinely beats AX+refs — and where it doesn't

**Genuinely beats it:**

1. **Non-semantic interactives.** The AX tree drops `<div onclick>` with no role. gstack needed a whole second ref namespace (`@c<N>`) to patch this. A DOM∪AX∪listener tree gets it in one model, and `DOMDebugger.getEventListeners` gives you the *evidence* (a real `click` listener) instead of a `cursor:pointer` heuristic.
2. **Closed shadow roots.** `DOM.getDocument{pierce:true}` returns closed shadow roots to a CDP client; page JS cannot. Every JS-injected snapshotter (browser-use, Stagehand, gstack skills) is blind here. This is a real, defensible capability.

   > **Verified 2026-08-04 — confirmed, and it is the strongest differentiator in this section.** Fixture with one `attachShadow({mode:'closed'})` host and one open host, Chrome 151. Page JS: `document.getElementById('host').shadowRoot` → **`false`** (null, as the spec requires). CDP: `DOM.getDocument{depth:-1, pierce:true}` **sees `ClosedShadowButton`**; so do `Accessibility.getFullAXTree` and `DOMSnapshot.captureSnapshot`. Unexpected extra datum: `DOM.getDocument{depth:-1, pierce:false}` **also** surfaced the closed-shadow content on Chrome 151 — so do not treat `pierce:false` as a privacy boundary or as a cheaper "light DOM only" mode; measure before relying on it either way.
3. **Paint order / occlusion.** `LayerTree` + `DOMSnapshot.captureSnapshot{computedStyles, includePaintOrder:true}` answers "is this element actually clickable" better than an AX role ever can, and is the correct basis for the "receives events" check (row 3 above).
4. **Provenance for edges in the site graph.** An AX snapshot is a photograph; your model needs the *trigger node* that produced a transition. Only a unified node model can carry `trigger_node_ref` on a graph edge.

**Does not beat it:**

5. **Token cost.** An AX-only snapshot of a typical page is a few hundred lines. A fat tree with CSS, listeners, layout and paint order is 10–50×. **The fat tree must never be the default output.** Default must be an AX-shaped projection; the fat data is fetched per-node on demand (`inspect node @e12 --with css,listeners,layout`). If you get this wrong the product is unusable regardless of how good the model is.
6. **Model comprehension.** No 2026 evidence exists that models use CSS cascade or paint order well. The benefit is to *your* correctness logic (hit-testing, screenshot clipping), not to the model's prompt.

---

## PART B — Framework adapters

### B0. The adapter contract (and the isolated-world problem)

**Injection.** `Page.addScriptToEvaluateOnNewDocument` — confirmed non-experimental in Chrome 151, with **experimental** optional params `worldName`, `includeCommandLineAPI`, `runImmediately`. Two worlds, two purposes:

```
worldName absent  -> MAIN world   : required for framework hooks (React/Vue/Angular globals)
worldName: "brow" -> ISOLATED world: safe for your own probe utilities; cannot see page globals
```

**This breaks the brief.** The spec says adapters are "injected JS in an isolated world at document start". For React that is impossible: React reads `window.__REACT_DEVTOOLS_GLOBAL_HOOK__` from the **main** world, and an isolated world has a separate `window`. I verified the main-world path works (see §Verified). Consequence for the security model: **adapter injection is a main-world write and must be a declared, per-origin capability**, not a free inspection primitive. Mitigations: (a) inject only a hook *stub* that records, never a full agent; (b) freeze the stub with `Object.defineProperty(..., {configurable:false, writable:false})`; (c) prefix everything `__brow_`; (d) always report `adapter.injected: true` in `inspect` output so the agent knows the page was touched; (e) keep the *read* side (walking fibers) in the main world but marshal only JSON-safe scalars out; (f) **read the stub back and verify a sentinel** — see below.

> **Verified 2026-08-04 — the draft flagged "collision with a real DevTools extension / CSP / pages that freeze globals" as untested. I tested all three on Chrome 151, and the honest answer is that the stub as designed LOSES by default.** Isolated-world control first: the same stub injected with `worldName:"browiso"` and then read from the main world gives `{stub:false, hook:"undefined"}` — the definitional claim is **confirmed**, an isolated world is invisible to the page.
>
> | Trial | Setup | Result |
> |---|---|---|
> | **A** | main-world stub, then load `react.dev` | `renderers` → `{1:{version:"19.0.0", bundleType:0, rendererPackageName:"react-dom"}}`, exactly **6** renderer keys, `getFiberRoots(1).size === 1`. Reproduces the draft. |
> | **B** | isolated-world stub, then load `react.dev` | `{stub:false, hook:"undefined"}` — invisible, as expected. |
> | **C** | page with `Content-Security-Policy: script-src 'self'` (no `unsafe-inline`) | The page's **own inline script was blocked** by its CSP; **our injected stub still ran** (`stub:true`). |
> | **D** | page assigns the hook from an external `<script src>` | Our stub runs first (`priorHookType:"undefined"`), then the page **silently overwrites it** → `finalHookOwner:"SITE"`. |
> | **E** | same as D, but we `Object.defineProperty(…, {writable:false, configurable:false})` after installing | We keep it (`finalHookOwner:"BROW_STUB"`), and the page's assignment fails **silently** — non-strict assignment to a non-writable property does not throw. |
> | **F** | page freezes the property *before* us | Our assignment fails and `window.__brow_install_error__` is **`null`** — no exception, no signal. `finalHookOwner:"SITE_FROZEN"`. |
>
> Three consequences the draft misses:
>
> 1. **D is the real React DevTools extension case.** Anything that assigns the hook after document-start wins, and we get no error. A stub that is not read back reports success while being gone.
> 2. **E is not a free mitigation.** Freezing keeps our stub but silently breaks the user's actual DevTools extension on that page, with no diagnostic on either side. Freeze must be opt-in (`--adapter-exclusive`), never the default.
> 3. **F shows the failure is silent in the other direction too.** `installError` was `null`. **Mandatory:** after injection, evaluate a sentinel (`window.__REACT_DEVTOOLS_GLOBAL_HOOK__?.BROW_STUB === true`) and branch on the answer.
>
> **`hookSource` therefore needs four values, not three:** `"injected"` (our stub is live) · `"native"` (a hook was already present and we left it alone) · `"displaced"` (we injected, something overwrote us — trial D) · `"blocked"` (the property was non-writable before us — trial F). Reporting `"injected"` in cases D and F is a lie to the agent, which is exactly the failure mode this document's own `nameSource`/`build` decision exists to prevent.
>
> **Separately, and this belongs in the security model:** trial C proves **`Page.addScriptToEvaluateOnNewDocument` bypasses the page's Content-Security-Policy.** Our script executed on a document whose CSP forbade the page's own inline script. That is a real privilege — brow's injection is strictly more powerful than anything the page can do to itself, and no site-side policy can opt out. It must be named in the capability docs, not discovered by a user.

**Degradation.** Every adapter returns a common envelope; absence is a first-class value, not an error:

```jsonc
{ "framework": "react", "detected": true, "version": "19.0.0",
  "build": "production",              // from bundleType: 0=prod, 1=dev
  "hookSource": "injected",           // "injected" | "native" | "displaced" | "blocked" | "absent"
                                      //   -- MUST be decided by reading a sentinel back, not by
                                      //      assuming the injection succeeded (see trials D/F above)
  "capabilities": { "componentTree": true, "props": true, "state": "partial",
                    "sourceLocation": false, "override": false },
  "nameSource": "minified",           // debug | displayName | minified | heuristic
  "warnings": ["production build: component names are minified, source locations unavailable"] }
```

**Same node model.** Adapters never emit their own tree. They emit `{ node_ref, component: {...} }` annotations that the `inspection` crate merges onto existing fat-tree nodes, keyed by `backendNodeId`. A component with no host DOM node (React fragments, RSC virtual instances) attaches to its nearest host descendant with `attachment: "nearest-host"`.

### B1. React (18/19)

**The hook is not there by default.** Verified on `react.dev`: `typeof window.__REACT_DEVTOOLS_GLOBAL_HOOK__ === "undefined"` in a clean Chrome with no extension. The hook is injected *by the DevTools extension*; React only calls it if present. So the adapter must install a stub before the page's React bundle evaluates.

Minimal stub (verified sufficient for React 19 to register):

```js
window.__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
  renderers: new Map(), supportsFiber: true,
  inject(r) { const id = ++uid; renderers.set(id, r); roots.set(id, new Set()); return id; },
  getFiberRoots(id) { return roots.get(id) ?? new Set(); },
  onCommitFiberRoot(id, root) { roots.get(id).add(root); },
  onCommitFiberUnmount() {}, onPostCommitFiberRoot() {},
  checkDCE() {}, on() {}, off() {}, emit() {}, sub() { return () => {}; },
};
```

Result on `react.dev`: `renderers` → `{1: {version: "19.0.0", bundleType: 0, rendererPackageName: "react-dom", …}}`, `getFiberRoots(1).size === 1`. **`bundleType: 0` means production; `1` means development** — use it as the authoritative build discriminator.

**DOM node → fiber, without the hook.** React always stamps DOM nodes with randomised-suffix keys. Verified present on `react.dev` (production, React 19): `__reactFiber$<rand>`, `__reactProps$<rand>`, `__reactContainer$<rand>`, `__reactEvents$<rand>`, `__reactMarker$<rand>`. Find them with `Object.keys(el).find(k => k.startsWith('__reactFiber$'))`. Walk `fiber.return` for ancestors, `.child`/`.sibling` for descendants, `.alternate` for the other of the double buffer.

**Fiber fields present in production React 19** (verified, exhaustive `Object.keys` on a real fiber): `tag, key, elementType, type, stateNode, return, child, sibling, index, ref, refCleanup, pendingProps, memoizedProps, updateQueue, memoizedState, dependencies, mode, flags, subtreeFlags, deletions, lanes, childLanes, alternate`.

**What is missing in production — say this loudly.** `_debugSource`, `_debugOwner`, `_debugInfo`, `_debugHookTypes` are **absent**. `fiber.type.displayName ?? fiber.type.name` on a minified build returns things like `"eA"`. So:

| Capability | Dev build | Production build |
|---|---|---|
| Component tree shape | ✅ | ✅ |
| Props (`memoizedProps`) | ✅ | ✅ (prop *names* survive unless mangled) |
| Hook state (`memoizedState` linked list) | ✅ (typed via `_debugHookTypes`) | ⚠️ untyped linked list, positional only |
| Component **name** | ✅ | ❌ minified (`"eA"`) |
| **Source file:line** | ✅ only with `@babel/plugin-transform-react-jsx-source` (default in dev) | ❌ **never** |
| `overrideProps` / `overrideHookState` | ✅ | ❌ (production renderer object exposes only 6 keys: `bundleType, version, rendererPackageName, currentDispatcherRef, findFiberByHostInstance, reconcilerVersion`) |

**React Server Components.** RSC components have no client fiber of their own; React DevTools reconstructs them from `fiber._debugInfo`, building "VirtualInstances" — *"each Fiber contains a list of its parent Server Components in `_debugInfo`"* (facebook/react PR #30684) [20]. Since `_debugInfo` is dev-only, **RSC component boundaries are not recoverable in a production Next.js build.** Report `capabilities.serverComponents: false` and stop. What *is* recoverable in production is the RSC flight payload on the wire (`self.__next_f.push(...)` script chunks) — that's a Next.js-specific heuristic, not an RSC API, and belongs in the Next adapter as `confidence: "inferred"`.

**Next.js specifics worth having:** `window.next.router` (`.route` = the route template, `.pathname`, `.query`, `.asPath`) and `__NEXT_DATA__` for the pages router; `self.__next_f` flight chunks for the app router; `window.next.version`. These give you **route templates for free** — directly feeding your site-graph "route template" requirement with `status: declared`.

**Alternative considered and rejected:** `react-devtools-inline`/`react-devtools-core` backend. It gives a full, battle-tested bridge, but it is MIT-licensed *JavaScript you must bundle and keep in sync with React*, it installs a much larger main-world surface, and it doesn't change any of the production limitations above. Ship the ~60-line stub.

### B2. Vue / Nuxt

- **`el.__vue_app__`** — set on the mount root. Verified present on `vuejs.org` (production). From it: `app._instance`, `app.version`, `app.config`.
- **`el.__vueParentComponent`** — I scanned 656 elements on `vuejs.org` and found **only `__vue_app__`**, no `__vueParentComponent`. Treat per-element parent-component back-references as **dev/`__VUE_PROD_DEVTOOLS__` only**; do not build the adapter's primary path on it. Fall back to walking `app._instance` → `subTree` → `component` and mapping `vnode.el` back to DOM nodes.

  > **Verified 2026-08-04 — upgraded from one-site inference to source-confirmed.** The draft flagged this as resting on a single production site (`vuejs.org`, which may itself ship `__VUE_PROD_DEVTOOLS__`) with the Vue runtime source unread. I read it. `vuejs/core` `packages/runtime-core/src/renderer.ts` guards the assignment explicitly:
  > ```js
  > if (__DEV__ || __FEATURE_PROD_DEVTOOLS__) {
  >   def(el, '__vnode', vnode, true)
  >   def(el, '__vueParentComponent', parentComponent, true)
  > }
  > ```
  > So the claim is **confirmed at the source level**, and it is precisely "dev **or** `__VUE_PROD_DEVTOOLS__`", not "dev only" — a build that sets the prod-devtools flag *will* expose it. Note `__vnode` is behind the same guard, so it is unavailable on the same builds. The adapter must therefore probe for both and degrade, rather than assume either. **Still unverified:** whether `app._instance` (the fallback path above) survives the same guard — check `apiCreateApp.ts` before relying on the fallback, because if `_instance` is also dev-gated the fallback is vapour and Vue production support collapses to `__vue_app__` + `version` only.
- **`window.__VUE_DEVTOOLS_GLOBAL_HOOK__`** — verified `undefined` on `vuejs.org` with no extension. Same stub trick as React: install a hook object with `Vue.__vue_devtools_global_hook__`-compatible `emit/on/once/off` before load; Vue's runtime calls `devtoolsInitApp`. Production builds only emit devtools events when `__VUE_PROD_DEVTOOLS__` was defined at build time [21] — so, again, **most production Vue apps give you the app instance and the vnode tree but no reactive-state stream**.
- Per-component: `instance.type.__name` / `.name`, `instance.type.__file` (**dev only** — this is the source-location field), `instance.props`, `instance.setupState`, `instance.data`.
- **Nuxt**: `window.__NUXT__` (payload, `data`, `state`, `route`) and `useNuxtApp()`'s `$router` give route templates.

### B3. Svelte 5, Angular

**Svelte 5.** Runes broke the old inspection story: there is no component instance object to read any more — the compiler emits plain closures and signal cells. The official debugging affordances are compile-time: `$inspect(value)` (re-logs on dependency change) and `$state.snapshot(x)` to unwrap a proxy [22]. Practically: `$state` proxies do not survive `JSON.stringify` cleanly, and even VS Code's debugger can't read them without `JSON.parse(JSON.stringify($$props.x))` workarounds [23]. **Recommendation: Svelte adapter is component-*boundary* detection only** — dev builds emit `data-svelte-h` hydration markers and `__svelte_meta` on elements (`{loc: {file, line, column}}`, dev-only). Report `capabilities.props: false, state: false` for Svelte 5 production and mean it. This is the weakest of the five adapters; scope it as such rather than promising parity.

**Angular.** The `window.ng` global (`getComponent`, `getContext`, `getOwningComponent`, `getRootComponents`, `getDirectives`, `getDirectiveMetadata`, `getHostElement`, `getInjector`, `getListeners`, `applyChanges`) is published by `publishDefaultGlobalUtils()` and is **development-mode only** — `enableProdMode()` removes it [24]. Verified `typeof window.ng === "undefined"` on the pages I probed. `ng.probe` is the pre-Ivy API and is gone. So: Angular adapter works beautifully on `ng serve` and returns `detected: true, capabilities: {componentTree: false}` on a production build. Fallback signals that *do* survive production: `_nghost-*` / `_ngcontent-*` attributes (component boundaries, ViewEncapsulation.Emulated), `ng-version` on the root element, and `ng-server-context` for SSR. Those give you boundaries and a version, nothing else.

### B4. Flutter Web — the hard one, and it is a real feature

**The rendering model.** Verified on DartPad (Flutter 3.44.8 / Dart 3.12.2): the light DOM is `<flutter-view><flt-glass-pane>`, and **`flt-glass-pane` has an OPEN shadow root** containing `flt-scene-host > flt-scene > flt-canvas-container > canvas` plus `flt-clip > flt-platform-view-slot > slot` for `HtmlElementView` platform views (which live in the light DOM as `<flt-platform-view id="flt-pv-0">` and are slotted in). Open shadow root ⇒ `DOM.getDocument{pierce:true}` and page JS both reach it; you don't need the closed-shadow-root machinery here.

**Semantics are OFF by default and nothing turns them on for you.** Controlled experiment on `https://dartpad.dev/`:

| Trial | Action at t=9 s | `flt-semantics` count at t=34 s |
|---|---|---|
| A (baseline) | none | **0** (polled every second for 34 s) |
| B | `Accessibility.enable` + `Accessibility.getFullAXTree{depth:-1}` | **0** (AX tree returned 144 nodes, all page chrome, no Flutter widgets) |
| C | `Input.dispatchMouseEvent` at the placeholder's reported centre `(0,0)` | **0** — the placeholder is a 1×1 box at `(-1,-1)`, so a real click at (0,0) misses it |
| D | scripted `element.click()` on `flt-semantics-placeholder` | **41**, placeholder removed |
| E | `DOM.setAttributeValue(style)` to move the placeholder into the viewport, then real `Input.dispatchMouseEvent` press+release at its box centre | **41**, placeholder removed |

Trial E is the answer, and it satisfies your "browser-level input only" constraint:

```rust
// crates/inspection/src/adapters/flutter.rs  (sketch)
let doc  = cdp.send("DOM.getDocument", json!({"depth": 1}), sid).await?;
let node = cdp.send("DOM.querySelector",
    json!({"nodeId": doc["root"]["nodeId"], "selector": "flt-semantics-placeholder"}), sid).await?;
// stash the original style so we can restore it
cdp.send("DOM.setAttributeValue", json!({
    "nodeId": node["nodeId"], "name": "style",
    "value": "position:fixed;left:200px;top:200px;width:60px;height:60px;\
              z-index:2147483647;opacity:0.01"}), sid).await?;
let bm = cdp.send("DOM.getBoxModel", json!({"nodeId": node["nodeId"]}), sid).await?;
let (x, y) = (bm.content[0] + bm.width/2.0, bm.content[1] + bm.height/2.0);
for t in ["mouseMoved", "mousePressed", "mouseReleased"] { dispatch_mouse(t, x, y).await?; }
// Flutter removes the placeholder itself; ~<1s later flt-semantics nodes exist.
```

After enabling, `Accessibility.getFullAXTree` returned **206 nodes** with genuine Flutter widget labels: `"Run"`, `"Show docs"`, `"Create"`, `"Create with Gemini"`, `"Samples"`, `"Create a new snippet DartPad"`, `"Dart 3.12.2 • Flutter 3.44.8"`. Each `flt-semantics` node carries `id="flt-semantic-node-<N>"`, an ARIA `role`, an `aria-label`, and an absolute-positioned inline style with the **exact widget rect** (`width`, `height`, `transform-origin`, `z-index`) — which means per-widget screenshots and per-widget hit-testing are both possible on top of it.

> **Verified 2026-08-04 against the Flutter engine source — the technique is real, but the draft picked the wrong mechanism and missed a mobile-mode hazard.** The draft's evidence was one app (DartPad) and no source. I read `flutter/flutter` `engine/src/flutter/lib/web_ui/lib/src/engine/semantics/semantics_helper.dart`, which defines `SemanticsHelper` delegating to two enablers:
>
> **`DesktopSemanticsEnabler`** — placeholder styled `position:absolute; left:-1px; top:-1px; width:1px; height:1px` (exactly the `{x:-1,y:-1,w:1,h:1}` box measured on DartPad, so trial C's failure is fully explained). It activates **immediately** when `event.target == _semanticsPlaceholder`, with **no timer**, for any of: `'click'`, `'keyup'`, `'keydown'`, `'mouseup'`, `'mousedown'`, `'pointerdown'`, `'pointerup'`.
>
> **This means the `DOM.setAttributeValue` reposition is not required.** `keyup`/`keydown` are in the accepted set, and keyboard events are delivered to the **focused** element regardless of geometry. So a cheaper and less invasive activation is:
> ```
> DOM.focus{nodeId: <flt-semantics-placeholder>}        // it is role="button", focusable
> Input.dispatchKeyEvent{type:"keyDown", key:"Enter", …}
> Input.dispatchKeyEvent{type:"keyUp",   key:"Enter", …}
> ```
> which is still a genuine browser-level input event (constraint satisfied) but **mutates no attribute**, so there is no original `style` to stash and restore and no window in which the page sees a repositioned element. Prefer this path; keep trial E's reposition as the fallback. *(Source-derived: the accepted-event list is read from the engine; I did not re-run DartPad with the keyboard path — verify it in week one before deleting the reposition code.)*
>
> **`MobileSemanticsEnabler` is a different animal and the draft does not mention it.** Its placeholder covers the **entire viewport** (`left/top/right/bottom: 0`), it **consumes events for `_periodToConsumeEvents` = 300 ms**, it requires the tap to land **within 1 px of the placeholder's centre** (because VoiceOver/TalkBack centre their synthetic taps), and it gives up after `kMaxSemanticsActivationAttempts` = **20** events. Two consequences for brow: (a) the desktop recipe above **does not transfer** to mobile mode; (b) more importantly, **whenever brow sets `Emulation.setDeviceMetricsOverride` to a mobile profile on a Flutter app, a full-viewport invisible element may swallow the first 300 ms of input** — so brow's *own* emulation setting silently changes which enabler is live and can eat the agent's first gesture on any page, not just when the adapter is requested. Detect the enabler in use before dispatching anything.

**Caveats to state in the docs:** (1) this is a **page mutation** — it belongs in `mutate` or a dedicated `adapter` capability, and (if you use the reposition path rather than the focus+key path above) the harness must restore the original `style` attribute afterwards; (2) `flt-semantic-node-<N>` ids are Flutter's own and are **not stable across rebuilds** of the semantics tree, so your refs must key on `backendNodeId`, not on the id; (3) app authors can opt in with `SemanticsBinding.instance.ensureSemantics()` [25], in which case the placeholder never appears and there is nothing to do — detect this case first; (4) the semantics tree is a *lossy* projection: unlabelled decorative widgets, custom painters and non-`Semantics`-wrapped `GestureDetector`s simply are not there. Content that isn't in the semantics tree is **unreachable by any DOM means** — it's pixels in a canvas. Vision (screenshot + coordinates) is the only fallback, and you should say so rather than pretend.

**The Dart VM service alternative: don't.** The VM service / Dart DevTools protocol (`--enable-vm-service`) exists in JIT/debug builds. Release web builds are compiled with dart2js or dart2wasm and **have no VM service at all**. It is not a channel you can rely on for a deployed Flutter web app. Mention it in docs as "debug builds only", never as a supported path.

---

## PART C — Agent-side integration

### C1. Portable packaging — the good news

`SKILL.md` became a genuine cross-vendor standard in 2026: **agentskills.io**, originally from Anthropic, released as an open standard [26]. The listed adopters include Claude Code, ChatGPT/Codex, Cursor, OpenCode, Gemini CLI, GitHub Copilot, VS Code, Goose, Amp, OpenHands, JetBrains Junie, Roo Code, Factory, Kiro, Letta, Mistral Vibe and ~25 more. The core is: a folder with `SKILL.md` (frontmatter with at minimum `name` + `description`) plus optional `scripts/`, `references/`, `assets/`; three-stage progressive disclosure (discovery → activation → execution).

**Discovery paths, per vendor (all fetched from vendor docs):**

| Harness | Paths | Source |
|---|---|---|
| Claude Code | `~/.claude/skills/<name>/SKILL.md` (personal), `.claude/skills/<name>/SKILL.md` (project, loaded from cwd **and every parent up to repo root**), `<plugin>/skills/<name>/SKILL.md`; nested `.claude/skills/` load lazily under a directory-qualified name `apps/web:deploy`; live file-watching, no restart | [27] |
| Codex / ChatGPT | `$CWD/.agents/skills`, `$REPO_ROOT/.agents/skills`, `$HOME/.agents/skills`, `/etc/codex/skills`, bundled. Disable via `~/.codex/config.toml`: `[[skills.config]] path=".../SKILL.md" enabled=false`. Optional `agents/openai.yaml` for `interface.display_name`, `policy.allow_implicit_invocation`, `dependencies.tools` | [28] |
| Cursor | `.agents/skills/`, `.cursor/skills/`, `~/.agents/skills/`, `~/.cursor/skills/` — **and also reads `.claude/skills/` and `.codex/skills/`**. Frontmatter: `name`, `description` required; `paths`, `disable-model-invocation`, `metadata` optional | [29] |
| OpenCode | **global:** `~/.config/opencode/skills/<name>/SKILL.md`, `~/.claude/skills/<name>/SKILL.md`, `~/.agents/skills/<name>/SKILL.md`; **project:** walks cwd up to the git worktree root reading `.opencode/skills/`, `.claude/skills/`, `.agents/skills/`. Frontmatter `name`+`description` required; `license`, `compatibility`, `metadata` optional. Also `opencode.json` for agents/commands/MCP | [30] |
| Gemini CLI / Copilot / VS Code / Goose / Amp | all listed as Agent Skills clients with their own doc pages | [26] |

**Concrete layout for `brow`:**

```
brow/
├── skills/browser/
│   ├── SKILL.md                    # ~400 lines, the only always-loaded surface
│   ├── references/
│   │   ├── refs-and-snapshots.md   # ref lifecycle, staleness recovery
│   │   ├── input.md                # gestures, IME, touch, drag
│   │   ├── jobs.md                 # detached jobs, approval gates
│   │   ├── sitemap.md              # graph model, edge statuses, coverage
│   │   └── troubleshooting.md      # error code -> action table
│   └── agents/openai.yaml          # Codex UI metadata (optional)
└── install/
    ├── claude-code.sh   # symlink -> ~/.claude/skills/browser
    ├── codex.sh         # symlink -> ~/.agents/skills/browser
    ├── opencode.sh      # symlink -> ~/.config/opencode/skills/browser   <-- NOT ~/.opencode/
    └── generic.sh       # copies into $AGENT_SKILLS_DIR
```

> **Corrected 2026-08-04 — this was a shipped-bug-in-waiting.** The draft's OpenCode path `~/.opencode/skills/` **is not a path OpenCode reads**. Per `opencode.ai/docs/skills/`, the global locations are `~/.config/opencode/skills/`, `~/.claude/skills/` and `~/.agents/skills/`; project locations are `.opencode/skills/`, `.claude/skills/`, `.agents/skills/` walking up to the git worktree root. `install/opencode.sh` as drafted would have created a symlink that OpenCode silently never loads — the worst class of install bug, because it fails as "the model just never uses the skill".
>
> **The same re-check produces a real simplification the draft missed.** Two of the four target harnesses read `~/.claude/skills/` as a compatibility path: **OpenCode** (confirmed above) and **Cursor** (`cursor.com/docs/context/skills` lists `.claude/skills/`, `.codex/skills/`, `~/.claude/skills/`, `~/.codex/skills/` as legacy-compatible alongside its own `.agents/skills/`, `.cursor/skills/`, `~/.agents/skills/`, `~/.cursor/skills/`). So **one symlink into `~/.claude/skills/browser` covers Claude Code + Cursor + OpenCode**, and a second into `~/.agents/skills/browser` covers Codex + Cursor + OpenCode. The install matrix is **two symlinks, not four**:
> ```
> ~/.claude/skills/browser   -> Claude Code, Cursor (legacy), OpenCode (global)
> ~/.agents/skills/browser   -> Codex, Cursor, OpenCode (global)
> ```
> Cursor frontmatter (verified): `name` + `description` **required**; `paths`, `disable-model-invocation`, `metadata` optional. Cursor also ships `/migrate-to-skills` — confirmed.

`browserctl install-skill --harness auto` should detect installed harnesses and **symlink** (not copy) the one folder into each location, so a `brow` upgrade updates every harness at once. Frontmatter must stay in the standard's intersection — `name`, `description` — with Claude-only fields (`allowed-tools`, `context`, `paths`) tolerated as unknown keys by other harnesses.

**Cursor/Windsurf rules** are a separate, older mechanism (`.cursor/rules/*.mdc` with `alwaysApply`, globs). Cursor itself ships `/migrate-to-skills`, migrating `alwaysApply: false` rules into skills and keeping `alwaysApply: true` ones as rules [29]. Don't ship rules; ship the skill.

**MCP as LCD:** keep it as an optional `browserctl mcp serve` in v2 for harnesses with no skill support. But note the cost: chrome-devtools-mcp needs `--experimentalPageIdRouting` just to let two agents share one server safely [2], and gstack's measurement puts MCP framing at ~1,500–2,000 tokens per call. Skill + CLI wins.

### C2. CLI design for an LLM consumer — concrete rules for `browserctl`

Synthesised from `gh`, `kubectl`, chrome-devtools-mcp's pagination, and gstack's output contract.

1. **Default output is compact text; `--json` is universal.** Every command accepts `--json` and emits a single JSON object on stdout. Like `gh`, allow field projection: `--json ref,role,name`. Never emit JSON to stdout in text mode and never mix logs into stdout (logs → stderr, always).
2. **Hard output budget with an explicit truncation contract.** Cap at a configurable `--max-bytes` (default 32 KB for text, 256 KB for `--json`); when exceeded, truncate and append a machine-readable marker, never silently:
   `... [truncated 812 of 1204 nodes; re-run with --depth 3 or --scope @e17 or --page 2]`
   Refusing to dump 200 KB of DOM is not an error — it is the product.
3. **Pagination on every list.** `--page N --page-size K`, and always print `page 2/7 · 300 of 2041 items`. chrome-devtools-mcp does exactly this for console and network (`pageIdx`, `pageSize`).
4. **Stable ref syntax `@e<N>`, generation-scoped.** Print the generation in the snapshot header: `snapshot g7 · https://… · 214 nodes · 41 interactive`. A stale ref must fail *immediately* and *specifically*:
   ```
   error: stale_ref
     @e12 belongs to generation g6; current generation is g7 (page navigated at 12:04:31)
     next: browserctl snapshot --interactive
   ```
5. **Errors are instructions.** Every error is `{code, message, next}` where `next` is a literal command the model can run. Non-negotiable error codes: `stale_ref`, `ref_not_found`, `not_visible`, `not_stable`, `occluded_by` (include the occluding ref!), `disabled`, `readonly`, `navigation_in_flight`, `dialog_open`, `needs_approval`, `capability_denied`, `frame_detached`, `timeout`.
6. **Actionability failures name the culprit.** `not clickable: @e12 is occluded by @e9 (role=dialog, "Cookie preferences") at point (412,318)` — with a ref, so the model's next move is obvious.
7. **Exit codes carry meaning.** `0` ok, `1` generic failure, `2` usage error, `3` precondition failed (stale ref, not actionable), `4` capability denied, `5` awaiting approval, `124` timeout. Same convention as gstack (0/non-zero) but finer-grained.
8. **Self-describing help.** `browserctl help --json` emits the full command tree with parameter schemas — so a skill can teach itself the surface without you keeping SKILL.md in sync. `browserctl <cmd> --explain` prints the exact CDP sequence it will run (auditability + debuggability, and it makes the "no raw CDP" policy inspectable).
9. **Idempotent, chainable, quiet.** `--quiet` for scripted use; `browserctl batch -` reading newline-delimited commands from stdin (gstack caps batches at 50 with per-command error isolation — copy that).
10. **Never print secrets.** Cookie/storage output is redacted by default (`value: "<redacted:36B>"`); `--reveal` is a `storage` capability and is logged.
11. **Screenshots go to files, paths go to stdout.** Never base64 into the agent's context unless `--stdout-base64` is passed explicitly.

### C3. The `SKILL.md` itself

Budget: the Agent Skills discovery stage loads only `name` + `description`; Codex caps the *listing* at **at most 2 % of the model's context window, or 8,000 characters when the context window is unknown** [28]. Claude Code truncates the combined `description` + `when_to_use` at **1,536 characters** in the listing [27]. So:

> **Corrected 2026-08-04.** Two errors in the draft's budget sentence. (1) The Codex cap is **not** "8,000 characters or 2 % of the context window, whichever is smaller" — it is 2 % of the context window, *falling back* to 8,000 characters when the window size is unknown, and Codex shortens descriptions first when many skills are installed. (2) The claim that Codex "recommends the SKILL.md body stay under **~5,000 tokens**" **does not appear in the documentation**; the doc says only that "when Codex selects a skill, it still reads the full SKILL.md instructions". Drop the 5,000-token figure or attribute it to house style, not to OpenAI. Claude Code's 1,536-character cap on `description` + `when_to_use` **is** confirmed verbatim, as are `disable-model-invocation` (default `false`), `user-invocable` (default `true`), `allowed-tools`, `disallowed-tools` and `context: fork`. Codex discovery paths confirmed, with one addition the draft omitted: `.agents/skills` is read from **the current directory and every parent up to the repo root**, not just cwd and repo root. `agents/openai.yaml` confirmed, with fields `interface` (`display_name`, `short_description`, `icon_small`, `icon_large`, `brand_color`, `default_prompt`), `policy.allow_implicit_invocation` (default true), `dependencies.tools`.

- **`description`**: one dense sentence naming the triggers — *"Drive a real Chromium browser: navigate, inspect the page, click/type with real user gestures, screenshot, record video, and map a site's routes. Use for any request involving a web page, a URL, a browser, a UI bug, a form, or 'check the site'."* Under 1,536 chars including `when_to_use`.
- **Body ≈ 250–450 lines.** Must contain, in this order:
  1. **The loop**: `snapshot → act → snapshot`. State plainly that refs die on navigation.
  2. **snapshot vs screenshot decision rule**: default to `snapshot` (text, cheap, refs); use `screenshot` only for visual questions (layout, colour, "does this look right"), for canvas/WebGL/Flutter content, or when a snapshot has no ref for something visibly present.
  3. **Stale-ref recovery**: verbatim recipe — on `stale_ref` or `ref_not_found`, re-run `browserctl snapshot --interactive` and re-match by role+name, never by ref number.
  4. **Capability modes** and which commands need which; what `needs_approval` means and that the correct response is to *ask the human*, not to retry.
  5. **5–8 worked recipes**: log into a site; fill and submit a form; find why a button does nothing; capture a repro video of a bug; map the routes of an app; diff a page before/after a deploy; inspect a React component's props.
  6. **Human-handoff triggers, explicit**: CAPTCHA, OTP/2FA, OS permission dialogs, Keychain/Touch ID, browser chrome UI, payment, anything in `waiting_for_approval`. The skill should say *"stop and tell the user exactly what to do"* and show the `browserctl job status` command that resumes.
  7. **Pointers to `references/*.md`** with one line each on when to read them. Never inline the full command reference — that's what `browserctl help --json` is for.
- Set `disable-model-invocation: false` (you want auto-activation) and `user-invocable: true` (you want `/browser`). Consider `allowed-tools: Bash(browserctl:*)` on Claude Code so the skill's own commands don't prompt.

---

## What we verified empirically

All on macOS Darwin 25.5.0 with `/Applications/Google Chrome.app`, driven by a from-scratch NUL-delimited CDP-over-pipe client (`--remote-debugging-pipe`, fd 3 in / fd 4 out) written in dependency-free Python. Every Chrome instance used a scratch `--user-data-dir` under `/private/tmp` and was killed afterwards.

| # | What I ran | Raw observation |
|---|---|---|
| 1 | `/json/version` on headless Chrome | `Browser: Chrome/151.0.7922.72`, `Protocol-Version: 1.3`, `V8-Version: 15.1.206.10` |
| 2 | `/json/protocol` | **57 domains**, incl. `WebMCP`, `Extensions`, `PWA`, `SmartCardEmulation`, `BluetoothEmulation`, `CrashReportContext`, `FileSystem`, `Autofill`, `FedCm` |
| 3 | Protocol introspection | `Page.frameStartedNavigating` = **EXPERIMENTAL** event, params `frameId, url, loaderId, navigationType:string`. `Accessibility.getFullAXTree` = **EXPERIMENTAL**, params `depth?`, `frameId?` — **no `includeIframes`**. `Page.addScriptToEvaluateOnNewDocument` = stable, with experimental `worldName?`, `includeCommandLineAPI?`, `runImmediately?` |
| 4 | Probe `https://react.dev/` (React 19, prod) | `__REACT_DEVTOOLS_GLOBAL_HOOK__` → **`"undefined"`**. DOM keys found: `__reactFiber$…`, `__reactProps$…`, `__reactContainer$…`, `__reactEvents$…`, `__reactMarker$…`. Fiber chain component names: `"eA"` (minified). `_debugSource` → `null` for every fiber. Full fiber key list captured (23 keys, no `_debug*`) |
| 5 | Inject hook stub via `Page.addScriptToEvaluateOnNewDocument` (main world) then load `react.dev` | `renderers` → `{1: {version: "19.0.0", bundleType: 0, rendererPackageName: "react-dom", keys: [bundleType, version, rendererPackageName, currentDispatcherRef, findFiberByHostInstance, reconcilerVersion]}}`; `getFiberRoots(1).size === 1`. **`bundleType: 0` = production; only 6 renderer keys ⇒ no `overrideProps`/`overrideHookState`** |
| 6 | Probe `https://vuejs.org/` | `__VUE_DEVTOOLS_GLOBAL_HOOK__` → `"undefined"`. Only DOM key found across 656 elements: **`__vue_app__`**. No `__vueParentComponent` anywhere |
| 7 | Probe `https://dartpad.dev/` (Flutter 3.44.8 / Dart 3.12.2) | Light DOM: `<flt-semantics-placeholder aria-label="Enable accessibility" role="button">` at `{x:-1,y:-1,w:1,h:1}`; `<flutter-view><flt-glass-pane>` with an **open** shadow root containing `flt-scene-host > flt-scene > flt-canvas-container > canvas`, plus `flt-clip > flt-platform-view-slot > slot`; `flt-announcement-host`, `flt-semantics-host` (present but empty) |
| 8 | Flutter trial A — poll 34 s, no action | `flt-semantics` count stayed **0** the whole time. Nothing auto-enables semantics |
| 9 | Flutter trial B — `Accessibility.enable` + `getFullAXTree{depth:-1}` | Returned 144 AX nodes (page chrome only); `flt-semantics` count stayed **0** for the following 25 s. **CDP a11y does not trigger Flutter semantics** |
| 10 | Flutter trial C — `Input.dispatchMouseEvent` at the placeholder's clamped centre (0,0) | `flt-semantics` count stayed **0** — a real click cannot reach a 1×1 element at (-1,-1) |
| 11 | Flutter trial D — scripted `element.click()` | placeholder removed, **41 `flt-semantics` nodes** within 1 s; subsequent `Accessibility.getFullAXTree` → **206 nodes** with labels `"Run"`, `"Show docs"`, `"Create with Gemini"`, `"Dart 3.12.2 • Flutter 3.44.8"` |
| 12 | Flutter trial E — `DOM.setAttributeValue(style → fixed 200,200 60×60)` then real `Input.dispatchMouseEvent` mouseMoved/Pressed/Released at box centre | `DOM.getBoxModel` → `content [200,200,260,200] w=60 h=60`; placeholder removed, **41 `flt-semantics` nodes** within 1 s. **A real browser-level gesture works after repositioning** |
| 13 | Read `chrome-devtools-mcp` sources | `TextSnapshot.ts` uid formula and `${loaderId}_${backendNodeId}` reuse key; `WaitForHelper.ts` constants 3000/100/100/3000 ms and the `Page.frameStartedNavigating` navigationType filter |

**Not verified (read only):** Puppeteer's `USKeyboardLayout` entry count (230) and license header — fetched the file, did not compile against it. gstack's "0 context tokens vs 30–40k for MCP" is the vendor's own claim. WebVoyager/WebArena scores are third-party leaderboards, not reproduced.

---

## Limits and impossibilities — bluntly

1. **"Adapters are injected JS in an isolated world" is false for React, Vue and Angular.** Framework hooks are main-world globals. An isolated world has a different `window` and will never see or be seen by them. Adapter injection is a main-world page mutation. Redesign the capability model around this or drop framework adapters entirely.
2. **Component names and source locations are unavailable in production for every framework.** React: minified `type.name`, `_debugSource` absent. Vue: `__file` is dev-only. Angular: `window.ng` is stripped by `enableProdMode()`. Svelte 5: `__svelte_meta` is dev-only. If the target app is a deployed production build — which is the common case for an automation harness — "framework components in the node model" degrades to *boundaries and shapes without names*. Design the output schema to say so; don't ship `"eA"` as a component name.
3. **React Server Components are not introspectable in production.** They rely on `fiber._debugInfo`, which does not exist in production builds. Best case is heuristics over the Next.js flight payload, which is `inferred`-grade evidence at best.
4. **Flutter content that isn't in the semantics tree does not exist to the DOM.** Custom painters, decorative widgets, un-`Semantics`-wrapped gesture regions: pixels in a canvas. No CDP call retrieves them. Vision + coordinates is the only fallback, and it is not a fallback your fat-tree model can absorb.
5. **The Dart VM service is not available in release Flutter Web builds.** dart2js/dart2wasm output has no VM service. Do not plan a channel around it.
6. **Enabling Flutter semantics mutates the page** (a style attribute, plus Flutter's own DOM changes and its ~30% frame-time cost). It is not an observe-only operation and cannot be offered under the `observe` capability.
7. **Svelte 5 runes have no runtime component-instance API.** There is nothing equivalent to a fiber or a Vue instance to walk. Any Svelte 5 "props/state" claim would be fabrication.
8. **`Accessibility.getFullAXTree` does not cross frames — including same-origin ones.** Puppeteer's `includeIframes` is Puppeteer's stitching, not CDP's. **Corrected 2026-08-04:** the draft called an OOPIF-correct implementation "genuinely hard"; measured, the *stitch* is ~5 call types and an afternoon (algorithm in §A1). Two things are true instead: (a) the blast radius is **larger** than the draft said — the top-frame AX tree omits *every* iframe's content, not just cross-origin ones, so any page with an embedded checkout, auth widget or docs iframe silently loses that content from snapshots **and from state signatures**; (b) OOPIFs do not appear in `Page.getFrameTree` at all and are reachable only through `Target.setAutoAttach`. What is genuinely hard is **frame lifecycle bookkeeping** under navigation/detach races, not the splice.
9. **You are reimplementing ~14 Playwright behaviours.** Rows 1–12 of §A2 are not optional polish; they are the difference between "clicks work" and "clicks work on real sites". Budget for them explicitly, and be honest that v1 will lose to Playwright-based tools on flaky pages until they land.
10. **Two projects already occupy this niche** (`gsd-browser`: Rust + daemon + refs + 90 commands; `gstack browse`: daemon + refs + CDP allowlist + skills runtime). Neither has the fat tree, real-input fidelity, or state-aware site graph — but "there is no prior art" is not a claim you can make. Check `gsd-browser`'s licence (MIT/Apache-2.0) before reading its source if you want to keep clean-room provenance.
11. **`Page.frameStartedNavigating` and `Accessibility.getFullAXTree` are both EXPERIMENTAL** in Chrome 151. Your auto-wait and your snapshot both sit on experimental CDP. Pin a Chrome version range, snapshot `browser_protocol.json` per supported Chrome, and fail loudly on protocol drift.

---

## Open questions for the owner

1. **Is main-world adapter injection acceptable?** If not, framework adapters are limited to what's readable from an isolated world through `DOM`/`Runtime.callFunctionOn` on DOM nodes — which for React means the `__reactFiber$` DOM keys only (still useful: tree shape + props), and for Angular/Svelte means essentially nothing.
2. **Do you want dev-build-only capabilities at all?** A "run the target app in dev mode for full introspection" story is much more valuable than a degraded production story, and it's a legitimate positioning (`brow` as a *development* harness, not a scraping tool).
3. **`gsd-browser` is a Rust CDP daemon with your exact shape.** Do you want to differentiate deliberately (fat tree + site graph + video/action-log sync) or is this convergent evolution you'd rather not know about? *(**Corrected 2026-08-04:** the draft added "they claim ~100–200 ms per command after warm-up". **`gsd-browser` makes no latency claim anywhere in its README** — it only says the daemon makes repeated commands fast. The nearest real number in the prior art is gstack's unrelated *"the second time you ask Claude to scrape a page, it runs in ~200ms"*, which is about skill replay, not per-command CDP latency. The figure was misattributed; do not use it as a benchmark target. If latency is the axis you want to compete on, measure `gsd-browser` yourself first.)*
4. **Which benchmark do you commit to?** My recommendation: skip WebVoyager (saturated at 97–98%), report Web Bench write-task success plus a *route-coverage* metric nobody else publishes.
5. **Does `browserctl` ship an MCP shim in v1?** I recommend no, but that closes the door on Cline, Zed, and any harness without Agent Skills support.
6. **Screenshot output policy:** files-only by default, or allow base64 into agent context? Files-only is correct for token cost but means the agent needs a Read-image tool, which not every harness has.
7. **`@c<N>` (cursor-interactive) second ref namespace, or unify into `@e<N>`?** gstack split them; your fat tree makes a single namespace possible, but then the snapshot must carry an `evidence` field (`aria` vs `listener` vs `cursor`) so the model knows how confident to be.
8. **Clean-room policy on Apache-2.0 code.** Puppeteer's `USKeyboardLayout` is the obvious candidate to transliterate with attribution. Confirm you're comfortable with an Apache-2.0 `NOTICE` in an otherwise MIT/Apache-dual repo.

---

## Sources

1. https://github.com/ChromeDevTools/chrome-devtools-mcp
2. https://raw.githubusercontent.com/ChromeDevTools/chrome-devtools-mcp/main/README.md
3. https://api.github.com/repos/ChromeDevTools/chrome-devtools-mcp/contents/src
4. https://raw.githubusercontent.com/ChromeDevTools/chrome-devtools-mcp/main/docs/tool-reference.md
5. https://raw.githubusercontent.com/ChromeDevTools/chrome-devtools-mcp/main/src/TextSnapshot.ts
6. https://raw.githubusercontent.com/ChromeDevTools/chrome-devtools-mcp/main/src/WaitForHelper.ts
7. https://playwright.dev/docs/actionability
8. https://news.ycombinator.com/item?id=44962869
9. https://playwright.dev/docs/api/class-browsercontext
10. https://www.browserbase.com/blog/stagehand-playwright-evolution-browser-automation
11. https://raw.githubusercontent.com/puppeteer/puppeteer/main/packages/puppeteer-core/src/common/USKeyboardLayout.ts
12. https://screenshotone.com/blog/capture-beyond-viewport-in-puppeteer-and-chrome-devtools-protocol/
13. https://dev.to/stevengonsalvez/browser-tools-for-ai-agents-part-2-the-framework-wars-browser-use-stagehand-skyvern-4gn
14. https://github.com/steel-dev/steel-browser
15. https://www.dualmedia.fr/en/ai-browsers-2026/
16. https://leaderboard.steel.dev/leaderboards/webvoyager/
17. https://www.skyvern.com/blog/web-bench-a-new-way-to-compare-ai-browser-agents/
18. https://github.com/garrytan/gstack/blob/main/BROWSER.md (raw: https://raw.githubusercontent.com/garrytan/gstack/main/BROWSER.md)
19. https://github.com/open-gsd/gsd-browser (mirror: https://github.com/gsd-build/gsd-browser)
20. https://github.com/facebook/react/pull/30684
21. https://mokkapps.de/vue-tips/force-enable-vue-devtools-in-production-build
22. https://github.com/sveltejs/svelte/discussions/17136
23. https://github.com/sveltejs/svelte/issues/15422
24. https://v18.angular.dev/api/core/globals/getComponent/ and https://angular.love/debugging-techniques-global-utils/
25. https://docs.flutter.dev/ui/accessibility/web-accessibility
26. https://agentskills.io/
27. https://code.claude.com/docs/en/skills
28. https://learn.chatgpt.com/docs/build-skills (was https://developers.openai.com/codex/skills)
29. https://cursor.com/docs/context/skills
30. https://opencode.ai/docs/skills/ (via agentskills.io client showcase) and https://github.com/joshuadavidthomas/opencode-agent-skills
31. https://crates.io/api/v1/crates/chromiumoxide (0.9.1, updated 2026-02-25)
32. https://github.com/spider-rs/chromey (chromey v2.39.0, fork of chromiumoxide, ex-`spider_chrome`)

---

## Verification pass — 2026-08-04 (adversarial re-check)

An independent pass re-tested this document's load-bearing claims against primary sources and live experiments. Environment: macOS Darwin 25.5.0, **Google Chrome 151.0.7922.72** (V8 15.1.206.10, Protocol-Version 1.3, **57 domains, 38 of them EXPERIMENTAL**), launched `--headless=new --remote-debugging-port=41337 --user-data-dir=/private/tmp/brow-verify/udd1`, driven by a from-scratch RFC6455 WebSocket CDP client (no Playwright/Puppeteer/`websockets`). All Chrome and fixture-server processes killed and the scratch profile deleted afterwards.

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| 1 | chrome-devtools-mcp uses Puppeteer | **CONFIRMED** | README verbatim: *"Uses puppeteer to automate actions in Chrome and automatically wait for action results."* |
| 2 | "64 tools in 10 categories" | **REFUTED → 52** | `docs/tool-reference.md` and README both give 52; the document's own table summed to 52. Fixed in §A1. |
| 3 | ~48.5k stars | **CONFIRMED** | GitHub API: 48,522 |
| 4 | `uid` reuse key `${loaderId}_${backendNodeId}` | **CONFIRMED** | `TextSnapshot.ts` quoted verbatim, incl. `accessibility.snapshot({includeIframes:true, interestingOnly:!verbose})` |
| 5 | `WaitForHelper` constants 3000/100/100/3000, `frameStartedNavigating` filter | **CONFIRMED** | source; filtered types `historySameDocument`, `historyDifferentDocument`, `sameDocument` |
| 6 | `Accessibility.getFullAXTree` EXPERIMENTAL, params `depth?`/`frameId?`, no `includeIframes` | **CONFIRMED** | live `/json/protocol`; domain also EXPERIMENTAL |
| 7 | `Page.frameStartedNavigating` EXPERIMENTAL | **CONFIRMED** | live `/json/protocol`, params `frameId, url, loaderId, navigationType` |
| 8 | AX frame stitching is "genuinely hard" | **PARTIAL — overstated, and mis-aimed** | Built it. Stitch = ~5 call types (§A1). But `getFullAXTree{}` omits **same-origin** frames too (11 nodes, no `SameOriginChildButton`), and OOPIFs are absent from `Page.getFrameTree`. Hard part is lifecycle, not splice. |
| 9 | Isolated world cannot see framework globals | **CONFIRMED** | stub injected with `worldName:"browiso"` → main world reads `{stub:false, hook:"undefined"}` |
| 10 | React stub → React 19 registers, `bundleType:0`, 6 renderer keys | **CONFIRMED** | reproduced on `react.dev`; `getFiberRoots(1).size === 1` |
| 11 | Main-world hook stub is safe enough to ship | **REFUTED as stated** | Trials C–F in §B0: page scripts silently displace our stub; freezing silently breaks the real DevTools; a pre-frozen property makes our install fail with `installError === null`. Needs sentinel read-back + `displaced`/`blocked` states. |
| 12 | (new) `addScriptToEvaluateOnNewDocument` bypasses page CSP | **CONFIRMED (new finding)** | our stub ran on a `script-src 'self'` page whose own inline script was blocked |
| 13 | Vue `__vueParentComponent` is dev-only | **CONFIRMED at source** | `vuejs/core` `runtime-core/src/renderer.ts`: `if (__DEV__ \|\| __FEATURE_PROD_DEVTOOLS__) { def(el,'__vnode',…); def(el,'__vueParentComponent',…) }` |
| 14 | Flutter reposition-then-real-click is the activation technique | **PARTIAL — a simpler path exists; mobile differs** | `semantics_helper.dart`: desktop placeholder is exactly `left:-1px;top:-1px;width:1px;height:1px` and activates on `keyup`/`keydown` too ⇒ focus + `Input.dispatchKeyEvent` needs no DOM mutation. Mobile enabler covers the whole viewport, consumes events for 300 ms, needs a tap within 1 px of centre, gives up after 20 attempts. |
| 15 | Closed shadow roots reachable via CDP, not via page JS | **CONFIRMED** | page JS `shadowRoot` → `false`; `DOM.getDocument{pierce:true}`, `getFullAXTree`, `DOMSnapshot.captureSnapshot` all see it (so does `pierce:false` on Chrome 151) |
| 16 | Agent Skills is a cross-vendor open standard; adopters as listed | **CONFIRMED** | agentskills.io: Anthropic-originated open standard, `name`+`description` minimum, 3-stage progressive disclosure, ~46 clients incl. every harness named here |
| 17 | Claude Code skill paths + 1,536-char listing cap | **CONFIRMED** | `code.claude.com/docs/en/skills`, incl. `disable-model-invocation`, `user-invocable`, `allowed-tools`, `disallowed-tools`, `context: fork`, nested `apps/web:deploy` naming, live file-watching |
| 18 | Codex cap "8,000 chars or 2 % of context, whichever is smaller"; body ≤ ~5,000 tokens | **PARTIAL / REFUTED** | Real: 2 % of context window, *or* 8,000 chars when unknown. The ~5,000-token body recommendation is **not in the docs**. |
| 19 | Cursor reads `.claude/skills/` and `.codex/skills/` | **CONFIRMED** | `cursor.com/docs/context/skills`; `/migrate-to-skills` confirmed |
| 20 | OpenCode reads `~/.opencode/skills/` | **REFUTED** | Real: `~/.config/opencode/skills/`, `~/.claude/skills/`, `~/.agents/skills/`; project `.opencode/`, `.claude/`, `.agents/`. Draft's install script would have been a silent no-op. |
| 21 | gstack's CLI-vs-MCP token numbers | **CONFIRMED as a quote, UNVERIFIED as a fact** | `BROWSER.md` verbatim; still the vendor's own marketing. Also confirmed: `CDP_ALLOWLIST`, 1 MB stdout cap, `@e<N>`/`@c<N>`, and that it is built on Playwright `ariaSnapshot()` |
| 22 | gsd-browser: Rust, chromiumoxide, daemon, 90+ commands, dual-licensed | **PARTIAL** | Rust ✓, `chromiumoxide = "0.9"` ✓ (from `Cargo.toml`, not the README), daemon ✓ over **loopback TCP** (`GSD_BROWSER_DAEMON_PORT`, default 9333), **92** commands ✓, dual MIT/Apache ✓. **`gsd-build` is canonical (252★), `open-gsd` is the mirror (37★) — draft had it backwards.** No `chromey`/stealth feature flags found. **No latency claim exists** — the "~100–200 ms" figure was misattributed. |
| 23 | Puppeteer `USKeyboardLayout` has 230 entries | **REFUTED → ~253** | live fetch, 18,746 bytes; 253 top-level entries; Apache-2.0 header verbatim |
| 24 | ChatGPT Atlas "retired 9 July 2026" | **PARTIAL** | Announced 2026-07-09; **functionality ends 2026-08-09**. Replace the `dualmedia.fr` citation with OpenAI's help-centre article. |

**Not re-verified (inherited on trust):** WebVoyager/WebArena/Web Bench leaderboard figures; browser-use and Skyvern architecture claims; Playwright's actionability semantics (read from docs, not exercised); the `--remote-debugging-pipe` fd 3/4 result (this pass used a TCP port, which is *not* the recommended transport — the pipe claim remains from the prior pass only).

**New gaps surfaced by this pass:** (a) the AX tree's frame-blindness affects **state signatures**, not just snapshots — see the companion correction in `90-crawler-and-site-graph.md` §2.3; (b) `hookSource` needed two more states; (c) the CSP-bypass privilege was undocumented; (d) the two-symlink install simplification (`~/.claude/skills` + `~/.agents/skills` covers all four named harnesses).
