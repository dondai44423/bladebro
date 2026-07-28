//! CDP message framing.
//!
//! CDP is JSON over WebSocket. Outgoing commands carry an `id`; the browser
//! replies with a matching `id` (success → `result`, failure → `error`).
//! Asynchronous notifications carry a `method` + `params` and no `id`.
//!
//! We keep the wire types generic: `params`/`result` are `serde_json::Value`.
//! This is deliberate — a thin layer that doesn't need to codegen every CDP
//! domain. Typed convenience wrappers live on [`crate::cdp::CdpClient`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An outgoing CDP command. Serialized to `{"id","method","params"}`.
#[derive(Debug, Serialize)]
pub struct CdpRequest {
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Set when targeting a sub-session (e.g. an attached target). `None`
    /// for the common case of connecting directly to a page's WebSocket.
    #[serde(skip_serializing_if = "Option::is_none", rename = "sessionId")]
    pub session_id: Option<String>,
}

impl CdpRequest {
    /// Build a request with the next id; the caller owns id assignment.
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self { id, method: method.into(), params, session_id: None }
    }
}

/// The error object embedded in a failed CDP response.
#[derive(Debug, Deserialize)]
pub struct CdpErrorPayload {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// An asynchronous event pushed by the browser.
#[derive(Debug, Clone)]
pub struct CdpEvent {
    /// e.g. `Page.frameNavigated`, `Runtime.executionContextCreated`.
    pub method: String,
    pub params: Value,
    /// Present when the event belongs to a sub-session.
    pub session_id: Option<String>,
}

/// Raw incoming frame, parsed into a single struct with all-optional fields,
/// then discriminated manually. This is more robust than a `#[serde(untagged)]`
/// enum: a response always has `id`, an event always has `method`, and we
/// branch on `id` presence without relying on serde's variant-order guessing.
#[derive(Debug, Deserialize)]
pub(crate) struct CdpMessage {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<CdpErrorPayload>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

/// The discriminated view of a [`CdpMessage`].
#[derive(Debug)]
pub(crate) enum CdpIncoming {
    /// A response to one of our commands.
    Response { id: u64, result: Value, error: Option<CdpErrorPayload> },
    /// An unsolicited event from the browser.
    Event(CdpEvent),
}

impl CdpMessage {
    /// Classify a raw message into a response or an event.
    pub(crate) fn classify(self) -> Option<CdpIncoming> {
        if let Some(id) = self.id {
            Some(CdpIncoming::Response {
                id,
                result: self.result.unwrap_or(Value::Null),
                error: self.error,
            })
        } else {
            self.method.map(|method| {
                CdpIncoming::Event(CdpEvent {
                    method,
                    params: self.params.unwrap_or(Value::Null),
                    session_id: self.session_id,
                })
            })
        }
    }
}
