//! The page layer: a [`Page`] handle ties a [`CdpClient`](crate::cdp::CdpClient)
//! connection to a [`LivePageModel`] and exposes the capture / observe loop.
//!
//! `Page` is what the future `act` / `see` / `run` MCP tools will operate on.
//! It owns the LPM across captures so refs stay stable and diffs accumulate.

pub mod intercept;
pub mod model;
pub mod perception;
pub mod refs;

use std::time::Duration;

pub use model::{LivePageModel, PageDelta, PageElement};
pub use perception::{capture, capture_content, dismiss_consent, dismiss_consent_with_stored, detect_block, re_settle, wait_for_load, wait_for_settle, wait_for_settle_with_network, PageCapture, RawElement};
pub use refs::{RefEntry, StateChange, StateProbe};

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use crate::cdp::{CdpClient, CdpSession};
use crate::cdp::list_page_targets;
use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Information about a JavaScript dialog (alert/confirm/prompt/beforeunload)
/// that was auto-dismissed by the dialog handler task.
#[derive(Debug, Clone)]
pub struct DialogInfo {
    /// Dialog type: "alert", "confirm", "prompt", or "beforeunload".
    pub kind: String,
    /// The dialog message text.
    pub message: String,
    /// Default prompt value (for `prompt()` dialogs only).
    pub default_prompt: Option<String>,
    /// Whether the dialog was accepted (true) or cancelled (false).
    /// alert=accepted, confirm/prompt/beforeunload=cancelled.
    pub accepted: bool,
}

/// A tracked download (V19). Updated by the download-watch task as
/// Page.downloadProgress events arrive.
#[derive(Debug, Clone)]
pub struct DownloadInfo {
    /// CDP download guid.
    pub guid: String,
    /// The URL being downloaded.
    pub url: String,
    /// Suggested filename.
    pub filename: String,
    /// "inProgress" | "completed" | "canceled".
    pub state: String,
    /// Bytes received so far.
    pub received_bytes: u64,
    /// Total bytes (0 if unknown).
    pub total_bytes: u64,
    /// Final path on disk (downloadPath/filename).
    pub path: String,
}

/// A live page session: one CDP connection + its persistent Live Page Model.
/// A completed/failed network request record (V8 introspection).
#[derive(Debug, Clone)]
pub struct NetEntry {
    pub method: String,
    pub url: String,
    /// HTTP status (0 = failed/no response).
    pub status: i64,
    /// Failure reason if the request failed.
    pub error: Option<String>,
}

/// An XHR/fetch request observed at START by the tracker (introspection).
/// Unlike `NetEntry` (pushed on completion), entries appear while still in
/// flight, and the URL is kept in FULL: API URLs carry their query state
/// (graphql operations, cursors, tokens) and truncation would defeat the
/// purpose. Small ring (128) — a working window, not a ledger.
#[derive(Debug, Clone)]
pub struct XhrEntry {
    pub id: String,
    pub method: String,
    pub url: String,
    /// HTTP status; 0 while in flight or on failure.
    pub status: i64,
    /// True once loading finished or failed (status/error meaningful).
    pub done: bool,
    /// Failure reason if the request failed.
    pub error: Option<String>,
    /// Auth/content header subset (`authorization`, `x-csrf-token`,
    /// `x-twitter-*`, `content-type`) — replayed verbatim by adapters.
    pub headers: Vec<(String, String)>,
}

/// Dedup key for the XHR ring: origin+path, query stripped. Repeats of the
/// same endpoint (telemetry beacons, polling) collapse into one entry so
/// the endpoints that matter are never evicted by noise.
fn xhr_key(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or("")
}

/// Media/opaque noise for the introspection ring: MSE/blob playback
/// segments (each retry mints a fresh UUID — they flooded the ring and
/// evicted API entries), data: URLs, and HLS/DASH chunk extensions.
fn is_media_url(url: &str) -> bool {
    if url.starts_with("blob:") || url.starts_with("data:") {
        return true;
    }
    let pl = url.split('?').next().unwrap_or("").to_ascii_lowercase();
    [".m4s", ".m4a", ".mp4", ".m4v", ".mpd", ".webm", ".vtt", ".ts"]
        .iter()
        .any(|e| pl.ends_with(e))
}

///
/// Also owns a background dialog-handler task that auto-dismisses
/// alert()/confirm()/prompt() dialogs so the page never deadlocks.
pub struct Page {
    cdp: CdpSession,
    /// Browser-level connection for target listing in pipe mode (S1). In WS
    /// mode this is `None` and tabs are listed over the HTTP debug endpoint.
    browser_client: Option<CdpClient>,
    lpm: LivePageModel,
    /// Queue of auto-dismissed dialogs, drained by the MCP server after each
    /// tool call and appended to the agent-facing result.
    dialogs: Arc<Mutex<Vec<DialogInfo>>>,
    /// Handle to the dialog-handler background task. Aborted on Drop so the
    /// task's CdpClient clone is released, allowing the connection to close.
    dialog_task: Option<tokio::task::JoinHandle<()>>,
    /// Count of in-flight network requests (for settle + header display).
    in_flight: Arc<AtomicUsize>,
    /// Ring buffer of the last 50 completed/failed requests (V8).
    net_log: Arc<Mutex<std::collections::VecDeque<NetEntry>>>,
    /// Ring of the last 128 XHR/fetch requests (start-observed, full URLs) —
    /// API introspection used by site fast paths to read the page's own API
    /// calls (query ids, cursors) and replay them.
    xhr_log: Arc<Mutex<std::collections::VecDeque<XhrEntry>>>,
    /// Handle to the network-tracker background task. Aborted on Drop.
    network_task: Option<tokio::task::JoinHandle<()>>,
    /// Ambient events (consent dismissed, block detected) for the agent.
    ambient: Arc<Mutex<Vec<String>>>,
    /// `host:port` for HTTP target discovery (new-tab detection).
    base: String,
    /// S5: epoch millis of the last action completion — drives pacing.
    last_action_epoch: Arc<AtomicU64>,
    /// S4: true during action execution — hum pauses while busy.
    is_busy: Arc<AtomicBool>,
    /// S4: idle-hum background task. Aborted on Drop.
    hum_task: Option<tokio::task::JoinHandle<()>>,
    /// Worker/OOPIF auto-attach handler (D22 + v3.9): injects the GL spoof
    /// into worker sessions, the full stealth script into out-of-process
    /// iframes, and resumes every attached target. Aborted on Drop.
    worker_task: Option<tokio::task::JoinHandle<()>>,
    /// Active stealth-injection registration — swapped (not stacked) when a
    /// per-domain profile changes the locale (S11 coherence).
    stealth_script_id: Option<crate::stealth::ScriptId>,
    /// Locale the current injection bakes in (None = no override).
    active_locale: Option<String>,
    /// Request-interception state shared with the Fetch task
    /// (block-class bitmask + page domain for third-party checks).
    intercept: intercept::InterceptState,
    /// Request-interception task handle.
    intercept_task: Option<tokio::task::JoinHandle<()>>,
    /// Tracked downloads (V19), updated by the download-watch task. Newest
    /// last. `act action=download` waits on the newest entry.
    downloads: std::sync::Arc<Mutex<Vec<DownloadInfo>>>,
    /// Download-watch task handle, aborted on shutdown.
    download_task: Option<tokio::task::JoinHandle<()>>,
    /// Domain knowledge base (consent selectors, block configs, timing).
    /// Set by the MCP server after attach. None during attach (cold start).
    knowledge: Option<crate::knowledge::SharedKnowledge>,
    /// Last mouse position — shared between action dispatch and idle hum.
    /// Used to calculate movementX/movementY deltas for behavioral biometrics.
    /// PerimeterX/HUMAN specifically tracks these coordinate deltas; missing
    /// or always-zero values are an instant bot flag.
    last_mouse: Arc<std::sync::Mutex<Option<(f64, f64)>>>,
    /// Isolated world execution context ID for DOM operations. None until
    /// lazily created. Reset on navigation. In the isolated world, DOM
    /// methods are native (not patched by anti-bot scripts), and
    /// Error.stack traces don't contain main-world eval frames.
    isolated_ctx: Arc<std::sync::Mutex<Option<i64>>>,
    /// Context pruning: how many `act` calls have happened on the current
    /// page without a `see` call or navigation to reset it. Used to
    /// progressively compress responses after turn 3.
    act_count: std::sync::atomic::AtomicU32,
    /// Context pruning: enabled by default, toggled via
    /// `BLADE_NO_COMPRESS=1` env var or `state compress off`.
    compress_enabled: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("cdp", &self.cdp)
            .field("lpm", &self.lpm)
            .finish_non_exhaustive()
    }
}

impl Page {
    /// Attach to an existing page target over `cdp`, enable the core domains,
    /// and run an initial capture to seed the model.
    /// `browser_client` is the browser-level connection in pipe mode (S1) —
    /// used for tab listing since pipe mode has no HTTP debug endpoint.
    pub async fn attach(cdp: CdpSession, base: &str, browser_client: Option<CdpClient>) -> Result<Self> {
        #[allow(unused_assignments)]
        let mut worker_task: Option<tokio::task::JoinHandle<()>> = None;
        cdp.enable("Page").await?;
        // DO NOT enable Runtime — DataDome's detection leverages the fact that
        // `Runtime.enable` changes console buffering behavior, making
        // `console.log(new Error())` trigger serialization (and thus getter
        // calls on Error.stack) only when CDP is connected. We can still use
        // `Runtime.evaluate` without enabling the domain — evaluate is a
        // standalone command, enable only turns on event notifications.
        cdp.enable("Network").await?;
        cdp.enable("DOM").await?;
        // UA override: only needed in headless mode where the UA contains
        // "HeadlessChrome". In headful mode (Xvfb or native), the real UA
        // is already correct. Read the real UA + platform, and if headless,
        // replace just "HeadlessChrome" with "Chrome" — preserving the real
        // Chrome version and OS. This is a CDP-level override, not JS.
        let ua_info = cdp.send("Runtime.evaluate", Some(serde_json::json!({
            "expression": "JSON.stringify({ua:navigator.userAgent,plt:navigator.platform,wd:navigator.webdriver})",
            "returnByValue": true,
        }))).await.ok()
            .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from))
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

        // Real-browser lane honesty probe (measured, Chrome 151.0.7922.108):
        // a browser armed with an EPHEMERAL `--remote-debugging-port=0` (or
        // via Chrome's chrome://inspect approval flow) reports
        // `navigator.webdriver === true` natively — Chromium's own behavior,
        // not a bladebro patch (fixed-port arms report false). The lane's
        // contract forbids masking it, so surface it once instead of letting
        // the agent discover it from a page.
        if crate::realbrowser::real_lane()
            && ua_info
                .as_ref()
                .and_then(|v| v.get("wd").and_then(|w| w.as_bool()))
                == Some(true)
        {
            eprintln!(
                "{} note: this browser reports navigator.webdriver=true — Chrome does \
                 that itself for an ephemeral debug port (`--remote-debugging-port=0`) or the \
                 chrome://inspect approval flow; the lane never masks it. For stealth-critical \
                 work, arm with a FIXED --remote-debugging-port, or use clone/profile mode.",
                crate::ui::dim("[realbrowser]")
            );
        }

        let need_override = ua_info.as_ref()
            .and_then(|v| v.get("ua").and_then(|u| u.as_str()))
            .map(|ua| ua.contains("HeadlessChrome"))
            .unwrap_or(true);

        // Real-browser lane: no UA rewriting — if the lane runs headless it
        // honestly reports HeadlessChrome. Masks are exactly what this lane
        // deletes.
        if need_override && !crate::realbrowser::real_lane() {
            let real_ua = ua_info.as_ref()
                .and_then(|v| v.get("ua").and_then(|u| u.as_str()))
                .unwrap_or({
                    #[cfg(target_arch = "aarch64")]
                    { "Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36" }
                    #[cfg(not(target_arch = "aarch64"))]
                    { "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36" }
                });
            let fixed_ua = real_ua.replace("HeadlessChrome", "Chrome");
            let real_platform = ua_info.as_ref()
                .and_then(|v| v.get("plt").and_then(|p| p.as_str()))
                .unwrap_or({
                    #[cfg(target_arch = "aarch64")]
                    { "Linux aarch64" }
                    #[cfg(not(target_arch = "aarch64"))]
                    { "Linux x86_64" }
                });

            // Override ONLY userAgent + platform. The previous version also
            // sent a hand-built userAgentMetadata with a hardcoded GREASE
            // brand ("Not:A-Brand"/99) that matched no real Chrome build —
            // the real Sec-CH-UA headers would never equal it. Chrome keeps
            // generating coherent metadata (brands, GREASE, full versions)
            // from its true version, which matches the fixed UA string
            // (only "HeadlessChrome" → "Chrome" differs).
            if let Err(e) = cdp.send("Network.setUserAgentOverride", Some(serde_json::json!({
                "userAgent": fixed_ua,
                "platform": real_platform,
            }))).await {
                eprintln!["[bladebro] WARNING: UA override failed: {e}"];
            }
        }
        // S6: geo-consistent identity — timezone and locale must match the
        // proxy's geographic location. Without a proxy, the system timezone
        // is already correct. BLADE_TZ and BLADE_LOCALE override explicitly.
        // Real-browser lane: skipped — both are page-visible masks, and this
        // lane's contract is that nothing page-visible is manufactured.
        if !crate::realbrowser::real_lane() {
            if let Ok(tz) = std::env::var("BLADE_TZ") {
                if !tz.is_empty() {
                    if let Err(e) = cdp.send("Emulation.setTimezoneOverride",
                        Some(serde_json::json!({ "timezoneId": tz }))).await
                    {
                        eprintln!["[bladebro] WARNING: timezone override failed: {e}"];
                    } else {
                        eprintln!["[bladebro] timezone override: {tz}"];
                    }
                }
            } else if std::env::var("BLADE_PROXY").is_ok() {
                eprintln!["[bladebro] WARNING: BLADE_PROXY set but BLADE_TZ not set — timezone/IP mismatch will be detected"];
            }
            if let Ok(locale) = std::env::var("BLADE_LOCALE") {
                if !locale.is_empty() {
                    let base = locale.split('-').next().unwrap_or(&locale).to_string();
                    let _ = cdp.send("Emulation.setLocaleOverride",
                        Some(serde_json::json!({ "locale": locale }))).await;
                    let _ = cdp.send("Network.setExtraHTTPHeaders",
                        Some(serde_json::json!({
                            "headers": { "Accept-Language": format!("{locale},{base};q=0.9") }
                        }))).await;
                    eprintln!["[bladebro] locale override: {locale}"];
                }
            }
        }
        // Inject the stealth script before any page loads. This runs at
        // document_start on every new document, removing CDP artifacts.
        let stealth_script_id = match crate::stealth::apply_stealth(&cdp, None).await {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!["[bladebro] WARNING: stealth injection failed: {e}"];
                None
            }
        };
        let active_locale = std::env::var("BLADE_LOCALE").ok().filter(|s| !s.is_empty());
        wait_for_load(&cdp, Duration::from_secs(10)).await?;

        // D22: Worker GL spoof — inject the GL spoof into Worker contexts too.
        // The main page GL spoof (via Page.addScriptToEvaluateOnNewDocument)
        // does NOT reach Worker contexts. Without this, Pixelscan/Creepjs
        // compare page GL (spoofed Intel) vs worker GL (real SwiftShader) and
        // flag "masking detected". We use CDP Target.setAutoAttach to inject
        // invisibly — no JS-level Worker constructor patching (which broke
        // sites via blob URLs in the previous attempt).
        //
        // v3.9 CRITICAL fixes here:
        // - EVERY auto-attached target is resumed (Runtime.runIfWaitingForDebugger).
        //   The old handler only resumed workers — cross-origin iframe (OOPIF)
        //   targets were attached with waitForDebuggerOnStart and then paused
        //   FOREVER: every embedded reCAPTCHA/Stripe/YouTube iframe froze.
        // - OOPIF child sessions get the FULL stealth script (they are
        //   separate targets; Page.addScriptToEvaluateOnNewDocument on the
        //   main session never reaches them).
        // - The task handle is stored on Page and aborted on Drop — it used
        //   to leak per switch_tab, with stale handlers racing the new one.
        // - broadcast Lagged is RECOVERABLE (continue), not fatal — a burst
        //   used to kill the handler, freezing every later worker.
        let worker_gl = crate::stealth::worker_gl_spoof(active_locale.as_deref());
        if worker_gl.is_some() || crate::stealth::has_full_script() {
            let _ = cdp.send("Target.setAutoAttach", Some(serde_json::json!({
                "autoAttach": true,
                "flatten": true,
                "waitForDebuggerOnStart": true
            }))).await;
            let client = cdp.client().clone();
            let worker_script = worker_gl.clone();
            let full_script = crate::stealth::full_script();
            let dbg = std::env::var("BLADE_DBG_WORKERS").is_ok();
            worker_task = Some(tokio::spawn(async move {
                let sub = client.subscribe();
                let mut rx = sub;
                loop {
                    match rx.recv().await {
                        Ok(event) if event.method == "Target.attachedToTarget" => {
                            let target_info = event.params.get("targetInfo");
                            let target_type = target_info
                                .and_then(|t| t.get("type"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            let session_id = event.params
                                .get("sessionId")
                                .and_then(|s| s.as_str())
                                .unwrap_or("");
                            if session_id.is_empty() {
                                continue;
                            }
                            if dbg {
                                eprintln!("[workers] attach type={target_type} sid={session_id}");
                            }
                            let worker_session = CdpSession::child(client.clone(), session_id);
                            // Runtime.evaluate against a PAUSED service worker
                            // deadlocks — its execution context only exists once
                            // the script runs — and the 30s command timeout then
                            // froze this whole handler (every later target stayed
                            // paused; live effect: hung SW registration and
                            // CreepJS's worker card reading `blocked`). Service
                            // workers are resumed FIRST, then patched best-effort.
                            let is_sw = target_type == "service_worker";
                            if is_sw {
                                let res = worker_session.send("Runtime.runIfWaitingForDebugger", None).await;
                                if dbg {
                                    eprintln!("[workers] resume(sw-first) {target_type}: {}", if res.is_ok() { "ok".to_string() } else { format!("ERR {:?}", res.err()) });
                                }
                            }
                            match target_type {
                                "worker" | "shared_worker" => {
                                    if let Some(ref script) = worker_script {
                                        // Bounded: a pathological target must not
                                        // stall the attach pipeline.
                                        let res = worker_session.send_with_timeout("Runtime.evaluate", Some(serde_json::json!({
                                            "expression": script,
                                            "returnByValue": true,
                                        })), std::time::Duration::from_secs(5)).await;
                                        if dbg {
                                            eprintln!("[workers] eval {target_type}: {}", if res.is_ok() { "ok".to_string() } else { format!("ERR {:?}", res.err()) });
                                        }
                                    }
                                }
                                "service_worker" => {
                                    // Already resumed above. The SW realm has no
                                    // WebGL; this only matters for the locale
                                    // patch — best-effort, bounded.
                                    if let Some(ref script) = worker_script {
                                        let res = worker_session.send_with_timeout("Runtime.evaluate", Some(serde_json::json!({
                                            "expression": script,
                                            "returnByValue": true,
                                        })), std::time::Duration::from_secs(3)).await;
                                        if dbg {
                                            eprintln!("[workers] eval {target_type}: {}", if res.is_ok() { "ok".to_string() } else { format!("ERR {:?}", res.err()) });
                                        }
                                    }
                                }
                                "iframe" | "oopif" => {
                                    // Full stealth into out-of-process frames:
                                    // document_start semantics via evaluate
                                    // before resume, so the frame's scripts
                                    // run against the patched environment.
                                    if let Some(ref script) = full_script {
                                        let _ = worker_session.send_with_timeout("Runtime.evaluate", Some(serde_json::json!({
                                            "expression": script,
                                            "returnByValue": true,
                                        })), std::time::Duration::from_secs(5)).await;
                                    }
                                }
                                _ => {}
                            }
                            // ALWAYS resume — an unresumed target stays frozen.
                            // (Service workers were already resumed above.)
                            if !is_sw {
                                let res = worker_session.send("Runtime.runIfWaitingForDebugger", None).await;
                                if dbg {
                                    eprintln!("[workers] resume {target_type}: {}", if res.is_ok() { "ok".to_string() } else { format!("ERR {:?}", res.err()) });
                                }
                            }
                        }
                        Ok(_) => {}
                        // Lagged: we skipped events but the connection is
                        // alive. Continue — never leave future targets paused.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            if dbg {
                                eprintln!("[workers] LAGGED {n} events dropped");
                            }
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            }));
        }

        // M17: Set download behavior so downloads don't hang the browser.
        // SECURITY: ~/.blade/downloads (0700) instead of a shared predictable
        // /tmp dir — downloads can contain private documents, and a shared
        // /tmp/bladebro-downloads leaked them across local users (and was a
        // symlink pre-placement target). The absolute path is returned to
        // the agent in the download result, so nothing depends on /tmp.
        let download_dir = crate::platform::blade_dir().join("downloads");
        let _ = crate::platform::secure_create_dir_all(&download_dir);
        let _ = cdp.send("Page.setDownloadBehavior", Some(serde_json::json!({
            "behavior": "allow",
            "downloadPath": download_dir.display().to_string(),
        }))).await;

        // Spawn the dialog-handler background task. It subscribes to
        // `Page.javascriptDialogOpening` events and auto-dismisses them so
        // the page never deadlocks on alert()/confirm()/prompt(). Dismissed
        // dialog info is queued for the MCP server to surface to the agent.
        //
        // Auto-dismiss strategy: alert=accept (only option),
        // confirm/prompt/beforeunload=cancel (safer — don't accidentally
        // confirm destructive actions).
        let dialogs: Arc<Mutex<Vec<DialogInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let cdp_for_dialogs = cdp.clone();
        let dq = dialogs.clone();
        let dialog_task = tokio::spawn(async move {
            let mut rx = cdp_for_dialogs.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Page.javascriptDialogOpening" => {
                        let kind = ev
                            .params
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("alert")
                            .to_string();
                        let message = ev
                            .params
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let default_prompt = ev
                            .params
                            .get("defaultPrompt")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        // alert=accept; confirm/prompt=cancel (safe default).
                        // beforeunload=accept: the agent ISSUED the navigation —
                        // cancelling it would silently block every nav away
                        // from a dirty form.
                        let accepted = kind == "alert" || kind == "beforeunload";
                        let _ = cdp_for_dialogs
                            .send(
                                "Page.handleJavaScriptDialog",
                                Some(serde_json::json!({ "accept": accepted })),
                            )
                            .await;
                        if let Ok(mut q) = dq.lock() {
                            q.push(DialogInfo {
                                kind,
                                message,
                                default_prompt,
                                accepted,
                            });
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Spawn the network-tracker task: counts in-flight requests so settle
        // can wait for data to arrive, not just DOM stability.
        let ambient: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        // V19: download-watch task. Page.setDownloadBehavior (M17, above) already
        // routes downloads to download_dir and enables Page.downloadWillBegin /
        // Page.downloadProgress events. Track each download's state and surface
        // a `download started` ambient note so a click that triggers a download
        // is never silent.
        let downloads: Arc<Mutex<Vec<DownloadInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let cdp_for_dl = cdp.clone();
        let dlq = downloads.clone();
        let dl_ambient = ambient.clone();
        let dl_dir = download_dir.clone();
        let download_task = tokio::spawn(async move {
            let mut rx = cdp_for_dl.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Page.downloadWillBegin" => {
                        let guid = ev.params.get("guid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let url = ev.params.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let filename = ev.params.get("suggestedFilename").and_then(|v| v.as_str()).unwrap_or("download").to_string();
                        let path = dl_dir.join(&filename).display().to_string();
                        if let Ok(mut q) = dlq.lock() {
                            q.push(DownloadInfo {
                                guid, url, filename: filename.clone(),
                                state: "inProgress".into(),
                                received_bytes: 0, total_bytes: 0, path,
                            });
                            if q.len() > 50 { let n = q.len() - 50; q.drain(0..n); }
                        }
                        if let Ok(mut a) = dl_ambient.lock() {
                            a.push(format!("download started: {filename}"));
                        }
                    }
                    Ok(ev) if ev.method == "Page.downloadProgress" => {
                        let guid = ev.params.get("guid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let state = ev.params.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let received = ev.params.get("receivedBytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let total = ev.params.get("totalBytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        if let Ok(mut q) = dlq.lock() {
                            if let Some(d) = q.iter_mut().find(|d| d.guid == guid) {
                                d.state = state;
                                d.received_bytes = received;
                                d.total_bytes = total;
                            }
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let in_flight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let net_log: Arc<Mutex<std::collections::VecDeque<NetEntry>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let xhr_log: Arc<Mutex<std::collections::VecDeque<XhrEntry>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let cdp_for_net = cdp.clone();
        let net_counter = in_flight.clone();
        let net_log_t = net_log.clone();
        let xhr_log_t = xhr_log.clone();
        let network_task = tokio::spawn(async move {
            use std::collections::HashMap;
            let mut rx = cdp_for_net.subscribe();
            // Track request IDs with timestamps: requestWillBeSent fires
            // once per REDIRECT HOP for the same requestId while
            // loadingFinished fires once — a counter drifts +1 per hop and
            // eventually every settle waits the full timeout.
            // Timestamps let us sweep stale entries (data URLs, long-poll,
            // server-sent events that never fire loadingFinished).
            let mut open: std::collections::HashMap<String, std::time::Instant> = std::collections::HashMap::new();
            // Pending request metadata for the V8 net log.
            let mut pending: HashMap<String, (String, String, i64)> = HashMap::new();
            let mut last_sweep = std::time::Instant::now();
            let mut sweep_tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                // Sweep on a timer too: a stuck request must expire even when
                // no further network events arrive to wake the loop (a pinned
                // counter made every settle pay its drain plateau).
                let msg = tokio::select! {
                    m = rx.recv() => Some(m),
                    _ = sweep_tick.tick() => None,
                };
                match msg {
                    Some(Ok(ev)) if ev.method == "Network.requestWillBeSent" => {
                        // WebSocket/EventSource "requests" never fire
                        // loadingFinished — counting them pinned the in-flight
                        // counter indefinitely on any page holding a socket.
                        let ty = ev.params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let id = ev.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() && ty != "WebSocket" && ty != "EventSource" {
                            open.insert(id.to_string(), std::time::Instant::now());
                            let req = ev.params.get("request");
                            let method = req.and_then(|r| r.get("method")).and_then(|m| m.as_str()).unwrap_or("GET").to_string();
                            let url = req.and_then(|r| r.get("url")).and_then(|u| u.as_str()).unwrap_or("").to_string();
                            if (ty == "XHR" || ty == "Fetch") && !url.is_empty() && !is_media_url(&url) {
                                if url.contains("/i/api/graphql/") {
                                    tracing::debug!("xhr gql: {} {}", method, url);
                                }
                                let mut hdrs: Vec<(String, String)> = Vec::new();
                                if let Some(h) = req.and_then(|r| r.get("headers")).and_then(|h| h.as_object()) {
                                    for (k, v) in h {
                                        let kl = k.to_ascii_lowercase();
                                        // Keep every header except transport noise the
                                        // browser regenerates itself (cookie, UA,
                                        // encodings, sizes, sec-*). X validates per-request
                                        // client headers on some ops (search) but not
                                        // others — a partial copy silently 404s replays.
                                        if kl != "cookie"
                                            && kl != "host"
                                            && kl != "content-length"
                                            && kl != "accept-encoding"
                                            && kl != "connection"
                                            && kl != "user-agent"
                                            && !kl.starts_with("sec-")
                                        {
                                            if let Some(s) = v.as_str() {
                                                hdrs.push((k.clone(), s.to_string()));
                                            }
                                        }
                                    }
                                }
                                if let Ok(mut log) = xhr_log_t.lock() {
                                    let key = xhr_key(&url);
                                    if let Some(pos) = log.iter().position(|e| xhr_key(&e.url) == key) {
                                        log.remove(pos);
                                    }
                                    log.push_back(XhrEntry {
                                        id: id.to_string(),
                                        method: method.clone(),
                                        url: url.clone(),
                                        status: 0,
                                        done: false,
                                        error: None,
                                        headers: hdrs,
                                    });
                                    if log.len() > 128 {
                                        log.pop_front();
                                    }
                                }
                            }
                            pending.insert(id.to_string(), (method, url, 0));
                        }
                    }
                    Some(Ok(ev)) if ev.method == "Network.responseReceived" => {
                        let id = ev.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                        let status = ev.params.get("response")
                            .and_then(|r| r.get("status"))
                            .and_then(|s| s.as_i64())
                            .unwrap_or(0);
                        if let Some(entry) = pending.get_mut(id) {
                            entry.2 = status;
                        }
                        if status > 0 {
                            if let Ok(mut log) = xhr_log_t.lock() {
                                if let Some(e) = log.iter_mut().rev().find(|e| e.id == id) {
                                    e.status = status;
                                }
                            }
                        }
                    }
                    Some(Ok(ev)) if ev.method == "Network.loadingFinished"
                        || ev.method == "Network.loadingFailed" =>
                    {
                        let id = ev.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() {
                            open.remove(id);
                                if let Ok(mut log) = xhr_log_t.lock() {
                                if let Some(e) = log.iter_mut().rev().find(|e| e.id == id) {
                                    e.done = true;
                                    if ev.method == "Network.loadingFailed" {
                                        e.error = Some(ev.params.get("errorText")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("failed")
                                            .to_string());
                                    }
                                    if e.url.contains("/i/api/graphql/") {
                                        let st = e.error.clone().unwrap_or_else(|| e.status.to_string());
                                        tracing::debug!(
                                            "xhr gql done: {} {} -> {}",
                                            e.method,
                                            e.url.split('?').next().unwrap_or("").replace("https://x.com", ""),
                                            st
                                        );
                                    }
                                }
                            }
                            if let Some((method, url, status)) = pending.remove(id) {
                                let error = if ev.method == "Network.loadingFailed" {
                                    Some(ev.params.get("errorText")
                                        .and_then(|e| e.as_str())
                                        .unwrap_or("failed")
                                        .to_string())
                                } else {
                                    None
                                };
                                if let Ok(mut log) = net_log_t.lock() {
                                    log.push_back(NetEntry { method, url, status, error });
                                    if log.len() > 50 { log.pop_front(); }
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        // Events were dropped — the set may now hold stale IDs.
                        // Clear rather than risk a permanently blocked settle.
                        open.clear();
                        pending.clear();
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                    None => {}
                }
                // Periodic sweep: remove entries older than 8 seconds.
                // Data URLs, long-poll connections, and server-sent events
                // may never fire loadingFinished. 8s is the in-flight horizon:
                // a legit resource still loading after that is a download, not
                // a page-load blocker.
                if last_sweep.elapsed().as_secs() >= 1 {
                    let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(8);
                    open.retain(|_, ts| *ts > cutoff);
                    last_sweep = std::time::Instant::now();
                }
                net_counter.store(open.len(), std::sync::atomic::Ordering::Relaxed);
            }
        });

        let lpm = LivePageModel::new();
        // M4+M6: Check for consent banners and block pages after initial load.
        let consent = dismiss_consent(&cdp).await.unwrap_or(None);
        let blocked = detect_block(&cdp).await.unwrap_or(None);
        if let Some(ref fw) = consent {
            if let Ok(mut a) = ambient.lock() {
                a.push(format!("consent: {} ({})", if std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".into()) != "accept" { "rejected" } else { "accepted" }, fw));
            }
        }
        if let Some(ref bt) = blocked {
            if let Ok(mut a) = ambient.lock() {
                a.push(format!("blocked: {}", bt));
                for step in crate::page::perception::remediation_ladder(bt) {
                    a.push(format!("  remediation: {}", step));
                }
            }
        }

        // S4+S5: shared action state for pacing + idle hum.
        let last_action_epoch = Arc::new(AtomicU64::new(0));
        let is_busy = Arc::new(AtomicBool::new(false));
        let last_mouse = Arc::new(std::sync::Mutex::new(None));
        let hum_task = crate::stealth::spawn_hum(
            cdp.clone(),
            last_action_epoch.clone(),
            is_busy.clone(),
            last_mouse.clone(),
        );

        // Request-interception task: answers Fetch.requestPaused events
        // with block/continue verdicts. Only receives events while
        // Fetch is enabled (blocking active); idle otherwise.
        let intercept = intercept::InterceptState::default();
        let intercept_task = tokio::spawn(intercept::run_interception(
            cdp.clone(),
            intercept.clone(),
            cdp.subscribe(),
        ));

        // Construct the Page BEFORE the last fallible step (capture):
        // on error the returned Err drops `page`, whose Drop aborts all
        // background tasks. The old `capture(...)?` leaked all five
        // spawned tasks on failure.
        let mut page = Self {
            cdp,
            browser_client,
            lpm,
            dialogs,
            dialog_task: Some(dialog_task),
            in_flight,
            net_log,
            xhr_log,
            network_task: Some(network_task),
            ambient,
            base: base.to_string(),
            last_action_epoch,
            is_busy,
            hum_task: Some(hum_task),
            worker_task,
            stealth_script_id,
            active_locale,
            intercept,
            intercept_task: Some(intercept_task),
            downloads,
            download_task: Some(download_task),
            knowledge: None,
            last_mouse,
            isolated_ctx: Arc::new(std::sync::Mutex::new(None)),
            act_count: std::sync::atomic::AtomicU32::new(0),
            compress_enabled: std::sync::atomic::AtomicBool::new(
                std::env::var("BLADE_NO_COMPRESS").as_deref() != Ok("1")
            ),
        };

        let cap = capture(&page.cdp).await?;
        page.lpm.ingest(cap);
        Ok(page)
    }

    /// The download tracker (V19). Newest download last.
    pub fn downloads(&self) -> std::sync::Arc<Mutex<Vec<DownloadInfo>>> {
        self.downloads.clone()
    }

    /// Set the domain knowledge base. Called by the MCP server after attach.
    /// Enables consent auto-apply, visit tracking, and cross-session learning.
    pub fn set_knowledge(&mut self, kb: crate::knowledge::SharedKnowledge) {
        self.knowledge = Some(kb);
    }

    // ---- Context pruning helpers ----

    /// Current act turn count on this page (resets on navigation/see/error).
    pub fn act_turn(&self) -> u32 {
        self.act_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Increment the act turn counter (called after each non-navigate act).
    pub fn incr_act_turn(&self) {
        self.act_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reset the act turn counter to 0 (called on navigation, see, or error).
    pub fn reset_act_turn(&self) {
        self.act_count.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Is context pruning enabled?
    pub fn compress_enabled(&self) -> bool {
        self.compress_enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Toggle context pruning on/off at runtime.
    pub fn set_compress_enabled(&self, enabled: bool) {
        self.compress_enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Shared last-mouse-position tracker for movementX/movementY calculation.
    /// Used by action dispatch and idle hum to produce realistic mouse deltas.
    pub fn last_mouse(&self) -> Arc<std::sync::Mutex<Option<(f64, f64)>>> {
        self.last_mouse.clone()
    }

    /// Evaluate a JS expression in an isolated world. The isolated world has
    /// DOM access but its own JavaScript context — anti-bot scripts in the
    /// main world cannot observe our queries via patched DOM methods or
    /// Error.stack frames. Lazily creates the world; recreates on navigation.
    /// Falls back to regular Runtime.evaluate if the isolated world fails.
    pub async fn eval_isolated(&self, expr: &str) -> Result<Value> {
        // Try to get or create the isolated world context.
        let ctx_id = {
            let guard = self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner());
            *guard
        };

        if let Some(id) = ctx_id {
            // Try the isolated world first.
            let res = self.cdp.send("Runtime.callFunctionOn", Some(json!({
                "executionContextId": id,
                "functionDeclaration": format!("function() {{ return ({expr}); }}"),
                "returnByValue": true,
                "awaitPromise": true,
            }))).await;

            match res {
                Ok(v) => return Ok(v),
                Err(_) => {
                    // Context is stale (navigation) — clear and recreate.
                    *self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
        }

        // Lazily create the isolated world.
        let frame_id = self.cdp.send("Page.getFrameTree", None).await
            .ok()
            .and_then(|v| v.get("frameTree")?.get("frame")?.get("id")?.as_str().map(String::from))
            .unwrap_or_default();
        if let Ok(v) = self.cdp.send("Page.createIsolatedWorld", Some(json!({
            "frameId": frame_id,
            "worldName": "",
            "grantUniveralAccess": true,
        }))).await {
            if let Some(id) = v.get("executionContextId").and_then(|i| i.as_i64()) {
                *self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                return self.cdp.send("Runtime.callFunctionOn", Some(json!({
                    "executionContextId": id,
                    "functionDeclaration": format!("function() {{ return ({expr}); }}"),
                    "returnByValue": true,
                    "awaitPromise": true,
                }))).await;
            }
        }

        // Fallback: regular Runtime.evaluate.
        self.cdp.send("Runtime.evaluate", Some(json!({
            "expression": expr,
            "returnByValue": true,
            "awaitPromise": true,
        }))).await
    }

    /// Reset the isolated world context (call on navigation).
    pub fn reset_isolated(&self) {
        *self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// List the browser's page targets — over the pipe's browser-level
    /// connection in pipe mode, over the HTTP debug endpoint in WS mode.
    async fn list_page_targets(&self) -> Vec<crate::cdp::TargetInfo> {
        if let Some(bc) = &self.browser_client {
            // Short timeout: Target.getTargets is browser-level and should
            // return in <10ms. If Chrome is busy (page navigating, pipe
            // congested), don't block the click flow for 30s — skip tab
            // detection instead.
            match bc.send_with_timeout("Target.getTargets", None, Duration::from_secs(3)).await {
                Ok(res) => res
                    .get("targetInfos")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                            .filter_map(|t| {
                                Some(crate::cdp::TargetInfo {
                                    id: t.get("targetId")?.as_str()?.to_string(),
                                    kind: "page".to_string(),
                                    title: t.get("title")?.as_str()?.to_string(),
                                    url: t.get("url")?.as_str()?.to_string(),
                                    attached: t.get("attached").and_then(|a| a.as_bool()).unwrap_or(false),
                                    web_socket_debugger_url: None,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            list_page_targets(&self.base).await.unwrap_or_default()
        }
    }

    /// List all page targets (tabs). Public wrapper for the MCP
    /// server's close-tab auto-switch.
    pub async fn tab_targets(&self) -> Vec<crate::cdp::TargetInfo> {
        self.list_page_targets().await
    }

    /// Create a new tab (browser-level Target.createTarget) and return its
    /// target id. Uses the browser-level pipe client in pipe mode; in WS
    /// mode connects to the browser's own debugger WebSocket (discovered
    /// via /json/version) — Target.createTarget belongs to the browser
    /// target, and the page session may be dead exactly when we need this
    /// (dead-tab recovery).
    pub async fn open_tab_target(&self, url: &str) -> Result<String> {
        // Bare hosts must work here exactly like `nav`: Target.createTarget
        // with a scheme-less URL ("localhost:3000/x") never loads — Chrome
        // reads "localhost:" as the scheme — leaving a stuck about:blank tab
        // whose screenshot then burns its full CDP timeout (reproduced live).
        let url = with_scheme(url);
        if let Some(bc) = &self.browser_client {
            let res = bc
                .send("Target.createTarget", Some(json!({ "url": url.as_str() })))
                .await?;
            return res
                .get("targetId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| BladeError::Other("no targetId from Target.createTarget".into()));
        }
        // WS mode: browser-level connection on demand.
        let ver = crate::cdp::version(&self.base).await?;
        let ws = ver
            .web_socket_debugger_url
            .ok_or_else(|| BladeError::Other("browser has no webSocketDebuggerUrl".into()))?;
        let client = CdpClient::connect(&ws).await?;
        let res = client
            .send("Target.createTarget", Some(json!({ "url": url.as_str() })))
            .await?;
        res.get("targetId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| BladeError::Other("no targetId from Target.createTarget".into()))
    }

    /// Switch the session to a different tab (target id from
    /// `state tabs`). The whole page state is rebuilt against the
    /// new tab: domains re-enabled, stealth re-injected, fresh
    /// capture. The old tab stays OPEN — this is focus switching,
    /// not closing. `*self = new_page` drops the old Page, whose
    /// Drop aborts its background tasks (dialogs/network/hum).
    pub async fn switch_tab(&mut self, target_id: &str) -> Result<()> {
        // Attach to the NEW target FIRST. The old code detached
        // the current session first — if the attach then failed,
        // the session was detached from everything (bricked).
        let session = if let Some(client) = &self.browser_client {
            // Pipe mode: flat-session attach via the browser-level client.
            let res = client
                .send(
                    "Target.attachToTarget",
                    Some(serde_json::json!({ "targetId": target_id, "flatten": true })),
                )
                .await?;
            let sid = res
                .get("sessionId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| BladeError::Other("Target.attachToTarget returned no sessionId".into()))?;
            CdpSession::child(client.clone(), sid)
        } else {
            // WS mode: connect to the target's own WebSocket URL.
            // Retry briefly: a just-created target can lag in HTTP
            // discovery ("tab not found" on a fresh open-tab was a race).
            let mut t = None;
            for _ in 0..5 {
                let targets = list_page_targets(&self.base).await?;
                t = targets.into_iter().find(|t| t.id == target_id);
                if t.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let t = t.ok_or_else(|| BladeError::Other(format!("tab not found: {target_id}")))?;
            let client = CdpClient::connect(t.ws_url()?).await?;
            CdpSession::root(client)
        };
        // New session attached — now detach the OLD one (pipe
        // mode) so only one session stays attached. Failure
        // here is harmless (two sessions attached briefly).
        if let Some(client) = &self.browser_client {
            if let Some(sid) = self.cdp.session_id() {
                let _ = client
                    .send(
                        "Target.detachFromTarget",
                        Some(serde_json::json!({ "sessionId": sid })),
                    )
                    .await;
            }
        }
        let new_page = Page::attach(session, &self.base, self.browser_client.clone()).await?;
        *self = new_page;
        Ok(())
    }

    /// Is the current tab still alive? A cheap probe used after
    /// close-tab to detect that the agent closed the tab the
    /// session was attached to.
    pub async fn current_tab_alive(&self) -> bool {
        self.cdp
            .send_with_timeout(
                "Runtime.evaluate",
                Some(serde_json::json!({ "expression": "1", "returnByValue": true })),
                Duration::from_secs(3),
            )
            .await
            .is_ok()
    }

    /// Re-capture the page and return the delta since the last capture.
    pub async fn recapture(&mut self) -> Result<PageDelta> {
        let cap = capture(&self.cdp).await?;
        // Keep the interception third-party baseline in sync with the
        // current page (covers SPA navigations that bypass navigate()).
        self.intercept.set_page_url(&cap.url);
        Ok(self.lpm.ingest(cap))
    }

    /// Set active resource-block classes ("images,fonts,media,trackers").
    /// Empty / "none" clears blocking. Toggles the CDP Fetch domain so
    /// interception adds zero overhead while blocking is off.
    pub async fn set_block_classes(&mut self, spec: &str) -> Result<u32> {
        let mask = if spec.trim().eq_ignore_ascii_case("none") || spec.trim().eq_ignore_ascii_case("clear") {
            0
        } else {
            intercept::InterceptState::parse_classes(spec)
        };
        let was = self.intercept.rules();
        self.intercept.set_rules(mask);
        if mask != 0 && was == 0 {
            // Enable interception: pause every request so we can decide.
            self.cdp
                .send(
                    "Fetch.enable",
                    Some(serde_json::json!({ "patterns": [{ "urlPattern": "*" }] })),
                )
                .await?;
        } else if mask == 0 && was != 0 {
            // Drain anything still paused, then stop intercepting.
            self.intercept.drain_pending_requests(&self.cdp).await;
            self.cdp.send("Fetch.disable", None).await?;
        }
        Ok(mask)
    }

    /// Persist the agent's explicit resource-block choice for the domain of
    /// `url`. `active` = the choice turned blocking on (mask != 0): the raw
    /// spec is stored so later navigations reproduce it. An inactive
    /// clear-word ("", "none", "clear") erases any stored config;
    /// anything else inactive (typo, unknown classes) is ignored.
    pub fn remember_block_choice(&self, url: &str, spec: &str, active: bool) {
        let domain = crate::knowledge::domain_from_url(url);
        if domain.is_empty() {
            return;
        }
        let stored = if active {
            spec.to_string()
        } else if spec.is_empty() || spec.eq_ignore_ascii_case("none") || spec.eq_ignore_ascii_case("clear") {
            String::new()
        } else {
            return;
        };
        if let Some(kb) = self.knowledge.as_ref() {
            if let Ok(mut kb) = kb.lock() {
                kb.learn_block_config(&domain, &stored);
            }
        }
    }

    /// Current block-class bitmask (for `state op=block get`).
    pub fn block_rules(&self) -> u32 {
        self.intercept.rules()
    }

    /// A full agent-facing view of the current model (the `see` output).
    pub fn view(&self, budget: usize) -> String {
        self.lpm.compress(budget, self.in_flight.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn view_filtered(&self, budget: usize, filter: &str) -> String {
        self.lpm.compress_filtered(budget, filter, self.in_flight.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Extract visible text content from the page body.
    pub async fn content(&self, budget: usize) -> Result<String> {
        capture_content(&self.cdp, budget).await
    }

    /// Extract page content as clean markdown (semantic content extraction).
    /// Returns headings, paragraphs, links, lists, code — no ref IDs, no
    /// actionability markers. For reading, not acting.
    pub async fn markdown(&self, budget: usize) -> Result<String> {
        crate::page::perception::capture_markdown(&self.cdp, budget).await
    }

    /// Extract just the page title + heading hierarchy. Ultra-minimal.
    pub async fn outline(&self) -> Result<String> {
        crate::page::perception::capture_outline(&self.cdp).await
    }

    /// The delta since the last capture, rendered (the observation).
    pub fn delta_view(&self, d: &PageDelta, budget: usize) -> String {
        self.lpm.compress_delta(d, budget, self.in_flight.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// S5: pacing governor — sleep so the inter-action gap follows a
    /// log-normal distribution matching fast human think-time. Skipped for
    /// the first action, disabled by BLADE_PACE=off.
    async fn pace(&mut self, action: &crate::action::Action) {
        if std::env::var("BLADE_PACE").as_deref() == Ok("off") {
            return;
        }
        let last = self.last_action_epoch.load(std::sync::atomic::Ordering::Relaxed);
        if last == 0 {
            return; // first action
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let elapsed = now.saturating_sub(last);

        let (median_ms, sigma) = match action {
            crate::action::Action::Click { .. } => (170.0, 0.5),
            crate::action::Action::Type { .. } => (130.0, 0.4),
            crate::action::Action::Scroll { .. } => (90.0, 0.4),
            crate::action::Action::Back => (280.0, 0.6),
            crate::action::Action::Hover { .. } => (150.0, 0.4),
            // Wait/Read are perception, not human actions — no pacing.
            crate::action::Action::Wait { .. } | crate::action::Action::Read { .. } => return,
            _ => (140.0, 0.4),
        };
        let mut rng = crate::stealth::biometrics::Rng::new();
        // v3.10 (speed pass): medians ~3x faster than the v3.9 pacing, and a
        // 3x-median cap so a rare log-normal tail can't stall an agent flow.
        let target = crate::stealth::biometrics::log_normal(&mut rng, median_ms, sigma);
        let target_ms = (target.as_millis() as u64).min((median_ms * 3.0) as u64);
        if elapsed < target_ms {
            let sleep_for = target_ms - elapsed;
            tokio::time::sleep(std::time::Duration::from_millis(sleep_for)).await;
        }
    }

    /// Perform an action and return the observation delta.
    /// Perform an action, returning (delta, verdict).
    ///
    /// V1: self-healing refs. If the action targets a ref that is not in
    /// the current model (page navigated, DOM re-rendered), the driver
    /// looks up what the ref USED to be (graveyard), re-resolves that
    /// identity against the live DOM, and acts on the healed element —
    /// all invisibly. The agent only finds out via a `[ref healed]` note
    /// in the verdict. If the element is truly gone, the error says what
    /// the ref used to be.
    pub async fn act(&mut self, action: crate::action::Action) -> Result<(PageDelta, String)> {
        // Manual-control pause (`rb pause`): the person has the browser.
        // Refuse input-dispatching actions so agent and human never fight
        // over clicks/keys; reads and waits still work.
        if crate::realbrowser::input_paused() && action.disrupts_page() {
            return Err(crate::realbrowser::paused_error());
        }
        let mut heal_note = if let Some(ref_id) = action.ref_id() {
            self.ensure_ref(ref_id).await?
        } else {
            None
        };
        // S5: pacing governor — realistic inter-action gaps.
        self.pace(&action).await;
        self.is_busy.store(true, std::sync::atomic::Ordering::Relaxed);
        // M5: For clicks, detect new tabs (target=_blank opens a new page).
        let is_click = matches!(action, crate::action::Action::Click { .. } | crate::action::Action::ClickCoord { .. });
        let before = if is_click {
            let r = self.list_page_targets().await;
            r
        } else {
            Vec::new()
        };
        let result = crate::action::perform_with_network(
            &self.cdp, &mut self.lpm, &action, Some(&self.in_flight), &self.last_mouse
        ).await;
        // V1b: DOM-drift heal. The model had the ref, but the live
        // DOM moved (SPA re-render between captures). Re-resolve the
        // element's identity and retry ONCE before giving up.
        let (delta, verdict) = match result {
            Ok(v) => v,
            Err(BladeError::ElementNotFound(_)) if action.ref_id().is_some() => {
                let ref_id = action.ref_id().unwrap().to_string();
                match self.heal_by_identity(&ref_id).await? {
                    Some(note) => {
                        heal_note = Some(note);
                        crate::action::perform_with_network(
                            &self.cdp, &mut self.lpm, &action, Some(&self.in_flight), &self.last_mouse
                        ).await?
                    }
                    None => {
                        self.is_busy.store(false, std::sync::atomic::Ordering::Relaxed);
                        return Err(BladeError::ElementNotFound(format!(
                            "{ref_id} not in the live DOM and cannot be re-resolved"
                        )));
                    }
                }
            }
            Err(e) => {
                self.is_busy.store(false, std::sync::atomic::Ordering::Relaxed);
                return Err(e);
            }
        };
        let verdict = match heal_note {
            Some(note) => format!("{verdict} [{note}]"),
            None => verdict,
        };
        if is_click {
            let after = self.list_page_targets().await;
            let new_tabs: Vec<_> = after.iter()
                .filter(|t| !before.iter().any(|b| b.id == t.id))
                .collect();
            if !new_tabs.is_empty() {
                // Override verdict: a new tab opened even if the current page
                // didn't change. This is the correct outcome for target=_blank
                // links and window.open() calls.
                let tab_info: Vec<String> = new_tabs.iter().map(|t| {
                    if t.title.is_empty() { t.url.clone() } else { t.title.clone() }
                }).collect();
                let new_verdict = format!(
                    "outcome: new tab opened — {}",
                    tab_info.join(", ")
                );
                if let Ok(mut a) = self.ambient.lock() {
                    a.push(new_verdict.clone());
                }
                return Ok((delta, new_verdict));
            }
        }
        // S4+S5: mark action complete — hum resumes, next action paces.
        self.is_busy.store(false, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_action_epoch.store(now, std::sync::atomic::Ordering::Relaxed);
        Ok((delta, verdict))
    }

    /// Ensure a ref is live, healing it from the graveyard if dead.
    /// Returns Some(heal note) if a heal happened, None if the ref was
    /// already live. Errors when the ref is dead AND cannot be
    /// re-resolved on the current page (with candidate list when
    /// ambiguous — candidates are adopted so their refs are usable).
    pub async fn ensure_ref(&mut self, ref_id: &str) -> Result<Option<String>> {
        if self.lpm.element(ref_id).is_some() {
            return Ok(None);
        }
        self.heal_by_identity(ref_id).await
    }

    /// Re-resolve a ref's identity (from the live model or the
    /// graveyard) against the live DOM. On a single confident match,
    /// the ref is re-adopted to the found element. On multiple, the
    /// error lists candidates with usable refs.
    async fn heal_by_identity(&mut self, ref_id: &str) -> Result<Option<String>> {
        // Identity from live model first, then graveyard. Keep the sig too:
        // with the V25c global per-frame rank sig scheme, the sig uniquely
        // identifies the ORIGINAL element even among duplicate-named ones.
        let (role, name, want_sig) = if let Some(el) = self.lpm.element(ref_id) {
            (el.raw.role.clone(), el.raw.name.clone(), Some(el.raw.sig.clone()))
        } else if let Some((sig, role, name)) = self.lpm.graveyard_lookup(ref_id) {
            (role, name, Some(sig))
        } else {
            // Never-seen ref — let the normal StaleRef path handle it.
            return Ok(None);
        };
        if name.is_empty() {
            return Err(crate::error::BladeError::StaleRef(format!(
                "{ref_id} was an unnamed {role} — cannot re-resolve. Use see to view the current page."
            )));
        }
        // Hidden text fields still heal: a facade composer's wrapper goes
        // invisible when its rich editor mounts, and the input path adopts
        // the live editor from the hidden wrapper (see find_sig "prepare").
        let include_hidden = matches!(role.as_str(), "textbox" | "combobox");
        let matches = crate::action::find_by_text(&self.cdp, &name, Some(&role), include_hidden).await?;
        // Precise heal: if exactly one candidate has the SAME sig as the
        // original element, that IS the original (not a same-named sibling).
        // Heals duplicate-named refs (header vs footer nav links) to the
        // correct element in ONE call instead of erroring with a candidate
        // list. Falls through to the count-based path when the DOM shifted
        // (rank changed) or the element is genuinely gone.
        if let Some(ws) = want_sig.as_deref() {
            let exact: Vec<_> = matches.iter().filter(|m| m.sig == ws).collect();
            if exact.len() == 1 {
                let m = exact[0];
                self.lpm.adopt_as(ref_id, &m.sig, &m.role, &m.name, &m.frame);
                return Ok(Some(format!(
                    "ref {ref_id} healed → {role} \"{}\"",
                    crate::page::model::truncate_pub(&name, 40)
                )));
            }
        }
        match matches.len() {
            0 => Err(crate::error::BladeError::StaleRef(format!(
                "{ref_id} was '{role} \"{name}\"' — gone from the current page. Use see to view it."
            ))),
            1 => {
                let m = &matches[0];
                self.lpm.adopt_as(ref_id, &m.sig, &m.role, &m.name, &m.frame);
                Ok(Some(format!(
                    "ref {ref_id} healed → {role} \"{}\"",
                    crate::page::model::truncate_pub(&name, 40)
                )))
            }
            n => {
                // Ambiguous heal: adopt every candidate so the agent gets
                // usable refs in the error, and can retry in ONE call.
                let mut lines = vec![format!(
                    "{ref_id} was '{role} \"{name}\"' — {n} candidates on the current page:"
                )];
                for m in matches.iter().take(6) {
                    let id = self.lpm.adopt(&m.sig, &m.role, &m.name, &m.frame);
                    lines.push(format!("  {id} {} \"{}\"", m.role, m.name));
                }
                if n > 6 {
                    lines.push(format!("  …and {} more", n - 6));
                }
                lines.push("retry with ref=<id> from the list above".to_string());
                Err(crate::error::BladeError::StaleRef(lines.join("\n")))
            }
        }
    }

    /// Perform a state operation (cookies/storage/tabs) and return a text result.
    pub async fn state(&self, op: crate::state::StateOp) -> Result<String> {
        crate::state::perform(&self.cdp, &op).await
    }

    /// Borrow the LPM (for inspection / testing).
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Borrow the in-flight request counter (for network-aware settle).
    pub fn in_flight_ref(&self) -> &std::sync::atomic::AtomicUsize {
        self.in_flight.as_ref()
    }

    pub fn model(&self) -> &LivePageModel {
        &self.lpm
    }

    /// Mutably borrow the LPM (for text-addressing ref adoption).
    pub fn model_mut(&mut self) -> &mut LivePageModel {
        &mut self.lpm
    }

    /// Borrow the CDP client (for MCP server navigate).
    pub fn cdp_ref(&self) -> &CdpSession {
        &self.cdp
    }

    /// Has the browser connection been closed (Chrome died)?
    /// The MCP server checks this before tool calls to self-heal.
    pub fn is_closed(&self) -> bool {
        self.cdp.is_closed()
    }

    /// Drain the queue of auto-dismissed dialogs. Called by the MCP server
    /// after each tool call to surface dialog notifications to the agent.
    pub fn drain_dialogs(&self) -> Vec<DialogInfo> {
        self.dialogs
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Drain ambient events (consent, block detection) for the agent.
    pub fn drain_ambient(&self) -> Vec<String> {
        self.ambient
            .lock()
            .map(|mut a| a.drain(..).collect())
            .unwrap_or_default()
    }

    /// V8: snapshot of the network request log (last 50).
    pub fn network_log(&self) -> Vec<NetEntry> {
        self.net_log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Recent XHR/fetch requests (last 24, start-observed, full URLs).
    pub fn xhr_log(&self) -> Vec<XhrEntry> {
        self.xhr_log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// V8: read the console log captured by the injection hook.
    /// Returns raw JSON (array of {l, m, t}). The ring buffer lives under
    /// the same Symbol.for('q') slot the injection script defines — a
    /// string-keyed window property was a page-readable detection marker.
    pub async fn console_log(&self) -> Result<serde_json::Value> {
        let res = self.cdp.send("Runtime.evaluate", Some(serde_json::json!({
            "expression": "window[Symbol.for('q')]||[]",
            "returnByValue": true,
        }))).await?;
        Ok(res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(serde_json::json!([])))
    }

    /// Navigate to a URL. Re-registers the stealth script for the new
    /// document, sends `Page.navigate`, waits for load + settle, then
    /// recaptures and returns the delta. Shared by `act navigate`, `run`
    /// navigate steps, and the CLI `nav` command.
    pub async fn navigate(&mut self, url: &str) -> Result<PageDelta> {
        // S4+S5: track action timing for pacing + idle hum.
        self.is_busy.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = self.navigate_inner(url).await;
        self.is_busy.store(false, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_action_epoch.store(now, std::sync::atomic::Ordering::Relaxed);
        result
    }

    async fn navigate_inner(&mut self, url: &str) -> Result<PageDelta> {
        // M16: Idempotent navigate \u{2014} if already on this URL, skip reload.
        if !self.lpm.url().is_empty() && normalize_url(url) == normalize_url(self.lpm.url()) {
            let cap = capture(&self.cdp).await?;
            return Ok(self.lpm.ingest(cap));
        }
        // NOTE: no stealth re-apply here — addScriptToEvaluateOnNewDocument
        // registrations persist for the target's lifetime. Re-applying on
        // every navigation used to STACK another identical script each time
        // (N navigations = N document_start scripts). Locale changes are
        // handled by apply_domain_profile swapping the registration.
        // S11: apply per-domain stealth settings from ~/.blade/profiles.json.
        self.apply_domain_profile(url).await;
        // Reset isolated world on navigation — old context is destroyed.
        self.reset_isolated();
        // Knowledge: per-domain resource-block config the agent set before.
        // Applied only when nothing is active — an explicit session choice
        // always wins; `state block clear` erases the stored config.
        let domain = crate::knowledge::domain_from_url(url);
        let (stored_block, learned_settle) = self.knowledge.as_ref()
            .and_then(|kb| kb.lock().ok())
            .map(|kb| (
                kb.get_block_config(&domain).map(|s| s.to_string()),
                kb.get_settle_ms(&domain),
            ))
            .unwrap_or((None, None));
        if let Some(spec) = stored_block.filter(|s| !s.is_empty()) {
            if self.block_rules() == 0 {
                if let Err(e) = self.set_block_classes(&spec).await {
                    eprintln!("[bladebro] stored block config failed to apply: {e}");
                }
            }
        }
        let settle_cap = crate::knowledge::nav_settle_cap_ms(learned_settle);
        let _nav_t = std::time::Instant::now();
        let _t = |label: &str| {
            if std::env::var("NAV_TIMING").is_ok() {
                eprintln!("[nav-timing] {label}: {:?}", _nav_t.elapsed());
            }
        };
        let wait = self
            .cdp
            .wait_for("Page.frameNavigated", Duration::from_secs(10));
        let target = with_scheme(url);
        self.cdp
            .send("Page.navigate", Some(serde_json::json!({ "url": target })))
            .await?;
        _t("sent");
        let _ = tokio::time::timeout(Duration::from_secs(10), wait).await;
        _t("frameNavigated");
        wait_for_load(&self.cdp, Duration::from_secs(10)).await?;
        _t("load");
        let _settle_t = std::time::Instant::now();
        wait_for_settle_with_network(&self.cdp, Duration::from_millis(settle_cap), Some(&self.in_flight)).await?;
        // Bounded post-drain re-quiet: a late fetch resolving after the
        // network plateau mounts its content a moment later; this catches
        // that mount without taxing interactions (nav-only).
        let _ = re_settle(&self.cdp).await;
        _t("settle");
        // Knowledge: learn this domain's real settle duration (only when it
        // finished early — a cap timeout is not a settle sample).
        if !domain.is_empty() {
            let elapsed = _settle_t.elapsed().as_millis() as u64;
            if elapsed + 150 < settle_cap {
                if let Some(kb) = self.knowledge.as_ref() {
                    if let Ok(mut kb) = kb.lock() {
                        kb.update_timing(&domain, elapsed);
                    }
                }
            }
        }
        // M4+M6: Check for consent banners and block pages after navigation.
        // Knowledge-base integration: try stored consent selector first,
        // learn from successful dismissals, record the visit.
        let stored_consent = self.knowledge.as_ref()
            .and_then(|kb| kb.lock().ok())
            .and_then(|kb| kb.get_consent(&domain).map(|c| c.selector.clone()));
        let consent = dismiss_consent_with_stored(&self.cdp, stored_consent.as_deref()).await.unwrap_or(None);
        let blocked = detect_block(&self.cdp).await.unwrap_or(None);
        // JS challenge handling: many anti-bot systems (Reddit, Cloudflare)
        // serve a JS challenge page that a real browser solves automatically.
        // The challenge page is simple HTML and settles fast, so detect_block
        // fires BEFORE the challenge JS has time to compute + redirect.
        // Wait up to 5s polling for either a URL change OR the block
        // disappearing (some challenges solve in-place without redirect).
        // If either happens, the challenge was solved — do NOT report a block.
        // Only JS-challenge types can self-solve; rate-limit/akamai/recaptcha
        // walls never do (waiting there only added 5s latency to a final verdict).
        // Knowledge: heavier vendors get a longer self-solve window (learned
        // per domain, raised by every real block we hit there).
        let domain_risk = self.knowledge.as_ref()
            .and_then(|kb| kb.lock().ok())
            .map(|kb| kb.get_bot_risk(&domain))
            .unwrap_or_default();
        let challenge_polls: u32 = if domain_risk >= crate::knowledge::BotRiskLevel::Heavy { 16 } else { 10 };
        let mut challenge_seen = false;
        let blocked = match blocked.as_deref() {
            Some("cloudflare") | Some("datadome") | Some("perimeterx") => {
                challenge_seen = true;
                let pre_url = eval_location_href(&self.cdp).await;
                let mut solved = false;
                for _ in 0..challenge_polls {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let post_url = eval_location_href(&self.cdp).await;
                    if post_url != pre_url && !post_url.is_empty() {
                        solved = true;
                        break;
                    }
                    // Also check if the block disappeared without a URL change
                    // (some challenges solve in-place, replacing page content).
                    if detect_block(&self.cdp).await.unwrap_or(None).is_none() {
                        solved = true;
                        break;
                    }
                }
                if solved {
                    wait_for_settle_with_network(
                        &self.cdp, Duration::from_millis(2500), Some(&self.in_flight),
                    ).await?;
                    let _ = re_settle(&self.cdp).await;
                    None // clear block — was a JS challenge, not a real block
                } else {
                    blocked
                }
            }
            Some("js-challenge") => {
                // Reddit's browser-solvable challenge: a hidden form that
                // auto-submits `solution=<token><token>` and sets the `loid`
                // token on the solved response. The page navigates itself in
                // ~1s; wait bounded for it (URL change or wall gone), then
                // settle on the real content. No interaction needed.
                challenge_seen = true;
                let pre_url = eval_location_href(&self.cdp).await;
                let mut solved = false;
                for _ in 0..challenge_polls {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let post_url = eval_location_href(&self.cdp).await;
                    if post_url != pre_url && !post_url.is_empty() {
                        solved = true;
                        break;
                    }
                    if detect_block(&self.cdp).await.unwrap_or(None).is_none() {
                        solved = true;
                        break;
                    }
                }
                if solved {
                    wait_for_settle_with_network(
                        &self.cdp, Duration::from_millis(2500), Some(&self.in_flight),
                    ).await?;
                    let _ = re_settle(&self.cdp).await;
                    None // clear block — the challenge solved itself
                } else {
                    blocked
                }
            }
            Some("reddit-humanity") => {
                // Reddit's one-time humanity check (reCAPTCHA v2 checkbox)
                // for sessions without the `loid` token. One humanized click
                // on the checkbox passes it; the solve grants `loid` and the
                // wall does not return for this profile. An image grid (if
                // Google serves one) is not solvable in-house — report it.
                challenge_seen = true;
                match solve_reddit_humanity(&self.cdp).await {
                    HumanityOutcome::Solved => {
                        wait_for_settle_with_network(
                            &self.cdp, Duration::from_millis(2500), Some(&self.in_flight),
                        ).await?;
                        let _ = re_settle(&self.cdp).await;
                        if let Ok(mut a) = self.ambient.lock() {
                            a.push(
                                "reddit: humanity check solved automatically (one humanized click — grant stored for this profile)".into(),
                            );
                        }
                        None
                    }
                    HumanityOutcome::Grid => {
                        if let Ok(mut a) = self.ambient.lock() {
                            a.push(
                                "reddit: the humanity check escalated to an image grid — not solvable automatically; solve it once manually in the browser (the grant then persists for this profile), or retry later".into(),
                            );
                        }
                        blocked
                    }
                    HumanityOutcome::Failed => blocked,
                }
            }
            Some("reddit") => {
                // The network-security wall is a soft, transient flag (its
                // own 403 carries `retry-after: 0`) — a reload clears it in
                // most cases. Walk a small jittered reload ladder before
                // reporting a block; a domain already known Heavy gets one
                // attempt instead of two.
                challenge_seen = true;
                let attempts: u32 = if domain_risk >= crate::knowledge::BotRiskLevel::Heavy {
                    1
                } else {
                    2
                };
                let mut cleared = false;
                for attempt in 0..attempts {
                    let jitter = 1100
                        + attempt as u64 * 900
                        + std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_millis() as u64)
                            .unwrap_or(200)
                            % 350;
                    tokio::time::sleep(Duration::from_millis(jitter)).await;
                    let _ = self
                        .cdp
                        .send("Page.reload", Some(serde_json::json!({ "ignoreCache": false })))
                        .await;
                    let _ = wait_for_load(&self.cdp, Duration::from_secs(10)).await;
                    let _ = wait_for_settle_with_network(
                        &self.cdp, Duration::from_millis(2500), Some(&self.in_flight),
                    ).await;
                    let _ = re_settle(&self.cdp).await;
                    let now = detect_block(&self.cdp).await.unwrap_or(None);
                    if !matches!(now.as_deref(), Some("reddit")) {
                        cleared = true;
                        break;
                    }
                }
                if cleared {
                    if let Ok(mut a) = self.ambient.lock() {
                        a.push(
                            "reddit: transient network-security wall — auto-cleared on reload".into(),
                        );
                    }
                    None
                } else {
                    blocked
                }
            }
            other => other.map(String::from),
        };
        // Knowledge: persist what this domain does to us — a real block wall
        // counts in stats and raises the domain's risk level; a solved JS
        // challenge marks the domain as challenge-serving (medium).
        if !domain.is_empty() {
            if let Some(kb) = self.knowledge.as_ref() {
                if let Ok(mut kb) = kb.lock() {
                    if let Some(ref bt) = blocked {
                        kb.record_block_detected();
                        kb.raise_bot_risk(&domain, crate::knowledge::vendor_risk(bt));
                    } else if challenge_seen {
                        kb.raise_bot_risk(&domain, crate::knowledge::BotRiskLevel::Medium);
                    }
                }
            }
        }
        // Learn from successful consent dismissal (only specific selectors, not "generic").
        if let Some(ref result) = consent {
            if result != "generic" && !result.is_empty() && !domain.is_empty() {
                if let Some(kb) = self.knowledge.as_ref() {
                    if let Ok(mut kb) = kb.lock() {
                        kb.learn_consent_result(&domain, result);
                        kb.record_consent_dismissed();
                    }
                }
            }
            if let Ok(mut a) = self.ambient.lock() {
                a.push(format!("consent: {} ({})",
                    if std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".into()) != "accept" { "rejected" } else { "accepted" }, result));
            }
        }
        if let Some(ref bt) = blocked {
            if let Ok(mut a) = self.ambient.lock() {
                a.push(format!("blocked: {}", bt));
                for step in crate::page::perception::remediation_ladder(bt) {
                    a.push(format!("  remediation: {}", step));
                }
            }
        }
        // Record visit + navigation for this domain.
        if !domain.is_empty() {
            if let Some(kb) = self.knowledge.as_ref() {
                if let Ok(mut kb) = kb.lock() {
                    kb.record_visit(&domain);
                    kb.record_navigation();
                }
            }
        }
        _t("consent/block");
        let r = self.recapture().await;
        _t("recapture");
        r
    }

    /// S11: apply per-domain stealth settings from ~/.blade/profiles.json.
    /// Stores timezone and locale overrides per-domain so the driver remembers
    /// which settings work for each site. The agent can edit the file directly.
    async fn apply_domain_profile(&mut self, url: &str) {
        // Real-browser lane: per-domain tz/locale overrides are page-visible
        // masks, and this lane's contract is that nothing page-visible is
        // manufactured. Return before any CDP call.
        if crate::realbrowser::real_lane() {
            return;
        }
        let domain = extract_domain(url);
        if domain.is_empty() {
            return;
        }
        let path = crate::platform::blade_dir().join("profiles.json");
        let profiles: std::collections::HashMap<String, DomainProfile> =
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
        if let Some(profile) = profiles.get(&domain) {
            if let Some(ref tz) = profile.tz {
                let _ = self
                    .cdp
                    .send(
                        "Emulation.setTimezoneOverride",
                        Some(serde_json::json!({ "timezoneId": tz })),
                    )
                    .await;
                eprintln!("[bladebro] domain profile {domain}: tz={tz}");
            }
            if let Some(ref locale) = profile.locale {
                let _ = self
                    .cdp
                    .send("Emulation.setLocaleOverride", Some(serde_json::json!({ "locale": locale })))
                    .await;
                // Keep Accept-Language in sync — it was set once at attach;
                // a swapped locale with a stale header is a cross-layer
                // mismatch fingerprint.
                let base = locale.split('-').next().unwrap_or(locale);
                let _ = self.cdp.send("Network.setExtraHTTPHeaders",
                    Some(serde_json::json!({
                        "headers": { "Accept-Language": format!("{locale},{base};q=0.9") }
                    }))).await;
                eprintln!("[bladebro] domain profile {domain}: locale={locale}");
            }

            // S11 coherence: navigator.language comes from the INJECTED
            // script, not the CDP override. If the injection bakes a
            // different locale, navigator.language and Intl disagree — a
            // fingerprint-visible mismatch. Swap the registration (remove
            // + re-add, never stack) so both layers speak the same locale.
            let want_locale = profile.locale.clone()
                .or_else(|| std::env::var("BLADE_LOCALE").ok().filter(|s| !s.is_empty()));
            if want_locale != self.active_locale {
                if let Some(id) = self.stealth_script_id.take() {
                    let _ = self.cdp.send(
                        "Page.removeScriptToEvaluateOnNewDocument",
                        Some(serde_json::json!({ "identifier": id })),
                    ).await;
                }
                match crate::stealth::apply_stealth(&self.cdp, profile.locale.as_deref()).await {
                    Ok(id) => {
                        self.stealth_script_id = Some(id);
                        self.active_locale = want_locale;
                    }
                    Err(e) => eprintln!("[bladebro] WARNING: locale swap injection failed: {e}"),
                }
            }
        }
    }
}

/// S11: per-domain stealth settings stored in ~/.blade/profiles.json.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct DomainProfile {
    tz: Option<String>,
    locale: Option<String>,
}

/// Extract the registrable domain from a URL for profile lookup.
/// Reddit's "Prove your humanity" wall is a reCAPTCHA v2 checkbox. One
/// humanized click on the (cross-origin) anchor iframe passes it — the
/// solve auto-submits to `?captcha=1` and grants the `loid` token, after
/// which the wall does not return for that profile.
///
/// Outcomes: `Solved` (grant obtained), `Grid` (Google escalated to an
/// image challenge — not solvable in-house, report honestly), `Failed`
/// (no widget to click, dispatch error, or no verdict in the window).
async fn solve_reddit_humanity(cdp: &CdpSession) -> HumanityOutcome {
    // The widget loads async (recaptcha scripts come from google) — wait
    // bounded for the anchor iframe to exist at a sane size, then one short
    // beat so the widget's own JS is listening before the click lands (an
    // early click is swallowed silently — observed live).
    let mut point: Option<(f64, f64)> = None;
    for _ in 0..12 {
        point = probe_click_point(cdp).await;
        if point.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let Some((x, y)) = point else {
        return HumanityOutcome::Failed;
    };
    tokio::time::sleep(Duration::from_millis(700)).await;
    let last_mouse = std::sync::Arc::new(std::sync::Mutex::new(None));
    if crate::action::dispatch_mouse_click(cdp, x, y, &last_mouse)
        .await
        .is_err()
    {
        return HumanityOutcome::Failed;
    }
    // Poll bounded for the verdict: URL change or a filled token = solved;
    // a visible b-frame twice in a row = an image grid is showing (bail).
    // One click only — re-clicking could disturb a slow-but-passing
    // verification, and a swallowed click is reported honestly instead.
    let pre_url = eval_location_href(cdp).await;
    let check = r#"(function(){var t=document.querySelector('#g-recaptcha-response');if(t&&t.value)return 'solved';var fs=document.querySelectorAll('iframe');for(var i=0;i<fs.length;i++){var s=fs[i].src||'';if(s.indexOf('bframe')>=0){var r=fs[i].getBoundingClientRect();if(r.y>-200&&r.width>0)return 'grid';}}return 'wait';})()"#;
    let mut grid_seen = 0u32;
    let mut re_clicks = 0u32;
    for i in 0..60 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let post_url = eval_location_href(cdp).await;
        if post_url != pre_url && !post_url.is_empty() {
            return HumanityOutcome::Solved;
        }
        let state = cdp
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({ "expression": check, "returnByValue": true })),
            )
            .await
            .ok()
            .and_then(|r| {
                r.get("result")
                    .and_then(|x| x.get("value"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();
        if state == "solved" {
            return HumanityOutcome::Solved;
        }
        if state == "grid" {
            grid_seen += 1;
            if grid_seen >= 2 {
                return HumanityOutcome::Grid;
            }
        } else {
            grid_seen = 0;
        }
        // An early click can be swallowed before the widget listens; two
        // spaced re-clicks recover exactly that case. A click that IS
        // being verified also keeps state 'wait' — a checking widget
        // ignores extra clicks, and this hedge is bounded at two.
        if state == "wait" && (i == 8 || i == 24) && re_clicks < 2 {
            re_clicks += 1;
            if let Some((x2, y2)) = probe_click_point(cdp).await {
                let lm = std::sync::Arc::new(std::sync::Mutex::new(None));
                let _ = crate::action::dispatch_mouse_click(cdp, x2, y2, &lm).await;
            }
        }
    }
    HumanityOutcome::Failed
}

/// Outcome of a humanity-check solve attempt.
enum HumanityOutcome {
    Solved,
    Grid,
    Failed,
}

/// Resolve the recaptcha anchor checkbox click point (left-center of the
/// anchor iframe), or None while the widget is not mounted at a sane size.
async fn probe_click_point(cdp: &CdpSession) -> Option<(f64, f64)> {
    let probe = r#"(function(){if(typeof window.grecaptcha==='undefined'||typeof window.grecaptcha.getResponse!=='function')return null;var fs=document.querySelectorAll('iframe');for(var i=0;i<fs.length;i++){var s=fs[i].src||'';if(s.indexOf('/recaptcha/api2/anchor')>=0){var r=fs[i].getBoundingClientRect();if(r.width<60||r.height<30)return null;return JSON.stringify({x:Math.round(r.x+30),y:Math.round(r.y+r.height/2)});}}return null;})()"#;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({ "expression": probe, "returnByValue": true })),
        )
        .await
        .ok()?;
    let point = res.get("result")?.get("value")?.as_str()?.to_string();
    let p: serde_json::Value = serde_json::from_str(&point).ok()?;
    let (x, y) = (p.get("x")?.as_f64()?, p.get("y")?.as_f64()?);
    if x <= 0.0 || y <= 0.0 {
        return None;
    }
    Some((x, y))
}

/// Get the current page URL via CDP. Used by JS challenge detection
/// to detect redirects after a challenge page is served.
async fn eval_location_href(cdp: &CdpSession) -> String {
    cdp.send("Runtime.evaluate", Some(json!({
        "expression": "location.href",
        "returnByValue": true,
    }))).await
        .ok()
        .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

fn extract_domain(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url)
        .split('/').next().unwrap_or("")
        .split(':').next().unwrap_or("")
        .trim_start_matches("www.")
        .to_string()
}

/// Add a scheme to user-supplied URLs so bare hosts work everywhere
/// (`example.com`, `localhost:3000`). Local/private hosts and IPs default to
/// http:// (dev servers rarely have certs), public hosts to https://.
/// URLs that already carry a scheme (http/https/about/file/data/blob/...) are
/// left untouched.
pub(crate) fn with_scheme(url: &str) -> String {
    let u = url.trim();
    if u.is_empty()
        || u.contains("://")
        || u.starts_with("about:")
        || u.starts_with("file:")
        || u.starts_with("data:")
        || u.starts_with("blob:")
        || u.starts_with("javascript:")
    {
        return u.to_string();
    }
    let host = u.split('/').next().unwrap_or(u);
    // localhost, loopback/private IPs, or an explicit port (dev-server
    // signal) default to http://; public hosts to https://.
    let is_local = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| match ip {
                std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
                std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local(),
            })
            .unwrap_or(false)
        || host.contains(':');
    if is_local {
        format!("http://{u}")
    } else {
        format!("https://{u}")
    }
}

/// Normalize a URL for comparison: strip scheme, fragment, trailing slash.
fn normalize_url(url: &str) -> String {
    let (s, https) = url
        .strip_prefix("https://")
        .map(|s| (s, true))
        .or_else(|| url.strip_prefix("http://").map(|s| (s, false)))
        .unwrap_or((url, false));
    let s = s.split('#').next().unwrap_or(s);
    // Strip default ports: :443 on https, :80 on http (host part only).
    let default_port = if https { ":443" } else { ":80" };
    let s = if let Some(slash) = s.find('/') {
        let (host, path) = s.split_at(slash);
        let host = host.strip_suffix(default_port).unwrap_or(host);
        format!("{host}{path}")
    } else {
        s.strip_suffix(default_port).unwrap_or(s).to_string()
    };
    s.strip_suffix('/').unwrap_or(&s).to_string()
}

impl Drop for Page {
    fn drop(&mut self) {
        // Abort background tasks so their CdpClient clones are dropped,
        // allowing the WebSocket connection to close cleanly.
        if let Some(handle) = self.dialog_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.network_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.hum_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.worker_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.intercept_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.download_task.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_scheme;

    #[test]
    fn with_scheme_handles_all_forms() {
        // Bare public hosts get https.
        assert_eq!(with_scheme("example.com"), "https://example.com");
        assert_eq!(with_scheme("example.com/path?q=1"), "https://example.com/path?q=1");
        // Local/private targets get http (dev servers rarely have certs).
        assert_eq!(with_scheme("localhost:3000"), "http://localhost:3000");
        assert_eq!(with_scheme("localhost"), "http://localhost");
        assert_eq!(with_scheme("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(with_scheme("192.168.1.5"), "http://192.168.1.5");
        // Explicit ports imply a dev server: http.
        assert_eq!(with_scheme("myserver.test:8443"), "http://myserver.test:8443");
        // Existing schemes untouched.
        assert_eq!(with_scheme("https://x.com"), "https://x.com");
        assert_eq!(with_scheme("http://x.com"), "http://x.com");
        assert_eq!(with_scheme("file:///tmp/x.html"), "file:///tmp/x.html");
        assert_eq!(with_scheme("about:blank"), "about:blank");
        assert_eq!(with_scheme("data:text/html,hi"), "data:text/html,hi");
        assert_eq!(with_scheme("blob:https://x/abcd"), "blob:https://x/abcd");
    }
}
