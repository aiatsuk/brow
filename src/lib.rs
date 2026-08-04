//! `brow` — a local-first browser harness for AI agents.
//!
//! The shape is deliberate:
//!
//! ```text
//! brow (CLI)  ──unix socket──▶  browd (daemon)  ──CDP pipe──▶  Chromium
//! ```
//!
//! * The **daemon** owns every browser, so a session outlives the one-shot
//!   process that an agent turn actually is.
//! * The **socket protocol** ([`ipc::Request`]) is the entire capability surface.
//!   There is no variant that carries a CDP method name, so "the agent cannot
//!   issue raw protocol commands" is enforced by the type system rather than by a
//!   filter that someone has to remember to update.
//! * The **CDP pipe** means the browser has no listening debug port, so no other
//!   process on the machine can drive it.

pub mod browser;
pub mod cdp;
pub mod cli;
pub mod client;
pub mod daemon;
pub mod ipc;
pub mod page;
pub mod paths;
pub mod redact;
