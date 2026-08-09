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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result as AnyResult};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Mutex};

use crate::page::{ImageFormat, MouseButton, NodeFingerprint, Page, ScreenshotTarget};

/// How long a parked job waits before giving up.
///
/// Approval is deliberately shorter: it is a human interrupt, and a stale
/// approval request is worse than none — the page it was reasoning about has
/// moved on.
pub const DECISION_TTL: Duration = Duration::from_secs(30 * 60);
pub const APPROVAL_TTL: Duration = Duration::from_secs(15 * 60);

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
                let ms = rest
                    .trim_end_matches("ms")
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("step `wait` wants milliseconds, got `{rest}`"))?;
                Step::Wait {
                    ms: ms.min(120_000),
                }
            }
            "screenshot" => Step::Screenshot,
            "check-errors" | "check_errors" => Step::CheckErrors,
            other => {
                return Err(format!(
                    "unknown step `{other}`. Known steps: open, click, fill, press, wait, \
                     screenshot, check-errors"
                ))
            }
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Step::Open { url } => format!("open {url}"),
            Step::Click { text } => format!("click {text:?}"),
            Step::Fill { field, value } => format!("fill {field:?} with {value:?}"),
            Step::Press { chord } => format!("press {chord}"),
            Step::Wait { ms } => format!("wait {ms}ms"),
            Step::Screenshot => "screenshot".into(),
            Step::CheckErrors => "check for errors".into(),
        }
    }
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
}

impl JobRecord {
    fn log(&mut self, text: impl Into<String>) {
        let line = LogLine {
            t_ms: now_ms(),
            text: text.into(),
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
    let mut bytes = serde_json::to_vec_pretty(record)
        .with_context(|| format!("serialize job {}", record.id))?;
    bytes.push(b'\n');

    tokio::fs::create_dir_all(&record.artifacts)
        .await
        .with_context(|| format!("create {}", record.artifacts.display()))?;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .await
        .with_context(|| format!("open temporary manifest {}", temp.display()))?;
    file.write_all(&bytes)
        .await
        .with_context(|| format!("write temporary manifest {}", temp.display()))?;
    file.flush()
        .await
        .with_context(|| format!("flush temporary manifest {}", temp.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("sync temporary manifest {}", temp.display()))?;
    drop(file);

    tokio::fs::rename(&temp, &path)
        .await
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    sync_directory(&record.artifacts).await
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
        if !record.state.is_terminal() {
            record.state = JobState::Interrupted;
            record.pending = None;
            record.error = Some(
                "the daemon stopped while this job was running; its browser went with it, \
                 so it cannot be resumed"
                    .into(),
            );
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
            let mut r = self.record.lock().await;
            if r.state.is_terminal() || *self.stop.borrow() {
                return;
            }
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
                    result = self.execute(page, &step) => result,
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
                    let mut r = self.record.lock().await;
                    // `job_stop` may already have made the authoritative state
                    // terminal, including Failed when its durable stop write did
                    // not commit. Never overwrite that outcome with Stopped.
                    if r.state.is_terminal() {
                        break;
                    }
                    r.state = JobState::Stopped;
                    r.log("stopped on request");
                    persist_or_mark_failed(&mut r).await;
                    break;
                }
                Err(StepOutcome::Failed(why)) => {
                    let mut r = self.record.lock().await;
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
            Step::Press { chord } => {
                let _action = self.begin_browser_action().await?;
                page.press(chord)
                    .await
                    .map_err(|e| StepOutcome::Failed(e.to_string()))
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

        page.click_if_unchanged(&node_ref, MouseButton::Left, 1, 0, &reviewed_target)
            .await
            .map(|_| ())
            .map_err(|e| StepOutcome::Failed(e.to_string()))
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
        let guard = Arc::clone(&self.action_gate).lock_owned().await;
        if *self.stop.borrow() || self.record.lock().await.state.is_terminal() {
            return Err(StepOutcome::Stopped);
        }
        Ok(guard)
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

enum StepOutcome {
    Failed(String),
    Stopped,
    PersistenceFailed,
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
        }
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
