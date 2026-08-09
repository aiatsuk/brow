//! Iteration 2 against a real Chromium: event capture and the full gesture set.

mod common;

use std::sync::Arc;
use std::time::Duration;

use brow::browser::{launch, Headless, LaunchOptions, Launched};
use brow::page::{Page, Point, PointTarget};

async fn open_fixture(tag: &str, url: &str) -> (Launched, Page, common::Scratch) {
    let scratch = common::Scratch::new(tag);
    let mut opts = LaunchOptions::new(scratch.0.join("profile"));
    opts.headless = Headless::New;
    opts.window_size = (1280, 800);
    let launched = launch(&opts).await.expect("launch chromium");
    let page = Page::create(Arc::clone(&launched.client), url)
        .await
        .expect("open fixture");
    (launched, page, scratch)
}

fn shutdown(mut launched: Launched) {
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

/// Polls until `f` holds or the deadline passes. Event delivery is asynchronous;
/// a fixed sleep is either flaky or slow, and usually both.
async fn until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    f()
}

#[tokio::test(flavor = "multi_thread")]
async fn console_and_network_are_captured_and_redacted() {
    if !common::chrome_available() {
        return common::skip("console_and_network_are_captured_and_redacted");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, page, _scratch) = open_fixture("events", &fixture.url("/events")).await;
    let log = Arc::clone(&page.events);

    // The page logs four messages and throws once, and issues two fetches — all
    // asynchronously. Wait for the *terminal* state of each, not merely for rows
    // to appear, or the assertions race the protocol.
    let got = until(Duration::from_secs(10), || {
        log.console(false, 100)
            .iter()
            .any(|e| e.level == "exception" || e.text.contains("TypeError"))
            && log
                .network(false, 100)
                .iter()
                .filter(|r| r.url.contains("/api/ok") || r.url.contains("/missing-endpoint"))
                .filter(|r| r.finished)
                .count()
                >= 2
    })
    .await;
    let console = log.console(false, 100);
    let network = log.network(false, 100);
    assert!(
        got,
        "captured only {} console and {} network entries",
        console.len(),
        network.len()
    );

    // ---- console -----------------------------------------------------------
    let levels: Vec<&str> = console.iter().map(|e| e.level.as_str()).collect();
    assert!(levels.contains(&"log"), "levels seen: {levels:?}");
    assert!(
        levels.contains(&"warning") || levels.contains(&"warn"),
        "{levels:?}"
    );
    assert!(levels.contains(&"error"), "{levels:?}");

    assert!(
        console.iter().any(|e| e.text.contains("plain message 42")),
        "console arguments must be flattened onto one line: {:?}",
        console.iter().map(|e| &e.text).collect::<Vec<_>>()
    );

    // The uncaught TypeError from setTimeout must be captured with a location.
    let thrown = console
        .iter()
        .find(|e| e.level == "exception" || e.text.contains("TypeError"))
        .expect("the uncaught exception must be recorded");
    assert!(thrown.is_error());

    assert_eq!(
        log.console(true, 100).len(),
        console.iter().filter(|e| e.is_error()).count(),
        "--errors must filter, not truncate"
    );

    // ---- redaction ---------------------------------------------------------
    let everything = format!("{console:?}{network:?}");
    assert!(
        !everything.contains("sk_live_9f8a7b6c5d4e"),
        "a bearer token in console output reached storage"
    );
    assert!(
        !everything.contains("supersecret12345"),
        "a credential in a request URL reached storage"
    );
    assert!(
        everything.contains("page=2"),
        "redaction must not eat ordinary query parameters"
    );

    // ---- network -----------------------------------------------------------
    let ok = network
        .iter()
        .find(|r| r.url.contains("/api/ok"))
        .expect("the successful request");
    assert_eq!(ok.status, Some(200));
    assert_eq!(ok.method, "GET");
    assert!(ok.finished);
    assert!(!ok.is_failure());
    assert!(ok.mime.as_deref().unwrap_or("").contains("json"));

    let missing = network
        .iter()
        .find(|r| r.url.contains("/missing-endpoint"))
        .expect("the 404");
    assert_eq!(missing.status, Some(404));
    assert!(missing.is_failure(), "a 404 must count as a failure");

    let failures = log.network(true, 100);
    assert!(
        failures.iter().all(|r| r.is_failure()),
        "--failed must return only failures"
    );
    assert!(failures.iter().any(|r| r.url.contains("/missing-endpoint")));

    // One request stays one row through its whole lifecycle.
    let doc_rows = network
        .iter()
        .filter(|r| r.url.ends_with("/events"))
        .count();
    assert_eq!(
        doc_rows, 1,
        "the document request was recorded {doc_rows} times"
    );

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_chain_is_recorded_as_complete_hops() {
    if !common::chrome_available() {
        return common::skip("redirect_chain_is_recorded_as_complete_hops");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, page, _scratch) =
        open_fixture("redirects", &fixture.url("/redirect-start")).await;
    let log = Arc::clone(&page.events);

    let complete = until(Duration::from_secs(10), || {
        let rows = log.network(false, 100);
        let hops: Vec<_> = rows
            .iter()
            .filter(|row| row.url.contains("/redirect-"))
            .collect();
        hops.len() == 3 && hops.iter().all(|row| row.finished)
    })
    .await;
    let rows = log.network(false, 100);
    let hops: Vec<_> = rows
        .iter()
        .filter(|row| row.url.contains("/redirect-"))
        .collect();

    assert!(complete, "redirect hops never completed: {hops:#?}");
    assert_eq!(hops.len(), 3, "unexpected redirect rows: {hops:#?}");
    assert_eq!(
        hops.iter().map(|row| row.status).collect::<Vec<_>>(),
        vec![Some(302), Some(307), Some(200)]
    );
    assert_eq!(
        hops.iter()
            .map(|row| row.url.rsplit('/').next().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["redirect-start", "redirect-middle", "redirect-final"]
    );
    assert!(
        hops.windows(2)
            .all(|pair| pair[0].request_id == pair[1].request_id),
        "Chromium redirect hops should share one requestId: {hops:#?}"
    );
    assert!(
        hops.iter().all(|row| !row.is_failure()),
        "redirect responses are not failures: {hops:#?}"
    );

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn touch_gestures_are_distinguishable_by_the_page() {
    if !common::chrome_available() {
        return common::skip("touch_gestures_are_distinguishable_by_the_page");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("gestures", &fixture.url("/gestures")).await;

    async fn read(page: &Page, selector: &str) -> String {
        let expr = format!("document.querySelector('{selector}').textContent");
        page.evaluate(&expr, true)
            .await
            .unwrap_or_default()
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    // Touch emulation is off until the first touch gesture, because turning it on
    // changes how responsive sites render.
    assert_eq!(
        read(&page, "#touchcap").await,
        "false:0",
        "touch must not be enabled before it is asked for"
    );

    let centre = PointTarget::At(Point { x: 200.0, y: 200.0 });

    // ---- tap ---------------------------------------------------------------
    page.tap(&centre).await.expect("tap");
    assert_eq!(
        read(&page, "#out").await,
        "tap",
        "a short touch must read as a tap"
    );

    // Feature detection is a different matter, and this is the trap: enabling
    // touch emulation updates `navigator.maxTouchPoints` immediately, but
    // `'ontouchstart' in window` is fixed when the document is created and does
    // not change under an already-loaded page.
    let recheck = "document.querySelector('#touchcap').textContent = \
                   ('ontouchstart' in window) + ':' + navigator.maxTouchPoints";
    page.evaluate(recheck, false).await.unwrap();
    assert_eq!(
        read(&page, "#touchcap").await,
        "false:5",
        "maxTouchPoints updates live; ontouchstart does not"
    );

    // Reloading is what makes a responsive site actually switch to its touch
    // layout, so the harness has to be able to say so.
    page.navigate(&fixture.url("/gestures")).await.unwrap();
    assert_eq!(
        read(&page, "#touchcap").await,
        "true:5",
        "after a reload the page must detect a touch device"
    );

    // ---- long press --------------------------------------------------------
    page.long_press(&centre, Duration::from_millis(700))
        .await
        .expect("long press");
    let out = read(&page, "#out").await;
    assert!(
        out.starts_with("longpress:"),
        "expected a long-press, page saw {out:?}"
    );
    let held: u64 = out.trim_start_matches("longpress:").parse().unwrap_or(0);
    assert!(held >= 650, "held for only {held}ms");

    // ---- swipe -------------------------------------------------------------
    page.swipe(
        &PointTarget::At(Point { x: 200.0, y: 350.0 }),
        &PointTarget::At(Point { x: 200.0, y: 60.0 }),
        Duration::from_millis(300),
        20,
    )
    .await
    .expect("swipe");
    let out = read(&page, "#out").await;
    assert!(out.starts_with("swipe:"), "page saw {out:?}");
    let moves: u32 = out.trim_start_matches("swipe:").parse().unwrap_or(0);
    assert!(
        moves >= 15,
        "a swipe must deliver a stream of touchmove events, page saw {moves}"
    );

    // ---- pinch -------------------------------------------------------------
    // The assertion is that the compositor accepts and completes the gesture;
    // observing page zoom would require reading the visual viewport, which is a
    // separate concern.
    page.pinch(&centre, 1.8, None).await.expect("pinch in");
    page.pinch(&centre, 0.5, None).await.expect("pinch out");

    // ---- pointer drag ------------------------------------------------------
    page.drag(
        &PointTarget::At(Point { x: 540.0, y: 140.0 }),
        &PointTarget::At(Point { x: 700.0, y: 300.0 }),
        Duration::from_millis(200),
        16,
    )
    .await
    .expect("drag");
    let out = read(&page, "#dragout").await;
    assert!(out.starts_with("dropped:"), "page saw {out:?}");
    let parts: Vec<&str> = out.split(':').collect();
    let moves: u32 = parts[1].parse().unwrap_or(0);
    let end_x: f64 = parts[2].parse().unwrap_or(0.0);
    assert!(
        moves >= 12,
        "the button must stay held across the whole move stream, saw {moves} moves"
    );
    assert!(
        (end_x - 700.0).abs() < 3.0,
        "the drop landed at x={end_x}, expected 700"
    );

    shutdown(launched);
}
