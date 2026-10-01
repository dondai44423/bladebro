//! `eval` handler: two-phase JS evaluation + artifact offload of big results.

use serde_json::json;

use crate::error::{BladeError, Result};
use crate::page::Page;

use super::artifact_hint;

/// V7: evaluate JS in the page. If `ref_id` is non-empty, the
/// element is resolved and exposed to the script as `el`.
/// Result is JSON-stringified, capped at 4KB inline; bigger
/// payloads go to an artifact file (V10).
pub async fn handle_eval(page: &mut Page, js: &str, ref_id: &str) -> Result<String> {
    // Two-phase completion-value evaluation, no fragile heuristics:
    // 1. Expression wrapper (no eval at all): `return (<js>);` — covers
    //    single expressions AND IIFEs, works on CSP pages (no eval used),
    //    keeps every declaration scoped to this call.
    // 2. Direct-eval wrapper: statements (`var x=5; x+7`) and statement
    //    code get real console semantics via `return eval(<js literal>);`
    //    — the last statement's value comes back. Attempted only when the
    //    expression wrapper hit a parse-time SyntaxError, so code with
    //    side effects never runs twice (parse errors happen before any
    //    statement executes).
    // If the page's CSP blocks eval and the code isn't a plain expression,
    // the honest EvalError is surfaced with a hint.
    let expr_wrapper = format!("(function(){{ return ({js}); }})()");
    let eval_wrapper = format!(
        "(function(){{ return eval({}); }})()",
        serde_json::to_string(js).unwrap_or_else(|_| "\"\"".into())
    );
    let expressions: Vec<String> = if ref_id.is_empty() {
        vec![expr_wrapper.clone(), eval_wrapper.clone()]
    } else {
        page.ensure_ref(ref_id).await?;
        let (sig, frame) = {
            let el = page
                .model()
                .element(ref_id)
                .ok_or_else(|| BladeError::StaleRef(ref_id.to_string()))?;
            (el.raw.sig.clone(), el.raw.frame.clone())
        };
        let sig_js = serde_json::to_string(&sig)?;
        let frame_js = serde_json::to_string(&frame)?;
        // Find the element by its CANONICAL sig — the same role()/name()/deepAll
        // scheme the capture script and find_by_sig use (V25c/W2). The prior
        // inline version matched tagName ('a') against the semantic role
        // ('link'), so eval-with-ref was broken for links and most inputs.
        // Then invoke the user's JS with `el` in scope.
        let template = "((sig,frame)=>{ ".to_string()
            + &crate::page::perception::JS_PREAMBLE
            + "let doc=document;for(const idx of frame){const ifr=[...doc.querySelectorAll('iframe')][idx];if(!ifr)return{__blade_not_found:true};try{doc=ifr.contentDocument;if(!doc)return{__blade_not_found:true};}catch(e){return{__blade_not_found:true};}}"
            + "const all=deepAll(doc,sel);const fps=frame.join(',');const counts={};let el=null;"
            + "for(const n of all){const r=role(n);if(r==='hidden')continue;const nm=name(n,false);const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;const s=fps+'|'+r+'|'+nm+'|'+counts[key];if(s===sig){el=n;break}}"
            + "if(!el)return{__blade_not_found:true};"
            + "const result=((el)=>{return("
            + "{candidate}"
            + ");})(el);return{__blade_result:result===undefined?null:result}})("
            + &sig_js
            + ","
            + &frame_js
            + ")";
        vec![
            template.replace("{candidate}", &expr_wrapper),
            template.replace("{candidate}", &eval_wrapper),
        ]
    };

    // Baseline of open page targets, captured once before evaluation, so the
    // popup report after the eval only lists NEW tabs (never pre-existing).
    let wants_popups =
        js.contains("window.open") || js.contains("open('") || js.contains("open(\"");
    let before_tabs: Vec<String> = if wants_popups {
        page.cdp_ref()
            .send("Target.getTargets", None)
            .await
            .map(|t| {
                t.get("targetInfos")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                            .filter_map(|t| {
                                t.get("targetId")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    for (i, expression) in expressions.iter().enumerate() {
        let res = match page
            .cdp_ref()
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                })),
            )
            .await
        {
            Ok(res) => res,
            // window.open (and any Window/DOM return) blows up returnByValue
            // with -32000 "Object reference chain is too long". The eval
            // itself ran: degrade gracefully to "unserializable result" and
            // let the popup detection below still report the new tab.
            Err(e) if e.to_string().contains("Object reference chain is too long") => {
                // window.open (and any Window/DOM return) blows up returnByValue
                // with -32000. The eval itself ran: treat as success with an
                // unserializable result and fall through so the popup
                // detection below still reports the new tab.
                json!({ "result": { "type": "string", "value": "unserializable object (Window/DOM node)" } })
            }
            Err(e) => return Err(e),
        };

        if let Some(exc) = res.get("exceptionDetails") {
            let msg = exc
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .or_else(|| exc.get("text").and_then(|t| t.as_str()))
                .unwrap_or("JS evaluation failed")
                .to_string();
            // Expression-wrapper SyntaxError = the code was statements, not
            // an expression. Retry via direct eval.
            if msg.contains("SyntaxError") && i + 1 < expressions.len() {
                continue;
            }
            if msg.contains("EvalError") && msg.contains("unsafe-eval") {
                return Err(BladeError::Other(
                    "eval failed: page CSP blocks eval(). Use a plain expression instead of statements on this page.".into(),
                ));
            }
            return Err(BladeError::Other(format!(
                "eval failed: {}",
                crate::platform::truncate_utf8(&msg, 200)
            )));
        }

        let value = res.get("result").and_then(|r| r.get("value")).cloned();
        if let Some(ref v) = value {
            if v.get("__blade_not_found").and_then(|b| b.as_bool()) == Some(true) {
                return Err(BladeError::ElementNotFound(format!(
                    "{ref_id} not found in live DOM"
                )));
            }
            if !ref_id.is_empty() {
                if let Some(inner) = v.get("__blade_result") {
                    let json_str = serde_json::to_string_pretty(inner)?;
                    return format_eval_result(&json_str).await;
                }
            }
        }

        let json_str = match &value {
            Some(v) => serde_json::to_string_pretty(v)?,
            None => "undefined".to_string(),
        };
        let result = format_eval_result(&json_str).await?;

        // Bug 3: Detect window.open popups. After eval, if the expression
        // contained window.open, check for new page targets and report them.
        // Without --disable-popup-blocking, popups are silently swallowed.
        if wants_popups {
            if let Ok(targets) = page.cdp_ref().send("Target.getTargets", None).await {
                if let Some(infos) = targets.get("targetInfos").and_then(|t| t.as_array()) {
                    let new_tabs: Vec<String> = infos
                        .iter()
                        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                        .filter_map(|t| {
                            let id = t.get("targetId").and_then(|v| v.as_str())?.to_string();
                            if before_tabs.contains(&id) {
                                return None;
                            }
                            let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            if !url.is_empty() && url != "about:blank" {
                                Some(format!(
                                    "{} → {}",
                                    id,
                                    crate::platform::truncate_utf8(url, 60)
                                ))
                            } else {
                                Some(id)
                            }
                        })
                        .collect();
                    if !new_tabs.is_empty() {
                        return Ok(format!(
                            "{result}\n✓ {} popup tab(s) created:\n  {}\n  use state tabs to inspect, switch-tab to move",
                            new_tabs.len(),
                            new_tabs.join("\n  ")
                        ));
                    }
                }
            }
        }
        return Ok(result);
    }

    unreachable!("at least one expression candidate is always provided");
}

async fn format_eval_result(json_str: &str) -> Result<String> {
    const INLINE_CAP: usize = 8000;
    if json_str.len() <= INLINE_CAP {
        Ok(format!("result: {json_str}"))
    } else {
        let path = crate::artifacts::write_artifact(json_str, "json")?;
        let preview: String = json_str.chars().take(1000).collect();
        Ok(format!(
            "result ({} bytes)\npreview: {preview}…\n{}",
            json_str.len(),
            artifact_hint(&path)
        ))
    }
}
