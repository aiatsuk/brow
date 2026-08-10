//! Background jobs.
//!
//! # The decision this encodes
//!
//! **The daemon never calls a model.** A job is a deterministic plan executed by
//! heuristics, which are treated as the *lower bound* on competence rather than
//! as an attempt at judgement. When the heuristics are not enough, the job parks
//! and asks — and who it asks depends on what kind of question it is:
//!
//! * [`PendingKind::Decision`] — a semantic question the engine cannot settle
//!   ("three buttons say Continue; which one?"). The **agent** answers it, on its
//!   own schedule, with `brow job answer`.
//! * [`PendingKind::Approval`] — an action that cannot be undone (delete, pay,
//!   publish, send). A human operator is expected to answer with `brow job
//!   approve`; the current same-UID socket does not prove human presence.
//!
//! The alternative — embedding a model client in the daemon — was rejected: it
//! would put API credentials in a background process and break the local-first
//! guarantee that is the reason this tool exists.
//!
//! # What a job cannot do
//!
//! Survive a daemon restart. Closing the CDP pipe is Chromium's shutdown signal,
//! so a restart takes the browsers with it. Rather than pretend otherwise, jobs
//! that were interrupted are loaded back as [`JobState::Interrupted`] with their
//! log intact, and are not resumable. Making them resumable needs a supervisor
//! process per browser, which is deliberately not built.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result as AnyResult};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

use crate::checkpoint::{self, CheckpointOptions};
use crate::ipc::{WaitConditions, WaitPolicy};
use crate::page::{ImageFormat, MouseButton, NodeFingerprint, Page, ScreenshotTarget};

/// How long a parked job waits before giving up.
///
/// Approval is deliberately shorter: it is a human interrupt, and a stale
/// approval request is worse than none — the page it was reasoning about has
/// moved on.
pub const DECISION_TTL: Duration = Duration::from_secs(30 * 60);
pub const APPROVAL_TTL: Duration = Duration::from_secs(15 * 60);
const JOB_WAIT_TIMEOUT_MS: u64 = 30_000;
const TERMINAL_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(5);

pub type JobId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    /// Blocked on a semantic question. An agent answers.
    NeedsDecision,
    /// Blocked on something irreversible. Intended for manual approval.
    WaitingForApproval,
    Succeeded,
    Failed,
    Stopped,
    /// The daemon died underneath it. Terminal, not resumable.
    Interrupted,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Stopped | JobState::Interrupted
        )
    }

    pub fn is_parked(self) -> bool {
        matches!(self, JobState::NeedsDecision | JobState::WaitingForApproval)
    }

    pub fn label(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::NeedsDecision => "needs_decision",
            JobState::WaitingForApproval => "waiting_for_approval",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Stopped => "stopped",
            JobState::Interrupted => "interrupted",
        }
    }
}

/// One instruction in a plan.
///
/// The vocabulary is small and literal on purpose. A plan is written by an agent
/// that *has* a model; the daemon executing it must not need one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case")]
pub enum Step {
    Open {
        url: String,
    },
    Click {
        text: String,
    },
    Fill {
        field: String,
        value: String,
    },
    Press {
        chord: String,
    },
    Wait {
        ms: u64,
    },
    WaitUrl {
        glob: String,
    },
    WaitGenerationAfter {
        generation: u64,
    },
    WaitLoad,
    WaitStable {
        #[serde(default = "default_job_quiet_ms")]
        quiet_ms: u64,
    },
    Back,
    Forward,
    Reload {
        #[serde(default)]
        ignore_cache: bool,
    },
    PointerPark,
    Checkpoint {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Screenshot,
    /// Fails the job if the page has logged errors or a request has failed.
    CheckErrors,
}

impl Step {
    /// Parses one `--step` argument.
    pub fn parse(raw: &str) -> Result<Step, String> {
        let raw = raw.trim();
        let (verb, rest) = match raw.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (raw, ""),
        };
        let need = |what: &str| -> Result<(), String> {
            if rest.is_empty() {
                Err(format!("step `{verb}` needs {what}, e.g. `{verb} {what}`"))
            } else {
                Ok(())
            }
        };

        Ok(match verb.to_ascii_lowercase().as_str() {
            "open" => {
                need("<url>")?;
                Step::Open {
                    url: rest.to_string(),
                }
            }
            "click" => {
                need("<visible text>")?;
                Step::Click {
                    text: unquote(rest),
                }
            }
            "fill" => {
                need("<field>=<value>")?;
                let (field, value) = rest
                    .split_once('=')
                    .ok_or_else(|| format!("step `fill` wants <field>=<value>, got `{rest}`"))?;
                Step::Fill {
                    field: unquote(field.trim()),
                    value: unquote(value.trim()),
                }
            }
            "press" => {
                need("<key or chord>")?;
                Step::Press {
                    chord: rest.to_string(),
                }
            }
            "wait" => {
                need("<milliseconds>")?;
                let (kind, value) = rest
                    .split_once(char::is_whitespace)
                    .map(|(kind, value)| (kind, value.trim()))
                    .unwrap_or((rest, ""));
                match kind {
                    "url" => {
                        if value.is_empty() {
                            return Err("step `wait url` needs a glob".into());
                        }
                        Step::WaitUrl {
                            glob: unquote(value),
                        }
                    }
                    "generation-after" => Step::WaitGenerationAfter {
                        generation: value.parse::<u64>().map_err(|_| {
                            format!("step `wait generation-after` wants an integer, got `{value}`")
                        })?,
                    },
                    "load" if value.is_empty() => Step::WaitLoad,
                    "stable" => {
                        let value = value.strip_prefix("quiet-ms=").unwrap_or(value);
                        let quiet_ms = if value.is_empty() {
                            default_job_quiet_ms()
                        } else {
                            value.parse::<u64>().map_err(|_| {
                                format!("step `wait stable` wants optional quiet milliseconds, got `{value}`")
                            })?
                        };
                        if quiet_ms > JOB_WAIT_TIMEOUT_MS {
                            return Err(format!(
                                "step `wait stable` quiet time must be <= {JOB_WAIT_TIMEOUT_MS}ms"
                            ));
                        }
                        Step::WaitStable { quiet_ms }
                    }
                    _ => {
                        let ms = rest
                            .trim_end_matches("ms")
                            .trim()
                            .parse::<u64>()
                            .map_err(|_| {
                                format!(
                                    "step `wait` wants milliseconds or a typed condition, got `{rest}`"
                                )
                            })?;
                        Step::Wait {
                            ms: ms.min(120_000),
                        }
                    }
                }
            }
            "back" if rest.is_empty() => Step::Back,
            "forward" if rest.is_empty() => Step::Forward,
            "reload" => match rest {
                "" => Step::Reload {
                    ignore_cache: false,
                },
                "ignore-cache" | "--ignore-cache" => Step::Reload { ignore_cache: true },
                _ => return Err("step `reload` accepts only optional `ignore-cache`".into()),
            },
            "pointer" if rest == "park" => Step::PointerPark,
            "checkpoint" => Step::Checkpoint {
                name: (!rest.is_empty()).then(|| unquote(rest)),
            },
            "screenshot" => Step::Screenshot,
            "check-errors" | "check_errors" => Step::CheckErrors,
            other => {
                return Err(format!(
                    "unknown step `{other}`. Known steps: open, click, fill, press, wait, back, \
                     forward, reload, pointer park, checkpoint, screenshot, check-errors"
                ))
            }
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Step::Open { url } => format!("open {}", crate::redact::url(url)),
            Step::Click { text } => format!("click {text:?}"),
            Step::Fill { field, .. } => format!("fill {field:?} with [redacted input]"),
            Step::Press { chord } => format!("press {chord}"),
            Step::Wait { ms } => format!("fixed delay {ms}ms (not readiness)"),
            Step::WaitUrl { glob } => {
                format!("wait for URL {:?}", crate::redact::url_glob(glob))
            }
            Step::WaitGenerationAfter { generation } => {
                format!("wait for generation after {generation}")
            }
            Step::WaitLoad => "wait for document load".into(),
            Step::WaitStable { quiet_ms } => {
                format!("wait for browser stability ({quiet_ms}ms quiet)")
            }
            Step::Back => "back".into(),
            Step::Forward => "forward".into(),
            Step::Reload { ignore_cache } => {
                if *ignore_cache {
                    "reload ignoring cache".into()
                } else {
                    "reload".into()
                }
            }
            Step::PointerPark => "pointer park".into(),
            Step::Checkpoint { name } => match name {
                Some(name) => format!("checkpoint {name:?}"),
                None => "checkpoint".into(),
            },
            Step::Screenshot => "screenshot".into(),
            Step::CheckErrors => "check for errors".into(),
        }
    }
}

fn default_job_quiet_ms() -> u64 {
    300
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Actions a background job must never take on its own.
///
/// Matched on whole words against an element's accessible name, so "Send" gates
/// but "Sender name" does not. The list is deliberately short: a gate that fires
/// on everything trains people to approve without reading, which is worse than no
/// gate at all.
const IRREVERSIBLE_WORDS: &[&str] = &[
    "delete",
    "remove",
    "destroy",
    "erase",
    "wipe",
    "purchase",
    "buy",
    "pay",
    "checkout",
    "order",
    "subscribe",
    "unsubscribe",
    "publish",
    "send",
    "deactivate",
    "terminate",
    "withdraw",
    "transfer",
    "revoke",
    "archive",
];

/// Names the word that makes an action irreversible, if any.
pub fn irreversible_reason(label: &str) -> Option<String> {
    let lower = label.to_ascii_lowercase();
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if IRREVERSIBLE_WORDS.contains(&word) {
            return Some(word.to_string());
        }
    }
    None
}

/// Human-readable semantics from the same fresh read used for identity binding.
fn fingerprint_display(fingerprint: &NodeFingerprint) -> Result<String, String> {
    let label = fingerprint
        .accessible_label
        .as_deref()
        .filter(|label| !label.is_empty())
        .ok_or_else(|| {
            format!(
                "{} no longer has an accessible label",
                fingerprint.identity.backend_node_id
            )
        })?;
    Ok(
        match fingerprint
            .role
            .as_deref()
            .filter(|role| *role != fingerprint.tag)
        {
            Some(role) => format!("{} role={role} {label:?}", fingerprint.tag),
            None => format!("{} {label:?}", fingerprint.tag),
        },
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingKind {
    /// The engine found several equally good matches and will not guess.
    Decision {
        question: String,
        options: Vec<String>,
    },
    /// The next action cannot be undone.
    Approval {
        action: String,
        reason: String,
        /// Screenshot taken when the request was raised, so a human approving
        /// later can see what the job saw.
        evidence: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    /// Monotonic within one runner. Control messages carry this value so a
    /// duplicate answer from an earlier park can never authorize a later one.
    pub id: u64,
    #[serde(flatten)]
    pub kind: PendingKind,
    pub asked_at_ms: u128,
    pub expires_at_ms: u128,
    /// The document generation the request was raised against. If the page has
    /// moved on by the time an answer arrives, the answer is about a page that no
    /// longer exists and must not be applied.
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub t_ms: u128,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: JobId,
    pub intent: String,
    pub state: JobState,
    pub steps: Vec<Step>,
    /// Index of the step being executed, or about to be.
    pub cursor: usize,
    pub log: Vec<LogLine>,
    pub artifacts: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_ms: u128,
    #[serde(default)]
    pub checkpoint_each_step: bool,
    #[serde(default)]
    pub checkpoints: Vec<PathBuf>,
}

impl JobRecord {
    fn log(&mut self, text: impl Into<String>) {
        // Log lines are evidence, never executable input. Redact once at ingest
        // so both the in-memory/status view and tracing's durable browd.log are
        // safe; manifest sanitization remains defense in depth for legacy rows.
        let text = crate::redact::storage_text(&text.into());
        let line = LogLine {
            t_ms: now_ms(),
            text,
        };
        tracing::info!(job = %self.id, "{}", line.text);
        self.log.push(line);
    }

    pub fn render_status(&self) -> String {
        let mut s = format!(
            "{}  {}  step {}/{}\n{}\n",
            self.id,
            self.state.label(),
            self.cursor.min(self.steps.len()),
            self.steps.len(),
            self.intent
        );
        if let Some(p) = &self.pending {
            match &p.kind {
                PendingKind::Decision { question, options } => {
                    s.push_str(&format!("\nneeds a decision: {question}\n"));
                    for (i, o) in options.iter().enumerate() {
                        s.push_str(&format!("  [{i}] {o}\n"));
                    }
                    s.push_str(&format!(
                        "answer with: brow job answer {} <index>\n",
                        self.id
                    ));
                }
                PendingKind::Approval {
                    action,
                    reason,
                    evidence,
                } => {
                    s.push_str(&format!(
                        "\nwaiting for a human: {action}\nthis looks irreversible ({reason})\n"
                    ));
                    if let Some(e) = evidence {
                        s.push_str(&format!("evidence: {}\n", e.display()));
                    }
                    s.push_str(&format!(
                        "approve with: brow job approve {}   (or --reject)\n",
                        self.id
                    ));
                }
            }
        }
        if let Some(e) = &self.error {
            s.push_str(&format!("\nerror: {e}\n"));
        }
        s
    }

    /// Builds the non-resumable, privacy-safe view written to `job.json`.
    /// The live Runner retains the original plan in memory; recovered jobs are
    /// always interrupted, so persisted form values never need to be replayed.
    fn sanitized_for_storage(&self) -> Self {
        let mut durable = self.clone();
        durable.intent = crate::redact::storage_text(&durable.intent);
        for step in &mut durable.steps {
            match step {
                Step::Open { url } => *url = crate::redact::url(url),
                Step::WaitUrl { glob } => *glob = crate::redact::url_glob(glob),
                Step::Fill { field, value } => {
                    *field = crate::redact::storage_text(field);
                    *value = crate::redact::MASK.into();
                }
                Step::Click { text } => *text = crate::redact::storage_text(text),
                Step::Checkpoint { name: Some(name) } => *name = crate::redact::storage_text(name),
                _ => {}
            }
        }
        for line in &mut durable.log {
            line.text = crate::redact::storage_text(&line.text);
        }
        if let Some(error) = &mut durable.error {
            *error = crate::redact::storage_text(error);
        }
        if let Some(pending) = &mut durable.pending {
            match &mut pending.kind {
                PendingKind::Decision { question, options } => {
                    *question = crate::redact::storage_text(question);
                    for option in options {
                        *option = crate::redact::storage_text(option);
                    }
                }
                PendingKind::Approval { action, reason, .. } => {
                    *action = crate::redact::storage_text(action);
                    *reason = crate::redact::storage_text(reason);
                }
            }
        }
        durable
    }
}

/// Sent from the control socket into a running job.
#[derive(Debug)]
pub enum Control {
    Answer(String),
    Approve,
    Reject,
}

#[derive(Debug)]
pub struct ControlMessage {
    pub pending_id: u64,
    pub control: Control,
}

/// The daemon's handle on a running job.
pub struct JobHandle {
    pub record: Arc<Mutex<JobRecord>>,
    pub control: mpsc::Sender<ControlMessage>,
    /// Interrupts a step that is already executing.
    ///
    /// Separate from `control` because a running job is not reading its control
    /// channel — it is inside a navigation or a `wait`. Without this, `job stop`
    /// marked the job stopped and returned success while the browser kept running
    /// until the current step finished, which for `wait 60000` is a minute of
    /// lying to the caller.
    pub stop: tokio::sync::watch::Sender<bool>,
    /// Linearizes the start of browser-side mutations against `job stop`.
    /// Stop raises the watch flag first, then takes this gate; a mutation that
    /// acquires it afterwards observes the flag and cannot be dispatched.
    pub action_gate: Arc<Mutex<()>>,
    /// Becomes true only after the Runner has finished terminal evidence,
    /// durably committed terminal state, released its final action gate, and
    /// the daemon has killed and reaped the job-owned browser.
    /// `job stop` waits for this acknowledgement before returning.
    pub terminalized: tokio::sync::watch::Receiver<bool>,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Time-ordered, collision-free within a daemon.
pub fn new_job_id(seq: u64) -> JobId {
    format!("job_{:013x}{:03x}", now_ms(), seq & 0xfff)
}

const MANIFEST_NAME: &str = "job.json";
const MANIFEST_TEMP_NAME: &str = "job.json.tmp";

/// Deterministic barrier for the crash window between checkpoint publication
/// and committing that path to the job manifest. It exists only in unit-test
/// builds; production has no branch, environment switch, or pause point.
#[cfg(test)]
mod checkpoint_publication_test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use tokio::sync::Notify;

    struct Barrier {
        job_id: String,
        path: Mutex<Option<PathBuf>>,
        entered: AtomicBool,
        released: AtomicBool,
        entered_notify: Notify,
        released_notify: Notify,
    }

    impl Barrier {
        fn new(job_id: String) -> Self {
            Self {
                job_id,
                path: Mutex::new(None),
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

        fn enter(&self, path: &Path) {
            *self.path.lock().expect("checkpoint publication test path") = Some(path.into());
            self.entered.store(true, Ordering::Release);
            self.entered_notify.notify_waiters();
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
            self.released_notify.notify_waiters();
        }
    }

    fn slot() -> &'static Mutex<Option<Arc<Barrier>>> {
        static SLOT: OnceLock<Mutex<Option<Arc<Barrier>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    pub(super) struct PublicationHold {
        barrier: Arc<Barrier>,
    }

    impl PublicationHold {
        pub(super) async fn wait_until_entered(&self) -> PathBuf {
            Barrier::wait_for(&self.barrier.entered, &self.barrier.entered_notify).await;
            self.barrier
                .path
                .lock()
                .expect("checkpoint publication test path")
                .clone()
                .expect("publication path is recorded before the barrier is entered")
        }
    }

    impl Drop for PublicationHold {
        fn drop(&mut self) {
            let mut slot = slot().lock().expect("checkpoint publication test barrier");
            if slot
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                *slot = None;
            }
            self.barrier.release();
        }
    }

    pub(super) fn hold_next(job_id: impl Into<String>) -> PublicationHold {
        let barrier = Arc::new(Barrier::new(job_id.into()));
        let mut slot = slot().lock().expect("checkpoint publication test barrier");
        assert!(
            slot.is_none(),
            "a checkpoint publication test barrier is already installed"
        );
        *slot = Some(Arc::clone(&barrier));
        PublicationHold { barrier }
    }

    pub(super) async fn hold_if_requested(job_id: &str, path: &Path) {
        let barrier = {
            let mut slot = slot().lock().expect("checkpoint publication test barrier");
            if slot
                .as_ref()
                .is_some_and(|pending| pending.job_id == job_id)
            {
                slot.take()
            } else {
                None
            }
        };
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter(path);
        Barrier::wait_for(&barrier.released, &barrier.released_notify).await;
    }
}

/// Writes the manifest so a later `brow job status` can still explain what
/// happened, even after the daemon that ran it is gone.
///
/// The public compatibility wrapper reports failures through tracing. Code that
/// owns a running record must use [`try_persist`] so it can also expose the
/// failure in the job state.
pub async fn persist(record: &JobRecord) {
    if let Err(error) = try_persist(record).await {
        tracing::error!(
            job = %record.id,
            artifacts = %record.artifacts.display(),
            %error,
            "could not persist job manifest"
        );
    }
}

/// Atomically commits `job.json` and makes both its contents and directory entry
/// durable before returning success.
pub async fn try_persist(record: &JobRecord) -> AnyResult<()> {
    let path = record.artifacts.join(MANIFEST_NAME);
    let temp = record.artifacts.join(MANIFEST_TEMP_NAME);
    let durable = record.sanitized_for_storage();
    let mut bytes = serde_json::to_vec_pretty(&durable)
        .with_context(|| format!("serialize job {}", record.id))?;
    bytes.push(b'\n');

    // The transaction below has no await points, so cancellation cannot split
    // it. This process-wide gate additionally prevents two explicit persistence
    // callers from racing through the shared per-job temporary name.
    static PERSISTENCE_GATE: OnceLock<StdMutex<()>> = OnceLock::new();
    let _persistence = PERSISTENCE_GATE
        .get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    crate::paths::create_dir_all_durable(&record.artifacts)
        .with_context(|| format!("create {}", record.artifacts.display()))?;

    // Keep the complete write/fsync/rename transaction in one poll. Tokio fs
    // delegates individual calls to blocking workers; cancelling between those
    // awaits can otherwise let an old rename race a terminal stop persist.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("open temporary manifest {}", temp.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write temporary manifest {}", temp.display()))?;
    file.flush()
        .with_context(|| format!("flush temporary manifest {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync temporary manifest {}", temp.display()))?;
    drop(file);

    std::fs::rename(&temp, &path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    crate::paths::sync_directory(&record.artifacts)
        .with_context(|| format!("sync directory {}", record.artifacts.display()))
}

async fn sync_directory(path: &Path) -> AnyResult<()> {
    let directory = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open directory {} for sync", path.display()))?;
    directory
        .sync_all()
        .await
        .with_context(|| format!("sync directory {}", path.display()))
}

/// Marks the in-memory record failed when its durable representation cannot be
/// committed. Deliberately does not try to persist that failure: recursively
/// claiming that a second failed write made the first one durable would be a lie.
pub(crate) async fn persist_or_mark_failed(record: &mut JobRecord) -> bool {
    let Err(error) = try_persist(record).await else {
        return true;
    };

    let persistence_error = format!(
        "job manifest persistence failed: {error}; this failed state exists only in memory"
    );
    let combined = match record.error.take() {
        Some(previous) => format!("{previous}; additionally, {persistence_error}"),
        None => persistence_error.clone(),
    };
    record.state = JobState::Failed;
    record.pending = None;
    record.error = Some(combined);
    record.log(format!("failed: {persistence_error}"));
    tracing::error!(job = %record.id, %error, "job stopped after manifest persistence failure");
    false
}

async fn read_manifest(path: &Path) -> AnyResult<Option<JobRecord>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read manifest {}", path.display()))
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("corrupt manifest {}", path.display()))
}

async fn promote_temp_manifest(temp: &Path, manifest: &Path) -> AnyResult<()> {
    tokio::fs::rename(temp, manifest)
        .await
        .with_context(|| format!("recover {} as {}", temp.display(), manifest.display()))?;
    let parent = manifest
        .parent()
        .context("job manifest has no parent directory")?;
    sync_directory(parent).await
}

async fn quarantine_corrupt_manifest(manifest: &Path) -> AnyResult<PathBuf> {
    let parent = manifest
        .parent()
        .context("job manifest has no parent directory")?;
    let timestamp = now_ms();
    let mut attempt = 0_u32;
    let quarantined = loop {
        let suffix = if attempt == 0 {
            timestamp.to_string()
        } else {
            format!("{timestamp}-{attempt}")
        };
        let candidate = parent.join(format!("{MANIFEST_NAME}.corrupt-{suffix}"));
        match tokio::fs::symlink_metadata(&candidate).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break candidate,
            Ok(_) => attempt += 1,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect recovery path {}", candidate.display()))
            }
        }
    };
    tokio::fs::rename(manifest, &quarantined)
        .await
        .with_context(|| {
            format!(
                "preserve corrupt manifest {} as {}",
                manifest.display(),
                quarantined.display()
            )
        })?;
    sync_directory(parent).await?;
    Ok(quarantined)
}

/// Reconciles the durable record with atomically published checkpoint
/// directories. This closes the unavoidable crash window after a checkpoint's
/// final rename but before its path can be committed into `job.json`.
/// Dot-prefixed partial directories are never promoted or listed.
async fn reconcile_committed_checkpoints(record: &mut JobRecord) -> bool {
    let root = record.artifacts.join("checkpoints");
    let mut recovery_log_changed = false;
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if record.checkpoints.is_empty() {
                return false;
            }
            let removed = record.checkpoints.len();
            record.checkpoints.clear();
            record.log(format!(
                "checkpoint recovery removed {removed} path(s) without committed evidence"
            ));
            return true;
        }
        Err(error) => {
            record.log(format!(
                "checkpoint recovery could not inspect {}: {error}",
                root.display()
            ));
            return true;
        }
    };

    let mut committed = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                record.log(format!(
                    "checkpoint recovery could not continue scanning {}: {error}",
                    root.display()
                ));
                recovery_log_changed = true;
                break;
            }
        };
        let file_name = entry.file_name();
        if file_name.to_string_lossy().starts_with('.') {
            continue;
        }
        match entry.file_type().await {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                record.log(format!(
                    "checkpoint recovery could not inspect {}: {error}",
                    entry.path().display()
                ));
                recovery_log_changed = true;
                continue;
            }
        }

        let manifest_path = entry.path().join("manifest.json");
        let manifest = match tokio::fs::read(&manifest_path).await {
            Ok(bytes) => match serde_json::from_slice::<checkpoint::CheckpointManifest>(&bytes) {
                Ok(manifest) => manifest,
                Err(error) => {
                    record.log(format!(
                        "checkpoint recovery ignored invalid final {}: {error}",
                        entry.path().display()
                    ));
                    recovery_log_changed = true;
                    continue;
                }
            },
            Err(error) => {
                record.log(format!(
                    "checkpoint recovery ignored final {} without readable manifest: {error}",
                    entry.path().display()
                ));
                recovery_log_changed = true;
                continue;
            }
        };
        let required = [
            "snapshot.json",
            "screenshot.png",
            "console-errors.json",
            "network-failures.json",
        ];
        if required
            .iter()
            .any(|name| !entry.path().join(name).is_file())
        {
            record.log(format!(
                "checkpoint recovery ignored incomplete final {}",
                entry.path().display()
            ));
            recovery_log_changed = true;
            continue;
        }
        committed.push((manifest.created_ms, entry.path()));
    }
    committed.sort_by(|(left_time, left_path), (right_time, right_path)| {
        left_time
            .cmp(right_time)
            .then_with(|| left_path.cmp(right_path))
    });

    let committed_set: HashSet<PathBuf> = committed.iter().map(|(_, path)| path.clone()).collect();
    let old = record.checkpoints.clone();
    let mut seen_existing = HashSet::new();
    record
        .checkpoints
        .retain(|path| committed_set.contains(path) && seen_existing.insert(path.clone()));
    let mut listed: HashSet<PathBuf> = record.checkpoints.iter().cloned().collect();
    for (_, path) in committed {
        if listed.insert(path.clone()) {
            record.log(format!(
                "checkpoint recovery found committed evidence {}",
                path.display()
            ));
            record.checkpoints.push(path);
            recovery_log_changed = true;
        }
    }
    if old != record.checkpoints {
        let removed = old
            .iter()
            .filter(|path| !record.checkpoints.contains(path))
            .count();
        if removed > 0 {
            record.log(format!(
                "checkpoint recovery removed {removed} uncommitted checkpoint path(s)"
            ));
        }
        true
    } else {
        recovery_log_changed
    }
}

fn damaged_manifest_record(artifacts: PathBuf, error: &anyhow::Error) -> JobRecord {
    let id = artifacts
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown_corrupt_job")
        .to_string();
    let message = format!(
        "could not recover job manifest: {error:#}; the original files were preserved in {}",
        artifacts.display()
    );
    JobRecord {
        id,
        intent: "unreadable recovered job manifest".into(),
        state: JobState::Failed,
        steps: Vec::new(),
        cursor: 0,
        log: vec![LogLine {
            t_ms: now_ms(),
            text: message.clone(),
        }],
        artifacts,
        pending: None,
        error: Some(message),
        created_ms: now_ms(),
        checkpoint_each_step: false,
        checkpoints: Vec::new(),
    }
}

/// Reads back manifests from previous daemon lifetimes.
///
/// Anything that was mid-flight is marked [`JobState::Interrupted`]: the browser
/// it was driving no longer exists, so claiming it could resume would be a lie.
pub async fn load_previous(root: &std::path::Path) -> Vec<JobRecord> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return out,
        Err(error) => {
            tracing::error!(path = %root.display(), %error, "could not scan previous jobs");
            return out;
        }
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                tracing::error!(path = %root.display(), %error, "could not continue scanning previous jobs");
                break;
            }
        };
        match entry.file_type().await {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::error!(path = %entry.path().display(), %error, "could not inspect previous job entry");
                continue;
            }
        }
        let artifacts = entry.path();
        let manifest = artifacts.join(MANIFEST_NAME);
        let temp = artifacts.join(MANIFEST_TEMP_NAME);

        let mut record = match read_manifest(&manifest).await {
            Ok(Some(record)) => record,
            Ok(None) => match read_manifest(&temp).await {
                Ok(Some(record)) => {
                    tracing::warn!(
                        path = %temp.display(),
                        "recovering job from a complete temporary manifest"
                    );
                    if let Err(error) = promote_temp_manifest(&temp, &manifest).await {
                        tracing::error!(path = %temp.display(), %error, "could not promote recovered manifest");
                    }
                    record
                }
                Ok(None) => continue,
                Err(error) => {
                    tracing::error!(path = %temp.display(), error = %error, "temporary job manifest is corrupt");
                    out.push(damaged_manifest_record(artifacts, &error));
                    continue;
                }
            },
            Err(manifest_error) => {
                tracing::error!(
                    path = %manifest.display(),
                    error = %manifest_error,
                    "job manifest is corrupt or unreadable"
                );
                match read_manifest(&temp).await {
                    Ok(Some(record)) => {
                        match quarantine_corrupt_manifest(&manifest).await {
                            Ok(quarantined) => {
                                tracing::warn!(
                                    path = %manifest.display(),
                                    preserved = %quarantined.display(),
                                    "recovered job from temporary manifest"
                                );
                                if let Err(error) = promote_temp_manifest(&temp, &manifest).await {
                                    tracing::error!(path = %temp.display(), %error, "could not promote recovered manifest");
                                }
                            }
                            Err(error) => {
                                tracing::error!(path = %manifest.display(), %error, "could not preserve corrupt manifest before recovery");
                            }
                        }
                        record
                    }
                    Ok(None) => {
                        out.push(damaged_manifest_record(artifacts, &manifest_error));
                        continue;
                    }
                    Err(temp_error) => {
                        let combined = manifest_error.context(format!(
                            "temporary recovery manifest also failed: {temp_error:#}"
                        ));
                        tracing::error!(path = %temp.display(), error = %temp_error, "temporary job manifest is also corrupt");
                        out.push(damaged_manifest_record(artifacts, &combined));
                        continue;
                    }
                }
            }
        };
        if record.artifacts != artifacts {
            tracing::warn!(
                job = %record.id,
                stored = %record.artifacts.display(),
                actual = %artifacts.display(),
                "job manifest artifact path does not match its directory; using the directory"
            );
            record.artifacts = artifacts;
        }
        let checkpoint_inventory_changed = reconcile_committed_checkpoints(&mut record).await;
        let was_interrupted = !record.state.is_terminal();
        if was_interrupted {
            record.state = JobState::Interrupted;
            record.pending = None;
            record.error = Some(
                "the daemon stopped while this job was running; its browser went with it, \
                 so it cannot be resumed"
                    .into(),
            );
        }
        if was_interrupted || checkpoint_inventory_changed {
            persist_or_mark_failed(&mut record).await;
        }
        out.push(record);
    }
    out.sort_by_key(|r| r.created_ms);
    out
}

/// Outcome of asking for a decision or an approval.
enum Resolution {
    Answered(String),
    Approved,
    Rejected,
    Stopped,
    TimedOut,
    PersistenceFailed,
}

/// Runs a plan to completion, parking for input when the heuristics run out.
pub struct Runner {
    pub record: Arc<Mutex<JobRecord>>,
    pub control: mpsc::Receiver<ControlMessage>,
    pub stop: tokio::sync::watch::Receiver<bool>,
    pub action_gate: Arc<Mutex<()>>,
    pub next_pending_id: u64,
}

impl Runner {
    pub async fn run(mut self, page: &mut Page) {
        {
            let r = self.record.lock().await;
            if r.state.is_terminal() {
                return;
            }
        }
        if *self.stop.borrow() {
            self.terminalize_stopped(page).await;
            return;
        }
        {
            let mut r = self.record.lock().await;
            r.state = JobState::Running;
            let intent = r.intent.clone();
            r.log(format!("started: {intent}"));
            if !persist_or_mark_failed(&mut r).await {
                return;
            }
        }

        loop {
            let step = {
                let r = self.record.lock().await;
                if r.state.is_terminal() {
                    break;
                }
                match r.steps.get(r.cursor) {
                    Some(s) => s.clone(),
                    None => break,
                }
            };

            // A step in flight has to be abandonable. Cloning the receiver keeps
            // the borrow checker happy while `execute` holds `&mut self`.
            let mut stop = self.stop.clone();
            let outcome = if *stop.borrow() {
                Err(StepOutcome::Stopped)
            } else {
                tokio::select! {
                    biased;
                    _ = stop.changed() => Err(StepOutcome::Stopped),
                    result = self.execute_with_checkpoint(page, &step) => result,
                }
            };

            match outcome {
                Ok(()) => {
                    let mut r = self.record.lock().await;
                    // `job_stop` owns terminal state. An action may have
                    // completed just as stop was raised, but it must never
                    // advance or resurrect the durable job afterwards.
                    if r.state.is_terminal() {
                        break;
                    }
                    r.cursor += 1;
                    if !persist_or_mark_failed(&mut r).await {
                        break;
                    }
                }
                Err(StepOutcome::Stopped) => {
                    self.terminalize_stopped(page).await;
                    break;
                }
                Err(StepOutcome::Failed(why)) => {
                    let checkpoint_failure =
                        if self.record.lock().await.checkpoint_each_step && !*self.stop.borrow() {
                            match self.begin_browser_action().await {
                                Ok(_action) => self
                                    .capture_job_checkpoint_with_policy(
                                        page,
                                        "terminal-failed".into(),
                                        WaitPolicy::Load,
                                    )
                                    .await
                                    .err(),
                                Err(_) => None,
                            }
                        } else {
                            None
                        };
                    let mut r = self.record.lock().await;
                    if r.state.is_terminal() {
                        break;
                    }
                    if let Some(error) = checkpoint_failure {
                        r.log(format!("terminal checkpoint unavailable: {error:?}"));
                    }
                    r.state = JobState::Failed;
                    r.log(format!("failed: {why}"));
                    r.error = Some(why);
                    persist_or_mark_failed(&mut r).await;
                    break;
                }
                Err(StepOutcome::PersistenceFailed) => {
                    // `park` already recorded the failure in memory. Retrying the
                    // same persistence operation here would only repeat the lie.
                    break;
                }
            }
        }

        let mut r = self.record.lock().await;
        if !r.state.is_terminal() {
            r.state = JobState::Succeeded;
            r.log("finished");
            persist_or_mark_failed(&mut r).await;
        }
    }

    async fn terminalize_stopped(&self, page: &mut Page) {
        // Runner owns cancellation terminalization. The daemon only raises
        // `stop` and waits for `terminalized`, so this is the final safe browser
        // boundary and no artifact can appear after the stop response.
        let terminal_action = Arc::clone(&self.action_gate).lock_owned().await;
        {
            let mut r = self.record.lock().await;
            // A cancelled capture may have crossed its atomic rename and then
            // been dropped while waiting to append the path. Account for that
            // committed evidence before publishing terminal evidence/state.
            reconcile_committed_checkpoints(&mut r).await;
        }
        let (checkpoint_enabled, checkpoint_safe) = {
            let r = self.record.lock().await;
            (
                r.checkpoint_each_step,
                !r.state.is_parked() && r.pending.is_none() && !r.state.is_terminal(),
            )
        };
        let checkpoint_failure = if checkpoint_enabled && checkpoint_safe {
            match tokio::time::timeout(
                TERMINAL_CHECKPOINT_TIMEOUT,
                self.capture_job_checkpoint_with_policy_and_timeout(
                    page,
                    "terminal-stopped".into(),
                    WaitPolicy::Load,
                    TERMINAL_CHECKPOINT_TIMEOUT,
                ),
            )
            .await
            {
                Ok(result) => result.err(),
                Err(_) => Some(StepOutcome::Failed(format!(
                    "terminal checkpoint exceeded its {}ms total deadline",
                    TERMINAL_CHECKPOINT_TIMEOUT.as_millis()
                ))),
            }
        } else {
            None
        };

        let mut r = self.record.lock().await;
        // A timed-out capture may have crossed its atomic publication point
        // before its future was cancelled. Reconcile once more before the
        // terminal manifest is committed.
        reconcile_committed_checkpoints(&mut r).await;
        if r.state.is_terminal() {
            drop(r);
            drop(terminal_action);
            return;
        }
        r.pending = None;
        if checkpoint_enabled && !checkpoint_safe {
            r.log(
                "terminal checkpoint skipped: cancellation occurred while waiting for human input",
            );
        }
        if let Some(error) = checkpoint_failure {
            r.log(format!("terminal checkpoint unavailable: {error:?}"));
        }
        r.state = JobState::Stopped;
        r.log("stopped on request");
        persist_or_mark_failed(&mut r).await;
        drop(r);
        drop(terminal_action);
    }

    async fn execute(&mut self, page: &mut Page, step: &Step) -> Result<(), StepOutcome> {
        {
            let mut r = self.record.lock().await;
            let n = r.cursor + 1;
            let total = r.steps.len();
            r.log(format!("[{n}/{total}] {}", step.describe()));
        }

        match step {
            Step::Open { url } => {
                let _action = self.begin_browser_action().await?;
                page.navigate(url)
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))
            }
            Step::Wait { ms } => {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                Ok(())
            }
            Step::WaitUrl { glob } => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .wait_for_conditions(
                        &WaitConditions {
                            url: Some(glob.clone()),
                            ..WaitConditions::default()
                        },
                        Duration::from_secs(30),
                        Duration::from_millis(300),
                    )
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::WaitGenerationAfter { generation } => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .wait_for_conditions(
                        &WaitConditions {
                            generation_after: Some(*generation),
                            ..WaitConditions::default()
                        },
                        Duration::from_secs(30),
                        Duration::from_millis(300),
                    )
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::WaitLoad => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .wait_for_conditions(
                        &WaitConditions {
                            load: true,
                            ..WaitConditions::default()
                        },
                        Duration::from_secs(30),
                        Duration::from_millis(300),
                    )
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::WaitStable { quiet_ms } => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .wait_for_conditions(
                        &WaitConditions {
                            stable: true,
                            ..WaitConditions::default()
                        },
                        Duration::from_secs(30),
                        Duration::from_millis(*quiet_ms),
                    )
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::Back => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .traverse_history(-1, None, Duration::from_secs(30))
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::Forward => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .traverse_history(1, None, Duration::from_secs(30))
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::Reload { ignore_cache } => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .reload_with_wait(*ignore_cache, None, Duration::from_secs(30))
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::PointerPark => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .park_pointer()
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_structured("receipt", &receipt).await;
                self.record.lock().await.log(format!(
                    "pointer parked at {},{}; {}",
                    receipt.x, receipt.y, receipt.warning
                ));
                Ok(())
            }
            Step::Checkpoint { name } => {
                let _action = self.begin_browser_action().await?;
                let fallback = {
                    let r = self.record.lock().await;
                    format!("step-{:03}", r.cursor + 1)
                };
                self.capture_job_checkpoint(page, name.clone().unwrap_or(fallback))
                    .await
            }
            Step::Press { chord } => {
                let _action = self.begin_browser_action().await?;
                let receipt = page
                    .press_with_wait(chord, WaitPolicy::Auto, Duration::from_secs(30))
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.log_receipt(&receipt).await;
                Ok(())
            }
            Step::Screenshot => {
                let shot = page
                    .screenshot(ScreenshotTarget::Viewport, ImageFormat::Png, None)
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                let path = {
                    let r = self.record.lock().await;
                    r.artifacts.join(format!("step-{:03}.png", r.cursor + 1))
                };
                shot.write_to(&path)
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))?;
                self.record
                    .lock()
                    .await
                    .log(format!("wrote {}", path.display()));
                Ok(())
            }
            Step::CheckErrors => {
                let errors = page.events.console(true, 200);
                let failures = page.events.network(true, 200);
                let event_stream_gaps = page.events.event_stream_gaps();
                if event_stream_gaps > 0 {
                    return Err(StepOutcome::Failed(format!(
                        "cannot prove the page is error-free: {event_stream_gaps} upstream CDP \
                         event(s) were unavailable; repeat the check in a fresh, quieter session"
                    )));
                }
                if errors.is_empty() && failures.is_empty() {
                    self.record
                        .lock()
                        .await
                        .log("no console errors, no failed requests");
                    return Ok(());
                }
                let mut detail = String::new();
                for e in errors.iter().take(10) {
                    detail.push_str(&format!("\n  console: {}", e.text));
                }
                for f in failures.iter().take(10) {
                    detail.push_str(&format!(
                        "\n  network: {} {}",
                        f.error
                            .clone()
                            .unwrap_or_else(|| f.status.map(|s| s.to_string()).unwrap_or_default()),
                        f.url
                    ));
                }
                Err(StepOutcome::Failed(format!(
                    "{} console error(s) and {} failed request(s):{detail}",
                    errors.len(),
                    failures.len()
                )))
            }
            Step::Click { text } => self.click_by_text(page, text).await,
            Step::Fill { field, value } => self.fill_by_text(page, field, value).await,
        }
    }

    async fn execute_with_checkpoint(
        &mut self,
        page: &mut Page,
        step: &Step,
    ) -> Result<(), StepOutcome> {
        self.execute(page, step).await?;
        let (enabled, name) = {
            let r = self.record.lock().await;
            (
                r.checkpoint_each_step,
                format!("after-step-{:03}", r.cursor + 1),
            )
        };
        if automatic_checkpoint_required(enabled, step) {
            let _action = self.begin_browser_action().await?;
            self.capture_job_checkpoint(page, name).await?;
        }
        Ok(())
    }

    async fn capture_job_checkpoint(
        &self,
        page: &mut Page,
        name: String,
    ) -> Result<(), StepOutcome> {
        self.capture_job_checkpoint_with_policy(page, name, WaitPolicy::Stable)
            .await
    }

    async fn capture_job_checkpoint_with_policy(
        &self,
        page: &mut Page,
        name: String,
        wait: WaitPolicy,
    ) -> Result<(), StepOutcome> {
        self.capture_job_checkpoint_with_policy_and_timeout(
            page,
            name,
            wait,
            Duration::from_secs(30),
        )
        .await
    }

    async fn capture_job_checkpoint_with_policy_and_timeout(
        &self,
        page: &mut Page,
        name: String,
        wait: WaitPolicy,
        timeout: Duration,
    ) -> Result<(), StepOutcome> {
        let (id, output_root) = {
            let r = self.record.lock().await;
            (r.id.clone(), r.artifacts.join("checkpoints"))
        };
        let result = checkpoint::create(
            page,
            CheckpointOptions {
                session: id.clone(),
                name,
                full_page: false,
                wait,
                timeout,
                quiet: Duration::from_millis(300),
                park_pointer: false,
                output_root: Some(output_root),
            },
        )
        .await
        .map_err(|e| StepOutcome::Failed(e.to_string()))?;
        #[cfg(test)]
        checkpoint_publication_test_support::hold_if_requested(&id, &result.path).await;
        let durability_warning = match &result.durability {
            checkpoint::CheckpointDurability::Durable => None,
            checkpoint::CheckpointDurability::PublishedSyncUnknown { error } => Some(format!(
                "checkpoint {} was published, but parent-directory sync failed; host-crash durability is unknown: {error}",
                result.path.display()
            )),
        };
        let structured = serde_json::to_string(&result)
            .unwrap_or_else(|error| format!(r#"{{"serialization_error":{error:?}}}"#));
        let mut r = self.record.lock().await;
        if !r.checkpoints.contains(&result.path) {
            r.checkpoints.push(result.path.clone());
        }
        r.log(format!("checkpoint {}", result.path.display()));
        r.log(format!("checkpoint_result {structured}"));
        if let Some(warning) = durability_warning {
            r.log(warning);
        }
        if persist_or_mark_failed(&mut r).await {
            Ok(())
        } else {
            Err(StepOutcome::PersistenceFailed)
        }
    }

    async fn log_receipt(&self, receipt: &crate::page::ActionReceipt) {
        self.log_structured("receipt", receipt).await;
    }

    async fn log_structured(&self, label: &str, value: &impl Serialize) {
        let text = serde_json::to_string(value)
            .unwrap_or_else(|error| format!(r#"{{"serialization_error":{error:?}}}"#));
        self.record.lock().await.log(format!("{label} {text}"));
    }

    /// Resolves visible text to exactly one element, or parks.
    async fn click_by_text(&mut self, page: &mut Page, query: &str) -> Result<(), StepOutcome> {
        let (node_ref, _) = match self.resolve_one(page, query, false).await? {
            Some(r) => r,
            None => return Ok(()), // stopped or rejected; state already set
        };

        // Never decide safety from the earlier snapshot label. Script can rename
        // or repurpose the same backend node without navigation, which would let
        // a unique "Save" become "Delete account" between resolution and this
        // click. The fresh fingerprint is both the gate input and the value later
        // revalidated immediately before dispatch.
        let reviewed_target = page.node_fingerprint(&node_ref).await.map_err(|e| {
            StepOutcome::Failed(format!(
                "could not bind click target {node_ref} to its exact current target: {e}. No \
                 click was sent; inspect the page and try again"
            ))
        })?;
        let actual_target = fingerprint_display(&reviewed_target).map_err(|why| {
            StepOutcome::Failed(format!(
                "could not identify click target {node_ref}: {why}. No click was sent"
            ))
        })?;
        if !actual_target
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
        {
            return Err(StepOutcome::Failed(format!(
                "the page changed before the click: {node_ref} is now {actual_target} and no \
                 longer matches {query:?}. No click was sent"
            )));
        }

        // The irreversibility gate sits between resolving the target and touching
        // it, so the human sees the actual element the job is about to click.
        let approved_target = if let Some(reason) = irreversible_reason(&actual_target) {
            // A document generation is not enough: script can replace or rename
            // this exact control without navigating. Bind the approval to the
            // browser-side identity and freshly read semantics of the target.
            let evidence = Some(self.capture_evidence(page, &node_ref).await?);
            let generation = page.generation();
            let action = format!("click {actual_target}");
            match self
                .park(
                    JobState::WaitingForApproval,
                    PendingKind::Approval {
                        action,
                        reason,
                        evidence,
                    },
                    APPROVAL_TTL,
                    generation,
                )
                .await
            {
                Resolution::Approved => {}
                Resolution::Rejected => {
                    return Err(StepOutcome::Failed("a human rejected this action".into()))
                }
                Resolution::Stopped => return Err(StepOutcome::Stopped),
                Resolution::TimedOut => {
                    return Err(StepOutcome::Failed(
                        "nobody approved this within the time limit".into(),
                    ))
                }
                Resolution::Answered(_) => {
                    return Err(StepOutcome::Failed(
                        "an approval can only be answered by a human, not by `job answer`".into(),
                    ))
                }
                Resolution::PersistenceFailed => return Err(StepOutcome::PersistenceFailed),
            }

            Some(reviewed_target.clone())
        } else {
            None
        };

        // This gate is the linearization boundary with `job stop`. Stop sets its
        // watch value before waiting for this gate; if that happened first, no
        // browser mutation is issued. If an action already owns the gate, stop
        // waits for its cancellation/completion before reporting success.
        let _action = self.begin_browser_action().await?;

        let current_target = page.node_fingerprint(&node_ref).await.map_err(|e| {
            StepOutcome::Failed(format!(
                "click target changed while waiting: {node_ref} could not be re-read ({e}). No \
                 click was sent; inspect the current page and try again"
            ))
        })?;
        if current_target != reviewed_target {
            let approval_context = if approved_target.is_some() {
                "approval target"
            } else {
                "click target"
            };
            let recovery = if approved_target.is_some() {
                "inspect the current page and request fresh approval"
            } else {
                "inspect the current page and try again"
            };
            return Err(StepOutcome::Failed(format!(
                "{approval_context} changed while waiting: reviewed {reviewed_target:?}, but now \
                 found {current_target:?}. Browser identity, tag, role, accessible label, and \
                 action attributes must match exactly. No click was sent; {recovery}"
            )));
        }

        if let Some(approved_target) = approved_target {
            // This is deliberately the first browser read after approval. Compare
            // the complete fingerprint so navigation, OOPIF/session movement,
            // replacement, tag/role changes, and accessible-label changes all
            // invalidate what was reviewed.
            debug_assert_eq!(current_target, approved_target);
        }

        let (_, receipt) = page
            .click_if_unchanged_with_wait(
                &node_ref,
                MouseButton::Left,
                1,
                0,
                &reviewed_target,
                WaitPolicy::Auto,
                Duration::from_secs(30),
            )
            .await
            .map_err(|e| StepOutcome::Failed(e.to_string()))?;
        self.log_receipt(&receipt).await;
        Ok(())
    }

    async fn fill_by_text(
        &mut self,
        page: &mut Page,
        field: &str,
        value: &str,
    ) -> Result<(), StepOutcome> {
        let (node_ref, _) = match self.resolve_one(page, field, true).await? {
            Some(r) => r,
            None => return Ok(()),
        };
        let _action = self.begin_browser_action().await?;
        page.fill(&node_ref, value)
            .await
            .map_err(|e| StepOutcome::Failed(e.to_string()))
    }

    /// Finds the one element matching `query`, or parks with the candidates.
    ///
    /// Returning `Ok(None)` means the job was stopped or the request expired and
    /// the state has already been set.
    async fn resolve_one(
        &mut self,
        page: &mut Page,
        query: &str,
        fields_only: bool,
    ) -> Result<Option<(String, String)>, StepOutcome> {
        let snap = page
            .snapshot()
            .await
            .map_err(|e| StepOutcome::Failed(e.to_string()))?;
        let needle = query.to_ascii_lowercase();

        let candidates: Vec<(String, String)> = snap
            .interactive()
            .filter(|n| !fields_only || matches!(n.tag.as_str(), "input" | "textarea" | "select"))
            .filter_map(|n| {
                let label = n
                    .name
                    .clone()
                    .or_else(|| n.text.clone())
                    .or_else(|| n.attrs.get("placeholder").cloned())
                    .or_else(|| n.attrs.get("aria-label").cloned())
                    .unwrap_or_default();
                label
                    .to_ascii_lowercase()
                    .contains(&needle)
                    .then(|| (n.node_ref.clone(), format!("{} {label:?}", n.tag)))
            })
            .collect();

        match candidates.len() {
            0 => Err(StepOutcome::Failed(format!(
                "nothing on the page matches {query:?}"
            ))),
            1 => Ok(Some(candidates[0].clone())),
            _ => {
                // Refusing to guess here is the whole point: picking the first of
                // three "Continue" buttons is how a background job silently does
                // the wrong thing for forty minutes.
                let mut reviewed_candidates = Vec::with_capacity(candidates.len());
                for (node_ref, _) in candidates {
                    let fingerprint = page.node_fingerprint(&node_ref).await.map_err(|e| {
                        StepOutcome::Failed(format!(
                            "could not bind decision option {node_ref} to its exact current \
                             target: {e}. No click was sent; inspect the page and try again"
                        ))
                    })?;
                    let display = fingerprint_display(&fingerprint).map_err(|why| {
                        StepOutcome::Failed(format!(
                            "could not present decision option {node_ref}: {why}. No click was \
                             sent; inspect the page and try again"
                        ))
                    })?;
                    if !display.to_ascii_lowercase().contains(&needle) {
                        return Err(StepOutcome::Failed(format!(
                            "the page changed while preparing the decision: {node_ref} is now \
                             {display} and no longer matches {query:?}. No click was sent; inspect \
                             the page and request a fresh decision"
                        )));
                    }
                    reviewed_candidates.push((node_ref, display, fingerprint));
                }
                let generation = page.generation();
                let options: Vec<String> = reviewed_candidates
                    .iter()
                    .map(|candidate| candidate.1.clone())
                    .collect();
                let question = format!("{} elements match {query:?}", reviewed_candidates.len());
                match self
                    .park(
                        JobState::NeedsDecision,
                        PendingKind::Decision {
                            question,
                            options: options.clone(),
                        },
                        DECISION_TTL,
                        generation,
                    )
                    .await
                {
                    Resolution::Answered(answer) => {
                        let index = answer.trim().parse::<usize>().map_err(|_| {
                            StepOutcome::Failed(format!("expected an option index, got {answer:?}"))
                        })?;
                        let chosen = reviewed_candidates.get(index).ok_or_else(|| {
                            StepOutcome::Failed(format!(
                                "option {index} does not exist; there were {}",
                                reviewed_candidates.len()
                            ))
                        })?;
                        let current = page.node_fingerprint(&chosen.0).await.map_err(|e| {
                            StepOutcome::Failed(format!(
                                "decision target changed while waiting: option [{index}] {} could \
                                 not be re-read ({e}). No click was sent; inspect the page and ask \
                                 for a fresh decision",
                                chosen.1
                            ))
                        })?;
                        if current != chosen.2 {
                            return Err(StepOutcome::Failed(format!(
                                "decision target changed while waiting: option [{index}] was \
                                 {:?}, but is now {current:?}. No click was sent; inspect the page \
                                 and ask for a fresh decision",
                                chosen.2
                            )));
                        }
                        self.record
                            .lock()
                            .await
                            .log(format!("agent chose [{index}] {}", chosen.1));
                        Ok(Some((chosen.0.clone(), chosen.1.clone())))
                    }
                    Resolution::Stopped => Err(StepOutcome::Stopped),
                    Resolution::TimedOut => Err(StepOutcome::Failed(
                        "nobody answered the decision request within the time limit".into(),
                    )),
                    Resolution::Approved | Resolution::Rejected => Err(StepOutcome::Failed(
                        "this is a decision for the agent, not an approval; use `job answer`"
                            .into(),
                    )),
                    Resolution::PersistenceFailed => Err(StepOutcome::PersistenceFailed),
                }
            }
        }
    }

    async fn capture_evidence(
        &self,
        page: &mut Page,
        node_ref: &str,
    ) -> Result<PathBuf, StepOutcome> {
        // `point_of` scrolls the exact target into view and performs the same
        // local + ancestor hit tests used by real pointer actions. The following
        // viewport therefore contains both the reviewed control and its context.
        page.point_of(&crate::page::PointTarget::Ref(node_ref.to_string()))
            .await
            .map_err(|e| {
                StepOutcome::Failed(format!(
                    "could not bring the approval target into view: {e}. No approval was \
                     requested and no click was sent"
                ))
            })?;
        let shot = page
            .screenshot(ScreenshotTarget::Viewport, ImageFormat::Png, None)
            .await
            .map_err(|e| {
                StepOutcome::Failed(format!(
                    "could not capture mandatory approval evidence: {e}. No approval was \
                     requested and no click was sent"
                ))
            })?;
        let path = {
            let r = self.record.lock().await;
            r.artifacts
                .join(format!("approval-{:03}.png", r.cursor + 1))
        };
        shot.write_to(&path).await.map_err(|e| {
            StepOutcome::Failed(format!(
                "could not write mandatory approval evidence to {}: {e}. No approval was \
                 requested and no click was sent",
                path.display()
            ))
        })?;
        Ok(path)
    }

    async fn begin_browser_action(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, StepOutcome> {
        acquire_browser_action(&self.action_gate, &self.stop, &self.record).await
    }

    /// Parks the job and waits for the control socket, a stop, or the deadline.
    async fn park(
        &mut self,
        state: JobState,
        kind: PendingKind,
        ttl: Duration,
        generation: u64,
    ) -> Resolution {
        self.next_pending_id = self.next_pending_id.saturating_add(1);
        let pending_id = self.next_pending_id;
        {
            let mut r = self.record.lock().await;
            r.state = state;
            r.pending = Some(Pending {
                id: pending_id,
                kind: kind.clone(),
                asked_at_ms: now_ms(),
                expires_at_ms: now_ms() + ttl.as_millis(),
                generation,
            });
            match &kind {
                PendingKind::Decision { question, .. } => {
                    r.log(format!("parked, needs a decision: {question}"))
                }
                PendingKind::Approval { action, reason, .. } => r.log(format!(
                    "parked for human approval: {action} (irreversible: {reason})"
                )),
            }
            if !persist_or_mark_failed(&mut r).await {
                return Resolution::PersistenceFailed;
            }
        }

        let mut stop = self.stop.clone();
        let deadline = tokio::time::Instant::now() + ttl;
        let resolution = loop {
            if *stop.borrow() {
                break Resolution::Stopped;
            }
            let next = tokio::select! {
                biased;
                _ = stop.changed() => break Resolution::Stopped,
                result = tokio::time::timeout_at(deadline, self.control.recv()) => result,
            };
            match next {
                Err(_) => break Resolution::TimedOut,
                Ok(None) => break Resolution::Stopped,
                Ok(Some(message)) if message.pending_id != pending_id => {
                    tracing::warn!(
                        expected = pending_id,
                        received = message.pending_id,
                        "discarded stale job control message"
                    );
                }
                Ok(Some(ControlMessage {
                    control: Control::Answer(answer),
                    ..
                })) => break Resolution::Answered(answer),
                Ok(Some(ControlMessage {
                    control: Control::Approve,
                    ..
                })) => break Resolution::Approved,
                Ok(Some(ControlMessage {
                    control: Control::Reject,
                    ..
                })) => break Resolution::Rejected,
            }
        };

        // Close the narrow race where a control item and stop became ready in
        // the same poll after the select chose the item.
        let resolution = if *self.stop.borrow() {
            Resolution::Stopped
        } else {
            resolution
        };

        if matches!(resolution, Resolution::Stopped) {
            // Preserve the parked state and pending kind until terminalization.
            // That causal evidence is what forbids a new checkpoint while an
            // approval/decision screenshot is the authoritative pre-answer view.
            return resolution;
        }

        let mut r = self.record.lock().await;
        r.pending = None;
        if !r.state.is_terminal() {
            r.state = JobState::Running;
        }
        if !persist_or_mark_failed(&mut r).await {
            return Resolution::PersistenceFailed;
        }
        resolution
    }
}

fn automatic_checkpoint_required(enabled: bool, step: &Step) -> bool {
    enabled && !matches!(step, Step::Checkpoint { .. })
}

#[derive(Debug)]
enum StepOutcome {
    Failed(String),
    Stopped,
    PersistenceFailed,
}

async fn acquire_browser_action(
    action_gate: &Arc<Mutex<()>>,
    stop: &tokio::sync::watch::Receiver<bool>,
    record: &Arc<Mutex<JobRecord>>,
) -> Result<tokio::sync::OwnedMutexGuard<()>, StepOutcome> {
    let guard = Arc::clone(action_gate).lock_owned().await;
    if *stop.borrow() || record.lock().await.state.is_terminal() {
        return Err(StepOutcome::Stopped);
    }
    Ok(guard)
}

/// Everything the daemon keeps about jobs.
#[derive(Default)]
pub struct JobStore {
    pub handles: HashMap<JobId, JobHandle>,
    /// Terminal jobs from this and previous daemon lifetimes.
    pub finished: Vec<JobRecord>,
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_in(artifacts: PathBuf, id: &str, state: JobState) -> JobRecord {
        JobRecord {
            id: id.into(),
            intent: "test persistence".into(),
            state,
            steps: vec![Step::Wait { ms: 1 }],
            cursor: 0,
            log: Vec::new(),
            artifacts,
            pending: None,
            error: None,
            created_ms: 7,
            checkpoint_each_step: false,
            checkpoints: Vec::new(),
        }
    }

    async fn write_committed_checkpoint(path: &Path, name: &str, created_ms: u128) {
        tokio::fs::create_dir_all(path).await.unwrap();
        for file in [
            "snapshot.json",
            "screenshot.png",
            "console-errors.json",
            "network-failures.json",
        ] {
            tokio::fs::write(path.join(file), file.as_bytes())
                .await
                .unwrap();
        }
        let manifest = checkpoint::CheckpointManifest {
            schema_version: 1,
            name: name.into(),
            created_ms,
            url: "https://example.test/".into(),
            title: "fixture".into(),
            generation: 1,
            complete: true,
            pointer_parked: false,
            wait: crate::page::ActionReceipt {
                operation: "checkpoint".into(),
                dispatched: Some(false),
                dispatch_state: crate::page::DispatchState::Prevented,
                navigation_trigger: crate::page::NavigationTrigger::Checkpoint,
                requested_wait: WaitPolicy::Load,
                effective_wait: WaitPolicy::Load,
                outcome: crate::page::WaitOutcome::Loaded,
                navigation: crate::page::NavigationKind::None,
                navigation_scope: crate::page::NavigationScope::None,
                redirect_count: 0,
                before_url: "https://example.test/".into(),
                final_url: "https://example.test/".into(),
                final_url_observed: true,
                before_generation: 1,
                final_generation: 1,
                elapsed_ms: 0,
                discovery_ms: 0,
                timeout_ms: 5_000,
                quiet_ms: 0,
                active_finite_requests: 0,
                excluded_long_lived_requests: 0,
                root_loading: false,
                target_settled: true,
                event_complete: true,
                event_gap_delta: 0,
                history_entry_id: None,
                history_from_index: None,
                history_to_index: None,
                reload_loader_id: None,
                dialog_type: None,
                dialog_message: None,
                stability_note: None,
                guidance: Vec::new(),
                blockers: Vec::new(),
                observed_conditions: std::collections::BTreeMap::new(),
            },
            screenshot: checkpoint::ScreenshotEvidence {
                format: "png".into(),
                width: 1,
                height: 1,
                tile_count: 1,
                tiled: false,
                truncated: None,
            },
            diagnostics: checkpoint::DiagnosticEvidence {
                console_observed: 0,
                console_retained: 0,
                console_limit: 200,
                console_ring_dropped: 0,
                network_observed: 0,
                network_retained: 0,
                network_limit: 200,
                network_ring_dropped: 0,
                event_stream_gaps: 0,
            },
            coverage_gaps: Vec::new(),
            files: std::collections::BTreeMap::new(),
            brow_version: "test".into(),
            browser_version: "test".into(),
            privacy_warning: "fixture".into(),
        };
        tokio::fs::write(
            path.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stop_wins_the_pre_dispatch_gate_race_in_one_hundred_iterations() {
        for iteration in 0..100 {
            let gate = Arc::new(Mutex::new(()));
            let held = Arc::clone(&gate).lock_owned().await;
            let record = Arc::new(Mutex::new(record_in(
                PathBuf::from(format!("/unused/race-{iteration}")),
                "race",
                JobState::Running,
            )));
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

            let waiting_gate = Arc::clone(&gate);
            let waiting_record = Arc::clone(&record);
            let task = tokio::spawn(async move {
                acquire_browser_action(&waiting_gate, &stop_rx, &waiting_record).await
            });
            tokio::task::yield_now().await;
            stop_tx.send(true).expect("raise stop");
            drop(held);

            assert!(matches!(task.await.unwrap(), Err(StepOutcome::Stopped)));
        }
    }

    #[test]
    fn explicit_checkpoint_step_is_not_automatically_duplicated() {
        assert!(automatic_checkpoint_required(
            true,
            &Step::Open {
                url: "https://example.test".into()
            }
        ));
        assert!(!automatic_checkpoint_required(
            true,
            &Step::Checkpoint {
                name: Some("manual".into())
            }
        ));
        assert!(!automatic_checkpoint_required(false, &Step::Wait { ms: 1 }));
    }

    #[test]
    fn steps_parse_from_the_command_line() {
        assert_eq!(
            Step::parse("open https://x.test/a").unwrap(),
            Step::Open {
                url: "https://x.test/a".into()
            }
        );
        assert_eq!(
            Step::parse("click \"Create account\"").unwrap(),
            Step::Click {
                text: "Create account".into()
            }
        );
        assert_eq!(
            Step::parse("wait url 'https://host/issues/*'").unwrap(),
            Step::WaitUrl {
                glob: "https://host/issues/*".into()
            }
        );
        assert_eq!(
            Step::parse("wait generation-after 42").unwrap(),
            Step::WaitGenerationAfter { generation: 42 }
        );
        assert_eq!(Step::parse("wait load").unwrap(), Step::WaitLoad);
        assert_eq!(
            Step::parse("wait stable quiet-ms=450").unwrap(),
            Step::WaitStable { quiet_ms: 450 }
        );
        assert_eq!(Step::parse("back").unwrap(), Step::Back);
        assert_eq!(Step::parse("forward").unwrap(), Step::Forward);
        assert_eq!(
            Step::parse("reload ignore-cache").unwrap(),
            Step::Reload { ignore_cache: true }
        );
        assert_eq!(Step::parse("pointer park").unwrap(), Step::PointerPark);
        assert_eq!(
            Step::parse("checkpoint 'after search'").unwrap(),
            Step::Checkpoint {
                name: Some("after search".into())
            }
        );
        assert_eq!(
            Step::parse("fill Email=someone@example.com").unwrap(),
            Step::Fill {
                field: "Email".into(),
                value: "someone@example.com".into()
            }
        );
        assert_eq!(
            Step::parse("fill \"Email address\" = \"a b\"").unwrap(),
            Step::Fill {
                field: "Email address".into(),
                value: "a b".into()
            }
        );
        assert_eq!(Step::parse("wait 250ms").unwrap(), Step::Wait { ms: 250 });
        assert_eq!(Step::parse("screenshot").unwrap(), Step::Screenshot);
        assert_eq!(Step::parse("check-errors").unwrap(), Step::CheckErrors);
    }

    #[test]
    fn bad_steps_explain_themselves() {
        let e = Step::parse("click").unwrap_err();
        assert!(e.contains("needs"), "{e}");
        let e = Step::parse("fill Email").unwrap_err();
        assert!(e.contains("<field>=<value>"), "{e}");
        let e = Step::parse("frobnicate x").unwrap_err();
        assert!(e.contains("Known steps"), "{e}");
        assert!(Step::parse("wait soon").is_err());
    }

    #[test]
    fn waits_are_capped_so_a_typo_cannot_hang_a_job() {
        assert_eq!(
            Step::parse("wait 99999999").unwrap(),
            Step::Wait { ms: 120_000 }
        );
        let error = Step::parse("wait stable quiet-ms=30001")
            .expect_err("quiet time cannot exceed the typed wait deadline");
        assert!(error.contains("<= 30000ms"), "{error}");
    }

    #[test]
    fn irreversible_actions_are_caught_on_word_boundaries() {
        for label in [
            "Delete account",
            "Send message",
            "Buy now",
            "Publish",
            "PAY $40",
            "Remove from cart",
            "Transfer funds",
        ] {
            assert!(
                irreversible_reason(label).is_some(),
                "{label:?} should require approval"
            );
        }
    }

    #[test]
    fn ordinary_actions_do_not_trip_the_gate() {
        // A gate that fires on everything trains people to approve blindly.
        for label in [
            "Save draft",
            "Sender name",
            "Continue",
            "Next",
            "Search",
            "Log in",
            "Add to cart",
            "Preview",
            "Undelete",
            "Resend?", // "resend" is one word, not "send"
        ] {
            assert_eq!(
                irreversible_reason(label),
                None,
                "{label:?} should not require approval"
            );
        }
    }

    #[test]
    fn the_gate_names_the_word_that_tripped_it() {
        assert_eq!(
            irreversible_reason("Delete this workspace").as_deref(),
            Some("delete")
        );
        assert_eq!(
            irreversible_reason("Confirm purchase").as_deref(),
            Some("purchase")
        );
    }

    #[test]
    fn state_classification_is_consistent() {
        assert!(JobState::Succeeded.is_terminal());
        assert!(JobState::Interrupted.is_terminal());
        assert!(!JobState::NeedsDecision.is_terminal());
        assert!(JobState::NeedsDecision.is_parked());
        assert!(JobState::WaitingForApproval.is_parked());
        assert!(!JobState::Running.is_parked());
    }

    #[test]
    fn job_ids_sort_chronologically() {
        let a = new_job_id(1);
        let b = new_job_id(2);
        assert_ne!(a, b);
        assert!(a.starts_with("job_"));
        // Same millisecond, so the sequence number breaks the tie.
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn status_rendering_tells_the_reader_what_to_do() {
        let mut r = JobRecord {
            id: "job_1".into(),
            intent: "check the signup flow".into(),
            state: JobState::WaitingForApproval,
            steps: vec![Step::Screenshot],
            cursor: 0,
            log: vec![],
            artifacts: PathBuf::from("/tmp/j"),
            pending: Some(Pending {
                id: 1,
                kind: PendingKind::Approval {
                    action: "click \"Delete account\"".into(),
                    reason: "delete".into(),
                    evidence: Some(PathBuf::from("/tmp/j/approval-001.png")),
                },
                asked_at_ms: 0,
                expires_at_ms: 0,
                generation: 3,
            }),
            error: None,
            created_ms: 0,
            checkpoint_each_step: false,
            checkpoints: Vec::new(),
        };
        let text = r.render_status();
        assert!(text.contains("waiting_for_approval"));
        assert!(text.contains("brow job approve job_1"), "{text}");
        assert!(text.contains("irreversible"), "{text}");

        r.state = JobState::NeedsDecision;
        r.pending = Some(Pending {
            id: 2,
            kind: PendingKind::Decision {
                question: "3 elements match \"Continue\"".into(),
                options: vec!["button \"Continue\"".into(), "a \"Continue\"".into()],
            },
            asked_at_ms: 0,
            expires_at_ms: 0,
            generation: 3,
        });
        let text = r.render_status();
        assert!(text.contains("brow job answer job_1"), "{text}");
        assert!(text.contains("[0] button"), "{text}");
    }

    #[tokio::test]
    async fn stale_control_cannot_answer_the_next_park() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("job_controls");
        let record = Arc::new(Mutex::new(record_in(
            artifacts,
            "job_controls",
            JobState::Running,
        )));
        let (control_tx, control_rx) = mpsc::channel(2);
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        control_tx
            .send(ControlMessage {
                pending_id: 1,
                control: Control::Approve,
            })
            .await
            .unwrap();
        let mut runner = Runner {
            record,
            control: control_rx,
            stop: stop_rx,
            action_gate: Arc::new(Mutex::new(())),
            // The new park will be id 2; id 1 is a duplicate from the previous
            // approval that arrived after it had already been consumed.
            next_pending_id: 1,
        };

        let correct = control_tx.clone();
        let answer = async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            correct
                .send(ControlMessage {
                    pending_id: 2,
                    control: Control::Reject,
                })
                .await
                .unwrap();
        };
        let parked = runner.park(
            JobState::WaitingForApproval,
            PendingKind::Approval {
                action: "click button \"Delete workspace\"".into(),
                reason: "delete".into(),
                evidence: None,
            },
            Duration::from_secs(1),
            1,
        );
        let (resolution, ()) = tokio::join!(parked, answer);
        assert!(matches!(resolution, Resolution::Rejected));
    }

    #[tokio::test]
    async fn stop_wins_when_approval_is_already_queued() {
        let root = tempfile::tempdir().unwrap();
        let record = Arc::new(Mutex::new(record_in(
            root.path().join("job_stop_priority"),
            "job_stop_priority",
            JobState::Running,
        )));
        let (control_tx, control_rx) = mpsc::channel(1);
        control_tx
            .send(ControlMessage {
                pending_id: 1,
                control: Control::Approve,
            })
            .await
            .unwrap();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        stop_tx.send(true).unwrap();
        let observed_record = Arc::clone(&record);
        let mut runner = Runner {
            record,
            control: control_rx,
            stop: stop_rx,
            action_gate: Arc::new(Mutex::new(())),
            next_pending_id: 0,
        };
        let resolution = runner
            .park(
                JobState::WaitingForApproval,
                PendingKind::Approval {
                    action: "click button \"Delete workspace\"".into(),
                    reason: "delete".into(),
                    evidence: None,
                },
                Duration::from_secs(1),
                1,
            )
            .await;
        assert!(matches!(resolution, Resolution::Stopped));
        let record = observed_record.lock().await;
        assert_eq!(record.state, JobState::WaitingForApproval);
        assert!(
            matches!(
                record.pending,
                Some(Pending {
                    kind: PendingKind::Approval { .. },
                    ..
                })
            ),
            "stop must preserve the approval park until terminalization"
        );
    }

    #[tokio::test]
    async fn atomic_persist_commits_valid_json_and_removes_the_temp_name() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("job_atomic");
        let record = record_in(artifacts.clone(), "job_atomic", JobState::Queued);

        try_persist(&record).await.unwrap();

        let stored: JobRecord = serde_json::from_slice(
            &tokio::fs::read(artifacts.join(MANIFEST_NAME))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored.id, "job_atomic");
        assert_eq!(stored.state, JobState::Queued);
        assert!(!artifacts.join(MANIFEST_TEMP_NAME).exists());
    }

    #[tokio::test]
    async fn manifest_transaction_has_no_cancellable_await_boundary() {
        let root = tempfile::tempdir().unwrap();
        let record = record_in(
            root.path().join("job_non_cancellable"),
            "job_non_cancellable",
            JobState::Stopped,
        );
        tokio::time::timeout(Duration::ZERO, try_persist(&record))
            .await
            .expect("the atomic manifest transaction must complete in one poll")
            .expect("persist manifest");
        let stored: JobRecord =
            serde_json::from_slice(&std::fs::read(record.artifacts.join(MANIFEST_NAME)).unwrap())
                .unwrap();
        assert_eq!(stored.state, JobState::Stopped);
    }

    #[tokio::test]
    async fn durable_manifest_redacts_plan_and_prose_without_mutating_live_job() {
        let root = tempfile::tempdir().unwrap();
        let mut record = record_in(
            root.path().join("job_private"),
            "job_private",
            JobState::Failed,
        );
        record.intent =
            "inspect https://intent-user:intent-pass@app.test/?token=intent-query".into();
        record.steps = vec![
            Step::Open {
                url: "https://open-user:open-pass@app.test/?access_token=open-query".into(),
            },
            Step::WaitUrl {
                glob: "*#refresh_token=wait-fragment*".into(),
            },
            Step::Fill {
                field: "Password".into(),
                value: "typed-form-secret".into(),
            },
        ];
        record.log.push(LogLine {
            t_ms: 8,
            text: "failed https://log-user:log-pass@app.test/#token=log-fragment".into(),
        });
        record.error = Some(
            "request https://error-user:error-pass@app.test/?api_key=error-query failed".into(),
        );
        let live_steps = record.steps.clone();
        let live_intent = record.intent.clone();

        try_persist(&record)
            .await
            .expect("persist privacy-safe manifest");

        assert_eq!(record.steps, live_steps, "Runner keeps the executable plan");
        assert_eq!(record.intent, live_intent, "live status remains unchanged");
        let encoded = std::fs::read_to_string(record.artifacts.join(MANIFEST_NAME)).unwrap();
        for secret in [
            "intent-user",
            "intent-pass",
            "intent-query",
            "open-user",
            "open-pass",
            "open-query",
            "wait-fragment",
            "typed-form-secret",
            "log-user",
            "log-pass",
            "log-fragment",
            "error-user",
            "error-pass",
            "error-query",
        ] {
            assert!(
                !encoded.contains(secret),
                "job.json leaked {secret}: {encoded}"
            );
        }
        assert!(encoded.contains(crate::redact::MASK));
        assert!(
            Step::Open {
                url: "https://describe-user:describe-pass@app.test/?token=describe-secret".into()
            }
            .describe()
            .contains(crate::redact::MASK),
            "the live progress log must redact Open URLs at source"
        );
    }

    #[test]
    fn job_log_redacts_before_in_memory_and_tracing_ingest() {
        let mut record = record_in(PathBuf::from("/unused"), "job_log", JobState::Running);
        record.log(
            "open https://trace-user:trace-pass@app.test/?token=trace-query with Bearer abcdefgh",
        );
        let stored = &record.log[0].text;
        for secret in ["trace-user", "trace-pass", "trace-query", "abcdefgh"] {
            assert!(
                !stored.contains(secret),
                "log line leaked {secret}: {stored}"
            );
        }
        assert!(stored.contains(crate::redact::MASK));
    }

    #[test]
    fn pre_feature_manifest_defaults_checkpoint_fields() {
        let old = serde_json::json!({
            "id": "job_old",
            "intent": "old manifest",
            "state": "succeeded",
            "steps": [{"verb": "wait", "ms": 1}],
            "cursor": 1,
            "log": [],
            "artifacts": "/tmp/job_old",
            "created_ms": 7
        });
        let record: JobRecord = serde_json::from_value(old).expect("load old manifest");
        assert!(!record.checkpoint_each_step);
        assert!(record.checkpoints.is_empty());
        assert_eq!(record.steps, vec![Step::Wait { ms: 1 }]);
    }

    #[tokio::test]
    async fn persistence_failure_is_visible_and_keeps_the_last_good_manifest() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("job_failed_write");
        let mut record = record_in(artifacts.clone(), "job_failed_write", JobState::Queued);
        try_persist(&record).await.unwrap();

        // Blocking the temp-file path fails before the atomic rename, so the
        // previously committed job.json must remain intact.
        tokio::fs::create_dir(artifacts.join(MANIFEST_TEMP_NAME))
            .await
            .unwrap();
        record.state = JobState::Running;
        assert!(!persist_or_mark_failed(&mut record).await);
        assert_eq!(record.state, JobState::Failed);
        assert!(record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("exists only in memory"));

        let durable: JobRecord = serde_json::from_slice(
            &tokio::fs::read(artifacts.join(MANIFEST_NAME))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(durable.state, JobState::Queued);
    }

    #[tokio::test]
    async fn load_previous_recovers_temp_and_preserves_corrupt_original() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("job_recoverable");
        tokio::fs::create_dir_all(&artifacts).await.unwrap();
        tokio::fs::write(artifacts.join(MANIFEST_NAME), b"{not json")
            .await
            .unwrap();
        let record = record_in(artifacts.clone(), "job_recoverable", JobState::Succeeded);
        tokio::fs::write(
            artifacts.join(MANIFEST_TEMP_NAME),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .await
        .unwrap();

        let loaded = load_previous(root.path()).await;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "job_recoverable");
        assert_eq!(loaded[0].state, JobState::Succeeded);
        let repaired: JobRecord = serde_json::from_slice(
            &tokio::fs::read(artifacts.join(MANIFEST_NAME))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(repaired.id, "job_recoverable");
        let preserved = std::fs::read_dir(&artifacts)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("job.json.corrupt-")
            })
            .expect("the corrupt original must be quarantined");
        assert_eq!(std::fs::read(preserved.path()).unwrap(), b"{not json");
    }

    #[tokio::test]
    async fn restart_reconciles_a_published_checkpoint_from_the_manifest_crash_window() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("job_checkpoint_crash");
        let checkpoints = artifacts.join("checkpoints");
        let final_path = checkpoints.join("after-step-001");
        let partial_path = checkpoints.join(".after-step-002.partial-123-0");

        let mut record = record_in(artifacts.clone(), "job_checkpoint_crash", JobState::Running);
        record.checkpoint_each_step = true;
        record.checkpoints.push(partial_path.clone()); // stale/torn legacy claim
        try_persist(&record).await.unwrap();
        write_committed_checkpoint(&final_path, "after-step-001", 10).await;
        tokio::fs::create_dir_all(&partial_path).await.unwrap();
        tokio::fs::write(partial_path.join("manifest.json"), b"{partial")
            .await
            .unwrap();

        let loaded = load_previous(root.path()).await;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].state, JobState::Interrupted);
        assert_eq!(loaded[0].cursor, 0, "a crash must not invent step success");
        assert_eq!(loaded[0].checkpoints, vec![final_path.clone()]);
        assert!(
            partial_path.is_dir(),
            "partial evidence was promoted or removed"
        );
        assert!(loaded[0].log.iter().any(|line| line
            .text
            .contains("checkpoint recovery found committed evidence")));

        let durable: JobRecord = serde_json::from_slice(
            &tokio::fs::read(artifacts.join(MANIFEST_NAME))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(durable.state, JobState::Interrupted);
        assert_eq!(durable.checkpoints, vec![final_path]);

        // Recovery is idempotent: a second daemon lifetime neither duplicates
        // the path nor promotes the partial.
        let reloaded = load_previous(root.path()).await;
        assert_eq!(reloaded[0].checkpoints.len(), 1);
        assert!(partial_path.is_dir());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_checkpoint_crash_boundary_is_reconciled_without_promoting_partials() {
        if crate::browser::find().is_err() {
            eprintln!(
                "SKIP real_checkpoint_crash_boundary_is_reconciled_without_promoting_partials"
            );
            return;
        }

        let _slot = crate::browser::test_browser_slot();
        let repetitions = std::env::var("BROW_JOB_CRASH_REPETITIONS")
            .map(|value| {
                value
                    .parse::<usize>()
                    .expect("BROW_JOB_CRASH_REPETITIONS must be an integer")
                    .clamp(1, 100)
            })
            .unwrap_or(3);
        let jobs_root = tempfile::tempdir().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let mut options = crate::browser::LaunchOptions::new(profile.path().join("profile"));
        options.headless = crate::browser::Headless::New;
        options.window_size = (320, 240);
        let mut launched = crate::browser::launch(&options)
            .await
            .expect("launch Chromium");
        let mut page = Page::create(Arc::clone(&launched.client), "about:blank")
            .await
            .expect("create job checkpoint page");
        let fixture_installed = page
            .evaluate(
                "(() => { document.documentElement.innerHTML = \
                 '<head><title>crash-window</title></head><body><main>crash-window evidence</main></body>'; \
                 return document.body?.textContent === 'crash-window evidence'; })()",
                false,
            )
            .await
            .expect("install checkpoint fixture");
        assert_eq!(
            fixture_installed,
            serde_json::Value::Bool(true),
            "checkpoint fixture did not install into a real document"
        );

        let mut expected = Vec::new();
        for iteration in 0..repetitions {
            let id = format!("job_real_checkpoint_crash_{iteration}");
            let artifacts = jobs_root.path().join(&id);
            let mut record = record_in(artifacts.clone(), &id, JobState::Running);
            record.checkpoint_each_step = true;
            try_persist(&record)
                .await
                .expect("persist pre-checkpoint running state");
            let record = Arc::new(Mutex::new(record));
            let (_control_tx, control) = mpsc::channel(1);
            let (_stop_tx, stop) = tokio::sync::watch::channel(false);
            let runner = Runner {
                record: Arc::clone(&record),
                control,
                stop,
                action_gate: Arc::new(Mutex::new(())),
                next_pending_id: 0,
            };
            let hold = checkpoint_publication_test_support::hold_next(&id);
            let checkpoint_name = format!("crash-window-{iteration:03}");

            // Dropping this future while the production path is paused models
            // process death at the exact boundary: checkpoint::create already
            // atomically renamed the final directory, but this Runner has not
            // appended the path or attempted job.json persistence yet.
            let published_path = {
                let capture = runner.capture_job_checkpoint_with_policy_and_timeout(
                    &mut page,
                    checkpoint_name,
                    WaitPolicy::Load,
                    Duration::from_secs(5),
                );
                tokio::pin!(capture);
                tokio::select! {
                    path = tokio::time::timeout(
                        Duration::from_secs(20),
                        hold.wait_until_entered(),
                    ) => path.expect("checkpoint did not reach the publication crash boundary"),
                    result = &mut capture => {
                        panic!("checkpoint escaped the crash barrier: {result:?}")
                    }
                }
            };
            drop(hold);

            assert!(published_path.join("manifest.json").is_file());
            assert!(
                record.lock().await.checkpoints.is_empty(),
                "the in-memory record advanced past the crash boundary"
            );
            let before_restart: JobRecord = serde_json::from_slice(
                &tokio::fs::read(artifacts.join(MANIFEST_NAME))
                    .await
                    .expect("pre-crash job manifest"),
            )
            .expect("valid pre-crash job manifest");
            assert_eq!(before_restart.state, JobState::Running);
            assert!(
                before_restart.checkpoints.is_empty(),
                "job.json was persisted after the held publication"
            );

            // A complete-looking hidden partial is deliberately left beside the
            // real final bundle. Recovery must never infer commitment from a
            // dot-prefixed staging name or promote it into the inventory.
            let partial_path = artifacts
                .join("checkpoints")
                .join(format!(".unpublished-{iteration:03}.partial-test"));
            tokio::fs::create_dir(&partial_path)
                .await
                .expect("create interrupted partial directory");
            for name in [
                "snapshot.json",
                "screenshot.png",
                "console-errors.json",
                "network-failures.json",
                "manifest.json",
            ] {
                tokio::fs::copy(published_path.join(name), partial_path.join(name))
                    .await
                    .expect("copy committed evidence into partial fixture");
            }
            expected.push((id, published_path, partial_path));
        }

        let recovered = load_previous(jobs_root.path()).await;
        assert_eq!(recovered.len(), repetitions);
        for (id, final_path, partial_path) in &expected {
            let record = recovered
                .iter()
                .find(|record| record.id == *id)
                .expect("recover every interrupted job");
            assert_eq!(record.state, JobState::Interrupted);
            assert_eq!(
                record
                    .checkpoints
                    .iter()
                    .filter(|path| *path == final_path)
                    .count(),
                1,
                "the real final checkpoint was omitted or duplicated"
            );
            assert_eq!(record.checkpoints, vec![final_path.clone()]);
            assert!(
                record.log.iter().any(|line| line
                    .text
                    .contains("checkpoint recovery found committed evidence")),
                "the crash-window checkpoint was not explicitly reconciled"
            );
            assert!(
                partial_path.is_dir(),
                "recovery promoted or removed a partial"
            );
            assert!(!record.checkpoints.contains(partial_path));

            let durable: JobRecord = serde_json::from_slice(
                &tokio::fs::read(record.artifacts.join(MANIFEST_NAME))
                    .await
                    .expect("recovered job manifest"),
            )
            .expect("valid recovered job manifest");
            assert_eq!(durable.state, JobState::Interrupted);
            assert_eq!(durable.checkpoints, vec![final_path.clone()]);
        }

        // A second restart proves reconciliation is idempotent and still does
        // not treat any staged path as committed evidence.
        let reloaded = load_previous(jobs_root.path()).await;
        for (id, final_path, partial_path) in &expected {
            let record = reloaded
                .iter()
                .find(|record| record.id == *id)
                .expect("reload every interrupted job");
            assert_eq!(record.checkpoints, vec![final_path.clone()]);
            assert!(partial_path.is_dir());
            assert!(!record.checkpoints.contains(partial_path));
        }

        let _ = launched.child.kill();
        let _ = launched.child.wait();
    }

    #[tokio::test]
    async fn load_previous_surfaces_an_unrecoverable_corrupt_manifest() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("job_corrupt");
        tokio::fs::create_dir_all(&artifacts).await.unwrap();
        tokio::fs::write(artifacts.join(MANIFEST_NAME), b"{still not json")
            .await
            .unwrap();

        let loaded = load_previous(root.path()).await;

        assert_eq!(loaded.len(), 1, "corrupt jobs must not disappear");
        assert_eq!(loaded[0].id, "job_corrupt");
        assert_eq!(loaded[0].state, JobState::Failed);
        assert!(loaded[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("corrupt manifest"));
        assert_eq!(
            tokio::fs::read(artifacts.join(MANIFEST_NAME))
                .await
                .unwrap(),
            b"{still not json"
        );
    }
}
