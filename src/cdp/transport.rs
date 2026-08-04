//! CDP transport over an anonymous pipe pair (`--remote-debugging-pipe`).
//!
//! Chromium reads NUL-delimited UTF-8 JSON from **fd 3** and writes NUL-delimited
//! UTF-8 JSON to **fd 4**. There is no HTTP endpoint and no WebSocket server, which
//! is exactly why we use it: nothing else on the machine can reach this browser.
//!
//! Verified empirically against Google Chrome on macOS (2026-08-04): a single
//! `Browser.getVersion` written to fd 3 as `{"id":1,...}\0` produces a response on
//! fd 4, and session-scoped messages carrying a top-level `sessionId` are routed
//! correctly over the same pipe (flat protocol).
//!
//! We deliberately use blocking reader/writer threads bridged to Tokio channels
//! rather than registering the pipe fds with the reactor: a pipe is a single
//! ordered byte stream, the framing is trivial, and this keeps us off the parts of
//! the async-fd API that differ across platforms.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};

use tokio::sync::mpsc;

/// A framed, bidirectional CDP pipe.
pub struct PipeTransport {
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl PipeTransport {
    /// Takes ownership of the parent-side pipe ends.
    ///
    /// # Safety
    /// `write_fd` and `read_fd` must be valid, owned, open file descriptors that
    /// nothing else will close.
    pub unsafe fn from_raw_fds(write_fd: RawFd, read_fd: RawFd) -> Self {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let mut writer = std::fs::File::from_raw_fd(write_fd);
        std::thread::Builder::new()
            .name("brow-cdp-write".into())
            .spawn(move || {
                while let Some(frame) = out_rx.blocking_recv() {
                    if writer.write_all(&frame).is_err() || writer.write_all(b"\0").is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
                // Dropping `writer` closes fd 3's parent end, which is how Chromium
                // learns we are gone and shuts itself down.
            })
            .expect("spawn cdp writer thread");

        let mut reader = std::fs::File::from_raw_fd(read_fd);
        std::thread::Builder::new()
            .name("brow-cdp-read".into())
            .spawn(move || {
                let mut buf = Vec::with_capacity(64 * 1024);
                let mut chunk = vec![0u8; 64 * 1024];
                loop {
                    let n = match reader.read(&mut chunk) {
                        Ok(0) => break, // browser closed the pipe
                        Ok(n) => n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    // Frames are NUL-delimited; a single read may carry many frames
                    // or a partial one.
                    while let Some(pos) = buf.iter().position(|b| *b == 0) {
                        let frame: Vec<u8> = buf.drain(..pos).collect();
                        buf.drain(..1); // the NUL itself
                        if frame.is_empty() {
                            continue;
                        }
                        if in_tx.send(frame).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("spawn cdp reader thread");

        Self {
            outbound: out_tx,
            inbound: in_rx,
        }
    }

    /// Queues one JSON frame for delivery. The NUL terminator is added by the writer.
    pub fn send(&self, frame: Vec<u8>) -> Result<(), TransportClosed> {
        self.outbound.send(frame).map_err(|_| TransportClosed)
    }

    /// A cloneable handle for sending frames.
    pub fn sender(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        self.outbound.clone()
    }

    /// Awaits the next complete frame. `None` means the browser closed the pipe.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.inbound.recv().await
    }

    /// Splits into a sender handle and the inbound stream.
    pub fn split(self) -> (mpsc::UnboundedSender<Vec<u8>>, mpsc::UnboundedReceiver<Vec<u8>>) {
        (self.outbound, self.inbound)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("CDP transport closed")]
pub struct TransportClosed;

/// Creates a pipe whose ends are both `FD_CLOEXEC`.
///
/// The child gets its copies via `dup2` in a `pre_exec` hook, and `dup2` clears
/// `FD_CLOEXEC` on the duplicate — so the originals vanish at `exec` and exactly
/// fds 3 and 4 survive into Chromium.
pub fn cloexec_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array for the duration of the call.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: `fd` was just returned by a successful `pipe(2)`.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            let err = std::io::Error::last_os_error();
            // SAFETY: both fds are open and owned by us at this point.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err);
        }
    }
    Ok((fds[0], fds[1])) // (read end, write end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_split_on_nul_across_chunk_boundaries() {
        let (r, w) = cloexec_pipe().unwrap();
        // Loop the pipe back on itself: we write to `w`, we read from `r`.
        let mut transport = unsafe { PipeTransport::from_raw_fds(w, r) };

        transport.send(br#"{"id":1}"#.to_vec()).unwrap();
        transport.send(br#"{"id":2}"#.to_vec()).unwrap();

        assert_eq!(transport.recv().await.unwrap(), br#"{"id":1}"#.to_vec());
        assert_eq!(transport.recv().await.unwrap(), br#"{"id":2}"#.to_vec());
    }

    #[tokio::test]
    async fn large_frame_survives_chunking() {
        let (r, w) = cloexec_pipe().unwrap();
        let mut transport = unsafe { PipeTransport::from_raw_fds(w, r) };

        // Comfortably larger than the 64 KiB read chunk.
        let payload = format!(r#"{{"blob":"{}"}}"#, "x".repeat(300_000));
        transport.send(payload.clone().into_bytes()).unwrap();

        let got = transport.recv().await.unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload.into_bytes());
    }
}
