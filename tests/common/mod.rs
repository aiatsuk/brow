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
    _shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Fixture {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

/// Starts the fixture server on an ephemeral port.
pub fn serve() -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let port = listener.local_addr().unwrap().port();
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || {
                let _ = handle(stream);
            });
        }
    });

    Fixture {
        base: format!("http://127.0.0.1:{port}"),
        _shutdown: shutdown,
    }
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
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
    let (status, content_type, body) = match path.split('?').next().unwrap_or("/") {
        "/second" => ("200 OK", "text/html; charset=utf-8", SECOND_PAGE),
        "/slow" => ("200 OK", "text/html; charset=utf-8", SLOW_PAGE),
        "/events" => ("200 OK", "text/html; charset=utf-8", EVENTS_PAGE),
        "/gestures" => ("200 OK", "text/html; charset=utf-8", GESTURES_PAGE),
        "/api/ok" => ("200 OK", "application/json", r#"{"ok":true}"#),
        "/missing-endpoint" => ("404 Not Found", "application/json", r#"{"error":"nope"}"#),
        _ => ("200 OK", "text/html; charset=utf-8", MAIN_PAGE),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
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
    // A *closed* shadow root: invisible to page JS, but CDP sees below the JS
    // boundary, so brow must still find this button.
    var sr = document.getElementById('host').attachShadow({ mode: 'closed' });
    sr.innerHTML = '<button id="shadowbtn">Shadow action</button>';
  </script>
</body></html>"##;

pub const SECOND_PAGE: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>second page</title></head>
<body><h1>Second</h1><button id="only">Only button</button></body></html>"##;

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

/// True when a browser is available; the browser-level tests skip without one.
pub fn chrome_available() -> bool {
    brow::browser::find().is_ok()
}

/// Prints a skip notice once, so a skipped test is visible rather than silent.
pub fn skip(test: &str) {
    eprintln!("SKIP {test}: no Chromium-family browser found (set BROW_CHROME to run it)");
}
