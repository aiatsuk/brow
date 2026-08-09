//! `browd`: the resident process that owns the browsers.
//!
//! The daemon exists so that a browser outlives any single agent invocation. An
//! agent runs for one turn; a login session, a half-filled form and a 40-minute
//! crawl do not. Keeping Chromium here also means exactly one process in the
//! system speaks CDP, which is what lets us make "no raw protocol access" true by
//! construction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock};

use crate::browser::{self, launch::Headless, LaunchOptions, Launched};
use crate::ipc::{Hello, Request, Response, ShotTarget, Target, PROTOCOL_VERSION};
use crate::jobs;
use crate::page::{
    capture, ImageFormat, MouseButton, Page, PageError, Point, PointTarget, ScreenshotTarget,
};
use crate::paths;

const MAX_CLICK_COUNT: i64 = 10;
const MAX_GESTURE_DURATION_MS: u64 = 120_000;
const MAX_GESTURE_STEPS: u32 = 1_000;

struct Session {
    launched: Launched,
    page: Page,
    profile: PathBuf,
}

/// A named browser's mutable CDP state and the immutable fields needed to list it.
///
/// `state` is the only lock held while talking to that browser. The registry only
/// stores these handles, so a slow command in one session never stalls another
/// session (or daemon status/shutdown).
struct SessionSlot {
    state: Arc<Mutex<Option<Session>>>,
    summary: OnceLock<SessionSummary>,
    generation: AtomicU64,
    closing: AtomicBool,
    closed: tokio::sync::watch::Sender<bool>,
}

struct SessionSummary {
    product: String,
    headless: bool,
    opened_at: SystemTime,
}

struct SessionGuard {
    state: tokio::sync::OwnedMutexGuard<Option<Session>>,
    slot: Arc<SessionSlot>,
}

impl std::ops::Deref for SessionGuard {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        self.state
            .as_ref()
            .expect("a SessionGuard is only built for a live session")
    }
}

impl std::ops::DerefMut for SessionGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state
            .as_mut()
            .expect("a SessionGuard is only built for a live session")
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(session) = self.state.as_ref() {
            self.slot
                .generation
                .store(session.page.generation(), Ordering::Release);
        }
    }
}

impl SessionSlot {
    fn new() -> Self {
        let (closed, _) = tokio::sync::watch::channel(false);
        Self {
            state: Arc::new(Mutex::new(None)),
            summary: OnceLock::new(),
            generation: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            closed,
        }
    }

    async fn wait_closed(&self) {
        let mut closed = self.closed.subscribe();
        if !*closed.borrow() {
            let _ = closed.changed().await;
        }
    }
}

pub struct Daemon {
    sessions: Mutex<HashMap<String, Arc<SessionSlot>>>,
    jobs: Mutex<jobs::JobStore>,
    started: Instant,
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Read permits allow normal requests to run concurrently. Shutdown takes
    /// the sole write permit, waits for every accepted operation to finish, then
    /// permanently closes admission before cleanup starts.
    lifecycle: RwLock<()>,
    shutting_down: AtomicBool,
}

/// Starts the daemon, serving until asked to stop.
pub async fn serve() -> anyhow::Result<()> {
    paths::ensure_layout()?;
    let socket_path = paths::socket();

    // Held until this function returns, which is what makes startup single-file.
    let _lock = acquire_lock()?;
    let listener = bind(&socket_path).await?;
    let _ = std::fs::write(paths::pid_file(), std::process::id().to_string());

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    // Jobs from previous daemon lifetimes come back as `interrupted` with their
    // logs intact, so `brow job status` can still explain what happened.
    let previous = jobs::load_previous(&paths::jobs_root()).await;
    if !previous.is_empty() {
        tracing::info!(count = previous.len(), "recovered job manifests");
    }
    let daemon = Arc::new(Daemon {
        sessions: Mutex::new(HashMap::new()),
        jobs: Mutex::new(jobs::JobStore {
            finished: previous,
            ..Default::default()
        }),
        started: Instant::now(),
        shutdown: shutdown_tx.clone(),
        lifecycle: RwLock::new(()),
        shutting_down: AtomicBool::new(false),
    });

    tracing::info!(socket = %socket_path.display(), pid = std::process::id(), "browd listening");

    let signals = {
        let shutdown = shutdown_tx.clone();
        async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("signal received, shutting down");
            let _ = shutdown.send(true);
        }
    };
    tokio::spawn(signals);

    let mut connections = tokio::task::JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let daemon = Arc::clone(&daemon);
                        connections.spawn(async move {
                            if let Err(e) = handle_conn(stream, daemon).await {
                                tracing::debug!(error = %e, "connection ended");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "accept failed"),
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Dispatch shutdown waits for all active requests via `lifecycle`, so tasks
    // here are only flushing their final response or idling on a keep-alive
    // socket. Give the shutdown caller a brief flush window, then abort idlers.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}

    // Take every browser down with us rather than leaking Chromium processes.
    daemon.stop_all_jobs().await;
    // Close them concurrently: a command already running in one session must
    // not delay cleanup of every other browser.
    daemon.close_all_sessions().await;

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(paths::pid_file());
    tracing::info!("browd stopped");
    Ok(())
}

/// An exclusive lock held for the daemon's lifetime.
///
/// Dropped — including on a crash, because the kernel releases `flock` when the
/// file descriptor closes — so there is no stale lock to clean up.
// The file is never read: holding it open *is* the lock, and dropping it releases.
struct StartupLock(#[allow(dead_code)] std::fs::File);

/// Takes the single-instance lock, or reports who holds it.
fn acquire_lock() -> anyhow::Result<StartupLock> {
    let path = paths::root().join("run").join("browd.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    // SAFETY: `file` is open for the duration of the call and outlives the lock.
    let rc = unsafe {
        use std::os::unix::io::AsRawFd;
        libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
    };
    if rc != 0 {
        anyhow::bail!("another browd is already starting or running");
    }
    Ok(StartupLock(file))
}

/// Binds the control socket, clearing a stale one from a crashed daemon.
///
/// The stale-socket dance — connect, fail, unlink, bind — races against itself:
/// two daemons starting together both fail to connect, both unlink, and the
/// second unlink deletes the first one's freshly bound socket, leaving a daemon
/// nobody can reach. The lock is taken before any of that so only one process is
/// ever inside this function.
async fn bind(path: &std::path::Path) -> anyhow::Result<UnixListener> {
    if path.exists() {
        // A socket file that nobody is listening on is left over from a crash.
        // One that answers means a daemon is already running, and we must not
        // steal its socket.
        match UnixStream::connect(path).await {
            Ok(_) => anyhow::bail!("another browd is already listening on {}", path.display()),
            Err(_) => {
                tracing::warn!(socket = %path.display(), "removing stale socket");
                let _ = std::fs::remove_file(path);
            }
        }
    }

    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(listener)
}

/// Returns the uid of the process on the other end of a Unix socket.
#[cfg(unix)]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `fd` is a live socket; `cred`/`len` are correctly sized.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0).then_some(cred.uid)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `fd` is a live socket; both out-params are valid for writes.
        let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        (rc == 0).then_some(uid)
    }
}

async fn handle_conn(stream: UnixStream, daemon: Arc<Daemon>) -> anyhow::Result<()> {
    // Defence in depth behind the 0700 directory: only this user may drive a
    // browser that holds this user's logged-in sessions.
    #[cfg(unix)]
    if let Some(uid) = peer_uid(&stream) {
        // SAFETY: getuid cannot fail.
        let ours = unsafe { libc::getuid() };
        if uid != ours {
            anyhow::bail!("rejecting connection from uid {uid}");
        }
    }

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let hello = Hello {
        brow: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL_VERSION,
        pid: std::process::id(),
    };
    write_half
        .write_all(format!("{}\n", serde_json::to_string(&hello)?).as_bytes())
        .await?;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => daemon.dispatch(req).await,
            Err(e) => Response::error_hint(
                format!("unparseable request: {e}"),
                "this is a bug in the brow CLI, or a version mismatch — try `brow daemon restart`",
            ),
        };
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
            .await?;
    }
    Ok(())
}

impl Daemon {
    async fn dispatch(&self, req: Request) -> Response {
        if matches!(&req, Request::Shutdown) {
            let _exclusive = self.lifecycle.write().await;
            let already = self.shutting_down.swap(true, Ordering::AcqRel);
            let _ = self.shutdown.send(true);
            return Response::ok_text(
                json!({"stopping": true, "already_stopping": already}),
                "browd stopping",
            );
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Response::error("browd is shutting down; request was not started");
        }
        let _active = self.lifecycle.read().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Response::error("browd is shutting down; request was not started");
        }
        match req {
            Request::Ping => Response::ok_text(json!({"pong": true}), "pong"),
            Request::Shutdown => {
                unreachable!("shutdown is handled before normal request admission")
            }
            Request::Status => {
                let uptime = self.started.elapsed().as_secs();
                let sessions = self
                    .sessions
                    .lock()
                    .await
                    .values()
                    .filter(|slot| {
                        slot.summary.get().is_some() && !slot.closing.load(Ordering::Acquire)
                    })
                    .count();
                Response::ok_text(
                    json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "pid": std::process::id(),
                        "uptime_seconds": uptime,
                        "sessions": sessions,
                    }),
                    format!(
                        "browd {} · pid {} · up {}s · {} session(s)",
                        env!("CARGO_PKG_VERSION"),
                        std::process::id(),
                        uptime,
                        sessions
                    ),
                )
            }
            Request::Sessions => {
                let mut rows = Vec::new();
                let mut text = String::new();
                let slots: Vec<(String, Arc<SessionSlot>)> = self
                    .sessions
                    .lock()
                    .await
                    .iter()
                    .map(|(name, slot)| (name.clone(), Arc::clone(slot)))
                    .collect();
                for (name, slot) in slots {
                    if slot.closing.load(Ordering::Acquire) {
                        continue;
                    }
                    let Some(summary) = slot.summary.get() else {
                        continue;
                    };
                    let generation = match slot.state.try_lock() {
                        Ok(state) => state
                            .as_ref()
                            .map(|session| session.page.generation())
                            .unwrap_or_else(|| slot.generation.load(Ordering::Acquire)),
                        Err(_) => slot.generation.load(Ordering::Acquire),
                    };
                    let age = summary
                        .opened_at
                        .elapsed()
                        .map(|d| d.as_secs())
                        .unwrap_or_default();
                    rows.push(json!({
                        "session": name,
                        "headless": summary.headless,
                        "product": summary.product,
                        "generation": generation,
                        "age_seconds": age,
                    }));
                    text.push_str(&format!(
                        "{name}  {}  generation {}  {}s\n",
                        if summary.headless {
                            "headless"
                        } else {
                            "headed"
                        },
                        generation,
                        age
                    ));
                }
                if text.is_empty() {
                    text.push_str("no open sessions\n");
                }
                Response::ok_text(json!(rows), text)
            }
            Request::Open {
                url,
                session,
                headless,
            } => self.open(&session, &url, headless).await,
            Request::Close { session } => self.close(&session).await,
            Request::Snapshot {
                session,
                interactive,
            } => {
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.snapshot().await {
                    Ok(snap) => {
                        let text = snap.render_text(interactive);
                        let count = snap.nodes.len();
                        let interactive_count = snap.interactive().count();
                        Response::ok_text(
                            json!({
                                "generation": snap.generation,
                                "url": snap.url,
                                "title": snap.title,
                                "nodes": snap.nodes,
                                "coverage_gaps": snap.coverage_gaps,
                                "counts": { "total": count, "interactive": interactive_count },
                            }),
                            text,
                        )
                    }
                    Err(e) => page_error(e),
                }
            }
            Request::Click {
                session,
                target,
                button,
                count,
                modifiers,
                force,
            } => {
                if !(1..=MAX_CLICK_COUNT).contains(&count) {
                    return Response::error(format!(
                        "click count must be between 1 and {MAX_CLICK_COUNT}, got {count}"
                    ));
                }
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let button = parse_button(&button);
                match target {
                    Target::Ref { node_ref } => {
                        match s
                            .page
                            .click(&node_ref, button, count, modifiers, force)
                            .await
                        {
                            Ok(p) => Response::ok_text(
                                json!({"clicked": node_ref, "x": p.x, "y": p.y}),
                                format!("clicked {node_ref} at {:.0},{:.0}", p.x, p.y),
                            ),
                            Err(e) => page_error(e),
                        }
                    }
                    Target::Point { x, y } => {
                        match s
                            .page
                            .click_at(Point { x, y }, button, count, modifiers)
                            .await
                        {
                            Ok(()) => Response::ok_text(
                                json!({"x": x, "y": y}),
                                format!("clicked {x:.0},{y:.0}"),
                            ),
                            Err(e) => page_error(e),
                        }
                    }
                }
            }
            Request::Hover { session, node_ref } => {
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.hover(&node_ref).await {
                    Ok(p) => Response::ok_text(
                        json!({"hovered": node_ref, "x": p.x, "y": p.y}),
                        format!("hovering {node_ref}"),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Fill {
                session,
                node_ref,
                text,
            } => {
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.fill(&node_ref, &text).await {
                    Ok(()) => {
                        Response::ok_text(json!({"filled": node_ref}), format!("filled {node_ref}"))
                    }
                    Err(e) => page_error(e),
                }
            }
            Request::Press { session, chord } => {
                let s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.press(&chord).await {
                    Ok(()) => {
                        Response::ok_text(json!({"pressed": chord}), format!("pressed {chord}"))
                    }
                    Err(e) => page_error(e),
                }
            }
            Request::Type {
                session,
                text,
                by_key,
            } => {
                let s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.type_text(&text, by_key).await {
                    Ok(()) => Response::ok_text(
                        json!({"typed": text.chars().count()}),
                        format!("typed {} characters", text.chars().count()),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Scroll { session, dx, dy } => {
                let s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.scroll(dx, dy).await {
                    Ok(()) => Response::ok_text(
                        json!({"dx": dx, "dy": dy}),
                        format!("scrolled {dx:.0},{dy:.0}"),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Screenshot {
                session,
                target,
                format,
                quality,
                out,
            } => {
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let Some(fmt) = ImageFormat::parse(&format) else {
                    return Response::error(format!("unknown image format {format:?}"));
                };
                let target = match target {
                    ShotTarget::Viewport => ScreenshotTarget::Viewport,
                    ShotTarget::FullPage => ScreenshotTarget::FullPage,
                    ShotTarget::Node { node_ref } => ScreenshotTarget::Node(node_ref),
                    ShotTarget::Rect {
                        x,
                        y,
                        width,
                        height,
                    } => ScreenshotTarget::Rect(capture::Clip {
                        x,
                        y,
                        width,
                        height,
                    }),
                };
                let shot = match s.page.screenshot(target, fmt, quality).await {
                    Ok(shot) => shot,
                    Err(e) => return page_error(e),
                };
                let path = match out {
                    Some(p) => PathBuf::from(p),
                    None => paths::artifacts(&session).join(format!(
                        "shot-{}.{}",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0),
                        fmt.extension()
                    )),
                };
                match shot.write_to(&path).await {
                    Ok(written) => {
                        let mut text = format!("wrote {}", written.display());
                        if let Some(warning) = &shot.truncated {
                            text.push_str(&format!("\nnote: {warning}"));
                        }
                        if shot.tiled {
                            text.push_str(&format!(
                                "\nnote: stitched full-page capture from {} tiles",
                                shot.tile_count
                            ));
                        }
                        Response::ok_text(
                            json!({
                                "path": written,
                                "bytes": shot.bytes.len(),
                                "truncated": shot.truncated,
                                "tiled": shot.tiled,
                                "tile_count": shot.tile_count,
                            }),
                            text,
                        )
                    }
                    Err(e) => Response::error(format!("could not write {}: {e}", path.display())),
                }
            }
            Request::JobStart {
                intent,
                steps,
                headless,
            } => self.job_start(intent, steps, headless).await,
            Request::JobList => {
                let mut rows = Vec::new();
                let mut text = String::new();
                // Running first, then history, newest last.
                let (finished, records) = {
                    let store = self.jobs.lock().await;
                    (
                        store.finished.clone(),
                        store
                            .handles
                            .values()
                            .map(|handle| Arc::clone(&handle.record))
                            .collect::<Vec<_>>(),
                    )
                };
                let mut live: Vec<jobs::JobRecord> = Vec::new();
                for record in records {
                    live.push(record.lock().await.clone());
                }
                live.sort_by_key(|r| r.created_ms);
                for r in finished.into_iter().chain(live) {
                    text.push_str(&format!(
                        "{}  {:<20} step {}/{}  {}\n",
                        r.id,
                        r.state.label(),
                        r.cursor.min(r.steps.len()),
                        r.steps.len(),
                        r.intent
                    ));
                    rows.push(json!({
                        "id": r.id,
                        "state": r.state.label(),
                        "cursor": r.cursor,
                        "steps": r.steps.len(),
                        "intent": r.intent,
                    }));
                }
                if text.is_empty() {
                    text.push_str("no jobs\n");
                }
                Response::ok_text(json!(rows), text)
            }
            Request::JobStatus { id, log_from } => match self.job_record(&id).await {
                Some(r) => {
                    let tail: Vec<&jobs::LogLine> = r.log.iter().skip(log_from).collect();
                    let mut text = r.render_status();
                    for line in &tail {
                        text.push_str(&format!("  {}\n", line.text));
                    }
                    Response::ok_text(
                        json!({
                            "id": r.id,
                            "state": r.state.label(),
                            "terminal": r.state.is_terminal(),
                            "parked": r.state.is_parked(),
                            "cursor": r.cursor,
                            "steps": r.steps.len(),
                            "intent": r.intent,
                            "pending": r.pending,
                            "error": r.error,
                            "artifacts": r.artifacts,
                            "log": tail,
                            "log_total": r.log.len(),
                        }),
                        text,
                    )
                }
                None => no_job(&id),
            },
            Request::JobAnswer { id, answer } => {
                self.job_control(
                    &id,
                    jobs::Control::Answer(answer),
                    jobs::JobState::NeedsDecision,
                    "is not waiting for a decision",
                )
                .await
            }
            Request::JobApprove { id, reject } => {
                let control = if reject {
                    jobs::Control::Reject
                } else {
                    jobs::Control::Approve
                };
                self.job_control(
                    &id,
                    control,
                    jobs::JobState::WaitingForApproval,
                    "is not waiting for approval",
                )
                .await
            }
            Request::JobStop { id } => self.job_stop(&id).await,
            Request::Console {
                session,
                errors,
                limit,
            } => {
                let s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let rows = s.page.events.console(errors, limit.clamp(1, 5_000));
                let (dropped, _) = s.page.events.dropped();
                let event_stream_gaps = s.page.events.event_stream_gaps();
                let mut text = rows
                    .iter()
                    .map(|r| r.render())
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    text = if errors {
                        "no console errors".into()
                    } else {
                        "console is empty".into()
                    };
                }
                if dropped > 0 {
                    text.push_str(&format!("\n({dropped} older entries dropped)"));
                }
                if event_stream_gaps > 0 {
                    text.push_str(&format!(
                        "\n({event_stream_gaps} upstream CDP event(s) unavailable; results are incomplete)"
                    ));
                }
                Response::ok_text(
                    json!({
                        "entries": rows,
                        "dropped": dropped,
                        "event_stream_gaps": event_stream_gaps,
                        "complete": event_stream_gaps == 0,
                    }),
                    text,
                )
            }
            Request::Network {
                session,
                failed,
                limit,
            } => {
                let s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let rows = s.page.events.network(failed, limit.clamp(1, 5_000));
                let (_, dropped) = s.page.events.dropped();
                let event_stream_gaps = s.page.events.event_stream_gaps();
                let mut text = rows
                    .iter()
                    .map(|r| r.render())
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    text = if failed {
                        "no failed requests".into()
                    } else {
                        "no requests recorded".into()
                    };
                }
                if dropped > 0 {
                    text.push_str(&format!("\n({dropped} older requests dropped)"));
                }
                if event_stream_gaps > 0 {
                    text.push_str(&format!(
                        "\n({event_stream_gaps} upstream CDP event(s) unavailable; results are incomplete)"
                    ));
                }
                Response::ok_text(
                    json!({
                        "requests": rows,
                        "dropped": dropped,
                        "event_stream_gaps": event_stream_gaps,
                        "complete": event_stream_gaps == 0,
                    }),
                    text,
                )
            }
            Request::Tap { session, target } => {
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let target = point_target(target);
                match s.page.tap(&target).await {
                    Ok(p) => Response::ok_text(
                        json!({"x": p.x, "y": p.y}),
                        format!("tapped {:.0},{:.0}", p.x, p.y),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::LongPress {
                session,
                target,
                duration_ms,
            } => {
                if duration_ms > MAX_GESTURE_DURATION_MS {
                    return Response::error(format!(
                        "gesture duration must be at most {MAX_GESTURE_DURATION_MS}ms"
                    ));
                }
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let target = point_target(target);
                match s
                    .page
                    .long_press(&target, std::time::Duration::from_millis(duration_ms))
                    .await
                {
                    Ok(p) => Response::ok_text(
                        json!({"x": p.x, "y": p.y, "duration_ms": duration_ms}),
                        format!("long-pressed {:.0},{:.0} for {duration_ms}ms", p.x, p.y),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Swipe {
                session,
                from,
                to,
                duration_ms,
                steps,
            } => {
                if duration_ms > MAX_GESTURE_DURATION_MS
                    || !(2..=MAX_GESTURE_STEPS).contains(&steps)
                {
                    return Response::error(format!(
                        "swipe requires duration <= {MAX_GESTURE_DURATION_MS}ms and 2..={MAX_GESTURE_STEPS} steps"
                    ));
                }
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let (a, b) = (point_target(from), point_target(to));
                match s
                    .page
                    .swipe(&a, &b, std::time::Duration::from_millis(duration_ms), steps)
                    .await
                {
                    Ok((a, b)) => Response::ok_text(
                        json!({"from": {"x": a.x, "y": a.y}, "to": {"x": b.x, "y": b.y}}),
                        format!(
                            "swiped {:.0},{:.0} → {:.0},{:.0} over {duration_ms}ms",
                            a.x, a.y, b.x, b.y
                        ),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Pinch {
                session,
                center,
                scale,
                speed,
            } => {
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let center = point_target(center);
                match s.page.pinch(&center, scale, speed).await {
                    Ok(p) => Response::ok_text(
                        json!({"x": p.x, "y": p.y, "scale": scale}),
                        format!("pinched {scale}× at {:.0},{:.0}", p.x, p.y),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Drag {
                session,
                from,
                to,
                duration_ms,
                steps,
            } => {
                if duration_ms > MAX_GESTURE_DURATION_MS
                    || !(2..=MAX_GESTURE_STEPS).contains(&steps)
                {
                    return Response::error(format!(
                        "drag requires duration <= {MAX_GESTURE_DURATION_MS}ms and 2..={MAX_GESTURE_STEPS} steps"
                    ));
                }
                let mut s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                let (a, b) = (point_target(from), point_target(to));
                match s
                    .page
                    .drag(&a, &b, std::time::Duration::from_millis(duration_ms), steps)
                    .await
                {
                    Ok((a, b)) => Response::ok_text(
                        json!({"from": {"x": a.x, "y": a.y}, "to": {"x": b.x, "y": b.y}}),
                        format!("dragged {:.0},{:.0} → {:.0},{:.0}", a.x, a.y, b.x, b.y),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Eval {
                session,
                expression,
                mutate,
            } => {
                let s = match self.lock_session(&session).await {
                    Ok(s) => s,
                    Err(response) => return response,
                };
                match s.page.evaluate(&expression, !mutate).await {
                    Ok(v) => {
                        let text = match &v {
                            Value::String(s) => s.clone(),
                            other => serde_json::to_string_pretty(other).unwrap_or_default(),
                        };
                        Response::ok_text(v, text)
                    }
                    Err(e) => page_error(e),
                }
            }
        }
    }

    async fn open(&self, session: &str, url: &str, headless: bool) -> Response {
        if let Err(error) = paths::ensure_session_storage_compatible(session) {
            return Response::error(format!("cannot open session {session:?}: {error}"));
        }
        loop {
            // Lock a fresh slot before publishing it. A concurrent request can
            // discover the handle immediately, but it cannot observe the
            // half-built browser inside it.
            let candidate = Arc::new(SessionSlot::new());
            let opening = Arc::clone(&candidate.state).lock_owned().await;
            let (slot, opening) = {
                let mut sessions = self.sessions.lock().await;
                match sessions.entry(session.to_string()) {
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        (Arc::clone(entry.get()), None)
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(Arc::clone(&candidate));
                        (candidate, Some(opening))
                    }
                }
            };

            let Some(mut state) = opening else {
                // An `open` that races with `close` is ordered after the close:
                // wait for the old browser/profile to be fully gone, then retry
                // against a fresh slot.
                if slot.closing.load(Ordering::Acquire) {
                    slot.wait_closed().await;
                    continue;
                }
                let mut live = Arc::clone(&slot.state).lock_owned().await;
                if slot.closing.load(Ordering::Acquire) {
                    drop(live);
                    slot.wait_closed().await;
                    continue;
                }
                let Some(existing) = live.as_mut() else {
                    drop(live);
                    self.retire_session_slot(session, &slot).await;
                    continue;
                };
                let response = match existing.page.navigate(url).await {
                    Ok(()) => Response::ok_text(
                        json!({"session": session, "url": url, "reused": true}),
                        format!("{session}: {url}"),
                    ),
                    Err(e) => page_error(e),
                };
                slot.generation
                    .store(existing.page.generation(), Ordering::Release);
                return response;
            };

            let profile = paths::profile(session);
            let mut opts = LaunchOptions::new(profile.clone());
            opts.headless = if headless {
                Headless::New
            } else {
                Headless::Off
            };

            let launched = match browser::launch(&opts).await {
                Ok(launched) => launched,
                Err(error) => {
                    drop(state);
                    self.retire_session_slot(session, &slot).await;
                    return Response::error_hint(
                        format!("could not start a browser: {error}"),
                        "set BROW_CHROME to a Chromium binary if it is installed somewhere unusual",
                    );
                }
            };

            let page = match Page::create(Arc::clone(&launched.client), url).await {
                Ok(page) => page,
                Err(error) => {
                    let mut launched = launched;
                    let _ = launched.child.kill();
                    let _ = launched.child.wait();
                    drop(state);
                    self.retire_session_slot(session, &slot).await;
                    return page_error(error);
                }
            };

            let product = launched.product.clone();
            let opened_at = SystemTime::now();
            let created = Session {
                launched,
                page,
                profile,
            };

            if slot.closing.load(Ordering::Acquire) {
                close_browser(session, created).await;
                drop(state);
                self.retire_session_slot(session, &slot).await;
                return Response::error(format!("session {session:?} was closed while opening"));
            }

            slot.generation
                .store(created.page.generation(), Ordering::Release);
            let _ = slot.summary.set(SessionSummary {
                product: product.clone(),
                headless,
                opened_at,
            });
            *state = Some(created);

            return Response::ok_text(
                json!({"session": session, "url": url, "product": product, "reused": false}),
                format!("{session}: {url}  ({product})"),
            );
        }
    }

    /// Starts a job in its own browser and returns without waiting for it.
    ///
    /// The job's browser is *not* one of the named sessions: a job runs
    /// unattended for a long time and must not have its page navigated out from
    /// under it by an interactive command. It also gets its own window, because
    /// only one page per browser window is `visible` and a background tab renders
    /// nothing.
    async fn job_start(&self, intent: String, steps: Vec<String>, headless: bool) -> Response {
        let parsed: Result<Vec<jobs::Step>, String> =
            steps.iter().map(|s| jobs::Step::parse(s)).collect();
        let steps = match parsed {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => return Response::error("a job needs at least one --step"),
            Err(e) => {
                return Response::error_hint(e, "run `brow job start --help` for the step syntax")
            }
        };

        let seq = {
            let mut store = self.jobs.lock().await;
            store.seq += 1;
            store.seq
        };
        let id = jobs::new_job_id(seq);
        let artifacts = paths::job(&id);
        if let Err(e) = tokio::fs::create_dir_all(&artifacts).await {
            return Response::error(format!("could not create {}: {e}", artifacts.display()));
        }

        let mut opts = LaunchOptions::new(artifacts.join("profile"));
        opts.headless = if headless {
            Headless::New
        } else {
            Headless::Off
        };
        let launched = match browser::launch(&opts).await {
            Ok(l) => l,
            Err(e) => return Response::error(format!("could not start a browser: {e}")),
        };
        let page = match Page::create(Arc::clone(&launched.client), "about:blank").await {
            Ok(p) => p,
            Err(e) => {
                let mut launched = launched;
                let _ = launched.child.kill();
                let _ = launched.child.wait();
                return page_error(e);
            }
        };

        let record = Arc::new(Mutex::new(jobs::JobRecord {
            id: id.clone(),
            intent: intent.clone(),
            state: jobs::JobState::Queued,
            steps,
            cursor: 0,
            log: Vec::new(),
            artifacts: artifacts.clone(),
            pending: None,
            error: None,
            created_ms: jobs::now_ms(),
        }));
        // Depth 1: control messages are rare, and a backlog would mean answers
        // arriving for questions the job has already given up on.
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(1);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let action_gate = Arc::new(tokio::sync::Mutex::new(()));

        let runner = jobs::Runner {
            record: Arc::clone(&record),
            control: control_rx,
            stop: stop_rx,
            action_gate: Arc::clone(&action_gate),
            next_pending_id: 0,
        };
        let mut page = page;
        let mut launched = launched;
        tokio::spawn(async move {
            runner.run(&mut page).await;
            // The browser belongs to the job, so it goes when the job does.
            let _ = launched.child.kill();
            let _ = launched.child.wait();
        });

        self.jobs.lock().await.handles.insert(
            id.clone(),
            jobs::JobHandle {
                record,
                control: control_tx,
                stop: stop_tx,
                action_gate,
            },
        );

        Response::ok_text(
            json!({ "id": id, "artifacts": artifacts, "state": "queued" }),
            format!("{id} started\nwatch it with: brow job logs {id} --follow"),
        )
    }

    /// Current record for a job, live or historical.
    async fn job_record(&self, id: &str) -> Option<jobs::JobRecord> {
        let (live, finished) = {
            let store = self.jobs.lock().await;
            (
                store
                    .handles
                    .get(id)
                    .map(|handle| Arc::clone(&handle.record)),
                store
                    .finished
                    .iter()
                    .find(|record| record.id == id)
                    .cloned(),
            )
        };
        if let Some(live) = live {
            let record = live.lock().await.clone();
            // Reaping on read keeps the live table from accumulating handles for
            // jobs that finished while nobody was looking.
            if record.state.is_terminal() {
                self.reap_job(id).await;
            }
            return Some(record);
        }
        finished
    }

    /// Moves a finished job out of the live table so its handle is released.
    async fn reap_job(&self, id: &str) {
        let record = {
            let store = self.jobs.lock().await;
            store
                .handles
                .get(id)
                .map(|handle| Arc::clone(&handle.record))
        };
        let Some(record) = record else {
            return;
        };
        let finished = record.lock().await.clone();
        if !finished.state.is_terminal() {
            return;
        }

        let mut store = self.jobs.lock().await;
        let same_record = store
            .handles
            .get(id)
            .map(|handle| Arc::ptr_eq(&handle.record, &record))
            .unwrap_or(false);
        if same_record {
            store.handles.remove(id);
            store.finished.push(finished);
        }
    }

    async fn job_stop(&self, id: &str) -> Response {
        let handle = {
            let store = self.jobs.lock().await;
            store.handles.get(id).map(|handle| {
                (
                    Arc::clone(&handle.record),
                    handle.stop.clone(),
                    Arc::clone(&handle.action_gate),
                )
            })
        };
        let Some((record, stop, action_gate)) = handle else {
            return no_job(id);
        };

        // Both paths, because a job is either parked (reading control) or
        // mid-step (watching the stop signal), and we do not know which.
        let _ = stop.send(true);
        let _action = action_gate.lock().await;
        let mut current = record.lock().await;
        if current.state.is_terminal() {
            let state = current.state;
            drop(current);
            self.reap_job(id).await;
            if state == jobs::JobState::Stopped {
                return Response::ok_text(
                    json!({ "stopped": id, "already_stopped": true }),
                    format!("stopped {id}"),
                );
            }
            return Response::error(format!("{id} is already {}", state.label()));
        }
        current.state = jobs::JobState::Stopped;
        let persisted = jobs::persist_or_mark_failed(&mut current).await;
        let persistence_error = (!persisted)
            .then(|| current.error.clone())
            .flatten()
            .unwrap_or_else(|| "job manifest persistence failed".to_string());
        drop(current);
        self.reap_job(id).await;

        if !persisted {
            return Response::error_hint(
                format!("could not durably stop {id}: {persistence_error}"),
                "the in-memory job is failed and its browser is being closed; inspect the artifact directory before restarting the daemon",
            );
        }
        Response::ok_text(json!({ "stopped": id }), format!("stopped {id}"))
    }

    /// Delivers a control message, refusing it if the job is not waiting for
    /// that particular kind of answer.
    async fn job_control(
        &self,
        id: &str,
        control: jobs::Control,
        expected: jobs::JobState,
        complaint: &str,
    ) -> Response {
        let handle = {
            let store = self.jobs.lock().await;
            store
                .handles
                .get(id)
                .map(|handle| (Arc::clone(&handle.record), handle.control.clone()))
        };
        let Some((record, sender)) = handle else {
            return no_job(id);
        };
        let current = record.lock().await;
        let state = current.state;
        if state != expected {
            // Answering the wrong kind of park is the mistake worth catching: an
            // agent must not be able to satisfy a human approval gate.
            return Response::error_hint(
                format!("{id} {complaint} (it is {})", state.label()),
                match expected {
                    jobs::JobState::WaitingForApproval => {
                        "approvals are for a human to give; an agent answers decisions with \
                         `brow job answer`"
                    }
                    _ => "check `brow job status <id>` for what it is actually waiting on",
                },
            );
        }
        let Some(pending_id) = current.pending.as_ref().map(|pending| pending.id) else {
            return Response::error(format!(
                "{id} is {state_label} but has no current pending request",
                state_label = state.label()
            ));
        };
        // Control is intentionally non-blocking: a full channel means the job is
        // not ready for another answer yet.
        let sent = sender.try_send(jobs::ControlMessage {
            pending_id,
            control,
        });
        drop(current);
        match sent {
            Ok(()) => {
                Response::ok_text(json!({ "id": id, "delivered": true }), format!("{id}: ok"))
            }
            Err(_) => Response::error(format!("{id} stopped listening before the answer arrived")),
        }
    }

    async fn lock_session(&self, name: &str) -> Result<SessionGuard, Response> {
        let slot = self.sessions.lock().await.get(name).cloned();
        let Some(slot) = slot else {
            return Err(no_session(name));
        };
        if slot.closing.load(Ordering::Acquire) {
            return Err(no_session(name));
        }

        let state = Arc::clone(&slot.state).lock_owned().await;
        if slot.closing.load(Ordering::Acquire) || state.is_none() {
            return Err(no_session(name));
        }
        Ok(SessionGuard { state, slot })
    }

    async fn close(&self, name: &str) -> Response {
        let slot = {
            let sessions = self.sessions.lock().await;
            let Some(slot) = sessions.get(name) else {
                return Response::error(format!("no session named {name:?}"));
            };
            if slot
                .closing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Response::error(format!("no session named {name:?}"));
            }
            Arc::clone(slot)
        };

        finish_session_close(name, &slot).await;
        self.retire_session_slot(name, &slot).await;
        Response::ok_text(json!({"closed": name}), format!("closed {name}"))
    }

    async fn retire_session_slot(&self, name: &str, slot: &Arc<SessionSlot>) {
        let mut sessions = self.sessions.lock().await;
        let is_current = sessions
            .get(name)
            .map(|current| Arc::ptr_eq(current, slot))
            .unwrap_or(false);
        if is_current {
            sessions.remove(name);
        }
        drop(sessions);
        slot.closed.send_replace(true);
    }

    async fn close_all_sessions(&self) {
        let slots: Vec<(String, Arc<SessionSlot>)> = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(name, slot)| {
                    slot.closing.store(true, Ordering::Release);
                    (name, slot)
                })
                .collect()
        };

        let mut closes = tokio::task::JoinSet::new();
        for (name, slot) in slots {
            closes.spawn(async move {
                finish_session_close(&name, &slot).await;
                slot.closed.send_replace(true);
            });
        }
        while closes.join_next().await.is_some() {}
    }

    async fn stop_all_jobs(&self) {
        let jobs: Vec<_> = {
            let store = self.jobs.lock().await;
            store
                .handles
                .values()
                .map(|handle| (handle.stop.clone(), Arc::clone(&handle.record)))
                .collect()
        };
        if jobs.is_empty() {
            return;
        }
        for (stop, _) in &jobs {
            let _ = stop.send(true);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut all_terminal = true;
            for (_, record) in &jobs {
                if !record.lock().await.state.is_terminal() {
                    all_terminal = false;
                    break;
                }
            }
            if all_terminal {
                // The runner kills and waits for its owned Chromium immediately
                // after setting terminal state; yield once so that cleanup tail
                // can run before the runtime exits.
                tokio::task::yield_now().await;
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::error!(
                    count = jobs.len(),
                    "timed out waiting for job browsers to stop during daemon shutdown"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn finish_session_close(name: &str, slot: &SessionSlot) {
    let mut state = Arc::clone(&slot.state).lock_owned().await;
    if let Some(session) = state.take() {
        close_browser(name, session).await;
    }
}

async fn close_browser(name: &str, mut session: Session) {
    // Ask nicely first so the profile is flushed cleanly, then make sure.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.launched.client.call("Browser.close", json!({})),
    )
    .await;
    let _ = session.launched.child.kill();
    let _ = session.launched.child.wait();
    tracing::info!(session = name, profile = %session.profile.display(), "session closed");
}

fn no_job(id: &str) -> Response {
    Response::error_hint(
        format!("no job called {id:?}"),
        "run `brow job list` to see jobs, including ones from earlier daemon lifetimes",
    )
}

fn no_session(session: &str) -> Response {
    Response::error_hint(
        format!("no session named {session:?} is open"),
        format!("run `brow open <url>` first (use --session {session} to name it)"),
    )
}

fn point_target(t: Target) -> PointTarget {
    match t {
        Target::Ref { node_ref } => PointTarget::Ref(node_ref),
        Target::Point { x, y } => PointTarget::At(Point { x, y }),
    }
}

fn parse_button(s: &str) -> MouseButton {
    match s.to_ascii_lowercase().as_str() {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        "back" => MouseButton::Back,
        "forward" => MouseButton::Forward,
        _ => MouseButton::Left,
    }
}

/// Turns an internal error into something an agent can act on.
fn page_error(e: PageError) -> Response {
    let hint = match &e {
        PageError::NoSnapshot => Some("run `brow snapshot` to mint refs".to_string()),
        PageError::Ref(crate::page::RefError::Stale { .. }) => {
            Some("the page changed — take a fresh `brow snapshot` and use the new refs".to_string())
        }
        PageError::Ref(crate::page::RefError::Unknown(_)) => {
            Some("that ref is not in the latest snapshot — run `brow snapshot` again".to_string())
        }
        PageError::Action(crate::page::input::ActionError::Occluded { .. }) => Some(
            "something is covering the element; dismiss it, or pass --force to click anyway"
                .to_string(),
        ),
        PageError::Action(crate::page::input::ActionError::Unstable) => {
            Some("wait for the animation to finish and retry".to_string())
        }
        _ => None,
    };
    match hint {
        Some(h) => Response::error_hint(e.to_string(), h),
        None => Response::error(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_parsing_defaults_to_left() {
        assert_eq!(parse_button("right"), MouseButton::Right);
        assert_eq!(parse_button("MIDDLE"), MouseButton::Middle);
        assert_eq!(parse_button("nonsense"), MouseButton::Left);
    }

    #[test]
    fn missing_session_errors_name_the_fix() {
        let r = no_session("qa");
        let ipc::Response::Error { message, hint } = r else {
            panic!("expected an error")
        };
        assert!(message.contains("qa"));
        assert!(hint.unwrap().contains("brow open"));
    }

    #[test]
    fn stale_ref_errors_tell_the_agent_to_resnapshot() {
        let e = PageError::Ref(crate::page::RefError::Stale {
            node_ref: "@node-3".into(),
            had: 1,
            now: 2,
        });
        let ipc::Response::Error { hint, .. } = page_error(e) else {
            panic!("expected an error")
        };
        assert!(hint.unwrap().contains("snapshot"));
    }

    use crate::ipc;
}
