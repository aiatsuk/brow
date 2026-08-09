//! Background jobs, end to end through the real binary.
//!
//! The assertions that matter here are about the two gates. A job that silently
//! guesses which "Continue" to click, or that deletes something without asking,
//! is worse than a job that does nothing — so these tests are mostly about the
//! engine *refusing* to act.

mod common;

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_brow");

struct Harness {
    home: std::path::PathBuf,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let home = std::path::PathBuf::from(format!("/tmp/brow-job-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create BROW_HOME");
        Harness { home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("BROW_HOME", &self.home)
            .output()
            .expect("run brow")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "`brow {}` failed\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut args = args.to_vec();
        args.push("--json");
        let stdout = self.ok(&args);
        serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("`brow {}` printed non-JSON: {e}\n{stdout}", args.join(" ")))
    }

    /// Polls until the job reaches one of `states`, then returns its status.
    fn wait_for(&self, id: &str, states: &[&str], secs: u64) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut last = serde_json::Value::Null;
        while std::time::Instant::now() < deadline {
            last = self.json(&["job", "status", id]);
            if states.contains(&last["state"].as_str().unwrap_or("")) {
                return last;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("job {id} never reached {states:?}; last status was {last:#}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["daemon", "stop"])
            .env("BROW_HOME", &self.home)
            .output();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn a_job_runs_a_plan_to_completion_in_the_background() {
    if !common::chrome_available() {
        return common::skip("a_job_runs_a_plan_to_completion_in_the_background");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("run");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "check that the signup button works",
        "--step",
        &format!("open {}", fixture.url("/")),
        "--step",
        "click \"Create account\"",
        "--step",
        "screenshot",
        "--step",
        "check-errors",
    ]);
    let id = started["id"].as_str().expect("a job id").to_string();
    assert_eq!(started["state"], "queued");

    // `job start` must return immediately rather than block for the run.
    let status = h.wait_for(&id, &["succeeded", "failed"], 60);
    assert_eq!(status["state"], "succeeded", "job failed: {status:#}");
    assert_eq!(status["cursor"], 4, "not every step ran: {status:#}");

    // The log is the record of what actually happened.
    let log: Vec<String> = status["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["text"].as_str().unwrap_or_default().to_string())
        .collect();
    let joined = log.join("\n");
    assert!(joined.contains("[2/4] click"), "{joined}");
    assert!(joined.contains("no console errors"), "{joined}");

    // The screenshot step wrote a real artifact.
    let artifacts = std::path::PathBuf::from(status["artifacts"].as_str().unwrap());
    let shot = artifacts.join("step-003.png");
    let bytes = std::fs::read(&shot).expect("the screenshot step must write a file");
    assert!(common::png_size(&bytes).is_some(), "not a valid PNG");

    // A finished job stays listable.
    let listed = h.json(&["job", "list"]);
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .any(|j| j["id"] == id.as_str() && j["state"] == "succeeded"));

    h.ok(&["daemon", "stop"]);
}

#[test]
fn an_ambiguous_step_parks_for_the_agent_instead_of_guessing() {
    if !common::chrome_available() {
        return common::skip("an_ambiguous_step_parks_for_the_agent_instead_of_guessing");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("decide");
    let fixture = common::serve();

    // "Continue" matches three controls on the ambiguity fixture.
    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "advance past the interstitial",
        "--step",
        &format!("open {}", fixture.url("/ambiguous")),
        "--step",
        "click Continue",
        "--step",
        "check-errors",
    ]);
    let id = started["id"].as_str().unwrap().to_string();

    let status = h.wait_for(&id, &["needs_decision", "failed", "succeeded"], 60);
    assert_eq!(
        status["state"], "needs_decision",
        "the engine guessed instead of asking: {status:#}"
    );

    let options = status["pending"]["options"]
        .as_array()
        .expect("the park must list the candidates");
    assert_eq!(options.len(), 3, "expected three candidates: {options:#?}");

    // The human-readable form must say how to unblock it.
    let text = h.ok(&["job", "status", &id]);
    assert!(text.contains(&format!("brow job answer {id}")), "{text}");

    // An agent must not be able to satisfy an approval gate...
    let wrong = h.run(&["job", "approve", &id]);
    assert_eq!(wrong.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&wrong.stderr);
    assert!(
        stderr.contains("not waiting for approval"),
        "wrong-gate answer was accepted: {stderr}"
    );

    // ...but answering the decision resumes it.
    let chosen = options
        .iter()
        .position(|o| o.as_str().unwrap_or("").contains("second"))
        .expect("a distinguishable option");
    h.ok(&["job", "answer", &id, &chosen.to_string()]);

    let status = h.wait_for(&id, &["succeeded", "failed"], 60);
    assert_eq!(status["state"], "succeeded", "{status:#}");

    // It clicked the option it was told to, not the first one.
    let clicked = h.json(&["job", "status", &id]);
    let joined = clicked["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["text"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("agent chose"), "{joined}");

    h.ok(&["daemon", "stop"]);
}

#[test]
fn a_decision_is_refused_when_the_selected_target_changes_without_navigation() {
    if !common::chrome_available() {
        return common::skip(
            "a_decision_is_refused_when_the_selected_target_changes_without_navigation",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("decision-target-changed");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "choose the reviewed continuation only",
        "--step",
        &format!("open {}", fixture.url("/ambiguous-mutating")),
        "--step",
        "click Continue",
        "--step",
        "wait 500",
    ]);
    let id = started["id"].as_str().unwrap().to_string();

    let parked = h.wait_for(&id, &["needs_decision", "failed", "succeeded"], 60);
    assert_eq!(parked["state"], "needs_decision", "{parked:#}");
    let options = parked["pending"]["options"]
        .as_array()
        .expect("the decision must expose its reviewed candidates");
    let chosen = options
        .iter()
        .position(|option| {
            option
                .as_str()
                .unwrap_or_default()
                .contains("Continue, second option")
        })
        .expect("the fixture's mutable option");

    fixture.mutate_decision_target();
    fixture.wait_for_decision_target_mutation(std::time::Duration::from_secs(10));
    assert!(!fixture.decision_target_clicked());

    h.ok(&["job", "answer", &id, &chosen.to_string()]);
    let status = h.wait_for(&id, &["failed", "succeeded", "waiting_for_approval"], 60);
    assert_eq!(
        status["state"], "failed",
        "the stale choice was applied to a changed target: {status:#}"
    );
    assert_eq!(
        status["cursor"], 1,
        "the changed choice advanced: {status:#}"
    );
    let error = status["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("decision target changed while waiting")
            && error.contains("No click was sent")
            && error.contains("fresh decision")
            && error.contains("Continue, second option")
            && error.contains("Delete production database"),
        "the decision failure was not actionable: {error}"
    );
    assert!(
        !fixture.decision_target_clicked(),
        "the renamed decision target received a click"
    );

    h.ok(&["daemon", "stop"]);
}

#[test]
fn an_irreversible_action_waits_for_a_human() {
    if !common::chrome_available() {
        return common::skip("an_irreversible_action_waits_for_a_human");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("approve");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "clean up the test workspace",
        "--step",
        &format!("open {}", fixture.url("/danger")),
        "--step",
        // The query deliberately omits "Delete". The gate must inspect the
        // resolved element's real label, not trust a harmless-looking substring.
        "click workspace",
    ]);
    let id = started["id"].as_str().unwrap().to_string();

    let status = h.wait_for(&id, &["waiting_for_approval", "failed", "succeeded"], 60);
    assert_eq!(
        status["state"], "waiting_for_approval",
        "a destructive click was not gated: {status:#}"
    );
    assert_eq!(status["pending"]["reason"], "delete");
    assert!(status["pending"]["action"]
        .as_str()
        .unwrap_or("")
        .contains("Delete workspace"));

    // A human approving later needs to see what the job saw.
    let evidence = status["pending"]["evidence"]
        .as_str()
        .expect("an approval request must carry a screenshot");
    let bytes = std::fs::read(evidence).expect("evidence file");
    assert!(common::png_size(&bytes).is_some());

    // Crucially: nothing has happened to the page yet.
    let marker = std::fs::read_to_string(
        std::path::PathBuf::from(status["artifacts"].as_str().unwrap()).join("job.json"),
    )
    .expect("manifest");
    assert!(
        marker.contains("waiting_for_approval"),
        "the manifest must record the park so a later status call can explain it"
    );

    // An agent cannot answer this with `job answer`.
    let wrong = h.run(&["job", "answer", &id, "0"]);
    assert_eq!(wrong.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&wrong.stderr);
    assert!(stderr.contains("not waiting for a decision"), "{stderr}");

    // Rejecting fails the job rather than skipping the step.
    h.ok(&["job", "approve", &id, "--reject"]);
    let status = h.wait_for(&id, &["failed", "succeeded", "stopped"], 60);
    assert_eq!(status["state"], "failed", "{status:#}");
    assert!(status["error"].as_str().unwrap_or("").contains("rejected"));

    h.ok(&["daemon", "stop"]);
}

#[test]
fn approving_lets_the_destructive_step_through() {
    if !common::chrome_available() {
        return common::skip("approving_lets_the_destructive_step_through");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("approved");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "confirm the delete button works",
        "--step",
        &format!("open {}", fixture.url("/danger")),
        "--step",
        "click \"Delete workspace\"",
        "--step",
        "wait 200",
        "--step",
        "screenshot",
    ]);
    let id = started["id"].as_str().unwrap().to_string();

    h.wait_for(&id, &["waiting_for_approval"], 60);
    h.ok(&["job", "approve", &id]);

    let status = h.wait_for(&id, &["succeeded", "failed"], 60);
    assert_eq!(status["state"], "succeeded", "{status:#}");
    assert_eq!(status["cursor"], 4);

    h.ok(&["daemon", "stop"]);
}

#[test]
fn approval_is_refused_when_the_exact_target_changes_without_navigation() {
    if !common::chrome_available() {
        return common::skip(
            "approval_is_refused_when_the_exact_target_changes_without_navigation",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("approval-target-changed");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "delete the reviewed test workspace only",
        "--step",
        &format!("open {}", fixture.url("/danger-mutating")),
        "--step",
        "click workspace",
        // If a broken implementation clicks through, exercise another step so
        // the wrong path cannot hide behind an immediately terminal job.
        "--step",
        "wait 500",
    ]);
    let id = started["id"].as_str().unwrap().to_string();

    let parked = h.wait_for(&id, &["waiting_for_approval", "failed", "succeeded"], 60);
    assert_eq!(
        parked["state"], "waiting_for_approval",
        "the destructive target was not parked: {parked:#}"
    );
    assert!(parked["pending"]["action"]
        .as_str()
        .unwrap_or_default()
        .contains("Delete workspace"));

    // Mutate the *same node* after the gate is visibly parked. This leaves the
    // document generation and immutable backend identity unchanged while the
    // reviewed accessible label becomes a different destructive action.
    fixture.mutate_danger_target();
    fixture.wait_for_danger_target_mutation(std::time::Duration::from_secs(10));
    assert!(!fixture.danger_target_clicked());

    h.ok(&["job", "approve", &id]);
    let status = h.wait_for(&id, &["failed", "succeeded"], 60);
    assert_eq!(
        status["state"], "failed",
        "approval was applied to a changed target: {status:#}"
    );
    assert_eq!(
        status["cursor"], 1,
        "the changed click step advanced: {status:#}"
    );
    let error = status["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("approval target changed while waiting")
            && error.contains("No click was sent")
            && error.contains("fresh approval")
            && error.contains("Delete workspace")
            && error.contains("Delete production database"),
        "the failure did not explain how to recover safely: {error}"
    );

    // The handler records the destructive side effect synchronously outside the
    // browser, so a failed status cannot hide a click that was already dispatched.
    assert!(
        !fixture.danger_target_clicked(),
        "the renamed destructive target received a click"
    );

    h.ok(&["daemon", "stop"]);
}

#[test]
fn approval_fails_closed_when_evidence_cannot_be_written() {
    if !common::chrome_available() {
        return common::skip("approval_fails_closed_when_evidence_cannot_be_written");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("approval-evidence-failure");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "delete only after reviewable evidence exists",
        "--step",
        &format!("open {}", fixture.url("/danger-evidence-delayed")),
        "--step",
        "click workspace",
    ]);
    let id = started["id"].as_str().unwrap().to_string();
    let artifacts = std::path::PathBuf::from(
        started["artifacts"]
            .as_str()
            .expect("job start must expose its artifact directory"),
    );

    // The page response is blocked until this exact output path has become a
    // directory, forcing the real screenshot writer to fail deterministically.
    let evidence_path = artifacts.join("approval-002.png");
    std::fs::create_dir_all(&evidence_path).expect("block approval evidence output");
    fixture.release_delayed_danger();

    let status = h.wait_for(&id, &["failed", "succeeded", "waiting_for_approval"], 60);
    assert_eq!(
        status["state"], "failed",
        "the job opened an approval gate without evidence: {status:#}"
    );
    assert_eq!(
        status["cursor"], 1,
        "the destructive step advanced: {status:#}"
    );
    assert!(
        status["pending"].is_null(),
        "an unusable gate was left open"
    );
    let error = status["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("mandatory approval evidence")
            && error.contains("No approval was requested")
            && error.contains("no click was sent"),
        "the evidence failure was not actionable: {error}"
    );
    assert!(
        !fixture.danger_target_clicked(),
        "the destructive target was clicked without evidence"
    );

    h.ok(&["daemon", "stop"]);
}

#[test]
fn a_bad_plan_is_rejected_before_a_browser_is_started() {
    let h = Harness::new("badplan");

    let out = h.run(&[
        "job",
        "start",
        "--intent",
        "x",
        "--step",
        "frobnicate everything",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Known steps"), "{stderr}");
    assert!(stderr.contains("--help"), "{stderr}");

    // Nothing was created for a plan that never ran.
    let jobs_dir = h.home.join("jobs");
    let count = std::fs::read_dir(&jobs_dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count, 0, "a rejected plan left a job directory behind");

    h.run(&["daemon", "stop"]);
}

#[test]
fn a_corrupt_manifest_is_reported_instead_of_disappearing() {
    let h = Harness::new("corrupt-manifest");
    let artifacts = h.home.join("jobs").join("job_corrupt_fixture");
    std::fs::create_dir_all(&artifacts).unwrap();
    let manifest = artifacts.join("job.json");
    std::fs::write(&manifest, b"{not valid json").unwrap();

    let listed = h.json(&["job", "list"]);
    let recovered = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == "job_corrupt_fixture")
        .expect("a corrupt job must remain visible");
    assert_eq!(recovered["state"], "failed");
    let status = h.json(&["job", "status", "job_corrupt_fixture"]);
    assert!(status["error"]
        .as_str()
        .unwrap_or_default()
        .contains("corrupt manifest"));
    assert_eq!(
        std::fs::read(&manifest).unwrap(),
        b"{not valid json",
        "diagnostics must not destroy the only recoverable evidence"
    );

    h.ok(&["daemon", "stop"]);
}

#[test]
fn stopping_a_job_ends_it_and_takes_its_browser() {
    if !common::chrome_available() {
        return common::skip("stopping_a_job_ends_it_and_takes_its_browser");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("stop");
    let fixture = common::serve();

    let started = h.json(&[
        "job",
        "start",
        "--intent",
        "a job that will be interrupted",
        "--step",
        &format!("open {}", fixture.url("/")),
        "--step",
        "wait 60000",
        "--step",
        "screenshot",
    ]);
    let id = started["id"].as_str().unwrap().to_string();
    h.wait_for(&id, &["running"], 30);

    let pattern = h.home.join("jobs").display().to_string();
    let browsers = |pattern: &str| {
        Command::new("pgrep")
            .arg("-f")
            .arg(pattern)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0)
    };
    assert!(browsers(&pattern) > 0, "the job did not start a browser");

    h.ok(&["job", "stop", &id]);
    let status = h.wait_for(&id, &["stopped", "failed", "succeeded"], 30);
    assert_eq!(status["state"], "stopped", "{status:#}");
    assert!(
        status["cursor"].as_u64().unwrap_or(99) < 3,
        "a stopped job must not have run its remaining steps"
    );

    // A job owns its browser, so ending the job must end the browser.
    let mut left = browsers(&pattern);
    for _ in 0..50 {
        if left == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        left = browsers(&pattern);
    }
    assert_eq!(left, 0, "a stopped job leaked its browser");

    h.ok(&["daemon", "stop"]);
}
