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

/// One-shot GL probe (G04): ONE bounded evaluate, no in-page poll. Returns a
/// JSON string: `{"m":"<renderer>","w":null}` when the main-thread context
/// exists; when it is null, a BOUNDED worker check (blob worker, 1.2s race)
/// adds the worker-side evidence — `w` = a renderer string (worker GL still
/// live; partial loss), `""` (worker answered, its context is null too;
/// total loss) or null (worker check unavailable). The main/worker split is
/// what tells a full renderer loss apart from a main-thread-only one — the
/// benchmark observed exactly that disagreement. `__bb_skip__` marks a
/// restricted origin (WebUI) where canvas access proves nothing.
const GL_PROBE_ONCE_EXPR: &str = r#"(async()=>{try{var p=location.protocol;if(p!=='http:'&&p!=='https:'&&p!=='file:'&&p!=='about:'&&p!=='data:'&&p!=='blob:')return '__bb_skip__';}catch(e){}
var R=function(g){try{var e=g.getExtension('WEBGL_debug_renderer_info');return e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):String(g.getParameter(g.RENDERER));}catch(_){return '';}};
var m='';
try{var c=document.createElement('canvas');var g=c.getContext('webgl');if(g)m=R(g);}catch(err){}
if(m)return JSON.stringify({m:m,w:null});
var w=null;
try{
if(typeof Worker!=='undefined'&&typeof Blob!=='undefined'&&typeof OffscreenCanvas!=='undefined'){
var src='self.onmessage=function(){var s="";try{var c=new OffscreenCanvas(1,1);var g=c.getContext("webgl");if(g){try{var e=g.getExtension("WEBGL_debug_renderer_info");s=e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):String(g.getParameter(g.RENDERER));}catch(_){}}}catch(_){}postMessage(s)};';
var u=URL.createObjectURL(new Blob([src],{type:'text/javascript'}));
var wk=new Worker(u);
w=await new Promise(function(res){var done=false;var t=setTimeout(function(){if(!done){done=true;res(null);}},1200);wk.onmessage=function(ev){if(!done){done=true;clearTimeout(t);res(String(ev.data||''));}};wk.onerror=function(){if(!done){done=true;clearTimeout(t);res(null);}};wk.postMessage('go');});
try{wk.terminate()}catch(_){}
try{URL.revokeObjectURL(u)}catch(_){}
}
}catch(_){w=null;}
return JSON.stringify({m:'',w:w});})()"#;

/// Evidence about worker-side WebGL when the main thread has none (G04).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerEvidence {
    /// A worker context exists; its renderer string.
    Live(String),
    /// The worker answered and its context creation also returned null.
    Null,
    /// The worker check did not run or did not answer (timeout / unsupported).
    Unavailable,
}

/// Live GL state from a fast probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlLive {
    /// A WebGL context exists; its renderer string.
    Live(String),
    /// The page answered and main-thread context creation returned null on
    /// every attempt; `worker` carries the bounded worker-side evidence.
    NoContext { worker: WorkerEvidence },
    /// Inconclusive: transport failure, malformed reply, or a restricted origin.
    Unknown,
}

/// Classification of one `Runtime.evaluate` reply to [`GL_PROBE_ONCE_EXPR`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum OnceReply {
    Live(String),
    NoContext(WorkerEvidence),
    Skip,
    Malformed,
}

fn extract_reply(v: &serde_json::Value) -> OnceReply {
    let Some(s) = v
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
    else {
        return OnceReply::Malformed;
    };
    if s == "__bb_skip__" {
        return OnceReply::Skip;
    }
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(s) else {
        return OnceReply::Malformed;
    };
    let m = obj.get("m").and_then(|m| m.as_str()).unwrap_or("");
    if !m.is_empty() {
        return OnceReply::Live(m.to_string());
    }
    let worker = match obj.get("w") {
        Some(serde_json::Value::String(r)) if !r.is_empty() => WorkerEvidence::Live(r.clone()),
        Some(serde_json::Value::String(_)) => WorkerEvidence::Null,
        _ => WorkerEvidence::Unavailable,
    };
    OnceReply::NoContext(worker)
}

/// Confirm the live GL state over a session: up to 3 quick evaluations,
/// 300 ms apart (a GPU restart in flight must not read as death). A single
/// live context wins; `NoContext` requires an *answer without a context* on
/// every attempt and keeps the strongest worker evidence seen; anything
/// else is `Unknown`.
pub async fn probe_gl_live(session: &crate::cdp::CdpSession) -> GlLive {
    let mut saw_no_context = false;
    let mut worker = WorkerEvidence::Unavailable;
    for _ in 0..3 {
        if let Ok(v) = session
            .send_with_timeout(
                "Runtime.evaluate",
                Some(json!({ "expression": GL_PROBE_ONCE_EXPR, "returnByValue": true, "awaitPromise": true })),
                Duration::from_secs(4),
            )
            .await
        {
            match extract_reply(&v) {
                OnceReply::Live(r) => return GlLive::Live(r),
                OnceReply::NoContext(w) => {
                    saw_no_context = true;
                    if !matches!(w, WorkerEvidence::Unavailable) {
                        worker = w;
                    }
                }
                OnceReply::Skip | OnceReply::Malformed => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    if saw_no_context {
        GlLive::NoContext { worker }
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

    fn once_reply(m: &str, w: Option<&str>) -> serde_json::Value {
        let payload = json!({ "m": m, "w": w });
        json!({ "result": { "value": payload.to_string() } })
    }

    #[test]
    fn extract_reply_classifies_replies() {
        // Main-thread live: the renderer; worker field not consulted.
        assert_eq!(
            extract_reply(&once_reply(
                "ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)",
                None
            )),
            OnceReply::Live(
                "ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)".into()
            )
        );
        // Main null, worker live: PARTIAL loss with worker evidence.
        assert_eq!(
            extract_reply(&once_reply("", Some("ANGLE (Intel, partial)"))),
            OnceReply::NoContext(WorkerEvidence::Live("ANGLE (Intel, partial)".into()))
        );
        // Main null, worker answered null: total loss.
        assert_eq!(
            extract_reply(&once_reply("", Some(""))),
            OnceReply::NoContext(WorkerEvidence::Null)
        );
        // Main null, worker unavailable (unsupported / timeout).
        assert_eq!(
            extract_reply(&once_reply("", None)),
            OnceReply::NoContext(WorkerEvidence::Unavailable)
        );
        // Skip / malformed / missing result.
        assert_eq!(
            extract_reply(&json!({ "result": { "value": "__bb_skip__" } })),
            OnceReply::Skip
        );
        assert_eq!(
            extract_reply(&json!({ "result": { "value": "nonsense" } })),
            OnceReply::Malformed
        );
        assert_eq!(
            extract_reply(&json!({ "result": {} })),
            OnceReply::Malformed
        );
        assert_eq!(extract_reply(&json!({})), OnceReply::Malformed);
    }

    #[test]
    fn once_expr_shape_is_stable() {
        // The main check is ONE sync context attempt; when it is null, ONE
        // bounded worker race (1.2s) adds evidence — no in-page polling loop
        // (the callers own the retry cadence).
        assert!(GL_PROBE_ONCE_EXPR.contains("getContext('webgl')"));
        assert!(GL_PROBE_ONCE_EXPR.contains("OffscreenCanvas"));
        assert!(GL_PROBE_ONCE_EXPR.contains("1200"));
        assert!(GL_PROBE_ONCE_EXPR.contains("JSON.stringify"));
        assert!(!GL_PROBE_ONCE_EXPR.contains("setInterval"));
    }

    #[test]
    fn once_expr_is_valid_js() {
        // A syntax break here silently degrades every health check to
        // Unknown — the same class the assembled-injection node checks guard.
        let dir = std::env::temp_dir().join("bladebro-probe-tests");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("gl-probe-once.js");
        if std::fs::write(&path, GL_PROBE_ONCE_EXPR).is_err() {
            return;
        }
        let out = match std::process::Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
        {
            Ok(o) => o,
            Err(_) => return, // node unavailable: skip
        };
        assert!(
            out.status.success(),
            "GL_PROBE_ONCE_EXPR must parse as JS:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
