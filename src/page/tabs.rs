//! Tab lifecycle: listing, opening, switching, aliveness — plus the
//! isolated-world eval used by internal probes.

use super::*;
use crate::cdp::list_page_targets;

/// True when `BLADE_NO_ACTIVATE=1` is set in the process environment.
/// Suppresses `Page.bringToFront` across input, navigation, and tab paths and
/// opens new tabs in the background so Bladebro never raises the browser
/// window above the user's frontmost application.
pub fn no_activate_enabled() -> bool {
    no_activate_from(std::env::var("BLADE_NO_ACTIVATE").ok().as_deref())
}

fn no_activate_from(val: Option<&str>) -> bool {
    matches!(val, Some("1"))
}

impl Page {
    /// Input is delivered to this target, including after a foreground popup.
    pub async fn activate(&self) -> Result<()> {
        self.cdp
            .send_with_timeout("Page.bringToFront", None, Duration::from_secs(5))
            .await?;
        Ok(())
    }

    /// Is this target still on screen as its window's active tab? A
    /// background tab reports `document.visibilityState == "hidden"`; one
    /// that is merely behind another application's window still reads
    /// "visible". An answer we cannot read counts as inactive — the caller
    /// then activates, which is the pre-#89 behavior.
    pub async fn is_active_tab(&self) -> bool {
        active_from_visibility(
            self.cdp
                .send_with_timeout(
                    "Runtime.evaluate",
                    Some(serde_json::json!({
                        "expression": "document.visibilityState",
                        "returnByValue": true,
                    })),
                    Duration::from_secs(2),
                )
                .await
                .ok()
                .as_ref(),
        )
    }

    /// Activate only when this target is not already its window's active tab.
    /// `Page.bringToFront` activates the browser window at the OS level on
    /// macOS — every click, navigation, and tab switch used to raise it above
    /// whatever the person was working in (issues #89, #100). It is only
    /// needed when input would otherwise go to a background tab (a popup took
    /// the foreground, #49). `BLADE_NO_ACTIVATE=1` never raises the window,
    /// accepting that trade.
    pub async fn activate_if_needed(&self) -> Result<()> {
        self.activate_if_needed_with(no_activate_enabled()).await
    }

    async fn activate_if_needed_with(&self, no_activate: bool) -> Result<()> {
        if no_activate {
            return Ok(());
        }
        if self.is_active_tab().await {
            return Ok(());
        }
        self.activate().await
    }

    /// Evaluate a JS expression in an isolated world. The isolated world has
    /// DOM access but its own JavaScript context — anti-bot scripts in the
    /// main world cannot observe our queries via patched DOM methods or
    /// Error.stack frames. Lazily creates the world; recreates on navigation.
    /// Falls back to regular Runtime.evaluate if the isolated world fails.
    pub async fn eval_isolated(&self, expr: &str) -> Result<Value> {
        self.cdp
            .eval_isolated_with_timeout(expr, true, true, Duration::from_secs(30))
            .await
    }

    /// Reset the isolated world context (call on navigation).
    pub fn reset_isolated(&self) {
        self.cdp.reset_isolated();
    }

    /// List the browser's page targets — over the pipe's browser-level
    /// connection in pipe mode, over the HTTP debug endpoint in WS mode.
    pub(super) async fn list_page_targets(&self) -> Vec<crate::cdp::TargetInfo> {
        if let Some(bc) = &self.browser_client {
            // Short timeout: Target.getTargets is browser-level and should
            // return in <10ms. If Chrome is busy (page navigating, pipe
            // congested), don't block the click flow for 30s — skip tab
            // detection instead.
            match bc
                .send_with_timeout("Target.getTargets", None, Duration::from_secs(3))
                .await
            {
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
                                    attached: t
                                        .get("attached")
                                        .and_then(|a| a.as_bool())
                                        .unwrap_or(false),
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
        self.open_tab_target_with(url, no_activate_enabled()).await
    }

    async fn open_tab_target_with(&self, url: &str, background: bool) -> Result<String> {
        // Bare hosts must work here exactly like `nav`: Target.createTarget
        // with a scheme-less URL ("localhost:3000/x") never loads — Chrome
        // reads "localhost:" as the scheme — leaving a stuck about:blank tab
        // whose screenshot then burns its full CDP timeout (reproduced live).
        let url = with_scheme(url);
        let mut params = json!({"url":url.as_str()});
        if background {
            params["background"] = json!(true);
        }
        if let Some(id) = self.cdp.browser_context_id() {
            params["browserContextId"] = json!(id);
        }
        if let Some(bc) = &self.browser_client {
            let res = bc.send("Target.createTarget", Some(params)).await?;
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
        let res = client.send("Target.createTarget", Some(params)).await?;
        res.get("targetId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| BladeError::Other("no targetId from Target.createTarget".into()))
    }

    /// Attach an auxiliary tab without detaching or replacing the active page.
    pub(crate) async fn attach_tab_target(&self, target_id: &str) -> Result<Page> {
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
                .ok_or_else(|| {
                    BladeError::Other("Target.attachToTarget returned no sessionId".into())
                })?;
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
        Page::attach(session, &self.base, self.browser_client.clone()).await
    }

    /// Switch the session to a different tab (target id from
    /// `state tabs`). The whole page state is rebuilt against the
    /// new tab: domains re-enabled, stealth re-injected, fresh
    /// capture. The old tab stays OPEN — this is focus switching,
    /// not closing. `*self = new_page` drops the old Page, whose
    /// Drop aborts its background tasks (dialogs/network/hum).
    pub async fn switch_tab(&mut self, target_id: &str) -> Result<()> {
        self.switch_tab_with(target_id, no_activate_enabled()).await
    }

    async fn switch_tab_with(&mut self, target_id: &str, no_activate: bool) -> Result<()> {
        // Attach to the NEW target FIRST. The old code detached
        // the current session first — if the attach then failed,
        // the session was detached from everything (bricked).
        let new_page = self.attach_tab_target(target_id).await?;
        // Honor BLADE_NO_ACTIVATE and avoid raising the OS window when the
        // target is already its window's active tab (#100). A real background
        // tab still gets activated when BLADE_NO_ACTIVATE is unset.
        new_page.activate_if_needed_with(no_activate).await?;
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
}

/// The activation decision from a `Runtime.evaluate` reply to
/// `document.visibilityState`: only an explicit "visible" means the target is
/// already its window's active tab, so `Page.bringToFront` is skipped. A
/// hidden tab, an error or an unreadable reply activates — the pre-#89
/// behavior, because input to a background tab was the #49 bug.
fn active_from_visibility(reply: Option<&Value>) -> bool {
    reply
        .and_then(|r| r.get("result"))
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        == Some("visible")
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::tungstenite::{protocol::Role, Message};
    use tokio_tungstenite::WebSocketStream;

    #[test]
    fn only_a_visible_page_skips_activation() {
        for (reply, active) in [
            (
                Some(json!({"result": {"type": "string", "value": "visible"}})),
                true,
            ),
            (
                Some(json!({"result": {"type": "string", "value": "hidden"}})),
                false,
            ),
            (Some(json!({"result": {}})), false),
            (Some(json!({"error": {"message": "target gone"}})), false),
            (None, false),
        ] {
            assert_eq!(active_from_visibility(reply.as_ref()), active, "{reply:?}");
        }
    }

    #[test]
    fn no_activate_parses_strictly() {
        assert!(no_activate_from(Some("1")));
        assert!(!no_activate_from(Some("0")));
        assert!(!no_activate_from(Some("")));
        assert!(!no_activate_from(None));
    }

    /// Mock CDP server recording every `(sessionId, method, params)` sent by
    /// the client, answering `document.visibilityState` with `visibility` and
    /// `Target.createTarget` / `Target.attachToTarget` with synthetic ids.
    async fn mock_page(
        visibility: &'static str,
    ) -> (Page, Arc<Mutex<Vec<(String, String, Value)>>>) {
        let (near, far) = tokio::io::duplex(1 << 20);
        let client_ws = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let mut browser = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        let client = CdpClient::from_socket(client_ws).unwrap();
        let seen: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_bg = seen.clone();
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = browser.next().await {
                let req: Value = serde_json::from_str(text.as_str()).unwrap();
                let id = req["id"].clone();
                let sid = req["sessionId"].as_str().unwrap_or("").to_string();
                let method = req["method"].as_str().unwrap_or("").to_string();
                let params = req.get("params").cloned().unwrap_or(json!({}));
                seen_bg
                    .lock()
                    .unwrap()
                    .push((sid.clone(), method.clone(), params.clone()));

                let result = match method.as_str() {
                    "Target.createTarget" => json!({ "targetId": "T_NEW" }),
                    "Target.attachToTarget" => json!({ "sessionId": "S_NEW" }),
                    "Runtime.evaluate" => {
                        let expr = params
                            .get("expression")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if expr == "document.visibilityState" {
                            json!({ "result": { "type": "string", "value": visibility } })
                        } else if expr.contains("navigator.userAgent") {
                            json!({
                                "result": {
                                    "type": "string",
                                    "value": r#"{"ua":"Chrome/150","plt":"MacIntel","wd":false,"href":"about:blank"}"#
                                }
                            })
                        } else {
                            json!({ "result": { "type": "undefined" } })
                        }
                    }
                    _ => json!({}),
                };

                let mut reply = json!({ "id": id, "result": result });
                if !sid.is_empty() {
                    reply["sessionId"] = json!(sid);
                }
                if browser
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let session = CdpSession::child(client.clone(), "S_INIT");
        let page = Page::attach(session, "mock", Some(client)).await.unwrap();
        seen.lock().unwrap().clear();
        (page, seen)
    }

    #[tokio::test]
    async fn switch_tab_skips_bring_to_front_under_no_activate_and_activates_hidden_tab_by_default()
    {
        // Background (hidden) tab + BLADE_NO_ACTIVATE=1 -> zero Page.bringToFront.
        let (mut page, seen) = mock_page("hidden").await;
        page.switch_tab_with("T_BG", true).await.unwrap();
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m, _)| m.clone())
            .collect();
        assert!(
            !methods.iter().any(|m| m == "Page.bringToFront"),
            "BLADE_NO_ACTIVATE=1 must suppress Page.bringToFront on switch_tab: {methods:?}"
        );

        // Background (hidden) tab + default -> emits Page.bringToFront.
        let (mut page, seen) = mock_page("hidden").await;
        page.switch_tab_with("T_BG", false).await.unwrap();
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m, _)| m.clone())
            .collect();
        assert!(
            methods.iter().any(|m| m == "Page.bringToFront"),
            "default switch_tab to a hidden tab must emit Page.bringToFront: {methods:?}"
        );

        // Already-visible tab + default -> skips Page.bringToFront (no OS window raise).
        let (mut page, seen) = mock_page("visible").await;
        page.switch_tab_with("T_VIS", false).await.unwrap();
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m, _)| m.clone())
            .collect();
        assert!(
            !methods.iter().any(|m| m == "Page.bringToFront"),
            "already-visible tab should not emit Page.bringToFront: {methods:?}"
        );
    }

    #[tokio::test]
    async fn open_tab_target_sets_background_only_when_no_activate() {
        let (page, seen) = mock_page("visible").await;
        let id_bg = page
            .open_tab_target_with("https://example.org", true)
            .await
            .unwrap();
        assert_eq!(id_bg, "T_NEW");
        let bg_params = seen
            .lock()
            .unwrap()
            .iter()
            .find(|(_, m, _)| m == "Target.createTarget")
            .map(|(_, _, p)| p.clone())
            .expect("Target.createTarget sent");
        assert_eq!(bg_params["background"], json!(true));

        seen.lock().unwrap().clear();
        let id_fg = page
            .open_tab_target_with("https://example.org", false)
            .await
            .unwrap();
        assert_eq!(id_fg, "T_NEW");
        let fg_params = seen
            .lock()
            .unwrap()
            .iter()
            .find(|(_, m, _)| m == "Target.createTarget")
            .map(|(_, _, p)| p.clone())
            .expect("Target.createTarget sent");
        assert!(
            fg_params.get("background").is_none(),
            "default open-tab must omit background: {fg_params}"
        );
    }
}
