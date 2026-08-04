# Capture: screenshots, node/frame/region shots, screencast video, diffing

> **Bottom line.** Everything the spec asks for is achievable with plain CDP, and I verified most of it against the local Chrome 151.0.7922.72 rather than trusting folklore. Three results overturn common belief: (1) `position:fixed` elements are **not** duplicated in a `captureBeyondViewport` full-page shot — they render exactly once at `y == scrollY`, so the only fix needed is "scroll to 0 first"; ~~(2) there is **no 16384 px texture ceiling** on `Page.captureScreenshot` in 2026 — I captured a 756×100000 CSS-px page headless and a 1600×200000 device-px PNG headful; the real limit is `W*H*4` bytes of RAM;~~ **(2) REFUTED 2026-08-04 — the 16384 px ceiling is alive and it fails silently.** Above 16384 **output (device) pixels** in either axis, `Page.captureScreenshot` returns an image of the *requested* dimensions whose content is the first 16384 rows/columns **tiled and repeated**; everything past 16384 is fabricated. No error, no truncation, so a dimension check passes. Verified in both headless and headful on Apple Silicon M4 (§1.4); (3) `Page.screencastFrame.metadata.timestamp` is **wall-clock epoch seconds**, measured 1.5 ms from the host's own clock, which makes action↔video sync a solved problem instead of the usual mess. The two genuine landmines are coordinate spaces — `DOM.getContentQuads` is **viewport-relative** while `captureScreenshot.clip` is **page-absolute**, so every node shot must add the scroll offset or it silently breaks the moment the page scrolls — and OOPIFs, where `pierce:true` does not cross the boundary and `Page.captureScreenshot` is flatly rejected on non-top-level targets, forcing a manual offset chain through `DOM.getBoxModel(iframe).content`. Lazy-loaded images below the fold are **not** loaded by `captureBeyondViewport` and come out blank; that is unfixable without scroll-priming. Recommend: own-drawn annotations (never `Overlay`), ffmpeg-via-sidecar as rung 1 of a 3-rung video ladder, and a JSON action log as the timing source of truth (video PTS quantizes to ~20 ms).

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| Full page = `scrollTo(0,0)` + `captureBeyondViewport:true` + explicit `clip` from `cssContentSize` | Verified: renders whole page, fixed elements once at top, no viewport resize needed | `Emulation.setDeviceMetricsOverride` to content height — triggers resize/media-query/relayout side effects, mutates page state | **confirmed** |
| Node shot: `DOM.getContentQuads` → union bbox → **add `cssLayoutViewport.pageX/pageY`** → `clip` | Verified quads are viewport-relative, clip is page-absolute; without the add, shots break after any scroll | Using quads directly (works only at scroll 0); `getBoxModel` (collapses fragmented inlines) | **confirmed** |
| Use `DOM.getBoxModel(iframe).content` — not `getContentQuads` — for the frame offset chain | Verified: `getContentQuads` on an `<iframe>` returns the **border** box; off by the border width | `getContentQuads` on the iframe element | **confirmed** |
| Never expose `fromSurface:false` | Verified it renders a different, smaller, view-path image; meaningless headless | Exposing it as a tunable | **confirmed** |
| Annotations drawn by us onto the PNG from known quads | Verified `Overlay.highlightNode` injects a DevTools tooltip that **occludes page content** and auto-places itself → non-reproducible | `Overlay.highlightNode` / `setShowHitTestBorders` | **confirmed** |
| Video = screencast JPEG → ffmpeg concat demuxer with per-frame `duration` + `-fps_mode passthrough` | Verified: preserves all 283 frames; default settings silently collapse 282→103 frames | `-vsync vfr` (quantizes to 25 fps), `-r` on input (destroys durations) | **confirmed** |
| Action log JSON is the timing source of truth; video PTS is for seeking only | Measured max PTS error 19.8 ms vs CDP timestamps — one frame at 60 fps | Deriving action times from video PTS | **confirmed** |
| Clock base = wall-clock epoch seconds (f64); normalise MonotonicTime via `requestWillBeSent.wallTime - .timestamp` | Screencast + Input already use TimeSinceEpoch; only 3 conversions needed | Monotonic base (screencast frames would need conversion) | **confirmed** |
| ffmpeg via `ffmpeg-sidecar` (spawn binary), not `ffmpeg-next` (FFI) | No C linkage, no LGPL linking question, degrades gracefully when absent | `ffmpeg-next` 8.1.0 | likely |
| Diff = own per-pixel mask + connected components; `image-compare` only for the scalar score | Verified: agents need bboxes + %, and our own mask lets us set AA tolerance and ignore-regions. Measured 3.6 ms + 0.6 ms + 0.5 ms on 756×469 | `dssim` (score only, no map), `dify` (0% documented as a lib, CLI-first) | **confirmed** |
| Occlusion: capture what is actually painted, report occluders as metadata | A node shot that hides the modal covering it is a lie to the agent | Clipping out occluders, or raising z-index | reasoned |

---

## 1. `Page.captureScreenshot` — exact surface

Parameter list read directly from **this Chrome's** `/json/protocol` (Chrome 151.0.7922.72), not from the website:

| Param | Type | Default | Status |
|---|---|---|---|
| `format` | `jpeg` \| `png` \| `webp` | `png` | stable |
| `quality` | integer 0–100 | — | stable, **jpeg only** per description (webp quality is not honoured through this param) |
| `clip` | `Page.Viewport` | — | stable |
| `fromSurface` | boolean | `true` | **EXPERIMENTAL** |
| `captureBeyondViewport` | boolean | `false` | **EXPERIMENTAL** |
| `optimizeForSpeed` | boolean | `false` | **EXPERIMENTAL** |

Returns `data` of protocol type `binary` (base64 on the wire).

`Page.Viewport` = `{x, y, width, height, scale}`. The protocol **describes these as DIP**, but empirically they behave as CSS pixels of the layout viewport (identical at `pageScaleFactor == 1`, which is the normal case). Source: [Page domain, tot](https://chromedevtools.github.io/devtools-protocol/tot/Page/).

### 1.1 The scale equation (verified)

```
output_pixels = clip.{width,height} × clip.scale × deviceScaleFactor
```

Measured across `deviceScaleFactor` 1/2/3 with a 200×100 clip:

| dsf | scale=1 | scale=2 | no clip (800×600 override) |
|---|---|---|---|
| 1 | 200×100 | 400×200 | 800×600 |
| 2 | 400×200 | 800×400 | 1600×1200 |
| 3 | 600×300 | 1200×600 | 2400×1800 |

`cssLayoutViewport` stayed 800×600 throughout — i.e. `clip` is in CSS px and `deviceScaleFactor` multiplies on top. Therefore:

- **Native device pixels** (retina-sharp): `clip.scale = 1`.
- **Exactly 1 CSS px per image px** (stable across machines — what you want for diffing): `clip.scale = 1.0 / deviceScaleFactor`.

Because the harness must produce **reproducible** artifacts across a retina laptop and a CI box, `browserctl` should default to pinning `Emulation.setDeviceMetricsOverride{deviceScaleFactor: 1}` for diff-grade captures and expose `--dpr N` for visual-fidelity captures.

### 1.2 Full-page: which combination is correct in 2026

Four variants, page with `cssContentSize` 756×5230:

| Variant | Result |
|---|---|
| no clip, no cbv | 756×469 (viewport only) |
| `captureBeyondViewport:true`, no clip | **756×5230** ✅ |
| `captureBeyondViewport:true` + clip=full contentSize | **756×5230** ✅ (52 026 bytes) |
| clip=full contentSize, `cbv:false` | 756×5230 dimensionally, but **37 677 bytes** — different content |

The last row is the trap: without `captureBeyondViewport`, a clip extending past the viewport still returns an image of the requested size, but the off-screen part is not properly rastered. A direct A/B at `clip{y:3000,h:200}` gave different md5s and byte counts (727 vs 1840 bytes — the `cbv:false` version is nearly-empty). **Always pass `captureBeyondViewport:true` for anything outside the current viewport.**

**Recommended full-page sequence:**

```jsonc
// 1. normalise scroll — see §1.3 for why this is mandatory
{"method":"Runtime.evaluate","params":{"expression":"window.scrollTo(0,0)"}}
// 2. (optional) settle: wait for Page.lifecycleEvent name=="networkIdle" or a rAF tick
// 3. authoritative size
{"method":"Page.getLayoutMetrics"}            // -> cssContentSize {width,height}
// 4. capture
{"method":"Page.captureScreenshot","params":{
   "format":"png",
   "captureBeyondViewport":true,
   "clip":{"x":0,"y":0,"width":<cssContentSize.width>,"height":<cssContentSize.height>,"scale":1}}}
```

Use `cssContentSize`, not the deprecated `contentSize`. `Page.getLayoutMetrics` returns both; `layoutViewport`, `visualViewport` and `contentSize` are all marked **DEPRECATED** in favour of the `css*` variants.

### 1.3 `position:fixed` — the folklore is wrong, but there is still a bug

Widely repeated claim: "fixed headers repeat down a full-page screenshot." **Not true in Chrome 151.** I sampled a column of a full-page PNG for the header's colour at three scroll positions:

| scrollY | red band(s) found in the 756×5230 PNG |
|---|---|
| 0 | `[[0, 49]]` |
| 1500 | `[[1500, 1549]]` |
| 3000 | `[[3000, 3049]]` |

Exactly **one** band, always at `y == scrollY`. So fixed elements are composited once, at the current scroll offset, into page-absolute space.

> **Verified 2026-08-04 — independently reproduced on a different fixture, and the n=1 worry is now n=2.** New page (`position:fixed` 50 px red header, `z-index:9`, over a 5000 px gradient), 800×600 viewport, `deviceScaleFactor:1`, headless with `--disable-gpu`. Full-page capture at each scroll position, then the red band located by decoding column x=400 of the PNG with `ffmpeg -vf crop=1:H:400:0 -f rawvideo -pix_fmt rgb24`:
> ```
> scrollY 0    -> bands [[0, 49]]
> scrollY 1500 -> bands [[1500, 1549]]
> scrollY 3000 -> bands [[3000, 3049]]
> ```
> Exactly one 50 px band, at `y == scrollY`, every time. **No duplication, confirmed twice on independent fixtures.** The `scrollTo(0,0)`-before-capture rule stands. Still untested (and the residual risk the original flagged is real): fixed elements with `backdrop-filter`, `will-change`, or promotion inside a non-root stacking context.

Consequences:

- No repetition bug. ✅
- But capture while scrolled and your header **floats in the middle of the image** over unrelated content. ✗
- Fix is trivial and mandatory: `scrollTo(0,0)` before every full-page capture, and restore the scroll afterwards if the caller cares.
- Same mechanism means the node-shot formula in §2 is **uniform** — it works for static, sticky and fixed elements alike, no special-casing.

`position:sticky` behaved identically (its quad `y0` stayed pinned at 50 across all scroll positions, i.e. viewport-relative, and the page-absolute conversion lands it correctly).

### 1.4 Maximum size — ~~the 16384 limit does not apply~~ **the 16384 limit DOES apply, and it corrupts silently**

> **REFUTED 2026-08-04. This is the most consequential error in this document.** The original test only checked PNG *dimensions*, which are always correct. The *pixels* are not. Fixture: a page of `#111` filler with 200 px colour bands at known y positions (red@0, green@0.25H, blue@0.5H, yellow@0.75H, magenta@H−200); full-page `captureBeyondViewport` capture; the PNG's column x=400 decoded to raw RGB with `ffmpeg -vf crop=1:H:400:0 -f rawvideo -pix_fmt rgb24` and run-length scanned.
>
> ```
> H=16384  dsf=1 scale=1  -> 800x16384  bands at 0,4096,8192,12288,16184   ALL 5 CORRECT
> H=16600  dsf=1 scale=1  -> 800x16600  bands at 0,4150,8300,12450, then RED AGAIN at 16384; magenta MISSING
> H=20000  dsf=1 scale=1  -> 800x20000  bands at 0,5000,10000,15000, RED at 16384; magenta MISSING
> H=200000 dsf=1 scale=1  -> 800x200000 red band repeats at 0,16384,32768,49152,…,196608; green/blue/yellow/magenta ALL MISSING
> ```
> **Rows ≥ 16384 are a verbatim repeat of rows 0..16383.** The image is not blank — it is *plausible wrong content*, which is worse.
>
> **The threshold is in OUTPUT device pixels, i.e. `clip.height × clip.scale × deviceScaleFactor`** — not CSS pixels:
>
> | capture | output px | result |
> |---|---|---|
> | CSS 16384, dsf 1, scale 1 | 16384 | ✅ all bands correct |
> | CSS 16600, dsf 1, scale 1 | 16600 | ✗ wraps at 16384 |
> | CSS 10000, **dsf 2**, scale 1 | 20000 | ✗ wraps at 16384 |
> | CSS 9000, dsf 1, **scale 2** | 18000 | ✗ wraps at 16384 |
> | CSS 20000, dsf 1, **scale 0.5** | 10000 | ✅ **all bands correct, incl. magenta at 9900** |
>
> **It is symmetric and it applies on both axes.** A 20000 CSS-px-wide page captured at dsf 1 wrapped at **x = 16384** with the magenta band at x=19800 missing (same run-length method on row y=200).
>
> **It is not a headless/software-raster artifact.** Identical wrap points (`16384`, `23884`) for a 30000 px page in headless *and* in headful GPU mode on this Apple Silicon M4. The original document's headful "1600×200000 ✅" row is a dimension check that the corruption passes.
>
> **What must change in the implementation:**
> 1. The mitigation is **not** `max_capture_megapixels`. A 800×20000 capture is 16 MP — far under any sane megapixel cap — and it is already corrupt. **Cap `height × scale × dsf` and `width × scale × dsf` at 16384 each.**
> 2. When a full-page capture would exceed it, either (a) auto-reduce `clip.scale` to `16384 / (dim × dsf)` — verified to produce correct content, at the cost of resolution — or (b) tile: N sequential `clip`s each ≤ 16384 output px, stitched by us. Tiling is the only option that preserves resolution.
> 3. Add a **self-check** in `brow doctor`: capture a fixture taller than 16384 output px with a known marker near the bottom and assert it is present. The threshold is a Chromium/GPU implementation detail and could move.
> 4. Every stored full-page artifact should record `capture_tiled: true|false` and the output dimensions, so a corrupt legacy artifact is identifiable after the fact.
>
> The tables and prose below are left as originally written **only** as a record of what a dimension-only check reports. Read them as "no error was returned", never as "the image is correct".

I pushed content height in headless (`--disable-gpu`, software raster) and headful (GPU, dsf=2):

| CSS content height | headless PNG | headful PNG (dsf 2) |
|---|---|---|
| 8 000 | 756×8000 ✅ | 1600×16000 ✅ |
| 16 384 | 756×16384 ✅ | 1600×32768 ✅ |
| 16 500 | 756×16500 ✅ | 1600×33000 ✅ |
| 32 768 | 756×32768 ✅ | 1600×65536 ✅ |
| 65 536 | 756×65536 ✅ | 1600×131072 ✅ |
| 100 000 | 756×100000 ✅ | **1600×200000** ✅ |

No truncation, no error, at any size, in either mode. ~~The classic 16384 px GPU texture limit clearly does not gate this path any more — Chromium rasters full-page captures in tiles.~~ **Corrected 2026-08-04: every row of this table above 16384 output px contains silently fabricated content (blockquote at the top of §1.4). "No error" is not "correct image".** **The secondary limit is memory**: the 1600×200000 case implies a ~1.28 GB RGBA buffer. Headless Chrome did in fact die on me twice during this session while doing very large captures plus many targets (§"What we verified"), so the harness must impose its own ceiling.

**Recommendation (revised 2026-08-04) — two independent caps, both mandatory:**
* **Correctness cap (hard, new):** `clip.width × scale × dsf ≤ 16384` **and** `clip.height × scale × dsf ≤ 16384`. Beyond either bound the image is fabricated (§1.4 blockquote). Auto-**tile** (N clips, stitched by us — preserves resolution) or auto-reduce `clip.scale` to `16384 / (dim × dsf)` (verified to produce correct content, loses resolution). Never emit a single oversized capture.
* **Stability cap (soft, as before):** configurable `max_capture_megapixels` for memory. With the 16384 bound in force the largest single capture is 16384² ≈ 268 MP ≈ 1.07 GB RGBA, so the megapixel cap is still required — it is simply no longer the first thing that bites.

> **Verified 2026-08-04 — the "silently blank at huge sizes" worry, partly addressed, plus a transport fact that removes one imagined constraint.** I captured a **5600×5600 canvas filled with `crypto.getRandomValues` noise** (deliberately incompressible, so content emptiness would be obvious in the byte count): the PNG came back at **94,376,178 bytes** — i.e. ~3.0 bytes per pixel, exactly what real noise costs. A blank or half-blank raster would have compressed to a few KB. So at ~31 MP the pixels are genuinely there, not fabricated. (This does **not** clear the 200,000 px case; the original's spot-check gap stands for extreme aspect ratios.)
>
> **Superseded 2026-08-04:** the 5600×5600 result is real but is *below* the 16384 bound in both axes, which is exactly why it looked fine. It says nothing about the 20000/200000 px cases, which are now known to be corrupt. See the blockquote at the top of §1.4.
>
> The same test settles a cross-document worry from `10-cdp-transport-and-process.md`: that capture's **base64 payload was 125,834,904 bytes** and it crossed the `--remote-debugging-pipe` **intact**, with the browser and the page session both healthy afterwards. Chrome's 100 MB `kReceiveBufferSizeForDevTools` applies only to the fd-3 **reader** (client → Chrome); responses are uncapped. **So there is no transport-level ceiling on screenshot size — only memory.** That makes `max_capture_megapixels` the *sole* defence, which raises its importance rather than lowering it. Encode time was 28.5 s for that single capture, which is its own argument for the cap.

### 1.5 Lazy-loaded images — a real, unfixable-in-place hole

Definitive test: `<img loading=lazy>` placed 9000 px below the fold.

```
before capture:       [complete=false, naturalWidth=0]
full-page capture:    756 x 9654
after capture:        [complete=false, naturalWidth=0]   <-- unchanged
after scrollIntoView: [complete=true,  naturalWidth=200]
```

`captureBeyondViewport` **does not** drive the IntersectionObserver / lazy-load machinery. The image is blank in the full-page shot. Same applies to any `IntersectionObserver`-gated content: skeleton loaders, infinite scroll, animation-on-scroll.

**Mitigation ladder** (must be an explicit, opt-in flag — it mutates page state and fires analytics):
1. `--prime-scroll`: step the viewport down in `0.8 × clientHeight` increments, `rAF`-wait at each stop, then `scrollTo(0,0)` and capture.
2. Wait for `Page.lifecycleEvent{name:"networkIdle"}` after priming.
3. Report `lazy_primed: true|false` in the artifact manifest so the agent knows whether blank regions are real.

There is no way to do this without scrolling. Say so in the SKILL.md.

### 1.6 `optimizeForSpeed`, formats, `fromSurface`

- `format:"webp"` is accepted (enum confirmed). `quality` is **documented** jpeg-only.

> **REFUTED 2026-08-04 — `quality` *is* honoured for webp; only the documentation says otherwise.** The protocol description in Chrome 151's own `/json/protocol` does read `"Compression quality from range [0..100] (jpeg only)."` — but the implementation disagrees. Same 800×600 clip, same page, three quality values:
>
> | format | q=1 | q=50 | q=100 |
> |---|---|---|---|
> | webp | **1,674 B** | **1,856 B** | **51,944 B** |
> | jpeg | 3,863 B | 4,654 B | 55,323 B |
>
> A 31× spread across webp quality settings is not noise. So webp is fully usable as a *controllable* lossy format, and at low quality it is materially smaller than jpeg (1.7 KB vs 3.9 KB at q=1). Recommendation: use `webp` for bulk crawl/thumbnail captures where size dominates, keep `png` for diff-grade artifacts. Because the behaviour contradicts the documented contract it could change without notice — probe it once at daemon start (capture a 2×2 clip at q=1 and q=100, compare sizes) and fall back to jpeg if the sizes match.
- `optimizeForSpeed:true` trades file size for encode latency — appropriate for screencast-adjacent bulk capture, not for stored artifacts. EXPERIMENTAL.
- `fromSurface:false` produced a visibly different and much smaller image (8 400 vs 17 859 bytes for the same page). It is the legacy view-capture path. **Do not expose it.** Default `true` is correct.

---

## 2. Node screenshots — the exact algorithm

This is where naive implementations break. Two facts, both verified, and they disagree:

**Fact A — `DOM.getContentQuads` is VIEWPORT-relative.** Same element (`#under`, CSS absolute top 720):

| scrollY | quad `y0` | `getBoundingClientRect().top` |
|---|---|---|
| 0 | 720 | 720 |
| 400 | 320 | 320 |
| 1000 | **−280** | −280 |

It tracks `getBoundingClientRect()` exactly, including going negative.

**Fact B — `captureScreenshot.clip` is PAGE-absolute.** Identical clip at scrollY 0 and 1000 produced **byte-identical PNGs** (md5 `85b3a04b8338` both times, and `7aec8e0178f3` both times with `cbv:true`). The clip does not care where the page is scrolled.

Therefore **every node shot must add the scroll offset.** Omit it and your code passes every test written at scroll 0 and corrupts silently in production.

### 2.1 Algorithm

```rust
/// Returns a page-absolute clip in CSS px for `backend_node_id`.
async fn node_clip(&self, node: BackendNodeId, opts: &ShotOpts) -> Result<Viewport> {
    // 0. Optionally bring it into view. Changes scroll AND may trigger lazy content.
    //    Skip for fixed/sticky nodes: they are already reachable at any scroll.
    if opts.scroll_into_view {
        self.send("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": node})).await?;
    }

    // 1. Quads in the node's OWN frame, viewport-relative, CSS px.
    //    Multiple quads for fragmented inline elements — verified 3 quads for a
    //    3-line <span>, whereas getBoxModel collapses them into one wrong union box.
    let quads: Vec<Quad> = self.send_in(node.session, "DOM.getContentQuads",
                                        json!({"backendNodeId": node})).await?.quads;
    if quads.is_empty() { bail!("node has no layout box (display:none / detached)"); }

    // 2. Union → axis-aligned bbox. Quads are rotated for transformed elements
    //    (verified: rotate(30deg) scale(1.5) gave a genuinely rotated 8-tuple),
    //    so min/max over all 4 corners, not just corners 0 and 2.
    let mut bb = BBox::empty();
    for q in &quads { for (x, y) in q.corners() { bb.extend(x, y); } }

    // 3. Walk the frame chain, adding each ancestor iframe's CONTENT-box origin.
    //    CRITICAL: getContentQuads on an <iframe> returns the BORDER box.
    //    Verified on a 400x250 iframe with a 10px border:
    //        getContentQuads      -> [70,300 .. 490,570]   (border box, 420x270)
    //        getBoxModel.content  -> [80,310 .. 480,560]   (content box, 400x250)  <-- use this
    let mut frame = node.frame_id.clone();
    while let Some(parent) = self.parent_frame(&frame) {
        // DOM.getFrameOwner must be called on the PARENT's session.
        let owner = self.send_in(parent.session, "DOM.getFrameOwner",
                                 json!({"frameId": frame})).await?.backend_node_id;
        let bm = self.send_in(parent.session, "DOM.getBoxModel",
                              json!({"backendNodeId": owner})).await?.model;
        bb.translate(bm.content[0], bm.content[1]);   // content-box top-left
        frame = parent.frame_id;
    }

    // 4. Viewport-relative -> page-absolute, using the MAIN frame's scroll.
    //    Child-frame internal scroll is already baked into step 1's quads.
    let lm = self.send("Page.getLayoutMetrics", json!({})).await?;
    bb.translate(lm.css_layout_viewport.page_x, lm.css_layout_viewport.page_y);

    // 5. Padding, then snap outward to whole CSS px to avoid a half-pixel seam.
    bb.inflate(opts.padding_css_px);
    Ok(Viewport { x: bb.x0.floor(), y: bb.y0.floor(),
                  width:  (bb.x1.ceil() - bb.x0.floor()).max(1.0),
                  height: (bb.y1.ceil() - bb.y0.floor()).max(1.0),
                  scale: opts.scale })   // 1.0 for device px, 1/dpr for CSS px
}
```

Then:

```jsonc
{"method":"Page.captureScreenshot","params":{
  "format":"png","captureBeyondViewport":true,"clip":<the Viewport above>}}
```

Note step 4 uses `cssLayoutViewport.pageX/pageY`. `cssVisualViewport.pageX/pageY` was identical in all my tests (`pageScaleFactor == 1`). They diverge under pinch-zoom; `getBoundingClientRect` is spec'd against the **layout** viewport, so layout is the correct pairing. *(The pinch-zoom divergence is **UNVERIFIED** — I did not test with `Emulation.setPageScaleFactor`.)*

### 2.2 `getBoxModel` vs `getContentQuads`

| | `getBoxModel` | `getContentQuads` |
|---|---|---|
| Status | stable | **EXPERIMENTAL** |
| Returns | `content`/`padding`/`border`/`margin` quads + `width`/`height` | array of content quads |
| Transforms | transform-aware (verified: returned the same rotated quad) | transform-aware |
| Fragmented inline | **one union box — wrong** (`[0,0 .. 84.8,48]` for a 3-line span) | **3 separate line boxes — correct** |
| iframe element | gives true content box ✅ | gives **border** box ✗ |

**Use `getContentQuads` for the target node** (fragmentation correctness), **`getBoxModel` for iframe ancestors** (content-box correctness). That split is not obvious and is the single most useful thing in this section.

### 2.3 OOPIFs — where the spec's "per-frame screenshot" partly fails

With a genuinely cross-origin iframe (`localhost` vs `127.0.0.1` — different origins, same port):

- `Target.getTargets` shows a separate target of `type: "iframe"`.
- `DOM.getDocument{depth:-1, pierce:true}` from the **main** session does **not** expose its `contentDocument`. Piercing stops at the OOPIF boundary. (It *does* cross same-origin iframes — verified: an in-process iframe's node returned main-frame coordinates `[105,1325]`, exactly `60+5+40, 1200+5+120`.)
- Attaching a session to the OOPIF target and calling `DOM.getContentQuads` returns **frame-local** coords: `[40,120 .. 140,170]` for an element at `left:40; top:120`.
- Adding the parent's iframe content origin `(80,310)` gives `(120,430)` — the correct main-frame position. The offset chain works.

**The hard limit:**

```
Page.captureScreenshot  on an OOPIF session
  -> {"code":-32000,"message":"Command can only be executed on top-level targets"}
```

You **cannot** screenshot an out-of-process frame directly. "Per-frame screenshots" must be implemented as *compute main-frame coordinates, then clip from the top-level session*. This works, but it means:

- A frame shot always includes whatever the parent paints on top of that region (overlays, modals).
- A frame that is scrolled out of the parent's rendered area, `display:none`, or in a cross-origin frame that the parent visually clips, cannot be captured in isolation.
- `Page.getLayoutMetrics` **does** work per-OOPIF-session (returned `cssContentSize` 400×400 for the inner doc), so you can still report frame content size even though you cannot capture it independently.

> **Verified 2026-08-04 — all three legs reproduced on a fresh fixture (300×200 OOPIF with a 10 px border at (70,300) in the parent).**
> ```
> Page.captureScreenshot   on the OOPIF session -> {"code":-32000,"message":"Command can only be executed on top-level targets"}
> Page.getLayoutMetrics    on the OOPIF session -> cssContentSize {width:300, height:200}   OK
> DOMSnapshot.captureSnapshot on the OOPIF session -> OK
> DOM.getBoxModel(iframe).content   (parent) -> [80,310, 380,310, 380,510, 80,510]   == 300x200 CONTENT box
> DOM.getContentQuads(iframe)       (parent) -> [70,300, 390,300, 390,520, 70,520]   == 320x220 BORDER box
> ```
> So the `getBoxModel`-for-iframe-ancestors / `getContentQuads`-for-the-target split in §2.2 is confirmed exactly — the two differ by precisely the 10 px border on all four sides. The screenshot restriction and the per-frame-session availability of `getLayoutMetrics`/`captureSnapshot` are confirmed too. **Still untested: nested OOPIF chains (OOPIF inside OOPIF)** — the offset composition is claimed but only ever demonstrated one level deep.

### 2.4 Elements bigger than the viewport, clipped, occluded

- **Bigger than viewport:** works. A 1500 px-tall element in a 469 px viewport produced a clean 200×1500 PNG with `cbv:true`. No stitching needed.
- **`overflow:hidden` clipping:** `getContentQuads` returns the element's **layout** box, not the visible intersection. A 400×200 child inside a 120×60 `overflow:hidden` parent returned quads for the full 400×200. A shot of those quads shows the child's *unclipped* rect — but the pixels outside the parent's clip are whatever the *page* paints there, not the child. **Fix:** intersect the node bbox with each scroll-clipping ancestor's border box before clipping, and set `clipped_by: "#ancestor"` in the manifest.
- **Occlusion:** verified with a blue `z-index:5` overlay covering a yellow box. The node shot of the covered element shows **the overlay**, because `captureScreenshot` composites the real page.

  **Argue: this is correct and should stay.** The agent's question is "what does the user see at this element?" If a cookie banner covers the button, the agent must know — otherwise it will confidently click an unclickable element. Hiding the overlay would produce a screenshot of a state that does not exist. But the harness must **tell** the agent, not leave it to infer from pixels:

  ```jsonc
  { "node": "@node-42", "occluded": true,
    "occluders": [{"ref":"@node-17","selector":"#cookie-banner","coverage":0.83}] }
  ```

  Detect with `DOM.getNodeForLocation{x,y}` sampled over a grid across the node's quad (I confirmed the method exists and errors cleanly with `"No node found at given location"` when the point is outside the visible viewport — so **scroll the node into view before hit-testing**, and note that hit-testing therefore cannot be done purely page-absolutely).

---

## 3. Annotated screenshots — draw them ourselves

I tested `Overlay.highlightNode` end-to-end. It **does** render into `Page.captureScreenshot` (md5 changed, 17 859 → 27 238 bytes, and reverted *exactly* to the baseline md5 after `Overlay.hideHighlight` — so it is clean and reversible).

But look at what it actually draws (rendered image inspected directly): a DevTools inspector tooltip reading `div#sticky  756 × 30` with an **ACCESSIBILITY** block listing Name / Role / Keyboard-focusable, occupying roughly 163×133 px and **covering page content** in the top-left.

That disqualifies it for artifacts:

| | `Overlay.*` | Draw onto the PNG ourselves |
|---|---|---|
| Occludes page content | **yes** — tooltip is opaque | no |
| Placement | auto, depends on free space | exact, ours |
| Styling | DevTools' (changes between Chrome versions) | pinned, ours |
| Reproducible across versions | no | yes |
| Works on the full-page/off-screen path | only what is composited | yes, we own the pixels |
| Multiple simultaneous annotations | awkward | trivial |
| Extra CDP round-trips + state to clean up | yes | none |

**Recommendation: draw our own.** We already have exact quads from §2. Rasterise with `tiny-skia` 0.12.0 (pure Rust, no C deps) for boxes/arrows and `ab_glyph` 0.2.32 for labels; both are in the sibling-repo idiom (pure-Rust, no system libs). Pipeline: capture PNG → decode with `image` 0.25.10 → convert quads to image space via

```
img_x = (quad_x + pageX - clip.x) * clip.scale * dpr
```

→ stroke → re-encode. `Overlay` stays unused except possibly for interactive human-handoff, which is out of scope for automation anyway.

`Overlay.setShowHitTestBorders`, `setShowFPSCounter`, `setShowPaintRects`, `setShowLayoutShiftRegions`, `setShowScrollBottleneckRects`, `setShowWebVitals` all exist in Chrome 151 (confirmed in the domain listing) and are debug-visualisation toys — useful for a `--debug-paint` diagnostic mode, never for normal artifacts.

---

## 4. Video

### 4.1 `Page.startScreencast` — real behaviour

All EXPERIMENTAL. Params: `format` (`jpeg`|`png` — **no webp**), `quality`, `maxWidth`, `maxHeight`, `everyNthFrame`.

`Page.screencastFrame` gives `{data, metadata, sessionId}` with `ScreencastFrameMetadata = {offsetTop, pageScaleFactor, deviceWidth, deviceHeight, scrollOffsetX, scrollOffsetY, timestamp?}`.

**Measured, 5 s of a `requestAnimationFrame`-animated page, 800×600 jpeg q70:**

| | new headless (`--headless=new`, `--disable-gpu`) | headful |
|---|---|---|
| Frames in ~5 s | 343 | 293 |
| Effective fps | **68.3** | **58.6** |
| Median inter-frame | 13.4 ms | 16.7 ms |
| Min / max inter-frame | 5.5 / 35.7 ms | 1.5 / 19.7 ms |
| Frames carrying `timestamp` | 343/343 | 293/293 |
| Page `rAF` counter at end | 344 | 294 |

**Findings:**

- **Screencast absolutely works in new headless.** The old "screencast doesn't work headless" advice is dead. It is in fact *faster* than headful because it is not vsync-locked; headful's tight 16.7 ms median is the 60 Hz display.
- **Near-zero frame loss under light load**: 343 captured vs 344 rendered. It is damage-driven — a static page emits *nothing*, which is a feature (no wasted bytes) and a hazard (a long pause produces no frames, so your muxer must hold the last frame; the concat `duration` approach in §4.3 handles this automatically).
- **Frame rate is genuinely variable** (5.5–35.7 ms). CFR muxing will desync. VFR is mandatory.
- **`sessionId` is NOT a frame number.** The protocol documents it as "Frame number"; across all 343 frames the value was **constant `1`**. It is a screencast-session id. You must still echo it back, and you **cannot** use it to detect drops. Budget a `frame_index` of your own.

> **Verified 2026-08-04 — reproduced.** 4 s screencast of a `requestAnimationFrame` fixture, jpeg q60, 800×600, headless `--disable-gpu`: **240 frames, `sessionId` distinct values = `[1]`**, 240/240 carrying `metadata.timestamp`, span 3.982 s → **60.0 fps**, median inter-frame **16.7 ms**, max **20.0 ms**. Chrome 151's `/json/protocol` still describes the field as `"Frame number."` — the description is wrong, the observation is right, in two independent runs. Note this run measured a clean 60 fps rather than the original's 68.3 fps headless; effective rate is machine/compositor dependent, so **do not hard-code a frame budget** — report observed fps per recording as the doc already recommends. `Page.screencastFrameAck` was required on every frame (240 acked) for the stream to keep flowing.
> **Verified 2026-08-04 — a hard architectural limit on concurrent recording that this document missed entirely.** **Only one page per browser *window* is `document.visibilityState === "visible"`. Every other tab in that window gets `requestAnimationFrame` at 0 fps and screencast emits nothing.** Measured on headless Chrome 151, a `rAF` counter sampled over 1 s:
> ```
> two pages, same BrowserContext (same window):   A = hidden/0fps    B = visible/60fps
> Target.activateTarget(A):                       A = visible/60fps  B = hidden/0fps   (it swaps, it does not add)
> third page with newWindow:true:                 A = visible/60fps  B = hidden/0fps  C = visible/61fps
> pages in SEPARATE Target.createBrowserContext:  A = visible/60fps  C = visible/61fps  D = visible/60fps  (all visible)
> ```
> `--disable-backgrounding-occluded-windows`, `--disable-renderer-backgrounding` and `--disable-background-timer-throttling` do **not** help. `Emulation.setFocusEmulationEnabled`, `Page.setWebLifecycleState{state:"active"}` and `Page.bringToFront` do **not** un-hide a same-window background tab. `Emulation.setPageVisibilityOverride` does not exist (`-32601`).
>
> **Consequences for `brow`:**
> * **Every concurrently recording job needs its own browser window** — i.e. its own `Target.createBrowserContext` (verified to give a new window) or `Target.createTarget{newWindow:true}`. Two jobs sharing a context = one of them records a frozen page. This turns the "BrowserContext per job" decision in `10-…` §4.3 from an isolation nicety into a **correctness requirement**.
> * A recording of a hidden tab is not merely low-fps, it is **empty** — screencast is damage-driven and a hidden tab produces no damage.
> * `Page.captureScreenshot` **does** still work on a hidden tab, and each capture appears to pump exactly one frame (two shots 0.6 s apart of an rAF animation produced different md5s even at 0 fps). So still-screenshot jobs are unaffected; only video and anything time-based is.
- `Page.screencastFrameAck{sessionId}` is mandatory — without it the stream stalls after a few frames (re-observed: 3 frames then stall).
- Average JPEG at q70/800×600 was ~7.7 KB → **~530 KB/s at 68 fps**. A 10-minute recording is ~320 MB of JPEG before muxing. Use `everyNthFrame` or cap fps for long jobs.
- `format:"png"` screencast is a bad idea: much larger, and during one PNG-screencast test the browser connection dropped (see §"What we verified"). Use jpeg.

### 4.2 Is there a better path in 2026?

I checked each candidate against Chrome 151's actual protocol:

| Path | Verdict |
|---|---|
| **`Tracing` with `disabled-by-default-devtools.screenshot`** | **Not viable for video.** `Tracing.start` gained `screenshotMaxSize` (default **500** px) and `screenshotMaxCount` (default **450**), both EXPERIMENTAL, explicitly clamped to a memory budget. That is a thumbnail filmstrip — 450 frames at ≤500 px. Fine for a perf timeline, useless as a recording. |
| **`HeadlessExperimental.beginFrame`** | Still present in Chrome 151 (`beginFrame`/`enable`/`disable`). Takes `frameTimeTicks`, `interval`, `noDisplayUpdates`, `screenshot`, returns `hasDamage` + `screenshotData`. Requires the target be created with **BeginFrameControl** and `--run-all-compositor-stages-before-draw`. This is the **deterministic** path: you drive virtual time and get exactly one frame per call, perfectly reproducible. Slow, and incompatible with real-time user-gesture playback. **Recommend as an opt-in `--deterministic-video` mode for regression fixtures**, not the default. |
| **In-page `getDisplayMedia`/`MediaRecorder`** | Violates the spec's own rules — needs `mutate.evaluate` to inject a recorder, requires granting display-capture permission (an approval-gated action), records only what the page can see, and pollutes the page. **Reject.** |
| **`chrome-headless-shell`** | Secondary sources report ~20 % better throughput for high-volume work, and that new headless costs 10–15 % more CPU/memory than old headless. **LIKELY**, not verified here. Irrelevant for us: it lacks the full browser stack the spec needs (extensions, real profiles). |
| **OS-level capture (AVFoundation/ffmpeg `avfoundation`)** | Captures the whole screen incl. browser chrome, needs a visible window, breaks headless, needs macOS screen-recording permission. **Reject** — contradicts "background daemon". |

**Conclusion: `Page.startScreencast` remains the right default in 2026**, with `HeadlessExperimental.beginFrame` as a determinism escape hatch.

### 4.3 JPEG stream → scrubbable video (verified recipe)

Chromium hands us timestamped JPEGs and no encoder. The concat demuxer with explicit per-frame `duration` is the correct bridge — but the obvious invocations are wrong.

**Manifest** (`list.ffconcat`, durations from consecutive CDP timestamps):

```
ffconcat version 1.0
file 'f00000.jpg'
duration 0.016713
file 'f00001.jpg'
duration 0.013402
...
file 'f00281.jpg'      # last file repeated — concat needs it to honour the final duration
```

**Measured, 282 input frames spanning 3.989 s:**

| Invocation | frames out | duration | verdict |
|---|---|---|---|
| `-fps_mode vfr` (or legacy `-vsync vfr`) | **103** | 4.120 s | ✗ silently drops 63 % |
| `-r 1000` before `-i` | 283 | **0.283 s** | ✗ input rate overrides durations |
| **`-fps_mode passthrough`** | **283** | **4.040 s** | ✅ |
| `-vf fps=60` (CFR) | 17 | 0.283 s | ✗ (combined with `-r`) |

**The command:**

```bash
ffmpeg -y -loglevel error \
  -f concat -safe 0 -i list.ffconcat \
  -fps_mode passthrough \
  -vf "scale=trunc(iw/2)*2:trunc(ih/2)*2" \
  -c:v libx264 -pix_fmt yuv420p -crf 23 \
  -video_track_timescale 90000 -movflags +faststart \
  out.mp4
```

`scale=trunc(iw/2)*2` is **not optional**: my 756×469 capture failed with `libx264 ... error code: -22 (Invalid argument)` and a zero-byte file because `yuv420p` requires even dimensions. Screencast frame heights are arbitrary. Every naive implementation hits this.

**Timing accuracy of the result** (video PTS vs the CDP timestamps):

```
frames: 283      max error 19.8 ms      mean error 9.7 ms
frame   0: cdp_rel=0.0000  pts=0.0000  (+0.0000)
frame 100: cdp_rel=1.3898  pts=1.4000  (+0.0102)
frame 281: cdp_rel=3.9891  pts=4.0000  (+0.0109)
```

I tested MP4 and WebM/VP9 — **identical 19.8 ms max error**, so this is the concat *demuxer's* timebase quantisation, not the container. Consequence, and it drives a design decision:

> **The video's PTS is accurate to ~20 ms — about one frame at 60 fps. It is good enough to scrub to, and not good enough to be the record of when things happened.** The JSON action log holds full-precision timestamps and is the source of truth.

### 4.4 The no-ffmpeg ladder

The spec forbids hard-requiring ffmpeg. Three rungs, chosen at runtime:

| Rung | Requirement | Output | Effort | Quality / cost |
|---|---|---|---|---|
| **1. ffmpeg** | `ffmpeg` on PATH (probe `-version` at daemon start, cache result) | `.mp4` (H.264) or `.webm` (VP9) | Low — spawn + a manifest. Use **`ffmpeg-sidecar` 2.5.2** (spawns the binary; no C linkage, no LGPL linking question) over `ffmpeg-next` 8.1.0 (FFI) | Best. ~57 KB for 4 s/283 frames at CRF 23; VP9 ~53 KB |
| **2. pure-Rust MJPEG-in-MP4** | none | `.mp4`, `jpeg` codec track, VFR timing | Medium. **`mp4-atom` 0.15.0** (updated 2026-07-31, actively maintained) writes the boxes; we **remux the JPEGs unchanged** — no re-encode, no encoder needed | Plays in Safari/QuickTime/VLC; **Chrome and Firefox will not play MJPEG-in-MP4**. Large: ~7.7 KB × frame count (a 4 s clip ≈ 2.2 MB vs 57 KB). Timing is exact — we write the stts/ctts ourselves, so this rung is actually *more* accurate than rung 1 |
| **3. frame directory + manifest** | none | `frames/f%05d.jpg` + `manifest.json` | Trivial | Always works. Not scrubbable in a player, but **an agent can read it directly** — arguably the most useful rung for our actual consumer |

Deliberately **rejected** for rung 2: `rav1e` 0.8.1 (pure-Rust AV1 — real, but far too slow for hundreds of frames and needs a container anyway), `vpx-encode` 0.6.2 (last updated **2022**, unmaintained), `openh264` 0.9.7 (Cisco C bindings — reintroduces the C dependency we were avoiding), `mp4` 0.14.0 (last updated **2023-08-01**, stale — use `mp4-atom`).

Rung 3 is always written first; rungs 1–2 are a post-pass over it. That way a crash mid-recording still leaves usable artifacts, and `--keep-frames` is free.

### 4.5 Clock normalisation — concrete scheme

CDP genuinely uses three clocks in three units. From this Chrome's protocol JSON:

| Source | Protocol type | Unit |
|---|---|---|
| `Page.screencastFrame.metadata.timestamp` | `Network.TimeSinceEpoch` | **wall seconds since epoch (f64)** |
| `Input.dispatchMouseEvent.timestamp` | `Input.TimeSinceEpoch` | **wall seconds since epoch** |
| `Network.requestWillBeSent.wallTime` | `Network.TimeSinceEpoch` | wall seconds |
| `Network.requestWillBeSent.timestamp` | `Network.MonotonicTime` | **monotonic seconds, arbitrary origin** |
| `Page.lifecycleEvent.timestamp` | `Network.MonotonicTime` | monotonic seconds |
| `Runtime.consoleAPICalled.timestamp` | `Runtime.Timestamp` | **milliseconds since epoch** |

**The key measurement:** I recorded the host's own `time.time()` at the moment frame #60 arrived and compared it to that frame's `metadata.timestamp`:

```
HOST wall clock at frame#60 = 1785863548.832330
CDP  metadata.timestamp     = 1785863548.830813
SKEW                        = 0.0015 s        <-- 1.5 ms, i.e. transport latency
```

**Screencast frames are already on the host's wall clock.** That settles the base clock choice.

```rust
/// One instance per session. Established once, refreshed opportunistically.
pub struct ClockBase {
    /// wallTime - timestamp, from any Network.requestWillBeSent. Seconds.
    mono_to_wall: f64,
    /// Wall-clock epoch seconds of recording start (= first screencast frame).
    t_zero: f64,
}

impl ClockBase {
    /// Rosetta stone: requestWillBeSent carries BOTH clocks for the same instant.
    pub fn observe_network(&mut self, wall_time: f64, monotonic: f64) {
        self.mono_to_wall = wall_time - monotonic;   // EWMA it if you like
    }
    #[inline] fn wall_from_mono(&self, t: f64) -> f64 { t + self.mono_to_wall }
    #[inline] fn wall_from_runtime_ms(&self, t: f64) -> f64 { t / 1000.0 }

    /// The only number that goes in the action log.
    pub fn rel(&self, wall: f64) -> f64 { wall - self.t_zero }
}
```

Rules:
1. `t_zero` = `metadata.timestamp` of the **first** screencast frame. Everything is relative to that.
2. Any `MonotonicTime` (network, lifecycle, paint) → `+ mono_to_wall`.
3. `Runtime.Timestamp` (console) → `/ 1000.0`.
4. `TimeSinceEpoch` (screencast, input) → use as-is.
5. For **our own** actions, stamp with the host's `SystemTime::now()` at dispatch — verified to agree with CDP's wall clock to 1.5 ms. Better: pass an explicit `timestamp` to `Input.dispatchMouseEvent` and log the same value, making the log exact by construction.

So `"00:04.122 click @node-42"` is honest to about **±2 ms** in the log, while seeking the video to it lands within **±20 ms**. Both numbers should be stated in the docs rather than implied to be equal.

### 4.6 Markers / chapters

`browserctl video marker "checkout started"` → append to the action log with `kind:"marker"`, timestamped by the same `ClockBase`.

Store as **one JSONL action log per recording** (append-only, crash-safe, greppable, streamable to `--follow`):

```jsonl
{"t":0.0000,"kind":"recording_start","frame":0,"wall":1785863548.003502}
{"t":0.8273,"kind":"marker","label":"checkout started"}
{"t":0.8288,"kind":"input","action":"click","ref":"@node-42","x":412,"y":690,"button":"left"}
{"t":0.9021,"kind":"network","method":"POST","url":"https://api/checkout","request_id":"1000.4"}
{"t":1.1440,"kind":"console","level":"error","text":"TypeError: ..."}
{"t":1.2077,"kind":"route","from":"/cart","to":"/checkout","kind_detail":"spa_pushstate"}
{"t":3.9891,"kind":"recording_stop","frames":282}
```

Emit chapters for players too — an ffmetadata sidecar passed as a second input to ffmpeg makes markers real MP4/WebM chapters:

```
;FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=827
END=3989
title=checkout started
```

`ffmpeg -i out.mp4 -i chapters.txt -map_metadata 1 -codec copy final.mp4`. Cheap, and makes the artifact navigable in QuickTime/VLC without our tooling. *(UNVERIFIED — I did not run the chapter-muxing step.)*

---

## 5. Image diffing

### 5.1 What an agent actually needs

A diff PNG is close to useless to a model — the spec says so, and it is right. The artifact must be **structured**: a score, bounding boxes, and a classification. The PNG is a secondary attachment for a human.

### 5.2 Crate survey (crates.io, fetched 2026-08-04)

| Crate | Version | Updated | Use |
|---|---|---|---|
| `image` | **0.25.10** | 2026-03-10 | decode/encode. Baseline. |
| `image-compare` | **0.5.0** | 2025-08-18 | scalar similarity score. Well documented. |
| `dssim` | **3.4.0** | 2025-09-21 | best-in-class perceptual (SSIM-based, Kornel's). Score-oriented. |
| `dify` | **0.8.0** | 2026-01-04 | pixelmatch-like CLI; **0 % documented as a library** — do not depend on it as a lib |
| `imageproc` | **0.27.0** | 2026-06-02 | connected components, morphology, if we do not hand-roll |
| `fast_image_resize` | **6.1.0** | 2026-07-21 | fast rescale when dimensions differ |
| `tiny-skia` / `ab_glyph` | 0.12.0 / 0.2.32 | 2026-02 / 2025-09 | drawing annotations & diff boxes |
| `blake3` | **1.8.5** | 2026-04-25 | content-hash dedup |
| `oxipng` | **10.1.1** | 2026-04-22 | optional lossless recompress on GC |

### 5.3 Verified pipeline

I built this for real (`cargo build --release`, `image` 0.25.10 + `image-compare` 0.5.0) and ran it on two actual screenshots from this session — the baseline vs the `Overlay.highlightNode` one:

```
a=(756, 469) b=(756, 469)
image-compare rgb_hybrid_compare score = 0.940414   (3.6 ms)
mask: changed 27398 / 354564 px = 7.7272%           (0.6 ms)
connected components -> 21 regions >=40px           (0.5 ms)
   {"x":0,"y":50,"w":756,"h":30,"px":22680}     <- the highlighted #sticky bar
   {"x":0,"y":81,"w":163,"h":133,"px":2346}     <- the DevTools tooltip
   {"x":20,"y":97,"w":35,"h":11,"px":230}
```

and on two byte-identical images:

```
image-compare rgb_hybrid_compare score = 1.000000
mask: changed 0 / 354564 px = 0.0000%
connected components -> 0 regions
```

The bounding boxes land exactly on the two things that actually changed. **~4.7 ms total for a 756×469 pair** — fast enough to diff every step of a crawl.

Two API traps worth writing down:

1. `rgb_hybrid_compare` returns `Similarity { score, image }` where **`score` is a similarity** (1.0 = identical) but the **per-pixel map is inverted** for hybrid modes (0.0 = identical, 1.0 = maximum difference). The crate's own docs call this out; it is easy to get backwards.
2. `SimilarityImage` is an enum that is **not re-exported at the crate root** (only `RGBSimilarityImage` etc. via `prelude`), so you cannot match on it from outside. Compute the mask yourself.

Computing the mask ourselves is the right call anyway — it is where tolerance and ignore-regions live:

```rust
let tol: i32 = 12;                    // ~5% per channel; absorbs JPEG + antialiasing noise
let d = (0..3).map(|i| (p[i] as i32 - q[i] as i32).abs()).max().unwrap();
if d > tol && !ignore.contains(x, y) { mask[idx] = true; }
```

**Anti-aliasing.** Text re-rendering flips edge pixels by small amounts. Two defences, both cheap: (a) the `tol` threshold above; (b) drop connected components smaller than `min_region_px` (I used 40) — AA noise is scattered 1–3 px specks and vanishes, while real changes survive. A full pixelmatch-style AA classifier (check whether a differing pixel has a neighbour matching the other image) is the fallback if (a)+(b) prove insufficient.

**Ignore regions** for clocks, avatars, ad slots. Accept them as node refs, not just rectangles — resolve `@node-*` → quads → page-absolute rects via §2, so they survive layout shifts:

```jsonc
{"ignore": [{"ref":"@node-9","reason":"live timestamp"},
            {"rect":{"x":0,"y":0,"w":1280,"h":64},"reason":"ad slot"}]}
```

### 5.4 Diff artifact format

```jsonc
{
  "baseline": "sha256:ab12…", "candidate": "sha256:cd34…",
  "dimensions": {"w": 756, "h": 469}, "device_scale_factor": 1,
  "score": {"rgb_hybrid": 0.940414, "changed_pixel_pct": 7.7272},
  "verdict": "changed",              // identical | within_tolerance | changed | size_mismatch
  "regions": [
    {"x":0,"y":50,"w":756,"h":30,"px":22680,"pct_of_image":6.40,
     "likely_nodes":["@node-7"], "label":"#sticky"}
  ],
  "region_count": 21,
  "ignored": [{"ref":"@node-9","rect":{"x":600,"y":12,"w":90,"h":18}}],
  "artifacts": {"diff_png":"…/diff.png","baseline_png":"…","candidate_png":"…"}
}
```

`likely_nodes` is the high-value field: intersect each changed bbox with the Unified Page Tree's quads and name the nodes. That turns "7.7 % of pixels changed" into "the sticky nav and a tooltip changed" — actionable without the agent seeing anything.

**`size_mismatch`** must be its own verdict, not a resize-and-compare. Different dimensions usually mean a layout regression, and silently rescaling hides it.

---

## 6. Storage: naming, dedup, GC

**Content-addressed store + human-readable symlinks.** Screenshots repeat constantly across a crawl (unchanged headers, identical error pages), so dedup is a large real win.

```
~/.local/share/brow/
  blobs/<algo>/<aa>/<aabbcc…>          # blake3, sharded 2 chars; the ONLY real bytes
  sessions/<session-id>/
    jobs/<job-id>/
      manifest.json                    # everything below, indexed
      actions.jsonl                    # §4.6 action log
      shots/0001-navigate-cart.png  -> ../../../blobs/b3/ab/ab12…
      video/recording.mp4
      video/frames/                    # rung 3, or --keep-frames
```

- **Hash:** `blake3` 1.8.5 — faster than SHA-256, and the tree hash lets us hash 200 MB captures incrementally without buffering.
- **Naming:** `<seq>-<verb>-<slug>.<ext>`. Monotonic `seq` gives chronological `ls` ordering; the slug makes it greppable. The symlink carries meaning, the blob carries bytes.
- **Dedup:** hash before write; if the blob exists, only link. For a site crawl with a fixed header this typically collapses a large fraction of near-duplicate viewport shots — *(the exact ratio is **UNVERIFIED**; I did not run a crawl)*.
- **GC:** refcount by scanning symlinks (cheap, no separate index to corrupt). `browserctl gc --older-than 14d --keep-pinned`. Retention policy per artifact class — video is 100–1000× larger than screenshots and should expire far sooner. Pin anything referenced by a `waiting_for_approval` job so evidence is never collected out from under a pending human decision.
- **Manifest** records provenance for every artifact: URL, `document_generation` (the ref-invalidation token from the page-tree design), DSF, viewport, clip, `lazy_primed`, `occluded`, `scroll_normalised`. Without this an agent cannot tell a genuinely blank region from an artifact of capture.

---

## What we verified empirically

Environment: macOS Darwin 25.5.0 (Apple M4), **Google Chrome 151.0.7922.72**, V8 15.1.206.10, rustc/cargo 1.97.1, **ffmpeg 8.1.2**. Chrome launched with a scratch `--user-data-dir` under `/private/tmp/browcap` on high ports (39471/39481/39491/39501/39511); a stdlib-only Python WebSocket CDP client was written for the tests; a local `python3 -m http.server` served crafted fixtures. **All spawned processes were killed at the end (verified 0 remaining).**

| # | What I ran | Raw observation |
|---|---|---|
| 1 | `curl /json/protocol` on Chrome 151 | Authoritative param lists for `captureScreenshot`, `startScreencast`, `ScreencastFrameMetadata`, `Viewport`, timestamp types. Used in preference to the website. |
| 2 | `Page.getLayoutMetrics`, no emulation | `cssContentSize 756×5230`, `cssLayoutViewport 756×469`; deprecated `contentSize`/`layoutViewport` present and equal |
| 3 | 4 full-page variants | viewport-only 756×469; `cbv:true` 756×5230; `cbv:true`+clip 756×5230 (52 026 B); clip-only-no-cbv 756×5230 but 37 677 B (different content) |
| 4 | Fixed-header colour banding at scrollY 0/1500/3000 | Exactly one red band, at `[[0,49]]`, `[[1500,1549]]`, `[[3000,3049]]` → renders once, at `y == scrollY` |
| 5 | Content height 8 000→100 000, headless + headful | No truncation or error anywhere; headful dsf=2 produced **1600×200000** |
| 6 | `clip` vs `deviceScaleFactor` 1/2/3 × `scale` 1/2 | `out = css × scale × dsf` exactly (table in §1.1) |
| 7 | `getContentQuads` on `rotate(30deg) scale(1.5)` | Genuinely rotated 8-tuple `[200.05,186.52, 329.95,261.52, 299.95,313.48, 170.05,238.48]` |
| 8 | 3-line `<span>` | `getContentQuads` → **3 quads**; `getBoxModel.content` → **1** union box `[0,0 .. 84.8,48]` |
| 9 | Same-origin iframe, `pierce:true` | Inner node quads `[105,1325 …]` = `60+5+40, 1200+5+120` → **main-frame-relative** |
| 10 | Cross-origin iframe (`localhost` vs `127.0.0.1`) | Separate `type:"iframe"` target; `pierce:true` returned **0** matches; OOPIF-session quads `[40,120 …]` = **frame-local**; `+ (80,310)` = `(120,430)` ✅ |
| 11 | `getBoxModel` on a bordered iframe | `content [80,310 .. 480,560]` vs `getContentQuads [70,300 .. 490,570]` → quads give the **border** box |
| 12 | `Page.captureScreenshot` on OOPIF session | `{"code":-32000,"message":"Command can only be executed on top-level targets"}` |
| 13 | Quads at scrollY 0/400/1000 | `y0` = 720 / 320 / **−280**, tracking `getBoundingClientRect().top` → **viewport-relative** |
| 14 | Same clip at scrollY 0 vs 1000 | **Byte-identical** PNGs (md5 `85b3a04b8338`, `7aec8e0178f3`) → clip is **page-absolute** |
| 15 | 1500 px element in a 469 px viewport | Clean 200×1500 PNG |
| 16 | `<img loading=lazy>` 9000 px down | `[false,0]` before **and after** full-page capture; `[true,200]` only after `scrollIntoView()` |
| 17 | `Overlay.highlightNode` + screenshot | md5 changed, 17 859→27 238 B, reverted to the exact baseline md5 after `hideHighlight`; rendered image shows a DevTools tooltip (`div#sticky 756 × 30`, ACCESSIBILITY block) **covering page content** |
| 18 | Screencast 5 s, headless vs headful | 343 frames/68.3 fps (median 13.4 ms) vs 293/58.6 fps (median 16.7 ms); `timestamp` on 100 % of frames |
| 19 | `sessionId` across 343 frames | **Constant `1`** — not a frame counter, contradicting the protocol description |
| 20 | Host wall clock vs `metadata.timestamp` | **skew 0.0015 s** → screencast timestamps are host wall-clock epoch seconds |
| 21 | 5 ffmpeg invocations on the same 282-frame manifest | `-fps_mode vfr` → 103 frames; `-r 1000` → 0.283 s; **`-fps_mode passthrough` → 283 frames, 4.040 s** ✅ |
| 22 | libx264 on 756×**469** | `error code: -22 (Invalid argument)`, 0-byte output → even-dimension filter mandatory |
| 23 | Video PTS vs CDP timestamps, MP4 and WebM | max **19.8 ms**, mean 9.7 ms — **identical in both containers** → demuxer quantisation |
| 24 | Rust diff: `image` 0.25.10 + `image-compare` 0.5.0, release build | score 0.940414 in 3.6 ms; mask 0.6 ms; 21 components in 0.5 ms; bboxes landed on the highlighted bar + tooltip; identical images → score exactly 1.000000, 0 regions |
| 25 | crates.io API for 37 crates | Versions in §4.4/§5.2. Notably `mp4` last updated **2023-08-01** and `vpx-encode` **2022-08-31** — both stale |

**Also observed (a caution, not a clean result):** headless Chrome died twice during the session — once after the 100 000 px capture sequence with several accumulated targets, once during a `format:"png"` screencast. I did not isolate root causes, so I will not claim them, but both are consistent with memory pressure and both argue for the caps in §1.4 and for jpeg-only screencast.

---

## Limits and impossibilities

Blunt list. Several of these contradict the spec as written.

1. **Per-frame screenshots of OOPIFs are impossible directly.** `Page.captureScreenshot` is rejected on non-top-level targets. The only implementation is compute-coords-then-clip-from-top, which means a "frame screenshot" always includes anything the parent paints over that region and cannot capture a frame the parent visually hides. The spec's "per-frame" screenshot mode should be documented as *"the region of the top-level page where that frame lives"*.
2. **Lazy-loaded / IntersectionObserver content is blank in full-page shots.** Verified. No parameter fixes it. Only scroll-priming works, and scroll-priming mutates page state, fires analytics, and can trigger infinite scroll. This must be an explicit flag with an honest manifest field, never a silent default.
3. **`captureBeyondViewport` + `position:fixed`**: not repeated (good), but pinned to the current scroll (bad) — mandatory `scrollTo(0,0)`. If the caller forbids scrolling (mid-drag, mid-IME-composition), a correct full-page shot is **not available**; return an error rather than a wrong image.
4. **Video timing is ~20 ms, not exact.** The concat demuxer quantises. Do not promise frame-exact "click at 00:04.122" from the video alone; the JSON log carries the precise number.
5. **`sessionId` cannot detect dropped frames.** It is constant. Under heavy load screencast *will* drop frames (damage-driven, no delivery guarantee) and we can only infer gaps from timestamp deltas — never prove them. Any "video is complete" claim would be a lie; report observed fps and largest gap instead.
6. **Screencast is damage-driven.** A completely static page emits zero frames. Duration must come from explicit start/stop timestamps, never from frame count ÷ fps.
7. **No pure-Rust rung produces a universally-playable video.** Rung 2 (MJPEG-in-MP4) does not play in Chrome or Firefox. A genuinely portable pure-Rust encoder does not exist at usable speed in 2026 (`rav1e` is AV1 and slow; `vpx-encode` is unmaintained since 2022). Without ffmpeg the honest deliverable is frames + manifest. Do not pretend otherwise in the SKILL.md.
8. **Captures above 16384 output pixels in either axis are silently corrupt.** ~~No protocol-level texture limit means nothing stops a 200 000 px capture from allocating >1 GB.~~ **REFUTED/REPLACED 2026-08-04.** The classic 16384 texture bound still applies; it just does not error. `Page.captureScreenshot` returns the requested dimensions and repeats rows/columns 0..16383 for everything beyond. Verified headless *and* headful, on both axes, with the threshold in **output device px** (`dim × clip.scale × deviceScaleFactor`) — `CSS 20000 @ scale 0.5` is correct, `CSS 9000 @ scale 2` is not. This is the single most dangerous fact in this document because the failure is invisible to every check the original text proposed. Cap output dimensions at 16384 and tile. The memory/megapixel cap remains, as a *second* limit: 16384² RGBA is still ~1.07 GB, and `browserd` is a shared daemon where one bad capture can take down every session.
8b. **Concurrent video recording requires one browser window per job.** Only one page per window is `visible`; all other tabs in that window run `requestAnimationFrame` at 0 fps and emit **zero** screencast frames. Not fixable with flags or with `activateTarget`/`bringToFront`/`setFocusEmulationEnabled` (all verified ineffective). Use one `BrowserContext` (or `newWindow:true`) per recording job. Still screenshots are unaffected.
9. ~~**`webp` quality is not controllable** — `quality` is documented jpeg-only.~~ **REFUTED 2026-08-04:** webp quality *is* honoured (1,674 B at q=1 vs 51,944 B at q=100 for the same clip). The protocol description is stale, not the implementation. Probe at startup rather than trusting either (§1.6).
10. **Occluded node shots cannot be "fixed."** We composite the real page. Anything else is fabrication. Report occluders; never hide them.
11. ~~**Pinch-zoom (`pageScaleFactor != 1`) is untested** by me. `cssLayoutViewport` vs `cssVisualViewport` diverge there and my formula picks layout on spec-reasoning alone.~~ **Tested 2026-08-04 — the layout-viewport choice is correct, and page scale turns out not to move any of the numbers §2.1 uses.** With `Emulation.setPageScaleFactor{2.0}` on an 800×600 metrics override, scrolled to y=600: `cssLayoutViewport {pageX:0, pageY:600, clientWidth:800, clientHeight:600}` and `cssVisualViewport {pageX:0, pageY:600, clientWidth:400, clientHeight:300, scale:2, offsetX:0, offsetY:0}` — **`pageX`/`pageY` were identical**; only `clientWidth/Height` and `scale` changed. `DOM.getContentQuads` and `DOM.getBoxModel` were unchanged by the page scale and tracked `getBoundingClientRect()` exactly (both `[200, 400, 300, 400]`, `bcr = [200,400]`), and an `Input.dispatchMouseEvent` at the quad centre arrived as `clientX/clientY = 250,425` — i.e. **all of CDP's geometry and input coordinates are layout-viewport CSS px and are unaffected by `pageScaleFactor`.** A node clip built with `cssLayoutViewport.pageX/pageY` produced the correct pixels (`ee0000` at the target). Residual gap: I could not drive `visualViewport.offsetX/offsetY` away from 0 via `setPageScaleFactor` alone, so the case where the visual viewport is *panned inside* the layout viewport (real two-finger pan on a touch device) is still untested — but since `pageX/pageY` are the only fields §2.1 reads and they agreed, the exposure is small.
12. **Out of scope by spec, and genuinely uncapturable anyway:** browser chrome, OS dialogs, Keychain/Touch ID. `fromSurface` does not reach them; they are not in the renderer's surface at all.

---

## Open questions for the owner

1. **Default DPR for artifacts.** Pin `deviceScaleFactor: 1` for reproducibility across dev laptop and CI, or capture native retina for fidelity and normalise at diff time? I lean pinned-at-1 by default with `--dpr` to override.
2. **Video default codec.** H.264/MP4 (universal playback, patent-encumbered encoder) vs VP9/WebM (free, ~7 % smaller here, slower encode)? Given "local-first, no cloud", WebM may fit the ethos better.
3. **Screencast fps cap.** 68 fps headless is more than any agent needs and costs ~530 KB/s. Default `everyNthFrame: 2` (~30 fps) to halve storage, or keep full rate for fidelity?
4. **Should `--prime-scroll` be the default for `--full-page`?** It makes screenshots much more often *correct*, at the cost of mutating page state and firing analytics. This is a policy decision that interacts with the `observe` capability mode — arguably scroll-priming is not "observe".
5. **Diff tolerance defaults.** I used `tol=12`, `min_region_px=40`. Should these be per-site profiles (marketing sites with video backgrounds need much looser settings than a design-system storybook)?
6. **Retention.** Concretely: how long for video vs screenshots vs frame directories? Video dominates disk by orders of magnitude.
7. **Is `HeadlessExperimental.beginFrame` determinism worth a second capture path?** It gives byte-reproducible video for regression fixtures but needs special target creation and a Chrome flag. Real value only if you intend golden-video tests.

---

## Sources

Primary and empirical first; everything below was actually fetched or executed during this session.

1. **Chrome 151.0.7922.72's own `/json/protocol`** — dumped locally via `curl http://127.0.0.1:39471/json/protocol`; 1 605 774 bytes. The authoritative source for every parameter name, type, `experimental` and `deprecated` flag quoted in this document.
2. https://chromedevtools.github.io/devtools-protocol/tot/Page/ — Page domain (`captureScreenshot`, `startScreencast`, `screencastFrame`, `screencastFrameAck`, `getLayoutMetrics`, `captureSnapshot`).
3. https://chromedevtools.github.io/devtools-protocol/tot/HeadlessExperimental/ — `beginFrame` (via search result).
4. https://docs.rs/image-compare/latest/image_compare/ — comparison functions, `Similarity { score, image }`, `to_color_map()`.
5. https://docs.rs/dify/latest/dify/ — confirmed 0 % documented as a library.
6. `https://crates.io/api/v1/crates/{image,dssim,image-compare,imageproc,mp4,mp4-atom,re_mp4,rav1e,vpx-encode,openh264,ffmpeg-next,ffmpeg-sidecar,blake3,tiny-skia,ab_glyph,oxipng,fast_image_resize,dify,…}` — 37 crates queried 2026-08-04 for `max_stable_version` and `updated_at`.
7. https://ffmpeg.org/ffmpeg-formats.html — concat demuxer semantics (`duration`, `ffconcat` header).
8. https://groups.google.com/a/chromium.org/g/headless-dev/c/6XKLTi5bsZA — screencast in headless Chromium (background; superseded by my direct measurement).
9. https://github.com/ChromeDevTools/devtools-protocol/issues/17 — historical "headless can't capture video" thread (**now obsolete** — contradicted by measurement #18).
10. https://groups.google.com/a/chromium.org/g/graphics-dev/c/LPWc1FDhZyY — frame-precise capture via BeginFrame.
11. https://copyprogramming.com/howto/headless-chrome-capture-screen-video-or-animation — 2026 guide; source of the LIKELY-only claims about `chrome-headless-shell` throughput and new-headless CPU overhead.
12. https://medium.com/@anchen.li/how-to-do-video-recording-on-headless-chrome-966e10b1221 — screencast→ffmpeg background.
13. **Local execution:** `Google Chrome 151.0.7922.72` headless + headful, `ffmpeg 8.1.2`, `cargo 1.97.1` release build of an `image` 0.25.10 + `image-compare` 0.5.0 diff harness. Tests 1–25 in "What we verified empirically".

---

## Verification pass — 2026-08-04 (adversarial review)

Re-run against **Google Chrome 151.0.7922.72** on macOS 26.5.1, over `--remote-debugging-pipe` (not the websocket the original used), headless with `--disable-gpu`, `deviceScaleFactor:1`, 800×600 viewport, fresh fixtures. PNG pixels inspected with `ffmpeg -vf crop -f rawvideo -pix_fmt rgb24`. All processes killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| `webp` quality is not controllable | **REFUTED** | webp q1/q50/q100 → 1,674 / 1,856 / **51,944** bytes for the same clip. Docs say "jpeg only"; the implementation honours it (§1.6) |
| `position:fixed` renders once at `y == scrollY`, never duplicated | **CONFIRMED (2nd independent fixture)** | Bands `[[0,49]] / [[1500,1549]] / [[3000,3049]]` at scrollY 0/1500/3000 (§1.3) |
| `captureBeyondViewport` does not drive lazy loading | **CONFIRMED** | `<img loading=lazy>` 9000 px down: `[complete=false, naturalWidth=0]` before **and** after full-page capture; `[true, 200]` only after `scrollIntoView()` (§1.5) |
| `Page.captureScreenshot` rejected on OOPIF sessions | **CONFIRMED** | `{"code":-32000,"message":"Command can only be executed on top-level targets"}`; `Page.getLayoutMetrics` and `DOMSnapshot.captureSnapshot` **do** work on that session (§2.3) |
| `getContentQuads` on an `<iframe>` gives the border box, `getBoxModel.content` the content box | **CONFIRMED exactly** | 10 px-bordered 300×200 frame: quads `[70,300 … 390,520]` vs content `[80,310 … 380,510]` (§2.2) |
| `screencastFrame.sessionId` is constant, not a frame counter | **CONFIRMED** | 240 frames, distinct sessionIds `[1]`, 240/240 with `timestamp`, 60.0 fps, median gap 16.7 ms (§4.1) |
| Huge captures are silently blank | **NOT OBSERVED at 31 MP** | 5600×5600 noise canvas → 94.4 MB PNG (~3.0 B/px) — content genuinely present (§1.4) |
| Transport caps screenshot payload size | **REFUTED** | 125.8 MB base64 response crossed the pipe intact; Chrome's 100 MB cap is inbound-only (§1.4, and `10-…` §1.1) |
| Chrome 151 `captureScreenshot` parameter surface | **CONFIRMED** | `/json/protocol`: `format`, `quality` *("jpeg only")*, `clip`, plus experimental `fromSurface`, `captureBeyondViewport`, `optimizeForSpeed`. No Canvas domain; 57 domains, protocol 1.3 |

**Not re-tested:** the scale equation across DSF 1/2/3, the 200,000 px extreme, `Overlay.highlightNode` tooltip rendering, the five ffmpeg invocations and the 19.8 ms PTS quantisation, the Rust diff pipeline timings, `HeadlessExperimental.beginFrame`, clock-skew drift over long recordings, content-hash dedup ratios.

---

## Verification pass 2 — 2026-08-04 (second adversarial review)

Chrome **151.0.7922.72**, macOS Darwin 25.5.0, driven over `--remote-debugging-pipe` (fds 3/4 via `os.posix_spawn` + `POSIX_SPAWN_DUP2`, `os.set_inheritable` on both pipe ends — without that last call Chrome refuses to start with *"Remote debugging pipe file descriptors are not open."*). Fixtures served from `http://127.0.0.1:8899`. PNG pixels decoded with `ffmpeg -vf crop=… -f rawvideo -pix_fmt rgb24` and run-length scanned in Python. All Chrome instances killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| "No 16384 px texture ceiling; the real limit is RAM" (bottom line #2, §1.4) | **REFUTED — highest-severity finding in this review** | Colour bands at known y: correct to 16383, then rows 0..16383 repeat verbatim. 800×20000 loses the band at 19800; 800×200000 shows the red band 13× at multiples of 16384. Threshold = `dim × clip.scale × dsf`: CSS 20000@scale 0.5 ✅, CSS 9000@scale 2 ✗, CSS 10000@dsf 2 ✗. Symmetric on width (20000 px-wide page wraps at x=16384). Same in headless and headful GPU (§1.4) |
| The 5600×5600 noise capture proves "pixels are genuinely there" at scale | **PARTIAL — true but not generalisable** | 5600 < 16384 in both axes, so it is inside the safe region by construction (§1.4) |
| `max_capture_megapixels` is a sufficient guard | **REFUTED** | 800×20000 = 16 MP, far under any sane cap, and already corrupt. Output-dimension cap at 16384 is the necessary guard (§1.4) |
| webp `quality` is honoured despite the "jpeg only" doc string | **CONFIRMED (independent run)** | Same 800×600 clip: webp q1/q50/q100 = **1,784 / 1,866 / 42,306** B; jpeg = 3,634 / 3,788 / 54,838 B (§1.6) |
| `cssLayoutViewport` (not `cssVisualViewport`) is the right basis for the node clip | **CONFIRMED** | Under `Emulation.setPageScaleFactor{2.0}`, `pageX/pageY` identical (0/600) in both; quads, box model and `Input` coords all layout-viewport CSS px and unchanged by page scale; clip built from layout pageY captured the correct `ee0000` pixels (Limits #11) |
| Screencast has near-zero frame loss (untested under load) | **CONFIRMED idle / REFUTED for concurrency, for a reason the doc did not anticipate** | Idle single tab: 240 frames vs 241 rAF, 60.4 fps, max gap 27.9 ms. But adding a second page to the same BrowserContext put the first at `visibilityState:"hidden"`, `rAF = 0 fps`, **0 screencast frames**. Separate BrowserContexts → all pages visible at 60 fps (§4.1) |
| `Page.screencastFrameAck` is mandatory | **CONFIRMED** | Un-acked stream stalled after 3 frames (§4.1) |
| `Page.captureScreenshot` works on a backgrounded tab | **CONFIRMED (new)** | Hidden tab, two shots 0.6 s apart of an rAF animation → different md5s; capture appears to pump one frame each time (§4.1) |
