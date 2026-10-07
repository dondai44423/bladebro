//! GL healthcheck probes — the warm-up evaluate that reads the live renderer.

use super::*;
use serde_json::json;

/// The healthcheck expression — evaluated in a normal document (the probe
/// navigates the target to about:blank first: the startup tab may be a
/// WebUI where canvas access can be restricted). In-page warm-up poll:
/// under software GL (Xvfb/llvmpipe) the GPU process needs around a second
/// to serve its first context, and a host-side sleep loop quantizes that
/// wait to its interval; this polls *inside* the page every 40ms and
/// returns the moment a context exists — same verdict, one round trip,
/// ~150-200ms sooner.
const GL_PROBE_WARM_EXPR: &str = "(async()=>{for(let i=0;i<70;i++){try{var c=document.createElement('canvas');var g=c.getContext('webgl');if(g){var e=g.getExtension('WEBGL_debug_renderer_info');return e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):String(g.getParameter(g.RENDERER));}}catch(err){}await new Promise(r=>setTimeout(r,40));}return '';})()";

/// Healthcheck the WS transport: evaluate the GL probe in the first page
/// target with bounded retries. Returns the renderer string, or None when
/// no context ever appears.
pub(super) async fn probe_gl_ws(base: &str) -> Option<String> {
    for _ in 0..3 {
        if let Some(renderer) = probe_gl_ws_once(base).await {
            return Some(renderer);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    None
}

async fn probe_gl_ws_once(base: &str) -> Option<String> {
    let target = crate::cdp::first_page_target(base).await.ok()?;
    let client = crate::cdp::CdpClient::connect(target.ws_url().ok()?)
        .await
        .ok()?;
    let session = crate::cdp::CdpSession::root(client);
    // The probe needs a normal document (the startup tab may be a WebUI where
    // canvas access is restricted) — but when it is already blank, skip the
    // navigate and the renderer swap it would force.
    if target.url != "about:blank" {
        let _ = session
            .send("Page.navigate", Some(json!({ "url": "about:blank" })))
            .await;
    }
    for _ in 0..3 {
        if let Ok(v) = session
            .send_with_timeout(
                "Runtime.evaluate",
                Some(json!({ "expression": GL_PROBE_WARM_EXPR, "returnByValue": true, "awaitPromise": true })),
                Duration::from_secs(5),
            )
            .await
        {
            if let Some(s) = v.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    None
}

/// One-shot GL probe: a single evaluate, no in-page poll. Empty string
/// means context creation returned null; `__bb_skip__` marks a restricted
/// origin (WebUI) where canvas access proves nothing.
const GL_PROBE_ONCE_EXPR: &str = "(()=>{try{var p=location.protocol;if(p!=='http:'&&p!=='https:'&&p!=='file:'&&p!=='about:'&&p!=='data:'&&p!=='blob:')return '__bb_skip__';}catch(e){}try{var c=document.createElement('canvas');var g=c.getContext('webgl');if(g){var e=g.getExtension('WEBGL_debug_renderer_info');return e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):String(g.getParameter(g.RENDERER));}}catch(err){}return '';})()";

/// Live GL state from a fast probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlLive {
    /// A WebGL context exists; its renderer string.
    Live(String),
    /// The page answered and context creation returned null (every attempt).
    NoContext,
    /// Inconclusive: transport failure, malformed reply, or a restricted origin.
    Unknown,
}

/// Parse a `Runtime.evaluate` reply to [`GL_PROBE_ONCE_EXPR`]:
/// `Some(Some(r))` = live renderer, `Some(None)` = replied without a context,
/// `None` = malformed / skip (inconclusive).
fn extract_renderer(v: &serde_json::Value) -> Option<Option<String>> {
    let s = v.get("result")?.get("value")?.as_str()?;
    if s == "__bb_skip__" {
        return None;
    }
    if s.is_empty() {
        Some(None)
    } else {
        Some(Some(s.to_string()))
    }
}

/// Confirm the live GL state over a session: up to 3 quick evaluations,
/// 300 ms apart (a GPU restart in flight must not read as death). A single
/// live context wins; `NoContext` requires an *answer without a context* on
/// every attempt; anything else is `Unknown`.
pub async fn probe_gl_live(session: &crate::cdp::CdpSession) -> GlLive {
    let mut saw_no_context = false;
    for _ in 0..3 {
        if let Ok(v) = session
            .send_with_timeout(
                "Runtime.evaluate",
                Some(json!({ "expression": GL_PROBE_ONCE_EXPR, "returnByValue": true })),
                Duration::from_secs(2),
            )
            .await
        {
            match extract_renderer(&v) {
                Some(Some(r)) => return GlLive::Live(r),
                Some(None) => saw_no_context = true,
                None => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    if saw_no_context {
        GlLive::NoContext
    } else {
        GlLive::Unknown
    }
}

/// Healthcheck the pipe transport over the browser-level connection
/// (`Target.getTargets` → `attachToTarget` flatten → evaluate → detach).
#[cfg(unix)]
pub(super) async fn probe_gl_pipe(client: &crate::cdp::CdpClient) -> Option<String> {
    for _ in 0..3 {
        if let Some(renderer) = probe_gl_pipe_once(client).await {
            return Some(renderer);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    None
}

#[cfg(unix)]
async fn probe_gl_pipe_once(client: &crate::cdp::CdpClient) -> Option<String> {
    let targets = client.send("Target.getTargets", None).await.ok()?;
    let empty = Vec::new();
    let infos = targets
        .get("targetInfos")
        .and_then(|t| t.as_array())
        .unwrap_or(&empty);
    let target_id = infos
        .iter()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .and_then(|t| t.get("targetId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    let res = client
        .send(
            "Target.attachToTarget",
            Some(json!({ "targetId": target_id, "flatten": true })),
        )
        .await
        .ok()?;
    let session_id = res.get("sessionId").and_then(|v| v.as_str())?.to_string();
    let session = crate::cdp::CdpSession::child(client.clone(), session_id.clone());
    let _ = session
        .send("Page.navigate", Some(json!({ "url": "about:blank" })))
        .await;
    let mut found = None;
    for _ in 0..3 {
        if let Ok(v) = session
            .send_with_timeout(
                "Runtime.evaluate",
                Some(json!({ "expression": GL_PROBE_WARM_EXPR, "returnByValue": true, "awaitPromise": true })),
                Duration::from_secs(5),
            )
            .await
        {
            if let Some(s) = v.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    found = Some(s.to_string());
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let _ = client
        .send(
            "Target.detachFromTarget",
            Some(json!({ "sessionId": session_id })),
        )
        .await;
    found
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn extract_renderer_classifies_replies() {
        let live = json!({ "result": { "value": "ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)" } });
        assert_eq!(
            extract_renderer(&live),
            Some(Some(
                "ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)".into()
            ))
        );
        let dead = json!({ "result": { "value": "" } });
        assert_eq!(extract_renderer(&dead), Some(None));
        let skip = json!({ "result": { "value": "__bb_skip__" } });
        assert_eq!(extract_renderer(&skip), None);
        assert_eq!(extract_renderer(&json!({ "result": {} })), None);
        assert_eq!(extract_renderer(&json!({})), None);
    }

    #[test]
    fn once_expr_shape_is_stable() {
        // The probe must never poll in-page (the callers own the retry
        // cadence) and must answer '' for a null context.
        assert!(!GL_PROBE_ONCE_EXPR.contains("setTimeout"));
        assert!(GL_PROBE_ONCE_EXPR.contains("getContext('webgl')"));
        assert!(GL_PROBE_ONCE_EXPR.contains("return '';"));
    }
}
