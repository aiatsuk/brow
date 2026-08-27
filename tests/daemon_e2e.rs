//! End-to-end through the real binary: CLI → socket → daemon → Chromium.
//!
//! Lives in its own test file because it sets `BROW_HOME` process-wide and drives
//! a single shared daemon; the assertions here are about the plumbing between the
//! pieces, which the library-level tests deliberately bypass.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use brow::ipc::MAX_IPC_FRAME_BYTES;

const BIN: &str = env!("CARGO_BIN_EXE_brow");

struct Harness {
    home: std::path::PathBuf,
}

struct RawClient {
    stream: BufReader<std::os::unix::net::UnixStream>,
    hello: serde_json::Value,
}

impl RawClient {
    fn connect(home: &std::path::Path) -> Self {
        let stream = std::os::unix::net::UnixStream::connect(home.join("run/brow.sock"))
            .expect("connect raw protocol client");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set raw client read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set raw client write timeout");
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).expect("read daemon hello");
        let hello = serde_json::from_str(&line).expect("parse daemon hello");
        Self { stream, hello }
    }

    fn send(&mut self, request: serde_json::Value) {
        let line = serde_json::to_string(&request).expect("serialize raw request");
        self.stream
            .get_mut()
            .write_all(line.as_bytes())
            .expect("write raw request");
        self.stream
            .get_mut()
            .write_all(b"\n")
            .expect("terminate raw request");
        self.stream.get_mut().flush().expect("flush raw request");
    }

    fn response(&mut self) -> serde_json::Value {
        let mut response = String::new();
        self.stream
            .read_line(&mut response)
            .expect("read raw response");
        serde_json::from_str(&response).expect("parse raw response")
    }

    fn request(&mut self, request: serde_json::Value) -> serde_json::Value {
        self.send(request);
        self.response()
    }
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

    fn error_json(&self, args: &[&str]) -> serde_json::Value {
        let mut args = args.to_vec();
        args.push("--json");
        let output = self.run(&args);
        assert!(
            !output.status.success(),
            "`brow {}` unexpectedly succeeded\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "`brow {}` printed a non-JSON error: {error}\nstdout: {}\nstderr: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
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
fn open_reports_the_proven_redirect_destination_for_new_and_reused_sessions() {
    if !common::chrome_available() {
        return common::skip(
            "open_reports_the_proven_redirect_destination_for_new_and_reused_sessions",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("open-final-url");
    let fixture = common::serve();
    let requested = fixture.url("/redirect-start");
    let expected = fixture.url("/redirect-final");

    let created = h.json(&["open", &requested, "--session", "redirect-proof"]);
    assert_eq!(created["reused"], false, "{created}");
    assert_eq!(created["requested_url"], requested, "{created}");
    assert_eq!(created["url"], expected, "{created}");

    let reused = h.json(&["open", &requested, "--session", "redirect-proof"]);
    assert_eq!(reused["reused"], true, "{reused}");
    assert_eq!(reused["requested_url"], requested, "{reused}");
    assert_eq!(reused["url"], expected, "{reused}");
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

fn assert_full_error_receipt(receipt: &serde_json::Value) {
    let object = receipt.as_object().expect("receipt object");
    for key in [
        "operation",
        "dispatched",
        "dispatch_state",
        "navigation_trigger",
        "requested_wait",
        "effective_wait",
        "outcome",
        "navigation",
        "navigation_scope",
        "redirect_count",
        "before_url",
        "final_url",
        "final_url_observed",
        "before_generation",
        "final_generation",
        "elapsed_ms",
        "discovery_ms",
        "timeout_ms",
        "quiet_ms",
        "active_finite_requests",
        "excluded_long_lived_requests",
        "root_loading",
        "target_settled",
        "event_complete",
        "event_gap_delta",
        "history_entry_id",
        "history_from_index",
        "history_to_index",
        "reload_loader_id",
        "dialog_type",
        "dialog_message",
        "stability_note",
        "guidance",
        "blockers",
        "observed_conditions",
    ] {
        assert!(
            object.contains_key(key),
            "error receipt omitted {key}: {receipt}"
        );
    }
    for key in [
        "operation",
        "dispatch_state",
        "navigation_trigger",
        "requested_wait",
        "effective_wait",
        "outcome",
        "navigation",
        "navigation_scope",
        "before_url",
        "final_url",
    ] {
        assert!(
            receipt[key].is_string(),
            "{key} must be a string: {receipt}"
        );
    }
    assert!(
        receipt["dispatched"].is_boolean() || receipt["dispatched"].is_null(),
        "dispatched must be boolean or null: {receipt}"
    );
    for key in [
        "redirect_count",
        "before_generation",
        "final_generation",
        "elapsed_ms",
        "discovery_ms",
        "timeout_ms",
        "quiet_ms",
        "active_finite_requests",
        "excluded_long_lived_requests",
        "event_gap_delta",
    ] {
        assert!(receipt[key].is_u64(), "{key} must be unsigned: {receipt}");
    }
    for key in [
        "final_url_observed",
        "root_loading",
        "target_settled",
        "event_complete",
    ] {
        assert!(
            receipt[key].is_boolean(),
            "{key} must be boolean: {receipt}"
        );
    }
    for key in ["guidance", "blockers"] {
        assert!(receipt[key].is_array(), "{key} must be an array: {receipt}");
    }
    assert!(
        receipt["observed_conditions"].is_object(),
        "observed_conditions must be an object: {receipt}"
    );
}

#[test]
fn mixed_v1_daemon_fails_loudly_without_requests_or_autostart() {
    let h = Harness::new("mixed-v1");
    let run = h.home.join("run");
    std::fs::create_dir_all(&run).expect("create fake daemon run directory");
    let socket = run.join("brow.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind fake v1 socket");
    listener
        .set_nonblocking(true)
        .expect("make fake v1 listener nonblocking");

    let (stop_tx, stop_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let server = std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set fake daemon read timeout");
                stream
                    .write_all(b"{\"brow\":\"0.0.9\",\"protocol\":1,\"pid\":424242}\n")
                    .expect("write fake v1 hello");
                stream.flush().expect("flush fake v1 hello");

                let mut request = [0_u8; 4096];
                let bytes = match stream.read(&mut request) {
                    Ok(0) => Vec::new(),
                    Ok(length) => request[..length].to_vec(),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        Vec::new()
                    }
                    Err(error) => panic!("read fake daemon request: {error}"),
                };
                observed_tx
                    .send(bytes)
                    .expect("report fake daemon connection");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept fake daemon connection: {error}"),
        }
    });

    let commands = [
        vec!["daemon", "status"],
        vec!["daemon", "start"],
        vec!["daemon", "stop"],
        vec!["daemon", "restart"],
        vec!["snapshot"],
    ];
    for args in commands {
        let started = Instant::now();
        let output = h.run(&args);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "`brow {}` did not surface the v1 handshake promptly",
            args.join(" ")
        );
        assert!(
            !output.status.success(),
            "`brow {}` accepted a v1 daemon\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("version mismatch"), "stderr: {stderr}");
        assert!(
            stderr.contains("matching brow v0.0.9 CLI"),
            "stderr: {stderr}"
        );
        assert!(stderr.contains("verify PID 424242"), "stderr: {stderr}");
        assert!(stderr.contains("daemon start"), "stderr: {stderr}");
        assert!(!stderr.contains("daemon restart"), "stderr: {stderr}");
        assert!(!stderr.contains("not running"), "stderr: {stderr}");

        let request = observed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("fake v1 daemon did not observe the CLI connection");
        assert!(
            request.is_empty(),
            "`brow {}` wrote request bytes after the incompatible hello: {:?}",
            args.join(" "),
            String::from_utf8_lossy(&request)
        );
    }

    assert!(
        matches!(
            observed_rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "the current CLI made an extra connection after the incompatible hello"
    );
    assert!(
        !h.home.join("logs/browd.log").exists(),
        "the current CLI tried to start a second daemon"
    );

    stop_tx.send(()).expect("stop fake v1 daemon");
    server.join().expect("fake v1 daemon panicked");
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
fn raw_protocol_rejects_invalid_wait_semantics_before_dispatch() {
    let h = Harness::new("raw-validation");
    h.ok(&["daemon", "start"]);
    let mut raw = RawClient::connect(&h.home);
    assert_eq!(raw.hello["protocol"], 2);

    let invalid = [
        serde_json::json!({
            "op": "wait",
            "session": "missing",
            "conditions": {},
            "timeout_ms": 100,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "wait",
            "session": "missing",
            "conditions": {"stable": true},
            "timeout_ms": 0,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "wait",
            "session": "missing",
            "conditions": {"stable": true},
            "timeout_ms": 120001,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "wait",
            "session": "missing",
            "conditions": {"stable": true},
            "timeout_ms": 1000,
            "quiet_ms": 1001
        }),
        serde_json::json!({
            "op": "wait",
            "session": "missing",
            "conditions": {"url": ""},
            "timeout_ms": 1000,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "back",
            "session": "missing",
            "wait": "auto",
            "timeout_ms": 1000
        }),
        serde_json::json!({
            "op": "forward",
            "session": "missing",
            "wait": "auto",
            "timeout_ms": 1000
        }),
        serde_json::json!({
            "op": "reload",
            "session": "missing",
            "ignore_cache": false,
            "wait": "auto",
            "timeout_ms": 1000
        }),
        serde_json::json!({
            "op": "checkpoint",
            "session": "missing",
            "name": "invalid",
            "wait": "auto",
            "timeout_ms": 1000,
            "quiet_ms": 10
        }),
    ];

    for request in invalid {
        let response = raw.request(request.clone());
        assert_eq!(response["status"], "error", "request: {request}");
        assert_eq!(
            response["data"]["code"], "invalid_request",
            "request bypassed semantic admission validation: {request}\nresponse: {response}"
        );
        assert_eq!(response["data"]["dispatched"], false);
        assert!(response["message"]
            .as_str()
            .is_some_and(|message| message.contains("invalid request")));
    }

    let started = Instant::now();
    let status = raw.request(serde_json::json!({"op": "status"}));
    assert_eq!(status["status"], "ok");
    assert_eq!(status["data"]["sessions"], 0);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an invalid raw request left daemon admission blocked"
    );
}

#[test]
fn raw_ipc_frame_limit_accepts_exact_boundary_rejects_next_byte_and_survives() {
    let h = Harness::new("ipc-frame-boundary");
    h.ok(&["daemon", "start"]);

    let prefix = b"{\"op\":\"eval\",\"session\":\"missing\",\"expression\":\"";
    let suffix = b"\",\"mutate\":false}";
    let padding = MAX_IPC_FRAME_BYTES
        .checked_sub(prefix.len() + suffix.len())
        .expect("IPC limit fits the fixed request envelope");
    let mut exact = Vec::with_capacity(MAX_IPC_FRAME_BYTES + 1);
    exact.extend_from_slice(prefix);
    exact.extend(std::iter::repeat_n(b'a', padding));
    exact.extend_from_slice(suffix);
    assert_eq!(exact.len(), MAX_IPC_FRAME_BYTES);
    exact.push(b'\n');

    let mut boundary = RawClient::connect(&h.home);
    boundary
        .stream
        .get_mut()
        .write_all(&exact)
        .expect("write exact-limit request");
    boundary.stream.get_mut().flush().unwrap();
    let response = boundary.response();
    assert_eq!(response["status"], "error", "{response}");
    assert!(response["message"]
        .as_str()
        .is_some_and(|message| message.contains("no session named")));

    let mut oversized = RawClient::connect(&h.home);
    let mut frame = vec![b'a'; MAX_IPC_FRAME_BYTES + 1];
    frame.push(b'\n');
    oversized
        .stream
        .get_mut()
        .write_all(&frame)
        .expect("write one-byte-oversized request");
    oversized.stream.get_mut().flush().unwrap();
    let response = oversized.response();
    assert_eq!(response["status"], "error", "{response}");
    assert_eq!(response["data"]["code"], "ipc_frame_too_large");
    assert_eq!(
        response["data"]["max_frame_bytes"],
        MAX_IPC_FRAME_BYTES as u64
    );
    let mut after_error = String::new();
    assert_eq!(
        oversized.stream.read_line(&mut after_error).unwrap(),
        0,
        "oversized peer was not closed after its protocol error"
    );

    let status = h.json(&["daemon", "status"]);
    assert!(
        status["pid"].as_u64().is_some(),
        "daemon stopped responding"
    );
}

#[test]
fn raw_connection_supports_sequential_stop_and_wait_requests() {
    let h = Harness::new("ipc-sequential");
    h.ok(&["daemon", "start"]);
    let mut raw = RawClient::connect(&h.home);

    for _ in 0..2 {
        let ping = raw.request(serde_json::json!({"op": "ping"}));
        assert_eq!(ping["status"], "ok", "{ping}");
        assert_eq!(ping["data"]["pong"], true, "{ping}");
    }
    let status = raw.request(serde_json::json!({"op": "status"}));
    assert_eq!(status["status"], "ok", "{status}");
}

#[test]
fn pipelined_frame_during_typed_wait_is_rejected_without_dispatching_the_second() {
    if !common::chrome_available() {
        return common::skip(
            "pipelined_frame_during_typed_wait_is_rejected_without_dispatching_the_second",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("ipc-pipelined-wait");
    let fixture = common::serve();
    h.json(&["open", &fixture.url("/second"), "--session", "pipelined"]);

    let wait = serde_json::json!({
        "op": "wait",
        "session": "pipelined",
        "conditions": {"url": "*will-never-match*"},
        "timeout_ms": 120_000,
        "quiet_ms": 0
    });
    let mut frames = format!("{}\n", serde_json::to_string(&wait).unwrap()).into_bytes();
    frames.extend_from_slice(b"{\"op\":\"ping\"}\n");

    let mut raw = RawClient::connect(&h.home);
    raw.stream
        .get_mut()
        .write_all(&frames)
        .expect("write two pipelined frames");
    raw.stream.get_mut().flush().unwrap();

    let response = raw.response();
    assert_eq!(response["status"], "error", "{response}");
    assert_eq!(
        response["data"]["code"], "ipc_pipelining_not_supported",
        "the second frame was accepted instead of rejected: {response}"
    );
    let mut after_error = String::new();
    assert_eq!(raw.stream.read_line(&mut after_error).unwrap(), 0);

    let title = h.json(&["eval", "document.title", "--session", "pipelined"]);
    assert_eq!(title, "second page");
}

#[test]
fn loaded_session_rejects_raw_wait_history_and_reload_before_side_effects() {
    if !common::chrome_available() {
        return common::skip(
            "loaded_session_rejects_raw_wait_history_and_reload_before_side_effects",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("raw-loaded-validation");
    let fixture = common::serve();

    h.json(&["open", &fixture.url("/second")]);
    h.json(&["open", &fixture.url("/navigation")]);
    let before = h.json(&["snapshot"]);
    let before_url = before["url"].as_str().expect("baseline URL").to_string();
    let before_generation = before["generation"].as_u64().expect("baseline generation");
    let history_before = h.json(&["eval", "({length: history.length, href: location.href})"]);
    let navigation_requests_before = fixture.navigation_request_count();

    let mut invalid_client = RawClient::connect(&h.home);
    let mut proving_client = RawClient::connect(&h.home);
    assert_eq!(invalid_client.hello["protocol"], 2);
    assert_eq!(proving_client.hello["protocol"], 2);
    let invalid = [
        serde_json::json!({
            "op": "wait",
            "session": "default",
            "conditions": {},
            "timeout_ms": 100,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "wait",
            "session": "default",
            "conditions": {"stable": true},
            "timeout_ms": 0,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "wait",
            "session": "default",
            "conditions": {"stable": true},
            "timeout_ms": 120001,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "wait",
            "session": "default",
            "conditions": {"stable": true},
            "timeout_ms": 1000,
            "quiet_ms": 1001
        }),
        serde_json::json!({
            "op": "wait",
            "session": "default",
            "conditions": {"url": ""},
            "timeout_ms": 1000,
            "quiet_ms": 0
        }),
        serde_json::json!({
            "op": "back",
            "session": "default",
            "wait": "auto",
            "timeout_ms": 1000
        }),
        serde_json::json!({
            "op": "reload",
            "session": "default",
            "ignore_cache": true,
            "wait": "auto",
            "timeout_ms": 1000
        }),
    ];

    for iteration in 0..10 {
        for request in &invalid {
            let response = invalid_client.request(request.clone());
            assert_eq!(response["status"], "error", "iteration {iteration}");
            assert_eq!(
                response["data"]["code"], "invalid_request",
                "invalid request reached the loaded session on iteration {iteration}: {request}\n{response}"
            );
            assert_eq!(response["data"]["dispatched"], false);
        }

        let status_started = Instant::now();
        let status = proving_client.request(serde_json::json!({"op": "status"}));
        assert_eq!(status["status"], "ok");
        assert!(
            status_started.elapsed() < Duration::from_millis(500),
            "invalid request pinned daemon admission on iteration {iteration}"
        );

        let wait_started = Instant::now();
        let valid_wait = proving_client.request(serde_json::json!({
            "op": "wait",
            "session": "default",
            "conditions": {"url": before_url.clone()},
            "timeout_ms": 100,
            "quiet_ms": 0
        }));
        assert_eq!(
            valid_wait["status"], "ok",
            "valid wait was blocked after invalid requests on iteration {iteration}: {valid_wait}"
        );
        assert!(
            wait_started.elapsed() < Duration::from_millis(500),
            "valid wait stayed blocked on iteration {iteration}"
        );
    }

    let after = h.json(&["snapshot"]);
    assert_eq!(after["url"], before_url);
    assert_eq!(after["generation"], before_generation);
    assert_eq!(
        h.json(&["eval", "({length: history.length, href: location.href})",]),
        history_before,
        "an invalid back request changed history"
    );
    assert_eq!(
        fixture.navigation_request_count(),
        navigation_requests_before,
        "an invalid reload request reached the fixture"
    );
}

#[test]
fn pre_dispatch_errors_return_complete_honest_action_receipts() {
    if !common::chrome_available() {
        return common::skip("pre_dispatch_errors_return_complete_honest_action_receipts");
    }
    // This scenario opens several daemon-owned sessions in one test. Reserve
    // the whole cross-binary pool so its browsers do not stack on top of four
    // unrelated Chrome processes and turn the 1 s side-effect oracle flaky.
    let _slots = common::exclusive_browser_slots();
    let h = Harness::new("error-receipts");
    let fixture = common::serve();

    h.json(&["open", &fixture.url("/")]);
    let snapshot = h.json(&["snapshot"]);
    let stale_ref = snapshot["nodes"]
        .as_array()
        .expect("snapshot nodes")
        .iter()
        .find(|node| node["name"] == "Create account")
        .and_then(|node| node["ref"].as_str())
        .expect("fixture action ref")
        .to_string();
    h.json(&["open", &fixture.url("/second")]);
    let current = h.json(&["snapshot"]);

    let stale = h.error_json(&[
        "click",
        &stale_ref,
        "--wait",
        "stable",
        "--timeout-ms",
        "1000",
    ]);
    let receipt = &stale["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(receipt["operation"], "click");
    assert_eq!(receipt["dispatched"], false);
    assert_eq!(receipt["dispatch_state"], "prevented");
    assert_eq!(receipt["navigation_trigger"], "input");
    assert_eq!(receipt["outcome"], "not_waited");
    assert_eq!(receipt["requested_wait"], "stable");
    assert_eq!(receipt["before_url"], current["url"]);
    assert_eq!(receipt["final_url"], current["url"]);
    assert_eq!(receipt["before_generation"], current["generation"]);
    assert_eq!(receipt["final_generation"], current["generation"]);
    assert!(receipt["root_loading"].is_boolean());
    assert!(receipt["target_settled"].is_boolean());
    assert!(stale["hint"]
        .as_str()
        .is_some_and(|hint| hint.contains("fresh `brow snapshot`")));

    h.json(&["open", &fixture.url("/")]);
    let current = h.json(&["snapshot"]);
    let current_ref = current["nodes"]
        .as_array()
        .expect("snapshot nodes")
        .iter()
        .find(|node| node["name"] == "Create account")
        .and_then(|node| node["ref"].as_str())
        .expect("current action ref");
    assert_eq!(
        h.ok(&["eval", "document.querySelector('#status').textContent"])
            .trim(),
        "idle"
    );
    let mut raw = RawClient::connect(&h.home);
    let invalid_button = raw.request(serde_json::json!({
        "op": "click",
        "session": "default",
        "target": {"kind": "ref", "node_ref": current_ref},
        "button": "banana",
        "count": 1,
        "modifiers": 0,
        "force": false,
        "wait": "none",
        "timeout_ms": 1000
    }));
    assert_eq!(invalid_button["status"], "error");
    assert_eq!(invalid_button["data"]["code"], "invalid_request");
    let receipt = &invalid_button["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(receipt["dispatched"], false);
    assert_eq!(receipt["dispatch_state"], "prevented");
    assert_eq!(receipt["before_url"], current["url"]);
    assert_eq!(receipt["final_url"], current["url"]);
    assert_eq!(receipt["before_generation"], current["generation"]);
    assert_eq!(receipt["final_generation"], current["generation"]);
    assert_eq!(
        h.ok(&["eval", "document.querySelector('#status').textContent"])
            .trim(),
        "idle",
        "an invalid raw button must not silently become a left click"
    );

    h.json(&["open", "about:blank", "--session", "empty-history"]);
    assert_eq!(raw.hello["protocol"], 2);
    let empty = raw.request(serde_json::json!({
        "op": "back",
        "session": "empty-history",
        "wait": "load",
        "timeout_ms": 1000
    }));
    assert_eq!(empty["status"], "error");
    let receipt = &empty["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(empty["data"]["code"], "no_history_entry");
    assert_eq!(receipt["operation"], "back");
    assert_eq!(receipt["dispatched"], false);
    assert_eq!(receipt["dispatch_state"], "prevented");
    assert_eq!(receipt["navigation_trigger"], "history");
    assert_eq!(receipt["outcome"], "not_waited");
    assert_eq!(receipt["requested_wait"], "load");
    assert_eq!(receipt["before_url"], "about:blank");
    assert_eq!(receipt["final_url"], "about:blank");
    assert!(receipt["root_loading"].is_boolean());
    assert!(receipt["target_settled"].is_boolean());

    let current = h.json(&["snapshot"]);
    let invalid_chord = h.error_json(&[
        "press",
        "DefinitelyNotAKey",
        "--wait",
        "stable",
        "--timeout-ms",
        "1000",
    ]);
    let receipt = &invalid_chord["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(receipt["operation"], "press");
    assert_eq!(receipt["dispatched"], false);
    assert_eq!(receipt["dispatch_state"], "prevented");
    assert_eq!(receipt["outcome"], "not_waited");
    assert_eq!(receipt["before_url"], current["url"]);
    assert_eq!(receipt["final_url"], current["url"]);
    assert_eq!(receipt["before_generation"], current["generation"]);
    assert_eq!(receipt["final_generation"], current["generation"]);
    let invalid_chord_text = h.run(&[
        "press",
        "DefinitelyNotAKey",
        "--wait",
        "stable",
        "--timeout-ms",
        "1000",
    ]);
    assert_eq!(invalid_chord_text.status.code(), Some(1));
    let invalid_chord_stderr = String::from_utf8_lossy(&invalid_chord_text.stderr);
    assert!(invalid_chord_stderr.contains("not dispatched"));
    assert!(invalid_chord_stderr.lines().count() <= 3);

    h.json(&["open", &fixture.url("/navigation")]);
    let snapshot = h.json(&["snapshot"]);
    let submit = snapshot["nodes"]
        .as_array()
        .expect("navigation nodes")
        .iter()
        .find(|node| node["name"] == "Submit once")
        .and_then(|node| node["ref"].as_str())
        .expect("submit ref");
    let timed_out = h.error_json(&["click", submit, "--wait", "stable", "--timeout-ms", "300"]);
    let receipt = &timed_out["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(receipt["operation"], "click");
    assert_eq!(receipt["dispatched"], true);
    assert_eq!(receipt["dispatch_state"], "sent");
    assert_eq!(receipt["outcome"], "timed_out");
    assert!(timed_out["hint"]
        .as_str()
        .is_some_and(|hint| hint.contains("may already have happened")));
    let deadline = Instant::now() + Duration::from_secs(1);
    while fixture.side_effect_count() < 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fixture.side_effect_count(), 1);

    h.json(&[
        "open",
        &fixture.url("/navigation"),
        "--session",
        "text-timeout",
    ]);
    let text_snapshot = h.json(&["snapshot", "--session", "text-timeout"]);
    let text_submit = text_snapshot["nodes"]
        .as_array()
        .expect("text timeout nodes")
        .iter()
        .find(|node| node["name"] == "Submit once")
        .and_then(|node| node["ref"].as_str())
        .expect("text submit ref");
    let timed_out_text = h.run(&[
        "click",
        text_submit,
        "--wait",
        "stable",
        "--timeout-ms",
        "300",
        "--session",
        "text-timeout",
    ]);
    assert_eq!(timed_out_text.status.code(), Some(1));
    let timed_out_stderr = String::from_utf8_lossy(&timed_out_text.stderr);
    assert!(timed_out_stderr.contains("may already have happened"));
    assert!(timed_out_stderr.lines().count() <= 3);
    let deadline = Instant::now() + Duration::from_secs(1);
    while fixture.side_effect_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fixture.side_effect_count(),
        2,
        "each independent timeout fixture must receive exactly one click"
    );

    h.json(&[
        "open",
        &fixture.url("/navigation"),
        "--session",
        "dialog-json",
    ]);
    let dialog_snapshot = h.json(&["snapshot", "--session", "dialog-json"]);
    let protect = dialog_snapshot["nodes"]
        .as_array()
        .expect("dialog nodes")
        .iter()
        .find(|node| node["name"] == "Protect navigation")
        .and_then(|node| node["ref"].as_str())
        .expect("protect ref");
    h.json(&["click", protect, "--session", "dialog-json"]);
    let protected_snapshot = h.json(&["snapshot", "--session", "dialog-json"]);
    let leave = protected_snapshot["nodes"]
        .as_array()
        .expect("protected nodes")
        .iter()
        .find(|node| node["name"] == "Slow cross-document")
        .and_then(|node| node["ref"].as_str())
        .expect("leave ref");
    let dialog_error = h.error_json(&[
        "click",
        leave,
        "--wait",
        "stable",
        "--timeout-ms",
        "3000",
        "--session",
        "dialog-json",
    ]);
    let receipt = &dialog_error["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(receipt["dispatched"], true);
    assert_eq!(receipt["dispatch_state"], "sent");
    assert_eq!(receipt["outcome"], "dialog_blocked");
    assert_eq!(receipt["dialog_type"], "beforeunload");
    assert_eq!(receipt["before_url"], receipt["final_url"]);
    assert_eq!(receipt["before_generation"], receipt["final_generation"]);
    assert!(dialog_error["hint"]
        .as_str()
        .is_some_and(|hint| hint.contains("may already have happened")));

    h.json(&[
        "open",
        &fixture.url("/navigation"),
        "--session",
        "dialog-text",
    ]);
    let dialog_text_snapshot = h.json(&["snapshot", "--session", "dialog-text"]);
    let protect_text = dialog_text_snapshot["nodes"]
        .as_array()
        .expect("dialog text nodes")
        .iter()
        .find(|node| node["name"] == "Protect navigation")
        .and_then(|node| node["ref"].as_str())
        .expect("text protect ref");
    h.json(&["click", protect_text, "--session", "dialog-text"]);
    let protected_text_snapshot = h.json(&["snapshot", "--session", "dialog-text"]);
    let leave_text = protected_text_snapshot["nodes"]
        .as_array()
        .expect("protected text nodes")
        .iter()
        .find(|node| node["name"] == "Slow cross-document")
        .and_then(|node| node["ref"].as_str())
        .expect("text leave ref");
    let dialog_text = h.run(&[
        "click",
        leave_text,
        "--wait",
        "stable",
        "--timeout-ms",
        "3000",
        "--session",
        "dialog-text",
    ]);
    assert_eq!(dialog_text.status.code(), Some(1));
    let dialog_stderr = String::from_utf8_lossy(&dialog_text.stderr);
    assert!(dialog_stderr.contains("may already have happened"));
    assert!(dialog_stderr.lines().count() <= 3);
}

#[test]
fn a_failure_after_the_first_click_reports_sent_and_does_not_invite_replay() {
    if !common::chrome_available() {
        return common::skip(
            "a_failure_after_the_first_click_reports_sent_and_does_not_invite_replay",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("partial-double-click");
    let fixture = common::serve();
    h.json(&["open", &fixture.url("/")]);
    h.json(&[
        "eval",
        "(() => { window.__receiptClicks = 0; const b = document.createElement('button'); b.textContent = 'Remove once'; b.onclick = () => { window.__receiptClicks += 1; b.remove(); }; document.body.appendChild(b); return true; })()",
        "--mutate",
    ]);
    let snapshot = h.json(&["snapshot"]);
    let node_ref = snapshot["nodes"]
        .as_array()
        .expect("snapshot nodes")
        .iter()
        .find(|node| node["name"] == "Remove once")
        .and_then(|node| node["ref"].as_str())
        .expect("self-removing button ref");

    let error = h.error_json(&[
        "click",
        node_ref,
        "--count",
        "2",
        "--force",
        "--wait",
        "none",
        "--timeout-ms",
        "1000",
    ]);
    let receipt = &error["data"]["receipt"];
    assert_full_error_receipt(receipt);
    assert_eq!(receipt["dispatched"], true, "{error}");
    assert_eq!(receipt["dispatch_state"], "sent", "{error}");
    assert_eq!(receipt["outcome"], "incomplete");
    assert!(error["hint"]
        .as_str()
        .is_some_and(|hint| hint.contains("may already have happened")));
    assert_eq!(
        h.ok(&["eval", "window.__receiptClicks"]).trim(),
        "1",
        "the first trusted click happened exactly once"
    );
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

#[test]
fn cli_navigation_receipts_history_wait_pointer_and_checkpoint_work_end_to_end() {
    if !common::chrome_available() {
        return common::skip(
            "cli_navigation_receipts_history_wait_pointer_and_checkpoint_work_end_to_end",
        );
    }
    let _slot = common::browser_slot();
    let h = Harness::new("flow");
    let fixture = common::serve();
    let typed_wait_secret = "typed-wait-secret";

    h.json(&[
        "open",
        &fixture.url(&format!("/navigation?access_token={typed_wait_secret}")),
    ]);
    let snapshot = h.json(&["snapshot"]);
    let baseline = snapshot["generation"].as_u64().unwrap();
    let spa = snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "Delayed SPA")
        .and_then(|node| node["ref"].as_str())
        .expect("SPA ref")
        .to_string();

    let click = h.json(&["click", &spa, "--wait", "auto", "--timeout-ms", "3000"]);
    assert_eq!(click["receipt"]["dispatched"], true);
    assert_eq!(click["receipt"]["outcome"], "committed");
    assert_eq!(click["receipt"]["navigation"], "same_document");
    assert!(click["receipt"]["final_url"]
        .as_str()
        .unwrap()
        .ends_with("#settled"));

    let baseline_arg = baseline.to_string();
    let wait = h.json(&[
        "wait",
        "--url",
        &format!("*access_token={typed_wait_secret}*#settled"),
        "--generation-after",
        &baseline_arg,
        "--load",
        "--timeout-ms",
        "3000",
    ]);
    assert_eq!(
        wait["receipt"]["observed_conditions"]["generation_after"]["matched"],
        true
    );
    let wait_receipt = serde_json::to_string(&wait["receipt"]).unwrap();
    assert!(
        !wait_receipt.contains(typed_wait_secret),
        "typed wait evidence leaked its URL credential: {wait_receipt}"
    );
    assert!(wait_receipt.contains("redacted"), "{wait_receipt}");

    let back = h.json(&["back", "--timeout-ms", "3000"]);
    assert_eq!(back["receipt"]["navigation_trigger"], "history");
    assert!(back["receipt"]["final_url"]
        .as_str()
        .unwrap()
        .contains("/navigation?access_token=[redacted]"));
    let forward = h.json(&["forward", "--timeout-ms", "3000"]);
    assert_eq!(forward["receipt"]["navigation_trigger"], "history");
    assert!(forward["receipt"]["final_url"]
        .as_str()
        .unwrap()
        .ends_with("#settled"));
    let reload = h.json(&["reload", "--ignore-cache", "--timeout-ms", "3000"]);
    assert_eq!(reload["receipt"]["navigation"], "cross_document");
    assert_eq!(reload["receipt"]["navigation_trigger"], "reload");

    let redirect_url = fixture.url("/redirect-start");
    let redirect_script = format!(
        "(() => {{ const b = document.createElement('button'); b.textContent = 'Redirect now'; b.onclick = () => {{ location.href = {}; }}; document.body.appendChild(b); return true; }})()",
        serde_json::to_string(&redirect_url).unwrap()
    );
    h.json(&["eval", &redirect_script, "--mutate"]);
    let snapshot = h.json(&["snapshot"]);
    let redirect_ref = snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "Redirect now")
        .and_then(|node| node["ref"].as_str())
        .expect("redirect button ref");
    let redirected = h.json(&[
        "click",
        redirect_ref,
        "--wait",
        "load",
        "--timeout-ms",
        "3000",
    ]);
    assert_eq!(redirected["receipt"]["navigation_trigger"], "input");
    assert_eq!(redirected["receipt"]["navigation"], "cross_document");
    assert!(
        redirected["receipt"]["redirect_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "at least one completed redirect hop must survive in the receipt: {redirected}"
    );
    assert!(
        redirected["receipt"]["final_url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/redirect-final")),
        "redirect receipt did not report the final URL: {redirected:#}"
    );

    let parked = h.json(&["pointer", "park"]);
    assert_eq!(parked["receipt"]["dispatched"], true);

    let output = h.home.join("explicit-checkpoints");
    let output_arg = output.to_string_lossy().into_owned();
    let checkpoint = h.json(&[
        "checkpoint",
        "--name",
        "cli-flow",
        "--quiet-ms",
        "100",
        "--timeout-ms",
        "3000",
        "-o",
        &output_arg,
    ]);
    assert_eq!(checkpoint["complete"], true);
    let path = std::path::PathBuf::from(checkpoint["path"].as_str().unwrap());
    for file in [
        "manifest.json",
        "snapshot.json",
        "screenshot.png",
        "console-errors.json",
        "network-failures.json",
    ] {
        assert!(path.join(file).is_file(), "missing {file}");
    }
}

#[test]
fn a_long_typed_wait_blocks_only_its_own_session() {
    if !common::chrome_available() {
        return common::skip("a_long_typed_wait_blocks_only_its_own_session");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("typed-wait-concurrency");
    let fixture = common::serve();
    h.json(&["open", &fixture.url("/navigation"), "--session", "waiting"]);
    h.json(&["open", &fixture.url("/second"), "--session", "fast"]);

    let (rx, thread) = spawn_brow(
        h.home.clone(),
        &[
            "wait",
            "--url",
            "*will-never-match*",
            "--timeout-ms",
            "1200",
            "--session",
            "waiting",
            "--json",
        ],
    );
    std::thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    let status = h.json(&["status"]);
    assert_eq!(status["sessions"], 2);
    let fast = h.json(&["snapshot", "--session", "fast"]);
    assert_eq!(fast["title"], "second page");
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "another session/status was serialized behind the long wait"
    );

    let output = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("typed wait did not finish");
    thread.join().unwrap();
    assert!(
        !output.status.success(),
        "impossible wait unexpectedly passed"
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["data"]["receipt"]["outcome"], "timed_out");
    assert!(error["hint"]
        .as_str()
        .is_some_and(|hint| hint.contains("observation-only")));
    assert!(!error["hint"]
        .as_str()
        .is_some_and(|hint| hint.contains("action may")));
}

#[test]
fn disconnecting_a_long_typed_wait_releases_its_session_immediately() {
    if !common::chrome_available() {
        return common::skip("disconnecting_a_long_typed_wait_releases_its_session_immediately");
    }
    let _slot = common::browser_slot();
    let h = Harness::new("typed-wait-disconnect");
    let fixture = common::serve();
    let start = Command::new(BIN)
        .args(["daemon", "start"])
        .env("BROW_HOME", &h.home)
        .env("BROW_LOG", "debug")
        .output()
        .expect("start debug-logged daemon");
    assert!(
        start.status.success(),
        "daemon start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    h.json(&[
        "open",
        &fixture.url("/second"),
        "--session",
        "cancelled-wait",
    ]);

    let mut abandoned = RawClient::connect(&h.home);
    abandoned.send(serde_json::json!({
        "op": "wait",
        "session": "cancelled-wait",
        "conditions": {"url": "*will-never-match*"},
        "timeout_ms": 120_000,
        "quiet_ms": 0
    }));
    let accepted_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let log = std::fs::read_to_string(h.home.join("logs/browd.log")).unwrap_or_default();
        if log.contains("typed wait acquired session") && log.contains("cancelled-wait") {
            break;
        }
        assert!(
            Instant::now() < accepted_deadline,
            "daemon never confirmed that the typed wait owned its SessionGuard\n{log}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let (follow_up, thread) = spawn_brow(
        h.home.clone(),
        &[
            "eval",
            "document.title",
            "--session",
            "cancelled-wait",
            "--json",
        ],
    );
    assert!(
        matches!(
            follow_up.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "same-session follow-up completed before the accepted wait was abandoned"
    );

    let disconnected_at = Instant::now();
    drop(abandoned);
    let output = follow_up
        .recv_timeout(Duration::from_secs(1))
        .expect("same-session follow-up stayed behind the abandoned 120s wait");
    thread.join().expect("same-session worker panicked");
    assert!(
        disconnected_at.elapsed() < Duration::from_secs(1),
        "abandoned wait was not cancelled within the one-second bound"
    );
    assert_eq!(output_json("post-disconnect eval", &output), "second page");

    let usable_at = Instant::now();
    let status = h.json(&["daemon", "status"]);
    assert_eq!(status["sessions"], 1, "{status}");
    assert!(
        usable_at.elapsed() < Duration::from_millis(500),
        "daemon remained blocked after cancelling the abandoned wait"
    );
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
