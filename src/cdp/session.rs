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

use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

use crate::cdp::client::CdpClient;
use crate::cdp::protocol::CdpEvent;
use crate::error::Result;

/// A handle to one CDP session on a shared connection. Cheap to clone.
#[derive(Clone)]
pub struct CdpSession {
    client: CdpClient,
    session_id: Option<String>,
}

impl CdpSession {
    /// The root session of a connection (browser-level on pipe, or the whole
    /// connection on a direct page WebSocket).
    pub fn root(client: CdpClient) -> Self {
        Self { client, session_id: None }
    }

    /// A child session (flat-mode `Target.attachToTarget` result).
    pub fn child(client: CdpClient, session_id: impl Into<String>) -> Self {
        Self { client, session_id: Some(session_id.into()) }
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

    /// Send a command with the default timeout.
    pub async fn send(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.client
            .send_session_with_timeout(self.session_id.as_deref(), method, params, crate::cdp::client::DEFAULT_TIMEOUT)
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
        }
    }

    /// Wait for the next event on this session whose method equals `method`.
    pub async fn wait_for(&self, method: &str, deadline: Duration) -> Result<CdpEvent> {
        let mut sub = self.subscribe();
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

/// A filtered view of the connection's event bus for one session. Mirrors
/// `broadcast::Receiver`'s recv semantics so existing loops compile unchanged.
pub struct SessionSubscription {
    rx: broadcast::Receiver<CdpEvent>,
    session_id: Option<String>,
}

impl SessionSubscription {
    /// Receive the next event belonging to this session. Root sessions
    /// (no id) accept every event.
    pub async fn recv(&mut self) -> std::result::Result<CdpEvent, broadcast::error::RecvError> {
        loop {
            let ev = self.rx.recv().await?;
            match (&self.session_id, &ev.session_id) {
                (None, _) => return Ok(ev),
                (Some(mine), Some(theirs)) if mine == theirs => return Ok(ev),
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
