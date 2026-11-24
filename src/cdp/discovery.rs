//! HTTP discovery of CDP targets.
//!
//! Before opening a WebSocket, we talk to Chrome's plain-HTTP debug endpoint
//! (`http://host:port/json` and `/json/version`) to enumerate page targets and
//! find their WebSocket debugger URLs. This is the same endpoint every CDP
//! client uses; it is not the automation fingerprint surface.

use serde::Deserialize;
use serde_json::Value;

use crate::error::{BladeError, Result};

/// Browser-level metadata from `/json/version`.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "Browser")]
    pub browser: String,
    #[serde(rename = "Protocol-Version", default)]
    pub protocol_version: String,
    #[serde(rename = "User-Agent", default)]
    pub user_agent: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub web_socket_debugger_url: Option<String>,
}

/// A single target (tab) from `/json` (a.k.a. `/json/list`).
#[derive(Debug, Clone, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub attached: bool,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub web_socket_debugger_url: Option<String>,
}

impl TargetInfo {
    /// Is this a page-type target (a real tab, not a service worker / browser).
    pub fn is_page(&self) -> bool {
        self.kind == "page"
    }

    /// The WebSocket URL to drive this target directly, or an error if absent.
    pub fn ws_url(&self) -> Result<&str> {
        self.web_socket_debugger_url
            .as_deref()
            .ok_or_else(|| BladeError::NoWebSocketUrl {
                id: self.id.clone(),
            })
    }
}

/// Shared HTTP client for the DevTools endpoint. Reused across startup polls
/// and tab discovery calls so each request skips rebuilding a rustls/Hyper
/// client pool. Never proxied: the endpoint is reached directly, exactly like
/// the CDP WebSocket that follows discovery (which never uses a proxy).
/// reqwest's environment proxy matcher has no implicit loopback bypass, so an
/// ambient `HTTP_PROXY`/`ALL_PROXY` with an empty `NO_PROXY` routed
/// `127.0.0.1:<port>/json/version` through the proxy and every launch failed
/// with "debug endpoint not responding" while Chrome was up and listening.
fn devtools_client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<std::result::Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    let res = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())
    });
    res.as_ref()
        .map_err(|e| BladeError::Other(format!("failed to build devtools HTTP client: {e}")))
}

/// Fetch `/json/version` from a browser debug endpoint.
///
/// `base` is the `host:port` (no scheme), e.g. `127.0.0.1:9222`.
pub async fn version(base: &str) -> Result<VersionInfo> {
    let url = format!("http://{base}/json/version");
    let client = devtools_client()?;
    let info: VersionInfo = client.get(&url).send().await?.json().await?;
    Ok(info)
}

/// List all targets from `/json`.
pub async fn list_targets(base: &str) -> Result<Vec<TargetInfo>> {
    let url = format!("http://{base}/json");
    let client = devtools_client()?;
    let targets: Vec<TargetInfo> = client.get(&url).send().await?.json().await?;
    Ok(targets)
}

/// Convenience: list only page-type targets with a usable WebSocket URL.
pub async fn list_page_targets(base: &str) -> Result<Vec<TargetInfo>> {
    Ok(list_targets(base)
        .await?
        .into_iter()
        .filter(TargetInfo::is_page)
        .collect())
}

/// Pick the best page target with a WebSocket URL, or [`BladeError::NoTarget`].
///
/// In a user's real browser with extensions or DevTools open, `/json` can list
/// `devtools://` or `chrome-extension://` targets ahead of normal web tabs.
/// Prefer real web pages (`http://`/`https://`), then ordinary blank/newtab
/// pages, and only fall back to extension/internal targets if nothing else
/// exists.
pub async fn first_page_target(base: &str) -> Result<TargetInfo> {
    let mut pages: Vec<TargetInfo> = list_page_targets(base)
        .await?
        .into_iter()
        .filter(|t| t.web_socket_debugger_url.is_some())
        .collect();
    if pages.is_empty() {
        return Err(BladeError::NoTarget);
    }
    let priority = |url: &str| -> u8 {
        if url.starts_with("https://") || url.starts_with("http://") {
            0
        } else if url.is_empty()
            || url == "about:blank"
            || url.starts_with("chrome://newtab")
            || url.starts_with("edge://newtab")
            || url.starts_with("brave://newtab")
        {
            1
        } else if url.starts_with("devtools://") || url.starts_with("chrome-extension://") {
            3
        } else {
            2
        }
    };
    if let Some((best_idx, _)) = pages
        .iter()
        .enumerate()
        .min_by_key(|(_, t)| priority(&t.url))
    {
        return Ok(pages.swap_remove(best_idx));
    }
    Err(BladeError::NoTarget)
}

/// Best-effort raw JSON dump of `/json` — used by the probe subcommand for
/// diagnostics. Returns the parsed value so callers can pretty-print.
pub async fn raw_targets(base: &str) -> Result<Value> {
    let url = format!("http://{base}/json");
    let client = devtools_client()?;
    Ok(client.get(&url).send().await?.json().await?)
}
