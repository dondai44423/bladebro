//! The page layer: a [`Page`] handle ties a [`CdpClient`](crate::cdp::CdpClient)
//! connection to a [`LivePageModel`] and exposes the capture / observe loop.
//!
//! `Page` is what the `act` / `see` / `run` MCP tools operate on.
//! It owns the LPM across captures so refs stay stable and diffs accumulate.
//!
//! Module map: this file is the Page struct + observation accessors and the
//! Drop contract; `attach` builds it, `tabs` / `navigate` / `heal` / `logs` hold
//! the method families, and `model` / `perception` / `refs` / `intercept` are the
//! capture pipeline.

pub mod intercept;
pub mod model;
pub mod perception;
pub mod refs;

pub mod attach;
pub mod heal;
pub mod logs;
pub mod navigate;
pub mod tabs;

pub use self::logs::{NetEntry, XhrEntry};
pub(crate) use self::navigate::with_scheme;

use std::time::Duration;

pub use model::{LivePageModel, PageDelta, PageElement};
pub use perception::{
    capture, capture_content, detect_block, dismiss_consent, dismiss_consent_with_stored,
    re_settle, wait_for_load, wait_for_settle, wait_for_settle_with_network, PageCapture,
    RawElement,
};
pub use refs::{RefEntry, StateChange, StateProbe};

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex};

use crate::cdp::{CdpClient, CdpSession};
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
        self.act_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reset the act turn counter to 0 (called on navigation, see, or error).
    pub fn reset_act_turn(&self) {
        self.act_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Is context pruning enabled?
    pub fn compress_enabled(&self) -> bool {
        self.compress_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Toggle context pruning on/off at runtime.
    pub fn set_compress_enabled(&self, enabled: bool) {
        self.compress_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Shared last-mouse-position tracker for movementX/movementY calculation.
    /// Used by action dispatch and idle hum to produce realistic mouse deltas.
    pub fn last_mouse(&self) -> Arc<std::sync::Mutex<Option<(f64, f64)>>> {
        self.last_mouse.clone()
    }
}

impl Page {
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
        let mask = if spec.trim().eq_ignore_ascii_case("none")
            || spec.trim().eq_ignore_ascii_case("clear")
        {
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
        } else if spec.is_empty()
            || spec.eq_ignore_ascii_case("none")
            || spec.eq_ignore_ascii_case("clear")
        {
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
        self.lpm.compress(
            budget,
            self.in_flight.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn view_filtered(&self, budget: usize, filter: &str) -> String {
        self.lpm.compress_filtered(
            budget,
            filter,
            self.in_flight.load(std::sync::atomic::Ordering::Relaxed),
        )
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

    /// Scoped markdown: ONE element's subtree (`see mode=content scope=eN`).
    /// Bypasses site branches; budget is honored inside the subtree.
    pub async fn markdown_scoped(
        &self,
        budget: usize,
        sig: &str,
        frame: &[usize],
    ) -> Result<String> {
        crate::page::perception::capture_markdown_scoped(&self.cdp, budget, sig, frame).await
    }

    /// Extract just the page title + heading hierarchy. Ultra-minimal.
    pub async fn outline(&self) -> Result<String> {
        crate::page::perception::capture_outline(&self.cdp).await
    }

    /// The delta since the last capture, rendered (the observation).
    pub fn delta_view(&self, d: &PageDelta, budget: usize) -> String {
        self.lpm.compress_delta(
            d,
            budget,
            self.in_flight.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// S5: pacing governor — sleep so the inter-action gap follows a
    /// log-normal distribution matching fast human think-time. Skipped for
    /// the first action, disabled by BLADE_PACE=off.
    async fn pace(&mut self, action: &crate::action::Action) {
        if std::env::var("BLADE_PACE").as_deref() == Ok("off") {
            return;
        }
        let last = self
            .last_action_epoch
            .load(std::sync::atomic::Ordering::Relaxed);
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
        self.is_busy
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // M5: For clicks, detect new tabs (target=_blank opens a new page).
        let is_click = matches!(
            action,
            crate::action::Action::Click { .. } | crate::action::Action::ClickCoord { .. }
        );
        let before = if is_click {
            let r = self.list_page_targets().await;
            r
        } else {
            Vec::new()
        };
        let result = crate::action::perform_with_network(
            &self.cdp,
            &mut self.lpm,
            &action,
            Some(&self.in_flight),
            &self.last_mouse,
        )
        .await;
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
                            &self.cdp,
                            &mut self.lpm,
                            &action,
                            Some(&self.in_flight),
                            &self.last_mouse,
                        )
                        .await?
                    }
                    None => {
                        self.is_busy
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        return Err(BladeError::ElementNotFound(format!(
                            "{ref_id} not in the live DOM and cannot be re-resolved"
                        )));
                    }
                }
            }
            Err(e) => {
                self.is_busy
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return Err(e);
            }
        };
        let verdict = match heal_note {
            Some(note) => format!("{verdict} [{note}]"),
            None => verdict,
        };
        if is_click {
            let after = self.list_page_targets().await;
            let new_tabs: Vec<_> = after
                .iter()
                .filter(|t| !before.iter().any(|b| b.id == t.id))
                .collect();
            if !new_tabs.is_empty() {
                // Override verdict: a new tab opened even if the current page
                // didn't change. This is the correct outcome for target=_blank
                // links and window.open() calls.
                let tab_info: Vec<String> = new_tabs
                    .iter()
                    .map(|t| {
                        if t.title.is_empty() {
                            t.url.clone()
                        } else {
                            t.title.clone()
                        }
                    })
                    .collect();
                let new_verdict = format!("outcome: new tab opened — {}", tab_info.join(", "));
                if let Ok(mut a) = self.ambient.lock() {
                    a.push(new_verdict.clone());
                }
                return Ok((delta, new_verdict));
            }
        }
        // S4+S5: mark action complete — hum resumes, next action paces.
        self.is_busy
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_action_epoch
            .store(now, std::sync::atomic::Ordering::Relaxed);
        Ok((delta, verdict))
    }
}

impl Page {
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
    use super::navigate::with_scheme;

    #[test]
    fn with_scheme_handles_all_forms() {
        // Bare public hosts get https.
        assert_eq!(with_scheme("example.com"), "https://example.com");
        assert_eq!(
            with_scheme("example.com/path?q=1"),
            "https://example.com/path?q=1"
        );
        // Local/private targets get http (dev servers rarely have certs).
        assert_eq!(with_scheme("localhost:3000"), "http://localhost:3000");
        assert_eq!(with_scheme("localhost"), "http://localhost");
        assert_eq!(with_scheme("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(with_scheme("192.168.1.5"), "http://192.168.1.5");
        // Explicit ports imply a dev server: http.
        assert_eq!(
            with_scheme("myserver.test:8443"),
            "http://myserver.test:8443"
        );
        // Existing schemes untouched.
        assert_eq!(with_scheme("https://x.com"), "https://x.com");
        assert_eq!(with_scheme("http://x.com"), "http://x.com");
        assert_eq!(with_scheme("file:///tmp/x.html"), "file:///tmp/x.html");
        assert_eq!(with_scheme("about:blank"), "about:blank");
        assert_eq!(with_scheme("data:text/html,hi"), "data:text/html,hi");
        assert_eq!(with_scheme("blob:https://x/abcd"), "blob:https://x/abcd");
    }
}
