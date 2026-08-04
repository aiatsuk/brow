//! Request/response correlation and event fan-out for one CDP pipe.
//!
//! One `CdpClient` multiplexes every browser-level and session-level call over the
//! single pipe. Sessions are addressed with the flat protocol: a top-level
//! `sessionId` on the message, obtained from `Target.attachToTarget{flatten:true}`.
//! There is no nested `Target.sendMessageToTarget` anywhere in this codebase.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::transport::PipeTransport;

/// Wall-clock ceiling for a single CDP round trip.
///
/// Generous, because `Page.navigate` and `DOMSnapshot.captureSnapshot` on a heavy
/// page are legitimately slow; callers that need to fail faster pass their own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, thiserror::Error)]
pub enum CdpError {
    /// The browser answered with a protocol-level error object.
    #[error("{method}: {message} (code {code})")]
    Protocol {
        method: String,
        code: i64,
        message: String,
        data: Option<String>,
    },
    #[error("{method}: timed out after {timeout:?}")]
    Timeout { method: String, timeout: Duration },
    #[error("browser connection closed")]
    Closed,
    #[error("{method}: malformed response: {source}")]
    Decode {
        method: String,
        #[source]
        source: Arc<serde_json::Error>,
    },
}

impl CdpError {
    /// True when the failure means the browser is gone, not that a call was bad.
    pub fn is_fatal(&self) -> bool {
        matches!(self, CdpError::Closed)
    }

    /// Chromium reports an unknown method as JSON-RPC `-32601`. We use this to
    /// detect capabilities that a given Chrome build simply does not have.
    pub fn is_method_not_found(&self) -> bool {
        matches!(self, CdpError::Protocol { code: -32601, .. })
    }
}

/// One event delivered by the browser.
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    /// `None` for browser-level events (e.g. `Target.targetCreated`).
    pub session_id: Option<String>,
}

impl CdpEvent {
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.params.clone())
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>;

/// A live connection to a Chromium instance.
pub struct CdpClient {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    next_id: AtomicU64,
    pending: Pending,
    events: broadcast::Sender<CdpEvent>,
}

impl CdpClient {
    /// Starts the router task over an established transport.
    pub fn start(transport: PipeTransport) -> Arc<Self> {
        let (tx, mut rx) = transport.split();
        // 4096 slots: an event burst during page load can be thousands of DOM
        // mutations. Slow subscribers lag rather than block the router.
        let (events, _) = broadcast::channel(4096);
        let client = Arc::new(Self {
            tx,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            events: events.clone(),
        });

        let pending = Arc::clone(&client.pending);
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let msg: Value = match serde_json::from_slice(&frame) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "undecodable CDP frame dropped");
                        continue;
                    }
                };

                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let waiter = pending.lock().expect("pending mutex").remove(&id);
                    let Some(waiter) = waiter else {
                        // A response to a call whose caller timed out or was
                        // cancelled. Nothing to do.
                        continue;
                    };
                    let outcome = if let Some(err) = msg.get("error") {
                        Err(CdpError::Protocol {
                            // The method name is filled in by the caller, which is
                            // the only place that knows it.
                            method: String::new(),
                            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                                .to_string(),
                            data: err
                                .get("data")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or_else(|| json!({})))
                    };
                    let _ = waiter.send(outcome);
                } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
                    let event = CdpEvent {
                        method: method.to_string(),
                        params: msg.get("params").cloned().unwrap_or_else(|| json!({})),
                        session_id: msg
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    };
                    // Err just means nobody is subscribed right now.
                    let _ = events.send(event);
                }
            }

            // Pipe closed: fail every in-flight call rather than let them time out
            // one by one.
            let mut guard = pending.lock().expect("pending mutex");
            for (_, waiter) in guard.drain() {
                let _ = waiter.send(Err(CdpError::Closed));
            }
        });

        client
    }

    /// Sends a browser-level command.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.call_inner(method, params, None, DEFAULT_TIMEOUT).await
    }

    /// Sends a command scoped to an attached session (flat protocol).
    pub async fn call_on(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        self.call_inner(method, params, Some(session_id), DEFAULT_TIMEOUT)
            .await
    }

    /// Session-scoped command with an explicit deadline.
    pub async fn call_on_timeout(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        self.call_inner(method, params, Some(session_id), timeout)
            .await
    }

    async fn call_inner(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending mutex").insert(id, tx);

        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            msg["sessionId"] = Value::String(sid.to_string());
        }

        let frame = serde_json::to_vec(&msg).map_err(|e| CdpError::Decode {
            method: method.to_string(),
            source: Arc::new(e),
        })?;

        if self.tx.send(frame).is_err() {
            self.pending.lock().expect("pending mutex").remove(&id);
            return Err(CdpError::Closed);
        }

        tracing::trace!(id, method, session = ?session_id, "cdp ->");

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => Err(err.with_method(method)),
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                self.pending.lock().expect("pending mutex").remove(&id);
                Err(CdpError::Timeout {
                    method: method.to_string(),
                    timeout,
                })
            }
        }
    }

    /// Subscribes to the event stream from this moment on.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    /// Waits for the first event matching `pred`.
    ///
    /// Subscribe *before* issuing the command that triggers the event, otherwise
    /// the event can land in the gap and the wait hangs until the timeout.
    pub async fn wait_for(
        rx: &mut broadcast::Receiver<CdpEvent>,
        timeout: Duration,
        mut pred: impl FnMut(&CdpEvent) -> bool,
    ) -> Option<CdpEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if pred(&ev) {
                        return Some(ev);
                    }
                }
                // Lagged: we missed events, but the one we want may still come.
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    tracing::warn!(skipped = n, "event subscriber lagged");
                }
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }
}

impl CdpError {
    fn with_method(self, method: &str) -> Self {
        match self {
            CdpError::Protocol {
                code,
                message,
                data,
                ..
            } => CdpError::Protocol {
                method: method.to_string(),
                code,
                message,
                data,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::transport::{cloexec_pipe, PipeTransport};

    /// Stands in for Chromium: echoes canned responses back over a loopback pipe.
    async fn fake_browser() -> (Arc<CdpClient>, mpsc::UnboundedSender<Vec<u8>>) {
        // us -> browser
        let (browser_read, us_write) = cloexec_pipe().unwrap();
        // browser -> us
        let (us_read, browser_write) = cloexec_pipe().unwrap();

        let transport = unsafe { PipeTransport::from_raw_fds(us_write, us_read) };
        let client = CdpClient::start(transport);

        // The "browser" side, driven by the test.
        let mut browser = unsafe { PipeTransport::from_raw_fds(browser_write, browser_read) };
        let to_us = browser.sender();
        tokio::spawn(async move {
            while let Some(frame) = browser.recv().await {
                let msg: Value = serde_json::from_slice(&frame).unwrap();
                let id = msg["id"].as_u64().unwrap();
                let method = msg["method"].as_str().unwrap();
                let reply = match method {
                    "Boom" => json!({"id": id, "error": {"code": -32601, "message": "'Boom' wasn't found"}}),
                    "Silent" => continue, // never answers, to exercise timeouts
                    _ => json!({"id": id, "result": {"echoed": method, "session": msg.get("sessionId")}}),
                };
                let _ = to_us.send(serde_json::to_vec(&reply).unwrap());
            }
        });

        let spare = client.tx.clone();
        (client, spare)
    }

    #[tokio::test]
    async fn correlates_responses_to_requests() {
        let (client, _) = fake_browser().await;
        let a = client.call("Browser.getVersion", json!({}));
        let b = client.call_on("sid-7", "Runtime.evaluate", json!({}));
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a.unwrap()["echoed"], "Browser.getVersion");
        let b = b.unwrap();
        assert_eq!(b["echoed"], "Runtime.evaluate");
        assert_eq!(b["session"], "sid-7", "flat sessionId must ride on the message");
    }

    #[tokio::test]
    async fn protocol_errors_carry_the_method_name() {
        let (client, _) = fake_browser().await;
        let err = client.call("Boom", json!({})).await.unwrap_err();
        assert!(err.is_method_not_found(), "got {err:?}");
        assert!(err.to_string().contains("Boom"));
    }

    #[tokio::test]
    async fn timeouts_release_the_pending_slot() {
        let (client, _) = fake_browser().await;
        let err = client
            .call_inner("Silent", json!({}), None, Duration::from_millis(80))
            .await
            .unwrap_err();
        assert!(matches!(err, CdpError::Timeout { .. }), "got {err:?}");
        assert!(
            client.pending.lock().unwrap().is_empty(),
            "timed-out call must not leak a pending entry"
        );
    }
}
