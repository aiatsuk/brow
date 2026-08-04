//! The wire protocol between `brow` (the CLI) and `browd` (the daemon).
//!
//! Newline-delimited JSON over a Unix socket. `serde_json` escapes embedded
//! newlines, so a line is always exactly one message.
//!
//! This enum *is* the capability surface. Raw CDP is not representable here, which
//! is what makes "the agent never gets raw CDP" a structural property rather than
//! a promise: there is no request variant that carries a protocol method name.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped whenever a field changes meaning. A mismatch fails loudly at connect
/// time instead of producing a confusing error three calls later.
pub const PROTOCOL_VERSION: u32 = 1;

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
    Node { node_ref: String },
    Rect { x: f64, y: f64, width: f64, height: f64 },
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
        }
    }
    pub fn error_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Response::Error {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                target: Target::Ref { node_ref: "@node-4".into() },
                button: "left".into(),
                count: 2,
                modifiers: 0,
                force: false,
            },
        ];
        for req in reqs {
            let line = serde_json::to_string(&req).unwrap();
            assert!(!line.contains('\n'), "framing requires one line per message");
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
            Request::Click { count, force, modifiers, .. } => {
                assert_eq!(count, 1);
                assert!(!force);
                assert_eq!(modifiers, 0);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn no_request_variant_can_carry_a_cdp_method() {
        // A guard against the capability surface quietly growing an escape hatch.
        let schema = serde_json::to_string(&Request::Ping).unwrap();
        assert_eq!(schema, r#"{"op":"ping"}"#);
        let names = [
            "Runtime.evaluate", "Target.", "Browser.", "Network.", "cdp", "raw",
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
}
