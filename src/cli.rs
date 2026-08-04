//! Command surface.
//!
//! Designed for an LLM caller first and a human second, which mostly means:
//! short verbs, one obvious way to do each thing, output that is small by
//! default, and errors that say what to do next. Anything that would dump
//! hundreds of kilobytes has to be asked for explicitly.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "brow",
    version,
    about = "Local-first browser harness for AI agents",
    long_about = "Drives a persistent Chromium over the DevTools Protocol.\n\
                  The browser outlives this command: `brow open` starts a session, \
                  later commands act on it, `brow close` ends it."
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Which browser session to act on.
    #[arg(long, global = true, default_value = "default", value_name = "NAME")]
    pub session: String,

    /// Increase log verbosity (repeat for more).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open a URL, starting the session if it is not running.
    Open {
        url: String,
        /// Show a real browser window instead of running headless.
        #[arg(long)]
        headed: bool,
    },

    /// Capture the page tree and mint fresh @node-N refs.
    Snapshot {
        /// Every visible node, not just the interactive ones. Large.
        #[arg(long)]
        all: bool,
        /// Interactive elements only (the default).
        #[arg(long, conflicts_with = "all")]
        interactive: bool,
    },

    /// Click an element, or a raw coordinate pair.
    Click {
        /// A ref like @node-42, or a point like 512,340.
        target: String,
        #[arg(long, default_value = "left", value_parser = ["left", "right", "middle", "back", "forward"])]
        button: String,
        /// Double-click.
        #[arg(long, conflicts_with = "count")]
        double: bool,
        /// Number of clicks.
        #[arg(long, default_value_t = 1)]
        count: i64,
        /// Click even if another element is on top.
        #[arg(long)]
        force: bool,
    },

    /// Move the pointer onto an element so hover states engage.
    Hover { node_ref: String },

    /// Focus a field, clear it, and type into it.
    Fill { node_ref: String, text: String },

    /// Type into whatever is focused.
    Type {
        text: String,
        /// Send one key event per character, for inputs driven by key handlers.
        #[arg(long)]
        by_key: bool,
    },

    /// Press a key or chord, e.g. Enter, Tab, Ctrl+A, Cmd+Shift+K.
    Press { chord: String },

    /// Scroll the page.
    Scroll {
        #[arg(value_parser = ["up", "down", "left", "right"])]
        direction: String,
        /// Distance in CSS pixels.
        #[arg(default_value_t = 500.0)]
        amount: f64,
    },

    /// Save a screenshot.
    Screenshot {
        /// The whole scrollable document.
        #[arg(long, conflicts_with_all = ["node", "rect"])]
        full_page: bool,
        /// A single element, by ref.
        #[arg(long, value_name = "REF", conflicts_with = "rect")]
        node: Option<String>,
        /// A document-space rectangle: x,y,width,height.
        #[arg(long, value_name = "X,Y,W,H")]
        rect: Option<String>,
        /// Where to write it. Defaults to the session's artifact directory.
        #[arg(long, short)]
        out: Option<String>,
        #[arg(long, default_value = "png", value_parser = ["png", "jpeg", "jpg", "webp"])]
        format: String,
        /// 0-100, for jpeg and webp.
        #[arg(long)]
        quality: Option<i64>,
    },

    /// Evaluate a JavaScript expression against the page.
    ///
    /// Read-only by default and enforced by V8: an expression that tries to
    /// mutate anything is aborted before it takes effect.
    Eval {
        expression: String,
        /// Allow the expression to change the page.
        #[arg(long)]
        mutate: bool,
    },

    /// Close a session and its browser.
    Close,

    /// List open sessions.
    Sessions,

    /// Show daemon status.
    Status,

    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    /// Start the daemon if it is not already running.
    Start,
    /// Stop the daemon, closing every session.
    Stop,
    /// Stop then start.
    Restart,
    /// Report whether the daemon is running.
    Status,
    /// Run the daemon in the foreground (used internally by auto-start).
    Serve,
}

/// Parses `512,340` into a point.
pub fn parse_point(s: &str) -> Option<(f64, f64)> {
    let (x, y) = s.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Parses `x,y,w,h`.
pub fn parse_rect(s: &str) -> Option<(f64, f64, f64, f64)> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
        parts[3].parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn click_target_accepts_refs_and_points() {
        assert_eq!(parse_point("512,340"), Some((512.0, 340.0)));
        assert_eq!(parse_point(" 12 , 8 "), Some((12.0, 8.0)));
        assert_eq!(parse_point("@node-42"), None, "a ref is not a point");
        assert_eq!(parse_point("512"), None);
    }

    #[test]
    fn rect_parsing_requires_four_numbers() {
        assert_eq!(parse_rect("1,2,3,4"), Some((1.0, 2.0, 3.0, 4.0)));
        assert_eq!(parse_rect("1,2,3"), None);
        assert_eq!(parse_rect("1,2,3,4,5"), None);
    }

    #[test]
    fn snapshot_defaults_to_the_small_output() {
        let cli = Cli::try_parse_from(["brow", "snapshot"]).unwrap();
        match cli.command {
            Command::Snapshot { all, .. } => assert!(!all, "the default must not dump the page"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["brow", "open", "http://x/", "--json", "--session", "qa"])
            .unwrap();
        assert!(cli.json);
        assert_eq!(cli.session, "qa");
    }

    #[test]
    fn conflicting_screenshot_regions_are_rejected() {
        assert!(
            Cli::try_parse_from(["brow", "screenshot", "--full-page", "--node", "@node-1"])
                .is_err()
        );
    }
}
