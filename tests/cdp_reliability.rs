//! Reliability checks that need a real V8 isolate rather than a fake CDP peer.

#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use brow::browser::{launch, Headless, LaunchOptions, Launched};
use brow::page::Page;
use serde_json::json;

fn shutdown(mut launched: Launched) {
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_evaluate_terminates_busy_loop_and_remains_usable() {
    if !common::chrome_available() {
        return common::skip("runtime_evaluate_terminates_busy_loop_and_remains_usable");
    }
    let _slot = common::browser_slot();
    let scratch = common::Scratch::new("cdp-eval-timeout");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;

    let launched = launch(&options).await.expect("launch chromium");
    let page = Page::create(Arc::clone(&launched.client), "about:blank")
        .await
        .expect("create page");

    let started = Instant::now();
    let error = page
        .evaluate("for (;;) {}", false)
        .await
        .expect_err("V8 must terminate a synchronous busy loop");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "busy loop exceeded the V8 budget: {elapsed:?} ({error})"
    );

    let answer = page
        .evaluate("6 * 7", false)
        .await
        .expect("renderer must accept evaluations after termination");
    assert_eq!(answer.as_i64(), Some(42));

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_created_page_closes_its_target_and_flat_session() {
    if !common::chrome_available() {
        return common::skip("dropping_a_created_page_closes_its_target_and_flat_session");
    }
    let _slot = common::browser_slot();
    let scratch = common::Scratch::new("page-drop-cleanup");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;

    let launched = launch(&options).await.expect("launch chromium");
    let page = Page::create(Arc::clone(&launched.client), "about:blank")
        .await
        .expect("create owned page");
    let target_id = page.target_id.clone();
    let session_id = page.session_id.clone();
    drop(page);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let targets = launched
            .client
            .call("Target.getTargets", json!({}))
            .await
            .expect("enumerate targets after Page drop");
        let still_present = targets
            .get("targetInfos")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|target| {
                target.get("targetId").and_then(serde_json::Value::as_str)
                    == Some(target_id.as_str())
            });
        if !still_present {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owned target {target_id} survived Page drop"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        launched
            .client
            .call_on(&session_id, "Runtime.evaluate", json!({"expression": "1"}))
            .await
            .is_err(),
        "the flat session remained usable after its owned target closed"
    );

    shutdown(launched);
}
