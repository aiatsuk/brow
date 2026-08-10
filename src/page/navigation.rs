//! Navigation settlement, typed waits, history traversal, and action receipts.
//!
//! The critical invariant is subscribe-before-dispatch. A trusted input closure is
//! executed once; every retry below is observation-only.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::cdp::conn::CdpEventHandle;
use crate::cdp::{CdpEvent, CdpEventReceiver, EVENT_STREAM_GAP_METHOD};
use crate::ipc::{WaitConditions, WaitPolicy};
use crate::redact;

use super::{
    input, loader_correlated_load, wait_for_navigation_ack_until, wait_for_router_processed_until,
    wait_for_target_settle_until, DispatchState, MouseButton, NodeFingerprint, Page, PageError,
    Point, PointTarget, TargetSettlement,
};

const AUTO_DISCOVERY: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationKind {
    None,
    SameDocument,
    CrossDocument,
    History,
    Reload,
    WindowOpen,
    Unknown,
}

/// What initiated observation of a possible document transition. This is
/// intentionally orthogonal to `navigation`: a reload may complete through an
/// HTTP redirect while the final document transition remains cross-document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationTrigger {
    Input,
    History,
    Reload,
    Wait,
    PointerPark,
    Checkpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NavigationScope {
    #[default]
    None,
    Root,
    Subframe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    NotWaited,
    NoNavigation,
    ConditionsMet,
    Committed,
    Loaded,
    Stable,
    TimedOut,
    Incomplete,
    DialogBlocked,
}

impl WaitOutcome {
    pub fn label(self) -> &'static str {
        match self {
            WaitOutcome::NotWaited => "not waited",
            WaitOutcome::NoNavigation => "no navigation",
            WaitOutcome::ConditionsMet => "conditions met",
            WaitOutcome::Committed => "committed",
            WaitOutcome::Loaded => "loaded",
            WaitOutcome::Stable => "stable",
            WaitOutcome::TimedOut => "timed out",
            WaitOutcome::Incomplete => "incomplete",
            WaitOutcome::DialogBlocked => "dialog blocked",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub operation: String,
    /// `null` means a transport failure made delivery unknowable.
    pub dispatched: Option<bool>,
    pub dispatch_state: DispatchState,
    pub navigation_trigger: NavigationTrigger,
    pub requested_wait: WaitPolicy,
    pub effective_wait: WaitPolicy,
    pub outcome: WaitOutcome,
    pub navigation: NavigationKind,
    pub navigation_scope: NavigationScope,
    /// Number of completed HTTP redirect hops observed during this operation.
    pub redirect_count: u32,
    pub before_url: String,
    pub final_url: String,
    /// False only when no post-operation location probe or causal root URL was
    /// available. Settled success never permits this to remain false.
    pub final_url_observed: bool,
    pub before_generation: u64,
    pub final_generation: u64,
    pub elapsed_ms: u64,
    pub discovery_ms: u64,
    pub timeout_ms: u64,
    pub quiet_ms: u64,
    pub active_finite_requests: usize,
    pub excluded_long_lived_requests: usize,
    pub root_loading: bool,
    pub target_settled: bool,
    pub event_complete: bool,
    pub event_gap_delta: u64,
    pub history_entry_id: Option<i64>,
    pub history_from_index: Option<i64>,
    pub history_to_index: Option<i64>,
    pub reload_loader_id: Option<String>,
    pub dialog_type: Option<String>,
    pub dialog_message: Option<String>,
    pub stability_note: Option<String>,
    #[serde(default)]
    pub guidance: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub observed_conditions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointerParkReceipt {
    #[serde(flatten)]
    pub action: ActionReceipt,
    /// CDP does not expose the current pointer coordinate, so `null` is the
    /// honest before-state unless brow becomes the sole pointer dispatcher.
    pub before: Option<Point>,
    pub after: Point,
    /// Compatibility coordinates for existing text/job consumers.
    pub x: f64,
    pub y: f64,
    pub warning: String,
}

#[derive(Debug, Clone)]
struct Baseline {
    started: Instant,
    url: String,
    generation: u64,
    gaps: u64,
    timeout_ms: u64,
    quiet_ms: u64,
    /// Published CDP prefix that predates this operation's subscriber.
    event_sequence: u64,
    /// Events at or below this exact pre-dispatch cursor are context, not a
    /// consequence of the operation whose receipt is being built.
    causal_event_floor: u64,
}

struct StableProof {
    url: String,
    generation: u64,
    activity_revision: u64,
    event_sequence: u64,
}

#[derive(Default)]
struct Observed {
    navigation: Option<NavigationKind>,
    scope: NavigationScope,
    committed: bool,
    loaded: bool,
    dialog_type: Option<String>,
    dialog_message: Option<String>,
    event_incomplete: bool,
    local_event_gap: u64,
    redirect_count: u32,
    root_url: Option<String>,
    /// Session -> renderer root frame, tracked from this receiver's own ordered
    /// attach/detach stream so queued OOPIF events survive router-state races.
    action_sessions: HashMap<String, String>,
    commit_event_sequence: Option<u64>,
    commit_loader_id: Option<String>,
    /// Loader known from request/lifecycle start even before frameNavigated.
    pending_loader_id: Option<String>,
    load_event_sequence: Option<u64>,
    last_event_sequence: u64,
    causal_event_floor: u64,
}

#[derive(Debug, Clone)]
struct ActionTarget {
    session_id: String,
    frame_id: String,
}

#[derive(Debug)]
struct SettlementFailure {
    message: String,
    outcome: WaitOutcome,
    blockers: Vec<String>,
}

fn begin_cross_document_observation(
    observed: &mut Observed,
    navigation: NavigationKind,
    scope: NavigationScope,
    preserve_fresh_commit: bool,
) {
    // A lifecycle completion is useful only after the matching new document
    // commits. Never let a commit from navigation A authorize a load/stop from
    // navigation B, whose commit may still be queued or may never arrive.
    if !(preserve_fresh_commit && observed.committed && !observed.loaded) {
        observed.committed = false;
        observed.commit_event_sequence = None;
        observed.commit_loader_id = None;
        observed.pending_loader_id = None;
    }
    observed.loaded = false;
    observed.load_event_sequence = None;
    observed.navigation = Some(navigation);
    observed.scope = scope;
}

fn mark_load_if_causally_committed(
    observed: &mut Observed,
    event_sequence: u64,
    load_loader_id: Option<&str>,
) -> bool {
    if !loader_correlated_load(
        observed.committed,
        observed.commit_event_sequence,
        observed.commit_loader_id.as_deref(),
        load_loader_id,
    ) {
        return false;
    }
    observed.loaded = true;
    observed.load_event_sequence = Some(event_sequence);
    true
}

fn has_unfinished_cross_document_navigation(observed: &Observed) -> bool {
    !observed.loaded
        && (observed.commit_loader_id.is_some() || observed.pending_loader_id.is_some())
}

fn event_evidence_complete(event_stream_gaps: u64) -> bool {
    event_stream_gaps == 0
}

fn observed_navigation_dimensions(observed: &Observed) -> (NavigationKind, NavigationScope) {
    (
        observed.navigation.unwrap_or(NavigationKind::None),
        observed.scope,
    )
}

fn history_entry_id(entries: &[Value], index: i64) -> Option<i64> {
    usize::try_from(index)
        .ok()
        .and_then(|index| entries.get(index))
        .and_then(|entry| entry.get("id"))
        .and_then(Value::as_i64)
}

fn history_selection_is_current(
    history: &Value,
    selected_index: i64,
    selected_current_entry_id: i64,
    selected_target_index: i64,
    selected_target_entry_id: i64,
    delta: i64,
) -> bool {
    let Some(current_index) = history.get("currentIndex").and_then(Value::as_i64) else {
        return false;
    };
    let Some(entries) = history.get("entries").and_then(Value::as_array) else {
        return false;
    };
    current_index == selected_index
        && history_entry_id(entries, current_index) == Some(selected_current_entry_id)
        && current_index.checked_add(delta) == Some(selected_target_index)
        && history_entry_id(entries, selected_target_index) == Some(selected_target_entry_id)
}

fn event_is_action_causal(event_sequence: u64, causal_event_floor: u64) -> bool {
    event_sequence > causal_event_floor
}

fn redacted_dialog_message(message: &str) -> String {
    redact::storage_text(message)
}

fn redacted_navigation_error_url(url: &str) -> String {
    redact::url(url)
}

impl SettlementFailure {
    fn new(message: impl Into<String>, outcome: WaitOutcome, blockers: Vec<String>) -> Self {
        Self {
            message: message.into(),
            outcome,
            blockers,
        }
    }
}

impl Page {
    fn root_action_target(&self) -> ActionTarget {
        ActionTarget {
            session_id: self.session_id.clone(),
            frame_id: self.frame_id.clone(),
        }
    }

    fn ref_action_target(&self, node_ref: &str) -> Result<ActionTarget, PageError> {
        let entry = self.resolve(node_ref)?;
        Ok(ActionTarget {
            session_id: entry.identity.session_id,
            frame_id: entry.identity.frame_id,
        })
    }

    fn point_action_target(&self, target: &PointTarget) -> Result<ActionTarget, PageError> {
        match target {
            PointTarget::Ref(node_ref) => self.ref_action_target(node_ref),
            PointTarget::At(_) => Ok(self.root_action_target()),
        }
    }

    /// Captures the immutable evidence seed before any operation preflight.
    async fn navigation_baseline(&self, timeout: Duration, quiet: Duration) -> Baseline {
        self.navigation_baseline_from(Instant::now(), timeout, quiet)
            .await
    }

    async fn navigation_baseline_from(
        &self,
        started: Instant,
        timeout: Duration,
        quiet: Duration,
    ) -> Baseline {
        let generation = self.generation();
        let gaps = self.events.event_stream_gaps();
        let location_budget = if timeout.is_zero() {
            Duration::from_secs(3)
        } else {
            timeout
                .saturating_sub(started.elapsed())
                .min(Duration::from_secs(3))
        };
        let location = if location_budget.is_zero() {
            None
        } else {
            self.location_with_timeout(location_budget)
                .await
                .map(|value| redact::url(&value.0))
                .ok()
        };
        let event_sequence = self.client.latest_published_event_sequence();
        Baseline {
            started,
            url: location.unwrap_or_default(),
            generation,
            gaps,
            timeout_ms: timeout.as_millis() as u64,
            quiet_ms: quiet.as_millis() as u64,
            event_sequence,
            causal_event_floor: event_sequence,
        }
    }

    async fn subscribed_navigation_baseline(
        &self,
        timeout: Duration,
        quiet: Duration,
    ) -> Result<(Baseline, CdpEventReceiver), PageError> {
        let (events, published_before_subscription) = self.client.subscribe_with_watermark();
        let synchronization_budget = if timeout.is_zero() {
            Duration::from_secs(3)
        } else {
            timeout
        };
        let deadline = tokio::time::Instant::now() + synchronization_budget;
        if published_before_subscription != 0
            && !wait_for_router_processed_until(
                &self.router,
                published_before_subscription,
                deadline,
            )
            .await
        {
            return Err(PageError::Navigation {
                url: self
                    .location()
                    .await
                    .map(|value| redacted_navigation_error_url(&value.0))
                    .unwrap_or_default(),
                reason: format!(
                    "target router did not process the pre-subscription CDP prefix through sequence {published_before_subscription}"
                ),
            });
        }
        if published_before_subscription != 0
            && !self
                .events
                .wait_processed_until(published_before_subscription, deadline)
                .await
        {
            return Err(PageError::Navigation {
                url: self
                    .location()
                    .await
                    .map(|value| redacted_navigation_error_url(&value.0))
                    .unwrap_or_default(),
                reason: format!(
                    "event recorder did not process the pre-subscription CDP prefix through sequence {published_before_subscription}"
                ),
            });
        }
        let mut baseline = self.navigation_baseline(timeout, quiet).await;
        baseline.event_sequence = published_before_subscription;
        baseline.causal_event_floor = published_before_subscription;
        Ok((baseline, events))
    }

    // These dimensions stay explicit at the central receipt boundary so a new
    // action cannot accidentally omit dispatch/wait/scope evidence.
    #[allow(clippy::too_many_arguments)]
    async fn finish_receipt(
        &self,
        operation: &str,
        dispatch_state: DispatchState,
        requested_wait: WaitPolicy,
        effective_wait: WaitPolicy,
        outcome: WaitOutcome,
        navigation: NavigationKind,
        navigation_scope: NavigationScope,
        baseline: &Baseline,
        blockers: Vec<String>,
    ) -> ActionReceipt {
        let configured = Duration::from_millis(baseline.timeout_ms);
        let remaining = configured.saturating_sub(baseline.started.elapsed());
        let final_observation = if baseline.timeout_ms == 0 {
            self.location_with_timeout(Duration::from_secs(3))
                .await
                .map(|v| redact::url(&v.0))
                .ok()
        } else if remaining.is_zero() {
            None
        } else {
            self.location_with_timeout(remaining.min(Duration::from_secs(3)))
                .await
                .map(|v| redact::url(&v.0))
                .ok()
        };
        let final_url_observed = final_observation.is_some();
        let final_url = final_observation.unwrap_or_else(|| baseline.url.clone());
        let activity = self.events.activity();
        let gaps = self.events.event_stream_gaps();
        let settlement = self.router.settlement();
        let mut guidance = Vec::new();
        if outcome == WaitOutcome::NoNavigation && requested_wait == WaitPolicy::Auto {
            guidance.push(
                "navigation starting after the discovery window requires an explicit typed wait"
                    .into(),
            );
        }
        if dispatch_state != DispatchState::Prevented
            && matches!(
                outcome,
                WaitOutcome::TimedOut | WaitOutcome::Incomplete | WaitOutcome::DialogBlocked
            )
        {
            guidance
                .push("do not replay the action blindly; inspect page/application state".into());
        }
        ActionReceipt {
            operation: operation.to_string(),
            dispatched: dispatch_state.receipt_value(),
            dispatch_state,
            navigation_trigger: navigation_trigger(operation),
            requested_wait,
            effective_wait,
            outcome,
            navigation,
            navigation_scope,
            redirect_count: 0,
            before_url: baseline.url.clone(),
            final_url,
            final_url_observed,
            before_generation: baseline.generation,
            final_generation: self.generation(),
            elapsed_ms: baseline.started.elapsed().as_millis() as u64,
            discovery_ms: if requested_wait == WaitPolicy::Auto {
                AUTO_DISCOVERY.as_millis().min(baseline.timeout_ms as u128) as u64
            } else {
                0
            },
            timeout_ms: baseline.timeout_ms,
            quiet_ms: baseline.quiet_ms,
            active_finite_requests: activity.active_finite,
            excluded_long_lived_requests: activity.excluded_long_lived,
            root_loading: settlement.root_loading,
            target_settled: settlement.target_settled,
            event_complete: event_evidence_complete(gaps),
            event_gap_delta: gaps.saturating_sub(baseline.gaps),
            history_entry_id: None,
            history_from_index: None,
            history_to_index: None,
            reload_loader_id: None,
            dialog_type: None,
            dialog_message: None,
            stability_note: (effective_wait == WaitPolicy::Stable)
                .then(|| "browser stability does not prove application business completion".into()),
            guidance,
            blockers,
            observed_conditions: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn wait_failure(
        &self,
        message: impl Into<String>,
        operation: &str,
        dispatch_state: DispatchState,
        requested: WaitPolicy,
        effective: WaitPolicy,
        outcome: WaitOutcome,
        observed: &Observed,
        baseline: &Baseline,
        blockers: Vec<String>,
    ) -> PageError {
        let receipt = self
            .finish_receipt(
                operation,
                dispatch_state,
                requested,
                effective,
                outcome,
                observed.navigation.unwrap_or(NavigationKind::None),
                observed.scope,
                baseline,
                blockers,
            )
            .await;
        let mut receipt = receipt;
        receipt.dialog_type = observed.dialog_type.clone();
        receipt.dialog_message = observed.dialog_message.clone();
        merge_observed_evidence(
            &mut receipt.event_complete,
            &mut receipt.event_gap_delta,
            &mut receipt.redirect_count,
            observed,
        );
        PageError::WaitFailure {
            message: message.into(),
            receipt: Box::new(receipt),
        }
    }

    async fn operation_failure(
        &self,
        error: PageError,
        operation: &str,
        dispatch_state: DispatchState,
        requested: WaitPolicy,
        effective: WaitPolicy,
        baseline: &Baseline,
    ) -> PageError {
        let prevented = dispatch_state == DispatchState::Prevented;
        let observed = Observed {
            navigation: (!prevented).then_some(NavigationKind::Unknown),
            ..Observed::default()
        };
        self.wait_failure(
            error.to_string(),
            operation,
            dispatch_state,
            requested,
            effective,
            if prevented {
                WaitOutcome::NotWaited
            } else {
                WaitOutcome::Incomplete
            },
            &observed,
            baseline,
            vec![error.to_string()],
        )
        .await
    }

    /// Mints a truthful pre-dispatch receipt for a semantically invalid request
    /// when the daemon has a real Page from which to capture the immutable seed.
    pub(crate) async fn rejected_request_receipt(
        &self,
        operation: &str,
        requested: WaitPolicy,
        effective: WaitPolicy,
        timeout: Duration,
        quiet: Duration,
        blocker: String,
    ) -> ActionReceipt {
        let baseline = self.navigation_baseline(timeout, quiet).await;
        self.finish_receipt(
            operation,
            DispatchState::Prevented,
            requested,
            effective,
            WaitOutcome::NotWaited,
            NavigationKind::None,
            NavigationScope::None,
            &baseline,
            vec![blocker],
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_after_dispatch(
        &self,
        operation: &str,
        dispatch_state: DispatchState,
        requested: WaitPolicy,
        effective: WaitPolicy,
        timeout: Duration,
        quiet: Duration,
        baseline: Baseline,
        dispatched_at: Instant,
        mut events: CdpEventReceiver,
        action_target: ActionTarget,
    ) -> Result<ActionReceipt, PageError> {
        let mut observed = Observed {
            action_sessions: HashMap::from([(
                action_target.session_id.clone(),
                self.router
                    .session(&action_target.session_id)
                    .map(|session| session.frame_id)
                    .unwrap_or_else(|| action_target.frame_id.clone()),
            )]),
            last_event_sequence: baseline.event_sequence,
            causal_event_floor: baseline.causal_event_floor,
            ..Observed::default()
        };
        if effective == WaitPolicy::Conditions {
            return Err(self
                .wait_failure(
                    "conditions is a receipt-only policy and cannot settle an input action",
                    operation,
                    dispatch_state,
                    requested,
                    effective,
                    WaitOutcome::Incomplete,
                    &observed,
                    &baseline,
                    vec!["use auto, none, commit, load, or stable for input actions".into()],
                )
                .await);
        }
        if effective == WaitPolicy::None {
            return Ok(self
                .finish_receipt(
                    operation,
                    dispatch_state,
                    requested,
                    effective,
                    WaitOutcome::NotWaited,
                    NavigationKind::None,
                    NavigationScope::None,
                    &baseline,
                    Vec::new(),
                )
                .await);
        }

        // Settlement timeout and Auto discovery both begin only after the
        // trusted input/history/reload command has acknowledged dispatch.
        // Baseline capture and hit-test preflight must not consume either.
        let started = tokio::time::Instant::from_std(dispatched_at);
        let deadline = started + timeout;
        let discovery_deadline = started + AUTO_DISCOVERY.min(timeout);
        loop {
            if let Err(failure) =
                self.drain_action_events(&mut events, &action_target, &mut observed)
            {
                return Err(self
                    .wait_failure(
                        failure.message,
                        operation,
                        dispatch_state,
                        requested,
                        effective,
                        failure.outcome,
                        &observed,
                        &baseline,
                        failure.blockers,
                    )
                    .await);
            }
            let now = tokio::time::Instant::now();
            if effective == WaitPolicy::Auto
                && observed.navigation.is_none()
                && now >= discovery_deadline
            {
                let url_cursor_before = self.client.latest_published_event_sequence();
                // These are independent CDP queries. Running them together keeps
                // the 250 ms discovery contract honest without weakening either
                // the authoritative target inventory or final-URL proof.
                let (target_refresh, final_url) = tokio::join!(
                    self.refresh_and_settle_targets(deadline),
                    self.final_url_probe_before(deadline)
                );
                let receipt_cursor = match self
                    .synchronize_router_evidence(
                        &mut events,
                        &action_target,
                        &mut observed,
                        deadline,
                    )
                    .await
                {
                    Ok(sequence) => sequence,
                    Err(failure) => {
                        return Err(self
                            .wait_failure(
                                failure.message,
                                operation,
                                dispatch_state,
                                requested,
                                effective,
                                failure.outcome,
                                &observed,
                                &baseline,
                                failure.blockers,
                            )
                            .await)
                    }
                };
                if observed.navigation.is_some() {
                    continue;
                }
                if let Err(failure) = target_refresh {
                    return Err(self
                        .wait_failure(
                            failure.message,
                            operation,
                            dispatch_state,
                            requested,
                            effective,
                            failure.outcome,
                            &observed,
                            &baseline,
                            failure.blockers,
                        )
                        .await);
                }
                if receipt_cursor != url_cursor_before {
                    continue;
                }
                let Some(final_url) = final_url else {
                    let mut receipt = self
                        .finish_receipt(
                            operation,
                            dispatch_state,
                            requested,
                            effective,
                            WaitOutcome::Incomplete,
                            NavigationKind::None,
                            NavigationScope::None,
                            &baseline,
                            vec![
                                "final URL was unavailable before the shared post-dispatch deadline"
                                    .into(),
                            ],
                        )
                        .await;
                    receipt.guidance.push(
                        "inspect current page state; do not treat this receipt as settled".into(),
                    );
                    return Err(PageError::WaitFailure {
                        message: format!(
                            "{operation} completed its navigation discovery window but the final URL could not be proven"
                        ),
                        receipt: Box::new(receipt),
                    });
                };
                if final_url != baseline.url {
                    // Runtime already observes a document/location transition,
                    // but its Page lifecycle event has not reached this ordered
                    // subscriber yet. Never call that NoNavigation.
                    continue;
                }
                let receipt_generation = self.generation();
                let receipt_settlement = self.router.settlement();
                let mut receipt = self
                    .finish_receipt(
                        operation,
                        dispatch_state,
                        requested,
                        effective,
                        WaitOutcome::NoNavigation,
                        NavigationKind::None,
                        NavigationScope::None,
                        &baseline,
                        Vec::new(),
                    )
                    .await;
                if self.client.latest_published_event_sequence() != receipt_cursor
                    || !self.router.processed_through(receipt_cursor)
                    || !self.events.processed_through(receipt_cursor)
                    || receipt.final_generation != receipt_generation
                    || self.router.settlement() != receipt_settlement
                    || !receipt.target_settled
                    || (receipt.final_url_observed && receipt.final_url != final_url)
                {
                    continue;
                }
                receipt.final_url = final_url;
                receipt.final_url_observed = true;
                return Ok(receipt);
            }

            let desired_reached = match effective {
                WaitPolicy::Auto | WaitPolicy::Commit => observed.committed,
                WaitPolicy::Load => {
                    observed.loaded
                        || (observed.committed
                            && observed.navigation == Some(NavigationKind::SameDocument))
                }
                // Explicit stability is meaningful even when the action only
                // starts data work or a same-document render with no navigation.
                WaitPolicy::Stable => true,
                WaitPolicy::None => true,
                WaitPolicy::Conditions => unreachable!("rejected above"),
            };
            if desired_reached {
                let mut causal_final_url = None;
                let mut epoch_url = None;
                let mut stable_proof = None;
                if effective == WaitPolicy::Stable {
                    stable_proof = match self
                        .wait_until_stable(
                            deadline,
                            quiet,
                            &baseline,
                            &mut events,
                            &action_target,
                            &mut observed,
                        )
                        .await
                    {
                        Ok(proof) => {
                            causal_final_url = Some(proof.url.clone());
                            epoch_url = Some(proof.url.clone());
                            Some(proof)
                        }
                        Err(failure) => {
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    operation,
                                    dispatch_state,
                                    requested,
                                    effective,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    failure.blockers,
                                )
                                .await)
                        }
                    };
                }
                if let Err(failure) = self
                    .synchronize_router_evidence(
                        &mut events,
                        &action_target,
                        &mut observed,
                        deadline,
                    )
                    .await
                {
                    return Err(self
                        .wait_failure(
                            failure.message,
                            operation,
                            dispatch_state,
                            requested,
                            effective,
                            failure.outcome,
                            &observed,
                            &baseline,
                            failure.blockers,
                        )
                        .await);
                }
                if let Err(failure) = self.refresh_and_settle_targets(deadline).await {
                    return Err(self
                        .wait_failure(
                            failure.message,
                            operation,
                            dispatch_state,
                            requested,
                            effective,
                            failure.outcome,
                            &observed,
                            &baseline,
                            failure.blockers,
                        )
                        .await);
                }
                let receipt_cursor_before = if effective == WaitPolicy::Stable {
                    None
                } else {
                    match self
                        .synchronize_router_evidence(
                            &mut events,
                            &action_target,
                            &mut observed,
                            deadline,
                        )
                        .await
                    {
                        Ok(sequence) => Some(sequence),
                        Err(failure) => {
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    operation,
                                    dispatch_state,
                                    requested,
                                    effective,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    failure.blockers,
                                )
                                .await)
                        }
                    }
                };
                if effective != WaitPolicy::Stable {
                    // The action deadline starts at acknowledged dispatch, while
                    // `finish_receipt` also serves pre-dispatch failures and uses
                    // the immutable baseline clock. Capture one root location
                    // here so lengthy hit testing cannot consume the final-URL
                    // proof budget of an otherwise successful action.
                    let probed_url = self.final_url_probe_before(deadline).await;
                    epoch_url = probed_url.clone();
                    causal_final_url = probed_url.or_else(|| observed.root_url.clone());
                }
                let receipt_cursor = match self
                    .synchronize_router_evidence(
                        &mut events,
                        &action_target,
                        &mut observed,
                        deadline,
                    )
                    .await
                {
                    Ok(sequence) => sequence,
                    Err(failure) => {
                        return Err(self
                            .wait_failure(
                                failure.message,
                                operation,
                                dispatch_state,
                                requested,
                                effective,
                                failure.outcome,
                                &observed,
                                &baseline,
                                failure.blockers,
                            )
                            .await)
                    }
                };
                let receipt_generation = self.generation();
                let receipt_settlement = self.router.settlement();
                if receipt_cursor_before.is_some_and(|before| before != receipt_cursor) {
                    continue;
                }
                if let Some(proof) = &stable_proof {
                    let activity = self.events.activity();
                    if receipt_cursor != proof.event_sequence
                        || receipt_generation != proof.generation
                        || activity.revision != proof.activity_revision
                        || !receipt_settlement.allows_stability()
                    {
                        // The post-proof observer drain, URL probe, or target
                        // inventory refresh advanced the evidence epoch.
                        continue;
                    }
                }
                let outcome = match effective {
                    WaitPolicy::Stable => WaitOutcome::Stable,
                    WaitPolicy::Load
                        if observed.navigation == Some(NavigationKind::SameDocument) =>
                    {
                        WaitOutcome::Committed
                    }
                    WaitPolicy::Load => WaitOutcome::Loaded,
                    _ => WaitOutcome::Committed,
                };
                let mut receipt = self
                    .finish_receipt(
                        operation,
                        dispatch_state,
                        requested,
                        effective,
                        outcome,
                        observed.navigation.unwrap_or(NavigationKind::None),
                        observed.scope,
                        &baseline,
                        Vec::new(),
                    )
                    .await;
                if self.client.latest_published_event_sequence() != receipt_cursor
                    || !self.router.processed_through(receipt_cursor)
                    || !self.events.processed_through(receipt_cursor)
                    || receipt.final_generation != receipt_generation
                    || self.router.settlement() != receipt_settlement
                    || !receipt.target_settled
                    || (effective == WaitPolicy::Load && receipt.root_loading)
                {
                    continue;
                }
                if let Some(proof) = &stable_proof {
                    let activity = self.events.activity();
                    if receipt.final_generation != proof.generation
                        || activity.revision != proof.activity_revision
                        || self.client.latest_published_event_sequence() != proof.event_sequence
                        || !self.router.settlement().allows_stability()
                    {
                        continue;
                    }
                }
                if receipt.final_url_observed
                    && epoch_url
                        .as_ref()
                        .is_some_and(|url| receipt.final_url != *url)
                {
                    continue;
                }
                if let Some(url) = causal_final_url {
                    // Stable's final revalidation is the strongest observation.
                    // For commit/load, prefer the later post-settlement Runtime
                    // location probe from `finish_receipt`; a causal frame event
                    // may describe an earlier redirect hop that was already in
                    // the subscriber queue. Use it only as a deadline fallback.
                    if effective == WaitPolicy::Stable || !receipt.final_url_observed {
                        receipt.final_url = url;
                        receipt.final_url_observed = true;
                    }
                }
                merge_observed_evidence(
                    &mut receipt.event_complete,
                    &mut receipt.event_gap_delta,
                    &mut receipt.redirect_count,
                    &observed,
                );
                if !receipt.final_url_observed {
                    receipt.outcome = WaitOutcome::Incomplete;
                    receipt.blockers.push(
                        "final URL was unavailable before the shared settlement deadline".into(),
                    );
                    receipt.guidance.push(
                        "inspect current page state; do not treat this receipt as settled".into(),
                    );
                    return Err(PageError::WaitFailure {
                        message: format!(
                            "{operation} reached its lifecycle target but the final URL could not be proven"
                        ),
                        receipt: Box::new(receipt),
                    });
                }
                return Ok(receipt);
            }

            if now >= deadline {
                let mut blockers = vec!["deadline expired".into()];
                if effective == WaitPolicy::Load
                    && observed.navigation == Some(NavigationKind::WindowOpen)
                {
                    blockers.push(
                        "window.open was observed, but this page session does not track the popup's load lifecycle"
                            .into(),
                    );
                }
                return Err(self
                    .wait_failure(
                        format!("{operation} was dispatched but did not reach {effective:?} before timeout"),
                        operation,
                        dispatch_state,
                        requested,
                        effective,
                        WaitOutcome::TimedOut,
                        &observed,
                        &baseline,
                        blockers,
                    )
                    .await);
            }

            let phase_deadline = if effective == WaitPolicy::Auto && observed.navigation.is_none() {
                discovery_deadline.min(deadline)
            } else {
                deadline
            };
            let slice = phase_deadline
                .saturating_duration_since(now)
                .min(POLL_INTERVAL);
            match tokio::time::timeout(slice, events.recv()).await {
                Ok(Ok(event)) => {
                    if let Some(failure) =
                        self.consume_action_event(&event, &action_target, &mut observed)
                    {
                        return Err(self
                            .wait_failure(
                                failure.message,
                                operation,
                                dispatch_state,
                                requested,
                                effective,
                                failure.outcome,
                                &observed,
                                &baseline,
                                failure.blockers,
                            )
                            .await);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    observed.event_incomplete = true;
                    observed.local_event_gap = n;
                    return Err(self
                        .wait_failure(
                            format!("navigation observer lagged and missed {n} event(s)"),
                            operation,
                            dispatch_state,
                            requested,
                            effective,
                            WaitOutcome::Incomplete,
                            &observed,
                            &baseline,
                            vec![format!("subscriber missed {n} event(s)")],
                        )
                        .await);
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    observed.event_incomplete = true;
                    observed.local_event_gap = 1;
                    return Err(self
                        .wait_failure(
                            "browser event stream closed while settling",
                            operation,
                            dispatch_state,
                            requested,
                            effective,
                            WaitOutcome::Incomplete,
                            &observed,
                            &baseline,
                            vec!["event stream closed".into()],
                        )
                        .await);
                }
                Err(_) => {}
            }
        }
    }

    fn consume_action_event(
        &self,
        event: &Arc<CdpEventHandle>,
        action_target: &ActionTarget,
        observed: &mut Observed,
    ) -> Option<SettlementFailure> {
        observed.last_event_sequence = observed.last_event_sequence.max(event.sequence());
        if event.method == EVENT_STREAM_GAP_METHOD {
            let dropped = event
                .params
                .get("dropped")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            observed.event_incomplete = true;
            observed.local_event_gap = observed.local_event_gap.saturating_add(dropped);
            return Some(SettlementFailure::new(
                "navigation evidence is incomplete because CDP events were lost",
                WaitOutcome::Incomplete,
                vec!["upstream CDP event gap".into()],
            ));
        }
        update_action_session_membership(event, action_target, &mut observed.action_sessions);
        if !event_is_action_causal(event.sequence(), observed.causal_event_floor) {
            return None;
        }
        if !self.observe_navigation_event(event, action_target, observed)
            || event.method != "Page.javascriptDialogOpening"
        {
            return None;
        }

        let kind = event
            .params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("dialog");
        let message = event
            .params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = redacted_dialog_message(message);
        observed.dialog_type = Some(kind.to_string());
        observed.dialog_message = Some(message.clone());
        Some(SettlementFailure::new(
            format!("navigation blocked by {kind}: {message}"),
            WaitOutcome::DialogBlocked,
            vec![format!("{kind}: {message}")],
        ))
    }

    fn note_observer_lag(observed: &mut Observed, skipped: u64) -> SettlementFailure {
        observed.event_incomplete = true;
        observed.local_event_gap = observed.local_event_gap.saturating_add(skipped);
        SettlementFailure::new(
            format!("navigation observer lagged and missed {skipped} event(s)"),
            WaitOutcome::Incomplete,
            vec![format!("subscriber missed {skipped} event(s)")],
        )
    }

    fn drain_action_events(
        &self,
        events: &mut CdpEventReceiver,
        action_target: &ActionTarget,
        observed: &mut Observed,
    ) -> Result<(), SettlementFailure> {
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if let Some(failure) =
                        self.consume_action_event(&event, action_target, observed)
                    {
                        return Err(failure);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => return Ok(()),
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    return Err(Self::note_observer_lag(observed, skipped));
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    observed.event_incomplete = true;
                    observed.local_event_gap = observed.local_event_gap.saturating_add(1);
                    return Err(SettlementFailure::new(
                        "browser event stream closed while settling",
                        WaitOutcome::Incomplete,
                        vec!["event stream closed".into()],
                    ));
                }
            }
        }
    }

    async fn consume_action_events_through(
        &self,
        events: &mut CdpEventReceiver,
        action_target: &ActionTarget,
        observed: &mut Observed,
        event_sequence: u64,
        deadline: tokio::time::Instant,
    ) -> Result<(), SettlementFailure> {
        self.drain_action_events(events, action_target, observed)?;
        while observed.last_event_sequence < event_sequence {
            let event = match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    return Err(Self::note_observer_lag(observed, skipped));
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    observed.event_incomplete = true;
                    observed.local_event_gap = observed.local_event_gap.saturating_add(1);
                    return Err(SettlementFailure::new(
                        "browser event stream closed while synchronizing published evidence",
                        WaitOutcome::Incomplete,
                        vec!["event stream closed before the captured cursor".into()],
                    ));
                }
                Err(_) => {
                    return Err(SettlementFailure::new(
                        "navigation observer did not consume the captured CDP prefix before timeout",
                        WaitOutcome::TimedOut,
                        vec![format!(
                            "navigation observer did not process CDP event sequence {event_sequence}"
                        )],
                    ));
                }
            };
            if let Some(failure) = self.consume_action_event(&event, action_target, observed) {
                return Err(failure);
            }
        }
        Ok(())
    }

    async fn synchronize_router_evidence(
        &self,
        events: &mut CdpEventReceiver,
        action_target: &ActionTarget,
        observed: &mut Observed,
        deadline: tokio::time::Instant,
    ) -> Result<u64, SettlementFailure> {
        // Capture a published cursor, then make all three independent consumers
        // (this action observer, the target router, and EventLog) process through
        // it. Re-read the cursor before returning; if publication advanced while
        // any consumer caught up, repeat against the newer prefix.
        loop {
            let through = self.client.latest_published_event_sequence();
            self.consume_action_events_through(events, action_target, observed, through, deadline)
                .await?;
            if navigation_requires_generation_ack(observed.navigation) {
                let Some(sequence) = observed.commit_event_sequence else {
                    // Chrome can acknowledge Runtime probes for the new
                    // document before this independent broadcast subscriber is
                    // scheduled to consume frameNavigated. A start signal is
                    // not proof of commit, but neither is it an incomplete
                    // stream: wait within the same deadline for the exact key.
                    let event = match tokio::time::timeout_at(deadline, events.recv()).await {
                        Ok(Ok(event)) => event,
                        Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                            return Err(Self::note_observer_lag(observed, skipped));
                        }
                        Ok(Err(broadcast::error::RecvError::Closed)) => {
                            return Err(SettlementFailure::new(
                                "browser event stream closed before the causal navigation committed",
                                WaitOutcome::Incomplete,
                                vec!["event stream closed without a root commit event".into()],
                            ));
                        }
                        Err(_) => {
                            return Err(SettlementFailure::new(
                                "navigation started but no causal commit event arrived before timeout",
                                WaitOutcome::TimedOut,
                                vec![
                                    "cross/same-document navigation lacked an exact router acknowledgement key before the shared deadline"
                                        .into(),
                                ],
                            ));
                        }
                    };
                    if let Some(failure) =
                        self.consume_action_event(&event, action_target, observed)
                    {
                        return Err(failure);
                    }
                    continue;
                };
                if wait_for_navigation_ack_until(&self.router, sequence, deadline)
                    .await
                    .is_none()
                {
                    return Err(SettlementFailure::new(
                        "the causal navigation committed, but ref invalidation was not acknowledged before timeout",
                        WaitOutcome::TimedOut,
                        vec![
                            "target router did not acknowledge the exact causal navigation event before the shared deadline".into(),
                        ],
                    ));
                }
            }
            if through != 0
                && !wait_for_router_processed_until(&self.router, through, deadline).await
            {
                return Err(SettlementFailure::new(
                    "target router did not consume the observed lifecycle prefix before timeout",
                    WaitOutcome::TimedOut,
                    vec![format!(
                        "target router did not process CDP event sequence {through}"
                    )],
                ));
            }
            if through != 0 && !self.events.wait_processed_until(through, deadline).await {
                return Err(SettlementFailure::new(
                    "event recorder did not consume the observed lifecycle prefix before timeout",
                    WaitOutcome::TimedOut,
                    vec![format!(
                        "EventLog did not process CDP event sequence {through}"
                    )],
                ));
            }
            self.drain_action_events(events, action_target, observed)?;
            if self.client.latest_published_event_sequence() == through
                && observed.last_event_sequence >= through
                && self.router.processed_through(through)
                && self.events.processed_through(through)
            {
                return Ok(through);
            }
        }
    }

    async fn refresh_and_settle_targets(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), SettlementFailure> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(SettlementFailure::new(
                "target inventory could not be refreshed before timeout",
                WaitOutcome::TimedOut,
                vec!["shared deadline expired before Target.getTargets".into()],
            ));
        }
        match tokio::time::timeout(
            remaining,
            super::refresh_target_inventory(&self.client, &self.router),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(SettlementFailure::new(
                    format!("target inventory refresh failed: {error}"),
                    WaitOutcome::Incomplete,
                    vec!["Target.getTargets failed after lifecycle synchronization".into()],
                ));
            }
            Err(_) => {
                return Err(SettlementFailure::new(
                    "target inventory refresh timed out",
                    WaitOutcome::TimedOut,
                    vec!["Target.getTargets exceeded the shared deadline".into()],
                ));
            }
        }
        wait_for_target_settle_until(&self.client, &self.router, deadline).await;
        if let Some(failure) = target_settlement_failure(
            self.router.settlement(),
            tokio::time::Instant::now() >= deadline,
            self.router.coverage_gaps(),
        ) {
            return Err(failure);
        }
        Ok(())
    }

    fn observe_navigation_event(
        &self,
        event: &Arc<CdpEventHandle>,
        action_target: &ActionTarget,
        observed: &mut Observed,
    ) -> bool {
        let Some(event_session) = event.session_id.as_deref() else {
            return false;
        };
        let (owned, session_root_frame) = {
            let state = self.router.state.lock().expect("target router mutex");
            (
                state.sessions.contains_key(event_session),
                state
                    .sessions
                    .get(event_session)
                    .map(|session| session.frame_id.clone()),
            )
        };
        let action_session_root = observed
            .action_sessions
            .get(event_session)
            .map(String::as_str);
        let action_session_owned = action_session_root.is_some();
        if !owned && !action_session_owned {
            return false;
        }
        let event_frame = event
            .params
            .get("frameId")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .params
                    .get("frame")
                    .and_then(|f| f.get("id"))
                    .and_then(Value::as_str)
            });
        let has_parent = event
            .params
            .get("frame")
            .and_then(|frame| frame.get("parentId"))
            .is_some();
        let is_session_root = event_is_session_root(
            action_target,
            event_frame,
            session_root_frame.as_deref(),
            has_parent,
        );
        if event.method != "Page.javascriptDialogOpening"
            && !action_event_is_local(
                action_target,
                event_session,
                event_frame,
                session_root_frame.as_deref(),
                action_session_root,
            )
        {
            return false;
        }
        let scope = if action_target.frame_id == self.frame_id {
            NavigationScope::Root
        } else {
            NavigationScope::Subframe
        };
        match event.method.as_str() {
            "Network.requestWillBeSent"
                if is_session_root && document_navigation_request(&event.params) =>
            {
                if completed_document_redirect(&event.params) {
                    observed.redirect_count = observed.redirect_count.saturating_add(1);
                }
                let request_loader_id = event.params.get("loaderId").and_then(Value::as_str);
                let preserve_fresh_commit = observed.committed
                    && !observed.loaded
                    && request_loader_id.is_some()
                    && request_loader_id == observed.commit_loader_id.as_deref();
                begin_cross_document_observation(
                    observed,
                    NavigationKind::CrossDocument,
                    scope,
                    preserve_fresh_commit,
                );
                observed.pending_loader_id = request_loader_id.map(str::to_string);
                true
            }
            "Page.frameRequestedNavigation" | "Page.frameScheduledNavigation"
                if is_session_root =>
            {
                begin_cross_document_observation(observed, NavigationKind::Unknown, scope, false);
                true
            }
            "Page.frameStartedLoading" if is_session_root => {
                begin_cross_document_observation(
                    observed,
                    NavigationKind::CrossDocument,
                    scope,
                    true,
                );
                true
            }
            "Page.lifecycleEvent"
                if is_session_root
                    && event.params.get("name").and_then(Value::as_str) == Some("init") =>
            {
                let loader_id = event.params.get("loaderId").and_then(Value::as_str);
                let preserve_fresh_commit = observed.committed
                    && !observed.loaded
                    && loader_id.is_some()
                    && loader_id == observed.commit_loader_id.as_deref();
                begin_cross_document_observation(
                    observed,
                    NavigationKind::CrossDocument,
                    scope,
                    preserve_fresh_commit,
                );
                observed.pending_loader_id = loader_id.map(str::to_string);
                true
            }
            "Page.frameNavigated" if is_session_root => {
                observed.navigation = Some(NavigationKind::CrossDocument);
                observed.scope = scope;
                observed.committed = true;
                observed.loaded = false;
                observed.load_event_sequence = None;
                observed.commit_event_sequence = Some(event.sequence());
                observed.commit_loader_id = event
                    .params
                    .get("frame")
                    .and_then(|frame| frame.get("loaderId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                observed.pending_loader_id = observed.commit_loader_id.clone();
                if scope == NavigationScope::Root {
                    observed.root_url = event
                        .params
                        .get("frame")
                        .and_then(|frame| frame.get("url"))
                        .and_then(Value::as_str)
                        .map(redact::url);
                }
                true
            }
            "Page.navigatedWithinDocument" if is_session_root => {
                if has_unfinished_cross_document_navigation(observed) {
                    // A pushState/hash update from the old document, or from a
                    // newly committed document before its load event, cannot
                    // settle the active cross-document loader.
                    return true;
                }
                observed.navigation = Some(NavigationKind::SameDocument);
                observed.scope = scope;
                observed.committed = true;
                observed.loaded = true;
                observed.commit_event_sequence = Some(event.sequence());
                observed.commit_loader_id = None;
                observed.pending_loader_id = None;
                observed.load_event_sequence = Some(event.sequence());
                if scope == NavigationScope::Root {
                    observed.root_url = event
                        .params
                        .get("url")
                        .and_then(Value::as_str)
                        .map(redact::url);
                }
                true
            }
            "Page.lifecycleEvent"
                if is_session_root
                    && event.params.get("name").and_then(Value::as_str) == Some("load") =>
            {
                observed.scope = scope;
                let matched = mark_load_if_causally_committed(
                    observed,
                    event.sequence(),
                    event.params.get("loaderId").and_then(Value::as_str),
                );
                if matched {
                    observed.pending_loader_id = None;
                }
                matched
            }
            "Page.windowOpen" => {
                observed.navigation = Some(NavigationKind::WindowOpen);
                observed.scope = scope;
                observed.committed = true;
                observed.loaded = false;
                observed.load_event_sequence = None;
                true
            }
            "Page.javascriptDialogOpening" => true,
            _ => false,
        }
    }

    async fn stability_probe(
        &self,
        deadline: tokio::time::Instant,
        phase: &str,
    ) -> Result<(String, bool), SettlementFailure> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(SettlementFailure::new(
                format!("{phase} could not start before the shared deadline"),
                WaitOutcome::TimedOut,
                vec![format!("{phase} had no remaining deadline budget")],
            ));
        }
        let value = self
            .evaluate_with_timeout("[location.href, document.readyState]", true, remaining)
            .await
            .map_err(|error| {
                let timed_out =
                    matches!(error, PageError::Cdp(crate::cdp::CdpError::Timeout { .. }))
                        || tokio::time::Instant::now() >= deadline;
                SettlementFailure::new(
                    if timed_out {
                        format!("{phase} exceeded the shared wait deadline")
                    } else {
                        format!("{phase} failed: {error}")
                    },
                    if timed_out {
                        WaitOutcome::TimedOut
                    } else {
                        WaitOutcome::Incomplete
                    },
                    vec![if timed_out {
                        format!("{phase} did not complete within the remaining deadline")
                    } else {
                        format!("{phase} could not be observed")
                    }],
                )
            })?;
        let url = value
            .get(0)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ready = value
            .get(1)
            .and_then(Value::as_str)
            .is_some_and(|state| state == "complete");
        Ok((url, ready))
    }

    async fn final_url_probe_before(&self, deadline: tokio::time::Instant) -> Option<String> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        self.location_with_timeout(remaining.min(Duration::from_secs(1)))
            .await
            .ok()
            .map(|(url, _)| redact::url(&url))
    }

    async fn wait_until_stable(
        &self,
        deadline: tokio::time::Instant,
        quiet: Duration,
        _baseline: &Baseline,
        events: &mut CdpEventReceiver,
        action_target: &ActionTarget,
        observed: &mut Observed,
    ) -> Result<StableProof, SettlementFailure> {
        let mut quiet_since: Option<tokio::time::Instant> = None;
        let mut last_url = String::new();
        let mut last_generation = 0;
        let mut last_activity_revision = self.events.activity().revision;
        loop {
            self.drain_action_events(events, action_target, observed)?;
            self.synchronize_router_evidence(events, action_target, observed, deadline)
                .await?;
            if !event_evidence_complete(self.events.event_stream_gaps()) {
                return Err(SettlementFailure::new(
                    "cannot prove stability because event evidence is incomplete",
                    WaitOutcome::Incomplete,
                    vec!["upstream CDP event gap".into()],
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                let activity = self.events.activity();
                let settlement = self.router.settlement();
                let mut blockers = vec![format!(
                    "{} finite request(s) still active or quiet interval incomplete",
                    activity.active_finite
                )];
                if settlement.root_loading {
                    blockers.push("root document is still loading".into());
                }
                if !settlement.target_settled {
                    blockers.push("one or more related targets are not settled".into());
                }
                return Err(SettlementFailure::new(
                    "browser did not become stable before timeout",
                    WaitOutcome::TimedOut,
                    blockers,
                ));
            }
            let generation = self.generation();
            let (url, ready) = self.stability_probe(deadline, "stability probe").await?;
            self.drain_action_events(events, action_target, observed)?;
            self.synchronize_router_evidence(events, action_target, observed, deadline)
                .await?;
            let activity = self.events.activity();
            let settlement = self.router.settlement();
            let unchanged = stability_evidence_unchanged(
                last_generation,
                &last_url,
                last_activity_revision,
                generation,
                &url,
                activity.revision,
            );
            if stable_candidate(ready, activity.active_finite, unchanged, settlement) {
                let since = quiet_since.get_or_insert_with(tokio::time::Instant::now);
                if since.elapsed() >= quiet {
                    if !self.two_animation_frames(deadline).await {
                        quiet_since = None;
                        continue;
                    }
                    self.drain_action_events(events, action_target, observed)?;
                    let final_cursor_before = self
                        .synchronize_router_evidence(events, action_target, observed, deadline)
                        .await?;
                    let (final_url, final_ready) = self
                        .stability_probe(deadline, "stable final revalidation")
                        .await?;
                    self.drain_action_events(events, action_target, observed)?;
                    let final_cursor_after = self
                        .synchronize_router_evidence(events, action_target, observed, deadline)
                        .await?;
                    if final_cursor_after != final_cursor_before {
                        quiet_since = None;
                        continue;
                    }
                    let final_activity = self.events.activity();
                    if !event_evidence_complete(self.events.event_stream_gaps()) {
                        return Err(SettlementFailure::new(
                            "cannot prove stability because event evidence changed during final revalidation",
                            WaitOutcome::Incomplete,
                            vec!["upstream CDP event gap after animation-frame revalidation".into()],
                        ));
                    }
                    self.refresh_and_settle_targets(deadline).await?;
                    let proof_event_sequence = self
                        .synchronize_router_evidence(events, action_target, observed, deadline)
                        .await?;
                    let proof_activity = self.events.activity();
                    if proof_event_sequence == final_cursor_after
                        && proof_activity.active_finite == 0
                        && proof_activity.revision == final_activity.revision
                        && proof_activity.revision == activity.revision
                        && self.generation() == generation
                        && final_url == url
                        && final_ready
                        && self.router.settlement().allows_stability()
                    {
                        return Ok(StableProof {
                            url: redact::url(&final_url),
                            generation,
                            activity_revision: proof_activity.revision,
                            event_sequence: proof_event_sequence,
                        });
                    }
                    quiet_since = None;
                }
            } else {
                quiet_since = None;
            }
            last_generation = generation;
            last_url = url;
            last_activity_revision = activity.revision;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                continue;
            }
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }

    async fn two_animation_frames(&self, deadline: tokio::time::Instant) -> bool {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        self.client
            .call_on_timeout(
                &self.session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve(true))))",
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
                remaining,
            )
            .await
            .is_ok()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn click_with_wait(
        &mut self,
        node_ref: &str,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        force: bool,
        wait: WaitPolicy,
        timeout: Duration,
    ) -> Result<(Point, ActionReceipt), PageError> {
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let mut dispatch = input::DispatchTracker::default();
        let action_target = match self.ref_action_target(node_ref) {
            Ok(target) => target,
            Err(error) => {
                return Err(self
                    .operation_failure(error, "click", dispatch.state(), wait, wait, &baseline)
                    .await)
            }
        };
        let point = match self
            .click_impl(
                node_ref,
                button,
                click_count,
                modifiers,
                force,
                None,
                &mut dispatch,
            )
            .await
        {
            Ok(point) => point,
            Err(error) => {
                return Err(self
                    .operation_failure(error, "click", dispatch.state(), wait, wait, &baseline)
                    .await)
            }
        };
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        let receipt = self
            .settle_after_dispatch(
                "click",
                dispatch.state(),
                wait,
                wait,
                timeout,
                Duration::from_millis(300),
                baseline,
                dispatched_at,
                events,
                action_target,
            )
            .await?;
        Ok((point, receipt))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn click_if_unchanged_with_wait(
        &mut self,
        node_ref: &str,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        expected: &NodeFingerprint,
        wait: WaitPolicy,
        timeout: Duration,
    ) -> Result<(Point, ActionReceipt), PageError> {
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let mut dispatch = input::DispatchTracker::default();
        let action_target = match self.ref_action_target(node_ref) {
            Ok(target) => target,
            Err(error) => {
                return Err(self
                    .operation_failure(error, "click", dispatch.state(), wait, wait, &baseline)
                    .await)
            }
        };
        let point = match self
            .click_impl(
                node_ref,
                button,
                click_count,
                modifiers,
                false,
                Some(expected),
                &mut dispatch,
            )
            .await
        {
            Ok(point) => point,
            Err(error) => {
                return Err(self
                    .operation_failure(error, "click", dispatch.state(), wait, wait, &baseline)
                    .await)
            }
        };
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        let receipt = self
            .settle_after_dispatch(
                "click",
                dispatch.state(),
                wait,
                wait,
                timeout,
                Duration::from_millis(300),
                baseline,
                dispatched_at,
                events,
                action_target,
            )
            .await?;
        Ok((point, receipt))
    }

    pub async fn click_at_with_wait(
        &self,
        point: Point,
        button: MouseButton,
        click_count: i64,
        modifiers: i64,
        wait: WaitPolicy,
        timeout: Duration,
    ) -> Result<ActionReceipt, PageError> {
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let action_target = self.root_action_target();
        let mut dispatch = input::DispatchTracker::default();
        if let Err(error) = self
            .click_at_tracked(point, button, click_count, modifiers, &mut dispatch)
            .await
        {
            return Err(self
                .operation_failure(error, "click", dispatch.state(), wait, wait, &baseline)
                .await);
        }
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        self.settle_after_dispatch(
            "click",
            dispatch.state(),
            wait,
            wait,
            timeout,
            Duration::from_millis(300),
            baseline,
            dispatched_at,
            events,
            action_target,
        )
        .await
    }

    pub async fn press_with_wait(
        &self,
        chord: &str,
        wait: WaitPolicy,
        timeout: Duration,
    ) -> Result<ActionReceipt, PageError> {
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let action_target = self.root_action_target();
        let mut dispatch = input::DispatchTracker::default();
        #[cfg(debug_assertions)]
        super::test_support::hold_press_before_dispatch_if_requested(&self.session_id).await;
        dispatch.note_event_floor(self.client.latest_published_event_sequence());
        if let Err(error) = self.press_tracked(chord, &mut dispatch).await {
            return Err(self
                .operation_failure(error, "press", dispatch.state(), wait, wait, &baseline)
                .await);
        }
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        self.settle_after_dispatch(
            "press",
            dispatch.state(),
            wait,
            wait,
            timeout,
            Duration::from_millis(300),
            baseline,
            dispatched_at,
            events,
            action_target,
        )
        .await
    }

    pub async fn tap_with_wait(
        &mut self,
        target: &PointTarget,
        wait: WaitPolicy,
        timeout: Duration,
    ) -> Result<(Point, ActionReceipt), PageError> {
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let mut dispatch = input::DispatchTracker::default();
        let action_target = match self.point_action_target(target) {
            Ok(target) => target,
            Err(error) => {
                return Err(self
                    .operation_failure(error, "tap", dispatch.state(), wait, wait, &baseline)
                    .await)
            }
        };
        let point = match self.tap_tracked(target, &mut dispatch).await {
            Ok(point) => point,
            Err(error) => {
                return Err(self
                    .operation_failure(error, "tap", dispatch.state(), wait, wait, &baseline)
                    .await)
            }
        };
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        let receipt = self
            .settle_after_dispatch(
                "tap",
                dispatch.state(),
                wait,
                wait,
                timeout,
                Duration::from_millis(300),
                baseline,
                dispatched_at,
                events,
                action_target,
            )
            .await?;
        Ok((point, receipt))
    }

    pub async fn wait_for_conditions(
        &self,
        conditions: &WaitConditions,
        timeout: Duration,
        quiet: Duration,
    ) -> Result<ActionReceipt, PageError> {
        let started = Instant::now();
        let (mut events, published_before_subscription) = self.client.subscribe_with_watermark();
        let deadline = tokio::time::Instant::from_std(started) + timeout;
        let action_target = self.root_action_target();
        let mut observed = Observed {
            action_sessions: HashMap::from([(
                action_target.session_id.clone(),
                action_target.frame_id.clone(),
            )]),
            ..Observed::default()
        };
        let wait_policy = if conditions.stable {
            WaitPolicy::Stable
        } else if conditions.load {
            WaitPolicy::Load
        } else {
            WaitPolicy::Conditions
        };
        if published_before_subscription != 0
            && !wait_for_router_processed_until(
                &self.router,
                published_before_subscription,
                deadline,
            )
            .await
        {
            let baseline = self.navigation_baseline_from(started, timeout, quiet).await;
            return Err(self
                .wait_failure(
                    "typed wait timed out synchronizing pre-subscription browser state",
                    "wait",
                    DispatchState::Prevented,
                    wait_policy,
                    wait_policy,
                    WaitOutcome::TimedOut,
                    &observed,
                    &baseline,
                    vec![format!(
                        "target router did not process CDP event sequence {published_before_subscription}"
                    )],
                )
                .await);
        }
        if published_before_subscription != 0
            && !self
                .events
                .wait_processed_until(published_before_subscription, deadline)
                .await
        {
            let baseline = self.navigation_baseline_from(started, timeout, quiet).await;
            return Err(self
                .wait_failure(
                    "typed wait timed out synchronizing pre-subscription event-log state",
                    "wait",
                    DispatchState::Prevented,
                    wait_policy,
                    wait_policy,
                    WaitOutcome::TimedOut,
                    &observed,
                    &baseline,
                    vec![format!(
                        "EventLog did not process CDP event sequence {published_before_subscription}"
                    )],
                )
                .await);
        }
        let mut baseline = self.navigation_baseline_from(started, timeout, quiet).await;
        baseline.event_sequence = published_before_subscription;
        observed.last_event_sequence = published_before_subscription;
        observed.causal_event_floor = published_before_subscription;
        let mut quiet_since = None;
        let mut last_url = String::new();
        let mut last_generation = 0;
        let mut last_activity_revision = self.events.activity().revision;
        loop {
            if let Err(failure) = self
                .synchronize_router_evidence(&mut events, &action_target, &mut observed, deadline)
                .await
            {
                return Err(self
                    .wait_failure(
                        failure.message,
                        "wait",
                        DispatchState::Prevented,
                        wait_policy,
                        wait_policy,
                        failure.outcome,
                        &observed,
                        &baseline,
                        failure.blockers,
                    )
                    .await);
            }
            let (url, ready) = match self.stability_probe(deadline, "typed wait probe").await {
                Ok(probe) => probe,
                Err(failure) => {
                    let mut blockers = failure.blockers;
                    self.append_current_stability_blockers(&mut blockers);
                    return Err(self
                        .wait_failure(
                            failure.message,
                            "wait",
                            DispatchState::Prevented,
                            wait_policy,
                            wait_policy,
                            failure.outcome,
                            &observed,
                            &baseline,
                            blockers,
                        )
                        .await);
                }
            };
            let generation = self.generation();
            let activity = self.events.activity();
            let settlement = self.router.settlement();
            #[cfg(debug_assertions)]
            super::test_support::hold_stable_poll_if_requested(
                &self.session_id,
                baseline.started.elapsed(),
            )
            .await;
            let url_ok = conditions
                .url
                .as_ref()
                .is_none_or(|p| wildcard_match(p, &url));
            let generation_ok = conditions.generation_after.is_none_or(|g| generation > g);
            let load_ok = !conditions.load || ready;
            let unchanged = stability_evidence_unchanged(
                last_generation,
                &last_url,
                last_activity_revision,
                generation,
                &url,
                activity.revision,
            );
            let stable_ok = if conditions.stable
                && stable_candidate(ready, activity.active_finite, unchanged, settlement)
            {
                quiet_since
                    .get_or_insert_with(tokio::time::Instant::now)
                    .elapsed()
                    >= quiet
            } else if conditions.stable {
                quiet_since = None;
                false
            } else {
                true
            };
            let event_stream_gaps = self.events.event_stream_gaps();
            if event_stream_gaps != baseline.gaps
                || (conditions.stable && !event_evidence_complete(event_stream_gaps))
            {
                return Err(self
                    .wait_failure(
                        "typed wait evidence is incomplete",
                        "wait",
                        DispatchState::Prevented,
                        wait_policy,
                        wait_policy,
                        WaitOutcome::Incomplete,
                        &observed,
                        &baseline,
                        vec!["upstream CDP event gap".into()],
                    )
                    .await);
            }
            if url_ok && generation_ok && load_ok && stable_ok {
                if let Err(failure) = self
                    .synchronize_router_evidence(
                        &mut events,
                        &action_target,
                        &mut observed,
                        deadline,
                    )
                    .await
                {
                    return Err(self
                        .wait_failure(
                            failure.message,
                            "wait",
                            DispatchState::Prevented,
                            wait_policy,
                            wait_policy,
                            failure.outcome,
                            &observed,
                            &baseline,
                            failure.blockers,
                        )
                        .await);
                }
                if let Err(failure) = self.refresh_and_settle_targets(deadline).await {
                    return Err(self
                        .wait_failure(
                            failure.message,
                            "wait",
                            DispatchState::Prevented,
                            wait_policy,
                            wait_policy,
                            failure.outcome,
                            &observed,
                            &baseline,
                            failure.blockers,
                        )
                        .await);
                }
                if self.generation() != generation {
                    quiet_since = None;
                    last_url = url;
                    last_generation = self.generation();
                    last_activity_revision = self.events.activity().revision;
                    continue;
                }
                if conditions.stable && !self.two_animation_frames(deadline).await {
                    quiet_since = None;
                    continue;
                }
                let receipt_url;
                let receipt_generation;
                let receipt_cursor;
                let receipt_settlement;
                let mut stable_epoch = None;
                if conditions.stable {
                    let final_cursor_before = match self
                        .synchronize_router_evidence(
                            &mut events,
                            &action_target,
                            &mut observed,
                            deadline,
                        )
                        .await
                    {
                        Ok(sequence) => sequence,
                        Err(failure) => {
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    "wait",
                                    DispatchState::Prevented,
                                    wait_policy,
                                    wait_policy,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    failure.blockers,
                                )
                                .await)
                        }
                    };
                    let (final_url, final_ready) = match self
                        .stability_probe(deadline, "typed wait final revalidation")
                        .await
                    {
                        Ok(probe) => probe,
                        Err(failure) => {
                            let mut blockers = failure.blockers;
                            self.append_current_stability_blockers(&mut blockers);
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    "wait",
                                    DispatchState::Prevented,
                                    wait_policy,
                                    wait_policy,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    blockers,
                                )
                                .await);
                        }
                    };
                    let final_event_sequence = match self
                        .synchronize_router_evidence(
                            &mut events,
                            &action_target,
                            &mut observed,
                            deadline,
                        )
                        .await
                    {
                        Ok(sequence) => sequence,
                        Err(failure) => {
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    "wait",
                                    DispatchState::Prevented,
                                    wait_policy,
                                    wait_policy,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    failure.blockers,
                                )
                                .await)
                        }
                    };
                    let final_activity = self.events.activity();
                    if !event_evidence_complete(self.events.event_stream_gaps()) {
                        return Err(self
                            .wait_failure(
                                "typed wait evidence changed during final revalidation",
                                "wait",
                                DispatchState::Prevented,
                                wait_policy,
                                wait_policy,
                                WaitOutcome::Incomplete,
                                &observed,
                                &baseline,
                                vec!["upstream CDP event gap after animation-frame revalidation"
                                    .into()],
                            )
                            .await);
                    }
                    if final_event_sequence != final_cursor_before
                        || self.generation() != generation
                        || final_url != url
                        || !final_ready
                        || final_activity.active_finite != 0
                        || final_activity.revision != activity.revision
                        || !self.router.settlement().allows_stability()
                    {
                        quiet_since = None;
                        last_url = final_url;
                        last_generation = self.generation();
                        continue;
                    }
                    stable_epoch = Some((final_activity.revision, final_event_sequence));
                    receipt_url = final_url;
                    receipt_generation = self.generation();
                    receipt_cursor = final_event_sequence;
                    receipt_settlement = self.router.settlement();
                } else {
                    let condition_cursor_before = match self
                        .synchronize_router_evidence(
                            &mut events,
                            &action_target,
                            &mut observed,
                            deadline,
                        )
                        .await
                    {
                        Ok(sequence) => sequence,
                        Err(failure) => {
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    "wait",
                                    DispatchState::Prevented,
                                    wait_policy,
                                    wait_policy,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    failure.blockers,
                                )
                                .await)
                        }
                    };
                    let (final_url, final_ready) = match self
                        .stability_probe(deadline, "typed wait final condition probe")
                        .await
                    {
                        Ok(probe) => probe,
                        Err(failure) => {
                            let mut blockers = failure.blockers;
                            self.append_current_stability_blockers(&mut blockers);
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    "wait",
                                    DispatchState::Prevented,
                                    wait_policy,
                                    wait_policy,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    blockers,
                                )
                                .await);
                        }
                    };
                    receipt_cursor = match self
                        .synchronize_router_evidence(
                            &mut events,
                            &action_target,
                            &mut observed,
                            deadline,
                        )
                        .await
                    {
                        Ok(sequence) => sequence,
                        Err(failure) => {
                            return Err(self
                                .wait_failure(
                                    failure.message,
                                    "wait",
                                    DispatchState::Prevented,
                                    wait_policy,
                                    wait_policy,
                                    failure.outcome,
                                    &observed,
                                    &baseline,
                                    failure.blockers,
                                )
                                .await)
                        }
                    };
                    receipt_generation = self.generation();
                    receipt_settlement = self.router.settlement();
                    let final_url_ok = conditions
                        .url
                        .as_ref()
                        .is_none_or(|pattern| wildcard_match(pattern, &final_url));
                    let final_generation_ok = conditions
                        .generation_after
                        .is_none_or(|required| receipt_generation > required);
                    let final_load_ok =
                        !conditions.load || (final_ready && !receipt_settlement.root_loading);
                    if receipt_cursor != condition_cursor_before
                        || !final_url_ok
                        || !final_generation_ok
                        || !final_load_ok
                    {
                        quiet_since = None;
                        last_url = final_url;
                        last_generation = receipt_generation;
                        last_activity_revision = self.events.activity().revision;
                        continue;
                    }
                    receipt_url = final_url;
                }
                let (observed_navigation, observed_scope) =
                    observed_navigation_dimensions(&observed);
                let mut receipt = self
                    .finish_receipt(
                        "wait",
                        DispatchState::Prevented,
                        wait_policy,
                        wait_policy,
                        if conditions.stable {
                            WaitOutcome::Stable
                        } else if conditions.load {
                            WaitOutcome::Loaded
                        } else {
                            WaitOutcome::ConditionsMet
                        },
                        observed_navigation,
                        observed_scope,
                        &baseline,
                        Vec::new(),
                    )
                    .await;
                if self.client.latest_published_event_sequence() != receipt_cursor
                    || !self.router.processed_through(receipt_cursor)
                    || !self.events.processed_through(receipt_cursor)
                    || self.router.settlement() != receipt_settlement
                    || !receipt.target_settled
                    || (conditions.load && receipt.root_loading)
                    || receipt.final_generation != receipt_generation
                    || (receipt.final_url_observed
                        && receipt.final_url != redact::url(&receipt_url))
                {
                    quiet_since = None;
                    continue;
                }
                if let Some((activity_revision, event_sequence)) = stable_epoch {
                    if self.events.activity().revision != activity_revision
                        || self.client.latest_published_event_sequence() != event_sequence
                        || !self.router.settlement().allows_stability()
                    {
                        quiet_since = None;
                        continue;
                    }
                }
                // `url` came from the successful final Runtime probe above, so
                // it is stronger evidence than a best-effort post-deadline
                // lookup inside `finish_receipt`.
                receipt.final_url = redact::url(&receipt_url);
                receipt.final_url_observed = true;
                if let Some(pattern) = &conditions.url {
                    receipt.observed_conditions.insert(
                        "url".into(),
                        json!({
                            "pattern": redact::url_glob(pattern),
                            "observed": redact::url(&receipt_url),
                            "matched": true
                        }),
                    );
                }
                if let Some(after) = conditions.generation_after {
                    receipt.observed_conditions.insert(
                        "generation_after".into(),
                        json!({"required": after, "observed": receipt_generation, "matched": true}),
                    );
                }
                if conditions.load {
                    receipt.observed_conditions.insert(
                        "load".into(),
                        json!({"ready_state": "complete", "matched": true}),
                    );
                }
                if conditions.stable {
                    receipt.observed_conditions.insert(
                        "stable".into(),
                        json!({"quiet_ms": quiet.as_millis(), "active_finite_requests": 0, "matched": true}),
                    );
                }
                return Ok(receipt);
            }
            if tokio::time::Instant::now() >= deadline {
                let mut blockers = Vec::new();
                if !url_ok {
                    blockers.push(format!("URL {:?} does not match", redact::url(&url)));
                }
                if !generation_ok {
                    blockers.push(format!("generation is {generation}"));
                }
                if !load_ok {
                    blockers.push("document.readyState is not complete".into());
                }
                if !stable_ok {
                    blockers.push(format!(
                        "{} finite request(s) active or quiet interval incomplete",
                        activity.active_finite
                    ));
                    if settlement.root_loading {
                        blockers.push("root document is still loading".into());
                    }
                    if !settlement.target_settled {
                        blockers.push("one or more related targets are not settled".into());
                    }
                }
                let mut error = self
                    .wait_failure(
                        "typed wait timed out",
                        "wait",
                        DispatchState::Prevented,
                        wait_policy,
                        wait_policy,
                        WaitOutcome::TimedOut,
                        &observed,
                        &baseline,
                        blockers,
                    )
                    .await;
                if let PageError::WaitFailure { receipt, .. } = &mut error {
                    if let Some(pattern) = &conditions.url {
                        receipt.observed_conditions.insert(
                            "url".into(),
                            json!({
                                "pattern": redact::url_glob(pattern),
                                "observed": redact::url(&url),
                                "matched": url_ok
                            }),
                        );
                    }
                    if let Some(after) = conditions.generation_after {
                        receipt.observed_conditions.insert(
                            "generation_after".into(),
                            json!({"required": after, "observed": generation, "matched": generation_ok}),
                        );
                    }
                    if conditions.load {
                        receipt.observed_conditions.insert(
                            "load".into(),
                            json!({
                                "ready_state": if ready { "complete" } else { "not_complete" },
                                "matched": load_ok
                            }),
                        );
                    }
                    if conditions.stable {
                        receipt.observed_conditions.insert(
                            "stable".into(),
                            json!({
                                "quiet_ms": quiet.as_millis(),
                                "active_finite_requests": activity.active_finite,
                                "matched": stable_ok
                            }),
                        );
                    }
                }
                return Err(error);
            }
            last_url = url;
            last_generation = generation;
            last_activity_revision = activity.revision;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                continue;
            }
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }

    fn append_current_stability_blockers(&self, blockers: &mut Vec<String>) {
        let activity = self.events.activity();
        let settlement = self.router.settlement();
        if settlement.root_loading {
            blockers.push("root document is still loading".into());
        }
        if !settlement.target_settled {
            blockers.push("one or more related targets are not settled".into());
        }
        if activity.active_finite != 0 {
            blockers.push(format!(
                "{} finite request(s) are still active",
                activity.active_finite
            ));
        }
    }

    pub async fn traverse_history(
        &self,
        delta: i64,
        requested: Option<WaitPolicy>,
        timeout: Duration,
    ) -> Result<ActionReceipt, PageError> {
        let operation = if delta < 0 { "back" } else { "forward" };
        let requested = requested.unwrap_or(WaitPolicy::Load);
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let history = match self
            .client
            .call_on(&self.session_id, "Page.getNavigationHistory", json!({}))
            .await
        {
            Ok(history) => history,
            Err(error) => {
                return Err(self
                    .operation_failure(
                        PageError::Cdp(error),
                        operation,
                        DispatchState::Prevented,
                        requested,
                        requested,
                        &baseline,
                    )
                    .await)
            }
        };
        let Some(index) = history.get("currentIndex").and_then(Value::as_i64) else {
            return Err(self
                .operation_failure(
                    PageError::Navigation {
                        url: baseline.url.clone(),
                        reason: "navigation history response has no currentIndex".into(),
                    },
                    operation,
                    DispatchState::Prevented,
                    requested,
                    requested,
                    &baseline,
                )
                .await);
        };
        let Some(entries) = history.get("entries").and_then(Value::as_array) else {
            return Err(self
                .operation_failure(
                    PageError::Navigation {
                        url: baseline.url.clone(),
                        reason: "navigation history response has no entries array".into(),
                    },
                    operation,
                    DispatchState::Prevented,
                    requested,
                    requested,
                    &baseline,
                )
                .await);
        };
        let next = index + delta;
        if next < 0 || next as usize >= entries.len() {
            let mut receipt = self
                .finish_receipt(
                    operation,
                    DispatchState::Prevented,
                    requested,
                    requested,
                    WaitOutcome::NotWaited,
                    NavigationKind::None,
                    NavigationScope::None,
                    &baseline,
                    vec!["no adjacent history entry".into()],
                )
                .await;
            receipt.history_from_index = Some(index);
            receipt.history_to_index = Some(next);
            return Err(PageError::NoHistoryEntry {
                current_index: index,
                entry_count: entries.len(),
                receipt: Box::new(receipt),
            });
        }
        let current_entry_id = match history_entry_id(entries, index) {
            Some(id) => id,
            None => {
                return Err(self
                    .operation_failure(
                        PageError::Navigation {
                            url: baseline.url.clone(),
                            reason: "current history entry has no id".into(),
                        },
                        operation,
                        DispatchState::Prevented,
                        requested,
                        requested,
                        &baseline,
                    )
                    .await)
            }
        };
        let entry_id = match history_entry_id(entries, next) {
            Some(id) => id,
            None => {
                return Err(self
                    .operation_failure(
                        PageError::Navigation {
                            url: baseline.url.clone(),
                            reason: "history entry has no id".into(),
                        },
                        operation,
                        DispatchState::Prevented,
                        requested,
                        requested,
                        &baseline,
                    )
                    .await)
            }
        };
        let selected_generation = self.generation();

        #[cfg(debug_assertions)]
        super::test_support::hold_history_before_revalidation_if_requested(&self.session_id).await;

        let current_history = match self
            .client
            .call_on(&self.session_id, "Page.getNavigationHistory", json!({}))
            .await
        {
            Ok(history) => history,
            Err(error) => {
                return Err(self
                    .operation_failure(
                        PageError::Cdp(error),
                        operation,
                        DispatchState::Prevented,
                        requested,
                        requested,
                        &baseline,
                    )
                    .await)
            }
        };
        if self.generation() != selected_generation
            || !history_selection_is_current(
                &current_history,
                index,
                current_entry_id,
                next,
                entry_id,
                delta,
            )
        {
            let mut error = self
                .operation_failure(
                    PageError::Navigation {
                        url: baseline.url.clone(),
                        reason: "navigation history changed before traversal dispatch; retry from the current page state"
                            .into(),
                    },
                    operation,
                    DispatchState::Prevented,
                    requested,
                    requested,
                    &baseline,
                )
                .await;
            if let PageError::WaitFailure { receipt, .. } = &mut error {
                receipt.history_entry_id = Some(entry_id);
                receipt.history_from_index = Some(index);
                receipt.history_to_index = Some(next);
            }
            return Err(error);
        }
        let action_target = self.root_action_target();
        let mut dispatch = input::DispatchTracker::default();
        let causal_event_floor = self.client.latest_published_event_sequence();
        if self.generation() != selected_generation {
            let mut error = self
                .operation_failure(
                    PageError::Navigation {
                        url: baseline.url.clone(),
                        reason: "page generation changed after history revalidation; traversal was not dispatched"
                            .into(),
                    },
                    operation,
                    DispatchState::Prevented,
                    requested,
                    requested,
                    &baseline,
                )
                .await;
            if let PageError::WaitFailure { receipt, .. } = &mut error {
                receipt.history_entry_id = Some(entry_id);
                receipt.history_from_index = Some(index);
                receipt.history_to_index = Some(next);
            }
            return Err(error);
        }
        dispatch.note_event_floor(causal_event_floor);
        let command = self
            .client
            .call_on(
                &self.session_id,
                "Page.navigateToHistoryEntry",
                json!({ "entryId": entry_id }),
            )
            .await;
        if let Err(error) = input::track_action_call(&mut dispatch, command) {
            return Err(self
                .operation_failure(
                    PageError::Cdp(error),
                    operation,
                    dispatch.state(),
                    requested,
                    requested,
                    &baseline,
                )
                .await);
        }
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        let result = self
            .settle_after_dispatch(
                operation,
                dispatch.state(),
                requested,
                requested,
                timeout,
                Duration::from_millis(300),
                baseline,
                dispatched_at,
                events,
                action_target,
            )
            .await;
        match result {
            Ok(mut receipt) => {
                if receipt.navigation == NavigationKind::SameDocument {
                    receipt.effective_wait = WaitPolicy::Commit;
                }
                receipt.history_entry_id = Some(entry_id);
                receipt.history_from_index = Some(index);
                receipt.history_to_index = Some(next);
                Ok(receipt)
            }
            Err(mut error) => {
                if let PageError::WaitFailure { receipt, .. } = &mut error {
                    receipt.history_entry_id = Some(entry_id);
                    receipt.history_from_index = Some(index);
                    receipt.history_to_index = Some(next);
                }
                Err(error)
            }
        }
    }

    pub async fn prepare_checkpoint(
        &self,
        policy: WaitPolicy,
        timeout: Duration,
        quiet: Duration,
    ) -> Result<ActionReceipt, PageError> {
        match policy {
            WaitPolicy::Stable => {
                self.wait_for_conditions(
                    &WaitConditions {
                        stable: true,
                        ..WaitConditions::default()
                    },
                    timeout,
                    quiet,
                )
                .await
            }
            WaitPolicy::Load => {
                self.wait_for_conditions(
                    &WaitConditions {
                        load: true,
                        ..WaitConditions::default()
                    },
                    timeout,
                    quiet,
                )
                .await
            }
            WaitPolicy::Commit | WaitPolicy::None => {
                let baseline = self.navigation_baseline(timeout, quiet).await;
                let mut receipt = self
                    .finish_receipt(
                        "checkpoint",
                        DispatchState::Prevented,
                        policy,
                        policy,
                        if policy == WaitPolicy::None {
                            WaitOutcome::NotWaited
                        } else {
                            WaitOutcome::Committed
                        },
                        NavigationKind::None,
                        NavigationScope::None,
                        &baseline,
                        Vec::new(),
                    )
                    .await;
                if policy == WaitPolicy::Commit && !receipt.final_url_observed {
                    receipt.outcome = WaitOutcome::Incomplete;
                    receipt
                        .blockers
                        .push("current document URL could not be observed".into());
                    return Err(PageError::WaitFailure {
                        message: "checkpoint commit precondition could not prove the current document URL"
                            .into(),
                        receipt: Box::new(receipt),
                    });
                }
                Ok(receipt)
            }
            WaitPolicy::Auto | WaitPolicy::Conditions => Err(PageError::Navigation {
                url: self
                    .location()
                    .await
                    .map(|value| redacted_navigation_error_url(&value.0))
                    .unwrap_or_default(),
                reason: "auto/conditions wait is not valid for checkpoint preconditions".into(),
            }),
        }
    }

    pub async fn reload_with_wait(
        &self,
        ignore_cache: bool,
        requested: Option<WaitPolicy>,
        timeout: Duration,
    ) -> Result<ActionReceipt, PageError> {
        let requested = requested.unwrap_or(WaitPolicy::Load);
        let (mut baseline, events) = self
            .subscribed_navigation_baseline(timeout, Duration::from_millis(300))
            .await?;
        let tree = match self
            .client
            .call_on(&self.session_id, "Page.getFrameTree", json!({}))
            .await
        {
            Ok(tree) => tree,
            Err(error) => {
                return Err(self
                    .operation_failure(
                        PageError::Cdp(error),
                        "reload",
                        DispatchState::Prevented,
                        requested,
                        requested,
                        &baseline,
                    )
                    .await)
            }
        };
        let loader_id = tree
            .pointer("/frameTree/frame/loaderId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let action_target = self.root_action_target();
        let mut params = json!({ "ignoreCache": ignore_cache });
        if let Some(loader_id) = &loader_id {
            params["loaderId"] = Value::String(loader_id.clone());
        }
        let mut dispatch = input::DispatchTracker::default();
        dispatch.note_event_floor(self.client.latest_published_event_sequence());
        let command = self
            .client
            .call_on(&self.session_id, "Page.reload", params)
            .await;
        if let Err(error) = input::track_action_call(&mut dispatch, command) {
            return Err(self
                .operation_failure(
                    PageError::Cdp(error),
                    "reload",
                    dispatch.state(),
                    requested,
                    requested,
                    &baseline,
                )
                .await);
        }
        baseline.causal_event_floor = dispatch.event_floor().unwrap_or(baseline.event_sequence);
        let dispatched_at = Instant::now();
        let result = self
            .settle_after_dispatch(
                "reload",
                dispatch.state(),
                requested,
                requested,
                timeout,
                Duration::from_millis(300),
                baseline,
                dispatched_at,
                events,
                action_target,
            )
            .await;
        match result {
            Ok(mut receipt) => {
                receipt.reload_loader_id = loader_id;
                Ok(receipt)
            }
            Err(mut error) => {
                if let PageError::WaitFailure { receipt, .. } = &mut error {
                    receipt.reload_loader_id = loader_id;
                }
                Err(error)
            }
        }
    }

    pub async fn park_pointer(&self) -> Result<PointerParkReceipt, PageError> {
        let baseline = self
            .navigation_baseline(Duration::ZERO, Duration::ZERO)
            .await;
        let point = Point { x: -1.0, y: -1.0 };
        let mut dispatch = input::DispatchTracker::default();
        if let Err(error) =
            input::move_pointer_tracked(&self.client, &self.session_id, point, 0, &mut dispatch)
                .await
        {
            return Err(self
                .operation_failure(
                    PageError::Cdp(error),
                    "pointer_park",
                    dispatch.state(),
                    WaitPolicy::None,
                    WaitPolicy::None,
                    &baseline,
                )
                .await);
        }
        let action = self
            .finish_receipt(
                "pointer_park",
                dispatch.state(),
                WaitPolicy::None,
                WaitPolicy::None,
                WaitOutcome::NotWaited,
                NavigationKind::None,
                NavigationScope::None,
                &baseline,
                Vec::new(),
            )
            .await;
        Ok(PointerParkReceipt {
            action,
            before: None,
            after: point,
            x: point.x,
            y: point.y,
            warning: "trusted pointer movement may run hover and mouseleave handlers".into(),
        })
    }
}

fn action_event_is_local(
    action_target: &ActionTarget,
    _event_session: &str,
    event_frame: Option<&str>,
    session_root_frame: Option<&str>,
    action_session_root: Option<&str>,
) -> bool {
    match event_frame {
        Some(frame_id) => frame_id == action_target.frame_id,
        None => [session_root_frame, action_session_root]
            .into_iter()
            .flatten()
            .any(|root| root == action_target.frame_id),
    }
}

fn event_is_session_root(
    action_target: &ActionTarget,
    event_frame: Option<&str>,
    session_root_frame: Option<&str>,
    has_parent: bool,
) -> bool {
    event_frame.is_none_or(|frame_id| {
        session_root_frame == Some(frame_id)
            || !has_parent
            // The router can consume a later detach before this independent
            // action receiver reaches an earlier queued navigation. In that
            // case its current session lookup is gone, but the event remains
            // causal when it belongs to an action-owned session and names the
            // exact frame that was acted upon.
            || frame_id == action_target.frame_id
    })
}

fn update_action_session_membership(
    event: &CdpEvent,
    action_target: &ActionTarget,
    sessions: &mut HashMap<String, String>,
) {
    match event.method.as_str() {
        "Target.attachedToTarget"
            if event
                .params
                .get("targetInfo")
                .and_then(|target| target.get("targetId"))
                .and_then(Value::as_str)
                == Some(action_target.frame_id.as_str()) =>
        {
            if let Some(session) = event.params.get("sessionId").and_then(Value::as_str) {
                sessions.insert(session.to_string(), action_target.frame_id.clone());
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(session) = event.params.get("sessionId").and_then(Value::as_str) {
                sessions.remove(session);
            }
        }
        _ => {}
    }
}

fn completed_document_redirect(params: &Value) -> bool {
    params.get("redirectResponse").is_some_and(Value::is_object)
        && params
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "Document")
}

fn document_navigation_request(params: &Value) -> bool {
    params
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "Document")
}

fn navigation_requires_generation_ack(navigation: Option<NavigationKind>) -> bool {
    matches!(
        navigation,
        Some(
            NavigationKind::SameDocument
                | NavigationKind::CrossDocument
                | NavigationKind::History
                | NavigationKind::Reload
        )
    )
}

fn stability_evidence_unchanged(
    last_generation: u64,
    last_url: &str,
    last_activity_revision: u64,
    generation: u64,
    url: &str,
    activity_revision: u64,
) -> bool {
    generation == last_generation && url == last_url && activity_revision == last_activity_revision
}

fn stable_candidate(
    ready: bool,
    active_finite: usize,
    unchanged: bool,
    settlement: TargetSettlement,
) -> bool {
    ready && active_finite == 0 && unchanged && settlement.allows_stability()
}

fn target_settlement_failure(
    settlement: TargetSettlement,
    deadline_reached: bool,
    coverage_gaps: Vec<super::CoverageGap>,
) -> Option<SettlementFailure> {
    if settlement.target_settled {
        return None;
    }
    let mut blockers = vec!["one or more related targets are not settled".into()];
    blockers.extend(
        coverage_gaps
            .into_iter()
            .take(5)
            .map(|gap| format!("target coverage gap: {}", gap.reason)),
    );
    Some(SettlementFailure::new(
        if deadline_reached {
            "target tree did not settle before the shared action deadline"
        } else {
            "target tree settlement is incomplete"
        },
        if deadline_reached {
            WaitOutcome::TimedOut
        } else {
            WaitOutcome::Incomplete
        },
        blockers,
    ))
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    let (mut pi, mut vi, mut star, mut retry) = (0usize, 0usize, None, 0usize);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            retry = vi;
        } else if let Some(s) = star {
            pi = s + 1;
            retry += 1;
            vi = retry;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn merge_observed_evidence(
    complete: &mut bool,
    gap_delta: &mut u64,
    redirect_count: &mut u32,
    observed: &Observed,
) {
    if observed.event_incomplete {
        *complete = false;
        *gap_delta = (*gap_delta).max(observed.local_event_gap);
    }
    *redirect_count = observed.redirect_count;
}

fn navigation_trigger(operation: &str) -> NavigationTrigger {
    match operation {
        "back" | "forward" => NavigationTrigger::History,
        "reload" => NavigationTrigger::Reload,
        "wait" => NavigationTrigger::Wait,
        "pointer_park" => NavigationTrigger::PointerPark,
        "checkpoint" => NavigationTrigger::Checkpoint,
        _ => NavigationTrigger::Input,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        action_event_is_local, begin_cross_document_observation, completed_document_redirect,
        event_evidence_complete, event_is_action_causal, event_is_session_root,
        has_unfinished_cross_document_navigation, history_selection_is_current,
        mark_load_if_causally_committed, merge_observed_evidence,
        navigation_requires_generation_ack, navigation_trigger, observed_navigation_dimensions,
        redacted_dialog_message, redacted_navigation_error_url, stability_evidence_unchanged,
        stable_candidate, target_settlement_failure, update_action_session_membership,
        wildcard_match, ActionTarget, NavigationKind, NavigationScope, NavigationTrigger, Observed,
    };
    use crate::cdp::CdpEvent;
    use crate::page::{CoverageGap, TargetSettlement, WaitOutcome};
    use serde_json::json;

    #[test]
    fn url_glob_matches_full_serialized_url() {
        assert!(wildcard_match(
            "https://host/issues/*",
            "https://host/issues/42"
        ));
        assert!(wildcard_match("*?tab=code", "https://host/r?tab=code"));
        assert!(!wildcard_match(
            "https://host/issues/*",
            "https://other/issues/42"
        ));
    }

    #[test]
    fn action_local_subscriber_loss_overrides_a_complete_global_log() {
        let mut complete = true;
        let mut gap_delta = 0;
        let mut redirect_count = 0;
        merge_observed_evidence(
            &mut complete,
            &mut gap_delta,
            &mut redirect_count,
            &Observed {
                event_incomplete: true,
                local_event_gap: 7,
                redirect_count: 2,
                ..Observed::default()
            },
        );
        assert!(!complete);
        assert_eq!(gap_delta, 7);
        assert_eq!(redirect_count, 2);
    }

    #[test]
    fn navigation_trigger_and_redirect_evidence_are_orthogonal() {
        assert_eq!(navigation_trigger("click"), NavigationTrigger::Input);
        assert_eq!(navigation_trigger("back"), NavigationTrigger::History);
        assert_eq!(navigation_trigger("reload"), NavigationTrigger::Reload);
        assert!(completed_document_redirect(&json!({
            "type": "Document",
            "redirectResponse": {"status": 302}
        })));
        assert!(!completed_document_redirect(&json!({
            "type": "Image",
            "redirectResponse": {"status": 302}
        })));
    }

    #[test]
    fn predispatch_events_are_context_not_action_causality() {
        assert!(!event_is_action_causal(50, 50));
        assert!(!event_is_action_causal(49, 50));
        assert!(event_is_action_causal(51, 50));
    }

    #[test]
    fn a_new_document_start_revokes_the_previous_commit_before_load() {
        let mut observed = Observed {
            navigation: Some(NavigationKind::CrossDocument),
            scope: NavigationScope::Root,
            committed: true,
            loaded: true,
            commit_event_sequence: Some(10),
            load_event_sequence: Some(11),
            ..Observed::default()
        };

        begin_cross_document_observation(
            &mut observed,
            NavigationKind::CrossDocument,
            NavigationScope::Root,
            false,
        );
        assert!(!observed.committed);
        assert!(!observed.loaded);
        assert_eq!(observed.commit_event_sequence, None);
        assert_eq!(observed.load_event_sequence, None);
        assert!(
            !mark_load_if_causally_committed(&mut observed, 12, Some("old-loader")),
            "navigation B must not borrow navigation A's commit"
        );

        observed.committed = true;
        observed.commit_event_sequence = Some(13);
        observed.commit_loader_id = Some("new-loader".into());
        assert!(
            !mark_load_if_causally_committed(&mut observed, 14, Some("old-loader")),
            "a late lifecycle load from the old loader must be ignored"
        );
        assert!(!observed.loaded);
        assert!(mark_load_if_causally_committed(
            &mut observed,
            15,
            Some("new-loader")
        ));
        assert!(observed.loaded);
        assert_eq!(observed.load_event_sequence, Some(15));

        observed.loaded = false;
        begin_cross_document_observation(
            &mut observed,
            NavigationKind::CrossDocument,
            NavigationScope::Root,
            true,
        );
        assert!(
            observed.committed,
            "Chrome may report frameStartedLoading after the matching frameNavigated"
        );
        assert_eq!(observed.commit_event_sequence, Some(13));
    }

    #[test]
    fn same_document_event_cannot_complete_an_active_cross_document_loader() {
        let observed = Observed {
            navigation: Some(NavigationKind::CrossDocument),
            scope: NavigationScope::Root,
            committed: true,
            loaded: false,
            commit_event_sequence: Some(20),
            commit_loader_id: Some("active-loader".into()),
            ..Observed::default()
        };
        assert!(has_unfinished_cross_document_navigation(&observed));

        let ordinary_same_document = Observed {
            navigation: Some(NavigationKind::SameDocument),
            committed: true,
            loaded: true,
            ..Observed::default()
        };
        assert!(!has_unfinished_cross_document_navigation(
            &ordinary_same_document
        ));
    }

    #[test]
    fn old_event_gap_never_becomes_complete_at_a_later_baseline() {
        let gaps_before_baseline = 1;
        assert!(!event_evidence_complete(gaps_before_baseline));
        assert!(event_evidence_complete(0));
    }

    #[test]
    fn generation_change_without_lifecycle_has_no_navigation_classification() {
        let observed = Observed::default();
        assert_eq!(
            observed_navigation_dimensions(&observed),
            (NavigationKind::None, NavigationScope::None)
        );
    }

    #[test]
    fn history_revalidation_requires_the_same_current_and_adjacent_entries() {
        let selected = json!({
            "currentIndex": 1,
            "entries": [{"id": 10}, {"id": 20}, {"id": 30}]
        });
        assert!(history_selection_is_current(&selected, 1, 20, 0, 10, -1));

        let pushed = json!({
            "currentIndex": 2,
            "entries": [{"id": 10}, {"id": 20}, {"id": 40}]
        });
        assert!(!history_selection_is_current(&pushed, 1, 20, 0, 10, -1));

        let replaced_target = json!({
            "currentIndex": 1,
            "entries": [{"id": 99}, {"id": 20}, {"id": 30}]
        });
        assert!(!history_selection_is_current(
            &replaced_target,
            1,
            20,
            0,
            10,
            -1
        ));
    }

    #[test]
    fn dialog_evidence_redacts_durable_credentials() {
        let message = redacted_dialog_message(
            "access_token=secret at https://user:password@example.test/path",
        );
        assert!(!message.contains("secret"));
        assert!(!message.contains("user:password"));
        assert!(message.contains("[redacted]"));
    }

    #[test]
    fn navigation_error_display_redacts_location_credentials() {
        let error = crate::page::PageError::Navigation {
            url: redacted_navigation_error_url(
                "https://user:password@example.test/path?access_token=secret",
            ),
            reason: "precondition failed".into(),
        };
        let rendered = error.to_string();
        assert!(!rendered.contains("user:password"));
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn action_navigation_causality_rejects_unrelated_oopif_lifecycle() {
        let root_action = ActionTarget {
            session_id: "root-session".into(),
            frame_id: "root-frame".into(),
        };
        assert!(action_event_is_local(
            &root_action,
            "root-session",
            Some("root-frame"),
            Some("root-frame"),
            Some("root-frame"),
        ));
        assert!(!action_event_is_local(
            &root_action,
            "oopif-session",
            Some("oopif-frame"),
            Some("oopif-frame"),
            None,
        ));

        let oopif_action = ActionTarget {
            session_id: "old-oopif-session".into(),
            frame_id: "oopif-frame".into(),
        };
        assert!(
            action_event_is_local(
                &oopif_action,
                "new-oopif-session",
                Some("oopif-frame"),
                Some("oopif-frame"),
                Some("oopif-frame"),
            ),
            "a renderer-session swap for the acted frame must retain causality"
        );

        assert!(
            event_is_session_root(
                &oopif_action,
                Some("oopif-frame"),
                None,
                true,
            ),
            "a queued acted-frame navigation remains causal after the router removes its detached session"
        );
        assert!(!event_is_session_root(
            &oopif_action,
            Some("other-frame"),
            None,
            true,
        ));
        assert!(navigation_requires_generation_ack(Some(
            NavigationKind::CrossDocument
        )));
        assert!(!navigation_requires_generation_ack(Some(
            NavigationKind::WindowOpen
        )));

        let same_process_child = ActionTarget {
            session_id: "root-session".into(),
            frame_id: "child-frame".into(),
        };
        assert!(!action_event_is_local(
            &same_process_child,
            "root-session",
            None,
            Some("root-frame"),
            Some("root-frame"),
        ));
        assert!(event_is_session_root(
            &same_process_child,
            Some("child-frame"),
            Some("root-frame"),
            true,
        ));
    }

    #[test]
    fn action_session_membership_follows_only_the_acted_frame_across_reattach() {
        let action = ActionTarget {
            session_id: "old-oopif-session".into(),
            frame_id: "acted-frame".into(),
        };
        let mut sessions = HashMap::from([(action.session_id.clone(), action.frame_id.clone())]);
        let attached = CdpEvent {
            method: "Target.attachedToTarget".into(),
            params: json!({
                "sessionId": "new-oopif-session",
                "targetInfo": {"targetId": "acted-frame", "type": "iframe"}
            }),
            session_id: None,
        };
        update_action_session_membership(&attached, &action, &mut sessions);
        assert_eq!(
            sessions.get("new-oopif-session").map(String::as_str),
            Some("acted-frame")
        );

        let unrelated = CdpEvent {
            method: "Target.attachedToTarget".into(),
            params: json!({
                "sessionId": "unrelated-session",
                "targetInfo": {"targetId": "other-frame", "type": "iframe"}
            }),
            session_id: None,
        };
        update_action_session_membership(&unrelated, &action, &mut sessions);
        assert!(!sessions.contains_key("unrelated-session"));

        let detached = CdpEvent {
            method: "Target.detachedFromTarget".into(),
            params: json!({"sessionId": "new-oopif-session"}),
            session_id: None,
        };
        update_action_session_membership(&detached, &action, &mut sessions);
        assert!(!sessions.contains_key("new-oopif-session"));
        assert!(sessions.contains_key("old-oopif-session"));
    }

    #[test]
    fn activity_revision_and_router_settlement_gate_the_quiet_window() {
        assert!(stability_evidence_unchanged(
            7,
            "https://x.test/",
            11,
            7,
            "https://x.test/",
            11
        ));
        assert!(!stability_evidence_unchanged(
            7,
            "https://x.test/",
            11,
            7,
            "https://x.test/",
            13
        ));

        let settled = TargetSettlement {
            root_loading: false,
            target_settled: true,
        };
        assert!(stable_candidate(true, 0, true, settled));
        assert!(!stable_candidate(
            true,
            0,
            true,
            TargetSettlement {
                root_loading: true,
                target_settled: true,
            }
        ));
        assert!(!stable_candidate(
            true,
            0,
            true,
            TargetSettlement {
                root_loading: false,
                target_settled: false,
            }
        ));
    }

    #[test]
    fn nonstable_action_fails_closed_when_target_tree_does_not_settle() {
        let settled = TargetSettlement {
            root_loading: true,
            target_settled: true,
        };
        assert!(target_settlement_failure(settled, false, Vec::new()).is_none());

        let incomplete = target_settlement_failure(
            TargetSettlement {
                root_loading: false,
                target_settled: false,
            },
            false,
            vec![CoverageGap {
                frame_id: Some("child-frame".into()),
                target_id: Some("child-target".into()),
                reason: "initializer failed".into(),
            }],
        )
        .expect("an unresolved target must fail closed");
        assert_eq!(incomplete.outcome, WaitOutcome::Incomplete);
        assert!(incomplete
            .blockers
            .iter()
            .any(|blocker| blocker.contains("initializer failed")));

        let timed_out = target_settlement_failure(
            TargetSettlement {
                root_loading: false,
                target_settled: false,
            },
            true,
            Vec::new(),
        )
        .expect("a held initializer must fail at the shared deadline");
        assert_eq!(timed_out.outcome, WaitOutcome::TimedOut);
    }
}
