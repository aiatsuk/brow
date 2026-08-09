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
use std::sync::{Arc, Condvar, Mutex};

use tokio::sync::mpsc;

/// Largest JSON message accepted in either direction (excluding the NUL).
///
/// This is deliberately much larger than ordinary commands and screenshots, but
/// finite so a malformed peer or accidental giant expression cannot allocate an
/// unbounded frame buffer.
pub const MAX_CDP_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Number of complete JSON messages that may wait for the pipe writer.
///
/// Sending is deliberately non-blocking: once this queue is full the caller gets
/// an explicit overload error and can release its pending-request slot instead of
/// waiting behind an arbitrarily large backlog.
const OUTBOUND_QUEUE_CAPACITY: usize = 256;

/// Aggregate payload bytes waiting for the pipe writer. A frame already dequeued
/// by the writer no longer counts against this queue budget.
const OUTBOUND_QUEUE_BYTE_CAPACITY: usize = 64 * 1024 * 1024;

/// Number of complete JSON messages that may wait for the async CDP router.
///
/// The pipe reader applies backpressure with `blocking_send` at this boundary.
/// The router never waits on event subscribers (its broadcast is itself bounded),
/// so draining this queue does not depend on application-level consumers and does
/// not introduce a response/event deadlock.
const INBOUND_QUEUE_CAPACITY: usize = 256;

/// Aggregate complete-frame bytes waiting for the async CDP router. The reader
/// stops reading the OS pipe while this budget is exhausted, applying bounded
/// backpressure independently of event subscribers.
const INBOUND_QUEUE_BYTE_CAPACITY: usize = 64 * 1024 * 1024;

const READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct ByteBudget {
    capacity: usize,
    state: Mutex<ByteBudgetState>,
    changed: Condvar,
}

#[derive(Debug)]
struct ByteBudgetState {
    available: usize,
    closed: bool,
}

#[derive(Debug)]
enum TryAcquireError {
    Closed,
    Exhausted,
}

impl ByteBudget {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            state: Mutex::new(ByteBudgetState {
                available: capacity,
                closed: false,
            }),
            changed: Condvar::new(),
        })
    }

    fn try_acquire(self: &Arc<Self>, size: usize) -> Result<BytePermit, TryAcquireError> {
        let mut state = self.state.lock().expect("byte budget mutex");
        if state.closed {
            return Err(TryAcquireError::Closed);
        }
        if size > state.available {
            return Err(TryAcquireError::Exhausted);
        }
        state.available -= size;
        Ok(BytePermit {
            budget: Arc::clone(self),
            size,
        })
    }

    fn acquire_blocking(self: &Arc<Self>, size: usize) -> Option<BytePermit> {
        debug_assert!(size <= self.capacity);
        let mut state = self.state.lock().expect("byte budget mutex");
        while size > state.available && !state.closed {
            state = self.changed.wait(state).expect("byte budget mutex");
        }
        if state.closed {
            return None;
        }
        state.available -= size;
        Some(BytePermit {
            budget: Arc::clone(self),
            size,
        })
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("byte budget mutex");
        state.closed = true;
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn available(&self) -> usize {
        self.state.lock().expect("byte budget mutex").available
    }
}

#[derive(Debug)]
struct BytePermit {
    budget: Arc<ByteBudget>,
    size: usize,
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock().expect("byte budget mutex");
        state.available += self.size;
        debug_assert!(state.available <= self.budget.capacity);
        self.budget.changed.notify_all();
    }
}

#[derive(Debug)]
struct QueuedFrame {
    bytes: Vec<u8>,
    _permit: BytePermit,
}

impl QueuedFrame {
    fn into_bytes(self) -> Vec<u8> {
        let Self { bytes, _permit } = self;
        drop(_permit);
        bytes
    }
}

/// Cloneable, bounded handle for queueing a frame to the pipe writer.
#[derive(Clone)]
pub struct PipeSender {
    inner: mpsc::Sender<QueuedFrame>,
    byte_budget: Arc<ByteBudget>,
    capacity: usize,
    max_frame_bytes: usize,
}

impl PipeSender {
    /// Queues one frame without waiting for capacity.
    pub fn send(&self, frame: Vec<u8>) -> Result<(), TransportSendError> {
        let size = frame.len();
        if size > self.max_frame_bytes {
            return Err(TransportSendError::FrameTooLarge {
                size,
                max: self.max_frame_bytes,
            });
        }
        let permit = match self.byte_budget.try_acquire(size) {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => return Err(TransportSendError::Closed),
            Err(TryAcquireError::Exhausted) => {
                return Err(TransportSendError::ByteBudgetExhausted {
                    size,
                    capacity: self.byte_budget.capacity,
                });
            }
        };
        let queued = QueuedFrame {
            bytes: frame,
            _permit: permit,
        };
        self.inner.try_send(queued).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => TransportSendError::Overloaded {
                capacity: self.capacity,
            },
            mpsc::error::TrySendError::Closed(_) => TransportSendError::Closed,
        })
    }

    /// Applies byte and message backpressure from the dedicated pipe-reader
    /// thread. This must never be called on a Tokio worker thread.
    fn send_blocking(&self, frame: Vec<u8>) -> Result<(), TransportSendError> {
        let size = frame.len();
        if size > self.max_frame_bytes {
            return Err(TransportSendError::FrameTooLarge {
                size,
                max: self.max_frame_bytes,
            });
        }
        let permit = self
            .byte_budget
            .acquire_blocking(size)
            .ok_or(TransportSendError::Closed)?;
        self.inner
            .blocking_send(QueuedFrame {
                bytes: frame,
                _permit: permit,
            })
            .map_err(|_| TransportSendError::Closed)
    }
}

/// Bounded inbound stream. Dequeueing releases the queue's byte permit before
/// handing ownership of the frame to the router.
pub struct PipeReceiver {
    inner: mpsc::Receiver<QueuedFrame>,
    byte_budget: Arc<ByteBudget>,
}

impl PipeReceiver {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.inner.recv().await.map(QueuedFrame::into_bytes)
    }

    fn blocking_recv(&mut self) -> Option<Vec<u8>> {
        self.inner.blocking_recv().map(QueuedFrame::into_bytes)
    }
}

impl Drop for PipeReceiver {
    fn drop(&mut self) {
        self.byte_budget.close();
    }
}

fn bounded_frame_channel(
    message_capacity: usize,
    byte_capacity: usize,
    max_frame_bytes: usize,
) -> (PipeSender, PipeReceiver) {
    assert!(message_capacity > 0);
    assert!(byte_capacity >= max_frame_bytes);
    let (inner, receiver) = mpsc::channel(message_capacity);
    let byte_budget = ByteBudget::new(byte_capacity);
    (
        PipeSender {
            inner,
            byte_budget: Arc::clone(&byte_budget),
            capacity: message_capacity,
            max_frame_bytes,
        },
        PipeReceiver {
            inner: receiver,
            byte_budget: Arc::clone(&byte_budget),
        },
    )
}

/// A framed, bidirectional CDP pipe.
pub struct PipeTransport {
    outbound: PipeSender,
    inbound: PipeReceiver,
}

impl PipeTransport {
    /// Takes ownership of the parent-side pipe ends.
    ///
    /// # Safety
    /// `write_fd` and `read_fd` must be valid, owned, open file descriptors that
    /// nothing else will close.
    pub unsafe fn from_raw_fds(write_fd: RawFd, read_fd: RawFd) -> Self {
        let (outbound, mut out_rx) = bounded_frame_channel(
            OUTBOUND_QUEUE_CAPACITY,
            OUTBOUND_QUEUE_BYTE_CAPACITY,
            MAX_CDP_FRAME_BYTES,
        );
        let (in_tx, inbound) = bounded_frame_channel(
            INBOUND_QUEUE_CAPACITY,
            INBOUND_QUEUE_BYTE_CAPACITY,
            MAX_CDP_FRAME_BYTES,
        );

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
            .spawn(
                move || match read_frames(&mut reader, &in_tx, MAX_CDP_FRAME_BYTES) {
                    ReaderExit::Eof | ReaderExit::ReceiverClosed => {}
                    ReaderExit::FrameTooLarge { size, max } => {
                        tracing::warn!(size, max, "oversized CDP frame closed the transport");
                    }
                    ReaderExit::Io(error) => {
                        tracing::warn!(%error, "CDP pipe read failed");
                    }
                },
            )
            .expect("spawn cdp reader thread");

        Self { outbound, inbound }
    }

    /// Queues one JSON frame for delivery. The NUL terminator is added by the writer.
    pub fn send(&self, frame: Vec<u8>) -> Result<(), TransportSendError> {
        self.outbound.send(frame)
    }

    /// A cloneable handle for sending frames.
    pub fn sender(&self) -> PipeSender {
        self.outbound.clone()
    }

    /// Awaits the next complete frame. `None` means the browser closed the pipe.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.inbound.recv().await
    }

    /// Splits into a sender handle and the inbound stream.
    pub fn split(self) -> (PipeSender, PipeReceiver) {
        (self.outbound, self.inbound)
    }
}

enum ReaderExit {
    Eof,
    ReceiverClosed,
    FrameTooLarge { size: usize, max: usize },
    Io(std::io::Error),
}

fn read_frames(reader: &mut impl Read, inbound: &PipeSender, max_frame_bytes: usize) -> ReaderExit {
    let initial_capacity = max_frame_bytes.min(READ_CHUNK_BYTES);
    let mut frame = Vec::with_capacity(initial_capacity);
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => return ReaderExit::Eof,
            Ok(n) => n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return ReaderExit::Io(error),
        };

        let mut start = 0;
        for end in (0..n).filter(|index| chunk[*index] == 0) {
            let segment = &chunk[start..end];
            let size = frame.len().saturating_add(segment.len());
            if size > max_frame_bytes {
                return ReaderExit::FrameTooLarge {
                    size,
                    max: max_frame_bytes,
                };
            }
            frame.extend_from_slice(segment);
            if !frame.is_empty() {
                let complete = std::mem::replace(&mut frame, Vec::with_capacity(initial_capacity));
                if inbound.send_blocking(complete).is_err() {
                    return ReaderExit::ReceiverClosed;
                }
            }
            start = end + 1;
        }

        let remainder = &chunk[start..n];
        let size = frame.len().saturating_add(remainder.len());
        if size > max_frame_bytes {
            return ReaderExit::FrameTooLarge {
                size,
                max: max_frame_bytes,
            };
        }
        frame.extend_from_slice(remainder);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransportSendError {
    #[error("CDP transport closed")]
    Closed,
    #[error("CDP outbound queue is full ({capacity} messages); retry later")]
    Overloaded { capacity: usize },
    #[error("CDP frame is {size} bytes; maximum is {max} bytes")]
    FrameTooLarge { size: usize, max: usize },
    #[error(
        "CDP outbound byte budget is exhausted (frame {size} bytes, budget {capacity} bytes); retry later"
    )]
    ByteBudgetExhausted { size: usize, capacity: usize },
}

/// Creates a pipe whose ends are both `FD_CLOEXEC`.
///
/// The child gets its copies via `dup2` in a `pre_exec` hook, and `dup2` clears
/// `FD_CLOEXEC` on the duplicate — so the originals vanish at `exec` and exactly
/// fds 3 and 4 survive into Chromium.
pub fn cloexec_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: `fds` is a valid two-element array; pipe2 sets CLOEXEC
        // atomically before either descriptor is visible to another thread.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        return Ok((fds[0], fds[1]));
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_sender_reports_overload_without_waiting() {
        let (sender, _receiver) = bounded_frame_channel(1, 8, 8);

        sender.send(vec![1]).unwrap();
        assert_eq!(
            sender.send(vec![2]).unwrap_err(),
            TransportSendError::Overloaded { capacity: 1 }
        );
        assert_eq!(
            sender.byte_budget.available(),
            7,
            "a rejected message must release its temporary byte permit"
        );
    }

    #[test]
    fn oversized_outbound_frame_is_rejected_before_queueing() {
        let (sender, _receiver) = bounded_frame_channel(2, 8, 4);

        assert_eq!(
            sender.send(vec![0; 5]).unwrap_err(),
            TransportSendError::FrameTooLarge { size: 5, max: 4 }
        );
        assert_eq!(sender.byte_budget.available(), 8);
    }

    #[tokio::test]
    async fn byte_budget_is_released_on_dequeue_and_receiver_drop() {
        let (sender, mut receiver) = bounded_frame_channel(4, 5, 5);

        sender.send(vec![1; 4]).unwrap();
        assert_eq!(
            sender.send(vec![2; 2]).unwrap_err(),
            TransportSendError::ByteBudgetExhausted {
                size: 2,
                capacity: 5,
            }
        );

        assert_eq!(receiver.recv().await.unwrap(), vec![1; 4]);
        assert_eq!(sender.byte_budget.available(), 5);

        sender.send(vec![3; 5]).unwrap();
        drop(receiver);
        assert_eq!(sender.byte_budget.available(), 5);
        assert_eq!(
            sender.send(vec![4]).unwrap_err(),
            TransportSendError::Closed
        );
        assert_eq!(sender.byte_budget.available(), 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blocking_backpressure_resumes_when_router_dequeues() {
        let (sender, mut receiver) = bounded_frame_channel(4, 4, 4);
        sender.send(vec![1; 4]).unwrap();

        let blocked_sender = sender.clone();
        let blocked = std::thread::spawn(move || blocked_sender.send_blocking(vec![2; 4]));
        std::thread::yield_now();
        assert!(!blocked.is_finished(), "second frame should wait for bytes");

        assert_eq!(receiver.recv().await.unwrap(), vec![1; 4]);
        blocked.join().unwrap().unwrap();
        assert_eq!(receiver.recv().await.unwrap(), vec![2; 4]);
        assert_eq!(sender.byte_budget.available(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_receiver_wakes_a_sender_blocked_on_bytes() {
        let (sender, receiver) = bounded_frame_channel(4, 4, 4);
        // Hold every permit outside the channel. Dropping the receiver therefore
        // cannot wake the waiter merely by dropping queued frames; the explicit
        // closed flag and Condvar notification are what must release it.
        let held_permit = sender.byte_budget.try_acquire(4).unwrap();

        let blocked_sender = sender.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let blocked = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(blocked_sender.send_blocking(vec![2; 4]))
                .unwrap();
        });
        started_rx.recv().unwrap();

        drop(receiver);
        assert_eq!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            Err(TransportSendError::Closed)
        );
        blocked.join().unwrap();
        assert_eq!(sender.byte_budget.available(), 0);
        drop(held_permit);
        assert_eq!(sender.byte_budget.available(), 4);
    }

    #[tokio::test]
    async fn nul_less_oversized_inbound_frame_closes_the_stream() {
        let (sender, mut receiver) = bounded_frame_channel(2, 32, 32);
        let mut input = std::io::Cursor::new(vec![b'x'; 33]);

        let exit = read_frames(&mut input, &sender, 32);
        assert!(matches!(
            exit,
            ReaderExit::FrameTooLarge { size: 33, max: 32 }
        ));
        drop(sender);
        assert!(receiver.recv().await.is_none());
    }

    /// This invariant is load-bearing, not hygiene.
    ///
    /// The parent keeps one end of each pipe. If those ends were inheritable, the
    /// *next* browser we launch would inherit them and hold the first browser's
    /// pipe open — and since closing the pipe is Chromium's only shutdown signal,
    /// killing the first browser would leave it running forever. One long session
    /// spawning browsers would accumulate multi-gigabyte orphans.
    #[test]
    fn pipe_ends_are_never_inherited_by_a_child() {
        let (r, w) = cloexec_pipe().unwrap();
        for fd in [r, w] {
            // SAFETY: both fds were just created and are still open.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD failed on fd {fd}");
            assert!(
                flags & libc::FD_CLOEXEC != 0,
                "fd {fd} would leak into every child process"
            );
        }
        // SAFETY: we own both fds and have not handed them to a transport.
        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }

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
