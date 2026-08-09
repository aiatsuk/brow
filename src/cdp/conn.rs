//! Request/response correlation and event fan-out for one CDP pipe.
//!
//! One `CdpClient` multiplexes every browser-level and session-level call over the
//! single pipe. Sessions are addressed with the flat protocol: a top-level
//! `sessionId` on the message, obtained from `Target.attachToTarget{flatten:true}`.
//! There is no nested `Target.sendMessageToTarget` anywhere in this codebase.

use std::collections::HashMap;
use std::io::Write;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use super::transport::{PipeSender, PipeTransport, TransportSendError, MAX_CDP_FRAME_BYTES};

/// Wall-clock ceiling for a single CDP round trip.
///
/// Generous, because `Page.navigate` and `DOMSnapshot.captureSnapshot` on a heavy
/// page are legitimately slow; callers that need to fail faster pass their own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Aggregate raw JSON bytes retained by the event broadcast and its receivers.
/// Responses never consume this budget and therefore cannot be dropped behind an
/// event storm.
const EVENT_RETAINED_BYTE_CAPACITY: usize = 64 * 1024 * 1024;

/// Synthetic event emitted when a real CDP event could not be retained within
/// the byte budget. It deliberately still advances the bounded broadcast ring:
/// otherwise old budgeted events can remain pinned in the ring forever and turn
/// one overload burst into a permanent, silent event blackout.
pub const EVENT_STREAM_GAP_METHOD: &str = "Brow.eventStreamGap";

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
    #[error("{method}: CDP outbound queue is full ({capacity} messages); retry later")]
    Overloaded { method: String, capacity: usize },
    #[error("{method}: serialized CDP request is {size} bytes; maximum is {max} bytes")]
    FrameTooLarge {
        method: String,
        size: usize,
        max: usize,
    },
    #[error(
        "{method}: CDP outbound byte budget is exhausted (request {size} bytes, budget {capacity} bytes); retry later"
    )]
    OverloadedBytes {
        method: String,
        size: usize,
        capacity: usize,
    },
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

#[derive(Debug)]
struct EventBudget {
    capacity: usize,
    available: Mutex<usize>,
}

impl EventBudget {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            available: Mutex::new(capacity),
        })
    }

    fn try_acquire(self: &Arc<Self>, size: usize) -> Option<EventBytePermit> {
        let mut available = self.available.lock().expect("event budget mutex");
        if size > *available {
            return None;
        }
        *available -= size;
        Some(EventBytePermit {
            budget: Arc::clone(self),
            size,
        })
    }

    #[cfg(test)]
    fn available(&self) -> usize {
        *self.available.lock().expect("event budget mutex")
    }
}

#[derive(Debug)]
struct EventBytePermit {
    budget: Arc<EventBudget>,
    size: usize,
}

impl Drop for EventBytePermit {
    fn drop(&mut self) {
        let mut available = self.budget.available.lock().expect("event budget mutex");
        *available += self.size;
        debug_assert!(*available <= self.budget.capacity);
    }
}

/// Shared event payload returned by subscriptions.
///
/// The `Arc` held by Tokio's broadcast ring and every receiver points to this one
/// `CdpEvent`, so `serde_json::Value` is not deep-cloned per subscriber. Its byte
/// permit is released only after the ring and every consumer drop the event.
#[derive(Debug)]
pub struct CdpEventHandle {
    event: CdpEvent,
    // Synthetic overflow markers are tiny and intentionally do not consume the
    // retained-event budget. Real browser events always carry a permit.
    _permit: Option<EventBytePermit>,
}

impl Deref for CdpEventHandle {
    type Target = CdpEvent;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

pub type CdpEventReceiver = broadcast::Receiver<Arc<CdpEventHandle>>;

type Waiter = oneshot::Sender<Result<Value, CdpError>>;

struct ConnectionState {
    closed: bool,
    pending: HashMap<u64, Waiter>,
}

type SharedState = Arc<Mutex<ConnectionState>>;

fn publish_event(
    events: &broadcast::Sender<Arc<CdpEventHandle>>,
    event_budget: &Arc<EventBudget>,
    event: CdpEvent,
    frame_size: usize,
    dropped_events: &mut u64,
) -> bool {
    if let Some(permit) = event_budget.try_acquire(frame_size) {
        // Err just means nobody is subscribed right now. The returned Arc is
        // dropped immediately and releases its permit.
        let _ = events.send(Arc::new(CdpEventHandle {
            event,
            _permit: Some(permit),
        }));
        return true;
    }

    *dropped_events = dropped_events.saturating_add(1);
    let source_method = event.method;
    let session_id = event.session_id;
    let gap = CdpEvent {
        method: EVENT_STREAM_GAP_METHOD.to_string(),
        params: json!({
            "dropped": 1,
            "droppedTotal": *dropped_events,
            "eventBytes": frame_size,
            "capacity": event_budget.capacity,
            "sourceMethod": source_method,
            "reason": "retained_event_byte_budget_exhausted",
        }),
        session_id,
    };
    // Always publish a bounded, permit-free marker. Besides making loss visible
    // to every subscriber, repeated markers advance the broadcast ring and evict
    // old retained events so their permits can be released.
    let _ = events.send(Arc::new(CdpEventHandle {
        event: gap,
        _permit: None,
    }));
    false
}

/// Removes an in-flight request on every exit path, including future
/// cancellation. Async destructors are not available, so this synchronous guard
/// owns only the small pending-map operation and never waits on transport I/O.
struct PendingRequest {
    id: u64,
    state: SharedState,
}

impl PendingRequest {
    fn insert(id: u64, state: &SharedState, reply: Waiter) -> Result<Self, CdpError> {
        let mut guard = state.lock().expect("connection state mutex");
        if guard.closed {
            return Err(CdpError::Closed);
        }
        guard.pending.insert(id, reply);
        Ok(Self {
            id,
            state: Arc::clone(state),
        })
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("connection state mutex")
            .pending
            .remove(&self.id);
    }
}

fn close_connection(state: &SharedState) {
    let waiters: Vec<_> = {
        let mut guard = state.lock().expect("connection state mutex");
        guard.closed = true;
        guard.pending.drain().map(|(_, waiter)| waiter).collect()
    };
    for waiter in waiters {
        let _ = waiter.send(Err(CdpError::Closed));
    }
}

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
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.size = self.size.saturating_add(buf.len());
        let keep = buf.len().min(self.max.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&buf[..keep]);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_frame(method: &str, message: &Value, max: usize) -> Result<Vec<u8>, CdpError> {
    let mut writer = CappedFrameWriter::new(max);
    serde_json::to_writer(&mut writer, message).map_err(|error| CdpError::Decode {
        method: method.to_string(),
        source: Arc::new(error),
    })?;
    if writer.size > max {
        return Err(CdpError::FrameTooLarge {
            method: method.to_string(),
            size: writer.size,
            max,
        });
    }
    Ok(writer.bytes)
}

fn send_error(method: &str, error: TransportSendError) -> CdpError {
    match error {
        TransportSendError::Closed => CdpError::Closed,
        TransportSendError::Overloaded { capacity } => CdpError::Overloaded {
            method: method.to_string(),
            capacity,
        },
        TransportSendError::FrameTooLarge { size, max } => CdpError::FrameTooLarge {
            method: method.to_string(),
            size,
            max,
        },
        TransportSendError::ByteBudgetExhausted { size, capacity } => CdpError::OverloadedBytes {
            method: method.to_string(),
            size,
            capacity,
        },
    }
}

/// A live connection to a Chromium instance.
pub struct CdpClient {
    tx: PipeSender,
    next_id: AtomicU64,
    state: SharedState,
    events: broadcast::Sender<Arc<CdpEventHandle>>,
}

impl CdpClient {
    /// Starts the router task over an established transport.
    pub fn start(transport: PipeTransport) -> Arc<Self> {
        let (tx, mut rx) = transport.split();
        // 4096 slots: an event burst during page load can be thousands of DOM
        // mutations. Slow subscribers lag rather than block the router.
        let (events, _) = broadcast::channel(4096);
        let event_budget = EventBudget::new(EVENT_RETAINED_BYTE_CAPACITY);
        let client = Arc::new(Self {
            tx,
            next_id: AtomicU64::new(1),
            state: Arc::new(Mutex::new(ConnectionState {
                closed: false,
                pending: HashMap::new(),
            })),
            events: events.clone(),
        });

        let state = Arc::clone(&client.state);
        tokio::spawn(async move {
            let mut dropped_events = 0u64;
            while let Some(frame) = rx.recv().await {
                let frame_size = frame.len();
                let mut msg: Value = match serde_json::from_slice(&frame) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "undecodable CDP frame dropped");
                        continue;
                    }
                };

                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let waiter = state
                        .lock()
                        .expect("connection state mutex")
                        .pending
                        .remove(&id);
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
                            data: err.get("data").and_then(Value::as_str).map(str::to_string),
                        })
                    } else {
                        Ok(msg
                            .as_object_mut()
                            .and_then(|object| object.remove("result"))
                            .unwrap_or_else(|| json!({})))
                    };
                    let _ = waiter.send(outcome);
                } else if let Some(method) = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    let event = CdpEvent {
                        method: method.clone(),
                        params: msg
                            .as_object_mut()
                            .and_then(|object| object.remove("params"))
                            .unwrap_or_else(|| json!({})),
                        session_id: msg
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    };
                    let retained = publish_event(
                        &events,
                        &event_budget,
                        event,
                        frame_size,
                        &mut dropped_events,
                    );
                    if retained {
                        continue;
                    }
                    if dropped_events == 1 || dropped_events.is_power_of_two() {
                        tracing::warn!(
                            dropped = dropped_events,
                            event_bytes = frame_size,
                            budget = EVENT_RETAINED_BYTE_CAPACITY,
                            method = %method,
                            "CDP event dropped because retained-event byte budget is exhausted"
                        );
                    }
                }
            }

            // Pipe closed: fail every in-flight call rather than let them time out
            // one by one.
            close_connection(&state);
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
        let _pending_request = PendingRequest::insert(id, &self.state, tx)?;

        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            msg["sessionId"] = Value::String(sid.to_string());
        }

        let frame = encode_frame(method, &msg, MAX_CDP_FRAME_BYTES)?;

        // Serialize outside the mutex, then make the final closed check and
        // non-blocking enqueue atomic with respect to router shutdown. If the
        // router closes immediately after this send, it drains our waiter and
        // the call still completes with `Closed` rather than its deadline.
        let send = {
            let guard = self.state.lock().expect("connection state mutex");
            if guard.closed {
                return Err(CdpError::Closed);
            }
            self.tx.send(frame)
        };
        if let Err(error) = send {
            if matches!(error, TransportSendError::Closed) {
                close_connection(&self.state);
            }
            return Err(send_error(method, error));
        }

        tracing::trace!(id, method, session = ?session_id, "cdp ->");

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => Err(err.with_method(method)),
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => Err(CdpError::Timeout {
                method: method.to_string(),
                timeout,
            }),
        }
    }

    /// Subscribes to the event stream from this moment on.
    pub fn subscribe(&self) -> CdpEventReceiver {
        self.events.subscribe()
    }

    /// Waits for the first event matching `pred`.
    ///
    /// Subscribe *before* issuing the command that triggers the event, otherwise
    /// the event can land in the gap and the wait hangs until the timeout.
    pub async fn wait_for(
        rx: &mut CdpEventReceiver,
        timeout: Duration,
        mut pred: impl FnMut(&CdpEvent) -> bool,
    ) -> Option<Arc<CdpEventHandle>> {
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
    async fn fake_browser() -> (Arc<CdpClient>, PipeSender) {
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
                    "Boom" => {
                        json!({"id": id, "error": {"code": -32601, "message": "'Boom' wasn't found"}})
                    }
                    "Silent" => continue, // never answers, to exercise timeouts
                    _ => {
                        json!({"id": id, "result": {"echoed": method, "session": msg.get("sessionId")}})
                    }
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
        assert_eq!(
            b["session"], "sid-7",
            "flat sessionId must ride on the message"
        );
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
            client.state.lock().unwrap().pending.is_empty(),
            "timed-out call must not leak a pending entry"
        );
    }

    #[tokio::test]
    async fn cancelling_a_call_releases_the_pending_slot() {
        let (client, _) = fake_browser().await;
        let task_client = Arc::clone(&client);
        let task = tokio::spawn(async move {
            task_client
                .call_inner("Silent", json!({}), None, Duration::from_secs(60))
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while client.state.lock().unwrap().pending.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("call never registered its pending request");

        task.abort();
        let _ = task.await;
        assert!(
            client.state.lock().unwrap().pending.is_empty(),
            "dropping a call future must not leak a pending entry"
        );
    }

    #[tokio::test]
    async fn calls_fail_fast_after_the_inbound_router_closes() {
        // Keep the outbound pipe open while independently closing the browser's
        // inbound writer. This reproduces an oversized/read-side failure without
        // relying on the writer thread to notice anything.
        let (browser_read, us_write) = cloexec_pipe().unwrap();
        let (us_read, browser_write) = cloexec_pipe().unwrap();
        let client = CdpClient::start(unsafe { PipeTransport::from_raw_fds(us_write, us_read) });

        // SAFETY: this browser-side fd has not been handed to a transport.
        unsafe { libc::close(browser_write) };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !client.state.lock().unwrap().closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("router did not observe inbound closure");

        let started = std::time::Instant::now();
        let error = client
            .call("Browser.getVersion", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(error, CdpError::Closed));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "closed client waited instead of failing immediately"
        );
        assert!(client.state.lock().unwrap().pending.is_empty());

        // SAFETY: this browser-side fd has not been handed to a transport.
        unsafe { libc::close(browser_read) };
    }

    #[test]
    fn oversized_serialized_request_is_a_structured_cdp_error() {
        let message = json!({"blob": "abcdef"});
        let expected_size = serde_json::to_vec(&message).unwrap().len();
        let max = expected_size - 1;

        let error = encode_frame("Test.large", &message, max).unwrap_err();
        assert!(matches!(
            error,
            CdpError::FrameTooLarge {
                ref method,
                size,
                max: error_max,
            } if method == "Test.large" && size == expected_size && error_max == max
        ));
    }

    #[test]
    fn retained_event_budget_follows_all_arc_clones() {
        let budget = EventBudget::new(8);
        let retained = Arc::new(CdpEventHandle {
            event: CdpEvent {
                method: "Test.event".into(),
                params: json!({"value": 1}),
                session_id: Some("session-1".into()),
            },
            _permit: Some(budget.try_acquire(6).unwrap()),
        });
        let subscriber_copy = Arc::clone(&retained);

        assert_eq!(budget.available(), 2);
        assert!(budget.try_acquire(3).is_none());
        assert!(Arc::ptr_eq(&retained, &subscriber_copy));

        drop(retained);
        assert_eq!(
            budget.available(),
            2,
            "one subscriber still retains the shared event"
        );
        drop(subscriber_copy);
        assert_eq!(budget.available(), 8);
    }

    #[tokio::test]
    async fn event_budget_overflow_is_visible_and_cannot_pin_the_ring_forever() {
        let budget = EventBudget::new(8);
        let (events, _) = broadcast::channel(2);
        let mut receiver = events.subscribe();
        let mut dropped = 0;
        let event = |method: &str| CdpEvent {
            method: method.into(),
            params: json!({}),
            session_id: Some("session-1".into()),
        };

        assert!(publish_event(
            &events,
            &budget,
            event("Test.first"),
            6,
            &mut dropped,
        ));
        assert!(!publish_event(
            &events,
            &budget,
            event("Test.droppedOne"),
            3,
            &mut dropped,
        ));
        // The second marker overwrites the retained first event in the two-slot
        // ring. That eviction must release its byte permit even though this
        // subscriber has not read anything yet.
        assert!(!publish_event(
            &events,
            &budget,
            event("Test.droppedTwo"),
            3,
            &mut dropped,
        ));
        assert_eq!(budget.available(), 8);
        assert!(publish_event(
            &events,
            &budget,
            event("Test.recovered"),
            3,
            &mut dropped,
        ));

        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        let mut seen_gap = false;
        let mut seen_recovery = false;
        while let Ok(received) = receiver.try_recv() {
            seen_gap |= received.method == EVENT_STREAM_GAP_METHOD;
            seen_recovery |= received.method == "Test.recovered";
        }
        assert!(seen_gap, "overflow must be represented in the event stream");
        assert!(seen_recovery, "real events must resume after ring eviction");
    }
}
