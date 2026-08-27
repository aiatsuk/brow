//! The wire protocol between `brow` (the CLI) and `browd` (the daemon).
//!
//! Newline-delimited JSON over a Unix socket. `serde_json` escapes embedded
//! newlines, so a line is always exactly one message.
//!
//! This enum *is* the capability surface. Raw CDP is not representable here, which
//! is what makes "the agent never gets raw CDP" a structural property rather than
//! a promise: there is no request variant that carries a protocol method name.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Bumped whenever a field changes meaning. A mismatch fails loudly at connect
/// time instead of producing a confusing error three calls later.
pub const PROTOCOL_VERSION: u32 = 2;

/// Largest JSON payload accepted on either side of the newline IPC transport.
/// The trailing newline is framing and does not count toward this limit.
pub const MAX_IPC_FRAME_BYTES: usize = 8 * 1024 * 1024;

const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_CLICK_COUNT: i64 = 10;
const MAX_GESTURE_DURATION_MS: u64 = 120_000;
const MAX_GESTURE_STEPS: u32 = 1_000;
const MAX_WAIT_URL_BYTES: usize = 4_096;

#[derive(Debug, Error)]
pub enum IpcFrameReadError {
    #[error("could not read IPC frame: {0}")]
    Io(#[from] io::Error),
    #[error("IPC frame exceeds the maximum of {max} bytes")]
    TooLarge { max: usize },
    #[error("connection closed before the IPC frame's terminating newline")]
    Unterminated,
}

#[derive(Debug, Error)]
pub enum IpcFrameEncodeError {
    #[error("could not serialize IPC frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("serialized IPC frame is {size} bytes; maximum is {max} bytes")]
    TooLarge { size: usize, max: usize },
}

/// A cancellation-safe, size-bounded reader for newline-delimited IPC frames.
///
/// Bytes consumed before an await are retained in `partial`, so cancelling a
/// `read_frame` future cannot silently discard the start of the next frame.
pub struct IpcFrameReader<R> {
    reader: R,
    partial: Vec<u8>,
}

impl<R> IpcFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            partial: Vec::new(),
        }
    }
}

impl<R: AsyncBufRead + Unpin> IpcFrameReader<R> {
    /// Waits until the peer sends another byte or closes its write half without
    /// consuming anything. Used to enforce one in-flight request per connection.
    pub async fn wait_for_activity(&mut self) -> io::Result<bool> {
        if !self.partial.is_empty() {
            return Ok(true);
        }
        Ok(!self.reader.fill_buf().await?.is_empty())
    }

    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, IpcFrameReadError> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.partial.is_empty() {
                    return Ok(None);
                }
                self.partial.clear();
                return Err(IpcFrameReadError::Unterminated);
            }

            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                if self.partial.len().saturating_add(newline) > MAX_IPC_FRAME_BYTES {
                    self.partial.clear();
                    return Err(IpcFrameReadError::TooLarge {
                        max: MAX_IPC_FRAME_BYTES,
                    });
                }
                self.partial.extend_from_slice(&available[..newline]);
                self.reader.consume(newline + 1);
                if self.partial.last() == Some(&b'\r') {
                    self.partial.pop();
                }
                return Ok(Some(std::mem::take(&mut self.partial)));
            }

            if self.partial.len().saturating_add(available.len()) > MAX_IPC_FRAME_BYTES {
                self.partial.clear();
                return Err(IpcFrameReadError::TooLarge {
                    max: MAX_IPC_FRAME_BYTES,
                });
            }
            let consumed = available.len();
            self.partial.extend_from_slice(available);
            self.reader.consume(consumed);
        }
    }
}

/// Serializes one JSON value and appends its newline delimiter without ever
/// putting an oversized frame on the wire.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, IpcFrameEncodeError> {
    let mut writer = CappedFrameWriter::new(MAX_IPC_FRAME_BYTES);
    serde_json::to_writer(&mut writer, value)?;
    if writer.size > MAX_IPC_FRAME_BYTES {
        return Err(IpcFrameEncodeError::TooLarge {
            size: writer.size,
            max: MAX_IPC_FRAME_BYTES,
        });
    }
    // The delimiter is outside the payload limit. Grow by exactly its one-byte
    // allowance so an exact-limit payload does not trigger Vec's amortized 2x
    // growth at the final push.
    writer.bytes.reserve_exact(1);
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

/// Counts the entire serialized frame while retaining at most the wire limit.
/// `serde_json` can therefore measure an oversized value without allocating a
/// second unbounded copy of it before the limit check.
struct CappedFrameWriter {
    bytes: Vec<u8>,
    size: usize,
    max: usize,
}

impl CappedFrameWriter {
    fn new(max: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max.min(64 * 1024)),
            size: 0,
            max,
        }
    }
}

impl Write for CappedFrameWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.size = self.size.saturating_add(buf.len());
        let retain = buf.len().min(self.max.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&buf[..retain]);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn default_wait_policy() -> WaitPolicy {
    WaitPolicy::Auto
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_quiet_ms() -> u64 {
    300
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum WaitPolicy {
    #[default]
    Auto,
    None,
    Commit,
    Load,
    Stable,
    /// Receipt-only policy for typed predicate waits; never accepted as an
    /// action/history/checkpoint request policy.
    #[value(skip)]
    Conditions,
}

/// Closed set of click buttons accepted by the wire protocol. `""` remains a
/// compatibility spelling for the serde-default left button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl ClickButton {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "" | "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "middle" => Some(Self::Middle),
            "back" => Some(Self::Back),
            "forward" => Some(Self::Forward),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WaitConditions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_after: Option<u64>,
    #[serde(default)]
    pub load: bool,
    #[serde(default)]
    pub stable: bool,
}

/// Sent by the daemon as the first line on every connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub brow: String,
    pub protocol: u32,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Sessions,
    Shutdown,
    Open {
        url: String,
        session: String,
        headless: bool,
    },
    Snapshot {
        session: String,
        interactive: bool,
    },
    Click {
        session: String,
        target: Target,
        #[serde(default)]
        button: String,
        #[serde(default = "one")]
        count: i64,
        #[serde(default)]
        modifiers: i64,
        #[serde(default)]
        force: bool,
        #[serde(default = "default_wait_policy")]
        wait: WaitPolicy,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    Hover {
        session: String,
        node_ref: String,
    },
    Fill {
        session: String,
        node_ref: String,
        text: String,
    },
    Press {
        session: String,
        chord: String,
        #[serde(default = "default_wait_policy")]
        wait: WaitPolicy,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    Type {
        session: String,
        text: String,
        #[serde(default)]
        by_key: bool,
    },
    Scroll {
        session: String,
        dx: f64,
        dy: f64,
    },
    Screenshot {
        session: String,
        target: ShotTarget,
        format: String,
        quality: Option<i64>,
        out: Option<String>,
    },
    Eval {
        session: String,
        expression: String,
        /// `false` means read-only, enforced by V8's side-effect guard.
        #[serde(default)]
        mutate: bool,
    },
    Console {
        session: String,
        #[serde(default)]
        errors: bool,
        #[serde(default = "fifty")]
        limit: usize,
    },
    Network {
        session: String,
        #[serde(default)]
        failed: bool,
        #[serde(default = "fifty")]
        limit: usize,
    },
    Tap {
        session: String,
        target: Target,
        #[serde(default = "default_wait_policy")]
        wait: WaitPolicy,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    LongPress {
        session: String,
        target: Target,
        #[serde(default = "default_press_ms")]
        duration_ms: u64,
    },
    Swipe {
        session: String,
        from: Target,
        to: Target,
        #[serde(default = "default_swipe_ms")]
        duration_ms: u64,
        #[serde(default = "default_steps")]
        steps: u32,
    },
    Pinch {
        session: String,
        center: Target,
        scale: f64,
        speed: Option<i64>,
    },
    Drag {
        session: String,
        from: Target,
        to: Target,
        #[serde(default = "default_swipe_ms")]
        duration_ms: u64,
        #[serde(default = "default_steps")]
        steps: u32,
    },
    Close {
        session: String,
    },
    Wait {
        session: String,
        conditions: WaitConditions,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_quiet_ms")]
        quiet_ms: u64,
    },
    Back {
        session: String,
        wait: Option<WaitPolicy>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    Forward {
        session: String,
        wait: Option<WaitPolicy>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    Reload {
        session: String,
        #[serde(default)]
        ignore_cache: bool,
        wait: Option<WaitPolicy>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    PointerPark {
        session: String,
    },
    Checkpoint {
        session: String,
        name: String,
        #[serde(default)]
        full_page: bool,
        wait: Option<WaitPolicy>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_quiet_ms")]
        quiet_ms: u64,
        #[serde(default)]
        park_pointer: bool,
        output_root: Option<String>,
    },

    // ---- background jobs ---------------------------------------------------
    JobStart {
        /// Free text for whoever reads the job later. It does **not** drive
        /// execution: the daemon never calls a model, so the steps are the plan.
        intent: String,
        steps: Vec<String>,
        #[serde(default = "yes")]
        headless: bool,
        #[serde(default)]
        checkpoint_each_step: bool,
    },
    JobList,
    JobStatus {
        id: String,
        /// Log lines already seen, so `--follow` can poll for the rest.
        #[serde(default)]
        log_from: usize,
    },
    /// Answers a `needs_decision` park. For agents.
    JobAnswer {
        id: String,
        answer: String,
    },
    /// Answers a `waiting_for_approval` park. Intended for a human operator, but
    /// the current local protocol does not authenticate human presence.
    JobApprove {
        id: String,
        #[serde(default)]
        reject: bool,
    },
    JobStop {
        id: String,
    },
}

/// A semantic protocol error in an otherwise well-formed request.
///
/// Deserialization only proves that a raw client supplied the right JSON types.
/// These checks are deliberately attached to `Request` so the official CLI and
/// the daemon admission boundary cannot disagree about whether a request is
/// safe to dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestValidationError {
    message: String,
}

impl std::fmt::Display for RequestValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RequestValidationError {}

impl RequestValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Request {
    /// Whether abandoning the connection may cancel an accepted request.
    ///
    /// Keep this match exhaustive: a new capability must make an explicit
    /// ownership decision. Only the observation-only typed wait is currently
    /// cancellation-safe; other operations may own browser/process/job cleanup
    /// or may be between paired input events.
    pub fn cancel_on_disconnect(&self) -> bool {
        match self {
            Request::Wait { .. } => true,
            Request::Ping
            | Request::Status
            | Request::Sessions
            | Request::Shutdown
            | Request::Open { .. }
            | Request::Snapshot { .. }
            | Request::Click { .. }
            | Request::Hover { .. }
            | Request::Fill { .. }
            | Request::Press { .. }
            | Request::Type { .. }
            | Request::Scroll { .. }
            | Request::Screenshot { .. }
            | Request::Eval { .. }
            | Request::Console { .. }
            | Request::Network { .. }
            | Request::Tap { .. }
            | Request::LongPress { .. }
            | Request::Swipe { .. }
            | Request::Pinch { .. }
            | Request::Drag { .. }
            | Request::Close { .. }
            | Request::Back { .. }
            | Request::Forward { .. }
            | Request::Reload { .. }
            | Request::PointerPark { .. }
            | Request::Checkpoint { .. }
            | Request::JobStart { .. }
            | Request::JobList
            | Request::JobStatus { .. }
            | Request::JobAnswer { .. }
            | Request::JobApprove { .. }
            | Request::JobStop { .. } => false,
        }
    }

    /// Validates constraints that are not expressible in the serde wire shape.
    ///
    /// This MUST run both while the CLI constructs a request and at the daemon
    /// admission boundary. The latter is authoritative because protocol-v2 is a
    /// public local socket protocol and callers may bypass the CLI entirely.
    pub fn validate(&self) -> Result<(), RequestValidationError> {
        match self {
            Request::Click {
                button,
                count,
                wait,
                timeout_ms,
                ..
            } => {
                validate_timeout(*timeout_ms)?;
                validate_input_action_wait(*wait)?;
                if ClickButton::parse(button).is_none() {
                    return Err(RequestValidationError::new(format!(
                        "click button must be left, right, middle, back, or forward; got {button:?}"
                    )));
                }
                if !(1..=MAX_CLICK_COUNT).contains(count) {
                    return Err(RequestValidationError::new(format!(
                        "click count must be between 1 and {MAX_CLICK_COUNT}, got {count}"
                    )));
                }
            }
            Request::Press {
                wait, timeout_ms, ..
            }
            | Request::Tap {
                wait, timeout_ms, ..
            } => {
                validate_timeout(*timeout_ms)?;
                validate_input_action_wait(*wait)?;
            }
            Request::Wait {
                conditions,
                timeout_ms,
                quiet_ms,
                ..
            } => {
                validate_timeout(*timeout_ms)?;
                validate_quiet(*quiet_ms, *timeout_ms)?;
                if conditions.url.is_none()
                    && conditions.generation_after.is_none()
                    && !conditions.load
                    && !conditions.stable
                {
                    return Err(RequestValidationError::new(
                        "wait requires at least one of --url, --generation-after, --load, or --stable",
                    ));
                }
                if conditions.url.as_ref().is_some_and(|pattern| {
                    pattern.is_empty()
                        || pattern.len() > MAX_WAIT_URL_BYTES
                        || pattern.chars().any(char::is_control)
                }) {
                    return Err(RequestValidationError::new(format!(
                        "--url must be a non-empty glob without control characters (maximum {MAX_WAIT_URL_BYTES} bytes)"
                    )));
                }
            }
            Request::Back {
                wait, timeout_ms, ..
            }
            | Request::Forward {
                wait, timeout_ms, ..
            }
            | Request::Reload {
                wait, timeout_ms, ..
            } => {
                validate_timeout(*timeout_ms)?;
                validate_expected_navigation_wait(*wait)?;
            }
            Request::Checkpoint {
                wait,
                timeout_ms,
                quiet_ms,
                ..
            } => {
                validate_timeout(*timeout_ms)?;
                validate_quiet(*quiet_ms, *timeout_ms)?;
                validate_expected_navigation_wait(*wait)?;
            }
            Request::LongPress { duration_ms, .. } => {
                if *duration_ms > MAX_GESTURE_DURATION_MS {
                    return Err(RequestValidationError::new(format!(
                        "gesture duration must be at most {MAX_GESTURE_DURATION_MS}ms"
                    )));
                }
            }
            Request::Swipe {
                duration_ms, steps, ..
            } => validate_gesture("swipe", *duration_ms, *steps)?,
            Request::Drag {
                duration_ms, steps, ..
            } => validate_gesture("drag", *duration_ms, *steps)?,
            _ => {}
        }
        Ok(())
    }
}

fn validate_timeout(timeout_ms: u64) -> Result<(), RequestValidationError> {
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(RequestValidationError::new(format!(
            "--timeout-ms must be between 1 and {MAX_TIMEOUT_MS}"
        )));
    }
    Ok(())
}

fn validate_quiet(quiet_ms: u64, timeout_ms: u64) -> Result<(), RequestValidationError> {
    if quiet_ms > timeout_ms || quiet_ms > MAX_TIMEOUT_MS {
        return Err(RequestValidationError::new(format!(
            "--quiet-ms must not exceed --timeout-ms or {MAX_TIMEOUT_MS}"
        )));
    }
    Ok(())
}

fn validate_expected_navigation_wait(
    wait: Option<WaitPolicy>,
) -> Result<(), RequestValidationError> {
    if matches!(wait, Some(WaitPolicy::Auto | WaitPolicy::Conditions)) {
        return Err(RequestValidationError::new(
            "navigation wait must be none, commit, load, or stable",
        ));
    }
    Ok(())
}

fn validate_input_action_wait(wait: WaitPolicy) -> Result<(), RequestValidationError> {
    if wait == WaitPolicy::Conditions {
        return Err(RequestValidationError::new(
            "conditions is a receipt-only wait policy; choose auto, none, commit, load, or stable",
        ));
    }
    Ok(())
}

fn validate_gesture(
    operation: &str,
    duration_ms: u64,
    steps: u32,
) -> Result<(), RequestValidationError> {
    if duration_ms > MAX_GESTURE_DURATION_MS || !(2..=MAX_GESTURE_STEPS).contains(&steps) {
        return Err(RequestValidationError::new(format!(
            "{operation} requires duration <= {MAX_GESTURE_DURATION_MS}ms and 2..={MAX_GESTURE_STEPS} steps"
        )));
    }
    Ok(())
}

fn yes() -> bool {
    true
}

fn fifty() -> usize {
    50
}
fn default_press_ms() -> u64 {
    800
}
fn default_swipe_ms() -> u64 {
    450
}
fn default_steps() -> u32 {
    24
}

fn one() -> i64 {
    1
}

/// A pointer target: either a ref from the last snapshot or raw coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Ref { node_ref: String },
    Point { x: f64, y: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShotTarget {
    Viewport,
    FullPage,
    Node {
        node_ref: String,
    },
    Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        #[serde(default)]
        data: Value,
        /// Human-readable text for the terminal; `data` is for `--json`.
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Error {
        message: String,
        /// What the caller should do about it. Agents act on this.
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        /// Structured partial-success or diagnostic data.
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Response::Ok { data, text: None }
    }
    pub fn ok_text(data: Value, text: impl Into<String>) -> Self {
        Response::Ok {
            data,
            text: Some(text.into()),
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Response::Error {
            message: message.into(),
            hint: None,
            data: None,
        }
    }
    pub fn error_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Response::Error {
            message: message.into(),
            hint: Some(hint.into()),
            data: None,
        }
    }
    pub fn error_data(message: impl Into<String>, hint: impl Into<String>, data: Value) -> Self {
        Response::Error {
            message: message.into(),
            hint: Some(hint.into()),
            data: Some(data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn bounded_frame_reader_accepts_exact_limit_and_rejects_one_more_byte() {
        let mut exact = vec![b'a'; MAX_IPC_FRAME_BYTES];
        exact.push(b'\n');
        let mut reader = IpcFrameReader::new(BufReader::new(std::io::Cursor::new(exact)));
        let frame = reader
            .read_frame()
            .await
            .expect("read exact-limit frame")
            .expect("exact-limit frame");
        assert_eq!(frame.len(), MAX_IPC_FRAME_BYTES);
        assert!(reader.read_frame().await.unwrap().is_none());

        let mut oversized = vec![b'a'; MAX_IPC_FRAME_BYTES + 1];
        oversized.push(b'\n');
        let mut reader = IpcFrameReader::new(BufReader::new(std::io::Cursor::new(oversized)));
        assert!(matches!(
            reader.read_frame().await,
            Err(IpcFrameReadError::TooLarge {
                max: MAX_IPC_FRAME_BYTES
            })
        ));
    }

    #[tokio::test]
    async fn bounded_frame_reader_rejects_unterminated_eof() {
        let mut reader = IpcFrameReader::new(BufReader::new(std::io::Cursor::new(b"{}")));
        assert!(matches!(
            reader.read_frame().await,
            Err(IpcFrameReadError::Unterminated)
        ));
    }

    #[test]
    fn encoder_enforces_the_same_payload_boundary() {
        let exact = "a".repeat(MAX_IPC_FRAME_BYTES - 2);
        let encoded = encode_frame(&exact).expect("JSON string exactly at limit");
        assert_eq!(encoded.len(), MAX_IPC_FRAME_BYTES + 1);

        let oversized = format!("{exact}a");
        assert!(matches!(
            encode_frame(&oversized),
            Err(IpcFrameEncodeError::TooLarge {
                size,
                max: MAX_IPC_FRAME_BYTES
            }) if size == MAX_IPC_FRAME_BYTES + 1
        ));
    }

    #[test]
    fn capped_encoder_writer_never_retains_more_than_the_wire_limit() {
        let mut writer = CappedFrameWriter::new(16);
        writer.write_all(&[b'x'; 64]).unwrap();
        writer.write_all(&[b'y'; 64]).unwrap();
        assert_eq!(writer.size, 128);
        assert_eq!(writer.bytes, vec![b'x'; 16]);
        assert_eq!(writer.bytes.capacity(), 16);
    }

    #[test]
    fn requests_round_trip_as_single_lines() {
        let reqs = vec![
            Request::Ping,
            Request::Open {
                url: "http://x/\n<-- embedded newline".into(),
                session: "default".into(),
                headless: true,
            },
            Request::Click {
                session: "default".into(),
                target: Target::Ref {
                    node_ref: "@node-4".into(),
                },
                button: "left".into(),
                count: 2,
                modifiers: 0,
                force: false,
                wait: WaitPolicy::Auto,
                timeout_ms: 30_000,
            },
        ];
        for req in reqs {
            let line = serde_json::to_string(&req).unwrap();
            assert!(
                !line.contains('\n'),
                "framing requires one line per message"
            );
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                line,
                "round trip must be lossless"
            );
        }
    }

    #[test]
    fn click_defaults_keep_old_clients_working() {
        let req: Request = serde_json::from_str(
            r#"{"op":"click","session":"default","target":{"kind":"ref","node_ref":"@node-1"}}"#,
        )
        .unwrap();
        match req {
            Request::Click {
                count,
                force,
                modifiers,
                ..
            } => {
                assert_eq!(count, 1);
                assert!(!force);
                assert_eq!(modifiers, 0);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn click_button_validation_is_closed_and_keeps_the_wire_default() {
        assert_eq!(ClickButton::parse(""), Some(ClickButton::Left));
        assert_eq!(ClickButton::parse("RIGHT"), Some(ClickButton::Right));
        let invalid = Request::Click {
            session: "default".into(),
            target: Target::Point { x: 1.0, y: 2.0 },
            button: "primary-ish".into(),
            count: 1,
            modifiers: 0,
            force: false,
            wait: WaitPolicy::None,
            timeout_ms: 1_000,
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn no_request_variant_can_carry_a_cdp_method() {
        // A guard against the capability surface quietly growing an escape hatch.
        let schema = serde_json::to_string(&Request::Ping).unwrap();
        assert_eq!(schema, r#"{"op":"ping"}"#);
        let names = [
            "Runtime.evaluate",
            "Target.",
            "Browser.",
            "Network.",
            "cdp",
            "raw",
        ];
        let all = format!("{:?}", std::any::type_name::<Request>());
        for n in names {
            assert!(!all.contains(n));
        }
    }

    #[test]
    fn errors_carry_actionable_hints() {
        let r = Response::error_hint("stale ref", "run `brow snapshot`");
        let line = serde_json::to_string(&r).unwrap();
        assert!(line.contains("\"status\":\"error\""));
        assert!(line.contains("brow snapshot"));
    }

    #[test]
    fn protocol_validation_rejects_invalid_wait_semantics() {
        let invalid = [
            Request::Click {
                session: "default".into(),
                target: Target::Point { x: 1.0, y: 2.0 },
                button: "left".into(),
                count: 1,
                modifiers: 0,
                force: false,
                wait: WaitPolicy::Conditions,
                timeout_ms: 1_000,
            },
            Request::Wait {
                session: "default".into(),
                conditions: WaitConditions::default(),
                timeout_ms: 100,
                quiet_ms: 0,
            },
            Request::Wait {
                session: "default".into(),
                conditions: WaitConditions {
                    stable: true,
                    ..WaitConditions::default()
                },
                timeout_ms: 0,
                quiet_ms: 0,
            },
            Request::Wait {
                session: "default".into(),
                conditions: WaitConditions {
                    stable: true,
                    ..WaitConditions::default()
                },
                timeout_ms: 120_001,
                quiet_ms: 0,
            },
            Request::Wait {
                session: "default".into(),
                conditions: WaitConditions {
                    stable: true,
                    ..WaitConditions::default()
                },
                timeout_ms: 1_000,
                quiet_ms: 1_001,
            },
            Request::Back {
                session: "default".into(),
                wait: Some(WaitPolicy::Auto),
                timeout_ms: 1_000,
            },
            Request::Forward {
                session: "default".into(),
                wait: Some(WaitPolicy::Auto),
                timeout_ms: 1_000,
            },
            Request::Reload {
                session: "default".into(),
                ignore_cache: false,
                wait: Some(WaitPolicy::Auto),
                timeout_ms: 1_000,
            },
        ];

        for request in invalid {
            request
                .validate()
                .expect_err("invalid raw protocol request must be rejected");
        }
        assert!(
            <WaitPolicy as clap::ValueEnum>::to_possible_value(&WaitPolicy::Conditions).is_none(),
            "the receipt-only policy must not be advertised by action CLI flags"
        );
    }

    #[test]
    fn protocol_validation_accepts_valid_wait_semantics() {
        for request in [
            Request::Wait {
                session: "default".into(),
                conditions: WaitConditions {
                    url: Some("*example.test/*".into()),
                    generation_after: Some(4),
                    load: true,
                    stable: true,
                },
                timeout_ms: 120_000,
                quiet_ms: 120_000,
            },
            Request::Back {
                session: "default".into(),
                wait: None,
                timeout_ms: 1,
            },
            Request::Reload {
                session: "default".into(),
                ignore_cache: true,
                wait: Some(WaitPolicy::Stable),
                timeout_ms: 30_000,
            },
        ] {
            request.validate().expect("valid request");
        }
    }
}
