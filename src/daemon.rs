//! `browd`: the resident process that owns the browsers.
//!
//! The daemon exists so that a browser outlives any single agent invocation. An
//! agent runs for one turn; a login session, a half-filled form and a 40-minute
//! crawl do not. Keeping Chromium here also means exactly one process in the
//! system speaks CDP, which is what lets us make "no raw protocol access" true by
//! construction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::browser::{self, launch::Headless, LaunchOptions, Launched};
use crate::ipc::{Hello, Request, Response, ShotTarget, Target, PROTOCOL_VERSION};
use crate::page::{
    capture, ImageFormat, MouseButton, Page, PageError, Point, PointTarget, ScreenshotTarget,
};
use crate::paths;

struct Session {
    launched: Launched,
    page: Page,
    profile: PathBuf,
    headless: bool,
    opened_at: SystemTime,
}

pub struct Daemon {
    sessions: HashMap<String, Session>,
    started: Instant,
    shutdown: tokio::sync::watch::Sender<bool>,
}

/// Starts the daemon, serving until asked to stop.
pub async fn serve() -> anyhow::Result<()> {
    paths::ensure_layout()?;
    let socket_path = paths::socket();

    let listener = bind(&socket_path).await?;
    let _ = std::fs::write(paths::pid_file(), std::process::id().to_string());

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let daemon = Arc::new(Mutex::new(Daemon {
        sessions: HashMap::new(),
        started: Instant::now(),
        shutdown: shutdown_tx,
    }));

    tracing::info!(socket = %socket_path.display(), pid = std::process::id(), "browd listening");

    let signals = {
        let daemon = Arc::clone(&daemon);
        async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("signal received, shutting down");
            let _ = daemon.lock().await.shutdown.send(true);
        }
    };
    tokio::spawn(signals);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let daemon = Arc::clone(&daemon);
                        tokio::spawn(async move {
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

    // Take every browser down with us rather than leaking Chromium processes.
    let mut guard = daemon.lock().await;
    let names: Vec<String> = guard.sessions.keys().cloned().collect();
    for name in names {
        guard.close_session(&name).await;
    }
    drop(guard);

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(paths::pid_file());
    tracing::info!("browd stopped");
    Ok(())
}

/// Binds the control socket, clearing a stale one from a crashed daemon.
async fn bind(path: &std::path::Path) -> anyhow::Result<UnixListener> {
    if path.exists() {
        // A socket file that nobody is listening on is left over from a crash.
        // One that answers means a daemon is already running, and we must not
        // steal its socket.
        match UnixStream::connect(path).await {
            Ok(_) => anyhow::bail!(
                "another browd is already listening on {}",
                path.display()
            ),
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
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
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

async fn handle_conn(stream: UnixStream, daemon: Arc<Mutex<Daemon>>) -> anyhow::Result<()> {
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
            Ok(req) => {
                let stop = matches!(req, Request::Shutdown);
                let mut guard = daemon.lock().await;
                let resp = guard.dispatch(req).await;
                if stop {
                    let _ = guard.shutdown.send(true);
                }
                resp
            }
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
    async fn dispatch(&mut self, req: Request) -> Response {
        match req {
            Request::Ping => Response::ok_text(json!({"pong": true}), "pong"),
            Request::Shutdown => Response::ok_text(json!({"stopping": true}), "browd stopping"),
            Request::Status => {
                let uptime = self.started.elapsed().as_secs();
                Response::ok_text(
                    json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "pid": std::process::id(),
                        "uptime_seconds": uptime,
                        "sessions": self.sessions.len(),
                    }),
                    format!(
                        "browd {} · pid {} · up {}s · {} session(s)",
                        env!("CARGO_PKG_VERSION"),
                        std::process::id(),
                        uptime,
                        self.sessions.len()
                    ),
                )
            }
            Request::Sessions => {
                let mut rows = Vec::new();
                let mut text = String::new();
                for (name, s) in &self.sessions {
                    let age = s
                        .opened_at
                        .elapsed()
                        .map(|d| d.as_secs())
                        .unwrap_or_default();
                    rows.push(json!({
                        "session": name,
                        "headless": s.headless,
                        "product": s.launched.product,
                        "generation": s.page.generation(),
                        "age_seconds": age,
                    }));
                    text.push_str(&format!(
                        "{name}  {}  generation {}  {}s\n",
                        if s.headless { "headless" } else { "headed" },
                        s.page.generation(),
                        age
                    ));
                }
                if text.is_empty() {
                    text.push_str("no open sessions\n");
                }
                Response::ok_text(json!(rows), text)
            }
            Request::Open { url, session, headless } => self.open(&session, &url, headless).await,
            Request::Close { session } => {
                if self.sessions.contains_key(&session) {
                    self.close_session(&session).await;
                    Response::ok_text(json!({"closed": session}), format!("closed {session}"))
                } else {
                    Response::error(format!("no session named {session:?}"))
                }
            }
            Request::Snapshot { session, interactive } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
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
                                "counts": { "total": count, "interactive": interactive_count },
                            }),
                            text,
                        )
                    }
                    Err(e) => page_error(e),
                }
            }
            Request::Click { session, target, button, count, modifiers, force } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
                };
                let button = parse_button(&button);
                match target {
                    Target::Ref { node_ref } => {
                        match s.page.click(&node_ref, button, count, modifiers, force).await {
                            Ok(p) => Response::ok_text(
                                json!({"clicked": node_ref, "x": p.x, "y": p.y}),
                                format!("clicked {node_ref} at {:.0},{:.0}", p.x, p.y),
                            ),
                            Err(e) => page_error(e),
                        }
                    }
                    Target::Point { x, y } => {
                        match s.page.click_at(Point { x, y }, button, count, modifiers).await {
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
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
                };
                match s.page.hover(&node_ref).await {
                    Ok(p) => Response::ok_text(
                        json!({"hovered": node_ref, "x": p.x, "y": p.y}),
                        format!("hovering {node_ref}"),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Fill { session, node_ref, text } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
                };
                match s.page.fill(&node_ref, &text).await {
                    Ok(()) => Response::ok_text(
                        json!({"filled": node_ref}),
                        format!("filled {node_ref}"),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Press { session, chord } => {
                let Some(s) = self.sessions.get(&session) else {
                    return no_session(&session);
                };
                match s.page.press(&chord).await {
                    Ok(()) => Response::ok_text(json!({"pressed": chord}), format!("pressed {chord}")),
                    Err(e) => page_error(e),
                }
            }
            Request::Type { session, text, by_key } => {
                let Some(s) = self.sessions.get(&session) else {
                    return no_session(&session);
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
                let Some(s) = self.sessions.get(&session) else {
                    return no_session(&session);
                };
                match s.page.scroll(dx, dy).await {
                    Ok(()) => Response::ok_text(
                        json!({"dx": dx, "dy": dy}),
                        format!("scrolled {dx:.0},{dy:.0}"),
                    ),
                    Err(e) => page_error(e),
                }
            }
            Request::Screenshot { session, target, format, quality, out } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
                };
                let Some(fmt) = ImageFormat::parse(&format) else {
                    return Response::error(format!("unknown image format {format:?}"));
                };
                let target = match target {
                    ShotTarget::Viewport => ScreenshotTarget::Viewport,
                    ShotTarget::FullPage => ScreenshotTarget::FullPage,
                    ShotTarget::Node { node_ref } => ScreenshotTarget::Node(node_ref),
                    ShotTarget::Rect { x, y, width, height } => {
                        ScreenshotTarget::Rect(capture::Clip { x, y, width, height })
                    }
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
                        Response::ok_text(
                            json!({
                                "path": written,
                                "bytes": shot.bytes.len(),
                                "truncated": shot.truncated,
                            }),
                            text,
                        )
                    }
                    Err(e) => Response::error(format!("could not write {}: {e}", path.display())),
                }
            }
            Request::Console { session, errors, limit } => {
                let Some(s) = self.sessions.get(&session) else {
                    return no_session(&session);
                };
                let rows = s.page.events.console(errors, limit.clamp(1, 5_000));
                let (dropped, _) = s.page.events.dropped();
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
                Response::ok_text(json!({ "entries": rows, "dropped": dropped }), text)
            }
            Request::Network { session, failed, limit } => {
                let Some(s) = self.sessions.get(&session) else {
                    return no_session(&session);
                };
                let rows = s.page.events.network(failed, limit.clamp(1, 5_000));
                let (_, dropped) = s.page.events.dropped();
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
                Response::ok_text(json!({ "requests": rows, "dropped": dropped }), text)
            }
            Request::Tap { session, target } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
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
            Request::LongPress { session, target, duration_ms } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
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
            Request::Swipe { session, from, to, duration_ms, steps } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
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
            Request::Pinch { session, center, scale, speed } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
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
            Request::Drag { session, from, to, duration_ms, steps } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return no_session(&session);
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
            Request::Eval { session, expression, mutate } => {
                let Some(s) = self.sessions.get(&session) else {
                    return no_session(&session);
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

    async fn open(&mut self, session: &str, url: &str, headless: bool) -> Response {
        if let Some(s) = self.sessions.get_mut(session) {
            return match s.page.navigate(url).await {
                Ok(()) => Response::ok_text(
                    json!({"session": session, "url": url, "reused": true}),
                    format!("{session}: {url}"),
                ),
                Err(e) => page_error(e),
            };
        }

        let profile = paths::profile(session);
        let mut opts = LaunchOptions::new(profile.clone());
        opts.headless = if headless { Headless::New } else { Headless::Off };

        let launched = match browser::launch(&opts).await {
            Ok(l) => l,
            Err(e) => {
                return Response::error_hint(
                    format!("could not start a browser: {e}"),
                    "set BROW_CHROME to a Chromium binary if it is installed somewhere unusual",
                )
            }
        };

        let page = match Page::create(Arc::clone(&launched.client), url).await {
            Ok(p) => p,
            Err(e) => {
                let mut launched = launched;
                let _ = launched.child.kill();
                let _ = launched.child.wait();
                return page_error(e);
            }
        };

        let product = launched.product.clone();
        self.sessions.insert(
            session.to_string(),
            Session {
                launched,
                page,
                profile,
                headless,
                opened_at: SystemTime::now(),
            },
        );

        Response::ok_text(
            json!({"session": session, "url": url, "product": product, "reused": false}),
            format!("{session}: {url}  ({product})"),
        )
    }

    async fn close_session(&mut self, name: &str) {
        let Some(mut s) = self.sessions.remove(name) else {
            return;
        };
        // Ask nicely first so the profile is flushed cleanly, then make sure.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            s.launched.client.call("Browser.close", json!({})),
        )
        .await;
        let _ = s.launched.child.kill();
        let _ = s.launched.child.wait();
        tracing::info!(session = name, profile = %s.profile.display(), "session closed");
    }
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
