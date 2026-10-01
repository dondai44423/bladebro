//! The MCP server — stdio JSON-RPC 2.0 loop.
//!
//! The server holds one [`Page`](crate::page::Page) (CDP connection + LPM) for
//! the lifetime of the session. It reads newline-delimited JSON-RPC messages
//! from stdin, dispatches to the appropriate tool, and writes responses to
//! stdout. stderr is for logging only — nothing else goes to stdout.
//!
//! Protocol: dual-dialect. Legacy clients (≤2025-11-25) use the
//! `initialize` handshake; the 2026-07-28 stateless revision (SEP-2575)
//! carries the protocol version per-request in
//! `_meta["io.modelcontextprotocol/protocolVersion"]` and discovers
//! capabilities via `server/discover`. Both are supported; the dialect
//! is negotiated per request. Mismatched versions get
//! `UnsupportedProtocolVersionError` (-32022).
//!
//! Module map: this file is the protocol loop + lifecycle (run/serve, lazy
//! launch, self-healing, tool dispatch) and re-exports the handlers; they
//! live in `act`/`see`/`state`/`run`/`vision`, with shared pieces in `eval`
//! (JS), `extract` (template/auto/collect), `files` (pdf/download), and
//! `resolve` (text/selector → ref).

use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};

use serde_json::{json, Value};


use crate::cdp::{self, CdpClient, CdpSession};
use crate::error::{BladeError, Result};
use crate::mcp::tools::tools_to_json;
use crate::page::Page;

mod act;
mod eval;
mod extract;
mod files;
mod resolve;
mod run;
mod see;
mod state;
mod vision;

pub use act::{handle_act, handle_fill};
pub use run::handle_run;
pub use see::handle_see;
pub use state::handle_state;
pub use vision::handle_vision;

/// Default protocol version for legacy clients that don't negotiate.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The 2026-07-28 stateless revision (SEP-2575). Requests carrying this
/// version in `_meta` get new-dialect results: `resultType` (SEP-2322),
/// server identity in `_meta`, and cache hints on list endpoints.
const STATELESS_VERSION: &str = "2026-07-28";

/// All protocol versions this server speaks. Legacy versions behave
/// identically for the methods we implement; the stateless revision
/// changes result shaping only.
const SUPPORTED_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    STATELESS_VERSION,
];

/// Server instructions shared by `initialize` and `server/discover`.
const INSTRUCTIONS: &str = "Stealth browser driver. `see` reads the page (diff-first), `act` interacts (click/type/navigate), `state` manages cookies/tabs/sessions, `run` runs step batches with branching/loops/inline reads, `vision` screenshots.";

/// Extract the per-request protocol version (SEP-2575). New-spec clients
/// send `_meta["io.modelcontextprotocol/protocolVersion"]` on every
/// request; legacy clients omit it and get the legacy dialect.
/// Err carries the unsupported version string.
fn request_version(params: &Value) -> std::result::Result<Option<&'static str>, String> {
    let v = params
        .get("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(|v| v.as_str());
    match v {
        None => Ok(None),
        Some(v) => match SUPPORTED_VERSIONS.iter().copied().find(|s| *s == v) {
            Some(s) => Ok(Some(s)),
            None => Err(v.to_string()),
        },
    }
}

/// Add 2026-07-28 dialect fields to a result payload: `resultType`
/// (required by SEP-2322) and server identity in `_meta` (SEP-2575).
fn shape_result(result: &mut Value, version: Option<&str>) {
    if version != Some(STATELESS_VERSION) {
        return;
    }
    if let Some(obj) = result.as_object_mut() {
        obj.entry("resultType").or_insert(json!("complete"));
        obj.entry("_meta").or_insert(json!({
            "io.modelcontextprotocol/serverInfo": {
                "name": "bladebro",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }));
    }
}

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
fn idle_timeout_secs() -> u64 {
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
async fn launch_browser(
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
            }.await;
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
            return Err(BladeError::Other(
                "pipe transport is Unix-only".into(),
            ));
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
        }.await;
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
async fn shutdown_browser(b: crate::browser::Browser) {
    let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
}

/// Wait for a termination signal (SIGTERM/SIGINT/SIGHUP on
/// Unix, Ctrl+C on Windows). Returns when the process should
/// shut down gracefully. OpenCode and other harnesses kill
/// MCP servers with SIGTERM — without this, Chrome + Xvfb
/// are orphaned every time a session ends.
async fn wait_for_shutdown_signal() {
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
async fn warm_profile(page: &mut Page) {
    let sites = [
        "https://www.google.com",
        "https://github.com",
        "https://www.wikipedia.org",
    ];
    let mut ok = 0;
    for url in &sites {
        // Navigate with a short timeout — if a site is unreachable,
        // skip it. Don't let warming block the agent's first action.
        match tokio::time::timeout(
            std::time::Duration::from_secs(4),
            page.navigate(url),
        ).await {
            Ok(Ok(_)) => {
                ok += 1;
                // Brief pause to let cookies/cache settle.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            _ => continue, // timeout or error — skip this site
        }
    }
    if ok > 0 {
        eprintln!("[bladebro] profile warmed ({ok}/{} sites visited)", sites.len());
    } else {
        eprintln!("[bladebro] WARNING: profile warming failed (all sites unreachable)");
        crate::session_profile::SessionProfile::release_warming();
    }
}

/// The stdio JSON-RPC loop, shared by both transports.
///
/// Chrome is NOT launched at startup. The server starts with no browser
/// process, using minimal RAM. Chrome launches lazily on the first
/// `tools/call` and shuts down after `idle_timeout_secs()` of inactivity.
/// Only `tools/call` needs Chrome; `initialize`, `tools/list`, etc. are
/// static metadata and never trigger a launch.
///
/// Self-healing: if Chrome crashes mid-session, the next `tools/call`
/// detects the dead connection, relaunches Chrome, and retries the call.
/// The agent never sees "browser connection closed".
///
/// Lifecycle guarantees (the reliability contract):
/// - stdin EOF (client gone) → Chrome shut down, profile synced, exit.
/// - SIGTERM/SIGINT/SIGHUP → same graceful teardown.
/// - SIGKILL/panic → the next launch's orphan reaper kills
///   the leaked Chrome/Xvfb and removes the session profile.
/// - A second bladebro NEVER touches this session's Chrome:
///   profiles are per-process (`~/.blade/profiles/sess-<pid>`).
async fn serve(
    use_pipe: bool,
    host: &str,
    port: u16,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut browser: Option<crate::browser::Browser> = None;
    let mut page: Option<Page> = None;
    let mut last_activity = std::time::Instant::now();
    let idle_secs = idle_timeout_secs();
    let mut idle_check = tokio::time::interval(std::time::Duration::from_secs(15));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Periodic profile sync-back: every 60s while Chrome is alive,
    // sync the session profile to the template. SIGKILL resilience —
    // even a force-kill only loses up to 60s of state.
    let mut last_sync = std::time::Instant::now();
    let sync_interval = std::time::Duration::from_secs(60);
    // Set when Chrome is relaunched after a crash/idle — the
    // next response tells the agent its page state was reset.
    let mut relaunch_note: Option<String> = None;
    // Fingerprint of the launch inputs the live browser started with; the
    // per-call drift check compares against it so `rb on|off` under a running
    // browser relaunches instead of being silently ignored.
    let mut launched = 0u64;
    // One-shot latch: has the stale-binary advisory been delivered?
    let mut stale_warned = false;
    // One-shot latch: has the GL-less advisory been delivered?
    let mut gl_warned = false;
    // Track resource-blocking config so it survives idle shutdown/relaunch.
    let mut block_classes: Option<String> = None;
    // Domain knowledge base: consent selectors, visit tracking, stats.
    // Loaded once at startup, synced to disk periodically + on shutdown.
    let knowledge = crate::knowledge::load_shared();

    // No startup banner — keep stderr clean for MCP harnesses that
    // forward stderr to the agent TUI.
    // Chrome launches lazily on the first tool call.

    loop {
        tokio::select! {
            _ = wait_for_shutdown_signal() => {
                eprintln!("[bladebro] termination signal — shutting down Chrome gracefully");
                break;
            }
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("[bladebro] stdin read error: {e}");
                        break;
                    }
                };

                if line.trim().is_empty() { continue; }

                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        // JSON-RPC 2.0 §5.1: a parse error gets a -32700
                        // response with id null. The old silent-drop left a
                        // client whose request was truncated mid-write
                        // waiting for a response forever.
                        eprintln!("[bladebro] invalid JSON: {e}");
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": null,
                            "error": { "code": -32700, "message": format!("Parse error: {e}") }
                        });
                        let resp_str = serde_json::to_string(&resp)?;
                        writeln!(out, "{resp_str}")?;
                        out.flush()?;
                        continue;
                    }
                };

                // §5.1: non-object requests (arrays, strings, numbers) get
                // -32600 Invalid Request — not "method not found".
                if !msg.is_object() || !msg.get("method").map(|m| m.is_string()).unwrap_or(false) {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": if msg.is_object() { msg.get("id").cloned() } else { None },
                        "error": { "code": -32600, "message": "Invalid Request: expected an object with a string 'method'" }
                    });
                    let resp_str = serde_json::to_string(&resp)?;
                    writeln!(out, "{resp_str}")?;
                    out.flush()?;
                    continue;
                }

                let id = msg.get("id").cloned();
                // §4.4: notifications (no id) never get a response.
                // Captured before `id` is consumed by the handlers.
                let is_notification = id.is_none();
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let params = msg.get("params").cloned().unwrap_or(Value::Null);

                // Per-request version negotiation (SEP-2575).
                let version = match request_version(&params) {
                    Ok(v) => v,
                    Err(bad) => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32022,
                                "message": format!("unsupported protocol version: {bad}"),
                                "data": { "supportedVersions": SUPPORTED_VERSIONS },
                            }
                        });
                        let resp_str = serde_json::to_string(&resp)?;
                        writeln!(out, "{resp_str}")?;
                        out.flush()?;
                        continue;
                    }
                };

                let response = match method {
                    "initialize" => Some(handle_initialize(id, &params)),
                    "initialized" | "notifications/initialized" => None,
                    "server/discover" => Some(handle_discover(id, version)),
                    "tools/list" => Some(handle_tools_list(id, version)),
                    "tools/call" => {
                        // === LAZY LAUNCH + SELF-HEAL ===
                        // Ensure Chrome is running before any tool call.
                        // Cases: first call (page=None), idle shutdown
                        // (page=None), Chrome crashed (is_closed), or the
                        // lane/launch settings changed under a live session —
                        // an owned browser (relaunch) or an ATTACH session
                        // (page alive, no owned browser: detach + relaunch).
                        //
                        // The drift check re-reads the lane + config every
                        // call: `rb on|off` (and `rb mode|use|profile|visible`)
                        // must take effect on a long-lived MCP session, whose
                        // browser would otherwise keep the old lane until it
                        // happens to die — the observed "rb on does nothing".
                        let lane_switched = crate::realbrowser::refresh_lane();
                        let session_live = browser.is_some()
                            || page.as_ref().map(|p| !p.is_closed()).unwrap_or(false);
                        let drifted = crate::realbrowser::session_drifted(
                            session_live,
                            lane_switched,
                            crate::realbrowser::launch_fingerprint(),
                            launched,
                        );
                        let need_launch = drifted
                            || page.is_none()
                            || page.as_ref().map(|p| p.is_closed()).unwrap_or(true);
                        if need_launch {
                            if browser.is_some() {
                                if drifted {
                                    eprintln!(
                                        "[bladebro] lane/settings changed — relaunching Chrome"
                                    );
                                    if let Some(b) = browser.take() {
                                        shutdown_browser(b).await;
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                    relaunch_note = Some(if crate::realbrowser::real_lane() {
                                        "note: the real-browser lane is now ON — Chrome was relaunched as the user's own browser (their profile data; page patches off). Page state reset to about:blank; navigate to continue.".into()
                                    } else {
                                        "note: the real-browser lane is now OFF — Chrome was relaunched as the isolated agent browser. Page state reset to about:blank; navigate to continue.".into()
                                    });
                                } else {
                                    // Chrome crashed or is dead, kill it first.
                                    eprintln!("[bladebro] browser connection lost, relaunching...");
                                    if let Some(b) = browser.take() {
                                        shutdown_browser(b).await;
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                    relaunch_note = Some(
                                        "note: Chrome was restarted (connection lost) — page state reset to about:blank. Navigate to continue.".into()
                                    );
                                }
                            } else if drifted {
                                // Attach session (never owned): `rb off` /
                                // settings change must DETACH — drop the
                                // attached page; the user's browser is not
                                // ours to close. The launch below opens the
                                // browser the current lane/settings call for.
                                eprintln!(
                                    "[bladebro] lane/settings changed — detaching from the attached browser"
                                );
                                page = None;
                                relaunch_note = Some(if crate::realbrowser::real_lane() {
                                    "note: the real-browser lane settings changed — bladebro detached from the attached browser (left running, untouched) and reopened per the current settings. Page state reset to about:blank; navigate to continue.".into()
                                } else {
                                    "note: the real-browser lane is now OFF — bladebro detached from the attached browser (left running, untouched) and relaunched as the isolated agent browser. Page state reset to about:blank; navigate to continue.".into()
                                });
                            } else if page.is_none() && relaunch_note.is_none() && last_activity.elapsed().as_secs() > idle_secs && idle_secs > 0 {
                                // Post-idle relaunch: the agent's refs
                                // are all gone. Say so explicitly.
                                relaunch_note = Some(
                                    "note: Chrome was restarted after idle shutdown — page state reset to about:blank. Navigate to continue.".into()
                                );
                            } else {
                                eprintln!("[bladebro] launching Chrome (first tool call)...");
                            }
                            match launch_browser(use_pipe, host, port).await {
                                Ok((new_page, new_browser)) => {
                                    browser = new_browser;
                                    page = Some(new_page);
                                    launched = crate::realbrowser::launch_fingerprint();
                                    if crate::realbrowser::real_lane() && relaunch_note.is_none() {
                                        relaunch_note = Some(
                                            "note: real-browser lane — this Chrome is the user's own browser (their profile data; page patches off).".into()
                                        );
                                    }
                                    // Set knowledge base on the new page.
                                    if let Some(ref mut p) = page {
                                        p.set_knowledge(knowledge.clone());
                                    }
                                    // Restore resource blocking after relaunch.
                                    if let Some(ref bc) = block_classes {
                                        if let Some(ref mut p) = page {
                                            let _ = p.set_block_classes(bc).await;
                                        }
                                    }
                                    // First-run warming: seed cache/cookies/HSTS
                                    // by visiting a few top sites. Only runs once.
                                    // The note explains WHY the first tool call
                                    // took ~10s before the agent's request ran.
                                    if !crate::realbrowser::real_lane()
                                        && crate::session_profile::SessionProfile::claim_warming()
                                    {
                                        if let Some(ref mut p) = page {
                                            warm_profile(p).await;
                                        }
                                        // Compose with any existing note (e.g. a
                                        // drift relaunch onto this lane) instead
                                        // of clobbering it — the agent must see
                                        // BOTH the lane switch and the warming.
                                        let warm = "note: first run — the profile was warmed (google.com, github.com, wikipedia.org visited to seed cookies/HSTS/history) before this call.";
                                        relaunch_note = Some(match relaunch_note.take() {
                                            Some(prev) => format!("{prev}\n{warm}"),
                                            None => warm.to_string(),
                                        });
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[bladebro] Chrome launch failed: {e}");
                                    let resp = json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "content": [{ "type": "text", "text":
                                                format!("\u{2717} Could not launch Chrome: {e}. Check that Chromium is installed and try again.") }],
                                            "isError": true,
                                        }
                                    });
                                    let resp_str = serde_json::to_string(&resp)?;
                                    writeln!(out, "{resp_str}")?;
                                    out.flush()?;
                                    continue;
                                }
                            }
                        }

                        // === CALL THE TOOL ===
                        // Clone id for retry paths — handle_tools_call consumes it.
                        let id_retry = id.clone();
                        let _hc_t = std::time::Instant::now();
                        let res = {
                            let p = page.as_mut().unwrap();
                            futures_util::FutureExt::catch_unwind(
                                std::panic::AssertUnwindSafe(handle_tools_call(id, &params, p)),
                            ).await
                        };
                        if std::env::var("NAV_TIMING").is_ok() {
                            eprintln!("[nav-timing] handle_tools_call total: {:?}", _hc_t.elapsed());
                        }

                        // handle_tools_call returns Result<Value, BladeError>:
                        // Ok(Value) = normal response. Err(Closed) = browser
                        // died during the call, need to relaunch + retry.
                        let resp = match res {
                            Ok(Ok(v)) => v,
                            Ok(Err(BladeError::Closed)) => {
                                // Self-heal: relaunch, then retry — but ONLY
                                // idempotent reads. Blindly re-running a
                                // click/type/submit after a crash can
                                // double-fire the action (double purchase,
                                // duplicate message): the first dispatch may
                                // have landed before the connection died.
                                let retry_safe = {
                                    let tn = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                    tn == "see"
                                        || (tn == "state" && matches!(
                                            params.get("arguments")
                                                .and_then(|a| a.get("op"))
                                                .and_then(|o| o.as_str()),
                                            Some("cookies") | Some("ls") | Some("ss") | Some("tabs")
                                        ))
                                };
                                eprintln!("[bladebro] browser closed during tool call, reconnecting...");
                                if let Some(b) = browser.take() {
                                    shutdown_browser(b).await;
                                }
                                page = None;
                                match launch_browser(use_pipe, host, port).await {
                                    Ok((new_page, new_browser)) => {
                                        browser = new_browser;
                                        page = Some(new_page);
                                        // Set knowledge base on the new page.
                                        if let Some(ref mut p) = page {
                                            p.set_knowledge(knowledge.clone());
                                        }
                                        // Restore resource blocking after relaunch.
                                        if let Some(ref bc) = block_classes {
                                            if let Some(ref mut p) = page {
                                                let _ = p.set_block_classes(bc).await;
                                            }
                                        }
                                        if retry_safe {
                                            relaunch_note = Some(
                                                "note: Chrome crashed and was restarted — page state reset to about:blank. Navigate to continue.".into()
                                            );
                                            let id_retry2 = id_retry.clone();
                                            let p = page.as_mut().unwrap();
                                            match handle_tools_call(id_retry2, &params, p).await {
                                                Ok(v) => v,
                                                Err(e) => {
                                                    eprintln!("[bladebro] retry after reconnect failed: {e}");
                                                    json!({
                                                        "jsonrpc": "2.0",
                                                        "id": id_retry,
                                                        "result": {
                                                            "content": [{ "type": "text", "text":
                                                                format!("\u{2717} Browser connection lost. Bladebro reconnected but the retry failed: {e}. Try the tool call again.") }],
                                                            "isError": true,
                                                        }
                                                    })
                                                }
                                            }
                                        } else {
                                            json!({
                                                "jsonrpc": "2.0",
                                                "id": id_retry,
                                                "result": {
                                                    "content": [{ "type": "text", "text":
                                                        "\u{2717} Browser connection was lost mid-action. Chrome has been restarted (page reset to about:blank). The action's outcome is UNKNOWN — it may have taken effect before the crash. Re-issue the action (and verify the result) rather than assuming it failed.".to_string() }],
                                                    "isError": true,
                                                }
                                            })
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("[bladebro] reconnect failed: {e}");
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id_retry,
                                            "result": {
                                                "content": [{ "type": "text", "text":
                                                    format!("\u{2717} Browser connection lost. Bladebro tried to reconnect but failed: {e}. The server is still running, try again in a moment.") }],
                                                "isError": true,
                                            }
                                        })
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                // Dead-tab recovery: the attached tab was
                                // closed externally (window.close, site
                                // nav). CDP reports it as a plain error,
                                // not Closed — detect, open a fresh tab,
                                // switch, and retry ONCE.
                                let msg = e.to_string();
                                let tab_died = msg.contains("Target closed")
                                    || msg.contains("No target with given id")
                                    || msg.contains("Session closed")
                                    || msg.contains("Target.detachedFromTarget");
                                if tab_died {
                                    eprintln!("[bladebro] attached tab died, opening a fresh tab...");
                                    let p = page.as_mut().unwrap();
                                    match recover_dead_tab(p).await {
                                        Ok(()) => {
                                            let p = page.as_mut().unwrap();
                                            match handle_tools_call(id_retry.clone(), &params, p).await {
                                                Ok(v) => v,
                                                Err(e2) => json!({
                                                    "jsonrpc": "2.0",
                                                    "id": id_retry,
                                                    "error": { "code": -32603, "message": e2.to_string() }
                                                }),
                                            }
                                        }
                                        Err(re) => json!({
                                            "jsonrpc": "2.0",
                                            "id": id_retry,
                                            "error": { "code": -32603, "message": format!("{msg} (tab recovery failed: {re})") }
                                        }),
                                    }
                                } else {
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": id_retry,
                                        "error": {
                                            "code": -32603,
                                            "message": msg,
                                        }
                                    })
                                }
                            }
                            Err(_) => json!({
                                "jsonrpc": "2.0",
                                "id": id_retry,
                                "error": {
                                    "code": -32603,
                                    "message": "internal panic in tool handler (see stderr) — session survived, retry or re-see",
                                }
                            }),
                        };
                        let mut resp = resp;
                        if let Some(result) = resp.get_mut("result") {
                            // Prepend advisory notes to the first text content
                            // block: a relaunch reset the page state, and/or
                            // this process runs a replaced binary (the fix is
                            // in the file on disk, not in the running process).
                            // One-time WebGL advisory: a GL-less browser is
                            // stock-equivalent, but the agent must know the GL
                            // mask is off (nothing to mask) instead of assuming
                            // the environment was spoofed.
                            if !gl_warned {
                                if let Some(crate::browser::GpuState::Missing) = crate::browser::gpu_state() {
                                    gl_warned = true;
                                    if relaunch_note.is_none() {
                                        relaunch_note = Some(
                                            "note: this browser has no WebGL (getContext('webgl') returns null — the same as stock Chrome on this host); no GL mask is applied.".into()
                                        );
                                    }
                                }
                            }
                            if !stale_warned && crate::platform::stale_binary() {
                                stale_warned = true;
                                if relaunch_note.is_none() {
                                    relaunch_note = Some(
                                        "note: this MCP process runs a binary that was replaced on disk — restart the app that spawned it (e.g. opencode) to pick up the new build; this session keeps working meanwhile.".into()
                                    );
                                }
                            }
                            // Prepend the relaunch note to the first
                            // text content block so the agent knows
                            // its page state was reset.
                            if let Some(note) = relaunch_note.take() {
                                if let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut()) {
                                    content.insert(0, json!({ "type": "text", "text": note }));
                                }
                            }
                            shape_result(result, version);
                        }
                        last_activity = std::time::Instant::now();
                        // Track block config for restoration after relaunch.
                        // Always sync from current page state so changes are captured.
                        {
                            let rules = page.as_ref().map(|p| p.block_rules()).unwrap_or(0);
                            if rules != 0 {
                                let mut classes = Vec::new();
                                if rules & 1 != 0 { classes.push("images"); }
                                if rules & 2 != 0 { classes.push("fonts"); }
                                if rules & 4 != 0 { classes.push("media"); }
                                if rules & 8 != 0 { classes.push("trackers"); }
                                block_classes = Some(classes.join(","));
                            } else {
                                block_classes = None;
                            }
                        }
                        Some(resp)
                    }
                    // Removed in 2026-07-28; kept for legacy keepalive clients.
                    "ping" => {
                        let mut result = json!({});
                        shape_result(&mut result, version);
                        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                    }
                    _ => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("method not found: {method}"),
                        }
                    })),
                };

                if let Some(resp) = response {
                    // §4.4: notifications (no id) NEVER get a response —
                    // the old code replied with id:null, which strict
                    // clients treat as a broken peer.
                    if is_notification {
                        continue;
                    }
                    let resp_str = serde_json::to_string(&resp)?;
                    writeln!(out, "{resp_str}")?;
                    out.flush()?;
                }
            }
            _ = idle_check.tick() => {
                // Periodic login snapshot for crash/power-loss resilience.
                // We persist the authoritative live cookie store (CDP), never
                // a hot copy of the on-disk profile: copying a live Chrome
                // profile tears its SQLite and you come back logged out.
                if browser.is_some() && last_sync.elapsed() >= sync_interval {
                    if let Some(ref p) = page {
                        if !p.cdp_ref().is_closed() && !crate::realbrowser::real_lane() {
                            let _ = crate::logins::snapshot(p.cdp_ref()).await;
                        }
                    }
                    // Sync knowledge base to disk (prune + write).
                    {
                        let kb = knowledge.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Ok(mut kb) = kb.lock() {
                                kb.prune();
                                kb.sync();
                            }
                        }).await;
                    }
                    last_sync = std::time::Instant::now();
                }
                if idle_secs > 0
                    && browser.is_some()
                    && last_activity.elapsed().as_secs() > idle_secs
                    && crate::realbrowser::should_idle_shutdown()
                {
                    eprintln!(
                        "[bladebro] idle timeout ({}s), shutting down Chrome to save memory",
                        idle_secs
                    );
                    if let Some(b) = browser.take() {
                        // Persist live logins before killing Chrome so nothing
                        // is lost between the last periodic snapshot and exit.
                        // (Never on the real lane — not our cookie store.)
                        if let Some(ref p) = page {
                            if !crate::realbrowser::real_lane() {
                                let _ = crate::logins::snapshot(p.cdp_ref()).await;
                            }
                        }
                        shutdown_browser(b).await;
                    }
                    page = None;
                }
            }
        }
    }

    // Clean up on exit: persist live logins, then kill Chrome gracefully
    // (flushes the session profile back to the template), abort page tasks.
    if let Some(b) = browser.take() {
        // Persist live logins before killing Chrome.
        // (Never on the real lane — not our cookie store.)
        if let Some(ref p) = page {
            if !crate::realbrowser::real_lane() {
                let _ = crate::logins::snapshot(p.cdp_ref()).await;
            }
        }
        shutdown_browser(b).await;
    }
    drop(page);
    // Sync knowledge base to disk on shutdown.
    if let Ok(mut kb) = knowledge.lock() {
        kb.prune();
        kb.sync();
    }
    Ok(())
}

/// Recover when the attached tab was closed externally:
/// create a fresh tab (browser-level — the page session may be dead,
/// which is exactly why we're here) and switch the session to it. The
/// retry then acts on a live page instead of erroring "Target closed"
/// forever. v3.9: this used to send Target.createTarget THROUGH the dead
/// page session, which cannot work — recovery was advertised but broken.
async fn recover_dead_tab(page: &mut Page) -> Result<()> {
    let new_id = page.open_tab_target("about:blank").await?;
    page.switch_tab(&new_id).await
}

fn handle_initialize(id: Option<Value>, params: &Value) -> Value {
    // Legacy handshake (removed in 2026-07-28 but old clients require
    // it). Negotiate: echo the client's version when we support it,
    // fall back to our default otherwise — the client decides whether
    // to continue with the offered version.
    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    let negotiated = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": negotiated,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": "bladebro",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        }
    })
}

/// `server/discover` — capability advertisement for the 2026-07-28
/// stateless revision (SEP-2575). Servers MUST implement this; new
/// clients may call it instead of the removed `initialize` handshake.
fn handle_discover(id: Option<Value>, version: Option<&str>) -> Value {
    let mut result = json!({
        "supportedVersions": SUPPORTED_VERSIONS,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "instructions": INSTRUCTIONS,
        "ttlMs": 3_600_000,
        "cacheScope": "public",
    });
    shape_result(&mut result, version);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn handle_tools_list(id: Option<Value>, version: Option<&str>) -> Value {
    let mut result = json!({
        "tools": tools_to_json(),
    });
    // CacheableResult (SEP-2549): the tool list is static per binary,
    // so cache aggressively. Tools are returned in a deterministic
    // order for client-side caching and prompt cache hits.
    if version == Some(STATELESS_VERSION) {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("ttlMs".into(), json!(3_600_000));
            obj.insert("cacheScope".into(), json!("public"));
        }
    }
    shape_result(&mut result, version);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub async fn handle_tools_call(
    id: Option<Value>,
    params: &Value,
    page: &mut Page,
) -> std::result::Result<Value, BladeError> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // Vision tool returns an image content block, not text.
    if name == "vision" {
        return handle_vision(id, &args, page).await;
    }

    let result = match name {
        "act" => handle_act(&args, page).await,
        "see" => handle_see(&args, page).await,
        "state" => handle_state(&args, page).await,
        "run" => handle_run(&args, page).await,
        _ => Err(crate::error::BladeError::Other(format!("unknown tool: {name}"))),
    };

    match result {
        Ok(mut text) => {
            // Drain any dialogs that were auto-dismissed during this call.
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\u{26a0} dialogs auto-dismissed:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    text.push_str(&format!("  {} \"{}\" \u{2014} {}\n", d.kind, d.message, action));
                    if let Some(p) = &d.default_prompt {
                        if !p.is_empty() {
                            text.push_str(&format!("    (prompt default: \"{}\")\n", p));
                        }
                    }
                }
            }
            // Drain ambient events (consent, block detection).
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\u{26a0} {}\n", a));
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }]
                }
            }))
        }
        // Propagate Closed so serve() can self-heal (relaunch Chrome
        // and retry the tool call). The agent never sees this error.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => {
            // Dead-tab errors propagate as-is so serve()'s tab-recovery
            // branch can run. The old code converted EVERY error to an
            // isError text response, which made that branch unreachable
            // dead code — a closed tab errored forever instead of healing.
            let msg = e.to_string();
            let tab_died = msg.contains("Target closed")
                || msg.contains("No target with given id")
                || msg.contains("Session closed")
                || msg.contains("Target.detachedFromTarget");
            if tab_died {
                return Err(e);
            }
            let mut text = format!("\u{2717} error: {e}");
            // Page-state contract: every error carries enough state for
            // the agent to recover without a separate see call. Handlers
            // that already embedded a state section (handle_act's action
            // path) are not doubled up.
            if !text.contains("--- current page state ---") {
                let view = page.view(1200);
                if !view.trim().is_empty() {
                    text.push_str(&format!("\n--- current page state ---\n{view}"));
                }
            }
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\n\u{26a0} dialogs auto-dismissed:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    text.push_str(&format!("  {} \"{}\" \u{2014} {}\n", d.kind, d.message, action));
                }
            }
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\u{26a0} {}\n", a));
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }],
                    "isError": true,
                }
            }))
        }
    }
}

/// Standard hint for offloaded payloads: the inline text is a truncated
/// prefix of the payload; the full data lives in the artifact file and reads
/// back through `see artifact="…"` (paged) — the path for pure-MCP clients
/// with no shell/file access.
fn artifact_hint(path: &str) -> String {
    format!("full payload: {path} — read it back in pages with see artifact=\"{path}\" (offset/limit) or any file tool")
}
