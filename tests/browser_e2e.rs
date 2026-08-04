//! End-to-end tests against a real Chromium over a real CDP pipe.
//!
//! These are the tests that matter: everything interesting about this project is
//! a claim about what the browser actually does, and only the browser can settle
//! it. They are grouped into a few large tests rather than many small ones so the
//! suite launches three Chromiums instead of a dozen.

mod common;

use std::sync::Arc;

use brow::browser::{launch, Headless, LaunchOptions, Launched};
use brow::page::{capture::Clip, ImageFormat, MouseButton, Page, PageError, ScreenshotTarget};

async fn open_fixture(tag: &str, url: &str) -> (Launched, Page, common::Scratch) {
    let scratch = common::Scratch::new(tag);
    let mut opts = LaunchOptions::new(scratch.0.join("profile"));
    opts.headless = Headless::New;
    opts.window_size = (1280, 800);

    let launched = launch(&opts).await.expect("launch chromium");
    let page = Page::create(Arc::clone(&launched.client), url)
        .await
        .expect("open the fixture page");
    (launched, page, scratch)
}

fn shutdown(mut launched: Launched) {
    let _ = launched.child.kill();
    let _ = launched.child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn tree_input_and_evaluation() {
    if !common::chrome_available() {
        return common::skip("tree_input_and_evaluation");
    }
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("tree", &fixture.url("/")).await;

    // ---- the unified tree ------------------------------------------------
    let snap = page.snapshot().await.expect("snapshot");
    assert!(!snap.nodes.is_empty(), "snapshot returned nothing");
    assert_eq!(snap.title, "brow fixture");

    let go = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Create account"))
        .expect("the submit button must appear in the interactive set")
        .clone();
    assert_eq!(go.tag, "button");
    assert_eq!(go.role.as_deref(), Some("button"));
    assert!(go.visible);

    let bounds = go.bounds.expect("an interactive node must carry its box");
    assert!(
        (bounds.x - 100.0).abs() < 2.0 && (bounds.width - 220.0).abs() < 2.0,
        "layout came back as {bounds:?}, expected x=100 w=220"
    );

    // A *closed* shadow root is opaque to page JS but not to CDP.
    let shadow = snap
        .nodes
        .iter()
        .find(|n| n.text.as_deref() == Some("Shadow action"))
        .expect("closed shadow DOM content must be reachable");
    assert!(shadow.in_shadow, "node should be marked as living in a shadow root");
    assert_eq!(shadow.tag, "button");

    // Text-only nodes must not be handed out as interactive noise.
    assert!(
        snap.interactive().count() < snap.nodes.len(),
        "the interactive filter should be doing something"
    );

    // ---- input is real -----------------------------------------------------
    page.click(&go.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect("click the button");
    let status = page
        .evaluate("document.querySelector('#status').textContent", true)
        .await
        .expect("read status");
    assert_eq!(
        status.as_str(),
        Some("clicked:true"),
        "a CDP-dispatched click must arrive with isTrusted=true"
    );

    // ---- filling a field ---------------------------------------------------
    let email = snap
        .interactive()
        .find(|n| n.tag == "input")
        .expect("the email input")
        .clone();
    page.fill(&email.node_ref, "someone@example.com")
        .await
        .expect("fill the input");
    let value = page
        .evaluate("document.querySelector('#email').value", true)
        .await
        .expect("read input value");
    assert_eq!(value.as_str(), Some("someone@example.com"));
    let status = page
        .evaluate("document.querySelector('#status').textContent", true)
        .await
        .expect("read status");
    assert_eq!(
        status.as_str(),
        Some("typed:someone@example.com"),
        "filling must fire input events, not just set .value"
    );

    // Filling again must replace, not append — the select-all path.
    page.fill(&email.node_ref, "second@example.com")
        .await
        .expect("refill");
    let value = page
        .evaluate("document.querySelector('#email').value", true)
        .await
        .unwrap();
    assert_eq!(value.as_str(), Some("second@example.com"));

    // ---- occlusion ---------------------------------------------------------
    let covered = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Covered button"))
        .expect("the covered button")
        .clone();
    let err = page
        .click(&covered.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect_err("clicking through an overlay must be refused");
    assert!(
        matches!(err, PageError::Action(brow::page::input::ActionError::Occluded { .. })),
        "expected an occlusion error, got {err:?}"
    );
    let status = page
        .evaluate("document.querySelector('#status').textContent", true)
        .await
        .unwrap();
    assert_ne!(
        status.as_str(),
        Some("covered-clicked"),
        "the refused click must not have been delivered anyway"
    );

    // --force is the documented escape hatch and must actually work.
    page.click(&covered.node_ref, MouseButton::Left, 1, 0, true)
        .await
        .expect("forced click");

    // ---- read-only evaluation is enforced by V8 ----------------------------
    let before = page.evaluate("document.title", true).await.unwrap();
    let err = page
        .evaluate("document.title = 'hijacked'", true)
        .await
        .expect_err("a mutation must be refused in read-only mode");
    let msg = err.to_string();
    assert!(msg.contains("side-effect"), "unexpected error: {msg}");
    assert!(
        msg.contains("querySelector") && msg.contains("--mutate"),
        "the error must explain the workaround, got: {msg}"
    );
    let after = page.evaluate("document.title", true).await.unwrap();
    assert_eq!(before, after, "the refused mutation must not have applied");

    // ...and --mutate genuinely lifts the guard.
    page.evaluate("document.title = 'hijacked'", false)
        .await
        .expect("mutate mode should allow it");
    let after = page.evaluate("document.title", true).await.unwrap();
    assert_eq!(after.as_str(), Some("hijacked"));

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_are_scoped_to_a_document_generation() {
    if !common::chrome_available() {
        return common::skip("refs_are_scoped_to_a_document_generation");
    }
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("refs", &fixture.url("/")).await;

    let snap = page.snapshot().await.expect("first snapshot");
    let go = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Create account"))
        .expect("button")
        .node_ref
        .clone();
    let generation_before = page.generation();

    // A ref from before any snapshot is a different failure than a stale one.
    assert!(matches!(
        page.click("@node-99999", MouseButton::Left, 1, 0, false).await,
        Err(PageError::Ref(brow::page::RefError::Unknown(_)))
    ));

    page.navigate(&fixture.url("/second"))
        .await
        .expect("navigate away");
    assert!(
        page.generation() > generation_before,
        "navigating must bump the generation"
    );

    let err = page
        .click(&go, MouseButton::Left, 1, 0, false)
        .await
        .expect_err("a ref from the previous document must not resolve");
    let msg = err.to_string();
    assert!(msg.contains("stale"), "got: {msg}");
    assert!(
        msg.contains("brow snapshot"),
        "the error must tell the agent how to recover, got: {msg}"
    );

    // Fresh snapshot, fresh refs, and the new page is genuinely different.
    let snap = page.snapshot().await.expect("second snapshot");
    assert_eq!(snap.title, "second page");
    assert!(snap
        .interactive()
        .any(|n| n.name.as_deref() == Some("Only button")));
    assert!(!snap
        .interactive()
        .any(|n| n.name.as_deref() == Some("Create account")));

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshots_cover_viewport_document_and_node() {
    if !common::chrome_available() {
        return common::skip("screenshots_cover_viewport_document_and_node");
    }
    let fixture = common::serve();
    let (launched, mut page, scratch) = open_fixture("shots", &fixture.url("/")).await;
    let snap = page.snapshot().await.expect("snapshot");

    // Viewport: exactly the window we asked for.
    let shot = page
        .screenshot(ScreenshotTarget::Viewport, ImageFormat::Png, None)
        .await
        .expect("viewport screenshot");
    let (vw, vh) = common::png_size(&shot.bytes).expect("valid PNG");
    assert!((1200..=1360).contains(&vw), "viewport width was {vw}");
    assert!((700..=860).contains(&vh), "viewport height was {vh}");

    // Full page: the fixture has a 3000px spacer, so it must be much taller.
    let full = page
        .screenshot(ScreenshotTarget::FullPage, ImageFormat::Png, None)
        .await
        .expect("full-page screenshot");
    let (fw, fh) = common::png_size(&full.bytes).expect("valid PNG");
    assert_eq!(fw, vw, "full page should not change the width");
    assert!(
        fh > vh + 2000,
        "full page was {fh}px tall, viewport {vh}px — captureBeyondViewport did not take"
    );
    assert!(full.truncated.is_none());

    // Node: the button is 220x48 CSS px, and the clip must land on it exactly.
    let go = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Create account"))
        .expect("button")
        .node_ref
        .clone();
    let node_shot = page
        .screenshot(ScreenshotTarget::Node(go), ImageFormat::Png, None)
        .await
        .expect("node screenshot");
    let (nw, nh) = common::png_size(&node_shot.bytes).expect("valid PNG");
    assert!(
        (nw as i64 - 220).abs() <= 2 && (nh as i64 - 48).abs() <= 2,
        "node screenshot was {nw}x{nh}, expected about 220x48"
    );
    let clip = node_shot.clip.expect("a node capture must report its clip");
    assert!(
        (clip.x - 100.0).abs() < 2.0 && (clip.y - 200.0).abs() < 2.0,
        "clip landed at {clip:?}, expected document coords near 100,200"
    );

    // A node below the fold: the clip must be in *document* space, so scrolling
    // to it cannot shift the answer.
    page.scroll(0.0, 600.0).await.expect("scroll down");
    let snap2 = page.snapshot().await.expect("re-snapshot after scroll");
    let spa = snap2
        .interactive()
        .find(|n| n.name.as_deref() == Some("Go to dashboard"))
        .expect("the link")
        .node_ref
        .clone();
    let link_shot = page
        .screenshot(ScreenshotTarget::Node(spa), ImageFormat::Png, None)
        .await
        .expect("node screenshot after scrolling");
    let clip = link_shot.clip.unwrap();
    assert!(
        (clip.y - 500.0).abs() < 4.0,
        "after scrolling, the link's document-space y was {}, expected ~500 — \
         viewport and document coordinates have been mixed up",
        clip.y
    );

    // Explicit rectangle.
    let rect = page
        .screenshot(
            ScreenshotTarget::Rect(Clip { x: 0.0, y: 0.0, width: 300.0, height: 150.0 }),
            ImageFormat::Png,
            None,
        )
        .await
        .expect("rect screenshot");
    let (rw, rh) = common::png_size(&rect.bytes).unwrap();
    assert!((rw as i64 - 300).abs() <= 2 && (rh as i64 - 150).abs() <= 2, "{rw}x{rh}");

    // And it round-trips to disk.
    let out = scratch.0.join("shot.png");
    let written = rect.write_to(&out).await.expect("write png");
    assert!(written.exists());
    assert_eq!(std::fs::read(&written).unwrap().len(), rect.bytes.len());

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn spa_route_changes_also_invalidate_refs() {
    if !common::chrome_available() {
        return common::skip("spa_route_changes_also_invalidate_refs");
    }
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("spa", &fixture.url("/")).await;

    let snap = page.snapshot().await.expect("snapshot");
    let link = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Go to dashboard"))
        .expect("the SPA link")
        .node_ref
        .clone();
    let before = page.generation();

    page.click(&link, MouseButton::Left, 1, 0, false)
        .await
        .expect("click the SPA link");

    // pushState fires Page.navigatedWithinDocument, not frameNavigated. A harness
    // that only watches the latter hands out refs into a tree that has been
    // re-rendered underneath it.
    let mut settled = page.generation();
    for _ in 0..50 {
        if settled > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        settled = page.generation();
    }
    assert!(
        settled > before,
        "an in-document route change must bump the generation (was {before}, still {settled})"
    );

    let status = page
        .evaluate("document.querySelector('#status').textContent", true)
        .await
        .unwrap();
    assert_eq!(status.as_str(), Some("route:dashboard"));

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn accessibility_names_cross_same_origin_iframes() {
    if !common::chrome_available() {
        return common::skip("accessibility_names_cross_same_origin_iframes");
    }
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("frames", &fixture.url("/frames")).await;
    let snap = page.snapshot().await.expect("snapshot");

    // The control in the top-level document is the easy case.
    assert!(
        snap.interactive().any(|n| n.name.as_deref() == Some("Outer close")),
        "the top-level icon button lost its accessible name"
    );

    // The regression this test exists for: Accessibility.getFullAXTree does not
    // cross iframe boundaries, not even same-origin ones. Without a per-frame
    // pass these two come back with no role and no name at all, and an icon-only
    // button is then indistinguishable from any other <button>.
    let inner = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Inner close"))
        .unwrap_or_else(|| {
            panic!(
                "the button inside the iframe has no accessible name; interactive set was {:?}",
                snap.interactive()
                    .map(|n| (&n.tag, &n.role, &n.name))
                    .collect::<Vec<_>>()
            )
        })
        .clone();
    assert_eq!(inner.role.as_deref(), Some("button"));
    assert!(
        snap.interactive()
            .any(|n| n.name.as_deref() == Some("Inner search")),
        "the input inside the iframe lost its accessible name"
    );

    // The second half of the same bug: DOMSnapshot reports layout per document,
    // so a node inside the iframe came back at its frame-local origin while the
    // click point (from getContentQuads) was global. Reported geometry and actual
    // geometry have to agree, or coordinates handed to an agent are a trap.
    let frame = snap
        .nodes
        .iter()
        .find(|n| n.tag == "iframe")
        .and_then(|n| n.bounds)
        .expect("the iframe's own box");
    let inner_bounds = inner.bounds.expect("the inner button's box");
    assert!(
        inner_bounds.x >= frame.x && inner_bounds.y >= frame.y,
        "inner button reported at {inner_bounds:?}, which is outside its own frame at \
         {frame:?} — the bounds are still frame-local"
    );
    assert!(
        inner_bounds.x < frame.x + frame.width && inner_bounds.y < frame.y + frame.height,
        "inner button at {inner_bounds:?} falls outside frame {frame:?}"
    );

    // ...and a node found through the iframe must actually be clickable, which
    // means the coordinates were resolved in the top-level viewport's space.
    page.click(&inner.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect("click a node inside an iframe");
    let clicked = page
        .evaluate(
            "document.querySelector('#f').contentDocument\
             .querySelector('#clicked').textContent",
            true,
        )
        .await
        .expect("read the iframe's state");
    assert_eq!(
        clicked.as_str(),
        Some("yes:true"),
        "the click did not land inside the iframe as a trusted event"
    );

    shutdown(launched);
}
