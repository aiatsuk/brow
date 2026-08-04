# The Unified Page Tree: DOM + Shadow + Iframes + AX + Layout + CSS + Listeners + JS Ownership

> **Bottom line.** The unified tree is buildable and CDP gives you more than you'd expect — `DOM.getDocument{pierce:true}` and `DOMSnapshot.captureSnapshot` **both see closed shadow roots** (verified on Chrome 151), `DOMDebugger.getEventListeners{depth:-1,pierce:true}` returns a **whole-page listener map in a single call** (also crossing closed shadow roots and same-process iframes), CDP hands you **selector specificity precomputed**, and the cross-frame global-coordinate problem has an empirically-verified affine solution — *exact for translate/scale/rotate/skew, and provably wrong under `perspective`, where the guard originally proposed in §5 fails to fire (corrected 2026-08-04)*. One further correction that changes a design: **framework expandos (`__reactFiber$*`, `__vue_app__`) are NOT visible from an isolated world** — verified — so the adapter layer must run in the main world (§8). Three things will hurt. (1) **OOPIFs are hard walls**: `DOMSnapshot.captureSnapshot` returns *nothing* for cross-origin frames, `DOM.getDocument{pierce:true}` returns the `<iframe>` node with a `frameId` but **no `contentDocument`**, and `Accessibility.getFullAXTree` does not cross *any* iframe boundary — not even same-origin. You must attach a session per frame and stitch, and every geometry number crossing a frame boundary must go through the affine map below. (2) **`backendNodeId` does not die on same-process navigation** — after navigating away, `DOM.describeNode`, `DOM.getBoxModel` and `DOM.scrollIntoViewIfNeeded` still *succeed* on the old document's nodes and return plausible-looking stale geometry. This is a silent wrong-action bug generator; refs MUST carry a document generation and be validated, not trusted. (3) **Token economics are brutal and the AX tree is the worst offender, not the best**: raw `getFullAXTree` on a Wikipedia article was **9.3 MB (~2.5M tokens)**, larger than `DOMSnapshot` (4.0 MB) and `getDocument{pierce}` (3.9 MB); one fat node on github.com cost **~309K tokens** (985 KB of `CSS.getMatchedStylesForNode`, of which 1.06 MB was the `inherited` array). The output design must be a compact line-oriented interactive view (~6K tokens for github.com, ~40K for Wikipedia — so viewport-scoping and paging are mandatory, not optional), with fat nodes emitted one at a time, on demand, and aggressively pruned.

All protocol facts below were read out of the **actual `/json/protocol` of the locally installed Chrome 151.0.7922.72** (1.6 MB JSON, dumped to disk) or observed live over a `--remote-debugging-pipe` session. Statements are tagged **[CONFIRMED]** (read the protocol definition or observed it running), **[LIKELY]** (secondary sources agree), **[UNVERIFIED]** (reasoning).

---

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| Ref `@n<backendNodeId>` keyed on a `(target_id, frame_id, doc_generation, backend_node_id)` tuple, plus a re-resolution fallback | `backendNodeId` survives reparent, remove+re-add and `innerHTML` wipes of siblings; `nodeId` does not. But `backendNodeId` **outlives navigation in the same renderer process**, so a generation counter is the only thing that makes refs safe | Bare `nodeId` (dies constantly); bare `objectId` (dies with the execution context); CSS-path-only refs (ambiguous, breaks on re-render) | **confirmed** |
| `DOMSnapshot.captureSnapshot` is the bulk path for DOM+layout+paint order+computed styles **per process**, `DOM.getDocument{pierce:true}` is the structural/identity path | 211 ms for 19.7K nodes on Wikipedia; one call replaces ~16K `getComputedStyleForNode` round-trips | Per-node CSS/DOM walking (thousands of RTTs); JS-injected `document.querySelectorAll` walker (cannot see closed shadow roots) | **confirmed** |
| One CDP session per frame **target**, one snapshot per session, stitched by the daemon | `captureSnapshot` and `getFullAXTree` are renderer-scoped and simply omit OOPIF content | Hoping `pierce:true` crosses OOPIFs (it does not) | **confirmed** |
| Cross-frame coordinates via an **affine basis derived from the owner iframe's content quad ÷ the child's `clientWidth/Height`** | Handles CSS transforms on the iframe; the naive "add the quad origin" formula misses a scaled iframe entirely (verified: naive click missed, affine click hit). **Re-verified 2026-08-04 for `rotate(25deg) scale(0.6)`: predicted click landed within 1 px.** | Naive origin addition; `DOM.getContentQuads` in the child (child-local only) | **confirmed for affine; breaks under `perspective` — and the §5 guard as originally written does NOT catch it (see §5)** |
| Framework adapters read `__reactFiber$*`/`__vue_app__` from the **main** world, not the isolated world | **Corrected 2026-08-04:** expandos live on the per-world V8 wrapper, so the isolated world sees `Object.keys(node) === []`. Main world is the only option, and it is page-observable/tamperable (§8) | Isolated-world adapters (was the original plan) | **REFUTED → redesigned** |
| Filter `shadowRootType == "user-agent"` out of the default tree | One `<input type=date>` + one `<video controls>` produced **21 UA shadow roots**; `DOM.getDocument` has no flag to suppress them | Shipping UA internals to the agent (node-count explosion) | **confirmed** |
| Present listeners as a **page-level delegation map**, not a per-node property | `isClickable` in the snapshot marked `#document` clickable because of a React-style root listener, and marked plain `<button>`s **not** clickable | Per-node "has click handler" booleans (actively misleading) | **confirmed** |
| Default agent output = line-oriented compact interactive view, viewport-scoped, paged; fat node only on `inspect node @n<id>` | Compact lines: 6.0K tokens for github.com vs 168K–240K raw; one fat node on github.com = 309K tokens | Fat JSON nodes for the whole tree (token death); AX-only aria snapshot (loses CSS/layout/listeners/paint order) | **confirmed** |
| Strip `name.sources`, `chromeRole`, `ignoredReasons` from AX before storing | ~50% of `getFullAXTree` bytes on github.com (888 KB → 440 KB) | Storing raw AX | **confirmed** |
| Never expose `CSS.getMatchedStylesForNode` raw; compute and emit only the winning declaration + the losing rules that touched the same property | `inherited` was 1,062,842 of ~1,080,000 bytes for a single `<a>` on github.com | Passing through the CDP result | **confirmed** |
| Injected helpers live in a named world created by `Page.addScriptToEvaluateOnNewDocument{worldName, runImmediately:true}` | Verified: world is auto-recreated per frame and survives navigation; main world cannot see the helper and vice versa | `Page.createIsolatedWorld` per navigation (races the first script on the page) | **confirmed** |

---

## 1. Node identity: what is stable across what

Four identifiers, none of which does the job alone.

| Identifier | Scope | Dies when | Cheap to get? |
|---|---|---|---|
| `DOM.NodeId` | Per **session**, allocated by the DevTools DOM agent on push | Node detached from the tree (even if re-attached), `DOM.documentUpdated`, DOM agent disable | Free — comes back in `getDocument` |
| `DOM.BackendNodeId` | Per **renderer process** (Blink `DOMNodeIds`) | Node is GC'd *and* nothing holds it; **NOT** on same-process navigation | Free — in `getDocument`, `DOMSnapshot`, AX nodes, listeners |
| `Runtime.RemoteObjectId` | Per **execution context** + object group | Context destroyed (any navigation), `Runtime.releaseObjectGroup` | Costs a `DOM.resolveNode` |
| `Accessibility.AXNodeId` | Per AX tree snapshot | Any AX tree rebuild | Comes with `getFullAXTree` |

### Observed lifetime behaviour (Chrome 151, live)

```
nodeId 15 / backendNodeId 22  (button#plain)
  insertBefore(<hr>) at body start  -> describeNode(nodeId 15)      OK          [CONFIRMED]
  scroller.appendChild(#plain)      -> describeNode(nodeId 15)      "Could not find node with given id"
  remove() + body.appendChild()     -> describeNode(nodeId 15)      "Could not find node with given id"
                                     -> pushNodesByBackendIdsToFrontend([22]) -> nodeIds [75]   OK
  scroller.innerHTML = '<b>gone</b>' -> describeNode(backendNodeId 22)          OK (nodeId 75)
  Page.navigate -> /frame_same.html (SAME origin, same process)
                                     -> document.URL == ".../frame_same.html"   (navigation really happened)
                                     -> describeNode(backendNodeId 22)          OK, nodeId 0   <-- TRAP
                                     -> getBoxModel(backendNodeId 22)           OK, returns OLD layout box
                                     -> scrollIntoViewIfNeeded(backendNodeId 22) OK
                                     -> resolveNode(backendNodeId 22)           ERROR (context gone)
  Page.navigate -> http://127.0.0.1:8782/... (CROSS origin, new process)
                                     -> describeNode(backendNodeId 22)          "No node found for given backend id"
```

`DOM.documentUpdated` is documented as *"Fired when `Document` has been totally updated. **Node ids are no longer valid.**"* — note it says **node ids**, and says nothing about backend node ids. That is exactly what we measured. [CONFIRMED — protocol JSON + live]

> **Verified 2026-08-04 — reproduced exactly on a different fixture; this is deterministic, not a GC/timing artifact.** `button#plain` → `backendNodeId 164`, box `content[220, 161.328125, 258.53125, …]`. `Page.navigate` to a same-origin page (`document.URL` confirmed changed to `/nav_target.html`, 4 `DOM.documentUpdated` events observed), then on the **destroyed** document's backend id:
> ```
> DOM.describeNode{backendNodeId:164}        -> OK  {nodeId:0, nodeName:"BUTTON", attributes:["id","plain"]}
> DOM.getBoxModel{backendNodeId:164}         -> OK  content[220, 161.328125, 258.53125, …]  <-- byte-identical to pre-nav
> DOM.scrollIntoViewIfNeeded{backendNodeId}  -> OK  {}
> DOM.resolveNode{backendNodeId:164}         -> ERROR -32000 "Node with given id does not belong to the document"
> ```
> The stale geometry is **the old layout box returned verbatim**, with no error and no warning. The generation-counter requirement in the resolution algorithm below is therefore not defensive polish — without it the harness will click coordinates from the previous page. Note the exact error string differs from the one originally recorded (`-32000 Node with given id does not belong to the document`, not "context gone"); match on the code, not the message.

Also observed: calling `DOM.getDocument` a second time **re-issues a different root `nodeId`** (1 → 110 → 145 across calls in one document) and invalidates the previously issued node ids. Treat `DOM.getDocument` as a destructive operation on your nodeId cache.

### The durable ref

```rust
/// Wire form: "@n42"  (42 == backend_node_id, chosen because it is what
/// DOMSnapshot / AX / getEventListeners all speak natively)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeRef {
    pub target_id: TargetId,          // which CDP target/session owns this node
    pub frame_id:  FrameId,           // Page.FrameId the node's document belongs to
    pub doc_gen:   u64,               // monotonic per (target, frame); ++ on documentUpdated
    pub backend:   BackendNodeId,     // stable within the renderer process
    pub fallback:  Fallback,          // used only to RE-BIND after doc_gen changes
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fallback {
    pub css_path:  CompactString,     // "body > main > form#login > input[name=email]"
    pub tag:       CompactString,
    pub ax_role:   Option<CompactString>,
    pub ax_name:   Option<CompactString>,
    pub text_hash: u64,               // hash of trimmed textContent, first 128 chars
    pub doc_order: u32,               // index in the pre-order snapshot walk
}
```

**Resolution algorithm** (`browserd::inspection::resolve`):

1. If `ref.doc_gen != current_gen(target, frame)` → **do not touch `backend`**. Go straight to step 4 (re-bind). This is the whole point: the stale-`backendNodeId` trap above means a `getBoxModel` on a stale ref returns a *plausible wrong answer*, not an error.
2. `DOM.pushNodesByBackendIdsToFrontend{backendNodeIds:[backend]}` → `nodeIds`. A returned `0` means "gone".
3. Cheap liveness assertion: `DOM.describeNode{backendNodeId}` and compare `nodeName` + a stable attribute (`id`, `name`, `data-testid`) against the fallback. Chrome will happily describe a node from a destroyed document, so this comparison is what catches it.
4. Re-bind: `DOM.querySelector{nodeId: doc_root, selector: fallback.css_path}`; if 0 hits or ambiguity, try `Accessibility.queryAXTree{backendNodeId: root, role, accessibleName}` — **note `accessibleName` is an exact match, not substring** (querying `"Star"` against github.com's "Star this repository" returned 0 nodes) [CONFIRMED]. If still ambiguous, fall back to nearest `doc_order` among candidates and mark the ref `rebound: true` in the response so the agent knows.
5. If re-binding fails, return `E_REF_STALE` with the last-known role/name/text so the agent can re-plan rather than silently no-op.

Generation counters are bumped on `DOM.documentUpdated`, `Page.frameNavigated` (for that frame), and `Target.targetDestroyed`/`attachedToTarget` for that frame's target.

`DOM.describeNode` takes `nodeId | backendNodeId | objectId` plus `depth` and `pierce`, so it is the cheapest single-node probe. `DOM.resolveNode{nodeId|backendNodeId, objectGroup, executionContextId}` is the only bridge into JS; passing `executionContextId` of your isolated world works [CONFIRMED — returned `HTMLButtonElement` in world 5]. **Important side effect:** retaining `RemoteObject`s in an object group keeps the underlying nodes alive and therefore keeps stale `backendNodeId`s resolvable. Always `Runtime.releaseObjectGroup` after an inspection batch.

---

## 2. The bulk-snapshot path: `DOMSnapshot.captureSnapshot`

Domain `DOMSnapshot` is **EXPERIMENTAL** at the domain level; `captureSnapshot` itself is not separately flagged. [CONFIRMED — protocol JSON]

```jsonc
// request
{"method":"DOMSnapshot.captureSnapshot","params":{
  "computedStyles": ["display","visibility","opacity","position","z-index",
                     "pointer-events","cursor","overflow-x","overflow-y",
                     "background-color","color","font-size"],   // required, whitelist
  "includePaintOrder": true,                                    // -> layout.paintOrders
  "includeDOMRects": true,                                      // -> offsetRects/scrollRects/clientRects
  "includeBlendedBackgroundColors": true,                       // EXPERIMENTAL
  "includeTextColorOpacities": true                             // EXPERIMENTAL
}}
// response: { documents: DocumentSnapshot[], strings: string[] }
```

Output is a **struct-of-arrays with a shared string table**, which is why it is fast and why you must not hand it to a model raw.

```
DocumentSnapshot
  documentURL,title,baseURL,contentLanguage,encodingName,publicId,systemId,frameId : StringIndex
  scrollOffsetX/Y, contentWidth/Height : number
  nodes : NodeTreeSnapshot        // parallel arrays, index == "node index"
    parentIndex[]         i32   (-1 for root)
    nodeType[]            i32
    nodeName[], nodeValue[]  StringIndex
    backendNodeId[]       BackendNodeId          <-- the join key to everything else
    attributes[]          ArrayOfStrings (flat name,value,name,value)
    shadowRootType        RareStringData  {index:[nodeIdx...], value:[StringIndex...]}
    pseudoType, pseudoIdentifier, textValue, inputValue, currentSourceURL, originURL : RareStringData
    inputChecked, optionSelected, isClickable : RareBooleanData {index:[...]}
    contentDocumentIndex  RareIntegerData        <-- iframe node -> documents[] index
  layout : LayoutTreeSnapshot     // ONLY nodes that have a layout object
    nodeIndex[]           i32   -> index into nodes
    styles[]              [StringIndex] in the same order as `computedStyles`
    bounds[]              [x,y,w,h]  DOCUMENT-space, transformed
    text[]                StringIndex
    stackingContexts      RareBooleanData (indices into the LAYOUT array)
    paintOrders[]         i32   global paint order; co-painted nodes share an index
    offsetRects[], scrollRects[], clientRects[]   [x,y,w,h]
    blendedBackgroundColors[] StringIndex  (EXP), textColorOpacities[] f64 (EXP)
  textBoxes : TextBoxSnapshot     // post-layout inline boxes
    layoutIndex[], bounds[], start[], length[]   (UTF-16 code-unit offsets!)
```

Measured on the local fixture: `offsetRects` = `offsetLeft/Top/Width/Height`, `clientRects` = `clientLeft/Top/Width/Height` (the same-origin `<iframe>` with `border:2px` gave `clientRect [2,2,300,140]`), `scrollRects` = `scrollLeft/Top/Width/Height`. `bounds` is the **transformed absolute box** — a button inside `transform: translate(30px,15px) scale(0.5)` reported width `58.65` for an untransformed `117`. [CONFIRMED]

### Coordinate space — the trap

**`DOMSnapshot.layout.bounds` is document space. `DOM.getBoxModel` / `DOM.getContentQuads` / `DOM.getNodeForLocation` / `Input.dispatch*Event` are viewport space.** Verified by scrolling to y=900:

| Element | snapshot `bounds.y` @scroll 0 | @scroll 900 | `getBoxModel` @scroll 900 |
|---|---|---|---|
| normal-flow `#b` | 4382.17 | 4382.17 | **3482.17** (= 4382.17 − 900) |
| `position:fixed` `#f` | 5 | **905** | **5** |
| `position:sticky` `#s` | 8 | **900** | — |

`documents[i].scrollOffsetY` gives you the value to subtract. Mixing the two spaces is the single most likely geometry bug in this project. [CONFIRMED]

**Device scale factor does not matter here.** With `Emulation.setDeviceMetricsOverride{deviceScaleFactor:3}` both snapshot bounds and `getBoxModel` were byte-identical to DPR 1 — CDP layout numbers are CSS pixels throughout. DPR only enters at `Page.captureScreenshot`. [CONFIRMED]

### What it does and does not cover

- **Shadow DOM: flattened in, including CLOSED roots.** The protocol says "Shadow DOM in the returned DOM tree is flattened." Observed `shadowRootType` rare-data entries with values `"open"` **and `"closed"`**, and text nodes `CLOSED-SHADOW-BTN` / `NESTED-CLOSED-BTN` (a closed root nested inside another closed root) present in `nodeValue`. [CONFIRMED]
- **Same-process iframes: in.** Two `DocumentSnapshot`s were returned, joined by `contentDocumentIndex` on the `<iframe>` node.
- **OOPIFs: completely absent.** The cross-origin frame produced no `DocumentSnapshot` and no `contentDocumentIndex` entry. Confirmed independently by [playwright#26856](https://github.com/microsoft/playwright/issues/26856). You must run `captureSnapshot` in the OOPIF's own session — where it returns exactly one document, in **that frame's** coordinate space starting at (0,0). [CONFIRMED]
- **`isClickable` is not "is interactive".** On the fixture it flagged exactly `#document`, `button#plain` and `<a>` — it missed three plain `<button>` elements that had no listeners, and it flagged `#document` because of a single delegated root listener. Do not build `--interactive` on it. [CONFIRMED]
- **`originURL`** ("the url of the script that generates this node") was empty in all runs; it appears to require `DOM.setNodeStackTracesEnabled` or a debug build. Use `DOM.getNodeStackTraces` instead (§8). [CONFIRMED empty / cause UNVERIFIED]

---

## 3. Accessibility

`Accessibility` is an **EXPERIMENTAL** domain; every command in it (`getPartialAXTree`, `getFullAXTree`, `getRootAXNode`, `getAXNodeAndAncestors`, `getChildAXNodes`, `queryAXTree`) is flagged experimental. [CONFIRMED]

```
getFullAXTree(depth?, frameId?)                        -> nodes: AXNode[]
getRootAXNode(frameId?)                                -> node
getChildAXNodes(id, frameId?)                          -> nodes
getAXNodeAndAncestors(nodeId?|backendNodeId?|objectId?) -> nodes
queryAXTree(nodeId?|backendNodeId?|objectId?, accessibleName?, role?) -> nodes
```

`AXNode` carries `backendDOMNodeId` — that is your join key into the unified tree — and `frameId`.

**What AX gets right:** roles and computed accessible names are the single best cheap semantic signal, and `queryAXTree{role:"button"}` returned 17 nodes / 15.5 KB on github.com, which is a genuinely cheap targeted lookup. Progressive walking is cheap too: `getRootAXNode` = 979 bytes, one level of children = 6.3 KB. [CONFIRMED]

**What AX gets wrong for this project:**

1. **It does not cross iframe boundaries at all — not even same-origin.** On the fixture, `getFullAXTree` in the main session returned 60 nodes, all with a single `frameId`, and `SAME-FRAME-BTN` (a button in a *same-origin* `<iframe>`) was **absent**. You must call it per frame (`frameId` param, or per OOPIF session). [CONFIRMED]

  > **Verified 2026-08-04 — reproduced independently, with the useful addendum the original run did not record.** Fresh fixture, Chrome 151: `Accessibility.getFullAXTree{}` on the main session returned **86 nodes**, one distinct `frameId`, `SAME-FRAME-BTN` **absent**, closed-shadow buttons **present**. `Accessibility.queryAXTree{backendNodeId: <document>, accessibleName:"SAME-FRAME-BTN"}` returned **0** nodes — it does not cross either.
  >
  > **But the `frameId` parameter fully solves the same-origin case from the same session — no extra target, no extra attach.** `Accessibility.getFullAXTree{frameId: <same-origin child frame id>}` returned **6 nodes containing the button**, and `Accessibility.getRootAXNode{frameId}` returned a `RootWebArea`. Frame ids come from `Page.getFrameTree` on that session. So the stitching cost is **one AX call per frame in the process**, not one session per frame — only genuine OOPIFs need their own session. Worth encoding in the enumeration loop in §5, which already says this but is easy to over-read as "one session per iframe".
2. **Raw `getFullAXTree` is the biggest payload in the protocol, not the smallest.** Wikipedia: **9,282,016 bytes / 23,573 nodes** vs `captureSnapshot` 4,006,833 and `getDocument{pierce}` 3,863,052. Roughly **50% of it is `name.sources` + `chromeRole` + `ignoredReasons`** (github.com: 888 KB → 440 KB after stripping those). The "AX snapshots are token-efficient" folklore is about *what Playwright and chrome-devtools-mcp print*, not about the CDP payload. [CONFIRMED]
3. **Ignored nodes dominate.** 459/2010 on github.com; chrome-devtools-mcp's own issue [#635](https://github.com/ChromeDevTools/chrome-devtools-mcp/issues/635) reports 278 ignored of 543 and ~1,668 wasted tokens per snapshot. Drop ignored nodes and reparent their children.
4. **AX has no layout, no paint order, no CSS, no listeners, no z-order, no clipping.** An agent asking "why can't I click this" needs paint order and `pointer-events`, which only the snapshot has. AX also flattens away visual truth: two visually distinct buttons with the same label are indistinguishable.
5. AX **does** include closed shadow content (`CLOSED-SHADOW-BTN`, `NESTED-CLOSED-BTN` both present). [CONFIRMED]

**Design:** AX is an *enrichment layer keyed by `backendDOMNodeId`*, fetched per frame, stripped of `sources`/`chromeRole`/`ignoredReasons`, ignored-nodes collapsed. It supplies `role`, `name`, `description`, `value`, and the subset of `properties` that matters (`focusable`, `disabled`, `checked`, `expanded`, `required`, `invalid`, `level`, `selected`, `pressed`).

---

## 4. Shadow DOM, open and closed

**`DOM.getDocument{depth:-1, pierce:true}` returns closed shadow roots.** Directly observed on Chrome 151:

```json
"open":   [{"host":"OPEN-HOST","hostId":"oh","backendNodeId":29,
            "children":["STYLE","BUTTON","SLOT"]}],
"closed": [{"host":"CLOSED-HOST","hostId":"ch","backendNodeId":37,
            "children":["STYLE","BUTTON","SLOT","INNER-CLOSED"]},
           {"host":"INNER-CLOSED","backendNodeId":44,"children":["BUTTON"]}]
```

Nested closed-inside-closed works. `DOM.getNodeForLocation` also lands *inside* a closed root (returned `BUTTON#cbtn`). `DOMDebugger.getEventListeners{pierce:true}` reaches into closed roots too (§7). And for contrast, from an **isolated world** `document.getElementById('ch').shadowRoot` is `null` — JS cannot see it, CDP can, because CDP operates below the JS visibility boundary. [ALL CONFIRMED]

This is the strongest single argument for a CDP-native tree over any JS-injection approach: **a JS walker structurally cannot see closed shadow DOM, and CDP can.** It matches the note in [whatwg/dom#1290](https://github.com/whatwg/dom/issues/1290) that closed roots are available via `DOM.getDocument`.

`ShadowRootType` = `"user-agent" | "open" | "closed"`.

**UA shadow roots are the node-count problem.** A page with one `<input type=date>` and one `<video controls>` produced **21 `user-agent` shadow roots**. `DOM.getDocument` has no `includeUserAgentShadowDOM` flag (only `DOM.getNodeForLocation` does). Filter them out by default; expose them behind `--ua-shadow`. [CONFIRMED]

**Representation.** Shadow boundaries are real tree edges, not a flattening artifact:

```
element node
  ├── kind: Element
  ├── shadow_roots: Vec<NodeIdx>     // DOM.Node.shadowRoots
  └── children: Vec<NodeIdx>         // light DOM
shadow root node
  ├── kind: ShadowRoot { mode: Open | Closed | UserAgent }
  ├── adopted_stylesheets: Vec<StyleSheetId>   // DOM.Node.adoptedStyleSheets (EXP)
  └── children
slot node
  └── assigned: Vec<BackendNodeId>   // DOM.Node.distributedNodes
light-DOM node
  └── assigned_slot: Option<BackendNodeId>     // DOM.Node.assignedSlot
```

`distributedNodes` and `assignedSlot` were both populated in the fixture (`SPAN` ↔ `SLOT` backendNodeId 33/41), so slot assignment is directly readable and does not need JS. Path rendering should use `>>>` for shadow crossings, e.g. `closed-host#ch >>> button#cbtn`, and the compact view should note `(closed shadow)` because the agent's own JS (in `inspect.evaluate`) will *not* be able to reach that node — only harness-mediated actions will.

---

## 5. Iframes and the cross-frame geometry problem

### Frame/target topology

- `Page.getFrameTree` in a session returns **only the frames in that session's process**. On the fixture the main session's tree showed the main frame and the same-origin child; the cross-origin child was absent entirely — with *and* without auto-attach.
- `DOM.getDocument{pierce:true}` returns the OOPIF `<iframe>` element with a `frameId` and **`contentDocument` absent** (`hasContentDoc: false`), versus present for the same-origin frame.
- `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:false, flatten:true}` yields an `iframe`-type target per OOPIF. **Auto-attach is not inherited** — you must re-issue it on every new session to reach OOPIFs nested inside OOPIFs. The fixture's `A(localhost) > B(127.0.0.1) > A(localhost)` chain produced two `iframe` targets, the deepest only after arming auto-attach on the middle session.
- Attach events can arrive with `targetInfo.url == ""` (before the navigation commits). Resolve the URL from `Target.getTargets`, not from the attach event.
- `DOM.getFrameOwner{frameId}` (EXPERIMENTAL) works **only from the parent's session**; called in the OOPIF's own session it errors `"Frame with the given id does not belong to the target."` It is the bridge from a child frame back to its `<iframe>` element: returns `{backendNodeId, nodeId?}`.
- `DOM.getNodeForLocation` does **not** pierce OOPIFs: pointed at the middle of a cross-origin frame it returned the `IFRAME` element itself (`backendNodeId` matched the iframe's), not the button inside. Its returned `frameId` is the frame *containing* the hit node; `describeNode(iframe).frameId` is the frame the iframe *owns* — two different values, easy to confuse. [ALL CONFIRMED]

### The exact global-coordinate algorithm

Both spaces are viewport-relative, so no scroll term appears — `DOM.getBoxModel` in the child session is already the child's scroll-adjusted viewport space, and `DOM.getBoxModel` on the `<iframe>` in the parent is already the parent's scroll-adjusted viewport space.

```rust
/// Map a point from frame F's viewport space to the top-level viewport space.
/// Correct under nested OOPIFs, independent scroll in each frame, and CSS
/// transforms (scale/translate/rotate/skew) applied to any iframe element.
fn to_root_viewport(mut p: Point, mut frame: FrameId, s: &Sessions) -> Result<Point> {
    while let Some(parent) = s.parent_of(frame) {
        let child = s.session_for(frame);
        let par   = s.session_for(parent);

        // child's own viewport size in CSS px (NOT the iframe's attribute width/height)
        let vm = par_or_child_layout_metrics(child)?;            // Page.getLayoutMetrics
        let (cw, ch) = (vm.css_layout_viewport.client_width as f64,
                        vm.css_layout_viewport.client_height as f64);

        // owner <iframe> CONTENT quad, in the parent's viewport space, transform applied
        let owner = cdp!(par, "DOM.getFrameOwner", { "frameId": frame })?.backend_node_id;
        let q = cdp!(par, "DOM.getBoxModel", { "backendNodeId": owner })?.model.content;
        // q = [x0,y0, x1,y1, x2,y2, x3,y3] == TL, TR, BR, BL

        // affine basis: one child CSS px along x and along y, expressed in parent space
        let ex = ((q[2] - q[0]) / cw, (q[3] - q[1]) / cw);
        let ey = ((q[6] - q[0]) / ch, (q[7] - q[1]) / ch);

        p = Point { x: q[0] + ex.0 * p.x + ey.0 * p.y,
                    y: q[1] + ex.1 * p.x + ey.1 * p.y };
        frame = parent;
    }
    Ok(p)
}
```

**Empirical verification** — for each case, predict the global point, dispatch `Input.dispatchMouseEvent` **in the top-level session**, and read `event.clientX/clientY` from a handler inside the OOPIF:

| Case | child scroll | page scroll | iframe transform | predicted global | result |
|---|---|---|---|---|---|
| P1 | 0 | 0 | none | (110, 905) | `null` — button is scrolled out of the child's 250 px viewport (**correct**: the algorithm exposes that you must scroll the child first) |
| P2 | 700 | 0 | none | (110, 205) | `HIT@105,140` = exact local centre ✅ |
| P3 | 700 | 40 | none | (110, 165) | `HIT@105,140` ✅ |
| P4 | 700 | 0 | `scale(0.5) translate(100px,40px)` | affine (105, 416.5) | `HIT@105,140` ✅ — **naive** (157.5, 486.5) → `null` ❌ |

Plus an earlier independent run on the first fixture: predicted (468.80, 425.375) → `CROSSBTN@109,93`, exactly the button's local centre (109.80, 93.5); a control click 200×60 px away hit nothing. [ALL CONFIRMED]

**Preconditions the harness must enforce before returning a global point:**
1. `0 <= p.x <= clientWidth && 0 <= p.y <= clientHeight` in **every** frame on the chain, else emit `needs_scroll{frame}` and call `DOM.scrollIntoViewIfNeeded` in that frame's session first (it takes `nodeId|backendNodeId|objectId` and an optional `rect` relative to the node's border box).
2. The owner quad must be convex and non-degenerate; a rotated iframe still works (the basis handles it), but a 3D-transformed / `perspective` iframe is a projective map that two basis vectors cannot express — detect via `q` not being a parallelogram and refuse with `E_NONAFFINE_FRAME`.

> **Corrected 2026-08-04 — the originally proposed detector is mathematically wrong and would let the exact failure it is meant to catch through silently.** Tested on Chrome 151 with two OOPIFs in one page: `#af` under `transform: rotate(25deg) scale(0.6)` (affine) and `#pf` under an ancestor `perspective:400px` with `transform: rotateY(45deg)` (projective).
>
> ```
> #pf  DOM.getBoxModel.content = [50,50, 292.58,119.31, 292.58,250, 50,250]     <- trapezoid
> #af  DOM.getBoxModel.content = [252.91,442.57, 416.05,518.64, 365.33,627.40, 202.20,551.33]
> ```
> The old test was `q[4]-q[2] != q[6]-q[0]`, i.e. `x2-x1` vs `x3-x0`. For `#pf` that is `292.58-292.58 = 0` vs `50-50 = 0` → **equal → "parallelogram" → accepted**. It only ever compares the *x* components of two edges and ignores *y* entirely, so it cannot see the vertical shear that perspective produces.
>
> **The correct test compares both components of both opposite edges** (`TL→TR` must equal `BL→BR`, which for a quad ordered TL,TR,BR,BL means `q1-q0 == q2-q3`):
> ```rust
> // q = [x0,y0, x1,y1, x2,y2, x3,y3] == TL, TR, BR, BL
> fn is_affine(q: &[f64; 8], eps: f64) -> bool {
>     let (ex_top_x, ex_top_y) = (q[2] - q[0], q[3] - q[1]);   // TL -> TR
>     let (ex_bot_x, ex_bot_y) = (q[4] - q[6], q[5] - q[7]);   // BL -> BR
>     (ex_top_x - ex_bot_x).abs() < eps && (ex_top_y - ex_bot_y).abs() < eps
> }
> ```
> Measured verdicts: old detector flagged **neither** frame as non-affine; the corrected test flags `#pf` (`Δy = 69.31`) and correctly passes `#af` (`Δ = 0,0`).
>
> **And the failure is real, not theoretical.** End-to-end click test — compute the affine prediction for the child button's centre, dispatch `Input.dispatchMouseEvent` in the top-level session, read `clientX/Y` from a handler inside the OOPIF:
>
> | iframe | child-local centre | affine prediction | what the OOPIF received |
> |---|---|---|---|
> | `#af` rotate+scale | (150, 100) | (309.1, 535.0) | `[149, 99]` on the button ✅ (1 px) |
> | `#pf` perspective | (150, 100) | (171.3, 184.7) | `["doc", 118, 120]` — **missed the button entirely**, landed on the document 32 px off ❌ |
>
> So: the affine model is **confirmed exact for rotate+scale+translate**, **confirmed broken under perspective**, and the shipped guard must be the corrected test. `perspective` + `rotateY` is not exotic — it is the standard CSS card-flip/carousel idiom — so `E_NONAFFINE_FRAME` will fire in the wild. A future improvement is to solve the full 8-DOF homography from the four quad corners (four point correspondences are exactly enough), which *would* handle perspective; that is strictly better than refusing, and is ~30 lines of linear algebra. [affine CONFIRMED; perspective failure CONFIRMED; old detector REFUTED]
3. Re-verify with `DOM.getNodeForLocation` at the computed point *in the deepest frame's own session* (child-local coords) before dispatching a destructive gesture.

### Stitching the tree across processes

```
for each attached target (page + iframe targets, recursively auto-attached):
    DOM.enable, CSS.enable, DOMSnapshot.enable, Accessibility.enable, Runtime.enable
    doc[t]  = DOM.getDocument{depth:-1, pierce:true}      // structure + shadow + identity
    snap[t] = DOMSnapshot.captureSnapshot{...}            // layout/paint/computed
    ax[t]   = for each frame in Page.getFrameTree(t):
                  Accessibility.getFullAXTree{frameId}     // AX does not cross frames
join:
    within a target:   backendNodeId is the primary key across doc/snap/ax/listeners
    across targets:    DOM.getFrameOwner{frameId} (parent session) -> owner backendNodeId
                       -> splice child document under the owner <iframe> node
                       -> record the affine basis on the edge for coordinate mapping
```

Note that `backendNodeId` is only unique **within a renderer process**, so the unified tree's primary key must be `(target_id, backend_node_id)`. Refs shown to the agent are `@n<seq>` allocated by the daemon, not raw backend ids, precisely so that two frames can't collide.

---

## 6. CSS

`CSS` is an **EXPERIMENTAL** domain but `getMatchedStylesForNode`, `getComputedStyleForNode`, `getInlineStylesForNode`, `getStyleSheetText`, `setStyleTexts`, `startRuleUsageTracking` are not individually flagged. [CONFIRMED]

### `CSS.getMatchedStylesForNode{nodeId}` — Chrome 151 returns

`inlineStyle?`, `attributesStyle?`, `matchedCSSRules?`, `pseudoElements?`, `inherited?`, `inheritedPseudoElements?`, `cssKeyframesRules?`, `cssPositionTryRules?`, `activePositionFallbackIndex?`, `cssPropertyRules?`, `cssPropertyRegistrations?`, `cssAtRules?`, `parentLayoutNodeId?` (EXP), `cssFunctionRules?` (EXP).

Each `RuleMatch` = `{rule: CSSRule, matchingSelectors: int[]}`. `CSSRule` carries `origin` (`"user-agent" | "injected" | "inspector" | "regular"`), `styleSheetId` (**absent for UA sheets**), `nestingSelectors?` (EXP), `originTreeScopeNodeId?` (EXP — which shadow tree the rule came from, exactly what you need for shadow-scoped CSS), and the condition arrays: `media[]`, `containerQueries[]` (EXP), `supports[]` (EXP), `layers[]` (EXP), `scopes[]` (EXP), `startingStyles[]` (EXP), `navigations[]` (EXP), `ruleTypes[]` (EXP). Each array is ordered **innermost → outermost**.

Observed on the fixture for `button#plain.cta`:

```
button                    origin=user-agent  layers=[]        ruleTypes=[]
button                    origin=regular     layers=[base]    ruleTypes=[LayerRule]
.cta                      origin=regular     layers=[theme]   ruleTypes=[LayerRule]
.cta                      origin=regular     media=[(min-width: 300px)]   ruleTypes=[MediaRule]
.cta                      origin=regular     supports=[(display: grid)]   ruleTypes=[SupportsRule]
```

**Specificity is given to you.** `SelectorList.selectors[].specificity` = `{a, b, c, components?}` where `a`=ID count, `b`=class/attr/pseudo-class count, `c`=type/pseudo-element count. `components` (EXP, new in recent Chrome) breaks it down per simple selector:

```json
{"a":0,"b":0,"c":1,"components":[{"text":"button","a":0,"b":0,"c":1}]}
```

So you **do not compute specificity yourself** — but you *do* have to implement the cascade to answer "which rule won and why", because CDP gives you all matches, not the winner. Order of precedence to implement: origin & importance → cascade layers (`CSS.getLayersForNode` returns the full `rootLayer` tree with `order` integers — observed `{implicit outer layer, [base:0, theme:1]}`) → specificity (`a,b,c`) → source order (`SourceRange` on each selector). Inline style beats all non-`!important` author rules. `matchingSelectors` tells you *which* selectors in a list actually matched, so use the max specificity among those, not over the whole list.

### `CSS.getComputedStyleForNode` — the token bomb

- Fixture node: **483** properties.
- github.com `<a>`: **2,465** properties, of which **1,984 are custom properties (`--*`)**, 153,971 bytes for one node. [CONFIRMED]

So yes, computed style **does** include CSS custom properties — inherited registered and unregistered ones alike (`--brand: #c0ffee`, `--pad: 8px` were both returned). It also returns `extraFields.isAppearanceBase` (EXP). Never return this raw. The harness ships a curated ~40-property set by default and `--css-all` / `--css '<prop>,<prop>'` for the rest.

### `CSS.resolveValues` (EXPERIMENTAL, present in Chrome 151)

`resolveValues{values:string[], nodeId, propertyName?, pseudoType?, pseudoIdentifier?} -> {results:string[]}`. Observed:

```
["var(--brand)", "calc(var(--pad) * 2)", "1em"]  with propertyName "background-color"
  -> ["rgb(192, 255, 238)", "16px", "13.3333px"]
```

This is the correct tool for "what does this variable actually evaluate to here" without a `Runtime.evaluate`. Also present: `CSS.getLonghandProperties{shorthandName,value}` (EXP) for expanding shorthands, and `CSS.getEnvironmentVariables` (EXP) which returned the 15 `safe-area-*`/`keyboard-inset-*`/`preferred-text-scale` vars. [CONFIRMED]

### Rest of the CSS surface

- `CSS.getInlineStylesForNode{nodeId}` → `{inlineStyle?, attributesStyle?}` (the latter is presentational attributes like `width="320"`).
- `CSS.getStyleSheetText{styleSheetId}`; `CSS.styleSheetAdded` events carry `sourceURL`, `sourceMapURL`, `origin`, `isInline`, `startLine/Column`, `hasSourceURL`. `styleSheetId` is `null` on UA rules, so UA rules have no text to fetch.
- **Coverage:** `CSS.startRuleUsageTracking` → `CSS.stopRuleUsageTracking` / `CSS.takeCoverageDelta` returns `RuleUsage{styleSheetId, startOffset, endOffset, used}`. Verified working; offsets are into the stylesheet text so you must fetch the text to render them.
- **Live edit + undo:** `CSS.setStyleTexts{edits:[{styleSheetId, range, text}]}` for declarations, `CSS.setRuleSelector`, `CSS.setMediaText`, `CSS.setSupportsText`, `CSS.setScopeText`, `CSS.setContainerQueryConditionText`, `CSS.addRule`, `CSS.createStyleSheet`. Undo is via the **DOM** domain: `DOM.markUndoableState` (EXP) then `DOM.undo` / `DOM.redo` (both EXP). These are `mutate`-capability operations; record the original `getStyleSheetText` before every edit so undo does not depend on an experimental command.
- `CSS.forcePseudoState{nodeId, forcedPseudoClasses:["hover","active","focus","focus-visible","focus-within","visited","target"]}` and `CSS.forceStartingStyle` for inspecting states without gestures.
- `CSS.getAnimatedStylesForNode` (EXP) and `cssKeyframesRules` for animation state. `CSS.getPlatformFontsForNode` for actual font fallback resolution.
- Pseudo-elements: `pseudoElements[]` in matched styles, plus `pseudoType`/`pseudoIdentifier` on nodes (observed `::before` for `#ov::before`). `DOM.getTopLayerElements` (EXP) returned `[::backdrop, DIALOG]` after `dialog.showModal()` — that is the reliable modal-state signal for the site-graph work.

---

## 7. Event listeners — and why per-node listeners lie

`DOMDebugger.getEventListeners{objectId, depth?, pierce?}` — **not experimental**. The brief assumed this is per-node and expensive. It is not:

> `depth` — The maximum depth at which Node children should be retrieved, defaults to 1. Use -1 for the entire subtree.
> `pierce` — Whether or not iframes and shadow roots should be traversed when returning the subtree. **Reports listeners for all contexts if pierce is enabled.**

Resolve the **document node** once (`DOM.resolveNode{nodeId: root}`) and call it with `depth:-1, pierce:true` → **the whole-page listener map in one call**, each entry carrying `backendNodeId`. Verified: it returned listeners on `document` (delegated capture handler), on individual elements, **inside a closed shadow root** (`CLOSED_SHADOW_LISTENER`), and **inside a same-origin iframe** (`IFRAME_LISTENER`). [CONFIRMED]

> **Verified 2026-08-04 at real-site scale — the "untested at scale, may OOM or truncate" risk does not materialise.** Chrome 151, 1280×900, live network:
> ```
> github.com/rust-lang/rust    2,394 elements -> 1,007 listeners, 158,205 B, 0.01 s
>                              top types: click 220, keydown 70, mouseover 36, mouseleave 34, keyup 33
> en.wikipedia.org/wiki/Rust…  9,534 elements -> 1,161 listeners, 181,868 B, 0.01 s
>                              top types: mouseover 380, mouseout 380, click 335, keydown 16
> ```
> One call, ~10 ms, ~180 KB on the two heaviest pages in this document's corpus. **No truncation, no OOM, latency negligible.** The residual concern is the one the original text raised and it is real: the result holds ~2× that many `RemoteObject` handles (`handler` + `originalHandler`) in the default object group, so `Runtime.releaseObjectGroup` after every batch is mandatory, not optional. Also note Wikipedia's profile — 760 of 1,161 listeners are `mouseover`/`mouseout` pairs from the page-preview feature — so "listener count" is a poor interactivity proxy on its own, which reinforces the delegation-map presentation below.

Each `EventListener` = `{type, useCapture, passive, once, scriptId, lineNumber, columnNumber, handler?: RemoteObject, originalHandler?: RemoteObject, backendNodeId?}`. `originalHandler` is the un-bound function when the site wrapped it — prefer it for source attribution.

Two gaps:
- **`window` listeners are not included** (window is not a DOM node). Do a separate `Runtime.evaluate{expression:"window"}` → `getEventListeners{objectId}`. Same for `document.defaultView`, `visualViewport`, `navigator`, and any `XMLHttpRequest`/`WebSocket` object you care about.
- **It does not cross OOPIFs** (nothing does). One call per target.

### The delegation problem, presented honestly

React ≥17 attaches a small number of root listeners to the app container; Vue/Svelte attach mostly per-element; jQuery `.on(sel, ...)` delegates from `document`. So a per-node "hasClickHandler" is wrong in both directions. The measured proof: the DOMSnapshot `isClickable` flag marked `#document` clickable (delegated handler) and did **not** mark three real `<button>`s.

Emit a **listener map** instead:

```json
{
  "delegation_roots": [
    {"ref":"@n5","node":"#document","types":["click"],"capture":true,
     "script":"index.html:65","fn":"delegatedRootHandler",
     "note":"root-level delegated handler; descendants may be interactive without their own listener"}
  ],
  "direct": [
    {"ref":"@n19","types":[{"t":"click","capture":false,"passive":false,"once":false},
                           {"t":"keydown","capture":true,"passive":false,"once":false}],
     "script":"index.html:63","fn":"directHandler"}
  ],
  "coverage_note": "3 of 7 interactive elements have no direct listener; 1 delegation root covers the subtree. Absence of a direct listener does NOT mean the element is inert."
}
```

And for a single node, `browserctl inspect listeners @n19 --effective` should walk the ancestor chain (through shadow hosts and frame owners) and report *which* delegated roots would also see a bubbling event from that node. That is the honest answer to "is this clickable".

---

## 8. JS ownership

- **Who created this node.** `DOM.setNodeStackTracesEnabled{enable:true}` (EXP) then `DOM.getNodeStackTraces{nodeId}` (EXP) → `{creation: {callFrames: [{functionName, scriptId, url, lineNumber, columnNumber}]}}`. Verified working. Enable it only for `inspect` sessions — it makes every node creation record a stack.
- **Where the handler lives.** From `EventListener.scriptId` + `lineNumber`/`columnNumber` → `Debugger.getScriptSource{scriptId}`. `Debugger.scriptParsed` events carry `url`, `sourceMapURL`, `hasSourceURL`, `length`, `executionContextId`, `hash`, `isModule`. Fetch `sourceMapURL` yourself (it may be a `data:` URI or a relative URL to resolve against the script URL) and map (line, column) → original file. Rust: `sourcemap` crate for the mapping.
- **Noise suppression.** `Debugger.setBlackboxPatterns{patterns, skipAnonymous?}` and `Debugger.setBlackboxedRanges{scriptId, positions}` to keep framework internals out of attributions. Sensible defaults: `/node_modules/`, `/webpack/`, `react-dom`, `zone.js`, `.min.js$`.
- **Closures.** `Runtime.getProperties{objectId, ownProperties:false}` → `internalProperties` with `[[FunctionLocation]]` (`{scriptId,lineNumber,columnNumber}`) and `[[Scopes]]` (a `subtype: "internal#scopeList"` object). Recursing into `[[Scopes]]` gave `["Closure (outer)", "Script", "Global"]`; each scope object can be expanded again to read the captured variables. Verified. This is how you answer "what state does this handler close over".
- **Frameworks** (optional adapter layer): React exposes `__reactFiber$*` / `__reactProps$*` expando keys on host DOM nodes; Vue 3 sets `__vue_app__` on the mount root and `__vnode`/`__vueParentComponent` on elements; Svelte 5 has no stable public hook (rely on `data-svelte-h` and compiler-emitted markers).

> **REFUTED 2026-08-04 — this is the highest-impact correction in this document, and it invalidates the adapter design as written.** The original text claimed expandos "are visible across worlds because they live on the DOM node". **They are not.** Blink gives every isolated world its **own V8 wrapper object** for the same underlying C++ `Node`; a property assigned by page script lands on the *main world's* wrapper and is invisible everywhere else.
>
> Test: a page sets `el.__myExpando = {a:1}`, `el['__reactFiber$xyz'] = {tag:5,…}`, `el['__reactProps$xyz'] = {...}` and `window.__mainWorldMarker = 1`. A `brow_world` isolated world was created via `Page.addScriptToEvaluateOnNewDocument{worldName:"brow_world", runImmediately:true}`. The **same node** was bridged into each world with `DOM.resolveNode{backendNodeId, executionContextId}` and inspected with `Runtime.callFunctionOn`:
>
> ```
> main     -> {"dunder":["__myExpando","__reactFiber$xyz","__reactProps$xyz"],
>              "reactFiberKey":"__reactFiber$xyz","reactFiberTag":5,
>              "expando":"{\"a\":1}","mainWorldMarker":"number","browHelper":"undefined"}
> isolated -> {"dunder":[],"reactFiberKey":null,"reactFiberTag":null,
>              "expando":null,"mainWorldMarker":"undefined","browHelper":"number"}
> ```
> `Object.keys(node)` in the isolated world is **empty**. Attribute/child/style access still works there (those go through the C++ node), but *anything a framework hung off the JS object is gone*.
>
> **Consequences the adapter crates must be scoped around:**
> 1. **React/Vue/Svelte adapters must run in the MAIN world** (`Runtime.callFunctionOn` with no `executionContextId`, or `Runtime.evaluate` on the default context). There is no isolated-world path to fiber data. This reopens exactly the tamper-surface question flagged in Open Question 6 — a hostile or merely defensive page can shadow `Object.keys`, poison prototypes, or booby-trap the getters the adapter touches.
> 2. Mitigation, since main-world execution is unavoidable: capture pristine references (`Object.getOwnPropertyNames`, `Reflect.ownKeys`, `Function.prototype.call`) in a `Page.addScriptToEvaluateOnNewDocument` **document-start main-world** script, before page script runs, and use only those. Treat every value read as untrusted input — size-cap it, never `eval` it, and mark adapter output `provenance: "main-world (page-observable, page-tamperable)"` in the node payload.
> 3. The security posture must be stated honestly: `inspect.evaluate` in the isolated world is tamper-proof; **framework enrichment is not**, and it is observable by the page. Consider gating adapters behind a capability flag rather than enabling them by default.
> 4. This is *also* why `window.__REACT_DEVTOOLS_GLOBAL_HOOK__` is unreachable from the isolated world — same mechanism, so there was never a two-tier story here.
>
> Independently confirmed in the same run: the named world **is** recreated per navigation (`Runtime.executionContextCreated` showed `('',1,default) ('brow_world',2) ('',3,default) ('brow_world',4)` across one `Page.navigate`), the helper is invisible from the main world and vice versa, and — a trap worth writing down — **you must use the newest `brow_world` context id**; resolving a node into the pre-navigation world id fails with `-32000 Node with given id does not belong to the document`.

---

## 9. Isolated worlds

```jsonc
// preferred: survives navigation, one world per frame, created before page scripts run
{"method":"Page.addScriptToEvaluateOnNewDocument","params":{
  "source":"globalThis.__brow = { /* helpers */ };",
  "worldName":"brow_world",
  "runImmediately":true}}
// -> {"identifier":"1"}   ; remove with Page.removeScriptToEvaluateOnNewDocument
```

Verified behaviour on Chrome 151:
- Worlds named `brow_world` were auto-created in **every** frame (main + same-origin child), reported via `Runtime.executionContextCreated` with `auxData.isDefault: false` and `context.name == "brow_world"`.
- `typeof globalThis.__brow_helper` = `"object"` in the world, `"undefined"` in the main world. The reverse also holds: a main-world `window.__brow_marker` was `"undefined"` from the isolated world.
- After `Page.navigate`, new `brow_world` contexts appeared and the helper was present again — **no re-injection needed**.
- `Page.createIsolatedWorld{frameId, worldName, grantUniveralAccess}` (note the protocol's actual misspelling **`grantUniveralAccess`**) still exists for ad-hoc worlds and returns an `executionContextId` directly.
- `DOM.resolveNode{nodeId, executionContextId: <world>}` bridges a CDP node into the world. [ALL CONFIRMED]

Why this matters for the security model in this project: `inspect.evaluate` runs in the isolated world, so page scripts cannot observe or tamper with harness helpers, and helpers cannot be poisoned by prototype patching in the main world. It also means the isolated world **cannot** see closed shadow roots (verified `shadowRoot === null`) — anything shadow-related must go through CDP, not through injected JS. Track the world id per `(frameId, generation)` and drop it on `Runtime.executionContextDestroyed` / `executionContextsCleared`.

---

## 10. Canvas, WebGL, and what is genuinely unavailable

Be blunt with the agent here.

| Thing | Status |
|---|---|
| `<canvas>` element geometry, attributes, listeners, paint order | Available — it is a normal DOM node |
| Canvas **pixel content** | Only via screenshot of the node's region. `DOM.getOuterHTML` returns `<canvas id="cv" width="120" height="60"></canvas>` — an empty shell. Verified |
| Canvas draw-call log / retained scene graph | **Not available.** The old `Canvas`/`WebGL` CDP domains were removed years ago; Chrome 151's protocol has no `Canvas` domain. `Schema.getDomains` itself is gone (`'Schema.getDomains' wasn't found`). Verified |
| Text inside canvas | Not in DOM, not in AX (unless the site provides a fallback subtree or ARIA). OCR is out of scope |
| WebGL/WebGPU state | Not available via CDP |
| Flutter Web / CanvasKit | Renders to canvas; the DOM tree is a handful of nodes. The **only** structural signal is Flutter's semantics tree, which Flutter emits into the DOM as `<flt-semantics>` elements **when accessibility is enabled** — off by default. The adapter must force it on and the harness must say "structure unavailable; enable semantics" when it isn't |
| `<video>`/`<audio>` frames | Not readable as pixels via CDP (screenshot only) |
| Cross-origin `<iframe>` under a restrictive `Permissions-Policy` | Still attachable as a target — the OOPIF boundary is a *process* boundary, not a permission one |

Honest statement to ship in `SKILL.md`: *"Canvas-rendered UI (including Flutter Web without semantics, and most charting libraries) is visible to the harness only as pixels. The harness can screenshot it, click coordinates inside it, and diff it — it cannot enumerate its contents."*

---

## 11. Output format for agents: the tiered view

### Measured cost of the naive approach

| Page | DOM nodes | `getDocument{pierce}` | `captureSnapshot` | `getFullAXTree` |
|---|---|---|---|---|
| en.wikipedia.org/wiki/Rust_(programming_language) | 19,707 | 3.86 MB / ~1.04M tok | 4.01 MB / ~1.08M tok (211 ms) | **9.28 MB / ~2.51M tok** (437 ms) |
| github.com/rust-lang/rust | 3,670 | 0.91 MB / ~246K tok | 0.62 MB / ~168K tok (57 ms) | 0.89 MB / ~240K tok (27 ms) |

(Token figures are estimates at 3.7 bytes/token for dense JSON — the byte counts are exact.)

> **Verified 2026-08-04 — the byte counts replicate almost exactly on an independent run**, which is the strongest evidence in this document that the token-economics argument is sound even if the bytes→tokens constant is not. Chrome 151, 1280×900, live network:
> ```
> Wikipedia  Accessibility.getFullAXTree     9,288,017 B / 23,573 nodes   (orig: 9,282,016 / 23,573)
> Wikipedia  DOMSnapshot.captureSnapshot     3,939,916 B                  (orig: 4,006,833)
> github     Accessibility.getFullAXTree       888,159 B /  2,010 nodes   (orig:   888,206 /  2,010)
> github     DOMSnapshot.captureSnapshot       608,799 B                  (orig:   620,000-ish)
> ```
> Node counts are identical to the unit. **Still UNVERIFIED: the 3.7 bytes/token conversion itself** — no tokenizer was run in either pass. Treat every "≈N tokens" figure in this document as ±40 %, and gate the output-size policy on *bytes*, which are measurable, not on tokens.

Single fat node, `<a>` on github.com: `describe` 465 B, `box` 171 B, `ax` 3,086 B, **`computed` 154,022 B**, **`matched` 985,478 B** (of which `inherited` = 1,062,842 B across only 11 entries — the universal `*` rules and `:root` custom-property blocks are repeated per inherited ancestor). Total **~309K tokens for one node.**

### Tier 0 — `browserctl page tree` default: compact interactive lines

`--interactive` qualifies a node when **all** of:

**Qualifies (any of):**
- tag ∈ `{a[href], button, input, select, textarea, summary, details, label, option, video, audio, iframe}`
- AX role ∈ `{button, link, textbox, searchbox, checkbox, radio, combobox, listbox, option, menuitem, menuitemcheckbox, menuitemradio, tab, switch, slider, spinbutton, treeitem, gridcell, columnheader, rowheader}`
- has a **direct** listener of type `click|mousedown|pointerdown|keydown|submit|change|input` (from the one-shot listener map)
- `[tabindex]` present and ≠ `-1`, or `[contenteditable]`, or `[role]` present, or `[onclick]` present
- is a delegation root (emitted separately, in its own section)

**Disqualifies (any of):**
- no entry in `layout.nodeIndex` (not rendered)
- computed `display:none` / `visibility:hidden|collapse` / `opacity:0`
- `bounds.w <= 0 || bounds.h <= 0`
- `pointer-events:none` and no descendant that re-enables it
- `aria-hidden="true"` or AX `ignored` with reason ∈ `{notRendered, ariaHiddenElement, ariaHiddenSubtree, presentationalRole}`
- `disabled` (kept, but rendered with a `!disabled` marker rather than dropped — the agent needs to know *why* the click won't work)
- covered: `paintOrders` of an opaque element strictly greater over the same `bounds` — emit `!occluded-by @nX`

Format (line-oriented, not JSON — measured ~22% cheaper than minified JSON for the same content):

```
@n1532 link      "Homepage"                    [32,20 32x32]   -> /
@n192  searchbox "Search Wikipedia"            [266,17 404x32]
@n7714 button    "Search"                      [669,17 71x32]
@n2201 button    "Star"                        [1103,88 78x28] !delegated:@n5
@n2210 link      "Fork"                        [1189,88 66x28] -> /rust-lang/rust/fork
@n3301 button    "Sign in"                     [1180,20 78x28] !occluded-by @n3290
frame @n1899 <iframe#checkout http://pay.example.com> 3 interactive nodes (use --frame @n1899)
```

Measured: **github.com 253 elements → 22,343 bytes ≈ 6.0K tokens** (vs 168K–246K raw). **Wikipedia 1,623 elements → 146,406 bytes ≈ 39.6K tokens** — still far too much, which is why the default must additionally be:

- **viewport-scoped** (`--scope viewport`, the default): on github.com 1,251 of 2,253 layout nodes were in the first viewport, so this is only ~2× — not enough alone;
- **paged** (`--limit 60 --cursor <opaque>`), with a trailer `… 1563 more interactive nodes; refine with --near @n192 | --role link | --text "..." | --region x,y,w,h`;
- **queryable first**: encourage `browserctl page find --role button --text "Sign in"` (backed by `Accessibility.queryAXTree{role}` = 15.5 KB / 17 nodes on github.com) over dumping the tree.

### Tier 1 — `page tree --structure`

Adds landmark/heading skeleton and containers (`main`, `nav`, `form`, `dialog`, `table`, `ul`) with child counts, so the agent can navigate before it enumerates. Target ≤ 2K tokens for any page.

### Tier 2 — `inspect node @n42` (the fat node), budgeted

Never the raw CDP payload. Composed and pruned:

```jsonc
{
  "ref": "@n42", "tag": "a", "id": null, "classes": ["Button","Button--secondary"],
  "frame": "@n1899 (oopif http://pay.example.com)",
  "path": "main > div.AppHeader >>> closed-shadow > a.Button",   // >>> = shadow crossing
  "ax": {"role":"link","name":"Star this repository","focusable":true,"disabled":false},
  "geometry": {
    "viewport": [1103,88,78,28], "document": [1103,1288,78,28],
    "clipped_by": "@n40 (overflow:auto)", "paint_order": 47,
    "stacking_context": "@n12", "occluded": false, "in_viewport": true
  },
  "css": {                       // ~40 curated props, not 2465
    "display":"inline-flex","position":"relative","z-index":"auto",
    "pointer-events":"auto","cursor":"pointer","opacity":"1",
    "background-color":"rgb(246,248,250)","color":"rgb(31,35,40)"
  },
  "css_winners": [               // only properties whose cascade was contested
    {"prop":"background-color","value":"rgb(246,248,250)",
     "won":{"sel":".Button--secondary","sheet":"primer.css:1204","layer":"components","spec":[0,1,0]},
     "lost":[{"sel":".Button","sheet":"primer.css:1180","layer":"components","spec":[0,1,0],"why":"source order"},
             {"sel":"a","sheet":"<user-agent>","spec":[0,0,1],"why":"origin"}]}
  ],
  "listeners": {"direct":[{"type":"click","capture":false,"passive":false,
                           "source":"app.js:2210:14 -> src/Button.tsx:88:7","fn":"handleStar"}],
                "delegated_ancestors":["@n5 click (capture)"]},
  "state": {"value":null,"checked":null,"selected":null,"scrollable":false},
  "framework": {"kind":"react","component":"StarButton","props_keys":["repoId","starred"]},
  "children_summary": "1 svg, 1 #text \"Star\""
}
```

Budget: **≤ 900 tokens**. Everything bigger is behind an explicit flag: `--css-all`, `--matched-raw`, `--listeners-source`, `--ax-sources`.

### Tier 3 — raw dumps to artifacts, never to the agent

`browserctl page snapshot --out snap.json` writes the full `DOMSnapshot` + AX + listener map to `artifacts/`, prints only a summary line and a path. The agent can then run `browserctl page query <jq-like>` server-side against it.

---

## 12. Rust implementation sketch

Latest stable versions from crates.io on 2026-08-04: `serde 1.0.229`, `serde_json 1.0.151`, `simd-json 0.17.3`, `tokio 1.53.1`, `slotmap 1.1.1`, `indexmap 2.14.0`, `smallvec 1.15.2`, `compact_str 0.10.0`, `rustc-hash 2.1.3`, `ahash 0.8.12`, `dashmap 6.2.1`, `parking_lot 0.12.5`, `memchr 2.8.3`. Plus `sourcemap` for §8. **No `chromiumoxide`, no `headless_chrome`, no `fantoccini`** — the CDP types are generated from the vendored `protocol/browser_protocol.json` + `js_protocol.json` by a `build.rs` in `crates/cdp-protocol`.

```rust
// crates/inspection/src/tree.rs
pub struct UnifiedTree {
    pub nodes: SlotMap<NodeKey, Node>,                       // slotmap 1.1
    pub by_backend: FxHashMap<(TargetId, BackendNodeId), NodeKey>,
    pub by_ref: FxHashMap<u32, NodeKey>,                     // @n42 -> node
    pub frames: FxHashMap<FrameId, FrameInfo>,               // affine basis lives here
    pub strings: StringInterner,                             // reuse the snapshot's table
    pub generation: u64,
}

pub struct Node {
    pub parent: Option<NodeKey>,
    pub children: SmallVec<[NodeKey; 4]>,
    pub kind: NodeKind,          // Element{tag} | Text | ShadowRoot{mode} | Document | Pseudo{ty} | FrameOwner{frame}
    pub backend: BackendNodeId,
    pub target: TargetId,
    pub attrs: SmallVec<[(StrId, StrId); 4]>,
    pub layout: Option<LayoutIdx>,          // -> parallel arrays from DOMSnapshot
    pub ax: Option<AxIdx>,
    pub listeners: SmallVec<[ListenerIdx; 1]>,
    pub flags: NodeFlags,        // INTERACTIVE | RENDERED | OCCLUDED | UA_SHADOW | IN_CLOSED_SHADOW | SCROLLABLE
}

pub struct FrameInfo {
    pub id: FrameId, pub parent: Option<FrameId>, pub target: TargetId,
    pub owner: Option<(TargetId, BackendNodeId)>,   // DOM.getFrameOwner
    pub basis: Option<Affine2>,                     // (origin, ex, ey) into parent viewport space
    pub client_size: (f64, f64),
    pub scroll: (f64, f64),
    pub doc_gen: u64,
}
```

Keep the snapshot's `strings` table as-is and intern into it — it was only 2,400 entries for github.com and 14,457 for Wikipedia, so string data is a tiny fraction of the payload and re-allocating it is pure waste. ~~Parse the snapshot with `simd-json` (the payload is 4 MB of numeric arrays; `serde_json` is the bottleneck at that size). [performance claim UNVERIFIED — benchmark before committing]~~

> **REFUTED 2026-08-04 — benchmarked, and `simd-json` is not worth it for the snapshot.** `cargo build --release` (rustc 1.97.1, `serde_json 1.0`, `simd-json 0.17.3`), Apple M4, parsing the *actual* payloads captured live from Chrome 151 into a generic `Value` / `OwnedValue`, 3 runs each:
>
> | payload | `serde_json` | `simd-json` | verdict |
> |---|---|---|---|
> | `DOMSnapshot.captureSnapshot`, Wikipedia, **4,362,821 B** | 12.08 / 10.22 / **9.42 ms** | 11.78 / 10.14 / **9.80 ms** | **a wash — no win** |
> | `Accessibility.getFullAXTree`, Wikipedia, **9,288,017 B** | 31.72 / 29.74 / **30.42 ms** | 22.28 / 21.87 / **17.98 ms** | ~35 % faster |
>
> The premise was wrong twice over: (a) `serde_json` parses the 4 MB snapshot in **~10 ms**, which is 5 % of the 211 ms Chrome spends *producing* it — it was never the bottleneck; (b) the win only appears on the AX tree, which is the payload the design already says to strip and never store raw (§3). And this benchmark uses the *slow* path (generic `Value`); deserialising straight into `#[derive(Deserialize)]` structs, which is what the codegen in `10-…` §9.3 produces, is faster still.
>
> **Decision: use `serde_json`. Do not take the `simd-json` dependency** — it needs mutable input buffers, has a divergent API, and buys ~0 ms where it matters. Revisit only if profiling shows AX-tree parsing on the hot path, and even then prefer "don't fetch the AX tree raw".

---

## What we verified empirically

Environment: macOS Darwin 25.5.0, **Google Chrome 151.0.7922.72** (V8 15.1.206.10), `--headless=new`, scratch `--user-data-dir`, driven over `--remote-debugging-pipe` (fd 3 in / fd 4 out, NUL-delimited JSON) by a dependency-free Python client using `os.posix_spawn` with `POSIX_SPAWN_DUP2` file actions. All Chrome processes and the two fixture HTTP servers were killed afterwards.

| # | What was run | Raw observation |
|---|---|---|
| 1 | `curl http://127.0.0.1:39411/json/protocol` | 1,605,774-byte protocol JSON for Chrome 151; used for every "exact param name" claim in this document |
| 2 | `DOM.getDocument{depth:-1,pierce:true}` on a fixture with open/closed/nested-closed shadow roots | Returned **both closed roots**, incl. a closed root nested inside a closed root; `shadowRootType:"closed"` |
| 3 | Same, on a page with `<input type=date>` + `<video controls>` | **21 `user-agent` shadow roots**; no flag to suppress them |
| 4 | `DOMSnapshot.captureSnapshot` with all five params | 2 documents (main + same-origin frame), 129 strings; `shadowRootType` rare-data contained `"closed"`; `SLOTTED-CLOSED`/`NESTED-CLOSED-BTN` text present; **cross-origin frame entirely absent** |
| 5 | `isClickable` on the fixture | `['#document','BUTTON','A']` — flagged the document because of a delegated handler, missed 3 plain `<button>`s |
| 6 | Snapshot at scroll 0 vs scroll 900 vs `getBoxModel` | snapshot `bounds` = document space (fixed element moved 5→905); `getBoxModel` = viewport space (4382.17→3482.17) |
| 7 | `Emulation.setDeviceMetricsOverride{deviceScaleFactor:3}` | Snapshot bounds and box model **identical** to DPR 1 — all CDP layout is CSS px |
| 8 | `Accessibility.getFullAXTree` on the fixture, main session | 60 nodes, 1 distinct `frameId`; closed-shadow buttons **present**; `SAME-FRAME-BTN` (same-origin iframe) **absent** |
| 9 | `Target.setAutoAttach{flatten:true}` on an `A > B > A` frame chain | 2 `iframe` targets; auto-attach had to be re-armed on the child session to reach the grandchild; attach events arrive with `url:""` |
| 10 | `DOM.getFrameOwner{frameId}` from parent vs from the OOPIF's own session | parent: `{backendNodeId:9,nodeId:66}`; self: `"Frame with the given id does not belong to the target."` |
| 11 | **Cross-frame click, no transform**: predict global, dispatch in top session, read `clientX/Y` in OOPIF | predicted (468.80, 425.375) → `CROSSBTN@109,93`; exact local centre was (109.80, 93.5); control click +200/+60 → `null` |
| 12 | **Cross-frame click, child scrolled 700 + page scrolled 40** | predicted (110, 165) → `HIT@105,140` = exact local centre |
| 13 | **Cross-frame click, iframe under `scale(0.5) translate(100px,40px)`** | affine prediction (105, 416.5) → `HIT@105,140` ✅; naive prediction (157.5, 486.5) → `null` ❌ |
| 14 | `DOM.getNodeForLocation` over an OOPIF | Returned the `IFRAME` element, not the button inside |
| 15 | `DOM.getNodeForLocation` over a closed shadow host | Returned `BUTTON#cbtn` **inside** the closed root |
| 16 | `DOMDebugger.getEventListeners{objectId:<document>, depth:-1, pierce:true}` | Whole-page map incl. a listener added inside a **closed shadow root** and one inside a **same-origin iframe**; every entry carried `backendNodeId`; `window` listeners **not** included |
| 17 | nodeId/backendNodeId lifetime matrix | See §1 table — `nodeId` dies on reparent; `backendNodeId` survives same-process navigation and `getBoxModel` returns **stale geometry without erroring** |
| 18 | `CSS.getMatchedStylesForNode` on `.cta` | `Specificity{a,b,c,components}` present; `layers:["base"]/["theme"]`, `media:["(min-width: 300px)"]`, `supports:["(display: grid)"]`, `ruleTypes:["LayerRule"/"MediaRule"/"SupportsRule"]` |
| 19 | `CSS.getComputedStyleForNode` | fixture 483 props incl. `--brand`/`--pad`; **github.com `<a>`: 2,465 props, 1,984 of them `--*`, 153,971 bytes** |
| 20 | `CSS.resolveValues` (EXP) | `["var(--brand)","calc(var(--pad)*2)","1em"]` → `["rgb(192, 255, 238)","16px","13.3333px"]` |
| 21 | `CSS.getLayersForNode` / `getEnvironmentVariables` / `startRuleUsageTracking` | Layer tree with `order` ints; 15 env vars; `RuleUsage` offsets returned |
| 22 | `Page.addScriptToEvaluateOnNewDocument{worldName:"brow_world",runImmediately:true}` | World created in every frame, `isDefault:false`; helper invisible from main world and vice versa; **survived navigation** |
| 23 | Isolated-world `document.getElementById('ch').shadowRoot` | `null` — JS cannot see the closed root that CDP just returned |
| 24 | `DOM.setNodeStackTracesEnabled` + `DOM.getNodeStackTraces` | Returned `{creation:{callFrames:[{scriptId:"8",lineNumber:0,columnNumber:65}]}}` |
| 25 | `DOM.getTopLayerElements` after `dialog.showModal()` | `[::backdrop, DIALOG]` |
| 26 | `Runtime.getProperties` on a closure | `[[FunctionLocation]]`, `[[Scopes]]` → `["Closure (outer)","Script","Global"]` |
| 27 | Payload sizes on Wikipedia + github.com | See §11 tables (exact byte counts) |
| 28 | AX payload with `name.sources`/`chromeRole`/`ignoredReasons` stripped | github.com 888,206 → 440,224 bytes (≈50%) |
| 29 | `Accessibility.queryAXTree{role:"button"}` vs `{accessibleName:"Star"}` | 17 nodes / 15,523 bytes; **name query is exact-match — "Star" returned 0** |
| 30 | `Schema.getDomains` | `'Schema.getDomains' wasn't found` — removed in Chrome 151 |

---

## Limits and impossibilities — read this section twice

1. **There is no single call that returns the whole cross-origin page.** The spec's "ONE node model" is achievable as a *daemon-side* data structure, but it is assembled from N sessions × 3 calls. Any latency budget must assume `O(frames)` round-trips. **This is not fixable.**
2. **`DOMSnapshot.captureSnapshot` returns nothing for OOPIFs.** Not a bug you can flag around — it is renderer-scoped by construction. Corroborated by playwright#26856.
3. **`Accessibility.getFullAXTree` does not cross even same-origin iframes.** Measured. Per-frame calls are mandatory.
4. **`backendNodeId` is a stale-reference hazard, not a safety net.** `describeNode`/`getBoxModel`/`scrollIntoViewIfNeeded` all *succeed* on nodes from a destroyed same-process document and return old geometry. If the harness does not enforce a generation check, it *will* eventually click at coordinates from a previous page. This is the highest-severity correctness risk in this dimension.
5. **The AX tree is not the token-cheap option.** Raw, it is 2.3× the DOM snapshot on Wikipedia. Its value is semantic, not size.
6. **A 3,000-node page cannot be shown to an agent in full at any fidelity.** Even the aggressively compact interactive view was ~40K tokens on Wikipedia. Paging, scoping and query-first are architectural requirements, not polish.
7. **`CSS.getMatchedStylesForNode` is unbounded.** 1.06 MB of `inherited` for one `<a>`. Any code path that can forward it to the agent is a bug; treat it as a size-limited internal call with a hard byte cap.
8. **The cascade winner must be computed by the harness.** CDP gives all matches + specificity + layers + source ranges, but not "this rule won". Getting `!important` × cascade layers × `@scope` × shadow tree order exactly right is genuinely hard; expect to be wrong in corners and label the field `winner_confidence`.
9. **Canvas/WebGL/Flutter-without-semantics contents are unavailable.** Pixels only. No CDP domain exists.
10. **`isClickable` and per-node listener presence are both misleading** in opposite directions on framework sites. Any "is this clickable" answer is a heuristic and must be labelled as one.
11. **Perspective/3D-transformed iframes** break the two-basis-vector affine model. **CONFIRMED 2026-08-04 with an end-to-end click miss** (§5): a `perspective:400px` + `rotateY(45deg)` OOPIF produced a trapezoidal owner quad, the affine prediction was 32 px off, and the click landed on the document instead of the button. **The detector originally proposed in §5 does not catch it** — use the corrected both-axes parallelogram test. Solvable in principle with a full 4-point homography; not solvable with two basis vectors.
12. **`Accessibility.queryAXTree{accessibleName}` is exact-match.** Substring/fuzzy search must be done harness-side over a fetched tree. It also does **not** cross frames (verified: exact-name query for a button in a same-origin iframe → 0 nodes).
13. **Everything load-bearing here is EXPERIMENTAL**: `DOMSnapshot`, `Accessibility` and `CSS` are experimental *domains* (re-confirmed against Chrome 151's `/json/protocol`: 57 domains, protocol 1.3); `DOM.getFrameOwner`, `DOM.pushNodesByBackendIdsToFrontend`, `DOM.getNodeStackTraces`, `DOM.getTopLayerElements`, `DOM.getContentQuads`, `CSS.resolveValues`, `CSS.getLayersForNode`, `Specificity.components`, `CSSRule.originTreeScopeNodeId` are experimental *commands/fields*. ~~`Schema.getDomains` has already been removed.~~ **Corrected 2026-08-04: `Schema.getDomains` is NOT removed** — it is `deprecated:true` in `/json/protocol` and still works on a *page* session (returned 35 domain names at version "1.2"); it only 404s on the *browser* session, which is where the original test must have called it. It remains useless for feature detection, but do not build a "this method was removed" precedent on it. Pin the vendored protocol JSON, generate types from it, and add a startup check that diffs the live `/json/protocol` against the vendored one and warns on drift.
14. **Framework expandos are invisible from the isolated world** (§8). React/Vue adapters must run main-world, which is page-observable and page-tamperable. This is a security-model change, not an implementation detail.

---

## Open questions for the owner

1. **Snapshot cadence.** `captureSnapshot` was 211 ms for 19.7K nodes. Re-snapshot on every command, or maintain an incremental tree from `DOM.childNodeInserted`/`childNodeRemoved`/`attributeModified` + `CSS.styleSheetChanged` and re-snapshot only on `documentUpdated` / explicit `--fresh`? Incremental is ~5× the code and a whole class of drift bugs; I lean re-snapshot with a 300 ms coalescing window.
2. **Ref numbering.** `@n42` = raw `backendNodeId` (leaks a Chrome internal, collides across frames) or a daemon-allocated dense sequence (stable-looking, needs a side table, survives re-binding)? I recommend daemon-allocated, with the tuple stored internally — but that means the same element gets a *different* `@n` after a re-snapshot unless you re-key by fallback identity. Is ref stability across snapshots a requirement?
3. **Closed shadow DOM policy.** The harness can see and act inside closed shadow roots; the site author explicitly opted out of that. Is piercing closed roots always on, on under `inspect`, or gated behind an explicit `--pierce-closed` with an audit-log entry?
4. **UA shadow DOM.** Hidden by default is clearly right for the tree — but date pickers, `<video>` controls and `<select>` popups are all UA shadow DOM, and "click the video's play button" needs them. Expose a curated allowlist of UA parts, or require coordinate clicks there?
5. **Cascade-winner fidelity.** Full cascade implementation (layers + `@scope` + shadow order + `!important`) is maybe a week of careful work plus a fixture corpus. Ship a "best-effort + confidence flag" v1, or defer `css_winners` to v2 and only show matched rules ordered by specificity?
6. ~~**Framework adapters and worlds.** Reading `__reactFiber$*` expandos from the isolated world is untested (§8).~~ **ANSWERED 2026-08-04: expandos are NOT visible cross-world (verified, §8). Adapters must be main-world.** The remaining question for the owner is the policy one: do framework adapters ship enabled-by-default with a `provenance: main-world` label, or gated behind an explicit capability because they execute page-observable code in the page's own world?
7. **`--interactive` occlusion checking.** Paint-order-based occlusion detection needs a rect-overlap pass over up to 16K layout entries per snapshot. Acceptable cost, or make `!occluded` opt-in?
8. **Wikipedia-scale pages.** At ~40K tokens for the compact view, is the intended default `--scope viewport --limit 60`, or should `page tree` refuse outright above N interactive nodes and force the agent to `page find` first? The latter is more honest but more annoying.

---

## Sources

Protocol facts marked [CONFIRMED] come from the two primary artifacts below; URLs are the corroborating public sources.

1. Local Chrome 151.0.7922.72 `/json/protocol` dump (1,605,774 bytes) — retrieved 2026-08-04 from `http://127.0.0.1:39411/json/protocol`, saved to the session scratchpad as `protocol.json`. Primary source for every method/parameter/experimental flag in this document.
2. Live CDP sessions against the same binary over `--remote-debugging-pipe` — experiments 1–8, summarised in "What we verified empirically".
3. https://chromedevtools.github.io/devtools-protocol/tot/DOMSnapshot/ — `captureSnapshot` signature and `DocumentSnapshot`/`NodeTreeSnapshot`/`LayoutTreeSnapshot`/`RareStringData` type definitions; the "Shadow DOM in the returned DOM tree is flattened" wording.
4. https://chromedevtools.github.io/devtools-protocol/tot/CSS/ — CSS domain reference (`resolveValues`, `getMatchedStylesForNode`).
5. https://chromedevtools.github.io/devtools-protocol/tot/ — tip-of-tree index; note the explicit "changes frequently and may break at any time" warning that motivates pinning the vendored JSON.
6. https://github.com/microsoft/playwright/issues/26856 — "[BUG] DOMSnapshot.captureSnapshot is not working for cross origin iframes"; independent confirmation that OOPIF documents are omitted.
7. https://github.com/ChromeDevTools/chrome-devtools-mcp/issues/635 — "Reduce token usage by skipping ignored accessibility nodes in snapshot output"; the 278-of-543 ignored-node and ~1,668-wasted-token figures.
8. https://github.com/ChromeDevTools/chrome-devtools-mcp/issues/703 — "Cross-Origin IFrame Support / Frame Selection / Target.setAutoAttach" (opened 2025-12-22, closed); confirms `Target.setAutoAttach{flatten:true}` is the accepted OOPIF approach.
9. https://raw.githubusercontent.com/ChromeDevTools/chrome-devtools-mcp/main/docs/tool-reference.md — `take_snapshot` / `uid` reference scheme and the "always use the latest snapshot" invalidation guidance.
10. https://github.com/whatwg/dom/issues/1290 — "Closed Shadow DOM blocks accessibility testing"; notes closed roots are available via `DOM.getDocument` but not modifiable through CDP.
11. https://yotam.net/posts/piercing-the-shadow-root-using-cdp/ — the `pierce` argument on `DOM.getDocument` vs `DOM.querySelector`.
12. https://www.chromium.org/developers/design-documents/oop-iframes/ — OOPIF architecture; why the parent renderer has only proxy frames and cannot read child DOM.
13. https://pkg.go.dev/github.com/chromedp/cdproto/domsnapshot and https://github.com/chromedp/cdproto/blob/master/domsnapshot/domsnapshot.go — cross-check of `captureSnapshot` parameter semantics and experimental flags.
14. https://crates.io/api/v1/crates/{serde,serde_json,simd-json,tokio,slotmap,indexmap,smallvec,compact_str,rustc-hash,ahash,dashmap,parking_lot,memchr} — version numbers retrieved 2026-08-04.

---

## Verification pass — 2026-08-04 (adversarial review)

Re-run against **Google Chrome 151.0.7922.72** on macOS 26.5.1, over `--remote-debugging-pipe`, with fresh fixtures (open/closed/nested-closed shadow roots, `<input type=date>` + `<video controls>`, a same-origin iframe, a cross-origin OOPIF at `localhost` vs `127.0.0.1`, and a page with both a `rotate+scale` and a `perspective+rotateY` OOPIF). All processes killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| Framework expandos readable from the isolated world | **REFUTED** | `Object.keys(node)` = `[]` in the isolated world vs `["__myExpando","__reactFiber$xyz","__reactProps$xyz"]` in main. Adapters must be main-world (§8) |
| Affine basis handles all real-world iframes; parallelogram detector catches the rest | **PARTIAL — algorithm right, detector REFUTED** | rotate(25°)+scale(0.6): predicted (309.1,535.0) → hit `[149,99]` ✅. perspective+rotateY(45°): predicted (171.3,184.7) → `["doc",118,120]`, missed ❌, and the old detector flagged **neither** (§5) |
| `backendNodeId` survives same-process navigation, `getBoxModel` returns stale geometry silently | **CONFIRMED (deterministic)** | describeNode/getBoxModel/scrollIntoViewIfNeeded all OK on the destroyed document; box byte-identical to pre-nav; only `resolveNode` errors (§1) |
| `Accessibility.getFullAXTree` does not cross same-origin iframes | **CONFIRMED + refined** | 86 nodes, button absent; `queryAXTree` by exact name → 0. But `getFullAXTree{frameId}` from the *same* session returns it (6 nodes) (§3) |
| `DOM.getDocument{pierce:true}` sees closed shadow roots, incl. nested closed | **CONFIRMED** | `shadowRootType` `closed` on `CLOSED-HOST` **and** `INNER-CLOSED`; both button texts present in the payload (§4) |
| One `<input type=date>` + one `<video controls>` ⇒ 21 UA shadow roots; no suppression flag | **CONFIRMED exactly** | 21 `user-agent` roots; `DOM.getDocument` params are exactly `["depth","pierce"]` in Chrome 151's `/json/protocol` (§4) |
| `DOMDebugger.getEventListeners{depth:-1,pierce:true}` = whole-page map in one call | **CONFIRMED** | 4 listeners returned: delegated `document` root, one **inside a closed shadow root**, one direct, one **inside a same-origin iframe** — each with `backendNodeId` (§7) |
| `DOMSnapshot.captureSnapshot` omits OOPIFs; `isClickable` is unreliable | **CONFIRMED** | 2 documents (main + same-origin), cross-origin text absent; `isClickable` = `[#document, BUTTON, INPUT, BUTTON, A]` — flagged the document, missed the three listener-less `<button>`s, and flagged an `<input type=date>` (§2) |
| `Schema.getDomains` removed in Chrome 151 | **REFUTED** | Works on a page session (35 domains, v"1.2"); `deprecated:true`, present, in `/json/protocol`. Only the *browser* session 404s (Limits #13) |
| `DOMSnapshot.captureSnapshot` works in an OOPIF's own session | **CONFIRMED** | Returned OK on the OOPIF session; `Page.getLayoutMetrics` there gave that frame's own `cssContentSize` |

**Not re-tested:** token/byte measurements on Wikipedia and github.com, `CSS.*` surface details (`resolveValues`, `getLayersForNode`, `Specificity.components`), `DOM.getNodeStackTraces`, `Runtime.getProperties` closure walking, `simd-json` vs `serde_json` performance, snapshot timing figures.

---

## Verification pass 2 — 2026-08-04 (second adversarial review)

Chrome **151.0.7922.72**, macOS Darwin 25.5.0, over `--remote-debugging-pipe`, 1280×900 at `deviceScaleFactor:1`, against the **live** github.com and en.wikipedia.org (not fixtures). Rust benchmark built with `cargo build --release`, rustc 1.97.1. All processes killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| `simd-json` is worth taking over `serde_json` for the 4 MB snapshot | **REFUTED** | 4,362,821 B snapshot: `serde_json` 9.42–12.08 ms vs `simd-json` 9.80–11.78 ms — a wash. `serde_json` was never the bottleneck (Chrome takes 211 ms to *produce* it). Only the 9.29 MB AX tree shows a win (30 ms → 18–22 ms), and that payload is one the design says never to keep raw (§12) |
| `DOMDebugger.getEventListeners{depth:-1,pierce:true}` at framework scale — may OOM/truncate/be slow | **CONFIRMED SAFE** | github 1,007 listeners / 158,205 B / 0.01 s; Wikipedia 1,161 / 181,868 B / 0.01 s. One call, no truncation. Object-group retention remains the real cost (§7) |
| Payload byte counts / node counts on real sites | **CONFIRMED (independent replication)** | AX: 9,288,017 B / 23,573 nodes (Wikipedia), 888,159 B / 2,010 nodes (github) — node counts identical to the original run, bytes within 0.1 % (§11) |
| The 3.7 bytes/token conversion | **STILL UNVERIFIED** | No tokenizer run in either pass. Bytes are exact; token figures are a heuristic (§11) |
| `Schema.getDomains` "removed in Chrome 151" (Limits #13, already corrected once) | **REFUTED again, from `/json/protocol`** | Local dump: **57 domains**, `version {major:"1", minor:"3"}`, `Schema` present with `"deprecated": true`. Also confirms **no `Canvas` domain** exists, so Limits #9 stands |
| `DOM.getDocument` really has only `depth`/`pierce` (no UA-shadow suppression) | **CONFIRMED** | `/json/protocol` parameter list for `DOM.getDocument` is exactly `[depth, pierce]`; `Accessibility.getFullAXTree` is `[depth, frameId]` and `experimental:true`; `DOM.getContentQuads` `experimental:true` |
