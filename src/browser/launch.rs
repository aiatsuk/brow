//! Spawning Chromium with a private CDP pipe on fds 3 and 4.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::cdp::{cloexec_pipe, CdpClient, PipeTransport};

use super::discover::{self, Installed};

/// How the browser window is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Headless {
    /// `--headless=new`: real Chrome, no window. The default for jobs.
    #[default]
    New,
    /// A visible window, for when a human needs to watch or take over.
    Off,
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub headless: Headless,
    /// Profile directory. A fresh one per session unless the caller pins it.
    pub user_data_dir: PathBuf,
    pub window_size: (u32, u32),
    /// Extra flags, appended last so they can override ours.
    pub extra_args: Vec<String>,
}

impl LaunchOptions {
    pub fn new(user_data_dir: PathBuf) -> Self {
        Self {
            headless: Headless::default(),
            user_data_dir,
            window_size: (1280, 800),
            extra_args: Vec::new(),
        }
    }
}

/// A running browser plus its protocol connection.
pub struct Launched {
    pub child: Child,
    pub client: Arc<CdpClient>,
    pub browser: Installed,
    pub product: String,
}

/// The flag set, with a reason for every line.
///
/// Everything here is either required for the harness to function or removes a
/// source of nondeterminism. Flags that weaken the renderer sandbox are
/// deliberately absent: `--no-sandbox` never appears in this codebase.
fn base_args(opts: &LaunchOptions) -> Vec<String> {
    let mut args: Vec<String> = vec![
        // The whole point: protocol over an inherited pipe, no listening socket.
        "--remote-debugging-pipe".into(),
        format!("--user-data-dir={}", opts.user_data_dir.display()),
        // Determinism / first-run noise.
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-search-engine-choice-screen".into(),
        "--ash-no-nudges".into(),
        "--no-service-autorun".into(),
        "--propagate-iph-for-testing".into(),
        // Do not phone home. This tool is local-first; that has to be true of the
        // browser it drives, not just of our own code.
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-domain-reliability".into(),
        "--disable-sync".into(),
        "--metrics-recording-only".into(),
        "--disable-breakpad".into(),
        "--no-pings".into(),
        // Background tabs must keep running: a detached job is *entirely*
        // background, and throttled timers make automation flaky in ways that look
        // like application bugs.
        "--disable-background-timer-throttling".into(),
        "--disable-backgrounding-occluded-windows".into(),
        "--disable-renderer-backgrounding".into(),
        "--disable-ipc-flooding-protection".into(),
        // Keep the OS credential store out of it; on macOS this avoids a Keychain
        // prompt that no automated job can answer.
        "--use-mock-keychain".into(),
        "--password-store=basic".into(),
        format!("--window-size={},{}", opts.window_size.0, opts.window_size.1),
    ];

    if opts.headless == Headless::New {
        args.push("--headless=new".into());
    }

    args.extend(opts.extra_args.iter().cloned());
    // A blank starting page keeps the first target predictable.
    args.push("about:blank".into());
    args
}

/// Launches a browser and completes the protocol handshake.
pub async fn launch(opts: &LaunchOptions) -> anyhow::Result<Launched> {
    let browser = discover::find()?;
    std::fs::create_dir_all(&opts.user_data_dir)?;

    // us -> browser becomes the child's fd 3; browser -> us becomes its fd 4.
    let (child_read, our_write) = cloexec_pipe()?;
    let (our_read, child_write) = cloexec_pipe()?;

    let mut cmd = Command::new(&browser.path);
    cmd.args(base_args(opts))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Chromium is chatty on stderr; the daemon captures it separately when
        // debugging, but by default it is noise.
        .stderr(Stdio::null());

    // SAFETY: `pre_exec` runs between fork and exec, where only async-signal-safe
    // calls are legal. `dup2` and `close` are both on the POSIX safe list, and we
    // allocate nothing. Duplicating onto 3/4 also clears FD_CLOEXEC on the copies,
    // which is exactly the inheritance we want; the CLOEXEC originals disappear at
    // exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(child_read, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(child_write, 4) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().map_err(|e| {
        // SAFETY: we still own all four fds; the child never started.
        unsafe {
            libc::close(child_read);
            libc::close(child_write);
            libc::close(our_read);
            libc::close(our_write);
        }
        anyhow::anyhow!("failed to launch {}: {e}", browser.path.display())
    })?;

    // The child has its own copies now; ours would otherwise hold the pipe open
    // and mask the browser exiting.
    // SAFETY: these two fds are ours and unused from here on.
    unsafe {
        libc::close(child_read);
        libc::close(child_write);
    }

    // SAFETY: `our_write`/`our_read` are owned, open, and handed over exactly once.
    let transport = unsafe { PipeTransport::from_raw_fds(our_write, our_read) };
    let client = CdpClient::start(transport);

    // Handshake. Over a pipe there is no /json/version to poll, so the first
    // successful command *is* the readiness signal.
    let version = tokio::time::timeout(
        Duration::from_secs(30),
        client.call("Browser.getVersion", json!({})),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{browser} did not answer Browser.getVersion within 30s"))?
    .map_err(|e| anyhow::anyhow!("{browser} rejected the protocol handshake: {e}"))?;

    let product = version
        .get("product")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    tracing::info!(%browser, %product, "browser ready");

    Ok(Launched {
        child,
        client,
        browser,
        product,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_disables_the_sandbox() {
        let opts = LaunchOptions::new(PathBuf::from("/tmp/x"));
        let args = base_args(&opts);
        assert!(
            !args.iter().any(|a| a.contains("no-sandbox")),
            "the renderer sandbox must never be disabled by default"
        );
    }

    #[test]
    fn always_uses_the_pipe_never_a_port() {
        let opts = LaunchOptions::new(PathBuf::from("/tmp/x"));
        let args = base_args(&opts);
        assert!(args.iter().any(|a| a == "--remote-debugging-pipe"));
        assert!(
            !args.iter().any(|a| a.starts_with("--remote-debugging-port")),
            "a listening debug port would let any local process drive this browser"
        );
    }

    #[test]
    fn extra_args_come_after_ours_so_they_win() {
        let mut opts = LaunchOptions::new(PathBuf::from("/tmp/x"));
        opts.extra_args = vec!["--window-size=1,1".into()];
        let args = base_args(&opts);
        let ours = args.iter().position(|a| a == "--window-size=1280,800").unwrap();
        let theirs = args.iter().position(|a| a == "--window-size=1,1").unwrap();
        assert!(theirs > ours);
    }

    #[test]
    fn headless_off_omits_the_flag() {
        let mut opts = LaunchOptions::new(PathBuf::from("/tmp/x"));
        opts.headless = Headless::Off;
        assert!(!base_args(&opts).iter().any(|a| a.starts_with("--headless")));
    }
}
