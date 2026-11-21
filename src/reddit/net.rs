//! Reddit's in-page HTML/DOM transport layer: observational live-DOM reads,
//! sequential same-origin HTML partial fetches parsed in-browser via
//! `DOMParser`, and honest gate/rate-limit classification.
//!
//! Zero `.json` endpoints: Shreddit renders and expands comment trees and
//! search feeds via HTML partials (`/svc/shreddit/more-comments/...`,
//! `/svc/shreddit/search/...`) and comment permalinks (`div.fold-more`).
//! Fetching those same-origin HTML paths one at a time matches the web app's
//! own traffic and avoids the `.json` API network-security blocks.

use serde_json::{json, Value};

use super::*;

/// Evaluate a pure, observational DOM-parsing JS expression in the isolated
/// world (stale-context resilient across mid-flight redirects).
pub(super) async fn eval_dom(cdp: &CdpSession, expr: &str) -> Result<Value> {
    let res = cdp
        .eval_isolated_with_timeout(expr, true, false, Duration::from_secs(15))
        .await?;
    extract_eval_value(&res, "dom read failed")
}

/// Fetch one same-origin Reddit HTML path (partial or permalink) sequentially
/// from page context and parse it inside the browser with `DOMParser`,
/// returning only the compact structured payload.
///
/// Retries at most once on transient network/5xx/stale-context errors; never
/// retries HTTP 403, HTTP 429, or network-security/challenge gates.
pub(super) async fn fetch_html_parsed(
    cdp: &CdpSession,
    path: &str,
    parser_lib: &str,
    parse_call: &str,
) -> Result<Value> {
    for attempt in 0..2 {
        match fetch_html_parsed_once(cdp, path, parser_lib, parse_call).await {
            Err(e) if attempt == 0 && transient_read(&e) => {
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
            result => return result,
        }
    }
    Err(BladeError::Other("reddit: read retries exhausted".into()))
}

fn transient_read(error: &BladeError) -> bool {
    if CdpSession::is_stale_context_err(error) {
        return true;
    }
    matches!(error, BladeError::Other(message) if ["HTTP 0", "HTTP 502", "HTTP 503", "HTTP 504"].iter().any(|suffix| message.ends_with(suffix)))
}

async fn fetch_html_parsed_once(
    cdp: &CdpSession,
    path: &str,
    parser_lib: &str,
    parse_call: &str,
) -> Result<Value> {
    let path_js = serde_json::to_string(path)?;
    // Parse inside the browser so raw HTML (50–300 KB per partial) never
    // crosses the CDP bridge. Gate snippets (first 2 KB) are returned only on
    // non-200 or empty-parse responses for accurate block classification.
    let expr = format!(
        "(async()=>{{try{{\
{parser_lib}\
const c=new AbortController();\
const tm=setTimeout(()=>c.abort(),{PAGE_FETCH_TIMEOUT_MS});\
const r=await fetch({path_js},{{credentials:'include',headers:{{'Accept':'text/html,application/xhtml+xml,*/*;q=0.8'}},signal:c.signal}});\
clearTimeout(tm);\
const ra=(r.headers&&typeof r.headers.get==='function')?(r.headers.get('retry-after')||''):'';\
const html=await r.text();\
if(r.status!==200)return {{s:r.status,ra,head:html.slice(0,2500),len:html.length}};\
const doc=new DOMParser().parseFromString(html,'text/html');\
const data={parse_call};\
return {{s:200,ra,data,head:html.length<25000?html.slice(0,2500):'',len:html.length}};\
}}catch(e){{return {{s:0,ra:'',head:String(e).slice(0,500),len:0}};}}}})()"
    );
    let res = cdp
        .eval_isolated_with_timeout(
            &expr,
            true,
            true,
            Duration::from_millis(PAGE_FETCH_TIMEOUT_MS + 5_000),
        )
        .await?;
    let val = extract_eval_value(&res, "html fetch failed")?;
    let status = val["s"].as_i64().unwrap_or(0);
    let retry_after = val["ra"].as_str().unwrap_or("").trim();
    let head = val["head"].as_str().unwrap_or("");
    let len = val["len"].as_u64().unwrap_or(0);

    if status == 429 {
        return Err(rate_limit_error(retry_after));
    }
    if status != 200 {
        return Err(classify_gate_body(path, head)
            .unwrap_or_else(|| BladeError::Other(format!("reddit: GET {path} → HTTP {status}"))));
    }
    if len == 0 {
        return Err(BladeError::Other(
            "reddit: empty response (throttled?)".into(),
        ));
    }
    let data = val.get("data").cloned().unwrap_or(Value::Null);
    let has_items = data
        .get("comments")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
        || data
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        || data
            .get("partials")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        || data
            .get("folds")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
    // Only inspect a 200 body for WAF/challenge walls when parsing produced no
    // comments/items/controls — real comment text can mention "blocked by
    // network security" without the response being a block page.
    if !has_items && !head.is_empty() {
        if let Some(gate) = classify_gate_body(path, head) {
            return Err(gate);
        }
    }
    Ok(data)
}

fn extract_eval_value(res: &Value, fallback: &str) -> Result<Value> {
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or(fallback);
        return Err(BladeError::Other(format!(
            "reddit: {}",
            crate::platform::truncate_utf8(msg.lines().next().unwrap_or(msg), 200)
        )));
    }
    Ok(res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Format an HTTP 429 error preserving any `Retry-After` header value.
pub(super) fn rate_limit_error(retry_after: &str) -> BladeError {
    let ra = retry_after.trim();
    if ra.is_empty() {
        BladeError::Other("reddit: rate limited (HTTP 429)".into())
    } else {
        let suffix = if ra.chars().all(|c| c.is_ascii_digit()) {
            format!("{ra}s")
        } else {
            crate::platform::truncate_utf8(ra, 64).to_string()
        };
        BladeError::Other(format!(
            "reddit: rate limited (HTTP 429, retry-after: {suffix})"
        ))
    }
}

/// Marker classification for HTML bodies served as Reddit WAF/challenge gates:
/// both mean "stop the sweep immediately — this is a gate, not a gap".
pub(super) fn classify_gate_body(path: &str, text: &str) -> Option<BladeError> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("blocked by network security") || lower.contains("whoa there, pardner") {
        return Some(BladeError::Other(format!(
            "reddit: network-security block (repeated requests may remain blocked) on {path}"
        )));
    }
    if lower.contains("js_challenge")
        || lower.contains("requestsubmit")
        || lower.contains("prove your humanity")
    {
        return Some(BladeError::Other(format!(
            "reddit: challenge gate served on {path}"
        )));
    }
    if lower.contains("too many requests") {
        return Some(BladeError::Other("reddit: rate limited (HTTP 429)".into()));
    }
    None
}

/// Rate-limit signal: Reddit throttles with HTTP 429 (with optional
/// `retry-after`) or empty responses. Never match bare "429" anywhere in a
/// request path (base36 ids / subreddit names can contain "429").
pub(super) fn is_rate_limit(e: &BladeError) -> bool {
    let s = e.to_string();
    s.contains("reddit: rate limited (HTTP 429")
        || s.ends_with("rate limited (HTTP 429)")
        || s.ends_with("empty response (throttled?)")
}

/// Extract the `retry-after` suffix (if present) from a rate-limit error.
pub(super) fn retry_after_from_err(e: &BladeError) -> Option<String> {
    let s = e.to_string();
    let idx = s.find("retry-after: ")?;
    let rest = &s[idx + "retry-after: ".len()..];
    let end = rest.find(')')?;
    let val = rest[..end].trim();
    (!val.is_empty()).then(|| val.to_string())
}

/// Security-wall or challenge-gate signal: stop the sweep immediately so we
/// never hammer a blocked session.
pub fn is_security_block(e: &BladeError) -> bool {
    let s = e.to_string();
    s.contains("network-security block") || s.contains("challenge gate served")
}

/// Short polite jitter between sequential partial fetches (120–240ms) so
/// expansion stays strictly serial and avoids burst rate-scoring.
pub(super) async fn polite_pause() {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u64 % 120)
        .unwrap_or(60);
    tokio::time::sleep(Duration::from_millis(120 + jitter)).await;
}

/// Build an isolated DOM-read expression wrapping a parser library and call.
pub(super) fn dom_parse_expr(parser_lib: &str, call_expr: &str) -> String {
    format!("(()=>{{{parser_lib}return {call_expr};}})()")
}

/// Helper to build a JSON-serialized options object for JS parsers.
pub(super) fn opts_json(is_fragment: bool, fallback_parent: &str) -> String {
    json!({
        "isFragment": is_fragment,
        "fallbackParent": fallback_parent,
    })
    .to_string()
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    async fn fixture(
        responses: Vec<(i64, &'static str, &'static str)>,
    ) -> (CdpSession, tokio::task::JoinHandle<usize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut count = 0;
            for (status, ra, head) in responses {
                let request = socket.next().await.unwrap().unwrap();
                let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                let val = if status == 200 {
                    json!({"s": 200, "ra": ra, "data": {"comments": [{"id": "c1"}]}, "head": head, "len": 120})
                } else {
                    json!({"s": status, "ra": ra, "head": head, "len": head.len()})
                };
                socket
                    .send(Message::Text(
                        json!({"id": request["id"], "result": {"result": {"value": val}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                count += 1;
            }
            count
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        (CdpSession::root(client), task)
    }

    #[tokio::test]
    async fn transient_503_recovers_once_but_403_and_429_never_retry() {
        let (cdp, task) = fixture(vec![(503, "", "unavailable"), (200, "", "")]).await;
        let v = fetch_html_parsed(&cdp, "/svc/shreddit/more-comments/r/rust/t3_abc", "", "{}")
            .await
            .unwrap();
        assert_eq!(v["comments"][0]["id"], "c1");
        assert_eq!(task.await.unwrap(), 2);

        let (cdp, task) = fixture(vec![(403, "", "You've been blocked by network security")]).await;
        let err = fetch_html_parsed(&cdp, "/svc/shreddit/more-comments/r/rust/t3_abc", "", "{}")
            .await
            .unwrap_err();
        assert!(is_security_block(&err), "{err}");
        assert_eq!(task.await.unwrap(), 1);

        let (cdp, task) = fixture(vec![(429, "45", "Too Many Requests")]).await;
        let err = fetch_html_parsed(&cdp, "/svc/shreddit/more-comments/r/rust/t3_abc", "", "{}")
            .await
            .unwrap_err();
        assert!(is_rate_limit(&err), "{err}");
        assert_eq!(retry_after_from_err(&err).as_deref(), Some("45s"));
        assert_eq!(task.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn permanent_503_stops_after_single_retry() {
        let (cdp, task) = fixture(vec![(503, "", "fail"), (503, "", "fail")]).await;
        let err = fetch_html_parsed(&cdp, "/svc/shreddit/more-comments/r/rust/t3_abc", "", "{}")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 503"), "{err}");
        assert_eq!(task.await.unwrap(), 2);
        assert!(!transient_read(&BladeError::Timeout(Duration::from_secs(
            5
        ))));
    }

    #[test]
    fn rate_limit_never_false_matches_path_substrings() {
        let ok_500 = BladeError::Other(
            "reddit: GET /r/throttled/comments/1ab429c/comment/k2xq/ → HTTP 500".into(),
        );
        assert!(!is_rate_limit(&ok_500));
        assert!(is_rate_limit(&rate_limit_error("")));
        assert!(is_rate_limit(&rate_limit_error("60")));
        assert_eq!(
            retry_after_from_err(&rate_limit_error("60")).as_deref(),
            Some("60s")
        );
    }
}
