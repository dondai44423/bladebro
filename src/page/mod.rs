//! The page layer: a [`Page`] handle ties a [`CdpClient`](crate::cdp::CdpClient)
//! connection to a [`LivePageModel`] and exposes the capture / observe loop.
//!
//! `Page` is what the future `act` / `see` / `run` MCP tools will operate on.
//! It owns the LPM across captures so refs stay stable and diffs accumulate.

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
use crate::error::Result;

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

/// A live page session: one CDP connection + its persistent Live Page Model.
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
        let in_flight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let cdp_for_net = cdp.clone();
        let net_counter = in_flight.clone();
        let network_task = tokio::spawn(async move {
            use std::collections::HashSet;
            let mut rx = cdp_for_net.subscribe();
            // Track request IDs, not a raw counter: requestWillBeSent fires
            // once per REDIRECT HOP for the same requestId while
            // loadingFinished fires once — a counter drifts +1 per hop and
            // eventually every settle waits the full timeout.
            let mut open: HashSet<String> = HashSet::new();
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Network.requestWillBeSent" => {
                        if let Some(id) = ev.params.get("requestId").and_then(|v| v.as_str()) {
                            open.insert(id.to_string());
                        }
                    }
                    Ok(ev) if ev.method == "Network.loadingFinished"
                        || ev.method == "Network.loadingFailed" =>
                    {
                        if let Some(id) = ev.params.get("requestId").and_then(|v| v.as_str()) {
                            open.remove(id);
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Events were dropped — the set may now hold stale IDs.
                        // Clear rather than risk a permanently blocked settle.
                        open.clear();
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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

        Ok(Self {
            cdp,
            browser_client,
            lpm,
            dialogs,
            dialog_task: Some(dialog_task),
            in_flight,
            network_task: Some(network_task),
            ambient,
            base: base.to_string(),
            last_action_epoch,
            is_busy,
            hum_task: Some(hum_task),
            stealth_script_id,
            active_locale,
        })
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

    /// Re-capture the page and return the delta since the last capture.
    pub async fn recapture(&mut self) -> Result<PageDelta> {
        let cap = capture(&self.cdp).await?;
        Ok(self.lpm.ingest(cap))
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
    pub async fn act(&mut self, action: crate::action::Action) -> Result<(PageDelta, String)> {
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
        let (delta, verdict) = crate::action::perform_with_network(
            &self.cdp, &mut self.lpm, &action, Some(&self.in_flight)
        ).await?;
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
        let wait = self
            .cdp
            .wait_for("Page.frameNavigated", Duration::from_secs(15));
        self.cdp
            .send("Page.navigate", Some(serde_json::json!({ "url": url })))
            .await?;
        let _ = tokio::time::timeout(Duration::from_secs(15), wait).await;
        wait_for_load(&self.cdp, Duration::from_secs(10)).await?;
        wait_for_settle_with_network(&self.cdp, Duration::from_secs(5), Some(&self.in_flight)).await?;
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
        self.recapture().await
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
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let s = s.split('#').next().unwrap_or(s);
    let s = s.strip_suffix('/').unwrap_or(s);
    s.to_string()
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
    }
}
