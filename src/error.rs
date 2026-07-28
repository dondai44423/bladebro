//! Error types for Bladebro.

use thiserror::Error;

/// All fallible Bladebro operations return this.
///
/// Kept as a single enum (not a `Box<dyn Error>`) so callers can match on the
/// concrete cause — important for the self-diagnosing-failure design where the
/// driver reports *why* an action failed, not just that it did.
#[derive(Debug, Error)]
pub enum BladeError {
    /// A CDP command returned a protocol-level error (non-zero code).
    #[error("CDP error ({code}) {message}")]
    Cdp { code: i64, message: String },

    /// The WebSocket transport to the browser failed or was closed.
    #[error("CDP transport closed: {0}")]
    Transport(String),

    /// A CDP command did not receive a response within the timeout.
    #[error("CDP command timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// The browser connection is no longer usable (process gone / dropped).
    #[error("browser connection closed")]
    Closed,

    /// No suitable target (page tab) was found on the browser.
    #[error("no page target available")]
    NoTarget,

    /// A target was found but exposes no WebSocket debugger URL.
    #[error("target {id} has no WebSocket debugger URL")]
    NoWebSocketUrl { id: String },

    /// HTTP discovery request to the browser's debug endpoint failed.
    #[error("HTTP error talking to browser debug endpoint: {0}")]
    Http(#[from] reqwest::Error),

    /// A URL could not be parsed.
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    /// JSON (de)serialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A WebSocket handshake/protocol error from tokio-tungstenite.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// Catch-all for unexpected conditions with a human-readable context.
    #[error("{0}")]
    Other(String),

    /// An I/O error (stdin/stdout) in the MCP server.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The agent referenced a ref that doesn't exist in the Live Page Model.
    #[error("stale ref: {0}")]
    StaleRef(String),

    /// The element was found in the LPM but couldn't be re-located in the live DOM
    /// (the page re-rendered and the element is gone or moved beyond signature match).
    #[error("element not found in DOM: {0}")]
    ElementNotFound(String),

    /// The element exists but can't be interacted with (hidden, disabled, wrong type).
    #[error("element not interactable: {0}")]
    NotInteractable(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, BladeError>;

impl BladeError {
    /// Construct an [`BladeError::Other`] from any Display-able value.
    pub fn other<E: std::fmt::Display>(e: E) -> Self {
        BladeError::Other(e.to_string())
    }
}
