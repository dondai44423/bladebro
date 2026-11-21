//! Session-scoped CDP access (S1: pipe transport multiplexing).
//!
//! One connection can carry many targets in flat mode: the browser-level
//! session plus one sub-session per attached target. [`CdpSession`] wraps a
//! shared [`CdpClient`] with an optional session id so the rest of the driver
//! (Page, actions, state, perception) speaks to exactly one session without
//! knowing which transport or mode is underneath.
//!
//! - WS mode (direct page WebSocket): session id is `None`; every call hits
//!   the page's own connection, events arrive unfiltered.
//! - Pipe mode (`--remote-debugging-pipe`): the root session drives browser
//!   domains (Target.*), child sessions drive page domains; events are
//!   filtered to the child's session id.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::cdp::client::CdpClient;
use crate::cdp::protocol::CdpEvent;
use crate::error::{BladeError, Result};

/// A handle to one CDP session on a shared connection. Cheap to clone.
#[derive(Clone)]
pub struct CdpSession {
    client: CdpClient,
    session_id: Option<String>,
    cookie_context: Option<(CdpClient, String)>,
    main_frame_id: Arc<Mutex<Option<String>>>,
    isolated_ctx: Arc<Mutex<Option<i64>>>,
}

impl CdpSession {
    /// The root session of a connection (browser-level on pipe, or the whole
    /// connection on a direct page WebSocket).
    pub fn root(client: CdpClient) -> Self {
        Self {
            client,
            session_id: None,
            cookie_context: None,
            main_frame_id: Arc::new(Mutex::new(None)),
            isolated_ctx: Arc::new(Mutex::new(None)),
        }
    }

    /// A child session (flat-mode `Target.attachToTarget` result).
    pub fn child(client: CdpClient, session_id: impl Into<String>) -> Self {
        Self {
            client,
            session_id: Some(session_id.into()),
            cookie_context: None,
            main_frame_id: Arc::new(Mutex::new(None)),
            isolated_ctx: Arc::new(Mutex::new(None)),
        }
    }

    /// Record the main frame id for isolated-world creation (set once at attach).
    pub(crate) fn set_main_frame(&self, frame_id: String) {
        if !frame_id.is_empty() {
            if let Ok(mut g) = self.main_frame_id.lock() {
                *g = Some(frame_id);
            }
        }
    }

    /// Drop the cached isolated-world execution context id (called on navigation).
    pub(crate) fn reset_isolated(&self) {
        if let Ok(mut g) = self.isolated_ctx.lock() {
            *g = None;
        }
    }

    async fn ensure_isolated_ctx(&self) -> Option<i64> {
        if let Ok(g) = self.isolated_ctx.lock() {
            if let Some(id) = *g {
                return Some(id);
            }
        }
        let frame_id = self.main_frame_id.lock().ok().and_then(|g| g.clone())?;
        let res = self
            .send_with_timeout(
                "Page.createIsolatedWorld",
                Some(json!({
                    "frameId": frame_id,
                    "worldName": "",
                    "grantUniveralAccess": true,
                })),
                Duration::from_secs(3),
            )
            .await
            .ok()?;
        let id = res.get("executionContextId")?.as_i64()?;
        if let Ok(mut g) = self.isolated_ctx.lock() {
            *g = Some(id);
        }
        Some(id)
    }

    pub(crate) fn is_stale_context_err(err: &BladeError) -> bool {
        let msg = err.to_string();
        msg.contains("Cannot find context")
            || msg.contains("Execution context was destroyed")
            || msg.contains("Inspected target navigated")
    }

    /// Evaluate JS in a cached isolated world when available (invisible to
    /// main-world `window` / `Object.getOwnPropertySymbols(window)` / patched
    /// DOM APIs), falling back to main-world `Runtime.evaluate` only when
    /// isolated worlds are unavailable (e.g. mock CDP servers in tests).
    pub(crate) async fn eval_isolated_with_timeout(
        &self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
        deadline: Duration,
    ) -> Result<Value> {
        for attempt in 0..3 {
            let Some(ctx_id) = self.ensure_isolated_ctx().await else {
                break;
            };
            match self
                .send_with_timeout(
                    "Runtime.evaluate",
                    Some(json!({
                        "contextId": ctx_id,
                        "expression": expression,
                        "returnByValue": return_by_value,
                        "awaitPromise": await_promise,
                    })),
                    deadline,
                )
                .await
            {
                Ok(v) => return Ok(v),
                Err(e) if matches!(e, BladeError::Closed | BladeError::Timeout(_)) => {
                    return Err(e)
                }
                Err(e) if attempt < 2 && Self::is_stale_context_err(&e) => {
                    self.reset_isolated();
                    tokio::time::sleep(Duration::from_millis(150 + attempt * 200)).await;
                    continue;
                }
                Err(_) => {
                    self.reset_isolated();
                    break;
                }
            }
        }
        match self
            .send_with_timeout(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expression,
                    "returnByValue": return_by_value,
                    "awaitPromise": await_promise,
                })),
                deadline,
            )
            .await
        {
            Err(e) if Self::is_stale_context_err(&e) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                self.send_with_timeout(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": expression,
                        "returnByValue": return_by_value,
                        "awaitPromise": await_promise,
                    })),
                    deadline,
                )
                .await
            }
            other => other,
        }
    }

    /// Evaluate a stealth-sensitive watcher strictly inside the isolated world
    /// on real browsers (so it never writes symbols or observers onto the
    /// page's main-world `window`). Falls back to main-world only when no main
    /// frame id was attached (mock CDP servers in tests).
    pub(crate) async fn eval_isolated_strict(
        &self,
        expression: &str,
        deadline: Duration,
    ) -> Result<()> {
        let has_frame = self.main_frame_id.lock().ok().is_some_and(|g| g.is_some());
        if !has_frame {
            let _ = self
                .send_with_timeout(
                    "Runtime.evaluate",
                    Some(json!({ "expression": expression })),
                    deadline,
                )
                .await;
            return Ok(());
        }
        for attempt in 0..2 {
            let Some(ctx_id) = self.ensure_isolated_ctx().await else {
                return Ok(());
            };
            match self
                .send_with_timeout(
                    "Runtime.evaluate",
                    Some(json!({
                        "contextId": ctx_id,
                        "expression": expression,
                    })),
                    deadline,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) if attempt == 0 && Self::is_stale_context_err(&e) => {
                    self.reset_isolated();
                    continue;
                }
                Err(_) => {
                    self.reset_isolated();
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The underlying connection (for browser-level commands from a child).
    pub fn client(&self) -> &CdpClient {
        &self.client
    }

    /// Has the transport been closed?
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// This session's id, if it is a child session.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn set_cookie_context(&mut self, browser: CdpClient, context: String) {
        self.cookie_context = Some((browser, context));
    }

    pub(crate) fn browser_context_id(&self) -> Option<&str> {
        self.cookie_context.as_ref().map(|(_, id)| id.as_str())
    }

    /// Storage's default is the regular browser profile, even on an OTR page.
    /// Only a browser-level command accepts browserContextId.
    pub(crate) async fn cookie_store(&self) -> Result<Value> {
        if let Some((browser, context)) = &self.cookie_context {
            browser
                .send(
                    "Storage.getCookies",
                    Some(serde_json::json!({"browserContextId":context})),
                )
                .await
        } else {
            self.send("Storage.getCookies", None).await
        }
    }

    /// Send a command with the default timeout.
    pub async fn send(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.client
            .send_session_with_timeout(
                self.session_id.as_deref(),
                method,
                params,
                crate::cdp::client::DEFAULT_TIMEOUT,
            )
            .await
    }

    /// Send a command with an explicit timeout.
    pub async fn send_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        self.client
            .send_session_with_timeout(self.session_id.as_deref(), method, params, deadline)
            .await
    }

    /// Enable a CDP domain on this session.
    pub async fn enable(&self, domain: &str) -> Result<Value> {
        self.send(&format!("{domain}.enable"), None).await
    }

    /// Disable a CDP domain on this session.
    pub async fn disable(&self, domain: &str) -> Result<Value> {
        self.send(&format!("{domain}.disable"), None).await
    }

    /// Subscribe to this session's event stream. On a child session, events
    /// belonging to other sessions are filtered out transparently.
    pub fn subscribe(&self) -> SessionSubscription {
        SessionSubscription {
            rx: self.client.subscribe(),
            session_id: self.session_id.clone(),
            closed: false,
        }
    }

    /// Wait for the next event on this session whose method equals `method`.
    pub fn wait_for(
        &self,
        method: &str,
        deadline: Duration,
    ) -> impl std::future::Future<Output = Result<CdpEvent>> {
        let mut sub = self.subscribe();
        let method = method.to_string();
        async move {
            tokio::time::timeout(deadline, async {
                loop {
                    match sub.recv().await {
                        Ok(ev) if ev.method == method => return Ok(ev),
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(crate::error::BladeError::Closed)
                        }
                    }
                }
            })
            .await
            .map_err(|_| crate::error::BladeError::Timeout(deadline))?
        }
    }
}

/// A filtered view of the connection's event bus for one session. Mirrors
/// `broadcast::Receiver`'s recv semantics so existing loops compile unchanged.
pub struct SessionSubscription {
    rx: broadcast::Receiver<CdpEvent>,
    session_id: Option<String>,
    closed: bool,
}

impl SessionSubscription {
    /// Receive the next event belonging to this session. Root sessions
    /// (no id) accept every event; child sessions end with `Closed` once
    /// Chrome detaches their target (`Target.detachedFromTarget`).
    pub async fn recv(&mut self) -> std::result::Result<CdpEvent, broadcast::error::RecvError> {
        if self.closed {
            return Err(broadcast::error::RecvError::Closed);
        }
        loop {
            let ev = self.rx.recv().await?;
            match (&self.session_id, &ev.session_id) {
                (None, _) => return Ok(ev),
                (Some(mine), Some(theirs)) if mine == theirs => return Ok(ev),
                (Some(mine), None)
                    if ev.method == "Target.detachedFromTarget"
                        && ev.params.get("sessionId").and_then(Value::as_str)
                            == Some(mine.as_str()) =>
                {
                    self.closed = true;
                    return Err(broadcast::error::RecvError::Closed);
                }
                _ => continue,
            }
        }
    }
}

impl std::fmt::Debug for CdpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpSession")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn child_subscription_closes_on_matching_target_detach() {
        let (tx, rx) = broadcast::channel(16);
        let mut sub = SessionSubscription {
            rx,
            session_id: Some("sess-a".into()),
            closed: false,
        };
        tx.send(CdpEvent {
            method: "Target.detachedFromTarget".into(),
            params: json!({"sessionId": "sess-a", "targetId": "t-a"}),
            session_id: None,
        })
        .unwrap();
        tx.send(CdpEvent {
            method: "Page.loadEventFired".into(),
            params: json!({}),
            session_id: Some("sess-a".into()),
        })
        .unwrap();

        assert!(matches!(
            sub.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
        assert!(matches!(
            sub.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn child_subscription_ignores_unrelated_detach_and_root_receives_all() {
        let (tx, rx_child) = broadcast::channel(16);
        let rx_root = tx.subscribe();
        let mut child = SessionSubscription {
            rx: rx_child,
            session_id: Some("sess-a".into()),
            closed: false,
        };
        let mut root = SessionSubscription {
            rx: rx_root,
            session_id: None,
            closed: false,
        };

        tx.send(CdpEvent {
            method: "Target.detachedFromTarget".into(),
            params: json!({"sessionId": "sess-other", "targetId": "t-other"}),
            session_id: None,
        })
        .unwrap();
        tx.send(CdpEvent {
            method: "Page.loadEventFired".into(),
            params: json!({"ts": 1}),
            session_id: Some("sess-a".into()),
        })
        .unwrap();

        let root_ev = root.recv().await.unwrap();
        assert_eq!(root_ev.method, "Target.detachedFromTarget");

        let child_ev = child.recv().await.unwrap();
        assert_eq!(child_ev.method, "Page.loadEventFired");
    }

    #[tokio::test]
    async fn eval_isolated_recovers_from_inspected_target_navigated() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut calls = 0usize;
            for step in 0..2 {
                let req = socket.next().await.unwrap().unwrap();
                let req: Value = serde_json::from_str(req.to_text().unwrap()).unwrap();
                calls += 1;
                let msg = if step == 0 {
                    json!({
                        "id": req["id"],
                        "error": { "code": -32000, "message": "Inspected target navigated or closed" }
                    })
                } else {
                    json!({
                        "id": req["id"],
                        "result": { "result": { "value": "recovered" } }
                    })
                };
                socket
                    .send(Message::Text(msg.to_string().into()))
                    .await
                    .unwrap();
            }
            calls
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let res = cdp
            .eval_isolated_with_timeout("'ok'", true, false, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(res["result"]["value"], "recovered");
        assert_eq!(task.await.unwrap(), 2);
    }
}
