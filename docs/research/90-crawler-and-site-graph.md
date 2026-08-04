# State-aware crawling and the site graph

> **Bottom line.** The academically-solved part of this problem is the *state-flow graph* (Crawljax, 2008–2012) and the *replay-from-root* navigation model (Burp Scanner). Both are ~15 years old and still correct; do not reinvent them. The genuinely hard, unsolved part is **state identity** — and the 2026 empirical literature is unambiguous that no single state-abstraction wins (Crawljax swings from 39.39% to 54.18% code coverage purely by swapping abstraction; on Dimeshift StringCmp produced 316 states where Gestalt produced 17). Therefore `brow` must ship a **composite, layered, tunable signature** (route template ⊕ AX-skeleton hash ⊕ interactive-affordance set ⊕ optional pHash) with per-crawl knobs and full provenance, not a single magic hash. Route *declaration* extraction is far more productive than blind clicking, and I verified empirically on 2026-live production sites that it works for React Router v7/v8 (`window.__reactRouterManifest`) and Vue/Nuxt (`$router.getRoutes()` → 293 routes on nuxt.com), and **does not work at all** for Next.js App Router (16.x exposes no route table), SvelteKit (only `{base, version}`) or production Angular (`window.ng` undefined at v22.1). Service-worker precache via `CacheStorage.requestCacheNames`/`requestEntries` is a goldmine *on prerendered/MPA sites* and near-worthless on true client-rendered SPAs. One architectural landmine found empirically: **framework route globals are invisible from an isolated world** — `inspect.evaluate` as specced (read-only, isolated) *cannot* read `__reactRouterManifest`. That needs a design decision before coding. And "all URLs" is undecidable: report coverage against *declared* routes with provenance, never claim exhaustiveness.

> **Verified 2026-08-04 (adversarial pass, Chrome 151.0.7922.72, live CDP).** Three headline corrections to this summary. (1) **React Router's manifest is a route *tree*, not a URL list.** On reactrouter.com the manifest held 7 entries on the landing page and 10 after navigating to a docs page carrying **351** internal links; the paths are `["","","*",null,"/:ref",null,"/brand","changelog","/color-scheme","home"]` — two splats and a `:ref` param covering thousands of URLs. Manifest entry count is therefore **not** a usable coverage denominator on any docs- or catalogue-shaped app. (2) **Service-worker precache yield is bimodal, not a general goldmine.** vite-pwa-org.netlify.app returned `returnCount: 249` (138 of the first 200 entries were `.js/.css/.wasm`); squoosh.app returned 15 entries of which exactly **one** was route-like (`/`). The originally-claimed "42 entries" no longer reproduces. (3) **The `Fetch.requestPaused` network backstop is not sound as specced** — service-worker-originated `fetch()` bypasses a page-session `Fetch.enable` entirely. See §4.3 and §13.

---

## Decisions

| # | Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|---|
| D1 | State identity = **composite tuple** `(route_template, ax_skeleton_hash, affordance_set_hash, modal_stack, auth_principal)` with a configurable **match policy** per crawl | 2026 empirical study: no abstraction dominates; abstraction choice ⇄ exploration strategy interaction is strong [1] | Single DOM string hash (Crawljax default) — produced **316** states on Dimeshift where Gestalt produced **17** [1, Table 6] | confirmed (literature) |
| D2 | **Route template induction** (`/users/123` → `/users/:id`) is a first-class primitive, applied *before* any DOM hashing | Kills the "500 rows = 500 states" explosion at the source; matches URL-normalisation-for-dedup literature [15][16] | Post-hoc state clustering — too late, you already paid the crawl cost | likely |
| D3 | State restoration = **replay action path from a known root**, not `Page.navigateToHistoryEntry` / browser back | Crawljax: "a dynamically changed DOM state does not register itself with the browser history engine … triggering the 'back' function usually does not bring us to the previous state" [2]. Burp: "reverts to the start location and navigates from there" [3] | Browser back / bfcache | confirmed (both primary sources) |
| D4 | Declared-route extraction (manifests, sitemaps, SW precache, speculation rules) runs **before** exploratory clicking and seeds the frontier | Verified: 715 sitemap URLs on nextjs.org; 42 precache entries incl. `.html` routes on a Vite PWA; 293 routes from Nuxt's router — all in seconds vs hours of clicking | Pure BFS clicking | confirmed (empirical) |
| D5 | Framework route extraction needs **main-world** evaluation; expose it as a *narrow, vetted, allow-listed* extractor capability (`inspect.routes`) whose payloads are fixed strings shipped by us, never agent-supplied | Verified: isolated world sees DOM but `window.__reactRouterManifest === undefined` | Running the extractor in the isolated world (does not work); or opening main-world `evaluate` to the agent (violates the hard constraint) | confirmed (empirical) |
| D6 | Edge statuses: `declared \| observed \| discovered \| inferred \| blocked`, stored **per edge**, plus `evidence_id` | Owner spec; also the only honest way to present a crawl | Boolean "visited" | confirmed (spec) |
| D7 | Storage = **SQLite (`rusqlite` 0.40.1)** for the graph + content-addressed blob store for evidence; `petgraph` 0.8.3 only as an in-memory analysis view | Crawls are long-running, resumable, and need queries ("all states with console errors"); artifacts are large binaries | Pure JSON file (no resume, no query), `redb`/`sled` (no SQL, weaker tooling) | likely |
| D8 | Export = **`sitegraph.json` (canonical, versioned schema)** + generated `.dot` and `.mmd` renderings | JSON is the machine contract; DOT/mermaid are throwaway views | Graphology/GEXF/GraphML as canonical | likely |
| D9 | Destructive-action guard is **3-layer**: lexical/ARIA pre-filter → capability mode gate → `Fetch.requestPaused` network-level block on non-idempotent methods, **enabled on every attached target (page + OOPIF + service worker + dedicated worker), not just the page session** | Keyword heuristics alone have a real false-negative rate (icon-only buttons, i18n, custom labels); the network layer is the *strongest* backstop but is **not sound** — see the verified caveats below | Keyword blocklist only; page-session-only `Fetch.enable` | **partial** (layer 3 measured leaky; FN rate of layer 1 still unmeasured) |
| D10 | Coverage is reported as a **matrix over declared routes × status**, plus explicit `unknown_unbounded: true` when the app has input-driven routing | "All URLs" is undecidable (Rice / halting-equivalent for input-driven routing) | A single "% coverage" number | confirmed (reasoning is sound and stated as such) |
| D11 | Re-run comparison diffs **states (added/removed/changed), edges, and per-state console/network error sets**, keyed by state signature and route template — never by state index | Crawl order is nondeterministic; indices are meaningless across runs | Diffing raw screenshots or state IDs | likely |

---

## 1. Prior art — what is already solved

### 1.1 Crawljax / Mesbah et al. (the canonical work)

I extracted the text of the TWEB journal version [2] directly and read the algorithm. The load-bearing facts:

- **State-flow graph.** Nodes = *UI states* (DOM instances), edges = *events on clickables*. This is the model, and it is the right one. The paper's `Algorithm 1` is: `crawl(currentState)` → for each candidate clickable `c`: fire event, compare resulting DOM to pre-event DOM, if changed → `getXPathExpr(c)`, `sm.addState(dom)`, `sm.addEdge(cs, ns, Event(c, xpath))`, `sm.changeToState(ns)`, recurse, `sm.changeToState(cs)`, `backtrack(cs)`.
- **State comparison (verbatim from the paper):** *"one way of comparing them is by calculating the edit distance between two DOM-trees … using the Levenshtein [1996] method. A similarity threshold τ is used under which two DOM trees are considered clones. This threshold (0.0–1.0) can be given as input. A threshold of 0 means two DOM states are seen as clones if they are exactly the same in terms of structure and content."* [2]
- **Clickable identification:** *"we use the tag name, the list of attribute names and values, and the XPath expression of each element to conduct the comparison. Additionally, a depth-level number can be defined to constrain the depth level of the recursive function."* [2]
- **Backtracking (`Algorithm 2`, verbatim-ish):** while the current state has a previous state with unexamined clickables — *if* `browser.history.canGoBack()` then `goBack()`, *else* `browser.reload()`, then `list e ← sm.getPathTo(ps)`, and for each `e`: `resolveElement(e)`, `robot.enterFormValues(re)`, `robot.fireEvent(re)`. The paper is explicit about why: *"a dynamically changed DOM state does not register itself with the browser history engine automatically, so triggering the 'back' function of the browser usually does not bring us to the previous state. Saving the whole browser state is also not feasible"* — and it notes Dijkstra shortest-path as an optimisation over the replay path. [2]
- **Scaling:** the 2012 version adds a browser pool + a dynamic partition function: *"we define work as: bringing the browser back into a given state and exploring the first unexplored candidate state from that state"* [2]. That is exactly the unit of work `brow`'s job scheduler should use.

Crawljax's weaknesses, honestly: XPath as element identity is brittle under SPA re-renders; Levenshtein over serialised DOM is O(n²) and semantically blind (a changed timestamp = a new state); it is Selenium/WebDriver-based (disqualifying for us anyway).

### 1.2 Burp Scanner's crawler (the best-engineered non-academic prior art)

PortSwigger's documented model [3] is startlingly aligned with the spec:

- *"a map of the application in the form of a directed graph, which represents the different locations in the application and the links between those locations."*
- *"identifies locations based on their contents, not the URL that it used to reach them"* — handles CSRF tokens/cache-busters in URLs, and same-URL-different-state.
- *"either navigates directly from its current location, or reverts to the start location and navigates from there. This behavior replicates the actions of a human user as closely as possible."* → **replay-from-root, deliberately, not browser back.**
- Volatile-content handling: re-identify the same location across visits despite ads/feeds/random content.
- Two-phase auth: unauthenticated crawl first (to find the login surface), then authenticated crawls per credential set.
- Budget: fingerprinting + breadth-first prioritisation of *new* content + *"configurable cutoffs that constrain the extent of the crawl."*

Steal all of this.

### 1.3 The 2026 state-abstraction study (most important recent source)

*"Understanding Automated Web GUI Testing: An Empirical Study Across Exploration Strategies and State Abstractions"* (arXiv 2606.16650) [1] crosses 6 abstractions × 5 strategies:

| Abstraction | Definition | Cost |
|---|---|---|
| StringCmp | exact HTML string equality (Crawljax default) | trivial |
| Gestalt | ratio-based sequence similarity over tag stream (WebExplor default) | cheap |
| RTED | robust tree edit distance over DOM, thresholded | expensive |
| PDiff | perceptual pixel diff of screenshots | medium (needs capture) |
| WebEmbed | unsupervised NN page embedding | needs a model |
| Judge | supervised NN classifier over page embeddings | needs a model |

Findings we must act on:
- **Model-based crawlers (like ours) want *strict, effective* abstractions.** Crawljax: Judge 54.18% vs PDiff 39.39% coverage — a 14.79 point swing from abstraction alone [1, Table 5].
- **RL-based crawlers want *compact* abstractions.** Inverted preference. So the abstraction must be a knob, not a constant.
- **State explosion is measured:** on Dimeshift, Gestalt → 17 states; StringCmp → 316 states [1, Table 6]. Over-fine abstraction makes a DFS crawler conclude it is done early (it never runs out of *new* states, so budget expires before breadth).
- **Coverage ≠ bug finding:** the paper's own framing is that *"code coverage is weakly correlated with failure-revealing ability"* [1]. Do not optimise the coverage number.

> **Corrected 2026-08-04.** Three numbers in this section were wrong or unsourced, and the correction changes the argument. (a) The state-count range "16–17 / 316–1358" is not in the paper; Table 6 gives **Gestalt 17, StringCmp 316** on Dimeshift. (b) The claim *"PDiff at 39.39% found the same 38 unique failures as Judge at 54.18%"* is **not supported** — the paper presents per-abstraction failure overlap only as Venn diagrams (Figure 3) and gives no per-abstraction unique-failure table. The conclusion survives on the paper's own weak-correlation finding; the number 38 must be struck. (c) **StringCmp scores 49.12% average coverage — second best, and ~10 points *above* PDiff.** That materially undercuts the framing of StringCmp as the naive loser: it explodes the state count *and* covers well. The real lesson is that state count and coverage are near-orthogonal, so a signature must be tuned against the *deliverable* (site map vs regression diff), not against a coverage number. Also: the study crosses 6 abstractions against **5 tools in 3 strategy categories** (model-based: Crawljax, FragGen; RL-based: WebExplor, WebRLED; LLM-based: GPTWeb), not "5 strategies". Verified by fetching https://arxiv.org/html/2606.16650 (paper exists, title matches, submitted 2026-06-15).

We cannot ship WebEmbed/Judge (they need an ML model — against the "no cloud, local artifacts" spirit, and a model dependency we don't want). Our composite signature is *intended* to approximate "strict but not brittle" without a neural net.

> **Verified 2026-08-04 — the "approximates Judge at zero model cost" claim is REFUTED as stated.** I built the L1 AX skeleton exactly as specced in §2.3 and ran it on matched fixture pairs. On **semantic markup** it is excellent: the modal state shows up as a discrete `dialog|short_text|modal|4` line, which is exactly the wanted signal. On **div soup** (identical visual content, zero ARIA roles) the same algorithm degrades to a sequence of `generic|…`, `StaticText|…`, `InlineTextBox|…` lines with **no role information whatsoever**. The two div-soup states still hashed differently — but only because the *node count* differed (34 vs 28 skeleton lines) and one text bucket flipped `short_text`→`long_text`. In other words on div soup the AX skeleton silently degenerates into a **node-count-and-depth** signature, which is the StringCmp failure mode (over-fragmentation on content churn), not the Judge behaviour. Worse: the div-soup modal was **invisible** to the skeleton — no `dialog` role, no `modal` state bit — so §4.7's `OverlayStack` detection also fails on those pages. There is no measurement anywhere showing this signature lands near Judge; treat "approximates neural abstraction quality" as an unbacked hypothesis and **ship the DOMSnapshot-based fallback skeleton in v1, not as a later addition**, selected automatically when the AX tree's non-`generic`/non-`none` role ratio falls below a threshold.

### 1.4 LLM-driven crawlers (2025–2026)

- **Go-Browse** (arXiv 2506.03533) [4]: structured BFS over discovered *pages as nodes*, with a *task proposer* + *feasibility checker* to prune redundant exploration. The useful transferable idea: separate "propose candidate interactions" from "decide whether this interaction is worth executing".
- **AutoCrawler** (arXiv 2404.12753) [5]: progressive-understanding agent that generates *crawler rules* rather than acting per page; notes stronger LLMs need fewer steps (GPT-4: 1.57 avg action sequences; Mistral-7B: 3.82).
- **Firecrawl `/map`**, **crawl4ai**, **katana**: production URL-discovery tools. Their practical contribution is the "known files" pass (robots.txt, sitemap.xml, `/.well-known/`) plus JS-literal scraping — not state modelling.

Positioning for `brow`: we are a **model-based crawler with an LLM in the action-selection seat**, not an LLM agent that happens to browse. The graph and the budget live in Rust (`browserd`); the agent gets summarised candidate sets and makes semantic choices ("this button says 'Delete workspace', skip it"; "this wizard step needs a plausible company name"). That division is the reason this project can be honest about coverage — the graph is not a hallucination surface.

---

## 2. State identity — the core problem

### 2.1 Candidate signatures, measured

I measured each candidate on live pages with Chrome 151.0.7922.72 (see §9 for method):

| Signature | CDP call | en.wikipedia.org (3043 DOM nodes) | reactrouter.com (731 nodes) | Stability | Discriminates |
|---|---|---|---|---|---|
| Interactive-affordance set (JS in page) | `Runtime.evaluate` | **3 ms**, sig ~KB | 7 ms | high | modal/drawer/tab state, enabled/disabled |
| DOM full tree | `DOM.getDocument{depth:-1,pierce:true}` | 19 ms, 1.27 MB | 5 ms, 437 KB | low (text noise) | everything, incl. noise |
| DOM snapshot minimal | `DOMSnapshot.captureSnapshot{computedStyles:[]}` | **21 ms**, 1.08 MB | 7 ms, 121 KB | medium | structure + text |
| DOM snapshot + paint + rects | `DOMSnapshot.captureSnapshot{includePaintOrder:true,includeDOMRects:true}` | 49 ms, 1.33 MB | 7 ms, 130 KB | medium | layout changes |
| Accessibility tree | `Accessibility.getFullAXTree` (EXPERIMENTAL) | **62 ms**, 2.64 MB | 2 ms, 82 KB | **high** | roles/names/states; ignores styling churn |
| Screenshot (for pHash) | `Page.captureScreenshot{format:'jpeg',quality:60}` | 15 ms, 60 KB | 13 ms, 31 KB | low-medium | visual-only changes; fooled by ads/animation |

Interpretation: **everything is cheap enough** (< 100 ms) that cost is not the deciding factor — semantic quality is. `Accessibility.getFullAXTree` is the surprise winner conceptually: it is exactly "structure + role + accessible name + state", which is what a human means by "the same screen". Its downside is that it's marked EXPERIMENTAL in the protocol (confirmed in the live `/json/protocol` dump) and it is large.

### 2.2 The recommended composite signature

```rust
/// crates/crawler/src/state_id.rs
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StateSignature {
    /// L0 — always present. Route template after induction, e.g. "/users/:id/settings".
    pub route: RouteTemplate,
    /// L1 — blake3 over the normalised AX skeleton (role|name-class|state, no text values).
    pub ax_skeleton: [u8; 16],      // truncated blake3-128
    /// L2 — blake3 over the sorted set of interactive affordances.
    pub affordances: [u8; 16],
    /// L3 — overlay/modal/drawer stack identity (dialog roles + aria-modal + top-layer).
    pub overlay: OverlayStack,
    /// L4 — who is logged in / which feature-flag cohort. Opaque label, set by the crawl config.
    pub principal: PrincipalId,
    /// L5 — OPTIONAL, off by default: 64-bit pHash of the viewport screenshot.
    pub phash: Option<u64>,
}
```

**Match policy** (the tuning knob, per crawl):

| Policy | Compares | Use when |
|---|---|---|
| `route` | L0 only | you want a page inventory, not a state graph |
| `structural` (**default**) | L0 + L1 + L3 + L4 | general SPA crawling |
| `strict` | L0 + L1 + L2 + L3 + L4 | you care about enabled/disabled affordance differences (wizards, forms) |
| `visual` | `structural` + L5 within Hamming ≤ `phash_tolerance` (default 6/64) | design-regression crawls |
| `loose` | L0 + L3 + L4 | huge catalogue sites; template-collapse everything |

### 2.3 Normalisation — the actual algorithm

The AX skeleton (L1) is computed from `Accessibility.getFullAXTree` output, walking in document order, emitting one line per node:

```
role[:subrole] | name_class | state_bits | depth
```

- **`name_class`** — never the raw accessible name. Bucket it: `empty`, `numeric`, `date`, `currency`, `email`, `url`, `uuid`, `short_text(≤3 words)`, `long_text`. This is what kills "500 rows = 500 states": each row's name differs, its class does not.
- **`state_bits`** — `checked`, `expanded`, `selected`, `disabled`, `pressed`, `invalid`, `modal`, `hidden`. These *are* semantic state and must be kept. **Filter on the property *value*, not on the property's presence** — `Accessibility.getFullAXTree` emits default/false-valued properties, so a plain `<button>` carries `{"name":"invalid","value":{"value":"false"}}` and a naive name-only filter tags every button `invalid`. (Observed directly; it produced `button|short_text|invalid|6` in my first implementation of this exact algorithm.)
- **Drop**: `ignored` nodes, **all `InlineTextBox` nodes**, `StaticText` leaves whose parent already contributes a name, live-region contents (`aria-live`), and any node under a subtree matching the configured `volatile_selectors` (ads, clocks, toasts, chat widgets).
- **Depth capping**: cap contribution at depth `d_max` (default 24) so deep virtualised lists don't dominate.

> **Verified 2026-08-04 — `InlineTextBox` must be dropped and the original spec did not say so.** `Accessibility.getFullAXTree` returns `InlineTextBox` nodes, which are **layout line boxes**: their count changes with viewport width, font loading and text wrapping. Including them makes the state signature viewport- and webfont-timing-dependent, i.e. nondeterministic across runs of the same state. Measured share of the AX tree: **89 of 415 nodes (21%) on a Wikipedia article**, 4 of 15 on a trivial fixture. On that same Wikipedia page the top roles were `none` (93), `InlineTextBox` (89), `StaticText` (76), `generic` (48), `link` (42), `listitem` (31) — so **~44% of AX nodes carry no semantic role at all**, which is the quantitative form of the div-soup degeneracy above.

The affordance set (L2): from an in-page pass over
`a[href], button, input, select, textarea, summary, [role=button], [role=link], [role=menuitem], [role=tab], [role=checkbox], [role=switch], [contenteditable], [tabindex]:not([tabindex="-1"])`, emit for each *visible, hit-testable* element:

```
tag | role | accessible_name_class | href_template | disabled | grid(x/16, y/16)
```

sorted and hashed. `href_template` is the route-template of the href, not the href. The coarse 16 px grid makes it robust to sub-pixel/responsive jitter while catching real layout state changes.

### 2.4 Route template induction (D2)

This is the single highest-leverage anti-explosion mechanism. Algorithm, in priority order:

1. **Declared templates win.** If we extracted `/docs/:version/*` from a framework manifest (§3), match the URL against those patterns first (longest-static-prefix wins). This is why §3 matters so much.
2. **Segment-wise induction** over observed URLs sharing a path prefix. A segment position is parameterised when, across ≥ `k` (default 3) sibling URLs at that position, the segment values are *high-cardinality* and match a value class:
   - numeric `^\d+$` → `:id`
   - uuid → `:uuid`
   - slug `^[a-z0-9]+(-[a-z0-9]+){1,}$` with cardinality ≥ k → `:slug`
   - date `^\d{4}-\d{2}-\d{2}$` → `:date`
   - hash-like `^[0-9a-f]{8,}$` → `:hash`
3. **Query normalisation.** Sort params; drop configured `volatile_params` (`utm_*`, `_`, `cb`, `t`, `v`, `token`, `csrf`); keep params that change the rendered state (heuristic: a param is *state-bearing* if two URLs differing only in it yield different L1 hashes — this is learned during the crawl and stored).
4. **Fragment**: keep `#/...` (hash routing) as path; drop pure anchors unless the state hash differs.

Guard rail: never parameterise a segment that appears in the declared route set as a static literal (e.g. `/users/new` must not collapse into `/users/:id`). Keep a static-literal allowlist from step 1.

### 2.5 State explosion — the mitigations, ranked

| Mitigation | Mechanism | Typical effect |
|---|---|---|
| Route templating | §2.4 | 500 rows → 1 state (biggest single win) |
| `name_class` bucketing | §2.3 | kills text-content churn |
| Volatile-subtree masking | configured selectors + auto-detected: re-hash the *same* state twice 2 s apart; any subtree that differs is volatile | kills clocks, ads, "3 minutes ago" |
| Equivalence classes on affordance sets | if two states differ only by which item in a repeated list is selected → merge into a *parameterised state* with a `variant` counter | list/detail masters |
| `list_sample_n` budget | in a detected repeated-item container, only visit the first `n` (default 2) + a random one | bounded catalogue crawls |
| Depth/breadth budgets | `max_depth` (default 6), `max_states`, `max_actions`, `max_wall_time` | hard stop |
| Fingerprint-before-act | before firing, predict the target state from `href_template`; if that template is already `saturated` (≥ `visits_per_template`, default 3), skip | avoids the "50 nav links to the same layout" trap |

The auto-volatile-detection deserves emphasis: **capture each new state's signature twice, ~1.5 s apart, before admitting it**. If they differ, diff the AX skeletons and add the differing subtrees to the volatile mask for the whole crawl. This is a 2-line idea that removes an entire class of false states, and Burp explicitly does the same thing ("It can then re-identify the same location on different visits, despite differences in response" [3]).

---

## 3. Route extraction, per framework — with empirical results

### 3.1 What I actually found on live production sites (2026-08-04, Chrome 151)

| Target | Framework/version detected | Runtime route table? | Evidence |
|---|---|---|---|
| reactrouter.com | React Router **8.0.0** (framework mode) | **YES (partial, and it is a route *tree*, not a URL list)** — `window.__reactRouterManifest.routes` = **7 route objects on the landing page, 10 after navigating into the docs**, with `{id, parentId, path, hasLoader, hasAction, module, imports, css}`; also `window.__reactRouterDataRouter` (`.routes` had length **1**), `.state.matches`, `.patchRoutes` | direct CDP `Runtime.evaluate` |
| nuxt.com | Nuxt 4 / Vue Router | **YES (complete)** — `window.useNuxtApp().$router.getRoutes()` → **293** routes | direct |
| nuxt.com (generic Vue path) | Vue 3 | **YES** — `document.querySelector('#app,[data-v-app]').__vue_app__.config.globalProperties.$router.getRoutes()` | direct |
| nextjs.org, vercel.com, tailwindcss.com | Next.js **16.3.0-canary.105 / 16.2.6**, `appDir: true` | **NO** — no `__NEXT_DATA__`, no `self.__BUILD_MANIFEST`. `window.next = {version, appDir, turbopack, router:{back,forward,prefetch,replace,push,refresh,hmrRefresh,bfcacheId}, __internal_src_page}`. Router exposes **no** route list. | direct |
| svelte.dev | SvelteKit | **NO** — `window.__sveltekit_1uabs51 = {base:"", version:"1785849980348"}` only | direct |
| angular.dev | Angular **22.1.0** | **NO** — `[ng-version]` attr present (detection ✔), but `window.ng`, `window.ngDevMode`, `window.getAllAngularRootElements` all `undefined` in prod | direct |

**This is the honest per-framework table.** Half the ecosystem does not hand you its routes.

> **Verified 2026-08-04 (independent re-run) — the Next.js / SvelteKit / Angular negatives all reproduce exactly; the React Router positive is weaker than reported.**
> Reproduced byte-for-byte: `nextjs.org` → Next.js `16.3.0-canary.105`, `appDir:true`, `window.next` top keys `["version","appDir","turbopack","router","__internal_src_page"]`, router keys `["back","forward","prefetch","replace","push","refresh","hmrRefresh","bfcacheId"]`, `__NEXT_DATA__` and `self.__BUILD_MANIFEST` both `undefined`, `__next_f` present. `svelte.dev` → `window.__sveltekit_1uabs51 = {"base":"","version":"1785849980348"}`, same global, nothing more. `angular.dev` → `ng-version="22.1.0+sha-ac3728e"`, `window.ng`/`ngDevMode`/`getAllAngularRootElements` all `undefined`. `nuxt.com` → **293** routes via both `useNuxtApp().$router.getRoutes()` and `document.querySelector('#__nuxt').__vue_app__…$router`.
> **What did not reproduce:** the React Router manifest held **7** routes, not 10, on the same landing URL, and grew to 10 only after a full page navigation into the docs. See §3.2 for what that means for coverage denominators.
> **Devtools-hook claims are worse than the table implies.** `__VUE_DEVTOOLS_GLOBAL_HOOK__` on nuxt.com is `undefined` (the dossier previously recorded `false`), and `__REACT_DEVTOOLS_GLOBAL_HOOK__` is `undefined` on both reactrouter.com and nextjs.org. Production React and Vue do **not** install a devtools hook — the *extension* installs it before page scripts run. Any framework adapter that plans to read `__REACT_DEVTOOLS_GLOBAL_HOOK__.renderers` must first inject its own hook shim via `Page.addScriptToEvaluateOnNewDocument` (main world, before first script) and then reload. On an already-loaded page it is too late. Budget for that; do not assume the hook is there.

### 3.2 Extractors, concretely

```js
// crates/crawler/src/extractors/ — each is a fixed, audited string, MAIN world, no agent input.

// react-router v7/v8 (framework mode)
(() => {
  const m = globalThis.__reactRouterManifest, dr = globalThis.__reactRouterDataRouter;
  if (!m) return null;
  const flat = Object.values(m.routes).map(r => ({id:r.id, parentId:r.parentId, path:r.path,
      hasLoader:r.hasLoader, hasAction:r.hasAction, module:r.module}));
  return {kind:'react-router', version: globalThis.__reactRouterVersion,
          discovery: (globalThis.__reactRouterContext||{}).routeDiscovery,  // {mode:'lazy', manifestPath:'/__manifest'}
          routes: flat, complete: ((globalThis.__reactRouterContext||{}).routeDiscovery||{}).mode !== 'lazy'};
})()

// vue-router / nuxt
(() => {
  let r = null;
  try { r = globalThis.useNuxtApp().$router; } catch {}
  if (!r) { const el = document.querySelector('#__nuxt,#app,[data-v-app]');
            try { r = el.__vue_app__.config.globalProperties.$router; } catch {} }
  if (!r || !r.getRoutes) return null;
  return {kind:'vue-router', complete:true,
          routes: r.getRoutes().map(x => ({path:x.path, name:x.name && String(x.name),
                                           meta:x.meta, children:(x.children||[]).length}))};
})()

// angular (dev builds only — prod returns null)
(() => {
  const el = document.querySelector('[ng-version]'); if (!el) return null;
  const v = el.getAttribute('ng-version');
  if (!globalThis.ng || !globalThis.ng.getInjector) return {kind:'angular', version:v, routes:null, complete:false};
  try { const R = globalThis.ng.getInjector(el).get(globalThis.ng.coreTokens?.Router ?? 'Router');
        const walk = (rs, p='') => rs.flatMap(r => { const full = p + '/' + (r.path ?? '');
          return [{path: full.replace(/\/+/g,'/')}, ...walk(r.children||[], full)]; });
        return {kind:'angular', version:v, routes: walk(R.config), complete:true};
  } catch { return {kind:'angular', version:v, routes:null, complete:false}; }
})()
```

Next.js App Router and SvelteKit have **no runtime path**. Fall back to:

| Fallback | How | Yield |
|---|---|---|
| **React Router fog-of-war expansion** | `routeDiscovery.mode === 'lazy'` is the **documented default** [6]. Manifest is partial and grows. Use (a) eager discovery — RR auto-discovers every `<Link>`/`<NavLink>` on the current page via a batched request [6] — and re-read the manifest after each state. The direct-fetch contract is **`GET /__manifest?paths=<comma-joined,URL-encoded>&version=<build-hash>`** → `200 application/json`. | **grows, but NOT toward complete — see below** |
| **Next.js App Router** | Server-side only artifacts (`.next/app-build-manifest.json`, `.next/routes-manifest.json`) — only if we have the repo. From the browser: parse `__next_f` RSC flight chunks for `"href"` literals; regex chunks for path literals; sitemap. | partial |
| **Next.js Pages Router (legacy)** | `window.__NEXT_DATA__.{buildId,page}` then `GET /_next/static/<buildId>/_buildManifest.js` → `self.__BUILD_MANIFEST` keys **are** the full route list incl. `[param]` templates. Still valid in 2026 for pages-router apps but I found **no** live pages-router site among 3 major Next.js properties — treat as a legacy path. | complete when present |
| **SvelteKit** | Routes live in the client manifest chunk emitted by `@sveltejs/kit` (`app/immutable/entry/app.*.js`, an array of `() => import(...)` per route with `routes:` regexes). Regex the entry chunk for `/^\/...\/?$/i` route regexes. Fragile across Kit versions. | partial |
| **Sourcemaps** | `//# sourceMappingURL=` in each chunk → fetch `.map` → `sourcemap` crate **9.3.2** → `sources` array often contains `app/(marketing)/pricing/page.tsx`, `src/routes/settings/+page.svelte` → **file-system routes reconstructable**. Verified negative on reactrouter.com (`.map` → HTTP 404); production sites frequently strip them. | jackpot when present |
| **Bundle literal regex** | over every JS response body (`Network.getResponseBody`): `/(?:["'\`])(\/(?:[A-Za-z0-9_\-~.]+|\[[^\]]+\]|:[A-Za-z0-9_]+)(?:\/(?:[A-Za-z0-9_\-~.]+|\[[^\]]+\]|:[A-Za-z0-9_]+))*)\/?(?:["'\`])/g`, then filter: must start `/`, length 2..120, not a file extension in `{js,css,png,svg,woff2,map,json}` unless `json` under `/api/`, not a MIME type, not a regex source. Noisy — mark results `inferred`, never `declared`. | high recall, low precision |

> **Corrected 2026-08-04 — the `/__manifest` HTTP 400 was a wrong query contract, and fixing it makes the fog-of-war problem *worse*, not better.**
> The parameter is **`paths=` (plural, comma-joined, URL-encoded), not `p=`**, and `version=` is the **build hash**, not the React Router semver. Sniffing `Network.requestWillBeSent` on reactrouter.com captured RR's own calls verbatim:
> ```
> GET /__manifest?paths=%2Fhome%2C%2Fupgrading%2C%2Fupgrading%2Fcomponent-routes%2C%2Fupgrading%2Fv7&version=e32f06ee   -> 200 application/json
> GET /__manifest?paths=%2F6.30.4%2C%2F7.18.2%2C%2F8.3.0%2C%2Fapi%2C%2Fapi%2Fcomponents%2C%2Fapi%2Fcomponents%2FAwait%2C…  -> 200 application/json
> GET /__manifest?paths=%2Fapi%2C%2Fapi%2Fframework-conventions%2C%2Fapi%2Fframework-conventions%2Froutes&version=e32f06ee -> 200 application/json
> ```
> With `p=` (the old guess) the server answers **204 No Content** for every path I tried, with or without `version` — so the previously recorded "HTTP 400" no longer reproduces either.
> **Eager `<Link>` discovery is confirmed end-to-end and is still not enough.** Landing page: 7 manifest routes, 4 internal links. After navigating to `/start/framework/installation`: **351 internal links**, 3 batched `__manifest` round-trips enumerating ~30 concrete paths — and the manifest grew only **7 → 10**. A subsequent client-side `dataRouter.navigate()` added **zero**. The reason is structural: the manifest stores *route definitions*, and this app routes everything through `/:ref` plus two `*` splats. **Conclusion: `__reactRouterManifest.routes.length` is not a coverage denominator and must never be printed as one.**
> **Actionable replacement, and it is better than the manifest:** intercept `Network.requestWillBeSent` for `__manifest?paths=`, URL-decode the `paths` parameter and split on `,`. That yields the **concrete link targets React Router itself discovered on the current page** — a real, app-declared URL list with a natural provenance (`source: "react_router_link_discovery"`). Prefer it over reading the manifest object. It also works from an isolated world / without any main-world evaluation at all, which sidesteps §3.4 entirely for React Router.

### 3.3 Framework-independent declared-route sources (do these first)

| Source | Retrieval | Verified |
|---|---|---|
| `sitemap.xml` / sitemap index | plain HTTP; `quick-xml` **0.41.0** (the `sitemap` crate is dead — last release 2020) or `sitemap-rs` **0.4.0** | ✔ `nextjs.org/sitemap.xml` → 100 KB, **715 `<loc>` entries**, not an index |
| `robots.txt` (`Sitemap:` lines, `Disallow:` = negative evidence) | plain HTTP; parse with `texting_robots` 0.2.2 (stale but fine) | ✔ `nextjs.org/robots.txt` → `Sitemap: https://nextjs.org/sitemap.xml` |
| **SPA catch-all trap** | ⚠️ A framework-mode SPA serves its HTML shell for paths that do not exist. You *must* validate by `Content-Type` **and** content sniff (`starts_with("<!DOCTYPE")` → reject); do not trust the status code **in either direction**. | ⚠️ **re-measured, see below** |
| **Service-worker precache** | `ServiceWorker.enable` → wait for `ServiceWorker.workerVersionUpdated` (status `activated`) → `CacheStorage.requestCacheNames{securityOrigin \| storageKey \| storageBucket}` → `CacheStorage.requestEntries{cacheId, skipCount, pageSize, pathFilter}` — **paginate on the returned `returnCount`**, a single call is capped by `pageSize`. | ⚠️ **bimodal, see below** — great on prerendered/MPA, near-zero on client-rendered SPA. Strip `?__WB_REVISION__=` before templating. |
| **Web App Manifest** | `Page.getAppManifest` → `manifest: WebAppManifest` with **`startUrl`** (camelCase in CDP, not the spec's `start_url`), `scope`, `scopeExtensions`, `shortcuts`. Note the `manifest` return field is **EXPERIMENTAL** and the older `parsed` field is **DEPRECATED**. | protocol-confirmed (Chrome 151 `/json/protocol`) |
| **Speculation Rules** | `Preload.enable` → `Preload.ruleSetUpdated{ruleSet:{id, loaderId, sourceText, backendNodeId?, url?, requestId?, errorType?, errorMessage?, tag?}}` — `sourceText` is the raw JSON with `urls: [...]` or `where: {href_matches: "/articles/*"}`. **Also subscribe to `Preload.preloadingAttemptSourcesUpdated`** — it gives Chrome's *resolved concrete URLs* plus the triggering `nodeIds`, which is strictly more useful. | ✔ mechanism verified on a local fixture; ⚠️ **zero yield on 2 live sites** — see below |

> **Verified 2026-08-04 — three corrections and one addition in this table.**
>
> **(1) The `reactrouter.com` robots/sitemap evidence no longer reproduces.** Both `https://reactrouter.com/robots.txt` and `/sitemap.xml` now return **HTTP 404** with `Content-Type: text/html` and a 6630-byte `<!DOCTYPE html>` SPA shell — not HTTP 200. The *rule* (content-sniff, don't trust the status code) is still right and still necessary, but the specific "HTTP 200 shell" observation is stale. Write the sniffer to reject an HTML body regardless of status, and to accept a 404 as a genuine negative only when the body is not HTML. Control: `nextjs.org/robots.txt` → `Sitemap: https://nextjs.org/sitemap.xml`; `nextjs.org/sitemap.xml` → 200, `application/xml`, 100,117 bytes, **715 `<loc>`**, flat (no `sitemapindex`) — that one reproduces exactly.
>
> **(2) Service-worker precache yield is bimodal and the "42 entries" figure is stale.** vite-pwa-org.netlify.app: cache `workbox-precache-v2-https://vite-pwa-org.netlify.app/`, **`returnCount: 249`** (I retrieved 200 with `pageSize:200`; 138 of those were `.js`/`.css`/`.wasm`, ~48 route-like incl. `/assets-generator/{api,cli,index,integrations,migrations}.html`). squoosh.app: cache `static-fef4647d6f2b904b3f63250d079f6dfd4f008d0c`, 15 entries, of which **exactly one** is route-like (`https://squoosh.app/`) — 5 are `.js/.css/.wasm`. This is the flagged risk confirmed with numbers: **a true client-rendered SPA precaches chunks and one shell, and yields essentially no routes.** The site graph must record `sw_precache` yield as `{entries, route_like}` so the coverage report can say "SW precache contributed 1 route" rather than implying a rich source.
>
> **(3) Speculation rules: mechanism confirmed, wild yield unproven.** On a local fixture serving `<script type="speculationrules">{"prerender":[{"where":{"href_matches":"/articles/*"},…}],"prefetch":[{"urls":["/next.html","/other.html"],…}]}</script>`, `Preload.ruleSetUpdated` fired once with the exact `sourceText` — so `href_matches` really is readable. **But `Preload.enable` on `developer.chrome.com/docs/web-platform/prerender-pages` and `wikipedia.org` produced zero `ruleSetUpdated` events.** Do not budget this as a meaningful route source; treat it as a cheap opportunistic extra.
>
> **(4) Better event, missed by the original table.** The same fixture run produced `Preload.preloadingAttemptSourcesUpdated` with `preloadingAttemptSources: [{key:{loaderId, action:"Prerender", url:"http://…/articles/one.html"}, ruleSetIds:["26871.0"], nodeIds:[3]}, …]` and `Preload.prefetchStatusUpdated{key:{action:"Prefetch", url:"…/next.html"}, prefetchUrl, initiatingFrameId, status:"Running"}`. That is a **resolved URL + the triggering DOM node + the rule that produced it** — i.e. a complete site-graph edge with provenance, handed over for free. Prefer it to parsing `sourceText`.
>
> **(5) Side effect worth flagging for the read-only posture:** a prerender/prefetch rule causes Chrome to **fetch those URLs from the server** without any crawler action, and (see §4.8) that traffic did not surface as an attachable target in my run. A "read-only" crawl therefore still generates unrequested server hits.
| **OpenAPI / GraphQL** | probe `/openapi.json`, `/swagger.json`, `/v3/api-docs`, `/.well-known/openapi`; GraphQL: POST introspection query to endpoints observed in `Network.requestWillBeSent` with `content-type: application/json` and a `query` body field | these are *API* deps, edges of kind `api`, not routes |
| **`Page.getResourceTree`** (EXPERIMENTAL) | enumerates every loaded resource per frame — cheap inventory for the bundle-regex pass | protocol-confirmed |

### 3.4 ⚠️ The isolated-world problem (highest-value finding for the architecture team)

The spec says `inspect.evaluate` is *read-only, isolated world*. I verified on reactrouter.com:

```
ISOLATED WORLD: {"rrManifest":"undefined","nextV":"undefined","domNodes":731,"loc":"/home"}
MAIN WORLD    : {"rrManifest":"object",   "nextV":"undefined","domNodes":731}
```

> **Verified 2026-08-04 — CONFIRMED, and it reproduces on all five frameworks.** Using `Page.createIsolatedWorld{frameId, worldName}` then `Runtime.evaluate{contextId}`, the DOM node counts are **identical** between worlds (reactrouter 118/118, nuxt.com 1945/1945, nextjs.org 2378/2378, svelte.dev 396/396, angular.dev 477/477) while **every** framework global reads `undefined` in the isolated world: `__reactRouterManifest`, `__reactRouterDataRouter`, `window.next`, `__sveltekit_1uabs51`, `useNuxtApp`, and `__vue_app__` on the root element. Note the one thing that *does* survive: `angular.dev`'s `[ng-version]` **attribute** was readable from the isolated world (it is DOM, not JS). So framework *detection* by DOM attribute works isolated; framework *route extraction* does not.

Isolated worlds share the DOM but **not** the JS global object. So **every framework route extractor in §3.2 fails from an isolated world.** Options:

1. **`inspect.routes` as a distinct, non-parameterised capability** (recommended): `browserd` owns a fixed set of audited extractor strings; the agent can invoke `inspect.routes` but can never supply code. Main-world execution, `throwOnSideEffect: true` where possible, `returnByValue: true`, `timeout: 2000`. The agent-facing risk is the *result*, not the code — and the result is a route list.

> **Verified 2026-08-04 — `throwOnSideEffect` works better than expected, but every knob in this recommendation is an EXPERIMENTAL protocol parameter.** Empirically, `Runtime.evaluate{throwOnSideEffect:true}` did **not** throw on `Object.keys(window.__reactRouterManifest.routes).map(x=>x).length` (returned 7) or on `document.querySelectorAll('a[href]').length` (returned 9/113/99/119/40 across the five sites) — V8's side-effect-free evaluation tolerates array iteration and DOM queries. Good news for option 1. **But** in Chrome 151's own `/json/protocol`, `Runtime.evaluate`'s `throwOnSideEffect`, `timeout`, `uniqueContextId` and `serializationOptions` are all marked `experimental: true`; `Page.addScriptToEvaluateOnNewDocument`'s `worldName`, `runImmediately` and `includeCommandLineAPI` (option 2) are **all three** experimental; `Runtime.addBinding`'s `executionContextId` is **deprecated *and* experimental** (use `executionContextName`, which is neither); and the `Runtime.bindingCalled` **event** that options 2 and 3 both depend on is itself **EXPERIMENTAL**. All three options are built on experimental surface. Pin each with a fixture test that fails loudly on protocol drift.
2. Inject at document start via `Page.addScriptToEvaluateOnNewDocument{source, runImmediately:true}` **without** `worldName` (→ main world) to snapshot globals into a DOM-visible channel, then read from the isolated world. More moving parts, same trust boundary, but it makes the main-world code run *once per document* under our control rather than on demand.
3. Use `Runtime.addBinding{name:"__brow_emit", executionContextName:"..."}` + `Runtime.bindingCalled{name,payload,executionContextId}` as the exfil channel for main-world hooks (also the right mechanism for `history.pushState` interception, §4.6).

Either way: **`inspect.evaluate` staying isolated is fine, but route extraction is a separate, privileged, fixed-payload path.** Please decide before crates/inspection is written.

---

## 4. Crawl execution

### 4.1 Frontier

```rust
struct FrontierItem {
    state_id: StateId,          // state this action departs from
    action: CandidateAction,    // what to do
    priority: f32,              // see below
    depth: u16,
    attempts: u8,
}
```

Priority (higher first), a weighted sum — all weights configurable:

```
p =  3.0 * is_declared_but_unobserved_route      // close the coverage gap first
  +  2.0 * leads_to_unseen_route_template
  +  1.5 * is_nav_landmark(role=navigation|menubar)
  +  1.0 * (1 / (1 + template_visit_count))
  -  2.0 * depth_penalty(depth)
  -  4.0 * destructive_score                     // see 4.3
  -  1.0 * requires_form_fill
  - 10.0 * is_external_origin (unless in scope)
```

Ordering is **breadth-first by default** (Burp's choice [3], and it avoids the DFS trap where a fine-grained abstraction makes you tunnel forever). Crawljax defaults to DFS [2]; make it a flag `--strategy bfs|dfs|priority`.

### 4.2 Action selection — "which of 200 clickable things?"

Pipeline per state:

1. **Enumerate** affordances (§2.3 selector set), in-viewport-or-scrollable, hit-testable (`document.elementFromPoint` at the centre returns self or a descendant — otherwise it's occluded and clicking it is a lie).
2. **Deduplicate by affordance identity** `(role, name_class, href_template, container_signature)`. 200 → typically 20–40.
3. **Collapse repeated containers.** Detect them structurally: a parent with ≥ `k` children whose AX skeletons are identical modulo `name_class`. Keep `list_sample_n` (default 2) + 1 random.
4. **Predict-and-skip.** If `href_template` is already saturated, drop.
5. **Classify** into `{navigation, disclosure (expand/tab/accordion), form, destructive, unknown}`.
6. **Hand the residual to the agent** as a compact list (≤ 40 items with `@node-ref`, role, name, predicted class, template) and let it choose/rank. This is where the LLM earns its keep — semantic judgement about which of 30 buttons matters, and which "Remove" is destructive.

Element identity across replays must **not** be XPath (Crawljax's weak point). Use a **stability-ranked locator chain**, recorded per edge and tried in order:
`data-testid` → `id` (if non-generated: no digits-suffix, no `:r0:`-style React ids) → `(role, accessible_name, nth-of-role-in-container)` → `container_path + text` → CSS path → absolute XPath (last resort). Record which link in the chain succeeded; a drop to a weaker link across runs is itself a reportable signal.

### 4.3 Avoiding destructive actions

**Layer 1 — lexical/ARIA pre-filter** (cheap, high recall, imperfect precision):

```
destructive_lexemes = delete|remove|destroy|purge|erase|drop|
  purchase|buy|checkout|pay|order|subscribe|upgrade|downgrade|cancel subscription|
  send|publish|post|submit for review|share|invite|
  deactivate|suspend|ban|revoke|reset|wipe|
  transfer|withdraw|refund|
  confirm|yes, .*|i understand
```
plus signals: `aria-label` matching the above; `class` containing `danger|destructive|btn-red`; computed colour in a red hue with a filled background; being inside `[role=alertdialog]`; `form[method=post]` with a matching submit; `<a>` with `data-method="delete"`.

**False negatives are real and unquantified** — icon-only buttons (a trash SVG with no label), non-English UIs, custom vocabulary ("Retire this workspace"), and a two-step flow where step 1 is innocuous. Do not pretend this layer is sound. Mitigations: run the accessible-name computation (not `textContent`) so icon buttons with `aria-label` are caught; add an icon-shape heuristic (`<svg>` whose `<title>`/`href`/class mentions trash/bin/x); let the agent veto/confirm.

**Layer 2 — capability gate.** Modes `observe|interact|inspect` never fire an action classified destructive. `mutate`/`control` park the job in `waiting_for_approval` with the trigger node, its screenshot, and the predicted network request.

**Layer 3 — network backstop (the strongest layer, but NOT sound).** `Fetch.enable{patterns:[{urlPattern:"*", requestStage:"Request"}]}` → on `Fetch.requestPaused`, if `request.method ∈ {POST, PUT, PATCH, DELETE}` and the URL is not on the crawl's `allow_mutations` list → `Fetch.failRequest{requestId, errorReason:"BlockedByClient"}` (`BlockedByClient` confirmed present in the `Network.ErrorReason` enum) and record an edge with status `blocked` + reason.

> **Verified 2026-08-04 — this layer was described as "the only sound one" and "the guarantee". That is REFUTED. It leaks, and one of the leaks is large.** I ran a local fixture with `Fetch.enable{urlPattern:"*", requestStage:"Request"}` on the page session and a real HTTP server recording ground truth.
>
> | Mutation path | `Fetch.requestPaused`? | `resourceType` | Blocked? | Reached server? |
> |---|---|---|---|---|
> | `fetch('/mutate/…', {method:'POST'})` | ✅ yes | `XHR` | ✅ | ❌ no |
> | `XMLHttpRequest` POST (sync) | ✅ yes | `XHR` | ✅ | ❌ no |
> | `navigator.sendBeacon('/mutate/…')` | ✅ yes | **`Ping`** | ✅ | ❌ no |
> | `<img src="/mutate/get-delete?id=1">` | ✅ yes | `Image` | ❌ (method is GET) | ✅ **yes** |
> | `WebSocket('ws://…')` | ❌ **no event at all** | — | ❌ | handshake left the browser |
> | `localStorage.clear()` | n/a | — | ❌ | n/a — **succeeded** |
> | `indexedDB.deleteDatabase()` | n/a | — | ❌ | n/a — **succeeded** |
> | **`fetch()` from inside a service worker** | ❌ **no event at all** | — | ❌ | ✅ **POST and GET both returned 200** |
>
> **Good news vs. the flagged risk:** `navigator.sendBeacon` **is** interceptable — it surfaces as `resourceType: "Ping"` and `failRequest` works. (Note the JS API still returns `true`, so the page believes the beacon was queued; that is fine for us, but it means you cannot detect blocking from page-side signals.)
>
> **The serious hole:** a page-session `Fetch.enable` **does not see service-worker-originated requests at all.** In my run `fetch('/mutate/sw-post', {method:'POST'})` executed inside an active service worker produced **zero** `Fetch.requestPaused` events and hit the server with a 200. Any PWA that routes writes through a Workbox `NetworkOnly`/background-sync handler will silently mutate through a "read-only" crawl. **Fix, verified working:** call `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}` at the *browser* session and call `Fetch.enable` **on every attached session**, including `type:"service_worker"` and `type:"worker"`. Once I enabled Fetch on the SW session (`sid=551AF5CF3324`, `url=http://…/sw.js`) both `/mutate/sw-post` and `/mutate/sw-get` paused correctly. This is a hard requirement on `crates/policy`, not an optimisation. `waitForDebuggerOnStart:true` matters: without it a SW can issue requests before you get `Fetch.enable` in.
>
> **Remaining unsound-by-construction:** mutating GETs (confirmed: `/mutate/get-delete?id=1` sailed through and reached the server), WebSocket-carried mutations (no `Fetch` event fired for the handshake; `Network.webSocketWillSendHandshakeRequest` is the only observation point and there is **no** CDP primitive to block a WebSocket frame), client-only destruction (`localStorage.clear()` and `indexedDB.deleteDatabase()` both succeeded), already-registered Background Sync, and prerender/prefetch traffic (§3.3 note 5). The honest posture: **`observe`/`inspect` modes must be presented as "best-effort read-only", never as a guarantee**, and the crawl artifact should carry a `mutation_containment: {sw_sessions_covered: N, mutating_get_requests_allowed: M, websocket_connections: K}` block so the user can see exactly how leaky the run was.

### 4.4 Form filling

- **Field typing** from `type`, `inputmode`, `autocomplete` (the strongest signal — `autocomplete="email|tel|cc-number|postal-code|given-name"` is a machine-readable schema), `pattern`, `name`/`id` lexemes, `aria-describedby` text, `<datalist>`, `min`/`max`/`step`, `maxlength`.
- **Value generation** from a local, deterministic fixture pack keyed by field class (seeded from the crawl id → reproducible). Ship `fixtures/form-values.toml`. Never generate real-looking payment data; use the documented test-card placeholders only when the user opts in.
- **Validation loop**: fill → blur → wait `networkAlmostIdle` (via `Page.lifecycleEvent`) → collect `[aria-invalid=true]`, `:invalid`, `[role=alert]`, and any text node newly appearing near the field → re-generate with the constraint applied → retry ≤ 3 times → if still invalid, record edge `blocked{reason:"validation", messages:[...]}`. Do **not** loop forever; this is the classic crawler hang.
- **Required-field discovery**: `required`, `aria-required`, and empirically — submit an empty form once, harvest the error messages, which usually enumerate every required field in one shot.
- **File inputs**: `Page.setInterceptFileChooserDialog{enabled:true}` + `Page.fileChooserOpened` → park as `waiting_for_approval` (spec requirement) rather than auto-supplying a file.

### 4.5 Login

Three modes, config-selected:

| Mode | Mechanism |
|---|---|
| `vault` | credentials from a local secret store (macOS: a file in the daemon's data dir with `0600`, or Keychain via `security` — note Keychain prompts are explicitly out of scope for automation, so a plain encrypted file is the pragmatic default). Injected via real `Input.dispatchKeyEvent` typing, never `value =`. |
| `session_import` | import cookies/storage from a prior recorded session (`Storage.setCookies`) — **spec says importing cookies is an approval-gated action** |
| `handoff` | park job `waiting_for_approval`, present a live view, human logs in, resume |

Two-phase crawl (Burp's model [3]): crawl unauthenticated first → the login form is itself a discovered node and the auth wall is an `auth_branch` edge → then crawl per principal. **`principal` is part of the state signature (L4)**, so `/dashboard` as `alice` and as `bob` are different states, which is correct and is how you find authorization bugs.

### 4.6 Back-navigation and state restoration

**Do not trust browser back.** Verified: after `__reactRouterDataRouter.navigate('/brand')`, `Page.getNavigationHistory` grew to 3 entries and the URL updated — so `navigateToHistoryEntry` *would* restore the URL. But that is exactly the trap: the URL restores, the *in-memory component state* (open drawer, wizard step, filter chips, scroll position, fetched-and-mutated store) does not. Crawljax says it plainly [2]; Burp deliberately replays from root [3].

Policy:

```
restore(target_state):
  if target_state.is_url_addressable and target_state.restorable_by_url:
      Page.navigate(target_state.url); verify signature; if match -> done   # cheap path
  path = graph.shortest_path(root, target_state)      # Dijkstra, edge weight = measured ms
  Page.navigate(root.url)
  for edge in path:
      resolve(edge.locator_chain); fill(edge.form_values); dispatch(edge.input_event)
      wait_settled(); if signature != edge.to.signature: mark edge non_deterministic; abort
```

`restorable_by_url` is *learned*: the first time we reach a state we also try reaching it by direct `Page.navigate(url)`; if the signature matches, flag it and never replay again. This converts most nav-link states to the cheap path and reserves replay for modal/wizard/filter states.

**Cost, honestly.** With average path length `L` and per-action settle time `t` (~0.5–2 s), restoring costs `L·t`. A 6-deep crawl with 300 states does O(300·6·1s) ≈ 30 min of pure backtracking. Mitigations: (a) BFS with a *state-major* work queue — drain all actions of the current state before moving (Crawljax's "work" unit [2]); (b) parallel tabs/contexts, one per frontier branch, each with its own `Target.createBrowserContext` when the principal differs; (c) cache `restorable_by_url`.

> **Corrected 2026-08-04 — the arithmetic is right but the model omits three mandatory costs, so the estimate is low by roughly 2–4×.** `300 × 6 × 1 s = 1800 s = 30 min` checks out, but each restore also pays: (i) a **full document load of the root** (`Page.navigate(root.url)` — 1–3 s on a real SSR app, and it is *not* part of `L·t`); (ii) the **double-signature volatility check** this document itself mandates in §2.5 ("capture each new state's signature twice, ~1.5 s apart, before admitting it") — that is a floor of +1.5 s per *new* state; (iii) signature computation, which I measured at 11–62 ms for the AX tree alone but which is per-action, not per-state, when you verify each replayed edge as the pseudocode does. A more defensible envelope is `states × (root_load + L·(t + settle) + 1.5 s)` ≈ `300 × (2 + 6×1.5 + 1.5)` ≈ **62 min**, and that still assumes no rate limiting, no single-use tokens, no cold caches, and a zero replay-divergence rate. **Divergence rate is completely unmeasured** — the pseudocode aborts the path on signature mismatch, so a 10% per-edge divergence rate on a 6-deep path fails ~47% of restores and the crawl never finishes. Before writing `crates/crawler`, build the cheapest possible instrument: replay 50 known paths on one real app and report the per-edge divergence rate. That single number decides whether replay-from-root is viable or whether the product has to be scoped to `restorable_by_url` states only. **This remains the largest schedule risk in the crawler and it is still unquantified.**

### 4.7 URL-less states (modals, drawers, wizards, tabs)

These are first-class nodes with `url = <the URL of the underlying document>` and a non-empty `overlay` component (L3). Detection:
- `[role=dialog], [role=alertdialog], [aria-modal=true]`, `<dialog open>`, and **top-layer** membership (a `::backdrop` present, or an element whose paint order from `DOMSnapshot.captureSnapshot{includePaintOrder:true}` exceeds all siblings' while `position:fixed` covering ≥ 40% of the viewport).
- Wizards: a `[role=tablist]`/step indicator whose `aria-current`/`aria-selected` moved, or a heading matching `Step \d+ of \d+`.
- Tabs: `[role=tab][aria-selected=true]` set.

`OverlayStack` = ordered vector of `(role, accessible_name_class, depth_in_top_layer)`. This is what makes "the delete-confirm dialog" a distinct state from the page behind it, which is exactly what you need for the approval workflow.

### 4.8 New windows and tabs

`Page.windowOpen{url, windowName, windowFeatures, userGesture}` fires on the opener (verified: `{"url":"https://example.com/","windowName":"_blank","windowFeatures":["menubar","toolbar","status","scrollbars","resizable"],"userGesture":false}`). Pair it with `Target.setDiscoverTargets{discover:true}` → `Target.targetCreated{targetInfo{targetId,type,url,browserContextId}}`, or better `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}` so the new target is paused before it runs script. Model it as an edge `kind: "new_window"` to a node in a different `frame_context`. **Filter target noise**: my run surfaced `type: "background_page"` (Chrome Web Store Payments), `type: "browser_ui"` (Omnibox Popup) and extension `service_worker` targets — only `type == "page"` with a matching `browserContextId` is ours.

> **Verified 2026-08-04 — extension noise CONFIRMED and larger than described; the prerender-target worry is REFUTED, and replaced by a worse one.**
> **Noise:** in a *fresh* headless profile with no user extensions, `Target.setAutoAttach{autoAttach:true, flatten:true}` at the browser session attached **9 targets**, of which 5 were bundled component extensions: `background_page` `chrome-extension://nkeimhogjdpnpccoofpliimaahmaaome/background.html` and `chrome-extension://nmmhkkegccagdldgiimedpiccmgmieda/_generated_background_page.html`, plus `service_worker` targets for `chrome-extension://fignfifoniblkonapihmkfakmlgkbkcf/service_worker.js` and `chrome-extension://ghbmnnjooekpmoecnnnilnnbdlolhkhi/service_worker_bin`. The `type == "page"` + own `browserContextId` filter handles these. **But note the tension with §4.3:** the fix for the service-worker mutation hole *requires* auto-attaching service-worker targets, so the filter must be `(type=="page" && own context)` for **graph nodes** and a different, wider predicate for **policy enforcement**. Do not use one filter for both.
> **Prerender:** on a fixture whose speculation rules prerendered `/articles/one.html`, `Target.setDiscoverTargets{discover:true}` reported **no prerender target at all** — every `TargetInfo.subtype` was `null`, and the prerendered document never appeared. So the flagged risk "prerender targets will appear and be indistinguishable from a new window" did not materialise. **The real problem is the opposite:** the prerendered page loads, runs script and issues network requests entirely **outside** any session the crawler holds — invisible to the site graph *and* to the §4.3 mutation backstop.
> **Also observed:** `window.open('/spec/other.html','_blank')` fired `Page.windowOpen` reliably but produced **no** `Target.targetCreated` within a 4 s window under `--headless=new`. Treat `Page.windowOpen` as the authoritative trigger signal and `Target.targetCreated` as best-effort; do not block the crawl waiting for the target event.

### 4.9 Infinite scroll

Treat as a *self-edge with a saturation counter*, not as new states:
- Detect: after `Input.dispatchMouseEvent{type:"mouseWheel"}` to the bottom, `scrollHeight` grew AND a `Network.requestWillBeSent` fired with `documentURL` unchanged.
- Bound with `max_scroll_iterations` (default 5) and `max_items_harvested` (default 100). Record `edge{kind:"scroll_append", iterations, items_added, exhausted:bool}` — `exhausted:false` is a *coverage* fact and must appear in the report.
- Virtualised lists (react-window/tanstack-virtual): DOM node count stays flat while content changes → detect via `scrollTop` change with constant child count, and record `virtualised: true` so the state hash isn't recomputed per scroll.

---

## 5. Edge evidence and the storage schema

### 5.1 Evidence captured per edge

| Field | Source |
|---|---|
| `trigger` | `{locator_chain, node_ref:"@node-42", doc_generation, role, accessible_name, bounding_box}` |
| `input` | the actual dispatched event(s): `Input.dispatchMouseEvent{type,x,y,button,clickCount,modifiers}` / `Input.dispatchKeyEvent` / touch sequence |
| `screenshot_before` / `screenshot_after` | `Page.captureScreenshot` (+ optional per-node clip) → content-addressed blobs |
| `network` | `Network.requestWillBeSent` … `loadingFinished` tuples in the action window: `{method, url_template, status, resource_type, initiator.type, duration_ms}`; redirect chains reconstructed from `redirectResponse` on successive `requestWillBeSent` with the same `requestId` |
| `navigation` | `Page.frameRequestedNavigation{reason ∈ anchorClick\|formSubmissionGet\|formSubmissionPost\|httpHeaderRefresh\|metaTagRefresh\|scriptInitiated\|reload\|initialFrameNavigation\|pageBlockInterstitial\|other, disposition ∈ currentTab\|newTab\|newWindow\|download}`, `Page.frameNavigated`, `Page.navigatedWithinDocument{navigationType ∈ fragment\|historyApi\|other}` |
| `console` | `Runtime.consoleAPICalled` + `Log.entryAdded` + `Runtime.exceptionThrown` during the window |
| `timing` | monotonic start/end, settle reason (`networkAlmostIdle` / `load` / timeout) |
| `video_offset` | if a recording job is active, the ms offset into the recording (ties into the action-log sync requirement) |

**Verified SPA-transition signal:** clicking through React Router produced exactly
`Page.frameStartedLoading` → `Page.navigatedWithinDocument{url:"https://reactrouter.com/brand", navigationType:"historyApi"}` → `Page.frameStoppedLoading` → `Page.lifecycleEvent{name:"firstImagePaint"}`.
Note that `Page.navigatedWithinDocument` is marked **EXPERIMENTAL** in the protocol — pin behaviour with a fixture test and keep a fallback (main-world `history.pushState` monkey-patch emitting via `Runtime.addBinding` → `Runtime.bindingCalled`).

### 5.2 Schema (SQLite, `rusqlite` 0.40.1)

```sql
CREATE TABLE crawl(
  id TEXT PRIMARY KEY, started_at INTEGER, finished_at INTEGER,
  seed_url TEXT, scope_json TEXT, budget_json TEXT,
  match_policy TEXT, signature_config_json TEXT,   -- reproducibility: the knobs used
  browser_version TEXT, protocol_version TEXT, harness_version TEXT
);

CREATE TABLE route_template(
  crawl_id TEXT, template TEXT, source TEXT,       -- framework_manifest|sitemap|sw_precache|speculation|manifest|bundle_regex|observed
  provenance_json TEXT, PRIMARY KEY(crawl_id, template, source)
);

CREATE TABLE state(
  id TEXT PRIMARY KEY,                              -- blake3 of the whole signature
  crawl_id TEXT, first_seen INTEGER, visit_count INTEGER,
  route_template TEXT, url_example TEXT, title TEXT,
  ax_skeleton_hash BLOB, affordance_hash BLOB, overlay_json TEXT,
  principal TEXT, phash INTEGER,
  is_error INTEGER, http_status INTEGER,
  console_error_count INTEGER, screenshot_blob TEXT,
  ax_blob TEXT, dom_blob TEXT                       -- content-addressed refs, may be NULL
);

CREATE TABLE edge(
  id TEXT PRIMARY KEY, crawl_id TEXT,
  from_state TEXT, to_state TEXT,                   -- to_state NULL when blocked/unreached
  kind TEXT,                                        -- link|spa_nav|form_submit|redirect|new_window|
                                                    -- overlay_open|overlay_close|scroll_append|api|
                                                    -- auth_branch|flag_branch|sw_route
  status TEXT CHECK(status IN
    ('declared','observed','discovered','inferred','blocked')),
  trigger_json TEXT, evidence_id TEXT,
  deterministic INTEGER, traversal_count INTEGER, mean_ms INTEGER,
  blocked_reason TEXT
);

CREATE TABLE evidence(
  id TEXT PRIMARY KEY, crawl_id TEXT,
  screenshot_before TEXT, screenshot_after TEXT, video_offset_ms INTEGER,
  network_json TEXT, console_json TEXT, navigation_json TEXT
);

CREATE TABLE blob(hash TEXT PRIMARY KEY, kind TEXT, bytes INTEGER, path TEXT);
CREATE INDEX edge_from ON edge(crawl_id, from_state);
CREATE INDEX state_route ON state(crawl_id, route_template);
```

Status semantics (pin these in docs, they are the honesty contract):

| Status | Means |
|---|---|
| `declared` | a route/edge the *app itself* claims exists (manifest, sitemap, SW precache, speculation rule, `<a href>` present in DOM) — **not** yet traversed |
| `observed` | we traversed it and captured before/after evidence |
| `discovered` | it appeared as a side effect we did not trigger deliberately (a redirect landed us there, a SW served it, a `window.open` happened) |
| `inferred` | derived from static analysis (bundle regex, sourcemap sources) with no runtime confirmation |
| `blocked` | we deliberately did not traverse (destructive gate, auth wall, CAPTCHA, out of scope, network backstop) — `blocked_reason` required |

### 5.3 Export

`sitegraph.json` (canonical, `schema_version: 1`):

```json
{
  "schema_version": 1,
  "crawl": {"id":"c_01J...","seed":"https://app.example.com","started_at":"2026-08-04T18:00:00Z",
            "match_policy":"structural","budget":{"max_states":300,"max_actions":2000,"max_wall_s":1800},
            "browser":"Chrome/151.0.7922.72","harness":"brow 0.1.0"},
  "route_templates": [
    {"template":"/users/:id","sources":["framework_manifest","observed"],"declared":true,"observed_count":3}
  ],
  "states": [
    {"id":"s_9f2a","route":"/users/:id","url_example":"/users/42","title":"Ada — Users",
     "principal":"alice","overlay":[],"console_errors":0,"screenshot":"blob:be31…","variants":7}
  ],
  "edges": [
    {"id":"e_18c","from":"s_home","to":"s_9f2a","kind":"spa_nav","status":"observed",
     "trigger":{"ref":"@node-42","role":"link","name":"Ada","locator":"[data-testid=user-row-42] a"},
     "evidence":"ev_18c","deterministic":true,"mean_ms":410}
  ],
  "coverage": { "…": "see §6" },
  "unreached": [{"template":"/billing/invoices/:id","source":"sitemap","reason":"never_linked_from_crawled_states"}]
}
```

Renderings generated from it (never hand-maintained): `sitegraph.dot` (Graphviz, clusters by route prefix, edge colour by status) and `sitegraph.mmd` (mermaid `stateDiagram-v2`, capped at N nodes with a `--collapse-templates` mode, because mermaid dies past a few hundred nodes).

---

## 6. Coverage honesty

### 6.1 Why "all URLs" is undecidable

An app whose routing depends on runtime data has an unbounded, non-enumerable URL set: `/orders/:id` has as many instances as the database has rows, which the crawler cannot know; a route may exist only under a feature flag whose value comes from a server-side experiment; a route may only be reachable after a state transition that requires a specific input (a coupon code, an OTP). Deciding "does any input sequence reach route R" is a reachability question over the app's full program semantics — for a Turing-complete client + opaque server it is not decidable in general. **So the crawler must never emit "100% coverage".** (This argument is reasoning, clearly flagged; the practical consequence is universally accepted in the crawling literature.)

### 6.2 The coverage report

Report **against declared routes**, per source, with provenance:

```
ROUTE COVERAGE                                    crawl c_01J...  (budget: exhausted@max_states)

  Declared routes (union of sources)                  128
    ├─ framework_manifest (react-router, PARTIAL*)     10
    ├─ sitemap.xml                                     94
    ├─ sw_precache (workbox-precache-v2)               42
    ├─ speculation_rules (href_matches)                 3
    └─ bundle_regex (inferred, low confidence)         31

  Observed (visited, evidence captured)               76 / 128   59%
  Discovered but not declared                         12         (routes no manifest knew about)
  Declared, reachable, not visited (budget)           38         (frontier non-empty at stop)
  Declared, unreachable from seed                      9         → listed with last-attempt reason
  Blocked                                              5         → 3 destructive, 1 auth, 1 captcha

  * react-router routeDiscovery.mode == "lazy" (fog of war): the manifest is INCOMPLETE by design.
    Declared-route count is a LOWER BOUND.

  UNBOUNDED: 6 route templates are parameterised (/users/:id, /orders/:id, …).
    Instances visited: 14. Total instances: UNKNOWN (data-driven).
    Coverage over instances is NOT computed and NOT meaningful.

  STATE COVERAGE                                      states 214, edges 611
    URL-less states (modal/drawer/wizard/tab)           58   (27%)  ← invisible to any URL-based tool
    States with console errors                          11
    Non-deterministic edges (replay mismatch)            4
```

Every number above is derivable from the schema in §5.2. The two lines that make this report *honest* rather than marketing are the `PARTIAL*` annotation and the `UNBOUNDED` block.

> **Corrected 2026-08-04 — a third honesty line is required, and the `framework_manifest` row needs a unit change.**
> (a) A React Router manifest entry is a **route definition**, not a URL — measured, 10 entries on reactrouter.com include two `*` splats and a `/:ref` param covering thousands of URLs (§3.2). Summing it into a "declared routes" total alongside 94 sitemap **URLs** and 42 precache **URLs** adds incompatible units and produces a meaningless denominator. Report framework-manifest contributions in a separate `route_definitions` count, or expand them to concrete URLs via the `__manifest?paths=` sniffing trick before they enter the total.
> (b) Add a **`MUTATION CONTAINMENT`** block, since §4.3 is now known to be leaky:
> ```
>   MUTATION CONTAINMENT                              posture: read-only (best-effort)
>     Blocked non-idempotent requests                   14
>     Service-worker sessions with Fetch enabled         2 / 2      ← must be N/N
>     Mutating-GET requests allowed through             37          ← NOT containable
>     WebSocket connections opened                       1          ← NOT observable at frame level
>     Prerender/prefetch server hits not attributable    6
>     Client-side storage writes (not blockable)      unknown
> ```
> Without this block the report implies a guarantee the harness does not provide.

---

## 7. Determinism and re-runs

A crawl is not reproducible: action order depends on timing, the server has state, ads/experiments vary. What *is* comparable is the **model**. Diff two `sitegraph.json` by matching on `(route_template, ax_skeleton_hash, overlay, principal)` — deliberately excluding `affordance_hash` and `phash` at match time so that affordance/visual changes show up as *changed*, not as *removed + added*.

```
brow crawl diff base.sitegraph.json head.sitegraph.json --format md
```

| Diff class | Rule | Why it matters |
|---|---|---|
| `state.added` | signature key in head, not base | new screen shipped, or new state fragmentation (regression in the app or in your abstraction) |
| `state.removed` | in base, not head | route deleted, or newly unreachable — the loud one |
| `state.changed` | same key, different `affordance_hash` | a button appeared/disappeared/became disabled |
| `state.visual_changed` | same key, `phash` Hamming > tol | design regression (only when `--visual`) |
| `edge.added` / `edge.removed` | on `(from_key, to_key, kind, trigger.name_class)` | navigation graph change |
| `edge.status_regressed` | `observed` → `blocked`/absent | something started failing |
| `console.new` | error signature (message with numbers/hex normalised) present in head's state, absent in base's | the single highest-signal regression class |
| `network.new_endpoint` | `(method, url_template)` new for a state | API surface drift |
| `coverage.delta` | declared/observed deltas per source | someone deleted a sitemap entry |

Flakiness control: re-traverse each edge `k` (default 2) times when `--stabilise` is set; edges whose destination signature differs across traversals get `deterministic = 0` and are **excluded from the diff** (reported separately in a `flaky` section). Without this, a diff of two crawls is 40% noise and nobody reads it.

Determinism aids: fixed viewport via `Emulation.setDeviceMetricsOverride`, fixed UA via `Network.setUserAgentOverride`, fixed locale/timezone via `Emulation.setLocaleOverride`/`setTimezoneOverride`, `--seed` for the fixture RNG, and optionally `Emulation.setVirtualTimePolicy` (EXPERIMENTAL, and known to be sharp-edged — don't default it on).

---

## 8. Budgeting and background-job progress

Budget dimensions, all hard limits, first to trip wins:

```toml
[budget]
max_wall_s      = 1800
max_states      = 300
max_actions     = 2000
max_depth       = 6
max_pages_loaded= 500        # full document loads (network cost proxy)
max_bytes        = "500MB"   # artifacts
per_action_timeout_ms = 15000
list_sample_n   = 2
visits_per_template = 3
max_scroll_iterations = 5
```

`browserctl job status <id>` should report *meaningful* progress, not a fake percentage. There is no denominator for exploration — but there *is* one for declared routes:

```json
{
  "job":"j_7Q…","phase":"exploring","state":"running",
  "elapsed_s":412,"budget":{"wall_s":{"used":412,"max":1800},
                            "states":{"used":143,"max":300},
                            "actions":{"used":892,"max":2000}},
  "progress":{"declared_routes":128,"observed":76,"frontier_size":57,
              "new_states_last_60s":4,"actions_per_min":130},
  "current":{"url":"https://app.example.com/users/42","route":"/users/:id","depth":3},
  "eta_hint":"budget-bound: ~24 min remaining at current rate; frontier not empty",
  "notable":[{"t":389,"kind":"console_error","state":"s_9f2a","message":"TypeError: …"},
             {"t":401,"kind":"waiting_for_approval","reason":"destructive","label":"Delete workspace"}]
}
```

The two honest signals are **`frontier_size`** (is it growing or draining?) and **`new_states_last_60s`** (are we still learning, or churning?). If `new_states_last_60s == 0` for `stall_s` (default 120) while the frontier is non-empty, that's a *saturation* condition worth surfacing — usually it means the abstraction is too coarse or every remaining frontier item is blocked.

Progress events stream over the unix socket as NDJSON so `browserctl job logs --follow` is a straight pipe.

---

## 9. What we verified empirically

All of the following was run locally on 2026-08-04 against **Google Chrome 151.0.7922.72** (V8 15.1.206.10), launched as
`--headless=new --remote-debugging-port=<39217..39219> --user-data-dir=/private/tmp/brow-scratch/uddN`, driven by a ~90-line pure-Python WebSocket CDP client I wrote for this task (no Playwright/Puppeteer). All instances were killed afterwards.

| # | What I ran | Raw observation |
|---|---|---|
| 1 | `GET /json/version` | `{"Browser":"Chrome/151.0.7922.72","Protocol-Version":"1.3","V8-Version":"15.1.206.10"}` |
| 2 | `GET /json/protocol` | **57 domains**. Confirmed present: `DOMSnapshot`, `CacheStorage`, `Preload`, `ServiceWorker`, `WebMCP` (new, EXPERIMENTAL — `enable/disable/invokeTool/cancelInvocation`), `Page.getAnnotatedPageContent` (EXPERIMENTAL, returns a base64 **protobuf** `AnnotatedPageContent` — unusable without the optimization_guide proto, do not depend on it) |
| 3 | Isolated vs main world on reactrouter.com | isolated: `{"rrManifest":"undefined","domNodes":731}`; main: `{"rrManifest":"object","domNodes":731}` → **framework globals are invisible from isolated worlds** |
| 4 | reactrouter.com globals | `__reactRouterContext`, `__reactRouterVersion="8.0.0"`, `__reactRouterManifest`, `__reactRouterRouteModules`, `__reactRouterDataRouter` (has `patchRoutes`, `_internalSetRoutes`, `state.matches`); `routeDiscovery = {"mode":"lazy","manifestPath":"/__manifest"}`. ⚠️ **Re-measured 2026-08-04: 7 routes on landing, not 10**; 10 only after navigating into the docs; `__reactRouterDataRouter.routes.length === 1` |
| 5 | `GET /__manifest?p=/&version=<v>` on RR 8.0.0 | ⚠️ **SUPERSEDED.** Re-measured: `p=` now returns **HTTP 204** (empty) for every path, with or without `version`. The real contract, captured off RR's own `Network.requestWillBeSent`, is **`?paths=<comma-joined,URL-encoded>&version=<build-hash>` → HTTP 200 `application/json`**. Eager `<Link>` discovery verified working (3 batched requests, ~30 paths enumerated) but grew the manifest only **7 → 10** against **351** on-page internal links — the manifest is a route *tree* with `*` splats and `/:ref`, not a URL list |
| 6 | nuxt.com | `window.useNuxtApp().$router.getRoutes()` → **293 routes** (reproduced exactly 2026-08-04); also reachable via `document.querySelector('#__nuxt').__vue_app__.config.globalProperties.$router`. ⚠️ **Corrected:** `typeof __VUE_DEVTOOLS_GLOBAL_HOOK__` is **`"undefined"`**, not `false`. Likewise `__REACT_DEVTOOLS_GLOBAL_HOOK__` is `undefined` on reactrouter.com and nextjs.org — production frameworks do not install a devtools hook, the extension does |
| 7 | nextjs.org / vercel.com / tailwindcss.com | Next.js `16.3.0-canary.105` / `16.2.6`, `appDir:true`; `__NEXT_DATA__` **absent**, `self.__BUILD_MANIFEST` **absent**; `window.next.router` has only imperative methods |
| 8 | svelte.dev | `window.__sveltekit_1uabs51 = {"base":"","version":"1785849980348"}` — **no route data**; global name carries a per-build hash suffix |
| 9 | angular.dev | root `<ADEV-ROOT ng-version="22.1.0+sha-ac3728e">`; `window.ng`, `window.ngDevMode`, `window.getAllAngularRootElements` all `undefined` |
| 10 | SPA transition via `__reactRouterDataRouter.navigate('/brand')` | events: `Page.frameStartedLoading` → **`Page.navigatedWithinDocument{"url":"https://reactrouter.com/brand","navigationType":"historyApi"}`** → `Page.frameStoppedLoading` → `Page.lifecycleEvent{"name":"firstImagePaint"}`; `Page.getNavigationHistory` grew 2 → 3 entries |
| 11 | `CacheStorage` on squoosh.app | cache `static-fef4647d6f2b904b3f63250d079f6dfd4f008d0c`, **15 entries** (`/`, `/c/Compress-5b50107e.js`, …) |
| 12 | `CacheStorage` on vite-pwa-org.netlify.app | cache `workbox-precache-v2-https://vite-pwa-org.netlify.app/`. ⚠️ **Re-measured 2026-08-04: `returnCount: 249`, not 42** — `requestEntries` is capped by `pageSize`, so you must paginate. Of the first 200 entries, **138 were `.js`/`.css`/`.wasm`** and ~48 route-like (`/404.html?__WB_REVISION__=…`, `/assets-generator/{api,cli,index,integrations,migrations}.html`). Contrast squoosh.app (client-rendered SPA): 15 entries, **1** route-like. "Goldmine" holds only for prerendered/MPA sites |
| 13 | `window.open('https://example.com','_blank')` | `Page.windowOpen{"url":"https://example.com/","windowName":"_blank","windowFeatures":["menubar","toolbar","status","scrollbars","resizable"],"userGesture":false}` + `Target.targetCreated`. Noise observed: `background_page` (Chrome Web Store Payments), `browser_ui` (Omnibox Popup), extension `service_worker` |
| 14 | Signature-cost benchmark | table in §2.1 (AX tree 62 ms / 2.64 MB on a 3043-node Wikipedia page; DOMSnapshot minimal 21 ms; screenshot 15 ms) |
| 15 | `nextjs.org/sitemap.xml` | 100,117 bytes, **715 `<loc>` entries**, flat (not a sitemapindex) |
| 16 | `reactrouter.com/robots.txt` and `/sitemap.xml` | ⚠️ **Re-measured 2026-08-04: both now return HTTP 404** with `content-type: text/html` and a 6630-byte `<!DOCTYPE html>` SPA shell — **not** HTTP 200. The rule (content-sniff, never trust the status alone) stands; the specific observation does not reproduce |
| 17 | `reactrouter.com/assets/root-*.js.map` | **HTTP 404** → sourcemaps stripped in prod; bundle body does contain backtick route literals |
| 18 | Crawljax TWEB PDF, text-extracted locally | quotes in §1.1 are from the actual paper text, not from memory |
| 19 | Backgrounded Chrome died between tool invocations twice | incidental confirmation that the daemon genuinely needs launchd/`start_new_session`, not `nohup &` |

Crate versions pulled live from crates.io on 2026-08-04: `petgraph 0.8.3`, `blake3 1.8.5`, `url 2.5.8`, `image 0.25.10`, `image_hasher 3.1.1` (use this, **not** `img_hash` — last release 2021), `sourcemap 9.3.2`, `quick-xml 0.41.0`, `sitemap-rs 0.4.0` (the `sitemap` crate is dead: 0.4.1, 2020), `rusqlite 0.40.1`, `serde_json 1.0.151`, `similar 3.1.2`, `regex 1.13.1`, `indexmap 2.14.0`, `dashmap 6.2.1`, `ahash 0.8.12`, `texting_robots 0.2.2` (stale), `tokio 1.53.1`.

---

## 10. Limits and impossibilities — read this section twice

1. **Next.js App Router, SvelteKit and production Angular do not expose their routes at runtime. Period.** Verified on three live Next.js properties, svelte.dev and angular.dev. Any product copy promising "framework-aware route extraction" must qualify: complete for Vue/Nuxt and React Router (modulo fog of war), *nonexistent* for Next App Router / SvelteKit / Angular-prod without the repo or sourcemaps. This is not a bug we can fix.
2. **React Router's manifest is partial by design.** `routeDiscovery.mode === "lazy"` means the initial manifest is a lower bound (10 of N on reactrouter.com). Coverage denominators derived from it are lower bounds and must be labelled as such.
3. **`inspect.evaluate` in an isolated world cannot read framework globals.** Verified. Either route extraction gets a privileged fixed-payload path, or the feature does not exist.
4. **No state abstraction is correct.** The 2026 study measures a 14.8-point coverage swing from abstraction choice alone [1]. Whatever we default to will be wrong for some app. The mitigation is exposing the knobs and *recording which knobs were used in the crawl artifact* so results are interpretable.
5. **"Coverage" over parameterised routes is meaningless** and we must refuse to compute it. `/orders/:id` with 4M rows is not 0.0003% covered; it is *template-covered, instance-unbounded*.
6. **Destructive-action detection by keywords/ARIA has an unmeasured false-negative rate.** Icon-only buttons, non-English UIs, euphemisms ("Archive forever"), and multi-step confirmations all defeat it. **And the `Fetch.requestPaused` network block is NOT sound either** — measured 2026-08-04: it misses service-worker-originated `fetch()` entirely unless you also `Fetch.enable` on the auto-attached SW target, never sees WebSocket handshakes, cannot block mutating GETs by construction, and does not touch `localStorage.clear()` / `indexedDB.deleteDatabase()` / already-registered Background Sync / prerender-triggered server hits. It *does* catch `navigator.sendBeacon` (as `resourceType: "Ping"`), which was previously assumed lost. Full table in §4.3. **No layer of the guard is sound; the product copy must say "best-effort read-only", never "cannot mutate".**
7. **Replay-based state restoration is O(depth) per backtrack and can fail.** Any app with server-side single-use tokens, rate limits, or non-idempotent GETs will diverge on replay. We detect divergence (signature mismatch) and mark the edge non-deterministic — we cannot fix it.
8. **Crawls are not reproducible.** Only the *model diff* is meaningful, and even that needs flakiness suppression (`--stabilise`) to be readable.
9. **CAPTCHA, OS permission dialogs, Keychain, Touch ID, browser chrome** — out of scope per spec; every one of them terminates a branch with `blocked` and a human-handoff prompt.
10. **The experimental surface is far larger than originally listed: 38 of 57 domains in Chrome 151's own `/json/protocol` are marked `experimental: true`.** Verified list: `Ads, Animation, Audits, Autofill, BackgroundService, BluetoothEmulation, CSS, CacheStorage, Cast, CrashReportContext, DOMSnapshot, DOMStorage, DeviceAccess, DeviceOrientation, EventBreakpoints, Extensions, FedCm, FileSystem, HeadlessExperimental, HeapProfiler, IndexedDB, Inspector, LayerTree, Media, Memory, Overlay, PWA, PerformanceTimeline, Preload, ServiceWorker, SmartCardEmulation, Storage, SystemInfo, Tethering, WebAudio, WebAuthn, WebMCP` plus `Accessibility`. **Two additions that matter and were missed: `CSS` and `CacheStorage`.** `CSS` is load-bearing for the owner's Unified Page Tree; `CacheStorage` is load-bearing for the SW-precache route source in §3.3. Member-level experimental flags on otherwise-stable domains also matter: `Page.getResourceTree`, `Page.frameStartedLoading`, `Page.navigatedWithinDocument`, `Page.frameRequestedNavigation`, `Page.getAnnotatedPageContent`, `Runtime.bindingCalled` (the **event**, though `Runtime.addBinding` is stable), `Emulation.setLocaleOverride` (but `setTimezoneOverride` is stable), `Emulation.setVirtualTimePolicy`, and the `manifest` return of `Page.getAppManifest`. Every one needs a fixture test in `tests/` that fails loudly on protocol drift, and `browserd` should record `Protocol-Version` + `Browser` in every crawl artifact (already in the schema).
11. **Extension/browser-UI targets pollute `Target.targetCreated`** even in headless with a fresh profile — measured: 5 bundled component-extension targets (2 `background_page`, 3 `service_worker`) out of 9 auto-attached. Filter on `type == "page"` + our `browserContextId` **for graph nodes**, but use a wider predicate for policy enforcement (§4.3 needs the SW targets). Prerendered documents, by contrast, did **not** appear as targets at all (§4.8) — the risk there is invisibility, not noise.
12. **Closed shadow roots are NOT a differentiator between these APIs — all three see through them.** Verified 2026-08-04 on a fixture with one `{mode:'closed'}` and one `{mode:'open'}` shadow root, each containing `<button aria-label="…ShadowButton">`: `DOM.getDocument{depth:-1,pierce:true}` returned both buttons and reported `"shadowRootType": ["closed","open"]`; `Accessibility.getFullAXTree` returned `button:ClosedShadowButton` and `button:OpenShadowButton`; **and `DOMSnapshot.captureSnapshot` returned both as well**. The earlier framing — that `DOMSnapshot` "flattens" shadow DOM and is therefore weaker for closed roots — is misleading: flattening means the *content is present but the shadow boundary is erased*. So the real trade-off is **provenance, not visibility**: use `DOM.getDocument{pierce:true}` when you need to know a node lives in a closed root; use AX or DOMSnapshot when you only need semantics. Keep the fixture.
13. **`Page.getAnnotatedPageContent`** looks tailor-made for AI page understanding. Verified present in Chrome 151 as `{includeActionableInformation?: boolean} → {content: binary}`, EXPERIMENTAL. On a trivial fixture it returned 2490 bytes of protobuf. Mild nuance on the earlier "opaque" verdict: the wire format is self-describing enough that string fields are readable without the schema (`\n\x04Acme` was plainly visible), so a generic protobuf scanner could harvest text. But the *structure* — which is the entire value — is tied to `components/optimization_guide/proto/` and changes without notice. Recommendation unchanged: **ignore for v1.**

---

## 11. Open questions for the owner

1. **Isolated vs main world for route extraction** (§3.4) — do you accept a `inspect.routes` capability that runs *our* fixed strings in the main world? If not, the framework-adapter value proposition collapses to "detect the framework and its version".
2. **Default match policy** — `structural` (my recommendation) or `strict`? `strict` finds more real states and more false states; `structural` is friendlier for site-map deliverables.
3. **Do we ship a repo-aware mode?** If `brow` can read the project's source dir, Next.js/SvelteKit/Angular route extraction becomes trivial and complete (`.next/routes-manifest.json`, `.svelte-kit/`, `src/app/**/page.tsx` globbing). That is a big capability delta but adds a filesystem-scope policy question.
4. **Mutation posture default** — is `Fetch.requestPaused`-based blocking of POST/PUT/PATCH/DELETE ON by default for crawl jobs? I recommend yes, with `--allow-mutations <glob>` to opt in.
5. **Parallelism** — one browser context per principal, or per frontier branch? Contexts are cheap (`Target.createBrowserContext`); memory is not. What's the target machine profile?
6. **Do we persist DOM/AX blobs per state?** They're 1–3 MB each. 300 states × 2 blobs = ~1 GB per crawl. Options: store only for states with anomalies, or store `zstd`-compressed with a retention policy.
7. **How much of §4.2 step 6 (agent action selection) is in-loop?** A fully autonomous background crawl can't round-trip to the agent 2000 times. Proposal: agent sets *policy* up front (priorities, veto lexemes, form fixtures) and is consulted only on `unknown` classifications and approval gates. Confirm.
8. **`WebMCP` domain** (new in Chrome 151, EXPERIMENTAL): sites can register agent-facing tools that Chrome exposes via `WebMCP.enable`/`invokeTool`. If sites start declaring tools, that is a *first-class declared-capability source* for the site graph. Worth a spike?

---

## 12. Sources

1. *Understanding Automated Web GUI Testing: An Empirical Study Across Exploration Strategies and State Abstractions*, arXiv 2606.16650 — https://arxiv.org/html/2606.16650
2. Mesbah, van Deursen, Lenselink, *Crawling Ajax-Based Web Applications through Dynamic Analysis of User Interface State Changes*, ACM TWEB — https://people.ece.ubc.ca/amesbah/resources/papers/tweb-final-old.pdf (PDF fetched and text-extracted locally)
3. PortSwigger, *Crawling* (Burp Scanner documentation) — https://portswigger.net/burp/documentation/scanner/crawling
4. *Go-Browse: Training Web Agents with Structured Exploration*, arXiv 2506.03533 — https://arxiv.org/pdf/2506.03533
5. *AutoCrawler: A Progressive Understanding Web Agent for Web Crawler Generation*, arXiv 2404.12753 — https://arxiv.org/html/2404.12753v1
6. React Router, *Lazy Route Discovery* — https://reactrouter.com/explanation/lazy-route-discovery
7. Remix blog, *Fog of War* — https://remix.run/blog/fog-of-war
8. Chrome for Developers, *Precaching with Workbox* — https://developer.chrome.com/docs/workbox/precaching-with-workbox
9. Chrome for Developers, *workbox-build* (`injectManifest`, `self.__WB_MANIFEST`) — https://developer.chrome.com/docs/workbox/modules/workbox-build/
10. Crawljax — https://github.com/crawljax/crawljax
11. *Judge: Effective State Abstraction for Guiding Automated Web GUI Testing*, ACM TOSEM — https://doi.org/10.1145/3736162
12. ACM TWEB landing page for [2] — https://dl.acm.org/doi/10.1145/2109205.2109208
13. Chrome DevTools Protocol (tot) — https://chromedevtools.github.io/devtools-protocol/ (protocol facts in this document were taken from the **live** `/json/protocol` of Chrome 151.0.7922.72, not the website)
14. Next.js App Router docs — https://nextjs.org/docs/app
15. Koppula et al., *Learning URL Patterns for Webpage De-duplication*, WSDM 2010 — http://www.wsdm-conference.org/2010/proceedings/docs/p381.pdf
16. Agarwal et al., *URL Normalization for De-duplication of Web Pages*, CIKM 2009 — https://www.cs.cornell.edu/~hema/papers/sp0955-agarwalATS.pdf
17. crates.io API (versions as of 2026-08-04) — https://crates.io/api/v1/crates/<name>
18. Vite PWA / Workbox `injectManifest` — https://vite-pwa-org.netlify.app/workbox/inject-manifest

---

## 13. Verification pass — 2026-08-04 (adversarial)

Independent re-verification against **Chrome 151.0.7922.72** (V8 15.1.206.10, Protocol-Version 1.3), launched as
`--headless=new --remote-debugging-port=<39411..39461> --user-data-dir=/private/tmp/brow-verify/uddN`, driven by a raw-socket WebSocket CDP client (no Playwright/Puppeteer/`websockets` dependency). Local HTTP + service-worker fixture server on 127.0.0.1. All Chrome instances killed after each run.

### Outcomes

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| 1 | Isolated world cannot read framework globals | **CONFIRMED** | Identical DOM node counts across worlds on 5 sites (118/118, 1945/1945, 2378/2378, 396/396, 477/477); every framework global `undefined` in the isolated world. `[ng-version]` **attribute** does survive (it is DOM) |
| 2 | Next.js App Router / SvelteKit / prod Angular expose no routes | **CONFIRMED verbatim** | `nextjs.org` 16.3.0-canary.105 `appDir:true`, no `__NEXT_DATA__`/`__BUILD_MANIFEST`; `svelte.dev` `__sveltekit_1uabs51={"base":"","version":"1785849980348"}`; `angular.dev` `ng-version="22.1.0+sha-ac3728e"`, `window.ng` undefined |
| 3 | Vue/Nuxt route extraction is complete | **CONFIRMED** (single sample) | nuxt.com → **293** routes, both access paths |
| 4 | React Router manifest gives a usable route set | **PARTIAL → effectively REFUTED as a denominator** | 7 routes on landing, 10 after nav, against **351** on-page links; paths are `["","","*",null,"/:ref",null,…]`. §3.2 |
| 5 | `/__manifest?p=…` returns HTTP 400 | **REFUTED** | `p=` → **204** for every path; real contract is `?paths=<csv>&version=<build-hash>` → **200 application/json**. §3.2 |
| 6 | Eager `<Link>` discovery grows the manifest toward complete | **PARTIAL** | Growth confirmed (3 batched requests, ~30 paths) but 7→10 only. Docs default `mode:"lazy"` confirmed at reactrouter.com/explanation/lazy-route-discovery |
| 7 | `Fetch.requestPaused` makes read-only crawls safe | **REFUTED** | SW-originated POST **and** GET reached the server with 200 and produced zero pause events; mutating GET passed; WebSocket handshake never paused; `localStorage.clear()`/`indexedDB.deleteDatabase()` succeeded. `sendBeacon` **was** caught (`Ping`). Fix (Fetch on auto-attached SW target) verified working. §4.3 |
| 8 | `Accessibility.getFullAXTree` is the best structural basis | **PARTIAL** | Excellent on semantic markup (`dialog\|short_text\|modal\|4`); on div soup it degrades to a node-count signature with **zero** role information and the modal is invisible. `InlineTextBox` (layout line boxes) must be dropped — 21% of AX nodes on Wikipedia; ~44% of nodes carried no semantic role. §2.3 |
| 9 | SW precache is a general route goldmine | **PARTIAL** | vite-pwa-org `returnCount:249` (138/200 were js/css/wasm); squoosh.app 15 entries, **1** route-like. Bimodal, exactly as flagged. §3.3 |
| 10 | Composite signature approximates Judge at zero model cost | **REFUTED as stated** | No measurement supports it; div-soup behaviour resembles StringCmp over-fragmentation, not Judge. §1.3 |
| 11 | The 2026 study's numbers | **PARTIAL** | Paper exists (arXiv 2606.16650, 2026-06-15, title exact). Judge 54.18% / PDiff 39.39% / 14.79pt swing **confirmed**; Gestalt **17** and StringCmp **316** on Dimeshift (not "16–17 / 316–1358"); **"38 unique failures" is unsupported** — no per-abstraction failure table exists; **StringCmp is 49.12%**, ~10pt above PDiff |
| 12 | Burp + Crawljax quotes | **CONFIRMED verbatim** | portswigger.net/burp/documentation/scanner/crawling returned all five passages word-for-word, incl. *"never 'jumps' to a pending link … reverts to the start location and navigates from there"* |
| 13 | Judge / Go-Browse citations | **CONFIRMED** | DOI 10.1145/3736162 = *Judge: Effective State Abstraction for Guiding Automated Web GUI Testing*, ACM TOSEM, 2026-02-13. arXiv 2506.03533 = *Go-Browse*, Gandhi & Neubig |
| 14 | The listed CDP domains are EXPERIMENTAL | **CONFIRMED and understated** | **38 of 57** domains, incl. the unlisted **`CSS`** and **`CacheStorage`**. §10.10 |
| 15 | Only `type=="page"` matters; extension noise is filterable | **CONFIRMED, with a caveat** | 5 of 9 auto-attached targets were bundled component extensions in a fresh profile. But policy enforcement now *needs* the SW targets — one filter cannot serve both. §4.8 |
| 16 | Prerender targets will pollute target discovery | **REFUTED** | No prerender target surfaced at all; all `subtype` values `null`. The real risk is invisibility. §4.8 |
| 17 | Speculation rules are a declared-route source | **PARTIAL** | Mechanism confirmed on a local fixture (`ruleSetUpdated.sourceText` carried `href_matches`); **0 rulesets** on 2 live sites. `Preload.preloadingAttemptSourcesUpdated` found to be the better event (resolved URLs + trigger `nodeIds`). §3.3 |
| 18 | `reactrouter.com` robots/sitemap return 200 + SPA shell | **REFUTED (stale)** | Both now **404** with an HTML body. Rule survives, evidence does not. §9 row 16 |
| 19 | Closed shadow roots need AX | **REFUTED** | `DOM.getDocument{pierce:true}`, `Accessibility.getFullAXTree` **and** `DOMSnapshot.captureSnapshot` all returned the closed-root button. §10.12 |
| 20 | Crate versions | **CONFIRMED, all 17** | crates.io API 2026-08-04: `petgraph 0.8.3`, `blake3 1.8.5`, `url 2.5.8`, `image 0.25.10`, `image_hasher 3.1.1`, `sourcemap 9.3.2`, `quick-xml 0.41.0`, `sitemap-rs 0.4.0`, `rusqlite 0.40.1`, `serde_json 1.0.151`, `similar 3.1.2`, `regex 1.13.1`, `indexmap 2.14.0`, `dashmap 6.2.1`, `ahash 0.8.12`, `texting_robots 0.2.2`, `tokio 1.53.1`. Deadness confirmed: `sitemap 0.4.1` (2020-11-03), `img_hash 3.2.0` (2021-05-04) |
| 21 | `nextjs.org/sitemap.xml` → 715 `<loc>`, flat | **CONFIRMED exactly** | 100,117 bytes, 715 `<loc>`, no `sitemapindex` |
| 22 | Protocol shapes cited throughout | **CONFIRMED** | `Network.ErrorReason` contains `BlockedByClient`; `Page.ClientNavigationReason` enum matches the 10 listed values; `Page.navigatedWithinDocument.navigationType` enum = `["fragment","historyApi","other"]`; `Fetch.RequestStage` = `["Request","Response"]`; `Preload.RuleSet` fields match; `WebMCP` = `enable/disable/invokeTool/cancelInvocation` + events `toolsAdded/toolsRemoved/toolInvoked/toolResponded`. One naming fix: CDP's `WebAppManifest` uses **`startUrl`**, not `start_url` |

### Scope judgement (staff-engineer read)

| Area | Honest size |
|---|---|
| Declared-route harvesting (sitemap, robots, SW precache, web manifest, speculation rules, RR `paths=` sniffing) | **~2 weeks.** All mechanisms verified; failure modes known and cheap to encode |
| SQLite schema + `sitegraph.json` + DOT/mermaid export + `crawl diff` | **~2–3 weeks.** Ordinary engineering, no research risk |
| Budgeting, frontier, job status/progress streaming | **~1–2 weeks** |
| Framework adapters | **~1 week for what actually works** (React Router, Vue/Nuxt, version detection everywhere). The Next.js App Router / SvelteKit / Angular half is **not a schedule item — it is impossible at runtime.** Either ship repo-aware mode (open question 3) or cut the promise |
| Destructive-action containment | **~2 weeks for the mechanism, unbounded for the guarantee.** The multi-target `Fetch.enable` fix is a few days. Making the claim *true* is not achievable; the deliverable is an honest containment report, not a guarantee |
| **State identity that actually works across app styles** | **3–6 months of empirical work, and it is the whole product.** The literature says no abstraction dominates; my own fixtures show the proposed one collapses on div soup; the fallback skeleton is unwritten. This needs a benchmark corpus and a measurement loop before it can be called done |
| **Replay-from-root state restoration** | **Unknown until divergence rate is measured — 3 weeks if it is <2%/edge, a redesign if it is >10%.** This is the single item that can invalidate the crawler architecture. Measure it in week one (§4.6) |
| Coverage reporting with honest provenance | **~1 week.** Easy, and the highest-credibility feature in the document |

**Recommended cut for v1:** ship declared-route harvesting + `restorable_by_url` states + the graph/diff/report machinery, and gate exploratory state-flow crawling behind an explicitly-labelled experimental flag until the divergence rate and the fallback skeleton are measured.
