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
//!   publish, send). Only a **human** answers this, with `brow job approve`. An
//!   agent answering its own approval request would make the gate decorative.
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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

use crate::page::{ImageFormat, MouseButton, Page, ScreenshotTarget};

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
    /// Blocked on something irreversible. A human answers.
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
            JobState::Succeeded
                | JobState::Failed
                | JobState::Stopped
                | JobState::Interrupted
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
    Open { url: String },
    Click { text: String },
    Fill { field: String, value: String },
    Press { chord: String },
    Wait { ms: u64 },
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
                Step::Open { url: rest.to_string() }
            }
            "click" => {
                need("<visible text>")?;
                Step::Click { text: unquote(rest) }
            }
            "fill" => {
                need("<field>=<value>")?;
                let (field, value) = rest.split_once('=').ok_or_else(|| {
                    format!("step `fill` wants <field>=<value>, got `{rest}`")
                })?;
                Step::Fill {
                    field: unquote(field.trim()),
                    value: unquote(value.trim()),
                }
            }
            "press" => {
                need("<key or chord>")?;
                Step::Press { chord: rest.to_string() }
            }
            "wait" => {
                need("<milliseconds>")?;
                let ms = rest
                    .trim_end_matches("ms")
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("step `wait` wants milliseconds, got `{rest}`"))?;
                Step::Wait { ms: ms.min(120_000) }
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
    "delete", "remove", "destroy", "erase", "wipe", "purchase", "buy", "pay",
    "checkout", "order", "subscribe", "unsubscribe", "publish", "send",
    "deactivate", "terminate", "withdraw", "transfer", "revoke", "archive",
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
                    s.push_str(&format!("answer with: brow job answer {} <index>\n", self.id));
                }
                PendingKind::Approval { action, reason, evidence } => {
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
    Stop,
}

/// The daemon's handle on a running job.
pub struct JobHandle {
    pub record: Arc<Mutex<JobRecord>>,
    pub control: mpsc::Sender<Control>,
    /// Interrupts a step that is already executing.
    ///
    /// Separate from `control` because a running job is not reading its control
    /// channel — it is inside a navigation or a `wait`. Without this, `job stop`
    /// marked the job stopped and returned success while the browser kept running
    /// until the current step finished, which for `wait 60000` is a minute of
    /// lying to the caller.
    pub stop: tokio::sync::watch::Sender<bool>,
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

/// Writes the manifest so a later `brow job status` can still explain what
/// happened, even after the daemon that ran it is gone.
pub async fn persist(record: &JobRecord) {
    let path = record.artifacts.join("job.json");
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Ok(text) = serde_json::to_string_pretty(record) {
        let _ = tokio::fs::write(&path, text).await;
    }
}

/// Reads back manifests from previous daemon lifetimes.
///
/// Anything that was mid-flight is marked [`JobState::Interrupted`]: the browser
/// it was driving no longer exists, so claiming it could resume would be a lie.
pub async fn load_previous(root: &std::path::Path) -> Vec<JobRecord> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let manifest = entry.path().join("job.json");
        let Ok(text) = tokio::fs::read_to_string(&manifest).await else {
            continue;
        };
        let Ok(mut record) = serde_json::from_str::<JobRecord>(&text) else {
            continue;
        };
        if !record.state.is_terminal() {
            record.state = JobState::Interrupted;
            record.pending = None;
            record.error = Some(
                "the daemon stopped while this job was running; its browser went with it, \
                 so it cannot be resumed"
                    .into(),
            );
            persist(&record).await;
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
}

/// Runs a plan to completion, parking for input when the heuristics run out.
pub struct Runner {
    pub record: Arc<Mutex<JobRecord>>,
    pub control: mpsc::Receiver<Control>,
    pub stop: tokio::sync::watch::Receiver<bool>,
}

impl Runner {
    pub async fn run(mut self, page: &mut Page) {
        {
            let mut r = self.record.lock().await;
            r.state = JobState::Running;
            let intent = r.intent.clone();
            r.log(format!("started: {intent}"));
            persist(&r).await;
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
            let outcome = tokio::select! {
                result = self.execute(page, &step) => result,
                _ = stop.changed() => Err(StepOutcome::Stopped),
            };

            match outcome {
                Ok(()) => {
                    let mut r = self.record.lock().await;
                    r.cursor += 1;
                    persist(&r).await;
                }
                Err(StepOutcome::Stopped) => {
                    let mut r = self.record.lock().await;
                    r.state = JobState::Stopped;
                    r.log("stopped on request");
                    persist(&r).await;
                    break;
                }
                Err(StepOutcome::Failed(why)) => {
                    let mut r = self.record.lock().await;
                    r.state = JobState::Failed;
                    r.log(format!("failed: {why}"));
                    r.error = Some(why);
                    persist(&r).await;
                    break;
                }
            }
        }

        let mut r = self.record.lock().await;
        if !r.state.is_terminal() {
            r.state = JobState::Succeeded;
            r.log("finished");
        }
        persist(&r).await;
    }

    async fn execute(&mut self, page: &mut Page, step: &Step) -> Result<(), StepOutcome> {
        {
            let mut r = self.record.lock().await;
            let n = r.cursor + 1;
            let total = r.steps.len();
            r.log(format!("[{n}/{total}] {}", step.describe()));
        }

        match step {
            Step::Open { url } => page
                .navigate(url)
                .await
                .map_err(|e| StepOutcome::Failed(e.to_string())),
            Step::Wait { ms } => {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                Ok(())
            }
            Step::Press { chord } => page
                .press(chord)
                .await
                .map_err(|e| StepOutcome::Failed(e.to_string())),
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
                if errors.is_empty() && failures.is_empty() {
                    self.record.lock().await.log("no console errors, no failed requests");
                    return Ok(());
                }
                let mut detail = String::new();
                for e in errors.iter().take(10) {
                    detail.push_str(&format!("\n  console: {}", e.text));
                }
                for f in failures.iter().take(10) {
                    detail.push_str(&format!(
                        "\n  network: {} {}",
                        f.error.clone().unwrap_or_else(|| f
                            .status
                            .map(|s| s.to_string())
                            .unwrap_or_default()),
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
        let node_ref = match self.resolve_one(page, query, false).await? {
            Some(r) => r,
            None => return Ok(()), // stopped or rejected; state already set
        };

        // The irreversibility gate sits between resolving the target and touching
        // it, so the human sees the actual element the job is about to click.
        if let Some(reason) = irreversible_reason(query) {
            let evidence = self.capture_evidence(page).await;
            let generation = page.generation();
            let action = format!("click {query:?}");
            match self
                .park(
                    JobState::WaitingForApproval,
                    PendingKind::Approval { action, reason, evidence },
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
            }

            // Re-check after the wait: an approval granted against a page that has
            // since navigated is about something else entirely.
            if page.generation() != generation {
                return Err(StepOutcome::Failed(
                    "the page changed while waiting for approval, so the approval no longer \
                     applies to what was reviewed"
                        .into(),
                ));
            }
        }

        page.click(&node_ref, MouseButton::Left, 1, 0, false)
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
        let node_ref = match self.resolve_one(page, field, true).await? {
            Some(r) => r,
            None => return Ok(()),
        };
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
    ) -> Result<Option<String>, StepOutcome> {
        let snap = page
            .snapshot()
            .await
            .map_err(|e| StepOutcome::Failed(e.to_string()))?;
        let needle = query.to_ascii_lowercase();

        let candidates: Vec<(String, String)> = snap
            .interactive()
            .filter(|n| {
                !fields_only || matches!(n.tag.as_str(), "input" | "textarea" | "select")
            })
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
            1 => Ok(Some(candidates[0].0.clone())),
            _ => {
                // Refusing to guess here is the whole point: picking the first of
                // three "Continue" buttons is how a background job silently does
                // the wrong thing for forty minutes.
                let generation = page.generation();
                let options: Vec<String> = candidates.iter().map(|c| c.1.clone()).collect();
                let question = format!("{} elements match {query:?}", candidates.len());
                match self
                    .park(
                        JobState::NeedsDecision,
                        PendingKind::Decision { question, options: options.clone() },
                        DECISION_TTL,
                        generation,
                    )
                    .await
                {
                    Resolution::Answered(answer) => {
                        let index = answer.trim().parse::<usize>().map_err(|_| {
                            StepOutcome::Failed(format!(
                                "expected an option index, got {answer:?}"
                            ))
                        })?;
                        let chosen = candidates.get(index).ok_or_else(|| {
                            StepOutcome::Failed(format!(
                                "option {index} does not exist; there were {}",
                                candidates.len()
                            ))
                        })?;
                        if page.generation() != generation {
                            return Err(StepOutcome::Failed(
                                "the page changed while waiting for an answer, so the chosen \
                                 element no longer exists"
                                    .into(),
                            ));
                        }
                        self.record
                            .lock()
                            .await
                            .log(format!("agent chose [{index}] {}", chosen.1));
                        Ok(Some(chosen.0.clone()))
                    }
                    Resolution::Stopped => Err(StepOutcome::Stopped),
                    Resolution::TimedOut => Err(StepOutcome::Failed(
                        "nobody answered the decision request within the time limit".into(),
                    )),
                    Resolution::Approved | Resolution::Rejected => Err(StepOutcome::Failed(
                        "this is a decision for the agent, not an approval; use `job answer`"
                            .into(),
                    )),
                }
            }
        }
    }

    async fn capture_evidence(&self, page: &mut Page) -> Option<PathBuf> {
        let shot = page
            .screenshot(ScreenshotTarget::Viewport, ImageFormat::Png, None)
            .await
            .ok()?;
        let path = {
            let r = self.record.lock().await;
            r.artifacts.join(format!("approval-{:03}.png", r.cursor + 1))
        };
        shot.write_to(&path).await.ok()
    }

    /// Parks the job and waits for the control socket, a stop, or the deadline.
    async fn park(
        &mut self,
        state: JobState,
        kind: PendingKind,
        ttl: Duration,
        generation: u64,
    ) -> Resolution {
        {
            let mut r = self.record.lock().await;
            r.state = state;
            r.pending = Some(Pending {
                kind: kind.clone(),
                asked_at_ms: now_ms(),
                expires_at_ms: now_ms() + ttl.as_millis(),
                generation,
            });
            match &kind {
                PendingKind::Decision { question, .. } => {
                    r.log(format!("parked, needs a decision: {question}"))
                }
                PendingKind::Approval { action, reason, .. } => {
                    r.log(format!("parked for human approval: {action} (irreversible: {reason})"))
                }
            }
            persist(&r).await;
        }

        let resolution = match tokio::time::timeout(ttl, self.control.recv()).await {
            Err(_) => Resolution::TimedOut,
            Ok(None) => Resolution::Stopped,
            Ok(Some(Control::Answer(a))) => Resolution::Answered(a),
            Ok(Some(Control::Approve)) => Resolution::Approved,
            Ok(Some(Control::Reject)) => Resolution::Rejected,
            Ok(Some(Control::Stop)) => Resolution::Stopped,
        };

        let mut r = self.record.lock().await;
        r.pending = None;
        if !r.state.is_terminal() {
            r.state = JobState::Running;
        }
        persist(&r).await;
        resolution
    }
}

enum StepOutcome {
    Failed(String),
    Stopped,
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

    #[test]
    fn steps_parse_from_the_command_line() {
        assert_eq!(
            Step::parse("open https://x.test/a").unwrap(),
            Step::Open { url: "https://x.test/a".into() }
        );
        assert_eq!(
            Step::parse("click \"Create account\"").unwrap(),
            Step::Click { text: "Create account".into() }
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
            Step::Fill { field: "Email address".into(), value: "a b".into() }
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
        assert_eq!(Step::parse("wait 99999999").unwrap(), Step::Wait { ms: 120_000 });
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
        assert_eq!(irreversible_reason("Delete this workspace").as_deref(), Some("delete"));
        assert_eq!(irreversible_reason("Confirm purchase").as_deref(), Some("purchase"));
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
}
