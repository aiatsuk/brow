//! End-to-end through the real binary: CLI → socket → daemon → Chromium.
//!
//! Lives in its own test file because it sets `BROW_HOME` process-wide and drives
//! a single shared daemon; the assertions here are about the plumbing between the
//! pieces, which the library-level tests deliberately bypass.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// Runs one CLI request on a separate thread so a concurrency assertion can
/// fail on a deadline instead of hanging behind the operation it is testing.
fn spawn_brow(
    home: std::path::PathBuf,
    args: &[&str],
) -> (mpsc::Receiver<Output>, std::thread::JoinHandle<()>) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let output = Command::new(BIN)
            .args(args)
            .env("BROW_HOME", home)
            .output()
            .expect("run brow on worker thread");
        let _ = tx.send(output);
    });
    (rx, thread)
}

fn output_json(label: &str, output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{label} failed with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{label} printed non-JSON: {error}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// An HTTP response whose headers are withheld until the test releases it.
/// Receiving `requested` proves the daemon is already inside Page.navigate.
struct BlockingPage {
    url: String,
    requested: mpsc::Receiver<()>,
    release: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BlockingPage {
    fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind blocking page");
        let address = listener.local_addr().expect("blocking page address");
        listener
            .set_nonblocking(true)
            .expect("make blocking page nonblocking");
        let (requested_tx, requested) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            'accept: loop {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let Ok(read_stream) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(read_stream);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => continue 'accept,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => {}
                    }
                }
                let _ = requested_tx.send(());
                if release_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                    return;
                }

                let body = "<!doctype html><title>released</title><p>released</p>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                return;
            }
        });

        Self {
            url: format!("http://{address}/blocked"),
            requested,
            release: Some(release),
            thread: Some(thread),
        }
    }

    fn wait_until_requested(&self) {
        self.requested
            .recv_timeout(Duration::from_secs(10))
            .expect("the blocking navigation never reached its HTTP server");
    }

    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BlockingPage {
    fn drop(&mut self) {
        self.release();
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
    let _slot = common::browser_slot();
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
    assert!(
        !pids.is_empty(),
        "no Chromium process found for the test profile"
    );

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
    assert_eq!(
        bad.status.code(),
        Some(1),
        "a failed action must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("→"),
        "an error should carry a hint: {stderr}"
    );

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
    assert_eq!(
        sessions.as_array().unwrap().len(),
        1,
        "close must drop just one"
    );

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

/// A request that is demonstrably inside one browser must not become a daemon-
/// wide critical section. This also pins down close ordering: close hides the
/// session immediately, waits for its in-flight command, and never races CDP or
/// lets a second browser reuse the same profile before teardown completes.
#[test]
fn a_slow_session_does_not_block_status_or_another_session() {
    if !common::chrome_available() {
        return common::skip("a_slow_session_does_not_block_status_or_another_session");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("concurrency");
    let fixture = common::serve();

    h.json(&["--session", "a", "open", &fixture.url("/")]);
    h.json(&["--session", "b", "open", &fixture.url("/second")]);

    let mut blocked = BlockingPage::new();
    let (navigation_rx, navigation_thread) = spawn_brow(
        h.home.clone(),
        &["--session", "a", "open", &blocked.url, "--json"],
    );
    blocked.wait_until_requested();

    // The HTTP request is accepted but has no response headers. Page.navigate in
    // session A is therefore known to be awaiting the server at this point.
    let (status_rx, status_thread) = spawn_brow(h.home.clone(), &["daemon", "status", "--json"]);
    let (session_b_rx, session_b_thread) = spawn_brow(
        h.home.clone(),
        &["--session", "b", "eval", "document.title", "--json"],
    );

    let status_output = status_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("daemon status was blocked by session A");
    let session_b_output = session_b_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("an operation in session B was blocked by session A");
    status_thread.join().expect("status worker panicked");
    session_b_thread.join().expect("session B worker panicked");

    let status = output_json("daemon status", &status_output);
    assert_eq!(status["sessions"], 2);
    assert_eq!(
        output_json("session B eval", &session_b_output),
        "second page"
    );

    // Close marks A unavailable without taking A's busy state lock. Its command
    // cannot finish until the server is released, but status and B stay usable.
    let (close_rx, close_thread) =
        spawn_brow(h.home.clone(), &["--session", "a", "close", "--json"]);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = h.json(&["daemon", "status"]);
        if status["sessions"] == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "close never made session A unavailable"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(close_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "close completed while session A still owned an in-flight operation"
    );

    let rejected = h.run(&["--session", "a", "eval", "document.title"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("no session named"),
        "a closing session accepted another operation: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(
        h.json(&["--session", "b", "eval", "document.title"]),
        "second page"
    );

    blocked.release();
    let navigation_output = navigation_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the released navigation did not finish");
    let close_output = close_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("close did not finish after the in-flight operation");
    navigation_thread
        .join()
        .expect("navigation worker panicked");
    close_thread.join().expect("close worker panicked");
    output_json("session A navigation", &navigation_output);
    let closed = output_json("session A close", &close_output);
    assert_eq!(closed["closed"], "a");

    let status = h.json(&["daemon", "status"]);
    assert_eq!(status["sessions"], 1);
    h.ok(&["daemon", "stop"]);
}

/// The CDP pipe is what makes the browser unreachable by any other process. The
/// price is that the browser's lifetime is tied to whoever holds the pipe, and
/// that needs to be a measured fact rather than an assumption.
#[test]
fn a_dead_daemon_takes_its_browsers_with_it_and_recovers_cleanly() {
    if !common::chrome_available() {
        return common::skip("a_dead_daemon_takes_its_browsers_with_it_and_recovers_cleanly");
    }
    let _slot = common::browser_slot();
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
    assert_eq!(
        snap["title"], "second page",
        "the recovered session is not usable"
    );

    h.ok(&["daemon", "stop"]);
}

/// Two daemons racing to start must not leave a socket nobody is listening on.
///
/// The stale-socket recovery path is inherently racy — connect, fail, unlink,
/// bind — and without a lock the second process's unlink can delete the first
/// one's freshly bound socket.
#[test]
fn concurrent_daemon_starts_leave_exactly_one_listener() {
    let h = Harness::new("race");
    // Eight at once, spawned before any of them can finish binding.
    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(BIN)
                .args(["daemon", "start"])
                .env("BROW_HOME", &h.home)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn brow daemon start")
        })
        .collect();
    for mut c in children {
        let _ = c.wait();
    }

    // Whoever won, the socket must exist and answer.
    let status = h.json(&["daemon", "status"]);
    assert!(
        status["pid"].as_u64().unwrap_or(0) > 0,
        "no daemon is reachable after a concurrent start: {status}"
    );

    // And there must be exactly one of them.
    let listeners = Command::new("pgrep")
        .arg("-f")
        .arg(h.home.display().to_string())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);
    assert!(
        listeners <= 1,
        "{listeners} browd processes survived the race"
    );

    h.ok(&["daemon", "stop"]);
}
