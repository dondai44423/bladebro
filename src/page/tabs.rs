//! Tab lifecycle: listing, opening, switching, aliveness — plus the
//! isolated-world eval used by internal probes.

use super::*;
use crate::cdp::list_page_targets;

impl Page {
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
            let res = self
                .cdp
                .send(
                    "Runtime.callFunctionOn",
                    Some(json!({
                        "executionContextId": id,
                        "functionDeclaration": format!("function() {{ return ({expr}); }}"),
                        "returnByValue": true,
                        "awaitPromise": true,
                    })),
                )
                .await;

            match res {
                Ok(v) => return Ok(v),
                Err(_) => {
                    // Context is stale (navigation) — clear and recreate.
                    *self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
        }

        // Lazily create the isolated world.
        let frame_id = self
            .cdp
            .send("Page.getFrameTree", None)
            .await
            .ok()
            .and_then(|v| {
                v.get("frameTree")?
                    .get("frame")?
                    .get("id")?
                    .as_str()
                    .map(String::from)
            })
            .unwrap_or_default();
        if let Ok(v) = self
            .cdp
            .send(
                "Page.createIsolatedWorld",
                Some(json!({
                    "frameId": frame_id,
                    "worldName": "",
                    "grantUniveralAccess": true,
                })),
            )
            .await
        {
            if let Some(id) = v.get("executionContextId").and_then(|i| i.as_i64()) {
                *self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                return self
                    .cdp
                    .send(
                        "Runtime.callFunctionOn",
                        Some(json!({
                            "executionContextId": id,
                            "functionDeclaration": format!("function() {{ return ({expr}); }}"),
                            "returnByValue": true,
                            "awaitPromise": true,
                        })),
                    )
                    .await;
            }
        }

        // Fallback: regular Runtime.evaluate.
        self.cdp
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expr,
                    "returnByValue": true,
                    "awaitPromise": true,
                })),
            )
            .await
    }

    /// Reset the isolated world context (call on navigation).
    pub fn reset_isolated(&self) {
        *self.isolated_ctx.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
}
