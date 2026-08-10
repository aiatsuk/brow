//! Deterministic coverage for navigation settlement and atomic checkpoints.

mod common;

use std::sync::Arc;
use std::time::Duration;
use std::{future::Future, task::Poll};

use brow::browser::{launch, Headless, LaunchOptions};
use brow::cdp::CdpClient;
use brow::checkpoint::{self, CheckpointOptions};
#[cfg(debug_assertions)]
use brow::ipc::WaitConditions;
use brow::ipc::WaitPolicy;
use brow::page::{
    MouseButton, NavigationKind, NavigationScope, NavigationTrigger, Page, PageError, WaitOutcome,
};
use sha2::{Digest, Sha256};

fn ref_named(snapshot: &brow::page::tree::Snapshot, name: &str) -> String {
    snapshot
        .interactive()
        .find(|node| node.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing interactive node {name:?}"))
        .node_ref
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_navigate_ignores_child_load_and_times_out_fail_closed() {
    if !common::chrome_available() {
        return common::skip("direct_navigate_ignores_child_load_and_times_out_fail_closed");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("direct-navigate-root-lifecycle");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/second"))
        .await
        .expect("open initial page");
    let root_session = page.session_id.clone();
    let root_frame = page.frame_id.clone();
    let mut lifecycle = launched.client.subscribe();
    let destination = fixture.url("/nav-stream-held");
    let mut navigation = Box::pin(page.navigate_and_observe(&destination, Duration::from_secs(3)));

    let child_stopped = CdpClient::wait_for(&mut lifecycle, Duration::from_secs(2), |event| {
        event.session_id.as_deref() == Some(root_session.as_str())
            && event.method == "Page.frameStoppedLoading"
            && event
                .params
                .get("frameId")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|frame| frame != root_frame)
    });
    tokio::pin!(child_stopped);
    tokio::select! {
        result = &mut navigation => panic!("direct navigation accepted a child-frame load while root was held: {result:?}"),
        event = &mut child_stopped => assert!(event.is_some(), "fast child frame did not finish"),
    }
    fixture.wait_for_held_navigation(Duration::from_secs(2));
    let completed = std::future::poll_fn(|context| {
        Poll::Ready(match navigation.as_mut().poll(context) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        })
    })
    .await;
    assert!(
        completed.is_none(),
        "root-held navigation completed from child lifecycle: {completed:?}"
    );
    fixture.release_held_navigation();
    let final_url = navigation
        .await
        .expect("root completion should settle direct navigation");
    assert_eq!(final_url, destination);

    // The same oracle with a short explicit deadline must return a typed error,
    // never silent success after wait_for returned None.
    let timeout_fixture = common::serve();
    let timeout_destination = timeout_fixture.url("/nav-stream-held");
    let error = page
        .navigate_and_observe(&timeout_destination, Duration::from_millis(400))
        .await
        .expect_err("held root navigation must time out fail-closed");
    timeout_fixture.release_held_navigation();
    let PageError::Navigation { reason, .. } = error else {
        panic!("navigation timeout was not typed: {error:?}");
    };
    assert!(reason.contains("timed out"), "unexpected reason: {reason}");

    let _ = page.close().await;
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn predispatch_navigation_is_not_attributed_to_a_later_press() {
    if !common::chrome_available() {
        return common::skip("predispatch_navigation_is_not_attributed_to_a_later_press");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("press-causal-event-floor");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    let session_id = page.session_id.clone();
    let dispatch_hold =
        brow::page::test_support::hold_next_press_before_dispatch(session_id.clone());
    let mut lifecycle = launched.client.subscribe();
    let mut action =
        Box::pin(page.press_with_wait("Tab", WaitPolicy::Commit, Duration::from_millis(400)));
    tokio::select! {
        result = &mut action => panic!("press completed before its pre-dispatch hook: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(2), dispatch_hold.wait_until_entered()) => {
            entered.expect("press reached its pre-dispatch hook");
        }
    }

    launched
        .client
        .call_on(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "history.pushState({}, '', '#before-press'); true",
                "returnByValue": true,
            }),
        )
        .await
        .expect("publish navigation before the press floor");
    let published = CdpClient::wait_for(&mut lifecycle, Duration::from_secs(1), |event| {
        event.session_id.as_deref() == Some(session_id.as_str())
            && event.method == "Page.navigatedWithinDocument"
    })
    .await;
    assert!(
        published.is_some(),
        "same-document navigation was not published"
    );
    dispatch_hold.release();

    let error = action
        .await
        .expect_err("a pre-dispatch navigation must not satisfy Commit for the later press");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("press causality failure was not structured: {error:?}");
    };
    assert_eq!(receipt.dispatched, Some(true), "{receipt:?}");
    assert_eq!(receipt.outcome, WaitOutcome::TimedOut, "{receipt:?}");
    assert_eq!(receipt.navigation, NavigationKind::None, "{receipt:?}");
    assert!(
        page.location()
            .await
            .expect("post-press location")
            .0
            .ends_with("#before-press"),
        "the pre-dispatch navigation itself must have happened"
    );

    page.close().await.expect("close page");
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn stable_action_keeps_navigation_causality_and_waits_for_root_loading() {
    if !common::chrome_available() {
        return common::skip("stable_action_keeps_navigation_causality_and_waits_for_root_loading");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("stable-navigation-causality");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");

    let snapshot = page.snapshot().await.expect("navigation snapshot");
    let slow_cross = ref_named(&snapshot, "Slow cross-document");
    let (_, receipt) = page
        .click_with_wait(
            &slow_cross,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Stable,
            Duration::from_secs(4),
        )
        .await
        .expect("stable cross-document action");
    assert_eq!(receipt.outcome, WaitOutcome::Stable);
    assert_eq!(receipt.navigation, NavigationKind::CrossDocument);
    assert_eq!(receipt.navigation_scope, NavigationScope::Root);
    assert!(receipt.final_url.ends_with("/nav-slow"), "{receipt:?}");
    assert!(receipt.final_generation > receipt.before_generation);

    page.navigate(&fixture.url("/navigation"))
        .await
        .expect("return to navigation fixture");
    let snapshot = page.snapshot().await.expect("held navigation snapshot");
    let held_cross = ref_named(&snapshot, "Held cross-document");
    let root_session = page.session_id.clone();
    let root_frame = page.frame_id.clone();
    let mut lifecycle = launched.client.subscribe();
    let mut action = Box::pin(page.click_with_wait(
        &held_cross,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Stable,
        Duration::from_secs(4),
    ));

    let loading = CdpClient::wait_for(&mut lifecycle, Duration::from_secs(2), |event| {
        event.session_id.as_deref() == Some(root_session.as_str())
            && event.method == "Page.frameStartedLoading"
            && event
                .params
                .get("frameId")
                .and_then(serde_json::Value::as_str)
                == Some(root_frame.as_str())
    });
    tokio::pin!(loading);
    tokio::select! {
        result = &mut action => panic!("stable action returned while root navigation was starting: {result:?}"),
        event = &mut loading => assert!(event.is_some(), "root loading event was not observed"),
    }
    fixture.wait_for_held_navigation(Duration::from_secs(2));

    // Poll once after the browser and fixture have both acknowledged the held
    // navigation. No duration is used as the oracle: the future must be pending
    // because root_loading is authoritative.
    let completed = std::future::poll_fn(|context| {
        Poll::Ready(match action.as_mut().poll(context) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        })
    })
    .await;
    assert!(
        completed.is_none(),
        "stable action completed while the root response was held: {completed:?}"
    );

    fixture.release_held_navigation();
    let (_, held_receipt) = action.await.expect("held navigation settles after release");
    assert_eq!(held_receipt.outcome, WaitOutcome::Stable);
    assert_eq!(held_receipt.navigation, NavigationKind::CrossDocument);
    assert!(!held_receipt.root_loading, "{held_receipt:?}");
    assert!(held_receipt.target_settled, "{held_receipt:?}");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn stable_action_reports_an_unaccepted_beforeunload_dialog() {
    if !common::chrome_available() {
        return common::skip("stable_action_reports_an_unaccepted_beforeunload_dialog");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("stable-beforeunload");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");

    let initial = page.snapshot().await.expect("initial snapshot");
    let protect = ref_named(&initial, "Protect navigation");
    page.click_with_wait(
        &protect,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Auto,
        Duration::from_secs(1),
    )
    .await
    .expect("install beforeunload with trusted input");

    let protected = page.snapshot().await.expect("protected snapshot");
    let leave = ref_named(&protected, "Slow cross-document");
    let (before_url, _) = page.location().await.expect("protected URL");
    let before_generation = page.generation();
    let root_session = page.session_id.clone();
    let mut dialog_events = launched.client.subscribe();

    let error = page
        .click_with_wait(
            &leave,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Stable,
            Duration::from_secs(1),
        )
        .await
        .expect_err("beforeunload must block a stable action");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("unexpected beforeunload action error: {error:?}");
    };
    assert_eq!(receipt.dispatched, Some(true));
    assert_eq!(receipt.outcome, WaitOutcome::DialogBlocked);
    assert_eq!(receipt.dialog_type.as_deref(), Some("beforeunload"));
    assert!(receipt.dialog_message.is_some());
    assert_eq!(receipt.navigation, NavigationKind::Unknown);
    assert_eq!(receipt.navigation_scope, NavigationScope::Root);
    assert_eq!(receipt.before_url, before_url);
    assert_eq!(receipt.final_url, before_url);
    assert_eq!(receipt.before_generation, before_generation);
    assert_eq!(receipt.final_generation, before_generation);

    let opening = CdpClient::wait_for(&mut dialog_events, Duration::from_millis(500), |event| {
        event.session_id.as_deref() == Some(root_session.as_str())
            && event.method == "Page.javascriptDialogOpening"
    })
    .await
    .expect("beforeunload opening remains observable");
    assert_eq!(
        opening
            .params
            .get("type")
            .and_then(serde_json::Value::as_str),
        Some("beforeunload")
    );
    let closed = CdpClient::wait_for(&mut dialog_events, Duration::from_millis(100), |event| {
        event.session_id.as_deref() == Some(root_session.as_str())
            && event.method == "Page.javascriptDialogClosed"
    })
    .await;
    assert!(
        closed.is_none(),
        "brow must not accept or dismiss the dialog"
    );

    page.close()
        .await
        .expect("close protected target without handling dialog");
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_history_stability_pointer_and_checkpoint_are_coherent() {
    if !common::chrome_available() {
        return common::skip("actions_history_stability_pointer_and_checkpoint_are_coherent");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("navigation-flow");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    options.window_size = (1280, 800);
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");

    // Dispatch-only input is allowed, but once Chromium reports a pending root
    // navigation, evidence reads must wait rather than snapshot the old page.
    let snap = page.snapshot().await.expect("initial navigation snapshot");
    let slow_cross = ref_named(&snap, "Slow cross-document");
    let (_, dispatch_only) = page
        .click_with_wait(
            &slow_cross,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::None,
            Duration::from_secs(1),
        )
        .await
        .expect("dispatch slow navigation");
    assert_eq!(dispatch_only.outcome, WaitOutcome::NotWaited);
    tokio::time::sleep(Duration::from_millis(75)).await;
    let snapshot_started = std::time::Instant::now();
    let settled_snapshot = page
        .snapshot()
        .await
        .expect("snapshot waits for known pending navigation");
    assert_eq!(settled_snapshot.title, "slow navigation complete");
    assert!(
        snapshot_started.elapsed() >= Duration::from_millis(150),
        "snapshot returned before the pending document settled"
    );
    page.navigate(&fixture.url("/navigation"))
        .await
        .expect("return to navigation fixture");

    // A timer-driven pushState starts after input dispatch. Auto must observe it
    // rather than returning a stale document to the following snapshot.
    let snap = page.snapshot().await.expect("navigation snapshot");
    let spa = ref_named(&snap, "Delayed SPA");
    let (_, receipt) = page
        .click_with_wait(
            &spa,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Auto,
            Duration::from_secs(3),
        )
        .await
        .expect("settled SPA click");
    assert_eq!(receipt.outcome, WaitOutcome::Committed);
    assert_eq!(receipt.navigation, NavigationKind::SameDocument);
    assert!(receipt.final_url.ends_with("/navigation#settled"));
    assert!(receipt.final_generation > receipt.before_generation);

    let back = page
        .traverse_history(-1, None, Duration::from_secs(3))
        .await
        .expect("same-document back");
    assert!(back.final_url.ends_with("/navigation"), "{back:?}");
    let forward = page
        .traverse_history(1, None, Duration::from_secs(3))
        .await
        .expect("same-document forward");
    assert!(forward.final_url.ends_with("#settled"), "{forward:?}");
    let reload = page
        .reload_with_wait(true, None, Duration::from_secs(3))
        .await
        .expect("guarded reload");
    assert_eq!(reload.navigation, NavigationKind::CrossDocument);
    assert_eq!(reload.navigation_trigger, NavigationTrigger::Reload);
    assert_eq!(reload.outcome, WaitOutcome::Loaded);

    // Stable waits through a finite fetch and the render that follows it.
    let snap = page.snapshot().await.expect("post-reload snapshot");
    let data = ref_named(&snap, "Load data");
    let (_, stable) = page
        .click_with_wait(
            &data,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Stable,
            Duration::from_secs(3),
        )
        .await
        .expect("data click reaches stable");
    assert_eq!(stable.outcome, WaitOutcome::Stable);
    let state = page
        .evaluate("document.querySelector('#state').textContent", true)
        .await
        .expect("read rendered data state");
    assert_eq!(state, "data-ready");

    // Pointer parking is explicit and fires the page's real mouseleave path.
    let snap = page.snapshot().await.expect("pointer snapshot");
    let hover = ref_named(&snap, "Hover target");
    page.hover(&hover).await.expect("engage hover card");
    let shown = page
        .evaluate("!document.querySelector('#hover-card').hidden", true)
        .await
        .unwrap();
    assert_eq!(shown, true);
    let parked = page.park_pointer().await.expect("park pointer");
    assert_eq!(parked.action.dispatched, Some(true));
    assert_eq!(parked.before, None);
    assert_eq!(parked.after.x, -1.0);
    let shown = page
        .evaluate("!document.querySelector('#hover-card').hidden", true)
        .await
        .unwrap();
    assert_eq!(shown, false);

    // A timeout after trusted input retains dispatched=true and does not replay.
    let snap = page.snapshot().await.expect("side-effect snapshot");
    let submit = ref_named(&snap, "Submit once");
    let error = page
        .click_with_wait(
            &submit,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Stable,
            Duration::from_millis(300),
        )
        .await
        .expect_err("hung fetch must prevent stable");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("expected a partial-success receipt, got {error:?}");
    };
    assert_eq!(receipt.dispatched, Some(true));
    assert_eq!(receipt.outcome, WaitOutcome::TimedOut);
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while fixture.side_effect_count() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fixture.side_effect_count(),
        1,
        "trusted action ran more than once"
    );

    // Cross-document auto settlement must make the next snapshot observe the
    // new document even though navigation starts from a delayed timer.
    page.navigate(&fixture.url("/navigation"))
        .await
        .expect("reset navigation fixture");
    let snap = page.snapshot().await.expect("cross-document snapshot");
    let cross = ref_named(&snap, "Delayed cross-document");
    let (_, cross_receipt) = page
        .click_with_wait(
            &cross,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Auto,
            Duration::from_secs(3),
        )
        .await
        .expect("settled cross-document click");
    assert_eq!(cross_receipt.navigation, NavigationKind::CrossDocument);
    let snap = page.snapshot().await.expect("new-document snapshot");
    assert_eq!(snap.title, "second page");
    page.evaluate(
        "(() => { const a = document.createElement('a'); \
         a.href = 'https://snapshot-user:snapshot-pass@app.test/private?access_token=snapshot-query'; \
         a.textContent = 'Private destination'; document.body.appendChild(a); return a.href; })()",
        false,
    )
    .await
    .expect("install credential-bearing snapshot attribute");

    let result = checkpoint::create(
        &mut page,
        CheckpointOptions {
            session: "navigation-flow".into(),
            name: "settled-second".into(),
            full_page: false,
            wait: WaitPolicy::Stable,
            timeout: Duration::from_secs(3),
            quiet: Duration::from_millis(100),
            park_pointer: false,
            output_root: Some(scratch.0.join("checkpoints")),
        },
    )
    .await
    .expect("atomic checkpoint");
    assert!(result.manifest.complete, "{:#?}", result.manifest);
    assert_eq!(result.manifest.generation, snap.generation);
    for required in [
        "manifest.json",
        "snapshot.json",
        "screenshot.png",
        "console-errors.json",
        "network-failures.json",
    ] {
        assert!(result.path.join(required).is_file(), "missing {required}");
    }
    for (name, expected) in &result.manifest.files {
        let bytes = std::fs::read(result.path.join(name)).expect("read evidence file");
        assert_eq!(expected.bytes, bytes.len());
        assert_eq!(expected.sha256, format!("{:x}", Sha256::digest(&bytes)));
    }
    let durable_snapshot =
        std::fs::read_to_string(result.path.join("snapshot.json")).expect("read snapshot evidence");
    for secret in ["snapshot-user", "snapshot-pass", "snapshot-query"] {
        assert!(
            !durable_snapshot.contains(secret),
            "checkpoint snapshot leaked {secret}: {durable_snapshot}"
        );
    }
    assert!(
        durable_snapshot.contains(brow::redact::MASK),
        "credential-bearing href must be visibly redacted"
    );
    assert!(std::fs::read_dir(scratch.0.join("checkpoints"))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".partial-")));

    let manifest_before = std::fs::read(result.path.join("manifest.json")).unwrap();
    let collision = checkpoint::create(
        &mut page,
        CheckpointOptions {
            session: "navigation-flow".into(),
            name: "settled-second".into(),
            full_page: false,
            wait: WaitPolicy::None,
            timeout: Duration::from_secs(1),
            quiet: Duration::ZERO,
            park_pointer: false,
            output_root: Some(scratch.0.join("checkpoints")),
        },
    )
    .await
    .expect_err("existing checkpoint must not be overwritten");
    assert!(matches!(
        collision,
        checkpoint::CheckpointError::Collision(_)
    ));
    assert_eq!(
        std::fs::read(result.path.join("manifest.json")).unwrap(),
        manifest_before
    );

    let confined = checkpoint::create(
        &mut page,
        CheckpointOptions {
            session: "navigation-flow".into(),
            name: "../../token=checkpoint-name-secret".into(),
            full_page: false,
            wait: WaitPolicy::None,
            timeout: Duration::from_secs(1),
            quiet: Duration::ZERO,
            park_pointer: false,
            output_root: Some(scratch.0.join("checkpoints")),
        },
    )
    .await
    .expect("traversal-shaped name is encoded");
    assert_eq!(
        confined.path.parent(),
        Some(scratch.0.join("checkpoints").as_path())
    );
    assert!(!confined
        .path
        .to_string_lossy()
        .contains("checkpoint-name-secret"));
    assert!(!confined.manifest.name.contains("checkpoint-name-secret"));
    assert!(confined.manifest.name.contains(brow::redact::MASK));

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn auto_discovery_window_starts_after_successful_click_dispatch() {
    if !common::chrome_available() {
        return common::skip("auto_discovery_window_starts_after_successful_click_dispatch");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("auto-post-dispatch-discovery");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");

    let snapshot = page.snapshot().await.expect("navigation snapshot");
    let delayed_spa = ref_named(&snapshot, "Delayed SPA");
    let hold = brow::page::test_support::hold_next_click_before_dispatch(page.session_id.clone());
    let mut action = Box::pin(page.click_with_wait(
        &delayed_spa,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Auto,
        Duration::from_secs(1),
    ));

    tokio::select! {
        result = &mut action => panic!("click completed before its dispatch barrier: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(2), hold.wait_until_entered()) => {
            entered.expect("click reached the pre-dispatch barrier");
        }
    }

    // Hold longer than both the 250 ms Auto discovery window and the complete
    // one-second action timeout. Success proves neither budget starts during
    // baseline capture or actionability preflight.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    hold.release();

    let (_, receipt) = tokio::time::timeout(Duration::from_secs(3), action)
        .await
        .expect("post-dispatch Auto wait remained bounded")
        .expect("delayed SPA click settled");
    assert_eq!(receipt.outcome, WaitOutcome::Committed, "{receipt:?}");
    assert_eq!(
        receipt.navigation,
        NavigationKind::SameDocument,
        "{receipt:?}"
    );
    assert!(
        receipt.final_url.ends_with("/navigation#settled"),
        "{receipt:?}"
    );
    assert!(receipt.final_url_observed, "{receipt:?}");
    assert!(receipt.final_generation > receipt.before_generation);

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn committed_receipt_waits_for_router_generation_acknowledgement() {
    if !common::chrome_available() {
        return common::skip("committed_receipt_waits_for_router_generation_acknowledgement");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("navigation-generation-barrier");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");

    let snapshot = page.snapshot().await.expect("navigation snapshot");
    let delayed_spa = ref_named(&snapshot, "Delayed SPA");
    let before_generation = snapshot.generation;
    let generation = brow::page::test_support::generation_handle(&page);
    let hold =
        brow::page::test_support::hold_next_root_navigation_in_router(page.session_id.clone());
    let mut action = Box::pin(page.click_with_wait(
        &delayed_spa,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Commit,
        Duration::from_secs(2),
    ));

    tokio::select! {
        result = &mut action => panic!("action completed before the router hold: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(1), hold.wait_until_entered()) => {
            entered.expect("target router received the causal navigation");
        }
    }
    assert!(generation.advance_unrelated() > before_generation);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut action)
            .await
            .is_err(),
        "the action receipt escaped before the router acknowledged ref invalidation"
    );

    hold.release();
    let (_, receipt) = action
        .await
        .expect("committed action settles after router ack");
    assert_eq!(receipt.outcome, WaitOutcome::Committed, "{receipt:?}");
    assert_eq!(receipt.navigation, NavigationKind::SameDocument);
    assert!(receipt.final_generation > before_generation, "{receipt:?}");
    assert!(
        receipt.final_generation > receipt.before_generation,
        "{receipt:?}"
    );

    page.navigate(&fixture.url("/navigation"))
        .await
        .expect("reset for stable router proof");
    let snapshot = page.snapshot().await.expect("stable navigation snapshot");
    let delayed_spa = ref_named(&snapshot, "Delayed SPA");
    let stable_hold =
        brow::page::test_support::hold_next_root_navigation_in_router(page.session_id.clone());
    let mut stable = Box::pin(page.click_with_wait(
        &delayed_spa,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Stable,
        Duration::from_secs(2),
    ));
    tokio::select! {
        result = &mut stable => panic!("stable action escaped before router hold: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(1), stable_hold.wait_until_entered()) => {
            entered.expect("router received stable action navigation");
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut stable)
            .await
            .is_err(),
        "stable receipt used the pre-router evidence epoch"
    );
    stable_hold.release();
    let (_, stable_receipt) = stable
        .await
        .expect("stable action reproves after router ack");
    assert_eq!(
        stable_receipt.outcome,
        WaitOutcome::Stable,
        "{stable_receipt:?}"
    );
    assert!(
        stable_receipt.final_generation > stable_receipt.before_generation,
        "{stable_receipt:?}"
    );

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn published_navigation_invalidates_a_ref_before_trusted_click_dispatch() {
    if !common::chrome_available() {
        return common::skip(
            "published_navigation_invalidates_a_ref_before_trusted_click_dispatch",
        );
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("navigation-predispatch-ref-barrier");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    page.evaluate(
        "(() => { window.__trustedClicks = 0; const button = document.createElement('button'); button.textContent = 'Barrier target'; button.onclick = () => { window.__trustedClicks += 1; }; document.body.appendChild(button); return true; })()",
        false,
    )
    .await
    .expect("install trusted-click sentinel");
    let snapshot = page.snapshot().await.expect("barrier target snapshot");
    let target = ref_named(&snapshot, "Barrier target");
    let session_id = page.session_id.clone();
    let dispatch_hold =
        brow::page::test_support::hold_next_click_before_dispatch(session_id.clone());
    let router_hold =
        brow::page::test_support::hold_next_root_navigation_in_router(session_id.clone());
    let mut action = Box::pin(page.click_with_wait(
        &target,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Commit,
        Duration::from_secs(2),
    ));

    tokio::select! {
        result = &mut action => panic!("click completed before its pre-dispatch hook: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(2), dispatch_hold.wait_until_entered()) => {
            entered.expect("click reached its final pre-dispatch hook");
        }
    }
    launched
        .client
        .call_on(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "history.pushState({}, '', '#stale-before-click'); true",
                "returnByValue": true,
            }),
        )
        .await
        .expect("publish pre-dispatch same-document navigation");
    tokio::time::timeout(Duration::from_secs(1), router_hold.wait_until_entered())
        .await
        .expect("router received the published ref invalidation");

    dispatch_hold.release();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut action)
            .await
            .is_err(),
        "click escaped while its published generation invalidation was held"
    );
    router_hold.release();
    let error = action
        .await
        .expect_err("stale pre-dispatch ref must prevent trusted input");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("expected a structured prevented-action receipt");
    };
    assert_eq!(receipt.dispatched, Some(false), "{receipt:?}");
    assert!(
        receipt
            .blockers
            .iter()
            .any(|blocker| blocker.contains("stale")),
        "{receipt:?}"
    );
    let clicks = page
        .evaluate("window.__trustedClicks", false)
        .await
        .expect("read trusted-click sentinel");
    assert_eq!(clicks.as_u64(), Some(0), "trusted mouse press leaked");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn stable_receipt_waits_for_the_eventlog_published_prefix() {
    if !common::chrome_available() {
        return common::skip("stable_receipt_waits_for_the_eventlog_published_prefix");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("stable-eventlog-prefix");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    let fetch_url = fixture.url("/api/ok?stable-eventlog-prefix=1");
    page.evaluate(
        &format!(
            "(() => {{ const button = document.createElement('button'); button.textContent = 'Fetch under recorder hold'; button.onclick = () => fetch({fetch_url:?}).then(response => response.arrayBuffer()); document.body.appendChild(button); return true; }})()"
        ),
        false,
    )
    .await
    .expect("install fetch control");
    let snapshot = page.snapshot().await.expect("fetch control snapshot");
    let target = ref_named(&snapshot, "Fetch under recorder hold");
    let event_log = Arc::clone(&page.events);
    let activity_before = event_log.activity();
    let recorder_hold =
        brow::page::test_support::hold_next_network_request_in_recorder(page.session_id.clone());
    let mut action = Box::pin(page.click_with_wait(
        &target,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Stable,
        Duration::from_secs(3),
    ));

    tokio::select! {
        result = &mut action => panic!("stable action completed before EventLog hold: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(2), recorder_hold.wait_until_entered()) => {
            entered.expect("EventLog received the published request");
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(400), &mut action)
            .await
            .is_err(),
        "stable action escaped while EventLog had not processed a published request"
    );
    assert_eq!(
        event_log.activity().revision,
        activity_before.revision,
        "held recorder must not have updated activity yet"
    );

    recorder_hold.release();
    let (_, receipt) = action
        .await
        .expect("stable action settles after recorder prefix catches up");
    assert_eq!(receipt.outcome, WaitOutcome::Stable, "{receipt:?}");
    assert_eq!(receipt.active_finite_requests, 0, "{receipt:?}");
    assert!(
        event_log.activity().revision >= activity_before.revision.saturating_add(2),
        "request start and finish must both advance the stable evidence epoch"
    );

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn preexisting_root_load_cannot_settle_a_later_input_action() {
    if !common::chrome_available() {
        return common::skip("preexisting_root_load_cannot_settle_a_later_input_action");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("preexisting-root-load-causality");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    page.evaluate(
        "(() => { const button = document.createElement('button'); button.textContent = 'Open streaming document'; button.onclick = () => { location.href = '/nav-stream-held'; }; document.body.appendChild(button); return true; })()",
        false,
    )
    .await
    .expect("install streaming navigation control");
    let snapshot = page.snapshot().await.expect("stream control snapshot");
    let stream = ref_named(&snapshot, "Open streaming document");
    let (_, commit) = page
        .click_with_wait(
            &stream,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Commit,
            Duration::from_secs(2),
        )
        .await
        .expect("streaming response reaches frame commit before body completion");
    assert_eq!(commit.outcome, WaitOutcome::Committed, "{commit:?}");
    assert!(
        commit.root_loading,
        "stream body should still be held: {commit:?}"
    );
    fixture.wait_for_held_navigation(Duration::from_secs(1));

    let mut later_action =
        Box::pin(page.press_with_wait("Tab", WaitPolicy::Load, Duration::from_millis(500)));
    tokio::select! {
        result = &mut later_action => panic!("later action settled before old load release: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    fixture.release_held_navigation();
    let error = later_action
        .await
        .expect_err("old document load must not satisfy the later action's Load wait");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("expected structured timeout receipt");
    };
    assert_ne!(receipt.outcome, WaitOutcome::Loaded, "{receipt:?}");
    assert_eq!(receipt.navigation, NavigationKind::None, "{receipt:?}");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn window_open_commits_without_claiming_popup_load_or_root_invalidation() {
    if !common::chrome_available() {
        return common::skip(
            "window_open_commits_without_claiming_popup_load_or_root_invalidation",
        );
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("window-open-receipt");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    page.evaluate(
        "(() => { for (const [id, label] of [['popup-commit', 'Open popup'], ['popup-load', 'Open popup and wait']]) { const button = document.createElement('button'); button.id = id; button.textContent = label; button.onclick = () => window.open('/nav-held', '_blank'); document.body.appendChild(button); } return true; })()",
        false,
    )
    .await
    .expect("install popup controls");

    let snapshot = page.snapshot().await.expect("popup snapshot");
    let before_generation = snapshot.generation;
    let open_popup = ref_named(&snapshot, "Open popup");
    let (_, receipt) = page
        .click_with_wait(
            &open_popup,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Commit,
            Duration::from_secs(2),
        )
        .await
        .expect("window.open reaches its own commit boundary");
    assert_eq!(
        receipt.navigation,
        NavigationKind::WindowOpen,
        "{receipt:?}"
    );
    assert_eq!(receipt.outcome, WaitOutcome::Committed, "{receipt:?}");
    assert_eq!(receipt.final_generation, before_generation, "{receipt:?}");
    assert!(receipt.target_settled, "{receipt:?}");
    fixture.wait_for_held_navigation(Duration::from_secs(1));
    fixture.release_held_navigation();

    let snapshot = page.snapshot().await.expect("post-popup root snapshot");
    let wait_popup = ref_named(&snapshot, "Open popup and wait");
    let error = page
        .click_with_wait(
            &wait_popup,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Load,
            Duration::from_millis(500),
        )
        .await
        .expect_err("root session cannot prove the popup target's load lifecycle");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("expected a typed wait failure, got {error:?}");
    };
    assert_eq!(
        receipt.navigation,
        NavigationKind::WindowOpen,
        "{receipt:?}"
    );
    assert_ne!(receipt.outcome, WaitOutcome::Loaded, "{receipt:?}");
    assert!(
        receipt
            .blockers
            .iter()
            .any(|blocker| blocker.contains("popup")),
        "{receipt:?}"
    );

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn typed_url_wait_uses_router_watermark_and_reports_conditions_met() {
    if !common::chrome_available() {
        return common::skip("typed_url_wait_uses_router_watermark_and_reports_conditions_met");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("typed-wait-router-watermark");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    let before_generation = page.generation();
    let hold =
        brow::page::test_support::hold_next_root_navigation_in_router(page.session_id.clone());
    page.evaluate("history.pushState({}, '', '#typed-wait'); true", false)
        .await
        .expect("publish navigation before typed wait subscribes");
    tokio::time::timeout(Duration::from_secs(1), hold.wait_until_entered())
        .await
        .expect("router received pre-subscription navigation");
    let conditions = WaitConditions {
        url: Some("*#typed-wait".into()),
        generation_after: None,
        load: false,
        stable: false,
    };
    let mut wait = Box::pin(page.wait_for_conditions(
        &conditions,
        Duration::from_secs(2),
        Duration::from_millis(100),
    ));

    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut wait)
            .await
            .is_err(),
        "Runtime URL observation must not outrun router generation truth"
    );
    hold.release();
    let receipt = wait
        .await
        .expect("typed wait settles after router watermark");
    assert_eq!(receipt.outcome, WaitOutcome::ConditionsMet, "{receipt:?}");
    assert_eq!(receipt.navigation, NavigationKind::None, "{receipt:?}");
    assert!(receipt.final_generation > before_generation, "{receipt:?}");

    let budget_hold =
        brow::page::test_support::hold_next_root_navigation_in_router(page.session_id.clone());
    page.evaluate("history.pushState({}, '', '#budget-held'); true", false)
        .await
        .expect("publish held event for timeout budget");
    tokio::time::timeout(Duration::from_secs(1), budget_hold.wait_until_entered())
        .await
        .expect("router received budget event");
    let never = WaitConditions {
        url: Some("*#never-matches".into()),
        generation_after: None,
        load: false,
        stable: false,
    };
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(220)).await;
        budget_hold.release();
    });
    let started = std::time::Instant::now();
    page.wait_for_conditions(
        &never,
        Duration::from_millis(300),
        Duration::from_millis(50),
    )
    .await
    .expect_err("pre-subscription synchronization must share the typed wait deadline");
    let elapsed = started.elapsed();
    assert!(
        elapsed <= Duration::from_millis(450),
        "typed wait reused its timeout after prefix synchronization: {elapsed:?}"
    );
    release.await.expect("budget barrier release task");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_waits_for_previously_published_router_navigation() {
    if !common::chrome_available() {
        return common::skip("snapshot_waits_for_previously_published_router_navigation");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("snapshot-router-watermark");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    let before_generation = page.generation();
    let hold =
        brow::page::test_support::hold_next_root_navigation_in_router(page.session_id.clone());
    page.evaluate("history.pushState({}, '', '#snapshot-fresh'); true", false)
        .await
        .expect("publish navigation before snapshot");
    tokio::time::timeout(Duration::from_secs(1), hold.wait_until_entered())
        .await
        .expect("router received pre-snapshot navigation");

    let mut snapshot = Box::pin(page.snapshot());
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut snapshot)
            .await
            .is_err(),
        "snapshot minted refs before the published navigation reached the router"
    );
    hold.release();
    let snapshot = snapshot
        .await
        .expect("snapshot captures the acknowledged epoch");
    assert!(snapshot.generation > before_generation, "{snapshot:?}");
    assert!(snapshot.url.ends_with("/navigation#snapshot-fresh"));
    assert!(snapshot
        .nodes
        .iter()
        .filter(|node| !node.node_ref.is_empty())
        .all(|node| node
            .node_ref
            .starts_with(&format!("@node-{}-", snapshot.generation))));

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn same_process_iframe_navigation_is_attributed_to_the_acted_frame() {
    if !common::chrome_available() {
        return common::skip("same_process_iframe_navigation_is_attributed_to_the_acted_frame");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("same-process-frame-navigation");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    page.evaluate(
        "new Promise(resolve => { const frame = document.createElement('iframe'); frame.onload = () => { const button = frame.contentDocument.createElement('button'); button.textContent = 'Navigate child frame'; button.onclick = () => { frame.contentWindow.location.href = '/second'; }; frame.contentDocument.body.appendChild(button); resolve(true); }; frame.src = '/frame-inner'; document.body.appendChild(frame); })",
        false,
    )
    .await
    .expect("install same-process child control");
    let snapshot = page.snapshot().await.expect("child-frame snapshot");
    let before_generation = snapshot.generation;
    let child = ref_named(&snapshot, "Navigate child frame");
    let (_, receipt) = page
        .click_with_wait(
            &child,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Commit,
            Duration::from_secs(2),
        )
        .await
        .expect("same-process child navigation commits");
    assert_eq!(
        receipt.navigation,
        NavigationKind::CrossDocument,
        "{receipt:?}"
    );
    assert_eq!(
        receipt.navigation_scope,
        NavigationScope::Subframe,
        "{receipt:?}"
    );
    assert!(receipt.final_generation > before_generation, "{receipt:?}");
    assert!(receipt.final_url.ends_with("/navigation"), "{receipt:?}");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn commit_waits_for_oopif_target_created_after_the_causal_event() {
    if !common::chrome_available() {
        return common::skip("commit_waits_for_oopif_target_created_after_the_causal_event");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("commit-oopif-after-causal-event");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");
    let oopif_url = fixture
        .url("/frame-inner")
        .replacen("127.0.0.1", "localhost", 1);
    page.evaluate(
        &format!(
            "(() => {{ const button = document.createElement('button'); button.textContent = 'Route and attach frame'; button.onclick = () => {{ history.pushState({{}}, '', '#with-frame'); const frame = document.createElement('iframe'); frame.src = {oopif_url:?}; document.body.appendChild(frame); }}; document.body.appendChild(button); return true; }})()"
        ),
        false,
    )
    .await
    .expect("install route-and-frame control");
    let snapshot = page.snapshot().await.expect("route-and-frame snapshot");
    let button = ref_named(&snapshot, "Route and attach frame");
    let root_hold =
        brow::page::test_support::hold_next_root_navigation_in_router(page.session_id.clone());
    let initializer_hold =
        brow::page::test_support::hold_next_target_initializer(page.session_id.clone());
    let mut action = Box::pin(page.click_with_wait(
        &button,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Commit,
        Duration::from_secs(3),
    ));

    tokio::select! {
        result = &mut action => panic!("action escaped before root router barrier: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(1), root_hold.wait_until_entered()) => {
            entered.expect("router received causal same-document event");
        }
    }
    root_hold.release();
    tokio::select! {
        result = &mut action => panic!("action escaped before OOPIF initializer: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(1), initializer_hold.wait_until_entered()) => {
            entered.expect("router reached queued OOPIF initializer");
        }
    }
    if let Ok(result) = tokio::time::timeout(Duration::from_millis(300), &mut action).await {
        panic!(
            "target_settled escaped while the causally queued OOPIF was uninitialized: {result:?}"
        );
    }
    initializer_hold.release();
    let (_, receipt) = action
        .await
        .expect("commit settles after OOPIF initialization");
    assert_eq!(
        receipt.navigation,
        NavigationKind::SameDocument,
        "{receipt:?}"
    );
    assert!(receipt.target_settled, "{receipt:?}");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn no_navigation_receipt_uses_the_post_dispatch_url_budget() {
    if !common::chrome_available() {
        return common::skip("no_navigation_receipt_uses_the_post_dispatch_url_budget");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("no-navigation-post-dispatch-url");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open navigation fixture");

    let snapshot = page.snapshot().await.expect("navigation snapshot");
    let hover = ref_named(&snapshot, "Hover target");
    let hold = brow::page::test_support::hold_next_click_before_dispatch(page.session_id.clone());
    let mut action = Box::pin(page.click_with_wait(
        &hover,
        MouseButton::Left,
        1,
        0,
        false,
        WaitPolicy::Auto,
        Duration::from_secs(1),
    ));

    tokio::select! {
        result = &mut action => panic!("click completed before its dispatch barrier: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(2), hold.wait_until_entered()) => {
            entered.expect("click reached the pre-dispatch barrier");
        }
    }
    // Expire the baseline-relative timeout while dispatch is still impossible.
    // The receipt must nevertheless use the post-dispatch action deadline.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    hold.release();

    let (_, receipt) = tokio::time::timeout(Duration::from_secs(2), action)
        .await
        .expect("post-dispatch Auto wait remained bounded")
        .expect("no-navigation action produced a truthful receipt");
    assert_eq!(receipt.outcome, WaitOutcome::NoNavigation, "{receipt:?}");
    assert!(receipt.final_url_observed, "{receipt:?}");
    assert!(receipt.final_url.ends_with("/navigation"), "{receipt:?}");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn stable_quiet_restarts_after_short_request_between_polls() {
    if !common::chrome_available() {
        return common::skip("stable_quiet_restarts_after_short_request_between_polls");
    }
    // This is a timing-sensitive 30-cycle network oracle. Reserve the complete
    // cross-binary Chrome pool so unrelated process/fixture fan-out cannot turn
    // a localhost fetch into a resource-exhaustion failure.
    let _slots = common::exclusive_browser_slots();
    let fixture = common::serve();
    let scratch = common::Scratch::new("stable-between-polls");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let page = Arc::new(
        Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
            .await
            .expect("open navigation fixture"),
    );

    // These offsets straddle the old 300 ms quiet boundary. The debug-only
    // barrier pauses immediately after an activity sample; a real localhost
    // Fetch is then started, consumed, and observed as finished before the next
    // sample can happen. Only the monotonic activity revision can reveal it.
    for offset_ms in [275_u64, 285, 295] {
        for run in 0..10 {
            let absolute = fixture.url(&format!("/api/ok?between-polls={offset_ms}-{run}"));
            page.evaluate("performance.clearResourceTimings(); true", false)
                .await
                .expect("clear resource timing");
            let hold = brow::page::test_support::hold_stable_poll_after(
                page.session_id.clone(),
                Duration::from_millis(offset_ms),
            );
            let waiting_page = Arc::clone(&page);
            let waiter = tokio::spawn(async move {
                waiting_page
                    .wait_for_conditions(
                        &WaitConditions {
                            stable: true,
                            ..WaitConditions::default()
                        },
                        Duration::from_secs(2),
                        Duration::from_millis(300),
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(1), hold.wait_until_entered())
                .await
                .expect("stability loop reached the between-polls barrier");

            let activity_before = page.events.activity();
            let fetch_result = page
                .evaluate(
                    &format!("fetch({absolute:?}).then(r => r.arrayBuffer()).then(() => true)"),
                    false,
                )
                .await;
            if let Err(error) = fetch_result {
                panic!(
                    "complete marked fetch while polling is held: {error:?}\nnetwork={:#?}",
                    page.events.network(false, 20)
                );
            }
            let event_deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                let activity = page.events.activity();
                if activity.active_finite == 0
                    && activity.revision >= activity_before.revision.saturating_add(2)
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < event_deadline,
                    "marked request did not reach the recorder: before={activity_before:?}, now={activity:?}"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            hold.release();

            let receipt = waiter
                .await
                .expect("stable waiter task")
                .expect("stable after marked fetch");
            assert_eq!(receipt.outcome, WaitOutcome::Stable);
            assert_eq!(receipt.active_finite_requests, 0);
            assert!(receipt.event_complete, "{receipt:?}");

            let timing = page
                .evaluate(
                    &format!(
                        "(() => {{\
                            const entries = performance.getEntriesByName({absolute:?});\
                            const entry = entries[0];\
                            return {{\
                                count: entries.length,\
                                duration: entry ? entry.duration : -1,\
                                quietAfterResponse: entry ? performance.now() - entry.responseEnd : -1\
                            }};\
                        }})()"
                    ),
                    false,
                )
                .await
                .expect("read marked resource timing");
            assert_eq!(timing["count"].as_u64(), Some(1), "{timing}");
            let post_response_quiet = timing["quietAfterResponse"]
                .as_f64()
                .expect("post-response quiet duration");
            assert!(
                post_response_quiet >= 290.0,
                "stable returned only {post_response_quiet:.1}ms after a short request: {timing}"
            );
        }
    }

    drop(page);
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_navigation_auto_actions_stay_within_the_latency_budget() {
    if !common::chrome_available() {
        return common::skip("no_navigation_auto_actions_stay_within_the_latency_budget");
    }
    let _slots = common::exclusive_browser_slots();
    let fixture = common::serve();
    let scratch = common::Scratch::new("navigation-latency");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open fixture");
    let snapshot = page.snapshot().await.expect("snapshot");
    let hover = ref_named(&snapshot, "Hover target");
    let mut samples = Vec::new();
    for _ in 0..30 {
        let started = std::time::Instant::now();
        let (_, receipt) = page
            .click_with_wait(
                &hover,
                MouseButton::Left,
                1,
                0,
                false,
                WaitPolicy::Auto,
                Duration::from_secs(2),
            )
            .await
            .expect("no-navigation action");
        assert_eq!(receipt.outcome, WaitOutcome::NoNavigation);
        samples.push(started.elapsed().as_millis() as u64);
    }
    samples.sort_unstable();
    let p95 = samples[28];
    assert!(p95 <= 350, "no-navigation p95 was {p95}ms: {samples:?}");

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn history_traversal_revalidates_adjacency_before_dispatch() {
    if !common::chrome_available() {
        return common::skip("history_traversal_revalidates_adjacency_before_dispatch");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("history-revalidation");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");
    let page = Page::create(Arc::clone(&launched.client), &fixture.url("/navigation"))
        .await
        .expect("open history fixture");
    page.evaluate("history.pushState({}, '', '#selected'); true", false)
        .await
        .expect("create the initially adjacent history entry");

    let hold =
        brow::page::test_support::hold_next_history_before_revalidation(page.session_id.clone());
    let mut traversal =
        Box::pin(page.traverse_history(-1, Some(WaitPolicy::Commit), Duration::from_secs(2)));
    tokio::select! {
        result = &mut traversal => panic!("history traversal dispatched before revalidation barrier: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(1), hold.wait_until_entered()) => {
            entered.expect("history traversal reached revalidation barrier");
        }
    }

    launched
        .client
        .call_on(
            &page.session_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "history.pushState({}, '', '#drift'); true",
                "returnByValue": true,
            }),
        )
        .await
        .expect("change history during the deterministic pre-dispatch gap");
    hold.release();

    let error = traversal
        .await
        .expect_err("stale adjacent history target must be prevented");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("history drift failure was not structured: {error:?}");
    };
    assert_eq!(receipt.dispatch_state, brow::page::DispatchState::Prevented);
    assert_eq!(receipt.outcome, WaitOutcome::NotWaited);
    assert!(receipt.history_entry_id.is_some(), "{receipt:?}");
    assert!(
        page.location()
            .await
            .expect("location after prevented traversal")
            .0
            .ends_with("/navigation#drift"),
        "prevented traversal must leave the new current entry in place"
    );

    page.close().await.expect("close history fixture");
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_history_is_a_noop_and_beforeunload_is_reported_without_approval() {
    if !common::chrome_available() {
        return common::skip(
            "empty_history_is_a_noop_and_beforeunload_is_reported_without_approval",
        );
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("history-errors");
    let mut options = LaunchOptions::new(scratch.0.join("profile"));
    options.headless = Headless::New;
    let mut launched = launch(&options).await.expect("launch Chromium");

    let empty = Page::create(Arc::clone(&launched.client), "about:blank")
        .await
        .expect("create single-entry target");
    let before = empty.location().await.expect("empty location");
    let generation = empty.generation();
    let error = empty
        .traverse_history(-1, None, Duration::from_secs(1))
        .await
        .expect_err("back from the first entry must be rejected");
    assert!(matches!(
        error,
        PageError::NoHistoryEntry {
            current_index: 0,
            entry_count: 1,
            ..
        }
    ));
    assert_eq!(empty.location().await.unwrap(), before);
    assert_eq!(empty.generation(), generation);
    empty.close().await.expect("close empty target");

    let mut protected = Page::create(Arc::clone(&launched.client), &fixture.url("/second"))
        .await
        .expect("create history origin");
    protected
        .navigate(&fixture.url("/navigation"))
        .await
        .expect("create adjacent history entry");
    let snapshot = protected.snapshot().await.expect("protected page snapshot");
    let protect = ref_named(&snapshot, "Protect navigation");
    protected
        .click_with_wait(
            &protect,
            MouseButton::Left,
            1,
            0,
            false,
            WaitPolicy::Auto,
            Duration::from_secs(2),
        )
        .await
        .expect("install beforeunload with trusted gesture");
    let error = protected
        .traverse_history(-1, None, Duration::from_secs(1))
        .await
        .expect_err("beforeunload must block history traversal");
    let PageError::WaitFailure { receipt, .. } = error else {
        panic!("unexpected beforeunload error: {error:?}");
    };
    assert_eq!(receipt.dispatched, Some(true));
    assert_eq!(receipt.outcome, WaitOutcome::DialogBlocked);
    assert_eq!(receipt.dialog_type.as_deref(), Some("beforeunload"));
    assert!(receipt.dialog_message.is_some());
    assert_eq!(receipt.before_url, receipt.final_url);

    let _ = launched.child.kill();
    let _ = launched.child.wait();
}
