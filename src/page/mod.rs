//! A single attached page: navigation, snapshots, input, capture, evaluation.

pub mod capture;
pub mod events;
pub mod input;
pub mod navigation;
pub mod tree;

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::cdp::conn::CdpEventHandle;
use crate::cdp::{CdpClient, CdpError, EVENT_STREAM_GAP_METHOD};
use crate::redact;

pub use capture::{Capture, ImageFormat, Region};
pub use events::{ConsoleEntry, EventLog, NetworkEntry};
pub use input::{DispatchState, MouseButton, Point};
pub use navigation::{
    ActionReceipt, NavigationKind, NavigationScope, NavigationTrigger, PointerParkReceipt,
    WaitOutcome,
};
pub use tree::{CoverageGap, Node, NodeFingerprint, NodeIdentity, RefError, Snapshot};

/// How long to wait for a navigation to settle before returning anyway.
const NAV_TIMEOUT: Duration = Duration::from_secs(30);
const TARGET_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
const NAVIGATION_EVENT_ACK_CAPACITY: usize = 4096;

/// V8 execution budget for one `Runtime.evaluate` call.
///
/// Unlike the transport deadline, this asks V8 to terminate synchronous script
/// execution, so a busy loop cannot keep the renderer occupied after brow stops
/// waiting. Chrome's `Runtime.TimeDelta` is expressed in milliseconds.
const EVALUATE_V8_TIMEOUT: Duration = Duration::from_secs(2);

/// End-to-end deadline for an evaluation request.
///
/// This is intentionally longer than the V8 budget so Chromium has time to send
/// the termination response. It also bounds cases V8's execution timer does not,
/// such as an `awaitPromise` result that never settles or a broken transport.
const EVALUATE_DEADLINE: Duration = Duration::from_secs(3);

/// Deterministic debug-build barriers used by real-Chromium integration tests.
/// They are absent from release builds and cannot affect production behavior.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod test_support {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use tokio::sync::Notify;

    struct Barrier {
        entered: AtomicBool,
        released: AtomicBool,
        entered_notify: Notify,
        released_notify: Notify,
    }

    impl Barrier {
        fn new() -> Self {
            Self {
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                entered_notify: Notify::new(),
                released_notify: Notify::new(),
            }
        }

        async fn wait_for(flag: &AtomicBool, notify: &Notify) {
            while !flag.load(Ordering::Acquire) {
                let notified = notify.notified();
                if flag.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }

        fn enter(&self) {
            self.entered.store(true, Ordering::Release);
            self.entered_notify.notify_waiters();
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
            self.released_notify.notify_waiters();
        }
    }

    /// Debug-only handle for injecting an unrelated ref invalidation after an
    /// action has captured its baseline. This proves causal barriers do not
    /// accept an arbitrary generation bump.
    pub struct GenerationTestHandle {
        generation: Arc<std::sync::atomic::AtomicU64>,
        router_notify: Arc<Notify>,
    }

    impl GenerationTestHandle {
        pub fn advance_unrelated(&self) -> u64 {
            let value = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.router_notify.notify_waiters();
            value
        }
    }

    pub fn generation_handle(page: &super::Page) -> GenerationTestHandle {
        GenerationTestHandle {
            generation: Arc::clone(&page.generation),
            router_notify: Arc::clone(&page.router.notify),
        }
    }

    fn target_initializer_slots() -> &'static Mutex<HashMap<String, Arc<Barrier>>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Barrier>>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds exactly the next attached-target initializer before it enables or
    /// resumes that renderer. Dropping the handle always releases the renderer.
    pub struct TargetInitializerHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl TargetInitializerHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for TargetInitializerHold {
        fn drop(&mut self) {
            let mut slots = target_initializer_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_target_initializer(
        root_session_id: impl Into<String>,
    ) -> TargetInitializerHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = target_initializer_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a target initializer debug barrier is already installed for this session"
        );
        slots.insert(root_session_id.clone(), Arc::clone(&barrier));
        TargetInitializerHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_target_initializer_if_requested(root_session_id: &str) {
        let barrier = {
            let mut slots = target_initializer_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(root_session_id)
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    fn click_dispatch_slots() -> &'static Mutex<HashMap<String, Arc<Barrier>>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Barrier>>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds exactly the next click after its final actionability checks and
    /// immediately before the first trusted button press. Dropping the handle
    /// always releases the click.
    pub struct ClickDispatchHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl ClickDispatchHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for ClickDispatchHold {
        fn drop(&mut self) {
            let mut slots = click_dispatch_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_click_before_dispatch(
        root_session_id: impl Into<String>,
    ) -> ClickDispatchHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = click_dispatch_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a click dispatch debug barrier is already installed for this session"
        );
        slots.insert(root_session_id.clone(), Arc::clone(&barrier));
        ClickDispatchHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_click_before_dispatch_if_requested(root_session_id: &str) {
        let barrier = {
            let mut slots = click_dispatch_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(root_session_id)
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    fn press_dispatch_slots() -> &'static Mutex<HashMap<String, Arc<Barrier>>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Barrier>>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds the next key action after its settlement subscriber exists but
    /// before the exact pre-dispatch event floor is captured.
    pub struct PressDispatchHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl PressDispatchHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for PressDispatchHold {
        fn drop(&mut self) {
            let mut slots = press_dispatch_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_press_before_dispatch(
        root_session_id: impl Into<String>,
    ) -> PressDispatchHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = press_dispatch_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a press dispatch debug barrier is already installed for this session"
        );
        slots.insert(root_session_id.clone(), Arc::clone(&barrier));
        PressDispatchHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_press_before_dispatch_if_requested(root_session_id: &str) {
        let barrier = {
            let mut slots = press_dispatch_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(root_session_id)
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    fn history_revalidation_slots() -> &'static Mutex<HashMap<String, Arc<Barrier>>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Barrier>>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds history traversal after its initial adjacent-entry selection and
    /// before the mandatory final history/generation revalidation.
    pub struct HistoryRevalidationHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl HistoryRevalidationHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for HistoryRevalidationHold {
        fn drop(&mut self) {
            let mut slots = history_revalidation_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_history_before_revalidation(
        root_session_id: impl Into<String>,
    ) -> HistoryRevalidationHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = history_revalidation_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a history revalidation debug barrier is already installed for this session"
        );
        slots.insert(root_session_id.clone(), Arc::clone(&barrier));
        HistoryRevalidationHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_history_before_revalidation_if_requested(root_session_id: &str) {
        let barrier = {
            let mut slots = history_revalidation_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(root_session_id)
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    fn router_navigation_slots() -> &'static Mutex<HashMap<String, Arc<Barrier>>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Barrier>>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds exactly the next root navigation event after the target router has
    /// received it but before it updates the generation. Dropping the handle
    /// always releases the router.
    pub struct RouterNavigationHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl RouterNavigationHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for RouterNavigationHold {
        fn drop(&mut self) {
            let mut slots = router_navigation_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_root_navigation_in_router(
        root_session_id: impl Into<String>,
    ) -> RouterNavigationHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = router_navigation_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a router navigation debug barrier is already installed for this session"
        );
        slots.insert(root_session_id.clone(), Arc::clone(&barrier));
        RouterNavigationHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_router_navigation_if_requested(
        root_session_id: &str,
        event_session_id: Option<&str>,
        method: &str,
    ) {
        if event_session_id != Some(root_session_id)
            || !matches!(
                method,
                "Page.frameNavigated" | "Page.navigatedWithinDocument"
            )
        {
            return;
        }
        let barrier = {
            let mut slots = router_navigation_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.remove(root_session_id)
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    struct RecorderEventBarrierRequest {
        method: &'static str,
        barrier: Arc<Barrier>,
    }

    fn recorder_event_slots() -> &'static Mutex<HashMap<String, RecorderEventBarrierRequest>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, RecorderEventBarrierRequest>>> =
            OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds the next root-renderer network request after the EventLog recorder
    /// receives it but before it updates activity state. Dropping the handle
    /// always releases the recorder.
    pub struct RecorderEventHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl RecorderEventHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for RecorderEventHold {
        fn drop(&mut self) {
            let mut slots = recorder_event_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(&pending.barrier, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_network_request_in_recorder(
        root_session_id: impl Into<String>,
    ) -> RecorderEventHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = recorder_event_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "an EventLog recorder debug barrier is already installed for this session"
        );
        slots.insert(
            root_session_id.clone(),
            RecorderEventBarrierRequest {
                method: "Network.requestWillBeSent",
                barrier: Arc::clone(&barrier),
            },
        );
        RecorderEventHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_recorder_event_if_requested(
        root_session_id: &str,
        event_session_id: Option<&str>,
        method: &str,
    ) {
        if event_session_id != Some(root_session_id) {
            return;
        }
        let barrier = {
            let mut slots = recorder_event_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(root_session_id)
                .is_some_and(|pending| pending.method == method)
            {
                slots.remove(root_session_id).map(|pending| pending.barrier)
            } else {
                None
            }
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    struct StablePollBarrierRequest {
        root_session_id: String,
        after: Duration,
        barrier: Arc<Barrier>,
    }

    fn stable_poll_slots() -> &'static Mutex<HashMap<String, StablePollBarrierRequest>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, StablePollBarrierRequest>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds one stability loop immediately after it samples network activity.
    /// The caller can complete a real request while no second poll is possible.
    pub struct StablePollHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl StablePollHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for StablePollHold {
        fn drop(&mut self) {
            let mut slots = stable_poll_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(&pending.barrier, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_stable_poll_after(
        root_session_id: impl Into<String>,
        after: Duration,
    ) -> StablePollHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = stable_poll_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a stable poll debug barrier is already installed for this session"
        );
        slots.insert(
            root_session_id.clone(),
            StablePollBarrierRequest {
                root_session_id: root_session_id.clone(),
                after,
                barrier: Arc::clone(&barrier),
            },
        );
        StablePollHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_stable_poll_if_requested(root_session_id: &str, elapsed: Duration) {
        let barrier = {
            let mut slots = stable_poll_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots.get(root_session_id).is_some_and(|pending| {
                pending.root_session_id == root_session_id && elapsed >= pending.after
            }) {
                slots.remove(root_session_id).map(|pending| pending.barrier)
            } else {
                None
            }
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }

    struct NodeClipBarrierRequest {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    fn node_clip_slots() -> &'static Mutex<HashMap<String, NodeClipBarrierRequest>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, NodeClipBarrierRequest>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Holds a node clip immediately after its DOM quad was sampled and before
    /// the closing viewport sample. This forces coordinate-epoch races without
    /// adding any production timing branch.
    pub struct NodeClipHold {
        root_session_id: String,
        barrier: Arc<Barrier>,
    }

    impl NodeClipHold {
        pub async fn wait_until_entered(&self) {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
        }

        pub fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for NodeClipHold {
        fn drop(&mut self) {
            let mut slots = node_clip_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(&self.root_session_id)
                .is_some_and(|pending| Arc::ptr_eq(&pending.barrier, &self.barrier))
            {
                slots.remove(&self.root_session_id);
            }
            self.barrier.release();
        }
    }

    pub fn hold_next_node_clip_after_quads(root_session_id: impl Into<String>) -> NodeClipHold {
        let root_session_id = root_session_id.into();
        let barrier = Arc::new(Barrier::new());
        let mut slots = node_clip_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !slots.contains_key(&root_session_id),
            "a node clip debug barrier is already installed for this session"
        );
        slots.insert(
            root_session_id.clone(),
            NodeClipBarrierRequest {
                root_session_id: root_session_id.clone(),
                barrier: Arc::clone(&barrier),
            },
        );
        NodeClipHold {
            root_session_id,
            barrier,
        }
    }

    pub(crate) async fn hold_node_clip_if_requested(root_session_id: &str) {
        let barrier = {
            let mut slots = node_clip_slots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots
                .get(root_session_id)
                .is_some_and(|pending| pending.root_session_id == root_session_id)
            {
                slots.remove(root_session_id).map(|pending| pending.barrier)
            } else {
                None
            }
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error(transparent)]
    Cdp(#[from] CdpError),
    #[error(transparent)]
    Ref(#[from] RefError),
    #[error(transparent)]
    Action(#[from] input::ActionError),
    #[error("navigation to {url} failed: {reason}")]
    Navigation { url: String, reason: String },
    #[error("no snapshot has been taken yet — run `brow snapshot` first")]
    NoSnapshot,
    #[error(
        "the frame/target tree kept changing while the snapshot was captured; retry `brow snapshot`"
    )]
    SnapshotUnstable,
    #[error("{message}")]
    WaitFailure {
        message: String,
        receipt: Box<navigation::ActionReceipt>,
    },
    #[error("no adjacent history entry (current index {current_index}, {entry_count} total)")]
    NoHistoryEntry {
        current_index: i64,
        entry_count: usize,
        receipt: Box<navigation::ActionReceipt>,
    },
}

/// One page target, plus everything that must stay consistent with it.
pub struct Page {
    client: Arc<CdpClient>,
    pub target_id: String,
    pub session_id: String,
    pub frame_id: String,
    refs: tree::RefTable,
    /// Bumped by a background watcher whenever the main frame's document is
    /// replaced. Every `@node-G-N` is scoped to one value of this counter.
    generation: Arc<AtomicU64>,
    router: TargetRouter,
    /// Aborts the event-router task before its strong client reference can keep
    /// the CDP transport and writer thread alive after this page is gone.
    router_task: TargetRouterTask,
    event_task: events::RecorderTask,
    /// Monotonic within the Page lifetime so a later snapshot can never recycle
    /// an old visible ref onto a different backend node in the same generation.
    next_ref_id: u64,
    /// Isolated worlds for our own helper code, one per frame, recreated per
    /// document generation.
    worlds: HashMap<String, (u64, i64)>,
    snapshotted: bool,
    /// Console, exception and network capture for this session.
    pub events: Arc<events::EventLog>,
    /// Touch emulation is off until a touch gesture is first requested; turning it
    /// on changes how responsive sites render, so it is not a default.
    touch_enabled: bool,
    /// Per-Page random SipHash key for approval semantics. Tokens cannot be
    /// compared across sessions or used as a deterministic secret oracle.
    fingerprint_hasher: RandomState,
    /// `create` owns and closes its target; `attach` only detaches its flat CDP
    /// session. Atomic because `close(&self)` disarms the Drop fallback.
    owns_target: bool,
    cleanup_armed: AtomicBool,
}

#[derive(Default)]
struct DirectNavigationObserved {
    last_event_sequence: u64,
    committed: bool,
    loaded: bool,
    command_commit_seen: bool,
    commit_event_sequence: Option<u64>,
    commit_loader_id: Option<String>,
    final_url: Option<String>,
}

impl DirectNavigationObserved {
    fn begin_cross_document(&mut self, preserve_fresh_commit: bool) {
        if !(preserve_fresh_commit && self.committed && !self.loaded) {
            self.committed = false;
            self.commit_event_sequence = None;
            self.commit_loader_id = None;
        }
        self.loaded = false;
    }

    fn observe_loader_load(&mut self, loader_id: Option<&str>) -> bool {
        if !loader_correlated_load(
            self.committed,
            self.commit_event_sequence,
            self.commit_loader_id.as_deref(),
            loader_id,
        ) {
            return false;
        }
        self.loaded = true;
        true
    }
}

fn observe_direct_navigation_event(
    event: &CdpEventHandle,
    root_session_id: &str,
    root_frame_id: &str,
    causal_event_floor: u64,
    expected_loader_id: Option<&str>,
    observed: &mut DirectNavigationObserved,
) -> Result<(), String> {
    observed.last_event_sequence = observed.last_event_sequence.max(event.sequence());
    if event.method == EVENT_STREAM_GAP_METHOD {
        return Err("the CDP event stream reported a gap while navigating".into());
    }
    if event.sequence() <= causal_event_floor
        || event.session_id.as_deref() != Some(root_session_id)
    {
        return Ok(());
    }
    let event_frame = event
        .params
        .get("frameId")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .params
                .get("frame")
                .and_then(|frame| frame.get("id"))
                .and_then(Value::as_str)
        });
    let is_root_frame = event_frame == Some(root_frame_id);
    match event.method.as_str() {
        "Network.requestWillBeSent"
            if is_root_frame
                && event
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| kind == "Document") =>
        {
            let request_loader_id = event.params.get("loaderId").and_then(Value::as_str);
            let preserve_fresh_commit = observed.committed
                && !observed.loaded
                && request_loader_id.is_some()
                && request_loader_id == observed.commit_loader_id.as_deref();
            observed.begin_cross_document(preserve_fresh_commit);
        }
        "Page.frameRequestedNavigation" | "Page.frameScheduledNavigation" if is_root_frame => {
            observed.begin_cross_document(false);
        }
        "Page.frameStartedLoading" if is_root_frame => {
            observed.begin_cross_document(true);
        }
        "Page.lifecycleEvent"
            if is_root_frame
                && event.params.get("name").and_then(Value::as_str) == Some("init") =>
        {
            let loader_id = event.params.get("loaderId").and_then(Value::as_str);
            let preserve_fresh_commit = observed.committed
                && !observed.loaded
                && loader_id.is_some()
                && loader_id == observed.commit_loader_id.as_deref();
            observed.begin_cross_document(preserve_fresh_commit);
        }
        "Page.frameNavigated" if is_root_frame => {
            let frame = event.params.get("frame").unwrap_or(&Value::Null);
            let loader_id = frame.get("loaderId").and_then(Value::as_str);
            if !observed.command_commit_seen
                && expected_loader_id.is_some()
                && loader_id != expected_loader_id
            {
                return Ok(());
            }
            observed.command_commit_seen = true;
            observed.committed = true;
            observed.loaded = false;
            observed.commit_event_sequence = Some(event.sequence());
            observed.commit_loader_id = loader_id.map(str::to_string);
            observed.final_url = frame.get("url").and_then(Value::as_str).map(redact::url);
        }
        "Page.navigatedWithinDocument" if is_root_frame && expected_loader_id.is_none() => {
            observed.command_commit_seen = true;
            observed.committed = true;
            observed.loaded = true;
            observed.commit_event_sequence = Some(event.sequence());
            observed.commit_loader_id = None;
            observed.final_url = event
                .params
                .get("url")
                .and_then(Value::as_str)
                .map(redact::url);
        }
        "Page.lifecycleEvent"
            if is_root_frame
                && event.params.get("name").and_then(Value::as_str) == Some("load") =>
        {
            let loader_id = event.params.get("loaderId").and_then(Value::as_str);
            observed.observe_loader_load(loader_id);
        }
        "Page.javascriptDialogOpening" => {
            let kind = event
                .params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("dialog");
            let message = event
                .params
                .get("message")
                .and_then(Value::as_str)
                .map(redact::storage_text)
                .unwrap_or_default();
            return Err(format!("navigation blocked by {kind}: {message}"));
        }
        _ => {}
    }
    Ok(())
}

/// A cross-document load is authoritative only when CDP names the loader that
/// committed that same document. The legacy unqualified load/stop events can
/// arrive late from a superseded loader and are therefore never load proof.
fn loader_correlated_load(
    committed: bool,
    commit_event_sequence: Option<u64>,
    commit_loader_id: Option<&str>,
    load_loader_id: Option<&str>,
) -> bool {
    committed
        && commit_event_sequence.is_some()
        && commit_loader_id.is_some()
        && commit_loader_id == load_loader_id
}

struct AttachCleanup {
    client: Arc<CdpClient>,
    session_id: String,
    armed: bool,
}

impl AttachCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AttachCleanup {
    fn drop(&mut self) {
        if self.armed {
            spawn_page_cleanup(
                Arc::clone(&self.client),
                String::new(),
                self.session_id.clone(),
                false,
            );
        }
    }
}

struct CreatedTargetCleanup {
    client: Arc<CdpClient>,
    target_id: String,
    armed: bool,
}

impl CreatedTargetCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreatedTargetCleanup {
    fn drop(&mut self) {
        if self.armed {
            spawn_page_cleanup(
                Arc::clone(&self.client),
                self.target_id.clone(),
                String::new(),
                true,
            );
        }
    }
}

fn spawn_page_cleanup(
    client: Arc<CdpClient>,
    target_id: String,
    session_id: String,
    owns_target: bool,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            target = %target_id,
            session = %session_id,
            "page dropped outside a Tokio runtime; browser-side target cleanup could not be scheduled"
        );
        return;
    };
    runtime.spawn(async move {
        if owns_target {
            let _ = client
                .call("Target.closeTarget", json!({ "targetId": target_id }))
                .await;
        } else if !session_id.is_empty() {
            let _ = client
                .call_on(
                    &session_id,
                    "Target.setAutoAttach",
                    json!({ "autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true }),
                )
                .await;
            let _ = client
                .call(
                    "Target.detachFromTarget",
                    json!({ "sessionId": session_id }),
                )
                .await;
        }
    });
}

impl Drop for Page {
    fn drop(&mut self) {
        self.router_task.abort();
        self.event_task.abort();
        if self.cleanup_armed.swap(false, Ordering::AcqRel) {
            spawn_page_cleanup(
                Arc::clone(&self.client),
                self.target_id.clone(),
                self.session_id.clone(),
                self.owns_target,
            );
        }
    }
}

/// Something a pointer gesture can aim at.
#[derive(Debug, Clone)]
pub enum PointTarget {
    Ref(String),
    At(Point),
}

#[derive(Debug, Clone)]
struct TargetSession {
    target_id: String,
    session_id: String,
    /// Root frame of this renderer target. For an OOPIF this is normally equal
    /// to `target_id`; we keep both because the protocol exposes both identities.
    frame_id: String,
    parent_session_id: Option<String>,
    ready: bool,
    failure: Option<String>,
}

#[derive(Debug, Clone)]
struct DiscoveredTarget {
    target_id: String,
    parent_frame_id: Option<String>,
}

struct RouterData {
    root_target_id: String,
    root_session_id: String,
    sessions: HashMap<String, TargetSession>,
    session_for_target: HashMap<String, String>,
    discovered: HashMap<String, DiscoveredTarget>,
    known_frame_ids: HashSet<String>,
    stream_gap: Option<String>,
    /// True after the root frame starts a cross-document navigation and until a
    /// loader-correlated lifecycle `load` event proves that exact document.
    /// Read operations consult this so they cannot mint evidence for a known
    /// superseded document.
    root_loading: bool,
    /// Loader currently authorized to complete `root_loading`. Unqualified
    /// `loadEventFired`/`frameStoppedLoading` events are deliberately ignored.
    root_loader_id: Option<String>,
    /// Exact causal navigation sequences already consumed by this router,
    /// paired with the generation that invalidated refs for them.
    navigation_event_acks: VecDeque<(u64, u64)>,
    /// Highest transport sequence for which this router has completed its
    /// synchronous state transition.
    processed_event_sequence: u64,
}

impl RouterData {
    fn related_target_ids(&self) -> HashSet<String> {
        // Every attached session was accepted only through an already-owned
        // parent session, so attached targets are authoritative. Discovery-only
        // targets are linked through TargetInfo.parentFrameId (a frame id, not a
        // target id) and the frame inventory gathered from Page frame trees.
        let mut related: HashSet<String> = self.session_for_target.keys().cloned().collect();
        related.insert(self.root_target_id.clone());
        let mut related_frames = self.known_frame_ids.clone();
        related_frames.extend(
            self.sessions
                .values()
                .map(|session| session.frame_id.clone()),
        );
        loop {
            let before = related.len();
            for target in self.discovered.values() {
                if target
                    .parent_frame_id
                    .as_ref()
                    .is_some_and(|parent| related_frames.contains(parent))
                {
                    related.insert(target.target_id.clone());
                    // Chromium uses the OOPIF target id as that target's root
                    // frame id. This lets discovery-only nested OOPIFs chain.
                    related_frames.insert(target.target_id.clone());
                }
            }
            if related.len() == before {
                break;
            }
        }
        related
    }

    fn ready_sessions(&self) -> Vec<TargetSession> {
        let related = self.related_target_ids();
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|session| {
                session.ready
                    && related.contains(&session.target_id)
                    && self.session_for_target.get(&session.target_id) == Some(&session.session_id)
            })
            .cloned()
            .collect();
        sessions.sort_by_key(|session| {
            let mut depth = 0usize;
            let mut parent = session.parent_session_id.as_deref();
            while let Some(id) = parent {
                depth += 1;
                parent = self
                    .sessions
                    .get(id)
                    .and_then(|record| record.parent_session_id.as_deref());
                if depth > self.sessions.len() {
                    break;
                }
            }
            (
                depth,
                session.parent_session_id.clone().unwrap_or_default(),
                session.target_id.clone(),
                session.session_id.clone(),
            )
        });
        sessions
    }

    fn coverage_gaps(&self) -> Vec<CoverageGap> {
        let related = self.related_target_ids();
        let mut gaps = Vec::new();
        for target_id in related {
            if target_id == self.root_target_id {
                continue;
            }
            let session = self
                .session_for_target
                .get(&target_id)
                .and_then(|id| self.sessions.get(id));
            if session.is_some_and(|record| record.ready) {
                continue;
            }
            let reason = session
                .and_then(|record| record.failure.clone())
                .unwrap_or_else(|| {
                    "iframe target discovered but no usable CDP session is attached".into()
                });
            gaps.push(CoverageGap {
                frame_id: Some(target_id.clone()),
                target_id: Some(target_id),
                reason,
            });
        }
        if let Some(reason) = &self.stream_gap {
            gaps.push(CoverageGap {
                frame_id: None,
                target_id: None,
                reason: reason.clone(),
            });
        }
        gaps
    }
}

#[derive(Clone)]
struct TargetRouter {
    state: Arc<Mutex<RouterData>>,
    notify: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetSettlement {
    root_loading: bool,
    target_settled: bool,
}

impl TargetSettlement {
    fn allows_stability(self) -> bool {
        !self.root_loading && self.target_settled
    }
}

impl TargetRouter {
    fn new(root: TargetSession, mut known_frame_ids: HashSet<String>) -> Self {
        let root_target_id = root.target_id.clone();
        let root_session_id = root.session_id.clone();
        let mut sessions = HashMap::new();
        sessions.insert(root.session_id.clone(), root.clone());
        let mut session_for_target = HashMap::new();
        session_for_target.insert(root.target_id.clone(), root.session_id.clone());
        known_frame_ids.insert(root.frame_id.clone());
        Self {
            state: Arc::new(Mutex::new(RouterData {
                root_target_id,
                root_session_id,
                sessions,
                session_for_target,
                discovered: HashMap::new(),
                known_frame_ids,
                stream_gap: None,
                root_loading: false,
                root_loader_id: None,
                navigation_event_acks: VecDeque::new(),
                processed_event_sequence: 0,
            })),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn ready_sessions(&self) -> Vec<TargetSession> {
        self.state
            .lock()
            .expect("target router mutex")
            .ready_sessions()
    }

    fn coverage_gaps(&self) -> Vec<CoverageGap> {
        self.state
            .lock()
            .expect("target router mutex")
            .coverage_gaps()
    }

    fn settlement(&self) -> TargetSettlement {
        let state = self.state.lock().expect("target router mutex");
        TargetSettlement {
            root_loading: state.root_loading,
            target_settled: state.coverage_gaps().is_empty(),
        }
    }

    fn session(&self, session_id: &str) -> Option<TargetSession> {
        self.state
            .lock()
            .expect("target router mutex")
            .sessions
            .get(session_id)
            .cloned()
    }

    fn session_for_target(&self, target_id: &str) -> Option<TargetSession> {
        let state = self.state.lock().expect("target router mutex");
        state
            .session_for_target
            .get(target_id)
            .and_then(|session| state.sessions.get(session))
            .cloned()
    }

    fn acknowledge_navigation_event(&self, event_sequence: u64, generation: u64) {
        let mut state = self.state.lock().expect("target router mutex");
        state
            .navigation_event_acks
            .push_back((event_sequence, generation));
        while state.navigation_event_acks.len() > NAVIGATION_EVENT_ACK_CAPACITY {
            state.navigation_event_acks.pop_front();
        }
    }

    fn acknowledged_navigation_generation(&self, event_sequence: u64) -> Option<u64> {
        self.state
            .lock()
            .expect("target router mutex")
            .navigation_event_acks
            .iter()
            .rev()
            .find_map(|(seen, generation)| (*seen == event_sequence).then_some(*generation))
    }

    fn mark_event_processed(&self, event_sequence: u64) {
        let mut state = self.state.lock().expect("target router mutex");
        state.processed_event_sequence = state.processed_event_sequence.max(event_sequence);
    }

    fn processed_through(&self, event_sequence: u64) -> bool {
        self.state
            .lock()
            .expect("target router mutex")
            .processed_event_sequence
            >= event_sequence
    }
}

struct RouterEventProcessed {
    router: TargetRouter,
    event_sequence: u64,
}

impl Drop for RouterEventProcessed {
    fn drop(&mut self) {
        self.router.mark_event_processed(self.event_sequence);
        self.router.notify.notify_waiters();
    }
}

struct TargetRouterTask {
    abort: tokio::task::AbortHandle,
}

impl TargetRouterTask {
    fn abort(&self) {
        self.abort.abort();
    }
}

impl Drop for TargetRouterTask {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

struct ViewportMetrics {
    page_x: f64,
    page_y: f64,
    width: f64,
    height: f64,
}

struct SessionGeometry {
    transform: tree::ViewportTransform,
    page_x: f64,
    page_y: f64,
    /// (parent session, owner iframe backend id), for tree depth splicing.
    owner: Option<(String, i64)>,
}

impl Page {
    /// Attaches to an existing target with the flat protocol and enables the
    /// domains every other operation depends on.
    pub async fn attach(client: Arc<CdpClient>, target_id: &str) -> Result<Self, PageError> {
        let attached = client
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Protocol {
                method: "Target.attachToTarget".into(),
                code: 0,
                message: "browser attached without returning a sessionId".into(),
                data: None,
            })?
            .to_string();
        let mut attach_cleanup = AttachCleanup {
            client: Arc::clone(&client),
            session_id: session_id.clone(),
            armed: true,
        };

        client
            .call_on(&session_id, "Page.enable", json!({}))
            .await?;
        client
            .call_on(
                &session_id,
                "Page.setLifecycleEventsEnabled",
                json!({ "enabled": true }),
            )
            .await?;
        client
            .call_on(&session_id, "Runtime.enable", json!({}))
            .await?;
        client.call_on(&session_id, "DOM.enable", json!({})).await?;

        let tree = client
            .call_on(&session_id, "Page.getFrameTree", json!({}))
            .await?;
        let frame_id = tree
            .get("frameTree")
            .and_then(|t| t.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let generation = Arc::new(AtomicU64::new(1));
        let mut known_frame_ids = HashSet::new();
        collect_frame_ids(tree.get("frameTree"), &mut known_frame_ids);
        let router = TargetRouter::new(
            TargetSession {
                target_id: target_id.to_string(),
                session_id: session_id.clone(),
                frame_id: frame_id.clone(),
                parent_session_id: None,
                ready: true,
                failure: None,
            },
            known_frame_ids,
        );
        let router_task = spawn_target_router(&client, router.clone(), Arc::clone(&generation));

        // Subscribe before auto-attach/enable. Child Runtime/Log/Network domains
        // can flush buffered events synchronously during initialization.
        let (events, event_task) = events::spawn_recorder(&client, &session_id).await;

        // Discovery lets snapshots report a target that exists but failed to
        // attach. Auto-attach is session-scoped and deliberately armed on the page
        // (then recursively on every child by `spawn_target_router`).
        client
            .call("Target.setDiscoverTargets", json!({ "discover": true }))
            .await?;
        refresh_target_inventory(&client, &router).await?;
        client
            .call_on(&session_id, "Target.setAutoAttach", auto_attach_params())
            .await?;
        wait_for_target_settle(&client, &router).await;

        let page = Self {
            client,
            target_id: target_id.to_string(),
            session_id,
            frame_id,
            refs: tree::RefTable::default(),
            generation,
            router,
            router_task,
            event_task,
            next_ref_id: 0,
            worlds: HashMap::new(),
            snapshotted: false,
            events,
            touch_enabled: false,
            fingerprint_hasher: RandomState::new(),
            owns_target: false,
            cleanup_armed: AtomicBool::new(true),
        };
        attach_cleanup.disarm();
        Ok(page)
    }

    /// Opens a fresh page target and attaches to it.
    pub async fn create(client: Arc<CdpClient>, url: &str) -> Result<Self, PageError> {
        let created = client
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut created_cleanup = CreatedTargetCleanup {
            client: Arc::clone(&client),
            target_id: target_id.clone(),
            armed: true,
        };
        let mut page = Self::attach(client, &target_id).await?;
        page.owns_target = true;
        created_cleanup.disarm();
        if !url.is_empty() && url != "about:blank" {
            page.navigate(url).await?;
        }
        Ok(page)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub async fn browser_version(&self) -> Result<String, PageError> {
        let version = self.client.call("Browser.getVersion", json!({})).await?;
        Ok(version
            .get("product")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    async fn ensure_target_is_current(&self, target: &PointTarget) -> Result<(), PageError> {
        if let PointTarget::Ref(node_ref) = target {
            self.synchronize_ref_before_dispatch(node_ref).await?;
        }
        Ok(())
    }

    async fn session_geometry(
        &self,
        session: &TargetSession,
        scroll_owners: bool,
    ) -> Result<SessionGeometry, PageError> {
        let initial_metrics = viewport_metrics(&self.client, &session.session_id).await?;
        if session.session_id == self.session_id {
            return Ok(SessionGeometry {
                transform: tree::ViewportTransform::IDENTITY,
                page_x: initial_metrics.page_x,
                page_y: initial_metrics.page_y,
                owner: None,
            });
        }

        let mut current = session.clone();
        let mut transform = tree::ViewportTransform::IDENTITY;
        let mut immediate_owner = None;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.session_id.clone()) {
                return Err(PageError::Cdp(geometry_error(
                    "cycle in OOPIF parent-session chain",
                )));
            }
            let parent_id = current.parent_session_id.clone().ok_or_else(|| {
                PageError::Cdp(geometry_error(format!(
                    "iframe target {} has no parent session",
                    current.target_id
                )))
            })?;
            let parent = self.router.session(&parent_id).ok_or_else(|| {
                PageError::Cdp(geometry_error(format!(
                    "parent session {parent_id} detached while mapping iframe {}",
                    current.target_id
                )))
            })?;
            let (edge, owner_backend) = self
                .frame_edge_transform(&current, &parent, scroll_owners)
                .await?;
            if immediate_owner.is_none() {
                immediate_owner = Some((parent.session_id.clone(), owner_backend));
            }
            transform = compose_transform(edge, transform);
            if parent.session_id == self.session_id {
                break;
            }
            current = parent;
        }

        Ok(SessionGeometry {
            transform,
            page_x: initial_metrics.page_x,
            page_y: initial_metrics.page_y,
            owner: immediate_owner,
        })
    }

    async fn frame_edge_transform(
        &self,
        child: &TargetSession,
        parent: &TargetSession,
        scroll_owner: bool,
    ) -> Result<(tree::ViewportTransform, i64), PageError> {
        let child_metrics = viewport_metrics(&self.client, &child.session_id).await?;
        if child_metrics.width <= 0.0 || child_metrics.height <= 0.0 {
            return Err(PageError::Cdp(geometry_error(format!(
                "iframe {} reported a zero-sized viewport",
                child.frame_id
            ))));
        }
        let owner = self
            .client
            .call_on(
                &parent.session_id,
                "DOM.getFrameOwner",
                json!({ "frameId": child.frame_id }),
            )
            .await?;
        let backend = owner
            .get("backendNodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                PageError::Cdp(geometry_error(format!(
                    "DOM.getFrameOwner returned no owner for {}",
                    child.frame_id
                )))
            })?;
        if scroll_owner {
            let _ = self
                .client
                .call_on(
                    &parent.session_id,
                    "DOM.scrollIntoViewIfNeeded",
                    json!({ "backendNodeId": backend }),
                )
                .await;
        }
        let model = self
            .client
            .call_on(
                &parent.session_id,
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend }),
            )
            .await?;
        let quad = model
            .get("model")
            .and_then(|model| model.get("content"))
            .and_then(Value::as_array)
            .ok_or_else(|| PageError::Cdp(geometry_error("iframe owner has no content quad")))?;
        if quad.len() < 8 {
            return Err(PageError::Cdp(geometry_error(
                "iframe owner content quad is incomplete",
            )));
        }
        let mut q = [0.0; 8];
        for (slot, value) in q.iter_mut().zip(quad.iter()) {
            *slot = value.as_f64().unwrap_or(0.0);
        }
        let area = ((q[0] * q[3] - q[2] * q[1])
            + (q[2] * q[5] - q[4] * q[3])
            + (q[4] * q[7] - q[6] * q[5])
            + (q[6] * q[1] - q[0] * q[7]))
            .abs()
            / 2.0;
        if area <= 1.0 {
            return Err(PageError::Cdp(geometry_error(format!(
                "iframe {} has a zero-area owner quad",
                child.frame_id
            ))));
        }
        if !quad_is_affine(&q, 1.0) {
            return Err(PageError::Cdp(geometry_error(format!(
                "iframe {} has a perspective/non-affine transform; refusing inaccurate coordinates",
                child.frame_id
            ))));
        }
        let transform = tree::ViewportTransform {
            xx: (q[2] - q[0]) / child_metrics.width,
            yx: (q[3] - q[1]) / child_metrics.width,
            xy: (q[6] - q[0]) / child_metrics.height,
            yy: (q[7] - q[1]) / child_metrics.height,
            tx: q[0],
            ty: q[1],
        };
        Ok((transform, backend))
    }

    async fn to_root_viewport(
        &self,
        identity: &NodeIdentity,
        mut point: Point,
        scroll_owners: bool,
        verify_ancestor_hit: bool,
    ) -> Result<Point, PageError> {
        let mut current = self.router.session(&identity.session_id).ok_or_else(|| {
            PageError::Ref(RefError::Stale {
                node_ref: "node".into(),
                had: identity.generation,
                now: self.generation(),
            })
        })?;
        let mut seen = HashSet::new();
        while current.session_id != self.session_id {
            if !seen.insert(current.session_id.clone()) {
                return Err(PageError::Cdp(geometry_error(
                    "cycle in OOPIF parent-session chain",
                )));
            }
            let parent_id = current.parent_session_id.clone().ok_or_else(|| {
                PageError::Cdp(geometry_error(format!(
                    "iframe target {} has no parent session",
                    current.target_id
                )))
            })?;
            let parent = self.router.session(&parent_id).ok_or_else(|| {
                PageError::Cdp(geometry_error(format!(
                    "parent session {parent_id} detached while mapping iframe {}",
                    current.target_id
                )))
            })?;
            let (edge, owner_backend) = self
                .frame_edge_transform(&current, &parent, scroll_owners)
                .await?;
            let (x, y) = edge.point(point.x, point.y);
            point = Point { x, y };

            if verify_ancestor_hit {
                let clear = input::node_at_point(&self.client, &parent.session_id, point)
                    .await?
                    .is_some_and(|(backend, frame_id)| {
                        backend == owner_backend || frame_id == current.frame_id
                    });
                if !clear {
                    return Err(PageError::Action(input::ActionError::Occluded {
                        x: point.x,
                        y: point.y,
                    }));
                }
            }
            current = parent;
        }
        Ok(point)
    }

    async fn ensure_local_actionable(
        &mut self,
        entry: &tree::RefEntry,
        point: Point,
    ) -> Result<(), PageError> {
        // The compositor's answer settles the common case and cannot be spoofed
        // by the page. When it names a nested/ancestor node, an isolated-world
        // containment check distinguishes a legitimate child icon from an
        // unrelated overlay.
        let hit = input::node_at_point(&self.client, &entry.identity.session_id, point).await?;
        let clear = match hit {
            Some((hit_backend, _)) if hit_backend == entry.identity.backend_node_id => true,
            Some((hit_backend, hit_frame)) => {
                let world = self
                    .helper_world(&entry.identity.session_id, &hit_frame)
                    .await
                    .ok();
                !input::covered_by_foreign_element(
                    &self.client,
                    &entry.identity.session_id,
                    entry.identity.backend_node_id,
                    hit_backend,
                    world,
                )
                .await?
            }
            None => false,
        };
        if !clear {
            return Err(PageError::Action(input::ActionError::Occluded {
                x: point.x,
                y: point.y,
            }));
        }
        Ok(())
    }

    async fn node_root_document_clip(
        &self,
        entry: &tree::RefEntry,
    ) -> Result<capture::Clip, PageError> {
        let _ = self
            .client
            .call_on(
                &entry.identity.session_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": entry.identity.backend_node_id }),
            )
            .await;
        let session = self
            .router
            .session(&entry.identity.session_id)
            .ok_or_else(|| {
                PageError::Ref(RefError::Stale {
                    node_ref: "node".into(),
                    had: entry.identity.generation,
                    now: self.generation(),
                })
            })?;
        for _ in 0..3 {
            // Wheel scrolling is asynchronous. If it lands between the quad and
            // viewport samples, adding the newer page offset to an older quad
            // mixes coordinate epochs (for example 500 + 600 => 1100). Only
            // accept a clip bracketed by one unchanged root viewport offset.
            let root_before = viewport_metrics(&self.client, &self.session_id).await?;
            let quads = self
                .client
                .call_on(
                    &entry.identity.session_id,
                    "DOM.getContentQuads",
                    json!({ "backendNodeId": entry.identity.backend_node_id }),
                )
                .await?;
            #[cfg(debug_assertions)]
            test_support::hold_node_clip_if_requested(&self.session_id).await;
            let list = quads
                .get("quads")
                .and_then(Value::as_array)
                .ok_or(PageError::Action(input::ActionError::NotRendered))?;
            let geometry = self.session_geometry(&session, true).await?;
            let mut min_x = f64::INFINITY;
            let mut min_y = f64::INFINITY;
            let mut max_x = f64::NEG_INFINITY;
            let mut max_y = f64::NEG_INFINITY;
            let mut seen = false;
            for quad in list {
                let Some(values) = quad.as_array() else {
                    continue;
                };
                for pair in values.chunks_exact(2).take(4) {
                    let (Some(x), Some(y)) = (pair[0].as_f64(), pair[1].as_f64()) else {
                        continue;
                    };
                    let (x, y) = geometry.transform.point(x, y);
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                    seen = true;
                }
            }
            if !seen || max_x <= min_x || max_y <= min_y {
                return Err(PageError::Action(input::ActionError::NotRendered));
            }
            let root_after = viewport_metrics(&self.client, &self.session_id).await?;
            if (root_before.page_x - root_after.page_x).abs() <= 0.5
                && (root_before.page_y - root_after.page_y).abs() <= 0.5
            {
                return Ok(capture::Clip {
                    x: min_x + root_after.page_x,
                    y: min_y + root_after.page_y,
                    width: max_x - min_x,
                    height: max_y - min_y,
                });
            }
            tokio::task::yield_now().await;
        }
        Err(PageError::Action(input::ActionError::Unstable))
    }

    /// Navigates and waits for a causal root-document commit and root load.
    pub async fn navigate(&mut self, url: &str) -> Result<(), PageError> {
        self.navigate_and_observe(url, NAV_TIMEOUT).await.map(drop)
    }

    /// Same contract as [`Page::navigate`], with a caller-supplied shared
    /// deadline. The returned URL is the redacted location observed at the
    /// exact settled event prefix, not an echo of the requested URL.
    pub async fn navigate_and_observe(
        &mut self,
        url: &str,
        timeout: Duration,
    ) -> Result<String, PageError> {
        let requested_url = redact::url(url);
        let fail = |reason: String| PageError::Navigation {
            url: requested_url.clone(),
            reason,
        };
        let deadline = tokio::time::Instant::now() + timeout;

        // Publication and subscription share one gate, so no CDP event can
        // land between these two facts. First synchronize the context prefix;
        // it must never be mistaken for a consequence of Page.navigate.
        let (mut events, subscription_watermark) = self.client.subscribe_with_watermark();
        if subscription_watermark != 0
            && !wait_for_router_processed_until(&self.router, subscription_watermark, deadline)
                .await
        {
            return Err(fail(format!(
                "target router did not process the pre-navigation event prefix through sequence {subscription_watermark}"
            )));
        }
        if subscription_watermark != 0
            && !self
                .events
                .wait_processed_until(subscription_watermark, deadline)
                .await
        {
            return Err(fail(format!(
                "event recorder did not process the pre-navigation event prefix through sequence {subscription_watermark}"
            )));
        }

        // This is the last synchronous boundary before the command is queued.
        // Events at or below it are context even though the receiver has them.
        let causal_event_floor = self.client.latest_published_event_sequence();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(fail("navigation deadline expired before dispatch".into()));
        }
        let response = self
            .client
            .call_on_timeout(
                &self.session_id,
                "Page.navigate",
                json!({ "url": url }),
                remaining,
            )
            .await
            .map_err(|error| fail(format!("Page.navigate dispatch failed: {error}")))?;
        if let Some(error) = response.get("errorText").and_then(Value::as_str) {
            return Err(fail(redact::storage_text(error)));
        }
        if response
            .get("isDownload")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(fail(
                "Page.navigate started a download instead of a document navigation".into(),
            ));
        }
        if response
            .get("frameId")
            .and_then(Value::as_str)
            .is_some_and(|frame| frame != self.frame_id)
        {
            return Err(fail(
                "Page.navigate acknowledged a non-root frame unexpectedly".into(),
            ));
        }
        let expected_loader_id = response
            .get("loaderId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut observed = DirectNavigationObserved {
            last_event_sequence: subscription_watermark,
            ..DirectNavigationObserved::default()
        };

        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(fail(format!(
                    "timed out waiting for a causal root commit and root load (commit={}, load={})",
                    observed.committed, observed.loaded
                )));
            }

            if observed.loaded {
                // Synchronize this observer, the ref-invalidating target router,
                // and the durable EventLog to one exact published prefix.
                let through = self.client.latest_published_event_sequence();
                while observed.last_event_sequence < through {
                    let event = match tokio::time::timeout_at(deadline, events.recv()).await {
                        Ok(Ok(event)) => event,
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                            return Err(fail(format!(
                                "navigation observer lagged and missed {skipped} event(s)"
                            )));
                        }
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                            return Err(fail(
                                "CDP event stream closed while synchronizing navigation".into(),
                            ));
                        }
                        Err(_) => {
                            return Err(fail(format!(
                                "timed out synchronizing navigation evidence through sequence {through}"
                            )));
                        }
                    };
                    observe_direct_navigation_event(
                        &event,
                        &self.session_id,
                        &self.frame_id,
                        causal_event_floor,
                        expected_loader_id.as_deref(),
                        &mut observed,
                    )
                    .map_err(&fail)?;
                }
                if !observed.loaded {
                    continue;
                }
                if through != 0
                    && !wait_for_router_processed_until(&self.router, through, deadline).await
                {
                    return Err(fail(format!(
                        "target router did not process navigation evidence through sequence {through}"
                    )));
                }
                if through != 0 && !self.events.wait_processed_until(through, deadline).await {
                    return Err(fail(format!(
                        "event recorder did not process navigation evidence through sequence {through}"
                    )));
                }
                let Some(commit_sequence) = observed.commit_event_sequence else {
                    return Err(fail(
                        "root load had no matching causal root commit sequence".into(),
                    ));
                };
                if wait_for_navigation_ack_until(&self.router, commit_sequence, deadline)
                    .await
                    .is_none()
                {
                    return Err(fail(format!(
                        "target router did not acknowledge the causal root commit at sequence {commit_sequence}"
                    )));
                }
                match tokio::time::timeout_at(
                    deadline,
                    refresh_target_inventory(&self.client, &self.router),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        return Err(fail(format!("target inventory refresh failed: {error}")));
                    }
                    Err(_) => {
                        return Err(fail("target inventory refresh timed out".into()));
                    }
                }
                wait_for_target_settle_until(&self.client, &self.router, deadline).await;
                let settlement = self.router.settlement();
                if !settlement.target_settled {
                    return Err(fail(
                        "one or more related frame targets did not settle before the navigation deadline"
                            .into(),
                    ));
                }
                if settlement.root_loading {
                    return Err(fail(
                        "root target remained loading after the observed lifecycle prefix".into(),
                    ));
                }
                let remaining = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(EVALUATE_DEADLINE);
                if remaining.is_zero() {
                    return Err(fail(
                        "navigation deadline expired before final URL proof".into(),
                    ));
                }
                let final_url = self
                    .location_with_timeout(remaining)
                    .await
                    .map(|location| location.0)
                    .map_err(|error| fail(format!("final URL could not be observed: {error}")))?;
                if self.client.latest_published_event_sequence() != through
                    || !self.router.processed_through(through)
                    || !self.events.processed_through(through)
                {
                    continue;
                }
                self.worlds.clear();
                self.snapshotted = false;
                return Ok(redact::url(&final_url));
            }

            let event = match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                    return Err(fail(format!(
                        "navigation observer lagged and missed {skipped} event(s)"
                    )));
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err(fail("CDP event stream closed while navigating".into()));
                }
                Err(_) => {
                    return Err(fail(format!(
                        "timed out waiting for a causal root commit and root load (commit={}, load={})",
                        observed.committed, observed.loaded
                    )));
                }
            };
            observe_direct_navigation_event(
                &event,
                &self.session_id,
                &self.frame_id,
                causal_event_floor,
                expected_loader_id.as_deref(),
                &mut observed,
            )
            .map_err(&fail)?;
        }
    }

    /// Captures the unified page tree and mints a fresh set of refs.
    pub async fn snapshot(&mut self) -> Result<Snapshot, PageError> {
        if !wait_for_latest_published_router_state(
            &self.client,
            &self.router,
            tokio::time::Instant::now() + NAV_TIMEOUT,
        )
        .await
        {
            return Err(PageError::SnapshotUnstable);
        }
        if !wait_for_pending_root_navigation(&self.router).await {
            return Err(PageError::SnapshotUnstable);
        }
        for _attempt in 0..3 {
            refresh_target_inventory(&self.client, &self.router).await?;
            wait_for_target_settle(&self.client, &self.router).await;
            if !wait_for_latest_published_router_state(
                &self.client,
                &self.router,
                tokio::time::Instant::now() + TARGET_SETTLE_TIMEOUT,
            )
            .await
            {
                continue;
            }

            let generation = self.generation();
            let sessions = self.router.ready_sessions();
            let mut fresh_refs = self.refs.next_snapshot(generation);
            let mut counter = self.next_ref_id;
            let mut nodes = Vec::new();
            let mut fragment_owners = HashMap::new();
            let mut gaps = self.router.coverage_gaps();
            let mut external_frames = Vec::new();
            let mut captured_targets = HashSet::new();
            let mut root_url = String::new();
            let mut root_title = String::new();

            for session in sessions {
                let geometry = match self.session_geometry(&session, false).await {
                    Ok(geometry) => geometry,
                    Err(error) if session.session_id != self.session_id => {
                        gaps.push(CoverageGap {
                            frame_id: Some(session.frame_id.clone()),
                            target_id: Some(session.target_id.clone()),
                            reason: format!("could not map iframe geometry: {error}"),
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let depth_offset = geometry
                    .owner
                    .as_ref()
                    .and_then(|(owner_session, owner_backend)| {
                        nodes.iter().find(|node: &&Node| {
                            node.session_id == *owner_session
                                && node.backend_node_id == *owner_backend
                        })
                    })
                    .map(|node| node.depth + 1)
                    .unwrap_or(0);
                if let Some((owner_session, owner_backend)) = &geometry.owner {
                    let owner = nodes.iter().find(|node: &&Node| {
                        node.session_id == *owner_session && node.backend_node_id == *owner_backend
                    });
                    if !owner.is_some_and(|node| {
                        node.visible
                            && node
                                .bounds
                                .is_some_and(|bounds| bounds.width > 1.0 && bounds.height > 1.0)
                    }) {
                        gaps.push(CoverageGap {
                            frame_id: Some(session.frame_id.clone()),
                            target_id: Some(session.target_id.clone()),
                            reason: "iframe owner is hidden, degenerate, or absent from the parent capture"
                                .into(),
                        });
                        continue;
                    }
                }
                let context = tree::CaptureContext {
                    target_id: &session.target_id,
                    session_id: &session.session_id,
                    root_frame_id: &session.frame_id,
                    generation,
                    transform: geometry.transform,
                    page_x: geometry.page_x,
                    page_y: geometry.page_y,
                    depth_offset,
                };
                let captured = match tree::capture_target(
                    &self.client,
                    &context,
                    &mut fresh_refs,
                    &mut counter,
                )
                .await
                {
                    Ok(captured) => captured,
                    Err(error) if session.session_id != self.session_id => {
                        gaps.push(CoverageGap {
                            frame_id: Some(session.frame_id.clone()),
                            target_id: Some(session.target_id.clone()),
                            reason: format!("attached iframe capture failed: {error}"),
                        });
                        continue;
                    }
                    Err(error) => return Err(PageError::Cdp(error)),
                };
                if session.session_id == self.session_id {
                    root_url = captured.url.clone();
                    root_title = captured.title.clone();
                }
                fragment_owners.insert(session.session_id.clone(), geometry.owner.clone());
                for warning in &captured.warnings {
                    gaps.push(CoverageGap {
                        frame_id: Some(session.frame_id.clone()),
                        target_id: Some(session.target_id.clone()),
                        reason: format!("target capture degraded: {warning}"),
                    });
                }
                captured_targets.insert(session.target_id.clone());
                external_frames.extend(
                    captured.external_frames.into_iter().map(|frame| {
                        (session.target_id.clone(), session.session_id.clone(), frame)
                    }),
                );
                nodes.extend(captured.nodes);
            }

            // Each target capture is preorder internally. Splice each OOPIF
            // fragment immediately after its owner iframe so sibling frame
            // content follows DOM owner order rather than HashMap/session order.
            nodes = splice_target_fragments(nodes, &fragment_owners, &self.session_id);

            // DOM.getDocument exposes an OOPIF owner with a frame id but no
            // contentDocument. The frame-id == target-id join is the last line of
            // defence against silently missing an attach event or failed session.
            for (parent_target, _parent_session, frame) in external_frames {
                if !captured_targets.contains(&frame.frame_id) {
                    let detail = self.router.session_for_target(&frame.frame_id);
                    gaps.push(CoverageGap {
                        frame_id: Some(frame.frame_id.clone()),
                        target_id: Some(frame.frame_id.clone()),
                        reason: detail
                            .and_then(|session| session.failure)
                            .unwrap_or_else(|| {
                                format!(
                                    "frame owner {} in target {parent_target} has no captured OOPIF session",
                                    frame.owner_backend_node_id
                                )
                            }),
                    });
                }
            }

            dedup_coverage_gaps(&mut gaps);
            if !wait_for_latest_published_router_state(
                &self.client,
                &self.router,
                tokio::time::Instant::now() + TARGET_SETTLE_TIMEOUT,
            )
            .await
            {
                continue;
            }
            if self.generation() != generation {
                continue;
            }

            fresh_refs.prune_absent_identities();
            self.refs = fresh_refs;
            self.next_ref_id = counter;
            self.snapshotted = true;
            return Ok(Snapshot {
                generation,
                url: root_url,
                title: root_title,
                nodes,
                coverage_gaps: gaps,
            });
        }
        Err(PageError::SnapshotUnstable)
    }

    /// Resolves a ref, distinguishing the three ways it can go wrong.
    ///
    /// The order matters: a ref minted before a navigation must report *stale*
    /// (re-snapshot and continue), not *unknown* (you made that up) and not *no
    /// snapshot* (you skipped a step). Each needs different advice.
    fn resolve(&self, node_ref: &str) -> Result<tree::RefEntry, PageError> {
        match self.refs.resolve(node_ref, self.generation()) {
            Ok(entry) => {
                let live = self.router.session(&entry.identity.session_id);
                if live.is_some_and(|session| {
                    session.ready && session.target_id == entry.identity.target_id
                }) {
                    Ok(entry)
                } else {
                    let now = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
                    Err(PageError::Ref(RefError::Stale {
                        node_ref: node_ref.to_string(),
                        had: entry.identity.generation,
                        now,
                    }))
                }
            }
            Err(RefError::Unknown(r)) if !self.snapshotted => {
                let _ = r;
                Err(PageError::NoSnapshot)
            }
            Err(e) => Err(PageError::Ref(e)),
        }
    }

    /// Makes every already-published generation invalidation visible, then
    /// re-resolves the ref at the last trusted-input boundary. The cursor check
    /// repeats if publication advances while the router catches up.
    async fn synchronize_ref_before_dispatch(&self, node_ref: &str) -> Result<(), PageError> {
        let deadline = tokio::time::Instant::now() + TARGET_SETTLE_TIMEOUT;
        loop {
            let through = self.client.latest_published_event_sequence();
            if through != 0
                && !wait_for_router_processed_until(&self.router, through, deadline).await
            {
                return Err(PageError::Action(input::ActionError::Unstable));
            }
            self.resolve(node_ref)?;
            if self.client.latest_published_event_sequence() == through
                && self.router.processed_through(through)
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PageError::Action(input::ActionError::Unstable));
            }
        }
    }

    /// Re-reads the node's semantic identity through its owning renderer
    /// session. Callers can compare the whole value across an approval wait: a
    /// renderer swap, navigation, changed tag/role, or changed accessible label
    /// all make the fingerprint unequal.
    pub async fn node_fingerprint(&mut self, node_ref: &str) -> Result<NodeFingerprint, PageError> {
        self.synchronize_ref_before_dispatch(node_ref).await?;
        let entry = self.resolve(node_ref)?;
        let generation = entry.identity.generation;
        if self.generation() != generation {
            return Err(PageError::Ref(RefError::Stale {
                node_ref: node_ref.to_string(),
                had: generation,
                now: self.generation(),
            }));
        }
        let world = self
            .helper_world(&entry.identity.session_id, &entry.identity.frame_id)
            .await?;
        let describe = self.client.call_on(
            &entry.identity.session_id,
            "DOM.describeNode",
            json!({ "backendNodeId": entry.identity.backend_node_id, "depth": 0 }),
        );
        let accessibility = self.client.call_on(
            &entry.identity.session_id,
            "Accessibility.getPartialAXTree",
            json!({
                "backendNodeId": entry.identity.backend_node_id,
                "fetchRelatives": false,
            }),
        );
        let resolve = self.client.call_on(
            &entry.identity.session_id,
            "DOM.resolveNode",
            json!({
                "backendNodeId": entry.identity.backend_node_id,
                "executionContextId": world,
            }),
        );
        let (describe, accessibility, resolved) =
            tokio::try_join!(describe, accessibility, resolve)?;
        self.synchronize_ref_before_dispatch(node_ref).await?;
        if self.generation() != generation {
            return Err(PageError::Ref(RefError::Stale {
                node_ref: node_ref.to_string(),
                had: entry.identity.generation,
                now: self.generation(),
            }));
        }
        let tag = describe
            .get("node")
            .and_then(|node| node.get("nodeName"))
            .and_then(Value::as_str)
            .unwrap_or(&entry.tag)
            .to_ascii_lowercase();
        let semantic = accessibility
            .get("nodes")
            .and_then(Value::as_array)
            .and_then(|nodes| {
                nodes.iter().find(|node| {
                    node.get("backendDOMNodeId").and_then(Value::as_i64)
                        == Some(entry.identity.backend_node_id)
                })
            });
        let role = semantic
            .and_then(|node| node.get("role"))
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let accessible_label = semantic
            .and_then(|node| node.get("name"))
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let mut action_attributes = BTreeMap::new();
        let mut raw_action_attributes = BTreeMap::new();
        let attributes = describe
            .get("node")
            .and_then(|node| node.get("attributes"))
            .and_then(Value::as_array);
        if let Some(attributes) = attributes {
            for pair in attributes.chunks_exact(2) {
                let (Some(key), Some(value)) = (pair[0].as_str(), pair[1].as_str()) else {
                    continue;
                };
                if matches!(
                    key.to_ascii_lowercase().as_str(),
                    "href"
                        | "action"
                        | "formaction"
                        | "formmethod"
                        | "type"
                        | "disabled"
                        | "aria-disabled"
                        | "target"
                        | "download"
                        | "rel"
                ) {
                    raw_action_attributes.insert(key.to_ascii_lowercase(), value.to_string());
                    let value = if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "href" | "action" | "formaction"
                    ) {
                        crate::redact::url(value)
                    } else {
                        value.to_string()
                    };
                    action_attributes.insert(key.to_ascii_lowercase(), value);
                }
            }
        }
        let object_id = resolved
            .get("object")
            .and_then(|object| object.get("objectId"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PageError::Cdp(CdpError::Protocol {
                    method: "DOM.resolveNode".into(),
                    code: 0,
                    message: "approval target could not be resolved in its isolated world".into(),
                    data: None,
                })
            })?;
        let effective = self
            .client
            .call_on(
                &entry.identity.session_id,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "returnByValue": true,
                    "functionDeclaration": "function() {\
                        const form = this.form || null;\
                        return {\
                            href: typeof this.href === 'string' ? this.href : '',\
                            formAction: typeof this.formAction === 'string' ? this.formAction : '',\
                            formMethod: typeof this.formMethod === 'string' ? this.formMethod : '',\
                            type: typeof this.type === 'string' ? this.type : '',\
                            disabled: this.disabled === true,\
                            formActionEffective: form && typeof form.action === 'string' ? form.action : '',\
                            formMethodEffective: form && typeof form.method === 'string' ? form.method : '',\
                            formTargetEffective: form && typeof form.target === 'string' ? form.target : ''\
                        };\
                    }",
                }),
            )
            .await?;
        let effective = effective
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let canonical = serde_json::to_vec(&json!({
            "attributes": raw_action_attributes,
            "effective": effective,
        }))
        .map_err(|error| {
            PageError::Cdp(CdpError::Decode {
                method: "node fingerprint".into(),
                source: Arc::new(error),
            })
        })?;
        let mut semantics_hasher = self.fingerprint_hasher.build_hasher();
        semantics_hasher.write(&canonical);
        let action_semantics_token = tree::ActionSemanticsToken::new(semantics_hasher.finish());
        Ok(NodeFingerprint {
            identity: entry.identity,
            tag,
            role,
            accessible_label,
            action_attributes,
            action_semantics_token,
        })
    }

    /// An isolated world in `frame_id`, so a hostile or merely eccentric page
    /// cannot observe or patch what we run.
    ///
    /// Keyed by frame as well as generation: a node inside an iframe has to be
    /// resolved in *that* frame's world, and resolving it in the main frame's
    /// world simply fails — which is how the iframe click bug got in.
    async fn helper_world(&mut self, session_id: &str, frame_id: &str) -> Result<i64, PageError> {
        let gen = self.generation();
        let key = format!("{session_id}\0{frame_id}");
        if let Some((cached_gen, ctx)) = self.worlds.get(&key) {
            if *cached_gen == gen {
                return Ok(*ctx);
            }
        }
        let res = self
            .client
            .call_on(
                session_id,
                "Page.createIsolatedWorld",
                // `grantUniveralAccess` — the typo is in the protocol itself.
                json!({
                    "frameId": frame_id,
                    "worldName": "__brow",
                    "grantUniveralAccess": true,
                }),
            )
            .await?;
        let ctx = res
            .get("executionContextId")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        self.worlds.insert(key, (gen, ctx));
        Ok(ctx)
    }

    /// Clicks a node after checking it is actually clickable.
    pub async fn click(
        &mut self,
        node_ref: &str,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        force: bool,
    ) -> Result<Point, PageError> {
        let mut dispatch = input::DispatchTracker::default();
        self.click_impl(
            node_ref,
            button,
            click_count,
            modifiers,
            force,
            None,
            &mut dispatch,
        )
        .await
    }

    // Keeping the complete trusted-input contract at one boundary is safer than
    // splitting phase state from actionability/revalidation parameters.
    #[allow(clippy::too_many_arguments)]
    async fn click_impl(
        &mut self,
        node_ref: &str,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        force: bool,
        expected: Option<&NodeFingerprint>,
        dispatch: &mut input::DispatchTracker,
    ) -> Result<Point, PageError> {
        let entry = self.resolve(node_ref)?;
        let local = input::prepare_target(
            &self.client,
            &entry.identity.session_id,
            entry.identity.backend_node_id,
        )
        .await?;

        if !force {
            self.ensure_local_actionable(&entry, local).await?;
        }

        // DOM geometry/hit testing is renderer-local. Trusted input, however,
        // must be injected through the top-level page session after mapping the
        // point through every owner iframe.
        let point = self
            .to_root_viewport(&entry.identity, local, true, !force)
            .await?;
        // Mapping and ancestor hit tests contain awaits. A child can navigate or
        // detach during them, so re-read the generation at the last possible
        // point before trusted input is dispatched.
        self.resolve(node_ref)?;
        dispatch.note_event_floor(self.client.latest_published_event_sequence());
        input::move_pointer_preparatory(&self.client, &self.session_id, point, modifiers, dispatch)
            .await?;

        // Pointer movement itself runs page hover handlers. Recompute geometry
        // and both renderer-local and ancestor hit tests after those handlers,
        // then press without another mouseMoved event in between.
        if let Some(expected) = expected {
            // Fingerprinting crosses several CDP round trips. Do it before the
            // final compositor checks so an overlay that appears while semantic
            // revalidation is running is still caught before mousePressed.
            if &self.node_fingerprint(node_ref).await? != expected {
                return Err(PageError::Action(input::ActionError::TargetChanged));
            }
        }
        let entry_after_fingerprint = self.resolve(node_ref)?;
        let local_after_move = input::prepare_target(
            &self.client,
            &entry_after_fingerprint.identity.session_id,
            entry_after_fingerprint.identity.backend_node_id,
        )
        .await?;
        if !force {
            self.ensure_local_actionable(&entry_after_fingerprint, local_after_move)
                .await?;
        }
        let point_after_move = self
            .to_root_viewport(
                &entry_after_fingerprint.identity,
                local_after_move,
                true,
                !force,
            )
            .await?;
        if (point_after_move.x - point.x).abs() > 1.0 || (point_after_move.y - point.y).abs() > 1.0
        {
            return Err(PageError::Action(input::ActionError::Unstable));
        }
        let mut final_point = point_after_move;
        for ordinal in 1..=click_count {
            #[cfg(debug_assertions)]
            if ordinal == 1 {
                test_support::hold_click_before_dispatch_if_requested(&self.session_id).await;
            }
            if let Some(expected) = expected {
                self.synchronize_ref_before_dispatch(node_ref).await?;
                if &self.node_fingerprint(node_ref).await? != expected {
                    return Err(PageError::Action(input::ActionError::TargetChanged));
                }
            }
            if ordinal > 1 {
                let current = self.resolve(node_ref)?;
                let local = input::prepare_target(
                    &self.client,
                    &current.identity.session_id,
                    current.identity.backend_node_id,
                )
                .await?;
                if !force {
                    self.ensure_local_actionable(&current, local).await?;
                }
                let remapped = self
                    .to_root_viewport(&current.identity, local, true, !force)
                    .await?;
                if (remapped.x - final_point.x).abs() > 1.0
                    || (remapped.y - final_point.y).abs() > 1.0
                {
                    return Err(PageError::Action(input::ActionError::Unstable));
                }
                self.synchronize_ref_before_dispatch(node_ref).await?;
                if let Some(expected) = expected {
                    if &self.node_fingerprint(node_ref).await? != expected {
                        return Err(PageError::Action(input::ActionError::TargetChanged));
                    }
                }
                final_point = remapped;
            }
            // Fingerprinting and repeat-click geometry checks both await CDP.
            // Close their stale-ref window at the actual mousePressed boundary.
            self.synchronize_ref_before_dispatch(node_ref).await?;
            input::click_once_at_tracked(
                &self.client,
                &self.session_id,
                final_point,
                button,
                ordinal,
                modifiers,
                dispatch,
            )
            .await?;
        }
        Ok(final_point)
    }

    pub async fn click_at(
        &self,
        point: Point,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
    ) -> Result<(), PageError> {
        let mut dispatch = input::DispatchTracker::default();
        self.click_at_tracked(point, button, click_count, modifiers, &mut dispatch)
            .await?;
        Ok(())
    }

    async fn click_at_tracked(
        &self,
        point: Point,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        dispatch: &mut input::DispatchTracker,
    ) -> Result<(), PageError> {
        dispatch.note_event_floor(self.client.latest_published_event_sequence());
        input::move_pointer_preparatory(&self.client, &self.session_id, point, modifiers, dispatch)
            .await?;
        for ordinal in 1..=click_count {
            input::click_once_at_tracked(
                &self.client,
                &self.session_id,
                point,
                button,
                ordinal,
                modifiers,
                dispatch,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn hover(&mut self, node_ref: &str) -> Result<Point, PageError> {
        let entry = self.resolve(node_ref)?;
        let local = input::prepare_target(
            &self.client,
            &entry.identity.session_id,
            entry.identity.backend_node_id,
        )
        .await?;
        self.ensure_local_actionable(&entry, local).await?;
        let point = self
            .to_root_viewport(&entry.identity, local, true, true)
            .await?;
        self.synchronize_ref_before_dispatch(node_ref).await?;
        input::hover_at(&self.client, &self.session_id, point, 0).await?;
        Ok(point)
    }

    /// Focuses a field, clears it, and types `text`.
    pub async fn fill(&mut self, node_ref: &str, text: &str) -> Result<(), PageError> {
        // Click first: focusing alone leaves some component libraries in a state
        // where they never open their dropdown or attach their input handler.
        self.click(node_ref, MouseButton::Left, 1, 0, false).await?;
        self.synchronize_ref_before_dispatch(node_ref).await?;
        let entry = self.resolve(node_ref)?;
        self.client
            .call_on(
                &entry.identity.session_id,
                "DOM.focus",
                json!({ "backendNodeId": entry.identity.backend_node_id }),
            )
            .await?;
        self.synchronize_ref_before_dispatch(node_ref).await?;
        // Select-all then insert replaces the value without assuming the field was
        // empty, and without an assignment that a controlled React input would
        // overwrite on its next render.
        input::select_all(&self.client, &self.session_id).await?;
        if text.is_empty() {
            input::press_key(&self.client, &self.session_id, "Delete").await?;
        } else {
            input::insert_text(&self.client, &self.session_id, text).await?;
        }
        Ok(())
    }

    pub async fn press(&self, chord: &str) -> Result<(), PageError> {
        input::press_key(&self.client, &self.session_id, chord).await?;
        Ok(())
    }

    async fn press_tracked(
        &self,
        chord: &str,
        dispatch: &mut input::DispatchTracker,
    ) -> Result<(), PageError> {
        input::press_key_tracked(&self.client, &self.session_id, chord, dispatch).await?;
        Ok(())
    }

    pub async fn type_text(&self, text: &str, by_key: bool) -> Result<(), PageError> {
        if by_key {
            input::type_text_by_key(
                &self.client,
                &self.session_id,
                text,
                Duration::from_millis(12),
            )
            .await?;
        } else {
            input::insert_text(&self.client, &self.session_id, text).await?;
        }
        Ok(())
    }

    pub async fn scroll(&self, dx: f64, dy: f64) -> Result<(), PageError> {
        // Scroll from the viewport centre so the gesture lands on the main
        // scroller rather than whatever happens to be at the origin.
        let metrics = self
            .client
            .call_on(&self.session_id, "Page.getLayoutMetrics", json!({}))
            .await?;
        let vw = metrics
            .get("cssVisualViewport")
            .and_then(|v| v.get("clientWidth"))
            .and_then(Value::as_f64)
            .unwrap_or(800.0);
        let vh = metrics
            .get("cssVisualViewport")
            .and_then(|v| v.get("clientHeight"))
            .and_then(Value::as_f64)
            .unwrap_or(600.0);
        input::wheel_at(
            &self.client,
            &self.session_id,
            Point {
                x: vw / 2.0,
                y: vh / 2.0,
            },
            dx,
            dy,
        )
        .await?;
        Ok(())
    }

    /// Resolves a gesture target to a viewport point.
    ///
    /// A ref goes through the full actionability check; explicit coordinates are
    /// taken at face value, because the caller asking for a raw point has already
    /// said they know better than our hit test.
    pub async fn point_of(&mut self, target: &PointTarget) -> Result<Point, PageError> {
        match target {
            PointTarget::At(p) => Ok(*p),
            PointTarget::Ref(node_ref) => {
                let entry = self.resolve(node_ref)?;
                let local = input::prepare_target(
                    &self.client,
                    &entry.identity.session_id,
                    entry.identity.backend_node_id,
                )
                .await?;
                self.ensure_local_actionable(&entry, local).await?;
                self.to_root_viewport(&entry.identity, local, true, true)
                    .await
            }
        }
    }

    async fn ensure_touch(&mut self) -> Result<(), PageError> {
        if !self.touch_enabled {
            input::enable_touch(&self.client, &self.session_id, true).await?;
            self.touch_enabled = true;
        }
        Ok(())
    }

    async fn ensure_touch_tracked(
        &mut self,
        dispatch: &mut input::DispatchTracker,
    ) -> Result<(), PageError> {
        if !self.touch_enabled {
            if let Err(error) = input::enable_touch(&self.client, &self.session_id, true).await {
                dispatch.preparatory_error(&error);
                return Err(PageError::Cdp(error));
            }
            self.touch_enabled = true;
        }
        Ok(())
    }

    pub async fn tap(&mut self, target: &PointTarget) -> Result<Point, PageError> {
        let mut dispatch = input::DispatchTracker::default();
        self.tap_tracked(target, &mut dispatch).await
    }

    async fn tap_tracked(
        &mut self,
        target: &PointTarget,
        dispatch: &mut input::DispatchTracker,
    ) -> Result<Point, PageError> {
        self.ensure_touch_tracked(dispatch).await?;
        let point = self.point_of(target).await?;
        self.ensure_target_is_current(target).await?;
        dispatch.note_event_floor(self.client.latest_published_event_sequence());
        input::tap_tracked(&self.client, &self.session_id, point, dispatch).await?;
        Ok(point)
    }

    pub async fn long_press(
        &mut self,
        target: &PointTarget,
        duration: Duration,
    ) -> Result<Point, PageError> {
        self.ensure_touch().await?;
        let point = self.point_of(target).await?;
        self.ensure_target_is_current(target).await?;
        input::long_press(&self.client, &self.session_id, point, duration).await?;
        Ok(point)
    }

    pub async fn swipe(
        &mut self,
        from: &PointTarget,
        to: &PointTarget,
        duration: Duration,
        steps: u32,
    ) -> Result<(Point, Point), PageError> {
        self.ensure_touch().await?;
        let a = self.point_of(from).await?;
        let b = self.point_of(to).await?;
        self.ensure_target_is_current(from).await?;
        self.ensure_target_is_current(to).await?;
        input::swipe(&self.client, &self.session_id, a, b, duration, steps).await?;
        Ok((a, b))
    }

    pub async fn pinch(
        &mut self,
        center: &PointTarget,
        scale: f64,
        speed: Option<i64>,
    ) -> Result<Point, PageError> {
        self.ensure_touch().await?;
        let point = self.point_of(center).await?;
        self.ensure_target_is_current(center).await?;
        input::pinch(&self.client, &self.session_id, point, scale, speed).await?;
        Ok(point)
    }

    pub async fn drag(
        &mut self,
        from: &PointTarget,
        to: &PointTarget,
        duration: Duration,
        steps: u32,
    ) -> Result<(Point, Point), PageError> {
        let initial_a = self.point_of(from).await?;
        let _initial_b = self.point_of(to).await?;
        self.ensure_target_is_current(from).await?;
        self.ensure_target_is_current(to).await?;

        // `mouseMoved` can install an overlay or replace the drag source. Move
        // first, then recompute the destination and make the source's compositor
        // hit test the final awaited gate before mousePressed.
        input::move_pointer(&self.client, &self.session_id, initial_a, 0).await?;
        let b = self.point_of(to).await?;
        let a = self.point_of(from).await?;
        if (a.x - initial_a.x).abs() > 1.0 || (a.y - initial_a.y).abs() > 1.0 {
            return Err(PageError::Action(input::ActionError::Unstable));
        }
        self.ensure_target_is_current(from).await?;
        self.ensure_target_is_current(to).await?;
        input::drag_after_move(&self.client, &self.session_id, a, b, duration, steps).await?;
        Ok((a, b))
    }

    pub async fn screenshot(
        &mut self,
        region: ScreenshotTarget,
        format: ImageFormat,
        quality: Option<i64>,
    ) -> Result<Capture, PageError> {
        if !wait_for_latest_published_router_state(
            &self.client,
            &self.router,
            tokio::time::Instant::now() + TARGET_SETTLE_TIMEOUT,
        )
        .await
        {
            return Err(PageError::SnapshotUnstable);
        }
        if !wait_for_pending_root_navigation(&self.router).await {
            return Err(PageError::SnapshotUnstable);
        }
        let region = match region {
            ScreenshotTarget::Viewport => Region::Viewport,
            ScreenshotTarget::FullPage => Region::FullPage,
            ScreenshotTarget::Node(node_ref) => {
                let entry = self.resolve(&node_ref)?;
                Region::Rect(self.node_root_document_clip(&entry).await?)
            }
            ScreenshotTarget::Rect(clip) => Region::Rect(clip),
        };
        Ok(capture::capture(&self.client, &self.session_id, region, format, quality).await?)
    }

    /// Evaluates an expression.
    ///
    /// `read_only` sets `throwOnSideEffect`, which V8 enforces itself: the
    /// expression is aborted the moment it tries to mutate anything. Verified
    /// 2026-08-04 — `document.title = 'x'` under this flag raises
    /// `EvalError: Possible side-effect in debug-evaluate` and the title is
    /// unchanged. This is a real guarantee, not a convention.
    ///
    /// Read-only evaluation deliberately runs in the **main** world: seeing the
    /// application's own globals is the entire point of inspection, and the
    /// side-effect guard is what makes that safe.
    ///
    /// The guard is *sound but conservative*, and callers need to know it. Measured
    /// on Chrome 2026-08-04:
    ///
    /// | expression                                     | read-only |
    /// |------------------------------------------------|-----------|
    /// | `document.querySelector('#x').textContent`      | allowed   |
    /// | `document.getElementById('x').textContent`      | REFUSED   |
    /// | `el.getBoundingClientRect().width`              | REFUSED   |
    /// | `window.appState`, `arr.map(...)`, `innerHTML`  | allowed   |
    ///
    /// Nothing that mutates ever slips through, but harmless reads do get refused,
    /// so the error explains the workaround instead of just saying "no".
    pub async fn evaluate(&self, expression: &str, read_only: bool) -> Result<Value, PageError> {
        self.evaluate_with_timeout(expression, read_only, EVALUATE_DEADLINE)
            .await
    }

    /// Evaluation bounded by a caller-owned deadline. Navigation waits use this
    /// so an individual location/ready-state probe can never outlive the shared
    /// wait deadline.
    async fn evaluate_with_timeout(
        &self,
        expression: &str,
        read_only: bool,
        timeout: Duration,
    ) -> Result<Value, PageError> {
        if timeout.is_zero() {
            return Err(PageError::Cdp(CdpError::Timeout {
                method: "Runtime.evaluate".into(),
                timeout,
            }));
        }
        let res = self
            .client
            .call_on_timeout(
                &self.session_id,
                "Runtime.evaluate",
                runtime_evaluate_params_with_timeout(
                    expression,
                    read_only,
                    EVALUATE_V8_TIMEOUT.min(timeout),
                ),
                timeout,
            )
            .await?;

        if let Some(details) = res.get("exceptionDetails") {
            let text = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation threw");
            let hint = if read_only && text.contains("side-effect") {
                "\nRead-only evaluation refused this. V8's side-effect check is \
                 conservative, so this is either a real mutation or a false positive on \
                 a harmless read — `getElementById` and `getBoundingClientRect` are \
                 refused even though they change nothing, while `querySelector` is not. \
                 Try rewriting with `querySelector`, or re-run with `--mutate` if the \
                 change is intended."
            } else {
                ""
            };
            return Err(PageError::Cdp(CdpError::Protocol {
                method: "Runtime.evaluate".into(),
                code: 0,
                message: format!("{text}{hint}"),
                data: None,
            }));
        }

        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Current document URL and title, cheaply.
    pub async fn location(&self) -> Result<(String, String), PageError> {
        self.location_with_timeout(EVALUATE_DEADLINE).await
    }

    async fn location_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(String, String), PageError> {
        let v = self
            .evaluate_with_timeout("[location.href, document.title]", true, timeout)
            .await?;
        let url = v
            .get(0)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let title = v
            .get(1)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok((url, title))
    }

    pub async fn close(&self) -> Result<(), PageError> {
        self.router_task.abort();
        self.event_task.abort();
        let result = self
            .client
            .call("Target.closeTarget", json!({ "targetId": self.target_id }))
            .await;
        if result.is_ok() {
            self.cleanup_armed.store(false, Ordering::Release);
        }
        result.map(|_| ()).map_err(PageError::from)
    }
}

#[cfg(test)]
fn runtime_evaluate_params(expression: &str, read_only: bool) -> Value {
    runtime_evaluate_params_with_timeout(expression, read_only, EVALUATE_V8_TIMEOUT)
}

fn runtime_evaluate_params_with_timeout(
    expression: &str,
    read_only: bool,
    v8_timeout: Duration,
) -> Value {
    json!({
        "expression": expression,
        "returnByValue": true,
        "awaitPromise": !read_only,
        "throwOnSideEffect": read_only,
        "userGesture": !read_only,
        // Chrome rejects a zero Runtime.TimeDelta. The transport timeout above
        // remains the authoritative wall-clock bound.
        "timeout": v8_timeout.as_millis().max(1) as u64,
    })
}

/// What `brow screenshot` was asked for, before refs are resolved.
#[derive(Debug, Clone)]
pub enum ScreenshotTarget {
    Viewport,
    FullPage,
    Node(String),
    Rect(capture::Clip),
}

fn auto_attach_params() -> Value {
    json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": true,
        "flatten": true,
        "filter": [
            { "type": "iframe", "exclude": false },
            { "exclude": true }
        ]
    })
}

fn discovered_target(value: &Value) -> Option<DiscoveredTarget> {
    if value.get("type").and_then(Value::as_str) != Some("iframe") {
        return None;
    }
    Some(DiscoveredTarget {
        target_id: value.get("targetId")?.as_str()?.to_string(),
        parent_frame_id: value
            .get("parentFrameId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

async fn refresh_target_inventory(
    client: &CdpClient,
    router: &TargetRouter,
) -> Result<(), CdpError> {
    let targets = client.call("Target.getTargets", json!({})).await?;
    let discovered = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(discovered_target)
        .map(|target| (target.target_id.clone(), target))
        .collect();
    router.state.lock().expect("target router mutex").discovered = discovered;
    router.notify.notify_waiters();
    Ok(())
}

async fn wait_for_target_settle(client: &CdpClient, router: &TargetRouter) {
    wait_for_target_settle_until(
        client,
        router,
        tokio::time::Instant::now() + TARGET_SETTLE_TIMEOUT,
    )
    .await;
}

async fn wait_for_target_settle_until(
    client: &CdpClient,
    router: &TargetRouter,
    deadline: tokio::time::Instant,
) {
    loop {
        let settled = {
            let state = router.state.lock().expect("target router mutex");
            state.related_target_ids().into_iter().all(|target_id| {
                target_id == state.root_target_id
                    || state
                        .session_for_target
                        .get(&target_id)
                        .and_then(|session| state.sessions.get(session))
                        .is_some_and(|session| session.ready || session.failure.is_some())
            })
        };
        if settled || tokio::time::Instant::now() >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        let _ = tokio::time::timeout(remaining, refresh_target_inventory(client, router)).await;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        tokio::select! {
            _ = router.notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(20).min(remaining)) => {}
        }
    }
}

/// Waits until the target router has invalidated refs for an already-observed
/// causal navigation. The action observer and target router are independent
/// broadcast consumers, so seeing `Page.frameNavigated` in one does not imply
/// that the other has advanced `generation` yet.
async fn wait_for_navigation_ack_until(
    router: &TargetRouter,
    event_sequence: u64,
    deadline: tokio::time::Instant,
) -> Option<u64> {
    loop {
        // Register before checking the predicate so `notify_waiters` cannot land
        // between the check and the await. `enable` performs the registration
        // eagerly rather than on the future's first poll.
        let notified = router.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(generation) = router.acknowledged_navigation_generation(event_sequence) {
            return Some(generation);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return router.acknowledged_navigation_generation(event_sequence);
        }
    }
}

async fn wait_for_router_processed_until(
    router: &TargetRouter,
    event_sequence: u64,
    deadline: tokio::time::Instant,
) -> bool {
    loop {
        let notified = router.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if router.processed_through(event_sequence) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return router.processed_through(event_sequence);
        }
    }
}

async fn wait_for_latest_published_router_state(
    client: &CdpClient,
    router: &TargetRouter,
    deadline: tokio::time::Instant,
) -> bool {
    let published = client.latest_published_event_sequence();
    published == 0 || wait_for_router_processed_until(router, published, deadline).await
}

async fn wait_for_pending_root_navigation(router: &TargetRouter) -> bool {
    let deadline = tokio::time::Instant::now() + NAV_TIMEOUT;
    loop {
        let (loading, incomplete) = {
            let state = router.state.lock().expect("target router mutex");
            (state.root_loading, state.stream_gap.is_some())
        };
        if incomplete {
            return false;
        }
        if !loading {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::select! {
            _ = router.notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

fn root_document_request(params: &Value) -> bool {
    params
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "Document")
}

/// Applies only root-frame events. Cross-document completion is loader exact:
/// neither a late global `loadEventFired`, nor a late root
/// `frameStoppedLoading`, nor a same-document URL change can settle a loader
/// that is still active.
fn update_root_loading_state(state: &mut RouterData, method: &str, params: &Value) -> bool {
    let before_loading = state.root_loading;
    let before_loader = state.root_loader_id.clone();
    match method {
        "Network.requestWillBeSent" if root_document_request(params) => {
            state.root_loading = true;
            state.root_loader_id = params
                .get("loaderId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        "Page.frameRequestedNavigation" | "Page.frameScheduledNavigation" => {
            state.root_loading = true;
            state.root_loader_id = None;
        }
        "Page.frameStartedLoading" => {
            if !state.root_loading {
                state.root_loader_id = None;
            }
            state.root_loading = true;
        }
        "Page.frameNavigated" => {
            state.root_loading = true;
            state.root_loader_id = params
                .get("frame")
                .and_then(|frame| frame.get("loaderId"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        "Page.lifecycleEvent"
            if params.get("name").and_then(Value::as_str) == Some("init") =>
        {
            state.root_loading = true;
            state.root_loader_id = params
                .get("loaderId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        "Page.lifecycleEvent"
            if params.get("name").and_then(Value::as_str) == Some("load") =>
        {
            let load_loader_id = params.get("loaderId").and_then(Value::as_str);
            if state.root_loading
                && state.root_loader_id.is_some()
                && state.root_loader_id.as_deref() == load_loader_id
            {
                state.root_loading = false;
            }
        }
        // Same-document navigation does not start a new document load and must
        // not clear an already-active cross-document load.
        "Page.navigatedWithinDocument" if state.root_loader_id.is_none() => {
            state.root_loading = false;
        }
        "Page.navigatedWithinDocument"
        // These events carry no loader identity and can arrive late.
        | "Page.loadEventFired"
        | "Page.frameStoppedLoading" => {}
        _ => {}
    }
    before_loading != state.root_loading || before_loader != state.root_loader_id
}

/// Owns the recursive flat-session target graph. Auto-attach is not transitive,
/// so the first operation on every new iframe session is to arm it again before
/// allowing the renderer to run.
fn spawn_target_router(
    client: &Arc<CdpClient>,
    router: TargetRouter,
    generation: Arc<AtomicU64>,
) -> TargetRouterTask {
    let (mut events, processed_before_subscription) = client.subscribe_with_watermark();
    router.mark_event_processed(processed_before_subscription);
    let client = Arc::clone(client);
    let task = tokio::spawn(async move {
        let mut initializers = tokio::task::JoinSet::new();
        loop {
            while initializers.try_join_next().is_some() {}
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    router
                        .state
                        .lock()
                        .expect("target router mutex")
                        .stream_gap = Some(format!(
                        "CDP target event stream lagged by {skipped} events; frame coverage may be incomplete"
                    ));
                    generation.fetch_add(1, Ordering::SeqCst);
                    router.notify.notify_waiters();
                    continue;
                }
                Err(_) => break,
            };
            #[cfg(debug_assertions)]
            {
                let root_session_id = router
                    .state
                    .lock()
                    .expect("target router mutex")
                    .root_session_id
                    .clone();
                test_support::hold_router_navigation_if_requested(
                    &root_session_id,
                    ev.session_id.as_deref(),
                    &ev.method,
                )
                .await;
            }
            let _processed = RouterEventProcessed {
                router: router.clone(),
                event_sequence: ev.sequence(),
            };
            let lifecycle_changed = {
                let mut state = router.state.lock().expect("target router mutex");
                if ev.session_id.as_deref() != Some(state.root_session_id.as_str()) {
                    false
                } else {
                    let root_frame = state
                        .sessions
                        .get(&state.root_session_id)
                        .map(|session| session.frame_id.as_str());
                    let event_frame =
                        ev.params
                            .get("frameId")
                            .and_then(Value::as_str)
                            .or_else(|| {
                                ev.params
                                    .get("frame")
                                    .and_then(|frame| frame.get("id"))
                                    .and_then(Value::as_str)
                            });
                    let is_root = event_frame.is_none_or(|frame| root_frame == Some(frame));
                    is_root && update_root_loading_state(&mut state, &ev.method, &ev.params)
                }
            };
            if lifecycle_changed {
                router.notify.notify_waiters();
            }
            match ev.method.as_str() {
                EVENT_STREAM_GAP_METHOD => {
                    let dropped = ev
                        .params
                        .get("droppedTotal")
                        .and_then(Value::as_u64)
                        .unwrap_or(1);
                    let source = ev
                        .params
                        .get("sourceMethod")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown event");
                    router
                        .state
                        .lock()
                        .expect("target router mutex")
                        .stream_gap = Some(format!(
                        "CDP retained-event budget dropped event #{dropped} ({source}); frame coverage may be incomplete"
                    ));
                    generation.fetch_add(1, Ordering::SeqCst);
                    router.notify.notify_waiters();
                }
                "Target.attachedToTarget" => {
                    let Some(parent_session_id) = ev.session_id.clone() else {
                        continue;
                    };
                    let Some(target) = ev.params.get("targetInfo").and_then(discovered_target)
                    else {
                        continue;
                    };
                    let Some(session_id) = ev
                        .params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    let waiting = ev
                        .params
                        .get("waitingForDebugger")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let accepted_previous = {
                        let mut state = router.state.lock().expect("target router mutex");
                        register_attached_session(
                            &mut state,
                            &parent_session_id,
                            target,
                            &session_id,
                        )
                    };
                    let Some(previous_session) = accepted_previous else {
                        let rejected_client = Arc::clone(&client);
                        initializers.spawn(async move {
                            discard_attached_session(&rejected_client, &session_id, waiting).await;
                        });
                        continue;
                    };
                    if let Some(previous) = previous_session {
                        let stale_client = Arc::clone(&client);
                        initializers.spawn(async move {
                            discard_attached_session(&stale_client, &previous, false).await;
                        });
                    }
                    generation.fetch_add(1, Ordering::SeqCst);
                    router.notify.notify_waiters();
                    let init_client = Arc::clone(&client);
                    let init_router = router.clone();
                    initializers.spawn(async move {
                        initialize_attached_target(
                            &init_client,
                            &init_router,
                            &session_id,
                            waiting,
                        )
                        .await;
                    });
                }
                "Target.detachedFromTarget" => {
                    let Some(session_id) = ev.params.get("sessionId").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if remove_session_subtree(&router, session_id) {
                        generation.fetch_add(1, Ordering::SeqCst);
                        router.notify.notify_waiters();
                    }
                }
                "Target.targetCreated" | "Target.targetInfoChanged" => {
                    let Some(target) = ev.params.get("targetInfo").and_then(discovered_target)
                    else {
                        continue;
                    };
                    let related = {
                        let mut state = router.state.lock().expect("target router mutex");
                        state
                            .discovered
                            .insert(target.target_id.clone(), target.clone());
                        state.related_target_ids().contains(&target.target_id)
                    };
                    if related {
                        generation.fetch_add(1, Ordering::SeqCst);
                        router.notify.notify_waiters();
                    }
                }
                "Target.targetDestroyed" | "Target.targetCrashed" => {
                    let Some(target_id) = ev
                        .params
                        .get("targetId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    let session_id = {
                        let mut state = router.state.lock().expect("target router mutex");
                        state.discovered.remove(&target_id);
                        state.session_for_target.get(&target_id).cloned()
                    };
                    if let Some(session_id) = session_id {
                        remove_session_subtree(&router, &session_id);
                        generation.fetch_add(1, Ordering::SeqCst);
                        router.notify.notify_waiters();
                    }
                }
                "Page.frameNavigated"
                | "Page.navigatedWithinDocument"
                | "Page.frameAttached"
                | "Page.frameDetached"
                | "Inspector.targetCrashed" => {
                    let owned = ev.session_id.as_deref().is_some_and(|session_id| {
                        router
                            .state
                            .lock()
                            .expect("target router mutex")
                            .sessions
                            .contains_key(session_id)
                    });
                    if owned {
                        if let Some(frame_id) = ev
                            .params
                            .get("frameId")
                            .and_then(Value::as_str)
                            .or_else(|| {
                                ev.params
                                    .get("frame")
                                    .and_then(|frame| frame.get("id"))
                                    .and_then(Value::as_str)
                            })
                        {
                            router
                                .state
                                .lock()
                                .expect("target router mutex")
                                .known_frame_ids
                                .insert(frame_id.to_string());
                        }
                        let value = generation.fetch_add(1, Ordering::SeqCst) + 1;
                        if matches!(
                            ev.method.as_str(),
                            "Page.frameNavigated" | "Page.navigatedWithinDocument"
                        ) {
                            router.acknowledge_navigation_event(ev.sequence(), value);
                        }
                        tracing::debug!(generation = value, event = %ev.method, "refs invalidated");
                        router.notify.notify_waiters();
                    }
                }
                _ => {}
            }
        }
    });
    TargetRouterTask {
        abort: task.abort_handle(),
    }
}

async fn initialize_attached_target(
    client: &CdpClient,
    router: &TargetRouter,
    session_id: &str,
    waiting_for_debugger: bool,
) {
    #[cfg(debug_assertions)]
    {
        let root_session_id = router
            .state
            .lock()
            .expect("target router mutex")
            .root_session_id
            .clone();
        test_support::hold_target_initializer_if_requested(&root_session_id).await;
    }

    let result: Result<String, CdpError> = async {
        client
            .call_on(session_id, "Target.setAutoAttach", auto_attach_params())
            .await?;
        client.call_on(session_id, "Page.enable", json!({})).await?;
        client
            .call_on(
                session_id,
                "Page.setLifecycleEventsEnabled",
                json!({ "enabled": true }),
            )
            .await?;
        client
            .call_on(session_id, "Runtime.enable", json!({}))
            .await?;
        client.call_on(session_id, "DOM.enable", json!({})).await?;
        client
            .call_on(session_id, "Accessibility.enable", json!({}))
            .await?;
        client.call_on(session_id, "Log.enable", json!({})).await?;
        client
            .call_on(
                session_id,
                "Network.enable",
                json!({
                    "maxTotalBufferSize": 16 * 1024 * 1024,
                    "maxResourceBufferSize": 4 * 1024 * 1024,
                }),
            )
            .await?;
        let tree = client
            .call_on(session_id, "Page.getFrameTree", json!({}))
            .await?;
        {
            let mut state = router.state.lock().expect("target router mutex");
            collect_frame_ids(tree.get("frameTree"), &mut state.known_frame_ids);
        }
        Ok(tree
            .get("frameTree")
            .and_then(|tree| tree.get("frame"))
            .and_then(|frame| frame.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
    .await;

    let resume = if waiting_for_debugger {
        client
            .call_on(session_id, "Runtime.runIfWaitingForDebugger", json!({}))
            .await
            .map(|_| ())
    } else {
        Ok(())
    };
    let result = match (result, resume) {
        (Ok(frame_id), Ok(())) => Ok(frame_id),
        (Err(init), Ok(())) => Err(init),
        (Ok(_), Err(resume)) => Err(resume),
        (Err(init), Err(resume)) => Err(CdpError::Protocol {
            method: "Target initialization".into(),
            code: 0,
            message: format!(
                "target initialization failed ({init}); renderer resume also failed ({resume})"
            ),
            data: None,
        }),
    };

    let mut state = router.state.lock().expect("target router mutex");
    let Some(session) = state.sessions.get_mut(session_id) else {
        return;
    };
    match result {
        Ok(frame_id) => {
            if !frame_id.is_empty() {
                session.frame_id = frame_id;
            }
            session.ready = true;
            session.failure = None;
        }
        Err(error) => {
            session.ready = false;
            session.failure = Some(format!(
                "failed to initialize attached iframe session: {error}"
            ));
        }
    }
    drop(state);
    router.notify.notify_waiters();
}

fn collect_frame_ids(frame_tree: Option<&Value>, out: &mut HashSet<String>) {
    let Some(frame_tree) = frame_tree else {
        return;
    };
    if let Some(frame_id) = frame_tree
        .get("frame")
        .and_then(|frame| frame.get("id"))
        .and_then(Value::as_str)
    {
        out.insert(frame_id.to_string());
    }
    if let Some(children) = frame_tree.get("childFrames").and_then(Value::as_array) {
        for child in children {
            collect_frame_ids(Some(child), out);
        }
    }
}

fn remove_session_subtree(router: &TargetRouter, session_id: &str) -> bool {
    let mut state = router.state.lock().expect("target router mutex");
    remove_session_subtree_locked(&mut state, session_id)
}

/// Atomically validates the recursive parent and installs a canonical child.
///
/// `None` rejects an event from a stale/unowned parent. `Some(previous)` accepts
/// it and returns the superseded session, if any, for browser-side cleanup.
fn register_attached_session(
    state: &mut RouterData,
    parent_session_id: &str,
    target: DiscoveredTarget,
    session_id: &str,
) -> Option<Option<String>> {
    if !state.sessions.contains_key(parent_session_id) {
        return None;
    }
    let previous = state
        .session_for_target
        .get(&target.target_id)
        .filter(|previous| previous.as_str() != session_id)
        .cloned();
    if let Some(previous) = previous.as_deref() {
        remove_session_subtree_locked(state, previous);
    }
    state
        .discovered
        .insert(target.target_id.clone(), target.clone());
    state
        .session_for_target
        .insert(target.target_id.clone(), session_id.to_string());
    state.sessions.insert(
        session_id.to_string(),
        TargetSession {
            target_id: target.target_id.clone(),
            session_id: session_id.to_string(),
            frame_id: target.target_id,
            parent_session_id: Some(parent_session_id.to_string()),
            ready: false,
            failure: None,
        },
    );
    Some(previous)
}

fn remove_session_subtree_locked(state: &mut RouterData, session_id: &str) -> bool {
    if !state.sessions.contains_key(session_id) || session_id == state.root_session_id {
        return false;
    }
    let mut remove = HashSet::from([session_id.to_string()]);
    loop {
        let before = remove.len();
        for session in state.sessions.values() {
            if session
                .parent_session_id
                .as_ref()
                .is_some_and(|parent| remove.contains(parent))
            {
                remove.insert(session.session_id.clone());
            }
        }
        if remove.len() == before {
            break;
        }
    }
    for id in &remove {
        if let Some(session) = state.sessions.remove(id) {
            if state.session_for_target.get(&session.target_id) == Some(id) {
                state.session_for_target.remove(&session.target_id);
            }
        }
    }
    true
}

async fn discard_attached_session(client: &CdpClient, session_id: &str, waiting: bool) {
    if waiting {
        let _ = client
            .call_on(session_id, "Runtime.runIfWaitingForDebugger", json!({}))
            .await;
    }
    let _ = client
        .call(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;
}

async fn viewport_metrics(
    client: &CdpClient,
    session_id: &str,
) -> Result<ViewportMetrics, CdpError> {
    let metrics = client
        .call_on(session_id, "Page.getLayoutMetrics", json!({}))
        .await?;
    let visual = metrics
        .get("cssVisualViewport")
        .or_else(|| metrics.get("visualViewport"));
    let layout = metrics
        .get("cssLayoutViewport")
        .or_else(|| metrics.get("layoutViewport"));
    let number = |object: Option<&Value>, field: &str| {
        object
            .and_then(|value| value.get(field))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    // On an OOPIF session Page.getLayoutMetrics may report the top-level layout
    // viewport even though DOM quads are child-local. The Window inner size is
    // the renderer's actual frame viewport and is the denominator verified by
    // the owner-quad transform.
    let inner = client
        .call_on(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": "[window.innerWidth, window.innerHeight, window.scrollX, window.scrollY]",
                "returnByValue": true,
                "throwOnSideEffect": true,
                "timeout": EVALUATE_V8_TIMEOUT.as_millis() as u64,
            }),
        )
        .await
        .ok();
    let inner_values = inner
        .as_ref()
        .and_then(|value| value.get("result"))
        .and_then(|result| result.get("value"))
        .and_then(Value::as_array);
    let inner_width = inner_values
        .and_then(|values| values.first())
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let inner_height = inner_values
        .and_then(|values| values.get(1))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let scroll_x = inner_values
        .and_then(|values| values.get(2))
        .and_then(Value::as_f64);
    let scroll_y = inner_values
        .and_then(|values| values.get(3))
        .and_then(Value::as_f64);
    Ok(ViewportMetrics {
        page_x: scroll_x.unwrap_or_else(|| number(visual, "pageX")),
        page_y: scroll_y.unwrap_or_else(|| number(visual, "pageY")),
        width: if inner_width > 0.0 {
            inner_width
        } else {
            number(layout, "clientWidth").max(number(visual, "clientWidth"))
        },
        height: if inner_height > 0.0 {
            inner_height
        } else {
            number(layout, "clientHeight").max(number(visual, "clientHeight"))
        },
    })
}

fn compose_transform(
    outer: tree::ViewportTransform,
    inner: tree::ViewportTransform,
) -> tree::ViewportTransform {
    tree::ViewportTransform {
        xx: outer.xx * inner.xx + outer.xy * inner.yx,
        xy: outer.xx * inner.xy + outer.xy * inner.yy,
        yx: outer.yx * inner.xx + outer.yy * inner.yx,
        yy: outer.yx * inner.xy + outer.yy * inner.yy,
        tx: outer.tx + outer.xx * inner.tx + outer.xy * inner.ty,
        ty: outer.ty + outer.yx * inner.tx + outer.yy * inner.ty,
    }
}

fn quad_is_affine(q: &[f64; 8], epsilon: f64) -> bool {
    let top = (q[2] - q[0], q[3] - q[1]);
    let bottom = (q[4] - q[6], q[5] - q[7]);
    let left = (q[6] - q[0], q[7] - q[1]);
    let right = (q[4] - q[2], q[5] - q[3]);
    (top.0 - bottom.0).abs() <= epsilon
        && (top.1 - bottom.1).abs() <= epsilon
        && (left.0 - right.0).abs() <= epsilon
        && (left.1 - right.1).abs() <= epsilon
}

fn geometry_error(message: impl Into<String>) -> CdpError {
    CdpError::Protocol {
        method: "OOPIF.geometry".into(),
        code: 0,
        message: message.into(),
        data: None,
    }
}

fn dedup_coverage_gaps(gaps: &mut Vec<CoverageGap>) {
    let mut seen = HashSet::new();
    gaps.retain(|gap| {
        seen.insert((
            gap.frame_id.clone(),
            gap.target_id.clone(),
            gap.reason.clone(),
        ))
    });
}

fn splice_target_fragments(
    nodes: Vec<Node>,
    owners: &HashMap<String, Option<(String, i64)>>,
    root_session_id: &str,
) -> Vec<Node> {
    let mut fragments: HashMap<String, Vec<Node>> = HashMap::new();
    for node in nodes {
        fragments
            .entry(node.session_id.clone())
            .or_default()
            .push(node);
    }

    let mut children: HashMap<(String, i64), Vec<String>> = HashMap::new();
    for (session, owner) in owners {
        if let Some((parent_session, owner_backend)) = owner {
            children
                .entry((parent_session.clone(), *owner_backend))
                .or_default()
                .push(session.clone());
        }
    }
    for sessions in children.values_mut() {
        sessions.sort();
        sessions.dedup();
    }

    fn emit(
        session_id: &str,
        fragments: &mut HashMap<String, Vec<Node>>,
        children: &mut HashMap<(String, i64), Vec<String>>,
        out: &mut Vec<Node>,
    ) {
        let Some(fragment) = fragments.remove(session_id) else {
            return;
        };
        for node in fragment {
            let owner_key = (session_id.to_string(), node.backend_node_id);
            out.push(node);
            if let Some(child_sessions) = children.remove(&owner_key) {
                for child in child_sessions {
                    emit(&child, fragments, children, out);
                }
            }
        }
    }

    let mut out = Vec::new();
    emit(root_session_id, &mut fragments, &mut children, &mut out);

    // Coverage-gap paths should normally prevent leftovers. Keep any fragment
    // deterministically rather than dropping observable nodes if an owner edge
    // disappeared during capture.
    let mut leftovers: Vec<_> = fragments.into_iter().collect();
    leftovers.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, fragment) in leftovers {
        out.extend(fragment);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;

    use super::*;

    fn test_session(target: &str, session: &str, parent: Option<&str>) -> TargetSession {
        TargetSession {
            target_id: target.into(),
            session_id: session.into(),
            frame_id: target.into(),
            parent_session_id: parent.map(str::to_string),
            ready: true,
            failure: None,
        }
    }

    fn test_node(session: &str, backend: i64, tag: &str, depth: usize) -> Node {
        Node {
            node_ref: format!("@node-1-{backend}"),
            backend_node_id: backend,
            target_id: format!("{session}-target"),
            session_id: session.into(),
            frame_id: Some(format!("{session}-frame")),
            tag: tag.into(),
            role: None,
            name: None,
            text: None,
            attrs: BTreeMap::new(),
            bounds: None,
            pointer_eligible: true,
            visible: true,
            disabled: false,
            interactive: false,
            depth,
            in_shadow: false,
            shadow_root_type: None,
        }
    }

    #[test]
    fn oopif_fragments_follow_their_dom_owner_order() {
        let nodes = vec![
            test_node("root", 1, "before", 1),
            test_node("root", 2, "iframe-a", 1),
            test_node("root", 3, "between", 1),
            test_node("root", 4, "iframe-b", 1),
            test_node("root", 5, "after", 1),
            // Deliberately reverse session capture order.
            test_node("child-b", 20, "child-b", 2),
            test_node("child-a", 10, "child-a", 2),
        ];
        let owners = HashMap::from([
            ("root".into(), None),
            ("child-a".into(), Some(("root".into(), 2))),
            ("child-b".into(), Some(("root".into(), 4))),
        ]);

        let ordered = splice_target_fragments(nodes, &owners, "root");
        assert_eq!(
            ordered
                .iter()
                .map(|node| node.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["before", "iframe-a", "child-a", "between", "iframe-b", "child-b", "after"]
        );
    }

    #[test]
    fn stale_parent_attach_cannot_replace_the_canonical_subtree() {
        let router = TargetRouter::new(test_session("root-target", "root", None), HashSet::new());
        let mut state = router.state.lock().expect("router state");
        state.sessions.insert(
            "parent-new".into(),
            test_session("parent-target", "parent-new", Some("root")),
        );
        state
            .session_for_target
            .insert("parent-target".into(), "parent-new".into());
        state.sessions.insert(
            "child-new".into(),
            test_session("child-target", "child-new", Some("parent-new")),
        );
        state
            .session_for_target
            .insert("child-target".into(), "child-new".into());
        state.sessions.insert(
            "grandchild".into(),
            test_session("grandchild-target", "grandchild", Some("child-new")),
        );
        state
            .session_for_target
            .insert("grandchild-target".into(), "grandchild".into());

        let decision = register_attached_session(
            &mut state,
            "parent-stale",
            DiscoveredTarget {
                target_id: "child-target".into(),
                parent_frame_id: Some("parent-stale-frame".into()),
            },
            "child-stale",
        );

        assert!(decision.is_none());
        assert_eq!(
            state
                .session_for_target
                .get("child-target")
                .map(String::as_str),
            Some("child-new")
        );
        assert!(state.sessions.contains_key("child-new"));
        assert!(state.sessions.contains_key("grandchild"));
        assert!(!state.sessions.contains_key("child-stale"));
    }

    #[test]
    fn stability_fails_closed_for_root_loading_and_related_target_gaps() {
        let router = TargetRouter::new(test_session("root-target", "root", None), HashSet::new());
        assert!(router.settlement().allows_stability());

        {
            let mut state = router.state.lock().expect("router state");
            state.root_loading = true;
        }
        assert!(!router.settlement().allows_stability());

        {
            let mut state = router.state.lock().expect("router state");
            state.root_loading = false;
            state.discovered.insert(
                "child-target".into(),
                DiscoveredTarget {
                    target_id: "child-target".into(),
                    parent_frame_id: Some("root-target".into()),
                },
            );
        }
        let settlement = router.settlement();
        assert!(!settlement.root_loading);
        assert!(!settlement.target_settled);
        assert!(!settlement.allows_stability());
    }

    #[test]
    fn direct_navigation_accepts_only_the_committed_loader_load() {
        let mut observed = DirectNavigationObserved {
            committed: true,
            loaded: false,
            commit_event_sequence: Some(40),
            commit_loader_id: Some("new-loader".into()),
            ..DirectNavigationObserved::default()
        };
        assert!(!observed.observe_loader_load(Some("old-loader")));
        assert!(!observed.loaded);
        assert!(observed.observe_loader_load(Some("new-loader")));
        assert!(observed.loaded);
    }

    #[test]
    fn root_loading_ignores_old_loader_unqualified_and_same_document_events() {
        let router = TargetRouter::new(test_session("root-target", "root", None), HashSet::new());
        let mut state = router.state.lock().expect("router state");

        assert!(update_root_loading_state(
            &mut state,
            "Page.frameNavigated",
            &json!({"frame": {"id": "root-target", "loaderId": "new-loader"}}),
        ));
        assert!(state.root_loading);
        assert_eq!(state.root_loader_id.as_deref(), Some("new-loader"));

        update_root_loading_state(
            &mut state,
            "Page.lifecycleEvent",
            &json!({"frameId": "root-target", "loaderId": "old-loader", "name": "load"}),
        );
        update_root_loading_state(&mut state, "Page.loadEventFired", &json!({}));
        update_root_loading_state(
            &mut state,
            "Page.frameStoppedLoading",
            &json!({"frameId": "root-target"}),
        );
        update_root_loading_state(
            &mut state,
            "Page.navigatedWithinDocument",
            &json!({"frameId": "root-target", "url": "https://example.test/#state"}),
        );
        assert!(
            state.root_loading,
            "old, unqualified, and same-document events cannot settle the active loader"
        );

        assert!(update_root_loading_state(
            &mut state,
            "Page.lifecycleEvent",
            &json!({"frameId": "root-target", "loaderId": "new-loader", "name": "load"}),
        ));
        assert!(!state.root_loading);

        update_root_loading_state(
            &mut state,
            "Page.frameStartedLoading",
            &json!({"frameId": "root-target"}),
        );
        assert!(state.root_loading);
        assert!(state.root_loader_id.is_none());
        update_root_loading_state(
            &mut state,
            "Page.navigatedWithinDocument",
            &json!({"frameId": "root-target", "url": "https://example.test/#ordinary"}),
        );
        assert!(
            !state.root_loading,
            "ordinary same-document navigation must retain its settled semantics"
        );
    }

    #[tokio::test]
    async fn navigation_barrier_waits_for_the_exact_router_event_ack() {
        let router = TargetRouter::new(test_session("root-target", "root", None), HashSet::new());
        let generation = AtomicU64::new(8);
        let before_generation = 7;
        let old_event_sequence = 41;
        let causal_event_sequence = 42;
        assert!(generation.load(Ordering::SeqCst) > before_generation);
        router.acknowledge_navigation_event(old_event_sequence, 8);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut barrier = Box::pin(wait_for_navigation_ack_until(
            &router,
            causal_event_sequence,
            deadline,
        ));

        assert!(std::future::poll_fn(|cx| match barrier.as_mut().poll(cx) {
            std::task::Poll::Ready(result) => std::task::Poll::Ready(Some(result)),
            std::task::Poll::Pending => std::task::Poll::Ready(None),
        })
        .await
        .is_none());

        // A generation bump and an old acknowledgement are already visible;
        // only the distinct later causal sequence may release the wait.
        generation.store(9, Ordering::SeqCst);
        router.acknowledge_navigation_event(causal_event_sequence, 9);
        router.notify.notify_waiters();
        assert_eq!(barrier.await, Some(9));
    }

    #[test]
    fn runtime_evaluate_contract_has_v8_timeout_inside_transport_deadline() {
        let read_only = runtime_evaluate_params("document.title", true);
        assert_eq!(read_only["expression"], "document.title");
        assert_eq!(read_only["returnByValue"], true);
        assert_eq!(read_only["awaitPromise"], false);
        assert_eq!(read_only["throwOnSideEffect"], true);
        assert_eq!(read_only["userGesture"], false);
        assert_eq!(
            read_only["timeout"].as_u64(),
            Some(EVALUATE_V8_TIMEOUT.as_millis() as u64)
        );

        let mutating = runtime_evaluate_params("Promise.resolve(42)", false);
        assert_eq!(mutating["awaitPromise"], true);
        assert_eq!(mutating["throwOnSideEffect"], false);
        assert_eq!(mutating["userGesture"], true);
        assert!(
            EVALUATE_DEADLINE > EVALUATE_V8_TIMEOUT,
            "the transport deadline must leave room for V8's timeout response"
        );
    }
}
