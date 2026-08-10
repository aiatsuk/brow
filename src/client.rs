//! The CLI side of the socket: connect, auto-start the daemon, one round trip.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::ipc::{encode_frame, Hello, IpcFrameReader, Request, Response, PROTOCOL_VERSION};
use crate::paths;

pub struct Client {
    reader: IpcFrameReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    pub hello: Hello,
}

impl Client {
    /// Connects to a running daemon, starting one if necessary.
    ///
    /// Auto-start is what makes the daemon invisible in normal use: an agent runs
    /// `brow open ...` and never has to know a background process exists.
    pub async fn connect_or_start() -> anyhow::Result<Self> {
        match Self::connect_if_running().await? {
            Some(client) => return Ok(client),
            None => spawn_daemon()?,
        }

        // The daemon has to create its socket, launch nothing, and bind. That is
        // fast, but not instant, and polling beats a fixed sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut backoff = Duration::from_millis(25);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(400));
            match Self::connect_if_running().await? {
                Some(client) => return Ok(client),
                None => continue,
            }
        }
        anyhow::bail!(
            "started browd but it never accepted a connection on {}\nCheck {}",
            paths::socket().display(),
            paths::daemon_log().display()
        )
    }

    /// Connects to an already-running daemon, or fails.
    pub async fn connect() -> anyhow::Result<Self> {
        Self::connect_if_running().await?.ok_or_else(|| {
            anyhow::anyhow!("no browd is listening on {}", paths::socket().display())
        })
    }

    /// Connects when a compatible daemon is listening.
    ///
    /// Only a missing socket and a refused connection mean "not running". Once
    /// the socket accepts us, every greeting or protocol failure belongs to that
    /// live endpoint and must reach the caller instead of triggering auto-start.
    pub async fn connect_if_running() -> anyhow::Result<Option<Self>> {
        let stream = match UnixStream::connect(paths::socket()).await {
            Ok(stream) => stream,
            Err(error) if connection_means_absent(&error) => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "cannot connect to browd on {}: {error}",
                    paths::socket().display()
                ));
            }
        };
        let (read_half, writer) = stream.into_split();
        let mut reader = IpcFrameReader::new(BufReader::new(read_half));

        let frame = reader
            .read_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("browd closed the connection before saying hello"))?;
        let hello: Hello = serde_json::from_slice(&frame)
            .map_err(|e| anyhow::anyhow!("browd sent an unreadable greeting: {e}"))?;

        ensure_protocol(&hello)?;

        Ok(Some(Self {
            reader,
            writer,
            hello,
        }))
    }

    /// Sends one request and reads its response.
    pub async fn request(&mut self, req: Request) -> anyhow::Result<Response> {
        let frame = encode_frame(&req)?;
        self.writer.write_all(&frame).await?;
        self.writer.flush().await?;

        let frame = self
            .reader
            .read_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("browd closed the connection without answering"))?;
        Ok(serde_json::from_slice(&frame)?)
    }
}

fn connection_means_absent(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
    )
}

fn ensure_protocol(hello: &Hello) -> anyhow::Result<()> {
    anyhow::ensure!(
        hello.protocol == PROTOCOL_VERSION,
        "version mismatch: this CLI speaks protocol {PROTOCOL_VERSION}, the running \
         browd (v{}, pid {}) speaks {}. Stop that daemon with the matching brow v{} \
         CLI, or first verify PID {} belongs to that browd and terminate it manually. \
         Then run `brow daemon start` with this CLI.",
        hello.brow,
        hello.pid,
        hello.protocol,
        hello.brow,
        hello.pid
    );
    Ok(())
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

/// Best-effort check for a live daemon endpoint without starting one.
///
/// `false` is reserved for the two socket errors that mean absence. A greeting,
/// protocol, permission, or other connection failure must not be mistaken for a
/// stopped daemon by callers that only need a conservative liveness signal.
pub async fn is_running() -> bool {
    !matches!(Client::connect_if_running().await, Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_protocol_versions_fail_at_hello_with_recovery_guidance() {
        let old = Hello {
            brow: "0.0.9".into(),
            protocol: 1,
            pid: 42,
        };
        let error = ensure_protocol(&old).expect_err("v1 daemon must not accept v2 requests");
        let message = error.to_string();
        assert!(message.contains("version mismatch"));
        assert!(message.contains("matching brow v0.0.9 CLI"));
        assert!(message.contains("verify PID 42"));
        assert!(message.contains("daemon start"));
        assert!(!message.contains("daemon restart"));

        let current = Hello {
            protocol: PROTOCOL_VERSION,
            ..old
        };
        ensure_protocol(&current).expect("matching protocol");
    }
}
