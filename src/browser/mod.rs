//! Browser discovery, launch and process ownership.

pub mod discover;
pub mod launch;

pub use discover::{find, Flavor, Installed};
pub use launch::{launch, Headless, LaunchOptions, Launched};
