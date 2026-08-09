//! Real-Chromium coverage for cross-origin out-of-process iframes.

#[allow(dead_code)]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brow::browser::{launch, Headless, LaunchOptions, Launched};
use brow::page::{ImageFormat, MouseButton, Node, Page, PointTarget, ScreenshotTarget, Snapshot};
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
async fn oopif_snapshot_actions_lifecycle_and_coverage_are_real() {
    if !common::chrome_available() {
        return common::skip("oopif_snapshot_actions_lifecycle_and_coverage_are_real");
    }
    let _slot = common::browser_slot();
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
    page.click(&navigate.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect("navigate inside OOPIF");
    wait_for_generation(&page, generation_before).await;
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
