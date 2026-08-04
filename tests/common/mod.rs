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
    let body = match path.split('?').next().unwrap_or("/") {
        "/second" => SECOND_PAGE,
        "/slow" => SLOW_PAGE,
        _ => MAIN_PAGE,
    };

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
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
