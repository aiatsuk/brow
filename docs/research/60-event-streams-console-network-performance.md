# Event Streams: Console, Exceptions, Network, WS/SSE, HAR, Performance, Tracing, Mutations

> **Bottom line.** Everything the brief asks for is buildable on raw CDP, but six load-bearing assumptions are wrong and must be corrected before the design freezes. (1) There are **three** error streams, not two — `Runtime.consoleAPICalled`, `Log.entryAdded`, and `Audits.issueAdded` — and the third is where modern Chrome puts deprecations, cookie problems, CSP detail and CORS detail in *structured* form. (2) **Deprecations never reach `Log.entryAdded`**; they land only in `Audits.issueAdded{DeprecationIssue}` and `Network.reportingApiReportAdded`. (3) Console/exception/log events are stamped in **wall-clock ms**, while network/page events are stamped in **monotonic seconds**, and `Audits.issueAdded` carries **no timestamp at all** — so "sort by monotonic" is not implementable without an explicit calibration step (the primitive is `requestWillBeSent.wallTime − .timestamp`, measured stable to **16 µs**). (4) Chrome **trace `ts` shares the network monotonic base exactly**, and `ResourceSendRequest.args.data.requestId` is byte-identical to `Network.requestId` — trace↔network correlation is a *join*, not a heuristic. (5) `Network.streamResourceContent` works from `requestWillBeSent` with **no Fetch interception**, but you must concatenate `bufferedData` with the inline chunks or you silently lose the first ~1 MB. (6) `PerformanceTimeline.enable` accepts only `largest-contentful-paint` and `layout-shift`, so Web Vitals **require** an injected `PerformanceObserver`. Build on: three merged-and-deduped error sources, a per-request state machine joining the `*ExtraInfo` events, ingest-time redaction, injected observers for vitals and mutations, and a slim gzip'd trace category set (**82 KB per 3 s** measured).

Every protocol claim below was checked against the `/json/protocol` dump of the **actual local Chrome: 151.0.7922.72, V8 15.1.206.10, protocol 1.3, 57 domains, 1,605,774 bytes of JSON**, and most were exercised against a live instrumented page driven by a from-scratch stdlib-only Python CDP client (no Playwright/Puppeteer/Selenium anywhere in the loop). Confidence is marked per claim: **CONFIRMED** = I read the protocol definition or observed the behaviour; **LIKELY** = secondary sources agree; **UNVERIFIED** = I am reasoning.

---

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| Consume **three** error streams: `Runtime.consoleAPICalled`, `Log.entryAdded`, `Audits.issueAdded` | Verified disjoint *and* overlapping in different places. Audits is the only structured source for deprecations, cookie-blocking, CSP directive detail, CORS detail | Two-stream (Runtime+Log) model — silently loses every deprecation and all structured issue detail | **confirmed** |
| **Dedupe across streams** by `(class, url, directive/reason)` | A single CSP violation fires 2 events; a single CORS failure fires **4**. Presenting 4 rows for 1 bug is agent-hostile | "Nothing to dedup" (true for Runtime-vs-Log alone, false once Audits is added) | **confirmed** |
| Normalise **all** timestamps to monotonic µs at ingest, via a per-session `wall_offset` calibrated from `requestWillBeSent{wallTime, timestamp}` | Runtime/Log are wall-ms; Network/Page are monotonic-s; Audits/ExtraInfo have none. Offset measured stable to **16 µs** over 6 samples | Sorting by monotonic (impossible — console has no monotonic field); sorting by wall (NTP jumps reorder the stream) | **confirmed** |
| Stamp `Audits.issueAdded`, `*ExtraInfo`, `Page.frameNavigated` with **daemon receipt time**, flagged `clock:"receipt"` | These events carry no timestamp field at all | Pretending they are ordered | **confirmed** |
| Correlate trace↔network by **exact `requestId` join**, not time windows | `ResourceSendRequest.args.data.requestId` == `Network.requestId`, verified byte-identical; trace `ts` µs == network `timestamp` s × 1e6 | Heuristic `navigationStart` alignment with a residual-error estimate | **confirmed** |
| Bodies via `Network.streamResourceContent` subscribed in the `requestWillBeSent` handler, **`bufferedData` ++ inline chunks** | Verified without Fetch: `/big` reconstructed to 3,072,003 B vs 3,072,000 B reported. Chunks alone lose 1,074,177 B | `Fetch` interception for all requests (serialises every load, corrupts timing); `getResponseBody` alone (evicted on nav) | **confirmed** |
| Fall back to `getResponseBody` on `-32602 "Request with the provided ID has already finished loading"` | Small/fast resources (favicon) finish inside the subscribe RTT. Exactly the ones cheap to re-fetch | Treating the race as an error | **confirmed** |
| Web Vitals via **injected `PerformanceObserver`** in an isolated world at `document-start`, `buffered:true` | `PerformanceTimeline.enable` rejects all types except LCP + layout-shift; observer covers 15 | `PerformanceTimeline` as primary; vendoring the `web-vitals` npm lib (external dep + telemetry surface) | **confirmed** |
| Mutations via isolated-world `MutationObserver`, rAF-batched | ~1,770× less wire volume at *higher* fidelity than `DOM.*` events | `DOM.enable` + `childNodeInserted/…` | **confirmed** |
| Redact at **ingest**, before the event reaches the ring buffer or disk | A secret that touches disk is a secret leaked; retention/export paths multiply | Redact at export/persistence | **confirmed** (design) |
| Slim trace categories + `streamCompression:"gzip"`; tracing opt-in per job | Measured 768 KB raw → **81.8 KB gzipped** per 3 s (9.4×). Default-ish set was 10 MB | Default DevTools category set; uncompressed streams | **confirmed** |
| Write our own Chrome trace-event parser (~200 LOC) | No maintained general-purpose Rust crate exists; format is a trivial JSON array of `{ph,ts,dur,name,cat,args}` | `chrome-trace-to-pprof` (V8-profile-specific), `perfetto` crate (0.0.0 placeholder) | **confirmed** |
| Per-target session fan-out via `Target.setAutoAttach{flatten:true}` for workers | Worker `console.*` reaches the page only as flat text in `Log.entryAdded{source:"worker"}`; structured args/stacks need the worker's own session | Page session only | **confirmed** |
| Emit HAR 1.2 + Chrome's `_`-prefixed extensions, replicating DevTools' `buildTimings` verbatim | Compatibility with DevTools/HAR viewers; DevTools' own algorithm is the de-facto spec | Hand-rolled timings | **confirmed** (read source) |
| Causality = explicit `actionId` + initiator stacks + `requestId` joins; time-window joins labelled `inferred` | Only initiator stacks, our own dispatch, and exact ID joins are ground truth | Presenting time-window joins as fact | **confirmed** (design) |

---

## 1. The three error streams

The brief assumes two sources. Chrome 151 has three. I loaded a fixture firing `console.*` calls, an uncaught throw, an unhandled rejection, two CSP violations, three distinct CORS failures, a 404, a `SameSite=None`-without-`Secure` cookie, an `unload` handler, and a quirks-mode document, with `Runtime.enable` + `Log.enable` + `Audits.enable` + `Network.enable` + `Network.enableReportingApi` + `Log.startViolationsReport` all active.

### 1.1 Routing table (CONFIRMED unless noted)

| Message class | `Runtime.*` | `Log.entryAdded` | `Audits.issueAdded` | Also |
|---|---|---|---|---|
| `console.log/info/debug/warn/error/table/trace/dir/dirxml/group/count/time*/assert` | `consoleAPICalled`, structured `args` | — | — | |
| Uncaught exception | `exceptionThrown`, `text:"Uncaught"` | — | — | |
| Unhandled promise rejection | `exceptionThrown`, `text:"Uncaught (in promise)"` | — | — | `exceptionRevoked` if later handled |
| Subresource 404 / `net::ERR_*` | — | `source:network`, **`networkRequestId`** | — | `loadingFailed` |
| **CSP violation** | — | `source:security` (prose) | **`ContentSecurityPolicyIssue`** (`blockedURL`, `violatedDirective`, `contentSecurityPolicyViolationType`, `sourceCodeLocation`) | `loadingFailed{blockedReason:"csp"}` |
| **CORS failure** | — | `source:javascript,category:cors` (prose) **+** `source:network,category:cors` | **`CorsIssue`** (`corsErrorStatus`, `initiatorOrigin`, `clientSecurityState`) | `loadingFailed{corsErrorStatus}` |
| **Deprecation** | — | **NEVER** | **`DeprecationIssue`** (`type`, `sourceCodeLocation`, `affectedFrame`) | `reportingApiReportAdded{type:"deprecation"}` |
| **Cookie blocked/warned** | — | — | **`CookieIssue`** (`cookieWarningReasons`, `cookieExclusionReasons`, `operation`) | `responseReceivedExtraInfo.blockedCookies` |
| Quirks mode | — | — | **`QuirksModeIssue`** | |
| Violations (longTask, handler…) | — | `source:violation`, **`level:verbose`** | — | needs `Log.startViolationsReport` |
| Worker `console.*` + worker throws | on the *worker's* session | on the *page* session: `source:worker`, flat text + `workerId` | — | |
| Mixed content, heavy ad, SRI, ORB, a11y, perf | — | sometimes prose | `MixedContentIssue`, `HeavyAdIssue`, `SRIMessageSignatureIssue`, `BlockedByResponseIssue`, `ElementAccessibilityIssue`, `PerformanceIssue` | |

**Raw observation — a deprecation, showing it is Audits-only:**
```json
// Audits.issueAdded
{"code":"DeprecationIssue","details":{"deprecationIssueDetails":{
  "affectedFrame":{"frameId":"E8ABAE4D…"},
  "sourceCodeLocation":{"scriptId":"5","url":"http://127.0.0.1:8791/","lineNumber":19,"columnNumber":8},
  "type":"UnloadHandler"}}}
// Network.reportingApiReportAdded
{"report":{"type":"deprecation","status":"Queued","body":{
  "id":"UnloadHandler","message":"Unload event listeners are deprecated and will be removed.",
  "sourceFile":"http://127.0.0.1:8791/","lineNumber":20,"columnNumber":8}}}
// Log.entryAdded with source=="deprecation":  ZERO.
```

**Raw observation — one CORS failure produces four events:**
```
Audits.issueAdded    CorsIssue{corsError:"MissingAllowOriginHeader", initiatorOrigin:"http://127.0.0.1:8793/"}
Network.loadingFailed{type:"Fetch", errorText:"net::ERR_FAILED", corsErrorStatus:{corsError:"MissingAllowOriginHeader"}}
Log.entryAdded       [javascript/error] cat=cors "Access to fetch at '…' from origin '…' has been blocked by CORS policy: No 'Access-Control-Allow-Origin'…"
Log.entryAdded       [network/error]    cat=cors "Failed to load resource: net::ERR_FAILED"
```

Note the **correction to the obvious guess**: the *explanatory* CORS message has `source: "javascript"`, not `"network"`. The `network`-sourced one is the useless `net::ERR_FAILED` line. If you filter `source == "network"` you keep the noise and drop the explanation.

### 1.2 Enums worth hard-coding

- `LogEntry.source`: `xml, javascript, network, storage, appcache, rendering, security, deprecation, worker, violation, intervention, recommendation, other`. `LogEntry.level`: `verbose, info, warning, error`. `LogEntry.category`: exactly one value, `cors`.
- `Audits.InspectorIssueCode` (30): `CookieIssue, MixedContentIssue, BlockedByResponseIssue, HeavyAdIssue, ContentSecurityPolicyIssue, SharedArrayBufferIssue, CorsIssue, AttributionReportingIssue, QuirksModeIssue, PartitioningBlobURLIssue, NavigatorUserAgentIssue, GenericIssue, DeprecationIssue, ClientHintIssue, FederatedAuthRequestIssue, BounceTrackingIssue, CookieDeprecationMetadataIssue, StylesheetLoadingIssue, FederatedAuthUserInfoRequestIssue, PropertyRuleIssue, SharedDictionaryIssue, ElementAccessibilityIssue, SRIMessageSignatureIssue, UnencodedDigestIssue, ConnectionAllowlistIssue, UserReidentificationIssue, PermissionElementIssue, PerformanceIssue, SelectivePermissionsInterventionIssue, EmailVerificationRequestIssue`.
- `Network.BlockedReason` (16): `other, csp, mixed-content, origin, inspector, integrity, subresource-filter, content-type, coep-frame-resource-needs-coep-header, coop-sandboxed-iframe-cannot-navigate-to-coop-page, corp-not-same-origin, corp-not-same-origin-after-defaulted-to-same-origin-by-coep, corp-not-same-origin-after-defaulted-to-same-origin-by-dip, corp-not-same-origin-after-defaulted-to-same-origin-by-coep-and-dip, corp-not-same-site, sri-message-signature-mismatch`.
- `Network.CorsError` (28): `DisallowedByMode, InvalidResponse, WildcardOriginNotAllowed, MissingAllowOriginHeader, MultipleAllowOriginValues, InvalidAllowOriginValue, AllowOriginMismatch, InvalidAllowCredentials, CorsDisabledScheme, PreflightInvalidStatus, PreflightDisallowedRedirect, PreflightWildcardOriginNotAllowed, PreflightMissingAllowOriginHeader, PreflightMultipleAllowOriginValues, PreflightInvalidAllowOriginValue, PreflightAllowOriginMismatch, PreflightInvalidAllowCredentials, PreflightMissingAllowExternal, PreflightInvalidAllowExternal, InvalidAllowMethodsPreflightResponse, InvalidAllowHeadersPreflightResponse, MethodDisallowedByPreflightResponse, HeaderDisallowedByPreflightResponse, RedirectContainsCredentials, InsecureLocalNetwork, InvalidLocalNetworkAccess, NoCorsRedirectModeNotFollow, LocalNetworkAccessPermissionDenied`.
- `Network.ResourceType` (19): `Document, Stylesheet, Image, Media, Font, Script, TextTrack, XHR, Fetch, Prefetch, EventSource, WebSocket, Manifest, SignedExchange, Ping, CSPViolationReport, Preflight, FedCM, Other`.

The `Console` domain (`Console.messageAdded`) is marked **`deprecated: true`** in Chrome 151's own protocol JSON. Never enable it.

### 1.3 Violations — CONFIRMED firing

`Log.startViolationsReport{config: ViolationSetting[]}` with names `longTask, longLayout, blockedEvent, blockedParser, discouragedAPIUse, handler, recurringHandler`, each with a `threshold` (ms). Observed against a 300 ms `setTimeout` body that itself synchronously dispatched a 400 ms click handler:

```
[violation/verbose] 'setTimeout' handler took 399ms
[violation/verbose] 'setTimeout' handler took 700ms
```

**Design trap: `level` is `verbose`.** A `browserctl console --level error,warning` filter silently drops every violation. Give violations their own `--source violation` selector and surface them in `perf longtasks` too.

### 1.4 Workers need their own sessions

`Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}` on the page session yields `worker` and `service_worker` targets. On the **worker's own** session (after `Runtime.enable` + `Runtime.runIfWaitingForDebugger`) you get fully structured `consoleAPICalled`/`exceptionThrown`. On the **page** session the same output appears only as flat text:

```
Log[worker] workerId=B7BF019B8E86C0DF0EFB612FD2D4FEE2  hello from dedicated worker
Log[worker] workerId=B7BF019B8E86C0DF0EFB612FD2D4FEE2  Uncaught Error: WORKER UNCAUGHT ERROR
```

`Runtime.enable` on a *page-level* auto-attached `service_worker` session **timed out**. Use browser-level auto-attach and/or the `ServiceWorker` domain for SW. Always send `Runtime.runIfWaitingForDebugger` after enabling domains or the worker hangs forever.

### 1.5 Stack traces and async chains

`Debugger.setAsyncCallStackDepth{maxDepth}` is accepted **without** `Debugger.enable` (returns `{}`), and populates `stackTrace.parentId` on `consoleAPICalled`. With `Debugger.enable` active the parent frame is **inlined** as `stackTrace.parent` (observed `parent.description == "setTimeout"`).

- Without `Debugger.enable`: you get `parentId`, and must call `Debugger.getStackTrace{stackTraceId}` — an extra RTT per event. Docs note the async chain is *automatically* attached only for `assert`, `error`, `trace`, `warning`.
- With `Debugger.enable`: parents inlined, no RTT, but a real V8 deopt cost and the risk of pausing on the page's own `debugger;` statements (mitigate with `Debugger.setSkipAllPauses{skip:true}`).

**Recommendation:** default `setAsyncCallStackDepth{maxDepth:32}` *without* `Debugger.enable`; resolve `parentId` lazily. Set depth 0 for long `--record-video` jobs (per-await allocation).

**Source maps:** `Debugger.scriptParsed{sourceMapURL, url, scriptId, hash}` (requires `Debugger.enable`) → resolve via the `sourcemap` crate (**9.3.2**, 2026-01-20). Fetch the `.map` with `Network.loadNetworkResource{frameId, url, options}` (EXPERIMENTAL) so it traverses the page's network stack and cookies — matters for authenticated maps. Cache by `(url, hash)`. **UNVERIFIED** end-to-end; pieces individually confirmed.

---

## 2. Serialising console args without token blowup

`Runtime.consoleAPICalled.args` is `RemoteObject[]`. **Chrome populates `preview` automatically for console args** even though `generatePreview` is a parameter of `evaluate`/`callFunctionOn`, not of the event.

```jsonc
// console.log(obj) with a circular self-reference
{"type":"object","className":"Object","description":"Object","objectId":"…2.1",
 "preview":{"type":"object","description":"Object","overflow":false,"properties":[
   {"name":"a","type":"number","value":"1"},{"name":"b","type":"string","value":"two"},
   {"name":"c","type":"object","value":"Array(3)","subtype":"array"},
   {"name":"d","type":"object","value":"Object"},
   {"name":"self","type":"object","value":"Object"}]}}   // circular → flat string, no recursion

// console.log(bigObj) with 200 keys
{"description":"Object","preview":{"overflow":true,"properties":[k0,k1,k2,k3,k4]}}  // capped at 5

// console.log(document.getElementById('a'))
{"type":"object","subtype":"node","className":"HTMLDivElement","description":"div#a",
 "preview":{"subtype":"node","description":"div#a","overflow":true,
            "properties":[align,title,lang,translate,dir]}}   // ← IDL attrs, not markup
```

Facts to design against (all CONFIRMED):

1. **`preview.properties` is capped at 5** with `overflow:true`. Free, bounded, always present → default tier.
2. **Circular refs are already safe** — the cycle renders as the string `"Object"`. No cycle detection at tier 1.
3. **DOM nodes preview terribly.** `subtype:"node"` gives IDL attributes, never markup. **Special-case:** on `subtype=="node"`, `DOM.requestNode{objectId}` → `nodeId`, then `DOM.getOuterHTML{nodeId}` (truncated) or bind to a `@node-NN` ref from the Unified Page Tree. Highest-value console improvement for an agent.
4. `console.table` args arrive as an array whose preview contains nested `valuePreview` one level deep — enough to render the table with no extra calls.

### Tiered rendering ladder

| Tier | Cost | Mechanism | When |
|---|---|---|---|
| 0 | free | `description` + primitive `value` | primitives, always |
| 1 | free | `preview` (≤5 props, `overflow`) | default for objects |
| 2 | 1 RTT | `Runtime.callFunctionOn{objectId, functionDeclaration, returnByValue:true}` with a **depth/breadth/byte-capped custom serialiser** | agent expands |
| 3 | 1 RTT | `DOM.requestNode` + `DOM.getOuterHTML` | `subtype=="node"` |
| 4 | 1 RTT | `serializationOptions:{serialization:"deep", maxDepth:N}` (EXPERIMENTAL) | rare deep dump |

**Never use bare `returnByValue:true`** on an unknown object — it is `JSON.stringify` semantics, uncapped, and **throws on circular references**. Always ship your own serialiser:

```rust
// crates/events/src/console/expand.rs
const EXPANDER: &str = r#"function(maxDepth, maxProps, maxStr, budget) {
  const seen = new WeakSet(); let used = 0;
  const walk = (v, d) => {
    if (used > budget) return {__t:"budget"};
    if (v === null || typeof v !== "object") {
      if (typeof v === "string" && v.length > maxStr) { used += maxStr; return v.slice(0,maxStr)+"…"; }
      used += 8; return typeof v === "bigint" ? String(v)+"n" : v;
    }
    if (seen.has(v)) return {__t:"circular"};
    seen.add(v);
    if (d >= maxDepth) return {__t:"truncated", ctor: v.constructor && v.constructor.name};
    if (Array.isArray(v)) return v.slice(0,maxProps).map(x => walk(x, d+1));
    const out = {}; let n = 0;
    for (const k of Object.keys(v)) {
      if (n++ >= maxProps) { out.__more = Object.keys(v).length - maxProps; break; }
      out[k] = walk(v[k], d+1);
    }
    return out;
  };
  return walk(this, 0);
}"#;
// callFunctionOn { objectId, functionDeclaration: EXPANDER,
//                  arguments:[{value:4},{value:30},{value:512},{value:16384}],
//                  returnByValue:true, objectGroup:"brow-console" }
```

**Lifetime discipline:** every `objectId` pins a JS object in the renderer heap — a guaranteed OOM on a 30-minute job. Both mitigations are required:
- Pass `objectGroup:"brow-console-<generation>"` on all expansions; `Runtime.releaseObjectGroup` on every navigation.
- Treat console `objectId`s as valid **only until the next `Runtime.executionContextsCleared`** (observed 2× during a single navigation). Record `expandable: bool` on the event so the agent knows the preview is now permanent.

---

## 3. Network: event sequence, ExtraInfo, timing, provenance

### 3.1 Observed sequence (CONFIRMED)

```
requestWillBeSent(requestId, loaderId, documentURL, request, timestamp, wallTime, initiator,
                  redirectHasExtraInfo*, redirectResponse, type, frameId, hasUserGesture,
                  renderBlockingBehavior*)
requestWillBeSentExtraInfo*(requestId, associatedCookies, headers, connectTiming*,
                  deviceBoundSessionUsages, clientSecurityState, siteHasCookieInOtherPartition,
                  appliedNetworkConditionsId)
[responseReceivedEarlyHints*]
responseReceived(requestId, loaderId, timestamp, type, response, hasExtraInfo*, frameId)
responseReceivedExtraInfo*(requestId, blockedCookies, headers, resourceIPAddressSpace,
                  statusCode, headersText, cookiePartitionKey*, cookiePartitionKeyOpaque,
                  exemptedCookies)
dataReceived(requestId, timestamp, dataLength, encodedDataLength, data*)   × N
loadingFinished(requestId, timestamp, encodedDataLength)
   | loadingFailed(requestId, timestamp, type, errorText, canceled, blockedReason, corsErrorStatus)
```
`*` = EXPERIMENTAL in Chrome 151's own protocol JSON.

**`requestId` has two formats.** Subresources get renderer-scoped `"34443.2"`; the main-document navigation request gets a 32-hex-char id (`"28B7E499107487376898BC1716FF2AF0"`) which, for the document, **equals the `loaderId`**. `Audits` issue `request.requestId` uses the same 32-hex form. Do not assume a shape; treat as opaque.

### 3.2 Why ExtraInfo is non-optional

`requestWillBeSent.request.headers` is the *renderer's view before the network stack runs* — no `Cookie`, nothing added by the network service or extensions. `responseReceived.response.headers` is likewise partially processed. Observed:

- `requestWillBeSentExtraInfo.headers` had 14 keys including the full `sec-ch-ua*` client-hints set, none of which appear in the renderer view.
- `responseReceivedExtraInfo` carried the wire truth and `headersText` (raw status line + header block). **`headersText` is the only way to compute HAR `headersSize` correctly.**
- **Multiple `Set-Cookie` headers are joined with `\n` in a single map value** (CONFIRMED):
  ```json
  {"Set-Cookie": "sess=SUPERSECRET123; SameSite=None\nplain=abc"}
  ```
  Split on `\n` before parsing or redacting, or you will miss every cookie after the first.
- Cookie *blocking* lives **only** in ExtraInfo:
  ```json
  "blockedCookies":[{"blockedReasons":["SameSiteNoneInsecure"],
    "cookieLine":"sess=SUPERSECRET123; SameSite=None",
    "cookie":{"name":"sess","value":"SUPERSECRET123","domain":"127.0.0.1","path":"/",
              "sameSite":"None","sourceScheme":"NonSecure","sourcePort":8791,…}}]
  ```
  This is how you tell an agent "your login failed because the cookie was rejected as `SameSiteNoneInsecure`" — exactly the bug class this harness exists to diagnose. Note the raw value is right there: **redaction is mandatory, not optional.**

**Ordering hazard (LIKELY — standard CDP behaviour, and the existence of `hasExtraInfo` implies it):** ExtraInfo events are not ordered relative to their partners. `requestWillBeSent.redirectHasExtraInfo` and `responseReceived.hasExtraInfo` exist so a consumer knows whether to *wait*. Implement a per-`requestId` state machine that buffers whichever half arrives first and emits the normalised record only when `hasExtraInfo == false` or both halves are present. Also: **ExtraInfo events carry no `timestamp`** (CONFIRMED) — inherit the partner's.

### 3.3 Redirects, initiator, timing, provenance, failures

**Redirects** do not produce `responseReceived`. A second `requestWillBeSent` arrives with the **same `requestId`** and a populated `redirectResponse`. Append to a `hops[]` vector; `redirectHasExtraInfo` says whether an ExtraInfo pair belongs to that hop.

**Initiator** — the causality goldmine:
```
Initiator { type: parser|script|preload|SignedExchange|preflight|FedCM|other,
            stack: Runtime.StackTrace, url, lineNumber, columnNumber, requestId }
```
`type:"script"` with a populated `stack` is the **only ground-truth causal link inside the network stream**: it names the JS frame that issued the fetch. With `setAsyncCallStackDepth` set, that stack chains back through `await`/`setTimeout` to the click handler.

**Timing** — observed `ResourceTiming` (three fields absent from the public docs page):
```json
{"requestTime":88738.065514,"proxyStart":-1,"proxyEnd":-1,"dnsStart":-1,"dnsEnd":-1,
 "connectStart":-1,"connectEnd":-1,"sslStart":-1,"sslEnd":-1,
 "workerStart":-1,"workerReady":-1,"workerFetchStart":-1,"workerRespondWithSettled":-1,
 "sendStart":0.215,"sendEnd":0.278,"pushStart":0,"pushEnd":0,
 "receiveHeadersStart":1.323,"receiveHeadersEnd":1.344}
```
`requestTime` is a **monotonic base in seconds**; every other field is **milliseconds relative to it**; `-1` means not applicable (here: connection reused). `response.responseTime` is **wall-clock ms since epoch** (`1785863279966.483`) — a second bridge between the clocks.

**Provenance flags** on `Network.Response` (all CONFIRMED present): `fromDiskCache`, `fromServiceWorker`, `fromPrefetchCache`, `connectionReused`, `connectionId`, `remoteIPAddress`, `remotePort`, `protocol`, `alternateProtocolUsage`, `securityState`, `encodedDataLength`, `mimeType`, `charset`, plus `serviceWorkerResponseSource` (`cache-storage|http-cache|fallback-code|network`) and `serviceWorkerRouterInfo`. `Network.requestServedFromCache{requestId}` is a separate event for memory-cache hits. For prefetch/prerender provenance beyond the flag, enable the **`Preload`** domain (EXPERIMENTAL): `prefetchStatusUpdated`, `prerenderStatusUpdated`, `preloadingAttemptSourcesUpdated`, with a 36-value `PrefetchStatus` and 40-value `PrerenderFinalStatus` enum that explain *why* a speculative load was not used.

**Failures:**
```json
{"requestId":"31111.2","type":"Script","errorText":"","canceled":false,"blockedReason":"csp"}
{"requestId":"31111.3","type":"Fetch","errorText":"net::ERR_CONNECTION_REFUSED","canceled":false}
{"requestId":"32947.2","type":"Fetch","errorText":"net::ERR_FAILED",
 "corsErrorStatus":{"corsError":"MissingAllowOriginHeader","failedParameter":""}}
{"requestId":"8611.12","type":"EventSource","errorText":"net::ERR_ABORTED","canceled":true}
```
**CSP failures have an empty `errorText`** — render `blockedReason` when `errorText` is empty or the agent sees a blank error. CORS failures have the generic `net::ERR_FAILED`; the real reason is in `corsErrorStatus` and, in prose, in the `javascript`-sourced `Log` entry.

### 3.4 Beyond HTTP: WebTransport and direct sockets

Chrome 151 emits `Network.webTransportCreated / webTransportConnectionEstablished / webTransportClosed` (stable), and an EXPERIMENTAL direct-socket family gated on `Network.enable{reportDirectSocketTraffic:true}`: `directTCPSocketCreated/Opened/Aborted/Closed/ChunkSent/ChunkReceived` and the `directUDPSocket*` equivalents plus `directUDPSocketJoined/LeftMulticastGroup`. Neither is in the brief; both are real transports a modern app can use, and neither appears in a HAR. Capture them into the event bus as first-class `stream.*` kinds; **UNVERIFIED** empirically (I did not build a WebTransport fixture).

---

## 4. Response bodies: the genuinely hard part

### 4.1 Eviction on navigation (CONFIRMED, worse than the brief implies)

```
before nav:               {"body":"{\"ok\": true, \"token\": \"eyJ…\"", …}                 ✅
after nav to about:blank: {"error":{"code":-32000,"message":"No resource with given identifier found"}} ❌
after cross-origin nav:   same error                                                        ❌
```
**Even a same-process navigation to `about:blank` destroys every buffered body.** This is not a rare race, it is the guaranteed outcome for any request from a previous document. Retroactive body fetching is not viable for an SPA crawler; body policy must be decided at `requestWillBeSent`.

Distinct second failure, on in-flight streams (observed on an open SSE connection): `{"error":{"message":"No data found for resource with given identifier"}}`. Different message, different meaning — surface both distinctly.

**However:** within a single document, `getResponseBody` is durable. I retrieved a 3,072,000-byte body and a 327,680-byte chunked body successfully *after* `loadingFinished`, on the same page, with no navigation.

### 4.2 The buffer knobs

`Network.enable{maxTotalBufferSize*, maxResourceBufferSize*, maxPostDataSize, reportDirectSocketTraffic*, enableDurableMessages*}`. With `maxTotalBufferSize: 50 MB, maxResourceBufferSize: 20 MB` a 3 MB body survived. Defaults are much smaller — set explicitly.

**There is no `Network.setDataSizeLimitsForTest` in Chrome 151** — the brief's guess is wrong. The full command list is: `setAcceptedEncodings, clearAcceptedEncodingsOverride, canClearBrowserCache, canClearBrowserCookies, canEmulateNetworkConditions, clearBrowserCache, clearBrowserCookies, continueInterceptedRequest, deleteCookies, disable, emulateNetworkConditions, emulateNetworkConditionsByRule, overrideNetworkState, enable, configureDurableMessages, getAllCookies, getCertificate, getCookies, getResponseBody, getRequestPostData, getResponseBodyForInterception, takeResponseBodyForInterceptionAsStream, replayXHR, searchInResponseBody, setBlockedURLs, setBypassServiceWorker, setCacheDisabled, setCookie, setCookies, setExtraHTTPHeaders, setAttachDebugStack, setRequestInterception, setUserAgentOverride, streamResourceContent, getSecurityIsolationStatus, enableReportingApi, enableDeviceBoundSessions, deleteDeviceBoundSession, fetchSchemefulSite, loadNetworkResource, setCookieControls`.

Two useful ones the brief misses: **`Network.searchInResponseBody{requestId, query, caseSensitive, isRegex}`** (EXPERIMENTAL) lets you answer "does this response contain X?" without transferring the body — ideal for a token-budget-constrained agent. **`Network.configureDurableMessages{maxTotalBufferSize, maxResourceBufferSize}`** (EXPERIMENTAL) stores bodies outside the renderer so they survive cross-process navigation; its own docstring says the `Network.enable{enableDurableMessages}` form "is being deprecated in favor of the dedicated `configureDurableMessages` command, due to the possibility of deadlocks when awaiting `Network.enable` before issuing `Runtime.runIfWaitingForDebugger`." Use the dedicated command. Accepted by Chrome 151 (`{}`); **UNVERIFIED** that it actually rescues bodies across navigation.

### 4.3 `Network.streamResourceContent` — CONFIRMED without Fetch, with a data-loss trap

I subscribed from the `requestWillBeSent` handler with **no Fetch domain and no pausing**:

```
rid       url             subscribe RTT   bufferedData(b64)   dataReceived   with inline data
34443.2   /big            17.6 ms         1,432,236            28             2   (2,663,768 b64)
34443.3   /slow (chunked) 10.0 ms            10,924            41            39   (  426,036 b64)
34443.4   /favicon.ico     0.2 ms         ERROR -32602 "Request with the provided ID has already finished loading"
```

Reconstruction check for `/big`: `bufferedData` decodes to 1,074,177 B, inline chunks to 1,997,826 B, **sum 3,072,003 B** vs the reported `Σ dataLength = 3,072,000 B` (difference is base64 padding). For `/slow`: 8,193 + 319,527 ≈ 327,720 vs 327,680 reported.

**Two hard consequences the brief and naive implementations get wrong:**

1. **You MUST concatenate `bufferedData` with the inline chunks.** For `/big`, only 2 of 28 `dataReceived` carried `data` — using the chunks alone loses 1,074,177 bytes (35% of the body) *silently*, with no error.
2. **Fast/small resources finish inside the subscribe RTT** and return `-32602 "Request with the provided ID has already finished loading"` — a distinct error string. This is not a failure; it is the signal to fall back to `getResponseBody`, which for exactly these small resources is cheap.

An earlier probe that called `streamResourceContent` *after* `loadingFinished` got `bufferedData` length 0 and **zero** subsequent inline data. Timing is everything.

### 4.4 Recommended body strategy

```
on requestWillBeSent(req):
    if policy.should_capture_body(req):                  # content-type / URL / size heuristics
        spawn: Network.streamResourceContent{requestId}  # do NOT block the event loop
          ok(bufferedData) -> buf = b64decode(bufferedData); mark streaming
          err(-32602 "already finished loading") -> mark fallback
on dataReceived(p):  if streaming && p.data -> buf.extend(b64decode(p.data))   # spill to disk >1 MB
on loadingFinished:  if fallback || buf.is_empty() -> Network.getResponseBody (best effort)
                     assert buf.len() ≈ Σ dataLength, else flag `_bodyTruncated`
```

Use the `Fetch` domain **only** where you must *modify* traffic (mocking, auth injection, `failRequest`) or where a body is business-critical and streaming failed. `Fetch.enable{patterns:[{urlPattern:"*", requestStage:"Response"}]}` pauses **every** response until `continueRequest` — every network round-trip gains a full CDP round-trip through the daemon, and it perturbs the very timings a performance harness exists to measure. `Fetch.takeResponseBodyAsStream` (EXPERIMENTAL) exists for large bodies at the Fetch layer.

**Encoding note:** `getResponseBody` returned `base64Encoded:false` for JSON/HTML and `base64Encoded:true` for a 9-byte `not found` body served **without a `Content-Type`**. The flag tracks Chrome's MIME sniffing, not the bytes. Always branch on the flag; never assume UTF-8.

---

## 5. WebSockets, SSE, long-poll

### WebSockets (CONFIRMED against a live echo server)

Order: `webSocketCreated{requestId,url,initiator}` → `webSocketWillSendHandshakeRequest{requestId,timestamp,wallTime,request}` → `webSocketHandshakeResponseReceived{requestId,timestamp,response}` → `webSocketFrameSent`/`webSocketFrameReceived{requestId,timestamp,response}` → `webSocketFrameError{requestId,timestamp,errorMessage}` → `webSocketClosed{requestId,timestamp}`.

`webSocketHandshakeResponseReceived.response` is rich: `status, statusText, headers, headersText, requestHeaders, requestHeadersText`, including `Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits`.

**Frame payloads — the detail that silently corrupts data if missed:**
```json
{"opcode":1,"mask":true, "payloadData":"hello-text"}   // text   → raw UTF-8
{"opcode":2,"mask":true, "payloadData":"AQID+g=="}     // binary → base64 of [1,2,3,250]
{"opcode":2,"mask":false,"payloadData":"AAEC//4="}     // binary → base64 of [0,1,2,255,254]
```
**There is no `isBinary` flag. `opcode` is the only discriminator: 1 = text (raw), 2 = binary (base64).** Opcode 8 = close, 9/10 = ping/pong. `mask:true` on client→server per RFC 6455.

**Caveat (LIKELY):** with `permessage-deflate` negotiated, `payloadData` is the **decompressed** payload — readable, but you cannot reconstruct wire bytes or compute true bandwidth from frames.

**Gap (CONFIRMED from the protocol definition):** `Network.webSocketClosed` carries only `requestId` and `timestamp` — **no close code, no reason string.** You cannot tell an agent *why* a socket closed from CDP alone.

### SSE (CONFIRMED)

`Network.eventSourceMessageReceived{requestId, timestamp, eventName, eventId, data}`:
```json
{"requestId":"8611.12","timestamp":88738.070389,"eventName":"tick","eventId":"0","data":"{\"n\":0}"}
{"requestId":"8611.12","timestamp":88738.070461,"eventName":"message","eventId":"0","data":"plain-0"}
```
Named events keep their name; unnamed get `"message"`. Fully parsed — no manual `text/event-stream` framing. The request stays in flight, so `getResponseBody` fails with `"No data found…"`, and closing it yields `loadingFailed{type:"EventSource", errorText:"net::ERR_ABORTED", canceled:true}` — **classify `EventSource` + `canceled:true` as normal termination**, or every SSE page reports a spurious failure.

> **Upgraded LIKELY → CONFIRMED, 2026-08-04.** Built the discriminating fixture the original pass lacked: one page consuming the **same** `/sse` endpoint two ways simultaneously — native `EventSource` *and* `fetch()` + `ReadableStream` — with `Network.enable` on the page session.
> ```
> page saw: ["ES:msg0","FETCH:event: tick\ndata: msg0","FETCH:...msg1","ES:msg1",
>            "FETCH:...msg2","ES:msg2","FETCH:...msg3","ES:msg3"]
> Network.eventSourceMessageReceived count: 4   -> ['msg0','msg1','msg2','msg3']
> resourceTypes per request: { "sse#47211.2": "Fetch", "sse#47211.3": "EventSource" }
> ```
> Both consumers received all 4 messages, but **exactly 4** `eventSourceMessageReceived` events fired — all attributable to the `EventSource` request (`requestId …3`). The `fetch()` consumer (`requestId …2`, `resourceType:"Fetch"`) produced **zero** framed events despite carrying identical `text/event-stream` bytes. **The hand-written framer is required; it is not speculative work.**

**Important limit (CONFIRMED — see above):** `eventSourceMessageReceived` fires for the **`EventSource` API only**. A large fraction of modern apps (and every LLM streaming UI) implements SSE over `fetch()` + `ReadableStream`, which appears as a plain `Fetch` request with a long-lived body. For those you get no framing at all — you must capture the body via `streamResourceContent` and parse `text/event-stream` yourself in `crates/events`. Budget for a hand-written SSE framer.

### Long-poll

CDP has no long-poll concept. Detect heuristically: same URL + `type: Fetch|XHR`, repeated ≥3×, each with `wait` (TTFB) > 1 s, gaps < 2 s. Emit a synthetic `stream.longpoll` event collapsing N requests into one row. Mark provenance `inferred`.

---

## 6. HAR 1.2: exact mapping and what is not derivable

Chrome DevTools' own writer (`front_end/models/har/Log.ts`, `Writer.ts`) is the compatibility target; I read the source.

### Derivable from CDP

| HAR field | CDP source |
|---|---|
| `log.version` / `creator` | literal `"1.2"` / `{name:"brow", version}` |
| `pages[].startedDateTime` | `pseudoWallTime(page.startTime)` |
| `pages[].pageTimings.onContentLoad` | `Page.domContentEventFired.timestamp` − navStart, ms |
| `pages[].pageTimings.onLoad` | `Page.loadEventFired.timestamp` − navStart, ms |
| `entries[].startedDateTime` | `pseudoWallTime(issueTime)` |
| `entries[].time` | `Σ max(t,0)` over `blocked,dns,connect,send,wait,receive` (**ssl excluded — inside connect**) |
| `request.method/url/httpVersion` | `request.method`, `request.url`, `response.protocol` |
| `request.headers` | **`requestWillBeSentExtraInfo.headers`** (not the renderer view) |
| `request.cookies` | parsed from `associatedCookies` |
| `request.queryString` | parsed from `request.url` |
| `request.postData` | `request.postData` / `Network.getRequestPostData` |
| `request.headersSize` | `requestHeadersText.length`, else `-1` |
| `response.status/statusText` | `response.status`, `response.statusText` |
| `response.headers` | **`responseReceivedExtraInfo.headers`** (split multi-value on `\n`) |
| `response.headersSize` | `responseReceivedExtraInfo.headersText.length`, else `-1` |
| `response.content.size` | `Σ dataReceived.dataLength` (decoded) |
| `response.content.mimeType` | `response.mimeType` or `"x-unknown"` |
| `response.content.text` / `.encoding` | streamed body; `encoding:"base64"` when `base64Encoded` |
| `response.content.compression` | `content.size − encodedDataLength` |
| `response.redirectURL` | `Location` header, else `""` |
| `response.bodySize` | `encodedDataLength`; **0 for 304**; `-1` if headers missing |
| `serverIPAddress` | `response.remoteIPAddress`, IPv6 brackets stripped |
| `connection` | `String(response.remotePort)` |
| `timings.*` | DevTools' `buildTimings()` — reproduce verbatim (below) |

### DevTools' `buildTimings()` — reproduce exactly

```
blocked  = toMs(issueTime < startTime ? startTime - issueTime : -1)
           + leastNonNegative([dnsStart, connectStart, sendStart])       // if not Infinity
           ; if (_blocked_proxy > blocked) blocked = _blocked_proxy
_blocked_proxy = proxyEnd - proxyStart                                   // only if proxyEnd !== -1
dns      = (dnsEnd >= 0 ? dnsEnd : -1) - (dnsEnd >= 0 ? blockedStart : 0)
ssl      = (sslEnd > 0 ? sslEnd : -1) - (sslEnd > 0 ? sslStart : 0)
connect  = (connectEnd >= 0 ? connectEnd : -1) - (connectEnd >= 0 ? leastNonNegative([dnsEnd, blockedStart]) : 0)
send     = max(0, (sendEnd >= 0 ? sendEnd : 0) - (sendEnd >= 0 ? max(connectEnd, dnsEnd, blockedStart) : 0))
highest  = max(sendEnd, connectEnd, sslEnd, dnsEnd, blockedStart, 0)
wait     = toMs(responseReceivedTime - requestTime) - highest
receive  = max(0, toMs(endTime - requestTime) - toMs(responseReceivedTime - requestTime))

leastNonNegative(vs) = vs.reduce((b,v) => (v >= 0 && v < b) ? v : b, Infinity)
// if there is no `timing` at all and responseReceivedTime === -1: blocked = toMs(endTime - issueTime), return.
// `send` is deliberately never -1, "for legacy reasons", even when served from cache.
// The `send < 0` clamp exists for QUIC (crbug.com/740792).
```

### Chrome extension fields to emit (all present in `Log.ts`)

`_initiator` (type/url/requestId/lineNumber/**stack**), `_priority`, `_resourceType`, `_fromCache` (`"memory"|"disk"`, deleted if absent), `_connectionId` (deleted if `"0"`), `_transferSize`, `_error`, `_blocked_queueing`, `_blocked_proxy`, `_workerStart`, `_workerReady`, `_workerFetchStart`, `_workerRespondWithSettled`, `_workerRouterEvaluationStart`, `_workerCacheLookupStart`, `_fetchedViaServiceWorker`, `_responseCacheStorageCacheName`, `_serviceWorkerResponseSource`, `_serviceWorkerRouterRuleIdMatched`, `_serviceWorkerRouterMatchedSourceType`, `_serviceWorkerRouterActualSourceType`, `_webSocketMessages`, `_eventSourceMessages`.

Add our own: `_browIssues` (linked `Audits` issue codes), `_browRedactions`, `_browActionId`, `_browBodySource` (`stream|getResponseBody|omitted`).

### NOT derivable from CDP — be honest in the output

| HAR field | Reality |
|---|---|
| `entries[].cache` (`beforeRequest`/`afterRequest`) | **Not available.** DevTools emits `cache: {}`. Emit `{}`, put provenance in `_fromCache`. |
| `timings.blocked` as *true* stalled time | A synthesis of queueing + proxy + pre-connect. Not measured. |
| `browser` | Fill from `/json/version` `Browser` + `User-Agent`. |
| `response.content.text` for evicted/streamed-past bodies | Genuinely absent. Emit `_bodyOmitted: "evicted"\|"too-large"\|"redacted"\|"not-captured"`. |
| Exact request `bodySize` for multipart uploads | `maxPostDataSize` truncates; record `_postDataTruncated: true`. |
| WebTransport / direct-socket traffic | HAR has no concept of it. Export separately. |

**Wall-clock:** `startedDateTime` needs `pseudoWallTime(monotonic)` — see §9.1. Use `requestWillBeSent.wallTime` as the anchor. Do **not** use the daemon's own clock; CDP timestamps come from the browser process.

**Rust:** the `har` crate (**0.9.0**, 2026-03-22, `github.com/mandrean/har-rs`) provides HAR 1.2 serde types but has none of Chrome's `_` fields. Define our own `serde` structs in `crates/artifacts/src/har.rs` with `#[serde(flatten)] extensions: BTreeMap<String, Value>`, and use `har` only as a schema cross-check in tests.

---

## 7. Redaction

**Redact at ingest, before the event enters the ring buffer.** After ingest an event is copied to the in-memory ring, the on-disk segment, the HAR export, the video action log, and the agent's stdout. Redacting at persistence leaves the secret live in the ring buffer, in daemon core dumps, and in `--follow` output. Ingest is the only single choke point. Cost is one pass over headers/bodies on the event thread — trivial next to the JSON parse already done.

### Layered rules

**1. Header deny-list (exact, case-insensitive) → `<redacted:header>`**
`authorization, proxy-authorization, cookie, set-cookie, x-api-key, api-key, x-auth-token, auth-token, x-csrf-token, x-xsrf-token, x-session-token, x-access-token, x-refresh-token, authentication, www-authenticate, proxy-authenticate, x-amz-security-token, x-goog-api-key, x-firebase-appcheck, dpop, x-hub-signature, x-hub-signature-256, x-signature, x-shopify-access-token, private-token, x-vault-token`

DevTools' own `sanitize` strips only `set-cookie`, `authorization`, `cookie` — far too narrow. My probe proved it: `X-Api-Key: sk_live_deadbeefcafebabe0123` was fully visible in both `responseReceived.response.headers` and `responseReceivedExtraInfo.headers`.

**2. Header prefix/glob deny-list:** `x-*-token`, `x-*-secret`, `x-*-key`, `x-*-signature`.

**3. Cookies.** Strip values in `associatedCookies[].cookie.value`, `blockedCookies[].cookie.value`, `blockedCookies[].cookieLine`, `exemptedCookies`, and every `Set-Cookie` value (**after splitting on `\n`**). **Keep name, domain, path, expires, sameSite, httpOnly, secure, sourceScheme, sourcePort, and all `blockedReasons`** — the diagnostic value is entirely in the metadata.

**4. Value regexes** (over header values, URL query values, JSON string leaves, form fields, `postData`, console args):

| Pattern | Regex sketch |
|---|---|
| JWT | `\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b` |
| Bearer | `(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}` |
| Basic | `(?i)\bbasic\s+[A-Za-z0-9+/=]{16,}` |
| Stripe-ish | `\b(sk\|pk\|rk)_(live\|test)_[A-Za-z0-9]{16,}\b` |
| GitHub | `\b(gh[pousr]\|github_pat)_[A-Za-z0-9_]{20,}\b` |
| AWS | `\bAKIA[0-9A-Z]{16}\b`, `\bASIA[0-9A-Z]{16}\b` |
| Google | `\bAIza[0-9A-Za-z_-]{35}\b` |
| Slack | `\bxox[baprs]-[0-9A-Za-z-]{10,}\b` |
| OpenAI/Anthropic | `\bsk-(ant-)?[A-Za-z0-9_-]{20,}\b` |
| PEM | `-----BEGIN [A-Z ]*PRIVATE KEY-----` |
| PAN (Luhn-checked) | `\b(?:\d[ -]?){13,19}\b` |

Compile once into a single `regex::RegexSet` (**regex 1.13.1**) so all patterns match in one pass; `aho-corasick` **1.1.5** for the literal prefix set. Never loop per-pattern.

**5. High-entropy fallback.** For token-shaped strings (len ≥ 24, charset ⊆ base64url/hex, no whitespace), compute Shannon entropy; redact if `> 3.5 bits/char` (base64-ish) or `> 3.0` (hex). Catches bespoke session IDs no regex knows. **Redact only the value, always record `len` and a salted `blake3` prefix** so the agent can still say "the token changed between request A and B" — often the whole bug.

**6. URL query params** by name: `token, access_token, id_token, refresh_token, code, state, api_key, apikey, key, secret, password, passwd, pwd, sig, signature, auth, session, sid, jwt, otp, code_verifier`. Rewrite the value but **keep the parameter present** — its absence changes route-template inference in the site graph.

**7. Form fields** (from `postData`, `Input` events, and the Page Tree): by `input[type=password]`, by `autocomplete` token (`current-password, new-password, cc-number, cc-csc, one-time-code`), and by name/id regex `(?i)(pass|pwd|secret|token|otp|cvv|cvc|ssn|card)`.

**8. `console.*` args and `mutate.evaluate` code** go through the same value pass. `console.log(authToken)` is the most common leak of all.

### The over-redaction failure mode

Over-redaction destroys debuggability — an agent debugging an auth flow that sees `<redacted>` everywhere cannot work. All four mitigations are required:

- **Never delete; substitute a structured stub:** `{"__redacted":{"rule":"header-denylist","len":36,"sha":"b3:9f2a1c…","classes":["jwt"]}}`. Length + stable hash preserve *comparison* without disclosure.
- **Policy-gated reveal:** `browserctl network requests --reveal <field-path> --request <id>` requires the `storage` capability and, in a background job, **parks the job in `waiting_for_approval`** like the other dangerous actions. Every reveal is audit-logged with actor, timestamp, field path.
- **Reveal reads from a bounded, memory-only reveal cache** (default 64 MB, 10-minute TTL, never in an on-disk segment, never in the HAR). Expired → reveal fails honestly. This is the one softening of "redact at ingest" and must be documented, not hidden.
- **Per-origin allow-list** in policy for dev environments (`localhost`, `*.test`), so local debugging is not crippled.

---

## 8. DOM mutation streaming: the measurement that settles it

Identical workload (50 `createElement`+`setAttribute`+`appendChild` every 50 ms, clearing at 400 children) under both strategies for ~3 s.

### Strategy A — CDP `DOM.*` events (`DOM.enable` + `DOM.getDocument{depth:-1, pierce:true}`)

```
DOM.getDocument materialised tree ≈ 185,210 bytes of JSON (a trivial page!)
Events in 3 s: { childNodeCountUpdated: 5651, scrollableFlagUpdated: 14,
                 childNodeRemoved: 2, childNodeInserted: 1 }
Total wire bytes: 775,226
```

**5,651 `childNodeCountUpdated` and exactly *one* `childNodeInserted`.** CDP sends `childNodeInserted` only for nodes whose parent has already been *pushed to the client*. For everything else you get `childNodeCountUpdated{nodeId, childNodeCount}` — a bare integer. You learn that *something* changed and nothing about what. Recovering content requires `DOM.requestChildNodes` per node, which generates more events, which grows the materialised set, which grows the event rate. A positive feedback loop. On top of that, `DOM.enable` obliges the daemon to mirror the full node set for the page's lifetime and to handle `DOM.documentUpdated` (total invalidation) correctly.

### Strategy B — isolated-world `MutationObserver`, aggregated

```
{"batches":63, "muts":2976, "added":2970, "removed":2708, "attrs":0, "chars":0}
Wire bytes for 6 aggregate messages at 500 ms cadence: 438 bytes total
```

Full fidelity (2,976 real `MutationRecord`s, correctly attributed) in **438 bytes** — a **~1,770× reduction** versus 775 KB, while telling you strictly more.

### Verdict and design

**Use `MutationObserver` in an isolated world as the mutation source. Use `DOM.enable` only transiently, while the Page Tree is being built.**

Injection (CONFIRMED working):
```
Runtime.addBinding{ name:"__browRelay", executionContextName:"brow_iso" }
Page.addScriptToEvaluateOnNewDocument{ source: OBSERVER_JS, worldName:"brow_iso",
                                       runImmediately:true }   → {"identifier":"1"}
→ Runtime.bindingCalled{ name, payload, executionContextId }
```
`worldName` + `executionContextName` keep the observer invisible to page JS (no `window` pollution, survives `Object.freeze` games). `runImmediately:true` applies it to the already-loaded document, not just future ones.

**Caveat (CONFIRMED):** state set in the isolated world is **not** visible to `Runtime.evaluate` in the main world (a naive read returned `{}`). All reads go through the binding or an `executionContextId`-targeted evaluate.

### What `mutations --follow` actually shows an agent

Never raw records. The observer ships one aggregate per animation frame (coalesced to ≥100 ms); the daemon coalesces again to the CLI cadence:

```
14:22:31.410  +42 −0   attrs:3   under @node-42 (main > ul.feed)   [class,aria-busy]
14:22:31.610  +0  −40  attrs:1   under @node-42                    [aria-busy]
14:22:32.100  TEXT     @node-77  "Loading…" → "12 results"
14:22:33.240  SUBTREE-REPLACED  @node-99 (div.modal-root)  4131 nodes  ⟵ action a7f3 (click @node-12)
```

Rules: (1) aggregate by nearest **stable ancestor** already carrying a `@node-NN` ref; (2) collapse >N sibling insertions into `+N`; (3) always render **attribute name lists** and **text before→after** in full — small and semantically load-bearing; (4) emit `SUBTREE-REPLACED` when removed+added under one parent exceed a threshold (the SPA route-change signature); (5) hard rate-limit to ~10 lines/s with a `… 4,120 more mutations suppressed` footer; (6) attribute to an `actionId` when inside that action's causal window (§9.3).

Record the observer options used (`childList, subtree, attributes, characterData`). `attributeOldValue`/`characterDataOldValue` roughly double record size — default **off**, enable via `--verbose`.

---

## 9. Performance, vitals, memory, coverage, tracing

### 9.1 The clock problem — solve this first

This is the most important practical finding in the whole dimension. Probing every event type for its timestamp field:

| Event | Timestamp | Domain |
|---|---|---|
| `Runtime.consoleAPICalled` | `1785867774330.792` | **WALL, ms since epoch** |
| `Runtime.exceptionThrown` | `1785867774466.241` | **WALL, ms since epoch** |
| `Log.entryAdded` (`entry.timestamp`) | `1785867774328.194` | **WALL, ms since epoch** |
| `Network.requestWillBeSent` | `93232.393611` | **MONOTONIC, seconds** |
| `Network.responseReceived/dataReceived/loadingFinished/loadingFailed` | `93232.40…` | **MONOTONIC, seconds** |
| `Page.domContentEventFired` / `loadEventFired` | `93232.548411` | **MONOTONIC, seconds** |
| `Performance.getMetrics` → `Timestamp` | `93236.408018` | **MONOTONIC, seconds** |
| Trace event `ts` | `93376560563` | **MONOTONIC, microseconds** |
| `Audits.issueAdded` | — | **NONE** |
| `Network.requestWillBeSentExtraInfo` / `responseReceivedExtraInfo` | — | **NONE** |
| `Page.frameNavigated` / `frameStarted*` / `frameStoppedLoading` | — | **NONE** |
| `Runtime.executionContextCreated` / `executionContextsCleared` | — | **NONE** |

So "sort only by monotonic" is not implementable: the console/exception/log stream has no monotonic field. The fix is a calibrated offset, and **`Network.requestWillBeSent` hands it to you for free on every single request** because it carries both clocks:

```
offset = requestWillBeSent.wallTime − requestWillBeSent.timestamp
```

Measured over 6 navigations spanning ~10 s:
```
1785774541.918136   1785774541.918140   1785774541.918142
1785774541.918145   1785774541.918147   1785774541.918152
n=6  spread = 16.0 µs   (monotonic upward drift ≈ 3 µs/sample — NTP slew, as expected)
cross-check: host_wall − Performance.Timestamp = 1785774541.918362   (Δ 226 µs, incl. CDP RTT)
cross-check: performance.timeOrigin/1000 − Performance.NavigationStart = same value
```

Three independent derivations agree to well under a millisecond. Rules:

1. Convert everything to **monotonic µs** at ingest — the inverse of the naive instinct.
2. `mono_us = (wall_ms/1000 − offset) × 1e6` for Runtime/Log events.
3. Re-derive `offset` on **every** `requestWillBeSent` and keep an EWMA; a step change > 100 ms means the wall clock jumped (NTP/suspend) — emit a `clock.jump` event rather than silently reordering history.
4. Events with **no** timestamp get daemon receipt time and `clock:"receipt"`. They are *not* precisely ordered; the CLI must not imply they are. `Audits.issueAdded` in particular is delivered asynchronously after the triggering load.
5. Trace `ts` is already the same base (§9.5) — divide by 1e6, no alignment needed.

### 9.2 `Performance.getMetrics` — the actual metric names (CONFIRMED, verbatim)

`Timestamp, AudioHandlers, AudioWorkletProcessors, Documents, Frames, JSEventListeners, LayoutObjects, MediaKeySessions, MediaKeys, Nodes, Resources, ContextLifecycleStateObservers, V8PerContextDatas, WorkerGlobalScopes, UACSSResources, RTCPeerConnections, ResourceFetchers, AdSubframes, DetachedScriptStates, ArrayBufferContents, LayoutCount, RecalcStyleCount, LayoutDuration, RecalcStyleDuration, DevToolsCommandDuration, ScriptDuration, V8CompileDuration, TaskDuration, TaskOtherDuration, ThreadTime, ProcessTime, JSHeapUsedSize, JSHeapTotalSize, FirstMeaningfulPaint, DomContentLoaded, NavigationStart`

36 metrics. Durations are **cumulative seconds** — diff two samples for an interval. `Performance.enable{timeDomain}` accepts `"timeTicks"` (monotonic, default) or `"threadTicks"`; `Performance.setTimeDomain` is EXPERIMENTAL and must be called **before** `enable`. A `Performance.metrics` event exists for push delivery.

These are **counters, not Web Vitals**. `FirstMeaningfulPaint` is a deprecated heuristic — do not present it as FCP.

### 9.3 `PerformanceTimeline` — the brief's assumption is wrong (CONFIRMED)

Probing `PerformanceTimeline.enable{eventTypes:[…]}` one type at a time against Chrome 151:

```
ACCEPTED: ['largest-contentful-paint', 'layout-shift']
REJECTED: longtask, long-animation-frame, first-input, event, paint, navigation, resource,
          mark, measure, element, visibility-state, back-forward-cache-restoration,
          soft-navigation, interaction, taskattribution, memory
          → error -32602 "Unknown or unsupported entry type"
```

The domain's `TimelineEvent` type structurally carries only `lcpDetails` and `layoutShiftDetails` (verified in the protocol JSON) — nothing else is even representable. **`PerformanceTimeline` is not a route to Web Vitals.** It gives exactly two, both valuable for their `DOM.BackendNodeId` bindings:

- LCP: `renderTime, loadTime, size, elementId, url, nodeId`
- CLS: `value, hadRecentInput, lastInputTime, sources[]` (each with `previousRect`/`currentRect`/`nodeId`)

Live sample:
```json
{"type":"largest-contentful-paint","name":"","time":1785863626.4765,
 "lcpDetails":{"renderTime":1785863626.4765,"loadTime":0,"size":234,"elementId":"a","nodeId":3}}
```
Note `time` is `Network.TimeSinceEpoch` — **wall-clock seconds**, a third clock convention. Normalise per §9.1.

### 9.4 The answer: injected `PerformanceObserver` (CONFIRMED)

Same isolated-world injection as the mutation observer. Chrome 151 reports:

```
PerformanceObserver.supportedEntryTypes = [
  element, event, first-input, interaction-contentful-paint, largest-contentful-paint,
  layout-shift, long-animation-frame, longtask, mark, measure, navigation, paint,
  resource, soft-navigation, visibility-state ]
```
(`interaction-contentful-paint` is new; absent from older docs.)

Live `navigation`, `paint` (`first-paint`, `first-contentful-paint`), `largest-contentful-paint`, `long-animation-frame`, `resource` and `visibility-state` entries arrived via `Runtime.bindingCalled`:
```json
{"entryType":"paint","name":"first-contentful-paint","startTime":36,"duration":0}
{"entryType":"largest-contentful-paint","name":"","startTime":36,"size":234,"id":"a","renderTime":36}
```

**Use `buffered:true`** so entries firing before injection are replayed — essential for `paint`/`navigation`. Compute INP yourself from `event` entries (`observe({type:'event', buffered:true, durationThreshold:16})`, high-percentile `interactionId` duration) and CLS from summed `layout-shift` values in session windows. **Do not vendor the `web-vitals` npm library** (external dep + its own reporting model); the arithmetic is ~120 lines.

**Gotcha (CONFIRMED):** `observe({type})` with an **unsupported** type does **not throw** — it silently no-ops forever. **Always gate on `supportedEntryTypes`** and report the supported set to the agent so it knows what it cannot have.

`PerformanceObserver` entry `startTime` is **milliseconds relative to `performance.timeOrigin`**, a fourth convention. Ship `performance.timeOrigin` with every batch and convert at ingest.

### 9.5 Tracing — measured, and the clock claim corrected

**`Tracing.start` must go to the *browser* session.** Sending it on a page session **reset the CDP connection**; on the browser session it returns `{}` immediately.

**Volume, measured.** A ~3 s trace of a trivial page with a DevTools-ish category set produced `10,016,478 bytes / 45,572 events`, dominated by `toplevel` (5.27 MB) and `toplevel,mojom` (1.86 MB). A **slim set** — `devtools.timeline`, `disabled-by-default-devtools.timeline`, `disabled-by-default-devtools.timeline.frame`, `blink.user_timing`, `loading`, `benchmark`, `rail` — over the same 3 s gave:

```
tracingComplete: {"dataLossOccurred":false,"stream":"1","traceFormat":"json","streamCompression":"gzip"}
compressed bytes:   81,811        ← what actually crosses the wire
decompressed bytes: 768,004
trace events:       4,032
bufferUsage: 5 reports, final percentFull = 0.00127
```

**`streamCompression:"gzip"` gives 9.4×** — 82 KB per 3 s ≈ **27 KB/s**, sustainable for a 30-minute job (~49 MB). Always use it; drain with `IO.read{handle,size}` → `IO.close`, gunzip with `flate2` **1.1.9**. `Tracing.getCategories` reports **283** categories. Always check `tracingComplete.dataLossOccurred` and report it rather than silently presenting a truncated trace. `Tracing.start` also accepts `perfettoConfig`, `tracingBackend` (`auto|chrome|system`), `screenshotMaxSize`, `screenshotMaxCount`, and `streamFormat: json|proto`.

**Trace clock — the brief's and the obvious assumption are both wrong.** Trace `ts` is **the same `CLOCK_MONOTONIC` base as `Network.timestamp`**, just µs instead of seconds. Verified by recording a trace and network capture simultaneously:

```
CDP   requestWillBeSent(Document).timestamp = 93422.068439 s
TRACE ResourceSendRequest (same URL)   ts   = 93422084130 us = 93422.084130 s   → Δ 15.7 ms
CDP   Page.domContentEventFired        ts   = 93422.226241 s
TRACE firstContentfulPaint             ts   = 93422226535 us = 93422.226535 s   → Δ 294 µs
TRACE navigationStart                  ts   = 93422066994 us = 93422.066994 s
```

The 15.7 ms on `ResourceSendRequest` is a genuine semantic difference (browser-process CDP dispatch vs renderer-side trace emission), not a clock offset — `firstContentfulPaint` matches to 294 µs. **No heuristic alignment is needed.** Better still:

```
trace ResourceSendRequest.args.data.requestId = 28B7E499107487376898BC1716FF2AF0
CDP   requestWillBeSent.requestId             = 28B7E499107487376898BC1716FF2AF0   ← identical
```

**`args.data.requestId` is an exact join key between the trace and the network stream.** Trace↔network correlation is a join, not an inference. Mark those links `KNOWN`.

### 9.6 Parsing the trace: FPS, dropped frames, long tasks

Format is `{traceEvents:[…], metadata:{…}}`. Observed phase distribution on the slim set: `X` (complete, 2626), `I` (instant, 550), `s`/`f` (flow, 217/217), `b`/`e` (async nestable, 151/149), `M` (metadata, 49), `R` (mark, 34), `n` (async instant, 31), `B` (begin, 8).

```json
// cat "disabled-by-default-devtools.timeline.frame"
{"name":"DrawFrame","ph":"I","ts":93376560563,"pid":35033,"tid":1759591,"s":"t","tts":1763,
 "args":{"frameSeqId":23018,"layerTreeId":2}}
// full set: NeedsBeginFrameChanged, BeginFrame, RequestMainThreadFrame,
//           BeginMainThreadFrame, ActivateLayerTree, DrawFrame

// cat "cc,benchmark,disabled-by-default-devtools.timeline.frame", ph "b"/"e", id2.local
{"name":"PipelineReporter","ph":"b","ts":…,"id2":{"local":"0x1"},
 "args":{"frame_reporter":{"state":"STATE_DROPPED","frame_sequence":35,
   "affects_smoothness":true,"has_high_latency":false,"scroll_state":"SCROLL_NONE",…}}}

// cat "disabled-by-default-devtools.timeline"
{"name":"RunTask","ph":"X","ts":…,"dur":125023,"tdur":…,"args":{}}
```

- **Long tasks** = `RunTask` with `dur > 50000` (µs). My fixture had one deliberate 120 ms busy loop; measured `n=1764, p50=5 µs, p95=76 µs, max=125,023 µs, >50 ms: 1`. **The detector works.** But prefer the injected `longtask`/`long-animation-frame` observer — orders of magnitude cheaper.
- **Dropped frames** = `PipelineReporter` async slices with `args.frame_reporter.state == "STATE_DROPPED"`. Observed distribution over 26 reporters: `STATE_PRESENTED_ALL: 8, STATE_NO_UPDATE_DESIRED: 4, STATE_DROPPED: 1, null: 13` (nulls are `e`-phase closers — match `b`/`e` by `(name, id2.local, pid, tid)`). `affects_smoothness` distinguishes "dropped and the user saw it" from "dropped harmlessly".
- **FPS = `DrawFrame` count per second — but this is unreliable in headless.** I measured **exactly 1 `DrawFrame` in 3 seconds** on the slim set, because `--headless=new` with no display produces almost no compositor frames. **FPS from tracing is only meaningful in headful mode or with a forced frame sink.** This is a real caveat for `--record-video` jobs.
- Also present: `Graphics.Pipeline` (cat `viz,benchmark,graphics.pipeline`, 329 events — the modern high-volume frame-pipeline event), `GPUTask`, `UpdateLayer`, `Commit`, `LayerTreeHostImpl::ActivateSyncTree`, `EventDispatch`, `ResourceSendRequest`/`ResourceReceiveResponse`, `firstContentfulPaint`, `largestContentfulPaint::Candidate`, `navigationStart`, `MinorGC`/`V8.GC_*`.
- **`LayoutShift` does NOT appear** in the trace with these categories. Get CLS from `PerformanceTimeline` or the injected observer, never from tracing.
- **Parsing gotcha:** 50 events had `ts == 0`, **all `ph == "M"`** (metadata: `thread_name`, `process_name`). Filter `ph != "M"` before computing any time range, or every trace looks like it starts at the epoch.

**Rust parser:** none suitable exists. crates.io has `chrome-trace-to-pprof` 0.1.3 (V8-CPU-profile-specific), `tracing-chrome` 0.7.2 (a *writer*, last updated 2024-03), and `perfetto` 0.0.0 (empty placeholder). **Write our own** in `crates/events/src/trace/`: a `serde` struct over `{name, cat, ph, ts, dur, tdur, pid, tid, id2, s, args}` plus an async-slice matcher keyed by `(name, id2.local, pid, tid)`. Use `simd-json` **0.17.3** or `sonic-rs` **0.5.8** for the parse; stream from `IO.read` chunks rather than materialising the whole string.

### 9.7 Memory, heap, coverage (all CONFIRMED callable)

- `Memory.getDOMCounters` → `{"documents":4,"nodes":37,"jsEventListeners":4}`. Cheap; poll for leak trend lines.
- `Memory.getDOMCountersForLeakDetection` → `-32000 "Failed to run leak detection"` in headless. **Do not rely on it.**
- `Runtime.getHeapUsage` → `{"usedSize":1077072,"totalSize":2097152,"embedderHeapUsedSize":4645216,"backingStorageSize":9646}` — cheapest heap signal, poll at 1 Hz.
- `Profiler.enable` + `Profiler.startPreciseCoverage{callCount:true, detailed:true}` → `{"timestamp":89089.626726}`; then `takePreciseCoverage`/`stopPreciseCoverage`. `detailed:true` gives block-level ranges (large) — default **off**.
- `HeapProfiler.takeHeapSnapshot{reportProgress, treatGlobalObjectsAsRoots, captureNumericValue, exposeInternals}` streams via **`HeapProfiler.addHeapSnapshotChunk`** events, *not* an `IO` handle. Real-app snapshots are 100s of MB. **Never buffer in memory** — write chunks straight to `artifacts/<job>/heap-<ts>.heapsnapshot`, hand the agent a path plus a summary, never the content. Gate behind an explicit `--heap-snapshot` flag. `HeapProfiler.startSampling{samplingInterval, stackDepth}` is the cheap alternative for allocation hot spots.

---

## 10. Unified event bus

### Normalised envelope

```rust
// crates/events/src/envelope.rs
pub struct Event {
    pub seq:       u64,               // daemon-assigned, total order, gap-free
    pub mono_us:   i64,               // normalised monotonic µs — the ONLY sort key
    pub clock:     Clock,             // Native | ConvertedFromWall { offset_us } | Receipt
    pub wall_us:   i64,               // derived, display only
    pub session:   SessionId,         // page / worker / service_worker / iframe target
    pub frame:     Option<FrameId>,
    pub gen:       DocGeneration,     // matches the Page Tree @node-NN generation
    pub kind:      Kind,              // Console | Exception | LogEntry | Issue | Net* | Ws* | Sse
                                      // | WebTransport | Perf | Mutation | Trace | Action | Job
    pub cause:     Cause,
    pub dedup_key: Option<u64>,       // cross-stream dedup (CSP/CORS fire 2–4×)
    pub payload:   Payload,
    pub redaction: RedactionReport,   // rules fired, counts — never the secrets
}

pub enum Clock { Native, ConvertedFromWall { offset_us: i64 }, Receipt }

pub enum Cause {
    Root,
    Action    { action_id: ActionId },                  // KNOWN: we performed it
    Initiator { action_id: ActionId, stack_hash: u64 }, // KNOWN: CDP initiator stack
    JoinId    { action_id: ActionId, via: &'static str },// KNOWN: exact id join (requestId, networkRequestId)
    Window    { action_id: ActionId, confidence: f32 }, // INFERRED: time-window join
    Unknown,
}
```

### Causality: what is known vs inferred — be blunt

| Link | Status | Mechanism |
|---|---|---|
| action → the DOM node it targeted | **KNOWN** | we issued the `Input.*` sequence at that ref |
| network request → issuing JS frame | **KNOWN** | `Initiator{type:"script", stack}` |
| network request → our action | **KNOWN, if** the initiator stack chains back to our synthetic handler (needs `setAsyncCallStackDepth > 0`) |
| **trace slice → network request** | **KNOWN** | `ResourceSendRequest.args.data.requestId == Network.requestId` (verified identical) |
| log entry (network source) → request | **KNOWN** | `LogEntry.networkRequestId` |
| **Audits issue → request** | **KNOWN** | `issue.details.*.request.requestId` |
| **Audits issue → source location** | **KNOWN** | `sourceCodeLocation{scriptId,url,lineNumber,columnNumber}` |
| console/exception → action | **KNOWN, if** the stack shares a frame with the action's handler |
| mutation → action | **INFERRED** | time window + subtree overlap |
| network request → route change | **INFERRED** | time window + `Page.frameNavigated`/History hooks |
| console error → causing request | **INFERRED** unless `networkRequestId` present | |
| `Audits.issueAdded` → *time* | **INFERRED** | no timestamp at all; receipt time only |

**Time-window heuristic:** after dispatching an action, open `[t_action, t_action + 2 s]`, extended while the network is non-idle up to a 10 s cap. Events in the window with no stronger link get `Cause::Window{confidence}`, decaying with elapsed time and rising with subtree overlap. **The CLI must render inferred links differently** (`⟵ action a7f3` known vs `≈ action a7f3 (inferred)`). An agent that believes an inferred link is fact writes a wrong bug report — worse than "unknown".

Two documented false-positive sources: (a) background polling/telemetry firing in every window (suppress via the long-poll/periodicity detector); (b) `requestAnimationFrame` render loops mutating continuously regardless of input.

### Cross-stream dedup

Compute `dedup_key` at ingest:
- CSP: `hash("csp", blockedURL, violatedDirective)` — joins `Log[security]` + `Audits{ContentSecurityPolicyIssue}` + `loadingFailed{blockedReason:"csp"}`.
- CORS: `hash("cors", url, corsError)` — joins **four** events.
- Cookie: `hash("cookie", cookie.name, domain, blockedReason)` — joins `Audits{CookieIssue}` + `responseReceivedExtraInfo.blockedCookies`.
- Deprecation: `hash("deprecation", type, sourceCodeLocation)` — joins `Audits{DeprecationIssue}` + `reportingApiReportAdded`.

Keep all raw events in the segment; present **one merged row** per `dedup_key` in the CLI, with the richest structured payload as primary and the prose message as `explanation`. The default view is one row; `--raw` shows all constituents.

---

## 11. Backpressure and retention

The brief's estimate is right, and my numbers scale it: the CDP DOM path alone was 775 KB / 3 s (≈ 465 MB / 30 min); a full trace was 10 MB / 3 s (≈ 6 GB / 30 min); the slim gzip'd trace is 27 KB/s (≈ 49 MB / 30 min, acceptable).

### Tiered retention

| Tier | Location | Retention | Contents |
|---|---|---|---|
| **Hot** | in-memory ring, per job, default 64 MB | evicts oldest | every event, full fidelity |
| **Warm** | `artifacts/<job>/events/NNNNN.jsonl.zst` | job lifetime | every event except sampled-out classes |
| **Cold** | derived artifacts | forever | HAR, action log, vitals summary, site graph, screenshots |
| **Never** | — | — | raw trace unless `--trace`; heap snapshots unless asked |

Segments roll at 16 MB uncompressed or 60 s, whichever first. `zstd` **0.13.3** level 3 (JSONL of similar events compresses ~10×). Each segment gets a sidecar index `{first_seq, last_seq, first_mono_us, last_mono_us, kind_counts, url_bloom}` so `--since`/`--filter` skip whole segments without decompressing.

### Class-specific policy

| Class | Policy |
|---|---|
| Console/exceptions | keep all; dedupe identical `(text, top stack frame)` into `count` + first/last ts |
| Audits issues | keep all — low volume, high value; dedupe by `dedup_key` |
| Network metadata | keep all — cheapest, highest-value stream |
| Network bodies | per policy; >1 MB spills to `artifacts/bodies/<sha256>`, event holds pointer + hash + size |
| WS frames | ring per socket (last N=1000 or 4 MB); count everything, retain a sample |
| SSE | same as WS |
| Mutations | aggregate at source (rAF); never store raw records |
| Perf entries | keep all — low volume by construction |
| Trace | off by default; slim categories + gzip; always to disk, never to the ring |

### Backpressure

The CDP transport must **never** block on a slow consumer. `tokio::sync::broadcast` per job with bounded capacity; on `RecvError::Lagged(n)` the CLI prints `⚠ dropped n events (consumer too slow)` — **visible loss, never silent loss**. The disk writer gets its own unbounded-but-spilling channel. If it falls behind by more than one segment, degrade in this order: (1) drop trace, (2) drop mutation aggregates, (3) drop WS/SSE frame payloads keeping counts, (4) drop console arg previews keeping text, (5) finally drop network metadata and set `degraded: true` on the job, reported prominently by `status`.

### Agent-facing query surface

```
browserctl network requests  --since 2m --url '*/api/*' --status '>=400' --type xhr,fetch \
                             --min-duration 500ms --initiator script --limit 20 --format table
browserctl network request <id> --headers --body --timing --initiator-stack --reveal <path>
browserctl network search <id> --query 'session_id'      # Network.searchInResponseBody, no body transfer
browserctl console          --follow --level error,warning --source runtime,log,violation --dedupe
browserctl issues           --code CorsIssue,CookieIssue,DeprecationIssue   # Audits stream
browserctl exceptions       --with-async-stack --source-mapped
browserctl ws frames <socket-id> --direction recv --since 1m --decode json --limit 50
browserctl perf vitals      # LCP/CLS/INP/FCP/TTFB + supported-entry-types caveat
browserctl perf longtasks   --min 100ms
browserctl mutations        --follow --scope @node-42 --min-batch 5
browserctl events           --since <ts> --kinds net,console,issue --cause <actionId>
browserctl export har       --out run.har [--include-bodies] [--unredacted]   # last needs approval
```

Every command defaults to a **compact table** with a hard row cap and `--format json` for machine reads. Every truncation prints `… N more (use --limit)`. Token discipline is a first-class requirement.

**Crates (versions verified on crates.io, 2026-08-04):** `tokio` **1.53.1**, `serde_json` **1.0.151**, `simd-json` **0.17.3**, `sonic-rs` **0.5.8**, `bytes` **1.12.1**, `zstd` **0.13.3**, `flate2` **1.1.9**, `regex` **1.13.1**, `aho-corasick` **1.1.5**, `memchr` **2.8.3**, `rustc-hash` **2.1.3**, `parking_lot` **0.12.5**, `url` **2.5.8**, `sourcemap` **9.3.2**, `jiff` **0.2.35** (over `chrono`) for HAR ISO-8601, `har` **0.9.0** (tests only), `uuid` **1.24.0**, `base64` **0.23.1**, `tungstenite` **0.30.0** (only if we ever need a WS client; the CDP pipe transport does not).

---

## What we verified empirically

Local Chrome **151.0.7922.72** (V8 15.1.206.10, protocol 1.3, HeadlessChrome/151), `--headless=new`, scratch `--user-data-dir` under `/private/tmp/browprobe`, driven by a from-scratch stdlib-only Python WebSocket CDP client against purpose-built local HTTP/SSE servers. **All Chrome instances and servers killed afterwards; port 39871 confirmed free.**

| # | What we ran | Raw observation |
|---|---|---|
| 1 | `/json/version`, `/json/protocol` | Chrome 151.0.7922.72; **57 domains**; 1,605,774 bytes |
| 2 | `Audits.enable` on a fixture | **5 issues**: `CookieIssue`, `ContentSecurityPolicyIssue` ×2, `DeprecationIssue`, `QuirksModeIssue`, each with structured details |
| 3 | Deprecation routing | `unload` handler → `Audits{DeprecationIssue, type:"UnloadHandler"}` + `reportingApiReportAdded{type:"deprecation"}`; **zero** `Log.entryAdded` with `source:"deprecation"` |
| 4 | CSP violation routing | fires in `Log[security/error]` **and** `Audits{ContentSecurityPolicyIssue}` **and** `loadingFailed{blockedReason:"csp"}` — dedup required |
| 5 | CORS routing (3 distinct failures) | each fires **4** events; explanatory `Log` entry has `source:"javascript", category:"cors"`, **not** `network`; `Audits{CorsIssue}` carries `MissingAllowOriginHeader` / `WildcardOriginNotAllowed` / `PreflightMissingAllowOriginHeader` |
| 6 | `Network.enableReportingApi` | `reportingApiReportAdded` then 2× `reportingApiReportUpdated`; status `Queued→Pending→Queued`; body has `message`, `sourceFile`, `lineNumber` |
| 7 | `Log.startViolationsReport` | **CONFIRMED firing**: `[violation/verbose] 'setTimeout' handler took 399ms` / `700ms`. Level is **`verbose`** — an error/warning filter drops them |
| 8 | Timestamp domain per event type | Runtime/Log = **wall ms**; Network/Page/Performance = **monotonic s**; trace = **monotonic µs**; `Audits.issueAdded`, `*ExtraInfo`, `frameNavigated`, `executionContext*` = **no timestamp** |
| 9 | Clock offset stability | `requestWillBeSent.wallTime − .timestamp` over 6 navigations: **spread 16.0 µs**, value `1785774541.9181…`; cross-checked to 226 µs against host clock and to sub-ms against `performance.timeOrigin − NavigationStart` |
| 10 | Trace vs network clock | trace `ResourceSendRequest ts` = 93422.084130 s vs CDP `requestWillBeSent` 93422.068439 s (Δ 15.7 ms, semantic); trace `firstContentfulPaint` vs `domContentEventFired` **Δ 294 µs** → **same monotonic base** |
| 11 | Trace↔network join key | trace `args.data.requestId` = `28B7E499107487376898BC1716FF2AF0` **== CDP `requestId`** |
| 12 | `streamResourceContent` from `requestWillBeSent`, **no Fetch** | `/big`: RTT 17.6 ms, `bufferedData` 1,432,236 b64, 2/28 `dataReceived` inline; **buffered ++ inline = 3,072,003 B vs 3,072,000 B reported** |
| 13 | Same, chunked/slow resource | `/slow`: `bufferedData` 10,924 b64, **39/41** `dataReceived` inline; total ≈ 327,720 vs 327,680 reported |
| 14 | `streamResourceContent` race | `/favicon.ico` → `-32602 "Request with the provided ID has already finished loading"` — new distinct error string |
| 15 | `getResponseBody` same-document | succeeded for **all** requests incl. the 3,072,000-byte body, post-`loadingFinished`, no navigation |
| 16 | Body eviction | OK before nav; **`-32000 "No resource with given identifier found"` after nav to `about:blank`** and after cross-origin nav |
| 17 | In-flight body | open SSE → `"No data found for resource with given identifier"` (distinct error) |
| 18 | Multi-`Set-Cookie` encoding | `responseReceivedExtraInfo.headers["Set-Cookie"]` = `"sess=SUPERSECRET123; SameSite=None\nplain=abc"` — **`\n`-joined** |
| 19 | Secret exposure | `X-Api-Key: sk_live_deadbeefcafebabe0123` and `blockedCookies[0].cookie.value = "SUPERSECRET123"` fully visible → redaction mandatory |
| 20 | Cookie blocking detail | `blockedReasons:["SameSiteNoneInsecure"]`, plus `Audits{CookieIssue}` with `cookieWarningReasons`/`cookieExclusionReasons` |
| 21 | `loadingFailed` shapes | CSP → **empty `errorText`** + `blockedReason:"csp"`; CORS → `net::ERR_FAILED` + `corsErrorStatus`; SSE close → `EventSource`/`ERR_ABORTED`/`canceled:true` |
| 22 | `requestId` formats | subresources `"34443.2"`; main document a 32-hex id **equal to its `loaderId`**; Audits uses the 32-hex form |
| 23 | Slim trace, gzip | **81,811 compressed / 768,004 decompressed / 4,032 events / 3 s** → 9.4× compression, ~27 KB/s |
| 24 | Trace long-task detector | `RunTask n=1764, p50=5 µs, p95=76 µs, max=125,023 µs`, **1 task >50 ms** matching the deliberate 120 ms busy loop |
| 25 | Dropped frames | `PipelineReporter` n=26, states `STATE_PRESENTED_ALL:8, STATE_NO_UPDATE_DESIRED:4, STATE_DROPPED:1, null:13` |
| 26 | FPS in headless | **`DrawFrame` count = 1 in 3 s** → FPS-from-tracing is not meaningful headless |
| 27 | Trace parsing gotcha | 50 events with `ts == 0`, **all `ph == "M"`** (metadata) |
| 28 | `Tracing.start` session | on a **page** session → CDP connection reset; on the **browser** session → `{}` |
| 29 | `PerformanceTimeline.enable` per type | **only `largest-contentful-paint` + `layout-shift` accepted**; 16 others → `-32602 "Unknown or unsupported entry type"`. **Re-verified 2026-08-04, independent run:** `largest-contentful-paint` ACCEPTED, `layout-shift` ACCEPTED; `paint`, `first-input`, `longtask`, `long-animation-frame`, `mark`, `measure`, `navigation`, `resource`, `element`, `event` all REJECTED with that exact error. `TimelineEvent` type properties are exactly `frameId, type, name, time, duration, lcpDetails, layoutShiftDetails` — structurally incapable of carrying the others |
| 30 | `Performance.getMetrics` | 36 metrics, names listed verbatim in §9.2 |
| 31 | Injected `PerformanceObserver` | `supportedEntryTypes` = 15 incl. `long-animation-frame`, `interaction-contentful-paint`; live FCP/LCP/navigation/resource/LoAF via `bindingCalled` |
| 32 | Unsupported `observe({type})` | **silently no-ops, does not throw** |
| 33 | CDP DOM mutations | **5,651 `childNodeCountUpdated`, 1 `childNodeInserted`, 775,226 bytes / 3 s**; `getDocument` tree = 185,210 bytes |
| 34 | `MutationObserver` isolated world | same workload → **2,976 records in 63 batches, 438 bytes shipped** |
| 35 | Isolated-world isolation | main-world `Runtime.evaluate` **cannot see** isolated-world state |
| 36 | Console vs Log disjointness | 36 `consoleAPICalled` vs 6 `Log.entryAdded`, **0 overlap** on `console.*` |
| 37 | RemoteObject previews | preview present **without** requesting it; capped at **5 props**; circular → `"Object"`; DOM node → IDL attrs, not markup |
| 38 | Exceptions | unhandled rejection distinguished **only** by `text == "Uncaught (in promise)"` |
| 39 | `Debugger.setAsyncCallStackDepth` | accepted without `Debugger.enable` → `parentId`; with it → inlined `parent{description:"setTimeout"}` |
| 40 | Workers | auto-attach yielded `worker` + `service_worker`; structured events on worker session; page session saw only `Log[worker]` flat text; **`Runtime.enable` on `service_worker` timed out** |
| 41 | WebSocket frames | opcode 1 → raw text; **opcode 2 → base64**; no `isBinary` flag; handshake headers incl. `permessage-deflate` |
| 42 | SSE | `eventSourceMessageReceived` with `eventName`/`eventId`/`data`; unnamed → `"message"` |
| 43 | Memory | `getDOMCounters` OK; **`getDOMCountersForLeakDetection` → `-32000 "Failed to run leak detection"`**; `Runtime.getHeapUsage` OK; `startPreciseCoverage` OK |
| 44 | `Tracing.getCategories` | 283 categories |
| 45 | DevTools HAR source | read `Log.ts` — exact `buildTimings`, extension fields, and its narrow `sanitize` (only `set-cookie`/`authorization`/`cookie`) |
| 46 | crates.io versions | all crate versions in §11 fetched live from the crates.io API on 2026-08-04 |

---

## Limits and impossibilities

Stated bluntly, as requested.

1. **`PerformanceTimeline` cannot deliver Web Vitals.** Only LCP and CLS. FCP, TTFB, INP, longtask, LoAF, `mark`/`measure` all require injecting JS. There is no CDP-only path. If injection is blocked, those metrics are simply unavailable.

2. **Retroactive body capture is impossible after navigation.** Verified even for a same-process `about:blank`. All body policy must be decided at `requestWillBeSent`. A crawler that navigates and *then* wants a body has already lost.

3. **`streamResourceContent` cannot capture small fast resources.** They finish inside the subscribe RTT (0.2 ms for favicon) and return `-32602`. The `getResponseBody` fallback covers exactly this gap, but that fallback is itself navigation-fragile — so there is a genuine window where a small resource's body is unrecoverable if you navigate immediately.

4. **You cannot have both un-perturbed timings and guaranteed bodies.** `Fetch` interception guarantees bodies but serialises every load and corrupts timing. `streamResourceContent` preserves timing but is EXPERIMENTAL and races. Pick per job; do not pretend one mode does both.

5. **`Audits.issueAdded` has no timestamp.** Issues cannot be precisely ordered against the network/console streams — only receipt-ordered. Any UI showing an issue interleaved at a precise time is fabricating precision.

6. **Console/log events have no monotonic timestamp**, so every cross-stream ordering depends on a calibrated offset. Stable to 16 µs in my measurement, but a wall-clock jump (NTP step, laptop suspend) invalidates it retroactively. Detect and flag; you cannot fully prevent it.

7. **CDP `DOM.*` events cannot give mutation *content* at scale.** `childNodeCountUpdated` is a counter. Recovering content requires per-node `requestChildNodes` calls that themselves generate events. No configuration makes this competitive with `MutationObserver`.

8. **`Network.webSocketClosed` carries no close code and no reason.** You cannot tell an agent *why* a socket closed from CDP. Workaround (hooking `WebSocket.prototype` in the isolated world) is page-visible instrumentation and violates the invisible-observer principle. Recommend documenting the gap.

9. **`fetch()`-based SSE is invisible to `eventSourceMessageReceived`.** Only the `EventSource` API produces framed events. Modern streaming UIs (including LLM chat) mostly use `fetch` + `ReadableStream` and require a hand-written `text/event-stream` framer over the streamed body. **CONFIRMED 2026-08-04** by a two-consumer fixture on one endpoint: `resourceType` `"EventSource"` → 4 framed events; `resourceType` `"Fetch"` → 0, with identical bytes. (Was LIKELY; the framer is now known-necessary.)

10. **FPS from tracing is not meaningful in headless.** 1 `DrawFrame` in 3 seconds. Frame-rate claims require headful mode or a forced frame sink, and `--record-video` jobs must state which.

11. **Service worker sessions are unreliable via page-level auto-attach** (`Runtime.enable` timed out). Do not promise full SW console/error capture in v1.

12. **Worker console output on the page session is text-only** — no structured args, no expandable objects, no stack traces. Structured worker diagnostics require N extra sessions and N× domain-enable cost.

13. **Most mutation and route-change causality is inferred.** Only our own dispatch, initiator stacks, and exact ID joins (`requestId`, `networkRequestId`, `Audits` `request.requestId`, trace `args.data.requestId`) are ground truth. Any UI rendering inferred and known links identically is lying to the agent.

14. **Redaction and debuggability are in genuine tension** and no regex resolves it. The honest resolution is a capability-gated reveal with a bounded memory-only cache — which means there *is* a window where secrets live in daemon memory. Document it.

15. **`Memory.getDOMCountersForLeakDetection` does not work in headless.** Leak detection is off the table for now.

16. **`enableDurableMessages` / `configureDurableMessages` behaviour is unverified.** Accepted by Chrome 151; I did not confirm it rescues bodies across navigation. Needs a dedicated test before the body strategy leans on it.

17. **`streamResourceContent`, `dataReceived.data`, `configureDurableMessages`, `searchInResponseBody`, `loadNetworkResource`, the whole `Audits` domain, `PerformanceTimeline`, `Preload`, and all `*ExtraInfo` events are EXPERIMENTAL.** They can change or vanish in any Chrome release. The design needs version-gated fallbacks and a CI canary against Chrome stable/beta.

---

## Open questions for the owner

1. **Body capture default:** stream *all* bodies under N KB, or only an allow-list of content types (`json`, `text`, `xml`, `html`, `javascript`)? The former is far more useful and far more expensive.
2. **Is `Audits.enable` on by default?** It is low-volume and high-value (it is the *only* structured source for deprecations and cookie problems), but it adds a third stream to merge and dedupe. My recommendation: yes, always on.
3. **Does `--record-video` imply tracing?** Frame-accurate FPS/dropped-frame data needs a trace at ~27 KB/s gzipped — but FPS is meaningless headless (§limit 10). Do we force headful for video jobs, or accept coarser LoAF-based smoothness metrics?
4. **Reveal cache:** is a 64 MB / 10-minute memory-only pre-redaction cache acceptable, or must redaction be genuinely irreversible (in which case `--reveal` cannot exist and auth debugging gets much harder)?
5. **Per-worker sessions by default?** Real cost on worker-heavy apps. Default on, default off, or auto-enable when a worker throws?
6. **Async stack depth default:** 32 (useful traces, real V8 cost) or 0 (cheap, but "at anonymous:1:1" is often useless)? Should it differ between interactive and background-job modes?
7. **`Debugger.enable` default?** Inlined async parents and source-map `scriptParsed` events are valuable, but risks pausing on the page's own `debugger;`. Suggest off by default, with `Debugger.setSkipAllPauses{skip:true}` if enabled.
8. **Event segment format:** JSONL+zstd (greppable, debuggable, simple) or Arrow/Parquet for fast `--filter` on long jobs? JSONL is my v1 recommendation.
9. **Do we capture WebTransport and direct-socket traffic in v1**, or defer? They cannot go in a HAR and need their own artifact format.
10. **Do we need HAR *import*?** Trivial via the `har` crate; would let an agent diff a run against a reference. v1 or later?
11. **CI canary against Chrome beta/dev** to catch removal of the experimental surface we depend on — worth the maintenance cost?

---

## Sources

1. https://chromedevtools.github.io/devtools-protocol/tot/Network/ — Network domain events, methods, `ResourceTiming`, `Initiator`, `CorsErrorStatus`, experimental markers
2. https://chromedevtools.github.io/devtools-protocol/tot/Runtime/ — `RemoteObject`, `ObjectPreview`, `PropertyPreview`, `callFunctionOn`, `evaluate`, `SerializationOptions`, `addBinding`
3. https://chromedevtools.github.io/devtools-protocol/tot/Log/ — `Log.entryAdded`, `LogEntry.source` enum, `startViolationsReport`, `ViolationSetting`
4. https://chromedevtools.github.io/devtools-protocol/tot/Audits/ — `Audits.issueAdded`, `InspectorIssueCode`, per-issue detail types
5. https://chromedevtools.github.io/devtools-protocol/tot/PerformanceTimeline/ — `enable{eventTypes}`, `TimelineEvent`, `LargestContentfulPaint`, `LayoutShift`
6. https://chromedevtools.github.io/devtools-protocol/tot/Tracing/ — `Tracing.start` params, `transferMode`, `streamFormat`, `tracingComplete`, `bufferUsage`, `IO.read`
7. https://chromedevtools.github.io/devtools-protocol/tot/Fetch/ — `Fetch.enable`, `RequestPattern`, `requestPaused`, `getResponseBody`, `takeResponseBodyAsStream`
8. https://chromedevtools.github.io/devtools-protocol/tot/Preload/ — `prefetchStatusUpdated`, `prerenderStatusUpdated`, `PrefetchStatus`/`PrerenderFinalStatus` enums
9. https://raw.githubusercontent.com/ChromeDevTools/devtools-frontend/main/front_end/models/har/Log.ts — **primary source** for `buildTimings()`, `pseudoWallTime`, `buildContent`, `sanitize`, all `_`-prefixed extension fields
10. https://github.com/ChromeDevTools/devtools-frontend/blob/main/front_end/models/har/Writer.ts — HAR stream writing and content-encoding decisions
11. https://developer.chrome.com/docs/devtools/performance/timeline-reference — DevTools timeline event categories
12. https://www.chromium.org/developers/how-tos/trace-event-profiling-tool/ — trace event format background
13. https://www.chromium.org/developers/how-tos/trace-event-profiling-tool/frame-viewer/ — frame viewer / `PipelineReporter` frame states
14. https://w3c.github.io/performance-timeline/ — `PerformanceObserver` / `PerformanceEntry` semantics (referenced by the CDP domain description itself)
15. https://crates.io/api/v1/crates/{tokio,serde_json,regex,aho-corasick,zstd,simd-json,sonic-rs,sourcemap,har,jiff,base64,bytes,memchr,uuid,parking_lot,rustc-hash,url,flate2,tungstenite} — version and freshness data, fetched 2026-08-04
16. `http://127.0.0.1:39871/json/version` and `/json/protocol` on local Chrome 151.0.7922.72, plus ~46 live CDP experiments described in "What we verified empirically" — **primary source** for every CONFIRMED claim
17. http://www.softwareishard.com/blog/har-12-spec/ — HAR 1.2 specification (**fetch previously FAILED: TLS certificate expired**; field semantics therefore taken from DevTools' `Log.ts`, which is the stronger compatibility target anyway)

---

## Verification pass — 2026-08-04 (adversarial re-check)

Re-read a fresh `/json/protocol` dump from local Chrome **151.0.7922.72** (1,605,774 B; 57 domains / 669 commands / 237 events / 616 types — identical to this document's figures) and re-ran the two claims that were most exposed.

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| 1 | `PerformanceTimeline.enable` accepts only LCP + layout-shift ⇒ Web Vitals need an injected observer | **CONFIRMED** | Live re-test: `largest-contentful-paint` ACCEPTED, `layout-shift` ACCEPTED; `paint`, `first-input`, `longtask`, `long-animation-frame`, `mark`, `measure`, `navigation`, `resource`, `element`, `event` → `-32602 "Unknown or unsupported entry type"`. `TimelineEvent` properties are exactly `frameId, type, name, time, duration, lcpDetails, layoutShiftDetails` — the limit is structural, not a filter |
| 2 | `fetch()`-based SSE is invisible to `eventSourceMessageReceived` (was **LIKELY**) | **CONFIRMED** | Two-consumer fixture on one `/sse` endpoint: `resourceType:"EventSource"` → 4 framed events; `resourceType:"Fetch"` → 0, identical bytes. Framer is required |
| 3 | `Audits.issueAdded` carries no timestamp | **CONFIRMED** | Parameters are exactly `['issue']` |
| 4 | `*ExtraInfo` events carry no timestamp and are EXPERIMENTAL | **CONFIRMED** | `requestWillBeSentExtraInfo` (exp) → `requestId, associatedCookies, headers, connectTiming, deviceBoundSessionUsages, clientSecurityState, siteHasCookieInOtherPartition, appliedNetworkConditionsId`; `responseReceivedExtraInfo` (exp) → `requestId, blockedCookies, headers, resourceIPAddressSpace, statusCode, headersText, cookiePartitionKey, cookiePartitionKeyOpaque, exemptedCookies`. No `timestamp` on either |
| 5 | `Page.frameNavigated` carries no timestamp | **CONFIRMED** | Parameters are exactly `['frame','type']` |
| 6 | `Network.webSocketClosed` gives no close code or reason | **CONFIRMED** | Parameters are exactly `requestId` (`RequestId`) and `timestamp` (`MonotonicTime`) |
| 7 | `streamResourceContent` and `dataReceived.data` are EXPERIMENTAL | **CONFIRMED** | `Network.streamResourceContent` `experimental: true`; `Network.dataReceived` is stable but its `data` parameter is `experimental: true` |
| 8 | `configureDurableMessages`, `searchInResponseBody`, `loadNetworkResource` exist and are experimental | **CONFIRMED** | `Network.configureDurableMessages` present; `searchInResponseBody` and `loadNetworkResource` both `experimental: true`. `Network.enable` also carries experimental `enableDurableMessages` and `reportDirectSocketTraffic` params |
| 9 | `Console` domain is deprecated | **CONFIRMED** | `deprecated: true` at domain level |
| 10 | `Runtime.consoleAPICalled` has a `timestamp` but `Log.entryAdded` uses `Runtime.Timestamp` (wall ms) | **CONFIRMED** | `consoleAPICalled` params `type, args, executionContextId, timestamp, stackTrace, context`; `LogEntry.timestamp` is `$ref: Runtime.Timestamp`. The calibration step this document prescribes is genuinely unavoidable |

**Not re-tested (still carry the document's own risk notes):** the 16 µs wall/monotonic offset stability over long jobs; trace `ts` clock identity across the GPU/network processes; the `streamResourceContent` subscribe-RTT race at scale; MutationObserver behaviour against adversarial pages; the redaction regex false-positive/negative rates; headless `DrawFrame` counts.

**Cross-reference added:** `Runtime.evaluate{timeout:N}` was verified (in `80-…`) to genuinely terminate a runaway script — 2008 ms for `timeout:2000` on `for(;;){i++}`. That is the bound to use on the isolated-world observer bootstrap and on any `callFunctionOn` expansion in §2, which this document currently leaves unbounded.
