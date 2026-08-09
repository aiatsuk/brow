//! Direct Chrome DevTools Protocol client.
//!
//! Deliberately hand-rolled and deliberately *not* generated from the full
//! `browser_protocol.json`: the generated surface is ~1.5 MB of Rust and buys us
//! nothing, because `brow` never exposes raw CDP to a caller. Only the methods the
//! capability API actually needs get typed wrappers; everything else goes through
//! `Value`.
//!
//! Note that `Schema.getDomains` was removed from Chromium (verified 2026-08-04:
//! `-32601 'Schema.getDomains' wasn't found`), so runtime capability discovery is
//! not available. Where a method may be missing on an older or newer Chrome, we
//! call it and treat `CdpError::is_method_not_found` as the feature probe.

pub mod conn;
pub mod transport;

pub use conn::{CdpClient, CdpError, CdpEvent, DEFAULT_TIMEOUT, EVENT_STREAM_GAP_METHOD};
pub use transport::{cloexec_pipe, PipeTransport};
