//! The action layer (decision D5/D6): the `act` tool.
//!
//! `act` resolves a stable ref to its signature, re-locates the element in the
//! live DOM by that signature, dispatches the action via CDP, waits for the
//! page to settle (navigation or DOM mutation), then recaptures and returns the
//! delta as the observation. This closes the loop: act → observe → act.
//!
//! Design choices:
//! - **Click** uses `Input.dispatchMouseEvent` at the element's center — real
//!   mouse events, not `el.click()`. Better for stealth and for sites that
//!   listen for mousedown/mouseup, not just synthetic click.
//! - **Type** uses `Input.insertText` into a focused element — fires proper
//!   input events, works for text inputs, textareas, and contenteditable.
//! - **Select** sets the `<select>` value in-page and dispatches change —
//!   mouse-based option selection is fragile across select implementations.
//! - **Press** dispatches keyDown + keyUp via `Input.dispatchKeyEvent`.
//! - **Scroll** uses `window.scrollBy` via evaluate.

use std::time::Duration;
use std::sync::atomic::AtomicUsize;

use serde::Deserialize;
use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::page::perception::JS_PREAMBLE;
use crate::page::{capture, wait_for_settle, wait_for_settle_with_network, PageDelta, LivePageModel};

/// What the agent can do. Few verbs, full control.
#[derive(Debug, Clone)]
pub enum Action {
    /// Click an element by its ref id.
    Click { ref_id: String },
    /// S10: Click at exact viewport coordinates — works on cross-origin
    /// iframes, canvas, shadow DOM (anything Input-domain can reach).
    ClickCoord { x: f64, y: f64 },
    /// Type text into a textbox, replacing existing content.
    Type { ref_id: String, text: String },
    /// Clear a textbox.
    Clear { ref_id: String },
    /// Select an option in a `<select>` by value or visible text.
    Select { ref_id: String, option: String },
    /// Press a key (e.g. "Enter", "Tab", "Escape", "ArrowDown").
    Press { key: String },
    /// Scroll by (dx, dy) CSS pixels.
    Scroll { dx: i64, dy: i64 },
    /// Read the text content of a specific element by ref id.
    Read { ref_id: String },
    /// Wait for a condition: "element" (element with matching role/name appears),
    /// "title" (title contains text), "settle" (DOM stabilizes).
    Wait { condition: String, text: String, timeout: Duration },
    /// Go back in browser history.
    Back,
    /// Go forward in browser history.
    Forward,
    /// Reload the current page.
    Reload,
    /// Hover over an element (move mouse to its center, no click).
    /// Triggers CSS :hover, hover-dropdowns, tooltips, hover-cards.
    Hover { ref_id: String },
    /// Upload a file to an `<input type="file">` element.
    Upload { ref_id: String, path: String },
}

impl Action {
    /// Returns the ref id this action targets, if any.
    pub fn ref_id(&self) -> Option<&str> {
        match self {
            Action::ClickCoord { .. } => None,
            Action::Click { ref_id }
            | Action::Type { ref_id, .. }
            | Action::Clear { ref_id }
            | Action::Select { ref_id, .. }
            | Action::Read { ref_id }
            | Action::Hover { ref_id }
            | Action::Upload { ref_id, .. } => Some(ref_id),
            Action::Press { .. } | Action::Scroll { .. } | Action::Wait { .. } | Action::Back | Action::Forward | Action::Reload => None,
        }
    }
}

/// Result of the find-by-sig script: the element's current box + metadata.
#[derive(Debug, Deserialize)]
struct FoundElement {
    ok: bool,
    #[serde(default)]
    #[allow(dead_code)]
    reason: Option<String>,
    #[serde(default, rename = "box")]
    box_: Option<[f64; 4]>,
    #[serde(default)]
    #[allow(dead_code)]
    tag: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    element_type: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    /// Whether the element is the topmost element at its center point.
    /// False when something (e.g. autocomplete overlay) covers it.
    #[serde(default, rename = "isTopmost")]
    is_topmost: Option<bool>,
    /// Text content of the element (for "read" mode).
    #[serde(default)]
    text: Option<String>,
}

/// Resolve a ref to its (signature, frame path) in the LPM.
fn resolve_ref(lpm: &LivePageModel, ref_id: &str) -> Result<(String, Vec<usize>)> {
    lpm.element(ref_id)
        .map(|e| (e.raw.sig.clone(), e.raw.frame.clone()))
        .ok_or_else(|| BladeError::StaleRef(format!(
            "{ref_id} \u{2014} page may have navigated. Use see or text addressing (act click text=\"...\")"
        )))
}

/// Read the text content of an element by its ref id.
/// Returns up to 5000 chars of innerText.
pub async fn read_text(cdp: &CdpSession, lpm: &LivePageModel, ref_id: &str) -> Result<String> {
    let (sig, frame) = resolve_ref(lpm, ref_id)?;
    let found = find_by_sig(cdp, &sig, &frame, "read", None).await?;
    if !found.ok {
        return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
    }
    Ok(found.text.unwrap_or_default())
}

/// A text-addressing match result.
#[derive(Debug, Deserialize)]
pub struct TextMatch {
    pub sig: String,
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub frame: Vec<usize>,
    pub score: i64,
}

/// Find actionable elements by text/label query. Returns up to 5 matches
/// sorted by relevance score. Used by text addressing (M3): `act click text="Sign in"`
/// resolves the text to an element ref without a prior `see` call.
pub async fn find_by_text(
    cdp: &CdpSession,
    query: &str,
    role_filter: Option<&str>,
) -> Result<Vec<TextMatch>> {
    let query_js = serde_json::to_string(query)?;
    let role_js = match role_filter {
        Some(r) => serde_json::to_string(r)?,
        None => "null".to_string(),
    };

    let expression = "((query,rf)=>{"
        .to_string()
        + "const d=document;if(!d||!d.body)return[];"
        + &JS_PREAMBLE
        + "const all=deepAll(d,sel);const results=[];const counts={};"
        + "const q=query.toLowerCase();"
        // Rank counted over ALL matches (vis-failing included, role-hidden
        // excluded) — identical to the capture script (V25c). Only vis-passing
        // matches are RETURNED, but the rank counts everything so sigs agree.
        // Frame prefix '' (main doc) + '|' matches capture's fps format.
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const snm=name(n,false);"
        + "const key=r+'\\u0000'+snm;counts[key]=(counts[key]||0)+1;"
        + "if(rf&&r!==rf)continue;"
        + "if(!vis(n))continue;"
        + "const sig='|'+r+'|'+snm+'|'+counts[key];"
        + "const nm=name(n,true);let score=0;"
        + "if(nm===query)score=100;else if(nm.toLowerCase()===q)score=80;"
        + "else if(nm.includes(query))score=70;else if(nm.toLowerCase().includes(q))score=60;"
        + "else{const al=n.getAttribute('aria-label')||'';const ph=n.placeholder||'';const ti=n.title||'';const alt=n.getAttribute('alt')||'';"
        + "if(al.toLowerCase()===q)score=50;else if(ph.toLowerCase()===q)score=45;"
        + "else if(ti.toLowerCase()===q)score=40;else if(alt.toLowerCase()===q)score=35;"
        + "else if(al.toLowerCase().includes(q))score=30;else if(ph.toLowerCase().includes(q))score=25;}"
        + "if(score>0)results.push({sig,score,role:r,name:nm,frame:[]});"
        + "}"
        + "results.sort((a,b)=>b.score-a.score);return results.slice(0,5);})"
        + "(" + &query_js + "," + &role_js + ")";

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown find-by-text error");
        return Err(BladeError::Other(format!("find-by-text failed: {msg}")));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| BladeError::Other("find-by-text returned no value".to_string()))?;

    let matches: Vec<TextMatch> = serde_json::from_value(value.clone())?;
    Ok(matches)
}

/// Compute a one-line outcome verdict from the action + delta + click info.
/// This is the M1 verdict — every act tells the agent what happened.
fn compute_verdict(
    action: &Action,
    delta: &PageDelta,
    lpm: &LivePageModel,
    click_via: Option<(&str, &[&str])>,
) -> String {
    let dom_changed = !delta.added.is_empty() || !delta.removed.is_empty() || !delta.changed.is_empty();
    match action {
        Action::ClickCoord { x, y } => {
            if delta.navigated {
                format!("outcome: navigated \u{2192} {} via coord-click({x:.0},{y:.0})", shorten_url(&delta.url))
            } else if dom_changed {
                format!("outcome: dom-changed (+{} \u{2212}{}) via coord-click({x:.0},{y:.0})", delta.added.len(), delta.removed.len())
            } else if delta.content_changed {
                format!("outcome: dom-changed (content) via coord-click({x:.0},{y:.0})")
            } else {
                format!("outcome: no-effect (coord-click at {x:.0},{y:.0})")
            }
        }
        Action::Click { .. } => {
            let (via, tried) = click_via.unwrap_or(("", &[][..]));
            if delta.navigated {
                format!("outcome: navigated \u{2192} {} via {}", shorten_url(&delta.url), via)
            } else if dom_changed {
                format!("outcome: dom-changed (+{} \u{2212}{}) via {}", delta.added.len(), delta.removed.len(), via)
            } else if delta.content_changed {
                // Mutation watcher saw DOM effects on non-actionable
                // content — text swaps, counters, live regions.
                format!("outcome: dom-changed (content) via {}", via)
            } else {
                format!("outcome: no-effect (tried: {} \u{2014} element may be disabled, hidden, or hover-gated)", tried.join(", "))
            }
        }
        Action::Type { ref_id, text } => {
            let actual = lpm
                .element(ref_id)
                .and_then(|e| e.raw.value.as_deref())
                .unwrap_or("");
            if actual == text {
                format!("outcome: typed \u{2192} value=\"{}\"", clip(text, 40))
            } else if !actual.is_empty() {
                format!(
                    "outcome: typed \"{}\" \u{2192} value=\"{}\" (incomplete \u{2014} framework may mask input)",
                    clip(text, 40), clip(actual, 40)
                )
            } else {
                format!("outcome: typed \"{}\" \u{2192} value empty (input may be framework-controlled)", clip(text, 40))
            }
        }
        Action::Clear { ref_id } => format!("outcome: cleared {ref_id}"),
        Action::Select { option, .. } => format!("outcome: selected \"{}\"", clip(option, 40)),
        Action::Press { key } => {
            if delta.navigated {
                format!("outcome: pressed {key} \u{2192} navigated \u{2192} {}", shorten_url(&delta.url))
            } else if dom_changed {
                format!("outcome: pressed {key} \u{2192} dom-changed (+{} \u{2212}{})", delta.added.len(), delta.removed.len())
            } else {
                format!("outcome: pressed {key}")
            }
        }
        Action::Scroll { dx, dy } => format!("outcome: scrolled ({dx}, {dy})"),
        Action::Read { .. } => "outcome: read".to_string(),
        Action::Wait { condition, .. } => format!("outcome: waited ({condition})"),
        Action::Back => {
            if delta.navigated {
                format!("outcome: navigated back \u{2192} {}", shorten_url(&delta.url))
            } else {
                "outcome: back (no navigation)".to_string()
            }
        }
        Action::Forward => {
            if delta.navigated {
                format!("outcome: navigated forward \u{2192} {}", shorten_url(&delta.url))
            } else {
                "outcome: forward (no navigation)".to_string()
            }
        }
        Action::Reload => {
            format!("outcome: reloaded {}", shorten_url(&delta.url))
        }
        Action::Hover { ref_id } => {
            if dom_changed && !delta.navigated {
                format!("outcome: hovered {ref_id} \u{2192} dom-changed (+{})", delta.added.len())
            } else {
                format!("outcome: hovered {ref_id}")
            }
        }
        Action::Upload { path, .. } => format!("outcome: uploaded \"{}\"", clip(path, 40)),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('\u{2026}');
        t
    }
}

fn shorten_url(u: &str) -> String {
    let s = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")).unwrap_or(u);
    if s.len() > 80 {
        format!("{}\u{2026}", &s[..77])
    } else {
        s.to_string()
    }
}

/// Check if a condition is met, optionally waiting up to `timeout`.
/// Returns `true` if the condition was met, `false` if timed out.
///
/// This is the shared condition evaluator used by both the `Wait` action
/// (which ignores the return value — it just blocks) and the `if` step in
/// `run` (which uses the return value to choose a branch).
///
/// Conditions:
/// - `"title"`: page title contains `text` (case-insensitive).
/// - `"element"`: a visible actionable element whose role or name contains
///   `text` (case-insensitive) exists in the DOM.
/// - `"settle"` (or unknown): wait for DOM to settle, always returns `true`.
pub async fn check_condition(
    cdp: &CdpSession,
    condition: &str,
    text: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    match condition {
        "title" => {
            let needle = text.to_lowercase();
            loop {
                let res = cdp
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": "document.title",
                            "returnByValue": true,
                        })),
                    )
                    .await;
                let title = res
                    .ok()
                    .and_then(|r| {
                        r.get("result")
                            .and_then(|result| result.get("value"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_default();
                if title.to_lowercase().contains(&needle) {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        "element" => {
            let needle = text.to_lowercase().replace('"', "\\\"");
            let check = format!(
                "(()=>{{const d=document;if(!d||!d.body)return false;const sel='{selector}';const all=[...d.querySelectorAll(sel)];const vis=n=>{{const r=n.getBoundingClientRect();if(r.width===0||r.height===0)return false;const s=getComputedStyle(n);if(s.display==='none'||s.visibility==='hidden'||s.opacity==='0')return false;return true;}};const nodes=all.filter(vis);const t=\"{needle}\".toLowerCase();return nodes.some(n=>{{const role=(n.getAttribute('role')||n.tagName.toLowerCase());const name=(n.getAttribute('aria-label')||n.textContent||n.placeholder||'').trim();return role.toLowerCase().includes(t)||name.toLowerCase().includes(t);}});}})()",
                selector = crate::page::perception::JS_SELECTOR
            );
            loop {
                let res = cdp
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": &check,
                            "returnByValue": true,
                        })),
                    )
                    .await;
                let found = res
                    .ok()
                    .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_bool()))
                    .unwrap_or(false);
                if found {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        _ => {
            // "settle" or unknown — wait for DOM to settle, always true.
            let _ = wait_for_settle(cdp, timeout).await;
            true
        }
    }
}

/// Inject the find-by-sig script: re-locates the element by its signature in
/// the live DOM and either returns its box (mode "box") or performs an in-page
/// action (mode "focus", "clear", "select").
async fn find_by_sig(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
    mode: &str,
    text: Option<&str>,
) -> Result<FoundElement> {
    let sig_js = serde_json::to_string(sig)?;
    let mode_js = serde_json::to_string(mode)?;
    let text_js = match text {
        Some(t) => serde_json::to_string(t)?,
        None => "null".to_string(),
    };
    let frame_js = serde_json::to_string(frame)?;

    let expression = "((sig,mode,text,frame)=>{"
        .to_string()
        + "const d=document;if(!d||!d.body)return null;"
        + &JS_PREAMBLE
        + "let doc=d;let ox=0,oy=0;"
        + "for(let i=0;i<frame.length;i++){"
        + "const ifs=doc.querySelectorAll('iframe');const f=ifs[frame[i]];"
        + "if(!f)return{ok:false,reason:'frame gone'};"
        + "try{doc=f.contentDocument;if(!doc)return{ok:false,reason:'frame inaccessible'};}catch(e){return{ok:false,reason:'cross-origin'};}"
        + "const ir=f.getBoundingClientRect();ox+=ir.x;oy+=ir.y;}"
        + "const all=deepAll(doc,sel);"
        // Sig = framePath | role | shortName | rank, rank counted over ALL
        // matches (vis-failing included, role-hidden excluded) — identical to
        // the capture script (V25c). vis() is checked only on the matched
        // element, never for ranking, so ranks stay aligned with capture.
        + "const fps=frame.join(',');const counts={};"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const nm=name(n,false);"
        + "const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;"
        + "const s=fps+'|'+r+'|'+nm+'|'+counts[key];"
        + "if(s==="
        + &sig_js
        + "){if(!vis(n))return{ok:false,reason:'element hidden'};const rect=n.getBoundingClientRect();"
        + "const cx=rect.x+rect.width/2;const cy=rect.y+rect.height/2;"
        + "const top=doc.elementFromPoint(cx,cy);"
        + "const isTopmost=top===n||n.contains(top);"
        + "if(mode==='box'){if(!isTopmost){n.scrollIntoView({block:'center'});const r2=n.getBoundingClientRect();const cx2=r2.x+r2.width/2;const cy2=r2.y+r2.height/2;const top2=doc.elementFromPoint(cx2,cy2);return{ok:true,box:[Math.round(r2.x+ox)||0,Math.round(r2.y+oy)||0,Math.round(r2.width)||0,Math.round(r2.height)||0],tag:n.tagName.toLowerCase(),type:n.type||null,disabled:!!n.disabled,isTopmost:top2===n||n.contains(top2)};}return{ok:true,box:[Math.round(rect.x+ox)||0,Math.round(rect.y+oy)||0,Math.round(rect.width)||0,Math.round(rect.height)||0],tag:n.tagName.toLowerCase(),type:n.type||null,disabled:!!n.disabled,isTopmost:isTopmost};}"
        + "if(mode==='prepare'){n.scrollIntoView({block:'center'});n.focus();if('value' in n){n.value='';n.dispatchEvent(new Event('input',{bubbles:true}));n.dispatchEvent(new Event('change',{bubbles:true}));}const r3=n.getBoundingClientRect();return{ok:true,box:[Math.round(r3.x+ox)||0,Math.round(r3.y+oy)||0,Math.round(r3.width)||0,Math.round(r3.height)||0],disabled:!!n.disabled};}"
        + "if(mode==='focus'){n.focus();return{ok:true};}"
        + "if(mode==='clear'){if('value' in n){n.value='';n.dispatchEvent(new Event('input',{bubbles:true}));n.dispatchEvent(new Event('change',{bubbles:true}));}return{ok:true};}"
        + "if(mode==='click'){n.click();return{ok:true};}"
        + "if(mode==='type'){n.focus();n.value="
        + &text_js
        + ";n.dispatchEvent(new Event('input',{bubbles:true}));n.dispatchEvent(new Event('change',{bubbles:true}));return{ok:true};}"
        + "if(mode==='select'){"
        + "if(n.tagName==='SELECT'){"
        + "var opts=[...n.options];var match=opts.find(o=>o.value===" + &text_js + ")||opts.find(o=>o.text.trim()===" + &text_js + ")||opts.find(o=>o.text.trim().toLowerCase()===(" + &text_js + ").toLowerCase())||opts.find(o=>o.value.toLowerCase()===(" + &text_js + ").toLowerCase());"
        + "if(match){n.value=match.value;n.dispatchEvent(new Event('change',{bubbles:true}));n.dispatchEvent(new Event('input',{bubbles:true}));return{ok:true};}"
        + "return{ok:false,reason:'option not found in select'};"
        + "}"
        + "n.value=" + &text_js + ";n.dispatchEvent(new Event('change',{bubbles:true}));n.dispatchEvent(new Event('input',{bubbles:true}));return{ok:true};}"
        + "if(mode==='read'){var txt=n.innerText||n.textContent||'';if(!txt&&('value' in n)&&n.value)txt=n.value;return{ok:true,text:txt.slice(0,5000)};}"
        + "if(mode==='hover'){n.scrollIntoView({block:'center'});const rect=n.getBoundingClientRect();const cx=rect.x+rect.width/2;const cy=rect.y+rect.height/2;const top=doc.elementFromPoint(cx,cy);const isTopmost=top===n||n.contains(top);return{ok:true,box:[Math.round(rect.x+ox)||0,Math.round(rect.y+oy)||0,Math.round(rect.width)||0,Math.round(rect.height)||0],isTopmost:isTopmost};}"
        + "return{ok:false,reason:'unknown mode'};}}"
        + "return{ok:false,reason:'not found'};})("
        + &sig_js
        + ","
        + &mode_js
        + ","
        + &text_js
        + ","
        + &frame_js
        + ")";

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown find error");
        return Err(BladeError::Other(format!("find-by-sig failed: {msg}")));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| BladeError::Other("find-by-sig returned no value".to_string()))?;

    let found: FoundElement = serde_json::from_value(value.clone())?;
    Ok(found)
}

/// Dispatch a human-like mouse move from a random start point to `target`.
/// Returns the final (click) position after bezier path + overshoot correction.
/// Shared by click (adds press/release after) and hover (no click).
async fn dispatch_mouse_move(cdp: &CdpSession, target: (f64, f64)) -> Result<(f64, f64)> {
    let mut rng = crate::stealth::Rng::new();
    // Random start point — offset from the target by 100-300px.
    let angle = rng.uniform() * 2.0 * std::f64::consts::PI;
    let start_dist = 100.0 + rng.uniform() * 200.0;
    let start = (target.0 + angle.cos() * start_dist, target.1 + angle.sin() * start_dist);

    // Generate the bezier path with overshoot + gaussian click offset.
    let path = crate::stealth::mouse_path(start, target, &mut rng);

    // Move along the path, dispatching mouseMoved events with natural delays.
    // 5s timeout: if Chrome is stuck (e.g. new tab loading), don't hang 30s.
    let to = Duration::from_secs(5);
    for pt in &path {
        cdp.send_with_timeout(
            "Input.dispatchMouseEvent",
            Some(json!({"type": "mouseMoved", "x": pt.x, "y": pt.y})),
            to,
        )
        .await?;
        tokio::time::sleep(pt.delay).await;
    }

    // Return the final click point (after overshoot correction).
    Ok(crate::stealth::click_target(&path))
}

/// Dispatch a real mouse click at (x, y) via `Input.dispatchMouseEvent`,
/// with a human-like bezier mouse path from a random start point.
async fn dispatch_mouse_click(cdp: &CdpSession, x: f64, y: f64) -> Result<()> {
    let (cx, cy) = dispatch_mouse_move(cdp, (x, y)).await?;

    let mut rng = crate::stealth::Rng::new();
    let to = Duration::from_secs(5);
    // Press + release at the final position.
    cdp.send_with_timeout(
        "Input.dispatchMouseEvent",
        Some(json!({"type": "mousePressed", "x": cx, "y": cy, "button": "left", "clickCount": 1})),
        to,
    )
    .await?;
    // Small delay between press and release (50-120ms).
    tokio::time::sleep(Duration::from_millis(50 + rng.range(0, 70) as u64)).await;
    cdp.send_with_timeout(
        "Input.dispatchMouseEvent",
        Some(json!({"type": "mouseReleased", "x": cx, "y": cy, "button": "left", "clickCount": 1})),
        to,
    )
    .await?;
    Ok(())
}

/// Dispatch a key press (keyDown + keyUp) via `Input.dispatchKeyEvent`.
async fn dispatch_key(cdp: &CdpSession, key: &str) -> Result<()> {
    let (code, vk) = key_code(key);
    // The `text` field is critical for keys that produce text — without it,
    // the browser fires keydown but doesn't process the default action
    // (e.g. Enter won't submit forms, Space won't scroll/click).
    let text = match key {
        "Enter" => Some("\r"),
        "Tab" => Some("\t"),
        " " => Some(" "),
        _ => None,
    };
    let mut key_down = json!({
        "type": "keyDown",
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": vk,
    });
    if let Some(t) = text {
        key_down["text"] = json!(t);
    }
    let key_up = json!({
        "type": "keyUp",
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": vk,
    });
    cdp.send("Input.dispatchKeyEvent", Some(key_down)).await?;
    cdp.send("Input.dispatchKeyEvent", Some(key_up)).await?;
    Ok(())
}

/// Map a key name to (code, windowsVirtualKeyCode) for CDP.
fn key_code(key: &str) -> (&'static str, u32) {
    match key {
        "Enter" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" => ("Delete", 46),
        "ArrowDown" => ("ArrowDown", 40),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", 39),
        "Home" => ("Home", 36),
        "End" => ("End", 35),
        "PageDown" => ("PageDown", 34),
        "PageUp" => ("PageUp", 33),
        " " => ("Space", 32),
        _ => ("Unidentified", 0),
    }
}

/// Map a printable character to (code, windowsVirtualKeyCode) for CDP key events.
/// These are needed for realistic `Input.dispatchKeyEvent` — without proper
/// `code` and `windowsVirtualKeyCode` fields, some sites ignore the key event
/// or anti-bot systems flag it as synthetic.
fn char_to_key_code(ch: char) -> (String, u32) {
    let code = match ch {
        'a'..='z' => format!("Key{}", ch.to_ascii_uppercase()),
        'A'..='Z' => format!("Key{}", ch),
        '0'..='9' => format!("Digit{}", ch),
        ' ' => "Space".to_string(),
        '.' => "Period".to_string(),
        ',' => "Comma".to_string(),
        '-' => "Minus".to_string(),
        '=' => "Equal".to_string(),
        '/' => "Slash".to_string(),
        '\\' => "Backslash".to_string(),
        ';' => "Semicolon".to_string(),
        '\'' => "Quote".to_string(),
        '`' => "Backquote".to_string(),
        '[' => "BracketLeft".to_string(),
        ']' => "BracketRight".to_string(),
        _ => "Unidentified".to_string(),
    };
    let vk = match ch {
        'a'..='z' | 'A'..='Z' => ch.to_ascii_uppercase() as u32,
        '0'..='9' => ch as u32,
        ' ' => 32,
        '.' => 190,
        ',' => 188,
        '-' => 189,
        '=' => 187,
        '/' => 191,
        '\\' => 220,
        ';' => 186,
        '\'' => 222,
        '`' => 192,
        '[' => 219,
        ']' => 221,
        _ => ch as u32,
    };
    (code, vk)
}

/// Perform an action against the page, then recapture and return the delta.
///
/// This is the core of the `act` tool. It:
/// 1. Resolves the ref → signature from the LPM (if the action targets a ref).
/// 2. Starts a `Page.frameNavigated` listener (in case the action navigates).
/// 3. Dispatches the action via CDP.
/// 4. Waits for settle.
/// 5. Recaptures and returns the delta.
pub async fn perform(
    cdp: &CdpSession,
    lpm: &mut LivePageModel,
    action: &Action,
) -> Result<(PageDelta, String)> {
    perform_with_network(cdp, lpm, action, None).await
}

/// Mutation watcher: installed before an action dispatch so the next
/// capture can report whether the DOM actually changed — including changes
/// to non-actionable content (text swaps, counters, live regions) that the
/// actionable-element delta cannot see. Reset happens inside the capture
/// script after it reads the count. No attributes: class/animation churn
/// would flood the counter on animated pages.
pub const MUT_WATCH: &str = "if(!window.__blade_mo){window.__blade_muts=0;window.__blade_mo=new MutationObserver(function(l){window.__blade_muts+=l.length;});window.__blade_mo.observe(document.documentElement,{childList:true,subtree:true,characterData:true});}window.__blade_muts=0;";

/// Eagerly-subscribed event waiter. `cdp.wait_for` subscribes LAZILY
/// on first poll — events fired between creation and poll (a
/// synchronous alert() during a click dispatch) are silently missed.
/// Create the subscription with `cdp.subscribe()` BEFORE dispatching,
/// then poll it with this.
async fn sub_fires(sub: &mut crate::cdp::SessionSubscription, method: &str) -> bool {
    loop {
        match sub.recv().await {
            Ok(ev) if ev.method == method => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// Perform an action with optional network-aware settle.
pub async fn perform_with_network(
    cdp: &CdpSession,
    lpm: &mut LivePageModel,
    action: &Action,
    in_flight: Option<&AtomicUsize>,
) -> Result<(PageDelta, String)> {
    // Resolve ref → (sig, frame) for ref-targeted actions.
    let sig_frame = match action.ref_id() {
        Some(ref_id) => Some(resolve_ref(lpm, ref_id)?),
        None => None,
    };

    // Start listening for navigation before dispatching. Eager
    // subscribe — a lazy wait_for would miss events fired synchronously
    // during dispatch (see sub_fires).
    let mut nav_sub = cdp.subscribe();

    match action {
        Action::ClickCoord { x, y } => {
            // S10: coordinate-based click — works on cross-origin iframes,
            // canvas, shadow DOM (anything Input-domain can reach).
            let _ = cdp.send("Runtime.evaluate", Some(json!({
                "expression": MUT_WATCH,
            }))).await;
            let mut rng = crate::stealth::biometrics::Rng::new();
            let target = (*x, *y);
            let angle = rng.uniform() * 2.0 * std::f64::consts::PI;
            let start_dist = 100.0 + rng.uniform() * 200.0;
            let start = (target.0 + angle.cos() * start_dist, target.1 + angle.sin() * start_dist);
            let path = crate::stealth::mouse_path(start, target, &mut rng);
            let to = Duration::from_secs(5);
            for point in &path {
                cdp.send_with_timeout("Input.dispatchMouseEvent", Some(json!({
                    "type": "mouseMoved", "x": point.x, "y": point.y,
                })), to).await?;
                tokio::time::sleep(point.delay).await;
            }
            cdp.send_with_timeout("Input.dispatchMouseEvent", Some(json!({
                "type": "mousePressed", "x": target.0, "y": target.1,
                "button": "left", "clickCount": 1,
            })), to).await?;
            tokio::time::sleep(std::time::Duration::from_millis(50 + rng.range(0, 80) as u64)).await;
            cdp.send_with_timeout("Input.dispatchMouseEvent", Some(json!({
                "type": "mouseReleased", "x": target.0, "y": target.1,
                "button": "left", "clickCount": 1,
            })), to).await?;
        }
        Action::Click { ref_id } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            let found = find_by_sig(cdp, sig, frame, "box", None).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            if found.disabled == Some(true) {
                return Err(BladeError::NotInteractable(format!(
                    "{ref_id} is disabled"
                )));
            }

            // M2: Click auto-escalation. Try mouse -> JS -> Enter until effect.
            // Install the mutation watcher first: it sees DOM effects on
            // non-actionable content that the element delta cannot.
            let _ = cdp.send("Runtime.evaluate", Some(json!({
                "expression": MUT_WATCH,
            }))).await;
            let strategies: &[&str] = if found.is_topmost == Some(true) {
                &["mouse", "js", "enter"]
            } else {
                &["js", "mouse", "enter"]
            };
            let mut tried: Vec<&str> = Vec::new();
            let mut via = "";
            let mut delta = PageDelta::default();
            let mut dialog_fired = false;

            for &strategy in strategies {
                tried.push(strategy);
                via = strategy;
                // Eager subscriptions BEFORE dispatch — a click handler
                // calling alert() fires the dialog event SYNCHRONOUSLY
                // during dispatch; a lazy wait_for subscribes on first
                // poll (after dispatch) and misses it, so every strategy
                // would re-fire the handler (3 alerts).
                let mut nav_sub = cdp.subscribe();
                let mut dlg_sub = cdp.subscribe();

                match strategy {
                    "mouse" => {
                        if let Some(box_) = found.box_ {
                            let cx = box_[0] + box_[2] / 2.0;
                            let cy = box_[1] + box_[3] / 2.0;
                            if dispatch_mouse_click(cdp, cx, cy).await.is_err() {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
                    "js" => {
                        if find_by_sig(cdp, sig, frame, "click", None).await.map(|f| f.ok).unwrap_or(false) {
                        } else {
                            continue;
                        }
                    }
                    "enter" => {
                        let _ = find_by_sig(cdp, sig, frame, "focus", None).await;
                        if dispatch_key(cdp, "Enter").await.is_err() {
                            continue;
                        }
                    }
                    _ => {}
                }

                // Dialogs win over the nav wait: an alert() blocks
                // rendering, so settle would stall without the dialog
                // task's auto-dismiss anyway.
                let early = async {
                    tokio::select! {
                        _ = sub_fires(&mut nav_sub, "Page.frameNavigated") => false,
                        _ = sub_fires(&mut dlg_sub, "Page.javascriptDialogOpening") => true,
                    }
                };
                match tokio::time::timeout(Duration::from_millis(500), early).await {
                    Ok(true) => dialog_fired = true,
                    Ok(false) => {
                        crate::page::wait_for_load(cdp, Duration::from_secs(10)).await?;
                    }
                    _ => {}
                }
                wait_for_settle_with_network(cdp, Duration::from_secs(5), in_flight).await?;
                let cap = capture(cdp).await?;
                delta = lpm.ingest(cap);

                if delta.navigated || !delta.is_empty() || delta.content_changed || dialog_fired {
                    break;
                }
            }

            let mut verdict = compute_verdict(action, &delta, lpm, Some((via, &tried)));
            if dialog_fired && !delta.navigated && delta.is_empty() && !delta.content_changed {
                verdict = format!(
                    "outcome: dialog opened via {via} (auto-dismissed — see ambient)"
                );
            }
            return Ok((delta, verdict));
        }
        Action::Type { ref_id, text } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Validate the element is typeable — prevent silently typing
            // into non-text elements (links, buttons, etc.).
            let el = lpm.element(ref_id)
                .ok_or_else(|| BladeError::StaleRef(ref_id.clone()))?;
            if el.raw.role != "textbox" && el.raw.role != "combobox" {
                return Err(BladeError::NotInteractable(format!(
                    "{ref_id} is a {}, not a text input (expected textbox or combobox)",
                    el.raw.role
                )));
            }
            // Focus + clear + get box in one eval — maintains focus state
            // across the prepare→typing transition (fixes Enter-after-type).
            let found = find_by_sig(cdp, sig, frame, "prepare", None).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            // Human-like typing: per-character key events with log-normal cadence.
            let mut rng = crate::stealth::Rng::new();
            let cadence = crate::stealth::typing_cadence(text, &mut rng);
            for (i, ch) in text.chars().enumerate() {
                let ch_str = ch.to_string();
                let (code, vk) = char_to_key_code(ch);
                cdp.send(
                    "Input.dispatchKeyEvent",
                    Some(json!({
                        "type": "keyDown",
                        "key": ch_str,
                        "text": ch_str,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                    })),
                )
                .await?;
                cdp.send(
                    "Input.dispatchKeyEvent",
                    Some(json!({
                        "type": "keyUp",
                        "key": ch_str,
                        "code": code,
                        "windowsVirtualKeyCode": vk,
                    })),
                )
                .await?;
                if i < cadence.len() {
                    tokio::time::sleep(cadence[i]).await;
                }
            }
        }
        Action::Clear { ref_id } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            let found = find_by_sig(cdp, sig, frame, "clear", None).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
        }
        Action::Select { ref_id, option } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Validate the element is a combobox — prevent silently setting
            // value on non-select elements (textareas, inputs).
            let el = lpm.element(ref_id)
                .ok_or_else(|| BladeError::StaleRef(ref_id.clone()))?;
            if el.raw.role != "combobox" {
                return Err(BladeError::NotInteractable(format!(
                    "{ref_id} is a {}, not a dropdown (expected combobox)",
                    el.raw.role
                )));
            }
            let found = find_by_sig(cdp, sig, frame, "select", Some(option)).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!(
                    "{ref_id} ({sig}) — option {option} not found"
                )));
            }
        }
        Action::Press { key } => {
            dispatch_key(cdp, key).await?;
        }
        Action::Scroll { dx, dy } => {
            // S16: eased multi-step scroll via Input.dispatchMouseEvent.
            // Produces trusted wheel events (window.scrollBy is untrusted).
            let total_x = *dx as f64;
            let total_y = *dy as f64;
            let steps = (((total_x.abs() + total_y.abs()) / 80.0).round() as usize)
                .clamp(6, 20);

            // Viewport center for the wheel event position.
            let (cx, cy) = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": "[window.innerWidth/2, window.innerHeight/2]",
                        "returnByValue": true,
                    })),
                )
                .await
                .ok()
                .and_then(|r| {
                    let arr = r.get("result")?.get("value")?.as_array()?;
                    Some((arr[0].as_f64()?, arr[1].as_f64()?))
                })
                .unwrap_or((480.0, 270.0));

            let mut rng = crate::stealth::biometrics::Rng::new();
            for i in 0..steps {
                // Smoothstep ease-in-out: 3t² - 2t³.
                let t0 = i as f64 / steps as f64;
                let t1 = (i + 1) as f64 / steps as f64;
                let e = |t: f64| 3.0 * t * t - 2.0 * t * t * t;
                let step_dx = total_x * (e(t1) - e(t0));
                let step_dy = total_y * (e(t1) - e(t0));

                let _ = cdp
                    .send(
                        "Input.dispatchMouseEvent",
                        Some(json!({
                            "type": "mouseWheel",
                            "x": cx,
                            "y": cy,
                            "deltaX": step_dx,
                            "deltaY": step_dy,
                        })),
                    )
                    .await;

                // 15-30ms jitter between steps.
                let jitter = 15 + rng.range(0, 15) as u64;
                tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
            }
        }
        Action::Read { .. } => {
            // Read is handled by handle_act directly (returns text, not delta).
            // This arm exists for exhaustiveness.
        }
        Action::Wait { condition, text, timeout } => {
            // check_condition polls until the condition is met or timeout.
            // The return value is ignored here — Wait just blocks until done.
            let _ = check_condition(cdp, condition, text, *timeout).await;
        }
        Action::Back => {
            cdp.send("Runtime.evaluate", Some(json!({
                "expression": "window.history.back()",
                "returnByValue": true,
            }))).await?;
        }
        Action::Forward => {
            cdp.send("Runtime.evaluate", Some(json!({
                "expression": "window.history.forward()",
                "returnByValue": true,
            }))).await?;
        }
        Action::Reload => {
            // Page.reload (CDP) is a real reload: bypasses bfcache,
            // re-fetches resources. ignoreCache=false keeps it a
            // normal F5, not a hard reload.
            cdp.send("Page.reload", Some(json!({
                "ignoreCache": false,
            }))).await?;
        }
        Action::Hover { ref_id } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Install the mutation watcher FIRST — hover menus
            // and tooltips mutate the DOM, and we want the delta
            // to reflect what the hover actually revealed.
            let _ = cdp.send("Runtime.evaluate", Some(json!({
                "expression": MUT_WATCH,
            }))).await;
            // "hover" mode scrolls the element into view
            // (block:center) and returns its post-scroll box.
            let found = find_by_sig(cdp, sig, frame, "hover", None).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            let box_ = found.box_.ok_or_else(|| {
                BladeError::Other(format!("element {ref_id} has no bounding box"))
            })?;
            let cx = box_[0] + box_[2] / 2.0;
            let cy = box_[1] + box_[3] / 2.0;
            // Bezier mouse move to element center — no click.
            // The human-like path triggers CSS :hover and JS
            // mouseover/mouseenter. On dispatch failure (page
            // navigating mid-hover), re-resolve and retry once.
            if dispatch_mouse_move(cdp, (cx, cy)).await.is_err() {
                let retry = find_by_sig(cdp, sig, frame, "hover", None).await?;
                if retry.ok {
                    if let Some(b2) = retry.box_ {
                        dispatch_mouse_move(cdp, (b2[0] + b2[2] / 2.0, b2[1] + b2[3] / 2.0)).await?;
                    }
                }
            }
            // Hover-driven dropdowns appear within ~300ms — give
            // the mutation watcher a beat to catch them before
            // the settle wait decides nothing changed.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        Action::Upload { ref_id, path } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Validate the element is a file input.
            let el = lpm.element(ref_id)
                .ok_or_else(|| BladeError::StaleRef(ref_id.clone()))?;
            if el.raw.role != "file" {
                return Err(BladeError::NotInteractable(format!(
                    "{ref_id} is a {}, not a file input (expected role 'file')",
                    el.raw.role
                )));
            }
            // Find the element in the DOM to get its backend nodeId.
            // We use a small script to locate it by sig and return its nodeId
            // via a temporary marker, then use DOM.setFileInputFiles.
            //
            // CDP's DOM.setFileInputFiles needs a nodeId. We can get it by
            // describing the node tree from the document root and searching
            // for our element. But simpler: use DOM.requestNode on the element
            // found via a JS evaluation that returns the element, then use
            // DOM.requestNode to get its nodeId.
            //
            // Actually, the cleanest approach: Runtime.evaluate returns
            // an objectId for a returned element, then DOM.requestNode
            // converts objectId → nodeId, then DOM.setFileInputFiles sets
            // the file.
            let found = find_by_sig(cdp, sig, frame, "box", None).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            // Get the objectId of the file input element.
            let sig_js = serde_json::to_string(sig)?;
            let frame_js = serde_json::to_string(frame)?;
            let expr = "((sig,frame)=>{".to_string()
                + "const d=document;if(!d||!d.body)return null;"
                + &JS_PREAMBLE
                + "let doc=d;for(let i=0;i<frame.length;i++){const ifs=doc.querySelectorAll('iframe');const f=ifs[frame[i]];if(!f)return null;try{doc=f.contentDocument;if(!doc)return null;}catch(e){return null;}}"
                + "const all=deepAll(doc,sel);const fps=frame.join(',');const counts={};"
                + "for(let i=0;i<all.length;i++){const n=all[i];const r=role(n);if(r==='hidden')continue;const nm=name(n,false);const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;const s=fps+'|'+r+'|'+nm+'|'+counts[key];if(s===sig)return vis(n)?n:null;}return null;})("
                + &sig_js
                + ","
                + &frame_js
                + ")";
            let res = cdp.send("Runtime.evaluate", Some(serde_json::json!({
                "expression": expr,
                "returnByValue": false,
            }))).await?;
            let object_id = res.get("result").and_then(|r| r.get("objectId")).and_then(|o| o.as_str());
            let object_id = match object_id {
                Some(id) => id.to_string(),
                None => return Err(BladeError::Other(format!("could not get objectId for file input {ref_id}"))),
            };
            // Set the file on the input. DOM.setFileInputFiles accepts
            // objectId directly — no need to convert to nodeId (which can
            // go stale if the DOM tree hasn't been explicitly requested).
            cdp.send("DOM.setFileInputFiles", Some(serde_json::json!({
                "objectId": object_id,
                "files": [path],
            }))).await?;
        }
    }

    // Check if navigation was triggered (with a short timeout).
    let nav_result = tokio::time::timeout(
        Duration::from_millis(500),
        sub_fires(&mut nav_sub, "Page.frameNavigated"),
    ).await;
    if let Ok(true) = nav_result {
        crate::page::wait_for_load(cdp, Duration::from_secs(10)).await?;
    }
    wait_for_settle_with_network(cdp, Duration::from_secs(5), in_flight).await?;

    // Recapture → delta.
    let cap = capture(cdp).await?;
    let delta = lpm.ingest(cap);
    let verdict = compute_verdict(action, &delta, lpm, None);
    Ok((delta, verdict))
}

