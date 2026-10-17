//! Reddit's in-page HTTP layer: the loid gate, JSON fetches, and gate
//! classification (network-security wall vs JS challenge vs generic failure).

use super::*;

/// Reddit gates its JSON paths (`.json`, `/api/info`, subtree listings) on
/// the `loid` client token: without it they answer with the network-security
/// wall (HTTP 403) rather than the JS challenge that HTML navigations get.
/// The token is set by the page-load challenge and persists in the profile,
/// so it is missing only on a cold profile or mid-challenge. Wait briefly
/// (the challenge auto-submits in ~1s), then re-serve the page once; give up
/// honestly rather than burn the sweep on guaranteed 403s.
pub(super) async fn ensure_loid(cdp: &CdpSession) -> bool {
    async fn has_loid(cdp: &CdpSession) -> bool {
        cdp.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "document.cookie.includes('loid=')",
                "returnByValue": true,
            })),
        )
        .await
        .ok()
        .and_then(|r| {
            r.get("result")
                .and_then(|x| x.get("value"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false)
    }

    if has_loid(cdp).await {
        return true;
    }
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if has_loid(cdp).await {
            return true;
        }
    }
    // Still tokenless — re-serve the page; the challenge resolves in ~1s
    // and its solved response sets `loid`.
    let _ = cdp
        .send(
            "Page.reload",
            Some(serde_json::json!({ "ignoreCache": false })),
        )
        .await;
    let _ = crate::page::wait_for_load(cdp, Duration::from_secs(10)).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if has_loid(cdp).await {
            return true;
        }
    }
    false
}

/// Marker classification for HTML bodies served to reddit API paths: the
/// network-security wall and the unsolved JS challenge both mean "stop the
/// sweep — this is a gate, not a gap". Anything else keeps the generic
/// HTTP / parse error.
pub(super) fn classify_gate_body(path: &str, text: &str) -> Option<BladeError> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("blocked by network security") || lower.contains("whoa there, pardner") {
        return Some(BladeError::Other(format!(
            "reddit: network-security block (API denied; repeated requests may remain blocked) on {path}"
        )));
    }
    if lower.contains("js_challenge") || lower.contains("requestsubmit") {
        return Some(BladeError::Other(format!(
            "reddit: JS challenge served to an API path on {path}"
        )));
    }
    None
}

/// Fetch one same-origin reddit JSON path from inside the page.
pub(super) async fn fetch_json(cdp: &CdpSession, path: &str) -> Result<Value> {
    for attempt in 0..3 {
        match fetch_json_once(cdp, path).await {
            Err(e) if attempt < 2 && transient_read(&e) => {
                tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    Err(BladeError::Other("reddit: read retries exhausted".into()))
}

// Only safe GET observations, never challenge denial or rate limiting. An
// unanswered CDP eval is not replayed: its pending fetch may still be alive.
fn transient_read(error: &BladeError) -> bool {
    matches!(error, BladeError::Other(message) if ["HTTP 0", "HTTP 502", "HTTP 503", "HTTP 504"].iter().any(|suffix| message.ends_with(suffix)))
}

async fn fetch_json_once(cdp: &CdpSession, path: &str) -> Result<Value> {
    let url_js = serde_json::to_string(path)?;
    let expr = format!(
        "(async()=>{{try{{const c=new AbortController();const t=setTimeout(()=>c.abort(),{PAGE_FETCH_TIMEOUT_MS});\
const r=await fetch({url_js},{{credentials:'include',signal:c.signal}});clearTimeout(t);const x=await r.text();\
return {{s:r.status,t:x}};}}catch(e){{return {{s:0,t:String(e)}}}}}})()"
    );
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            })),
        )
        .await?;
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("fetch failed");
        return Err(BladeError::Other(format!(
            "reddit: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }
    let val = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    let status = val["s"].as_i64().unwrap_or(0);
    let text = val["t"].as_str().unwrap_or_default();
    if status == 429 {
        return Err(BladeError::Other("reddit: rate limited (HTTP 429)".into()));
    }
    if status != 200 {
        return Err(classify_gate_body(path, text)
            .unwrap_or_else(|| BladeError::Other(format!("reddit: GET {path} → HTTP {status}"))));
    }
    if text.trim().is_empty() {
        return Err(BladeError::Other(
            "reddit: empty response (throttled?)".into(),
        ));
    }
    serde_json::from_str(text).map_err(|e| {
        classify_gate_body(path, text).unwrap_or_else(|| {
            BladeError::Other(format!("reddit: non-JSON response from {path} ({e})"))
        })
    })
}

/// Fetch several reddit JSON paths concurrently in one CDP round-trip.
/// Each URL resolves to `Ok(value)` / `Err(reason)` independently.
pub(super) async fn fetch_json_many(
    cdp: &CdpSession,
    urls: &[String],
) -> Result<Vec<Result<Value>>> {
    let urls_js = serde_json::to_string(urls)?;
    let expr = format!(
        "(async()=>{{const us={urls_js};return await Promise.all(us.map(u=>{{const c=new AbortController();\
const tm=setTimeout(()=>c.abort(),{PAGE_FETCH_TIMEOUT_MS});\
return fetch(u,{{credentials:'include',signal:c.signal}}).then(r=>r.text().then(t=>({{s:r.status,t}})))\
.catch(e=>({{s:0,t:String(e)}})).finally(()=>clearTimeout(tm));}}));}})()"
    );
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            })),
        )
        .await?;
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("batch fetch failed");
        return Err(BladeError::Other(format!(
            "reddit: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for (item, url) in arr.iter().zip(urls) {
        let status = item["s"].as_i64().unwrap_or(0);
        let text = item["t"].as_str().unwrap_or_default();
        if status == 429 {
            out.push(Err(BladeError::Other("rate limited (HTTP 429)".into())));
        } else if status != 200 {
            out.push(Err(classify_gate_body(url, text)
                .unwrap_or_else(|| BladeError::Other(format!("HTTP {status}")))));
        } else if text.trim().is_empty() {
            out.push(Err(BladeError::Other("empty response (throttled?)".into())));
        } else {
            out.push(serde_json::from_str(text).map_err(|e| {
                classify_gate_body(url, text)
                    .unwrap_or_else(|| BladeError::Other(format!("non-JSON response ({e})")))
            }));
        }
    }
    Ok(out)
}

/// Rate-limit signal: Reddit throttles with 429s (and empty bodies).
pub(super) fn is_rate_limit(e: &BladeError) -> bool {
    let s = e.to_string();
    s.contains("429") || s.contains("throttled")
}

/// Security-wall signal: the sweep hit reddit's network-security block page
/// and must stop — it is transient, and hammering extends it.
pub fn is_security_block(e: &BladeError) -> bool {
    e.to_string().contains("network-security block")
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    async fn fixture(statuses: Vec<i64>) -> (CdpSession, tokio::task::JoinHandle<usize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut count = 0;
            for status in statuses {
                let request = socket.next().await.unwrap().unwrap();
                let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                let body = if status == 200 {
                    "{\"ok\":true}"
                } else if status == 403 {
                    "blocked by network security"
                } else {
                    "temporary server failure"
                };
                socket.send(Message::Text(json!({"id":request["id"],"result":{"result":{"value":{"s":status,"t":body}}}}).to_string().into())).await.unwrap();
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
    async fn transient_get_recovers_but_denials_never_retry() {
        let (cdp, task) = fixture(vec![503, 502, 200]).await;
        assert_eq!(fetch_json(&cdp, "/fixture.json").await.unwrap()["ok"], true);
        assert_eq!(task.await.unwrap(), 3);
        for status in [403, 429] {
            let (cdp, task) = fixture(vec![status]).await;
            assert!(fetch_json(&cdp, "/fixture.json").await.is_err());
            assert_eq!(task.await.unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn permanent_transient_read_stops_after_three_attempts() {
        let (cdp, task) = fixture(vec![503, 503, 503]).await;
        let error = fetch_json(&cdp, "/fixture.json").await.unwrap_err();
        assert!(error.to_string().contains("HTTP 503"));
        assert_eq!(task.await.unwrap(), 3);
        assert!(!transient_read(&BladeError::Timeout(Duration::from_secs(
            5
        ))));
    }
}
