# State-aware crawling and the site graph

> **Bottom line.** The state-flow-graph model (Crawljax, 2008–2012) and replay-from-root navigation (Burp Scanner) are solved, ~15-year-old prior art — copy them. The unsolved part is **state identity**, and the literature is brutal about it: the ICSE-2020 near-duplicate study (493k page pairs, 6,000 sites, 10 algorithms) found the *best* algorithm reaches only **F1 ≈ 0.60** at classifying two pages as same/different, and the best inferred *model* F1 was **0.66**, with RTED decaying from **0.95 → 0.45** as a crawl accumulates near-duplicates [1]. No abstraction wins; ship a **composite, layered, per-crawl-tunable signature** with recorded knobs, not a magic hash. On the route-extraction side I ran a fresh empirical pass on 2026-08-04 (Chrome 151.0.7922.72) and got one **major correction to the previous version of this document: SvelteKit routes *are* fully extractable** — not from a `window` global but statically, from `_app/immutable/entry/app.*.js`, which carries the complete `dictionary` of route ids including params and auth-gated `(admin)` groups (verified on 3 live sites: 23 / 48 / 8 routes, from files of 4–20 KB, no JS execution required). Conversely **Next.js App Router is close to hopeless at runtime**: bundle-literal regex recovered **14 of 715 sitemap paths (2.0 % recall)** on nextjs.org, `__next_f` was empty, and every build manifest 404s — while the same regex on Nuxt recovered **240/251 declared non-asset routes (95.6 %)**. Replay-from-root, flagged last pass as the largest unquantified schedule risk, measured **0 divergences in 36 replayed edges** across three live sites — encouraging, but on the easy case, and partly inflated by signature collisions I also measured. "All URLs" stays undecidable; report coverage against *declared* routes with provenance and never emit a single percentage.

---

## Decisions

| # | Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|---|
| D1 | State identity = **composite tuple** `(route_template, ax_skeleton_hash, affordance_hash, overlay_stack, principal)` + a per-crawl **match policy** | No abstraction dominates: best state-pair F1 0.60, best model F1 0.66 [1]; 14.79-pt coverage swing from abstraction alone [2] | Single DOM string hash (Crawljax default) | confirmed (literature + own measurement) |
| D2 | Ship **both** an AX-based skeleton and a **DOMSnapshot-based skeleton in v1**, auto-selected by measured `semantic_role_ratio` — **and stitch both across frames (see §2.3)** | Measured ratio spans **0.138 (Wikipedia) → 0.542 (login form)**; below ~0.25 the AX skeleton degenerates into a node-count signature. DOMSnapshot skeleton is also **4.4× cheaper** (74 ms vs 328 ms on a 23.5k-node page) | AX-only (v1 plan of the previous draft) | confirmed (own measurement); ⚠️ **frame-stitching added 2026-08-04 — both skeletons are frame-blind without it, see §2.3 / Limits #16** |
| D3 | Route-template induction is **leaf-only + ≥k siblings + ≥60 % of siblings**, and declared static literals always win | Naive slug induction on 715 real nextjs.org URLs produced **wrong** templates (`/docs/pages/:slug/cli`, where `:slug` is a static literal); leaf-only guard fixed correctness at identical compression (1.82×) | Unrestricted segment-class induction | confirmed (own measurement) |
| D4 | Declared-route harvesting (sitemap, robots, SvelteKit `app.*.js`, Vue router, RR `__manifest?paths=` sniffing, SW precache, speculation rules) runs **before** exploratory clicking | Measured yields: 715 sitemap URLs in one request; 48 SvelteKit routes incl. hidden `/(admin)/*` from a 20 KB file; 293 Nuxt routes; vs hours of clicking | Pure BFS clicking | confirmed (own measurement) |
| D5 | Framework globals need **main-world** evaluation → a narrow `inspect.routes` capability with fixed, audited payloads; **but prefer HTTP-only extractors where they exist** | Isolated world sees DOM, not framework globals (prior pass, 5/5 frameworks). SvelteKit + sitemap + RR-`__manifest` sniffing need **no** evaluation at all, which removes the trust question entirely for those | Main-world `evaluate` exposed to the agent (violates hard constraint); isolated-world extractors (do not work) | confirmed |
| D6 | Restoration = **replay action path from root**, with a learned `restorable_by_url` fast path | Crawljax and Burp both say browser-back is unreliable in SPAs [3][4]; measured divergence **0/36 edges** makes replay affordable in the common case | `Page.navigateToHistoryEntry` / bfcache as primary | confirmed (sources + own measurement, easy case only) |
| D7 | Storage = **SQLite (`rusqlite 0.40.1`)** for the graph + content-addressed blob store; `petgraph 0.8.3` as an in-memory analysis view only | Long-running, resumable, queryable ("all states with console errors"); artifacts are large binaries | Single JSON file; `redb 4.1.0` (no SQL/tooling) | likely |
| D8 | Canonical export `sitegraph.json` (versioned) + generated `.dot` / `.mmd` | JSON is the machine contract; renderings are throwaway | GraphML/GEXF as canonical | likely |
| D9 | Destructive-action guard is 3-layer (lexical/ARIA → capability gate → `Fetch.enable` **on every auto-attached target**), and is documented as **best-effort, never a guarantee** | Prior pass measured SW-originated `fetch()` bypassing a page-session `Fetch.enable` entirely; this pass measured **2–45 % of interactive elements have no accessible name at all**, so layer 1 is blind by construction | "Read-only mode cannot mutate" (false) | confirmed (measured leaks both layers) |
| D10 | Coverage = **matrix over declared routes × status × source**, plus `unknown_unbounded` | "All URLs" is undecidable for input-driven routing | A single "% covered" | confirmed (reasoning, flagged) |
| D11 | Re-run diffs key on `(route_template, skeleton_hash, overlay, principal)`, never on state index | Crawl order is nondeterministic | Screenshot or index diffing | likely |
| D12 | Every crawl records `politeness` + **degraded-response detection** | Measured: rapid navigation got Hacker News to serve a **6-AX-node** page where the real page has **887** — a naive crawler files that as a legitimate state | Ignore rate limiting | confirmed (own measurement) |

---

## 1. Prior art — read this before designing anything

### 1.1 Crawljax / Mesbah et al. — the canonical state-flow graph [3]

- **Model:** nodes = UI states (DOM instances), edges = events on clickables. Still the right model.
- **State comparison (verbatim):** *"calculating the edit distance between two DOM-trees … using the Levenshtein method. A similarity threshold τ is used under which two DOM trees are considered clones."*
- **Backtracking (verbatim):** *"a dynamically changed DOM state does not register itself with the browser history engine automatically, so triggering the 'back' function of the browser usually does not bring us to the previous state. Saving the whole browser state is also not feasible"* — hence replay-from-root, with Dijkstra shortest-path as the optimisation.
- **Unit of parallel work (2012):** *"bringing the browser back into a given state and exploring the first unexplored candidate state from that state."* Use exactly this as `browserd`'s job unit.
- Weaknesses to fix: XPath as element identity (brittle under re-render), Levenshtein over serialised DOM (O(n²), semantically blind), WebDriver dependency (disqualifying).

### 1.2 Memon's GUI ripping / event-flow graphs — the desktop ancestor [5]

GUITAR (Memon et al., WCRE'03 → ASE journal 2014) established the pattern Crawljax later ported to the web: a **ripper** drives the app to extract a GUI structure (a forest of windows/widgets) plus an **event-flow graph** (EFG) whose edges mean "event *b* may be performed immediately after event *a*", and test cases are then generated as paths through the EFG. Two transferable ideas: (a) separating *structure extraction* from *behaviour model*, which maps to our `page tree` vs `site graph` split; (b) EFG edges are a **may-follow relation**, not observed traversals — which is exactly our `inferred` edge status. GUITAR's known failure is combinatorial blow-up of event sequences; the web analogue is our budget system.

### 1.3 Yandrapally, Stocco & Mesbah, *Near-Duplicate Detection in Web App Model Inference*, ICSE 2020 [1] — **the most important source for state identity**

I extracted the PDF text locally and read the tables. Hard numbers:

- **Dataset:** 493k webpage pairs from 6,000+ websites; a labelled random sample of 1,000 state-pairs → **441 clones, 275 near-duplicates, 284 distinct**. Near-duplicates split into **Nd1 = 45** (background/image changes), **Nd2 = 219** (dynamic data), **Nd3 = 11** (duplicated functionality).
- **Algorithms evaluated (10, three domains):** IR — `SimHash`, `TLSH`; web testing — `RTED` (tree edit distance), `Levenshtein` (DOM string), string equality; vision — `BlockHash`, `pHash`, `Hyst` (histogram), `PDiff`, `SSIM`, `SIFT`.
- **State-pair classification F1 (Table 5, avg over test sets):** PDiff 0.60, BlockHash 0.58, SSIM 0.57, SIFT 0.52–0.53, pHash 0.51, Levenshtein 0.50–0.52, RTED 0.47–0.50, TLSH 0.45–0.48, Hyst 0.44, **SimHash 0.33** (worse than the 0.32 random baseline on the disjoint set). *Every* technique is barely above chance.
- **As a crawler state-abstraction function (Table 9, best 5-min crawls):** RTED F1 **0.66** (recall 0.61 / precision 0.79) — best; Hyst 0.58; Levenshtein and BlockHash 0.54; pHash 0.52; SSIM 0.51; PDiff 0.44; SIFT 0.39. Speed in states/minute: RTED 25, BlockHash 17, pHash/Hyst 16, Levenshtein 11, SSIM 8, SIFT 5, **PDiff 4** — visual methods are 3–6× slower, which itself costs coverage.
- **30-minute crawls (Table 10):** best model F1 **0.62** (RTED).
- **Model decay (verbatim finding):** *"although RTED was able to achieve a high accuracy F1 score of 0.95 initially, the final produced model had only an F1 of 0.45. This deterioration is due to the accumulation of numerous near-duplicates."*
- **Conclusion (verbatim):** *"no technique is able to detect Nd3 near-duplicates leading to poor inferred models"*, and universal thresholds are not feasible — *"the characteristics of web apps cannot be ignored while tuning thresholds."*

**What this means for `brow`, concretely:** (i) any single-number "state identity" claim is marketing; (ii) thresholds must be **per-crawl config, recorded in the artifact**; (iii) model quality *degrades over crawl length*, so the honest product surface is a **coverage + provenance report**, not "here is your app's model"; (iv) duplicated-functionality states (Nd3 — two different screens that do the same thing, e.g. "add user" reachable from two menus) are **undetectable** by every published technique — we must expose them as a manual review queue rather than pretend.

### 1.4 FragGen — fragment-based state abstraction (TSE 2022 / ICSE'23 journal-first) [6]

Same group's answer to their own 2020 result: stop comparing whole pages. FragGen decomposes a page **screenshot into component-level fragments by layout**, and defines state equivalence as a **set comparison over fragment sets**, which also yields fine-grained test oracles. Reported to beat whole-page techniques on near-duplicate detection, model quality and regression usefulness. Transferable design, and it maps cleanly onto our Unified Page Tree: our fragments are already available structurally (containers with ≥k structurally identical children, dialog subtrees, landmark regions) — we can do set-based fragment comparison **without** the screenshot-segmentation step. This is the single most promising v2 upgrade to D1.

### 1.5 The 2026 abstraction study (arXiv 2606.16650) [2]

6 abstractions × 5 tools in 3 strategy families. Retained findings from the prior pass, which independently checked them: Crawljax Judge 54.18 % vs PDiff 39.39 % code coverage (14.79-pt swing from abstraction alone); StringCmp 49.12 % average — *second best on coverage while producing 316 states on Dimeshift where Gestalt produced 17*. So **state count and coverage are near-orthogonal**: tune the signature against the deliverable (site map vs regression diff), not against a coverage number. Neural abstractions (WebEmbed [7], Judge) win but need a model — out of scope for a local-first, no-cloud harness.

### 1.6 Burp Scanner (best-engineered non-academic prior art) [4]

*"a map of the application in the form of a directed graph"*; *"identifies locations based on their contents, not the URL"*; *"either navigates directly from its current location, or reverts to the start location and navigates from there"*; volatile-content re-identification; two-phase (unauthenticated then per-credential) crawling; *"configurable cutoffs that constrain the extent of the crawl."* Steal all of it.

### 1.7 LLM-driven crawlers (2025–2026)

Go-Browse (arXiv 2506.03533) — structured BFS over pages-as-nodes with a *task proposer* + *feasibility checker*; AutoCrawler (arXiv 2404.12753) — generates crawler *rules* rather than acting per page. Firecrawl `/map`, crawl4ai, katana contribute the "known files" pass (robots/sitemap/`.well-known`) and JS-literal scraping, not state modelling.

**Positioning:** `brow` is a **model-based crawler with an LLM in the action-selection seat**. Graph, budget and evidence live in Rust; the agent gets summarised candidate sets and makes semantic calls ("this says *Delete workspace* — skip"). That split is why the coverage report can be honest: the graph is not a hallucination surface.

---

## 2. State identity

### 2.1 Measured candidates (this pass, 16 URLs across 5 origins)

| Signature | How computed | Distinct states / 16 URLs | Cost (6 → 23,573 AX nodes) | Notes |
|---|---|---|---|---|
| `sig_ax_strict` — AX role + raw name | `Accessibility.getFullAXTree` | **16** | 1 → 328 ms | fragments on any text change |
| `sig_ax_bucketed` — role + name-*class* + state bits + depth (L1) | same | **15** | same | merged `/tag/frontend` ≡ `/tag/programming` |
| `sig_dom` — DOMSnapshot skeleton (tag, `role` attr, has-href, text class, depth) | `DOMSnapshot.captureSnapshot{computedStyles:[]}` | **15** | 0 → **74 ms** | same merge; **4.4× cheaper**; works on div soup |
| `sig_aff` — sorted interactive-affordance set (L2) | in-page pass | **16** | ~3 ms | catches enabled/disabled, layout moves |
| route template only (L0) | URL induction | 13 | ~0 | inventory only |

Stability check: each page's L1 signature was captured **twice, 1.5 s apart — 16/16 identical**. Volatility masking is still needed for ad/clock-bearing pages, but on this corpus the AX skeleton with `InlineTextBox` dropped is deterministic.

The one collision is instructive and worth internalising: **`/tag/frontend` and `/tag/programming` on demo.realworld.show hashed identically under both L1 and the DOM skeleton** (110 skeleton lines each) while differing under `strict`. That is the ICSE-2020 **Nd2 (dynamic data)** case. It is *correct* if the deliverable is a route-template map, and *wrong* if the deliverable is per-tag coverage. Hence the match policy must be an explicit knob, and the artifact must record which one was used.

### 2.2 The composite signature

```rust
// crates/crawler/src/state_id.rs
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StateSignature {
    pub route: RouteTemplate,          // L0 — after induction, e.g. "/users/:id/settings"
    pub skeleton: [u8; 16],            // L1 — blake3-128 over AX *or* DOM skeleton (see D2)
    pub skeleton_kind: SkeletonKind,   //      Ax | DomSnapshot  — recorded, part of the key
    pub affordances: [u8; 16],         // L2 — sorted interactive-affordance set
    pub overlay: OverlayStack,         // L3 — modal/drawer/wizard stack
    pub principal: PrincipalId,        // L4 — who is logged in / flag cohort
    pub phash: Option<u64>,            // L5 — off by default (visual policy only)
}

pub enum MatchPolicy { Route, Structural, Strict, Visual, Loose }
```

| Policy | Compares | Use when |
|---|---|---|
| `route` | L0 | page inventory |
| `structural` (**default**) | L0+L1+L3+L4 | general SPA crawling |
| `strict` | L0+L1+L2+L3+L4 | wizards/forms where affordance enablement matters |
| `visual` | `structural` + L5 within Hamming ≤ 6/64 | design-regression crawls |
| `loose` | L0+L3+L4 | huge catalogues |

### 2.3 Skeleton computation, and the div-soup fallback (D2)

**AX skeleton (L1a).** From `Accessibility.getFullAXTree` (EXPERIMENTAL; params `depth`, `frameId` — both optional, `depth` lets you cap server-side), one line per non-ignored node in document order:

```
role[:subrole] | name_class | state_bits | depth
```

- `name_class` ∈ `{empty, numeric, date, currency, email, url, uuid, short_text(≤3 words), long_text}` — never the raw name. This is what stops "500 rows = 500 states".
- `state_bits` from `properties`, filtered **on the value, not the presence** — the AX tree emits `{"name":"invalid","value":{"value":"false"}}` on plain buttons; a name-only filter tags everything `invalid`.
- **Drop all `InlineTextBox` nodes** — they are layout line boxes and change with viewport width and font loading. Measured share of the AX tree: 79/293 (27 %) on a small SPA page, 8,099/23,573 (34 %) on a Wikipedia article.
- Cap depth at 24.

> ⚠️ **Verified 2026-08-04 — the AX skeleton is FRAME-BLIND, and this document never said so.** `Accessibility.getFullAXTree{}` on a page session returns **only the calling frame**, and this includes **same-origin** iframes, not merely OOPIFs. Measured on Chrome 151 with a fixture page containing one same-origin iframe and one cross-origin iframe: the call returned **11 nodes** containing `TopButton` but **not** `SameOriginChildButton` and **not** `Example Domain`; every returned node carried the top frame's `frameId`.
>
> **Consequence for D1/D2, which is severe and unbudgeted:** every state signature computed from `sig_ax_*` **silently ignores 100 % of iframe content**. On any app with an iframed checkout, payment widget, auth wall, embedded editor, chat widget or docs viewer, two genuinely different states — cart vs. payment-entered, logged-out vs. mid-SSO — hash **identically**, because the only thing that changed lives in a frame the signature never saw. This is not the Nd2 dynamic-data collision of §2.1 that the document already accounts for; it is a structural blind spot that produces **false merges with no detectable symptom**, and it invalidates the 36/36 replay result for any target with frames (all three sites measured in §4.6 happen to be frame-light).
>
> **Required fix before the signature is trustworthy.** Stitch, then hash:
> ```
> ax = Accessibility.getFullAXTree{}                              // top frame only
> for n in ax where n.role == "Iframe":                           // has childIds:[] and NO frameId
>     fid = DOM.describeNode{backendNodeId: n.backendDOMNodeId}.node.frameId
>     if fid in Page.getFrameTree():  child = getFullAXTree{frameId: fid}   // same session
>     else:                           child = getFullAXTree{}              // OOPIF: session whose targetId == fid
>     splice child under n
> ```
> Verified pieces: `getFullAXTree{frameId}` for the same-origin child returned its 9 nodes **on the same session**; `DOM.describeNode{backendNodeId}` returned `frameId` for **both** iframes (`contentDocument` present for same-origin, absent for the OOPIF); for the OOPIF, **`frameId` is byte-identical to the iframe target's `targetId`**, which is the join key. **`Page.getFrameTree` from the page session does not list OOPIFs at all** — they arrive only via `Target.setAutoAttach{autoAttach:true, flatten:true}` → `Target.attachedToTarget{targetInfo.type=="iframe"}`. Note §4.3 already requires browser-level auto-attach for the mutation backstop, so the same subscription serves both.
>
> Until stitching lands, the artifact **must** record `frames_elided: <n>` per state so a reader can see the signature is partial. The **DOMSnapshot skeleton (L1b) is not a workaround**: `DOMSnapshot.captureSnapshot` sees same-origin `contentDocument` subtrees but `DOM.getDocument{pierce:true}` did **not** cross into the OOPIF (`"Example Domain"` absent, only the `src` URL present), so it is frame-blind for cross-origin content too — just differently.

**Why a fallback is mandatory, with numbers.** Define `semantic_role_ratio` = share of non-ignored AX nodes whose role is not in `{generic, none, StaticText, InlineTextBox, paragraph, LineBreak, ?}`. Measured this pass:

| Page | AX nodes | semantic_role_ratio |
|---|---|---|
| demo.realworld.show `/login` | 63 | **0.542** |
| demo.realworld.show `/tag/frontend` | 176 | 0.340 |
| nuxt.com docs (4 pages) | 999–2,121 | 0.245–0.353 |
| news.ycombinator.com `/` | 1,614 | 0.308 |
| en.wikipedia.org `/wiki/Rust_(programming_language)` | 23,573 | **0.138** |

Below ~0.25 the AX skeleton is mostly `generic|…`/`StaticText|…` lines, i.e. a **node-count-and-depth signature** — the StringCmp over-fragmentation failure mode, and modals in div soup carry no `dialog` role at all so §4.7 overlay detection silently fails. Therefore compute `semantic_role_ratio` on first visit and pick:

```rust
let kind = if semantic_role_ratio >= cfg.ax_min_semantic_ratio /* default 0.25 */
           { SkeletonKind::Ax } else { SkeletonKind::DomSnapshot };
```

**DOM skeleton (L1b)** — from `DOMSnapshot.captureSnapshot{computedStyles:[]}`, walking `documents[0].nodes` (`nodeName`, `nodeType`, `parentIndex`, `attributes`, `nodeValue` — all string-table indices):

```
tagname | role_attr | has_href | depth        (element nodes)
#text   | short|long | depth                  (non-empty text nodes)
```

Measured cost 0–74 ms vs 1–328 ms for the AX tree on the same pages, and it produced the *same* 15-way partition of the 16-URL corpus. It is the cheaper default for large content pages; AX is the better default for app-shaped pages.

**Affordance set (L2)** — from `a[href], button, input, select, textarea, summary, [role=button|link|menuitem|tab|checkbox|switch], [contenteditable], [tabindex]:not([tabindex="-1"])`, keeping visible, hit-testable elements, emitting `tag | role | name_class | href_template | disabled | grid(x/16,y/16)`, sorted and hashed.

### 2.4 Route-template induction — and why the naive version is wrong (D3)

Priority order: (1) declared templates from §3 win; (2) segment-class induction; (3) query normalisation (sort params, drop `utm_*`, `_`, `cb`, `t`, `v`; learn which params are state-bearing by comparing L1 hashes); (4) keep `#/...` hash routes as path.

I ran induction over the **715 real URLs of nextjs.org/sitemap.xml**:

| Variant | Templates | Compression | Correctness |
|---|---|---|---|
| naive class induction (`:id\|:slug\|:hash`, k=3) | 387 | 1.82× | **wrong** — produced `/docs/pages/:slug/cli`, `/docs/pages/:slug/components/font`, where the "slug" is the static literal `api-reference`/`building-your-application` |
| naive, k=10 | 430 | 1.66× | still wrong |
| **leaf-only + k≥3 + ≥60 % of siblings** | 393 | 1.82× | **20 param templates, all semantically correct** (`/blog/:slug`, `/learn/seo/:slug`, `/docs/app/guides/:slug`, …) |

```rust
fn parameterise(node: &TrieNode, k: usize) -> Option<SegClass> {
    let kids = node.children();
    let by_class = group_by(kids, seg_class);            // :id | :uuid | :date | :hash | :slug
    by_class.into_iter().find(|(_, members)| {
        members.len() >= k
        && members.len() as f32 >= 0.6 * kids.len() as f32
        && members.iter().all(|m| m.is_leaf())           // ← the guard that fixes correctness
        && !members.iter().any(|m| declared_static.contains(m.segment()))
    }).map(|(c, _)| c)
}
```

**Honest caveat the previous draft oversold:** "500 rows → 1 state" is real for catalogue/CRUD apps, but on a docs site template induction buys only **1.8×**. Do not promise order-of-magnitude collapse in general.

### 2.5 State-explosion mitigations, ranked

| Mitigation | Mechanism | Measured / expected effect |
|---|---|---|
| Route templating | §2.4 | 1.8× on docs; large on `/orders/:id`-shaped apps |
| `name_class` bucketing | §2.3 | merged 2 of 16 corpus URLs; kills text churn |
| Repeated-container collapse (proto-FragGen) | parent with ≥k structurally identical children → keep `list_sample_n`=2 + 1 random | biggest win on lists |
| Volatile-subtree masking | capture signature twice ~1.5 s apart; differing subtrees join the mask | measured 0/16 needed it on this corpus; still required for ad/clock pages |
| Fingerprint-before-act | if the target `href_template` is saturated (`visits_per_template`, default 3), skip | avoids "50 nav links, same layout" |
| Depth/breadth budgets | `max_depth` 6, `max_states`, `max_actions`, `max_wall_s` | hard stop |
| Degraded-response detection (D12) | AX node count < 10 % of the same route's median, or known rate-limit text → `blocked{reason:"rate_limited"}`, back off, retry once | **measured need:** HN served 6 AX nodes where the page normally has 887 |

---

## 3. Route extraction, per framework

### 3.1 Empirical table (2026-08-04, Chrome 151.0.7922.72; ✚ = new/changed this pass)

| Target | Framework | Runtime route table | Static route table | Verdict |
|---|---|---|---|---|
| nuxt.com | Nuxt 4 / Vue Router | **YES, complete** — `useNuxtApp().$router.getRoutes()` → **293** | bundle regex → 95.6 % of them | **solved** |
| reactrouter.com | React Router 8 | **PARTIAL** — `__reactRouterManifest.routes` is a route *tree* (7 on landing, 10 after nav) with `*` splats and `/:ref` | ✚ sniff `GET /__manifest?paths=<csv>&version=<hash>` off `Network.requestWillBeSent` → concrete link targets | partial, **manifest length is not a denominator** |
| svelte.dev, sveltesociety.dev, joyofcode.xyz | SvelteKit | NO (`window.__sveltekit_*` = `{base, version}` only) | ✚ **YES, complete** — `_app/immutable/entry/app.*.js` exports `dictionary` = `{"/blog/[slug]":[…], "/(admin)/admin/bulk-import":[…]}`; **23 / 48 / 8 routes**, files 4.4–20 KB | ✚ **solved, statically** |
| nextjs.org, vercel.com | Next.js 16 App Router | NO — `window.next.router` is imperative only; `__NEXT_DATA__` absent | ✚ **NO** — `__next_f` yielded **0 chars**; `app-build-manifest.json` / `routes-manifest.json` **404**; zero `/chunks/app/**`-shaped chunk paths; bundle regex **14/715 = 2.0 % recall** | **unsolved — use sitemap** |
| angular.dev | Angular 22.1 | NO in prod (`window.ng` undefined; `[ng-version]` attribute gives detection only) | untested | detection only |

✚ **The SvelteKit finding is the headline change.** It needs no main-world evaluation, no JS execution and no CDP at all — a plain HTTP fetch of the HTML plus one 4–20 KB JS file:

```rust
// crates/crawler/src/extractors/sveltekit.rs   (HTTP-only, no evaluation, no policy question)
static ENTRY: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r#"[^"'\s]*_app/immutable/entry/app\.[A-Za-z0-9._-]+\.js"#).unwrap());
static ROUTE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r#""(/[^"]{0,120})":\["#).unwrap());        // dictionary keys

// on svelte.dev this yields, verbatim:
// "/", "/(authed)/apps", "/blog", "/blog/[slug]", "/docs", "/docs/[topic]",
// "/docs/[topic]/[...path]", "/e/[code]", "/(authed)/playground/[id]/embed", "/tutorial/[...slug]", …
```

> **Verified 2026-08-04 (independent re-run) — reproduces exactly.** `curl` on `https://svelte.dev/` → entry `./_app/immutable/entry/app.CoLZTV1a.js` (**6,626 bytes**); the `"(/…)":\[` regex yields **23 unique route keys**, including verbatim `/`, `/(authed)/apps`, `/(authed)/playground/[id]/embed`, `/blog/[slug]`, `/docs/[topic]`, `/docs/[topic]/[...path]`, `/e/[code]`, `/tutorial/[...slug]`. The literal token `dictionary` appears once in the file. **The headline claim stands: HTTP-only, no JS execution, no CDP, complete including auth-gated groups.**
>
> Two cautions the draft understates. (1) **The file-size figures in M7 do not reproduce** — M7 records svelte.dev at 4,390 bytes and joyofcode.xyz at 6,626; today svelte.dev is 6,626. Either the bundle changed or the two figures were transposed. Harmless in itself, but it means **the fingerprint you pin a fixture test to is a moving target**, which is the very fragility the risk note calls out. Pin on *extracted route count and key shape*, never on bytes or hash. (2) The `dictionary` object literal is a Vite/SvelteKit **implementation detail with no stability guarantee** — the risk flag is correct and should not be softened by this reproduction. Two successful runs weeks apart is not a compatibility promise; budget a fixture test per Kit major and a loud failure mode when the regex yields 0.

Note two bonuses: SvelteKit layout groups `(authed)`/`(admin)` are **in the key**, so the extractor tells you *which routes are auth-gated before you crawl them* — on sveltesociety.dev it exposed 12 `/(admin)/admin/**` routes that are not linked from the public UI. Those become `declared` + `blocked{reason:"auth"}` rows in the coverage report, which is exactly the honest output. (Gotcha: resolve the entry URL against the document base — `kit.svelte.dev` redirects and a naively concatenated `./_app/...` fetched the HTML page instead, yielding 0 routes.)

### 3.2 Framework-independent sources (do these first)

| Source | Retrieval | Measured yield |
|---|---|---|
| `sitemap.xml` (+ index) | HTTP; `quick-xml 0.41.0` or `sitemap-rs 0.4.0` (the `sitemap` crate is dead, 2020) | **715 `<loc>` in one 100 KB request** on nextjs.org — the *only* viable App Router source |
| `robots.txt` | HTTP; `texting_robots 0.2.2` (stale but fine); `Sitemap:` lines + `Disallow:` as negative evidence | confirmed on nextjs.org |
| **SPA catch-all trap** | ✚ Validate by **content sniff**, never status. Reject any body starting `<!DOCTYPE`/`<html` regardless of status — **including on 4xx** (see correction below) | ✚ measured both directions on live sites |
| SW precache | `ServiceWorker.enable` → `CacheStorage.requestCacheNames{securityOrigin\|storageKey\|storageBucket}` → `CacheStorage.requestEntries{cacheId, skipCount, pageSize, pathFilter}`; **paginate on `returnCount`** | bimodal (prior pass): 249 entries on a Vite PWA (~48 route-like) vs 15 entries / **1** route-like on squoosh.app |
| Web App Manifest | `Page.getAppManifest` → `{url, errors, data, parsed(DEPRECATED), manifest(EXPERIMENTAL)}`; CDP spells it **`startUrl`** | protocol-confirmed |
| Speculation rules | `Preload.ruleSetUpdated{ruleSet}`; better: `Preload.preloadingAttemptSourcesUpdated{loaderId, preloadingAttemptSources}` → resolved URL + triggering `nodeIds` | mechanism confirmed on a fixture; **0 rulesets on 2 live sites** — opportunistic only |
| Bundle-literal regex | over `Network.getResponseBody` of every `Script` response | ✚ **see §3.3 — 95.6 % recall on Nuxt, 2.0 % on Next App Router** |
| Sourcemaps | `//# sourceMappingURL=` → `.map` → `sourcemap 9.3.2` → `sources` often contains `src/routes/settings/+page.svelte` | jackpot when present; stripped on most prod sites |
| OpenAPI/GraphQL | probe `/openapi.json`, `/swagger.json`, `/v3/api-docs`; GraphQL introspection on observed JSON endpoints | produces `api` edges, not routes |

> **Verified 2026-08-04, with one correction that strengthens the rule.** Re-probed all four paths live:
>
> | URL | status | content-type | bytes |
> |---|---|---|---|
> | `nextjs.org/_next/app-build-manifest.json` | 404 | **`text/html`** | 8,197 |
> | `nextjs.org/_next/routes-manifest.json` | 404 | **`text/html`** | 8,197 |
> | `nextjs.org/_next/static/chunks/app-build-manifest.json` | 404 | `text/plain` | 9 |
> | `vercel.com/_next/routes-manifest.json` | **200** | `text/html` | **1,181,486** |
>
> The 404s and the `vercel.com` 200 both reproduce. **Correction:** the draft attributed "404 `text/plain`" to `nextjs.org/_next/routes-manifest.json`; that path actually returns an **8 KB HTML 404 page**. Only the `/chunks/` path returns the 9-byte `text/plain` body. This makes the content-sniff rule *more* important than stated: an HTML body arrives on **both** 200 and 404, so a sniff gated on `status == 200` still admits garbage. Sniff unconditionally. (`nextjs.org/sitemap.xml` re-fetched: 100,117 bytes, **715 `<loc>`** — unchanged.)

### 3.3 Bundle-literal regex — measured precision and recall

Regex used (route-shaped string literals, extension-filtered):

```
["'`](\/(?:[A-Za-z0-9_\-~.]+|\[[^\]\/]+\]|:[A-Za-z0-9_]+)(?:\/(?:[A-Za-z0-9_\-~.]+|\[[^\]\/]+\]|:[A-Za-z0-9_]+))*)\/?["'`]
```

| Target | Corpus | Unique hits | Ground truth | Recall | Precision |
|---|---|---|---|---|---|
| **nuxt.com** | 187 responses, 11.84 MB | 428 | 293 router routes (251 non-asset) | **0.956** (240/251) | 0.561 |
| **nextjs.org** | 37 scripts, 1.85 MB | 45 | 715 sitemap paths | **0.020** (14/715) | 0.311 |

Analysis of the Nuxt run: the **11 missed routes were all parameterised** (`/blog/:slug()`, `/docs/:slug(.*)*`, `/modules/:slug()`) — generated at runtime, never literals. Of the 188 "false positives", **13 were real `/api/*` endpoints** (valuable as `api` edges), 2 were template-literal artifacts (`/[${t}]`), ~8 were junk from unrelated libraries (`/dev/null`, `/dev/shm`, `/dir-name`), and **most of the rest were real concrete URLs** covered by a splat route — i.e. precision against *templates* understates usefulness. Conclusion: keep the regex, always tag results `inferred`, and post-filter with a HEAD/GET content sniff before promoting to `declared`.

Analysis of the Next.js run: App Router simply does not ship route strings to the client. 2 % recall means **the bundle regex must not be advertised as a Next.js route source**.

### 3.4 The isolated-world constraint (unchanged, still architectural)

Verified in the prior pass on 5 frameworks: `Page.createIsolatedWorld` + `Runtime.evaluate{contextId}` sees an **identical DOM node count** but every framework global reads `undefined`. So `inspect.evaluate` (read-only, isolated) **cannot** read `__reactRouterManifest`, `useNuxtApp`, `__vue_app__` or `window.next`. Options, in preference order:

1. **Prefer HTTP-only extractors** — SvelteKit `app.*.js`, sitemap, robots, `__manifest?paths=` sniffing, bundle regex. These need no evaluation at all and dissolve the policy question. ✚ After this pass, that covers SvelteKit (complete), Next.js (sitemap only) and React Router (link-discovery targets).
2. `inspect.routes` — a distinct, **non-parameterised** capability whose payloads are fixed audited strings shipped by us, executed in the main world with `returnByValue:true`, `timeout`, `throwOnSideEffect:true`. Needed only for Vue/Nuxt and React Router objects.

   > **Corrected 2026-08-04:** the draft said "all three are EXPERIMENTAL params on `Runtime.evaluate`". Live `/json/protocol` on Chrome 151 disagrees on one: **`returnByValue` is stable (`experimental: false`)**. `timeout` and `throwOnSideEffect` *are* experimental, as are `generatePreview`, `disableBreaks`, `replMode`, `allowUnsafeEvalBlockedByCSP`, `uniqueContextId` and `serializationOptions`. Stable params on `Runtime.evaluate`: `expression`, `objectGroup`, `includeCommandLineAPI`, `silent`, `contextId`, `returnByValue`, `userGesture`, `awaitPromise`. This matters because `throwOnSideEffect` is the one carrying the safety argument for a read-only capability, and it is exactly the one that is experimental — the drift-detection fixture must cover it specifically.
3. `Page.addScriptToEvaluateOnNewDocument{source, runImmediately}` (main world) snapshotting globals into a DOM channel + `Runtime.addBinding`/`Runtime.bindingCalled` (event is EXPERIMENTAL) for exfil — also the right mechanism for `history.pushState` interception (§4.6).

---

## 4. Crawl execution

### 4.1 Frontier and priority

```rust
struct FrontierItem { state_id: StateId, action: CandidateAction, priority: f32, depth: u16, attempts: u8 }
```

```
p =  3.0 * is_declared_but_unobserved_route     // close the coverage gap first
  +  2.0 * leads_to_unseen_route_template
  +  1.5 * is_nav_landmark(role in {navigation, menubar})
  +  1.0 / (1.0 + template_visit_count)
  -  2.0 * depth_penalty(depth)
  -  4.0 * destructive_score
  -  1.0 * requires_form_fill
  - 10.0 * is_external_origin (unless in scope)
```

Breadth-first by default (Burp's choice, and it avoids the DFS tunnel that a fine-grained abstraction creates); `--strategy bfs|dfs|priority`.

### 4.2 Action selection — which of 200 clickables?

1. **Enumerate** affordances; require visible + hit-testable (`document.elementFromPoint(cx,cy)` returns self/descendant, else it is occluded and clicking is a lie).
2. **Deduplicate** by `(role, name_class, href_template, container_signature)` — typically 200 → 20–40.
3. **Collapse repeated containers** (proto-FragGen, §1.4): keep `list_sample_n`=2 + 1 random.
4. **Predict-and-skip** saturated `href_template`s.
5. **Classify** into `{navigation, disclosure, form, destructive, unknown}`.
6. **Hand ≤ 40 residual candidates to the agent** with `@node-ref`, role, name, predicted class and template; the agent ranks/vetoes. This is where the LLM earns its keep.

Element identity across replays must **not** be XPath. Use a stability-ranked locator chain, recorded per edge and tried in order: `data-testid` → non-generated `id` → `(role, accessible_name, nth-of-role-in-container)` → `container_path + text` → CSS path → absolute XPath. Record which link succeeded; a drop to a weaker link across runs is itself a reportable signal. (My replay harness used the third form — `(name, nth)` over `a[href]` — and it resolved **36/36** times across three sites.)

### 4.3 Avoiding destructive actions

**Layer 1 — lexical/ARIA.** `delete|remove|destroy|purge|drop|purchase|buy|checkout|pay|order|subscribe|cancel subscription|send|publish|post|share|invite|deactivate|suspend|ban|revoke|reset|wipe|transfer|withdraw|refund|confirm|yes, .*` over the **computed accessible name** (not `textContent`), plus `class` matching `danger|destructive`, red filled background, membership in `[role=alertdialog]`, `form[method=post]`, `<a data-method="delete">`.

✚ **Measured blindness of layer 1.** Share of visible interactive elements with *no* accessible name at all (aria-label ∨ text ∨ title all empty):

| Page | interactive | nameless | % | of which icon-bearing |
|---|---|---|---|---|
| demo.realworld.show `/register` | 11 | 5 | **45 %** | 2 |
| demo.realworld.show `/login` | 10 | 4 | **40 %** | 2 |
| demo.realworld.show `/` | 41 | 6 | 15 % | 6 |
| news.ycombinator.com `/` | 228 | 31 | 14 % | 1 |
| nuxt.com docs | 120–158 | 6 | 4–5 % | 1 |
| en.wikipedia.org (3 articles) | 686–2,291 | 12–101 | 2–6 % | 6–18 |

**2–45 % of clickable elements are invisible to any keyword rule** — a hard lower bound on layer 1's false-negative rate before you even consider i18n or euphemisms ("Archive forever"). Report it per crawl.

**Layer 2 — capability gate.** `observe|interact|inspect` never fire a destructive-classified action; `mutate|control` park the job in `waiting_for_approval` with the trigger node, its screenshot and the predicted request.

**Layer 3 — network backstop.** `Fetch.enable{patterns:[{urlPattern:"*", requestStage:"Request"}]}` → on `Fetch.requestPaused`, if `request.method ∈ {POST,PUT,PATCH,DELETE}` and not allow-listed → `Fetch.failRequest{requestId, errorReason:"BlockedByClient"}` (confirmed in `Network.ErrorReason`). **Prior-pass measurement stands and is load-bearing:** a page-session `Fetch.enable` sees `fetch`/XHR/`sendBeacon` (as `resourceType:"Ping"`) but **not** service-worker-originated requests, which reached the server with 200. Fix (verified working): `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}` at the **browser** session and `Fetch.enable` on **every** attached session including `type:"service_worker"`/`"worker"`. Still unsound by construction: mutating GETs, WebSocket-carried mutations (no CDP primitive blocks a frame), `localStorage.clear()`, `indexedDB.deleteDatabase()`, Background Sync, prerender/prefetch server hits.

**Product copy must say "best-effort read-only", never "cannot mutate".**

### 4.4 Form filling

- **Field typing** from `autocomplete` (strongest — it is a machine-readable schema), `type`, `inputmode`, `pattern`, `min/max/step`, `maxlength`, `name`/`id` lexemes, `aria-describedby`, `<datalist>`.
- **Values** from a local deterministic fixture pack seeded by crawl id (`fixtures/form-values.toml`). Never generate real-looking payment data.
- **Validation loop**: fill → blur → wait `networkAlmostIdle` (`Page.lifecycleEvent`) → collect `[aria-invalid=true]`, `:invalid`, `[role=alert]`, new text near the field → regenerate → **max 3 retries** → else `edge{status:"blocked", reason:"validation", messages:[…]}`. Unbounded retry is the classic crawler hang.
- **Required-field discovery**: submit empty once and harvest the error list — usually enumerates every required field in one shot.
- **File inputs**: `Page.setInterceptFileChooserDialog{enabled:true}` + `Page.fileChooserOpened` → `waiting_for_approval` per spec.

### 4.5 Login

`vault` (local `0600` secret file; typed via real `Input.dispatchKeyEvent`, never `value =`), `session_import` (`Storage.setCookies`, approval-gated per spec), `handoff` (park, live view, human logs in, resume). Two-phase crawl per Burp: unauthenticated first (the login form is a discovered node, the wall is an `auth_branch` edge), then per principal. **`principal` is part of the signature (L4)** so `/dashboard` as `alice` ≠ as `bob` — which is how you find authorization bugs. ✚ SvelteKit `(authed)`/`(admin)` route groups (§3.1) let you pre-label which declared routes need which principal.

### 4.6 Back-navigation and restoration — ✚ now measured

Policy unchanged (replay from root, with a learned URL fast path):

```
restore(target):
  if target.restorable_by_url { Page.navigate(target.url); verify signature; return if match }
  path = graph.shortest_path(root, target)              // Dijkstra, weight = measured ms
  Page.navigate(root.url)
  for edge in path {
      resolve(edge.locator_chain); fill(edge.form_values); dispatch(edge.input_event);
      wait_settled();
      if signature() != edge.to.signature { mark edge non_deterministic; abort }
  }
```

✚ **Divergence experiment (the item flagged last pass as the largest unquantified risk).** For each of three live sites I recorded a depth-3 path of real link clicks (`Input.dispatchMouseEvent` press/release at the hit-tested centre), then replayed it from root 4× and compared the L1 signature at every step:

| Site | Shape | Edges replayed | Signature match | Locator resolve failures |
|---|---|---|---|---|
| nuxt.com/docs | Vue SPA docs | 12 | **12** | 0 |
| news.ycombinator.com | server-rendered MPA | 12 | **12** | 0 |
| demo.realworld.show | React/Vue SPA with live API data | 12 | **12** | 0 |
| **total** | | **36** | **36 (0 % divergence)** | **0** |

Signature line counts were identical to the digit across replays (e.g. 827 / 863 / 601 AX skeleton lines on nuxt.com every time). **Interpretation, honestly:** this is the *easy* case — public, read-only, unauthenticated, no single-use tokens, no per-user data, 3 levels deep, minutes apart. It shows the mechanism (locator chain + `Input`-level clicking + AX signature) is sound and that name-bucketing removes enough churn to make replay verification usable. It does **not** establish divergence rates for authenticated CRUD apps with server state, which remains the number to measure in week one against a real target. Also note **one of the 36 "matches" is partly an artifact**: `/tag/frontend` and `/tag/programming` share a signature (§2.1), so a signature match does not prove you are on the intended page — replay verification should additionally assert the **route template** and, when `restorable_by_url` is false, the trigger's own accessible name.

**Cost model** (unchanged from the corrected prior estimate): `states × (root_load + L·(t + settle) + volatility_recheck)` ≈ `300 × (2 s + 6×1.5 s + 1.5 s)` ≈ **62 min** of pure backtracking for a 300-state, depth-6 crawl. Mitigations: state-major work queue, `restorable_by_url` caching (converts most nav states to a single `Page.navigate`), and parallel browser contexts (`Target.createBrowserContext`) per principal.

### 4.7 URL-less states (modals, drawers, wizards, tabs)

First-class nodes with `url` = the underlying document and a non-empty `overlay`. Detection: `[role=dialog|alertdialog]`, `[aria-modal=true]`, `<dialog open>`, top-layer membership (`::backdrop`, or paint order from `DOMSnapshot.captureSnapshot{includePaintOrder:true}` exceeding siblings while `position:fixed` covering ≥40 % of the viewport). Wizards: `[role=tablist]`/`aria-current` movement or `Step \d+ of \d+`. `OverlayStack` = ordered `(role, name_class, top_layer_depth)`. ⚠️ On div-soup pages (semantic ratio < 0.25) **none of the role-based signals fire** — the DOM-skeleton fallback plus the top-layer/paint-order geometry is the only detector, and it is weaker.

### 4.8 New windows, tabs, targets

`Page.windowOpen{url, windowName, windowFeatures, userGesture}` fires on the opener and is the **authoritative** trigger signal; `Target.targetCreated` is best-effort (prior pass: `window.open` in `--headless=new` produced no target event within 4 s). Use `Target.setAutoAttach{autoAttach, waitForDebuggerOnStart, flatten}` so new targets pause before running script. **Two different filters:** graph nodes = `type=="page"` ∧ our `browserContextId`; policy enforcement = *all* attached sessions incl. `service_worker`/`worker` (§4.3). Prior pass measured 5 of 9 auto-attached targets in a fresh headless profile were bundled component extensions. Prerendered documents never surfaced as targets at all — their traffic is invisible to both the graph and the mutation backstop.

### 4.9 Infinite scroll

Self-edge with a saturation counter, not new states. Detect: after a `mouseWheel` `Input.dispatchMouseEvent` to the bottom, `scrollHeight` grew **and** a `Network.requestWillBeSent` fired with unchanged `documentURL`. Bound with `max_scroll_iterations` (5) and `max_items_harvested` (100); record `edge{kind:"scroll_append", iterations, items_added, exhausted}` — `exhausted:false` is a **coverage fact** and must appear in the report. Virtualised lists: constant child count with changing content → `virtualised:true`, do not re-hash per scroll.

### 4.10 ✚ Politeness and degraded responses

Rapid sequential navigation on news.ycombinator.com produced a page with **6 AX nodes and 0 affordances** where the same URL normally yields **887 AX nodes** (verified minutes later in a fresh browser). A naive crawler files that as a legitimate distinct state and, worse, as a dead end. Requirements: per-origin `min_interval_ms` (default 500) and concurrency 1 per origin by default; a **degraded-response detector** (AX node count < 10 % of the route's median, or `<title>` matching known throttle strings, or HTTP 429/503) that yields `blocked{reason:"rate_limited"}`, backs off exponentially, retries once, and **never admits the degraded snapshot as a state**.

---

## 5. Evidence and storage

### 5.1 Per-edge evidence

| Field | Source |
|---|---|
| `trigger` | `{locator_chain, node_ref:"@node-42", doc_generation, role, accessible_name, bbox}` |
| `input` | the dispatched events (`Input.dispatchMouseEvent{type,x,y,button,clickCount,modifiers}`, `Input.dispatchKeyEvent`, touch sequences) |
| `screenshot_before/after` | `Page.captureScreenshot` (+ optional node clip) → content-addressed blobs |
| `network` | `Network.requestWillBeSent` → `loadingFinished` tuples in the action window: `{method, url_template, status, type, initiator.type, duration_ms}`; redirect chains from successive `requestWillBeSent` carrying `redirectResponse` under the same `requestId` |
| `navigation` | `Page.frameRequestedNavigation{frameId, reason, url, disposition}` — `reason ∈ {anchorClick, formSubmissionGet, formSubmissionPost, httpHeaderRefresh, initialFrameNavigation, metaTagRefresh, other, pageBlockInterstitial, reload, scriptInitiated}`, `disposition ∈ {currentTab, newTab, newWindow, download}` (both EXPERIMENTAL, enum verified in Chrome 151); `Page.navigatedWithinDocument{frameId, url, navigationType}` (EXPERIMENTAL) |
| `console` | `Runtime.consoleAPICalled`, `Log.entryAdded`, `Runtime.exceptionThrown` in the window |
| `timing` | monotonic start/end + settle reason (`networkAlmostIdle` / `load` / timeout) |
| `video_offset_ms` | offset into an active recording (action-log sync) |

### 5.2 SQLite schema (`rusqlite 0.40.1`)

```sql
CREATE TABLE crawl(
  id TEXT PRIMARY KEY, started_at INTEGER, finished_at INTEGER,
  seed_url TEXT, scope_json TEXT, budget_json TEXT,
  match_policy TEXT, signature_config_json TEXT,      -- knobs, for interpretability (ICSE'20 lesson)
  browser_version TEXT, protocol_version TEXT, harness_version TEXT);

CREATE TABLE route_template(
  crawl_id TEXT, template TEXT, source TEXT,          -- sveltekit_dictionary|vue_router|react_router_manifest|
                                                      -- react_router_link_discovery|sitemap|robots|sw_precache|
                                                      -- speculation|web_manifest|bundle_regex|sourcemap|observed
  confidence REAL, provenance_json TEXT,
  PRIMARY KEY(crawl_id, template, source));

CREATE TABLE state(
  id TEXT PRIMARY KEY, crawl_id TEXT, first_seen INTEGER, visit_count INTEGER,
  route_template TEXT, url_example TEXT, title TEXT,
  skeleton_kind TEXT, skeleton_hash BLOB, semantic_role_ratio REAL,
  affordance_hash BLOB, overlay_json TEXT, principal TEXT, phash INTEGER,
  is_error INTEGER, http_status INTEGER, degraded INTEGER,
  console_error_count INTEGER, ax_node_count INTEGER,
  screenshot_blob TEXT, ax_blob TEXT, dom_blob TEXT);

CREATE TABLE edge(
  id TEXT PRIMARY KEY, crawl_id TEXT, from_state TEXT, to_state TEXT,   -- to_state NULL when blocked
  kind TEXT,      -- link|spa_nav|form_submit|redirect|new_window|overlay_open|overlay_close|
                  -- scroll_append|api|auth_branch|flag_branch|sw_route
  status TEXT CHECK(status IN ('declared','observed','discovered','inferred','blocked')),
  trigger_json TEXT, evidence_id TEXT,
  deterministic INTEGER, traversal_count INTEGER, mean_ms INTEGER, blocked_reason TEXT);

CREATE TABLE evidence(
  id TEXT PRIMARY KEY, crawl_id TEXT,
  screenshot_before TEXT, screenshot_after TEXT, video_offset_ms INTEGER,
  network_json TEXT, console_json TEXT, navigation_json TEXT);

CREATE TABLE blob(hash TEXT PRIMARY KEY, kind TEXT, bytes INTEGER, path TEXT);
CREATE INDEX edge_from   ON edge(crawl_id, from_state);
CREATE INDEX state_route ON state(crawl_id, route_template);
```

Status semantics — the honesty contract:

| Status | Means |
|---|---|
| `declared` | the app itself claims it exists (SvelteKit dictionary, router table, sitemap, SW precache, speculation rule, `<a href>` in DOM) — **not** traversed |
| `observed` | traversed, before/after evidence captured |
| `discovered` | appeared as a side effect we did not deliberately trigger (redirect, SW-served, `window.open`) |
| `inferred` | static analysis only (bundle regex, sourcemap `sources`) — no runtime confirmation |
| `blocked` | deliberately not traversed (destructive gate, auth wall, CAPTCHA, out of scope, rate limited, network backstop) — `blocked_reason` required |

### 5.3 Export

```json
{
  "schema_version": 1,
  "crawl": {"id":"c_01J…","seed":"https://app.example.com","match_policy":"structural",
            "skeleton_kind":"ax","signature_config":{"ax_min_semantic_ratio":0.25,"depth_cap":24},
            "budget":{"max_states":300,"max_actions":2000,"max_wall_s":1800},
            "browser":"Chrome/151.0.7922.72","protocol":"1.3","harness":"brow 0.1.0"},
  "route_templates":[{"template":"/(admin)/admin/content/[id]","sources":["sveltekit_dictionary"],
                      "declared":true,"observed_count":0,"blocked_reason":"auth"}],
  "states":[{"id":"s_9f2a","route":"/users/:id","url_example":"/users/42","principal":"alice",
             "overlay":[],"console_errors":0,"semantic_role_ratio":0.41,"screenshot":"blob:be31…"}],
  "edges":[{"id":"e_18c","from":"s_home","to":"s_9f2a","kind":"spa_nav","status":"observed",
            "trigger":{"ref":"@node-42","role":"link","name":"Ada","locator":"[data-testid=user-row-42] a"},
            "evidence":"ev_18c","deterministic":true,"mean_ms":410}],
  "coverage":{"…":"§6"},
  "unreached":[{"template":"/billing/invoices/:id","source":"sitemap","reason":"never_linked_from_crawled_states"}]
}
```

Generated views only: `sitegraph.dot` (Graphviz, clustered by route prefix, edge colour by status) and `sitegraph.mmd` (mermaid `stateDiagram-v2`, `--collapse-templates`, capped — mermaid dies past a few hundred nodes).

---

## 6. Coverage honesty

### 6.1 Why "all URLs" is undecidable

An app whose routing depends on runtime data has an unbounded, non-enumerable URL set (`/orders/:id` has as many instances as the database has rows); routes can exist only under a server-side feature flag; a route may require a specific input (coupon, OTP) to become reachable. "Does any input sequence reach route R" is a reachability question over the full client+server program semantics — undecidable in general. *(Reasoning, explicitly flagged as such; the practical consequence is universally accepted in the crawling literature.)* **Never emit "100 % coverage".**

### 6.2 The report

```
ROUTE COVERAGE                                   crawl c_01J…   (stopped: max_states)

  Declared URLs (union of URL-valued sources)         809
    ├─ sitemap.xml                                    715
    ├─ sveltekit_dictionary  (expanded, static ids)     36
    ├─ sw_precache (workbox-precache-v2)                48   [route_like 48 / entries 249]
    ├─ react_router_link_discovery (__manifest?paths=)  27
    └─ speculation_rules                                 3
  Declared ROUTE DEFINITIONS (not URLs — do not sum)    12
    ├─ sveltekit_dictionary, parameterised               8   e.g. /docs/[topic]/[...path]
    └─ react_router_manifest (lazy: LOWER BOUND)         4   incl. 2 splats, 1 :ref

  Observed (visited, evidence captured)               476 / 809   59 %
  Discovered but not declared                          12
  Declared, reachable, not visited (budget)           268
  Declared, unreachable from seed                      41   → listed with last-attempt reason
  Blocked                                              12   → 12 auth (/(admin)/**), 0 destructive, 0 captcha
  Inferred only (bundle regex, unconfirmed)           188   ← never counted as coverage

  UNBOUNDED: 8 route templates are parameterised. Instances visited: 74.
             Total instances: UNKNOWN (data-driven). Instance coverage NOT computed.

  STATE COVERAGE                                     states 214, edges 611
    URL-less states (modal/drawer/wizard/tab)          58  (27 %)  ← invisible to URL-based tools
    States with console errors                         11
    Non-deterministic edges (replay mismatch)           4
    States merged by signature collision               9   ← same signature, different URL
    Degraded/rate-limited responses discarded          3

  MUTATION CONTAINMENT                               posture: read-only (BEST-EFFORT)
    Blocked non-idempotent requests                    14
    Service-worker sessions with Fetch enabled        2 / 2      ← must be N/N
    Mutating-GET requests allowed through              37        ← NOT containable
    WebSocket connections opened                        1        ← NOT observable per-frame
    Interactive elements with no accessible name       9.4 %     ← layer-1 blind spot
```

Four lines make this honest rather than marketing: the URL/definition unit split, `UNBOUNDED`, `Inferred only` being excluded from coverage, and `MUTATION CONTAINMENT`.

---

## 7. Determinism and re-runs

A crawl is not reproducible; the **model** is comparable. Diff two `sitegraph.json` matching on `(route_template, skeleton_hash, overlay, principal)` — deliberately excluding `affordance_hash` and `phash` from the *key* so those changes show as `changed`, not `removed+added`.

| Diff class | Rule | Why it matters |
|---|---|---|
| `state.added` | key in head only | new screen — or new fragmentation (regression in the app *or* in your abstraction) |
| `state.removed` | key in base only | route deleted or newly unreachable — the loud one |
| `state.changed` | same key, different `affordance_hash` | a button appeared/disappeared/became disabled |
| `state.visual_changed` | `phash` Hamming > tol (`--visual`) | design regression |
| `edge.added/removed` | on `(from_key, to_key, kind, trigger.name_class)` | navigation graph change |
| `edge.status_regressed` | `observed` → `blocked`/absent | something started failing |
| `console.new` | normalised error signature new for that state | highest-signal regression class |
| `network.new_endpoint` | `(method, url_template)` new for that state | API surface drift |
| `coverage.delta` | per-source declared/observed deltas | someone deleted a sitemap entry |
| `skeleton_kind.changed` | ax ⇄ domsnapshot for the same route | markup semantics regressed (a11y regression signal, free) |

Flakiness control: with `--stabilise`, re-traverse each edge k=2 times; edges whose destination signature differs are `deterministic=0` and are **excluded from the diff**, reported in a separate `flaky` section. Determinism aids: `Emulation.setDeviceMetricsOverride`, `Network.setUserAgentOverride`, `Emulation.setLocaleOverride` (EXPERIMENTAL) / `setTimezoneOverride` (stable), `--seed` for the fixture RNG. Avoid `Emulation.setVirtualTimePolicy` by default.

---

## 8. Budgeting and job progress

```toml
[budget]
max_wall_s = 1800
max_states = 300
max_actions = 2000
max_depth = 6
max_pages_loaded = 500
max_bytes = "500MB"
per_action_timeout_ms = 15000
list_sample_n = 2
visits_per_template = 3
max_scroll_iterations = 5
per_origin_min_interval_ms = 500   # politeness (§4.10)
```

```json
{"job":"j_7Q…","phase":"exploring","state":"running","elapsed_s":412,
 "budget":{"wall_s":{"used":412,"max":1800},"states":{"used":143,"max":300},"actions":{"used":892,"max":2000}},
 "progress":{"declared_urls":809,"observed":476,"frontier_size":57,
             "new_states_last_60s":4,"actions_per_min":130,"replay_divergence_rate":0.02},
 "current":{"url":"https://app.example.com/users/42","route":"/users/:id","depth":3},
 "eta_hint":"budget-bound: ~24 min remaining at current rate; frontier not empty",
 "notable":[{"t":389,"kind":"console_error","state":"s_9f2a","message":"TypeError: …"},
            {"t":401,"kind":"waiting_for_approval","reason":"destructive","label":"Delete workspace"},
            {"t":404,"kind":"rate_limited","origin":"news.example.com","backoff_s":8}]}
```

The four honest signals are `frontier_size` (growing or draining?), `new_states_last_60s` (still learning?), `replay_divergence_rate` (is the model trustworthy?) and the ratio `observed / declared_urls` — the only progress figure with a real denominator. If `new_states_last_60s == 0` for `stall_s` (120) with a non-empty frontier, surface a **saturation** condition. Progress streams as NDJSON over the unix socket so `browserctl job logs --follow` is a straight pipe.

---

## 9. What we verified empirically

All runs 2026-08-04 against **Google Chrome 151.0.7922.72** (V8 15.1.206.10, Protocol-Version 1.3, **57 protocol domains**), launched `--headless=new --remote-debugging-port=<39000-45000> --user-data-dir=/private/tmp/brow-scr/udd-N`, driven by a ~110-line raw-socket WebSocket CDP client written for this task (no Playwright/Puppeteer/`websockets`). All instances killed afterwards.

| # | What I ran | Raw observation |
|---|---|---|
| M1 | Replay-from-root, 3 sites × depth-3 path × 4 replays, `Input.dispatchMouseEvent` clicks, AX-skeleton verification | **36/36 edges matched, 0 divergences, 0 locator-resolve failures**; identical skeleton line counts every replay (nuxt 827/863/601, HN 201/194/122, realworld 110/138/110) |
| M2 | Signature sweep, 16 URLs / 5 origins | distinct signatures: strict AX **16**, bucketed AX **15**, DOM skeleton **15**, affordance set **16**; the single collision is `/tag/frontend` ≡ `/tag/programming` (Nd2) |
| M3 | Intra-page stability, +1.5 s | **16/16 identical** bucketed-AX signatures |
| M4 | Signature cost | AX `getFullAXTree` 1 ms @6 nodes → **328 ms @23,573 nodes**; DOMSnapshot skeleton 0 → **74 ms** on the same pages (**4.4×** cheaper) |
| M5 | `semantic_role_ratio` | **0.138** (Wikipedia article) → **0.542** (login form); nuxt docs 0.245–0.353; HN 0.296–0.308 |
| M6 | Nameless interactive elements | **2 %–45 %** of visible affordances have no accessible name (`/register` 5/11, `/login` 4/10, HN 31/228, Wikipedia 12–101 of 686–2,291) |
| M7 | SvelteKit static route extraction | `svelte.dev` → **23** routes; `www.sveltesociety.dev` → **48** incl. 12 `/(admin)/admin/**`; `joyofcode.xyz` → **8**; from `_app/immutable/entry/app.*.js` (4,390 / 19,958 / 6,626 bytes). Verbatim keys incl. `/blog/[slug]`, `/docs/[topic]/[...path]`, `/(authed)/playground/[id]/embed` |
| M8 | Nuxt bundle-literal regex vs router ground truth | 187 responses / 11.84 MB → **428 unique hits**; ground truth 293 routes (251 non-asset); overlap **240** → recall **0.956**, precision **0.561**; all 11 misses parameterised; 13 hits were real `/api/*` |
| M9 | Next.js App Router route recovery | nextjs.org: 37 scripts / 1.85 MB → 45 hits, **14/715 sitemap paths = 2.0 % recall**, precision 0.311; `__next_f` concatenated to **0 chars**; `/_next/app-build-manifest.json`, `/_next/routes-manifest.json`, `/_next/static/chunks/app-build-manifest.json` → **404**; **0** `/chunks/app/**`-shaped chunk URLs |
| M10 | SPA catch-all trap, both directions | `vercel.com/_next/routes-manifest.json` → **HTTP 200 `text/html`**; `nextjs.org` same path → 404 `text/plain` |
| M11 | Route-template induction over 715 real sitemap URLs | naive: 387 templates (1.82×) but **semantically wrong** (`/docs/pages/:slug/cli`); **leaf-only + k≥3 + ≥60 %**: 393 templates, **20 parameterised, all correct** |
| M12 | Rate-limit degradation | news.ycombinator.com `/ask` returned **6 AX nodes / 0 affordances** during rapid crawling; **887 AX nodes** in a fresh browser minutes later (title `Ask \| Hacker News`) |
| M13 | Live `/json/protocol` shape checks | `Accessibility.getFullAXTree` EXPERIMENTAL, params `{depth?, frameId?}`; `CacheStorage.requestEntries{cacheId, skipCount?, pageSize?, pathFilter?} → {cacheDataEntries, returnCount}`; `DOMSnapshot.captureSnapshot{computedStyles, includePaintOrder?, includeDOMRects?}`; `Page.getAppManifest → {url, errors, data, parsed, manifest}`; `Page.navigate → {frameId, loaderId, errorText, isDownload}`; `Target.setAutoAttach{autoAttach, waitForDebuggerOnStart, flatten?(exp), filter?(exp)}`; `Fetch.requestPaused` params incl. `resourceType`, `networkId`, `redirectedRequestId`; `Network.ErrorReason` contains `BlockedByClient`; `Page.ClientNavigationReason` = 10 values as listed in §5.1. Domain-level EXPERIMENTAL: `Accessibility`, `CacheStorage`, `Preload`, `DOMSnapshot`, `ServiceWorker`, `Storage` = **true**; `Fetch`, `Target`, `Page` = false |
| M14 | ICSE-2020 near-duplicate paper | PDF fetched and text-extracted locally; all numbers in §1.3 are read from the paper's tables, not memory. ✅ **Independently re-extracted 2026-08-04** (zlib-inflate of the PDF content streams): *"493k pairs of webpages obtained from over 6,000 websites"*; final dataset **493,088 state-pairs** from **1,031 sites / 29,704 states**; labelled `RS` = 1,000 pairs → *"found 441 clones, 275 near-duplicates (45 Nd1, 219 Nd2, 11 Nd3)"* ⇒ 284 distinct. Decay quote verbatim: *"although RTED was able to achieve a high accuracy F1 score of 0.95 initially, the final produced model had only an F1 of 0.45."* Nd3 quote verbatim: *"no technique is able to detect Nd3 near-duplicates leading to poor inferred models."* **All §1.3 figures confirmed against the primary PDF.** |
| M15 | crates.io versions (live API) | `petgraph 0.8.3`, `rusqlite 0.40.1`, `blake3 1.8.5`, `image_hasher 3.1.1`, `quick-xml 0.41.0`, `sourcemap 9.3.2`, `url 2.5.8`, `regex 1.13.1`, `serde_json 1.0.151`, `similar 3.1.2`, `tokio 1.53.1`, `indexmap 2.14.0`, `dashmap 6.2.1`, `ahash 0.8.12`, `image 0.25.10`, `redb 4.1.0`, `sitemap-rs 0.4.0` (2025-08-28), `texting_robots 0.2.2` (2023 — stale) |

**Retained from the earlier verification pass (not re-run today, still believed):** isolated worlds cannot read framework globals (5/5 frameworks, identical DOM node counts); React Router `__manifest?paths=<csv>&version=<hash>` → 200 JSON while `?p=` → 204; RR manifest 7→10 routes against 351 on-page links; SW precache bimodality (249 entries on a Vite PWA vs 15/1 route-like on squoosh.app); `Fetch.enable` on a page session misses service-worker `fetch()` entirely, catches `sendBeacon` as `resourceType:"Ping"`, and the auto-attach fix works; 5 of 9 auto-attached targets are bundled component extensions; prerendered documents never surface as targets; `DOM.getDocument{pierce:true}`, `Accessibility.getFullAXTree` **and** `DOMSnapshot.captureSnapshot` all see through closed shadow roots; 38 of 57 protocol domains are EXPERIMENTAL.

---

## 10. Limits and impossibilities — read twice

1. **State identity is not solvable to a high standard.** Best published state-pair F1 ≈ **0.60**; best crawler-model F1 ≈ **0.66**, decaying to 0.45–0.62 as the crawl grows [1]. Whatever we ship will mis-merge and mis-split. The deliverable must therefore be *coverage + provenance + a review queue*, not "your app's model".
2. **Nd3 near-duplicates (same functionality, different screens) are undetectable** by every technique in the literature. Do not claim de-duplication of duplicated functionality.
3. **Next.js App Router does not expose routes at runtime.** Measured 2.0 % recall from bundles, empty `__next_f`, 404 manifests. The only source is `sitemap.xml` (or the repo). Any product copy promising framework-aware Next.js route extraction is false.
4. **Production Angular exposes nothing** (`window.ng` undefined); `[ng-version]` gives detection only.
5. **React Router's manifest is partial by design** (`routeDiscovery.mode:"lazy"`) and is a route *tree* with splats — **never** a coverage denominator.
6. ✚ **SvelteKit is solved — but statically.** It depends on the bundler layout (`_app/immutable/entry/app.*.js` + a `dictionary` object literal), which is a Kit/Vite implementation detail with no stability guarantee. Pin a fixture test per Kit major and expect it to break.
7. **`inspect.evaluate` in an isolated world cannot read framework globals.** Either route extraction gets a privileged fixed-payload path, or (better, where possible) it uses HTTP-only extractors.
8. **Coverage over parameterised routes is meaningless** and we must refuse to compute it.
9. **Destructive-action detection is unsound at every layer.** Layer 1 is blind to **2–45 %** of clickables (no accessible name at all) plus i18n and euphemism; layer 3 misses service-worker traffic unless every target is attached, and by construction misses mutating GETs, WebSocket frames, `localStorage.clear()`, `indexedDB.deleteDatabase()`, Background Sync and prerender traffic. Say **"best-effort read-only"**.
10. **Replay-based restoration is O(depth) and can fail.** Measured 0 % divergence on three public read-only sites — that is the *easy* case and does not generalise to authenticated CRUD with single-use tokens. Budget the week-one measurement on a real target before committing to exploratory crawling.
11. **A signature match is not proof of location** — measured collision between two different tag pages. Verify route template *and* signature on replay.
12. **Crawlers get rate-limited and silently record garbage states** (measured: 6 vs 887 AX nodes). Without degraded-response detection the graph is quietly wrong.
13. **Crawls are not reproducible.** Only the model diff is meaningful, and only with flakiness suppression.
14. **CAPTCHA, OS permission dialogs, Keychain, Touch ID, browser chrome** — out of scope; each terminates a branch as `blocked` + human handoff.
15. **The protocol surface we depend on is mostly EXPERIMENTAL** (`Accessibility`, `DOMSnapshot`, `CacheStorage`, `Preload`, `ServiceWorker`, `Storage`, `LayerTree` domains — **38 of 57 domains total, re-counted 2026-08-04**; `Page.navigatedWithinDocument`, `Page.frameRequestedNavigation`, `Runtime.bindingCalled` events; `Runtime.evaluate{throwOnSideEffect,timeout}` — but **not** `returnByValue`, which is stable; `Emulation.setLocaleOverride` is experimental while `setTimezoneOverride` and `setDeviceMetricsOverride` are stable). Every one needs a fixture test that fails loudly on drift, and every artifact records `Browser` + `Protocol-Version`.

16. ⚠️ **The AX-based state signature does not see inside any iframe** — same-origin or cross-origin. Measured 2026-08-04 (§2.3): `Accessibility.getFullAXTree{}` returned 11 nodes for a page whose same-origin child frame contained a labelled button, and that button was absent. Any target with an embedded checkout, auth widget or editor will **false-merge states with no symptom**, and the DOMSnapshot fallback is blind to cross-origin frames too. This is a correctness bug in D1/D2 as written, not a coverage gap — it must be fixed by per-frame stitching (algorithm in §2.3) or declared with `frames_elided` per state. The 0/36 replay result in §4.6 was measured on frame-light sites and does **not** speak to this.

17. **OOPIFs are invisible to `Page.getFrameTree`.** Measured: a page session's frame tree listed only the top frame and its same-origin child; the cross-origin child appeared only as a `Target.attachedToTarget{type:"iframe"}` event under `Target.setAutoAttach`. Any crawler logic that enumerates frames via `getFrameTree` alone will silently under-report the page.

---

## 11. Open questions for the owner

1. **Isolated vs main world for route extraction** — accept an `inspect.routes` capability running *our* fixed strings in the main world? After this pass it is needed only for Vue/Nuxt and React Router; SvelteKit/Next/sitemap paths are HTTP-only.
2. **Default match policy** — `structural` (recommended) vs `strict`; and default `ax_min_semantic_ratio` (proposed 0.25).
3. **Repo-aware mode?** With the project source dir, Next.js/Angular route extraction becomes trivial and complete (`.next/routes-manifest.json`, `src/app/**/page.tsx`, `Router.config`). Big capability delta, new filesystem-scope policy question. Given M9, this is the *only* honest fix for Next.js.
4. **Mutation posture default** — POST/PUT/PATCH/DELETE blocking ON by default for crawl jobs, with `--allow-mutations <glob>`? (Recommended yes.)
5. **Parallelism** — one browser context per principal, or per frontier branch? Target machine profile?
6. **Blob retention** — DOM/AX blobs are 1–3 MB/state; 300 states ≈ 1 GB. Store only for anomalous states, or `zstd` + retention policy?
7. **Agent-in-the-loop budget** — a 2,000-action background crawl cannot round-trip to the agent per action. Proposal: agent sets policy up front (priorities, veto lexemes, form fixtures) and is consulted only on `unknown` classifications and approval gates. Confirm.
8. **FragGen-style fragment sets as the v2 abstraction** (§1.4) — worth a spike now that we know whole-page hashing tops out at F1 ≈ 0.66?
9. **Do we ship a "review queue" surface** for suspected Nd3 duplicates and signature collisions, or silently accept them?

---

## 12. Sources

1. Yandrapally, Stocco, Mesbah, *Near-Duplicate Detection in Web App Model Inference*, ICSE 2020 — https://tsigalko18.github.io/assets/pdf/2020-Yandrapally-ICSE.pdf (PDF fetched and text-extracted locally); DOI https://dl.acm.org/doi/abs/10.1145/3377811.3380416
2. *Understanding Automated Web GUI Testing: An Empirical Study Across Exploration Strategies and State Abstractions*, arXiv 2606.16650 — https://arxiv.org/html/2606.16650
3. Mesbah, van Deursen, Lenselink, *Crawling Ajax-Based Web Applications through Dynamic Analysis of User Interface State Changes*, ACM TWEB — https://people.ece.ubc.ca/amesbah/resources/papers/tweb-final-old.pdf
4. PortSwigger, *Crawling* (Burp Scanner docs) — https://portswigger.net/burp/documentation/scanner/crawling
5. Memon et al., *GUITAR: an innovative tool for automated testing of GUI-driven software*, Automated Software Engineering 21(1) — https://link.springer.com/article/10.1007/s10515-013-0128-9 ; *GUI Ripping: Reverse Engineering of GUIs for Testing*, WCRE'03 — https://www.cs.umd.edu/~atif/pubs/MemonCohenTutorial2013.pdf
6. Yandrapally & Mesbah, *Fragment-Based Test Generation for Web Apps*, IEEE TSE (ICSE'23 journal-first) — https://ieeexplore.ieee.org/document/9765776/ ; preprint https://www.alphaxiv.org/abs/2110.14043
7. *Neural Embeddings for Web Testing* (WebEmbed), arXiv 2306.07400 — https://arxiv.org/pdf/2306.07400
8. *Judge: Effective State Abstraction for Guiding Automated Web GUI Testing*, ACM TOSEM — https://doi.org/10.1145/3736162
9. *Go-Browse: Training Web Agents with Structured Exploration*, arXiv 2506.03533 — https://arxiv.org/pdf/2506.03533
10. *AutoCrawler: A Progressive Understanding Web Agent for Web Crawler Generation*, arXiv 2404.12753 — https://arxiv.org/html/2404.12753v1
11. React Router, *Lazy Route Discovery* — https://reactrouter.com/explanation/lazy-route-discovery
12. Chrome DevTools Protocol (tot) — https://chromedevtools.github.io/devtools-protocol/ (all protocol facts here read from the **live** `/json/protocol` of Chrome 151.0.7922.72)
13. Chrome for Developers, *Precaching with Workbox* — https://developer.chrome.com/docs/workbox/precaching-with-workbox
14. Next.js App Router docs — https://nextjs.org/docs/app
15. Koppula et al., *Learning URL Patterns for Webpage De-duplication*, WSDM 2010 — http://www.wsdm-conference.org/2010/proceedings/docs/p381.pdf
16. Agarwal et al., *URL Normalization for De-duplication of Web Pages*, CIKM 2009 — https://www.cs.cornell.edu/~hema/papers/sp0955-agarwalATS.pdf
17. crates.io API (versions as of 2026-08-04) — https://crates.io/api/v1/crates/&lt;name&gt;
18. Live measurement targets used this pass: https://svelte.dev, https://www.sveltesociety.dev, https://joyofcode.xyz, https://nuxt.com, https://nextjs.org/sitemap.xml, https://vercel.com/docs, https://demo.realworld.show, https://news.ycombinator.com, https://en.wikipedia.org

---

## Verification pass — 2026-08-04 (adversarial re-check)

An independent pass re-tested this document's load-bearing claims. Environment: Chrome **151.0.7922.72**, `--headless=new --remote-debugging-port=41337`, scratch `--user-data-dir` under `/private/tmp`, driven by a from-scratch RFC6455 CDP client; local fixture server on `127.0.0.1:8731` for the frame and shadow-DOM experiments. All processes killed and the profile deleted afterwards.

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| 1 | ICSE-2020 figures (493k pairs, 441/275/284, 45/219/11, RTED 0.95→0.45, "no technique detects Nd3") | **CONFIRMED** | independently re-extracted from the primary PDF; both key sentences match **verbatim** (see M14) |
| 2 | arXiv **2606.16650** exists and is the abstraction study | **CONFIRMED** | *"Understanding Automated Web GUI Testing: An Empirical Study Across Exploration Strategies and State Abstractions"*, Liu, Yang, Zhang, Xie |
| 3 | FragGen decomposes screenshots into layout fragments, set-based state equivalence, threshold-free | **CONFIRMED (description)** | ICSE-2023 journal-first listing + abstract. **The document's own extrapolation — that our *structural* fragments can substitute for screenshot segmentation — remains unverified reasoning and is still correctly flagged.** |
| 4 | SvelteKit routes fully extractable from `_app/immutable/entry/app.*.js` | **CONFIRMED (reproduced)** | svelte.dev → `app.CoLZTV1a.js`, 6,626 B, **23 route keys** incl. `(authed)` groups and `[...path]`. §3.1 note added: M7's byte sizes do not reproduce; pin fixtures on route shape, not bytes. |
| 5 | Next.js App Router: manifests 404, sitemap is the only source | **CONFIRMED, with a correction** | all three manifests 404; sitemap 715 `<loc>`. **`nextjs.org/_next/routes-manifest.json` returns 404 `text/html` (8,197 B), not `text/plain`** — so content-sniffing must be unconditional, not gated on status 200. |
| 6 | SPA catch-all trap (`vercel.com` 200 `text/html`) | **CONFIRMED** | 200, `text/html`, **1,181,486 bytes** |
| 7 | `Accessibility.getFullAXTree` params `{depth?, frameId?}`, EXPERIMENTAL | **CONFIRMED** | live `/json/protocol` |
| 8 | (unstated in the draft) AX skeleton spans iframes | **REFUTED — new correctness bug** | `getFullAXTree{}` returned 11 nodes and omitted a **same-origin** child frame's labelled button. Silent false-merges. See §2.3 and Limits #16. |
| 9 | (unstated) `Page.getFrameTree` enumerates all frames | **REFUTED** | OOPIF absent from the page session's frame tree; reachable only via `Target.setAutoAttach`. Limits #17. |
| 10 | `Runtime.evaluate{returnByValue, timeout, throwOnSideEffect}` "all three EXPERIMENTAL" | **REFUTED for `returnByValue`** | `returnByValue` is stable; `timeout` and `throwOnSideEffect` are experimental |
| 11 | 38 of 57 protocol domains EXPERIMENTAL; `Accessibility`/`CacheStorage`/`Preload`/`DOMSnapshot`/`ServiceWorker`/`Storage` true, `Fetch`/`Target`/`Page` false | **CONFIRMED** | live `/json/protocol`; `LayerTree` is also experimental, `Emulation`/`Input`/`DOMDebugger` are not |
| 12 | `Network.ErrorReason` contains `BlockedByClient` | **CONFIRMED** | full enum read from the live protocol |
| 13 | `Emulation.setLocaleOverride` EXPERIMENTAL, `setTimezoneOverride` stable | **CONFIRMED** | live `/json/protocol` |
| 14 | Closed shadow roots visible to CDP, not to page JS | **CONFIRMED** | page JS `shadowRoot` → `false`; `DOM.getDocument{pierce:true}`, `getFullAXTree`, `DOMSnapshot.captureSnapshot` all see it. Note `pierce:false` also saw it on Chrome 151. |
| 15 | crates.io versions in M15 | **CONFIRMED** | 10 of 10 spot-checked match exactly (`petgraph 0.8.3`, `rusqlite 0.40.1`, `blake3 1.8.5`, `image_hasher 3.1.1`, `quick-xml 0.41.0`, `sourcemap 9.3.2`, `tokio 1.53.1`, `serde_json 1.0.151`, `regex 1.13.1`, `chromiumoxide 0.9.1`) |
| 16 | Replay-from-root divergence 0/36 | **NOT REPRODUCED — flag stands, and is now worse** | Not re-run (needs the same three live sites and 4× replay). The document's own caveat is correct. **Added risk:** because the signature is frame-blind (#8), a "match" on any frame-bearing page is weaker evidence than the 36/36 suggests. Treat 0 % as an upper bound on the easy case only. |
| 17 | Bundle-regex recall (95.6 % Nuxt / 2.0 % Next) | **NOT RE-RUN** | single-site-per-framework evidence; flag stands unchanged |
| 18 | `semantic_role_ratio < 0.25` switch point; 2–45 % nameless interactives | **NOT RE-RUN** | eyeballed threshold with no validation set; flags stand unchanged |

**Highest-value change from this pass:** claim #8. The frame-blindness of `getFullAXTree` is a *correctness* defect in the core state-identity design (D1/D2), it was not mentioned anywhere in the document, and it is invisible in testing unless you deliberately point the crawler at a framed app. Fix or declare it before the first authenticated-CRUD measurement, otherwise that measurement will be uninterpretable.
