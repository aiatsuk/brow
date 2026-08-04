//! Console, exception and network capture for one page session.
//!
//! A background task subscribes to the CDP event stream and folds it into two
//! bounded ring buffers. Bounded is the point: a chatty SPA emits tens of
//! thousands of events in a few minutes, and an unbounded log turns a long-running
//! session into an out-of-memory bug.
//!
//! **Timestamps are receive time, not browser event time.** CDP mixes at least
//! three clocks — `Network.MonotonicTime`, `Runtime.Timestamp` (epoch ms) and the
//! screencast frame clock — and reconciling them properly is its own piece of
//! work. Until that exists, every entry is stamped when this process saw it, which
//! is consistent, monotonic, and honest about being a few milliseconds late.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::{CdpClient, CdpEvent};
use crate::redact;

/// Kept deliberately modest: this is a debugging window, not an archive.
const CONSOLE_CAPACITY: usize = 2_000;
const NETWORK_CAPACITY: usize = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub seq: u64,
    /// Seconds since the session was attached.
    pub t: f64,
    /// `log`, `info`, `warn`, `error`, `debug`, or `exception`.
    pub level: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

impl ConsoleEntry {
    pub fn is_error(&self) -> bool {
        self.level == "error" || self.level == "exception"
    }

    pub fn render(&self) -> String {
        let mut s = format!("{:>8.3}  {:<9} {}", self.t, self.level, self.text);
        if let Some(url) = &self.url {
            let file = url.rsplit('/').next().unwrap_or(url);
            match self.line {
                Some(line) => s.push_str(&format!("  ({file}:{line})")),
                None => s.push_str(&format!("  ({file})")),
            }
        }
        s
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEntry {
    pub seq: u64,
    pub t: f64,
    pub request_id: String,
    pub method: String,
    /// Already redacted; credential query parameters never reach this field.
    pub url: String,
    pub resource_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub from_cache: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoded_bytes: Option<f64>,
    /// The response *body* finished transferring — not merely that headers
    /// arrived. A `fetch()` whose body is never read stays unfinished
    /// indefinitely while still reporting its status, which is correct and
    /// occasionally surprising.
    pub finished: bool,
}

impl NetworkEntry {
    pub fn is_failure(&self) -> bool {
        self.error.is_some() || self.status.is_some_and(|s| s >= 400)
    }

    pub fn render(&self) -> String {
        let status = match (&self.error, self.status) {
            (Some(e), _) => format!("FAIL {e}"),
            (None, Some(s)) => s.to_string(),
            (None, None) => "…".to_string(),
        };
        let mut s = format!(
            "{:>8.3}  {:<6} {:<12} {}",
            self.t, self.method, status, self.url
        );
        if self.from_cache {
            s.push_str("  (cache)");
        }
        if let Some(bytes) = self.encoded_bytes.filter(|b| *b > 0.0) {
            s.push_str(&format!("  {:.1} KB", bytes / 1024.0));
        }
        s
    }
}

#[derive(Debug, Default)]
struct Inner {
    console: VecDeque<ConsoleEntry>,
    network: VecDeque<NetworkEntry>,
    next_seq: u64,
    console_dropped: u64,
    network_dropped: u64,
}

/// Bounded, thread-safe capture for one session.
pub struct EventLog {
    inner: Mutex<Inner>,
    started: Instant,
}

impl EventLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            started: Instant::now(),
        })
    }

    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn next_seq(inner: &mut Inner) -> u64 {
        inner.next_seq += 1;
        inner.next_seq
    }

    /// Most recent console entries, oldest first.
    pub fn console(&self, errors_only: bool, limit: usize) -> Vec<ConsoleEntry> {
        let inner = self.inner.lock().expect("event log");
        let mut rows: Vec<ConsoleEntry> = inner
            .console
            .iter()
            .filter(|e| !errors_only || e.is_error())
            .cloned()
            .collect();
        if rows.len() > limit {
            rows.drain(..rows.len() - limit);
        }
        rows
    }

    /// Most recent network entries, oldest first.
    pub fn network(&self, failures_only: bool, limit: usize) -> Vec<NetworkEntry> {
        let inner = self.inner.lock().expect("event log");
        let mut rows: Vec<NetworkEntry> = inner
            .network
            .iter()
            .filter(|e| !failures_only || e.is_failure())
            .cloned()
            .collect();
        if rows.len() > limit {
            rows.drain(..rows.len() - limit);
        }
        rows
    }

    /// How many entries fell off the back of each ring.
    pub fn dropped(&self) -> (u64, u64) {
        let inner = self.inner.lock().expect("event log");
        (inner.console_dropped, inner.network_dropped)
    }

    /// Forgets everything captured so far.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("event log");
        inner.console.clear();
        inner.network.clear();
        inner.console_dropped = 0;
        inner.network_dropped = 0;
    }

    fn push_console(&self, mut entry: ConsoleEntry) {
        let mut inner = self.inner.lock().expect("event log");
        entry.seq = Self::next_seq(&mut inner);
        if inner.console.len() == CONSOLE_CAPACITY {
            inner.console.pop_front();
            inner.console_dropped += 1;
        }
        inner.console.push_back(entry);
    }

    fn push_network(&self, mut entry: NetworkEntry) {
        let mut inner = self.inner.lock().expect("event log");
        entry.seq = Self::next_seq(&mut inner);
        if inner.network.len() == NETWORK_CAPACITY {
            inner.network.pop_front();
            inner.network_dropped += 1;
        }
        inner.network.push_back(entry);
    }

    /// Updates an in-flight request in place.
    ///
    /// Linear from the back because responses arrive close behind their requests;
    /// with a 2000-entry ring the scan is bounded and an index would have to be
    /// rebuilt on every eviction anyway.
    fn update_network(&self, request_id: &str, f: impl FnOnce(&mut NetworkEntry)) -> bool {
        let mut inner = self.inner.lock().expect("event log");
        for entry in inner.network.iter_mut().rev() {
            if entry.request_id == request_id {
                f(entry);
                return true;
            }
        }
        false
    }
}

/// Enables the domains that produce the events, then records them until the
/// session ends.
pub async fn spawn_recorder(client: &Arc<CdpClient>, session_id: &str) -> Arc<EventLog> {
    let log = EventLog::new();

    // `Runtime.enable` is already on from `Page::attach`; `Log` adds the messages
    // the browser itself generates (CSP violations, deprecations, network errors)
    // which never appear as `Runtime.consoleAPICalled`.
    let _ = client.call_on(session_id, "Log.enable", json!({})).await;
    // Capping the buffers keeps a long session from growing unboundedly inside the
    // browser process, which we cannot see and cannot clear.
    let _ = client
        .call_on(
            session_id,
            "Network.enable",
            json!({
                "maxTotalBufferSize": 16 * 1024 * 1024,
                "maxResourceBufferSize": 4 * 1024 * 1024,
            }),
        )
        .await;

    let mut events = client.subscribe();
    let session_id = session_id.to_string();
    let sink = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "event recorder lagged");
                    continue;
                }
                Err(_) => break,
            };
            if ev.session_id.as_deref() != Some(session_id.as_str()) {
                continue;
            }
            record(&sink, &ev);
        }
    });

    log
}

fn record(log: &EventLog, ev: &CdpEvent) {
    let t = log.now();
    match ev.method.as_str() {
        "Runtime.consoleAPICalled" => {
            let level = ev
                .params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("log")
                .to_string();
            let text = redact::text(&render_args(ev.params.get("args")));
            let (url, line) = first_frame(ev.params.get("stackTrace"));
            log.push_console(ConsoleEntry { seq: 0, t, level, text, url, line });
        }
        "Runtime.exceptionThrown" => {
            let details = ev.params.get("exceptionDetails");
            let text = details
                .and_then(|d| d.get("exception"))
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.and_then(|d| d.get("text")).and_then(Value::as_str))
                .unwrap_or("uncaught exception")
                .to_string();
            let url = details
                .and_then(|d| d.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let line = details.and_then(|d| d.get("lineNumber")).and_then(Value::as_u64);
            log.push_console(ConsoleEntry {
                seq: 0,
                t,
                level: "exception".into(),
                text: redact::text(&text),
                url,
                line,
            });
        }
        "Log.entryAdded" => {
            let entry = ev.params.get("entry");
            let level = entry
                .and_then(|e| e.get("level"))
                .and_then(Value::as_str)
                .unwrap_or("info")
                .to_string();
            let text = entry
                .and_then(|e| e.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let url = entry
                .and_then(|e| e.get("url"))
                .and_then(Value::as_str)
                .map(redact::url);
            let line = entry.and_then(|e| e.get("lineNumber")).and_then(Value::as_u64);
            log.push_console(ConsoleEntry {
                seq: 0,
                t,
                level,
                text: redact::text(&text),
                url,
                line,
            });
        }
        "Network.requestWillBeSent" => {
            let request = ev.params.get("request");
            log.push_network(NetworkEntry {
                seq: 0,
                t,
                request_id: string_at(&ev.params, "requestId"),
                method: request
                    .and_then(|r| r.get("method"))
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_string(),
                url: redact::url(
                    request
                        .and_then(|r| r.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                resource_type: ev
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("Other")
                    .to_string(),
                status: None,
                mime: None,
                error: None,
                from_cache: false,
                encoded_bytes: None,
                finished: false,
            });
        }
        "Network.responseReceived" => {
            let response = ev.params.get("response");
            let status = response.and_then(|r| r.get("status")).and_then(Value::as_i64);
            let mime = response
                .and_then(|r| r.get("mimeType"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let from_cache = response
                .and_then(|r| r.get("fromDiskCache"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            log.update_network(&string_at(&ev.params, "requestId"), |e| {
                e.status = status;
                e.mime = mime;
                e.from_cache = from_cache;
            });
        }
        "Network.loadingFinished" => {
            let bytes = ev.params.get("encodedDataLength").and_then(Value::as_f64);
            log.update_network(&string_at(&ev.params, "requestId"), |e| {
                e.encoded_bytes = bytes;
                e.finished = true;
            });
        }
        "Network.loadingFailed" => {
            let error = ev
                .params
                .get("errorText")
                .and_then(Value::as_str)
                .unwrap_or("failed")
                .to_string();
            let canceled = ev
                .params
                .get("canceled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            log.update_network(&string_at(&ev.params, "requestId"), |e| {
                // A cancelled request is usually the page changing its mind, not a
                // problem; label it so it does not read as an error.
                e.error = Some(if canceled { format!("canceled ({error})") } else { error });
                e.finished = true;
            });
        }
        _ => {}
    }
}

fn string_at(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Flattens `console.log(a, b, c)` arguments into one line.
///
/// Object arguments come back as a `RemoteObject` with a preview rather than a
/// value. Rendering the preview shallowly keeps a single logged object from
/// costing hundreds of tokens.
fn render_args(args: Option<&Value>) -> String {
    let Some(args) = args.and_then(Value::as_array) else {
        return String::new();
    };
    args.iter()
        .map(render_remote_object)
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_remote_object(obj: &Value) -> String {
    if let Some(value) = obj.get("value") {
        return match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(desc) = obj.get("description").and_then(Value::as_str) {
        return desc.to_string();
    }
    if let Some(preview) = obj.get("preview") {
        let props: Vec<String> = preview
            .get("properties")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .take(8)
                    .map(|p| {
                        format!(
                            "{}: {}",
                            p.get("name").and_then(Value::as_str).unwrap_or("?"),
                            p.get("value").and_then(Value::as_str).unwrap_or("…")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let overflow = preview
            .get("overflow")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return format!("{{{}{}}}", props.join(", "), if overflow { ", …" } else { "" });
    }
    obj.get("type")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

/// Source location of the innermost stack frame, if there is one.
fn first_frame(stack: Option<&Value>) -> (Option<String>, Option<u64>) {
    let Some(frame) = stack
        .and_then(|s| s.get("callFrames"))
        .and_then(Value::as_array)
        .and_then(|f| f.first())
    else {
        return (None, None);
    };
    (
        frame.get("url").and_then(Value::as_str).map(redact::url),
        frame.get("lineNumber").and_then(Value::as_u64),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.to_string(),
            params,
            session_id: Some("s".into()),
        }
    }

    #[test]
    fn console_args_render_on_one_line() {
        let log = EventLog::new();
        record(
            &log,
            &event(
                "Runtime.consoleAPICalled",
                json!({
                    "type": "warn",
                    "args": [
                        {"type": "string", "value": "cart total"},
                        {"type": "number", "value": 42},
                        {"type": "object", "preview": {"properties": [
                            {"name": "id", "value": "7"}, {"name": "sku", "value": "abc"}
                        ], "overflow": true}}
                    ],
                    "stackTrace": {"callFrames": [{"url": "http://x/app.js", "lineNumber": 1821}]}
                }),
            ),
        );
        let rows = log.console(false, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, "warn");
        assert_eq!(rows[0].text, "cart total 42 {id: 7, sku: abc, …}");
        assert_eq!(rows[0].line, Some(1821));
        assert!(rows[0].render().contains("app.js:1821"));
    }

    #[test]
    fn secrets_are_redacted_before_they_are_stored() {
        let log = EventLog::new();
        record(
            &log,
            &event(
                "Runtime.consoleAPICalled",
                json!({"type": "log", "args": [
                    {"type": "string", "value": "retrying with Bearer sk_live_9f8a7b6c5d"}
                ]}),
            ),
        );
        record(
            &log,
            &event(
                "Network.requestWillBeSent",
                json!({
                    "requestId": "r1",
                    "type": "XHR",
                    "request": {"method": "POST", "url": "https://api.test/v1?access_token=abc123&page=1"}
                }),
            ),
        );

        let stored = format!("{:?}{:?}", log.console(false, 10), log.network(false, 10));
        assert!(!stored.contains("sk_live_9f8a7b6c5d"), "token reached storage");
        assert!(!stored.contains("abc123"), "query credential reached storage");
        assert!(stored.contains("page=1"), "non-secret parameters must survive");
    }

    #[test]
    fn a_request_accumulates_its_response_and_size() {
        let log = EventLog::new();
        record(
            &log,
            &event(
                "Network.requestWillBeSent",
                json!({"requestId": "r1", "type": "Document",
                       "request": {"method": "GET", "url": "https://x.test/"}}),
            ),
        );
        record(
            &log,
            &event(
                "Network.responseReceived",
                json!({"requestId": "r1",
                       "response": {"status": 200, "mimeType": "text/html", "fromDiskCache": false}}),
            ),
        );
        record(
            &log,
            &event(
                "Network.loadingFinished",
                json!({"requestId": "r1", "encodedDataLength": 2048.0}),
            ),
        );

        let rows = log.network(false, 10);
        assert_eq!(rows.len(), 1, "a request must stay one row, not three");
        assert_eq!(rows[0].status, Some(200));
        assert_eq!(rows[0].mime.as_deref(), Some("text/html"));
        assert!(rows[0].finished);
        assert!(rows[0].render().contains("2.0 KB"));
        assert!(!rows[0].is_failure());
    }

    #[test]
    fn failures_and_cancellations_are_distinguished() {
        let log = EventLog::new();
        for (id, canceled) in [("r1", false), ("r2", true)] {
            record(
                &log,
                &event(
                    "Network.requestWillBeSent",
                    json!({"requestId": id, "type": "XHR",
                           "request": {"method": "GET", "url": "https://x.test/a"}}),
                ),
            );
            record(
                &log,
                &event(
                    "Network.loadingFailed",
                    json!({"requestId": id, "errorText": "net::ERR_ABORTED", "canceled": canceled}),
                ),
            );
        }
        let rows = log.network(true, 10);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].error.as_ref().unwrap().contains("canceled"));
        assert!(rows[1].error.as_ref().unwrap().contains("canceled"));
    }

    #[test]
    fn http_errors_count_as_failures() {
        let log = EventLog::new();
        record(&log, &event("Network.requestWillBeSent",
            json!({"requestId": "r1", "type": "XHR",
                   "request": {"method": "GET", "url": "https://x.test/missing"}})));
        record(&log, &event("Network.responseReceived",
            json!({"requestId": "r1", "response": {"status": 404, "mimeType": "text/html"}})));
        assert_eq!(log.network(true, 10).len(), 1);
        assert_eq!(log.network(false, 10).len(), 1);
    }

    #[test]
    fn rings_are_bounded_and_report_what_they_dropped() {
        let log = EventLog::new();
        for i in 0..(CONSOLE_CAPACITY + 25) {
            record(
                &log,
                &event(
                    "Runtime.consoleAPICalled",
                    json!({"type": "log", "args": [{"type": "string", "value": format!("m{i}")}]}),
                ),
            );
        }
        assert_eq!(log.console(false, usize::MAX).len(), CONSOLE_CAPACITY);
        assert_eq!(log.dropped().0, 25);
        // The newest entries are the ones kept.
        let rows = log.console(false, 1);
        assert_eq!(rows[0].text, format!("m{}", CONSOLE_CAPACITY + 24));
    }

    #[test]
    fn limit_returns_the_most_recent_entries_oldest_first() {
        let log = EventLog::new();
        for i in 0..10 {
            record(&log, &event("Runtime.consoleAPICalled",
                json!({"type": "log", "args": [{"type": "string", "value": format!("m{i}")}]})));
        }
        let rows = log.console(false, 3);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["m7", "m8", "m9"]
        );
    }

    #[test]
    fn browser_generated_messages_are_captured_too() {
        let log = EventLog::new();
        record(
            &log,
            &event(
                "Log.entryAdded",
                json!({"entry": {
                    "level": "error",
                    "text": "Refused to load the script because it violates CSP",
                    "url": "https://x.test/page",
                    "lineNumber": 12
                }}),
            ),
        );
        let rows = log.console(true, 10);
        assert_eq!(rows.len(), 1, "CSP violations never appear as consoleAPICalled");
        assert!(rows[0].is_error());
    }
}
