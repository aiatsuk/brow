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

fn png_rgba(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().expect("PNG header");
    let mut pixels = vec![0; reader.output_buffer_size().expect("PNG buffer size")];
    let info = reader.next_frame(&mut pixels).expect("PNG pixels");
    pixels.truncate(info.buffer_size());
    assert_eq!(info.color_type, png::ColorType::Rgba);
    (info.width, info.height, pixels)
}

#[tokio::test(flavor = "multi_thread")]
async fn tree_input_and_evaluation() {
    if !common::chrome_available() {
        return common::skip("tree_input_and_evaluation");
    }
    let _slot = common::browser_slot();
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
    assert!(
        shadow.in_shadow,
        "node should be marked as living in a shadow root"
    );
    assert_eq!(shadow.tag, "button");

    // Text-only nodes must not be handed out as interactive noise.
    assert!(
        snap.interactive().count() < snap.nodes.len(),
        "the interactive filter should be doing something"
    );

    // Opacity-zero native controls are a common implementation detail beneath
    // styled checkboxes. They are not perceptually visible, but they remain real
    // pointer targets and therefore must stay in the default interactive tree.
    let transparent = snap
        .interactive()
        .find(|n| n.name.as_deref() == Some("Transparent toggle"))
        .expect("a transparent pointer target must remain actionable")
        .clone();
    assert!(!transparent.visible);
    assert!(transparent.pointer_eligible);
    let compact = snap.render_text(true);
    assert!(
        compact.contains("\"Transparent toggle\"") && compact.contains(" transparent"),
        "compact output must disclose the transparent actionable control: {compact}"
    );

    // The inverse case matters too: a painted button with pointer-events:none
    // must not be advertised as a click target.
    let no_pointer = snap
        .nodes
        .iter()
        .find(|n| n.name.as_deref() == Some("No pointer action"))
        .expect("the DOM node itself should still be represented");
    assert!(no_pointer.visible);
    assert!(no_pointer.interactive);
    assert!(!no_pointer.pointer_eligible);
    assert!(!snap
        .interactive()
        .any(|n| n.name.as_deref() == Some("No pointer action")));

    page.click(&transparent.node_ref, MouseButton::Left, 1, 0, false)
        .await
        .expect("click the transparent native control");
    let status = page
        .evaluate("document.querySelector('#status').textContent", true)
        .await
        .expect("read transparent-control status");
    assert_eq!(status.as_str(), Some("transparent:true:true"));

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
        matches!(
            err,
            PageError::Action(brow::page::input::ActionError::Occluded { .. })
        ),
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
    let _slot = common::browser_slot();
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
        page.click("@node-99999", MouseButton::Left, 1, 0, false)
            .await,
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
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, mut page, scratch) = open_fixture("shots", &fixture.url("/")).await;
    let snap = page.snapshot().await.expect("snapshot");

    // Viewport: the visual viewport in page pixels. Must share a width with
    // full-page `cssContentSize` even when Chrome's layout viewport is one
    // classic-scrollbar narrower than `window.innerWidth`.
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
            ScreenshotTarget::Rect(Clip {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 150.0,
            }),
            ImageFormat::Png,
            None,
        )
        .await
        .expect("rect screenshot");
    let (rw, rh) = common::png_size(&rect.bytes).unwrap();
    assert!(
        (rw as i64 - 300).abs() <= 2 && (rh as i64 - 150).abs() <= 2,
        "{rw}x{rh}"
    );

    // And it round-trips to disk.
    let out = scratch.0.join("shot.png");
    let written = rect.write_to(&out).await.expect("write png");
    assert!(written.exists());
    assert_eq!(std::fs::read(&written).unwrap().len(), rect.bytes.len());

    shutdown(launched);
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn node_clip_retries_when_scroll_changes_between_quad_and_viewport_samples() {
    if !common::chrome_available() {
        return common::skip(
            "node_clip_retries_when_scroll_changes_between_quad_and_viewport_samples",
        );
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("clip-scroll-race", &fixture.url("/")).await;
    let snapshot = page.snapshot().await.expect("snapshot");
    let spa = snapshot
        .interactive()
        .find(|node| node.name.as_deref() == Some("Go to dashboard"))
        .expect("SPA link")
        .node_ref
        .clone();
    let root_session = page.session_id.clone();
    let hold = brow::page::test_support::hold_next_node_clip_after_quads(root_session.clone());

    let capture = tokio::spawn(async move {
        page.screenshot(ScreenshotTarget::Node(spa), ImageFormat::Png, None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), hold.wait_until_entered())
        .await
        .expect("node clip reached the post-quad barrier");

    let scrolled = launched
        .client
        .call_on(
            &root_session,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "(() => { window.scrollTo(0, 600); return window.scrollY; })()",
                "returnByValue": true,
            }),
        )
        .await
        .expect("force scroll between coordinate samples");
    assert!(
        scrolled
            .get("result")
            .and_then(|result| result.get("value"))
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|scroll_y| scroll_y >= 590.0),
        "fixture did not scroll: {scrolled}"
    );
    hold.release();

    let shot = capture
        .await
        .expect("node clip task")
        .expect("node clip retries after viewport epoch changes");
    let clip = shot.clip.expect("node capture clip");
    assert!(
        (clip.y - 500.0).abs() < 4.0,
        "mixed viewport epochs produced document y={} instead of ~500",
        clip.y
    );

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn spa_route_changes_also_invalidate_refs() {
    if !common::chrome_available() {
        return common::skip("spa_route_changes_also_invalidate_refs");
    }
    let _slot = common::browser_slot();
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
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("frames", &fixture.url("/frames")).await;
    let snap = page.snapshot().await.expect("snapshot");

    // The control in the top-level document is the easy case.
    assert!(
        snap.interactive()
            .any(|n| n.name.as_deref() == Some("Outer close")),
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

#[tokio::test(flavor = "multi_thread")]
async fn oversized_captures_are_cut_rather_than_silently_duplicated() {
    if !common::chrome_available() {
        return common::skip("oversized_captures_are_cut_rather_than_silently_duplicated");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let (launched, mut page, _scratch) = open_fixture("huge", &fixture.url("/")).await;

    // Past 16384 output pixels Chromium returns an image of exactly the requested
    // size in which the rows beyond the limit are verbatim copies of rows from the
    // top. Measured: row 16390 was byte-identical to row 6. Nothing downstream can
    // detect that, so the only defence is never to ask.
    let shot = page
        .screenshot(
            ScreenshotTarget::Rect(Clip {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 20_000.0,
            }),
            ImageFormat::Png,
            None,
        )
        .await
        .expect("oversized rect capture");

    let (w, h) = common::png_size(&shot.bytes).expect("valid PNG");
    assert_eq!(w, 800);
    assert!(
        h <= 16_384,
        "capture came back {h}px tall; everything past 16384 is duplicated content"
    );
    let note = shot
        .truncated
        .expect("an oversized capture must say it was cut");
    assert!(note.contains("16384"), "{note}");
    assert!(
        note.contains("20000"),
        "the note must state what was asked for: {note}"
    );

    // A capture inside the limit must not be flagged.
    let ok = page
        .screenshot(
            ScreenshotTarget::Rect(Clip {
                x: 0.0,
                y: 0.0,
                width: 400.0,
                height: 400.0,
            }),
            ImageFormat::Png,
            None,
        )
        .await
        .expect("normal capture");
    assert!(ok.truncated.is_none());

    // A full-page PNG does not have to be cut. It is captured as multiple safe
    // clips and stitched, preserving pixels on both sides of Chromium's 16384px
    // corruption boundary. Distinct edge colours catch both truncation and the
    // much nastier old failure mode where Chrome repeats rows from the top.
    let dimensions = page
        .evaluate(
            "document.documentElement.style.cssText='margin:0;padding:0';\
             document.body.style.cssText='margin:0;padding:0;height:20000px;background:rgb(0,64,128)';\
             document.body.innerHTML='<div style=\"position:fixed;z-index:1;top:0;left:0;width:100%;height:8px;background:rgb(0,255,0)\"></div>' +\
               '<div style=\"height:64px;background:rgb(255,0,0)\"></div>' +\
               '<div style=\"position:absolute;top:19936px;left:0;width:100%;height:64px;background:rgb(255,0,255)\"></div>';\
             [document.scrollingElement.scrollWidth,document.scrollingElement.scrollHeight,window.devicePixelRatio]",
            false,
        )
        .await
        .expect("install tall capture fixture");
    let dims = dimensions.as_array().expect("dimension array");
    let expected_width = (dims[0].as_f64().unwrap() * dims[2].as_f64().unwrap()).round() as u32;
    let expected_height = (dims[1].as_f64().unwrap() * dims[2].as_f64().unwrap()).round() as u32;

    let full = page
        .screenshot(ScreenshotTarget::FullPage, ImageFormat::Png, None)
        .await
        .expect("tiled full-page capture");
    assert!(
        full.tiled,
        "an oversized full page should use tiled capture"
    );
    assert!(
        full.tile_count >= 2,
        "expected multiple clips, got {}",
        full.tile_count
    );
    assert!(full.truncated.is_none());

    let (full_width, full_height, pixels) = png_rgba(&full.bytes);
    assert_eq!((full_width, full_height), (expected_width, expected_height));
    let sample = |x: u32, y: u32| {
        let offset = ((y * full_width + x) * 4) as usize;
        &pixels[offset..offset + 4]
    };
    assert_eq!(sample(2, 2), [0, 255, 0, 255], "fixed header was lost");
    assert_eq!(sample(2, 32), [255, 0, 0, 255], "top marker was lost");
    assert_eq!(
        sample(2, 16_385),
        [0, 64, 128, 255],
        "fixed header was duplicated at a tile boundary"
    );
    assert_eq!(
        sample(2, full_height - 2),
        [255, 0, 255, 255],
        "bottom marker was truncated or replaced by repeated top-page pixels"
    );

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn tiled_full_page_capture_accounts_for_device_scale_factor() {
    if !common::chrome_available() {
        return common::skip("tiled_full_page_capture_accounts_for_device_scale_factor");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("huge-dpr2");
    let mut opts = LaunchOptions::new(scratch.0.join("profile"));
    opts.headless = Headless::New;
    opts.window_size = (640, 400);
    opts.extra_args.push("--force-device-scale-factor=2".into());

    let launched = launch(&opts).await.expect("launch dpr2 chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/"))
        .await
        .expect("open dpr2 fixture");
    let dimensions = page
        .evaluate(
            "document.documentElement.style.cssText='margin:0;padding:0';\
             document.body.style.cssText='margin:0;padding:0;height:8200px;background:rgb(12,34,56)';\
             document.body.innerHTML='<div style=\"position:absolute;top:8176px;left:0;width:100%;height:24px;background:rgb(1,222,3)\"></div>';\
             [document.scrollingElement.scrollWidth,document.scrollingElement.scrollHeight,window.devicePixelRatio]",
            false,
        )
        .await
        .expect("install dpr2 fixture");
    let dims = dimensions.as_array().expect("dimension array");
    let dpr = dims[2].as_f64().expect("devicePixelRatio");
    assert!(
        (dpr - 2.0).abs() < 0.01,
        "Chrome ignored the DPR flag: {dpr}"
    );

    let full = page
        .screenshot(ScreenshotTarget::FullPage, ImageFormat::Png, None)
        .await
        .expect("dpr2 tiled full-page capture");
    assert!(full.tiled);
    assert!(full.tile_count >= 2);
    let (width, height, pixels) = png_rgba(&full.bytes);
    let expected_width = (dims[0].as_f64().unwrap() * dpr).round() as u32;
    let expected_height = (dims[1].as_f64().unwrap() * dpr).round() as u32;
    assert_eq!((width, height), (expected_width, expected_height));
    let bottom = (((height - 2) * width + 2) * 4) as usize;
    assert_eq!(
        &pixels[bottom..bottom + 4],
        [1, 222, 3, 255],
        "DPR-scaled bottom marker was lost"
    );

    shutdown(launched);
}

#[tokio::test(flavor = "multi_thread")]
async fn tiled_full_page_capture_handles_fractional_device_scale_factor() {
    if !common::chrome_available() {
        return common::skip("tiled_full_page_capture_handles_fractional_device_scale_factor");
    }
    let _slot = common::browser_slot();
    let fixture = common::serve();
    let scratch = common::Scratch::new("huge-dpr-fractional");
    let mut opts = LaunchOptions::new(scratch.0.join("profile"));
    opts.headless = Headless::New;
    opts.window_size = (640, 400);
    opts.extra_args
        .push("--force-device-scale-factor=1.25".into());

    let launched = launch(&opts).await.expect("launch fractional-DPR chromium");
    let mut page = Page::create(Arc::clone(&launched.client), &fixture.url("/"))
        .await
        .expect("open fractional-DPR fixture");
    let dimensions = page
        .evaluate(
            "document.documentElement.style.cssText='margin:0;padding:0';\
             document.body.style.cssText='margin:0;padding:0;height:8200px;background:rgb(21,43,65)';\
             document.body.innerHTML='<div style=\"position:absolute;top:8176px;left:0;width:100%;height:24px;background:rgb(231,17,99)\"></div>';\
             [document.scrollingElement.scrollWidth,document.scrollingElement.scrollHeight,window.devicePixelRatio]",
            false,
        )
        .await
        .expect("install fractional-DPR fixture");
    let dims = dimensions.as_array().expect("dimension array");
    let dpr = dims[2].as_f64().expect("devicePixelRatio");
    assert!(
        (dpr - 1.25).abs() < 0.01,
        "Chrome ignored the fractional DPR flag: {dpr}"
    );

    let full = page
        .screenshot(ScreenshotTarget::FullPage, ImageFormat::Png, None)
        .await
        .expect("fractional-DPR tiled full-page capture");
    assert!(full.tiled, "the per-frame area bound should require tiles");
    assert!(full.tile_count >= 2);
    let (width, height, pixels) = png_rgba(&full.bytes);
    let expected_width = (dims[0].as_f64().unwrap() * dpr).round() as u32;
    let expected_height = (dims[1].as_f64().unwrap() * dpr).round() as u32;
    assert_eq!((width, height), (expected_width, expected_height));
    let bottom = (((height - 2) * width + 2) * 4) as usize;
    assert_eq!(
        &pixels[bottom..bottom + 4],
        [231, 17, 99, 255],
        "fractional-DPR bottom marker was lost at a tile boundary"
    );

    shutdown(launched);
}
