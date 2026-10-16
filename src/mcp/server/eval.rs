//! `eval` handler: two-phase JS evaluation + artifact offload of big results.

use serde_json::json;

use crate::error::{BladeError, Result};
use crate::page::Page;

use super::artifact_hint;

/// V7: evaluate JS in the page. If `ref_id` is non-empty, the
/// element is resolved and exposed to the script as `el`.
///
/// RESULT CONTRACT (P1, 2026-10-13) — the whole point is that a fresh agent
/// never has to guess how to read this back:
/// - text mode: ONE line `result: <compact JSON of the returned value>`, so
///   `read the rest of that line, json.loads it` always works. Pretty-printing
///   is gone (a multi-line payload used to break line-anchored parsing).
/// - a returned value that is *itself* a string containing JSON gets a
///   `note:` line saying so (the silent double-encode trap), and the exact
///   recovery move; so does a DOM node/Window (which cannot be serialized).
/// - `undefined` reads `result: undefined`, and is `null` in json mode.
/// - `format:"json"` (json mode): the tool result IS the value — bare JSON,
///   `{"data":…,"note":…}` when a note rides along, or the
///   `{"artifact":…,"bytes":…,"next_offset":0}` envelope when oversized.
/// - oversized text mode: `result: artifact (<shape>, N bytes JSON)` + preview
///   + the artifact hint (never a silently truncated payload).
pub async fn handle_eval(
    page: &mut Page,
    js: &str,
    ref_id: &str,
    json_out: bool,
) -> Result<String> {
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
            // A runtime SyntaxError can follow a mutation. Parse the SAME
            // wrapper inside an uncalled function before choosing the
            // statement fallback. `void` discards the inert function, so
            // neither user code nor a remote object survives this check.
            // compileScript requires Runtime.enable, which leaks automation.
            if exc.pointer("/exception/className").and_then(|v| v.as_str()) == Some("SyntaxError")
                && i + 1 < expressions.len()
            {
                if let Ok(parsed) = page.cdp_ref().send_with_timeout(
                    "Runtime.evaluate",
                    Some(json!({"expression": format!("void function(){{return ({expression});}}"), "returnByValue": true})),
                    std::time::Duration::from_secs(3),
                ).await {
                    if parsed.pointer("/exceptionDetails/exception/className").and_then(|v| v.as_str())
                        == Some("SyntaxError")
                    {
                        continue;
                    }
                }
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
                    let inner = inner.clone();
                    return format_eval_value(&inner, json_out).await;
                }
            }
        }

        let result = match &value {
            Some(v) => format_eval_value(v, json_out).await?,
            None => format_eval_undefined(json_out)?,
        };

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

/// One-word shape of a returned value — used where the payload moves out of
/// line (artifact header) and in notes, never as a required preamble line.
fn shape_of(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(a) => format!("array[{}]", a.len()),
        serde_json::Value::Object(o) => format!("object{{{}}}", o.len()),
        serde_json::Value::String(s) => format!("string[{}]", s.chars().count()),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::Bool(_) => "boolean".to_string(),
        serde_json::Value::Null => "null".to_string(),
    }
}

/// The note the caller MUST see, or "" when the value speaks for itself.
/// Both cases here are silent traps that cost agents real work: a string whose
/// content is JSON (parsed twice or thrown away), and a DOM node/Window that
/// came back as a placeholder string.
fn eval_note(value: &serde_json::Value) -> String {
    let serde_json::Value::String(s) = value else {
        return String::new();
    };
    if s.starts_with("unserializable object") {
        return "the script returned a DOM node/Window, which cannot be serialized - return a property instead (el.href, el.innerText, Object.keys(el))".to_string();
    }
    let t = s.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(t) {
            return format!(
                "the script returned a STRING containing JSON (parses as {}) - parse the value once more, or return the object directly from the script",
                shape_of(&parsed)
            );
        }
    }
    String::new()
}

/// `undefined` in text mode is stated in words; in json mode it is `null`
/// (documented in the tool schema) because JSON has no undefined.
fn format_eval_undefined(json_out: bool) -> Result<String> {
    if json_out {
        return Ok("null".to_string());
    }
    Ok("result: undefined\nnote: the script returned undefined (no value) - check the property name, or return a value (return document.title)".to_string())
}

async fn format_eval_value(value: &serde_json::Value, json_out: bool) -> Result<String> {
    const INLINE_CAP: usize = 8000;
    let compact = serde_json::to_string(value)?;
    let note = eval_note(value);
    if json_out {
        if compact.len() > INLINE_CAP {
            let path = crate::artifacts::write_artifact(&compact, "json")?;
            return Ok(serde_json::json!({
                "artifact": path,
                "bytes": compact.len(),
                "next_offset": 0,
            })
            .to_string());
        }
        if note.is_empty() {
            return Ok(compact);
        }
        return Ok(serde_json::json!({ "data": value, "note": note }).to_string());
    }
    if compact.len() > INLINE_CAP {
        let path = crate::artifacts::write_artifact(&compact, "json")?;
        let preview: String = compact.chars().take(600).collect();
        return Ok(format!(
            "result: artifact ({}, {} bytes JSON)\npreview: {preview}…\n{}",
            shape_of(value),
            compact.len(),
            artifact_hint(&path)
        ));
    }
    if note.is_empty() {
        return Ok(format!("result: {compact}"));
    }
    Ok(format!("result: {compact}\nnote: {note}"))
}

#[cfg(test)]
mod eval_contract_tests {
    //! P1: the eval result contract is the thing agents parse. These tests pin
    //! the exact shapes that were ambiguous before: compact single-line values,
    //! the string-containing-JSON note, undefined, and json mode. Everything
    //! here stays under the artifact cap, so no test writes to disk.

    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn text_mode_is_one_compact_line_of_json() {
        // The old format pretty-printed and prefixed, so a line-anchored
        // parser saw a truncated value. Now the whole value is one line.
        let out = format_eval_value(&json!([{"a": 1}, {"b": "x"}]), false)
            .await
            .unwrap();
        assert_eq!(out, "result: [{\"a\":1},{\"b\":\"x\"}]");
        assert!(!out.contains('\n'), "result line must not wrap: {out}");
        // Scalars keep their JSON type explicitly.
        assert_eq!(
            format_eval_value(&json!(12), false).await.unwrap(),
            "result: 12"
        );
        assert_eq!(
            format_eval_value(&json!(true), false).await.unwrap(),
            "result: true"
        );
        assert_eq!(
            format_eval_value(&json!(null), false).await.unwrap(),
            "result: null"
        );
    }

    #[tokio::test]
    async fn a_string_containing_json_says_so() {
        // The agent's real bug: the script returned a JSON STRING, the caller's
        // parser expected a bare array, and the data was thrown away silently.
        let out = format_eval_value(&json!("[{\"total\":0}]"), false)
            .await
            .unwrap();
        assert!(out.starts_with("result: \"[{\\\"total\\\":0}]\""), "{out}");
        assert!(out.contains("STRING containing JSON"), "{out}");
        assert!(out.contains("array[1]"), "shape of the parsed value: {out}");
        // A plain string needs no note — the quotes speak.
        let out = format_eval_value(&json!("hello"), false).await.unwrap();
        assert_eq!(out, "result: \"hello\"");
        // A DOM node comes back as a placeholder: say what to do instead.
        let out = format_eval_value(&json!("unserializable object (Window/DOM node)"), false)
            .await
            .unwrap();
        assert!(out.contains("return a property instead"), "{out}");
    }

    #[tokio::test]
    async fn undefined_is_stated_in_words_and_null_in_json_mode() {
        let out = format_eval_undefined(false).unwrap();
        assert!(out.starts_with("result: undefined"), "{out}");
        assert!(out.contains("note:"), "{out}");
        assert_eq!(format_eval_undefined(true).unwrap(), "null");
    }

    #[tokio::test]
    async fn json_mode_returns_bare_json_or_a_documented_envelope() {
        // Bare: the whole tool result IS the value.
        assert_eq!(
            format_eval_value(&json!({"n": 3, "ok": true}), true)
                .await
                .unwrap(),
            "{\"n\":3,\"ok\":true}"
        );
        // A note wraps the payload as {data, note} — still valid JSON.
        let out = format_eval_value(&json!("[1,2]"), true).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON envelope");
        assert!(v.get("data").is_some() && v.get("note").is_some(), "{out}");
        // Shape labels are what the artifact header and notes use.
        assert_eq!(shape_of(&json!([1, 2, 3])), "array[3]");
        assert_eq!(shape_of(&json!({"a": 1})), "object{1}");
        assert_eq!(shape_of(&json!("abc")), "string[3]");
        assert_eq!(shape_of(&json!(1.5)), "number");
        assert_eq!(shape_of(&json!(false)), "boolean");
        assert_eq!(shape_of(&json!(null)), "null");
    }
}
