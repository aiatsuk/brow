use clap::Parser;
use serde_json::json;

use brow::cli::{parse_point, parse_rect, Cli, Command, DaemonAction, JobAction};
use brow::client::{self, Client};
use brow::ipc::{Request, Response, ShotTarget, Target};
use brow::{daemon, paths};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match run(&cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("brow: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("BROW_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(cli: &Cli) -> anyhow::Result<std::process::ExitCode> {
    // The daemon lifecycle commands do not go over the socket.
    if let Command::Daemon { action } = &cli.command {
        return daemon_command(action, cli.json).await;
    }
    // `job logs --follow` is a loop of ordinary requests rather than a streaming
    // response: it keeps the protocol strictly request/response, and the polling
    // cost is trivial next to running a browser.
    if let Command::Job { action: JobAction::Logs { id, follow } } = &cli.command {
        return follow_logs(id, *follow, cli.json).await;
    }

    let request = build_request(cli)?;
    let mut client = Client::connect_or_start().await?;
    let response = client.request(request).await?;
    Ok(render(response, cli.json))
}

fn build_request(cli: &Cli) -> anyhow::Result<Request> {
    let session = cli.session.clone();
    Ok(match &cli.command {
        Command::Open { url, headed } => Request::Open {
            url: normalize_url(url),
            session,
            headless: !headed,
        },
        Command::Snapshot { all, .. } => Request::Snapshot {
            session,
            interactive: !all,
        },
        Command::Click { target, button, double, count, force } => {
            let target = as_target(target);
            Request::Click {
                session,
                target,
                button: button.clone(),
                count: if *double { 2 } else { *count },
                modifiers: 0,
                force: *force,
            }
        }
        Command::Hover { node_ref } => Request::Hover { session, node_ref: node_ref.clone() },
        Command::Fill { node_ref, text } => Request::Fill {
            session,
            node_ref: node_ref.clone(),
            text: text.clone(),
        },
        Command::Type { text, by_key } => Request::Type {
            session,
            text: text.clone(),
            by_key: *by_key,
        },
        Command::Press { chord } => Request::Press { session, chord: chord.clone() },
        Command::Scroll { direction, amount } => {
            let (dx, dy) = match direction.as_str() {
                "up" => (0.0, -amount),
                "down" => (0.0, *amount),
                "left" => (-amount, 0.0),
                _ => (*amount, 0.0),
            };
            Request::Scroll { session, dx, dy }
        }
        Command::Screenshot { full_page, node, rect, out, format, quality } => {
            let target = if *full_page {
                ShotTarget::FullPage
            } else if let Some(node_ref) = node {
                ShotTarget::Node { node_ref: node_ref.clone() }
            } else if let Some(rect) = rect {
                let (x, y, width, height) = parse_rect(rect).ok_or_else(|| {
                    anyhow::anyhow!("--rect wants x,y,width,height (e.g. 100,200,600,400)")
                })?;
                ShotTarget::Rect { x, y, width, height }
            } else {
                ShotTarget::Viewport
            };
            Request::Screenshot {
                session,
                target,
                format: format.clone(),
                quality: *quality,
                out: out.clone(),
            }
        }
        Command::Eval { expression, mutate } => Request::Eval {
            session,
            expression: expression.clone(),
            mutate: *mutate,
        },
        Command::Console { errors, limit } => Request::Console {
            session,
            errors: *errors,
            limit: *limit,
        },
        Command::Network { failed, limit } => Request::Network {
            session,
            failed: *failed,
            limit: *limit,
        },
        Command::Tap { target } => Request::Tap {
            session,
            target: as_target(target),
        },
        Command::LongPress { target, duration_ms } => Request::LongPress {
            session,
            target: as_target(target),
            duration_ms: *duration_ms,
        },
        Command::Swipe { from, to, duration_ms, steps } => Request::Swipe {
            session,
            from: as_target(from),
            to: as_target(to),
            duration_ms: *duration_ms,
            steps: *steps,
        },
        Command::Pinch { center, scale, speed } => Request::Pinch {
            session,
            center: as_target(center),
            scale: *scale,
            speed: *speed,
        },
        Command::Drag { from, to, duration_ms, steps } => Request::Drag {
            session,
            from: as_target(from),
            to: as_target(to),
            duration_ms: *duration_ms,
            steps: *steps,
        },
        Command::Close => Request::Close { session },
        Command::Job { action } => match action {
            JobAction::Start { intent, steps, headed } => Request::JobStart {
                intent: intent.clone(),
                steps: steps.clone(),
                headless: !headed,
            },
            JobAction::List => Request::JobList,
            JobAction::Status { id } => Request::JobStatus {
                id: id.clone(),
                log_from: 0,
            },
            JobAction::Answer { id, answer } => Request::JobAnswer {
                id: id.clone(),
                answer: answer.clone(),
            },
            JobAction::Approve { id, reject } => Request::JobApprove {
                id: id.clone(),
                reject: *reject,
            },
            JobAction::Stop { id } => Request::JobStop { id: id.clone() },
            JobAction::Logs { .. } => unreachable!("handled before this point"),
        },
        Command::Sessions => Request::Sessions,
        Command::Status => Request::Status,
        Command::Daemon { .. } => unreachable!("handled before this point"),
    })
}

/// A pointer target is a ref unless it parses as `x,y`.
fn as_target(raw: &str) -> Target {
    match parse_point(raw) {
        Some((x, y)) => Target::Point { x, y },
        None => Target::Ref { node_ref: raw.to_string() },
    }
}

/// Accepts `example.com` as well as a full URL, because agents and humans both
/// type the short form and a "protocol error" here is pure friction.
fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("file://")
        || trimmed.starts_with("about:")
        || trimmed.starts_with("data:")
    {
        return trimmed.to_string();
    }
    format!("https://{trimmed}")
}

fn render(response: Response, as_json: bool) -> std::process::ExitCode {
    match response {
        Response::Ok { data, text } => {
            if as_json {
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
            } else if let Some(text) = text {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Response::Error { message, hint } => {
            if as_json {
                let payload = json!({ "status": "error", "message": message, "hint": hint });
                println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            } else {
                eprintln!("brow: {message}");
                if let Some(hint) = hint {
                    eprintln!("  → {hint}");
                }
            }
            std::process::ExitCode::from(1)
        }
    }
}

/// Prints a job's log, optionally until it reaches a terminal state.
///
/// Stops on a park as well as on completion: a job waiting for a decision or an
/// approval is waiting for *the caller*, so blocking there would deadlock the
/// person who has to answer.
async fn follow_logs(
    id: &str,
    follow: bool,
    as_json: bool,
) -> anyhow::Result<std::process::ExitCode> {
    let mut client = Client::connect_or_start().await?;
    let mut cursor = 0usize;

    loop {
        let response = client
            .request(Request::JobStatus {
                id: id.to_string(),
                log_from: cursor,
            })
            .await?;

        let Response::Ok { data, .. } = &response else {
            return Ok(render(response, as_json));
        };

        if as_json {
            println!("{}", serde_json::to_string(&data).unwrap_or_default());
        } else {
            for line in data["log"].as_array().into_iter().flatten() {
                if let Some(text) = line["text"].as_str() {
                    println!("{text}");
                }
            }
        }
        cursor = data["log_total"].as_u64().unwrap_or(0) as usize;

        let terminal = data["terminal"].as_bool().unwrap_or(true);
        let parked = data["parked"].as_bool().unwrap_or(false);
        if !follow || terminal || parked {
            if !as_json && parked {
                // The reason it stopped following is the actionable part.
                if let Response::Ok { text: Some(text), .. } = &response {
                    print!("{text}");
                }
            }
            let failed = data["state"].as_str() == Some("failed");
            return Ok(if failed {
                std::process::ExitCode::from(1)
            } else {
                std::process::ExitCode::SUCCESS
            });
        }

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

async fn daemon_command(
    action: &DaemonAction,
    as_json: bool,
) -> anyhow::Result<std::process::ExitCode> {
    match action {
        DaemonAction::Serve => {
            daemon::serve().await?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        DaemonAction::Start => {
            if client::is_running().await {
                println!("browd is already running");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            let mut c = Client::connect_or_start().await?;
            let resp = c.request(Request::Status).await?;
            Ok(render(resp, as_json))
        }
        DaemonAction::Status => {
            if !client::is_running().await {
                if as_json {
                    println!("{}", json!({ "running": false }));
                } else {
                    println!("browd is not running");
                }
                // Not an error: "is it up?" answered truthfully is a success.
                return Ok(std::process::ExitCode::SUCCESS);
            }
            let mut c = Client::connect().await?;
            let resp = c.request(Request::Status).await?;
            Ok(render(resp, as_json))
        }
        DaemonAction::Stop => {
            if !client::is_running().await {
                println!("browd is not running");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            let mut c = Client::connect().await?;
            let resp = c.request(Request::Shutdown).await?;
            wait_for_socket_release().await;
            Ok(render(resp, as_json))
        }
        DaemonAction::Restart => {
            if client::is_running().await {
                let mut c = Client::connect().await?;
                let _ = c.request(Request::Shutdown).await;
                wait_for_socket_release().await;
            }
            let mut c = Client::connect_or_start().await?;
            let resp = c.request(Request::Status).await?;
            Ok(render(resp, as_json))
        }
    }
}

/// Waits for the old daemon to actually let go, so a restart cannot race it.
async fn wait_for_socket_release() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !paths::socket().exists() && !client::is_running().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("  example.com/a?b=1 "), "https://example.com/a?b=1");
    }

    #[test]
    fn real_urls_are_left_alone() {
        for url in [
            "http://localhost:8080/x",
            "https://example.com",
            "file:///tmp/a.html",
            "about:blank",
            "data:text/html,<p>hi",
        ] {
            assert_eq!(normalize_url(url), url);
        }
    }
}
