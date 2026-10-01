//! `vision` handler: screenshot (+ optional Set-of-Marks overlay).

use serde_json::{json, Value};

use crate::error::BladeError;
use crate::page::Page;

/// Capture a screenshot of the current page and return it as an MCP image
/// content block (base64 PNG).
///
/// This is the `vision` tool (decision D5) — a rare fallback for canvas
/// content, exotic layouts, or when the structural model fails.
pub async fn handle_vision(
    id: Option<Value>,
    args: &Value,
    page: &mut Page,
) -> std::result::Result<Value, BladeError> {
    let marks = args.get("marks").and_then(|m| m.as_bool()).unwrap_or(false);
    let mut note = String::new();

    // A tab that is not the frontmost one can stall captureScreenshot —
    // Chromium throttles occluded/background renderers, so the call burns
    // its full CDP timeout and vision fails (reproduced live: vision right
    // after open-tab timed out at 30s, twice, on the new tab). Activate
    // first; best-effort (fails harmlessly when the target is mid-detach).
    let _ = page.cdp_ref().send("Page.bringToFront", None).await;

    if marks {
        // V14: Set-of-Marks. Paint numbered ref badges on
        // visible elements — the refs match the structural
        // model exactly, so a vision-capable agent can say
        // "click e5" and the act tool just works.
        let items: Vec<(String, String)> = page.model().elements()
            .iter()
            .map(|e| (e.ref_id.clone(), e.raw.sig.clone()))
            .collect();
        let items_js = serde_json::to_string(&items)?;
        let overlay = "((items)=>{".to_string()
            + "const d=document;if(!d||!d.body)return 0;"
            + &crate::page::perception::JS_PREAMBLE
            + "const old=d.getElementById('blade-marks');if(old)old.remove();"
            + "const ov=d.createElement('div');ov.id='blade-marks';"
            + "ov.style.cssText='position:fixed;inset:0;pointer-events:none;z-index:2147483647;';"
            + "const vw=innerWidth,vh=innerHeight;"
            + "const all=deepAll(d,sel);"
            + "const sigs=new Map();const counts={};"
            // Sig = '' frame prefix (main doc) | role | shortName | rank,
            // rank counted over ALL matches (vis-failing included) — matches
            // the capture script (V25c). Only vis-passing elements are mapped
            // for marking, but the rank counts everything so sigs agree.
            + "for(let i=0;i<all.length;i++){const n=all[i];const r=role(n);if(r==='hidden')continue;"
            + "const nm=name(n,false);const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;"
            + "if(!vis(n))continue;"
            + "sigs.set('|'+r+'|'+nm+'|'+counts[key],n);}"
            + "let marked=0;"
            + "for(const[ref,sig]of items){const el=sigs.get(sig);if(!el)continue;"
            + "const rect=el.getBoundingClientRect();"
            + "if(rect.bottom<0||rect.top>vh||rect.right<0||rect.left>vw)continue;"
            + "const b=d.createElement('div');b.textContent=ref;"
            + "b.style.cssText='position:fixed;left:'+Math.max(0,rect.x+rect.width/2-12)+'px;top:'+Math.max(0,rect.y+rect.height/2-8)+'px;background:rgba(220,0,110,0.92);color:#fff;font:bold 11px/14px monospace;padding:0 4px;border-radius:3px;border:1px solid #fff;';"
            + "ov.appendChild(b);marked++;}"
            + "d.body.appendChild(ov);return marked;})(" + &items_js + ")";
        let res = page.cdp_ref().send("Runtime.evaluate", Some(json!({
            "expression": overlay,
            "returnByValue": true,
        }))).await?;
        if let Some(exc) = res.get("exceptionDetails") {
            let msg = exc.get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .unwrap_or("overlay failed");
            note = format!(" (marks overlay error: {})", crate::platform::truncate_utf8(msg, 120));
        } else {
            let marked = res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_i64()).unwrap_or(0);
            note = format!(" ({marked} elements marked; badge refs match the structural model)");
        }
    }

    let cdp = page.cdp_ref();
    let result = cdp
        .send(
            "Page.captureScreenshot",
            Some(serde_json::json!({
                "format": "png",
            })),
        )
        .await;

    // Remove the overlay BEFORE processing the result —
    // the page must never keep our marks.
    if marks {
        let _ = page.cdp_ref().send("Runtime.evaluate", Some(json!({
            "expression": "(()=>{const o=document.getElementById('blade-marks');if(o)o.remove();return true;})()",
            "returnByValue": true,
        }))).await;
    }

    match result {
        Ok(res) => {
            let data = res.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "Screenshot returned no data." }],
                        "isError": true,
                    }
                }));
            }
            // H2: multi-MB screenshots (tall pages, 4K full-page) inline
            // through the single stdout writer backpressure the whole
            // server (stdin unread, signals unhandled, idle timer frozen)
            // on a slow client. Offload big ones to a PNG artifact and
            // return the path; vision-capable agents read the file.
            if data.len() > 900_000 {
                if let Some(bytes) = crate::cli::base64_decode(data) {
                    if let Ok(path) = crate::artifacts::write_artifact_bytes(&bytes, "png") {
                        return Ok(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": format!(
                                    "screenshot{note} ({} KB — too large to inline, saved to {path})",
                                    bytes.len() / 1024
                                ) }]
                            }
                        }));
                    }
                }
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [
                        { "type": "text", "text": format!("screenshot{note}") },
                        {
                            "type": "image",
                            "data": data,
                            "mimeType": "image/png"
                        }
                    ]
                }
            }))
        }
        // Propagate Closed so serve() can self-heal.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("\u{2717} error: {e}") }],
                "isError": true,
            }
        })),
    }
}
