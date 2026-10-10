//! The stdio JSON-RPC 2.0 loop (`serve`): lazy launch on the first
//! `tools/call`, idle shutdown, self-healing relaunch on a dead Chrome, and
//! dead-tab recovery. Split from the `server` core.

use std::io::Write;

use tokio::io::BufReader;

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;

use super::boot::{
    idle_timeout_secs, launch_browser, shutdown_browser, wait_for_shutdown_signal, warm_profile,
};
use super::proto::{handle_discover, handle_initialize, handle_tools_call, handle_tools_list};
use super::{request_version, shape_result, SUPPORTED_VERSIONS};

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
pub(super) async fn serve(use_pipe: bool, host: &str, port: u16) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut input = Vec::new();
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
    // (GL and other degradation advisories live in `browser::alerts` —
    // process-global one-shot latches shared with the daemon lane.)
    let mut stale_warned = false;
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
            line = crate::framing::read_until_limited(&mut reader, b'\n', &mut input, 8 * 1024 * 1024) => {
                let line = match line {
                    Ok(0) => break,
                    Ok(_) => match String::from_utf8(std::mem::take(&mut input)) {
                        Ok(line) => line,
                        Err(e) => {
                            let resp = json!({"jsonrpc":"2.0", "id":null, "error":{"code":-32700, "message":format!("Parse error: invalid UTF-8: {e}")}});
                            writeln!(out, "{resp}")?;
                            out.flush()?;
                            continue;
                        }
                    },
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
                                let retry_safe = super::retry_safe(
                                    params.get("name").and_then(Value::as_str).unwrap_or(""),
                                    &params["arguments"],
                                );
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
                                                        "\u{2717} Browser connection was lost mid-action. Chrome has been restarted (page reset to about:blank). The action's outcome is UNKNOWN — it may have taken effect before the crash. Inspect the result at the destination before deciding whether to repeat the action.".to_string() }],
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
                                        Ok(()) if super::retry_safe(
                                            params.get("name").and_then(Value::as_str).unwrap_or(""),
                                            &params["arguments"],
                                        ) => {
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
                                        Ok(()) => json!({
                                            "jsonrpc": "2.0", "id": id_retry,
                                            "result": {"content": [{"type":"text", "text":"Tab closed mid-action; outcome UNKNOWN. A fresh tab is ready. Inspect the destination before repeating the action."}], "isError":true}
                                        }),
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
                        // Revalidate GL liveness on a bounded cadence: a
                        // mid-session GPU crash degrades the browser to null
                        // WebGL while the recorded verdict still claims GL.
                        // The refresh demotes the state so the advisory below
                        // fires on THIS result instead of the lane silently
                        // serving a broken GL profile.
                        if let Some(p) = page.as_mut() {
                            p.gl_health_refresh().await;
                        }
                        let mut resp = resp;
                        if let Some(result) = resp.get_mut("result") {
                            // Prepend advisory notes to the first text content
                            // block: a relaunch reset the page state, a
                            // degradation was detected (GL loss, headless or
                            // sandbox fallback, ...), and/or this process runs
                            // a replaced binary (the fix is in the file on
                            // disk, not in the running process). Every
                            // applicable one-shot note is appended — a taken
                            // slot must never drop another note.
                            let mut notes: Vec<String> = Vec::new();
                            if let Some(note) = relaunch_note.take() {
                                notes.push(note);
                            }
                            if !stale_warned && crate::platform::stale_binary() {
                                stale_warned = true;
                                notes.push(
                                    "note: this MCP process runs a binary that was replaced on disk — restart the app that spawned it (e.g. opencode) to pick up the new build; this session keeps working meanwhile.".into()
                                );
                            }
                            notes.extend(crate::browser::pending_notes());
                            if !notes.is_empty() {
                                if let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut()) {
                                    content.insert(0, json!({ "type": "text", "text": notes.join("\n") }));
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
                            if let Err(e) = crate::logins::snapshot(p.cdp_ref()).await { eprintln!("[bladebro] login snapshot failed: {e}"); }
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
                // Proactive GL re-probe between tool calls (S2): a degraded
                // GPU usually recovers on its own, and the refresh re-arms
                // the mask on a verdict transition before the agent's next
                // call. Internally rate-limited to once a minute; skips the
                // real lane.
                if browser.is_some() {
                    if let Some(p) = page.as_mut() {
                        if !p.cdp_ref().is_closed() {
                            p.gl_health_refresh().await;
                        }
                    }
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
                                if let Err(e) = crate::logins::snapshot(p.cdp_ref()).await { eprintln!("[bladebro] login snapshot failed: {e}"); }
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
                if let Err(e) = crate::logins::snapshot(p.cdp_ref()).await {
                    eprintln!("[bladebro] login snapshot failed: {e}");
                }
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
