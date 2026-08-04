# Event Streams: Console, Exceptions, Network, WS/SSE, HAR, Performance, Tracing, Mutations

> **Bottom line.** Everything the spec asks for is achievable over raw CDP, but four of the assumptions in the brief are wrong and must be corrected before design freezes. (1) `Runtime.consoleAPICalled` and `Log.entryAdded` do **not** overlap at all in Chrome 151 — they are disjoint sources and you must consume both. (2) `PerformanceTimeline.enable` accepts **only** `largest-contentful-paint` and `layout-shift`; every other entry type is rejected outright, so Web Vitals (FCP/INP/TTFB/longtask/LoAF) **require** an injected `PerformanceObserver` in an isolated world — this is not optional. (3) Response bodies are evicted by *any* navigation, including same-process `about:blank`; the reliable answer is `Network.streamResourceContent` (experimental, verified working), **not** Fetch interception, which serialises the load. (4) The CDP `DOM.*` mutation events are a low-fidelity firehose — measured 5,651 events / 775 KB in 3 s that carry *counts, not content* — while an isolated-world `MutationObserver` captured the same 2,976 real mutations and shipped 438 bytes. Build on: dual console sources, a per-request state machine that joins `*ExtraInfo` events, ingest-time redaction, injected observers for vitals and mutations, and a slim trace category set (measured 396 KB vs 10 MB for the default-ish set).

All protocol claims below were checked against the `/json/protocol` dump of the **actual local Chrome: 151.0.7922.72, V8 15.1.206.10, protocol 1.3, 51 domains**, and most were exercised against a live instrumented page. Confidence is marked per claim.

---

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| Consume **both** `Runtime.consoleAPICalled` and `Log.entryAdded`; never dedupe | Verified disjoint: 36 console events vs 6 Log entries, zero overlap. Log carries CSP/network/deprecation/worker; Runtime carries `console.*` | Log-only (loses all `console.*` args); Runtime-only (loses CSP, net errors, deprecations) | **confirmed** |
| Web Vitals via **injected `PerformanceObserver`** in an isolated world at `document-start` | `PerformanceTimeline.enable` rejects all types except LCP + layout-shift; observer covers 15 types | `PerformanceTimeline` domain as primary; injecting the `web-vitals` npm lib (external dep, telemetry surface) | **confirmed** |
| Bodies via `Network.streamResourceContent` subscribed at `requestWillBeSent` | Verified: inline `data` on `dataReceived` delivered 4.19 MB b64 (~3 MB) with zero load serialisation | `Fetch` interception for all requests (serialises every load, changes timing); `getResponseBody` alone (evicted on nav) | **confirmed** |
| Also call `Network.configureDurableMessages` at session start | Survives cross-process navigation; `enableDurableMessages` on `Network.enable` is documented as being deprecated in favour of it (deadlock risk) | `Network.enable{enableDurableMessages}` | **confirmed** (protocol text) / **unverified** (behaviour) |
| Mutations via isolated-world `MutationObserver`, rAF-batched | 1,770× less wire volume at *higher* fidelity than `DOM.*` events | `DOM.enable` + `childNodeInserted/…` | **confirmed** |
| Redact at **ingest**, before the event ever reaches the ring buffer or disk | A secret that touches disk is a secret leaked; retention/export paths multiply | Redact at export/persistence | **confirmed** (design) |
| Slim trace categories; tracing is opt-in per job, never always-on | Measured 10.0 MB / 45,572 events for 3 s; `toplevel` alone was 5.27 MB. Slim set = 396 KB | Default DevTools category set | **confirmed** |
| Write our own Chrome trace-event parser (~200 LOC) | No maintained general-purpose Rust crate exists; format is a trivial JSON array of `{ph,ts,dur,name,cat,args}` | `chrome-trace-to-pprof` (V8-profile-specific), `perfetto` crate (0.0.0 placeholder) | **confirmed** |
| Per-target session fan-out via `Target.setAutoAttach{flatten:true}` for workers | Worker `console.*` reaches the page only as flat text in `Log.entryAdded` (source=`worker`); structured args/stacks need the worker's own session | Page session only | **confirmed** |
| Emit HAR 1.2 + Chrome's `_`-prefixed extensions, replicating DevTools' `buildTimings` verbatim | Compatibility with DevTools/HAR viewers; DevTools' own algorithm is the de-facto spec | Hand-rolled timings | **confirmed** (read source) |
| Causality = explicit `actionId` propagation + initiator stacks; time-window joins marked `inferred` | Only initiator stacks are ground truth; everything else is a heuristic and must be labelled | Presenting time-window joins as fact | **confirmed** (design) |

---

## 1. Console and errors: exact routing

**This is the single most misunderstood area, and I measured it.** I loaded a page firing 18 distinct `console.*` calls, an unhandled rejection, two uncaught throws, a CSP-blocked script, three failing fetches, and a 404 image, with `Runtime.enable` + `Log.enable` + `Log.startViolationsReport` all active.

**Result: the two streams were completely disjoint.**

`Log.entryAdded` fired exactly 6 times, all of them browser-generated:

```
[network/error]  networkRequestId=8611.2  Failed to load resource: the server responded with a status of 404 (Not Found)
[security/error] networkRequestId=null    Loading the script 'https://example.invalid/blocked.js' violates the following
                                          Content Security Policy directive: "script-src 'self' 'unsafe-inline'" ...
[network/error]  networkRequestId=8611.7  Failed to load resource: net::ERR_UNSAFE_PORT
[network/error]  networkRequestId=8611.8  Failed to load resource: net::ERR_UNSAFE_PORT
[network/error]  networkRequestId=8611.5  Failed to load resource: ... 404 ...
[network/error]  networkRequestId=8611.13 Failed to load resource: ... 404 ...
```

`Runtime.consoleAPICalled` fired 36 times and carried **every** `console.*` call — `log, warning, error, table, trace, count, timeEnd, startGroup, endGroup, assert, dir` — with structured `args: RemoteObject[]`. **Not one `console.*` call appeared in `Log.entryAdded`.**

### The routing table (CONFIRMED unless noted)

| Message class | Event | `LogEntry.source` | Notes |
|---|---|---|---|
| `console.log/info/debug/warn/error/table/trace/dir/dirxml/group/count/time*/assert` | `Runtime.consoleAPICalled` | — | 18-value `type` enum; structured `args` |
| Uncaught exception | `Runtime.exceptionThrown` | — | `exceptionDetails.text = "Uncaught"` |
| Unhandled promise rejection | `Runtime.exceptionThrown` | — | `text = "Uncaught (in promise)"` — **only** discriminator |
| Subresource load failure (404, ERR_*) | `Log.entryAdded` | `network` | Carries **`networkRequestId`** → join key to the Network stream |
| CSP violation | `Log.entryAdded` | `security` | No `networkRequestId`; also surfaces as `loadingFailed{blockedReason:"csp"}` |
| CORS failure | `Log.entryAdded` | `network`, `category:"cors"` | `category` enum has exactly one value: `cors` |
| Deprecation warnings | `Log.entryAdded` | `deprecation` | Not triggered in my fixture — **likely**, from the enum |
| Rendering/layout warnings | `Log.entryAdded` | `rendering` | **likely** |
| Interventions | `Log.entryAdded` | `intervention` | **likely** |
| Violations (longTask, handler, …) | `Log.entryAdded` | `violation` | Requires `Log.startViolationsReport` |
| Worker `console.*` + worker uncaught errors | `Log.entryAdded` on the **page** session | `worker` | **Flat text only + `workerId`.** No args, no stack |
| XML parse errors, storage, appcache | `Log.entryAdded` | `xml`/`storage`/`appcache` | **likely** |

`LogEntry.source` full enum (from the local protocol dump): `xml, javascript, network, storage, appcache, rendering, security, deprecation, worker, violation, intervention, recommendation, other`. `level`: `verbose, info, warning, error`.

**Design consequence:** `browserctl console` must merge two streams. Tag each normalised record with `origin: "runtime" | "log"` so an agent can filter. Do **not** attempt dedup — there is nothing to dedup.

### Violations

`Log.startViolationsReport{config: ViolationSetting[]}` with names `longTask, longLayout, blockedEvent, blockedParser, discouragedAPIUse, handler, recurringHandler`, each with a `threshold` (ms). CONFIRMED accepted by Chrome 151. My fixture was too fast to trip any — **unverified** that entries actually arrive, but this is the same mechanism DevTools' "Verbose → Violations" filter uses.

### Workers need their own sessions

Verified with `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}` on the page session:

```
attachedToTarget: [('worker',         '/w.js',  waitingForDebugger=True),
                   ('service_worker', '/sw.js', waitingForDebugger=True)]
```

On the **worker's own session** (after `Runtime.enable` + `Runtime.runIfWaitingForDebugger`) I got fully structured `Runtime.consoleAPICalled` and `Runtime.exceptionThrown`. On the **page** session the same events appeared only as:

```
Log[worker] workerId=B7BF019B8E86C0DF0EFB612FD2D4FEE2  hello from dedicated worker
Log[worker] workerId=B7BF019B8E86C0DF0EFB612FD2D4FEE2  Uncaught Error: WORKER UNCAUGHT ERROR
```

**`Runtime.enable` on the auto-attached `service_worker` session timed out.** Service workers attached from a *page*-level auto-attach are not reliably drivable; use browser-level `Target.setAutoAttach` and/or the `ServiceWorker` domain. Flag as a known rough edge.

Always send `Runtime.runIfWaitingForDebugger` after enabling domains, or the worker hangs forever.

### Stack traces and async chains

`Debugger.setAsyncCallStackDepth{maxDepth}` is accepted **without** `Debugger.enable` (returns `{}`), and I observed `stackTrace.parentId` populated on `consoleAPICalled` in that mode. With `Debugger.enable` active, the parent frame is **inlined** as `stackTrace.parent` (verified: `parent.description == "setTimeout"`). So:

- **Without `Debugger.enable`:** you get `parentId` and must call `Debugger.getStackTrace{stackTraceId}` to resolve — an extra round-trip per event. (`consoleAPICalled` docs state the async chain is *automatically* reported only for `assert`, `error`, `trace`, `warning`.)
- **With `Debugger.enable`:** parents are inlined, no round-trip, but you pay a real V8 deopt cost and risk pausing on breakpoints.

**Recommendation:** default `Debugger.setAsyncCallStackDepth{maxDepth: 32}` *without* `Debugger.enable`; resolve `parentId` lazily and only when the agent asks for a deep trace. Set depth to 0 for long-running `--record-video` jobs — async stack capture is a per-await allocation.

**Source maps:** CDP gives you `Debugger.scriptParsed{sourceMapURL, url, scriptId, hash}` (requires `Debugger.enable`). Resolve `file:line:col` → original via the `sourcemap` crate (**9.3.2**, 2026-01-20). Fetch the map with `Network.loadNetworkResource{frameId, url, options}` (EXPERIMENTAL) so it goes through the page's network stack, cookies and all, rather than a separate Rust HTTP client — this matters for authenticated `.map` files behind a login. Cache maps keyed by `(url, hash)`. **Unverified** end-to-end; the pieces are individually confirmed.

---

## 2. Serialising console args without token blowup

`Runtime.consoleAPICalled.args` is `RemoteObject[]`. **Verified: Chrome populates `preview` automatically for console args even though `generatePreview` is a parameter of `evaluate`/`callFunctionOn`, not of the event.** Raw observations:

```jsonc
// console.log(obj) where obj has a circular self-reference
{"type":"object","className":"Object","description":"Object","objectId":"…2.1",
 "preview":{"type":"object","description":"Object","overflow":false,"properties":[
   {"name":"a","type":"number","value":"1"}, {"name":"b","type":"string","value":"two"},
   {"name":"c","type":"object","value":"Array(3)","subtype":"array"},
   {"name":"d","type":"object","value":"Object"},
   {"name":"self","type":"object","value":"Object"}]}}     // circular → flat string, no recursion

// console.log(bigObj) with 200 keys
{"description":"Object","preview":{"overflow":true,"properties":[k0,k1,k2,k3,k4]}}   // capped at 5

// console.log(document.getElementById('a'))
{"type":"object","subtype":"node","className":"HTMLDivElement","description":"div#a",
 "preview":{"subtype":"node","description":"div#a","overflow":true,
            "properties":[align,title,lang,translate,dir]}}   // ← useless IDL attrs, not the markup
```

**Confirmed facts to design against:**

1. **`preview.properties` is capped at 5 entries** with `overflow:true`. Free, bounded, always present. This is your default rendering tier.
2. **Circular references are already safe** — the cycle renders as the string `"Object"`. No cycle detection needed at tier 1.
3. **DOM nodes preview terribly.** `subtype:"node"` gives you IDL attributes (`align`, `title`, `lang`…), never the markup. **Special-case it:** on `subtype == "node"`, call `DOM.requestNode{objectId}` → `nodeId`, then `DOM.getOuterHTML{nodeId}` (truncated) or bind it to a `@node-NN` ref from the Unified Page Tree. This is the single highest-value console improvement for an agent.
4. `console.table` args arrive as an array whose preview contains **nested `valuePreview` one level deep** — enough to render the table without extra calls.

### Tiered rendering ladder

| Tier | Cost | Mechanism | When |
|---|---|---|---|
| 0 | free | `description` + primitive `value` | primitives, always |
| 1 | free | `preview` (≤5 props, `overflow` flag) | default for objects |
| 2 | 1 RTT | `Runtime.callFunctionOn{objectId, functionDeclaration, returnByValue:true}` running a **depth/breadth/byte-capped custom serialiser** | agent asks to expand |
| 3 | 1 RTT | `DOM.requestNode` + `DOM.getOuterHTML` | `subtype == "node"` |
| 4 | 1 RTT | `serializationOptions:{serialization:"deep", maxDepth:N}` (EXPERIMENTAL) | rare; deep structural dump |

**Never use bare `returnByValue: true`** on an unknown object — it is `JSON.stringify` semantics with no depth cap and **throws on circular references**. Always ship your own serialiser as the function body:

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
    for (const k of Object.keys(v)) { if (n++ >= maxProps) { out.__more = Object.keys(v).length - maxProps; break; }
                                      out[k] = walk(v[k], d+1); }
    return out;
  };
  return walk(this, 0);
}"#;
// callFunctionOn { objectId, functionDeclaration: EXPANDER,
//                  arguments:[{value:4},{value:30},{value:512},{value:16384}],
//                  returnByValue:true, objectGroup:"brow-console" }
```

**Lifetime discipline:** every `objectId` in a console arg pins a JS object in the renderer heap. On a 30-minute job that is a guaranteed OOM. Two mitigations, both required:
- Pass `objectGroup: "brow-console-<generation>"` on all expansions and call `Runtime.releaseObjectGroup` on every navigation.
- Treat `objectId`s from `consoleAPICalled` as **valid only until the next `Runtime.executionContextsCleared`** (observed 2× during my single navigation). After that, expansion fails and the agent gets the tier-1 preview permanently. Record this in the event: `expandable: bool`.

---

## 3. Network: the event sequence and why ExtraInfo matters

### Observed sequence (CONFIRMED)

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

### Why the ExtraInfo events are non-optional

`requestWillBeSent.request.headers` is the **renderer's view before the network stack runs**. It is missing `Cookie`, and misses anything added by extensions or the network service. `responseReceived.response.headers` is also a partially-processed view. The ExtraInfo events carry the wire truth. My observations:

- `requestWillBeSentExtraInfo.headers` had 14 keys including the full `sec-ch-ua*` client-hints set — none of which appear in the renderer view.
- `responseReceivedExtraInfo.headers` carried `Set-Cookie: session=SUPERSECRETVALUE; Path=/; HttpOnly` and `headersText` (the raw status line + headers block). **`headersText` is the only way to compute HAR `headersSize` correctly.**
- Cookie *blocking* information (`blockedCookies`, `exemptedCookies`, `associatedCookies[].blockedReasons`) exists **only** in ExtraInfo. This is how you tell an agent "your login failed because the cookie was blocked as SameSite=Lax cross-site" — which is exactly the class of bug the harness exists to diagnose.

**Ordering hazard (LIKELY, standard CDP folklore, and consistent with the `hasExtraInfo` flag existing at all):** ExtraInfo events are **not ordered** relative to their partners — `responseReceivedExtraInfo` can arrive before `responseReceived`. `requestWillBeSent.redirectHasExtraInfo` and `responseReceived.hasExtraInfo` exist precisely so a consumer knows whether to *wait*. Implement a per-`requestId` state machine that buffers whichever half arrives first and emits the normalised record only when `hasExtraInfo == false` or both halves are present. **Do not** assume ordering.

### Redirect chains

A redirect does **not** produce `responseReceived`. Instead a second `requestWillBeSent` arrives with the **same `requestId`** and a populated `redirectResponse`. Chain reconstruction: append `redirectResponse` to a `hops[]` vector on the existing record and overwrite the current request. `redirectHasExtraInfo` tells you whether an ExtraInfo pair also belongs to that hop.

### Initiator (the causality goldmine)

```
Initiator { type: parser|script|preload|SignedExchange|preflight|FedCM|other,
            stack: Runtime.StackTrace, url, lineNumber, columnNumber, requestId }
```
`type: "script"` with a populated `stack` is the **only ground-truth causal link** in the whole event system: it tells you exactly which JS frame issued the fetch. With `setAsyncCallStackDepth` set, that stack can chain back through `await`/`setTimeout` to the click handler. Everything else in §11 is inference.

### Timing

Observed `ResourceTiming` (note three fields absent from the public docs page):
```json
{"requestTime":88738.065514,"proxyStart":-1,"proxyEnd":-1,"dnsStart":-1,"dnsEnd":-1,
 "connectStart":-1,"connectEnd":-1,"sslStart":-1,"sslEnd":-1,
 "workerStart":-1,"workerReady":-1,"workerFetchStart":-1,"workerRespondWithSettled":-1,
 "sendStart":0.215,"sendEnd":0.278,"pushStart":0,"pushEnd":0,
 "receiveHeadersStart":1.323,"receiveHeadersEnd":1.344}
```
`requestTime` is a **monotonic** base in seconds; all other fields are **milliseconds relative to it**; `-1` means "not applicable" (here: connection reused, so no DNS/connect/SSL). `response.responseTime` is a **wall-clock ms-since-epoch** float (`1785863279966.483`) — this is the bridge between monotonic and wall time.

### Provenance flags

All CONFIRMED present on `Network.Response`: `fromDiskCache`, `fromServiceWorker`, `fromPrefetchCache`, `connectionReused`, `connectionId`, `remoteIPAddress`, `remotePort`, `protocol` (`"http/1.1"`), `alternateProtocolUsage`, `securityState`, `encodedDataLength`, `mimeType`, `charset`. Plus `Network.requestServedFromCache{requestId}` as a separate event for memory-cache hits. Service-worker detail lives in `serviceWorkerResponseSource` and `serviceWorkerRouterInfo` (used by DevTools' HAR writer).

### Failures

```json
{"requestId":"8611.11","type":"Script","errorText":"","canceled":false,"blockedReason":"csp"}
{"requestId":"8611.7","type":"Fetch","errorText":"net::ERR_UNSAFE_PORT","canceled":false}
{"requestId":"8611.12","type":"EventSource","errorText":"net::ERR_ABORTED","canceled":true}
```
**Note the CSP case has an empty `errorText`** — you must render `blockedReason` when `errorText` is empty or the agent sees a blank error. `corsErrorStatus{corsError, failedParameter}` appears for CORS rejections and is the only place the precise CORS reason lives.

---

## 4. Response bodies: the genuinely hard part

### Failure mode 1: eviction on navigation (CONFIRMED, and worse than expected)

```
before nav:                          {"body":"{\"ok\": true, \"token\": \"eyJ…\"", …}  ✅
after nav to about:blank:            {"error":{"code":-32000,
                                       "message":"No resource with given identifier found"}}  ❌
after cross-origin nav:              same error                                              ❌
```

**Even a same-process navigation to `about:blank` destroys every buffered body.** The brief's phrasing "body no longer available" understates this: it is not a rare race, it is the guaranteed outcome for any request from a previous page. Retroactive body fetching is not a viable strategy for an SPA crawler.

Second failure mode, on in-flight streams: `{"error":{"message":"No data found for resource with given identifier"}}` — observed on the still-open SSE connection. Distinct message, distinct meaning; surface both distinctly.

### The buffer knobs

`Network.enable{maxTotalBufferSize*, maxResourceBufferSize*, maxPostDataSize, reportDirectSocketTraffic*, enableDurableMessages*}`. With `maxTotalBufferSize: 50 MB, maxResourceBufferSize: 20 MB` I successfully retrieved a **3,145,738-byte** body. Defaults are much smaller; set these explicitly. There is **no** `Network.setDataSizeLimitsForTest` in Chrome 151 — the brief's guess is wrong; the command list is: `setAcceptedEncodings, clearAcceptedEncodingsOverride, canClearBrowserCache, canClearBrowserCookies, canEmulateNetworkConditions, clearBrowserCache, clearBrowserCookies, continueInterceptedRequest, deleteCookies, disable, emulateNetworkConditions, emulateNetworkConditionsByRule, overrideNetworkState, enable, configureDurableMessages, getAllCookies, getCertificate, getCookies, getResponseBody, getRequestPostData, getResponseBodyForInterception, takeResponseBodyForInterceptionAsStream, replayXHR, searchInResponseBody, setBlockedURLs, setBypassServiceWorker, setCacheDisabled, setCookie, setCookies, setExtraHTTPHeaders, setAttachDebugStack, setRequestInterception, setUserAgentOverride, streamResourceContent, getSecurityIsolationStatus, enableReportingApi, enableDeviceBoundSessions, deleteDeviceBoundSession, fetchSchemefulSite, loadNetworkResource, setCookieControls`.

`Network.configureDurableMessages{maxTotalBufferSize, maxResourceBufferSize}` (EXPERIMENTAL) stores bodies **outside the renderer** so they survive cross-process navigation. Its own docstring says the `Network.enable{enableDurableMessages}` form "is being deprecated in favor of the dedicated `configureDurableMessages` command, due to the possibility of deadlocks when awaiting `Network.enable` before issuing `Runtime.runIfWaitingForDebugger`." **Use the dedicated command.** Accepted by Chrome 151 (returns `{}`); I did **not** verify it actually rescues bodies across navigation (my test enabled it after the fact).

### The answer: `Network.streamResourceContent` (CONFIRMED WORKING)

```
Fetch.requestPaused networkId=17944.14
Network.streamResourceContent{requestId:"17944.14"} -> bufferedData(b64) len=0
→ dataReceived=3, withInlineData=2, totalInlineB64Bytes=4,194,320   (≈3 MB decoded ✅)
```

Subscribing to a request **before its body completes** switches `Network.dataReceived` into carrying the chunk payload inline in its experimental `data` field (base64). This gives you streamed bodies with **no load serialisation** — the request proceeds at full speed. It returns already-buffered bytes in `bufferedData` and streams the rest.

Earlier I called `streamResourceContent` *after* `loadingFinished` and got `bufferedData` of length 0 with **zero** subsequent inline data. **Timing is everything: you must subscribe from the `requestWillBeSent` handler.**

### Recommended body strategy

```
on requestWillBeSent(req):
    if policy.should_capture_body(req):          # content-type / URL / size heuristics
        send Network.streamResourceContent{requestId}   # fire-and-forget, do not await
        # accumulate dataReceived.data chunks into a spill-to-disk buffer
on loadingFinished:
    finalise; if no streamed bytes → fallback Network.getResponseBody (best effort, may 404)
```

Use the `Fetch` domain **only** where you must *modify* traffic (mocking, auth injection, `failRequest`) or where a body is business-critical and streaming failed. `Fetch.enable{patterns:[{urlPattern:"*", requestStage:"Response"}]}` pauses **every** response until you call `continueRequest` — every network round-trip now includes a full CDP round-trip through the daemon. It also perturbs the timings you are trying to measure, which makes it self-defeating for a performance harness. `Fetch.takeResponseBodyAsStream` (EXPERIMENTAL) exists for large bodies at the Fetch layer.

**Encoding note:** `getResponseBody` returned `base64Encoded: false` for the JSON/HTML bodies and `base64Encoded: true` for a 9-byte `not found` body served **without a `Content-Type`**. The flag tracks Chrome's MIME sniffing, not the actual bytes. Always branch on the flag; never assume UTF-8.

---

## 5. WebSockets, SSE, long-poll

### WebSockets (all CONFIRMED against a live echo server)

Event order: `webSocketCreated{requestId,url,initiator}` → `webSocketWillSendHandshakeRequest{requestId,timestamp,wallTime,request}` → `webSocketHandshakeResponseReceived{requestId,timestamp,response}` → `webSocketFrameSent`/`webSocketFrameReceived{requestId,timestamp,response}` → `webSocketFrameError{requestId,timestamp,errorMessage}` → `webSocketClosed{requestId,timestamp}`.

`webSocketHandshakeResponseReceived.response` is rich: `status, statusText, headers, headersText, requestHeaders, requestHeadersText` — including `Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits`.

**Frame payloads (the important detail):**
```json
{"opcode":1,"mask":true, "payloadData":"hello-text"}          // text  → raw UTF-8
{"opcode":2,"mask":true, "payloadData":"AQID+g=="}            // binary→ base64 of [1,2,3,250]
{"opcode":2,"mask":false,"payloadData":"AAEC//4="}            // binary→ base64 of [0,1,2,255,254]
```
**There is no explicit `isBinary` flag. `opcode` is the only discriminator: 1 = text (raw), 2 = binary (base64).** Opcode 8 = close, 9/10 = ping/pong. `mask: true` on client→server frames per RFC 6455. Getting this wrong silently corrupts every binary frame.

**Caveat:** with `permessage-deflate` negotiated, `payloadData` is the **decompressed** payload — good for readability, but it means you cannot reconstruct exact wire bytes or compute true bandwidth from frames. **Likely**, from the fact that DevTools shows readable frames on deflate-enabled sockets; not separately verified.

**Not covered:** `Network.webSocketClosed` did not fire in my run because I never closed the socket — so close codes/reasons are **unverified**, and note the event carries only `requestId` and `timestamp`, **no close code and no reason string**. That is a real gap: you cannot tell the agent *why* a socket closed from CDP alone.

### SSE (CONFIRMED)

`Network.eventSourceMessageReceived{requestId, timestamp, eventName, eventId, data}`:
```json
{"requestId":"8611.12","timestamp":88738.070389,"eventName":"tick","eventId":"0","data":"{\"n\":0}"}
{"requestId":"8611.12","timestamp":88738.070461,"eventName":"message","eventId":"0","data":"plain-0"}
```
Named events keep their name; unnamed events get `eventName: "message"`. `eventId` reflects the SSE `id:` field. This is fully parsed for you — no manual `text/event-stream` framing needed. The underlying request stays in-flight, so `getResponseBody` on it fails (`"No data found…"`), and closing it produces `loadingFailed{type:"EventSource", errorText:"net::ERR_ABORTED", canceled:true}` — **classify `EventSource` + `canceled:true` as a normal termination, not an error**, or every SSE page reports a spurious failure.

### Long-poll

CDP has no long-poll concept. Detect heuristically in `crates/events`: same URL + `type: Fetch|XHR`, repeated ≥3×, each with `wait` (TTFB) > 1 s, gaps < 2 s. Emit a synthetic `stream.longpoll` event that collapses N requests into one row so the agent's `network requests` output isn't drowned. Mark provenance `inferred`.

---

## 6. HAR 1.2: exact mapping and what is not derivable

Chrome DevTools' own writer (`front_end/models/har/Log.ts`, `Writer.ts`) is the compatibility target. I read the source.

### Derivable from CDP

| HAR field | CDP source |
|---|---|
| `log.version` / `creator` | literal `"1.2"` / `{name:"brow", version}` |
| `pages[].startedDateTime` | `pseudoWallTime(page.startTime)` |
| `pages[].pageTimings.onContentLoad` | `Page.domContentEventFired.timestamp` − navStart, ms |
| `pages[].pageTimings.onLoad` | `Page.loadEventFired.timestamp` − navStart, ms |
| `entries[].startedDateTime` | `pseudoWallTime(issueTime)` — see below |
| `entries[].time` | `Σ max(t,0)` over `blocked,dns,connect,send,wait,receive` (**ssl excluded — it is inside connect**) |
| `request.method/url/httpVersion` | `request.method`, `request.url`, `response.protocol` |
| `request.headers` | **`requestWillBeSentExtraInfo.headers`** (not the renderer view) |
| `request.cookies` | parsed from `associatedCookies` |
| `request.queryString` | parsed from `request.url` |
| `request.postData` | `request.postData` / `Network.getRequestPostData` |
| `request.headersSize` | `requestHeadersText.length`, else `-1` |
| `response.status/statusText` | `response.status`, `response.statusText` |
| `response.headers` | **`responseReceivedExtraInfo.headers`** |
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

### DevTools' `buildTimings()` — reproduce this exactly

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

### Chrome extension fields to emit (all seen in `Log.ts`)

`_initiator` (type/url/requestId/lineNumber/**stack**), `_priority`, `_resourceType`, `_fromCache` (`"memory"|"disk"`, deleted if absent), `_connectionId` (deleted if `"0"`), `_transferSize`, `_error`, `_blocked_queueing`, `_blocked_proxy`, `_workerStart`, `_workerReady`, `_workerFetchStart`, `_workerRespondWithSettled`, `_workerRouterEvaluationStart`, `_workerCacheLookupStart`, `_fetchedViaServiceWorker`, `_responseCacheStorageCacheName`, `_serviceWorkerResponseSource`, `_serviceWorkerRouterRuleIdMatched`, `_serviceWorkerRouterMatchedSourceType`, `_serviceWorkerRouterActualSourceType`, `_webSocketMessages`, `_eventSourceMessages`.

### NOT derivable from CDP — be honest in the output

| HAR field | Reality |
|---|---|
| `entries[].cache` (`beforeRequest`/`afterRequest`) | **Not available.** DevTools emits `cache: {}`. Emit `{}` and put provenance in `_fromCache`. |
| `timings.blocked` as *true* stalled time | It is a synthesis of queueing + proxy + pre-connect. Not a measured value. |
| `browser` | Fill from `/json/version` `Browser` + `User-Agent`. |
| `response.content.text` for evicted/streamed-past bodies | Genuinely absent. Emit `_bodyOmitted: "evicted"|"too-large"|"redacted"|"not-captured"`. |
| Exact request `bodySize` for multipart uploads | `maxPostDataSize` truncates; record `_postDataTruncated: true`. |

**Wall-clock:** `startedDateTime` needs `pseudoWallTime(monotonic)`. Compute the offset once per session: `wall_offset = response.responseTime/1000.0 − response_monotonic_ts`, then `wall(t) = t + wall_offset`. `requestWillBeSent.wallTime` gives you a direct anchor too. Do **not** use the daemon's own clock — CDP timestamps come from the browser process.

**Rust:** the `har` crate (**0.9.0**, 2026-03-22, `github.com/mandrean/har-rs`) provides HAR 1.2 serde types. It will not have Chrome's `_` fields, so define our own `serde` structs in `crates/artifacts/src/har.rs` with `#[serde(flatten)] extensions: BTreeMap<String, Value>`, and use `har` only as a schema cross-check in tests.

---

## 7. Redaction

**Redact at ingest, before the event enters the ring buffer.** The argument is simple: after ingest an event is copied to the in-memory ring, the on-disk segment, the HAR export, the video action log, and the agent's stdout. Redacting at persistence leaves the secret live in the ring buffer, in daemon core dumps, and in `--follow` output. Ingest is the only single choke point. Cost is one pass over headers/bodies on the daemon's event thread — trivial next to the JSON parse you already did.

### Layered rules

**1. Header deny-list (exact, case-insensitive) → replaced with `<redacted:header>`**
`authorization, proxy-authorization, cookie, set-cookie, x-api-key, api-key, x-auth-token, auth-token, x-csrf-token, x-xsrf-token, x-session-token, x-access-token, x-refresh-token, authentication, www-authenticate, proxy-authenticate, x-amz-security-token, x-goog-api-key, x-firebase-appcheck, dpop, x-hub-signature, x-hub-signature-256, x-signature, x-shopify-access-token, private-token, x-vault-token`

Note DevTools' own `sanitize` option only strips `set-cookie`, `authorization`, `cookie` — far too narrow. My probe proved the point: `X-Api-Key: sk_live_1234567890abcdef` was fully visible in both `responseReceived.response.headers` and `responseReceivedExtraInfo.headers`.

**2. Header prefix deny-list:** `x-*-token`, `x-*-secret`, `x-*-key`, `x-*-signature` (glob).

**3. Cookies:** strip `associatedCookies[].cookie.value`, `blockedCookies`, `exemptedCookies`, and any `Set-Cookie` value; **keep name, domain, path, expires, sameSite, httpOnly, secure, and all `blockedReasons`** — the diagnostic value is entirely in the metadata, never the value.

**4. Value regexes** (over header values, URL query values, JSON string leaves, form fields, `postData`):

| Pattern | Regex sketch |
|---|---|
| JWT | `\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b` |
| Bearer | `(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}` |
| Basic | `(?i)\bbasic\s+[A-Za-z0-9+/=]{16,}` |
| Stripe-ish | `\b(sk|pk|rk)_(live|test)_[A-Za-z0-9]{16,}\b` |
| GitHub | `\b(gh[pousr]|github_pat)_[A-Za-z0-9_]{20,}\b` |
| AWS | `\bAKIA[0-9A-Z]{16}\b`, `\bASIA[0-9A-Z]{16}\b` |
| Google | `\bAIza[0-9A-Za-z_-]{35}\b` |
| Slack | `\bxox[baprs]-[0-9A-Za-z-]{10,}\b` |
| OpenAI/Anthropic | `\bsk-(ant-)?[A-Za-z0-9_-]{20,}\b` |
| PEM | `-----BEGIN [A-Z ]*PRIVATE KEY-----` |
| PAN (Luhn-checked) | `\b(?:\d[ -]?){13,19}\b` |

Compile once into a single `regex::RegexSet` (**regex 1.13.1**) so all patterns are matched in one pass; `aho-corasick` **1.1.5** for the literal prefix set. Do **not** run these per-pattern in a loop.

**5. High-entropy fallback.** For a token-shaped string (len ≥ 24, charset ⊆ base64url/hex, no whitespace), compute Shannon entropy over the character distribution; redact if `> 3.5 bits/char` for base64-ish or `> 3.0` for hex. This catches bespoke session IDs no regex knows. **Redact only the value, and always record `len` and a salted `blake3` prefix** so the agent can still say "the token changed between request A and B" — which is often the whole bug.

**6. URL query params** by name: `token, access_token, id_token, refresh_token, code, state, api_key, apikey, key, secret, password, passwd, pwd, sig, signature, auth, session, sid, jwt, otp, code_verifier`. Rewrite the value to `<redacted>` but **keep the parameter present** — its absence changes route-template inference in the site graph.

**7. Form fields** (from `postData`, `Input` events, and the Page Tree): redact by `input[type=password]`, by `autocomplete` token (`current-password, new-password, cc-number, cc-csc, one-time-code`), and by name/id regex `(?i)(pass|pwd|secret|token|otp|cvv|cvc|ssn|card)`.

**8. `console.*` args and `mutate.evaluate` code** go through the same value-regex pass. A `console.log(authToken)` is the most common leak of all.

### The over-redaction failure mode

Over-redaction destroys debuggability — an agent debugging an auth flow that sees `<redacted>` everywhere cannot work. Mitigations, all required:

- **Never delete; always substitute a structured stub:** `{"__redacted": {"rule":"header-denylist","len":36,"sha":"b3:9f2a1c…","classes":["jwt"]}}`. Length + stable hash preserve *comparison* without disclosure.
- **Policy-gated reveal:** `browserctl network requests --reveal <field-path> --request <id>` requires the `storage` capability (per the mode ladder) and, in a background job, **parks the job in `waiting_for_approval`** exactly like the other dangerous actions. Every reveal is written to the job's audit log with actor, timestamp, and field path.
- **Reveal reads from the original.** This means the daemon must keep the pre-redaction bytes *somewhere*. Resolution: keep them **only** in a bounded, memory-only, encrypted-at-rest-if-spilled `reveal cache` (default 64 MB, 10-minute TTL, never in the on-disk segment, never in the HAR). If the cache has expired, reveal fails honestly. This is the one place where "redact at ingest" is softened, and it must be an explicit, documented, capability-gated exception.
- **Per-origin allow-list** in policy for known-safe dev environments (`localhost`, `*.test`), so local debugging is not crippled.

---

## 8. DOM mutation streaming: the measurement that settles it

I ran the identical workload (50 `createElement`+`setAttribute`+`appendChild` every 50 ms, clearing at 400 children) under both strategies for ~3 seconds.

### Strategy A — CDP `DOM.*` events (`DOM.enable` + `DOM.getDocument{depth:-1, pierce:true}`)

```
DOM.getDocument materialised tree ≈ 185,210 bytes of JSON (a trivial page!)
Events in 3 s: { childNodeCountUpdated: 5651,
                 scrollableFlagUpdated: 14,
                 childNodeRemoved: 2,
                 childNodeInserted: 1 }
Total wire bytes: 775,226
```

**Read that again: 5,651 `childNodeCountUpdated` and exactly *one* `childNodeInserted`.** This is the crucial fidelity finding. CDP only sends `childNodeInserted` for nodes whose parent has already been *pushed to the client*. For every other subtree you get `childNodeCountUpdated{nodeId, childNodeCount}` — a bare integer. You learn that *something* changed under a node and nothing about what. To recover content you must call `DOM.requestChildNodes` per node, which triggers more events, which grows the materialised set, which grows the event rate. It is a positive feedback loop.

On top of that, `DOM.enable` obliges the daemon to maintain a full mirrored node set for the lifetime of the page and to handle `DOM.documentUpdated` (total invalidation) correctly.

### Strategy B — isolated-world `MutationObserver`, aggregated

```
{"batches":63, "muts":2976, "added":2970, "removed":2708, "attrs":0, "chars":0}
Wire bytes for 6 aggregate messages at 500 ms cadence: 438 bytes total
```

Full fidelity (2,976 actual `MutationRecord`s, correctly attributing 2,970 insertions and 2,708 removals), delivered in **438 bytes** — a **~1,770× reduction** versus 775 KB, while telling you strictly more.

### Verdict and design

**Use `MutationObserver` in an isolated world as the mutation source. Use `DOM.enable` only transiently, when the Page Tree is actually being built.**

Injection (all CONFIRMED working):
```
Runtime.addBinding{ name:"__browRelay", executionContextName:"brow_iso" }
Page.addScriptToEvaluateOnNewDocument{ source: OBSERVER_JS,
                                       worldName:"brow_iso", runImmediately:true }
   → {"identifier":"1"}
→ Runtime.bindingCalled{ name, payload, executionContextId }
```
`worldName` + `executionContextName` keep the observer invisible to page JS (no `window` pollution, survives `Object.freeze` games) and satisfy the "isolated world" requirement. `runImmediately: true` applies it to the *already-loaded* document, not just future ones.

**Caveat I hit and you will too:** state set in the isolated world is **not** visible to `Runtime.evaluate` in the main world (my first attempt returned `{}`). All reads must go through the binding or through an `executionContextId`-targeted evaluate.

### What `mutations --follow` actually shows an agent

Never raw records. The observer ships one aggregate per animation frame (coalesced to ≥100 ms), and the daemon further coalesces to the CLI's cadence:

```
14:22:31.410  +42 −0   attrs:3   under @node-42 (main > ul.feed)      [class,aria-busy]
14:22:31.610  +0  −40  attrs:1   under @node-42                       [aria-busy]
14:22:32.100  TEXT     @node-77  "Loading…" → "12 results"
14:22:33.240  SUBTREE-REPLACED  @node-99 (div.modal-root)  4131 nodes  ⟵ action a7f3 (click @node-12)
```

Rules: (1) aggregate by nearest **stable ancestor** that the Page Tree already has a `@node-NN` ref for; (2) collapse >N sibling insertions into `+N`; (3) always render **attribute name lists** and **text before→after** in full, since these are small and carry the semantic signal; (4) emit a distinct `SUBTREE-REPLACED` when removed+added under one parent exceed a threshold — that's the SPA route-change signature; (5) hard rate-limit to ~10 lines/s with a `… 4,120 more mutations suppressed` footer; (6) attribute to an `actionId` when inside that action's causal window (§11).

Record the observer options actually used (`childList, subtree, attributes, characterData`; `attributeOldValue`/`characterDataOldValue` are opt-in and roughly double the record size — default them **off**, enable via `--verbose`).

---

## 9. Performance, vitals, memory, coverage, tracing

### `Performance.getMetrics` — the actual metric names (CONFIRMED, verbatim from a live page)

`Timestamp, AudioHandlers, AudioWorkletProcessors, Documents, Frames, JSEventListeners, LayoutObjects, MediaKeySessions, MediaKeys, Nodes, Resources, ContextLifecycleStateObservers, V8PerContextDatas, WorkerGlobalScopes, UACSSResources, RTCPeerConnections, ResourceFetchers, AdSubframes, DetachedScriptStates, ArrayBufferContents, LayoutCount, RecalcStyleCount, LayoutDuration, RecalcStyleDuration, DevToolsCommandDuration, ScriptDuration, V8CompileDuration, TaskDuration, TaskOtherDuration, ThreadTime, ProcessTime, JSHeapUsedSize, JSHeapTotalSize, FirstMeaningfulPaint, DomContentLoaded, NavigationStart`

36 metrics. Durations are **cumulative seconds**; you must diff two samples to get an interval. `Performance.enable{timeDomain}` accepts `"timeTicks"` (monotonic, default) or `"threadTicks"`; `Performance.setTimeDomain` exists but **must be called before `enable`**. There is also a `Performance.metrics` event for push delivery.

These are **counters, not Web Vitals**. `FirstMeaningfulPaint` is a deprecated heuristic; do not present it as FCP.

### `PerformanceTimeline` — the brief's assumption is wrong (CONFIRMED)

I probed `PerformanceTimeline.enable{eventTypes:[…]}` one entry type at a time against Chrome 151:

```
ACCEPTED: ['largest-contentful-paint', 'layout-shift']
REJECTED: longtask, long-animation-frame, first-input, event, paint, navigation, resource,
          mark, measure, element, visibility-state, back-forward-cache-restoration,
          soft-navigation, interaction, taskattribution, memory
          → error -32602 "Unknown or unsupported entry type"
```

The domain's own `TimelineEvent` type only carries `lcpDetails` and `layoutShiftDetails` — the protocol structurally supports nothing else. **`PerformanceTimeline` is NOT a route to Web Vitals.** It gives you exactly two: LCP (with `renderTime`, `loadTime`, `size`, `elementId`, `url`, and — valuably — a `DOM.BackendNodeId` you can bind to a `@node-NN` ref) and CLS (`value`, `hadRecentInput`, `lastInputTime`, `sources[]` each with `previousRect`/`currentRect`/`nodeId`).

Verified working: one `timelineEventAdded` arrived —
```json
{"type":"largest-contentful-paint","name":"","time":1785863626.4765,
 "lcpDetails":{"renderTime":1785863626.4765,"loadTime":0,"size":234,"elementId":"a","nodeId":3}}
```

### The answer: injected `PerformanceObserver` (CONFIRMED)

Same isolated-world injection as the mutation observer. Chrome 151 reports:

```
PerformanceObserver.supportedEntryTypes = [
  element, event, first-input, interaction-contentful-paint, largest-contentful-paint,
  layout-shift, long-animation-frame, longtask, mark, measure, navigation, paint,
  resource, soft-navigation, visibility-state ]
```
(`interaction-contentful-paint` is new; not in older docs.)

I received live `navigation`, `paint` (`first-paint`, `first-contentful-paint`), `largest-contentful-paint`, `long-animation-frame`, `resource`, and `visibility-state` entries via `Runtime.bindingCalled`. Sample:
```json
{"entryType":"paint","name":"first-contentful-paint","startTime":36,"duration":0}
{"entryType":"largest-contentful-paint","name":"","startTime":36,"size":234,"id":"a","renderTime":36}
```

**Use `buffered: true`** so entries that fired before injection are replayed — essential because `paint`/`navigation` happen early. Compute INP yourself from `event` entries (`observe({type:'event', buffered:true, durationThreshold:16})`, take the high-percentile `interactionId` duration) and CLS from summed `layout-shift` values in session windows. **We must not vendor the `web-vitals` npm library** (external dep + its own reporting model); the ~120 lines of arithmetic are ours.

**Gotcha (CONFIRMED):** `observe({type})` with an **unsupported** type does **not throw** — my `back-forward-cache-restoration` observe "succeeded" and simply never fired, and zero `poFail` messages were relayed. **Always gate on `PerformanceObserver.supportedEntryTypes` before observing**, and report the supported set to the agent so it knows what it can't have.

### Memory / heap / coverage (all CONFIRMED callable)

- `Memory.getDOMCounters` → `{"documents":4,"nodes":37,"jsEventListeners":4}`. Cheap; poll for leak trend lines.
- `Memory.getDOMCountersForLeakDetection` → `-32000 "Failed to run leak detection"` in headless. **Do not rely on it.**
- `Runtime.getHeapUsage` → `{"usedSize":1077072,"totalSize":2097152,"embedderHeapUsedSize":4645216,"backingStorageSize":9646}` — cheapest heap signal, poll at 1 Hz.
- `Profiler.enable` + `Profiler.startPreciseCoverage{callCount:true, detailed:true}` → `{"timestamp":89089.626726}`; then `takePreciseCoverage` / `stopPreciseCoverage`. `detailed:true` gives block-level ranges (large); default it **off**.
- `HeapProfiler.takeHeapSnapshot{reportProgress}` streams via **`HeapProfiler.addHeapSnapshotChunk`** events, not an `IO` handle. A real-app snapshot is 100s of MB. **Never buffer in memory** — write chunks straight to `artifacts/<job>/heap-<ts>.heapsnapshot` and hand the agent a path plus a summary, never the content. Gate behind an explicit `--heap-snapshot` flag.

### Tracing — measured cost

Two hard facts:

**1. `Tracing.start` must go to the *browser* session.** Sending it on a page session **reset my CDP connection** (`ConnectionResetError`). On the browser session it returned `{}` immediately.

**2. The volume is brutal.** A ~3-second trace of a *trivial* page with a DevTools-ish category set:
```
tracingComplete: {"dataLossOccurred":false,"stream":"1","traceFormat":"json","streamCompression":"none"}
trace bytes: 10,016,478      trace events: 45,572
```
Category cost breakdown:
```
   5.27 MB  toplevel
   1.86 MB  toplevel,mojom
   1.83 MB  disabled-by-default-devtools.timeline
   0.57 MB  cc
   0.26 MB  loading
   0.10 MB  devtools.timeline
   0.03 MB  cc,benchmark,disabled-by-default-devtools.timeline.frame
```
Dropping `toplevel` + `mojom` removes ~71%. A **slim set** of `devtools.timeline` + `disabled-by-default-devtools.timeline.frame` + `blink.user_timing` + `loading` yields **2,105 events / 396 KB** for the same 3 s — about 130 KB/s, which is sustainable. `Tracing.getCategories` reported **283** available categories.

Use `transferMode:"ReturnAsStream", streamFormat:"json", streamCompression:"gzip"` and drain with `IO.read{handle,size}` → `IO.close`. Watch `Tracing.bufferUsage` (set `bufferUsageReportingInterval`) and **always check `tracingComplete.dataLossOccurred`** — report it to the agent rather than silently presenting a truncated trace.

### Parsing the trace: FPS and dropped frames (CONFIRMED event shapes)

The format is a JSON object `{traceEvents: [...], metadata: {...}}`. Observed phase distribution: `X` (complete, 37,375), `I` (instant, 5,999), `M` (metadata, 190), `R` (mark, 183), `b`/`e` (async nestable, 204/169), `s`/`f` (flow, 640/640), `n` (async instant, 88), `B` (begin, 84).

```json
// cat: "disabled-by-default-devtools.timeline.frame"
{"name":"DrawFrame","ph":"I","ts":89570731509,"pid":17468,"tid":1609536,
 "args":{"frameSeqId":40,"layerTreeId":2}}
{"name":"BeginFrame","ph":"I","ts":89570715036,"args":{"frameSeqId":40,"layerTreeId":2}}
// full set in that category: NeedsBeginFrameChanged, BeginFrame, RequestMainThreadFrame,
//                            BeginMainThreadFrame, ActivateLayerTree, DrawFrame

// cat: "cc,benchmark,disabled-by-default-devtools.timeline.frame", ph "b"/"e", id2.local
{"name":"PipelineReporter","ph":"b","ts":89570631706,"id2":{"local":"0x1"},
 "args":{"frame_reporter":{"state":"STATE_NO_UPDATE_DESIRED","frame_sequence":35,
    "affects_smoothness":false,"has_high_latency":false,"scroll_state":"SCROLL_NONE",
    "checkerboarded_needs_raster":false,"has_missing_content":false,"layer_tree_host_id":1,
    "surface_frame_trace_id":-1489707801736109738}}}

// cat: "disabled-by-default-devtools.timeline"
{"name":"RunTask","ph":"X","ts":89570705796,"dur":420,"tdur":16,"args":{}}
```

- **FPS** = count of `DrawFrame` instants per second (`ts` is µs).
- **Dropped frames** = `PipelineReporter` async slices where `args.frame_reporter.state == "STATE_DROPPED"`. Observed state distribution over 24 reporters: `STATE_NO_UPDATE_DESIRED: 8, STATE_PRESENTED_ALL: 3, STATE_DROPPED: 1, null: 12` (nulls are the `e`-phase closers). `affects_smoothness` distinguishes "dropped and the user saw it" from "dropped harmlessly".
- **Long tasks** = `RunTask` with `dur > 50000` (µs). In my idle-ish run: p50 = 4 µs, p95 = 62 µs, max = 18,179 µs, zero long tasks. Note: prefer the injected `longtask`/`long-animation-frame` PerformanceObserver for this — it is orders of magnitude cheaper than tracing.
- Also present and useful: `firstContentfulPaint`, `largestContentfulPaint::Candidate`, `navigationStart`, `ResourceSendRequest` (11), `ResourceReceiveResponse` (10), `EventDispatch` (115), `Layout`/`UpdateLayoutTree`.
- **`LayoutShift` did NOT appear in the trace** with these categories (zero matches for any name containing "shift"). Get CLS from `PerformanceTimeline` or the injected observer, **not** from tracing.

**Rust parser:** none suitable exists. crates.io has `chrome-trace-to-pprof` 0.1.3 (V8-CPU-profile-specific), `tracing-chrome` 0.7.2 (a *writer*, last updated 2024-03), and `perfetto` 0.0.0 (an empty placeholder). **Write our own** in `crates/events/src/trace/`: a `serde` struct over `{name, cat, ph, ts, dur, tdur, pid, tid, id2, s, args}` plus an async-slice matcher keyed by `(name, id2.local, pid, tid)`. For the 10 MB case, use `simd-json` **0.17.3** or `sonic-rs` **0.5.8** for the parse; stream from the `IO.read` chunks rather than materialising the whole string.

---

## 10. Unified event bus

### Normalised envelope

```rust
// crates/events/src/envelope.rs
pub struct Event {
    pub seq:        u64,              // daemon-assigned, total order, gap-free
    pub mono_us:    i64,              // CDP monotonic (seconds) → µs, the ONLY sort key
    pub wall_us:    i64,              // derived: mono_us + wall_offset_us
    pub session:    SessionId,        // page / worker / service_worker / iframe target
    pub frame:      Option<FrameId>,
    pub gen:        DocGeneration,    // matches the Page Tree @node-NN generation
    pub kind:       Kind,             // Console | Exception | LogEntry | Net* | Ws* | Sse
                                      // | Perf | Mutation | Trace | Action | Job
    pub cause:      Cause,            // see below
    pub payload:    Payload,
    pub redaction:  RedactionReport,  // rules fired, counts — never the secrets
}

pub enum Cause {
    Root,
    Action    { action_id: ActionId },                 // KNOWN: we performed it
    Initiator { action_id: ActionId, stack_hash: u64 },// KNOWN: CDP initiator stack
    Window    { action_id: ActionId, confidence: f32 },// INFERRED: time-window join
    Unknown,
}
```

### Timestamp normalisation

CDP hands you at least four clocks: `Runtime.Timestamp` (ms since epoch), `Network.MonotonicTime` (seconds, monotonic), `Network.TimeSinceEpoch` (seconds, wall), and trace `ts` (µs, monotonic, **different base**). Rules:

1. Convert everything to **µs**.
2. Sort **only** by monotonic. Wall time is display-only and can jump.
3. Establish `wall_offset_us` once per session from a `responseReceived` (`responseTime` wall ms + monotonic `timestamp`), refresh hourly.
4. **Trace `ts` has its own base** (observed `89570705796` µs vs network `88738.06` s). Align by correlating a `navigationStart` trace mark with `Page.frameNavigated`. Mark aligned trace events `clock: "trace-aligned"` with a residual error estimate. Do not pretend sub-millisecond accuracy across this boundary.
5. Events from different targets (page vs worker) share the browser's monotonic clock — safe to interleave. **Likely**, not verified across processes.

### Causality: what is known vs inferred — be blunt

| Link | Status | Mechanism |
|---|---|---|
| action → the DOM node it targeted | **KNOWN** | we issued the `Input.*` sequence at that ref |
| network request → issuing JS frame | **KNOWN** | `Initiator{type:"script", stack}` from CDP |
| network request → our action | **KNOWN, if** the initiator stack chains back to our synthetic event handler, with `setAsyncCallStackDepth > 0` |
| console/exception → action | **KNOWN, if** the stack shares a frame with the action's handler |
| log entry (network source) → request | **KNOWN** | `LogEntry.networkRequestId` |
| mutation → action | **INFERRED** | time window + subtree overlap |
| network request → route change | **INFERRED** | time window + `Page.frameNavigated`/History API hooks |
| console error → the request that caused it | **INFERRED** unless `networkRequestId` present | |
| trace slice → action | **INFERRED** | timestamp containment |

**The time-window heuristic:** after dispatching an action, open a window `[t_action, t_action + 2s]`, extended while the network is non-idle (any in-flight request) up to a 10 s cap. Events in the window with no stronger link get `Cause::Window{confidence}` where confidence decays with elapsed time and rises with subtree overlap. **The CLI must render inferred links differently** (`⟵ action a7f3` for known, `≈ action a7f3 (inferred)` for windowed). An agent that believes an inferred link is a fact will write a wrong bug report, and that is a worse outcome than saying "unknown".

Two known false-positive sources to document: (a) background polling/telemetry requests that fire in every window (suppress via the long-poll/periodicity detector); (b) `requestAnimationFrame` render loops that mutate continuously regardless of input.

---

## 11. Backpressure and retention

The brief's estimate is right: a 30-minute chatty-SPA job produces hundreds of MB. My own numbers scale it — the CDP DOM path alone was 775 KB / 3 s (≈ 465 MB / 30 min), and a full trace was 10 MB / 3 s (≈ 6 GB / 30 min). Neither is acceptable without control.

### Tiered retention

| Tier | Location | Retention | Contents |
|---|---|---|---|
| **Hot** | in-memory ring, per job, default 64 MB | evicts oldest | every event, full fidelity |
| **Warm** | on-disk segments, `artifacts/<job>/events/NNNNN.jsonl.zst` | job lifetime | every event except sampled-out classes |
| **Cold** | derived artifacts | forever | HAR, action log, vitals summary, site graph, screenshots |
| **Never** | — | — | raw trace unless `--trace`, heap snapshots unless asked |

Segments roll at 16 MB uncompressed or 60 s, whichever first. `zstd` **0.13.3** at level 3 (JSONL of similar events compresses ~10×). Each segment gets a sidecar index `{first_seq, last_seq, first_mono_us, last_mono_us, kind_counts, url_bloom}` so `--since` and `--filter` skip whole segments without decompressing.

### Class-specific policy

| Class | Policy |
|---|---|
| Console/exceptions | keep all; dedupe identical `(text, top stack frame)` into `count` + first/last ts |
| Network metadata | keep all — this is the cheapest, highest-value stream |
| Network bodies | capture per policy; >1 MB spills to `artifacts/bodies/<sha256>` and the event holds a pointer, hash, and size |
| WS frames | ring-buffer per socket (last N=1000 or 4 MB); count everything, retain a sample. A chat app will emit 100k frames |
| SSE | same as WS |
| Mutations | aggregate at source (rAF); never store raw records |
| Perf entries | keep all — low volume by construction |
| Trace | off by default; when on, slim categories; always to disk, never to the ring |

### Backpressure

The CDP transport must **never** block on a slow consumer. `tokio::sync::broadcast` per job with a bounded capacity; on `RecvError::Lagged(n)` the CLI prints `⚠ dropped n events (consumer too slow)` — **visible loss, never silent loss**. The disk writer gets its own unbounded-but-spilling channel. If the disk writer falls behind by more than one segment, degrade in this order: (1) drop trace, (2) drop mutation aggregates, (3) drop WS/SSE frame payloads keeping counts, (4) drop console arg previews keeping text, (5) finally, drop network metadata and set a `degraded: true` flag on the job that `status` reports prominently.

### Agent-facing query surface

```
browserctl network requests  --since 2m --url '*/api/*' --status '>=400' --type xhr,fetch \
                             --min-duration 500ms --initiator script --limit 20 --format table
browserctl network request <id> --headers --body --timing --initiator-stack --reveal <path>
browserctl console          --follow --level error,warning --source runtime,log --dedupe --since 30s
browserctl exceptions       --with-async-stack --source-mapped
browserctl ws frames <socket-id> --direction recv --since 1m --decode json --limit 50
browserctl perf vitals      # LCP/CLS/INP/FCP/TTFB + supported-entry-types caveat
browserctl perf longtasks   --min 100ms
browserctl mutations        --follow --scope @node-42 --min-batch 5
browserctl events           --since <ts> --kinds net,console --cause <actionId>  # unified stream
browserctl export har       --out run.har [--include-bodies] [--unredacted]  # last needs approval
```

Every command defaults to a **compact table** and a hard row cap, with `--format json` for machine reads. Every truncation prints an explicit `… N more (use --limit)`. Token discipline is a first-class requirement, not a nicety.

**Crates:** `tokio` **1.53.1**, `serde_json` **1.0.151** (+ `simd-json` **0.17.3** for trace), `bytes` **1.12.1**, `zstd` **0.13.3**, `regex` **1.13.1**, `aho-corasick` **1.1.5**, `memchr` **2.8.3**, `dashmap` **6.2.1** or `rustc-hash` **2.1.3** maps behind `parking_lot` **0.12.5**, `url` **2.5.8**, `sourcemap` **9.3.2**, `jiff` **0.2.35** (modern, over `chrono` 0.4.45) for HAR ISO-8601, `har` **0.9.0** for schema cross-checks in tests, `uuid` **1.24.0**, `base64` **0.23.1**.

---

## What we verified empirically

Local Chrome **151.0.7922.72** (V8 15.1.206.10, protocol 1.3, HeadlessChrome/151), launched `--headless=new` on scratch `--user-data-dir`s, driven by a from-scratch stdlib-only Python WebSocket CDP client (no Playwright/Puppeteer anywhere), against a purpose-built local HTTP/WS/SSE server. All instances killed afterwards.

| # | What we ran | Raw observation |
|---|---|---|
| 1 | `/json/protocol` dump | 51 domains; 1,605,774 bytes |
| 2 | Console vs Log routing | 36 `consoleAPICalled`, 6 `Log.entryAdded` (4 `network`, 1 `security`/CSP, all with `networkRequestId` except CSP), **0 overlap** |
| 3 | Exceptions | 3 `exceptionThrown`; unhandled rejection distinguished **only** by `text == "Uncaught (in promise)"` |
| 4 | RemoteObject previews | preview present **without** requesting it; capped at **5 props**, `overflow:true`; circular → `"Object"`; DOM node → IDL attrs, not markup |
| 5 | `Debugger.setAsyncCallStackDepth` | accepted without `Debugger.enable` → `parentId`; with `Debugger.enable` → inlined `parent{description:"setTimeout"}` |
| 6 | Workers | auto-attach yielded `worker` + `service_worker` sessions; structured events on worker session; page session saw only `Log[worker]` flat text + `workerId`; **`Runtime.enable` on `service_worker` timed out** |
| 7 | Network ExtraInfo | `X-Api-Key: sk_live_…` and `Set-Cookie: session=SUPERSECRETVALUE` fully visible → redaction is mandatory |
| 8 | ResourceTiming | 19 fields incl. undocumented `workerFetchStart`, `pushStart`, `pushEnd`; `-1` sentinels; `responseTime` = wall ms |
| 9 | `loadingFailed` | CSP case has **empty `errorText`** + `blockedReason:"csp"`; SSE close = `EventSource`/`ERR_ABORTED`/`canceled:true` |
| 10 | Body eviction | OK before nav; **`"No resource with given identifier found"` after nav to `about:blank`** and after cross-origin nav |
| 11 | In-flight body | SSE → `"No data found for resource with given identifier"` (distinct error) |
| 12 | 3 MB body | retrieved intact with `maxResourceBufferSize: 20 MB` |
| 13 | `streamResourceContent` (late) | `bufferedData` len 0, **0 of 19** `dataReceived` carried `data` |
| 14 | `streamResourceContent` (early, via Fetch pause) | **2 of 3** `dataReceived` carried inline `data`, **4,194,320 b64 bytes ≈ 3 MB** ✅ |
| 15 | WebSocket frames | opcode 1 → raw text; **opcode 2 → base64**; no `isBinary` flag; full handshake headers incl. `permessage-deflate` |
| 16 | SSE | `eventSourceMessageReceived` with `eventName`/`eventId`/`data`; unnamed → `"message"` |
| 17 | `PerformanceTimeline.enable` per type | **only `largest-contentful-paint` + `layout-shift` accepted**; 16 others → `-32602 "Unknown or unsupported entry type"` |
| 18 | `Performance.getMetrics` | 36 metrics, names listed verbatim in §9 |
| 19 | Injected `PerformanceObserver` | `supportedEntryTypes` = 15 types incl. `long-animation-frame`, `interaction-contentful-paint`; live FCP/LCP/navigation/resource/LoAF received via `bindingCalled` |
| 20 | Unsupported `observe({type})` | **silently no-ops, does not throw** |
| 21 | CDP DOM mutations | **5,651 `childNodeCountUpdated`, 1 `childNodeInserted`, 775,226 bytes / 3 s**; `getDocument` tree = 185,210 bytes |
| 22 | `MutationObserver` isolated world | same workload → **2,976 records in 63 batches, 438 bytes shipped** |
| 23 | Isolated-world isolation | main-world `Runtime.evaluate` **cannot see** isolated-world state (returned `{}`) |
| 24 | Tracing on page session | **reset the CDP connection**; browser session works |
| 25 | Trace volume | **10,016,478 bytes / 45,572 events / 3 s**; `toplevel` 5.27 MB; slim set **2,105 events / 396 KB** |
| 26 | Trace frame events | `DrawFrame`/`BeginFrame` `{frameSeqId,layerTreeId}`; `PipelineReporter` states `STATE_NO_UPDATE_DESIRED`(8)/`STATE_PRESENTED_ALL`(3)/**`STATE_DROPPED`(1)**; **no `LayoutShift` events at all** |
| 27 | `RunTask` durations | p50 4 µs, p95 62 µs, max 18,179 µs; 0 long tasks |
| 28 | Memory | `getDOMCounters` OK; **`getDOMCountersForLeakDetection` → `-32000 "Failed to run leak detection"`**; `Runtime.getHeapUsage` OK; `startPreciseCoverage` OK |
| 29 | `Tracing.getCategories` | 283 categories |
| 30 | DevTools HAR source | read `Log.ts` (571 lines) — exact `buildTimings`, extension fields, and its narrow `sanitize` (only `set-cookie`/`authorization`/`cookie`) |

---

## Limits and impossibilities

Stated bluntly, as requested.

1. **`PerformanceTimeline` cannot deliver Web Vitals.** Only LCP and CLS. FCP, TTFB, INP, longtask, LoAF, `mark`/`measure` all require injecting JS into the page. If the injection is blocked (a CSP with no `unsafe-eval` still permits isolated worlds, so this is rare — but `addScriptToEvaluateOnNewDocument` can be defeated by a page that runs before it in exotic cases), those metrics are simply unavailable. There is no CDP-only path.

2. **Retroactive body capture is impossible after navigation.** Any body not streamed at request time is gone — verified even for a same-process `about:blank`. A crawler that navigates and *then* decides it wants a body has already lost. All body policy must be decided at `requestWillBeSent`.

3. **You cannot have both un-perturbed timings and guaranteed bodies.** `Fetch` interception guarantees bodies but serialises every load and corrupts the timing data. `streamResourceContent` preserves timing but is EXPERIMENTAL and can miss data if you subscribe late. Pick per job; do not pretend one mode does both.

4. **CDP `DOM.*` events cannot give you mutation *content* at scale.** `childNodeCountUpdated` is a counter. Recovering content requires per-node `requestChildNodes` calls that themselves generate events. There is no configuration in which this is competitive with `MutationObserver`.

5. **`Network.webSocketClosed` carries no close code and no reason.** You cannot tell an agent *why* a socket closed from CDP. Workaround: hook `WebSocket.prototype` in the isolated world to capture `CloseEvent.code`/`.reason` — but that is page-visible instrumentation and violates the "invisible observer" principle. Recommend documenting the gap rather than hooking.

6. **Service worker sessions are unreliable via page-level auto-attach.** `Runtime.enable` timed out. Expect to need browser-level auto-attach plus the `ServiceWorker` domain, and expect rough edges. Do not promise full SW console/error capture in v1.

7. **Worker console output on the page session is text-only.** No structured args, no expandable objects, no stack traces. Structured worker diagnostics *require* a per-worker session, which means N extra sessions and N× the domain-enable cost on worker-heavy apps.

8. **Trace and network clocks have different bases** and must be correlated heuristically. Sub-millisecond cross-stream causality claims are not supportable.

9. **Most causality is inferred, not known.** Only initiator stacks and our own action dispatch are ground truth. Mutation→action and route-change→action are heuristics. Any UI that renders them identically is lying to the agent.

10. **Redaction and debuggability are in genuine tension** and no clever regex resolves it. The honest resolution is the capability-gated reveal with a bounded, memory-only cache — which means there *is* a window in which secrets live in daemon memory. This must be documented, not hidden.

11. **`Memory.getDOMCountersForLeakDetection` does not work in headless.** Leak detection as a feature is off the table for now.

12. **`enableDurableMessages` behaviour is unverified.** The protocol text warns of deadlocks with the `Network.enable` form. I confirmed `configureDurableMessages` is accepted but did **not** confirm it rescues bodies across navigation. This needs a dedicated test before we build the body strategy on it.

13. **The `dataReceived.data` field and `streamResourceContent` are both EXPERIMENTAL.** They can be removed or changed in any Chrome release. Our body strategy therefore needs a version-gated fallback path and a CI canary against Chrome stable/beta.

---

## Open questions for the owner

1. **Body capture default:** stream *all* bodies under N KB, or only content-types on an allow-list (`json`, `text`, `xml`, `html`, `javascript`)? The former is far more useful for debugging and far more expensive.
2. **Is `--record-video` allowed to imply tracing?** Frame-accurate FPS/dropped-frame data needs a trace at ~130 KB/s (slim). Or do we accept coarser FPS from the injected `long-animation-frame` observer only?
3. **Reveal cache:** is a 64 MB / 10-minute memory-only pre-redaction cache acceptable, or must redaction be genuinely irreversible (in which case `--reveal` cannot exist and debugging auth flows gets much harder)?
4. **Per-worker sessions by default?** Cost is real on worker-heavy apps. Default on, default off, or auto-enable when a worker throws?
5. **Async stack depth default:** 32 (useful traces, real V8 cost) or 0 (cheap, but "at anonymous:1:1" is often useless)? Should it differ between interactive and background-job modes?
6. **`Debugger.enable` default?** Inlined async parents and source-map `scriptParsed` events are valuable, but it risks pausing on a page's own `debugger;` statements. Suggest: off by default, `Debugger.setSkipAllPauses{skip:true}` if we do enable it.
7. **Event segment format:** JSONL+zstd (greppable, debuggable, simple) or Arrow/Parquet (`arrow`/`parquet` **59.1.0**) for fast `--filter` on long jobs? JSONL is my recommendation for v1.
8. **Do we need HAR *import*?** Trivial via the `har` crate, and would let an agent diff a recorded run against a reference. Out of scope for v1?
9. **CI canary against Chrome beta/dev** to catch removal of `streamResourceContent` / `dataReceived.data` — worth the maintenance cost?

---

## Sources

1. https://chromedevtools.github.io/devtools-protocol/tot/Network/ — Network domain events, methods, `ResourceTiming`, `Initiator`, `CorsErrorStatus`, experimental markers
2. https://chromedevtools.github.io/devtools-protocol/tot/Runtime/ — `RemoteObject`, `ObjectPreview`, `PropertyPreview`, `callFunctionOn`, `evaluate`, `SerializationOptions`, `addBinding`
3. https://chromedevtools.github.io/devtools-protocol/tot/Log/ — `Log.entryAdded`, `LogEntry.source` enum, `startViolationsReport`, `ViolationSetting`
4. https://chromedevtools.github.io/devtools-protocol/tot/Tracing/ — `Tracing.start` params, `transferMode`, `streamFormat`, `tracingComplete`, `bufferUsage`, `IO.read`
5. https://chromedevtools.github.io/devtools-protocol/tot/Fetch/ — `Fetch.enable`, `RequestPattern`, `requestPaused`, `getResponseBody`, `takeResponseBodyAsStream`, `continueRequest`/`Response`
6. https://raw.githubusercontent.com/ChromeDevTools/devtools-frontend/main/front_end/models/har/Log.ts — **primary source** for `buildTimings()`, `pseudoWallTime`, `buildContent`, `sanitize`, all `_`-prefixed extension fields (fetched via curl, 571 lines)
7. https://github.com/ChromeDevTools/devtools-frontend/blob/main/front_end/models/har/Writer.ts — HAR stream writing and content-encoding decisions
8. https://developer.chrome.com/docs/devtools/performance/timeline-reference — DevTools timeline event categories (did **not** contain low-level trace event names; answered empirically instead)
9. https://www.chromium.org/developers/how-tos/trace-event-profiling-tool/ — trace event format background
10. https://www.chromium.org/developers/how-tos/trace-event-profiling-tool/frame-viewer/ — frame viewer / `PipelineReporter` frame states
11. https://groups.google.com/a/chromium.org/g/graphics-dev/c/vab1W1gP-iw — `PipelineReporter` states and dropped-frame interpretation
12. https://crates.io/api/v1/crates/{tokio,serde_json,regex,aho-corasick,memchr,zstd,bytes,simd-json,sonic-rs,sourcemap,har,uuid,jiff,chrono,parking_lot,dashmap,rustc-hash,url,base64,arrow,parquet,flate2,brotli-decompressor,tokio-util,futures-util,…} — version and freshness data as of 2026-08-04
13. https://crates.io/api/v1/crates?q=chrome+trace+event / `q=perfetto` — established that **no** maintained general-purpose Rust Chrome-trace parser exists
14. `http://127.0.0.1:<port>/json/version` and `/json/protocol` on local Chrome 151.0.7922.72 — **primary source** for every "CONFIRMED" protocol claim, plus ~30 live CDP experiments described in "What we verified empirically"
15. http://www.softwareishard.com/blog/har-12-spec/ — HAR 1.2 specification (**fetch FAILED: TLS certificate expired**; HAR field semantics were therefore taken from DevTools' `Log.ts` type definitions in source 6, which is the stronger compatibility target anyway)
