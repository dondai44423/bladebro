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
            .ok_or_else(|| BladeError::NoWebSocketUrl { id: self.id.clone() })
    }
}

/// Fetch `/json/version` from a browser debug endpoint.
///
/// `base` is the `host:port` (no scheme), e.g. `127.0.0.1:9222`.
pub async fn version(base: &str) -> Result<VersionInfo> {
    let url = format!("http://{base}/json/version");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let info: VersionInfo = client.get(&url).send().await?.json().await?;
    Ok(info)
}

/// List all targets from `/json`.
pub async fn list_targets(base: &str) -> Result<Vec<TargetInfo>> {
    let url = format!("http://{base}/json");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
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

/// Pick the first page target with a WebSocket URL, or [`BladeError::NoTarget`].
pub async fn first_page_target(base: &str) -> Result<TargetInfo> {
    list_page_targets(base)
        .await?
        .into_iter()
        .find(|t| t.web_socket_debugger_url.is_some())
        .ok_or(BladeError::NoTarget)
}

/// Best-effort raw JSON dump of `/json` — used by the probe subcommand for
/// diagnostics. Returns the parsed value so callers can pretty-print.
pub async fn raw_targets(base: &str) -> Result<Value> {
    let url = format!("http://{base}/json");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    Ok(client.get(&url).send().await?.json().await?)
}
