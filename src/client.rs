//! The CLI side of the socket: connect, auto-start the daemon, one round trip.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::ipc::{Hello, Request, Response, PROTOCOL_VERSION};
use crate::paths;

pub struct Client {
    reader: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    pub hello: Hello,
}

impl Client {
    /// Connects to a running daemon, starting one if necessary.
    ///
    /// Auto-start is what makes the daemon invisible in normal use: an agent runs
    /// `brow open ...` and never has to know a background process exists.
    pub async fn connect_or_start() -> anyhow::Result<Self> {
        if let Ok(client) = Self::connect().await {
            return Ok(client);
        }
        spawn_daemon()?;

        // The daemon has to create its socket, launch nothing, and bind. That is
        // fast, but not instant, and polling beats a fixed sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut backoff = Duration::from_millis(25);
        let mut last: Option<anyhow::Error> = None;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(400));
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(e) => last = Some(e),
            }
        }
        anyhow::bail!(
            "started browd but it never accepted a connection on {}{}\nCheck {}",
            paths::socket().display(),
            last.map(|e| format!(" ({e})")).unwrap_or_default(),
            paths::daemon_log().display()
        )
    }

    /// Connects to an already-running daemon, or fails.
    pub async fn connect() -> anyhow::Result<Self> {
        let stream = UnixStream::connect(paths::socket()).await?;
        let (read_half, writer) = stream.into_split();
        let mut reader = BufReader::new(read_half).lines();

        let line = reader
            .next_line()
            .await?
            .ok_or_else(|| anyhow::anyhow!("browd closed the connection before saying hello"))?;
        let hello: Hello = serde_json::from_str(&line)
            .map_err(|e| anyhow::anyhow!("browd sent an unreadable greeting: {e}"))?;

        anyhow::ensure!(
            hello.protocol == PROTOCOL_VERSION,
            "version mismatch: this CLI speaks protocol {PROTOCOL_VERSION}, the running \
             browd (v{}, pid {}) speaks {}. Run `brow daemon restart`.",
            hello.brow,
            hello.pid,
            hello.protocol
        );

        Ok(Self { reader, writer, hello })
    }

    /// Sends one request and reads its response.
    pub async fn request(&mut self, req: Request) -> anyhow::Result<Response> {
        let line = serde_json::to_string(&req)?;
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        let line = self
            .reader
            .next_line()
            .await?
            .ok_or_else(|| anyhow::anyhow!("browd closed the connection without answering"))?;
        Ok(serde_json::from_str(&line)?)
    }
}

/// Launches `browd` fully detached so it outlives this CLI process.
fn spawn_daemon() -> anyhow::Result<()> {
    paths::ensure_layout()?;
    let exe: PathBuf = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::daemon_log())?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));

    // SAFETY: `setsid` is async-signal-safe and is the whole point — a new session
    // means the daemon survives this shell, its terminal, and Ctrl-C.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                // Already a session leader is fine; anything else is not.
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EPERM) {
                    return Err(err);
                }
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    // Do not reap it here: we want it to keep running. Detach the handle.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Best-effort check for a live daemon without starting one.
pub async fn is_running() -> bool {
    Client::connect().await.is_ok()
}
