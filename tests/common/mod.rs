//! Test fixtures: a tiny HTTP server and the page `brow` is exercised against.
//!
//! Hand-rolled rather than pulled from a crate so the test suite has no network
//! and no dependency surface of its own — the point of these tests is to observe
//! a real Chromium, not to test somebody else's server.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

pub struct Fixture {
    pub base: String,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    state: std::sync::Arc<FixtureState>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct FixtureState {
    danger_mutation_requested: std::sync::atomic::AtomicBool,
    danger_mutation_observed: std::sync::atomic::AtomicBool,
    danger_target_clicked: std::sync::atomic::AtomicBool,
    decision_mutation_requested: std::sync::atomic::AtomicBool,
    decision_mutation_observed: std::sync::atomic::AtomicBool,
    decision_target_clicked: std::sync::atomic::AtomicBool,
    delayed_danger_released: std::sync::atomic::AtomicBool,
    side_effect_count: std::sync::atomic::AtomicUsize,
    navigation_request_count: std::sync::atomic::AtomicUsize,
    held_navigation: std::sync::Mutex<HeldNavigation>,
    held_navigation_changed: std::sync::Condvar,
}

#[derive(Default)]
struct HeldNavigation {
    arrived: bool,
    released: bool,
}

impl Fixture {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Tells the mutation fixture to rename its destructive control in place.
    #[allow(dead_code)]
    pub fn mutate_danger_target(&self) {
        self.state
            .danger_mutation_requested
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Waits until the fixture page confirms that the DOM mutation ran.
    #[allow(dead_code)]
    pub fn wait_for_danger_target_mutation(&self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self
                .state
                .danger_mutation_observed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("danger fixture did not confirm its target mutation within {timeout:?}");
    }

    /// Whether the destructive control was actually clicked.
    #[allow(dead_code)]
    pub fn danger_target_clicked(&self) -> bool {
        self.state
            .danger_target_clicked
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Tells the ambiguity fixture to rename the option selected by the test.
    #[allow(dead_code)]
    pub fn mutate_decision_target(&self) {
        self.state
            .decision_mutation_requested
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Waits until the ambiguity fixture confirms its in-place rename.
    #[allow(dead_code)]
    pub fn wait_for_decision_target_mutation(&self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self
                .state
                .decision_mutation_observed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("decision fixture did not confirm its target mutation within {timeout:?}");
    }

    /// Whether the ambiguity fixture's renamed option was actually clicked.
    #[allow(dead_code)]
    pub fn decision_target_clicked(&self) -> bool {
        self.state
            .decision_target_clicked
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Lets the delayed destructive page finish loading after test setup.
    #[allow(dead_code)]
    pub fn release_delayed_danger(&self) {
        self.state
            .delayed_danger_released
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[allow(dead_code)]
    pub fn side_effect_count(&self) -> usize {
        self.state
            .side_effect_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn navigation_request_count(&self) -> usize {
        self.state
            .navigation_request_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn wait_for_held_navigation(&self, timeout: std::time::Duration) {
        let state = self.state.held_navigation.lock().expect("held navigation");
        let (state, _) = self
            .state
            .held_navigation_changed
            .wait_timeout_while(state, timeout, |state| !state.arrived)
            .expect("held navigation condvar");
        assert!(
            state.arrived,
            "held navigation did not arrive within {timeout:?}"
        );
    }

    #[allow(dead_code)]
    pub fn release_held_navigation(&self) {
        let mut state = self.state.held_navigation.lock().expect("held navigation");
        state.released = true;
        self.state.held_navigation_changed.notify_all();
    }
}

/// Starts the fixture server on an ephemeral port.
pub fn serve() -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let port = listener.local_addr().unwrap().port();
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = std::sync::Arc::clone(&shutdown);
    let state = std::sync::Arc::new(FixtureState::default());
    let server_state = std::sync::Arc::clone(&state);
    listener
        .set_nonblocking(true)
        .expect("make fixture listener nonblocking");

    let thread = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // On macOS an accepted socket can inherit O_NONBLOCK from
                    // the listener. The per-request handler uses blocking
                    // BufRead; without this reset an early read races the first
                    // request byte, returns EAGAIN, and closes the connection.
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("fixture could not make accepted stream blocking: {error}");
                        continue;
                    }
                    let request_state = std::sync::Arc::clone(&server_state);
                    std::thread::spawn(move || {
                        if let Err(error) = handle(stream, &request_state) {
                            eprintln!("fixture request handler failed: {error}");
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                // A nonblocking listener may transiently report EINTR or an
                // aborted connection. Never turn that into a silent fixture
                // shutdown: retry while the owner is alive and surface the
                // error so a persistent OS failure remains diagnosable.
                Err(error) => {
                    eprintln!("fixture accept failed; retrying: {error}");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    });

    Fixture {
        base: format!("http://127.0.0.1:{port}"),
        shutdown,
        state,
        thread: Some(thread),
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(mut stream: TcpStream, state: &FixtureState) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain headers so the client is not left blocked on a half-read request.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let route = path.split('?').next().unwrap_or("/");
    if route == "/navigation" {
        state
            .navigation_request_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
    if route == "/nav-stream-held" {
        // The same-process child completes while the root response remains
        // open. A navigation waiter that accepts any frameStoppedLoading will
        // therefore return early; correct code must wait for the root frame.
        let prefix = "<!doctype html><html><head><meta charset=\"utf-8\"><title>streaming navigation</title></head><body><h1>Streaming navigation</h1><iframe id=\"fast-child\" src=\"/second\"></iframe>";
        let suffix = "</body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
            prefix.len() + suffix.len(),
            prefix
        );
        stream.write_all(response.as_bytes())?;
        stream.flush()?;
        let mut held = state.held_navigation.lock().expect("held navigation");
        held.arrived = true;
        state.held_navigation_changed.notify_all();
        let (held, _) = state
            .held_navigation_changed
            .wait_timeout_while(held, std::time::Duration::from_secs(10), |held| {
                !held.released
            })
            .expect("held navigation condvar");
        assert!(held.released, "streaming navigation was never released");
        drop(held);
        stream.write_all(suffix.as_bytes())?;
        return stream.flush();
    }
    let (status, content_type, extra_headers, body) = match route {
        "/second" => ("200 OK", "text/html; charset=utf-8", "", SECOND_PAGE),
        "/slow" => ("200 OK", "text/html; charset=utf-8", "", SLOW_PAGE),
        "/navigation" => ("200 OK", "text/html; charset=utf-8", "", NAVIGATION_PAGE),
        "/nav-slow" => {
            std::thread::sleep(std::time::Duration::from_millis(300));
            ("200 OK", "text/html; charset=utf-8", "", NAV_SLOW_PAGE)
        }
        "/nav-held" => {
            let state_guard = state.held_navigation.lock().expect("held navigation");
            let mut state_guard = state_guard;
            state_guard.arrived = true;
            state.held_navigation_changed.notify_all();
            let (state_guard, _) = state
                .held_navigation_changed
                .wait_timeout_while(state_guard, std::time::Duration::from_secs(10), |state| {
                    !state.released
                })
                .expect("held navigation condvar");
            assert!(state_guard.released, "held navigation was never released");
            ("200 OK", "text/html; charset=utf-8", "", NAV_HELD_PAGE)
        }
        "/events" => ("200 OK", "text/html; charset=utf-8", "", EVENTS_PAGE),
        "/frames" => ("200 OK", "text/html; charset=utf-8", "", FRAMES_PAGE),
        "/ambiguous" => ("200 OK", "text/html; charset=utf-8", "", AMBIGUOUS_PAGE),
        "/ambiguous-mutating" => (
            "200 OK",
            "text/html; charset=utf-8",
            "",
            MUTATING_AMBIGUOUS_PAGE,
        ),
        "/danger" => ("200 OK", "text/html; charset=utf-8", "", DANGER_PAGE),
        "/danger-evidence-delayed" => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !state
                .delayed_danger_released
                .load(std::sync::atomic::Ordering::Acquire)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (
                "200 OK",
                "text/html; charset=utf-8",
                "",
                MUTATING_DANGER_PAGE,
            )
        }
        "/danger-mutating" => (
            "200 OK",
            "text/html; charset=utf-8",
            "",
            MUTATING_DANGER_PAGE,
        ),
        "/danger-mutation-state"
            if state
                .danger_mutation_requested
                .load(std::sync::atomic::Ordering::Acquire) =>
        {
            ("200 OK", "text/plain; charset=utf-8", "", "mutate")
        }
        "/danger-mutation-state" => ("200 OK", "text/plain; charset=utf-8", "", "wait"),
        "/danger-mutation-observed" => {
            state
                .danger_mutation_observed
                .store(true, std::sync::atomic::Ordering::Release);
            ("200 OK", "text/plain; charset=utf-8", "", "ok")
        }
        "/danger-target-clicked" => {
            state
                .danger_target_clicked
                .store(true, std::sync::atomic::Ordering::Release);
            (
                "200 OK",
                "text/html; charset=utf-8",
                "",
                CLICKED_DANGER_PAGE,
            )
        }
        "/decision-mutation-state"
            if state
                .decision_mutation_requested
                .load(std::sync::atomic::Ordering::Acquire) =>
        {
            ("200 OK", "text/plain; charset=utf-8", "", "mutate")
        }
        "/decision-mutation-state" => ("200 OK", "text/plain; charset=utf-8", "", "wait"),
        "/decision-mutation-observed" => {
            state
                .decision_mutation_observed
                .store(true, std::sync::atomic::Ordering::Release);
            ("200 OK", "text/plain; charset=utf-8", "", "ok")
        }
        "/decision-target-clicked" => {
            state
                .decision_target_clicked
                .store(true, std::sync::atomic::Ordering::Release);
            (
                "200 OK",
                "text/html; charset=utf-8",
                "",
                CLICKED_DANGER_PAGE,
            )
        }
        "/frame-inner" => ("200 OK", "text/html; charset=utf-8", "", FRAME_INNER_PAGE),
        "/gestures" => ("200 OK", "text/html; charset=utf-8", "", GESTURES_PAGE),
        "/redirect-start" => (
            "302 Found",
            "text/plain; charset=utf-8",
            "Location: /redirect-middle\r\n",
            "redirecting",
        ),
        "/redirect-middle" => (
            "307 Temporary Redirect",
            "text/plain; charset=utf-8",
            "Location: /redirect-final\r\n",
            "redirecting again",
        ),
        "/redirect-final" => (
            "200 OK",
            "text/html; charset=utf-8",
            "",
            REDIRECT_FINAL_PAGE,
        ),
        "/api/ok" => ("200 OK", "application/json", "", r#"{"ok":true}"#),
        "/api/delayed" => {
            std::thread::sleep(std::time::Duration::from_millis(180));
            ("200 OK", "application/json", "", r#"{"ready":true}"#)
        }
        "/api/hang" => {
            std::thread::sleep(std::time::Duration::from_secs(2));
            ("200 OK", "application/json", "", r#"{"late":true}"#)
        }
        "/side-effect" => {
            state
                .side_effect_count
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            ("200 OK", "application/json", "", r#"{"accepted":true}"#)
        }
        "/missing-endpoint" => (
            "404 Not Found",
            "application/json",
            "",
            r#"{"error":"nope"}"#,
        ),
        _ => ("200 OK", "text/html; charset=utf-8", "", MAIN_PAGE),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra_headers}\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// The page every browser-level assertion runs against.
///
/// Element positions are absolute and explicit so geometry assertions have exact
/// expected values rather than "whatever the layout engine felt like".
pub const MAIN_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>brow fixture</title></head>
<body style="margin:0;padding:0">
  <div id="status">idle</div>

  <button id="go" style="position:absolute;left:100px;top:200px;width:220px;height:48px">
    Create account
  </button>

  <input id="email" type="text" placeholder="Email address"
         style="position:absolute;left:100px;top:300px;width:220px;height:40px">

  <button id="covered" style="position:absolute;left:100px;top:400px;width:200px;height:40px">
    Covered button
  </button>
  <div id="overlay"
       style="position:absolute;left:80px;top:390px;width:260px;height:60px;background:rgba(255,0,0,0.6)"></div>

  <a id="spa" href="#/dashboard" style="position:absolute;left:100px;top:500px">Go to dashboard</a>

  <!-- Styled-control pattern used by TodoMVC and many design systems: the
       native input is transparent, but remains the real pointer target. -->
  <input id="transparent-toggle" type="checkbox" aria-label="Transparent toggle"
         style="position:absolute;left:400px;top:580px;width:40px;height:40px;opacity:0">

  <!-- Painted and semantically interactive, but deliberately removed from
       CSS pointer hit testing. It must not be offered as a clickable ref. -->
  <button id="no-pointer" aria-label="No pointer action"
          style="position:absolute;left:500px;top:580px;width:160px;height:40px;pointer-events:none">
    No pointer action
  </button>

  <div id="host"></div>

  <div style="height:3000px"></div>

  <script>
    document.getElementById('go').addEventListener('click', function (e) {
      document.getElementById('status').textContent = 'clicked:' + e.isTrusted;
    });
    document.getElementById('email').addEventListener('input', function (e) {
      document.getElementById('status').textContent = 'typed:' + e.target.value;
    });
    document.getElementById('covered').addEventListener('click', function () {
      document.getElementById('status').textContent = 'covered-clicked';
    });
    document.getElementById('spa').addEventListener('click', function (e) {
      e.preventDefault();
      history.pushState({}, '', '#/dashboard');
      document.getElementById('status').textContent = 'route:dashboard';
    });
    document.getElementById('transparent-toggle').addEventListener('change', function (e) {
      document.getElementById('status').textContent =
        'transparent:' + e.isTrusted + ':' + e.target.checked;
    });
    // A *closed* shadow root: invisible to page JS, but CDP sees below the JS
    // boundary, so brow must still find this button.
    var sr = document.getElementById('host').attachShadow({ mode: 'closed' });
    sr.innerHTML = '<button id="shadowbtn">Shadow action</button>';
  </script>
</body></html>"##;

pub const SECOND_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>second page</title></head>
<body><h1>Second</h1><button id="only">Only button</button></body></html>"##;

pub const NAVIGATION_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>navigation fixture</title></head>
<body>
  <button id="cross">Delayed cross-document</button>
  <button id="slow-cross">Slow cross-document</button>
  <button id="held-cross">Held cross-document</button>
  <button id="spa">Delayed SPA</button>
  <button id="data">Load data</button>
  <button id="side-effect">Submit once</button>
  <button id="beforeunload">Protect navigation</button>
  <button id="hover">Hover target</button>
  <div id="state">idle</div>
  <div id="hover-card" hidden>hover card</div>
  <script>
    document.getElementById('cross').addEventListener('click', function () {
      setTimeout(function () { location.href = '/second'; }, 50);
    });
    document.getElementById('slow-cross').addEventListener('click', function () {
      setTimeout(function () { location.href = '/nav-slow'; }, 20);
    });
    document.getElementById('held-cross').addEventListener('click', function () {
      location.href = '/nav-held';
    });
    document.getElementById('spa').addEventListener('click', function () {
      setTimeout(function () {
        history.pushState({}, '', '#settled');
        document.getElementById('state').textContent = 'spa-ready';
      }, 50);
    });
    document.getElementById('data').addEventListener('click', function () {
      document.getElementById('state').textContent = 'skeleton';
      fetch('/api/delayed').then(function (response) { return response.json(); }).then(function () {
        document.getElementById('state').textContent = 'data-ready';
      });
    });
    document.getElementById('side-effect').addEventListener('click', function () {
      fetch('/side-effect', {method: 'POST'}).then(function (response) { return response.text(); });
      fetch('/api/hang').then(function (response) { return response.text(); });
    });
    document.getElementById('beforeunload').addEventListener('click', function () {
      window.onbeforeunload = function () { return 'fixture has unsaved work'; };
      document.getElementById('state').textContent = 'navigation-protected';
    });
    var hover = document.getElementById('hover');
    hover.addEventListener('mouseenter', function () {
      document.getElementById('hover-card').hidden = false;
    });
    hover.addEventListener('mouseleave', function () {
      document.getElementById('hover-card').hidden = true;
    });
  </script>
</body></html>"##;

pub const NAV_SLOW_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>slow navigation complete</title></head>
<body><h1>Slow navigation complete</h1></body></html>"##;

pub const NAV_HELD_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>held navigation complete</title></head>
<body><h1>Held navigation complete</h1></body></html>"##;

pub const REDIRECT_FINAL_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>redirect complete</title></head>
<body><h1 id="redirect-complete">Redirect complete</h1></body></html>"##;

/// Exercises every console level, an uncaught exception, a successful request
/// carrying a credential in its query string, and a 404.
pub const EVENTS_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>events</title></head>
<body><div id="done">no</div>
<script>
  console.log('plain message', 42);
  console.warn('a warning');
  console.error('an error happened');
  console.log('retrying with Bearer sk_live_9f8a7b6c5d4e');
  // Bodies are consumed on purpose: an unread response body leaves the request
  // without a loadingFinished event, and real application code reads them.
  Promise.all([
    fetch('/api/ok?access_token=supersecret12345&page=2').then(function (r) { return r.text(); }).catch(function(){}),
    fetch('/missing-endpoint').then(function (r) { return r.text(); }).catch(function(){})
  ]).then(function () { document.getElementById('done').textContent = 'yes'; });
  setTimeout(function () { window.__nope.boom(); }, 30);
</script>
</body></html>"##;

/// Three controls that all say "Continue". A job must refuse to pick one.
pub const AMBIGUOUS_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>ambiguous</title></head>
<body style="margin:0">
  <button id="a" aria-label="Continue as guest">Continue as guest</button>
  <button id="b" aria-label="Continue, second option">Continue, second option</button>
  <a id="c" href="#next" aria-label="Continue to checkout">Continue to checkout</a>
  <div id="picked">none</div>
  <script>
    for (const el of document.querySelectorAll('button, a')) {
      el.addEventListener('click', function (e) {
        e.preventDefault();
        document.getElementById('picked').textContent = this.id;
      });
    }
  </script>
</body></html>"##;

/// The selected ambiguity option can be renamed in place after the job parks.
pub const MUTATING_AMBIGUOUS_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>mutating ambiguity</title></head>
<body style="margin:0">
  <button id="a" aria-label="Continue as guest">Continue as guest</button>
  <button id="b" aria-label="Continue, second option">Continue, second option</button>
  <a id="c" href="#next" aria-label="Continue to checkout">Continue to checkout</a>
  <div id="picked">none</div>
  <script>
    var chosen = document.getElementById('b');
    chosen.addEventListener('click', function () {
      document.getElementById('picked').textContent = 'b';
      var marker = new XMLHttpRequest();
      marker.open('POST', '/decision-target-clicked', false);
      marker.send(null);
    });

    var mutationPoll = setInterval(function () {
      fetch('/decision-mutation-state?' + Date.now(), { cache: 'no-store' })
        .then(function (response) { return response.text(); })
        .then(function (command) {
          if (command.trim() !== 'mutate') return;
          clearInterval(mutationPoll);
          // Preserve document, session, frame, backend id, and tag. Only the
          // meaning visible to the person choosing this option changes.
          chosen.setAttribute('aria-label', 'Delete production database');
          chosen.textContent = 'Delete production database';
          return new Promise(function (resolve) {
            requestAnimationFrame(function () { requestAnimationFrame(resolve); });
          }).then(function () {
            return fetch('/decision-mutation-observed?' + Date.now(), {
              method: 'POST',
              cache: 'no-store'
            });
          });
        })
        .catch(function () {});
    }, 25);
  </script>
</body></html>"##;

/// One unmistakably destructive control.
pub const DANGER_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>danger</title></head>
<body style="margin:0">
  <button id="del">Delete workspace</button>
  <div id="state">intact</div>
  <script>
    document.getElementById('del').addEventListener('click', function () {
      document.getElementById('state').textContent = 'deleted';
    });
  </script>
</body></html>"##;

/// A destructive control whose accessible label can be changed without a
/// navigation or node replacement, under explicit control of the test process.
pub const MUTATING_DANGER_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>mutating danger</title></head>
<body style="margin:0">
  <button id="del">Delete workspace</button>
  <div id="state">intact</div>
  <script>
    var button = document.getElementById('del');
    button.addEventListener('click', function () {
      document.getElementById('state').textContent = 'clicked';
      // Synchronous on purpose: if brow dispatches a click, the test server's
      // external marker is committed before the handler can return.
      var marker = new XMLHttpRequest();
      marker.open('POST', '/danger-target-clicked', false);
      marker.send(null);
    });

    var mutationPoll = setInterval(function () {
      fetch('/danger-mutation-state?' + Date.now(), { cache: 'no-store' })
        .then(function (response) { return response.text(); })
        .then(function (command) {
          if (command.trim() !== 'mutate') return;
          clearInterval(mutationPoll);
          // Keep the exact DOM node and document, but change what the approved
          // control now means. A generation-only approval check misses this.
          button.setAttribute('aria-label', 'Delete production database');
          button.textContent = 'Delete production database';
          document.getElementById('state').textContent = 'mutated';
          // Acknowledge after a rendering turn so a subsequent CDP
          // accessibility read cannot race the DOM mutation.
          return new Promise(function (resolve) {
            requestAnimationFrame(function () { requestAnimationFrame(resolve); });
          }).then(function () {
            return fetch('/danger-mutation-observed?' + Date.now(), {
              method: 'POST',
              cache: 'no-store'
            });
          });
        })
        .catch(function () {});
    }, 25);
  </script>
</body></html>"##;

pub const CLICKED_DANGER_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>destructive target clicked</title></head>
<body><div id="state">clicked</div></body></html>"##;

/// A same-origin iframe whose controls are labelled *only* by `aria-label`.
///
/// Icon buttons with no text are the case that breaks: if the accessibility tree
/// is not read per frame, they come back nameless and an agent cannot tell them
/// apart.
pub const FRAMES_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>frames</title></head>
<body style="margin:0">
  <button id="top" aria-label="Outer close"><svg width="10" height="10"></svg></button>
  <iframe id="f" src="/frame-inner" style="width:400px;height:200px;border:0"></iframe>
</body></html>"##;

pub const FRAME_INNER_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0">
  <button id="deep" aria-label="Inner close"><svg width="10" height="10"></svg></button>
  <input id="q" aria-label="Inner search">
  <div id="clicked">no</div>
  <script>
    document.getElementById('deep').addEventListener('click', function (e) {
      document.getElementById('clicked').textContent = 'yes:' + e.isTrusted;
    });
  </script>
</body></html>"##;

/// Distinguishes tap, long-press and swipe from each other, and tracks a
/// pointer-based drag.
pub const GESTURES_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>gestures</title></head>
<body style="margin:0">
  <div id="pad" style="position:absolute;left:0;top:0;width:400px;height:400px;background:#eee"></div>
  <div id="out" style="position:absolute;left:0;top:420px">none</div>
  <div id="handle" style="position:absolute;left:500px;top:100px;width:80px;height:80px;background:#89f"></div>
  <div id="dragout" style="position:absolute;left:500px;top:220px">idle</div>
  <div id="touchcap" style="position:absolute;left:0;top:460px"></div>
<script>
  var pad = document.getElementById('pad');
  var out = document.getElementById('out');
  var moves = 0, startT = 0;
  pad.addEventListener('touchstart', function (e) {
    moves = 0; startT = Date.now();
    out.textContent = 'start:' + e.touches.length;
  });
  pad.addEventListener('touchmove', function () { moves++; });
  pad.addEventListener('touchend', function () {
    var dt = Date.now() - startT;
    if (moves > 3) out.textContent = 'swipe:' + moves;
    else if (dt > 500) out.textContent = 'longpress:' + dt;
    else out.textContent = 'tap';
  });

  var handle = document.getElementById('handle');
  var dragout = document.getElementById('dragout');
  var dragging = false, seen = 0;
  handle.addEventListener('mousedown', function () { dragging = true; seen = 0; dragout.textContent = 'down'; });
  document.addEventListener('mousemove', function (e) {
    if (dragging) { seen++; dragout.textContent = 'moving:' + seen + ':' + Math.round(e.clientX); }
  });
  document.addEventListener('mouseup', function (e) {
    if (dragging) { dragging = false; dragout.textContent = 'dropped:' + seen + ':' + Math.round(e.clientX); }
  });

  document.getElementById('touchcap').textContent =
    ('ontouchstart' in window) + ':' + navigator.maxTouchPoints;
</script>
</body></html>"##;

pub const SLOW_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>slow</title></head>
<body><div id="late">pending</div>
<script>setTimeout(function(){document.getElementById('late').textContent='ready'},300)</script>
</body></html>"##;

/// A scratch profile directory that cleans itself up.
#[allow(dead_code)] // not every test file uses every helper here
pub struct Scratch(pub PathBuf);

#[allow(dead_code)]
impl Scratch {
    pub fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "brow-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reads width and height out of a PNG's IHDR chunk.
#[allow(dead_code)] // used by the screenshot tests only
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

/// How many Chromium instances the whole test suite may run at once.
///
/// `cargo test` runs each test binary in parallel *and* threads within each one,
/// so without a bound the suite launches upwards of fifteen browsers on one
/// machine and starts failing on timeouts that have nothing to do with the code.
const BROWSER_SLOTS: usize = 4;

/// A reservation for one concurrent browser, released when dropped.
///
/// Uses `flock` on a small pool of files so the limit holds *across* test
/// binaries, which a `Mutex` or a thread-count flag cannot do.
#[allow(dead_code)]
pub struct BrowserSlot(std::fs::File);

#[allow(dead_code)]
pub struct ExclusiveBrowserSlots(Vec<std::fs::File>);

/// Blocks until a slot is free.
#[allow(dead_code)]
pub fn browser_slot() -> BrowserSlot {
    use std::os::unix::io::AsRawFd;

    let dir = std::path::Path::new("/tmp/brow-test-slots");
    let _ = std::fs::create_dir_all(dir);
    loop {
        // The shared pool gate makes selecting one slot atomic with respect to
        // an exclusive reservation. It is held only while scanning, never for
        // the lifetime of the browser, so ordinary tests still use all slots.
        let gate = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("pool.gate"))
            .expect("open browser test pool gate");
        // SAFETY: `gate` is open for the complete shared-lock lifetime.
        let rc = unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_SH) };
        assert_eq!(rc, 0, "lock shared browser test pool gate");
        for i in 0..BROWSER_SLOTS {
            let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(dir.join(format!("{i}.lock")))
            else {
                continue;
            };
            // SAFETY: `file` is open and outlives the lock it takes.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return BrowserSlot(file);
            }
        }
        drop(gate);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Reserves the entire cross-binary Chromium pool for a latency-sensitive test.
/// An exclusive pool gate prevents new ordinary reservations while existing
/// browsers drain. Slot locks are then taken in deterministic order, so there is
/// no circular wait and the exclusive test cannot starve under a busy suite.
#[allow(dead_code)]
pub fn exclusive_browser_slots() -> ExclusiveBrowserSlots {
    use std::os::unix::io::AsRawFd;

    let dir = std::path::Path::new("/tmp/brow-test-slots");
    let _ = std::fs::create_dir_all(dir);
    let gate = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("pool.gate"))
        .expect("open exclusive browser test pool gate");
    // SAFETY: `gate` is open until every slot below has been reserved.
    let rc = unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "lock exclusive browser test pool gate");
    let mut files = Vec::with_capacity(BROWSER_SLOTS);
    for i in 0..BROWSER_SLOTS {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(format!("{i}.lock")))
            .expect("open exclusive browser test slot");
        // SAFETY: `file` is open and retained by `ExclusiveBrowserSlots` for the
        // complete latency test.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(rc, 0, "lock exclusive browser test slot {i}");
        files.push(file);
    }
    // With all slots retained in `files`, later ordinary scans may proceed but
    // cannot acquire one until `ExclusiveBrowserSlots` is dropped.
    drop(gate);
    ExclusiveBrowserSlots(files)
}

/// True when a browser is available; the browser-level tests skip without one.
pub fn chrome_available() -> bool {
    let available = brow::browser::find().is_ok();
    if !available && std::env::var_os("BROW_REQUIRE_CHROME").is_some() {
        panic!("BROW_REQUIRE_CHROME is set but no Chromium-family browser was found");
    }
    available
}

/// Prints a skip notice once, so a skipped test is visible rather than silent.
pub fn skip(test: &str) {
    eprintln!("SKIP {test}: no Chromium-family browser found (set BROW_CHROME to run it)");
}
