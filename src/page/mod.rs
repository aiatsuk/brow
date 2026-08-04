//! A single attached page: navigation, snapshots, input, capture, evaluation.

pub mod capture;
pub mod events;
pub mod input;
pub mod tree;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cdp::{CdpClient, CdpError, CdpEvent};

pub use capture::{Capture, ImageFormat, Region};
pub use events::{ConsoleEntry, EventLog, NetworkEntry};
pub use input::{MouseButton, Point};
pub use tree::{Node, RefError, Snapshot};

/// How long to wait for a navigation to settle before returning anyway.
const NAV_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error(transparent)]
    Cdp(#[from] CdpError),
    #[error(transparent)]
    Ref(#[from] RefError),
    #[error(transparent)]
    Action(#[from] input::ActionError),
    #[error("navigation to {url} failed: {reason}")]
    Navigation { url: String, reason: String },
    #[error("no snapshot has been taken yet — run `brow snapshot` first")]
    NoSnapshot,
}

/// One page target, plus everything that must stay consistent with it.
pub struct Page {
    client: Arc<CdpClient>,
    pub target_id: String,
    pub session_id: String,
    pub frame_id: String,
    refs: tree::RefTable,
    /// Bumped by a background watcher whenever the main frame's document is
    /// replaced. Every `@node-N` is scoped to one value of this counter.
    generation: Arc<AtomicU64>,
    /// Isolated world for our own helper code, recreated per document.
    world: Option<(u64, i64)>,
    snapshotted: bool,
    /// Console, exception and network capture for this session.
    pub events: Arc<events::EventLog>,
    /// Touch emulation is off until a touch gesture is first requested; turning it
    /// on changes how responsive sites render, so it is not a default.
    touch_enabled: bool,
}

/// Something a pointer gesture can aim at.
#[derive(Debug, Clone)]
pub enum PointTarget {
    Ref(String),
    At(Point),
}

impl Page {
    /// Attaches to an existing target with the flat protocol and enables the
    /// domains every other operation depends on.
    pub async fn attach(client: Arc<CdpClient>, target_id: &str) -> Result<Self, PageError> {
        let attached = client
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Protocol {
                method: "Target.attachToTarget".into(),
                code: 0,
                message: "browser attached without returning a sessionId".into(),
                data: None,
            })?
            .to_string();

        client.call_on(&session_id, "Page.enable", json!({})).await?;
        client.call_on(&session_id, "Runtime.enable", json!({})).await?;
        client.call_on(&session_id, "DOM.enable", json!({})).await?;

        let tree = client
            .call_on(&session_id, "Page.getFrameTree", json!({}))
            .await?;
        let frame_id = tree
            .get("frameTree")
            .and_then(|t| t.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let generation = Arc::new(AtomicU64::new(1));
        spawn_generation_watcher(&client, &session_id, &frame_id, Arc::clone(&generation));
        let events = events::spawn_recorder(&client, &session_id).await;

        Ok(Self {
            client,
            target_id: target_id.to_string(),
            session_id,
            frame_id,
            refs: tree::RefTable::default(),
            generation,
            world: None,
            snapshotted: false,
            events,
            touch_enabled: false,
        })
    }

    /// Opens a fresh page target and attaches to it.
    pub async fn create(client: Arc<CdpClient>, url: &str) -> Result<Self, PageError> {
        let created = client
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut page = Self::attach(client, &target_id).await?;
        if !url.is_empty() && url != "about:blank" {
            page.navigate(url).await?;
        }
        Ok(page)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Navigates and waits for the load event.
    pub async fn navigate(&mut self, url: &str) -> Result<(), PageError> {
        // Subscribe *before* navigating: the load event for a cached page can
        // arrive before the navigate call even returns.
        let mut events = self.client.subscribe();
        let session = self.session_id.clone();

        let res = self
            .client
            .call_on(&self.session_id, "Page.navigate", json!({ "url": url }))
            .await?;

        if let Some(err) = res.get("errorText").and_then(Value::as_str) {
            return Err(PageError::Navigation {
                url: url.to_string(),
                reason: err.to_string(),
            });
        }

        CdpClient::wait_for(&mut events, NAV_TIMEOUT, |ev: &CdpEvent| {
            ev.session_id.as_deref() == Some(session.as_str())
                && (ev.method == "Page.loadEventFired" || ev.method == "Page.frameStoppedLoading")
        })
        .await;

        // A navigation invalidates every ref and the isolated world with them.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.world = None;
        self.snapshotted = false;
        Ok(())
    }

    /// Captures the unified page tree and mints a fresh set of refs.
    pub async fn snapshot(&mut self) -> Result<Snapshot, PageError> {
        self.refs.set_generation(self.generation());
        let snap = tree::capture(&self.client, &self.session_id, &mut self.refs).await?;
        self.snapshotted = true;
        Ok(snap)
    }

    /// Resolves a ref, distinguishing the three ways it can go wrong.
    ///
    /// The order matters: a ref minted before a navigation must report *stale*
    /// (re-snapshot and continue), not *unknown* (you made that up) and not *no
    /// snapshot* (you skipped a step). Each needs different advice.
    fn resolve(&self, node_ref: &str) -> Result<i64, PageError> {
        match self.refs.resolve(node_ref, self.generation()) {
            Ok(entry) => Ok(entry.backend_node_id),
            Err(RefError::Unknown(r)) if !self.snapshotted => {
                let _ = r;
                Err(PageError::NoSnapshot)
            }
            Err(e) => Err(PageError::Ref(e)),
        }
    }

    /// An isolated world for our helper code, so a hostile or merely eccentric
    /// page cannot observe or patch what we run.
    async fn helper_world(&mut self) -> Result<i64, PageError> {
        let gen = self.generation();
        if let Some((cached_gen, ctx)) = self.world {
            if cached_gen == gen {
                return Ok(ctx);
            }
        }
        let res = self
            .client
            .call_on(
                &self.session_id,
                "Page.createIsolatedWorld",
                // `grantUniveralAccess` — the typo is in the protocol itself.
                json!({
                    "frameId": self.frame_id,
                    "worldName": "__brow",
                    "grantUniveralAccess": true,
                }),
            )
            .await?;
        let ctx = res
            .get("executionContextId")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        self.world = Some((gen, ctx));
        Ok(ctx)
    }

    /// Clicks a node after checking it is actually clickable.
    pub async fn click(
        &mut self,
        node_ref: &str,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        force: bool,
    ) -> Result<Point, PageError> {
        let backend = self.resolve(node_ref)?;
        let point = input::prepare_target(&self.client, &self.session_id, backend).await?;

        if !force {
            let world = self.helper_world().await.ok();
            let ok = input::hit_test(&self.client, &self.session_id, backend, world, point).await?;
            if !ok {
                return Err(PageError::Action(input::ActionError::Occluded {
                    x: point.x,
                    y: point.y,
                }));
            }
        }

        input::click_at(
            &self.client,
            &self.session_id,
            point,
            button,
            click_count,
            modifiers,
        )
        .await?;
        Ok(point)
    }

    pub async fn click_at(
        &self,
        point: Point,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
    ) -> Result<(), PageError> {
        input::click_at(
            &self.client,
            &self.session_id,
            point,
            button,
            click_count,
            modifiers,
        )
        .await?;
        Ok(())
    }

    pub async fn hover(&mut self, node_ref: &str) -> Result<Point, PageError> {
        let backend = self.resolve(node_ref)?;
        let point = input::prepare_target(&self.client, &self.session_id, backend).await?;
        input::hover_at(&self.client, &self.session_id, point, 0).await?;
        Ok(point)
    }

    /// Focuses a field, clears it, and types `text`.
    pub async fn fill(&mut self, node_ref: &str, text: &str) -> Result<(), PageError> {
        let backend = self.resolve(node_ref)?;
        // Click first: focusing alone leaves some component libraries in a state
        // where they never open their dropdown or attach their input handler.
        let point = input::prepare_target(&self.client, &self.session_id, backend).await?;
        input::click_at(&self.client, &self.session_id, point, MouseButton::Left, 1, 0).await?;
        self.client
            .call_on(
                &self.session_id,
                "DOM.focus",
                json!({ "backendNodeId": backend }),
            )
            .await?;
        // Select-all then insert replaces the value without assuming the field was
        // empty, and without an assignment that a controlled React input would
        // overwrite on its next render.
        input::select_all(&self.client, &self.session_id).await?;
        if text.is_empty() {
            input::press_key(&self.client, &self.session_id, "Delete").await?;
        } else {
            input::insert_text(&self.client, &self.session_id, text).await?;
        }
        Ok(())
    }

    pub async fn press(&self, chord: &str) -> Result<(), PageError> {
        input::press_key(&self.client, &self.session_id, chord).await?;
        Ok(())
    }

    pub async fn type_text(&self, text: &str, by_key: bool) -> Result<(), PageError> {
        if by_key {
            input::type_text_by_key(
                &self.client,
                &self.session_id,
                text,
                Duration::from_millis(12),
            )
            .await?;
        } else {
            input::insert_text(&self.client, &self.session_id, text).await?;
        }
        Ok(())
    }

    pub async fn scroll(&self, dx: f64, dy: f64) -> Result<(), PageError> {
        // Scroll from the viewport centre so the gesture lands on the main
        // scroller rather than whatever happens to be at the origin.
        let metrics = self
            .client
            .call_on(&self.session_id, "Page.getLayoutMetrics", json!({}))
            .await?;
        let vw = metrics
            .get("cssVisualViewport")
            .and_then(|v| v.get("clientWidth"))
            .and_then(Value::as_f64)
            .unwrap_or(800.0);
        let vh = metrics
            .get("cssVisualViewport")
            .and_then(|v| v.get("clientHeight"))
            .and_then(Value::as_f64)
            .unwrap_or(600.0);
        input::wheel_at(
            &self.client,
            &self.session_id,
            Point { x: vw / 2.0, y: vh / 2.0 },
            dx,
            dy,
        )
        .await?;
        Ok(())
    }

    /// Resolves a gesture target to a viewport point.
    ///
    /// A ref goes through the full actionability check; explicit coordinates are
    /// taken at face value, because the caller asking for a raw point has already
    /// said they know better than our hit test.
    pub async fn point_of(&mut self, target: &PointTarget) -> Result<Point, PageError> {
        match target {
            PointTarget::At(p) => Ok(*p),
            PointTarget::Ref(node_ref) => {
                let backend = self.resolve(node_ref)?;
                Ok(input::prepare_target(&self.client, &self.session_id, backend).await?)
            }
        }
    }

    async fn ensure_touch(&mut self) -> Result<(), PageError> {
        if !self.touch_enabled {
            input::enable_touch(&self.client, &self.session_id, true).await?;
            self.touch_enabled = true;
        }
        Ok(())
    }

    pub async fn tap(&mut self, target: &PointTarget) -> Result<Point, PageError> {
        self.ensure_touch().await?;
        let point = self.point_of(target).await?;
        input::tap(&self.client, &self.session_id, point).await?;
        Ok(point)
    }

    pub async fn long_press(
        &mut self,
        target: &PointTarget,
        duration: Duration,
    ) -> Result<Point, PageError> {
        self.ensure_touch().await?;
        let point = self.point_of(target).await?;
        input::long_press(&self.client, &self.session_id, point, duration).await?;
        Ok(point)
    }

    pub async fn swipe(
        &mut self,
        from: &PointTarget,
        to: &PointTarget,
        duration: Duration,
        steps: u32,
    ) -> Result<(Point, Point), PageError> {
        self.ensure_touch().await?;
        let a = self.point_of(from).await?;
        let b = self.point_of(to).await?;
        input::swipe(&self.client, &self.session_id, a, b, duration, steps).await?;
        Ok((a, b))
    }

    pub async fn pinch(
        &mut self,
        center: &PointTarget,
        scale: f64,
        speed: Option<i64>,
    ) -> Result<Point, PageError> {
        self.ensure_touch().await?;
        let point = self.point_of(center).await?;
        input::pinch(&self.client, &self.session_id, point, scale, speed).await?;
        Ok(point)
    }

    pub async fn drag(
        &mut self,
        from: &PointTarget,
        to: &PointTarget,
        duration: Duration,
        steps: u32,
    ) -> Result<(Point, Point), PageError> {
        let a = self.point_of(from).await?;
        let b = self.point_of(to).await?;
        input::drag(&self.client, &self.session_id, a, b, duration, steps).await?;
        Ok((a, b))
    }

    pub async fn screenshot(
        &mut self,
        region: ScreenshotTarget,
        format: ImageFormat,
        quality: Option<i64>,
    ) -> Result<Capture, PageError> {
        let region = match region {
            ScreenshotTarget::Viewport => Region::Viewport,
            ScreenshotTarget::FullPage => Region::FullPage,
            ScreenshotTarget::Node(node_ref) => Region::Node {
                backend_node_id: self.resolve(&node_ref)?,
            },
            ScreenshotTarget::Rect(clip) => Region::Rect(clip),
        };
        Ok(capture::capture(&self.client, &self.session_id, region, format, quality).await?)
    }

    /// Evaluates an expression.
    ///
    /// `read_only` sets `throwOnSideEffect`, which V8 enforces itself: the
    /// expression is aborted the moment it tries to mutate anything. Verified
    /// 2026-08-04 — `document.title = 'x'` under this flag raises
    /// `EvalError: Possible side-effect in debug-evaluate` and the title is
    /// unchanged. This is a real guarantee, not a convention.
    ///
    /// Read-only evaluation deliberately runs in the **main** world: seeing the
    /// application's own globals is the entire point of inspection, and the
    /// side-effect guard is what makes that safe.
    ///
    /// The guard is *sound but conservative*, and callers need to know it. Measured
    /// on Chrome 2026-08-04:
    ///
    /// | expression                                     | read-only |
    /// |------------------------------------------------|-----------|
    /// | `document.querySelector('#x').textContent`      | allowed   |
    /// | `document.getElementById('x').textContent`      | REFUSED   |
    /// | `el.getBoundingClientRect().width`              | REFUSED   |
    /// | `window.appState`, `arr.map(...)`, `innerHTML`  | allowed   |
    ///
    /// Nothing that mutates ever slips through, but harmless reads do get refused,
    /// so the error explains the workaround instead of just saying "no".
    pub async fn evaluate(&self, expression: &str, read_only: bool) -> Result<Value, PageError> {
        let res = self
            .client
            .call_on(
                &self.session_id,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": !read_only,
                    "throwOnSideEffect": read_only,
                    "userGesture": !read_only,
                }),
            )
            .await?;

        if let Some(details) = res.get("exceptionDetails") {
            let text = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation threw");
            let hint = if read_only && text.contains("side-effect") {
                "\nRead-only evaluation refused this. V8's side-effect check is \
                 conservative, so this is either a real mutation or a false positive on \
                 a harmless read — `getElementById` and `getBoundingClientRect` are \
                 refused even though they change nothing, while `querySelector` is not. \
                 Try rewriting with `querySelector`, or re-run with `--mutate` if the \
                 change is intended."
            } else {
                ""
            };
            return Err(PageError::Cdp(CdpError::Protocol {
                method: "Runtime.evaluate".into(),
                code: 0,
                message: format!("{text}{hint}"),
                data: None,
            }));
        }

        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Current document URL and title, cheaply.
    pub async fn location(&self) -> Result<(String, String), PageError> {
        let v = self
            .evaluate("[location.href, document.title]", true)
            .await?;
        let url = v.get(0).and_then(Value::as_str).unwrap_or_default().to_string();
        let title = v.get(1).and_then(Value::as_str).unwrap_or_default().to_string();
        Ok((url, title))
    }

    pub async fn close(&self) -> Result<(), PageError> {
        self.client
            .call("Target.closeTarget", json!({ "targetId": self.target_id }))
            .await?;
        Ok(())
    }
}

/// What `brow screenshot` was asked for, before refs are resolved.
#[derive(Debug, Clone)]
pub enum ScreenshotTarget {
    Viewport,
    FullPage,
    Node(String),
    Rect(capture::Clip),
}

/// Watches for main-frame document replacement and invalidates refs.
///
/// Both events matter: `Page.frameNavigated` covers real navigations, and
/// `Page.navigatedWithinDocument` covers SPA route changes, which replace the
/// entire rendered tree without a new document. Treating the latter as a
/// generation bump costs one extra snapshot and prevents a whole class of
/// silently-wrong clicks.
fn spawn_generation_watcher(
    client: &Arc<CdpClient>,
    session_id: &str,
    frame_id: &str,
    generation: Arc<AtomicU64>,
) {
    let mut events = client.subscribe();
    let session_id = session_id.to_string();
    let frame_id = frame_id.to_string();
    tokio::spawn(async move {
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            if ev.session_id.as_deref() != Some(session_id.as_str()) {
                continue;
            }
            let is_main_frame_nav = match ev.method.as_str() {
                "Page.frameNavigated" => ev
                    .params
                    .get("frame")
                    .and_then(|f| f.get("id"))
                    .and_then(Value::as_str)
                    .map(|id| id == frame_id)
                    .unwrap_or(false),
                "Page.navigatedWithinDocument" => ev
                    .params
                    .get("frameId")
                    .and_then(Value::as_str)
                    .map(|id| id == frame_id)
                    .unwrap_or(false),
                _ => false,
            };
            if is_main_frame_nav {
                let g = generation.fetch_add(1, Ordering::SeqCst) + 1;
                tracing::debug!(generation = g, "document replaced; refs invalidated");
            }
        }
    });
}
