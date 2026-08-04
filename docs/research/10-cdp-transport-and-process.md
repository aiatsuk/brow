# CDP Transport, Chromium Process Management, Targets and Sessions

> **Verification pass 2026-08-04 — three corrections that change decisions, detailed inline and summarised at the end of this document.** (1) The `setsid()` supervisor **is confirmed to survive `launchctl bootout` on macOS**, but `setsid()` does **not** escape a systemd cgroup — Linux needs `systemd-run --user --scope` (§8.2). (2) The 100 MB pipe cap is **client→Chrome only**; a 125.8 MB screenshot response transits fine (§1.1). (3) **`--remote-debugging-pipe` sets `navigator.webdriver === true` unconditionally** — websocket mode does not — so every page `brow` touches is identifiable as automation (§6.4). Also: `Schema.getDomains` is *not* removed, the Windows `--remote-debugging-io-pipes` reading is *confirmed* against the consuming code, and the "copied profile can't be decrypted" claim is *unsupported on macOS*.

> **Bottom line.** Use `--remote-debugging-pipe` (fd 3 read / fd 4 write, NUL-delimited UTF-8 JSON — confirmed in Chromium source and verified against local Chrome 151.0.7922.72). It gives you an unforgeable, unlistenable, single-client channel that no other local process can hijack, and it kills the browser automatically when the holder of the fds goes away — which is simultaneously the best orphan-prevention primitive available and **the single biggest architectural constraint in this whole document**: a persistent background Chromium *cannot* survive a `browserd` restart if `browserd` itself holds the pipe. The fix is a per-browser `setsid()`-detached supervisor process that owns fds 3/4 and re-exports the protocol over a `0600` unix socket. Use the flat protocol (`sessionId` on every envelope) with `Target.setAutoAttach{autoAttach, waitForDebuggerOnStart, flatten:true, filter}` applied **recursively on every new page/iframe session** — without that, cross-origin iframes are simply invisible (verified: parent session's `iframe.contentDocument === null`). Use `Target.createBrowserContext` for per-job isolation (verified full cookie/localStorage isolation; marginal cost ≈150 MB per extra live page, ≈0 for an empty context) rather than separate `--user-data-dir` processes (≈500 MB + 6 processes each). Pin `browser_protocol.json` + `js_protocol.json` from the `ChromeDevTools/devtools-protocol` repo (rolled ~daily; head at `r1672245`, 2026-08-01) and generate Rust types at build time — because over a pipe there is **no** way to ask Chrome for its own protocol descriptor (`Schema.getDomains` is deprecated, renderer-only, and returns nothing but domain names at version "1.2"; `/json/protocol` needs an HTTP port you do not want).

---

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| `--remote-debugging-pipe` (JSON/ASCIIZ mode) as the only transport | No listening socket → no local privilege escalation; single client by construction; fds die with the process | `--remote-debugging-port` + WebSocket: any local process can connect, no auth, defeats "raw CDP is never exposed" | **CONFIRMED** (source + observed) |
| ASCIIZ (NUL-delimited JSON), not CBOR | CBOR mode is marked *"Experimental (!)"* in `devtools_pipe_handler.h`; JSON keeps debuggability and lets us log the wire | `--remote-debugging-pipe=cbor` | **CONFIRMED** (source) |
| A per-browser `brow-supervisor` process, `setsid()`-detached, owns fds 3/4 and re-exports over `$XDG_RUNTIME_DIR`/`~/Library/Application Support/brow/run/<id>.sock` | Chrome exits ~0.5 s after the pipe reader sees EOF (observed). Without a separate holder, restarting `browserd` kills every browser | (a) browserd holds the pipe → browsers die on restart; (b) TCP port → security hole | **CONFIRMED** (observed pipe-close → exit rc=0 in 0.5 s) |
| Flat protocol everywhere: `flatten: true`, `sessionId` in the envelope | Non-flat (`Target.sendMessageToTarget` / `receivedMessageFromTarget`) is `deprecated` in the PDL; flat is the stated future default (crbug.com/991325) | Nested sessions | **CONFIRMED** (PDL) |
| One global monotonic `u64` request id across all sessions | Chrome echoes `id` **and** `sessionId` on every reply, so ids *could* be per-session — but a single counter removes an entire class of correlation bugs at zero cost | Per-session id namespaces | **CONFIRMED** (observed: id 99 sent on browser + page session concurrently, both replies distinguishable only by `sessionId`) |
| Recursive `Target.setAutoAttach(..., flatten:true, filter:[...])` on browser session **and** on every new page/iframe session | Auto-attach only reaches *immediate* children of the target it was set on. A→B→C needs three calls | Single browser-level `setAutoAttach` | **CONFIRMED** (observed OOPIF attach only after page-session setAutoAttach) |
| `Target.createBrowserContext{disposeOnDetach:true}` for per-job isolation | Verified full cookie + localStorage isolation, context-scoped `Storage.*` and `Browser.grantPermissions`; ~free when empty | Separate `--user-data-dir` Chrome processes | **CONFIRMED** (observed) |
| Never launch against Chrome's default profile directory | Chrome 136+ silently ignores `--remote-debugging-port`/`--remote-debugging-pipe` when pointed at the default data dir | "Attach to the user's logged-in Chrome" | **LIKELY** (official Chrome blog; not empirically retested — testing it would touch the user's real profile) |
| Pin protocol JSON in-tree, codegen at build time via `build.rs` | No runtime protocol descriptor exists over a pipe | Runtime `Schema.getDomains` | **CONFIRMED** (observed: `Schema.getDomains` → `-32601` on browser session; on a page session returns 21 names at version "1.2") |
| `tokio::net::unix::pipe::{Sender,Receiver}` on Unix; `tokio::net::windows::named_pipe` + `--remote-debugging-io-pipes` on Windows | Native async, no extra crate, `from_owned_fd` accepts the fds we created | `interprocess`, blocking threads | **CONFIRMED** (docs.rs API listing) |

---

## 1. Transport: `--remote-debugging-pipe`

### 1.1 Wire format — confirmed from Chromium source

`content/browser/devtools/devtools_pipe_handler.cc` ([source](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_pipe_handler.cc)) defines exactly two modes:

```cpp
enum class ProtocolMode {
  // Legacy text protocol format with messages separated by \0's.
  kASCIIZ,
  // Experimental (!) CBOR (RFC 7049) based binary format.
  kCBOR
};
...
std::string str_mode = base::ToLowerASCII(
    base::CommandLine::ForCurrentProcess()->GetSwitchValueASCII(
        switches::kRemoteDebuggingPipe));
mode_ = str_mode == "cbor" ? ProtocolMode::kCBOR : ProtocolMode::kASCIIZ;
```

So:

| Flag | Mode |
|---|---|
| `--remote-debugging-pipe` | ASCIIZ: UTF-8 JSON, each message terminated by a single `0x00` byte |
| `--remote-debugging-pipe=JSON` | same (anything that isn't `cbor`) |
| `--remote-debugging-pipe=cbor` | CBOR with a tag-24 envelope; framing is self-describing (length in the envelope header) |

Writer (`PipeWriterASCIIZ::WriteIntoPipe`) does `WriteBytes(msg); WriteBytes("\0", 1);` — **no length prefix, no trailing newline**. Reader scans each chunk for `0x00` and splits. Two hard numbers from the same file:

```cpp
const size_t kReceiveBufferSizeForDevTools = 100 * 1024 * 1024;  // 100Mb
const size_t kWritePacketSize = 1 << 16;                          // 64 KiB write chunks
```

**Consequence:** a single message larger than 100 MB **sent by the client to Chrome** terminates the connection (`"Connection closed, not enough capacity"`).

> **Corrected 2026-08-04:** the original text also claimed screenshot/`getResponseBody` *responses* were at risk. **They are not.** `kReceiveBufferSizeForDevTools` is applied in exactly one place — `PipeReaderASCIIZ`'s constructor (`read_buffer_->set_max_buffer_size(kReceiveBufferSizeForDevTools)`, [devtools_pipe_handler.cc:316](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_pipe_handler.cc)) — i.e. on the **fd-3 reader**, which only ever carries *commands from us*. `PipeWriterBase::WriteIntoPipe` (fd 4, Chrome → us) has **no size cap at all**, only a 64 KiB chunking loop. Two live tests on Chrome 151:
> * **Response direction, 125.8 MB:** a 5600×5600 noise canvas captured with `Page.captureScreenshot` returned a **125,834,904-byte base64** payload (94.4 MB PNG) in 28.5 s. Transport intact; `Browser.getVersion` and the page session both still answered afterwards. **No cap on responses.**
> * **Command direction, 110 MB:** a `Runtime.evaluate` whose `expression` was a 110 MB string produced, in Chrome's stderr, exactly:
>   ```
>   [ERROR:net/server/http_connection.cc:44] Too large read data is pending: capacity=104857600, max_buffer_size=104857600, read=104857600
>   [ERROR:content/browser/devtools/devtools_pipe_handler.cc:324] Connection closed, not enough capacity
>   ```
>   and the browser went away. **The cap is real, and it is client→Chrome only.**
>
> So the things to bound are *outbound* payloads: `Runtime.evaluate.expression`, `Page.addScriptToEvaluateOnNewDocument.source`, `Input.dispatchDragEvent.data`, `Network.setCookies`, `Fetch.fulfillRequest.body`, `Runtime.callFunctionOn.arguments`. Add a hard client-side guard at ~64 MB on the write path. Inbound sizing is purely *our* memory problem (§1.4 of `50-capture-*` still applies — cap capture megapixels), not a transport kill.
>
> Note also: `PipeReaderCBOR` has **no** `set_max_buffer_size` call at all — it reads the CBOR envelope length and `resize()`s to it. CBOR mode therefore has *no* 100 MB inbound limit and a different (worse) DoS profile. Another reason to stay on ASCIIZ.

### 1.2 Which fds

`content/public/browser/devtools_agent_host.h` ([source](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/public/browser/devtools_agent_host.h)):

```cpp
// File descriptor used by DevTools remote debugging pipe handler
// to read and write protocol messages.
static constexpr int kReadFD = 3;
static constexpr int kWriteFD = 4;
```

Chrome **reads** commands from fd 3 and **writes** events/responses to fd 4. Node/Puppeteer/Playwright achieve this with `stdio: ['ignore','pipe','pipe','pipe','pipe']` (indices 3 and 4). From Rust: create two `pipe(2)` pairs and `dup2()` them onto 3 and 4 in the child.

Since Chrome 136-ish there is a **hard pre-flight check** — `chrome/app/chrome_main_delegate.cc`:

```cpp
// The DevTools remote debugging pipe file descriptors need to be checked
// before any other files are opened, see https://crbug.com/40259890.
if (is_browser && command_line.HasSwitch(::switches::kRemoteDebuggingPipe) &&
    !pipes_are_specified_explicitly &&
    !devtools_pipe::AreFileDescriptorsOpen()) {
  LOG(ERROR) << "Remote debugging pipe file descriptors are not open.";
  return CHROME_RESULT_CODE_UNSUPPORTED_PARAM;
}
```

I hit this exact error empirically on my first attempt (CPython closes non-`pass_fds` descriptors *after* `preexec_fn`, so fds 3/4 were closed again before `exec`). `AreFileDescriptorsOpen()` is `fcntl(3, F_GETFL) != -1 && fcntl(4, F_GETFL) != -1` on POSIX ([components/devtools/devtools_pipe/devtools_pipe.cc](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/components/devtools/devtools_pipe/devtools_pipe.cc)).

**Windows.** `content/public/common/content_switches.cc`:

```
// Specifies pipe names for the incoming and outbound messages on the Windows
// platform. This is a comma separated list of two pipe handles serialized as
// unsigned integers, e.g. "--remote-debugging-io-pipes=3,4".
const char kRemoteDebuggingIoPipes[] = "remote-debugging-io-pipes";
```

This is Windows-only (`#if BUILDFLAG(IS_WIN)`), and when present it bypasses the fd-open check entirely (`pipes_are_specified_explicitly`). **This is the sane Windows path**: create two inheritable anonymous-pipe or named-pipe `HANDLE`s, pass their numeric values in the switch, and set `bInheritHandles=TRUE` / `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. Do *not* try to reproduce Node's CRT `lpReserved2` fd-table trick from Rust.

> **Verified 2026-08-04:** this was previously flagged as the risky reading. It is **correct**, and I found the consuming code. `DevToolsAgentHost::StartRemoteDebuggingPipeHandler` in [`content/browser/devtools/devtools_agent_host_impl.cc`](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_agent_host_impl.cc) starts from `read_fd = kReadFD (3); write_fd = kWriteFD (4);` and then, `#if BUILDFLAG(IS_WIN)` only, calls `AdoptPipes(io_pipes, read_fd, write_fd)`. `AdoptPipes` splits the switch on `,`, and `AdoptHandle` does:
> ```cpp
> uint32_t handle_as_uint32;
> if (!base::StringToUint(serialized_pipe, &handle_as_uint32)) return -1;
> HANDLE handle = base::win::Uint32ToHandle(handle_as_uint32);
> if (GetFileType(handle) != FILE_TYPE_PIPE) return -1;
> return _open_osfhandle(reinterpret_cast<intptr_t>(handle), flags);
> ```
> with the in-source comment *"The parent process is expected to serialize the input and the output pipe **handles** as unsigned integers"* and *"We use the fact that inherited handles in the child process have the same value and access rights as in the parent process."*
>
> Four implementation facts that follow, none of which were in the original text:
> 1. **Values are raw `HANDLE`s as unsigned decimal `uint32`**, not CRT fd numbers. The `lpReserved2` reading is wrong. `base::StringToUint` → decimal only, no `0x`.
> 2. **Order is `<handle Chrome READS from>,<handle Chrome WRITES to>`** — `pipe_names[0]` → `AdoptHandle(in_pipe, _O_RDONLY)`, `pipe_names[1]` → `AdoptHandle(out_pipe, 0)`.
> 3. **`GetFileType(handle) != FILE_TYPE_PIPE` is rejected.** It must be an anonymous pipe (`CreatePipe`) or a named pipe — a file handle or a socket will not be accepted.
> 4. **Failure is near-silent**: if `AdoptPipes` returns false, `StartRemoteDebuggingPipeHandler` immediately runs `on_disconnect` and returns, i.e. the browser shuts down without ever speaking protocol. Budget a startup timeout and a clear error for this.
>
> On Windows the fds also go through the CRT: `PipeReaderBase`/`PipeWriterBase` call `_get_osfhandle(fd)` and then use `ReadFile`/`WriteFile`/`CancelIoEx` on the resulting `HANDLE`. So Windows support is a real, documented path — no reverse engineering needed.

### 1.3 Does the pipe survive without a websocket server? Yes.

`DevToolsPipeHandler`'s constructor does `DevToolsAgentHost::CreateForBrowser(nullptr, DevToolsAgentHost::CreateServerSocketCallback())` and attaches itself directly. **No HTTP handler, no `/json/version`, no `DevToolsActivePort` file** (verified: with pipe-only launch the file is absent; with `--remote-debugging-port=0` it appears and contains `62160\n/devtools/browser/7374a7be-…`).

Also verified: `--remote-debugging-pipe` and `--remote-debugging-port=NNNN` **can be used simultaneously** — both channels worked at once. Keep the port off by default; expose it behind an explicit `brow doctor --unsafe-open-port` escape hatch only.

### 1.4 Why pipe is the right choice — and where it is *not* more privileged

Both the pipe client and the websocket client return `MayAccessAllCookies() == true`; the WS handler (`content/browser/devtools/devtools_http_handler.cc`, `DevToolsAgentHostClientImpl`) does the same. So the pipe is **not** more restricted — the pipe handler additionally returns `AllowUnsafeOperations() == true`. The security win is purely topological:

* No `listen()`, so no other local process/UID can attach. A `remote-debugging-port` on 127.0.0.1 is reachable by *every* process on the machine, including the browser's own renderers via DNS-rebinding-ish tricks and by any other agent.
* No `Host:`/`Origin:` header validation surface (`--remote-allow-origins`, the CVE-adjacent area).
* No port number to leak into `ps`, logs, or `DevToolsActivePort`.
* Chrome dies when the pipe holder dies — orphan prevention for free.

Playwright uses `--remote-debugging-pipe` by default and only falls back to `--remote-debugging-port=0` for Selenium Grid ([chromium.ts](https://raw.githubusercontent.com/microsoft/playwright/main/packages/playwright-core/src/server/chromium/chromium.ts)). That is a strong production signal.

### 1.5 Chrome 136+ default-profile restriction — **read this**

Per the official Chrome blog ([Changes to remote debugging switches to improve security](https://developer.chrome.com/blog/remote-debugging-port)): from Chrome 136, `--remote-debugging-port` **and** `--remote-debugging-pipe` "will no longer be respected if attempting to debug the default Chrome data directory"; they "must now be accompanied by the `--user-data-dir` switch to point to a non-standard directory". Rationale: a non-default data dir gets a different OS-keychain encryption key, so an attacker who turns on CDP cannot decrypt the real profile's cookies/passwords.

Practical fallout for `brow`:

* We must always pass an explicit `--user-data-dir` under `~/Library/Application Support/brow/profiles/<name>` (macOS) / `~/.local/share/brow/profiles/<name>` (Linux). Fine — we wanted that anyway.
* **"Automate the user's already-logged-in Chrome profile" is off the table** with stock Chrome — but for a narrower reason than originally written. Human handoff (log in once inside a `brow` profile) is the only honest answer, and it aligns with the "Keychain/Touch ID out of scope" constraint.

> **Corrected 2026-08-04.** The original text asserted that copying `Cookies`/`Login Data` into our dir *cannot work* because the encryption key "is now keyed per data directory". That is **not supported on macOS** and should not be shipped as an impossibility.
> * The blog sentence is real and quoted correctly — *"A non-standard data directory uses a different encryption key meaning Chrome's data is now protected from attackers."* (re-fetched today) — but it is a one-line rationale, not a spec, and it does not say *how*.
> * On this machine the OSCrypt v10 key lives in **one application-wide Keychain item**, verified with `security find-generic-password -s "Chrome Safe Storage"`: `svce="Chrome Safe Storage"`, `acct="Chrome"`. There is **no data-directory or profile component in the item's identity**. (I read only the attribute names, never the secret, and never touched the user's profile.)
> * The mechanism that genuinely binds a key to more than a password is **App-Bound Encryption, which is Windows-only** (`chrome/browser/os_crypt/app_bound_encryption_provider_win.cc`, with `PROTECTION_PATH_VALIDATION` / `PROTECTION_PATH_VALIDATION_WITH_ISOLATION` and an explicit *"Modified user data dir, signal temporarily unavailable"* branch).
>
> **Revised claim:** the load-bearing blocker is the *first* bullet, not the crypto — Chrome ≥136 refuses to honour `--remote-debugging-port`/`--remote-debugging-pipe` at all against the default data directory, so there is no way to drive the live profile regardless of whether its secrets are decryptable. Secondary practical blockers: SQLite locking on a running profile, and the fact that importing cookies is a `waiting_for_approval` action per the spec anyway. **Do not write "the key is bound to the data directory" in SKILL.md** — say "Chrome refuses remote debugging on its default profile; log in once inside a `brow` profile."
* Chrome for Testing keeps the old behaviour, but shipping it means downloading a browser binary → violates a hard constraint.

### 1.6 Rust skeleton (Unix)

```rust
use std::os::fd::{AsRawFd, OwnedFd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe::{Receiver, Sender};

// to_chrome: we write -> child reads on fd 3
// from_chrome: child writes on fd 4 -> we read
let (chrome_read, our_write) = nix::unistd::pipe()?;   // OwnedFd, OwnedFd
let (our_read, chrome_write) = nix::unistd::pipe()?;

let mut cmd = std::process::Command::new(&chrome_bin);
cmd.args(flags);
unsafe {
    let r = chrome_read.as_raw_fd();
    let w = chrome_write.as_raw_fd();
    cmd.pre_exec(move || {
        // dup2 clears CLOEXEC on the *new* fd, which is what we want.
        if libc::dup2(r, 3) == -1 { return Err(std::io::Error::last_os_error()); }
        if libc::dup2(w, 4) == -1 { return Err(std::io::Error::last_os_error()); }
        libc::setsid();                 // own session -> own process group
        Ok(())
    });
}
let child = cmd.spawn()?;
drop(chrome_read); drop(chrome_write);   // parent must close its copies

// tokio wants non-blocking fds; from_owned_fd sets O_NONBLOCK for us.
let mut tx: Sender   = Sender::from_owned_fd(our_write)?;
let mut rx: Receiver = Receiver::from_owned_fd(our_read)?;
```

`Sender::from_owned_fd` / `Receiver::from_owned_fd` (plus `_unchecked` variants that skip the FIFO `fstat` check, and `into_nonblocking_fd`) are confirmed present in tokio 1.53.1 ([docs.rs](https://docs.rs/tokio/latest/tokio/net/unix/pipe/struct.Receiver.html)).

Framing loop — trivial, but note the two traps:

```rust
let mut buf = BytesMut::with_capacity(64 * 1024);
loop {
    if rx.read_buf(&mut buf).await? == 0 { return Err(Transport::Eof); }
    while let Some(i) = memchr::memchr(0, &buf) {
        let frame = buf.split_to(i);
        buf.advance(1);                       // drop the NUL
        if frame.len() > MAX_FRAME { return Err(Transport::Oversize); }
        dispatch(serde_json::from_slice::<Incoming>(&frame)?);
    }
    if buf.len() > 100 * 1024 * 1024 { return Err(Transport::Oversize); } // mirror Chrome's cap
}
```

Traps: (1) `\0` never appears inside valid JSON, so a naive `memchr` split is correct — but a JSON string *can* contain the escape `\u0000`, which serde emits as the two bytes `\` `u`… so the encoder is safe as long as you use `serde_json` (it never emits a raw NUL). (2) Writes must be a single `write_all` of `json ++ [0u8]`; interleaving from two tasks corrupts the stream — serialize behind one writer task fed by an `mpsc` channel.

---

## 2. Routing, sessions and the flat protocol

### 2.1 Envelope shapes (all observed on Chrome 151)

```jsonc
// outbound command, browser session (no sessionId)
{"id":1,"method":"Browser.getVersion"}
// outbound command, page session
{"id":8,"sessionId":"EC1C…","method":"Runtime.evaluate","params":{"expression":"1+1","returnByValue":true}}
// inbound response (sessionId echoed back)
{"id":8,"result":{"result":{"type":"number","value":2}},"sessionId":"EC1C…"}
// inbound event, session-scoped
{"method":"Inspector.targetCrashed","params":{},"sessionId":"8CA1…"}
// inbound event, browser-scoped
{"method":"Target.targetCrashed","params":{"targetId":"B281…","status":"crashed","errorCode":5}}
```

Over a pipe you are **already attached to the browser target** — no `Target.attachToBrowserTarget` needed; any message without `sessionId` goes to the browser session. (`attachToBrowserTarget` exists and is `experimental`; it is for websocket clients attached to a page.)

### 2.2 Error codes actually observed

| Code | Message | Meaning |
|---|---|---|
| `-32601` | `'Nope.doesNotExist' wasn't found` | unknown method **or** method not available on this session type |
| `-32602` | `No session with given id` | wrong routing (see below) / bad params |
| `-32001` | `Session with given id not found.` | you addressed an envelope to a session that no longer exists |

**Routing gotcha, verified.** `Target.detachFromTarget{sessionId: S}` sent *inside* session `S` fails with `-32602 No session with given id`: the command is routed to `S`, and `S`'s Target domain has no knowledge of itself. It must be sent on the **browser session** (no envelope `sessionId`), where it returns `{}` and subsequent commands to `S` return `-32001`. Same for `Target.closeTarget`, `Target.disposeBrowserContext`, `Storage.getCookies{browserContextId}`, `Browser.grantPermissions{browserContextId}`.

### 2.3 Message id namespacing

Chrome echoes both `id` and `sessionId`. Two concurrent commands with `id: 99` — one on the browser session, one on a page session — both returned correctly and were distinguishable only by the echoed `sessionId` (observed). **Recommendation:** single process-wide `AtomicU64`; key the pending map on `id` alone; assert the echoed `sessionId` matches what you sent (defensive — catches routing bugs immediately).

```rust
struct Pending { tx: oneshot::Sender<Result<Value, CdpError>>, session: Option<SessionId>, method: &'static str, deadline: Instant }
// map: DashMap<u64, Pending>
```

### 2.4 Detached sessions

* Explicit `Target.detachFromTarget` → `Target.detachedFromTarget{sessionId}` event, then `-32001` for anything further.
* Target destroyed / browser context disposed → the session dies the same way (observed: after `Target.disposeBrowserContext`, a command on a session belonging to it returned `-32001`).
* **In-flight commands on a dying session may never be answered.** There is no "session closed, here are your cancelled ids" message. Every pending request needs a deadline *and* a subscription to `Target.detachedFromTarget` / `Target.targetDestroyed` / `Target.targetCrashed` so it can be failed deterministically.

---

## 3. Target lifecycle

### 3.1 The exact type strings

From `content/browser/devtools/devtools_agent_host_impl.cc` ([source](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_agent_host_impl.cc)):

```
"tab"  "page"  "iframe"  "worker"  "shared_worker"  "service_worker"
"worklet"  "browser"  "webview"  "other"  "auction_worklet"
"assistive_technology"  "browser_ui"
```

Chrome (not `content/`) adds more via its manager delegate — I observed **`background_page`** for MV2 extension background pages, which is *not* in the list above. **Treat `TargetInfo.type` as an open string**, not a closed Rust enum; use `#[serde(other)] Unknown(String)`.

`TargetInfo` fields (from `domains/Target.pdl` on main): `targetId, type, title, url, attached, parentId?, openerId?, canAccessOpener, openerFrameId?, parentFrameId?, browserContextId?, subtype?, embedderData?`. `subtype` carries e.g. `"prerender"` for prerendered pages; `embedderData` is only set for `type: "tab"`.

### 3.2 The default filter is not what you want

PDL comment, verbatim:

```
# If filter is not specified, the one assumed is
# [{type: "browser", exclude: true}, {type: "tab", exclude: true}, {}]
# (i.e. include everything but `browser` and `tab`).
```

Verified: `Target.getTargets` with no filter omitted `tab` targets; with `filter: [{}]` I got `{'service_worker':1,'background_page':1,'page':1,'tab':2}`.

Entries are matched **sequentially, first match wins**. Recommended explicit filter for `brow`:

```json
[{"type":"page"},{"type":"iframe"},{"type":"worker"},{"type":"shared_worker"},
 {"type":"service_worker"},{"type":"worklet"},{"type":"webview"},
 {"type":"background_page"},{"type":"other"},{"exclude":true}]
```

Whether to include `tab` targets: a `tab` target is the container above `page`, and it is the only place where **prerender / bfcache page swaps** are observable as one continuous entity. For a state-aware site mapper that matters. Recommendation: attach to `tab` targets *in addition to* `page` targets, and treat `tab` as the stable identity for "this browser tab" while `page` targets come and go under it. Mark as **UNVERIFIED** — I did not empirically exercise a prerender swap.

### 3.3 OOPIF: what breaks if you skip it

**Verified, and it is severe.** Parent page = `data:text/html,<iframe src="https://example.com/">`, `--site-per-process`:

* In the parent session, `document.querySelector('iframe').contentDocument === null` → **`true`**. The parent session's `DOM.*`, `Accessibility.*`, `CSS.*`, `Runtime.*` cannot see one byte of the iframe.
* Only after calling `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true}` **on the page session** did I get:
  ```json
  {"method":"Target.attachedToTarget","params":{"sessionId":"B841…",
    "targetInfo":{"targetId":"48C7…","type":"iframe","url":"","attached":true,
      "parentId":"5F36…","parentFrameId":"5F36…","browserContextId":"FB29…"},
    "waitingForDebugger":true},
   "sessionId":"30AF…"}                <-- envelope = the PARENT page session
  ```
  and in that new session `Runtime.evaluate("location.href")` → `"https://example.com/"`.
* Auto-attach reaches only **immediate** children. For A→B→C you must call `setAutoAttach` again on B's session. Confirmed by the PDL wording and by the CDP mailing-list / chrome-devtools-mcp discussions ([chrome-devtools-mcp#703](https://github.com/ChromeDevTools/chrome-devtools-mcp/issues/703)).

So the Unified Page Tree code path is: **on every `Target.attachedToTarget` for a `page`/`iframe`/`webview` target, immediately (a) `Target.setAutoAttach{...flatten:true, filter}` on that new session, then (b) the per-session domain enables, then (c) `Runtime.runIfWaitingForDebugger`.** Use `waitForDebuggerOnStart: true` so no script runs before your listeners are installed — otherwise you lose early `Network.*`, `Console.*` and `Page.frameNavigated` events on freshly created frames.

The alternative `Target.autoAttachRelated{targetId, waitForDebuggerOnStart, filter}` (experimental, browser session only) monitors one target and all its descendants, "cancels the effect of any previous setAutoAttach and is also cancelled by subsequent setAutoAttach". Attractive but it is a *replacement*, not an addition — it makes multi-tab bookkeeping worse. Recommendation: stick with recursive `setAutoAttach`.

### 3.4 Do **not** disable site isolation to dodge OOPIFs

Flag lists aimed at "bot detection evasion" often include `--disable-features=IsolateOrigins,site-per-process`. Doing so would merge cross-origin iframes back into the parent renderer and make the parent DOM tree "just work" — at the cost of (a) turning off Chrome's main memory-corruption mitigation on a machine that browses arbitrary sites for an agent, and (b) diverging from what real users see. Also note Puppeteer disables `IsolateSandboxedIframes` and `ProcessPerSiteUpToMainFrameThreshold` by default ([ChromeLauncher.ts](https://raw.githubusercontent.com/puppeteer/puppeteer/main/packages/puppeteer-core/src/node/ChromeLauncher.ts)) — those are narrower and defensible; the site-per-process kill is not.

### 3.5 Lifecycle events to subscribe to

| Event | Scope | Use |
|---|---|---|
| `Target.targetCreated{targetInfo}` | browser (needs `setDiscoverTargets`) | inventory, including targets you don't attach to |
| `Target.attachedToTarget{sessionId,targetInfo,waitingForDebugger}` | browser or parent session | the real workhorse; **envelope `sessionId` tells you the parent session** |
| `Target.detachedFromTarget{sessionId}` | browser or parent session | fail pending requests |
| `Target.targetInfoChanged{targetInfo}` | browser | URL/title changes; fires a lot (observed several per navigation) |
| `Target.targetDestroyed{targetId}` | browser | invalidate `@node-*` refs bound to that document generation |
| `Target.targetCrashed{targetId,status,errorCode}` | browser | observed `status:"crashed", errorCode:5` after `Page.crash` |
| `Inspector.targetCrashed` (params `{}`) | the crashed session | same event, session-scoped; needs `Inspector.enable` |
| `Inspector.targetReloadedAfterCrash` | session | sad-tab was reloaded |
| `Inspector.detached{reason}` | session | "remote debugging connection is about to be terminated" |

---

## 4. BrowserContext isolation

### 4.1 What it actually isolates — verified

Two contexts via `Target.createBrowserContext{disposeOnDetach:true}`, one page each on `https://example.com`:

```
set in ctx1: document.cookie='brow=ctx1'; localStorage.k='ctx1'
ctx2 reads:        ["", null]
ctx1 reads:        ["brow=ctx1", "ctx1"]
default ctx reads: ["", null]
Storage.getCookies{browserContextId: ctx1} -> [{name:"brow",value:"ctx1",domain:"example.com",...}]
Storage.getCookies{browserContextId: ctx2} -> []
Storage.getCookies{} (default)             -> []
Browser.grantPermissions{permissions:["geolocation"],origin:"https://example.com",browserContextId:ctx1} -> {}
Target.disposeBrowserContext{ctx1} -> targets 5 -> 4, its sessions return -32001
```

So: cookies, localStorage, permissions and all `Storage.*` state are per-context. This is Chrome's incognito-profile mechanism with N profiles instead of 1. `createBrowserContext` also takes `proxyServer`, `proxyBypassList`, `originsWithUniversalNetworkAccess` (all `experimental`) — **per-job proxying without relaunching the browser**, which is a genuinely useful capability for the crawler.

### 4.2 Cost — measured (`ps rss` sums, macOS, so shared pages are double-counted; treat as an upper bound)

| Scenario | Total RSS | Processes |
|---|---|---|
| Browser with `--no-startup-window`, 2 empty contexts | ~505 MB | 6 |
| + 1 page (`https://example.com`) in ctx1 | ~958 MB | 8 |
| + 1 page in ctx2 (different context) | ~1107 MB (**+149 MB**) | 9 |
| 5 pages, all in the **same** context | +692 MB | +5 procs |
| 5 more pages, each in its **own** context | +661 MB | +5 procs |

**Conclusion: an empty BrowserContext costs essentially nothing; the cost is per live renderer, and it is the same whether or not the pages share a context.** Whereas a second `--user-data-dir` Chrome costs a whole browser: ~500 MB and 6 processes before you open anything, plus 0.2–0.3 s startup.

Startup latency to first successful `Browser.getVersion` over the pipe (measured): **headless 0.19 s, headful 0.27 s** on this machine, warm disk cache.

### 4.3 Which to use for per-job isolation

Use **BrowserContext per job**, `disposeOnDetach: true`. Use a **separate `--user-data-dir` browser** only when the job needs:
* different launch flags (device emulation is fine per-context via `Emulation.*`, but `--lang`, `--force-device-scale-factor`, `--proxy-server` at browser level, extensions, or a different Chrome channel are not);
* persistent logged-in state that must survive across jobs (BrowserContexts are ephemeral — **there is no way to persist a non-default BrowserContext to disk**; the default context in the profile dir is the only durable one);
* crash-blast-radius isolation for something known-hostile.

**Important corollary for the auth-branch part of the site mapper:** logged-in sessions cannot live in a disposable BrowserContext across daemon restarts. Either keep long-lived "named profiles" as separate `--user-data-dir` browsers, or export/import cookies + storage explicitly via `Storage.getCookies`/`Network.setCookies`/`Storage.setStorageItems` — and cookie import is on the spec's `waiting_for_approval` list anyway.

---

## 5. Discovering an installed browser (no downloads)

### macOS

Priority order (mirrors [chrome-launcher/src/chrome-finder.ts](https://raw.githubusercontent.com/GoogleChrome/chrome-launcher/main/src/chrome-finder.ts), which is the maintained reference):

1. `$BROW_CHROME_PATH` (ours), then `$CHROME_PATH`
2. `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`  ← present on this machine, `151.0.7922.72`
3. `/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta`
4. `/Applications/Google Chrome Dev.app/…`, `…Canary.app/Contents/MacOS/Google Chrome Canary`
5. `/Applications/Chromium.app/Contents/MacOS/Chromium`
6. `/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge`
7. `/Applications/Brave Browser.app/Contents/MacOS/Brave Browser`
8. `$HOME/Applications/…` for each of the above
9. LaunchServices sweep:
   `/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -dump`, grep for `.app` paths matching `Chrome|Chromium|Edge|Brave`. Slow (seconds) — cache the result in `~/Library/Application Support/brow/browsers.json` keyed by mtime of `/Applications`.

Weighting from chrome-launcher (`CHROME_PATH`:151, `LIGHTHOUSE_CHROMIUM_PATH`:150, `/Applications/*Canary*`:101, `/Applications/*Chrome.app`:100, `~/Applications/*`:51/50, `/Volumes/*`:-1/-2 — **negative weights for `/Volumes` because that's a mounted DMG, not an install**). Copy that: never launch a browser from `/Volumes`.

### Linux

1. `$BROW_CHROME_PATH`, `$CHROME_PATH`
2. `which`/`PATH`: `google-chrome-stable`, `google-chrome`, `chromium-browser`, `chromium`, `microsoft-edge-stable`, `brave-browser`
3. Desktop-entry scan: `~/.local/share/applications/*.desktop`, `/usr/share/applications/*.desktop`, parse `Exec=`
4. Well-known: `/opt/google/chrome/chrome`, `/usr/bin/chromium`, `/snap/bin/chromium`, `/var/lib/flatpak/exports/bin/com.google.Chrome`
5. **Snap/Flatpak caveat:** snap-confined Chromium cannot open a `--user-data-dir` outside `$HOME` and its fd inheritance through the snap wrapper is unreliable. Detect (`readlink -f` resolves into `/snap/`) and warn loudly rather than half-working.

### Windows

1. `%BROW_CHROME_PATH%`, `%CHROME_PATH%`
2. Registry (authoritative): `HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe` → `(Default)`, then `HKLM\…\App Paths\chrome.exe`; same for `msedge.exe`, `brave.exe`. Also `HKLM\SOFTWARE\Google\Chrome\BLBeacon` → `version` for presence detection.
3. Path probing over `%LOCALAPPDATA%`, `%PROGRAMFILES%`, `%PROGRAMFILES(X86)%`, plus hardcoded `C:\Program Files`, `C:\Program Files (x86)`, `D:\…` fallbacks (exactly what Puppeteer does): `\Google\Chrome\Application\chrome.exe`, `\Google\Chrome SxS\Application\chrome.exe`, `\Microsoft\Edge\Application\msedge.exe`, `\BraveSoftware\Brave-Browser\Application\brave.exe`.

### When nothing is found

Fail with an actionable error, never download:

```
brow: no Chromium-family browser found.
Looked in: <n> locations (run `brow doctor --verbose` for the list).
Fix one of:
  • install Google Chrome from https://google.com/chrome
  • export BROW_CHROME_PATH=/path/to/chrome
  • brow config set browser.path /path/to/chrome
```

Validate a candidate before trusting it: `exec($path, "--version")` with a 5 s timeout and parse `^(Google Chrome|Chromium|Microsoft Edge|Brave Browser) (\d+)\.` — reject anything below a configured minimum milestone (suggest **≥ 128**, so that `--headless=new`, flat auto-attach filters and modern `Emulation`/`Input` params are all present; hard-fail below 136 would be defensible too given the user-data-dir rule).

---

## 6. Launch flags, 2026

### 6.1 Headless status

* `--headless=old` was removed in Chrome 132; old headless now lives only as the separate `chrome-headless-shell` binary ([developer.chrome.com/blog/removing-headless-old-from-chrome](https://developer.chrome.com/blog/removing-headless-old-from-chrome), [chrome-headless-shell](https://developer.chrome.com/blog/chrome-headless-shell)).
* `--headless` == `--headless=new` from 132 on. chrome-launcher's flag doc lists `--headless=new` as *"unnecessary from Chrome 132"*.
* **Observed anomaly:** on Chrome 151, `--headless=old --dump-dom about:blank` did **not** error — it printed `<html><head></head><body></body></html>`, i.e. it ran (presumably silently mapped to new headless). Don't rely on either behaviour; just pass `--headless=new` (harmless, explicit) or nothing for headful.

  > **Verified 2026-08-04:** re-tested on the *full* `--remote-debugging-pipe` path, not just `--dump-dom`. Launching with `--headless=old` + pipe: `Browser.getVersion` succeeded, `Target.createTarget` + `Runtime.evaluate` worked, `navigator.userAgent` = `…HeadlessChrome/151.0.0.0…`, `navigator.webdriver` = `true` (i.e. it behaves exactly like new headless over a pipe). So the flag is **accepted and silently mapped**, not rejected — the removal blog post's "prints a helpful error" is stale for 151. The guidance stands regardless: never pass it.
* **For `brow` the default should be headful**, not headless: the spec demands real user gestures, correct paint order, sticky/fixed screenshot fidelity, device scale factor, and real compositing. New headless is much closer to headful than old headless was, but headful is the ground truth and costs only ~80 ms more startup here. Offer `--headless` for background jobs where no visual fidelity claim is made.

### 6.2 The recommended `brow` flag set

Derived from [chrome-launcher DEFAULT_FLAGS](https://raw.githubusercontent.com/GoogleChrome/chrome-launcher/main/src/flags.ts), [chrome-flags-for-tools.md](https://raw.githubusercontent.com/GoogleChrome/chrome-launcher/main/docs/chrome-flags-for-tools.md) (the maintained list), [Playwright chromiumSwitches.ts](https://raw.githubusercontent.com/microsoft/playwright/main/packages/playwright-core/src/server/chromium/chromiumSwitches.ts) and [Puppeteer ChromeLauncher.ts](https://raw.githubusercontent.com/puppeteer/puppeteer/main/packages/puppeteer-core/src/node/ChromeLauncher.ts).

**Mandatory**
```
--remote-debugging-pipe
--user-data-dir=<abs path, never Chrome's default>
--no-first-run
--no-default-browser-check
--no-startup-window            # we create every target explicitly via Target.createTarget
```

**Quiet the profile / kill background network chatter**
```
--disable-background-networking
--disable-component-update
--disable-client-side-phishing-detection
--disable-domain-reliability
--disable-sync
--metrics-recording-only
--no-pings
--disable-breakpad
--disable-crash-reporter
--disable-default-apps
--disable-search-engine-choice-screen
--password-store=basic
--use-mock-keychain             # macOS: prevents Keychain prompts. Aligns with "Keychain out of scope"
--propagate-iph-for-testing
--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,\
CalculateNativeWinOcclusion,InterestFeedContentSuggestions,CertificateTransparencyComponentUpdater,\
AutofillServerCommunication,PrivacySandboxSettings4,DestroyProfileOnBrowserClose,GlobalMediaControls,\
LensOverlay,AcceptCHFrame
```

**Determinism / don't-throttle-my-automation**
```
--disable-background-timer-throttling
--disable-backgrounding-occluded-windows
--disable-renderer-backgrounding
--disable-ipc-flooding-protection
--disable-hang-monitor              # see caveat below
--disable-prompt-on-repost
--disable-popup-blocking            # window.open must reach us as a new target
--force-color-profile=srgb          # required for stable image diffs
--allow-pre-commit-input            # input events accepted before first paint commit
--export-tagged-pdf
```

**Situational**
```
--disable-dev-shm-usage             # Linux containers only; harmful/pointless on macOS
--hide-scrollbars --mute-audio      # headless jobs only; NOT for screenshot fidelity work
--disable-extensions                # default on; the spec has no extension story yet
--enable-features=CDPScreenshotNewSurface   # Playwright's 2026 screenshot path; validate before adopting
```

### 6.3 Flags that are obsolete, harmful, or a trap in 2026

| Flag | Verdict |
|---|---|
| `--headless=old` | Removed in 132; use `chrome-headless-shell` if you truly need it |
| `--disable-infobars` | Removed May 2019; Playwright still passes it but only as a Chrome-for-Testing infobar hack |
| `--disable-translate` | Removed 2017 → `--disable-features=Translate` |
| `--disable-save-password-bubble` | Removed 2016 |
| `--safebrowsing-disable-auto-update` | Removed 2017 |
| `--ignore-autoplay-restrictions` | Removed 2017 → `--autoplay-policy=no-user-gesture-required` |
| `--no-sandbox` | **Harmful.** You are pointing a browser at arbitrary sites on the user's laptop. Puppeteer gates it behind `PUPPETEER_DANGEROUS_NO_SANDBOX=true`. Never default it; on Linux, if user namespaces are unavailable, fail with instructions rather than silently disabling the sandbox |
| `--single-process` | Breaks OOPIFs, breaks crash isolation, unsupported. Never |
| `--disable-web-security` | Chrome now *strips it* unless `--user-data-dir` is non-default (`chrome_main_delegate.cc` logs "Web security may only be disabled if '--user-data-dir' is also specified with a non-default value"). Still: don't |
| `--disable-features=IsolateOrigins,site-per-process` | Kills OOPIF targets and the main renderer mitigation. Never (see §3.4) |
| `--disable-gpu` | Cargo-culted from old headless on Windows. On macOS 2026 it degrades compositing and screenshot fidelity. Don't |
| `--disable-hang-monitor` | Genuinely useful (no "page unresponsive" dialog stealing focus) **but** it also means Chrome will not proactively kill a wedged renderer — you own hang detection (§8.4) |
| `--enable-automation` | See below |
| `--disable-back-forward-cache` | Playwright sets it so `goBack()` re-issues the main request. For a *site mapper* you probably want bfcache **on**, because bfcache restores are a real state transition worth recording. Decide deliberately |

### 6.4 `--enable-automation` — measured, and it does less than folklore says

I probed `navigator.webdriver` in Chrome 151 headless via the pipe:

| Launch | `navigator.webdriver` |
|---|---|
| plain (`--headless=new`, no automation flag) | **`true`** |
| `+ --enable-automation` | `true` |
| `+ --disable-blink-features=AutomationControlled` | **`false`** |

> **Corrected 2026-08-04 — this was measured wrong, and dossier `40-input-synthesis.md` measured the opposite. Both were right about their own transport and wrong to generalise. The trigger is `--remote-debugging-pipe`, not "CDP".** Full matrix on Chrome 151.0.7922.72, macOS 26.5.1:
>
> | Launch | transport | `navigator.webdriver` |
> |---|---|---|
> | headless, no debugging switch at all (`--dump-dom` control) | none | **`false`** |
> | headless `--remote-debugging-port` | websocket | **`false`** |
> | headful `--remote-debugging-port` | websocket | **`false`** |
> | headless `--remote-debugging-port --enable-automation` | websocket | **`true`** |
> | headless `--remote-debugging-pipe` | **pipe** | **`true`** |
> | headful `--remote-debugging-pipe` | **pipe** | **`true`** |
> | headless `--remote-debugging-pipe --enable-automation` | pipe | `true` (no change) |
> | headless `--remote-debugging-pipe --disable-blink-features=AutomationControlled` | pipe | `false` |
>
> So: headless alone does **not** set it; websocket CDP does **not** set it; `--enable-automation` **does** set it (contradicting this section's original claim); and **`--remote-debugging-pipe` sets it unconditionally** (contradicting `40-input-synthesis.md` §11).
>
> **Consequence for `brow`, and it is not small.** Our chosen transport is the pipe, so **every page `brow` ever touches sees `navigator.webdriver === true`**, in headful, with no way to turn it off short of `--disable-blink-features=AutomationControlled` (an evasion flag we have decided not to ship). The recommendation below — "don't pass `--enable-automation`" — is still right but is now *cosmetic*: it changes the infobar and the password-save UI, not the fingerprint. Any doc text implying `brow` is indistinguishable on this axis must be removed, and `SKILL.md` should state plainly: *sites that gate on `navigator.webdriver` will treat every `brow` session as automation.* If a job genuinely needs `webdriver === false` (testing a code path that is disabled under automation), the only honest options are (a) run that job over `--remote-debugging-port` on loopback behind an explicit opt-in, accepting the local-process exposure, or (b) accept the flag.

Per the chrome-launcher docs, `--enable-automation` additionally suppresses the password-save UI and some infobars, and shows the "Chrome is being controlled by automated test software" bar in headful; web-platform-tests deliberately avoids it for authenticity.

**Recommendation for `brow`:** do **not** pass `--enable-automation` by default (headful users don't need the yellow bar; we already suppress password UI via `--password-store=basic`). Do **not** pass `--disable-blink-features=AutomationControlled` either — that is an evasion measure, it makes the harness lie about itself, and given the "no cloud, local-first, honest coverage reporting" ethos it should be an explicit, logged, per-job opt-in at most.

---

## 7. Startup handshake over a pipe

There is no `/json/version`, no `DevToolsActivePort`. The handshake is:

1. `spawn` with fds 3/4 wired and `setsid()`.
2. Immediately start the read loop (Chrome may emit nothing at all until you ask).
3. Send `{"id":1,"method":"Browser.getVersion"}`. **The first response is the readiness signal.** Measured: 0.19 s headless / 0.27 s headful.
4. Optionally `Target.getBrowserContexts` → gives you `defaultBrowserContextId` (observed: `{"browserContextIds":[],"defaultBrowserContextId":"3B8D…"}`) which you need for context-scoped calls against the default profile.
5. `Target.setDiscoverTargets{discover:true, filter:[{}]}` then `Target.setAutoAttach{autoAttach:true, waitForDebuggerOnStart:true, flatten:true, filter:[…]}`.

Timeout policy:
* 10 s hard cap on step 3. On timeout: kill the process group, capture stderr (Chrome writes `[pid:tid:date:ERROR:file] msg` lines there) and surface it. The two failure modes you will actually see are `Remote debugging pipe file descriptors are not open.` (fd plumbing bug) and a profile lock conflict.
* Do not parse Chrome's stderr for readiness (that's the port-mode `DevTools listening on ws://…` trick) — it doesn't exist in pipe mode.
* Chrome writes a lot of benign `ERROR:` noise to stderr (GCM registration, `task_policy_set`, `GPU process exited unexpectedly: exit_code=15` during shutdown — all observed). Log at debug, don't treat as fatal.

---

## 8. Failure modes and recovery

### 8.1 Pipe close ⇒ browser exit (the load-bearing fact)

**Observed:** closing the parent's write end of the fd-3 pipe caused Chrome to exit with `rc=0` within **0.5 s**. Source path: `PipeReaderBase::ReadBytes` sees `read()<=0` → `DevToolsPipeHandler::OnDisconnect` → the embedder's `on_disconnect_` closure → browser shutdown.

> **Verified 2026-08-04 (independent reproduction, tighter number):** `os.close(write_fd)` → `waitpid` reaped the browser with `rc=0` in **0.053 s** (headless, Chrome 151, pipe launch). The mechanism and the exit code are confirmed; it is an order of magnitude faster than the 0.5 s originally recorded, so any shutdown grace period can be short.

Also observed: `Browser.close` over the pipe returns `{}` and the process exits `rc=0` cleanly (beforeunload handlers skipped for disposed contexts, per `disposeBrowserContext` docs).

Consequences:
* **Orphan Chromes are structurally impossible** as long as somebody holds fd 3. That is a big win over port mode, where a crashed daemon leaves a fully-open debuggable browser behind.
* **A `browserd` restart kills every browser** unless the fds live somewhere else. Hence the supervisor design.

### 8.2 The supervisor design (recommended)

```
launchd/systemd ──> browserd  (stateless-ish; restartable; owns policy, jobs, artifacts)
                       │  unix socket, 0600, SO_PEERCRED/LOCAL_PEERCRED uid check
                       ▼
                  brow-supervisor <id>   (tiny, setsid(), never restarted by the daemon)
                       │  fd3/fd4 NUL-JSON
                       ▼
                    Chromium
```

* `brow-supervisor` is ~300 lines: hold the pipe, accept exactly one unix-socket client at a time, forward frames verbatim in both directions, keep a bounded replay buffer of the last N browser-scoped events so a reconnecting `browserd` can resync target state, and exit (closing the pipe, killing Chrome) if no client reconnects within a configurable grace period (default: never — that's what "persistent background Chromium" means; make it explicit).
* Because the supervisor is the *only* thing that ever touches raw CDP framing, the "raw CDP is never exposed to the agent" invariant is enforced by process boundary, not just by code discipline.
* Socket path `$XDG_RUNTIME_DIR/brow/<instance>.sock` on Linux, `~/Library/Application Support/brow/run/<instance>.sock` on macOS (macOS has no `XDG_RUNTIME_DIR`; `/tmp` is world-readable — do not use it). `chmod 0600`, and verify the peer uid with `LOCAL_PEERCRED` (macOS `getsockopt(SOL_LOCAL, LOCAL_PEERCRED)`) / `SO_PEERCRED` (Linux). **UNVERIFIED**: I did not build this; the CDP-side behaviour it depends on is verified.

> **Verified 2026-08-04 — the `setsid()` survival question is now answered, and the two platforms differ. This was the single most load-bearing unknown in the document.**
>
> **macOS: the design works, empirically.** I installed a real LaunchAgent (`launchctl bootstrap gui/$UID`) whose job spawned two grandchildren — one that called `os.setsid()` (new pgid) and one that did not (same pgid as the job). Then `launchctl bootout gui/$UID/<label>`:
> ```
> before:  job 22414 (pgid 22414) | A 22416 (pgid 22416, setsid) | B 22417 (pgid 22414)
> after:   job 22414 DEAD          | A 22416 ALIVE               | B 22417 DEAD
> ```
> That matches the documented rule in `man 5 launchd.plist` verbatim: *"When a job dies, launchd kills any remaining processes with the same process group ID as the job. Setting this key [`AbandonProcessGroup`] to true disables that behavior."* So the reaping is **PGID-based**, `setsid()` escapes it, and `AbandonProcessGroup` is **not** required (it is a belt-and-braces addition, harmless to set). `KeepAlive`/`ExitTimeOut` do not enter into it. **`brow-supervisor` must call `setsid()` — that one syscall is what makes "persistent background Chromium" true on macOS.**
>
> **Linux: `setsid()` is NOT sufficient, and the doc must say so.** `systemd.kill(5)`, `KillMode=` (fetched from the systemd source-of-truth man page): *"Defaults to `control-group`. If set to `control-group`, **all remaining processes in the control group of this unit will be killed** on unit stop."* Process groups are irrelevant to cgroup membership, so a `setsid()`ed supervisor inside `browserd.service`'s cgroup is killed with the unit. systemd also explicitly discourages the obvious workarounds: *"it is not recommended to set `KillMode=` to `process` or even `none`, as this allows processes to escape the service manager's lifecycle and resource management."*
>
> **The correct Linux mechanism is a separate transient unit, not a kill-mode tweak:** have `browserd` launch each supervisor via `systemd-run --user --scope --unit=brow-sup-<id> -- brow-supervisor <id>` (or the equivalent D-Bus `StartTransientUnit` call, which avoids the `systemd-run` binary dependency). A transient **scope** is its own unit with its own cgroup, so restarting or stopping `browserd.service` does not touch it; `browserd` then reconnects over the unix socket exactly as on macOS, and `systemctl --user stop brow-sup-<id>` is the clean teardown. Fallback if `systemd-run` is unavailable (containers, non-systemd distros): double-fork + `setsid()` and accept that a cgroup-wide kill will take the browser with it — degrade loudly, do not pretend.
>
> **Still UNVERIFIED:** I have no Linux box, so the `systemd-run --scope` recipe is read from the man pages, not executed. Prototype it before writing the Linux packaging. Windows service semantics (job objects, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) are also untested and have the same shape of risk — a Windows job object kills the whole tree regardless of process groups, so the supervisor there must be created with `CREATE_BREAKAWAY_FROM_JOB` or live in its own job.
* On Windows, the equivalent is a named pipe `\\.\pipe\brow-<instance>` with a DACL restricted to the current user SID.

Fallback if the owner rejects the extra process: accept that browsers restart with the daemon, and persist enough state (per-job URL list, context cookie exports, scroll/route position) to re-establish. Simpler, loses live page state, breaks long-running `job start --detached` jobs across daemon upgrades.

### 8.3 Crash handling

`Page.crash` (a real CDP command, useful for tests) produced **both**:
```json
{"method":"Inspector.targetCrashed","params":{},"sessionId":"8CA1…"}
{"method":"Target.targetCrashed","params":{"targetId":"B281…","status":"crashed","errorCode":5}}
```
Recovery: the page target survives as a sad tab. `Page.reload` or `Target.closeTarget` + recreate. All `@node-*` refs bound to that document generation must be invalidated on `targetCrashed` exactly as on navigation.

Browser process death (SIGKILL, OOM): the supervisor sees EOF on fd 4 → mark instance dead → fail every pending request with `BrowserGone` → notify `browserd` over the socket → jobs move to `failed` (or `waiting_for_approval` if configured to ask before relaunching). Renderer/GPU/utility children are killed by the browser process on normal exit; if the browser is SIGKILLed, children are reparented. **Therefore: `setsid()` the browser and, on cleanup, `kill(-pgid, SIGTERM)` then `SIGKILL` after a grace period.** Never rely on `kill(pid)` alone.

### 8.4 Hung renderer — measured semantics

With a page spinning `while(Date.now()-t<9000){}`:

| Probe | Result |
|---|---|
| `Browser.getVersion` on the **browser session** | ok in **0.02 s** |
| `Runtime.evaluate` on the **spinning page session** | **no reply within 4 s** (eventually returned at ~9 s) |
| `Target.createTarget` (new page, other renderer) | ok |
| `Target.closeTarget{targetId}` on the spinning target | `{"success":true}` in **0.02 s** |

So the design writes itself:
* Heartbeat `Browser.getVersion` on the browser session every ~5 s → distinguishes "browser wedged" from "one renderer wedged".
* Per-session command deadlines (default 30 s; `Page.navigate`/`Runtime.evaluate` configurable). On expiry, mark the session `unresponsive`, surface it to the agent, and offer `Target.closeTarget` — which works even while the renderer spins.
* `Page.crash` is your integration-test primitive for the whole recovery path.
* Note: with `--disable-hang-monitor` Chrome will not show/kill on its own — this watchdog is not optional.

### 8.5 Orphan cleanup on daemon restart

Even though the pipe makes orphans structurally rare, be defensive:
1. State file `~/Library/Application Support/brow/run/instances.json`: `{instance_id, supervisor_pid, browser_pid, pgid, socket, user_data_dir, started_at, boot_id}`.
2. On daemon start, for each entry: `kill(pid, 0)` liveness + verify the process's start time / executable (`sysinfo` crate, or `proc_pidpath` on macOS) so a recycled PID isn't mistaken for ours. On Linux compare `/proc/<pid>/stat` field 22 (starttime) against the record.
3. If the socket connects → reattach. If the pid is dead → clean up the `user_data_dir` lock artifacts and the socket file.
4. If a browser pid is alive but its supervisor is gone → it is a genuine orphan (only possible after `SIGKILL -9` of the supervisor); `kill(-pgid, SIGTERM)`.
5. Chrome's own profile lock (`SingletonLock`/`SingletonSocket` symlinks in the user-data-dir, encoding `hostname-pid`) is a secondary signal; note that I did **not** observe those files in a headless `--user-data-dir` on macOS 151, so do not depend on them.

### 8.6 Reconnecting to an already-running browser

**Over a pipe: impossible.** The pipe has exactly one client — whoever holds fds 3/4 — and Chrome offers no way to hand it to a new process. There is no "attach a second pipe". Options:

| Approach | Verdict |
|---|---|
| Supervisor holds the pipe, `browserd` reconnects to the supervisor's unix socket | **Recommended.** Real reconnect, no listening TCP port |
| Pass the fds to a new `browserd` over the socket via `SCM_RIGHTS` | Works on Unix, elegant, but leaves a window where nobody holds them and requires a live handoff (not a crash-recovery path). **UNVERIFIED** |
| Also launch with `--remote-debugging-port=0`, reconnect via `DevToolsActivePort` | Verified that pipe+port coexist and both work. But it opens the browser to every local process for the entire session. Debug-only |
| Accept the restart | Simple, honest, loses page state |

---

## 9. Protocol versioning and Rust codegen

### 9.1 There is no runtime protocol descriptor over a pipe

Verified on Chrome 151:
* `Schema.getDomains` on the **browser session** → `-32601 'Schema.getDomains' wasn't found`.
* `Schema.getDomains` on a **page session** → works, but returns only `[{"name":"Inspector","version":"1.2"},{"name":"Memory","version":"1.2"},…]` — renderer-side domain *names* at a frozen version "1.2". Useless for capability negotiation. The domain itself is marked `deprecated` in the live `/json/protocol` dump.

  > **Verified 2026-08-04, and note the disagreement with `30-unified-page-tree.md`.** That dossier states *"`Schema.getDomains` has ALREADY been removed in Chrome 151"* — **that is wrong**; it was almost certainly called on the browser session, where it has never existed. On a page session it works fine. Re-measured today: `-32601` on the browser session, and on a page session **35** domains (not 21) all at version `"1.2"`, beginning `Inspector, Memory, Page, Emulation, Security, Network, Database, IndexedDB…`. The count is **not fixed** — it reflects which renderer-side agents have been instantiated in that session, so it varies with how many domains you have enabled. And the domain is present-but-`deprecated:true` in `/json/protocol` (57 domains, protocol `{major:1, minor:3}`), not removed. The *conclusion* is unchanged — it is useless for feature detection — but "removed" is a factual error that will mislead whoever writes the capability probe.
* The full descriptor is available **only** at `http://127.0.0.1:<port>/json/protocol` — which requires the port you don't want. (Local dump: 57 domains, `version {major:1, minor:3}`.)
* `Browser.getVersion` gives you `{protocolVersion:"1.3", product:"Chrome/151.0.7922.72", revision:"@2903d855…", jsVersion:"15.1.206.10"}`. `protocolVersion` has been "1.3" for years and is worthless for feature detection; **`product` (the milestone) is the only usable version signal.**

**Therefore: pin, and feature-gate by milestone number parsed from `product`.**

### 9.2 What to pin

Vendor into `protocol/`:
* `browser_protocol.json` (1.41 MB) and `js_protocol.json` (182 KB) from [ChromeDevTools/devtools-protocol](https://github.com/ChromeDevTools/devtools-protocol/tree/master/json) — rolled roughly daily; head at commit *"Roll protocol to r1672245"*, 2026-08-01. The npm mirror `devtools-protocol@0.0.1672245` encodes the same revision number, so `0.0.<chromium-revision>` is a clean pin identifier.
* Record the pinned revision + the Chromium milestone it corresponds to in `protocol/PINNED.toml`.

Note for the codegen author: upstream Chromium **has split the PDL**. `third_party/blink/public/devtools_protocol/browser_protocol.pdl` is now a 62-line index of `include domains/<Domain>.pdl` files. If you generate from PDL rather than JSON you must follow the includes. Generating from the **JSON** (which the devtools-protocol repo publishes already-merged) is simpler and is what `chromiumoxide` does.

### 9.3 Codegen sketch

```
crates/cdp-protocol/
  build.rs            # reads ../../protocol/*.json, emits OUT_DIR/cdp.rs
  src/lib.rs          # include!(concat!(env!("OUT_DIR"), "/cdp.rs"));
```

* Parse with `serde_json` 1.0.151; emit with `quote` 1.0.47 + `proc-macro2` 1.0.107 + `prettyplease` 0.3.0 (`syn` 3.0.3 only if you need to re-parse).
* One Rust module per domain (`pub mod target { … }`), `#[derive(Debug, Clone, Serialize, Deserialize)]`, `#[serde(rename_all = "camelCase")]`, `Option<T>` for `optional`, `#[serde(skip_serializing_if = "Option::is_none")]`.
* Emit a `trait Command { const METHOD: &'static str; type Response: DeserializeOwned; }` so the transport is type-safe end to end.
* **Emit stability metadata**: `pub const EXPERIMENTAL: bool` / `DEPRECATED: bool` per command and per parameter, and put experimental items behind a cargo feature `cdp-experimental` (default on — you need `Storage`, `CSS`, `Overlay`, `DOMSnapshot`, `Accessibility`, `Target.setAutoAttach.filter`, all of which are experimental; but the metadata lets `browserctl doctor` tell the user what's on thin ice).
* Bake generated code into the published crate for release builds (chromiumoxide's approach) so downstream builds don't need the generator.
* **Do not depend on `chromiumoxide` (0.9.1) itself** — it bundles a browser-handler/launcher layer. Its `chromiumoxide_pdl` generator is MIT/Apache and is a legitimate reference to read, but the constraint list forbids Playwright/Puppeteer/Selenium, not third-party CDP codegen. Writing ~600 lines of generator keeps the dependency graph clean and is the house style.

### 9.4 Version skew policy

| Situation | Behaviour |
|---|---|
| Local Chrome **newer** than pin | Fine by default. Unknown fields must not break deserialization: `#[serde(default)]` + **never** `deny_unknown_fields`. Unknown enum variants → `#[serde(other)] Unknown`. Unknown events → route to a generic `RawEvent{method, params}` sink, count them, expose in `brow doctor` |
| Local Chrome **older** than pin | A command may 404 with `-32601`. Do not pre-flight the whole protocol; instead maintain a small `capabilities` table populated lazily: first `-32601` for method M marks M unsupported for this browser instance, degrades the feature, and reports it as a coverage gap (which the site-mapper spec already wants: `blocked` edges with provenance) |
| Startup | Parse milestone from `Browser.getVersion.product`; if `< MIN_MILESTONE`, refuse with a clear message; if `< PINNED_MILESTONE`, log a warning listing known-missing features |
| CI | A `xtask check-protocol` that downloads the current `devtools-protocol` head, diffs against the pin, and fails on removed/renamed methods `brow` actually calls |

---

## 10. Rust crate stack (versions from crates.io, 2026-08-04)

| Crate | Version | Use |
|---|---|---|
| `tokio` | **1.53.1** | runtime; `net::unix::pipe::{Sender,Receiver}` (unix), `net::windows::named_pipe`, `net::UnixListener`, `process::Command` |
| `serde` / `serde_json` | **1.0.229** / **1.0.151** | CDP messages. `serde_json::value::RawValue` for zero-copy passthrough in the supervisor |
| `bytes` | **1.12.1** | `BytesMut` framing buffer |
| `memchr` | (latest) | NUL scanning |
| `tokio-util` | **0.7.19** | `codec` if you prefer a `Decoder` over a hand loop; `CancellationToken` |
| `nix` | **0.31.3** | `pipe()`, `setsid()`, `killpg()`, `LOCAL_PEERCRED` |
| `rustix` | **1.1.4** | alternative to `nix`, no libc-crate churn; pick one |
| `libc` | **0.2.189** | `dup2` in `pre_exec` |
| `thiserror` | **2.0.19** | error enums |
| `anyhow` | **1.0.104** | binaries only |
| `tracing` / `tracing-subscriber` | **0.1.44** / **0.3.23** | structured logs; per-session spans |
| `clap` | **4.6.5** | `browserctl` |
| `dashmap` | **6.2.1** | pending-request map (or `parking_lot::Mutex<HashMap>` **0.12.5** — measure) |
| `sysinfo` | **0.39.6** | orphan detection, process-start-time verification |
| `which` | **8.0.5** | Linux `PATH` browser discovery |
| `directories` | **6.0.0** | platform data/runtime dirs |
| `camino` | **1.2.5** | UTF-8 paths |
| `uuid` | **1.24.0** | instance/job ids |
| `quote`/`proc-macro2`/`prettyplease` | **1.0.47** / **1.0.107** / **0.3.0** | build-time codegen |
| `tempfile` | **3.27.0** | scratch profiles in tests |

Explicitly **not** used: `chromiumoxide` (0.9.1), `headless_chrome`, `fantoccini`, `thirtyfour`, `tungstenite`/`tokio-tungstenite` (0.30.0 — no websocket needed at all).

---

## What we verified empirically

All against **Google Chrome 151.0.7922.72** (`V8 15.1.206.10`, revision `@2903d8558c752b5a554a1a47b4ea7219ba1a31ef`) on macOS Darwin 25.5.0, driven from a hand-written Python CDP client over `--remote-debugging-pipe` with scratch `--user-data-dir`s under `/private/tmp/browtest`. Everything started was killed.

1. **Pipe wire format.** NUL-delimited JSON on fd 3 (in) / fd 4 (out) works exactly as the source says. No length prefix, no newline.
2. **fd pre-flight check is real.** With fds 3/4 not open, Chrome refuses to start: `ERROR:chrome/app/chrome_main_delegate.cc:1101] Remote debugging pipe file descriptors are not open.`
3. **No HTTP endpoint in pipe mode.** `DevToolsActivePort` absent. With `--remote-debugging-port=0` it appears containing `62160\n/devtools/browser/7374a7be-1344-441e-aed0-f8aa3d01d230`.
4. **Pipe + port coexist.** Both channels served the same browser simultaneously.
5. **Startup latency to first `Browser.getVersion`:** headless **0.19 s**, headful **0.27 s**.
6. **Closing the input pipe exits Chrome in ≤0.5 s with rc=0.** `Browser.close` likewise → rc=0.
7. **`Schema.getDomains`** → `-32601` on the browser session; on a page session returns 21 names at version "1.2". `/json/protocol` over HTTP shows 57 domains, protocol `1.3`, with `Schema` and `Console` marked deprecated and 40 domains marked experimental.
8. **Flat protocol.** `Target.setAutoAttach{flatten:true}` → `{}`; `Target.attachToTarget{flatten:false}` still returns a sessionId on 151 (non-flat not yet removed). Commands with `sessionId` in the envelope get the `sessionId` echoed on the reply.
9. **Id collision across sessions is safe.** Two concurrent `id:99` commands (browser + page session) both answered, distinguishable by echoed `sessionId`.
10. **Routing gotcha.** `Target.detachFromTarget{sessionId:S}` sent *inside* session S → `-32602 No session with given id`. Sent on the browser session → `{}`, then S returns `-32001 Session with given id not found.`
11. **OOPIF invisibility.** Parent session: `iframe.contentDocument === null` → `true`. Nested `Target.setAutoAttach` on the *page* session produced `attachedToTarget{type:"iframe", parentId, parentFrameId, waitingForDebugger:true}`, whose session evaluated `location.href` → `"https://example.com/"`.
12. **Target filters work as documented.** `Target.getTargets{filter:[{"type":"iframe"},{"exclude":true}]}` returned only the iframe. Default (no filter) hid `tab` targets; `filter:[{}]` revealed `{'service_worker':1,'background_page':1,'page':1,'tab':2}`. `background_page` is a type not present in content's `kType*` list.
13. **BrowserContext isolation is real.** Cookie + localStorage set in ctx1 invisible in ctx2 and in the default context; `Storage.getCookies{browserContextId}` correctly scoped; `Browser.grantPermissions{browserContextId}` accepted; `disposeBrowserContext` removed the target and killed its session.
14. **Memory/process cost.** Empty contexts ≈ free; ~+149 MB and +1 process per additional live page regardless of context; 5 pages in one context (+692 MB) ≈ 5 pages in 5 contexts (+661 MB); a bare browser with `--no-startup-window` ≈ 505 MB / 6 processes. (macOS `ps rss` double-counts shared pages — upper bound.)
15. **Crash signalling.** `Page.crash` → `Inspector.targetCrashed{}` on the session **and** `Target.targetCrashed{status:"crashed",errorCode:5}` on the browser session.
16. **Hung renderer.** Browser session answered in 0.02 s while a renderer spun for 9 s; the spinning session did not answer within 4 s; other targets unaffected; `Target.closeTarget` on the spinning target succeeded in 0.02 s.
17. **`navigator.webdriver`** is `true` on plain CDP-driven headless Chrome 151 with **no** `--enable-automation`; `--enable-automation` did not change it; `--disable-blink-features=AutomationControlled` set it to `false`.
18. **`--headless=old` on Chrome 151** did not error — it rendered and printed the DOM, contrary to the "prints a helpful error" note in the removal blog post. Don't rely on either.

---

## Limits and impossibilities — blunt

1. **You cannot have a browser that outlives the process holding the pipe.** "Persistent background Chromium" + `--remote-debugging-pipe` + a restartable `browserd` are three things you can only have two of, unless a separate long-lived supervisor owns the fds. **This is a required architectural component, not an optimisation.** Alternative (TCP port) violates the security posture.
2. **You cannot reconnect to a pipe-mode browser you did not launch.** No handshake, no discovery, no second client. Anything that "reconnects to an already-running browser" must go through *our* socket, or through a debugging TCP port.
3. **You cannot drive the user's real Chrome profile.** Chrome ≥136 ignores both remote-debugging switches against the default data directory. *(Corrected 2026-08-04: the follow-on claim that the encryption key is "bound to the data directory" is **not established on macOS** — the OSCrypt key is one app-wide Keychain item `svce="Chrome Safe Storage", acct="Chrome"`. See §1.5. The impossibility rests on the switch restriction, not on crypto.)* Logged-in state must be established inside a `brow`-owned profile by a human, once. This directly limits the "auth branches" ambition of the site mapper: the harness can *explore* an authenticated area only after a human hands it a session.
4. **You cannot enumerate the local Chrome's protocol at runtime over a pipe.** `Schema.getDomains` is a deprecated husk — *present but useless* (35 renderer domain names at a frozen version "1.2", count varying with which agents are instantiated), **not removed**. Feature detection is: parse the milestone, then discover `-32601` lazily. Any claim of "we validate the protocol against your browser at startup" would be false unless you also open an HTTP port.
4b. **Every page sees `navigator.webdriver === true`** because we use `--remote-debugging-pipe`, which sets it unconditionally (§6.4). Websocket mode does not. This is not something the harness can honestly opt out of.
5. **Auto-attach is not transitive.** A→B→C requires `setAutoAttach` on A's session and on B's session. Any code path that creates a session and forgets to re-arm auto-attach silently loses an entire subtree of the page — and it loses it *quietly*, which is the worst failure mode for a tool whose selling point is "complete page introspection". This must be an invariant enforced in one place (`on_attached()`), not a call site convention.
6. **`waitForDebuggerOnStart:true` is required for completeness and costs latency.** Without it you race the renderer and lose early network/console events on new frames. With it, every new target adds a round trip before it runs. There is no third option.
7. **BrowserContexts are not persistable.** Only the default context in the user-data-dir survives a restart. Per-job "logged-in isolated sandbox that survives" needs either a dedicated `--user-data-dir` browser (≈500 MB, 6 processes) or explicit cookie/storage export-import.
8. **100 MB hard message cap on the pipe — but only in the client→Chrome direction** (`kReceiveBufferSizeForDevTools`, applied solely to `PipeReaderASCIIZ`). Exceeding it kills the connection rather than erroring the command. **Corrected 2026-08-04:** responses are *not* capped — a 125.8 MB base64 `captureScreenshot` result transited the pipe intact with the browser and session healthy afterwards. So this is a constraint on large *commands* (`Runtime.evaluate.expression`, `addScriptToEvaluateOnNewDocument.source`, `dispatchDragEvent.data`, `Fetch.fulfillRequest.body`), not on screenshots or `getResponseBody`. Guard the write path at ~64 MB. Huge responses remain a *memory* problem for both processes (see the megapixel cap in `50-capture-*` §1.4) — just not a transport-kill one.
9. **Snap/Flatpak Chromium on Linux is a coin flip** for fd inheritance and out-of-`$HOME` user-data-dirs. Detect and refuse rather than produce mysterious failures.
10. **`--disable-hang-monitor` shifts hang responsibility to us.** There is no CDP event for "this renderer is wedged". Detection is timeout-based only; a renderer that is slow-but-alive is indistinguishable from one that is wedged, which means the harness will occasionally kill a page that would have recovered.
11. **`Target.exposeDevToolsProtocol`** exists and injects a `window.cdp` binding into a page. It is an outright violation of the "raw CDP is never exposed" invariant. It must be hard-blocked in the policy crate with a test that asserts the method string never appears in any allowed capability path.
12. Everything about the **supervisor process, `SCM_RIGHTS` fd handoff, Windows named-pipe transport, and Linux/systemd behaviour is UNVERIFIED** — I had only macOS + Chrome 151 to test against.

---

## Open questions for the owner

1. **Supervisor process: yes or no?** It is the only way to get "persistent Chromium" + "restartable daemon" + "no TCP port". It adds one tiny binary and one IPC hop. If no, which of the three do we give up?
2. **Headful by default?** I recommend yes (fidelity of gestures, paint, screenshots), with headless reserved for detached background jobs. That means a visible window on the user's desktop — acceptable? Off-screen positioning (`--window-position=-32000,-32000`) is a hack that breaks screenshots on macOS.
3. **Do we attach to `tab` targets in addition to `page` targets?** Needed for prerender/bfcache transitions in the site graph; adds a second identity layer to the model.
4. **bfcache on or off?** Playwright disables it for deterministic navigation interception. For a *site mapper*, bfcache restores are real transitions worth recording. Which wins?
5. **Minimum supported Chrome milestone?** I'd propose 136 (the user-data-dir rule is then unconditional) or 128 (broader compatibility, more feature-gating).
6. **Edge/Brave/Chromium support tier?** Discovery is easy; Brave injects Shields (breaks network expectations) and Edge injects `msForceBrowserSignIn`/updater behaviour. Tier-1 Chrome + Chromium, tier-2 Edge, tier-3 Brave?
7. **`--enable-automation`: off (my recommendation) or on?** Off means no "controlled by automated software" bar — some users find that *more* alarming, not less, from a transparency standpoint.
8. **Do we ever expose `--remote-debugging-port`,** even behind `brow doctor --unsafe`? It's genuinely useful for debugging the harness itself with real DevTools.
9. **CBOR mode?** Marked experimental in-source, but would remove JSON parse cost on very chatty sessions (DOM snapshots, tracing). Worth benchmarking later; not for v1.
10. **Named persistent profiles** (separate `--user-data-dir` browsers) as a first-class concept for logged-in sites, given that BrowserContexts cannot persist?

---

## Sources

1. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_pipe_handler.cc — pipe wire format, ASCIIZ/CBOR modes, 100 MB / 64 KiB constants, `MayAccessAllCookies`, `AllowUnsafeOperations`
2. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_pipe_handler.h — `ProtocolMode` enum with the "Experimental (!)" CBOR comment
3. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/public/browser/devtools_agent_host.h — `kReadFD = 3`, `kWriteFD = 4`
4. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/public/common/content_switches.cc — `kRemoteDebuggingPipe`, `kRemoteDebuggingPort`, `kRemoteDebuggingIoPipes`
5. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/components/devtools/devtools_pipe/devtools_pipe.cc — `AreFileDescriptorsOpen()`
6. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/chrome/app/chrome_main_delegate.cc — fd pre-flight check, `--disable-web-security` stripping
7. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_agent_host_impl.cc — exact target type strings
8. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/browser/devtools/devtools_http_handler.cc — websocket client privileges for comparison
9. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/third_party/blink/public/devtools_protocol/browser_protocol.pdl — new split-PDL index
10. https://chromium.googlesource.com/chromium/src/+/refs/heads/main/third_party/blink/public/devtools_protocol/domains/Target.pdl — full Target domain definition, default filter, experimental markers
11. https://chromedevtools.github.io/devtools-protocol/tot/Target/ — Target domain reference
12. https://chromedevtools.github.io/devtools-protocol/tot/Inspector/ — Inspector domain events
13. https://github.com/ChromeDevTools/devtools-protocol/tree/master/json — pinnable `browser_protocol.json` / `js_protocol.json`; head `r1672245`, 2026-08-01
14. https://registry.npmjs.org/devtools-protocol — `0.0.1672245`, published 2026-08-01
15. https://developer.chrome.com/blog/remote-debugging-port — Chrome 136 default-data-dir restriction
16. https://developer.chrome.com/blog/removing-headless-old-from-chrome — `--headless=old` removed in 132
17. https://developer.chrome.com/blog/chrome-headless-shell — `chrome-headless-shell` replacement binary
18. https://raw.githubusercontent.com/GoogleChrome/chrome-launcher/main/src/flags.ts — `DEFAULT_FLAGS`
19. https://raw.githubusercontent.com/GoogleChrome/chrome-launcher/main/docs/chrome-flags-for-tools.md — maintained flag list, obsolete flags, `--enable-automation` notes
20. https://raw.githubusercontent.com/GoogleChrome/chrome-launcher/main/src/chrome-finder.ts — macOS/Linux/Windows discovery + priority weights
21. https://raw.githubusercontent.com/microsoft/playwright/main/packages/playwright-core/src/server/chromium/chromiumSwitches.ts — 2026 Playwright switch set with rationale comments
22. https://raw.githubusercontent.com/microsoft/playwright/main/packages/playwright-core/src/server/chromium/chromium.ts — pipe-by-default, `--no-startup-window`, headless extras
23. https://raw.githubusercontent.com/puppeteer/puppeteer/main/packages/puppeteer-core/src/node/ChromeLauncher.ts — `defaultArgs`, feature toggles, sandbox gating
24. https://raw.githubusercontent.com/puppeteer/puppeteer/main/packages/browsers/src/browser-data/chrome.ts — system executable paths per platform
25. https://docs.rs/tokio/latest/tokio/net/unix/pipe/struct.Receiver.html and .../struct.Sender.html — `from_owned_fd`, `from_file_unchecked`, `into_nonblocking_fd`
26. https://github.com/ChromeDevTools/chrome-devtools-mcp/issues/703 — cross-origin iframe / `setAutoAttach` discussion (2026)
27. https://github.com/chromedp/chromedp/issues/1607 — `--remote-debugging-pipe` support discussion, pipe rationale
28. https://crates.io/api/v1/crates/{tokio,serde,serde_json,…} — crate versions as of 2026-08-04

---

## Verification pass — 2026-08-04 (adversarial review)

Re-tested against **Google Chrome 151.0.7922.72** on macOS 26.5.1 (Darwin 25.5.0), driven by a hand-written Python pipe client (fd 3/4, NUL-JSON) plus a stdlib WebSocket client for port-mode comparisons. Scratch profiles under `/private/tmp/browver`; every process launched was killed.

| Claim under test | Outcome | Evidence |
|---|---|---|
| `setsid()`-detached supervisor survives the service manager | **CONFIRMED on macOS / REFUTED on Linux** | Real LaunchAgent + `launchctl bootout`: setsid'd grandchild ALIVE, same-pgid grandchild DEAD. `man 5 launchd.plist` says reaping is PGID-based. `systemd.kill(5)`: `KillMode=control-group` (default) kills *all processes in the cgroup* — use `systemd-run --user --scope` instead (§8.2) |
| Windows `--remote-debugging-io-pipes` = inheritable HANDLE values | **CONFIRMED** | Found the consumer: `AdoptPipes`/`AdoptHandle` in `devtools_agent_host_impl.cc` — `base::StringToUint` → `Uint32ToHandle` → `GetFileType(...) == FILE_TYPE_PIPE` → `_open_osfhandle` (§1.2) |
| 100 MB cap threatens screenshot **responses** | **REFUTED** | 125,834,904-byte base64 `captureScreenshot` transited intact, browser healthy. Cap is reader-only; a 110 MB *command* produced `Connection closed, not enough capacity` and killed the browser (§1.1) |
| `navigator.webdriver` is set by "being driven by CDP" | **REFUTED / it is the pipe** | port mode → `false`; pipe mode → `true`; `--enable-automation` → `true` in port mode; no-CDP control → `false` (§6.4). Also refutes `40-input-synthesis.md` §11 |
| `Schema.getDomains` removed in Chrome 151 (claimed in `30-…`) | **REFUTED** | Works on a page session: 35 domains at version "1.2"; `deprecated:true` in `/json/protocol` (§9.1) |
| Copied profile undecryptable because the key is per-data-directory | **PARTIAL / unsupported on macOS** | `security find-generic-password -s "Chrome Safe Storage"` → one app-wide item (`acct="Chrome"`). App-Bound Encryption is Windows-only (§1.5) |
| Chrome 136+ ignores both switches against the default data dir | **CONFIRMED** | Blog re-fetched; wording quoted verbatim in §1.5 |
| Pipe close → browser exit rc=0 in ≤0.5 s | **CONFIRMED (faster)** | 0.053 s, rc=0 (§8.1) |
| `--headless=old` accepted on 151 | **CONFIRMED, and generalises** | Full pipe launch: `Browser.getVersion` ok, page created, UA `HeadlessChrome/151.0.0.0` (§6.1) |
| `kReceiveBufferSizeForDevTools = 100 MB`, `kWritePacketSize = 64 KiB`, `kReadFD=3/kWriteFD=4` | **CONFIRMED** | Re-read from `devtools_pipe_handler.cc` and `devtools_agent_host.h` on `main` |
| CBOR mode has the same 100 MB inbound cap | **REFUTED** | `PipeReaderCBOR` never calls `set_max_buffer_size`; it resizes to the envelope length. Unbounded (§1.1) |

**Not re-tested** (still carrying the original document's confidence): BrowserContext memory figures, target-filter behaviour, hung-renderer timings, crash signalling, id-collision safety, Snap/Flatpak, `SCM_RIGHTS` handoff, `tab`-target prerender/bfcache semantics.
