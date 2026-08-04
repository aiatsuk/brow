//! Screenshots.
//!
//! The subtlety is coordinate spaces. `DOM.getContentQuads` returns **viewport**
//! coordinates; `Page.captureScreenshot`'s `clip` is in **document** coordinates.
//! Mixing them up produces a screenshot of the wrong part of the page that still
//! looks plausible, which is the worst kind of bug — so the conversion happens in
//! exactly one place here, using the visual viewport's page offset.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::cdp::{CdpClient, CdpError};

use super::input;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    fn cdp(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Webp => "webp",
        }
    }
    pub fn extension(self) -> &'static str {
        self.cdp()
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "png" => Some(ImageFormat::Png),
            "jpeg" | "jpg" => Some(ImageFormat::Jpeg),
            "webp" => Some(ImageFormat::Webp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// What to capture.
#[derive(Debug, Clone)]
pub enum Region {
    /// Exactly what is on screen.
    Viewport,
    /// The whole scrollable document.
    FullPage,
    /// One node's content box.
    Node { backend_node_id: i64 },
    /// An explicit document-space rectangle.
    Rect(Clip),
}

/// Chromium's maximum texture dimension, in **output** pixels.
///
/// Past this, a capture is not rejected and is not visibly wrong: it comes back
/// with the requested dimensions, no error, and every row beyond the limit is a
/// verbatim copy of a row from the top of the image. Measured 2026-08-04 on an
/// 800×20000 clip — row 16390 was byte-identical to row 6, row 19000 to row 2616.
///
/// Nothing downstream can detect this. The dimensions are right, the byte count is
/// plausible, and the repeated content is real page content. The only defence is
/// to never ask for more than the limit, so the clamp below is a correctness
/// requirement rather than a nicety.
const MAX_OUTPUT_PIXELS: f64 = 16_384.0;

pub struct Capture {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub clip: Option<Clip>,
    /// Set when the requested region had to be shrunk to fit Chromium's limits.
    pub truncated: Option<String>,
}

impl Capture {
    pub async fn write_to(&self, path: &Path) -> std::io::Result<PathBuf> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, &self.bytes).await?;
        Ok(path.to_path_buf())
    }
}

/// A clip clamped to what Chromium can actually render, plus a note when it had
/// to be cut.
///
/// `scale` is the multiplier passed to `Page.captureScreenshot`; `device_ratio`
/// is `window.devicePixelRatio`. Both multiply into output pixels, which is why
/// a clamp expressed in CSS pixels silently fails on a HiDPI display: a 10000 px
/// tall page at `devicePixelRatio` 2 is 20000 output pixels and comes back
/// corrupted while every CSS-pixel check passes.
fn clamp_clip(clip: Clip, scale: f64, device_ratio: f64) -> (Clip, Option<String>) {
    let factor = (scale * device_ratio).max(0.01);
    let max_css = MAX_OUTPUT_PIXELS / factor;

    if clip.width <= max_css && clip.height <= max_css {
        return (clip, None);
    }

    let clamped = Clip {
        width: clip.width.min(max_css),
        height: clip.height.min(max_css),
        ..clip
    };
    let note = format!(
        "requested {:.0}x{:.0} CSS px at {factor:.0}x, which is {:.0}x{:.0} output pixels; \
         Chromium silently repeats content past {MAX_OUTPUT_PIXELS:.0}, so the capture was \
         cut to {:.0}x{:.0} CSS px",
        clip.width,
        clip.height,
        clip.width * factor,
        clip.height * factor,
        clamped.width,
        clamped.height,
    );
    (clamped, Some(note))
}

/// Document-space offset of the visual viewport, plus the full content size.
struct Metrics {
    page_x: f64,
    page_y: f64,
    content_width: f64,
    content_height: f64,
}

/// `window.devicePixelRatio`, which `Page.getLayoutMetrics` does not report.
async fn device_pixel_ratio(client: &CdpClient, session_id: &str) -> f64 {
    let res = client
        .call_on(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": "window.devicePixelRatio",
                "returnByValue": true,
                "throwOnSideEffect": true,
            }),
        )
        .await;
    res.ok()
        .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_f64))
        // Assuming 1 would under-clamp on HiDPI, which is the failure we are
        // guarding against, so guess high when we cannot tell.
        .filter(|r| *r > 0.0)
        .unwrap_or(2.0)
}

async fn metrics(client: &CdpClient, session_id: &str) -> Result<Metrics, CdpError> {
    let m = client
        .call_on(session_id, "Page.getLayoutMetrics", json!({}))
        .await?;
    // Prefer the `css*` fields: the legacy ones are in device pixels on some
    // platforms and silently disagree with `clip`, which is always CSS pixels.
    let visual = m.get("cssVisualViewport").or_else(|| m.get("visualViewport"));
    let content = m.get("cssContentSize").or_else(|| m.get("contentSize"));
    let num = |v: Option<&Value>, k: &str| -> f64 {
        v.and_then(|v| v.get(k)).and_then(Value::as_f64).unwrap_or(0.0)
    };
    Ok(Metrics {
        page_x: num(visual, "pageX"),
        page_y: num(visual, "pageY"),
        content_width: num(content, "width"),
        content_height: num(content, "height"),
    })
}

/// Captures `region` and returns the encoded image bytes.
pub async fn capture(
    client: &CdpClient,
    session_id: &str,
    region: Region,
    format: ImageFormat,
    quality: Option<i64>,
) -> Result<Capture, CdpError> {
    // `clip.scale` multiplies on top of the device scale factor. Keeping it at 1
    // means one CSS pixel maps to one device pixel's worth of detail.
    const CLIP_SCALE: f64 = 1.0;

    let clip: Option<Clip> = match region {
        Region::Viewport => None,
        Region::FullPage => {
            let m = metrics(client, session_id).await?;
            Some(Clip {
                x: 0.0,
                y: 0.0,
                width: m.content_width,
                height: m.content_height,
            })
        }
        Region::Node { backend_node_id } => {
            // Bring it on screen first: a node parked far outside the viewport can
            // have degenerate quads until it is laid out.
            let _ = client
                .call_on(
                    session_id,
                    "DOM.scrollIntoViewIfNeeded",
                    json!({ "backendNodeId": backend_node_id }),
                )
                .await;

            let quads = client
                .call_on(
                    session_id,
                    "DOM.getContentQuads",
                    json!({ "backendNodeId": backend_node_id }),
                )
                .await?;
            let box_ = bounding_box(&quads).ok_or_else(|| CdpError::Protocol {
                method: "DOM.getContentQuads".into(),
                code: 0,
                message: "node has no rendered box, so there is nothing to capture".into(),
                data: None,
            })?;
            let m = metrics(client, session_id).await?;
            // Viewport space -> document space.
            Some(Clip {
                x: box_.x + m.page_x,
                y: box_.y + m.page_y,
                width: box_.width,
                height: box_.height,
            })
        }
        Region::Rect(c) => Some(c),
    };

    // Every clipped path goes through the clamp, not just the full-page one: an
    // explicit `--rect 0,0,800,20000` is just as capable of producing a silently
    // duplicated image, and used to.
    let mut truncated = None;
    let clip = match clip {
        Some(c) => {
            let dpr = device_pixel_ratio(client, session_id).await;
            let (clamped, note) = clamp_clip(c, CLIP_SCALE, dpr);
            truncated = note;
            Some(clamped)
        }
        None => None,
    };

    // `captureBeyondViewport` must track whether a clip was given. With a clip it
    // is required, or anything below the fold comes back blank. *Without* a clip
    // it silently expands the capture to the whole document — a plain "viewport"
    // screenshot of the test fixture came back 3039 px tall instead of 800.
    let mut params = json!({
        "format": format.cdp(),
        "captureBeyondViewport": clip.is_some(),
        "fromSurface": true,
    });
    if let (Some(q), ImageFormat::Png) = (quality, format) {
        let _ = q; // PNG is lossless; quality is meaningless and Chrome ignores it.
    } else if let Some(q) = quality {
        params["quality"] = json!(q.clamp(0, 100));
    }
    if let Some(c) = clip {
        params["clip"] = json!({
            "x": c.x, "y": c.y,
            "width": c.width, "height": c.height,
            "scale": CLIP_SCALE,
        });
    }

    let res = client
        .call_on(session_id, "Page.captureScreenshot", params)
        .await?;
    let data = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::Protocol {
            method: "Page.captureScreenshot".into(),
            code: 0,
            message: "response carried no image data".into(),
            data: None,
        })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| CdpError::Protocol {
            method: "Page.captureScreenshot".into(),
            code: 0,
            message: format!("image was not valid base64: {e}"),
            data: None,
        })?;

    Ok(Capture { bytes, format, clip, truncated })
}

/// Axis-aligned bounding box over every quad a node occupies.
fn bounding_box(quads: &Value) -> Option<Clip> {
    let list = quads.get("quads").and_then(Value::as_array)?;
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    let mut seen = false;

    for quad in list {
        let Some(q) = quad.as_array() else { continue };
        if q.len() < 8 {
            continue;
        }
        seen = true;
        for i in 0..4 {
            let x = q[i * 2].as_f64().unwrap_or(0.0);
            let y = q[i * 2 + 1].as_f64().unwrap_or(0.0);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }

    if !seen || max_x <= min_x || max_y <= min_y {
        return None;
    }
    Some(Clip {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

/// Convenience: capture a node identified by a click point (for `--at x,y`).
pub async fn node_at(
    client: &CdpClient,
    session_id: &str,
    point: input::Point,
) -> Result<i64, CdpError> {
    let res = client
        .call_on(
            session_id,
            "DOM.getNodeForLocation",
            json!({
                "x": point.x as i64,
                "y": point.y as i64,
                "includeUserAgentShadowDOM": false,
            }),
        )
        .await?;
    res.get("backendNodeId")
        .and_then(Value::as_i64)
        .ok_or_else(|| CdpError::Protocol {
            method: "DOM.getNodeForLocation".into(),
            code: 0,
            message: format!("nothing is rendered at ({}, {})", point.x, point.y),
            data: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounding_box_spans_every_quad() {
        // An inline element wrapped across two lines produces two quads.
        let quads = json!({ "quads": [
            [10.0, 10.0, 50.0, 10.0, 50.0, 30.0, 10.0, 30.0],
            [ 0.0, 30.0, 40.0, 30.0, 40.0, 50.0,  0.0, 50.0]
        ]});
        let b = bounding_box(&quads).unwrap();
        assert_eq!(b, Clip { x: 0.0, y: 10.0, width: 50.0, height: 40.0 });
    }

    #[test]
    fn degenerate_quads_yield_nothing() {
        assert!(bounding_box(&json!({ "quads": [] })).is_none());
        let flat = json!({ "quads": [[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]] });
        assert!(bounding_box(&flat).is_none());
    }

    #[test]
    fn a_clip_within_the_limit_is_untouched() {
        let c = Clip { x: 0.0, y: 0.0, width: 1280.0, height: 4000.0 };
        let (out, note) = clamp_clip(c, 1.0, 1.0);
        assert_eq!(out, c);
        assert!(note.is_none());
    }

    #[test]
    fn a_tall_page_is_cut_and_says_so() {
        let c = Clip { x: 0.0, y: 0.0, width: 800.0, height: 20_000.0 };
        let (out, note) = clamp_clip(c, 1.0, 1.0);
        assert_eq!(out.height, MAX_OUTPUT_PIXELS);
        assert_eq!(out.width, 800.0, "the narrow axis must not be touched");
        assert!(note.unwrap().contains("16384"));
    }

    #[test]
    fn the_limit_is_output_pixels_not_css_pixels() {
        // The bug this guards: 10000 CSS px passes any CSS-pixel check, but on a
        // HiDPI display it is 20000 output pixels and Chromium silently repeats
        // the content past 16384.
        let c = Clip { x: 0.0, y: 0.0, width: 800.0, height: 10_000.0 };

        let (out, note) = clamp_clip(c, 1.0, 1.0);
        assert_eq!(out.height, 10_000.0, "fine at 1x");
        assert!(note.is_none());

        let (out, note) = clamp_clip(c, 1.0, 2.0);
        assert_eq!(out.height, 8_192.0, "at 2x the same clip must be halved");
        let note = note.expect("truncation at 2x must be reported");
        assert!(note.contains("20000 output pixels"), "{note}");

        // clip.scale multiplies on top of the device ratio.
        let (out, _) = clamp_clip(c, 2.0, 2.0);
        assert_eq!(out.height, 4_096.0);
    }

    #[test]
    fn clamping_preserves_the_origin() {
        let c = Clip { x: 120.0, y: 640.0, width: 30_000.0, height: 30_000.0 };
        let (out, _) = clamp_clip(c, 1.0, 1.0);
        assert_eq!((out.x, out.y), (120.0, 640.0));
        assert_eq!((out.width, out.height), (MAX_OUTPUT_PIXELS, MAX_OUTPUT_PIXELS));
    }

    #[test]
    fn format_parsing_accepts_the_usual_spellings() {
        assert_eq!(ImageFormat::parse("PNG"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::parse("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("jpeg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("gif"), None);
    }
}
