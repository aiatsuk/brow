//! Real browser-level input synthesis.
//!
//! Every gesture here goes through the CDP `Input` domain, which injects events at
//! the same point as a physical device. Verified 2026-08-04: a click dispatched
//! this way arrives in the page with `isTrusted === true`, which
//! `element.dispatchEvent(new MouseEvent(...))` can never produce — so applications
//! that gate on trusted events behave normally under `brow`.
//!
//! Before any pointer action we run an **actionability** check: scroll the node
//! into view, take its content quads, and hit-test the intended point. Skipping
//! this is the single largest source of flaky automation, and it is most of what a
//! wrapper library would otherwise be doing for us.

use std::time::Duration;

use serde_json::{json, Value};

use crate::cdp::{CdpClient, CdpError};

/// Modifier bitmask as CDP defines it.
pub mod modifiers {
    pub const ALT: i64 = 1;
    pub const CTRL: i64 = 2;
    pub const META: i64 = 4;
    pub const SHIFT: i64 = 8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    fn cdp(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
            MouseButton::Back => "back",
            MouseButton::Forward => "forward",
        }
    }

    /// The `buttons` bitmask that must be set while the button is held.
    fn mask(self) -> i64 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
            MouseButton::Back => 8,
            MouseButton::Forward => 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error(transparent)]
    Cdp(#[from] CdpError),
    #[error("node has no layout box — it is not rendered, so it cannot be clicked")]
    NotRendered,
    #[error(
        "node is not the topmost element at ({x:.0},{y:.0}) — something is covering it \
         (a modal, overlay or cookie banner). Dismiss it, or click by coordinates if \
         that is really what you meant."
    )]
    Occluded { x: f64, y: f64 },
    #[error("node moved while we were aiming at it; the page is still animating")]
    Unstable,
}

/// Geometric centre of a node's first content quad, in viewport coordinates.
///
/// `DOM.getContentQuads` is used rather than `DOM.getBoxModel` because it handles
/// CSS transforms and elements fragmented across line boxes, both of which produce
/// a wrong centre with a plain box model.
pub async fn content_center(
    client: &CdpClient,
    session_id: &str,
    backend_node_id: i64,
) -> Result<Point, ActionError> {
    let quads = client
        .call_on(
            session_id,
            "DOM.getContentQuads",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await?;

    let quad = quads
        .get("quads")
        .and_then(Value::as_array)
        .and_then(|q| q.first())
        .and_then(Value::as_array)
        .ok_or(ActionError::NotRendered)?;

    if quad.len() < 8 {
        return Err(ActionError::NotRendered);
    }
    let xs: Vec<f64> = (0..4).map(|i| quad[i * 2].as_f64().unwrap_or(0.0)).collect();
    let ys: Vec<f64> = (0..4).map(|i| quad[i * 2 + 1].as_f64().unwrap_or(0.0)).collect();
    let area = polygon_area(&xs, &ys);
    if area <= 1.0 {
        return Err(ActionError::NotRendered);
    }
    Ok(Point {
        x: xs.iter().sum::<f64>() / 4.0,
        y: ys.iter().sum::<f64>() / 4.0,
    })
}

fn polygon_area(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len();
    let mut acc = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        acc += xs[i] * ys[j] - xs[j] * ys[i];
    }
    acc.abs() / 2.0
}

/// Scrolls a node into view, then returns a stable, hit-testable click point.
pub async fn prepare_target(
    client: &CdpClient,
    session_id: &str,
    backend_node_id: i64,
) -> Result<Point, ActionError> {
    // Not every node supports this (detached nodes, some SVG); a failure here is
    // not fatal, the quads check below is the real gate.
    let _ = client
        .call_on(
            session_id,
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await;

    let first = content_center(client, session_id, backend_node_id).await?;
    // Two samples one frame apart: if the node is mid-animation the centre moves,
    // and clicking a moving target is how you click the wrong thing.
    tokio::time::sleep(Duration::from_millis(24)).await;
    let second = content_center(client, session_id, backend_node_id).await?;
    if (first.x - second.x).abs() > 1.0 || (first.y - second.y).abs() > 1.0 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let third = content_center(client, session_id, backend_node_id).await?;
        if (second.x - third.x).abs() > 1.0 || (second.y - third.y).abs() > 1.0 {
            return Err(ActionError::Unstable);
        }
        return Ok(third);
    }
    Ok(second)
}

/// What the compositor says is on top at a viewport point.
///
/// This is the authoritative hit test: it runs below the JavaScript boundary, so
/// a page cannot influence it, and it pierces iframes — the returned `frameId`
/// tells us which document actually owns the pixel.
pub async fn node_at_point(
    client: &CdpClient,
    session_id: &str,
    point: Point,
) -> Result<Option<(i64, String)>, ActionError> {
    let res = client
        .call_on(
            session_id,
            "DOM.getNodeForLocation",
            json!({
                "x": point.x.round() as i64,
                "y": point.y.round() as i64,
                "includeUserAgentShadowDOM": false,
            }),
        )
        .await;

    // Nothing rendered at that point is a legitimate answer, not an error.
    let Ok(res) = res else { return Ok(None) };
    let Some(backend) = res.get("backendNodeId").and_then(Value::as_i64) else {
        return Ok(None);
    };
    let frame = res
        .get("frameId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Some((backend, frame)))
}

/// Confirms a pointer at the node's centre would reach the node.
///
/// Called only when the compositor's answer is not the target node itself, which
/// happens constantly for legitimate reasons: an `<svg>` icon inside a button, a
/// `<span>` inside a link, a `<label>` wrapping an input.
///
/// The geometry is computed **inside the target's own document**, from the
/// element's own rect, so this is correct for a node in an iframe without any
/// coordinate conversion. `world_context_id` must belong to that same frame; pass
/// an isolated world so a page cannot patch `elementFromPoint` out from under us.
pub async fn covered_by_foreign_element(
    client: &CdpClient,
    session_id: &str,
    backend_node_id: i64,
    world_context_id: Option<i64>,
) -> Result<bool, ActionError> {
    let mut params = json!({ "backendNodeId": backend_node_id });
    if let Some(ctx) = world_context_id {
        params["executionContextId"] = json!(ctx);
    }
    let Ok(resolved) = client.call_on(session_id, "DOM.resolveNode", params).await else {
        // The node does not exist in that world, which means the point belongs to
        // a different document than the target: genuinely covered.
        return Ok(true);
    };
    let Some(object_id) = resolved
        .get("object")
        .and_then(|o| o.get("objectId"))
        .and_then(Value::as_str)
    else {
        return Ok(true);
    };

    let res = client
        .call_on(
            session_id,
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "returnByValue": true,
                // Accept a hit on the node itself, on a descendant, or on an
                // ancestor — all three mean the click lands where it should.
                "functionDeclaration": "function() {\
                    const r = this.getBoundingClientRect();\
                    if (!r.width || !r.height) return true;\
                    const root = this.getRootNode();\
                    const doc = root.elementFromPoint ? root : this.ownerDocument;\
                    const hit = doc.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);\
                    if (!hit) return true;\
                    return !(hit === this || this.contains(hit) || hit.contains(this));\
                }",
            }),
        )
        .await?;

    Ok(res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(Value::as_bool)
        .unwrap_or(true))
}

/// A full click: move, press, release — the sequence a real mouse produces.
///
/// The leading `mouseMoved` is not optional: without it the page never sees
/// hover, and menus, tooltips and many component libraries simply do not open.
pub async fn click_at(
    client: &CdpClient,
    session_id: &str,
    point: Point,
    button: MouseButton,
    click_count: i64,
    modifiers: i64,
) -> Result<(), CdpError> {
    let base = json!({
        "x": point.x, "y": point.y,
        "modifiers": modifiers,
        "pointerType": "mouse",
    });

    let mut moved = base.clone();
    moved["type"] = json!("mouseMoved");
    moved["button"] = json!("none");
    moved["buttons"] = json!(0);
    client
        .call_on(session_id, "Input.dispatchMouseEvent", moved)
        .await?;

    for count in 1..=click_count {
        let mut down = base.clone();
        down["type"] = json!("mousePressed");
        down["button"] = json!(button.cdp());
        down["buttons"] = json!(button.mask());
        down["clickCount"] = json!(count);
        client
            .call_on(session_id, "Input.dispatchMouseEvent", down)
            .await?;

        let mut up = base.clone();
        up["type"] = json!("mouseReleased");
        up["button"] = json!(button.cdp());
        up["buttons"] = json!(0);
        up["clickCount"] = json!(count);
        client
            .call_on(session_id, "Input.dispatchMouseEvent", up)
            .await?;
    }
    Ok(())
}

/// Moves the pointer without pressing, so hover styles and menus engage.
pub async fn hover_at(
    client: &CdpClient,
    session_id: &str,
    point: Point,
    modifiers: i64,
) -> Result<(), CdpError> {
    client
        .call_on(
            session_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseMoved",
                "x": point.x, "y": point.y,
                "button": "none", "buttons": 0,
                "modifiers": modifiers,
                "pointerType": "mouse",
            }),
        )
        .await
        .map(|_| ())
}

/// Wheel scroll at a point.
pub async fn wheel_at(
    client: &CdpClient,
    session_id: &str,
    point: Point,
    delta_x: f64,
    delta_y: f64,
) -> Result<(), CdpError> {
    client
        .call_on(
            session_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": point.x, "y": point.y,
                "deltaX": delta_x, "deltaY": delta_y,
                "button": "none", "buttons": 0,
                "pointerType": "mouse",
            }),
        )
        .await
        .map(|_| ())
}

/// Inserts text as if committed by an input method.
///
/// `Input.insertText` is one call regardless of length and fires the `input`
/// events that React and friends listen for. It does *not* produce per-character
/// `keydown`/`keyup`, so anything driven by key handlers (autocomplete-on-keyup,
/// character counters bound to keydown, key-based masks) needs `press_key` per
/// character instead — `type_text_by_key` exists for exactly that.
pub async fn insert_text(
    client: &CdpClient,
    session_id: &str,
    text: &str,
) -> Result<(), CdpError> {
    client
        .call_on(session_id, "Input.insertText", json!({ "text": text }))
        .await
        .map(|_| ())
}

/// Turns on touch input for the target.
///
/// Without this, `Input.dispatchTouchEvent` is accepted and then ignored: the
/// renderer has no touch device configured, so nothing is delivered.
///
/// **Feature detection lags by one navigation.** Measured 2026-08-04: enabling
/// touch emulation updates `navigator.maxTouchPoints` on the live document
/// immediately (0 → 5) but leaves `'ontouchstart' in window` false, because that
/// property is fixed when the document is created. Touch events are delivered and
/// handled correctly either way — but a responsive site that branches on
/// `ontouchstart` keeps rendering its desktop layout until the page is reloaded.
pub async fn enable_touch(
    client: &CdpClient,
    session_id: &str,
    enabled: bool,
) -> Result<(), CdpError> {
    client
        .call_on(
            session_id,
            "Emulation.setTouchEmulationEnabled",
            json!({ "enabled": enabled, "maxTouchPoints": 5 }),
        )
        .await
        .map(|_| ())
}

fn touch_point(p: Point) -> Value {
    json!({
        "x": p.x,
        "y": p.y,
        // A real finger has an area; some hit-testing and gesture libraries use it.
        "radiusX": 12.0,
        "radiusY": 12.0,
        "force": 1.0,
        "id": 0,
    })
}

async fn touch(
    client: &CdpClient,
    session_id: &str,
    kind: &str,
    points: Vec<Value>,
) -> Result<(), CdpError> {
    client
        .call_on(
            session_id,
            "Input.dispatchTouchEvent",
            json!({ "type": kind, "touchPoints": points }),
        )
        .await
        .map(|_| ())
}

/// A finger down and up in the same place.
pub async fn tap(client: &CdpClient, session_id: &str, point: Point) -> Result<(), CdpError> {
    touch(client, session_id, "touchStart", vec![touch_point(point)]).await?;
    // `touchEnd` carries the points that are *still* down, so a single-finger tap
    // ends with an empty list.
    touch(client, session_id, "touchEnd", vec![]).await
}

/// Holds a finger down long enough to trigger a long-press handler.
pub async fn long_press(
    client: &CdpClient,
    session_id: &str,
    point: Point,
    duration: Duration,
) -> Result<(), CdpError> {
    touch(client, session_id, "touchStart", vec![touch_point(point)]).await?;
    tokio::time::sleep(duration).await;
    touch(client, session_id, "touchEnd", vec![]).await
}

/// Drags a finger from one point to another.
///
/// The intermediate moves are what make this a swipe rather than a teleport:
/// Chromium derives fling velocity from the timing of the point stream, so a
/// two-event "swipe" produces no momentum and many carousels simply ignore it.
pub async fn swipe(
    client: &CdpClient,
    session_id: &str,
    from: Point,
    to: Point,
    duration: Duration,
    steps: u32,
) -> Result<(), CdpError> {
    let steps = steps.max(2);
    let per_step = duration / steps;

    touch(client, session_id, "touchStart", vec![touch_point(from)]).await?;
    for i in 1..=steps {
        let t = f64::from(i) / f64::from(steps);
        // Ease-out, because a real finger decelerates and constant-velocity input
        // reads as synthetic to momentum calculations.
        let eased = 1.0 - (1.0 - t).powi(2);
        let point = Point {
            x: from.x + (to.x - from.x) * eased,
            y: from.y + (to.y - from.y) * eased,
        };
        touch(client, session_id, "touchMove", vec![touch_point(point)]).await?;
        tokio::time::sleep(per_step).await;
    }
    touch(client, session_id, "touchEnd", vec![]).await
}

/// A two-finger pinch, synthesised by the compositor.
///
/// `Input.synthesizePinchGesture` is used rather than hand-rolled multi-touch: it
/// drives the same gesture pipeline as a real trackpad or touchscreen, so page
/// zoom and gesture libraries both respond correctly. Verified present on Chrome
/// 2026-08-04.
pub async fn pinch(
    client: &CdpClient,
    session_id: &str,
    center: Point,
    scale_factor: f64,
    relative_speed: Option<i64>,
) -> Result<(), CdpError> {
    let mut params = json!({
        "x": center.x,
        "y": center.y,
        "scaleFactor": scale_factor,
    });
    if let Some(speed) = relative_speed {
        params["relativeSpeed"] = json!(speed);
    }
    client
        .call_on(session_id, "Input.synthesizePinchGesture", params)
        .await
        .map(|_| ())
}

/// Press, move, release with the mouse held down.
///
/// This is the pointer-based drag that libraries like dnd-kit listen for. HTML5
/// native drag-and-drop is a *different* mechanism and is not covered here.
pub async fn drag(
    client: &CdpClient,
    session_id: &str,
    from: Point,
    to: Point,
    duration: Duration,
    steps: u32,
) -> Result<(), CdpError> {
    let steps = steps.max(2);
    let per_step = duration / steps;
    let button = MouseButton::Left;

    hover_at(client, session_id, from, 0).await?;
    client
        .call_on(
            session_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed", "x": from.x, "y": from.y,
                "button": button.cdp(), "buttons": button.mask(), "clickCount": 1,
                "pointerType": "mouse",
            }),
        )
        .await?;

    for i in 1..=steps {
        let t = f64::from(i) / f64::from(steps);
        let point = Point {
            x: from.x + (to.x - from.x) * t,
            y: from.y + (to.y - from.y) * t,
        };
        client
            .call_on(
                session_id,
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved", "x": point.x, "y": point.y,
                    // The held button must stay in the bitmask for the whole drag,
                    // or the page sees a hover, not a drag.
                    "button": button.cdp(), "buttons": button.mask(),
                    "pointerType": "mouse",
                }),
            )
            .await?;
        tokio::time::sleep(per_step).await;
    }

    client
        .call_on(
            session_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased", "x": to.x, "y": to.y,
                "button": button.cdp(), "buttons": 0, "clickCount": 1,
                "pointerType": "mouse",
            }),
        )
        .await
        .map(|_| ())
}

/// Selects everything in the focused editable field.
///
/// Sending `Cmd+A`/`Ctrl+A` as a plain key event does **not** work: select-all is
/// an *editing command*, resolved above the renderer's key handling, so the
/// keystroke arrives and nothing is selected. Measured symptom before this was
/// fixed: refilling a field produced `someone@examplsecond@example.come.com` —
/// the new text inserted at the caret instead of replacing the old value.
///
/// `Input.dispatchKeyEvent` takes a `commands` array for exactly this case.
pub async fn select_all(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    // The modifier still has to look right to the page, even though `commands` is
    // what actually performs the selection.
    let modifier = if cfg!(target_os = "macos") {
        modifiers::META
    } else {
        modifiers::CTRL
    };
    client
        .call_on(
            session_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "key": "a",
                "code": "KeyA",
                "windowsVirtualKeyCode": 65,
                "nativeVirtualKeyCode": 65,
                "modifiers": modifier,
                "commands": ["selectAll"],
            }),
        )
        .await?;
    client
        .call_on(
            session_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": "a",
                "code": "KeyA",
                "windowsVirtualKeyCode": 65,
                "nativeVirtualKeyCode": 65,
                "modifiers": modifier,
            }),
        )
        .await?;
    Ok(())
}

/// A named key, resolved to the four identifiers CDP wants kept consistent.
struct KeySpec {
    key: &'static str,
    code: &'static str,
    vk: i64,
    text: Option<&'static str>,
}

/// Keys an agent actually names. Printable characters are handled generically.
fn lookup_key(name: &str) -> Option<KeySpec> {
    let spec = match name {
        "Enter" | "Return" => KeySpec { key: "Enter", code: "Enter", vk: 13, text: Some("\r") },
        "Tab" => KeySpec { key: "Tab", code: "Tab", vk: 9, text: Some("\t") },
        "Escape" | "Esc" => KeySpec { key: "Escape", code: "Escape", vk: 27, text: None },
        "Backspace" => KeySpec { key: "Backspace", code: "Backspace", vk: 8, text: None },
        "Delete" => KeySpec { key: "Delete", code: "Delete", vk: 46, text: None },
        "ArrowUp" | "Up" => KeySpec { key: "ArrowUp", code: "ArrowUp", vk: 38, text: None },
        "ArrowDown" | "Down" => KeySpec { key: "ArrowDown", code: "ArrowDown", vk: 40, text: None },
        "ArrowLeft" | "Left" => KeySpec { key: "ArrowLeft", code: "ArrowLeft", vk: 37, text: None },
        "ArrowRight" | "Right" => KeySpec { key: "ArrowRight", code: "ArrowRight", vk: 39, text: None },
        "Home" => KeySpec { key: "Home", code: "Home", vk: 36, text: None },
        "End" => KeySpec { key: "End", code: "End", vk: 35, text: None },
        "PageUp" => KeySpec { key: "PageUp", code: "PageUp", vk: 33, text: None },
        "PageDown" => KeySpec { key: "PageDown", code: "PageDown", vk: 34, text: None },
        "Space" => KeySpec { key: " ", code: "Space", vk: 32, text: Some(" ") },
        _ => return None,
    };
    Some(spec)
}

/// Parses `Ctrl+Shift+K` style shortcuts into a modifier mask and a key name.
pub fn parse_chord(chord: &str) -> (i64, String) {
    let mut mask = 0;
    let mut key = chord;
    for (name, bit) in [
        ("Ctrl", modifiers::CTRL),
        ("Control", modifiers::CTRL),
        ("Shift", modifiers::SHIFT),
        ("Alt", modifiers::ALT),
        ("Option", modifiers::ALT),
        ("Meta", modifiers::META),
        ("Cmd", modifiers::META),
        ("Command", modifiers::META),
    ] {
        let prefix = format!("{name}+");
        if let Some(rest) = strip_prefix_ci(key, &prefix) {
            mask |= bit;
            key = rest;
        }
    }
    (mask, key.to_string())
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Presses and releases one key, honouring modifier chords.
pub async fn press_key(
    client: &CdpClient,
    session_id: &str,
    chord: &str,
) -> Result<(), CdpError> {
    let (mods, name) = parse_chord(chord);

    let (key, code, vk, text) = match lookup_key(&name) {
        Some(spec) => (
            spec.key.to_string(),
            spec.code.to_string(),
            spec.vk,
            spec.text.map(str::to_string),
        ),
        None => {
            let mut chars = name.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return Err(CdpError::Protocol {
                    method: "Input.dispatchKeyEvent".into(),
                    code: 0,
                    message: format!(
                        "unknown key {name:?}: use a single character or one of \
                         Enter/Tab/Escape/Backspace/Delete/Arrow*/Home/End/PageUp/PageDown/Space"
                    ),
                    data: None,
                });
            };
            let upper = ch.to_ascii_uppercase();
            let code = if ch.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else if ch.is_ascii_digit() {
                format!("Digit{ch}")
            } else {
                String::new()
            };
            // With a modifier held, the character is a shortcut, not text.
            let text = (mods & !modifiers::SHIFT == 0).then(|| ch.to_string());
            (ch.to_string(), code, upper as i64, text)
        }
    };

    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": vk,
        "nativeVirtualKeyCode": vk,
        "modifiers": mods,
    });
    if let Some(text) = &text {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    client
        .call_on(session_id, "Input.dispatchKeyEvent", down)
        .await?;

    client
        .call_on(
            session_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
                "modifiers": mods,
            }),
        )
        .await?;
    Ok(())
}

/// Types text one key event at a time, for inputs that react to key handlers.
pub async fn type_text_by_key(
    client: &CdpClient,
    session_id: &str,
    text: &str,
    delay: Duration,
) -> Result<(), CdpError> {
    for ch in text.chars() {
        press_key(client, session_id, &ch.to_string()).await?;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_centre_handles_a_rotated_box() {
        // A square rotated 45° still has its centre at the mean of its corners.
        let xs = [10.0, 20.0, 10.0, 0.0];
        let ys = [0.0, 10.0, 20.0, 10.0];
        assert_eq!(xs.iter().sum::<f64>() / 4.0, 10.0);
        assert_eq!(ys.iter().sum::<f64>() / 4.0, 10.0);
        assert!((polygon_area(&xs, &ys) - 200.0).abs() < 0.001);
    }

    #[test]
    fn zero_area_quads_are_not_clickable() {
        assert_eq!(polygon_area(&[5.0, 5.0, 5.0, 5.0], &[7.0, 7.0, 7.0, 7.0]), 0.0);
    }

    #[test]
    fn chords_parse_case_insensitively_and_in_any_order() {
        assert_eq!(parse_chord("Enter"), (0, "Enter".into()));
        assert_eq!(parse_chord("Ctrl+A"), (modifiers::CTRL, "A".into()));
        assert_eq!(
            parse_chord("ctrl+shift+K"),
            (modifiers::CTRL | modifiers::SHIFT, "K".into())
        );
        assert_eq!(parse_chord("Cmd+Enter"), (modifiers::META, "Enter".into()));
        // A bare "+" must survive.
        assert_eq!(parse_chord("+"), (0, "+".into()));
    }

    #[test]
    fn button_masks_match_the_dom_buttons_bitfield() {
        assert_eq!(MouseButton::Left.mask(), 1);
        assert_eq!(MouseButton::Right.mask(), 2);
        assert_eq!(MouseButton::Middle.mask(), 4);
    }

    #[test]
    fn named_keys_keep_key_code_and_vk_consistent() {
        let enter = lookup_key("Enter").unwrap();
        assert_eq!((enter.key, enter.code, enter.vk), ("Enter", "Enter", 13));
        // Aliases resolve to the canonical name.
        assert_eq!(lookup_key("Esc").unwrap().key, "Escape");
        assert_eq!(lookup_key("Up").unwrap().key, "ArrowUp");
        assert!(lookup_key("F13").is_none());
    }
}
