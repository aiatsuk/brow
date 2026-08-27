//! Real-Chromium coverage for cross-origin out-of-process iframes.

#[allow(dead_code)]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use brow::browser::{launch, Headless, LaunchOptions, Launched};
#[cfg(debug_assertions)]
use brow::ipc::WaitConditions;
use brow::ipc::WaitPolicy;
#[cfg(debug_assertions)]
use brow::page::{DispatchState, PageError};
use brow::page::{
    ImageFormat, MouseButton, NavigationKind, NavigationScope, Node, Page, PointTarget,
    ScreenshotTarget, Snapshot, WaitOutcome,
};
use serde_json::json;

type Handler = dyn Fn(&str, &str) -> String + Send + Sync + 'static;

struct HttpFixture {
    base: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn start(host: &str, handler: Arc<Handler>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind OOPIF fixture");
        let port = listener.local_addr().expect("fixture address").port();
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&handler);
                        std::thread::spawn(move || {
                            let _ = serve_request(stream, &handler);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(3));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base: format!("http://{host}:{port}"),
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_request(mut stream: TcpStream, handler: &Arc<Handler>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");
    let body = handler(method, path);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[derive(Default)]
struct ChildState {
    clicks: AtomicUsize,
    typed: Mutex<String>,
}

fn nested_fixture() -> (HttpFixture, Arc<AtomicUsize>) {
    let clicks = Arc::new(AtomicUsize::new(0));
    let handler_clicks = Arc::clone(&clicks);
    let handler: Arc<Handler> = Arc::new(move |method, path| {
        if path.starts_with("/nested-clicked") {
            if method == "POST" || method == "GET" {
                handler_clicks.fetch_add(1, Ordering::SeqCst);
            }
            return "ok".into();
        }
        "<!doctype html><html><body style='margin:0'>\
         <button id='nested' aria-label='Nested cross origin action' \
           style='position:absolute;left:18px;top:24px;width:210px;height:48px'>nested</button>\
         <script>nested.onclick=()=>fetch('/nested-clicked',{method:'POST'})</script>\
         </body></html>"
            .into()
    });
    (HttpFixture::start("localhost", handler), clicks)
}

fn child_fixture(nested_base: String) -> (HttpFixture, Arc<ChildState>) {
    let state = Arc::new(ChildState::default());
    let handler_state = Arc::clone(&state);
    let handler: Arc<Handler> = Arc::new(move |method, path| {
        if path.starts_with("/clicked") {
            if method == "POST" || method == "GET" {
                handler_state.clicks.fetch_add(1, Ordering::SeqCst);
            }
            return "ok".into();
        }
        if let Some(value) = path.strip_prefix("/typed?value=") {
            *handler_state.typed.lock().expect("typed mutex") = value.to_string();
            return "ok".into();
        }
        if let Some(iteration) = path.strip_prefix("/stress-next/") {
            return format!(
                "<!doctype html><html><body style='margin:0'>\
                 <button aria-label='Stress replacement {iteration}'>replacement</button>\
                 </body></html>"
            );
        }
        if let Some(iteration) = path.strip_prefix("/stress/") {
            return format!(
                "<!doctype html><html><body style='margin:0'>\
                 <button id='advance' aria-label='Stress initial {iteration}' \
                   style='position:absolute;left:20px;top:20px;width:180px;height:42px'>advance</button>\
                 <script>advance.onclick=()=>location.href='/stress-next/{iteration}'</script>\
                 </body></html>"
            );
        }
        if path.starts_with("/nested") {
            return format!(
                "<!doctype html><html><body style='margin:0'>\
                 <iframe src='{nested_base}/nested' \
                   style='position:absolute;left:35px;top:45px;width:300px;height:150px;border:0'></iframe>\
                 </body></html>"
            );
        }
        if path.starts_with("/nav-v2") {
            return "<!doctype html><html><body><button aria-label='Replacement action'>new</button></body></html>".into();
        }
        if path.starts_with("/nav") {
            return "<!doctype html><html><body><button id='navigate' aria-label='Navigate child'>go</button><script>navigate.onclick=()=>location.href='/nav-v2'</script></body></html>".into();
        }
        "<!doctype html><html><head><title>child</title></head><body style='margin:0'>\
         <button id='action' aria-label='Cross origin action' \
           style='position:absolute;left:30px;top:40px;width:180px;height:52px;background:#36c;color:white'>act</button>\
         <input id='field' aria-label='Cross origin input' \
           style='position:absolute;left:30px;top:115px;width:220px;height:38px'>\
         <script>\
           action.onclick=()=>fetch('/clicked',{method:'POST'});\
           field.oninput=()=>fetch('/typed?value='+encodeURIComponent(field.value));\
         </script></body></html>"
            .into()
    });
    (HttpFixture::start("127.0.0.1", handler), state)
}

fn main_fixture(child_base: String) -> HttpFixture {
    let handler: Arc<Handler> = Arc::new(move |_method, _path| {
        format!(
            "<!doctype html><html><head><title>OOPIF parent</title></head>\
             <body style='margin:0'>\
               <h1>parent</h1>\
               <iframe id='foreign' src='{child_base}/child' \
                 style='position:absolute;left:120px;top:140px;width:420px;height:260px;border:0'></iframe>\
             </body></html>"
        )
    });
    // `localhost` vs `127.0.0.1` are different schemeful sites, not merely
    // different ports, so Chromium site isolation must create a real OOPIF.
    HttpFixture::start("localhost", handler)
}

#[derive(Default)]
struct OverlapGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl OverlapGate {
    fn mark_action_started_and_wait(&self) {
        let mut state = self.state.lock().expect("overlap gate");
        state.0 = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.1)
            .expect("overlap gate condvar");
        assert!(state.1, "unrelated OOPIF navigation was never released");
    }

    fn wait_for_action(&self) {
        let state = self.state.lock().expect("overlap gate");
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.0)
            .expect("overlap gate condvar");
        assert!(state.0, "root action did not reach the overlap barrier");
    }

    fn release_action(&self) {
        let mut state = self.state.lock().expect("overlap gate");
        state.1 = true;
        self.changed.notify_all();
    }
}

fn overlap_main_fixture(child_base: String) -> (HttpFixture, Arc<OverlapGate>) {
    let gate = Arc::new(OverlapGate::default());
    let handler_gate = Arc::clone(&gate);
    let handler: Arc<Handler> = Arc::new(move |_method, path| {
        if path.starts_with("/root-action-started") {
            handler_gate.mark_action_started_and_wait();
            return "ok".into();
        }
        format!(
            "<!doctype html><html><head><title>OOPIF overlap parent</title></head>\
             <body style='margin:0'>\
               <button id='root-action' aria-label='Root no navigation'>root action</button>\
               <iframe id='foreign' src='{child_base}/child' \
                 style='position:absolute;left:120px;top:140px;width:420px;height:260px;border:0'></iframe>\
               <script>rootAction=document.querySelector('#root-action');\
                 rootAction.onclick=()=>{{const marker=new XMLHttpRequest();\
                 marker.open('POST','/root-action-started',false);marker.send(null);}};</script>\
             </body></html>"
        )
    });
    (HttpFixture::start("localhost", handler), gate)
}

async fn open_page(url: &str) -> (Launched, Page, common::Scratch) {
    let scratch = common::Scratch::new("oopif");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    options.window_size = (1000, 720);
    let launched = launch(&options).await.expect("launch Chromium");
    let page = Page::create(Arc::clone(&launched.client), url)
        .await
        .expect("open OOPIF fixture");
    (launched, page, scratch)
}

fn shutdown(mut launched: Launched) {
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

async fn snapshot_until(page: &mut Page, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let mut last = None;
    for _ in 0..80 {
        match page.snapshot().await {
            Ok(snapshot) if predicate(&snapshot) => return snapshot,
            Ok(snapshot) => {
                last = Some(format!(
                    "nodes={:?}, gaps={:?}",
                    snapshot
                        .interactive()
                        .map(|node| node.name.clone())
                        .collect::<Vec<_>>(),
                    snapshot.coverage_gaps
                ))
            }
            Err(error) => last = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("snapshot predicate never became true; last={last:?}");
}

fn named(snapshot: &Snapshot, name: &str) -> Node {
    snapshot
        .interactive()
        .find(|node| node.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing {name:?}"))
        .clone()
}

async fn wait_for_generation(page: &Page, before: u64) {
    for _ in 0..100 {
        if page.generation() > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    panic!("generation did not advance beyond {before}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unrelated_oopif_navigation_cannot_settle_a_root_action() {
    if !common::chrome_available() {
        return common::skip("unrelated_oopif_navigation_cannot_settle_a_root_action");
    }
    // OOPIF scenarios fan out into several renderer/helper processes each.
    // Reserving the pool keeps three parallel OOPIF tests from exhausting
    // Chrome during the protocol handshake after the larger E2E suite.
    let _slots = common::exclusive_browser_slots();
    let (child, _child_state) = child_fixture(String::new());
    let (parent, gate) = overlap_main_fixture(child.base.clone());
    let (launched, mut page, _scratch) = open_page(&parent.url("/")).await;

    let snapshot = snapshot_until(&mut page, |snapshot| {
        ["Root no navigation", "Cross origin action"]
            .into_iter()
            .all(|name| {
                snapshot
                    .interactive()
                    .any(|node| node.name.as_deref() == Some(name))
            })
    })
    .await;
    let root_action = named(&snapshot, "Root no navigation");
    let child_action = named(&snapshot, "Cross origin action");
    assert_eq!(root_action.session_id, page.session_id);
    assert_ne!(child_action.session_id, page.session_id);

    let client = Arc::clone(&launched.client);
    let child_session = child_action.session_id.clone();
    let navigation_gate = Arc::clone(&gate);
    let unrelated_navigation = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || navigation_gate.wait_for_action())
            .await
            .expect("overlap waiter");
        client
            .call_on(
                &child_session,
                "Runtime.evaluate",
                json!({
                    "expression": "location.hash='unrelated-navigation'",
                    "returnByValue": true
                }),
            )
            .await
            .expect("dispatch unrelated OOPIF navigation");
        gate.release_action();
    });

    let (_, receipt) = page
        .click_with_wait(
            &root_action.node_ref,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Auto,
            Duration::from_secs(2),
        )
        .await
        .expect("root action remains navigation-free");
    unrelated_navigation
        .await
        .expect("unrelated navigation task");
    assert_eq!(receipt.outcome, WaitOutcome::NoNavigation, "{receipt:?}");
    assert_eq!(receipt.navigation, NavigationKind::None, "{receipt:?}");
    assert_eq!(
        receipt.navigation_scope,
        NavigationScope::None,
        "{receipt:?}"
    );

    shutdown(launched);
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn stable_fails_closed_while_an_oopif_initializer_is_held() {
    if !common::chrome_available() {
        return common::skip("stable_fails_closed_while_an_oopif_initializer_is_held");
    }
    let _slots = common::exclusive_browser_slots();
    let child_handler: Arc<Handler> = Arc::new(|_method, _path| {
        "<!doctype html><html><body><button aria-label='Held child ready'>ready</button></body></html>"
            .into()
    });
    let child = HttpFixture::start("127.0.0.1", child_handler);
    let held_child_url = child.url("/held");
    let parent_handler: Arc<Handler> = Arc::new(move |_method, path| {
        if path.starts_with("/with-child") {
            return format!(
                "<!doctype html><html><head><title>action initializer gate</title></head>\
                 <body><h1>navigated root</h1><iframe src='{held_child_url}'></iframe></body></html>"
            );
        }
        "<!doctype html><html><head><title>initializer gate</title></head>\
         <body><h1>root ready</h1>\
           <button id='navigate' aria-label='Navigate to held target tree'>navigate</button>\
           <script>navigate.onclick=()=>location.href='/with-child'</script>\
         </body></html>"
            .into()
    });
    let parent = HttpFixture::start("localhost", parent_handler);
    let (launched, mut page, _scratch) = open_page(&parent.url("/")).await;

    let hold = brow::page::test_support::hold_next_target_initializer(page.session_id.clone());
    page.evaluate(
        &format!(
            "const frame=document.createElement('iframe');frame.src={:?};document.body.appendChild(frame)",
            child.url("/held")
        ),
        false,
    )
    .await
    .expect("create held OOPIF");
    tokio::time::timeout(Duration::from_secs(2), hold.wait_until_entered())
        .await
        .expect("OOPIF initializer reached deterministic barrier");

    let error = page
        .wait_for_conditions(
            &WaitConditions {
                stable: true,
                ..WaitConditions::default()
            },
            Duration::from_millis(350),
            Duration::from_millis(100),
        )
        .await
        .expect_err("held related target must prevent a stable claim");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("expected typed wait failure, got {error:?}");
    };
    assert_eq!(receipt.outcome, WaitOutcome::TimedOut, "{receipt:?}");
    assert!(!receipt.root_loading, "{receipt:?}");
    assert!(!receipt.target_settled, "{receipt:?}");
    assert!(
        receipt
            .blockers
            .iter()
            .any(|blocker| blocker.contains("related target")),
        "{receipt:?}"
    );

    hold.release();
    let snapshot = snapshot_until(&mut page, |snapshot| {
        snapshot.coverage_gaps.is_empty()
            && snapshot
                .interactive()
                .any(|node| node.name.as_deref() == Some("Held child ready"))
    })
    .await;
    assert!(snapshot.coverage_gaps.is_empty());
    let settled = page
        .wait_for_conditions(
            &WaitConditions {
                stable: true,
                ..WaitConditions::default()
            },
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "stable succeeds after related target initialization: {error:?}; network={:#?}",
                page.events.network(false, 20)
            )
        });
    assert!(!settled.root_loading, "{settled:?}");
    assert!(settled.target_settled, "{settled:?}");

    // A navigation action must apply the same target-tree gate as an explicit
    // stable wait. Hold the new OOPIF's initializer across a real root
    // navigation and prove Load cannot report success while that related target
    // is known but unusable.
    let navigation_snapshot = snapshot_until(&mut page, |snapshot| {
        snapshot
            .interactive()
            .any(|node| node.name.as_deref() == Some("Navigate to held target tree"))
    })
    .await;
    let navigate = named(&navigation_snapshot, "Navigate to held target tree");
    assert_eq!(navigate.session_id, page.session_id);

    let action_hold =
        brow::page::test_support::hold_next_target_initializer(page.session_id.clone());
    let action_result = {
        let action = page.click_with_wait(
            &navigate.node_ref,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Load,
            Duration::from_millis(700),
        );
        tokio::pin!(action);
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = action_hold.wait_until_entered() => {}
                result = &mut action => {
                    panic!("navigation action completed before its OOPIF initializer was held: {result:?}")
                }
            }
        })
        .await
        .expect("action-created OOPIF reached deterministic barrier");
        tokio::time::timeout(Duration::from_secs(2), &mut action)
            .await
            .expect("held target action respected its own deadline")
    };
    let error = action_result.expect_err("unsettled action target tree must fail closed");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("expected action wait failure, got {error:?}");
    };
    assert_eq!(receipt.dispatch_state, DispatchState::Sent, "{receipt:?}");
    assert_eq!(receipt.dispatched, Some(true), "{receipt:?}");
    assert_eq!(
        receipt.navigation,
        NavigationKind::CrossDocument,
        "{receipt:?}"
    );
    assert_eq!(
        receipt.navigation_scope,
        NavigationScope::Root,
        "{receipt:?}"
    );
    assert!(
        matches!(
            receipt.outcome,
            WaitOutcome::TimedOut | WaitOutcome::Incomplete
        ),
        "{receipt:?}"
    );
    assert!(!receipt.target_settled, "{receipt:?}");
    assert!(!receipt.blockers.is_empty(), "{receipt:?}");

    action_hold.release();
    let action_snapshot = snapshot_until(&mut page, |snapshot| {
        snapshot.coverage_gaps.is_empty()
            && snapshot
                .interactive()
                .any(|node| node.name.as_deref() == Some("Held child ready"))
    })
    .await;
    assert!(action_snapshot.coverage_gaps.is_empty());

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn oopif_snapshot_actions_lifecycle_and_coverage_are_real() {
    if !common::chrome_available() {
        return common::skip("oopif_snapshot_actions_lifecycle_and_coverage_are_real");
    }
    let _slots = common::exclusive_browser_slots();
    let (nested, nested_clicks) = nested_fixture();
    let (child, child_state) = child_fixture(nested.base.clone());
    let parent = main_fixture(child.base.clone());
    let (launched, mut page, _scratch) = open_page(&parent.url("/")).await;

    let snapshot = snapshot_until(&mut page, |snapshot| {
        snapshot
            .interactive()
            .any(|node| node.name.as_deref() == Some("Cross origin action"))
    })
    .await;
    assert!(
        snapshot.coverage_gaps.is_empty(),
        "fully attached fixture reported gaps: {:?}",
        snapshot.coverage_gaps
    );
    let button = named(&snapshot, "Cross origin action");
    let field = named(&snapshot, "Cross origin input");
    assert_ne!(
        button.session_id, page.session_id,
        "the button must come from a genuine child CDP session, not same-process DOM piercing"
    );
    assert_ne!(button.target_id, page.target_id);

    let frame = snapshot
        .nodes
        .iter()
        .find(|node| node.tag == "iframe")
        .and_then(|node| node.bounds)
        .expect("owner iframe bounds");
    let bounds = button.bounds.expect("OOPIF button bounds");
    assert!(
        bounds.x >= frame.x && bounds.y >= frame.y,
        "{bounds:?} vs {frame:?}"
    );
    assert!(
        bounds.x + bounds.width <= frame.x + frame.width + 2.0
            && bounds.y + bounds.height <= frame.y + frame.height + 2.0,
        "child-local geometry was not transformed to the top viewport: {bounds:?} vs {frame:?}"
    );

    let point = page
        .click(&button.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect("click OOPIF button through top-level Input");
    assert!(
        point.x >= frame.x && point.y >= frame.y,
        "global click point: {point:?}"
    );
    for _ in 0..100 {
        if child_state.clicks.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        child_state.clicks.load(Ordering::SeqCst),
        1,
        "click missed: point={point:?}, frame={frame:?}, button={bounds:?}"
    );

    // A top-level overlay is invisible to child-session hit testing. The mapped
    // point must be checked through every parent compositor before root Input.
    child_state.clicks.store(0, Ordering::SeqCst);
    page.evaluate(
        &format!(
            "const o=document.createElement('div');o.id='root-cover';\
             Object.assign(o.style,{{position:'absolute',left:'{}px',top:'{}px',\
             width:'{}px',height:'{}px',zIndex:'9999',background:'red'}});\
             document.body.appendChild(o)",
            bounds.x, bounds.y, bounds.width, bounds.height
        ),
        false,
    )
    .await
    .expect("install parent overlay");
    let covered = page
        .click(&button.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect_err("ancestor overlay must block an OOPIF ref click");
    assert!(covered.to_string().contains("covering"), "{covered}");
    assert_eq!(child_state.clicks.load(Ordering::SeqCst), 0);
    page.evaluate("document.querySelector('#root-cover').remove()", false)
        .await
        .expect("remove parent overlay");

    // `mouseMoved` runs hover handlers before the button press. Revalidate after
    // that event so a child overlay installed by mouseover cannot receive press.
    launched
        .client
        .call_on(
            &button.session_id,
            "Runtime.evaluate",
            json!({
                "expression": "action.onmousemove=()=>{if(!document.querySelector('#hover-cover')){const o=document.createElement('div');o.id='hover-cover';Object.assign(o.style,{position:'absolute',left:'30px',top:'40px',width:'180px',height:'52px',zIndex:'9999',background:'red'});document.body.appendChild(o)}}"
            }),
        )
        .await
        .expect("install hover mutation");
    let hover_race = page
        .click(&button.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect_err("hover-installed overlay must block press");
    assert!(hover_race.to_string().contains("covering"), "{hover_race}");
    assert_eq!(child_state.clicks.load(Ordering::SeqCst), 0);
    launched
        .client
        .call_on(
            &button.session_id,
            "Runtime.evaluate",
            json!({"expression": "action.onmousemove=null;document.querySelector('#hover-cover')?.remove()"}),
        )
        .await
        .expect("remove hover mutation");

    // Drag has the same hover-to-press boundary as click. A source-side
    // mousemove handler must not be able to put an overlay under the pending
    // mouse press.
    launched
        .client
        .call_on(
            &button.session_id,
            "Runtime.evaluate",
            json!({
                "expression": "window.dragSourceDown=0;window.dragCoverDown=0;action.onmousedown=()=>dragSourceDown++;action.onmousemove=()=>{if(!document.querySelector('#drag-cover')){const o=document.createElement('div');o.id='drag-cover';o.onmousedown=()=>dragCoverDown++;Object.assign(o.style,{position:'absolute',left:'30px',top:'40px',width:'180px',height:'52px',zIndex:'9999',background:'red'});document.body.appendChild(o)}}"
            }),
        )
        .await
        .expect("install drag hover mutation");
    let drag_race = page
        .drag(
            &PointTarget::Ref(button.node_ref.clone()),
            &PointTarget::Ref(field.node_ref.clone()),
            Duration::from_millis(50),
            3,
        )
        .await
        .expect_err("hover-installed overlay must block drag press");
    assert!(drag_race.to_string().contains("covering"), "{drag_race}");
    let down_counts = launched
        .client
        .call_on(
            &button.session_id,
            "Runtime.evaluate",
            json!({
                "expression": "JSON.stringify([dragSourceDown,dragCoverDown])",
                "returnByValue": true
            }),
        )
        .await
        .expect("read drag press counters");
    assert_eq!(
        down_counts
            .get("result")
            .and_then(|result| result.get("value"))
            .and_then(serde_json::Value::as_str),
        Some("[0,0]")
    );
    launched
        .client
        .call_on(
            &button.session_id,
            "Runtime.evaluate",
            json!({"expression": "action.onmousemove=null;action.onmousedown=null;document.querySelector('#drag-cover')?.remove()"}),
        )
        .await
        .expect("remove drag mutation");

    page.fill(&field.node_ref, "oopif-value")
        .await
        .expect("fill input in OOPIF");
    for _ in 0..100 {
        if child_state.typed.lock().expect("typed mutex").as_str() == "oopif-value" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        child_state.typed.lock().expect("typed mutex").as_str(),
        "oopif-value"
    );

    let image = page
        .screenshot(
            ScreenshotTarget::Node(button.node_ref.clone()),
            ImageFormat::Png,
            None,
        )
        .await
        .expect("capture OOPIF node through root page clip");
    let (width, height) = common::png_size(&image.bytes).expect("valid node PNG");
    assert!(width > 20 && height > 20 && width < 500 && height < 300);

    // A -> B -> A creates two recursively attached OOPIF targets. The deepest
    // node must carry its own third session and still map through both owners for
    // top-level trusted input.
    let child_base = serde_json::to_string(&child.base).unwrap();
    page.evaluate(
        &format!("document.querySelector('#foreign').src={child_base}+'/nested'"),
        false,
    )
    .await
    .expect("open nested OOPIF fixture");
    let nested_snapshot = snapshot_until(&mut page, |snapshot| {
        snapshot
            .interactive()
            .any(|node| node.name.as_deref() == Some("Nested cross origin action"))
    })
    .await;
    let nested_button = named(&nested_snapshot, "Nested cross origin action");
    assert_ne!(nested_button.session_id, page.session_id);
    assert_ne!(nested_button.session_id, button.session_id);
    page.click(&nested_button.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect("click through two OOPIF owner transforms");
    for _ in 0..100 {
        if nested_clicks.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(nested_clicks.load(Ordering::SeqCst), 1);

    // Child navigation must invalidate a ref even though the top-level document
    // and its own CDP session survive.
    page.evaluate(
        &format!("document.querySelector('#foreign').src={child_base}+'/nav'"),
        false,
    )
    .await
    .expect("navigate iframe to nav fixture");
    let nav_snapshot = snapshot_until(&mut page, |snapshot| {
        snapshot
            .interactive()
            .any(|node| node.name.as_deref() == Some("Navigate child"))
    })
    .await;
    let navigate = named(&nav_snapshot, "Navigate child");
    let generation_before = page.generation();
    let (_, receipt) = page
        .click_with_wait(
            &navigate.node_ref,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Auto,
            Duration::from_secs(3),
        )
        .await
        .expect("navigate inside OOPIF");
    assert_eq!(receipt.outcome, WaitOutcome::Committed);
    assert_eq!(receipt.navigation_scope, NavigationScope::Subframe);
    assert!(receipt.final_generation > generation_before, "{receipt:?}");
    let stale = page
        .click(&navigate.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect_err("pre-navigation OOPIF ref must be stale");
    assert!(stale.to_string().contains("stale"), "{stale}");
    snapshot_until(&mut page, |snapshot| {
        snapshot
            .interactive()
            .any(|node| node.name.as_deref() == Some("Replacement action"))
    })
    .await;

    // Deliberately turn auto-attach off. Target discovery still sees the iframe,
    // so omission must be reported as a coverage gap in both structured and text
    // output.
    launched
        .client
        .call_on(
            &page.session_id,
            "Target.setAutoAttach",
            json!({
                "autoAttach": false,
                "waitForDebuggerOnStart": false,
                "flatten": true,
            }),
        )
        .await
        .expect("disable auto attach for coverage-gap probe");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let incomplete = page
        .snapshot()
        .await
        .expect("incomplete snapshot still succeeds");
    assert!(!incomplete.coverage_gaps.is_empty());
    assert!(incomplete.render_text(false).contains("coverage_gap"));

    launched
        .client
        .call_on(
            &page.session_id,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                "filter": [
                    { "type": "iframe", "exclude": false },
                    { "exclude": true }
                ],
            }),
        )
        .await
        .expect("restore auto attach");
    snapshot_until(&mut page, |snapshot| {
        snapshot.coverage_gaps.is_empty()
            && snapshot
                .interactive()
                .any(|node| node.name.as_deref() == Some("Replacement action"))
    })
    .await;

    // Repeatedly create, navigate, and detach a fresh OOPIF. This is deliberately
    // large enough to shake out session-map leaks/races without making normal CI
    // launch a hundred renderer processes.
    let stress_cycles = std::env::var("BROW_OOPIF_STRESS_CYCLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(12);
    assert!(stress_cycles > 0, "stress cycle count must be positive");
    for iteration in 0..stress_cycles {
        page.evaluate(
            &format!(
                "document.querySelector('#foreign')?.remove(); var f=document.createElement('iframe'); f.id='foreign'; \
                 f.style='position:absolute;left:120px;top:140px;width:420px;height:260px;border:0'; \
                 f.src={child_base}+'/stress/{iteration}'; document.body.appendChild(f);"
            ),
            false,
        )
        .await
        .expect("attach stress OOPIF");
        let initial_name = format!("Stress initial {iteration}");
        let initial = snapshot_until(&mut page, |snapshot| {
            snapshot
                .interactive()
                .any(|node| node.name.as_deref() == Some(&initial_name))
        })
        .await;
        let initial = named(&initial, &initial_name);
        let before = page.generation();
        page.click(&initial.node_ref, MouseButton::Left, 1, 0, false)
            .await
            .expect("navigate stress OOPIF");
        wait_for_generation(&page, before).await;
        let replacement_name = format!("Stress replacement {iteration}");
        snapshot_until(&mut page, |snapshot| {
            snapshot
                .interactive()
                .any(|node| node.name.as_deref() == Some(&replacement_name))
        })
        .await;
        let before_detach = page.generation();
        page.evaluate("foreign.remove()", false)
            .await
            .expect("detach stress OOPIF");
        wait_for_generation(&page, before_detach).await;
    }

    shutdown(launched);
}
