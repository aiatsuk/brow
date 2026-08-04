//! End-to-end through the real binary: CLI → socket → daemon → Chromium.
//!
//! Lives in its own test file because it sets `BROW_HOME` process-wide and drives
//! a single shared daemon; the assertions here are about the plumbing between the
//! pieces, which the library-level tests deliberately bypass.

mod common;

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_brow");

struct Harness {
    home: std::path::PathBuf,
}

impl Harness {
    fn new(tag: &str) -> Self {
        // Short path on purpose: a Unix socket path must fit in 104 bytes on
        // macOS, and the system temp dir is already long.
        let home = std::path::PathBuf::from(format!("/tmp/brow-it-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create test BROW_HOME");
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
            "`brow {}` failed with {:?}\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.status.code(),
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

/// PIDs of Chromium processes started against our throwaway profile.
fn chrome_pids(home: &std::path::Path) -> Vec<String> {
    // Match on the profile path only. A pattern starting with `--` is swallowed
    // by pgrep as one of its own options and silently matches nothing.
    let pattern = home.join("profiles").display().to_string();
    let out = Command::new("pgrep").arg("-f").arg(&pattern).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn cli_drives_a_browser_through_the_daemon() {
    if !common::chrome_available() {
        return common::skip("cli_drives_a_browser_through_the_daemon");
    }
    let h = Harness::new("cli");
    let fixture = common::serve();

    // Nothing running yet.
    let status = h.ok(&["daemon", "status"]);
    assert!(status.contains("not running"), "got: {status}");

    // `open` must auto-start the daemon: an agent should never have to know the
    // daemon exists.
    let opened = h.json(&["open", &fixture.url("/")]);
    assert_eq!(opened["session"], "default");
    assert_eq!(opened["reused"], false);
    assert!(
        opened["product"].as_str().unwrap_or("").contains('/'),
        "expected a browser product string, got {:?}",
        opened["product"]
    );

    let status = h.json(&["daemon", "status"]);
    assert_eq!(status["sessions"], 1);
    assert!(status["pid"].as_u64().unwrap() > 0);

    // A browser really is running against our scratch profile.
    let pids = chrome_pids(&h.home);
    assert!(!pids.is_empty(), "no Chromium process found for the test profile");

    // ---- snapshot ---------------------------------------------------------
    let snap = h.json(&["snapshot"]);
    assert_eq!(snap["title"], "brow fixture");
    let nodes = snap["nodes"].as_array().expect("nodes array");
    let go = nodes
        .iter()
        .find(|n| n["name"] == "Create account")
        .expect("the button must be in the snapshot");
    let go_ref = go["ref"].as_str().unwrap().to_string();

    // The default text rendering must stay small enough for an agent to read.
    let text = h.ok(&["snapshot"]);
    assert!(text.contains(&go_ref), "text output should list the button");
    assert!(
        text.len() < 4000,
        "the default snapshot rendered {} bytes; it is meant to be compact",
        text.len()
    );
    assert!(
        text.lines().count() < nodes.len(),
        "the default view must be filtered, not the whole tree"
    );

    // ---- act --------------------------------------------------------------
    h.ok(&["click", &go_ref]);
    let status_text = h.ok(&["eval", "document.querySelector('#status').textContent"]);
    assert_eq!(
        status_text.trim(),
        "clicked:true",
        "the click did not reach the page as a trusted event"
    );

    // ---- screenshot to an explicit path ------------------------------------
    let shot = h.home.join("shot.png");
    let result = h.json(&["screenshot", "--out", shot.to_str().unwrap()]);
    assert_eq!(result["path"].as_str(), shot.to_str());
    let bytes = std::fs::read(&shot).expect("screenshot file");
    let (w, hh) = common::png_size(&bytes).expect("a valid PNG");
    assert!(w > 100 && hh > 100, "screenshot was {w}x{hh}");

    // ---- errors are actionable --------------------------------------------
    let bad = h.run(&["click", "@node-99999"]);
    assert_eq!(bad.status.code(), Some(1), "a failed action must exit non-zero");
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(stderr.contains("→"), "an error should carry a hint: {stderr}");

    let bad = h.run(&["eval", "document.title = 'nope'"]);
    assert_eq!(bad.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("--mutate"),
        "read-only refusal must name the escape hatch: {stderr}"
    );

    let bad = h.run(&["--session", "nonexistent", "snapshot"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("brow open"));

    // ---- sessions ----------------------------------------------------------
    let sessions = h.json(&["sessions"]);
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["session"], "default");

    // A second, independent session gets its own browser and its own profile.
    h.json(&["--session", "qa", "open", &fixture.url("/second")]);
    let sessions = h.json(&["sessions"]);
    assert_eq!(sessions.as_array().unwrap().len(), 2);
    let qa_snap = h.json(&["--session", "qa", "snapshot"]);
    assert_eq!(qa_snap["title"], "second page");
    // ...and the first session is untouched by it.
    let default_snap = h.json(&["snapshot"]);
    assert_eq!(default_snap["title"], "brow fixture");

    // ---- teardown ----------------------------------------------------------
    h.ok(&["close"]);
    let sessions = h.json(&["sessions"]);
    assert_eq!(sessions.as_array().unwrap().len(), 1, "close must drop just one");

    h.ok(&["daemon", "stop"]);
    let status = h.ok(&["daemon", "status"]);
    assert!(status.contains("not running"), "got: {status}");

    // Stopping the daemon must not leave Chromium behind.
    let mut leftover = chrome_pids(&h.home);
    for _ in 0..40 {
        if leftover.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        leftover = chrome_pids(&h.home);
    }
    assert!(
        leftover.is_empty(),
        "daemon shutdown leaked Chromium processes: {leftover:?}"
    );

    // The socket is gone, so the next `brow` will start a clean daemon.
    assert!(!h.home.join("run").join("brow.sock").exists());
}

/// The CDP pipe is what makes the browser unreachable by any other process. The
/// price is that the browser's lifetime is tied to whoever holds the pipe, and
/// that needs to be a measured fact rather than an assumption.
#[test]
fn a_dead_daemon_takes_its_browsers_with_it_and_recovers_cleanly() {
    if !common::chrome_available() {
        return common::skip("a_dead_daemon_takes_its_browsers_with_it_and_recovers_cleanly");
    }
    let h = Harness::new("crash");
    let fixture = common::serve();

    h.json(&["open", &fixture.url("/")]);
    let daemon_pid = h.json(&["daemon", "status"])["pid"].as_u64().expect("pid");
    assert!(
        !chrome_pids(&h.home).is_empty(),
        "no browser was started for the test profile"
    );

    // SIGKILL: no chance to clean up, which is exactly the scenario.
    let killed = Command::new("kill")
        .args(["-9", &daemon_pid.to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());

    // Closing the pipe is Chromium's shutdown signal, so the browsers go too.
    // This is the honest cost of the pipe transport: "persistent" means across
    // CLI invocations, not across a daemon restart.
    let mut leftover = chrome_pids(&h.home);
    for _ in 0..50 {
        if leftover.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        leftover = chrome_pids(&h.home);
    }
    assert!(
        leftover.is_empty(),
        "a crashed daemon orphaned Chromium processes: {leftover:?}"
    );

    // The socket file survives a SIGKILL, so the next start has to recognise it
    // as stale rather than refuse to bind.
    assert!(
        h.home.join("run").join("brow.sock").exists(),
        "expected a stale socket to be left behind"
    );
    let status = h.ok(&["daemon", "status"]);
    assert!(status.contains("not running"), "got: {status}");

    let reopened = h.json(&["open", &fixture.url("/second")]);
    assert_eq!(reopened["reused"], false);
    let snap = h.json(&["snapshot"]);
    assert_eq!(snap["title"], "second page", "the recovered session is not usable");

    h.ok(&["daemon", "stop"]);
}
