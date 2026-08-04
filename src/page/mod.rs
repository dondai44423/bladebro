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
pub use perception::{capture, capture_content, dismiss_consent, detect_block, wait_for_load, wait_for_settle, wait_for_settle_with_network, PageCapture, RawElement};
pub use refs::{RefEntry, StateChange, StateProbe};

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use crate::cdp::{CdpClient, CdpSession};
use crate::cdp::list_page_targets;
use crate::error::{BladeError, Result};

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
        cdp.enable("Page").await?;
        // DO NOT enable Runtime — DataDome's detection leverages the fact that
        // `Runtime.enable` changes console buffering behavior, making
        // `console.log(new Error())` trigger serialization (and thus getter
        // calls on Error.stack) only when CDP is connected. We can still use
        // `Runtime.evaluate` without enabling the domain — evaluate is a
        // standalone command, enable only turns on event notifications.
        cdp.enable("Network").await?;
        cdp.enable("DOM").await?;
        // Override the User-Agent to replace "HeadlessChrome" with "Chrome".
        // Uses Network.setUserAgentOverride with full Client Hints metadata
        // so the UA string, Sec-CH-UA headers, and navigator.userAgentData
        // are all consistent. This is a CDP-level override, not a JS override —
        // it can't be detected via property descriptor inspection.
        if let Err(e) = cdp
            .send(
                "Network.setUserAgentOverride",
                Some(serde_json::json!({
                    "userAgent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36",
                    "platform": "Linux x86_64",
                    "userAgentMetadata": {
                        "brands": [
                            {"brand": "Chromium", "version": "150"},
                            {"brand": "Google Chrome", "version": "150"},
                            {"brand": "Not:A-Brand", "version": "99"}
                        ],
                        "fullVersionList": [
                            {"brand": "Chromium", "version": "150.0.7871.128"},
                            {"brand": "Google Chrome", "version": "150.0.7871.128"}
                        ],
                        "platform": "Linux",
                        "platformVersion": "6.5.0",
                        "architecture": "x86",
                        "model": "",
                        "mobile": false,
                        "bitness": "64"
                    }
                })),
            )
            .await
        {
            eprintln!["[bladebro] WARNING: UA override failed: {e}"];
        }
        // S6: geo-consistent identity — timezone and locale must match the
        // proxy's geographic location. Without a proxy, the system timezone
        // is already correct. BLADE_TZ and BLADE_LOCALE override explicitly.
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

        // M17: Set download behavior so downloads don't hang the browser.
        let download_dir = std::env::temp_dir().join("bladebro-downloads");
        std::fs::create_dir_all(&download_dir).ok();
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
        let cdp_for_net = cdp.clone();
        let net_counter = in_flight.clone();
        let net_log_t = net_log.clone();
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
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Network.requestWillBeSent" => {
                        let id = ev.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() {
                            open.insert(id.to_string(), std::time::Instant::now());
                            let req = ev.params.get("request");
                            let method = req.and_then(|r| r.get("method")).and_then(|m| m.as_str()).unwrap_or("GET").to_string();
                            let url = req.and_then(|r| r.get("url")).and_then(|u| u.as_str()).unwrap_or("").to_string();
                            pending.insert(id.to_string(), (method, url, 0));
                        }
                    }
                    Ok(ev) if ev.method == "Network.responseReceived" => {
                        let id = ev.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                        let status = ev.params.get("response")
                            .and_then(|r| r.get("status"))
                            .and_then(|s| s.as_i64())
                            .unwrap_or(0);
                        if let Some(entry) = pending.get_mut(id) {
                            entry.2 = status;
                        }
                    }
                    Ok(ev) if ev.method == "Network.loadingFinished"
                        || ev.method == "Network.loadingFailed" =>
                    {
                        let id = ev.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() {
                            open.remove(id);
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
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Events were dropped — the set may now hold stale IDs.
                        // Clear rather than risk a permanently blocked settle.
                        open.clear();
                        pending.clear();
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                // Periodic sweep: remove entries older than 30 seconds.
                // Data URLs, long-poll connections, and server-sent events
                // may never fire loadingFinished, leaving stale entries.
                if last_sweep.elapsed().as_secs() >= 5 {
                    let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(30);
                    open.retain(|_, ts| *ts > cutoff);
                    last_sweep = std::time::Instant::now();
                }
                net_counter.store(open.len(), std::sync::atomic::Ordering::Relaxed);
            }
        });

        let mut lpm = LivePageModel::new();
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
        let cap = capture(&cdp).await?;
        lpm.ingest(cap);

        // S4+S5: shared action state for pacing + idle hum.
        let last_action_epoch = Arc::new(AtomicU64::new(0));
        let is_busy = Arc::new(AtomicBool::new(false));
        let hum_task = crate::stealth::spawn_hum(
            cdp.clone(),
            last_action_epoch.clone(),
            is_busy.clone(),
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

        Ok(Self {
            cdp,
            browser_client,
            lpm,
            dialogs,
            dialog_task: Some(dialog_task),
            in_flight,
            net_log,
            network_task: Some(network_task),
            ambient,
            base: base.to_string(),
            last_action_epoch,
            is_busy,
            hum_task: Some(hum_task),
            stealth_script_id,
            active_locale,
            intercept,
            intercept_task: Some(intercept_task),
            downloads,
            download_task: Some(download_task),
        })
    }

    /// The download tracker (V19). Newest download last.
    pub fn downloads(&self) -> std::sync::Arc<Mutex<Vec<DownloadInfo>>> {
        self.downloads.clone()
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
            let targets = list_page_targets(&self.base).await?;
            let t = targets
                .iter()
                .find(|t| t.id == target_id)
                .ok_or_else(|| BladeError::Other(format!("tab not found: {target_id}")))?;
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
    /// log-normal distribution matching real human think-time. Skipped for
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
            crate::action::Action::Click { .. } => (800.0, 0.6),
            crate::action::Action::Type { .. } => (500.0, 0.4),
            crate::action::Action::Scroll { .. } => (400.0, 0.5),
            crate::action::Action::Back => (1500.0, 0.7),
            crate::action::Action::Hover { .. } => (600.0, 0.5),
            // Wait/Read are perception, not human actions — no pacing.
            crate::action::Action::Wait { .. } | crate::action::Action::Read { .. } => return,
            _ => (600.0, 0.5),
        };
        let mut rng = crate::stealth::biometrics::Rng::new();
        let target = crate::stealth::biometrics::log_normal(&mut rng, median_ms, sigma);
        if elapsed < target.as_millis() as u64 {
            let sleep_for = target.as_millis() as u64 - elapsed;
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
            &self.cdp, &mut self.lpm, &action, Some(&self.in_flight)
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
                            &self.cdp, &mut self.lpm, &action, Some(&self.in_flight)
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
        let matches = crate::action::find_by_text(&self.cdp, &name, Some(&role)).await?;
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

    /// V8: read the console log captured by the injection hook.
    /// Returns raw JSON (array of {l, m, t}).
    pub async fn console_log(&self) -> Result<serde_json::Value> {
        let res = self.cdp.send("Runtime.evaluate", Some(serde_json::json!({
            "expression": "window.__uxa||[]",
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
        let _nav_t = std::time::Instant::now();
        let _t = |label: &str| {
            if std::env::var("NAV_TIMING").is_ok() {
                eprintln!("[nav-timing] {label}: {:?}", _nav_t.elapsed());
            }
        };
        let wait = self
            .cdp
            .wait_for("Page.frameNavigated", Duration::from_secs(10));
        self.cdp
            .send("Page.navigate", Some(serde_json::json!({ "url": url })))
            .await?;
        _t("sent");
        let _ = tokio::time::timeout(Duration::from_secs(10), wait).await;
        _t("frameNavigated");
        wait_for_load(&self.cdp, Duration::from_secs(10)).await?;
        _t("load");
        wait_for_settle_with_network(&self.cdp, Duration::from_secs(3), Some(&self.in_flight)).await?;
        _t("settle");
        // M4+M6: Check for consent banners and block pages after navigation.
        let consent = dismiss_consent(&self.cdp).await.unwrap_or(None);
        let blocked = detect_block(&self.cdp).await.unwrap_or(None);
        if let Some(ref fw) = consent {
            if let Ok(mut a) = self.ambient.lock() {
                a.push(format!("consent: {} ({})",
                    if std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".into()) != "accept" { "rejected" } else { "accepted" }, fw));
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
        _t("consent/block");
        let r = self.recapture().await;
        _t("recapture");
        r
    }

    /// S11: apply per-domain stealth settings from ~/.blade/profiles.json.
    /// Stores timezone and locale overrides per-domain so the driver remembers
    /// which settings work for each site. The agent can edit the file directly.
    async fn apply_domain_profile(&mut self, url: &str) {
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
fn extract_domain(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url)
        .split('/').next().unwrap_or("")
        .split(':').next().unwrap_or("")
        .trim_start_matches("www.")
        .to_string()
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
        if let Some(handle) = self.intercept_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.download_task.take() {
            handle.abort();
        }
    }
}
