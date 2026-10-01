//! Page logs: dialogs (auto-dismissed), ambient notes, console, network and
//! the XHR ring the site adapters read their own traffic from.

use super::*;

/// A completed/failed network request record (V8 introspection).
#[derive(Debug, Clone)]
pub struct NetEntry {
    pub method: String,
    pub url: String,
    /// HTTP status (0 = failed/no response).
    pub status: i64,
    /// Failure reason if the request failed.
    pub error: Option<String>,
}

/// An XHR/fetch request observed at START by the tracker (introspection).
/// Unlike `NetEntry` (pushed on completion), entries appear while still in
/// flight, and the URL is kept in FULL: API URLs carry their query state
/// (graphql operations, cursors, tokens) and truncation would defeat the
/// purpose. Small ring (128) — a working window, not a ledger.
#[derive(Debug, Clone)]
pub struct XhrEntry {
    pub id: String,
    pub method: String,
    pub url: String,
    /// HTTP status; 0 while in flight or on failure.
    pub status: i64,
    /// True once loading finished or failed (status/error meaningful).
    pub done: bool,
    /// Failure reason if the request failed.
    pub error: Option<String>,
    /// Auth/content header subset (`authorization`, `x-csrf-token`,
    /// `x-twitter-*`, `content-type`) — replayed verbatim by adapters.
    pub headers: Vec<(String, String)>,
}

/// Dedup key for the XHR ring: origin+path, query stripped. Repeats of the
/// same endpoint (telemetry beacons, polling) collapse into one entry so
/// the endpoints that matter are never evicted by noise.
pub(super) fn xhr_key(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or("")
}

/// Media/opaque noise for the introspection ring: MSE/blob playback
/// segments (each retry mints a fresh UUID — they flooded the ring and
/// evicted API entries), data: URLs, and HLS/DASH chunk extensions.
pub(super) fn is_media_url(url: &str) -> bool {
    if url.starts_with("blob:") || url.starts_with("data:") {
        return true;
    }
    let pl = url.split('?').next().unwrap_or("").to_ascii_lowercase();
    [
        ".m4s", ".m4a", ".mp4", ".m4v", ".mpd", ".webm", ".vtt", ".ts",
    ]
    .iter()
    .any(|e| pl.ends_with(e))
}

impl Page {
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

    /// V8: snapshot of the network request log (last 50).
    pub fn network_log(&self) -> Vec<NetEntry> {
        self.net_log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Recent XHR/fetch requests (last 24, start-observed, full URLs).
    pub fn xhr_log(&self) -> Vec<XhrEntry> {
        self.xhr_log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// V8: read the console log captured by the injection hook.
    /// Returns raw JSON (array of {l, m, t}). The ring buffer lives under
    /// the same Symbol.for('q') slot the injection script defines — a
    /// string-keyed window property was a page-readable detection marker.
    pub async fn console_log(&self) -> Result<serde_json::Value> {
        let res = self
            .cdp
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": "window[Symbol.for('q')]||[]",
                    "returnByValue": true,
                })),
            )
            .await?;
        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::json!([])))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_media_url, xhr_key};

    #[test]
    fn xhr_key_strips_query_and_fragment() {
        assert_eq!(
            xhr_key("https://x.com/i/api/graphql/abc?q=1#frag"),
            "https://x.com/i/api/graphql/abc"
        );
        assert_eq!(xhr_key("https://x.com/i/api"), "https://x.com/i/api");
        assert_eq!(xhr_key(""), "");
    }

    #[test]
    fn media_urls_are_filtered_from_the_ring() {
        // Opaque playback sources and HLS/DASH chunks are ring noise.
        assert!(is_media_url("blob:https://x.com/00-11-22-33"));
        assert!(is_media_url("data:video/mp4;base64,AAAA"));
        assert!(is_media_url("https://video.twimg.com/seg.m4s?tag=1"));
        assert!(is_media_url("https://cdn.example.com/CHUNK.MP4"));
        // API traffic stays visible.
        assert!(!is_media_url("https://x.com/i/api/graphql/abc"));
        assert!(!is_media_url("https://cdn.example.com/data.json"));
    }
}
