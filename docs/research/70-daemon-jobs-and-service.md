# browserd: daemon architecture, IPC protocol, job engine, OS service integration

> **Bottom line.** One daemon per user (not per project), one Chromium process per *session*, one `Target.createBrowserContext` per *job* inside it. Keep `--remote-debugging-pipe`: I verified that it makes Chrome die within one second of the daemon being `SIGKILL`ed — ~~Chrome can never be orphaned~~ **[corrected 2026-08-04: Chrome is orphan-free only if no other process holds a duplicate of the fd-3 write end; `FD_CLOEXEC` on the CDP fds is a hard requirement, see §9.1]** — but it also means **no browser survives a daemon restart, so no job can truly "resume" after a crash**; design checkpointing that admits this instead of pretending. The IPC should stay newline-JSON but grow a v2 envelope with request ids, server-push `ev` frames for `--follow`, and a `cancel` op — JSON-RPC 2.0 buys nothing here because it has no correlated streams and you would reinvent LSP's `$/progress` anyway. Auto-start must move from the current `path.exists()` + unlink + bind sequence (I demonstrated the TOCTOU race is real) to `flock(LOCK_EX|LOCK_NB)` on a lock file held for the daemon's lifetime. Ship a launchd LaunchAgent on macOS (verified: `launchctl bootstrap gui/502` works, re-bootstrap fails with exit 5), a `systemd --user` unit plus `loginctl enable-linger` on Linux, and a Task Scheduler at-logon task on Windows — there is no per-user Windows service and pretending otherwise wastes a week. Finally, the big fork: **do not embed a model client in the daemon.** MCP sampling — the one standard mechanism for a server to borrow the host's LLM — was deprecated in protocol revision `2026-07-28`. The daemon should be a *durable deterministic executor* of agent-authored plans, parking at decision gates. That satisfies "local-first, no cloud API" by construction.

---

## Naming note

The brief says `browserd` / `browserctl`; the repo at `/Users/gmh-basket/pr/brow` (iteration 2, already building) ships `browd` / `brow` with `src/daemon.rs`, `src/ipc.rs`, `src/client.rs`, `src/paths.rs`. This document uses the **repo names** and maps to the planned crate split: `crates/browserd` ← `src/daemon.rs`, `crates/browserctl` ← `src/{main,cli,client}.rs`, new `crates/jobs`, `crates/artifacts`, `crates/policy`.

## Decisions

| Decision | Why | Rejected alternative | Confidence |
|---|---|---|---|
| One daemon **per user**, keyed by `$BROW_HOME` (default `~/.brow`) | Sessions/profiles/cookies are user-scoped; per-project daemons multiply Chromium RSS (measured 1.07 GB for one headless browser with 5 targets) | Per-project daemon (opt-in only via `BROW_HOME`) | confirmed |
| One **Chromium process per session**, one **BrowserContext per job** | `Target.disposeBrowserContext` reclaimed 5 renderers in <2 s and cost 0.05 s for 5 contexts; a whole browser per job costs ~250 MB browser process + helpers | Whole browser per job | confirmed |
| Keep `--remote-debugging-pipe` | Verified: daemon `SIGKILL` → entire Chrome tree gone in <1 s. No listening port. **Trigger is fd-3 EOF specifically; "no orphans" holds only under `FD_CLOEXEC` on the CDP fds (§9.1)** | `--remote-debugging-port` on loopback (survives daemon death — and is reachable by any local process) | confirmed, with the CLOEXEC precondition |
| Newline-JSON **v2 envelope** with `id` / `ev` / `end` / `cancel`, not JSON-RPC 2.0 | JSON-RPC has no correlated server-push stream; LSP had to bolt on `$/progress` + `$/cancelRequest`. A bespoke tagged enum also keeps "no request variant can carry a CDP method" a type-level property | JSON-RPC 2.0; length-prefixed LSP framing; gRPC | likely |
| `flock(LOCK_EX\|LOCK_NB)` on `run/browd.lock` as the single-instance gate | Verified: `bind()` over a live socket path gives `EADDRINUSE(48)`, and unlink-then-bind lets a second listener steal the path while the first still runs. `flock` gave `EAGAIN(35)` correctly | Socket-bind-as-lock; PID file with `O_EXCL` | confirmed |
| Peer check = `getpeereid`/`SO_PEERCRED` uid **plus** `UCred::pid()` for audit | tokio 1.53.1 `UCred::pid()` is implemented on macOS. uid is the real boundary; pid is only for logging | Shared-secret token file (adds no boundary above mode 0600) | confirmed |
| **Agent owns the LLM; daemon executes plans and parks at gates** | MCP sampling deprecated `2026-07-28` (SEP-2577); a daemon-embedded client either needs a cloud key (violates the constraint) or a local model (violates "no automatic download") | Daemon-embedded model client; pure agent-drives-daemon with no detached execution | confirmed |
| Pause a page with `Page.setWebLifecycleState{state:"frozen"}` — **but it is not a durable pause; see the correction in §5.3** | Reversible: timers froze, `"active"` restored them | `Emulation.setVirtualTimePolicy{policy:"pause"}` — verified **one-way**: `advance` ran ~8 800× real time | **downgraded to _partial_ — `Page.captureScreenshot` silently un-freezes it** |
| `--follow` = **daemon push**, not file tailing | Push can deliver the terminal frame, gives backpressure, survives log rotation, and carries events that never hit disk | inotify/kqueue tail of `events.jsonl` | likely |
| Durable job state in **SQLite (rusqlite 0.40.1, `bundled`, WAL)**; artifacts on the filesystem | Crash-atomic state transitions, cheap queries for `brow job ls`; blobs do not belong in a DB | JSON manifests only (no atomicity); redb 4.1.0 (fine, but no ad-hoc query for `doctor`) | likely |
| Jobs interrupted by a daemon restart go to a distinct terminal state `interrupted`, never silently "resumed" | Browser is provably gone (pipe EOF). Only *re-execution from a checkpoint* is honest | Auto-resume onto a new browser | confirmed |
| Idle exit with status **0** after `idle_timeout` (default 20 min, zero sessions and zero live jobs) | Composes with launchd `KeepAlive={SuccessfulExit:false}` — clean exit is not restarted; failure is | Always-on daemon | confirmed |

---

## 1. Process model

### 1.1 Topology

```
browd (1 per user)
 ├─ session "default"  → chrome #1 (pipe fds 3/4)
 │    ├─ BrowserContext A  ← job j-01H… (crawl)
 │    ├─ BrowserContext B  ← job j-01J… (record video)
 │    └─ default context   ← interactive `brow open/click/...`
 └─ session "qa"       → chrome #2  (separate profile dir, separate cookie jar)
```

`Target.createBrowserContext` is **not experimental**; all four of its parameters are (`disposeOnDetach`, `proxyServer`, `proxyBypassList`, `originsWithUniversalNetworkAccess`). `Target.createTarget(url, browserContextId?)` — `browserContextId` is itself marked experimental in Chrome 151's `/json/protocol`, which is worth a comment in code but has been stable for years in practice ([devtools-protocol Target](https://chromedevtools.github.io/devtools-protocol/tot/Target/)).

Do **not** pass `disposeOnDetach: true` for job contexts. The daemon holds the only CDP connection; if it briefly detaches (it should not, but bugs happen) the context would be destroyed under a running job. Dispose explicitly on job completion.

### 1.2 Memory, measured

On this machine (Chrome 151.0.7922.72, macOS 26.5.1, arm64), one `--headless=new` browser with 5 open targets across 3 browser contexts:

| process | RSS |
|---|---|
| browser | 233.8 MB |
| GPU | 98.8 MB |
| NetworkService | 85.8 MB |
| StorageService | 61.9 MB |
| renderers (×6) | 75–115 MB each |
| **total RSS** | **1 072 MB** |

> **Corrected 2026-08-04 — the RSS caveat is backwards, and the planning figure is too *low*, not too high.** The doc assumed RSS over-counts shared framework pages and therefore overstates real memory. Measured both ways on a fresh headless browser with 5 `BrowserContext`s / 5 targets (12 processes):
> ```
> sum RSS            : 1364 MB
> sum phys_footprint : 2030 MB     <- macOS "real" memory (task_info / footprint(8))
> ratio              : 0.67x       <- RSS is 33% BELOW phys_footprint
> ```
> `phys_footprint` is what macOS actually charges a process (it includes compressed pages and IOKit/GPU allocations that never appear in RSS). For the browser process alone: RSS ≈ 234 MB vs `phys_footprint: 99 MB` — so the direction *inverts* between the browser process and the tree as a whole, and the aggregate is what matters. **Use ~2 GB, not ~1 GB, as the per-session planning figure.** The `max_running_jobs=2` / `max_contexts_per_browser=6` defaults are therefore **not** over-conservative; if anything they are about right or slightly generous on a 16 GB machine. Sample `phys_footprint` (via `proc_pid_rusage`/`task_info`, or `sysinfo` + a correction factor), not RSS, in `limits.rs`.

RSS is *not* a safe planning proxy on macOS (see above); treat **~2 GB per session** with 5 contexts as the figure.

**`--renderer-process-limit=2` did not work.** With that flag set and 5 browser contexts open I counted 8 renderer processes. Chromium treats it as a soft hint tied to memory-pressure heuristics, not a hard cap. Conclusion: **there is no reliable browser-side memory ceiling.** Enforce it daemon-side:

> **Verified 2026-08-04 (independent re-run).** `--renderer-process-limit=2`, 5 `BrowserContext`s, 5 targets, `--headless=new`:
> `{"renderer": 7, "browser": 1, "gpu": 1, "utility:NetworkService": 1, "utility:StorageService": 1}`. **7 renderers against a limit of 2 — the flag is not honoured.** The author's own caveat ("one negative result is the weakest inference in the document") is now a second independent negative result on a different context/target arrangement. Treat it as settled for the macOS/headless configuration brow ships; still worth one Linux check before the docs make an absolute claim.

```rust
// crates/browserd/src/limits.rs
pub struct Ceilings {
    pub soft_bytes: u64,   // default 3 GiB: stop admitting new contexts
    pub hard_bytes: u64,   // default 5 GiB: park running jobs, reap idle contexts
    pub max_contexts_per_browser: usize, // default 6
    pub max_running_jobs: usize,         // default 2
}
```

Sample with `sysinfo = "0.39.6"` every 5 s over the browser's process group (`Process::parent()` chain or, better, the pgid we set at spawn). On soft breach: refuse `job start` with a typed error and call `Memory.forciblyPurgeJavaScriptMemory` (EXPERIMENTAL, `Memory` domain is experimental as a whole) on idle contexts. On hard breach: transition the largest job to `paused` with `reason: "memory_ceiling"`.

On Linux only, you can get a real ceiling by launching Chrome inside a transient scope: `systemd-run --user --scope -p MemoryMax=3G -- chrome …`. macOS has no cgroup equivalent; `ProcessType=Background` in launchd applies I/O and CPU throttling, not a memory cap. Say this in the docs rather than shipping a `--memory-limit` flag that silently does nothing on macOS.

### 1.3 Concurrency and queueing

Single global queue, FIFO with a priority bump for jobs whose `session` already has a warm browser (avoids a 1–2 s cold start). Admission control is `max_running_jobs` (default 2) **and** the memory ceiling **and** free-disk guard (§9.4). Everything else sits in `queued`. Interactive commands (`brow click`, `brow snapshot`) never queue — they bypass the job scheduler entirely and run against the session's default context.

### 1.4 Idle shutdown

```
idle = (sessions.is_empty() || all sessions older than session_ttl with no traffic)
       && jobs.none(|j| j.state.is_live())
       && no connected clients
```
After `idle_timeout` (default 20 min, `0` = never) call `Browser.close` on every session, then `exit(0)`. Exit code 0 is load-bearing: it is what tells launchd/systemd "this was deliberate, do not restart" (§4).

---

## 2. IPC protocol (v2)

### 2.1 Framing

Keep newline-delimited JSON over `SOCK_STREAM`. `serde_json` escapes embedded newlines so a line is exactly one message; this is already asserted by a test in `src/ipc.rs` (`requests_round_trip_as_single_lines`). Length-prefixed LSP framing buys nothing and makes `nc`-level debugging painful.

The v1 protocol in the repo is strictly one-request-one-response per connection turn, which cannot express `--follow`. v2 adds an envelope:

```jsonc
// client → daemon
{"v":2,"id":7,"op":"job.follow","p":{"job":"j-01JQ...","from_seq":0,"kinds":["log","state","approval"]}}
{"v":2,"op":"cancel","target":7}                       // no id: fire-and-forget
// daemon → client
{"v":2,"id":7,"ev":"state","seq":1,"t":1785862197.123,"d":{"from":"queued","to":"running"}}
{"v":2,"id":7,"ev":"log","seq":2,"t":1785862198.001,"d":{"level":"info","msg":"visited /pricing"}}
{"v":2,"id":7,"end":{"status":"ok","d":{"visited":41},"text":"41 routes"}}
{"v":2,"id":8,"end":{"status":"error","code":"stale_ref","message":"...","hint":"run `brow snapshot`"}}
```

Rules:
- Every request carries a client-chosen monotonic `id`. Responses and stream frames echo it. Multiple requests may be in flight on one connection (needed so `--follow` does not block a concurrent `job.stop`).
- A request terminates with exactly one `end` frame. Streams emit zero or more `ev` frames first.
- `seq` is per-`(job, stream)` and **persisted**, so `--follow --from-seq N` resumes exactly where a `Ctrl-C`'d CLI left off.
- `cancel` targets an in-flight `id`. Per LSP's rule, the cancelled request still produces an `end` frame, with `code: "cancelled"` — never leave an id hanging ([LSP 3.15 `$/cancelRequest`](https://microsoft.github.io/language-server-protocol/specifications/specification-3-15/)).
- Backpressure: the daemon holds a bounded `tokio::sync::mpsc` (capacity 1024) per stream. On overflow it drops and emits `{"ev":"dropped","d":{"count":N}}` — same honesty rule the event ring already uses in `src/page/events.rs`.

### 2.2 Why not JSON-RPC 2.0

| requirement | JSON-RPC 2.0 | verdict |
|---|---|---|
| request/response correlation | yes (`id`) | equal |
| **server push correlated to a request** | no — notifications have no `id` | must invent a progress token (this is exactly why LSP added `$/progress` in 3.15) |
| cancellation | no | must invent (`$/cancelRequest`) |
| batch | yes, and unwanted | negative |
| "no variant can name a CDP method" | `method` is a free string | **negative — this is disqualifying** |

The last row is the decisive one. `Request` being a closed Rust enum is what makes "the agent cannot issue raw CDP" a property of the type rather than a filter someone must remember to update; the repo already has a test guarding it (`no_request_variant_can_carry_a_cdp_method`). A JSON-RPC `method: String` reopens that hole. Keep the enum; borrow JSON-RPC's *error* discipline (stable string `code` + human `message` + actionable `hint`) and nothing else.

### 2.3 Handshake and version skew

The current `PROTOCOL_VERSION: u32` with strict equality is right in spirit but produces an unnecessary hard failure whenever someone `cargo install`s a new `brow` while a daemon is live. Negotiate a range:

```jsonc
// daemon → client, first line, unsolicited
{"v":2,"hello":{"brow":"0.3.0","build":"g1a2b3c4","pid":41231,
                "proto_min":2,"proto_max":3,
                "features":["jobs","follow","approvals","video"],
                "started_at":1785860000,"home":"/Users/x/.brow"}}
```

CLI picks `min(cli.proto_max, hello.proto_max)`; if that is `< max(cli.proto_min, hello.proto_min)` it fails loudly with the exact remedy:

```
brow: this CLI speaks protocol 3–4; the running browd 0.2.1 (pid 41231, up 3h) speaks 1–2.
      Run `brow daemon restart --if-idle`   (2 sessions and 1 running job would be lost)
```

`--if-idle` refuses when anything is live; `--force` does it anyway. Never auto-restart a daemon that owns live state.

### 2.4 Authentication — and the honest limit

Layers, in order of actual strength:

1. `~/.brow` and `~/.brow/run` are `0700`; the socket is `0600`. (Already implemented in `src/paths.rs::ensure_layout`.)
2. `getpeereid()` on macOS / `SO_PEERCRED` on Linux; reject `uid != getuid()`. (Already implemented in `src/daemon.rs::peer_uid`.) With tokio 1.53.1 you can drop the `libc` block entirely: `UnixStream::peer_cred() -> io::Result<UCred>` exposes `uid()`, `gid()` and `pid()`, and `pid()` **is** implemented on macOS ([docs.rs UCred](https://docs.rs/tokio/latest/tokio/net/unix/struct.UCred.html)).
3. Log `UCred::pid()` and the peer's executable path with every mutating request, into `~/.brow/logs/audit.jsonl`.

**Say this loudly in the README: the security boundary is the uid, not `brow`.** Any process running as you — a malicious `npm postinstall`, a compromised VS Code extension, another agent — can `connect()` to this socket and drive a browser holding your logged-in sessions. That is strictly more power than "read `~/.ssh`". Peer-executable allowlisting (`proc_pidpath` on macOS, `/proc/<pid>/exe` on Linux) is TOCTOU-racy against `exec` and trivially defeated by `LD_PRELOAD`/`DYLD_INSERT_LIBRARIES`; ship it as an audit signal, never as a gate. The only real mitigations are (a) `capability modes` enforced daemon-side so the blast radius of a hijacked socket is bounded by what the *session* was opened with, and (b) approval gates on dangerous actions (§8) that require a human even for a local caller.

---

## 3. Auto-start and single-instance

### 3.1 The race in the current code is real

`src/daemon.rs::bind()` does: `path.exists()` → try `connect()` → on failure `remove_file()` → `bind()`. I verified both halves of the problem:

- `bind()` over a path with a live listener returns `EADDRINUSE` (errno 48 on macOS) — so the unlink is genuinely required.
- After `unlink()`, a second `bind()` **succeeds while the first listener is still open and accepting**. New clients reach the second listener; the first is alive, holding browsers, and unreachable. Two `brow` invocations racing at cold start hit this.

### 3.2 The fix: flock, then bind

`flock(LOCK_EX|LOCK_NB)` on `~/.brow/run/browd.lock` gave `EAGAIN` (errno 35) to the second holder on macOS — real exclusion, and the kernel releases it automatically if the daemon is `SIGKILL`ed, so there is nothing stale to clean up.

```rust
// crates/browserd/src/single.rs   — fs4 = "1.1.0"
use fs4::fs_std::FileExt;

pub struct Instance { _lock: std::fs::File }

pub fn acquire(root: &Path) -> Result<Instance, StartError> {
    let lock = OpenOptions::new().create(true).read(true).write(true)
        .open(root.join("run/browd.lock"))?;
    lock.try_lock_exclusive().map_err(|_| StartError::AlreadyRunning)?;
    // We hold the lock: nothing else can be mid-bind. Now it is safe to clear
    // a socket left behind by a SIGKILLed daemon.
    let sock = root.join("run/brow.sock");
    let _ = std::fs::remove_file(&sock);
    // ... UnixListener::bind(&sock) ...
    // ... write run/browd.pid AFTER a successful bind ...
    Ok(Instance { _lock: lock })
}
```

Never `remove_file` the socket without holding the lock. `fs4 1.1.0` wraps `flock`/`LockFileEx` cross-platform.

### 3.3 Detached spawn

The existing `client::spawn_daemon` is already close to correct: `setsid()` in `pre_exec`, `stdin` null, stdout/stderr to `~/.brow/logs/browd.log`. Three fixes:

1. Also `chdir("/")` in `pre_exec`, so the daemon does not pin a deleted project directory.
2. Do not spawn a thread just to `wait()`. `setsid()` + immediate `_exit` of an intermediate is the classic double-fork; simpler here: `std::mem::forget(child)` and let init reap, or keep the thread — but on Linux add `PR_SET_PDEATHSIG` **deliberately unset** (we want survival), which is the default.
3. Race handling on spawn: if the child exits with `StartError::AlreadyRunning` (a distinct exit code, say 3), that is a *success* — another `brow` won the race. Retry `connect()` rather than reporting a failure. The current 15 s poll loop then does the right thing.

The spawn-vs-service interaction matters: if a LaunchAgent/systemd unit is installed, `brow` must not fork a second unmanaged daemon. Detect the service and delegate:

```rust
match service::installed()? {
    Service::Launchd { label } => run(["launchctl","kickstart","-p",&format!("gui/{uid}/{label}")]),
    Service::Systemd { unit }  => run(["systemctl","--user","start",&unit]),
    Service::None              => spawn_detached()?,
}
```

Precedent: `sccache` auto-spawns its server; `rust-analyzer` is spawned by the editor and never self-starts; `tmux` uses socket-as-lock; `docker` deliberately does *not* auto-start. `brow` should behave like `sccache` — invisible auto-start — because an agent turn cannot ask a human to run `systemctl`.

---

## 4. OS service integration

### 4.1 macOS — launchd LaunchAgent (empirically verified)

`~/Library/LaunchAgents/com.iatsuk.brow.browd.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.iatsuk.brow.browd</string>
  <key>ProgramArguments</key>
    <array><string>/Users/you/.cargo/bin/brow</string><string>daemon</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>ExitTimeOut</key><integer>20</integer>
  <key>StandardOutPath</key><string>/Users/you/.brow/logs/browd.out.log</string>
  <key>StandardErrorPath</key><string>/Users/you/.brow/logs/browd.err.log</string>
  <key>WorkingDirectory</key><string>/</string>
  <key>EnvironmentVariables</key><dict><key>BROW_LOG</key><string>info</string></dict>
</dict></plist>
```

Semantics, from `launchd.plist(5)` ([xcode man pages](https://keith.github.io/xcode-man-pages/launchd.plist.5.html)):

- `KeepAlive` dictionary keys are **ORed**. `SuccessfulExit=false` means "restart only if the exit status was non-zero" — exactly what pairs with our idle `exit(0)`.
- `ThrottleInterval` overrides the 10 s minimum respawn interval; leaving it at 10 is correct (a crash-looping daemon should not eat the CPU).
- `ProcessType=Background` applies "processes that do work that was not directly requested by the user" resource limits (I/O and CPU throttling). Note the trade-off: a background-classified daemon gets deprioritised, which will slow a video-recording job. If recording latency matters, use `Adaptive`. `Interactive` means "no limits". **`ProcessType` is not a memory cap.**
- `ExitTimeOut` is the SIGTERM→SIGKILL grace period; 20 s gives `Browser.close` time to flush profiles.
- `RunAtLoad=true` plus our own idle-exit is a slight contradiction — the daemon starts at login, sits idle 20 min, exits 0, and is then auto-started on demand by `brow`. If you want a truly on-demand agent, drop `RunAtLoad` and rely on §3.3 delegation, or use launchd **socket activation** (`Sockets` dict + `SockPathName` + `launch_activate_socket(3)`). Socket activation from Rust requires FFI to `launch_activate_socket`; there is no maintained crate as of 2026-08. Not worth it for v1.

Commands, all verified on this machine (macOS 26.5.1, uid 502):

```bash
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.iatsuk.brow.browd.plist   # exit 0
launchctl print     gui/$UID/com.iatsuk.brow.browd                               # state = running, pid = N
launchctl kickstart -p gui/$UID/com.iatsuk.brow.browd                            # start / restart
launchctl bootout   gui/$UID/com.iatsuk.brow.browd                               # exit 0
```

Observed details worth coding against:
- `launchctl print` reported `type = LaunchAgent`, `state = running`, `spawn type = background (5)` (from `ProcessType`), `properties = runatload | inferred program`.
- **A second `bootstrap` of an already-bootstrapped label fails: `Bootstrap failed: 5: Input/output error`, exit code 5.** The installer must be idempotent: `launchctl bootout … 2>/dev/null; launchctl bootstrap …`, or probe with `launchctl print` first.
- After `bootout`, `launchctl print` says `Could not find service … in domain for user gui: 502`. That is the clean uninstall check.
- `launchctl load/unload/start/stop` still work but are deprecated in favour of `bootstrap`/`bootout`/`kickstart` ([launchd.info](https://www.launchd.info/)).
- Since macOS 13, any plist dropped into `~/Library/LaunchAgents` appears in **System Settings → General → Login Items & Extensions** and triggers a "Background item added" notification; the user can toggle it off, which disables the agent without deleting the plist. `brow doctor` must therefore check *runtime* state via `launchctl print`, not merely that the plist file exists.
- `SMAppService` (macOS 13+) is the Apple-blessed modern path, but it requires a signed **app bundle**; a `cargo install`ed CLI cannot use it. LaunchAgent plist is correct here.

### 4.2 Linux — systemd `--user`

`~/.config/systemd/user/browd.service`:

```ini
[Unit]
Description=brow browser harness daemon
Documentation=https://github.com/iatsuk/brow

[Service]
Type=exec
ExecStart=%h/.cargo/bin/brow daemon serve --foreground
Restart=on-failure
RestartSec=2
TimeoutStopSec=20
KillMode=mixed
Environment=BROW_LOG=info
# Optional real memory ceiling — the thing macOS cannot do:
MemoryHigh=3G
MemoryMax=5G

[Install]
WantedBy=default.target
```

- `Type=exec` (not `simple`) makes systemd consider the unit started only after `execve()` succeeds, so a typo'd path fails at `systemctl start` instead of silently "succeeding" ([systemd.service(5)](https://man7.org/linux/man-pages/man5/systemd.service.5.html)).
- `Type=notify` + `sd-notify = "0.5.0"` is strictly better — `sd_notify(0, "READY=1")` after the socket is bound removes the CLI's poll loop for the service path. Worth doing in v2; requires `NotifyAccess=main` (the default for `Type=notify`).
- `Restart=on-failure` respects our `exit(0)` idle shutdown. `RestartSec` defaults to 100 ms, which would hammer; set 2 s.
- `KillMode=mixed` sends SIGTERM to the main process only, then SIGKILL to the whole cgroup after `TimeoutStopSec` — right for us, since `browd` must be the one to close Chrome gracefully.
- **`loginctl enable-linger $USER` is mandatory** if the daemon must survive logout / run over SSH sessions; without it systemd stops the user manager shortly after the last session ends, taking `browd` with it ([ArchWiki systemd/User](https://wiki.archlinux.org/title/Systemd/User)).
- Socket activation is a genuine option on Linux and cheap: add `browd.socket` with `ListenStream=%t/brow/brow.sock`, `SocketMode=0600`, then in `browd` use `listenfd = "1.0.2"` to take fd 3 instead of binding. This makes cold start invisible and removes §3.3's spawn logic entirely on Linux. Recommended as a v2 refinement, with the caveat that `%t` (`$XDG_RUNTIME_DIR`, typically `/run/user/1000`) must be short enough for `sun_path`.

### 4.3 Windows — the honest answer

**There is no per-user Windows service.** Windows services run in session 0, are installed machine-wide, require Administrator to install, and cannot see the user's desktop. Running Chrome from session 0 is broken for headed mode and unpleasant even headless.

What actually works, in order of preference:

| approach | works? | notes |
|---|---|---|
| **Task Scheduler, trigger "At log on" of the current user, "Run only when user is logged on"** | yes — recommended | Runs in the interactive session, no admin, no UAC. `schtasks /Create /TN "brow\browd" /TR "%USERPROFILE%\.cargo\bin\brow.exe daemon serve" /SC ONLOGON /RL LIMITED /F`. Uninstall: `schtasks /Delete /TN "brow\browd" /F`. |
| Startup folder shortcut (`shell:startup`) | yes, weakest | No restart-on-failure, no logging, visible to the user, easily disabled by Task Manager's Startup tab |
| Registry `HKCU\...\Run` | yes | Same limits as Startup folder |
| Real Windows service | no (for our purpose) | Machine-wide, needs admin, session 0 isolation. Only correct for a multi-user shared host, which we are not |
| WinSW / NSSM wrappers | no | Same session-0 problem, plus a third-party binary |

Recommendation: **rely on `brow`'s own auto-spawn as the primary mechanism on Windows** (a `CreateProcess` with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`), and offer the at-logon scheduled task as an opt-in `brow service install` for people who want the daemon warm before the first command. Note that "Run whether user is logged on or not" gives a non-interactive session and must **not** be used.

IPC on Windows: `tokio::net::windows::named_pipe` (feature `net`) with `\\.\pipe\brow-<sid>`. There is no `SO_PEERCRED`; the equivalents are `GetNamedPipeClientProcessId` plus a **security descriptor on the pipe** restricting access to the current user SID — which `ServerOptions` does not expose, so this needs a small `windows-sys` FFI call to `CreateNamedPipeW` with a `SECURITY_ATTRIBUTES`. Budget a day for this; do not assume the Unix design ports for free. (The repo's README already flags "Unix only" for the CDP pipe — Windows handle inheritance for `--remote-debugging-pipe` is a separate, larger piece of work.)

### 4.4 Uninstall must be clean

```
brow service uninstall     # bootout / systemctl --user disable --now / schtasks /Delete, then rm the unit file
brow purge                 # + rm -rf $BROW_HOME after listing what it will delete
```

The `~/.brow`-single-root layout in `src/paths.rs` is exactly right for this and should not be traded for XDG scatter. Checklist: plist/unit/task removed **and** deactivated; `run/` empty; no Chrome process whose `--user-data-dir` is under `~/.brow/profiles`; PATH binary untouched (that is cargo's business).

---

## 5. Job engine

### 5.1 States

```
                 ┌──────────────── stop ────────────────┐
                 ▼                                       │
  queued ──admit──▶ running ──gate──▶ waiting_for_approval ──approve──▶ running
     │                │  ▲                    │  │                        │
   cancel           pause│resume          deny│  │timeout                 │
     │                ▼  │                    ▼  ▼                        │
     └───▶ stopped ◀── paused ◀──────────── stopped/failed          succeeded
                         │
                     (daemon restart)
                         ▼
                    interrupted   ← terminal. NOT resumable in place.
```

| state | live? | meaning |
|---|---|---|
| `queued` | yes | admitted to the queue, no browser context yet |
| `running` | yes | plan interpreter executing |
| `waiting_for_approval` | yes | parked on a `policy` gate; browser context frozen |
| `paused` | yes | parked by user or by a resource guard (`memory_ceiling`, `low_disk`) |
| `succeeded` / `failed` / `stopped` | no | terminal; `stopped` = user-initiated |
| `interrupted` | no | daemon died while live. Browser is provably gone |
| `expired` | no | `waiting_for_approval` past its deadline |

Transitions are written to SQLite inside a transaction together with the corresponding `job_events` row, so the on-disk state and the event log can never disagree.

### 5.2 Durable state

```sql
-- ~/.brow/state.db, PRAGMA journal_mode=WAL; synchronous=NORMAL
CREATE TABLE jobs (
  id TEXT PRIMARY KEY,              -- ULID (ulid 3.0.0): sortable by creation time
  session TEXT NOT NULL,
  state TEXT NOT NULL,
  capability_mode TEXT NOT NULL,    -- observe|interact|inspect|storage|mutate|control
  plan_hash TEXT NOT NULL,
  intent TEXT,                      -- the original natural-language string, for provenance
  created_at INTEGER, started_at INTEGER, ended_at INTEGER,
  exit_reason TEXT, browser_pid INTEGER, context_id TEXT,
  daemon_boot_id TEXT NOT NULL      -- invalidates cross-restart claims
);
CREATE TABLE job_events (
  job_id TEXT, seq INTEGER, stream TEXT, t REAL, kind TEXT, data TEXT,
  PRIMARY KEY (job_id, stream, seq)
);
CREATE TABLE approvals (
  job_id TEXT, n INTEGER, action TEXT, evidence TEXT,
  requested_at INTEGER, deadline_at INTEGER,
  decision TEXT, decided_at INTEGER, decided_by TEXT,
  PRIMARY KEY (job_id, n)
);
```

`daemon_boot_id` is a fresh ULID per daemon process. Any row with a live state and a stale `daemon_boot_id` at startup is force-transitioned to `interrupted` — idempotent recovery in one `UPDATE`.

`rusqlite = { version = "0.40.1", features = ["bundled"] }` avoids depending on the system SQLite; `bundled` compiles the amalgamation, which costs ~10 s of build time and removes a whole class of "works on my machine".

### 5.3 What PAUSE actually means — verified

Three separable things could be "paused", and they are not equally pausable.

| target | mechanism | works? | evidence |
|---|---|---|---|
| the **plan interpreter** | daemon-side flag checked between steps | yes, always, instant | trivial |
| **page JS timers / rAF** | `Page.setWebLifecycleState{state:"frozen"}` (EXPERIMENTAL) | **yes, and reversible** | Verified: a `setInterval` counter did not advance for 1 s while frozen; `{state:"active"}` restored it |
| page JS timers, alternative | `Emulation.setVirtualTimePolicy{policy:"pause"}` (EXPERIMENTAL) | pauses, **but is a one-way door** | Verified: counter froze; `{policy:"advance", budget:200}` ran then re-paused at budget expiry; `{policy:"advance"}` with no budget advanced the counter by **439 678 ticks in 0.5 s wall** (~8 800× real time). There is no `disable` value — the enum is only `advance` / `pause` / `pauseIfNetworkFetchesPending` |
| JS execution mid-statement | `Debugger.pause` | works, but poisons the session | Requires `Debugger.enable` (deoptimises V8, changes timing); while paused, `Runtime.evaluate` on that context blocks. Do not use for user-facing pause |
| **in-flight network responses** | — | **no** | Bytes already in flight land. You would need `Fetch.enable` interception on every request to hold them, which changes timing and breaks streaming responses |
| **the server** | — | **no** | Session timeouts, rate limits and OTP validity keep ticking. A 40-minute pause can invalidate a login |
| **video recording** | ffmpeg segment boundary | partially | You cannot "pause" a running encode cleanly; close the current segment, and on resume start a new one. Concat at job end and record the wall-clock gap in the action log |

> **Corrected 2026-08-04 — `Page.captureScreenshot` silently un-freezes a frozen page.** This is the single most consequential correction in this document, because the approval-gate flow in §7 freezes a context *and* captures evidence.
>
> Fixture: `setInterval(()=>window.n++,10)` plus `freeze`/`resume` lifecycle listeners appending `Date.now()`.
> ```
> baseline n: 119
> freeze -> ok
>   t+0.5s  LC=["f@1785869596078"]                     n=120
>   t+2.0s  LC=["f@1785869596078"]                     n=120     <- genuinely frozen
>   --> Page.captureScreenshot (succeeds, 4,388 B, 28 ms)
>   after shot LC=["f@...596078","r@1785869598092"]    n=126     <- RESUME fired, counter running
>   t+5s     LC=[... same ...]                          n=129
> ```
> Control run — freeze, then **4 s with no CDP traffic that forces a frame**: `LC=["f@1785869601155"] n=0`. The freeze holds; it is the *screenshot* that lifts it, and **it is never re-applied**. Nothing in the CDP response signals this.
>
> Two further corrections from the same run:
> - **Screenshots, `DOM.getDocument` and `Runtime.evaluate` all work normally on a frozen target** (the doc previously listed this as untested) — but the first of them ends the pause.
> - **In-flight network completes and its continuations mutate page state during the "pause".** A `fetch()` to a 3-second endpoint issued just before freezing resolved while frozen and `window.F` read back `"SLOWDONE"`. So "the whole world stops" is false even for the page's own JS.
> - `document.wasDiscarded` stayed `false`; the page observed **both** `freeze` and `resume`, so an adversarial page gets a reliable "the agent just paused and screenshotted me" signal.
>
> **Revised recommendation.** Treat `frozen` as a *best-effort timer quiesce*, not a pause primitive:
> 1. Capture all evidence **first**, then freeze — never freeze-then-screenshot.
> 2. After any capture on a paused job, **re-issue `Page.setWebLifecycleState{"frozen"}`** and record a `pause_leaked_ms` field in the action log.
> 3. The **only** authoritative pause is the daemon-side interpreter flag. Say in the docs that `pause` stops *brow*, not the page and not the server.

**Recommended `pause` = freeze the interpreter (authoritative) + best-effort `Page.setWebLifecycleState{"frozen"}` on every target in the job's context + close the current video segment.** Caveats to document: freezing fires `freeze`/`resume` lifecycle events that a page can observe, any screenshot silently resumes it, and Chrome throttles timers in non-visible targets anyway (after unfreezing a background target I measured only ~1 tick/0.5 s where the same page in a foreground target ran ~100 ticks/0.5 s — background timer throttling, mitigable with `--disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows`).

### 5.4 Resume after a daemon restart: it does not exist

This is not a design preference, it is a measured fact. I launched Chrome with `--remote-debugging-pipe` from a parent, confirmed CDP worked over fds 3/4, then `SIGKILL`ed the parent with the pipe fds still open. **Within one second the entire Chrome tree — browser plus 8 helper processes — was gone.** (A separate control showed the trigger is EOF on the debugging pipe, not parent death per se; an earlier "it survived" reading was a zombie artifact, since `kill(pid,0)` succeeds on un-reaped zombies.)

So: daemon dies ⇒ browser dies ⇒ every DOM, every ref generation, every in-page JS state, every un-persisted form field is gone. What survives is the **profile directory** (`~/.brow/profiles/<session>`): cookies, localStorage, IndexedDB, service-worker registrations.

Honest checkpointing therefore looks like this:

```jsonc
// ~/.brow/jobs/<id>/checkpoint.json  — rewritten atomically (tmp + rename) after every plan step
{ "version": 1, "plan_hash": "sha256:…", "step_index": 37,
  "resumable": true, "resume_kind": "url_frontier",
  "session": "default", "profile_persistent": true,
  "frontier": ["https://x/pricing", "https://x/docs/a"],
  "visited": {"count": 41, "bloom": "base64:…"},
  "site_graph_path": "graph.json",
  "artifacts": ["screenshots/…png"],
  "invalidated_on_resume": ["node_refs", "page_dom", "in_page_js_state", "unsubmitted_forms"] }
```

`brow job restart <id>` starts a **new** job that replays the plan from `step_index` against a **new** browser, and says so:

```
brow: job j-01JQ… was interrupted (browd restarted at 14:02).
      Restarting as j-01JR… from checkpoint step 37/120.
      Carried over: URL frontier (79 left), visited set, site graph, 41 artifacts, cookies.
      Lost: page DOM, all @node refs, 1 partially-filled form on /signup.
```

Set `resumable: false` for any plan that contains a `fill`/`upload`/`otp` step not followed by a committing navigation. Do not guess.

---

## 6. The autonomous loop — who runs the LLM

This is the fork the spec leaves open. Three options.

**A. Daemon-embedded model client.** `browd` holds an API key or a local model and runs the observe→decide→act loop itself.
*Pros:* a detached job is genuinely autonomous; no agent process needed for 40 minutes.
*Cons:* directly violates "no cloud APIs" unless the model is local — and a local model means either bundling weights (violates "no automatic download") or requiring Ollama/llama.cpp, which is a heavy, unstated dependency and produces markedly worse web-navigation decisions. It also duplicates the agent's context and repo knowledge, and makes `browd` a credential holder — the single worst place to put one, given §2.4's uid-only boundary. **Reject.**

**B. LLM stays in the agent; the daemon runs only deterministic playbooks.** Every decision comes from the agent turn; `browd` executes typed steps.
*Pros:* zero credentials in the daemon, fully deterministic and replayable, trivially testable, no telemetry surface, aligns with the existing "the socket protocol is the whole capability surface" invariant.
*Cons:* a "detached" job cannot make novel decisions. Nothing autonomous happens after the agent turn ends.

**C. Hybrid — agent compiles a plan; the daemon executes it and parks at decision gates.**
The `/browser` skill turns *"walk all pages, check for errors, collect a sitemap"* into a plan artifact. `browd` executes it deterministically. When the plan reaches a point it cannot decide (an unrecognised modal, an auth wall, a form with no declared value), the job transitions to `waiting_for_agent` — the same machinery as `waiting_for_approval`, different audience. The agent (this turn, a later turn, or a `claude -p` / `--bg` background session) polls `brow job next`, reads the parked context (screenshot + page tree + the question), answers, and the job continues.

**Recommendation: C, built as B plus gates.** Rationale:

1. **MCP sampling is deprecated** (the doc previously said "dead" — see the nuance below). The one standardised way for a server to borrow the host's model — `sampling/createMessage` — is **deprecated as of MCP protocol revision `2026-07-28` (SEP-2577)**, with the spec stating "New implementations **SHOULD NOT** adopt it; existing implementations **SHOULD** migrate to integrating directly with LLM provider APIs" ([MCP spec: Sampling](https://modelcontextprotocol.io/specification/draft/client/sampling)).

   > **Verified 2026-08-04 (fetched the spec page; the quote is verbatim) — with two corrections that soften "dead" without changing the recommendation.**
   > (a) The same warning block continues: *"Under the **feature lifecycle policy**, it remains in the specification for **at least twelve months** after this revision's release before it becomes eligible for removal."* So sampling is guaranteed present until at least **2027-07-28**; "dead" overstates it, "do not build on it" does not.
   > (b) The mechanism was **reshaped, not just deprecated**: `sampling/createMessage` is now delivered inside an `InputRequiredResult.inputRequests` (the MRTR pattern) and returned via `inputResponses` on a retried call, and it gained `tools`/`toolChoice` with a `sampling.tools` client capability. A brow implementation written against the older shape would already be wrong.
   > `includeContext: "thisServer" | "allServers"` is *separately* deprecated (SEP-2596). **Recommendation C stands, and for a stronger reason than "the door is closing": the door moved.** Building the daemon's autonomy on a callback into the host's LLM would be building on a feature scheduled for removal.
2. **The harnesses already solved detachment.** Claude Code has `/background` (`/bg`), `--bg`, headless `claude -p`, and `run_in_background` in the Agent SDK. The right division of labour is: *the harness owns the long-lived LLM loop; `brow` owns the long-lived browser.* Duplicating the former inside `browd` competes with the harness instead of complementing it.
3. **It makes crawls honest.** The spec demands route **coverage and provenance**, not a pretend-exhaustive crawl. A declarative plan plus a recorded list of decision gates *is* the provenance record. `edge.status ∈ {declared, observed, discovered, inferred, blocked}` maps cleanly onto "the plan declared it" / "the executor observed it" / "a gate answer inferred it".
4. **It is testable.** A plan is a file. You can diff it, replay it against a fixture site, and assert byte-identical artifacts — impossible if a model is in the loop.

Plan shape (versioned, hashed, stored at `jobs/<id>/plan.json`):

```jsonc
{ "plan": 1, "intent": "walk all pages, check for errors, collect a sitemap",
  "authored_by": {"agent": "claude-code", "at": 1785862000},
  "capability_mode": "interact",
  "budget": {"max_steps": 500, "max_seconds": 2400, "max_bytes": 2147483648},
  "steps": [
    {"op":"open","url":"https://example.com"},
    {"op":"crawl","scope":{"same_origin":true,"exclude":["/logout","/admin/*"]},
     "max_pages":200,"per_page":[{"op":"console.collect"},{"op":"screenshot","target":"viewport"}]},
    {"op":"gate","when":"auth_wall","ask":"Log in as which user?","kind":"agent"},
    {"op":"gate","when":"destructive","kinds":["publish","delete","purchase","send_message","otp","file_upload","permission_grant","cookie_import"],"kind":"human"},
    {"op":"emit","artifact":"sitemap.json"}
  ]}
```

Everything the daemon can do is a closed enum of `op`s — the same structural guarantee as the IPC `Request` enum. Escalation path if C proves too rigid: add narrow, auditable "local heuristics" (a form-field name→value matcher, a modal-dismissal classifier) as *deterministic* steps, not as a model.

---

## 7. Approval gates

Trigger set is fixed by the spec: publish, delete, purchase, send message, OTP entry, file upload, granting camera/mic/location, importing cookies. Detection lives in `crates/policy` and must be evaluated **before** the input event is synthesised, not after.

Flow:

1. Interpreter reaches a gated action → writes `approvals` row, captures evidence (`screenshot` before, target node path, resolved URL, planned CDP effects in human words), transitions to `waiting_for_approval`, freezes the context (§5.3), pushes `{"ev":"approval"}` on every follower stream.
2. **Notification, best-effort, in this order:**
   - the `ev` frame (any attached `--follow` sees it instantly);
   - a desktop notification. On macOS, `notify-rust 4.18.0` goes through `mac-notification-sys`, and `NSUserNotification` does not work from a plain Foundation tool — `terminal-notifier` exists precisely because notifications need an app bundle. I verified that **`osascript -e 'display notification …'` works and exits 0 from a CLI** (it borrows Script Editor's bundle identity, so the banner is attributed to "Script Editor" — ugly but functional). Shell out to `osascript` on macOS, use `notify-rust` (D-Bus) on Linux, and skip on Windows for v1;

   > **Verified 2026-08-04 — the specific untested worry ("can a launchd Background-type job post at all?") is resolved: YES.** Installed a throwaway `~/Library/LaunchAgents/com.brow.verifytest.plist` with `ProcessType=Background`, `RunAtLoad=true`, running a shell script, then `launchctl bootstrap gui/502`. Script output:
   > ```
   > run at Tue Aug  4 20:59:23 CEST 2026
   > uid=502 tty=not a tty
   > osascript_exit=0
   > osascript_exit2=0
   > gui/502/com.brow.verifytest = { active count = 1 ... type = LaunchAgent  state = running }
   > ```
   > Two `display notification` calls both exited 0 from a **non-interactive, TTY-less, Background-classified LaunchAgent**. Also re-confirmed: a second `bootstrap` of the same label fails `Bootstrap failed: 5: Input/output error` (exit 5), so installers must `bootout` first. Agent and plist were removed; `~/Library/LaunchAgents` restored byte-identical.
   >
   > Still **not** verified: behaviour under Focus/Do-Not-Disturb, or when the user has denied notifications for Script Editor. Exit 0 does not prove a banner was *seen* — keep the `ev` stream and exit code 5 as the authoritative channels.
   - an optional user-configured `on_approval` hook command (`~/.brow/config.toml`), which is how someone wires it to Slack, `tmux display-message`, or a Claude Code hook.
3. Human or agent decides:
   ```
   brow job approvals <id>              # list pending, with evidence paths
   brow job approve  <id> --n 3 [--note "yes, staging only"]
   brow job deny     <id> --n 3
   brow job approve  <id> --all --for 10m     # standing approval window, still logged per action
   ```
4. `deadline_at` defaults to 30 min. On expiry → `expired` (terminal), never auto-approve. Configurable per gate; `0` = wait forever.
5. Exit codes so scripts and agents can branch without parsing text: `0` ok, `2` usage, `3` daemon unreachable, `4` job failed, `5` **waiting for approval**, `6` denied, `7` expired, `124` timeout.

Approvals are recorded with the decider's uid **and** `UCred::pid()`, so "the agent approved its own gate" is at least visible in `audit.jsonl`. It is not preventable — same uid boundary as §2.4.

---

## 8. Artifacts, logs, `--follow`, retention

### 8.1 Layout

```
~/.brow/                                   0700
├── run/  brow.sock (0600)  browd.pid  browd.lock
├── logs/ browd.log  browd.log.1.zst …    audit.jsonl
├── state.db  state.db-wal  state.db-shm
├── profiles/<session>/                    (Chrome user-data-dir)
└── jobs/<ulid>/
    ├── manifest.json      # denormalised copy of the jobs row — survives a lost DB
    ├── plan.json          # what was authored
    ├── checkpoint.json    # atomically rewritten
    ├── events.jsonl       # every state change, step, gate, error  (the action log)
    ├── console.jsonl  network.jsonl
    ├── screenshots/<seq>-<slug>.png
    ├── video/segment-0001.mp4 … video.mp4
    ├── approvals/<n>.json
    └── graph.json         # site graph, if the plan emitted one
```

`events.jsonl` is the spec's "video recording synced to an action log". Every line: `{"seq":N,"t":<monotonic_ms_since_job_start>,"wall":<unix_ms>,"kind":"click|request|route|console|state|gate","d":{…}}`. `t` is the *video* clock — start it at the first video frame, not at job creation, or the sync is silently off by the browser cold-start time.

Keep `~/.brow` as the single root. XDG purity would scatter state across `XDG_STATE_HOME`/`XDG_DATA_HOME`/`XDG_RUNTIME_DIR` and break the `rm -rf` uninstall story; the existing `paths.rs` comment about `sun_path` being 104 bytes on macOS is the correct call and there is a test guarding it. Offer `BROW_HOME` (already present) and, on Linux only, put the socket in `$XDG_RUNTIME_DIR/brow/` when the resulting path is under 100 bytes — that is what socket activation would need anyway.

### 8.2 `--follow`: push, not tail

Arguments for daemon push over `inotify`/`kqueue` tailing:

1. **Termination.** A tailer cannot distinguish "job finished" from "nothing written yet". Push sends `{"ev":"state","d":{"to":"succeeded"}}` then `end`.
2. **Rotation races.** kqueue watches an *inode*; a rotated log leaves the follower reading a deleted file. Every tail implementation eventually reimplements `tail -F` badly.
3. **Structured events that never hit disk.** Progress ("41/200 pages") should not be persisted at 10 Hz; push can send it and drop it.
4. **Backpressure.** A slow terminal should slow the stream or get an explicit `dropped` count — a tailer just falls behind invisibly.
5. **Correctness after `Ctrl-C`.** `--follow --from-seq N` replays from SQLite/JSONL, then switches to live. A tailer would need byte offsets that shift on rotation.

`notify = "8.2.0"` stays useful for one thing only: watching `plan.json` in a `brow job dev` mode.

### 8.3 Retention and disk guards

```toml
[retention]
keep_jobs        = 50          # most recent terminal jobs
keep_days        = 14
max_total_bytes  = "5GiB"
min_free_bytes   = "2GiB"      # refuse to START a recording job below this
pause_free_bytes = "1GiB"      # PAUSE running recorders below this
```

GC runs at daemon start and hourly: never touch a job in a live state; delete oldest-terminal-first until under both budgets; delete video before screenshots before JSONL before `manifest.json` (manifests are tiny and are the provenance record — keep them past the blobs, with `"artifacts_gc'd": true`). `brow gc --dry-run` prints exactly what would go.

Free-space check via `statvfs` (`rustix 1.1.4` or `sysinfo 0.39.6`). A 1080p `libx264` screen recording is roughly 1–5 MB/s; a 40-minute crawl with video is 2.4–12 GB. **The disk guard is not optional** — this is the single most likely way `brow` ruins someone's afternoon.

Log rotation: `tracing-appender = "0.2.5"` gives daily rolling out of the box; compress rotated files with `zstd = "0.13.3"`.

---

## 9. Crash safety and orphan cleanup

### 9.1 Orphans are mostly designed away

Because of `--remote-debugging-pipe`, a `SIGKILL`ed daemon takes Chrome with it (verified, <1 s, all 9 processes). This is a much stronger guarantee than any pid-file scheme. Do not switch to `--remote-debugging-port` to gain restart survivability — it would also make the browser reachable by every local process, which the README correctly calls out as unacceptable.

> **Verified and narrowed 2026-08-04.** The mechanism is confirmed and is *specifically* **EOF on fd 3**, not parent death and not fd-4 `EPIPE`:
> - close only the parent's fd-3 write end → `CHROME EXITED after 1.01s rc=0` (2/2)
> - close only the parent's fd-4 read end → `CHROME STILL ALIVE after 45.3s`, state `Ss` (not a zombie)
> - renderer spinning in `while(Date.now()-t<120000){}` → still `CHROME EXITED after 1.00s`. **The "hung renderer ignores pipe EOF" worry in the risk note does not reproduce** — the browser process handles the pipe, and it exits regardless of what a renderer is doing.
>
> **But "orphans are structurally impossible" is REFUTED as stated.** A pipe reader sees EOF only when **every** write end is closed. Repeating the test with `/bin/sleep 120` spawned holding an inherited duplicate of the fd-3 write end:
> ```
> HS ok: True
> HELPER pid 42969 inherited fd 4
> CHROME STILL ALIVE after 40.2s  <== ORPHAN
> ```
> `browserd` spawns ffmpeg for `--record-video`. If that spawn inherits the CDP write fd — the default on any path that doesn't set `FD_CLOEXEC`, and easy to reintroduce with a `dup()` or a raw-fd hand-off — **every video job leaves a ~1 GB orphan browser when the daemon is killed.** Make `FD_CLOEXEC` on both CDP fds an asserted invariant with a unit test, and keep the `ppid==1` pgid sweep below as the backstop. This is a week-one bug, not a theoretical one.

Residual risks and their handling:

| risk | handling |
|---|---|
| Chrome hangs and ignores pipe EOF | On daemon start, enumerate processes (`sysinfo 0.39.6`) whose command line contains `--user-data-dir=<BROW_HOME>/profiles/` and whose `ppid == 1`; `kill(-pgid, SIGTERM)`, then `SIGKILL` after 5 s |
| **PID files lie** (pid reuse after reboot) | Store `{pid, start_time, boot_id}` in `run/browd.pid`. `sysinfo`'s `Process::start_time()` disambiguates reuse. Treat a pid file whose start_time does not match as stale, unconditionally |
| Helper processes leak | Verified: every Chrome helper is a **direct child of the browser process and shares its pgid** when the browser is `setsid()`ed. So `kill(-pgid)` is a complete kill. Set the pgid deliberately at spawn (`setsid` in `pre_exec`) and record it in `jobs.browser_pid` |
| Zombie confusion | Verified pitfall: `kill(pid, 0)` returns success for a zombie. Always `waitpid` (or check `sysinfo` status ≠ `Zombie`) before concluding a process is alive |
| A crashed *renderer* | Subscribe to `Target.targetCrashed` (params `targetId`, `status`, `errorCode`; **not** experimental) and `Inspector.targetCrashed` / `Inspector.targetReloadedAfterCrash`. A renderer crash fails the job's current step with `code: "renderer_crashed"` and, if the plan allows, re-creates the target from the checkpoint URL |

### 9.2 Recovery on start (idempotent)

```rust
async fn recover(db: &Db, boot: &BootId) -> Result<Recovery> {
    let stale = db.jobs_live_with_other_boot(boot).await?;   // one UPDATE … WHERE
    for j in &stale { db.transition(j.id, State::Interrupted, "browd restarted").await?; }
    kill_orphan_browsers(&paths::root())?;   // pgid sweep, see table
    gc::run(&db).await?;                     // retention, honours live states (none are live now)
    Ok(Recovery { interrupted: stale.len() })
}
```

Idempotent because every step is "bring the world to state X", never "apply delta". Running it twice changes nothing.

---

## 10. Observability

```
brow daemon status         # pid, uptime, protocol range, sessions, jobs by state, RSS, socket path
brow ps                    # jobs table: id, state, session, step, elapsed, RSS, artifacts size
brow sessions              # existing; add browser pid, pgid, context count, profile size
brow doctor                # the thing you run when it is broken
brow logs --follow         # daemon log, pushed
brow job logs <id> --follow
```

`brow doctor` checks, each PASS/WARN/FAIL with a one-line remedy:

| check | how |
|---|---|
| Chrome binary found and version | `$BROW_CHROME` → known paths → PATH; run `--version` |
| Chrome ≥ minimum | parse `Browser.getVersion` if a session exists |
| daemon reachable | `connect()` + hello; report protocol range mismatch explicitly |
| protocol compatible | CLI range ∩ daemon range |
| service installed **and active** | macOS: `launchctl print gui/$UID/<label>`; Linux: `systemctl --user is-active`; Windows: `schtasks /Query`. Checking only for the plist file misses "user disabled it in Login Items" |
| socket permissions | mode `0600`, parent dir `0700`, owner == uid |
| lock file sane | `flock` held by the pid in `browd.pid`, start_time matches |
| orphan Chromes | count of `ppid==1` Chromes under `BROW_HOME/profiles` |
| disk | free bytes vs `min_free_bytes`; `~/.brow` size, largest jobs |
| state.db | `PRAGMA integrity_check`, WAL size |
| clock | daemon `started_at` vs now (catches suspend/resume weirdness in event timestamps) |
| ffmpeg | `ffmpeg -version` on PATH (8.1.2 here) — required only for video jobs |

`brow doctor --json` for agent consumption. Anything that FAILs must print the exact command that fixes it; the repo's existing `Response::error_hint` convention already carries this and should be reused verbatim.

---

## What we verified empirically

All on macOS 26.5.1 (Darwin 25.5.0, arm64), Chrome **151.0.7922.72**, CDP protocol 1.3, V8 15.1.206.10, 57 domains in `/json/protocol`. Every Chrome I started used a scratch `--user-data-dir` under `/private/tmp/browtest` and was killed afterwards; the test LaunchAgent was removed and `~/Library/LaunchAgents` restored to its original 3 entries.

| # | What I ran | Raw observation |
|---|---|---|
| 1 | `Emulation.setVirtualTimePolicy{policy:"pause"}` on a page with `setInterval(()=>n++,10)` | counter `149 → 149` over 1 s wall — **frozen**. `Runtime.evaluate` still worked while paused |
| 2 | `{policy:"advance", budget:200}` then wait | counter advanced, then `60 → 60` over 1 s — **budget expiry re-pauses** |
| 3 | `{policy:"advance"}` with **no** budget | counter `131 → 439 809` in 0.5 s wall — **~8 800× real time. Virtual time is a one-way door; there is no `disable`** |
| 4 | `Page.setWebLifecycleState{"frozen"}` / `{"active"}` | `39 → 39` over 1 s frozen; resumed after `"active"` — reversible. **Amended 2026-08-04: `Page.captureScreenshot` silently fires `resume` and restarts timers (`n 120→126`); in-flight `fetch` completes and mutates page state while "frozen". Not a durable pause — see §5.3** |
| 5 | `--renderer-process-limit=2` with 5 browser contexts | `{browser:1, renderer:8, GPU:1, NetworkService:1, StorageService:1}` — **flag not honoured**. Re-verified 2026-08-04 on a separate run: `{renderer:7, browser:1, gpu:1, NetworkService:1, StorageService:1}` |
| 6 | `Target.disposeBrowserContext` ×5 | 0.05 s total; renderers `8 → 3` within 2 s |
| 7 | `Browser.close` | connection dropped immediately; all 12 processes gone within ~3 s |
| 8 | `Browser.getBrowserCommandLine` | `error -32000: "Command line not returned because --enable-automation not set."` |
| 9 | RSS of one headless browser, 5 targets | browser 233.8 MB, GPU 98.8, Network 85.8, Storage 61.9, renderers 75–115 each; **total 1 072 MB**. **Corrected 2026-08-04: RSS understates. Same shape re-measured → sum RSS 1 364 MB vs sum `phys_footprint` 2 030 MB (0.67×). Plan for ~2 GB per session** |
| 10 | Chrome via `--remote-debugging-pipe` on fds 3/4, then `SIGKILL` the parent | `Browser.getVersion` worked over the pipe; **1 s after parent SIGKILL the browser and all 8 helpers were gone** |
| 11 | Same, but parent closes both pipe fds and stays alive | Chrome also exits (EOF is the trigger). My first reading of "survived" was a **zombie** — `kill(pid,0)` succeeds on zombies. **Narrowed 2026-08-04: the trigger is fd-3 EOF *only*. fd-3 write end closed → exit in 1.01 s; fd-4 read end closed → `STILL ALIVE after 45.3s`** |
| 11b | **fd-3 write end duplicated into another child, then parent closes its copy** (new, 2026-08-04) | `CHROME STILL ALIVE after 40.2s <== ORPHAN`. The no-orphan guarantee requires `FD_CLOEXEC` on the CDP fds — see §9.1 |
| 11c | fd-3 EOF while a renderer spins in a 120 s busy loop (new) | `CHROME EXITED after 1.00s rc=0` — the "hung renderer ignores EOF" worry does **not** reproduce |
| 12 | `ps` of the Chrome tree | all helpers are direct children of the browser and share its pgid (browser was `setsid`ed) → `kill(-pgid)` is complete |
| 13 | `bind()` a second socket on a live path | `errno 48 EADDRINUSE` |
| 14 | `unlink()` then `bind()` while the first listener is open | **second bind succeeded; new clients reached the second listener while the first stayed alive and unreachable — the TOCTOU race in `src/daemon.rs::bind()` is real** |
| 15 | `flock(LOCK_EX\|LOCK_NB)` twice on one file | second call `errno 35 EAGAIN` — correct exclusion on macOS |
| 16 | `launchctl bootstrap gui/502 <plist>` | exit 0; `launchctl print` showed `type = LaunchAgent`, `state = running`, `program = /bin/sleep`, `pid = 38261`, `spawn type = background (5)`, `properties = runatload \| inferred program` |
| 17 | `launchctl bootstrap` a second time | **`Bootstrap failed: 5: Input/output error`, exit 5** — installers must bootout first |
| 18 | `launchctl bootout gui/502/<label>` then `print` | exit 0; `Could not find service "…" in domain for user gui: 502` |
| 19 | `osascript -e 'display notification …'` | exit 0, banner delivered from a plain CLI (attributed to Script Editor) |
| 19b | Same, from inside a real `ProcessType=Background` LaunchAgent (new, 2026-08-04) | `uid=502 tty=not a tty` / `osascript_exit=0` / `osascript_exit2=0`, `launchctl print` showed `state = running`. **A launchd Background job CAN post notifications.** Agent booted out and plist deleted; `~/Library/LaunchAgents` restored identical |
| 20 | `Target.targetCrashed` in `/json/protocol` | present, **not** experimental, params `targetId, status, errorCode` |
| 21 | crates.io API, 2026-08-04 | tokio 1.53.1 · serde_json 1.0.151 · rusqlite 0.40.1 · redb 4.1.0 · fs4 1.1.0 · fd-lock 4.0.4 · notify 8.2.0 · sysinfo 0.39.6 · nix 0.31.3 · rustix 1.1.4 · interprocess 2.4.3 · uuid 1.24.0 · ulid 3.0.0 · tokio-util 0.7.19 · tokio-stream 0.1.19 · tracing-appender 0.2.5 · notify-rust 4.18.0 · sd-notify 0.5.0 · listenfd 1.0.2 · zstd 0.13.3 · humantime 2.4.0 · clap 4.6.5 · thiserror 2.0.19 · anyhow 1.0.104 |
| 22 | docs.rs tokio 1.53.1 `UCred` | `uid()`, `gid()`, `pid()`; **`pid()` is implemented on macOS** |

---

## Limits and impossibilities

0. **"Pause" does not stop the page, and any screenshot cancels the freeze.** Verified 2026-08-04 (§5.3): `Page.captureScreenshot` on a frozen target fires the page's `resume` event and restarts its timers, permanently, with no CDP signal. In-flight `fetch` continuations also run and mutate page state while frozen. The only authoritative pause is the daemon-side interpreter flag. **`brow job pause` must not be documented as "the page is stopped".**
1. **No job can resume in place after a daemon restart.** Verified in #10/#11. The browser is gone with the daemon. Only replay-from-checkpoint is possible, and only for plans whose state lives in URLs and cookies. Any spec language implying "pause a job, restart browd, resume" must be rewritten.
1b. **Orphan Chromes are possible after all** — the no-orphan guarantee is conditional on `FD_CLOEXEC` for the CDP pipe fds. Verified orphan (#11b) when another child inherits the fd-3 write end, which is exactly what an ffmpeg spawn does by default. Ship the CLOEXEC assertion *and* the pgid sweep.
2. **`Emulation.setVirtualTimePolicy` cannot be turned off** (#3). Use it only for deterministic capture in a throwaway context, never for a pause the user will resume from.
3. **You cannot cap Chrome's memory on macOS.** `--renderer-process-limit` is ignored (#5); `ProcessType=Background` is CPU/IO only; there is no cgroup. Daemon-side sampling and job parking are the only lever. Linux gets a real cap via `systemd-run --scope -p MemoryMax=`.
4. **In-flight network cannot be paused** without `Fetch.enable` interception on every request, which perturbs exactly the timing you are trying to observe. And nothing pauses the *server*: sessions expire, rate limits reset, OTPs die.
5. **There is no per-user Windows service.** Task Scheduler at-logon is the answer; a real service means session 0 and admin rights. Also, `--remote-debugging-pipe` handle inheritance on Windows is unwritten work, and named-pipe access control needs `windows-sys` FFI because `tokio::net::windows::named_pipe::ServerOptions` exposes no security descriptor.
6. **The security boundary is the uid.** Any process running as the user can drive the socket. Peer-executable checks are TOCTOU-racy and preload-defeatable. Do not market socket permissions as sandboxing.
7. **MCP sampling is deprecated** (protocol `2026-07-28`, SEP-2577). If you were counting on "the daemon asks the host's LLM", that door is closing. Confirmed from the spec text.
8. **Desktop notifications from a CLI on macOS are second-class.** `NSUserNotification` needs an app bundle; `osascript` works but is attributed to Script Editor, and can be silenced by the user without any signal back to `brow`. Treat notification as best-effort; the authoritative channels are the `ev` stream and exit code 5.
9. **`brow doctor` cannot detect a user disabling the LaunchAgent by file existence** — since macOS 13 the toggle lives in Login Items and leaves the plist in place. Must query `launchctl print`.
10. **Video cannot be paused, only segmented.** The action-log timeline will contain wall-clock gaps; make them explicit rather than papering over them.
11. **`Memory`, `SystemInfo`, `Storage`, `Emulation.setVirtualTimePolicy`, `Page.setWebLifecycleState`, `Browser.setDownloadBehavior`, `Target.attachToBrowserTarget` are all EXPERIMENTAL** in Chrome 151's own `/json/protocol`. `SystemInfo.getProcessInfo` returns only `{type, id, cpuTime}` — **no memory field**; per-process RSS must come from the OS, not CDP.

---

## Open questions for the owner

1. **Is C (agent-authored plans + decision gates) acceptable?** It means `brow job start --detached "walk all pages"` is really `agent compiles plan → brow executes it`. If you want true autonomy with no agent process alive, the only local-first answer is Ollama, and that is a large, opinionated dependency. Decide before `crates/jobs` is written — it is the schema.
2. **Should the daemon ever be always-on, or is idle-exit(0) + auto-spawn the only mode?** Always-on costs ~15 MB RSS idle (no browser) but keeps launchd/systemd meaningful. Idle-exit makes the service unit nearly decorative.
3. **Per-project daemons:** ship `BROW_HOME` as the documented escape hatch, or actively support `brow --project` with a derived socket path? The former is one line; the latter multiplies Chromium RSS.
4. **Does `brow` ever *install* the service itself, or only print the plist/unit for the user to install?** Silent installation of a background item now triggers a macOS "Background item added" notification and shows up in Login Items; some users will find that presumptuous.
5. **Approval timeout default:** 30 min then `expired`, or wait forever? Forever is safer but leaks browser contexts and disk.
6. **How much of iteration-2's v1 protocol may break?** The v2 envelope is not wire-compatible. Bump `PROTOCOL_VERSION` to 2 and require a daemon restart, or dual-parse for one release?
7. **Windows scope for v1** — is "auto-spawn only, no service, no pipe transport" acceptable, or is Windows a v1 blocker? It is realistically 1–2 weeks of separate work.
8. **Do we ship `state.db` at all**, or keep everything in per-job JSON and accept slower `brow ps`? SQLite adds a build dependency (`bundled`) but buys atomic transitions.

---

## Sources

1. https://chromedevtools.github.io/devtools-protocol/tot/Emulation/#method-setVirtualTimePolicy — `setVirtualTimePolicy`, `VirtualTimePolicy` enum, EXPERIMENTAL, no documented disable
2. https://chromedevtools.github.io/devtools-protocol/tot/Target/ — `createBrowserContext`, `createTarget`, `disposeBrowserContext`
3. Local Chrome 151.0.7922.72 `/json/protocol` (1 605 774 bytes, 57 domains) — every method/param/EXPERIMENTAL flag quoted above was read from this file
4. https://docs.rs/tokio/latest/tokio/net/unix/struct.UCred.html — `uid()`/`gid()`/`pid()`, `pid()` on macOS, tokio 1.53.1
5. https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/index.html — Windows named pipes, feature `net`, no security-descriptor API
6. https://keith.github.io/xcode-man-pages/launchd.plist.5.html — `KeepAlive`/`SuccessfulExit`, `ThrottleInterval`, `ProcessType` values, `ExitTimeOut`, `Sockets`, `EnableTransactions`
7. https://www.launchd.info/ — `~/Library/LaunchAgents`, `bootstrap`/`bootout`/`kickstart` vs deprecated `load`/`unload`/`start`/`stop`
8. https://man7.org/linux/man-pages/man5/systemd.service.5.html — `Type=` values, `Restart=`, `RestartSec` default 100 ms, `WatchdogSec`, `NotifyAccess`
9. https://wiki.archlinux.org/title/Systemd/User — user units, `loginctl enable-linger`
10. https://modelcontextprotocol.io/specification/draft/client/sampling — **Sampling deprecated as of protocol version `2026-07-28` (SEP-2577); "New implementations SHOULD NOT adopt it"**
11. https://microsoft.github.io/language-server-protocol/specifications/specification-3-15/ — `$/cancelRequest` (cancelled requests must still respond), `$/progress` added in 3.15
12. https://learn.microsoft.com/en-us/windows/desktop/TaskSchd/logontrigger — Task Scheduler `LogonTrigger`
13. https://inventivehq.com/knowledge-base/macos/how-to-manage-launchagents-launchdaemons-macos — macOS 13+ Login Items & Extensions surfacing of third-party LaunchAgents
14. https://github.com/h4llow3En/mac-notification-sys and https://github.com/julienXX/terminal-notifier — `NSUserNotification` does not work from a Foundation tool; app-bundle requirement
15. https://docs.rs/notify-rust/ — notify-rust 4.18.0, macOS backend limitations
16. https://crates.io/api/v1/crates/{tokio,rusqlite,fs4,sysinfo,…} — all versions in the table, queried 2026-08-04
17. Local repo: `/Users/gmh-basket/pr/brow/src/{daemon,ipc,client,paths}.rs` — current v1 implementation, the `bind()` race, `peer_uid`, `spawn_daemon`, `PROTOCOL_VERSION`

---

## Verification pass — 2026-08-04 (adversarial re-check)

macOS 26.5.1 / Darwin 25.5.0, Chrome **151.0.7922.72**. Scratch `--user-data-dir` under `/private/tmp/browverify`; all Chromes killed, the test LaunchAgent booted out and its plist deleted (`~/Library/LaunchAgents` verified byte-identical afterwards).

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| 1 | Chrome always dies ≤1 s of daemon death; **orphans structurally impossible** | **PARTIAL → the second half is REFUTED** | fd-3 EOF → `EXITED after 1.01s rc=0` (2/2), and `1.00s` even with a spinning renderer. **But** with another child holding an inherited copy of the fd-3 write end → `STILL ALIVE after 40.2s <== ORPHAN`. Also narrowed: closing only the fd-4 read end → `STILL ALIVE after 45.3s`, so fd-3 EOF is the sole trigger |
| 2 | `Page.setWebLifecycleState{"frozen"}` is the right pause primitive | **REFUTED as a pause** | `Page.captureScreenshot` fires the page's `resume` and restarts timers (`n 120→126`), never re-freezing. In-flight `fetch` resolved during the freeze (`window.F == "SLOWDONE"`). Screenshots/DOM/evaluate all work while frozen. Control with no CDP traffic: freeze held, `n=0` after 4 s |
| 3 | `--renderer-process-limit` is not honoured | **CONFIRMED** (2nd independent run) | limit=2, 5 contexts/targets → `{renderer:7, browser:1, gpu:1, NetworkService:1, StorageService:1}` |
| 4 | 1 072 MB RSS is the planning figure; RSS *overstates* real memory | **REFUTED (direction inverted)** | sum RSS **1 364 MB** vs sum `phys_footprint` **2 030 MB** (0.67×). RSS *understates*. Use ~2 GB/session; the conservative `max_running_jobs=2` default is justified |
| 5 | `osascript` notifications from a launchd `ProcessType=Background` agent | **CONFIRMED** | Real LaunchAgent, `tty=not a tty`, `osascript_exit=0` twice, `state = running` |
| 6 | Second `launchctl bootstrap` fails with exit 5 | **CONFIRMED** | `Bootstrap failed: 5: Input/output error`, exit 5; `bootout` exit 0 |
| 7 | MCP sampling deprecated in revision `2026-07-28` (SEP-2577) | **CONFIRMED verbatim, with nuance** | Spec page quote matches exactly. **Missed:** it "remains in the specification for **at least twelve months**" (so ≥2027-07-28), and the delivery shape changed to `InputRequiredResult.inputRequests` + `sampling.tools`. "Dead" → "deprecated and reshaped" |
| 8 | `Emulation.setVirtualTimePolicy` has no `disable` | **CONFIRMED** | Protocol JSON: `VirtualTimePolicy` enum is exactly `["advance","pause","pauseIfNetworkFetchesPending"]`, domain + command EXPERIMENTAL |
| 9 | `SystemInfo.getProcessInfo` has no memory field | **CONFIRMED** | `ProcessInfo` properties are exactly `type`, `id`, `cpuTime` |
| 10 | `Page.setWebLifecycleState` is EXPERIMENTAL | **CONFIRMED** | `experimental: true`, `state` enum `["frozen","active"]` |
| 11 | `Target.createBrowserContext` not experimental, all 4 params are | **CONFIRMED** | `disposeOnDetach`, `proxyServer`, `proxyBypassList`, `originsWithUniversalNetworkAccess` all `experimental: true` |
| 12 | Crate versions (tokio 1.53.1, rusqlite 0.40.1, fs4 1.1.0, sysinfo 0.39.6, notify-rust 4.18.0, …) | **CONFIRMED** | crates.io API re-queried 2026-08-04 |

**Not re-tested:** the `flock`/`bind` TOCTOU race, `Target.disposeBrowserContext` timings, systemd/Linux behaviour, Windows Task Scheduler.
