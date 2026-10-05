//! MCP lifecycle + transport boot: the `run`/`run_pipe` entry points, the
//! lazy Chrome launch, idle/shutdown handling, and first-run profile
//! warming. Split from the `server` core.

#[cfg(unix)]
use serde_json::json;

use crate::cdp::{self, CdpClient, CdpSession};
use crate::error::{BladeError, Result};
use crate::page::Page;

use super::serve::serve;

/// Run the MCP server over stdio (WS transport). Blocks until stdin closes.
/// Chrome is NOT launched here — it starts lazily on the first tool call.
pub async fn run(host: &str, port: u16) -> Result<()> {
    serve(false, host, port).await
}

/// Serve MCP over a zero-port CDP pipe connection (S1).
/// Chrome is NOT launched here — it starts lazily on the first tool call.
/// Unix-only: Windows uses WS transport.
#[cfg(unix)]
pub async fn run_pipe() -> Result<()> {
    serve(true, "", 0).await
}

/// Attach to the first page target over the browser-level pipe connection.
/// Creates a fresh tab when none exists (restored-session edge cases).
/// Unix-only: called by run_pipe which is also Unix-only.
#[cfg(unix)]
async fn attach_pipe(client: &CdpClient) -> Result<CdpSession> {
    let targets = client.send("Target.getTargets", None).await?;
    let empty = Vec::new();
    let infos = targets
        .get("targetInfos")
        .and_then(|t| t.as_array())
        .unwrap_or(&empty);
    let page_target = infos
        .iter()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .and_then(|t| t.get("targetId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let target_id = match page_target {
        Some(id) => id,
        None => {
            let res = client
                .send("Target.createTarget", Some(json!({ "url": "about:blank" })))
                .await?;
            res.get("targetId")
                .and_then(|v| v.as_str())
                .ok_or(BladeError::NoTarget)?
                .to_string()
        }
    };

    let res = client
        .send(
            "Target.attachToTarget",
            Some(json!({ "targetId": target_id, "flatten": true })),
        )
        .await?;
    let session_id = res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BladeError::Other("Target.attachToTarget returned no sessionId".into()))?;
    Ok(CdpSession::child(client.clone(), session_id))
}

/// Idle timeout: Chrome is shut down after this many seconds of no tool
/// calls, freeing RAM. 0 disables. Default: 600 (10 minutes).
/// Configurable via `BLADE_IDLE_TIMEOUT` env var (seconds).
pub(super) fn idle_timeout_secs() -> u64 {
    std::env::var("BLADE_IDLE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(600)
}

/// Launch Chrome and create a fresh `Page`. Used for three purposes:
/// 1. Lazy init: first tool call in the MCP session.
/// 2. Self-healing: Chrome crashed, relaunch before retrying.
/// 3. Post-idle: Chrome was shut down after inactivity, relaunch on demand.
///
/// The caller is responsible for dropping the old `Browser` (if any)
/// before calling this, to kill the old Chrome process.
pub(super) async fn launch_browser(
    use_pipe: bool,
    host: &str,
    port: u16,
) -> Result<(Page, Option<crate::browser::Browser>)> {
    // Re-read the lane at every launch: `rb on|off` (or a hand-edited
    // config) takes effect on the next launch, even inside a long-lived
    // MCP session.
    crate::realbrowser::init_lane();
    // Real-browser lane (S18): never the pipe transport (it flips
    // navigator.webdriver) and never the isolated agent browser — this lane
    // launches or attaches the user's own browser (see `launch_lane`).
    if crate::realbrowser::real_lane() && port == 0 {
        let (browser, base) = crate::browser::launch_lane().await?;
        let result = async {
            let target = cdp::first_page_target(&base).await?;
            let client = CdpClient::connect(target.ws_url()?).await?;
            let page = Page::attach(CdpSession::root(client), &base, None).await?;
            Ok(page)
        }
        .await;
        return match result {
            Ok(page) => Ok((page, browser)),
            Err(e) => {
                if let Some(b) = browser {
                    let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                }
                Err(e)
            }
        };
    }
    if use_pipe {
        #[cfg(unix)]
        {
            let (browser, client) = crate::browser::Browser::launch_pipe().await?;
            let result = async {
                let session = attach_pipe(&client).await?;
                let page = Page::attach(session, "pipe", Some(client)).await?;
                Ok(page)
            }
            .await;
            match result {
                Ok(page) => {
                    // Re-inject saved logins before anything navigates.
                    let _ = crate::logins::restore(page.cdp_ref()).await;
                    return Ok((page, Some(browser)));
                }
                Err(e) => {
                    let _ = tokio::task::spawn_blocking(move || browser.shutdown()).await;
                    return Err(e);
                }
            }
        }
        #[cfg(not(unix))]
        {
            return Err(BladeError::Other("pipe transport is Unix-only".into()));
        }
    }

    // WS transport.
    if port == 0 {
        // Auto-launch: pick a free port.
        let browser = crate::browser::Browser::launch(0).await?;
        let base = browser.base();
        let result = async {
            let target = cdp::first_page_target(&base).await?;
            let client = CdpClient::connect(target.ws_url()?).await?;
            let page = Page::attach(CdpSession::root(client), &base, None).await?;
            Ok(page)
        }
        .await;
        match result {
            Ok(page) => {
                // Re-inject saved logins before anything navigates.
                let _ = crate::logins::restore(page.cdp_ref()).await;
                Ok((page, Some(browser)))
            }
            Err(e) => {
                // Clean up the browser we just launched.
                let _ = tokio::task::spawn_blocking(move || browser.shutdown()).await;
                Err(e)
            }
        }
    } else {
        // Connect to an existing Chrome on the given port.
        let base = format!("{host}:{port}");
        let target = cdp::first_page_target(&base).await?;
        let client = CdpClient::connect(target.ws_url()?).await?;
        let page = Page::attach(CdpSession::root(client), &base, None).await?;
        // Re-inject saved logins before anything navigates. Never on the
        // real lane: those are the user's own cookies in there.
        if !crate::realbrowser::real_lane() {
            let _ = crate::logins::restore(page.cdp_ref()).await;
        }
        Ok((page, None))
    }
}

/// Shut down Chrome without blocking the async executor:
/// Browser::drop sends SIGTERM and waits up to 3s
/// synchronously — on the executor thread that stalls every
/// other task. Offload to a blocking thread.
pub(super) async fn shutdown_browser(b: crate::browser::Browser) {
    let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
}

/// Wait for a termination signal (SIGTERM/SIGINT/SIGHUP on
/// Unix, Ctrl+C on Windows). Returns when the process should
/// shut down gracefully. OpenCode and other harnesses kill
/// MCP servers with SIGTERM — without this, Chrome + Xvfb
/// are orphaned every time a session ends.
pub(super) async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut int = signal(SignalKind::interrupt()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        tokio::select! {
            _ = async { if let Some(s) = &mut term { s.recv().await } else { std::future::pending().await } } => {}
            _ = async { if let Some(s) = &mut int { s.recv().await } else { std::future::pending().await } } => {}
            _ = async { if let Some(s) = &mut hup { s.recv().await } else { std::future::pending().await } } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Warm the profile on first run: navigate to a few top sites to seed
/// cache, cookies, HSTS, and browsing history. Only runs once (when
/// `~/.blade/.warmed` doesn't exist). Best-effort: failed navigations are
/// skipped, never fatal. Total time: ~4-6s.
pub(super) async fn warm_profile(page: &mut Page) {
    let sites = [
        "https://www.google.com",
        "https://github.com",
        "https://www.wikipedia.org",
    ];
    let mut ok = 0;
    for url in &sites {
        // Navigate with a short timeout — if a site is unreachable,
        // skip it. Don't let warming block the agent's first action.
        match tokio::time::timeout(std::time::Duration::from_secs(4), page.navigate(url)).await {
            Ok(Ok(_)) => {
                ok += 1;
                // Brief pause to let cookies/cache settle.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            _ => continue, // timeout or error — skip this site
        }
    }
    if ok > 0 {
        eprintln!(
            "[bladebro] profile warmed ({ok}/{} sites visited)",
            sites.len()
        );
    } else {
        eprintln!("[bladebro] WARNING: profile warming failed (all sites unreachable)");
        crate::session_profile::SessionProfile::release_warming();
    }
}
