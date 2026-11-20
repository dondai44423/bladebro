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

/// Latest main-frame Document HTTP response observed by the network tracker.
/// Used to report exact HTTP error status codes (e.g. 429, 403, 404, 503) and
/// `Retry-After` headers when Chrome renders an empty-body error page
/// (`net::ERR_HTTP_RESPONSE_CODE_FAILURE`) or a site returns an HTTP 4xx/5xx body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocResponse {
    pub request_id: String,
    pub url: String,
    pub status: i64,
    pub status_text: String,
    pub retry_after: Option<String>,
}

/// Standard HTTP reason phrase fallback. HTTP/2 and HTTP/3 omit reason phrases
/// on the wire, so CDP's `response.statusText` is often empty on modern sites.
pub fn http_status_reason(status: i64) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        418 => "I'm a Teapot",
        422 => "Unprocessable Content",
        423 => "Locked",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        520..=526 => "Origin/Edge Error",
        _ => "",
    }
}

/// Extract a bounded `Retry-After` header value from a CDP headers map
/// (case-insensitive). Returns `None` if absent or empty.
pub(super) fn extract_retry_after(headers: Option<&Value>) -> Option<String> {
    let obj = headers?.as_object()?;
    for (k, v) in obj {
        if k.eq_ignore_ascii_case("retry-after") {
            let s = v.as_str()?.trim();
            if !s.is_empty() {
                return Some(crate::platform::truncate_utf8(s, 64).to_string());
            }
        }
    }
    None
}

impl DocResponse {
    /// Human-readable status label, e.g. `429 Too Many Requests` (falling back
    /// to standard reason phrases when HTTP/2+3 omit `statusText`).
    pub fn label(&self) -> String {
        let reason = self.status_text.trim();
        let reason = if reason.is_empty() {
            http_status_reason(self.status)
        } else {
            reason
        };
        if reason.is_empty() {
            self.status.to_string()
        } else {
            format!("{} {reason}", self.status)
        }
    }

    /// Optional `(retry-after: ...)` suffix, appending `s` to bare integer seconds.
    pub fn retry_suffix(&self) -> String {
        match self.retry_after.as_deref() {
            Some(ra) if ra.bytes().all(|b| b.is_ascii_digit()) => {
                format!(" (retry-after: {ra}s)")
            }
            Some(ra) => format!(" (retry-after: {ra})"),
            None => String::new(),
        }
    }

    /// Actionable one-line guidance for common HTTP error classes.
    pub fn guidance(&self) -> Option<&'static str> {
        match self.status {
            401 => Some("authentication required"),
            403 => Some("access forbidden by server"),
            404 => Some("page not found"),
            407 => Some("proxy authentication required"),
            410 => Some("resource permanently gone"),
            429 => Some("rate limited; wait/back off before retrying"),
            451 => Some("unavailable for legal reasons"),
            500..=599 => Some("server error; transient, retry later"),
            _ => None,
        }
    }

    /// Concise ambient notice for a landed HTTP 4xx/5xx document.
    pub fn ambient_note(&self, chrome_error_page: bool) -> String {
        let mut out = format!("http: {}{}", self.label(), self.retry_suffix());
        if let Some(g) = self.guidance() {
            out.push_str(&format!(" — {g}"));
        }
        if chrome_error_page {
            out.push_str(" (Chrome error page; empty response body)");
        }
        out
    }
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
    /// Every request header except the transport ones the browser
    /// regenerates itself (`cookie`, `host`, `user-agent`,
    /// `content-length`, `accept-encoding`, `connection`, `sec-*`) —
    /// replayed verbatim by adapters.
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

    /// Arm the one-use dialog expectation (G03); returns the previous one
    /// when it was still armed (replaced unused).
    pub fn set_dialog_expect(&self, exp: DialogExpect) -> Option<DialogExpect> {
        self.dialog_expect
            .lock()
            .ok()
            .and_then(|mut g| g.replace(exp))
    }

    /// Disarm + return the armed expectation, if any.
    pub fn take_dialog_expect(&self) -> Option<DialogExpect> {
        self.dialog_expect.lock().ok().and_then(|mut g| g.take())
    }

    /// G09: one-line load-state observation for error/recovery surfaces.
    /// Returns None when the page is settled (a quiet complete page adds
    /// nothing); otherwise reports readyState + in-flight requests so an
    /// agent can tell "still loading" apart from "the action did nothing".
    /// Safe observation — never mutates the page.
    pub async fn load_state_note(&self) -> Option<String> {
        let res = self
            .cdp
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": "document.readyState",
                    "returnByValue": true,
                })),
            )
            .await
            .ok()?;
        let rs = res
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let inflight = self.in_flight();
        if rs == "complete" && inflight == 0 {
            return None;
        }
        Some(format!(
            "page still loading (readyState={rs}, {inflight} request(s) in flight) - recovery reads may race the load; re-read after it settles"
        ))
    }

    /// One-shot note: an armed expectation expired without ever matching.
    /// Drained by the MCP/CLI response path like dialogs, so an unused arm
    /// is always reported instead of silently forgotten.
    pub fn drain_dialog_expect_note(&self) -> Option<String> {
        let mut g = self.dialog_expect.lock().ok()?;
        let expired = g
            .as_ref()
            .is_some_and(|e| std::time::Instant::now() >= e.deadline);
        if expired {
            let e = g.take()?;
            Some(format!(
                "dialog expectation for '{}' expired unused - no matching dialog opened; default dialog handling is unchanged",
                e.kind
            ))
        } else {
            None
        }
    }

    /// Drain ambient events (consent, block detection) for the agent.
    pub fn drain_ambient(&self) -> Vec<String> {
        self.ambient
            .lock()
            .map(|mut a| a.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn clear_xhr_log(&self) {
        if let Ok(mut log) = self.xhr_log.lock() {
            log.clear();
        }
    }

    /// V8: snapshot of the network request log (last 50).
    pub fn network_log(&self) -> Vec<NetEntry> {
        self.net_log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear_network_log(&self) {
        if let Ok(mut log) = self.net_log.lock() {
            log.clear();
        }
    }

    /// Latest main-frame Document HTTP response observed by the network tracker.
    pub fn doc_response(&self) -> Option<DocResponse> {
        self.doc_response.lock().ok().and_then(|g| g.clone())
    }

    /// Clear the tracked main-frame Document HTTP response before a fresh navigation.
    pub fn clear_doc_response(&self) {
        if let Ok(mut g) = self.doc_response.lock() {
            *g = None;
        }
    }

    pub async fn clear_console_log(&self) -> Result<()> {
        let key = crate::stealth::console_secret();
        self.cdp
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": format!("(()=>{{try{{console.debug.call('{key}',1);}}catch(e){{}}}})()"),
                })),
            )
            .await?;
        Ok(())
    }

    /// Recent XHR/fetch requests (ring of 128, one entry per endpoint,
    /// start-observed, full URLs).
    pub fn xhr_log(&self) -> Vec<XhrEntry> {
        self.xhr_log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// V8: read the console log captured by the injection hook.
    /// Returns raw JSON (array of {l, m, t}). The ring buffer stays inside
    /// the injection closure and is read via a per-process secret receiver
    /// token so zero properties or symbols are placed on `window`.
    pub async fn console_log(&self) -> Result<serde_json::Value> {
        let key = crate::stealth::console_secret();
        let res = self
            .cdp
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": format!("(()=>{{try{{const q=console.debug.call('{key}',0);return Array.isArray(q)?q:[];}}catch(e){{return [];}}}})()"),
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

    #[test]
    fn doc_response_formats_status_retry_after_and_guidance() {
        use super::{extract_retry_after, http_status_reason, DocResponse};
        use serde_json::json;

        assert_eq!(http_status_reason(429), "Too Many Requests");
        assert_eq!(http_status_reason(404), "Not Found");
        assert_eq!(http_status_reason(503), "Service Unavailable");
        assert_eq!(
            extract_retry_after(Some(&json!({"Retry-After": " 30 "}))),
            Some("30".into())
        );
        assert_eq!(
            extract_retry_after(Some(
                &json!({"retry-after": "Wed, 21 Oct 2026 07:28:00 GMT"})
            )),
            Some("Wed, 21 Oct 2026 07:28:00 GMT".into())
        );
        assert_eq!(
            extract_retry_after(Some(&json!({"Content-Length": "0"}))),
            None
        );

        let d = DocResponse {
            request_id: "r1".into(),
            url: "https://reddit.com/r/rust".into(),
            status: 429,
            status_text: String::new(),
            retry_after: Some("60".into()),
        };
        assert_eq!(d.label(), "429 Too Many Requests");
        assert_eq!(d.retry_suffix(), " (retry-after: 60s)");
        let note = d.ambient_note(false);
        assert!(
            note.contains("http: 429 Too Many Requests (retry-after: 60s)"),
            "{note}"
        );
        assert!(note.contains("rate limited"), "{note}");
        let err_note = d.ambient_note(true);
        assert!(err_note.contains("Chrome error page"), "{err_note}");
    }
}
