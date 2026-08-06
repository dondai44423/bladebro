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

use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};

use serde_json::{json, Value};

use crate::action::Action;
use crate::cdp::{self, CdpClient, CdpSession};
use crate::error::{BladeError, Result};
use crate::mcp::tools::tools_to_json;
use crate::page::Page;
use crate::state::StateOp;

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
const INSTRUCTIONS: &str = "Stealth browser driver. `see` reads the page (diff-first), `act` interacts (click/type/navigate), `state` manages cookies/tabs/sessions, `run` executes batch JS, `vision` screenshots.";

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
    if use_pipe {
        #[cfg(unix)]
        {
            let (browser, client) = crate::browser::Browser::launch_pipe().await?;
            let session = attach_pipe(&client).await?;
            let page = Page::attach(session, "pipe", Some(client)).await?;
            return Ok((page, Some(browser)));
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
        let target = cdp::first_page_target(&base).await?;
        let client = CdpClient::connect(target.ws_url()?).await?;
        let page = Page::attach(CdpSession::root(client), &base, None).await?;
        Ok((page, Some(browser)))
    } else {
        // Connect to an existing Chrome on the given port.
        let base = format!("{host}:{port}");
        let target = cdp::first_page_target(&base).await?;
        let client = CdpClient::connect(target.ws_url()?).await?;
        let page = Page::attach(CdpSession::root(client), &base, None).await?;
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
    // Track resource-blocking config so it survives idle shutdown/relaunch.
    let mut block_classes: Option<String> = None;
    // Domain knowledge base: consent selectors, visit tracking, stats.
    // Loaded once at startup, synced to disk periodically + on shutdown.
    let knowledge = crate::knowledge::load_shared();

    eprintln!(
        "[bladebro] MCP server ready (Chrome launches on first tool call{}",
        if idle_secs > 0 {
            format!(", idle timeout: {idle_secs}s)")
        } else {
            ")".to_string()
        }
    );

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
                        eprintln!("[bladebro] invalid JSON: {e}");
                        continue;
                    }
                };

                let id = msg.get("id").cloned();
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
                        // Three cases: first call (page=None), idle shutdown
                        // (page=None), or Chrome crashed (is_closed).
                        let need_launch = page.is_none()
                            || page.as_ref().map(|p| p.is_closed()).unwrap_or(true);
                        if need_launch {
                            if browser.is_some() {
                                // Chrome crashed or is dead, kill it first.
                                eprintln!("[bladebro] browser connection lost, relaunching...");
                                if let Some(b) = browser.take() {
                                    shutdown_browser(b).await;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                relaunch_note = Some(
                                    "note: Chrome was restarted (connection lost) — page state reset to about:blank. Navigate to continue.".into()
                                );
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
                                    if crate::session_profile::SessionProfile::claim_warming() {
                                        if let Some(ref mut p) = page {
                                            warm_profile(p).await;
                                        }
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
                                // Self-heal: relaunch and retry once.
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
                    let resp_str = serde_json::to_string(&resp)?;
                    writeln!(out, "{resp_str}")?;
                    out.flush()?;
                }
            }
            _ = idle_check.tick() => {
                // Periodic sync-back for SIGKILL resilience.
                if browser.is_some() && last_sync.elapsed() >= sync_interval {
                    if let Some(ref b) = browser {
                        let dir = b.profile_dir().to_path_buf();
                        // Offload to blocking thread: profile copy is I/O-heavy.
                        let _ = tokio::task::spawn_blocking(move || {
                            crate::session_profile::SessionProfile::sync_back_only(&dir);
                        }).await;
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
                {
                    eprintln!(
                        "[bladebro] idle timeout ({}s), shutting down Chrome to save memory",
                        idle_secs
                    );
                    if let Some(b) = browser.take() {
                        shutdown_browser(b).await;
                    }
                    page = None;
                }
            }
        }
    }

    // Clean up on exit: kill Chrome gracefully (flushes the
    // session profile back to the template), abort page tasks.
    if let Some(b) = browser.take() {
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
/// create a fresh tab and switch the session to it. The
/// retry then acts on a live page instead of erroring
/// "Target closed" forever.
async fn recover_dead_tab(page: &mut Page) -> Result<()> {
    let res = page.cdp_ref()
        .send("Target.createTarget", Some(json!({ "url": "about:blank" })))
        .await?;
    let new_id = res.get("targetId").and_then(|v| v.as_str())
        .ok_or_else(|| BladeError::Other("no targetId on tab recovery".into()))?;
    page.switch_tab(new_id).await
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

async fn handle_tools_call(
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
            // Also drain dialogs on error.
            let mut text = format!("\u{2717} error: {e}");
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
                text.push_str(&format!("\n\u{26a0} {}\n", a));
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

/// M3: Resolve a text query to an element ref. Searches the LPM (page model)
/// first — it has all elements from all frames, so label addressing works
/// for iframe content too. Falls back to live DOM search via find_by_text
/// if the LPM has no matches (e.g. the page changed since last capture).
///
/// Three-tier matching: exact/contains name → alias group → type-based.
/// Alias groups map common synonyms (username→acct, password→pw, etc.).
/// Type-based uses the HTML input type as a universal hint.
async fn resolve_text_target(
    page: &mut Page,
    query: &str,
    role_filter: Option<&str>,
    nth: Option<usize>,
) -> Result<String> {
    let q = query.to_lowercase();

    // Alias groups: all names in a group are interchangeable.
    // When the query matches any name in a group, any element whose
    // name is also in that group is a candidate.
    const FIELD_ALIASES: &[&[&str]] = &[
        &["username", "user", "login", "acct", "account", "userid", "user id",
          "uid", "uname", "login name", "loginid", "login id", "signin",
          "sign in", "user name", "member", "handle", "nick", "nickname"],
        &["password", "pw", "passwd", "pwd", "pass", "secret",
          "current password", "new password", "confirm password"],
        &["email", "mail", "e mail", "e-mail", "emailaddress",
          "email address", "eml", "emailaddr"],
        &["search", "query", "find", "filter", "keyword", "q", "s"],
        &["phone", "tel", "mobile", "phone number", "mobile number",
          "telephone", "contact", "cell", "cellphone"],
        &["name", "fullname", "full name", "first name", "firstname",
          "given name", "family name", "last name", "lastname", "display name"],
    ];

    // Find which alias group the query belongs to (if any).
    let alias_group: Option<&[&str]> = FIELD_ALIASES
        .iter()
        .find(|g| g.iter().any(|a| *a == q))
        .copied();

    // Phase 1: Search the LPM directly.
    let mut lpm_matches: Vec<(String, String, String, Vec<usize>, i64)> = Vec::new();
    for el in page.model().elements() {
        let role = &el.raw.role;
        if role == "hidden" { continue; }
        if let Some(rf) = role_filter {
            if role != rf { continue; }
        }
        let name = &el.raw.name;
        let name_lower = name.to_lowercase();
        let mut score = 0i64;
        if name == query { score = 100; }
        else if name_lower == q { score = 80; }
        else if name.contains(query) { score = 70; }
        else if name_lower.contains(&q) { score = 60; }
        else {
            // Check placeholder as fallback.
            let al = el.raw.placeholder.as_deref().unwrap_or("");
            if !al.is_empty() && al.to_lowercase().contains(&q) { score = 30; }
        }
        // Alias group matching: if both query and element name are in
        // the same alias group, it's a strong match (score 55).
        if score == 0 {
            if let Some(group) = alias_group {
                if group.iter().any(|a| *a == name_lower) {
                    score = 55;
                }
            }
        }
        // Type-based matching: if query matches the HTML input type.
        if score == 0 {
            if let Some(ref ty) = el.raw.element_type {
                let ty_lower = ty.to_lowercase();
                let type_match = (q == "password" && ty_lower == "password")
                    || (q == "email" && ty_lower == "email")
                    || (q == "search" && ty_lower == "search")
                    || (q == "phone" && ty_lower == "tel")
                    || (q == "url" && ty_lower == "url");
                if type_match { score = 50; }
            }
        }
        if score > 0 {
            lpm_matches.push((
                el.ref_id.clone(),
                role.clone(),
                name.clone(),
                el.raw.frame.clone(),
                score,
            ));
        }
    }
    if !lpm_matches.is_empty() {
        lpm_matches.sort_by_key(|b| std::cmp::Reverse(b.4));
        if let Some(n) = nth {
            if n >= 1 && n <= lpm_matches.len() {
                return Ok(lpm_matches[n - 1].0.clone());
            }
        }
        return Ok(lpm_matches[0].0.clone());
    }

    // Phase 2: Positional fallback for forms. If the query is a common
    // field type (username/password/email) and there are textboxes in
    // the model, pick the first textbox for username/email and the
    // password-typed one for password.
    if let Some(group) = alias_group {
        let is_username_like = group.iter().any(|a| {
            *a == "username" || *a == "user" || *a == "login" || *a == "acct"
        });
        let is_password_like = group.iter().any(|a| *a == "password" || *a == "pw");
        if is_username_like || is_password_like {
            let textboxes: Vec<_> = page.model().elements().iter()
                .filter(|e| e.raw.role == "textbox" || e.raw.role == "combobox")
                .collect();
            if is_password_like {
                // Prefer password-typed inputs.
                if let Some(pw) = textboxes.iter().find(|e| {
                    e.raw.element_type.as_deref() == Some("password")
                }) {
                    return Ok(pw.ref_id.clone());
                }
            }
            if is_username_like && !textboxes.is_empty() {
                // First non-password textbox is the username field.
                if let Some(tb) = textboxes.iter().find(|e| {
                    e.raw.element_type.as_deref() != Some("password")
                }) {
                    return Ok(tb.ref_id.clone());
                }
            }
        }
    }

    // Phase 3: Live DOM search via find_by_text. Used when the LPM
    // has no matches (page changed since last capture).
    let matches = crate::action::find_by_text(page.cdp_ref(), query, role_filter).await?;
    if matches.is_empty() {
        let view = page.view(2000);
        return Err(BladeError::Other(format!(
            "no element matching \"{}\" found\n\n--- current page ---\n{}",
            query, view
        )));
    }
    if let Some(n) = nth {
        if n >= 1 && n <= matches.len() {
            let m = &matches[n - 1];
            return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
        }
    }
    if matches.len() == 1 {
        let m = &matches[0];
        return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
    }
    let top = &matches[0];
    let id = page.model_mut().adopt(&top.sig, &top.role, &top.name, &top.frame);
    for m in &matches[1..] {
        let _ = page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame);
    }
    Ok(id)
}

async fn handle_act(args: &Value, page: &mut Page) -> Result<String> {
    let action_str = args.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let ref_id = args.get("ref").and_then(|r| r.as_str()).unwrap_or("");
    let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let key = args.get("key").and_then(|k| k.as_str()).unwrap_or("");
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let dx = args.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
    let dy = args.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);
    let label = args.get("label").and_then(|l| l.as_str()).unwrap_or("");
    let role_str = args.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let expect = args.get("expect").and_then(|e| e.as_str()).unwrap_or("");
    let press = args.get("press").and_then(|p| p.as_str()).unwrap_or("");
    let nth = args.get("nth").and_then(|n| n.as_u64()).map(|n| n as usize);

    // Navigate first if url is given for a non-navigate action.
    // Previously url was silently ignored for fill/type/click etc.
    // causing the action to run on the wrong page.
    // Skip for actions that handle url themselves (download, collect)
    // and state ops (set-cookie uses url for cookie scope, open-tab for tab URL).
    if !url.is_empty()
        && action_str != "navigate"
        && action_str != "download"
        && action_str != "collect"
        && action_str != "state"
        && action_str != "set-cookie"
        && action_str != "cookies"
        && action_str != "del-cookie"
        && action_str != "open-tab"
        && action_str != "close-tab"
        && action_str != "switch-tab"
        && action_str != "save"
        && action_str != "load"
    {
        page.navigate(url).await?;
    }

    // Resource blocking (W1): `act navigate block=images,fonts,...`.
    // Applied before the navigation so the rules are live for the load.
    if action_str == "navigate" {
        if let Some(block) = args.get("block").and_then(|b| b.as_str()) {
            page.set_block_classes(block).await?;
        }
    }

    let action = match action_str {
        "click" => {
            let cx = args.get("x").and_then(|v| v.as_f64());
            let cy = args.get("y").and_then(|v| v.as_f64());
            if let (Some(x), Some(y)) = (cx, cy) {
                Action::ClickCoord { x, y }
            } else {
                let resolved = if !ref_id.is_empty() {
                    ref_id.to_string()
                } else if !text.is_empty() {
                    let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                    resolve_text_target(page, text, rf, nth).await?
                } else if !label.is_empty() {
                    let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                    resolve_text_target(page, label, rf, nth).await?
                } else {
                    return Err(BladeError::Other("click requires 'ref', 'text', 'label', or 'x'+'y'".into()));
                };
                Action::Click { ref_id: resolved }
            }
        }
        "type" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(BladeError::Other("type requires 'ref' or 'label' + 'text'".into()));
            };
            Action::Type { ref_id: resolved, text: text.into() }
        }
        "clear" => Action::Clear { ref_id: ref_id.into() },
        "select" => {
            let opt = args.get("option").and_then(|o| o.as_str())
                .or_else(|| args.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            Action::Select { ref_id: ref_id.into(), option: opt.into() }
        }
        "press" => Action::Press { key: key.into() },
        "scroll" => Action::Scroll { dx, dy },
        "reload" => Action::Reload,
        "forward" => Action::Forward,
        "eval" => {
            // V7: JS eval. Handled inline (returns data, not delta).
            let js = args.get("js").and_then(|j| j.as_str()).unwrap_or("");
            if js.is_empty() {
                return Err(BladeError::Other("eval requires 'js'".into()));
            }
            return handle_eval(page, js, ref_id).await;
        }
        "pdf" => {
            // V20: export the current page as a PDF artifact.
            return handle_pdf(page, args).await;
        }
        "download" => {
            // V19: wait for the most recent download to complete.
            return handle_download(page, args).await;
        }
        "collect" => {
            // V22: auto-extract + scroll + dedupe loop. Infinite-scroll collection.
            return handle_collect(page, args).await;
        }
        "hover" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !text.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, text, rf, nth).await?
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(BladeError::Other("hover requires 'ref', 'text', or 'label'".into()));
            };
            Action::Hover { ref_id: resolved }
        }
        "upload" => Action::Upload { ref_id: ref_id.into(), path: text.into() },
        "read" => {
            if ref_id.is_empty() {
                return Err(BladeError::Other(
                    "read requires 'ref' (an element id like e5 from see)".into(),
                ));
            }
            // Self-heal: the ref may have died since the agent saw it.
            let heal = page.ensure_ref(ref_id).await?;
            let text_content = crate::action::read_text(page.cdp_ref(), page.model(), ref_id).await?;
            let el = page.model().element(ref_id);
            let role = el.map(|e| e.raw.role.clone()).unwrap_or_default();
            let name = el.map(|e| e.raw.name.clone()).unwrap_or_default();
            let note = heal.map(|n| format!(" [{n}]")).unwrap_or_default();
            return Ok(format!(
                "Page: {} | phase: {} | {} actionable\n{} {} \"{}\"{}\n  text: \"{}\"\n",
                page.model().url(),
                page.model().phase(),
                page.model().actionables(),
                ref_id, role, name, note, text_content
            ));
        }
        "fill" => {
            let fields = args.get("fields").and_then(|f| f.as_array())
                .ok_or_else(|| BladeError::Other("fill requires 'fields' array".into()))?;
            let submit = args.get("submit").and_then(|s| s.as_str()).unwrap_or("");
            let mut last_verdict = String::new();
            let mut count = 0usize;
            for field in fields {
                let f_ref = field.get("ref").and_then(|r| r.as_str()).unwrap_or("");
                let f_label = field.get("label").and_then(|l| l.as_str()).unwrap_or("");
                let f_text = field.get("text").and_then(|t| t.as_str())
                    .or_else(|| field.get("option").and_then(|o| o.as_str()))
                    .unwrap_or("");
                let f_check = field.get("check").and_then(|c| c.as_bool());

                // Resolve the ref — try as-is first, then by label.
                let resolved = if !f_ref.is_empty() {
                    f_ref.to_string()
                } else if !f_label.is_empty() {
                    // Don't restrict to textbox — the field could be a
                    // checkbox or select. Search all actionable elements.
                    resolve_text_target(page, f_label, None, None).await?
                } else {
                    continue;
                };

                // Dispatch the right action based on element type.
                let role = page.model().element(&resolved)
                    .map(|e| e.raw.role.clone())
                    .unwrap_or_default();
                let action = match role.as_str() {
                    "checkbox" | "radio" => {
                        // For checkboxes/radios: click to toggle.
                        // If 'check' is specified, only click if current
                        // state doesn't match desired state.
                        let should_click = match f_check {
                            Some(want_checked) => {
                                let is_checked = page.model().element(&resolved)
                                    .and_then(|e| e.raw.checked)
                                    .unwrap_or(false);
                                is_checked != want_checked
                            }
                            None => true, // no 'check' param → just click
                        };
                        if should_click {
                            Action::Click { ref_id: resolved }
                        } else {
                            count += 1;
                            continue; // already in desired state
                        }
                    }
                    "combobox" => {
                        Action::Select { ref_id: resolved, option: f_text.into() }
                    }
                    _ => {
                        // Default: type into text-like fields.
                        Action::Type { ref_id: resolved, text: f_text.into() }
                    }
                };
                let (_, verdict) = page.act(action).await?;
                last_verdict = verdict;
                count += 1;
            }
            let last_delta = if !submit.is_empty() {
                // Refs are 'e' followed by digits (e1, e2, ...).
                // 'Edit', 'Enter', 'Email' start with 'e' but are text, not refs.
                let is_ref = submit.starts_with('e') && submit[1..].chars().all(|c| c.is_ascii_digit());
                let resolved = if is_ref {
                    submit.to_string()
                } else {
                    resolve_text_target(page, submit, None, None).await?
                };
                // Wait briefly for field validation to settle before clicking submit.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let (delta, verdict) = page.act(Action::Click { ref_id: resolved.clone() }).await?;
                last_verdict = verdict.clone();
                // If the mouse click had no effect (no navigation, no DOM change),
                // the submit button may be a div styled as a button or require
                // JS dispatch. Try el.click() as a fallback.
                if !delta.navigated && delta.is_empty() {
                    match handle_eval(page, "el ? (el.click(), true) : false", &resolved).await {
                        Ok(_) => {
                            last_verdict = format!("{verdict} (submit via JS click fallback)");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            page.recapture().await?
                        }
                        Err(_) => delta,
                    }
                } else {
                    delta
                }
            } else {
                page.recapture().await?
            };
            return Ok(format!("filled {count} fields\n{last_verdict}\n{}",
                page.delta_view(&last_delta, 8000)));
        }
        "wait" => {
            let condition = args.get("condition").and_then(|c| c.as_str()).unwrap_or("settle");
            let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(10);
            let match_text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Action::Wait {
                condition: condition.into(),
                text: match_text.into(),
                timeout: std::time::Duration::from_secs(timeout_secs),
            }
        }
        "back" => Action::Back,
        "batch" => {
            // D49: run each step sequentially in this one MCP call. The key
            // token-efficiency win: the agent does see → batch([click e2,
            // type e3 "user", click e4, type e5 "pass", click e6]) → see
            // instead of 11 calls for a 5-field form. Each nested step
            // recaptures internally (no stale refs), ONE final recapture
            // renders the cumulative delta.
            let steps = args.get("steps").and_then(|s| s.as_array())
                .ok_or_else(|| BladeError::Other("batch requires 'steps' array".into()))?;
            if steps.is_empty() {
                return Err(BladeError::Other("batch requires at least one step".into()));
            }
            let mut verdicts: Vec<String> = Vec::new();
            let mut ok_count = 0usize;
            let mut halted: Option<usize> = None;
            let start_url = page.model().url().to_string();
            let mut prev_url = start_url.clone();
            for (i, step) in steps.iter().enumerate() {
                let step_action = step.get("action").and_then(|a| a.as_str()).unwrap_or("unknown");
                match Box::pin(handle_act(step, page)).await {
                    Ok(verdict) => {
                        ok_count += 1;
                        let vline = verdict.lines().next().unwrap_or("ok").trim().to_string();
                        let curr_url = page.model().url().to_string();
                        // Auto-settle: if this step caused navigation, give the
                        // SPA time to render and recapture for fresh refs.
                        // Without this, the next step acts on a half-rendered page.
                        if curr_url != prev_url {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let _ = page.recapture().await;
                            verdicts.push(format!("step{}[{}]: {} (→ {})", i+1, step_action, vline, curr_url));
                        } else {
                            verdicts.push(format!("step{}[{}]: {}", i+1, step_action, vline));
                        }
                        prev_url = curr_url;
                    }
                    Err(e) => {
                        halted = Some(i+1);
                        verdicts.push(format!("step{}[{}]: HALT: {}", i+1, step_action, e));
                        break;
                    }
                }
            }
            // Cumulative final delta — one render, all the changes.
            let final_delta = page.recapture().await?;
            let view = page.delta_view(&final_delta, 8000);
            let summary = if let Some(halt) = halted {
                format!("batch stopped at step {halt} ({ok_count} ok)")
            } else {
                format!("batch ({} steps, {} ok)", steps.len(), ok_count)
            };
            return Ok(format!("{summary}\n{verdicts}\n{view}",
                summary=summary,
                verdicts=if verdicts.is_empty() { String::new() } else { format!("(steps: {})\n", verdicts.join(" | ")) },
                view=view));
        }
        "navigate" => {
            let delta = page.navigate(url).await?;
            let _rt = std::time::Instant::now();
            let verdict = if delta.navigated {
                format!("outcome: navigated \u{2192} {}", page.model().url())
            } else {
                "outcome: already here".to_string()
            };
            if std::env::var("NAV_TIMING").is_ok() {
                eprintln!("[nav-timing] navigate completed: {:?}", _rt.elapsed());
            }
            // Refs (budget 3000) + brief content preview (1500 chars).
            // The agent gets enough to act AND read — skipping a separate
            // see call for most tasks. For dense pages use see mode=model
            // (more refs) or mode=content (full text).
            let top = page.view(3000);
            if delta.navigated {
                let content = page.content(1500).await.unwrap_or_default();
                if !content.is_empty() {
                    return Ok(format!("{verdict}\n{top}\n--- content ---\n{content}"));
                }
            }
            return Ok(format!("{verdict}\n{top}"));
        }
        "state" | "open-tab" | "close-tab" | "switch-tab" | "save" | "load" | "cookies" | "set-cookie" => {
            // Allow state ops as action shortcuts in batch/run steps.
            let mut state_args = args.clone();
            if action_str != "state" {
                if let Some(obj) = state_args.as_object_mut() {
                    if !obj.contains_key("op") {
                        obj.insert("op".to_string(), serde_json::Value::String(action_str.to_string()));
                    }
                }
            }
            return handle_state(&state_args, page).await;
        }
        _ => {
            return Err(crate::error::BladeError::Other(format!(
                "unknown action: {action_str}"
            )))
        }
    };

    // For scroll, the delta is useless (scrolling doesn't add/remove elements).
    // Return the full view so the agent sees what's on the page after scrolling.
    let is_scroll = matches!(action, Action::Scroll { .. });

    let result = page.act(action).await;

    // If `type` had a `press` param (e.g. press="Enter"), fire the key
    // after the text is in the field. This is the most common agent
    // pattern: type a search query + Enter to submit.
    let result = if action_str == "type" && !press.is_empty() {
        match result {
            Ok((type_delta, type_verdict)) => {
                let press_result = page.act(Action::Press { key: press.into() }).await;
                match press_result {
                    Ok((press_delta, press_verdict)) => {
                        // Merge: the press result is what matters (it triggers
                        // navigation / form submit). Include the type verdict
                        // as context.
                        let merged_verdict = format!(
                            "{type_verdict} then {press_verdict}"
                        );
                        Ok((press_delta, merged_verdict))
                    }
                    // Press failed — return the type result with a note.
                    Err(e) => Ok((type_delta, format!(
                        "{type_verdict} (press {press} failed: {e})"
                    ))),
                }
            }
            other => other,
        }
    } else {
        result
    };

    match result {
        Ok((delta, verdict)) => {
            // M14: Check expect param against observed outcome.
            let expect_note = if !expect.is_empty() {
                let observed = if delta.navigated { "navigation" }
                    else if !delta.is_empty() { "dom-change" }
                    else { "none" };
                if expect != observed && expect != "any" {
                    format!("\n\u{26a0} expected {expect}, got {observed} \u{2014} may have hit wrong target")
                } else { String::new() }
            } else { String::new() };
            // V13: slim mode — verdict only, no delta body. For
            // agents mid-`run` or confident in the outcome.
            let slim = args.get("slim").and_then(|s| s.as_bool()).unwrap_or(false);
            if slim {
                return Ok(format!("{verdict}{expect_note}"));
            }
            if is_scroll {
                Ok(format!("{verdict}{expect_note}\n{}", page.view(8000)))
            } else {
                let view = page.delta_view(&delta, 8000);
                if delta.navigated {
                    let content = page.content(1500).await.unwrap_or_default();
                    if !content.is_empty() {
                        Ok(format!("{verdict}{expect_note}\n{view}\n--- content ---\n{content}"))
                    } else {
                        Ok(format!("{verdict}{expect_note}\n{view}"))
                    }
                } else {
                    Ok(format!("{verdict}{expect_note}\n{view}"))
                }
            }
        }
        // Error context: recapture and include available elements so the
        // agent doesn't need a separate `see` call to understand the failure.
        // CRITICAL: BladeError::Closed propagates UNWRAPPED — serve()
        // detects it and self-heals (relaunch + retry). Wrapping it in
        // Other would kill transparent crash recovery.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => {
            let _ = page.recapture().await;
            let view = page.view(3000);
            Err(crate::error::BladeError::Other(format!(
                "{e}\n\n--- current page state ---\n{view}"
            )))
        }
    }
}

async fn handle_see(args: &Value, page: &mut Page) -> Result<String> {
    let budget = args.get("budget").and_then(|b| b.as_u64()).unwrap_or(8000) as usize;
    let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("");
    let want_content = args.get("content").and_then(|c| c.as_bool()).unwrap_or(false);
    let find = args.get("find").and_then(|f| f.as_str()).unwrap_or("");
    let extract = args.get("extract").and_then(|e| e.as_str()).unwrap_or("");
    let scope = args.get("scope").and_then(|s| s.as_str()).unwrap_or("");
    let logs = args.get("logs").and_then(|l| l.as_str()).unwrap_or("");
    let template = args.get("template").cloned();
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
    let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("");

    // mode=content: clean markdown extraction for reading. No refs, no
    // actionability markers — just the page text as structured markdown.
    // Headings, links, lists, code blocks, tables preserved.
    if mode == "content" {
        let md = page.markdown(budget).await?;
        if md.is_empty() {
            return Ok("page has no text content (may be a SPA that hasn't rendered — try waiting, or use mode=model for interactive elements)".into());
        }
        return Ok(md);
    }

    // mode=outline: just headings. Ultra-minimal for "what's on this page".
    if mode == "outline" {
        let out = page.outline().await?;
        return Ok(out);
    }

    // V8: logs — console (injection hook) or network (tracker ring).
    if !logs.is_empty() {
        return handle_logs(page, logs).await;
    }

    // V9: template extraction — structured data in ONE call.
    if extract == "json" {
        let tpl = template.ok_or_else(|| BladeError::Other(
            "extract=json requires 'template': {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css|css@attr\"}}}. For template-free structured extraction use extract=auto.".into()
        ))?;
        return handle_template_extract(page, &tpl, limit).await;
    }

    // V21: auto-extract — deterministic structural analysis. Finds the DOM
    // container with the most repeated structurally-similar children (the
    // "main list": products, articles, results), extracts per-item fields,
    // and INFERS field names by content type (title/link/image/price/date).
    // No template, no LLM.
    if extract == "auto" {
        return handle_auto_extract(page, limit).await;
    }

    // M11: find — search all actionable elements by text, return matches with refs.
    if !find.is_empty() {
        let matches = crate::action::find_by_text(page.cdp_ref(), find, None).await?;
        if matches.is_empty() {
            // M11 contract is full-page search: when no ACTIONABLE element
            // matches, probe the plain text. Returns a context snippet so
            // agents can verify outcomes ("Order confirmed") without
            // dumping full page content.
            let q_json = serde_json::to_string(&find.to_lowercase()).unwrap_or_else(|_| "''".into());
            let probe = page.cdp_ref().send("Runtime.evaluate", Some(json!({
                "expression": format!("(function(){{var q={q_json};var t=(document.body&&document.body.innerText)||'';var i=t.toLowerCase().indexOf(q);if(i<0)return null;return t.slice(Math.max(0,i-60),i+q.length+60).replace(/\\s+/g,' ').trim();}})()"),
                "returnByValue": true,
            }))).await.ok()
                .and_then(|r| r.get("result").and_then(|x| x.get("value")).cloned());
            if let Some(serde_json::Value::String(snip)) = probe {
                return Ok(format!(
                    "no actionable elements matching \"{find}\", but text present in page:\n  \"…{snip}…\"\n  (see content=true to read full context)"
                ));
            }
            return Ok(format!("no elements matching \"{find}\" found", ));
        }
        let mut out = format!("find \"{}\": {} match{}\n", find, matches.len(), if matches.len() > 1 { "es" } else { "" });
        for m in &matches {
            let ref_id = page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame);
            out.push_str(&format!("{} {} \"{}\" (score: {})\n", ref_id, m.role, m.name, m.score));
        }
        return Ok(out);
    }

    // M12: extract — structured data extraction (links/forms).
    if !extract.is_empty() {
        let expr = match extract {
            "links" => r#"(()=>{const links=[...document.querySelectorAll('a[href]')];return JSON.stringify(links.map(a=>({text:(a.textContent||'').trim().slice(0,80),href:a.href})).filter(l=>l.text||l.href));})()"#,
            "forms" => r#"(()=>{
function extractForms(doc){
const forms=[...doc.querySelectorAll('form')];
return forms.map(f=>({action:f.action,method:(f.method||'get').toLowerCase(),fields:[...f.elements].filter(e=>e.tagName!=='FIELDSET'&&e.tagName!=='BUTTON').map(e=>{
// Label resolution priority: <label for=id> > aria-label > aria-labelledby > placeholder > wrapping <label> > preceding text
var label='';
if(e.id){var lbl=doc.querySelector('label[for="'+e.id+'"]');if(lbl)label=lbl.textContent.trim().slice(0,60);}
if(!label&&e.getAttribute('aria-label'))label=e.getAttribute('aria-label').trim().slice(0,60);
if(!label&&e.getAttribute('aria-labelledby')){var lb=doc.getElementById(e.getAttribute('aria-labelledby'));if(lb)label=lb.textContent.trim().slice(0,60);}
if(!label&&e.placeholder)label=e.placeholder.trim().slice(0,60);
if(!label&&e.closest('label'))label=e.closest('label').textContent.trim().slice(0,60);
return{tag:e.tagName.toLowerCase(),type:e.type||null,name:e.name||'',label:label};
})}));
}
var allForms=extractForms(document);
// Search iframes too (W3Schools TryIt etc render forms in iframes).
try{for(const ifr of document.querySelectorAll('iframe')){try{if(ifr.contentDocument){allForms=allForms.concat(extractForms(ifr.contentDocument));}}catch(e){}}
}catch(e){}
return JSON.stringify(allForms);
})()"#,
            _ => return Err(BladeError::Other(format!("unknown extract type: {extract} (use 'links' or 'forms')"))),
        };
        let res = page.cdp_ref().send("Runtime.evaluate", Some(serde_json::json!({
            "expression": expr,
            "returnByValue": true,
        }))).await?;
        let json_str = res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("[]");
        // V10: offload large extracts to a file.
        if json_str.len() > 6000 {
            let path = crate::artifacts::write_artifact(json_str, "json")?;
            let count = json_str.matches("href").count();
            return Ok(format!(
                "extract {extract} (~{count} items, {} bytes) → {path}\npreview: {}…\nread the file for the full data",
                json_str.len(),
                json_str.chars().take(600).collect::<String>()
            ));
        }
        return Ok(format!("extract {extract}:\n{json_str}"));
    }

    // M12: scope — return the text content of one element's subtree.
    if !scope.is_empty() {
        let text = crate::action::read_text(page.cdp_ref(), page.model(), scope).await?;
        let el = page.model().element(scope);
        let (role, name) = el.map(|e| (e.raw.role.clone(), e.raw.name.clone())).unwrap_or_default();
        return Ok(format!("scope {scope} {role} \"{}\":\n{}", name, text));
    }

    // Recapture to get fresh state.
    let _delta = page.recapture().await?;
    let mut out = if filter.is_empty() {
        page.view(budget)
    } else {
        page.view_filtered(budget, filter)
    };
    // Auto-include content when the page has very few actionable elements —
    // the agent almost certainly needs the text, not just the empty element list.
    let auto_content = !want_content && page.model().actionables() <= 3 && filter.is_empty();
    if want_content || auto_content {
        let content_budget = if auto_content { budget } else { budget.min(4000) };
        let content = page.content(content_budget).await?;
        if !content.is_empty() {
            out.push_str("\n--- page content ---\n");
            out.push_str(&content);
            out.push('\n');
        }
    }
    Ok(out)
}

/// V8: `see logs=console|network` — introspection for agent
/// self-diagnosis. Errors/warnings first. Artifact-offloaded
/// when the log is long.
async fn handle_logs(page: &mut Page, kind: &str) -> Result<String> {
    match kind {
        "console" => {
            let entries = page.console_log().await?;
            let arr = entries.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                return Ok("console: (empty)".to_string());
            }
            // Errors first, then warnings, then the rest.
            let mut errors = Vec::new();
            let mut warnings = Vec::new();
            let mut rest = Vec::new();
            for e in &arr {
                let level = e.get("l").and_then(|l| l.as_str()).unwrap_or("");
                let msg = e.get("m").and_then(|m| m.as_str()).unwrap_or("");
                let line = format!("{level}: {msg}");
                match level {
                    "error" | "exception" | "unhandledrejection" => errors.push(line),
                    "warn" => warnings.push(line),
                    _ => rest.push(line),
                }
            }
            let mut out = format!("console ({} entries):\n", arr.len());
            for l in errors.iter().chain(warnings.iter()).chain(rest.iter()).take(30) {
                out.push_str(l);
                out.push('\n');
            }
            if arr.len() > 30 {
                let json_str = serde_json::to_string_pretty(&arr)?;
                let path = crate::artifacts::write_artifact(&json_str, "json")?;
                out.push_str(&format!("…and {} more → {path}\n", arr.len() - 30));
            }
            Ok(out)
        }
        "network" => {
            let entries = page.network_log();
            if entries.is_empty() {
                return Ok("network: (no completed requests)".to_string());
            }
            // Failures and 4xx/5xx first — that's what the agent
            // is debugging.
            let mut bad = Vec::new();
            let mut good = Vec::new();
            for e in &entries {
                let status_str = if e.status > 0 {
                    e.status.to_string()
                } else {
                    e.error.clone().unwrap_or_else(|| "ERR".into())
                };
                let short_url = if e.url.len() > 90 {
                    format!("{}…", &e.url[..87])
                } else {
                    e.url.clone()
                };
                let line = format!("{} {} {}", e.method, status_str, short_url);
                if e.error.is_some() || e.status >= 400 {
                    bad.push(line);
                } else {
                    good.push(line);
                }
            }
            let mut out = format!("network ({} requests):\n", entries.len());
            for l in bad.iter().chain(good.iter()).take(30) {
                out.push_str(l);
                out.push('\n');
            }
            if entries.len() > 30 {
                out.push_str(&format!("…and {} more\n", entries.len() - 30));
            }
            Ok(out)
        }
        _ => Err(BladeError::Other(format!(
            "unknown logs kind: {kind} (use 'console' or 'network')"
        ))),
    }
}

/// V9: template extraction. The agent provides a declarative
/// template; the driver runs ONE query and returns structured
/// JSON. Zero LLM in the loop — the fastest extraction of any
/// agent browser.
///
/// Template shape:
/// ```json
/// {"items": {"container": "css", "fields": {"name": "css|css@attr"}}}
/// ```
/// Multiple top-level keys are allowed (multiple lists in one
/// call). A field value of "" reads the container element
/// itself. `@attr` reads an attribute; default is textContent.
async fn handle_template_extract(
    page: &mut Page,
    template: &Value,
    limit: usize,
) -> Result<String> {
    let obj = template.as_object().ok_or_else(|| {
        BladeError::Other("template must be a JSON object".into())
    })?;

    // Build ONE JS expression covering all lists.
    let mut list_builders = Vec::new();
    for (list_name, spec) in obj {
        let container = spec.get("container").and_then(|c| c.as_str()).unwrap_or("");
        if container.is_empty() {
            return Err(BladeError::Other(format!(
                "template list '{list_name}' needs a 'container' selector"
            )));
        }
        let fields = spec.get("fields").and_then(|f| f.as_object()).cloned().unwrap_or_default();
        let mut field_parts = Vec::new();
        for (fname, fsel) in &fields {
            let sel = fsel.as_str().unwrap_or("");
            field_parts.push(format!(
                "{}:read(c,{})",
                serde_json::to_string(fname)?,
                serde_json::to_string(sel)?
            ));
        }
        list_builders.push(format!(
            "{}:(()=>{{const cs=[...document.querySelectorAll({})].slice(0,{});return cs.map(c=>({{{}}}));}})()",
            serde_json::to_string(list_name)?,
            serde_json::to_string(container)?,
            limit,
            field_parts.join(","),
        ));
    }

    let expr = format!(
        "(()=>{{const read=(c,sel)=>{{let s=sel,attr=null;const ai=sel.lastIndexOf('@');if(ai>0){{attr=sel.slice(ai+1);s=sel.slice(0,ai);}}const el=s?c.querySelector(s):c;if(!el)return null;if(attr)return el.getAttribute(attr);return(el.innerText||el.textContent||'').trim();}};return {{{}}};}})()",
        list_builders.join(",")
    );

    let res = page.cdp_ref().send("Runtime.evaluate", Some(json!({
        "expression": expr,
        "returnByValue": true,
    }))).await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc.get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("template extraction failed");
        return Err(BladeError::Other(format!("extract failed: {}", &msg[..msg.len().min(200)])));
    }

    let value = res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(json!({}));
    let json_str = serde_json::to_string_pretty(&value)?;

    // Count total items across lists.
    let total: usize = value.as_object()
        .map(|o| o.values().filter_map(|v| v.as_array().map(|a| a.len())).sum())
        .unwrap_or(0);

    if json_str.len() > 6000 {
        let path = crate::artifacts::write_artifact(&json_str, "json")?;
        let preview: String = json_str.chars().take(600).collect();
        return Ok(format!(
            "extract json ({total} items, {} bytes) → {path}\npreview: {preview}…\nread the file for the full data",
            json_str.len()
        ));
    }
    Ok(format!("extract json ({total} items):\n{json_str}"))
}

/// V21: Auto-extract — deterministic structural list extraction.
/// Finds the DOM container whose direct children are the most
/// structurally-repeated (the "main list"), extracts per-item fields by
/// content type, returns a JSON array. No template, no LLM.
fn auto_extract_expr(limit: usize) -> String {
    let lim = limit.min(500);
    r#"(()=>{
// Price: currency symbol required (no bare decimal numbers — false positives).
const PRICE=/([$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?)/;
const DATE=/(\b\d{4}-\d{2}-\d{2}\b|\b\d{1,2}[\/]\d{1,2}[\/]\d{2,4}\b|\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+\d{1,2},?\s*\d{2,4}\b|\b\d+\s+(?:second|minute|hour|day|week|month|year)s?\s+ago\b)/i;
const HOST=location.hostname;const IS_REDDIT=HOST.includes('reddit.com');const IS_GITHUB=HOST==='github.com';function parseCount(s){s=s.trim();let n=parseFloat(s.replace(/,/g,''));if(/k$/i.test(s))n*=1000;else if(/m$/i.test(s))n*=1000000;return Math.round(n);}
const ACT_PAT=/^(vote|upvote|downvote|comment|comments|discuss|reply|replies|share|save|hide|flag|report|favorite|fav|like|dislike|follow|Subscribe|Pin|Unpin|More|less|edit|delete|remove|add|new|open|show|expand|collapse|permalink|embed|cite|parent|context|full story|read more|continue reading|view|all|next|prev|previous)$/i;
const ACT_HREF=/\/vote|\/comment|\/reply|\/action|javascript:|#comment|#respond|#reply/i;
function sig(el){const k=[...el.children].map(c=>c.tagName).join(',');return el.tagName+'['+k+']';}
function txt(el){return(el.innerText||el.textContent||'').replace(/\s+/g,' ').trim();}
function links(el){return[...el.querySelectorAll('a[href]')];}
function extLink(el){return links(el).find(a=>a.hostname&&a.hostname!==HOST);}
function norm(s){return s.toLowerCase().replace(/[^a-z0-9 ]/g,'').replace(/\s+/g,' ').trim();}
function isActionLink(a){
const t=txt(a).toLowerCase();
if(t.split(/\s+/).length<=3&&ACT_PAT.test(t))return true;
if(ACT_HREF.test(a.href))return true;
if(/\d+\s+(point|comment|vote|reply|reaction)/i.test(t))return true;
return false;
}
function bestLink(item,title){
const lnks=links(item).filter(a=>!isActionLink(a)&&a.offsetParent!==null);
if(lnks.length===0)return null;
const ext=lnks.find(a=>a.hostname&&a.hostname!==HOST);
if(ext)return ext;
if(title){
const tn=norm(title);
if(tn){const match=lnks.find(a=>{const an=norm(txt(a));return an&&tn.includes(an)&&an.length>3});if(match)return match;}
}
return lnks.reduce((best,a)=>{const at=txt(a).length,bt=txt(best).length;return at>bt?a:best;},lnks[0]);
}
// Shopping field extraction (universal e-commerce enhancement).
function rating(el){const a=el.querySelector('[aria-label*="star"],[aria-label*="rating"]');if(a){const al=a.getAttribute('aria-label')||'';const m=al.match(/(\d+\.?\d*)\s*out of\s*\d+/i)||al.match(/(\d+\.?\d*)/);if(m)return parseFloat(m[1]);}const r=el.querySelector('[data-testid*="rating"],[class*="rating"],[class*="star"],[data-hook*="rating"],i[class*="a-icon-star"],span[class*="a-icon-alt"]');if(r){const m=(r.innerText||'').match(/(\d+\.?\d*)/);if(m)return parseFloat(m[1]);}const t=txt(el);const tm=t.match(/(\d+\.?\d*)\s*(?:out of\s*5|\/5|stars?)/i);if(tm)return parseFloat(tm[1]);return null;}
function reviews(el){const t=txt(el);let m=t.match(/(\d[\d,]*)\s*(?:global\s+)?(?:ratings?|reviews?)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);m=t.match(/(\d[\d,]*)\s*(?:ratings?|reviews?)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);m=t.match(/(\d[\d,]*)\+?\s*(?:bought|purchased|sold)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);return null;}
function avail(el){const t=txt(el).toLowerCase();if(/in stock|in-store only/.test(t))return 'in stock';if(/out of stock|currently unavailable/.test(t))return 'out of stock';const ol=t.match(/only\s+(\d+)\s+left/i);if(ol)return 'only '+ol[1]+' left';const sh=t.match(/usually ships[^.]{0,50}/i);if(sh)return sh[0].trim();if(/pre-order/i.test(t))return 'pre-order';return null;}
function origPrice(el){const d=el.querySelector('del,s,[data-testid*="original"],[class*="was-price"],[class*="list-price"],[class*="original-price"]');if(d){const m=(d.innerText||'').match(PRICE);if(m)return m[0];}const t=txt(el);const m=t.match(/was\s+([$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?)/i);if(m)return m[1];return null;}
function isSponsored(el){const t=txt(el).toLowerCase();if(t.includes('sponsored')||t.includes('sponsored ad'))return true;const b=el.querySelector('[data-testid*="sponsored"],[class*="sponsored"],[aria-label*="sponsored"]');return!!b;}
// Reddit field extraction (text-based, works through shadow DOM via innerText).
function rdScore(el){const t=txt(el);let m=t.match(/(\d[\d.,]*[KkMm]?)\s*(?:upvotes?|votes?|points?)/i);if(m)return parseCount(m[1]);return null;}
function rdComments(el){const t=txt(el);let m=t.match(/(\d[\d.,]*)\s*comments?/i);if(m)return parseInt(m[1].replace(/[,.]/g,''),10);return null;}
function rdAuthor(el){const a=el.querySelector('a[href*="/user/"]');if(a){const m=a.href.match(/\/user\/([\w-]+)/);if(m)return 'u/'+m[1];}const t=txt(el);const m=t.match(/u\/(\w[\w-]*)/i);if(m)return 'u/'+m[1];const img=el.querySelector('img[alt*="avatar"]');if(img){const m2=(img.alt||'').match(/u\/(\w[\w-]*)/i);if(m2)return 'u/'+m2[1];}return null;}
function rdSub(el){const u=location.href.match(/\/r\/(\w[\w-]*)/);if(u)return 'r/'+u[1];const t=txt(el);const m=t.match(/r\/(\w[\w-]*)/i);if(m)return 'r/'+m[1];return null;}
// GitHub field extraction.
function ghStars(el){var s=el.querySelector('a[href$="/stargazers"]');if(s){var m=(s.innerText||'').match(/(\d[\d.,]*[KkMm]?)/);if(m)return parseCount(m[1]);}var sb=el.querySelector('button[aria-label*="star"],a[aria-label*="star"]');if(sb){var al=sb.getAttribute('aria-label')||'';var m2=al.match(/(\d[\d.,]*[KkMm]?)/);if(m2)return parseCount(m2[1]);}var t=txt(el);var m3=t.match(/(\d[\d.,]*[KkMm]?)\s*stars?\b/i);if(m3)return parseCount(m3[1]);return null;}
function ghForks(el){const f=el.querySelector('a[href$="/forks"]');if(f){const m=(f.innerText||'').match(/(\d[\d.,]*[KkMm]?)/);if(m)return parseCount(m[1]);}const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*forks?\b/i);if(m)return parseCount(m[1]);return null;}
function ghStarsToday(el){const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*stars?\s*today/i);if(m)return parseCount(m[1]);return null;}
function ghLabels(el){const ls=el.querySelectorAll('a[data-name],.IssueLabel,.Label,.labels a');const r=[];for(const l of ls){const n=l.getAttribute('data-name')||txt(l);if(n&&n.length>1&&!r.includes(n))r.push(n);}return r;}
function ghNumber(el){const a=el.querySelector('a[href*="/issues/"],a[href*="/pull/"]');if(a){const m=a.href.match(/\/(?:issues|pull)\/(\d+)/);if(m)return parseInt(m[1],10);}const t=txt(el);const m=t.match(/#(\d+)/);if(m)return parseInt(m[1],10);return null;}
function ghStatus(el){const t=txt(el);const m=t.match(/Status:\s*(\w+)/i);if(m)return m[1].toLowerCase();if(el.querySelector('[data-testid="open-issue"],[class*="open-issue"],[aria-label*="open"]'))return 'open';if(el.querySelector('[data-testid="closed-issue"],[class*="closed-issue"],[aria-label*="closed"]'))return 'closed';return null;}
function isProductPage(){const b=document.body;if(!b)return false;const t=(b.innerText||'').toLowerCase();const hp=/[$€£¥₹]\s?\d/.test(t);const cb=document.querySelector('#add-to-cart-button,#buy-now-button,[data-testid*="add-to-cart"],[data-testid*="buy-now"],button[name*="cart"],input[name*="cart"],#add-to-cart,#buy-now');const hc=/add to cart|buy now|add to basket|add to bag|in winkelwagen|au panier/i.test(t);const u=location.href.toLowerCase();const pu=/\/dp\/|\/gp\/product\/|\/product\/|\/itm\/|\/products\/|\/p\//.test(u);const h1=document.querySelector('h1');const hh=h1&&h1.innerText.trim().length>5;return hp&&hh&&(pu||cb||hc);}
function extractProduct(){const o={};const h1=document.querySelector('h1');const title=h1?h1.innerText.trim():document.title;if(title)o.title=title.slice(0,300);o.url=location.href;const pe=document.querySelector('#priceblock_ourprice,#priceblock_dealprice,.a-price .a-offscreen,[data-testid*="price"],[class*="price"]:not([class*="was"]):not([class*="original"]):not([class*="save"]),[id*="price"]:not([id*="was"]):not([id*="original"])');if(pe){const m=(pe.innerText||'').match(PRICE);if(m)o.price=m[0];}if(!o.price){const m=(document.body.innerText||'').match(PRICE);if(m)o.price=m[0];}const op=origPrice(document.body);if(op)o.original_price=op;const rt=rating(document.body);if(rt!==null)o.rating=rt;const rv=reviews(document.body);if(rv!==null)o.reviews=rv;const av=avail(document.body);if(av)o.availability=av;const img=document.querySelector('#landingImage,#imgBlkFront,[data-testid*="product-image"],.product-image img,img[class*="product"]:not([src*="logo"]):not([src*="icon"]):not([src*="sprite"])');if(img&&img.src){o.image=img.src;if(img.alt)o.image_alt=img.alt.slice(0,100);}const fs=[];const sec=document.querySelector('#feature-bullets,#productOverview_feature_div,#detailBullets_feature_div,[data-feature-name="productDescription"],#productDescription,#aplus,.product-facts-details,[data-testid="featureBullets"]')||(h1||pe||document.body).closest('section,div,main,[role="main"]')||document.body;if(sec){const bs=sec.querySelectorAll('li,[role="listitem"],span.a-list-item');for(const b of bs){const bt=(b.innerText||'').trim();if(bt.length>10&&bt.length<300&&fs.length<10&&!/add to cart|buy now|sign in|subscribe|follow|see more|show more/i.test(bt))fs.push(bt);}}if(fs.length>0)o.features=fs;return o;}
const SKIP_TAGS=new Set(['STYLE','SCRIPT','HEAD','NOSCRIPT','SVG','TEMPLATE','LINK','META','BR','HR','PATH','DEFS','USE','G','RECT','CIRCLE','LINE','POLYGON','POLYLINE']);
let best=null,bestScore=0,bestSig='';
for(const c of document.querySelectorAll('*')){
if(SKIP_TAGS.has(c.tagName))continue;
const kids=[...c.children].filter(k=>k.nodeType===1&&k.offsetParent!==null);
if(kids.length<3)continue;
const groups={};
for(const k of kids){const s=sig(k);if(!groups[s])groups[s]=[];groups[s].push(k);}
for(const s in groups){
const items=groups[s];
if(items.length<3)continue;
if(items[0]&&SKIP_TAGS.has(items[0].tagName))continue;
let totalText=0,extCount=0,hCount=0,imgCount=0,linkCount=0;
for(const it of items){totalText+=txt(it).length;if(extLink(it))extCount++;if(it.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]'))hCount++;if(it.querySelector('img[src]'))imgCount++;if(links(it).length>0)linkCount++;}
const count=items.length;const avgText=totalText/count;
if(avgText<5)continue;
const tf=Math.min(Math.max(avgText/50,0.5),4);
const score=count*tf*(1+(extCount/count)*2+(hCount/count)+(imgCount/count)*0.5+(linkCount/count)*0.3);
if(score>bestScore){bestScore=score;best=c;bestSig=s;}
}
}
if(!best){if(isProductPage()){const p=extractProduct();if(Object.keys(p).length>1)return JSON.stringify({container:'product',count:1,items:[p]});}return JSON.stringify({error:'no repeated list found',items:[]});}
// Field extraction: clean, typed, deduplicated. No 'text' field.
const items=[...best.children].filter(k=>k.nodeType===1&&sig(k)===bestSig).map(item=>{
const fullText=txt(item);const o={};
// Title: heading → longest link text → first sentence.
const h=item.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]');
let title=h?txt(h):'';
const link=bestLink(item,title);
if(link){const lt=txt(link);if(lt&&(!title||title.length<10||lt.length>title.length*1.5))title=lt.slice(0,200);}
if(!title){const sentence=fullText.split(/\.|!|\?/)[0];title=(sentence&&sentence.length>10?sentence:fullText).slice(0,200);}
if(title)o.title=title.slice(0,200);
if(link)o.url=link.href;
// Image.
const img=item.querySelector('img[src]');
if(img){o.image=img.src;if(img.alt)o.image_alt=img.alt.slice(0,100);}
// Price: currency symbol required, not in title.
const pr=(fullText.match(PRICE)||[])[0];
if(pr&&!title.includes(pr))o.price=pr;
// Date: only in non-title text.
const nonTitle=fullText.slice((title||'').length);
const dt=(nonTitle.match(DATE)||[])[0];
if(dt)o.date=dt;
// Description: non-link, non-heading text. Only if different from title.
const clone=item.cloneNode(true);
clone.querySelectorAll('a,script,style,noscript,svg').forEach(e=>e.remove());
const hd=clone.querySelector('h1,h2,h3,h4,h5,h6');
if(hd)hd.remove();
const desc=(clone.innerText||clone.textContent||'').replace(/\s+/g,' ').trim();
if(desc&&desc.length>15){const dn=norm(desc),tn=norm(title||'');if(dn&&!tn.includes(dn)&&!dn.includes(tn))o.description=desc.slice(0,300);}
// Site-specific fields.
if(IS_REDDIT){var sp=item.tagName==='SHREDDIT-POST'?item:item.querySelector('shreddit-post');if(sp){var sps=sp.getAttribute('score');if(sps)o.score=parseInt(sps,10);var spc=sp.getAttribute('comment-count');if(spc)o.comments=parseInt(spc,10);var spa=sp.getAttribute('author');if(spa)o.author='u/'+spa;var spsub=sp.getAttribute('subreddit-prefixed-name');if(spsub)o.subreddit=spsub;var spt=sp.getAttribute('post-title');if(spt)o.title=spt.slice(0,200);var spl=sp.getAttribute('permalink');if(spl)o.url='https://www.reddit.com'+spl;}else{var rsc=rdScore(item);if(rsc!==null)o.score=rsc;var rcm=rdComments(item);if(rcm!==null)o.comments=rcm;var rau=rdAuthor(item);if(rau)o.author=rau;var rsu=rdSub(item);if(rsu)o.subreddit=rsu;var rcl=item.querySelector('a[href*="/comments/"]');if(rcl){o.url=rcl.href;var rclt=txt(rcl);if(rclt&&rclt.length>5&&rclt.length<300)o.title=rclt.slice(0,200);}}}
else if(IS_GITHUB){const st=ghStars(item);if(st!==null)o.stars=st;const fk=ghForks(item);if(fk!==null)o.forks=fk;const sd=ghStarsToday(item);if(sd!==null)o.stars_today=sd;const lb=ghLabels(item);if(lb.length>0)o.labels=lb;const nm=ghNumber(item);if(nm!==null)o.number=nm;const gs=ghStatus(item);if(gs)o.status=gs;}
else{const rt=rating(item);if(rt!==null)o.rating=rt;const rv=reviews(item);if(rv!==null)o.reviews=rv;const av=avail(item);if(av)o.availability=av;const op=origPrice(item);if(op)o.original_price=op;if(isSponsored(item))o.sponsored=true;}
return o;
}).filter(o=>Object.keys(o).length>0).slice(0,"#.to_string()
    + &lim.to_string()
    + r#");
return JSON.stringify({container:best.tagName.toLowerCase(),count:items.length,items});
})()"#
}

/// Run auto-extract and return the parsed JSON value.
async fn run_auto_extract(page: &Page, limit: usize) -> Result<serde_json::Value> {
    let expr = auto_extract_expr(limit);
    let res = page.cdp_ref().send("Runtime.evaluate", Some(serde_json::json!({
        "expression": expr,
        "returnByValue": true,
    }))).await?;
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc.get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("auto-extract eval failed");
        return Err(BladeError::Other(format!("auto-extract: {}", &msg[..msg.len().min(200)])));
    }
    let json_str = res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("{}");
    Ok(serde_json::from_str(json_str).unwrap_or_else(|_| serde_json::json!({"error": "parse failed", "items": []})))
}

async fn handle_auto_extract(page: &mut Page, limit: usize) -> Result<String> {
    let val = run_auto_extract(page, limit).await?;
    let json_str = serde_json::to_string(&val)?;
    if json_str.len() > 12000 {
        let path = crate::artifacts::write_artifact(&json_str, "json")?;
        let preview: String = json_str.chars().take(1000).collect();
        return Ok(format!(
            "extract auto ({} bytes) → {path}\npreview: {preview}…\nread the file for the full data",
            json_str.len(),
        ));
    }
    Ok(format!("extract auto:\n{json_str}"))
}

/// V22: collect — auto-extract + scroll + dedupe loop. ONE call collects
/// an entire infinite-scroll feed into a single artifact.
async fn handle_collect(page: &mut Page, args: &Value) -> Result<String> {
    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(30);
    let max = args.get("max").and_then(|m| m.as_u64()).unwrap_or(100) as usize;
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");

    // Navigate first if url is provided. Without this, collect
    // extracts from whatever page happens to be current (Bug: asked
    // for Reddit, got eBay items).
    if !url.is_empty() {
        page.navigate(url).await?;
    }

    let mut all_items: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut no_new_streak = 0u32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        let val = run_auto_extract(page, 500).await?;
        let items = val.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
        let mut new_count = 0usize;
        for item in items {
            let key = item.get("url").or_else(|| item.get("title"))
                .and_then(|v| v.as_str()).unwrap_or("").to_string();
            if key.is_empty() || seen.insert(key) {
                all_items.push(item);
                new_count += 1;
            }
        }

        if all_items.len() >= max { break; }
        if new_count == 0 {
            no_new_streak += 1;
            if no_new_streak >= 2 { break; }
        } else {
            no_new_streak = 0;
        }
        if std::time::Instant::now() > deadline { break; }

        let _ = page.cdp_ref().send("Runtime.evaluate", Some(serde_json::json!({
            "expression": "window.scrollBy(0, Math.floor(window.innerHeight*0.9))",
            "returnByValue": true,
        }))).await;

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    let json = serde_json::to_string_pretty(&all_items)?;
    if json.len() <= 12000 {
        return Ok(format!("collected {} items:\n{json}", all_items.len()));
    }
    let path = crate::artifacts::write_artifact(&json, "json")?;
    let preview: String = json.chars().take(1000).collect();
    Ok(format!("collected {} items ({} bytes) → {path}\npreview: {preview}…", all_items.len(), json.len()))
}

async fn handle_state(args: &Value, page: &mut Page) -> Result<String> {
    let op_str = args.get("op").and_then(|o| o.as_str()).unwrap_or("");
    let name = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let target_id = args.get("target_id").and_then(|t| t.as_str()).unwrap_or("");

    // Tab lifecycle ops are handled HERE (not via state.rs) because
    // they re-attach the session — they need &mut Page.
    match op_str {
        "open-tab" => {
            // Create + auto-focus. Every agent that opens a tab
            // wants to act in it — a separate switch-tab call
            // would be pure waste.
            let res = page.cdp_ref()
                .send("Target.createTarget", Some(json!({ "url": url })))
                .await?;
            let new_id = res.get("targetId").and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::BladeError::Other("no targetId".into()))?;
            page.switch_tab(new_id).await?;
            let view = page.view(1500);
            return Ok(format!("\u{2713} opened + switched to tab {new_id}\n{view}"));
        }
        "switch-tab" => {
            page.switch_tab(target_id).await?;
            let view = page.view(1500);
            return Ok(format!("\u{2713} switched to tab {target_id}\n{view}"));
        }
        "close-tab" => {
            page.cdp_ref()
                .send("Target.closeTarget", Some(json!({ "targetId": target_id })))
                .await?;
            // If the agent closed the tab the session was attached
            // to, the session is now dead — auto-switch to a
            // remaining tab so the next command doesn't error.
            if !page.current_tab_alive().await {
                let tabs = page.tab_targets().await;
                if let Some(first) = tabs.first() {
                    page.switch_tab(&first.id).await?;
                    let view = page.view(1500);
                    return Ok(format!(
                        "\u{2713} closed tab {target_id} (was current; switched to {})\n{view}",
                        first.id
                    ));
                }
                return Ok(format!("\u{2713} closed tab {target_id} (was the last tab)"));
            }
            return Ok(format!("\u{2713} closed tab {target_id}"));
        }
        "block" => {
            let classes = args.get("classes").and_then(|c| c.as_str()).unwrap_or("");
            if !classes.is_empty() {
                let mask = page.set_block_classes(classes).await?;
                return Ok(format!("blocking: {}", crate::page::intercept::InterceptState::describe(mask)));
            }
            if args.get("clear").and_then(|c| c.as_bool()).unwrap_or(false) {
                page.set_block_classes("none").await?;
                return Ok("blocking: none".to_string());
            }
            return Ok(format!("blocking: {}", crate::page::intercept::InterceptState::describe(page.block_rules())));
        }
        _ => {}
    }

    let op = match op_str {
        "cookies" => {
            // If url is provided, filter cookies to that domain.
            // Otherwise, use the current page URL so the agent gets
            // relevant cookies, not a 100+ line dump of all browser cookies.
            let filter_url = if !url.is_empty() {
                Some(url.to_string())
            } else {
                let current = page.model().url().to_string();
                if current.is_empty() || current == "about:blank" { None } else { Some(current) }
            };
            StateOp::GetCookies { urls: filter_url.map(|u| vec![u]).unwrap_or_default() }
        },
        "set-cookie" => StateOp::SetCookie {
            name: name.into(), value: value.into(),
            // CDP Network.setCookie requires either url or domain.
            // Prefer url when the agent provides it; fall back to the
            // current page's url so the call never fails for missing scope.
            url: if !url.is_empty() { Some(url.into()) } else {
                let current = page.model().url().to_string();
                if current.is_empty() || current == "about:blank" { None } else { Some(current) }
            },
            domain: None, path: None, secure: None, http_only: None, same_site: None,
        },
        "del-cookie" => StateOp::DeleteCookies {
            name: name.into(),
            domain: args.get("domain").and_then(|d| d.as_str()).map(String::from),
            // CDP Network.deleteCookies requires either url or domain.
            // Prefer url when the agent provides it; fall back to the
            // current page's url so the call never fails for missing scope.
            url: if !url.is_empty() { Some(url.into()) } else {
                let current = page.model().url().to_string();
                if current.is_empty() || current == "about:blank" { None } else { Some(current) }
            },
        },
        "ls" => StateOp::GetLocalStorage,
        "ss" => StateOp::GetSessionStorage,
        "set-ls" => StateOp::SetLocalStorage { key: name.into(), value: value.into() },
        "set-ss" => StateOp::SetSessionStorage { key: name.into(), value: value.into() },
        "rm-ls" => StateOp::RemoveLocalStorage { key: name.into() },
        "rm-ss" => StateOp::RemoveSessionStorage { key: name.into() },
        "clear-ls" => StateOp::ClearLocalStorage,
        "clear-ss" => StateOp::ClearSessionStorage,
        "tabs" => StateOp::ListTabs,
        "save" => StateOp::SaveSession { name: name.into() },
        "load" => StateOp::LoadSession { name: name.into() },
        _ => return Err(crate::error::BladeError::Other(format!("unknown state op: {op_str}"))),
    };

    page.state(op).await
}

async fn handle_run(args: &Value, page: &mut Page) -> Result<String> {
    let steps = args.get("steps").and_then(|s| s.as_array()).ok_or_else(|| {
        crate::error::BladeError::Other("run requires 'steps' array".into())
    })?;

    let mut observations = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let step_num = i + 1; // 1-based for human-readable error messages
        execute_step(page, step, &step_num.to_string(), &mut observations).await?;
    }
    Ok(observations.join("\n"))
}

/// Build an Action from a step's JSON fields. Used by `execute_step` for
/// regular (non-special) actions. Supports the same addressing as `act`:
/// ref, text (+role/nth) for click, label for type.
async fn build_action(step: &Value, page: &mut Page) -> Result<Action> {
    let action_str = step.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let ref_id = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
    let text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let key = step.get("key").and_then(|k| k.as_str()).unwrap_or("");
    let dx = step.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
    let dy = step.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);
    let role_str = step.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let label = step.get("label").and_then(|l| l.as_str()).unwrap_or("");
    let nth = step.get("nth").and_then(|n| n.as_u64()).map(|n| n as usize);

    match action_str {
        "click" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !text.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, text, rf, nth).await?
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(crate::error::BladeError::Other(
                    "click step requires 'ref', 'text', or 'label'".into(),
                ));
            };
            Ok(Action::Click { ref_id: resolved })
        }
        "type" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(crate::error::BladeError::Other(
                    "type step requires 'ref' or 'label'".into(),
                ));
            };
            Ok(Action::Type { ref_id: resolved, text: text.into() })
        }
        "clear" => Ok(Action::Clear { ref_id: ref_id.into() }),
        "select" => {
            let opt = step.get("option").and_then(|o| o.as_str())
                .or_else(|| step.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            Ok(Action::Select { ref_id: ref_id.into(), option: opt.into() })
        }
        "press" => Ok(Action::Press { key: key.into() }),
        "scroll" => Ok(Action::Scroll { dx, dy }),
        "reload" => Ok(Action::Reload),
        "forward" => Ok(Action::Forward),
        "hover" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !text.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, text, rf, nth).await?
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(crate::error::BladeError::Other(
                    "hover step requires 'ref', 'text', or 'label'".into(),
                ));
            };
            Ok(Action::Hover { ref_id: resolved })
        }
        "upload" => Ok(Action::Upload { ref_id: ref_id.into(), path: text.into() }),
        "wait" => {
            let condition = step.get("condition").and_then(|c| c.as_str()).unwrap_or("settle");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(10);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Ok(Action::Wait {
                condition: condition.into(),
                text: match_text.into(),
                timeout: std::time::Duration::from_secs(timeout_secs),
            })
        }
        "back" => Ok(Action::Back),
        _ => Err(crate::error::BladeError::Other(format!(
            "unknown action: {action_str}"
        ))),
    }
}

/// V7: evaluate JS in the page. If `ref_id` is non-empty, the
/// element is resolved and exposed to the script as `el`.
/// Result is JSON-stringified, capped at 4KB inline; bigger
/// payloads go to an artifact file (V10).
async fn handle_eval(page: &mut Page, js: &str, ref_id: &str) -> Result<String> {
    // Always wrap in IIFE to prevent variable leakage to global scope.
    // Without this, `const posts = [...]` in one eval call causes
    // "Identifier 'posts' has already been declared" in the next.
    // If the code has a `return`, let it control the IIFE return.
    // If not, treat the whole thing as an expression and return it.
    let has_return = js.contains("return");
    let js_wrapped = if has_return {
        format!("(function(){{ {js} }})()")
    } else {
        format!("(function(){{ return ({js}); }})()")
    };
    let expression = if ref_id.is_empty() {
        js_wrapped
    } else {
        page.ensure_ref(ref_id).await?;
        let (sig, frame) = {
            let el = page.model().element(ref_id).ok_or_else(|| {
                BladeError::StaleRef(ref_id.to_string())
            })?;
            (el.raw.sig.clone(), el.raw.frame.clone())
        };
        let sig_js = serde_json::to_string(&sig)?;
        let frame_js = serde_json::to_string(&frame)?;
        // Find the element by its CANONICAL sig — the same role()/name()/deepAll
        // scheme the capture script and find_by_sig use (V25c/W2). The prior
        // inline version matched tagName ('a') against the semantic role
        // ('link'), so eval-with-ref was broken for links and most inputs.
        // Then invoke the user's JS with `el` in scope.
        "((sig,frame)=>{".to_string()
            + &crate::page::perception::JS_PREAMBLE
            + "let doc=document;for(const idx of frame){const ifr=[...doc.querySelectorAll('iframe')][idx];if(!ifr)return{__blade_not_found:true};try{doc=ifr.contentDocument;if(!doc)return{__blade_not_found:true};}catch(e){return{__blade_not_found:true};}}"
            + "const all=deepAll(doc,sel);const fps=frame.join(',');const counts={};let el=null;"
            + "for(const n of all){const r=role(n);if(r==='hidden')continue;const nm=name(n,false);const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;const s=fps+'|'+r+'|'+nm+'|'+counts[key];if(s===sig){el=n;break}}"
            + "if(!el)return{__blade_not_found:true};"
            + "const result=((el)=>{return("
            + &js_wrapped
            + ");})(el);return{__blade_result:result===undefined?null:result}})("
            + &sig_js
            + ","
            + &frame_js
            + ")"
    };

    let res = page.cdp_ref().send("Runtime.evaluate", Some(json!({
        "expression": expression,
        "returnByValue": true,
        "awaitPromise": true,
    }))).await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc.get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("JS evaluation failed");
        return Err(BladeError::Other(format!("eval failed: {}", &msg[..msg.len().min(200)])));
    }

    let value = res.get("result").and_then(|r| r.get("value")).cloned();

    if let Some(ref v) = value {
        if v.get("__blade_not_found").and_then(|b| b.as_bool()) == Some(true) {
            return Err(BladeError::ElementNotFound(format!("{ref_id} not found in live DOM")));
        }
        if !ref_id.is_empty() {
            if let Some(inner) = v.get("__blade_result") {
                let json_str = serde_json::to_string_pretty(inner)?;
                return format_eval_result(&json_str).await;
            }
        }
    }

    let json_str = match &value {
        Some(v) => serde_json::to_string_pretty(v)?,
        None => "undefined".to_string(),
    };
    let result = format_eval_result(&json_str).await?;

    // Bug 3: Detect window.open popups. After eval, if the expression
    // contained window.open, check for new page targets and report them.
    // Without --disable-popup-blocking, popups are silently swallowed.
    if js.contains("window.open") || js.contains("open('") || js.contains("open(\"") {
        if let Ok(targets) = page.cdp_ref().send("Target.getTargets", None).await {
            if let Some(infos) = targets.get("targetInfos").and_then(|t| t.as_array()) {
                let page_targets: Vec<&Value> = infos.iter()
                    .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                    .collect();
                if page_targets.len() > 1 {
                    let new_tabs: Vec<String> = page_targets.iter()
                        .filter_map(|t| {
                            let id = t.get("targetId").and_then(|v| v.as_str()).unwrap_or("");
                            let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            if !url.is_empty() && url != "about:blank" {
                                Some(format!("{} → {}", id, &url[..url.len().min(60)]))
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !new_tabs.is_empty() {
                        return Ok(format!("{result}\n\u{2713} popup tabs created: {}\n  use state tabs / switch-tab to access",
                            new_tabs.join(", ")));
                    }
                }
            }
        }
    }

    Ok(result)
}

/// Format an eval result: inline if small, artifact file if big.
async fn format_eval_result(json_str: &str) -> Result<String> {
    const INLINE_CAP: usize = 8000;
    if json_str.len() <= INLINE_CAP {
        Ok(format!("result: {json_str}"))
    } else {
        let path = crate::artifacts::write_artifact(json_str, "json")?;
        let preview: String = json_str.chars().take(1000).collect();
        Ok(format!(
            "result ({} bytes) → {path}\npreview: {preview}…\nread the file for the full result",
            json_str.len()
        ))
    }
}

/// V20: export the current page as a PDF. Page.printToPDF → base64 → decode
/// → artifact file. Optional `path` writes to an explicit location instead.
/// Options: landscape (default false), printBackground (default true),
/// scale (default 1.0, clamped 0.1-2.0).
async fn handle_pdf(page: &mut Page, args: &Value) -> Result<String> {
    let landscape = args.get("landscape").and_then(|v| v.as_bool()).unwrap_or(false);
    let print_bg = args.get("printBackground").and_then(|v| v.as_bool()).unwrap_or(true);
    let scale = args.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0).clamp(0.1, 2.0);

    let res = page.cdp_ref().send("Page.printToPDF", Some(json!({
        "landscape": landscape,
        "printBackground": print_bg,
        "scale": scale,
    }))).await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc.get("exception").and_then(|e| e.get("description"))
            .and_then(|d| d.as_str()).unwrap_or("printToPDF failed");
        return Err(BladeError::Other(format!("pdf failed: {msg}")));
    }
    let data = res.get("data").and_then(|d| d.as_str()).unwrap_or("");
    if data.is_empty() {
        return Err(BladeError::Other("printToPDF returned no data".into()));
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)
        .map_err(|e| BladeError::Other(format!("pdf base64 decode: {e}")))?;

    let path = match args.get("path").and_then(|p| p.as_str()) {
        Some(p) if !p.is_empty() => {
            let pb = std::path::PathBuf::from(p);
            if let Some(parent) = pb.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| BladeError::Other(format!("pdf dir: {e}")))?;
            }
            std::fs::write(&pb, &bytes)
                .map_err(|e| BladeError::Other(format!("pdf write: {e}")))?;
            pb.display().to_string()
        }
        _ => crate::artifacts::write_artifact_bytes(&bytes, "pdf")?,
    };
    Ok(format!("pdf saved: {} ({} bytes)", path, bytes.len()))
}

/// V19: wait for the most recent download to finish and return its path+size.
/// Downloads are routed to a temp dir (M17) and tracked by the download-watch
/// task. `act action=download` after a click that triggers a download blocks
/// until it completes (or `timeout` secs, default 60).
async fn handle_download(page: &mut Page, args: &Value) -> Result<String> {
    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(30);
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");

    if !url.is_empty() {
        // Use fetch + Blob + <a download> to trigger a download without
        // navigating away from the current page. Direct navigation loads
        // PDFs in Chrome's viewer instead of downloading them.
        let js_url = serde_json::to_string(url)
            .map_err(|_| crate::error::BladeError::Other("invalid URL for download".into()))?;
        let js_expr = format!(
            r#"(async function() {{
                try {{
                    const resp = await fetch({js_url});
                    if (!resp.ok) throw new Error('HTTP ' + resp.status);
                    const blob = await resp.blob();
                    const a = document.createElement('a');
                    const objUrl = URL.createObjectURL(blob);
                    a.href = objUrl;
                    const fname = ({js_url}).split('/').pop().split('?')[0].split('#')[0] || 'download';
                    a.download = fname;
                    a.style.display = 'none';
                    document.body.appendChild(a);
                    a.click();
                    setTimeout(() => {{ URL.revokeObjectURL(objUrl); document.body.removeChild(a); }}, 10000);
                    return 'fetch-ok';
                }} catch(e) {{
                    return 'fetch-failed:' + e.message;
                }}
            }})()"#,
        );
        let res = page.cdp_ref().send("Runtime.evaluate", Some(serde_json::json!({
            "expression": js_expr,
            "awaitPromise": true,
            "returnByValue": true,
        }))).await?;
        let outcome = res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("");
        if outcome.starts_with("fetch-failed") {
            let err = &outcome["fetch-failed:".len()..];
            if err.contains("CORS") || err.contains("Failed to fetch") || err.contains("NetworkError") {
                // CORS-protected URL. Fall back to opening a new tab —
                // Chrome may still download it if the server returns
                // Content-Disposition: attachment.
                let create_res = page.cdp_ref().send("Target.createTarget", Some(serde_json::json!({
                    "url": url,
                }))).await?;
                let new_id = create_res.get("targetId").and_then(|v| v.as_str())
                    .ok_or_else(|| crate::error::BladeError::Other("no targetId for download tab".into()))?;
                // Wait for download from global tracker, then close tab.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                let downloads = page.downloads();
                let start_count = {
                    let q = downloads.lock().unwrap_or_else(|e| e.into_inner());
                    q.len()
                };
                let mut found = None;
                loop {
                    {
                        let q = downloads.lock().unwrap_or_else(|e| e.into_inner());
                        if q.len() > start_count {
                            if let Some(d) = q.iter().last() {
                                if d.state == "completed" {
                                    found = Some((d.path.clone(), d.url.clone(), d.received_bytes));
                                    break;
                                }
                            }
                        }
                    }
                    if std::time::Instant::now() >= deadline { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                // Close the download tab regardless of outcome.
                let _ = page.cdp_ref().send("Target.closeTarget", Some(serde_json::json!({
                    "targetId": new_id,
                }))).await;
                match found {
                    Some((path, dl_url, size)) => {
                        return Ok(format!(
                            "download complete: {} ({} bytes)\nfrom: {}",
                            path, size, dl_url
                        ));
                    }
                    None => {
                        return Err(crate::error::BladeError::Other("URL is CORS-protected and Chrome opened it in the viewer instead of downloading.\n\
                             The URL serves content inline (e.g. PDF, HTML) without Content-Disposition: attachment.\n\
                             Workaround: navigate to the page that links to this file, then click the download link:\n\
                             act navigate url=... ; act click text=\"Download\" ; act download".to_string()));
                    }
                }
            }
            return Err(crate::error::BladeError::Other(format!("download failed: {err}")));
        }
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let downloads = page.downloads();
    loop {
        let latest = {
            let q = downloads.lock().unwrap_or_else(|e| e.into_inner());
            q.last().cloned()
        };
        match latest {
            Some(d) if d.state == "completed" => {
                let size = std::fs::metadata(&d.path).map(|m| m.len()).unwrap_or(d.received_bytes);
                return Ok(format!(
                    "download complete: {} ({} bytes)\nfrom: {}",
                    d.path, size, d.url
                ));
            }
            Some(d) if d.state == "canceled" => {
                return Err(BladeError::Other(format!(
                    "download canceled: {} ({})", d.filename, d.url
                )));
            }
            Some(_) => { /* in progress — keep waiting */ }
            None => { /* no download started yet — keep waiting */ }
        }
        if std::time::Instant::now() >= deadline {
            return Err(BladeError::Other(format!(
                "no completed download within {timeout_secs}s. If a download is in progress, retry with a longer timeout."
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

/// Execute one step in a `run` sequence. Handles regular actions (via
/// `build_action` + `page.act`), `navigate` (special: stealth + navigate +
/// wait), `read` (special: returns text, not delta), and `if` (conditional
/// branching with `then`/`else` sub-steps). Recursion supports nested `if`.
///
/// `path` is a display label for the step (e.g. "0" for top-level, "0.1" for
/// the first sub-step inside step 0's branch).
async fn execute_step(
    page: &mut Page,
    step: &Value,
    path: &str,
    observations: &mut Vec<String>,
) -> Result<()> {
    let action_str = step.get("action").and_then(|a| a.as_str()).unwrap_or("");

    match action_str {
        "if" => {
            let condition = step.get("condition").and_then(|c| c.as_str()).unwrap_or("settle");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(5);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let then_steps = step.get("then").and_then(|s| s.as_array()).cloned().unwrap_or_default();
            let else_steps = step.get("else").and_then(|s| s.as_array()).cloned().unwrap_or_default();

            // Evaluate the condition (waits up to timeout).
            let met = crate::action::check_condition(
                page.cdp_ref(),
                condition,
                match_text,
                std::time::Duration::from_secs(timeout_secs),
            )
            .await;

            let (branch, label) = if met {
                (&then_steps, "then")
            } else {
                (&else_steps, "else")
            };

            if branch.is_empty() {
                if met {
                    observations.push(format!("step {path}: if({condition} \"{match_text}\") → then (no steps)"));
                } else {
                    observations.push(format!("step {path}: if({condition} \"{match_text}\") → skipped (timeout {timeout_secs}s)"));
                }
            } else {
                observations.push(format!("step {path}: if({condition} \"{match_text}\") → {label}"));
                // If the condition was met, the page may have just changed —
                // wait for settle before recapturing for fresh refs.
                if met {
                    crate::page::wait_for_settle_with_network(
                        page.cdp_ref(),
                        std::time::Duration::from_secs(1),
                        Some(page.in_flight_ref()),
                    )
                    .await?;
                }
                // Always recapture to get fresh refs for branch steps.
                let _ = page.recapture().await?;
                for (j, sub_step) in branch.iter().enumerate() {
                    let sub_path = format!("{path}.{j}");
                    Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                }
            }
        }
        "while" => {
            let condition = step.get("condition").and_then(|c| c.as_str()).unwrap_or("element");
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(5);
            let max = step.get("max").and_then(|m| m.as_u64()).unwrap_or(10) as usize;
            let body = step.get("steps").and_then(|s| s.as_array()).cloned().unwrap_or_default();
            for i in 0..max {
                let met = crate::action::check_condition(
                    page.cdp_ref(), condition, match_text,
                    std::time::Duration::from_secs(timeout_secs),
                ).await;
                if !met {
                    observations.push(format!("step {path}: while({condition} \"{match_text}\") \u{2192} done after {i} iterations"));
                    break;
                }
                observations.push(format!("step {path}: while({condition} \"{match_text}\") iteration {i}"));
                let _ = page.recapture().await?;
                for (j, sub_step) in body.iter().enumerate() {
                    let sub_path = format!("{path}.{i}.{j}");
                    Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                }
                if i + 1 == max {
                    observations.push(format!("step {path}: while \u{2192} reached max ({max}) iterations"));
                }
            }
        }
        "navigate" => {
            let url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let delta = page.navigate(url).await?;
            if delta.navigated {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let _ = page.recapture().await;
            }
            observations.push(format!("step {path}: {}", page.delta_view(&delta, 4000)));
        }
        "read" => {
            let ref_id = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
            let text_content = crate::action::read_text(page.cdp_ref(), page.model(), ref_id).await?;
            let (role, name) = page
                .model()
                .element(ref_id)
                .map(|e| (e.raw.role.clone(), e.raw.name.clone()))
                .unwrap_or_default();
            let truncated: String = text_content.chars().take(200).collect();
            observations.push(format!(
                "step {path}: read {ref_id} {role} \"{name}\"\n  text: \"{truncated}\""
            ));
        }
        "js" | "eval" => {
            // V7: JS eval step. Result is captured as an
            // observation, capped inline.
            let js_code = step.get("js").and_then(|j| j.as_str())
                .or_else(|| step.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            if js_code.is_empty() {
                return Err(crate::error::BladeError::Other(
                    "js step requires 'js' field".into(),
                ));
            }
            let step_ref = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
            match handle_eval(page, js_code, step_ref).await {
                Ok(result) => {
                    let capped: String = result.chars().take(500).collect();
                    observations.push(format!("step {path}: js → {capped}"));
                }
                // Closed propagates unwrapped so serve() self-heals.
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(2000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} js failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "state" | "open-tab" | "close-tab" | "switch-tab" | "save" | "load" | "cookies" | "set-cookie" => {
            let mut state_args = step.clone();
            if action_str != "state" {
                if let Some(obj) = state_args.as_object_mut() {
                    if !obj.contains_key("op") {
                        obj.insert("op".to_string(), serde_json::Value::String(action_str.to_string()));
                    }
                }
            }
            match handle_state(&state_args, page).await {
                Ok(result) => {
                    let capped: String = result.chars().take(2000).collect();
                    observations.push(format!("step {path}: {capped}"));
                }
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} state failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "download" => {
            match handle_download(page, step).await {
                Ok(result) => {
                    observations.push(format!("step {path}: {result}"));
                }
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(2000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} download failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "collect" => {
            match handle_collect(page, step).await {
                Ok(result) => {
                    observations.push(format!("step {path}: {result}"));
                }
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(2000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} collect failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        _ => {
            // Navigate first if url is given for a non-navigate action.
            let step_url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if !step_url.is_empty() && action_str != "navigate" {
                page.navigate(step_url).await?;
            }
            let action = build_action(step, page).await?;
            match page.act(action).await {
                Ok((delta, verdict)) => {
                    if delta.navigated {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let _ = page.recapture().await;
                    }
                    observations.push(format!("step {path}: {verdict}\n{}", page.delta_view(&delta, 4000)));
                }
                // Closed propagates unwrapped so serve() self-heals.
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Capture a screenshot of the current page and return it as an MCP image
/// content block (base64 PNG).
///
/// This is the `vision` tool (decision D5) — a rare fallback for canvas
/// content, exotic layouts, or when the structural model fails.
async fn handle_vision(
    id: Option<Value>,
    args: &Value,
    page: &mut Page,
) -> std::result::Result<Value, BladeError> {
    let marks = args.get("marks").and_then(|m| m.as_bool()).unwrap_or(false);
    let mut note = String::new();

    if marks {
        // V14: Set-of-Marks. Paint numbered ref badges on
        // visible elements — the refs match the structural
        // model exactly, so a vision-capable agent can say
        // "click e5" and the act tool just works.
        let items: Vec<(String, String)> = page.model().elements()
            .iter()
            .map(|e| (e.ref_id.clone(), e.raw.sig.clone()))
            .collect();
        let items_js = serde_json::to_string(&items)?;
        let overlay = "((items)=>{".to_string()
            + "const d=document;if(!d||!d.body)return 0;"
            + &crate::page::perception::JS_PREAMBLE
            + "const old=d.getElementById('blade-marks');if(old)old.remove();"
            + "const ov=d.createElement('div');ov.id='blade-marks';"
            + "ov.style.cssText='position:fixed;inset:0;pointer-events:none;z-index:2147483647;';"
            + "const vw=innerWidth,vh=innerHeight;"
            + "const all=deepAll(d,sel);"
            + "const sigs=new Map();const counts={};"
            // Sig = '' frame prefix (main doc) | role | shortName | rank,
            // rank counted over ALL matches (vis-failing included) — matches
            // the capture script (V25c). Only vis-passing elements are mapped
            // for marking, but the rank counts everything so sigs agree.
            + "for(let i=0;i<all.length;i++){const n=all[i];const r=role(n);if(r==='hidden')continue;"
            + "const nm=name(n,false);const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;"
            + "if(!vis(n))continue;"
            + "sigs.set('|'+r+'|'+nm+'|'+counts[key],n);}"
            + "let marked=0;"
            + "for(const[ref,sig]of items){const el=sigs.get(sig);if(!el)continue;"
            + "const rect=el.getBoundingClientRect();"
            + "if(rect.bottom<0||rect.top>vh||rect.right<0||rect.left>vw)continue;"
            + "const b=d.createElement('div');b.textContent=ref;"
            + "b.style.cssText='position:fixed;left:'+Math.max(0,rect.x+rect.width/2-12)+'px;top:'+Math.max(0,rect.y+rect.height/2-8)+'px;background:rgba(220,0,110,0.92);color:#fff;font:bold 11px/14px monospace;padding:0 4px;border-radius:3px;border:1px solid #fff;';"
            + "ov.appendChild(b);marked++;}"
            + "d.body.appendChild(ov);return marked;})(" + &items_js + ")";
        let res = page.cdp_ref().send("Runtime.evaluate", Some(json!({
            "expression": overlay,
            "returnByValue": true,
        }))).await?;
        if let Some(exc) = res.get("exceptionDetails") {
            let msg = exc.get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .unwrap_or("overlay failed");
            note = format!(" (marks overlay error: {})", &msg[..msg.len().min(120)]);
        } else {
            let marked = res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_i64()).unwrap_or(0);
            note = format!(" ({marked} elements marked; badge refs match the structural model)");
        }
    }

    let cdp = page.cdp_ref();
    let result = cdp
        .send(
            "Page.captureScreenshot",
            Some(serde_json::json!({
                "format": "png",
            })),
        )
        .await;

    // Remove the overlay BEFORE processing the result —
    // the page must never keep our marks.
    if marks {
        let _ = page.cdp_ref().send("Runtime.evaluate", Some(json!({
            "expression": "(()=>{const o=document.getElementById('blade-marks');if(o)o.remove();return true;})()",
            "returnByValue": true,
        }))).await;
    }

    match result {
        Ok(res) => {
            let data = res.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "Screenshot returned no data." }],
                        "isError": true,
                    }
                }));
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [
                        { "type": "text", "text": format!("screenshot{note}") },
                        {
                            "type": "image",
                            "data": data,
                            "mimeType": "image/png"
                        }
                    ]
                }
            }))
        }
        // Propagate Closed so serve() can self-heal.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("\u{2717} error: {e}") }],
                "isError": true,
            }
        })),
    }
}
