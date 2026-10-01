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
        .send("Page.reload", Some(serde_json::json!({ "ignoreCache": false })))
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
            "reddit: network-security block (soft, transient — retry shortly) on {path}"
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
        return Err(BladeError::Other(format!("reddit: {}", crate::platform::truncate_utf8(msg, 200))));
    }
    let val = res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
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
        return Err(BladeError::Other("reddit: empty response (throttled?)".into()));
    }
    serde_json::from_str(text).map_err(|e| {
        classify_gate_body(path, text)
            .unwrap_or_else(|| BladeError::Other(format!("reddit: non-JSON response from {path} ({e})")))
    })
}

/// Fetch several reddit JSON paths concurrently in one CDP round-trip.
/// Each URL resolves to `Ok(value)` / `Err(reason)` independently.
pub(super) async fn fetch_json_many(cdp: &CdpSession, urls: &[String]) -> Result<Vec<Result<Value>>> {
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
        return Err(BladeError::Other(format!("reddit: {}", crate::platform::truncate_utf8(msg, 200))));
    }
    let value = res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
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
