//! Screenshots.
//!
//! The subtlety is coordinate spaces. `DOM.getContentQuads` returns **viewport**
//! coordinates; `Page.captureScreenshot`'s `clip` is in **document** coordinates.
//! Mixing them up produces a screenshot of the wrong part of the page that still
//! looks plausible, which is the worst kind of bug — so the conversion happens in
//! exactly one place here, using the visual viewport's page offset.

use std::ffi::OsString;
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

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
/// Maximum output-pixel area of one CDP screenshot response.
///
/// CDP embeds images as base64 in one JSON frame. Eight million worst-case RGBA
/// pixels are 32 MB before PNG compression and about 43 MB after base64, leaving
/// ample framing headroom below the transport's 64 MiB per-frame limit.
const MAX_CAPTURE_FRAME_PIXELS: u64 = 8_000_000;
/// Hard bound for an in-memory stitched RGBA result (256 MiB before encoding).
const MAX_STITCH_PIXELS: u64 = 64_000_000;
static CAPTURE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// A stitched capture can own up to 256 MiB of RGBA plus one decoded tile and
/// the encoded result. Serialize this path process-wide so concurrent sessions
/// cannot multiply that peak until streaming PNG assembly replaces it.
static TILED_CAPTURE_SLOT: Semaphore = Semaphore::const_new(1);

pub struct Capture {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub clip: Option<Clip>,
    /// Set when the requested region had to be shrunk to fit Chromium's limits.
    pub truncated: Option<String>,
    /// True when a full-page PNG was assembled from multiple safe CDP clips.
    pub tiled: bool,
    /// Number of CDP image clips represented by this capture.
    pub tile_count: usize,
}

impl Capture {
    pub async fn write_to(&self, path: &Path) -> std::io::Result<PathBuf> {
        let parent = publication_parent(path);
        crate::paths::create_dir_all_durable(parent)?;
        let sequence = CAPTURE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(path.file_name().unwrap_or_default());
        temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temp = path.with_file_name(temp_name);
        // Deliberately contains no await point. Job cancellation uses `select!`;
        // splitting write/fsync/rename across cancellable Tokio filesystem
        // futures can publish an orphan PNG after `job stop` has already replied.
        // Once this future is polled, publication completes (or fails) before
        // cancellation can be observed.
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&self.bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, path)?;
            #[cfg(unix)]
            {
                // The manifest may be fsynced immediately after this returns. Sync
                // the directory too, so it can never durably point at evidence
                // whose rename existed only in the kernel cache.
                crate::paths::sync_directory(parent)?;
            }
            Ok::<(), std::io::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
        Ok(path.to_path_buf())
    }
}

fn publication_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
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
    let mut clamped = Clip {
        width: clip.width.min(max_css),
        height: clip.height.min(max_css),
        ..clip
    };
    let max_css_area = MAX_CAPTURE_FRAME_PIXELS as f64 / (factor * factor);
    if clamped.width > 0.0 && clamped.width * clamped.height > max_css_area {
        clamped.height = (max_css_area / clamped.width).max(1.0 / factor);
    }
    if clamped == clip {
        return (clip, None);
    }

    let note = format!(
        "requested {:.0}x{:.0} CSS px at {factor:.0}x, which is {:.0}x{:.0} output pixels; \
         a single safe capture is limited to {MAX_OUTPUT_PIXELS:.0} pixels per axis and \
         {MAX_CAPTURE_FRAME_PIXELS} pixels total, so the capture was cut to \
         {:.0}x{:.0} CSS px",
        clip.width,
        clip.height,
        clip.width * factor,
        clip.height * factor,
        clamped.width,
        clamped.height,
    );
    (clamped, Some(note))
}

/// Document-space offset of the visual viewport, plus the full content size
/// and both viewport rectangles from `Page.getLayoutMetrics`.
struct Metrics {
    page_x: f64,
    page_y: f64,
    content_width: f64,
    content_height: f64,
    visual_width: f64,
    visual_height: f64,
    layout_width: f64,
    layout_height: f64,
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
        .and_then(|v| {
            v.get("result")
                .and_then(|r| r.get("value"))
                .and_then(Value::as_f64)
        })
        // Assuming 1 would under-clamp on HiDPI, which is the failure we are
        // guarding against, so guess high when we cannot tell.
        .filter(|r| *r > 0.0)
        .unwrap_or(2.0)
}

fn json_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
}

/// `window.innerWidth` / `innerHeight` — the CSS viewport the page actually uses.
async fn inner_size(client: &CdpClient, session_id: &str) -> Option<(f64, f64)> {
    let res = client
        .call_on(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": "[window.innerWidth, window.innerHeight]",
                "returnByValue": true,
                "throwOnSideEffect": true,
            }),
        )
        .await
        .ok()?;
    let values = res.get("result")?.get("value")?.as_array()?;
    let width = json_number(values.first()?)?;
    let height = json_number(values.get(1)?)?;
    (width > 0.0 && height > 0.0).then_some((width, height))
}

async fn metrics(client: &CdpClient, session_id: &str) -> Result<Metrics, CdpError> {
    let m = client
        .call_on(session_id, "Page.getLayoutMetrics", json!({}))
        .await?;
    // Prefer the `css*` fields: the legacy ones are in device pixels on some
    // platforms and silently disagree with `clip`, which is always CSS pixels.
    let visual = m
        .get("cssVisualViewport")
        .or_else(|| m.get("visualViewport"));
    let layout = m
        .get("cssLayoutViewport")
        .or_else(|| m.get("layoutViewport"));
    let content = m.get("cssContentSize").or_else(|| m.get("contentSize"));
    let num = |v: Option<&Value>, k: &str| -> f64 {
        v.and_then(|v| v.get(k))
            .and_then(json_number)
            .unwrap_or(0.0)
    };
    Ok(Metrics {
        page_x: num(visual, "pageX"),
        page_y: num(visual, "pageY"),
        content_width: num(content, "width"),
        content_height: num(content, "height"),
        visual_width: num(visual, "clientWidth"),
        visual_height: num(visual, "clientHeight"),
        layout_width: num(layout, "clientWidth"),
        layout_height: num(layout, "clientHeight"),
    })
}

/// Visible page rectangle in document coordinates.
///
/// Prefer `cssVisualViewport`: on Chrome 148 / Linux it matches
/// `window.innerWidth` (1280). `cssLayoutViewport.clientWidth` can be one
/// classic-scrollbar narrower (1265) even when JS `clientWidth`/`scrollWidth`
/// stay at 1280, so using the layout viewport would crop real page pixels and
/// disagree with a `cssContentSize` full-page capture.
fn viewport_clip(metrics: &Metrics) -> Option<Clip> {
    let width = [metrics.visual_width, metrics.layout_width]
        .into_iter()
        .find(|width| *width > 0.0)?;
    let height = [metrics.visual_height, metrics.layout_height]
        .into_iter()
        .find(|height| *height > 0.0)?;
    Some(Clip {
        x: metrics.page_x,
        y: metrics.page_y,
        width,
        height,
    })
}

/// Full-page width on the same CSS grid as the viewport capture.
fn full_page_width(metrics: &Metrics) -> f64 {
    metrics
        .content_width
        .max(metrics.visual_width)
        .max(metrics.layout_width)
}

fn capture_error(method: &str, message: impl Into<String>) -> CdpError {
    CdpError::Protocol {
        method: method.into(),
        code: 0,
        message: message.into(),
        data: None,
    }
}

async fn capture_clip_png(
    client: &CdpClient,
    session_id: &str,
    clip: Clip,
) -> Result<Vec<u8>, CdpError> {
    let res = client
        .call_on(
            session_id,
            "Page.captureScreenshot",
            json!({
                "format": "png",
                "captureBeyondViewport": true,
                "fromSurface": true,
                "clip": {
                    "x": clip.x,
                    "y": clip.y,
                    "width": clip.width,
                    "height": clip.height,
                    "scale": 1.0,
                }
            }),
        )
        .await?;
    decode_base64_image(&res)
}

fn decode_base64_image(res: &Value) -> Result<Vec<u8>, CdpError> {
    let data = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| capture_error("Page.captureScreenshot", "response carried no image data"))?;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| {
            capture_error(
                "Page.captureScreenshot",
                format!("image was not valid base64: {e}"),
            )
        })
}

#[derive(Debug)]
struct RgbaTile {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

fn decode_png(bytes: &[u8], max_pixels: u64) -> Result<RgbaTile, CdpError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| capture_error("png.decode", e.to_string()))?;
    let decoded_pixels = u64::from(reader.info().width)
        .checked_mul(u64::from(reader.info().height))
        .ok_or_else(|| capture_error("png.decode", "decoded PNG dimensions overflow"))?;
    if decoded_pixels > max_pixels {
        return Err(capture_error(
            "png.decode",
            format!(
                "decoded PNG declares {decoded_pixels} pixels; the safe decode limit is \
                 {max_pixels} pixels"
            ),
        ));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| capture_error("png.decode", "decoded PNG size overflow"))?;
    let mut raw = vec![0; size];
    let info = reader
        .next_frame(&mut raw)
        .map_err(|e| capture_error("png.decode", e.to_string()))?;
    raw.truncate(info.buffer_size());

    let pixels = match info.color_type {
        png::ColorType::Rgba => raw,
        png::ColorType::Rgb => raw
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => raw
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => raw.into_iter().flat_map(|v| [v, v, v, 255]).collect(),
        png::ColorType::Indexed => {
            return Err(capture_error(
                "png.decode",
                "palette PNG remained indexed after expansion",
            ))
        }
    };
    Ok(RgbaTile {
        width: info.width as usize,
        height: info.height as usize,
        pixels,
    })
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, CdpError> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|e| capture_error("png.encode", e.to_string()))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| capture_error("png.encode", e.to_string()))?;
    }
    Ok(encoded)
}

#[derive(Debug, Clone, PartialEq)]
struct TilePlan {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    dst_x: usize,
    dst_y: usize,
    copy_width: usize,
    copy_height: usize,
}

/// Device-pixel tile grid with CSS clips that still cover those pixels after
/// Chromium integerizes `Page.captureScreenshot` rectangles.
///
/// Walking the page in CSS and converting with `round` asked Chrome 148 for a
/// last clip of 36.8 CSS px at DPR 1.25 (required 46 device px). Chromium
/// integerizes that height to 36 CSS px and returns 45. Asking for the integer
/// CSS rect that covers the remaining device rows avoids the shortfall.
fn tiled_full_page_tiles(full: Clip, device_ratio: f64) -> Vec<TilePlan> {
    let factor = device_ratio.max(0.01);
    let output_width = (full.width * factor).round().max(1.0) as u64;
    let output_height = (full.height * factor).round().max(1.0) as u64;
    let max_axis_output = (MAX_OUTPUT_PIXELS - 2.0) as u64;
    let tile_output_width = output_width.min(max_axis_output).max(1);
    // Covering integer CSS can add about one device pixel per axis; keep that
    // expansion inside the per-frame decode budget (784×10205 = 8_000_720).
    let axis_slack = factor.ceil().max(1.0) as u64;
    let tile_output_height = (MAX_CAPTURE_FRAME_PIXELS
        / tile_output_width.saturating_add(axis_slack))
    .saturating_sub(axis_slack)
    .min(max_axis_output)
    .max(1);

    let mut tiles = Vec::new();
    let mut dst_y = 0_u64;
    while dst_y < output_height {
        let copy_height = tile_output_height.min(output_height - dst_y);
        let mut dst_x = 0_u64;
        while dst_x < output_width {
            let copy_width = tile_output_width.min(output_width - dst_x);
            let x0 = (dst_x as f64 / factor).floor().max(0.0);
            let y0 = (dst_y as f64 / factor).floor().max(0.0);
            let x1 = ((dst_x + copy_width) as f64 / factor)
                .ceil()
                .min(full.width);
            let y1 = ((dst_y + copy_height) as f64 / factor)
                .ceil()
                .min(full.height);
            tiles.push(TilePlan {
                x: full.x + x0,
                y: full.y + y0,
                width: (x1 - x0).max(1.0 / factor),
                height: (y1 - y0).max(1.0 / factor),
                dst_x: dst_x as usize,
                dst_y: dst_y as usize,
                copy_width: copy_width as usize,
                copy_height: copy_height as usize,
            });
            dst_x += copy_width;
        }
        dst_y += copy_height;
    }
    tiles
}

/// Captures a full page as a grid of clips that each remain below Chromium's
/// silent 16,384-output-pixel corruption threshold, then stitches them in memory.
async fn capture_tiled_full_page(
    client: &CdpClient,
    session_id: &str,
    full: Clip,
    device_ratio: f64,
) -> Result<Capture, CdpError> {
    let output_width = (full.width * device_ratio).round().max(1.0) as u64;
    let output_height = (full.height * device_ratio).round().max(1.0) as u64;
    let pixels = output_width
        .checked_mul(output_height)
        .ok_or_else(|| capture_error("tiled capture", "output dimensions overflow"))?;
    if pixels > MAX_STITCH_PIXELS {
        return Err(capture_error(
            "tiled capture",
            format!(
                "full-page image would require {pixels} pixels ({output_width}x{output_height}); \
                 the safe in-memory stitch limit is {MAX_STITCH_PIXELS} pixels"
            ),
        ));
    }
    let width = u32::try_from(output_width)
        .map_err(|_| capture_error("tiled capture", "output width exceeds PNG limits"))?;
    let height = u32::try_from(output_height)
        .map_err(|_| capture_error("tiled capture", "output height exceeds PNG limits"))?;
    let _memory_slot = TILED_CAPTURE_SLOT
        .acquire()
        .await
        .map_err(|_| capture_error("tiled capture", "capture memory scheduler is closed"))?;
    let rgba_len = usize::try_from(pixels)
        .ok()
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| capture_error("tiled capture", "RGBA allocation size overflow"))?;
    let mut rgba = vec![0_u8; rgba_len];
    let mut tile_count = 0;
    for plan in tiled_full_page_tiles(full, device_ratio) {
        let bytes = capture_clip_png(
            client,
            session_id,
            Clip {
                x: plan.x,
                y: plan.y,
                width: plan.width,
                height: plan.height,
            },
        )
        .await?;
        let tile = decode_png(&bytes, MAX_CAPTURE_FRAME_PIXELS)?;
        if tile.width < plan.copy_width || tile.height < plan.copy_height {
            return Err(capture_error(
                "tiled capture",
                format!(
                    "Chromium returned tile {}x{}, smaller than required {}x{} at ({},{})",
                    tile.width, tile.height, plan.copy_width, plan.copy_height, plan.x, plan.y
                ),
            ));
        }
        for row in 0..plan.copy_height {
            let src = row * tile.width * 4;
            let dst = ((plan.dst_y + row) * width as usize + plan.dst_x) * 4;
            let len = plan.copy_width * 4;
            rgba[dst..dst + len].copy_from_slice(&tile.pixels[src..src + len]);
        }
        tile_count += 1;
    }

    Ok(Capture {
        bytes: encode_png(width, height, &rgba)?,
        format: ImageFormat::Png,
        clip: Some(full),
        truncated: None,
        tiled: true,
        tile_count,
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

    let full_page = matches!(&region, Region::FullPage);
    let clip: Option<Clip> = match region {
        Region::Viewport => {
            let mut m = metrics(client, session_id).await?;
            if let Some((width, height)) = inner_size(client, session_id).await {
                // Trust the live CSS viewport when CDP's layout viewport is one
                // classic-scrollbar narrower than `window.innerWidth`.
                m.visual_width = m.visual_width.max(width);
                m.visual_height = m.visual_height.max(height);
            }
            viewport_clip(&m)
        }
        Region::FullPage => {
            let m = metrics(client, session_id).await?;
            Some(Clip {
                x: 0.0,
                y: 0.0,
                width: full_page_width(&m),
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
            if !c.x.is_finite()
                || !c.y.is_finite()
                || !c.width.is_finite()
                || !c.height.is_finite()
                || c.width <= 0.0
                || c.height <= 0.0
            {
                return Err(capture_error(
                    "Page.captureScreenshot",
                    "capture coordinates must be finite and width/height must be positive",
                ));
            }
            let dpr = device_pixel_ratio(client, session_id).await;
            let output_width = (c.width * CLIP_SCALE * dpr).ceil().max(1.0) as u64;
            let output_height = (c.height * CLIP_SCALE * dpr).ceil().max(1.0) as u64;
            let oversized = output_width > MAX_OUTPUT_PIXELS as u64
                || output_height > MAX_OUTPUT_PIXELS as u64
                || output_width.saturating_mul(output_height) > MAX_CAPTURE_FRAME_PIXELS;
            if full_page && oversized {
                if format != ImageFormat::Png {
                    return Err(capture_error(
                        "tiled capture",
                        "oversized full-page capture requires PNG for lossless stitching; rerun with --format png",
                    ));
                }
                return capture_tiled_full_page(client, session_id, c, dpr).await;
            }
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
    let bytes = decode_base64_image(&res)?;

    Ok(Capture {
        bytes,
        format,
        clip,
        truncated,
        tiled: false,
        tile_count: 1,
    })
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

    #[tokio::test]
    async fn screenshot_publication_has_no_cancellable_await_boundary() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("evidence.png");
        let capture = Capture {
            bytes: b"atomic screenshot bytes".to_vec(),
            format: ImageFormat::Png,
            clip: None,
            truncated: None,
            tiled: false,
            tile_count: 1,
        };

        let written = tokio::time::timeout(std::time::Duration::ZERO, capture.write_to(&path))
            .await
            .expect("the complete fsync/rename transaction must finish in one poll")
            .expect("publish screenshot");
        assert_eq!(written, path);
        assert_eq!(std::fs::read(&written).unwrap(), capture.bytes);
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "no temporary publication name remains"
        );
    }

    #[test]
    fn relative_screenshot_output_syncs_the_current_directory() {
        assert_eq!(
            publication_parent(Path::new("evidence.png")),
            Path::new(".")
        );
        assert_eq!(
            publication_parent(Path::new("artifacts/evidence.png")),
            Path::new("artifacts")
        );
    }

    #[test]
    fn oversized_tile_dimensions_are_rejected_before_pixel_allocation() {
        let width = 4_000;
        let height = 2_001;
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width, height);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&vec![0; (width * height) as usize])
                .unwrap();
        }
        let error = decode_png(&encoded, MAX_CAPTURE_FRAME_PIXELS).unwrap_err();
        assert!(
            error.to_string().contains("8004000 pixels"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn bounding_box_spans_every_quad() {
        // An inline element wrapped across two lines produces two quads.
        let quads = json!({ "quads": [
            [10.0, 10.0, 50.0, 10.0, 50.0, 30.0, 10.0, 30.0],
            [ 0.0, 30.0, 40.0, 30.0, 40.0, 50.0,  0.0, 50.0]
        ]});
        let b = bounding_box(&quads).unwrap();
        assert_eq!(
            b,
            Clip {
                x: 0.0,
                y: 10.0,
                width: 50.0,
                height: 40.0
            }
        );
    }

    #[test]
    fn degenerate_quads_yield_nothing() {
        assert!(bounding_box(&json!({ "quads": [] })).is_none());
        let flat = json!({ "quads": [[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]] });
        assert!(bounding_box(&flat).is_none());
    }

    #[test]
    fn a_clip_within_the_limit_is_untouched() {
        let c = Clip {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 4000.0,
        };
        let (out, note) = clamp_clip(c, 1.0, 1.0);
        assert_eq!(out, c);
        assert!(note.is_none());
    }

    #[test]
    fn a_tall_page_is_cut_and_says_so() {
        let c = Clip {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 20_000.0,
        };
        let (out, note) = clamp_clip(c, 1.0, 1.0);
        assert_eq!(out.height, 10_000.0, "the response byte budget is tighter");
        assert_eq!(out.width, 800.0, "the narrow axis must not be touched");
        assert!(note.unwrap().contains("16384"));
    }

    #[test]
    fn the_limit_is_output_pixels_not_css_pixels() {
        // The bug this guards: 10000 CSS px passes any CSS-pixel check, but on a
        // HiDPI display it is 20000 output pixels and Chromium silently repeats
        // the content past 16384.
        let c = Clip {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 10_000.0,
        };

        let (out, note) = clamp_clip(c, 1.0, 1.0);
        assert_eq!(out.height, 10_000.0, "fine at 1x");
        assert!(note.is_none());

        let (out, note) = clamp_clip(c, 1.0, 2.0);
        assert_eq!(
            out.height, 2_500.0,
            "2x must also fit the frame area budget"
        );
        let note = note.expect("truncation at 2x must be reported");
        assert!(note.contains("20000 output pixels"), "{note}");

        // clip.scale multiplies on top of the device ratio.
        let (out, _) = clamp_clip(c, 2.0, 2.0);
        assert_eq!(out.height, 625.0);
    }

    #[test]
    fn clamping_preserves_the_origin() {
        let c = Clip {
            x: 120.0,
            y: 640.0,
            width: 30_000.0,
            height: 30_000.0,
        };
        let (out, _) = clamp_clip(c, 1.0, 1.0);
        assert_eq!((out.x, out.y), (120.0, 640.0));
        assert_eq!(
            (out.width, out.height),
            (
                MAX_OUTPUT_PIXELS,
                MAX_CAPTURE_FRAME_PIXELS as f64 / MAX_OUTPUT_PIXELS
            )
        );
    }

    #[test]
    fn format_parsing_accepts_the_usual_spellings() {
        assert_eq!(ImageFormat::parse("PNG"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::parse("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("jpeg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("gif"), None);
    }

    #[test]
    fn viewport_clip_prefers_visual_viewport_over_narrower_layout() {
        let metrics = Metrics {
            page_x: 0.0,
            page_y: 12.0,
            content_width: 1280.0,
            content_height: 3039.0,
            visual_width: 1280.0,
            visual_height: 713.0,
            layout_width: 1265.0,
            layout_height: 713.0,
        };
        assert_eq!(
            viewport_clip(&metrics),
            Some(Clip {
                x: 0.0,
                y: 12.0,
                width: 1280.0,
                height: 713.0
            })
        );
        assert_eq!(full_page_width(&metrics), 1280.0);
        let missing = Metrics {
            visual_width: 0.0,
            visual_height: 0.0,
            layout_width: 0.0,
            layout_height: 0.0,
            ..metrics
        };
        assert_eq!(viewport_clip(&missing), None);
    }

    #[test]
    fn fractional_dpr_last_tile_requests_integer_css_that_covers_device_rows() {
        // Chrome 148 / Linux: a 627.2×8200 CSS page at DPR 1.25 used to ask for
        // the last clip at y=8163.2 height=36.8 (required 46 device px). Chromium
        // integerized that height to 36 CSS px and returned 45.
        let full = Clip {
            x: 0.0,
            y: 0.0,
            width: 627.2,
            height: 8200.0,
        };
        let tiles = tiled_full_page_tiles(full, 1.25);
        assert!(tiles.len() >= 2, "the area bound must split this page");
        let last = tiles.last().expect("last tile");
        assert_eq!(
            last.y.fract(),
            0.0,
            "clip origin must be integer CSS: {last:?}"
        );
        assert_eq!(
            last.height.fract(),
            0.0,
            "clip height must be integer CSS: {last:?}"
        );
        assert!(
            last.height * 1.25 >= last.copy_height as f64,
            "integer CSS clip must cover the required device rows: {last:?}"
        );
        for tile in &tiles {
            let requested = (tile.width * 1.25).ceil() as u64 * (tile.height * 1.25).ceil() as u64;
            assert!(
                requested <= MAX_CAPTURE_FRAME_PIXELS,
                "covering clip {tile:?} would decode {requested} pixels"
            );
        }
    }
}
