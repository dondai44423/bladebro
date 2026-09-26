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
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use crate::stealth::biometrics::gaussian;
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

    /// True for actions that dispatch synthetic input events (or file
    /// uploads) — refused while manual control is claimed (`rb pause`), so
    /// the agent never fights the person using the browser.
    pub fn injects_input(&self) -> bool {
        matches!(
            self,
            Action::Click { .. }
                | Action::ClickCoord { .. }
                | Action::Type { .. }
                | Action::Clear { .. }
                | Action::Select { .. }
                | Action::Press { .. }
                | Action::Scroll { .. }
                | Action::Hover { .. }
                | Action::Upload { .. }
        )
    }

    /// True for actions the pause must refuse: input dispatch plus the
    /// page-level moves that would yank the user's context out from under
    /// them (history, reload). Reads, waits and eval stay available.
    pub fn disrupts_page(&self) -> bool {
        self.injects_input() || matches!(self, Action::Back | Action::Forward | Action::Reload)
    }
}

/// Result of the find-by-sig script: the element's current box + metadata.
#[derive(Debug, Deserialize)]
struct FoundElement {
    ok: bool,
    #[serde(default)]
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
    /// Short accessible description (role "name") of the resolved click
    /// target. When leaf-targeting redirects a container-center click to
    /// the nearest exposed native control, this names the control actually
    /// clicked, so `no-effect` verdicts expose the real target.
    #[serde(default, rename = "hit_tgt")]
    hit_tgt: Option<String>,
    /// When the element is NOT topmost at its click point: a description of
    /// the element that actually receives clicks there (or "outside the
    /// viewport"). Turns a silent no-op into "clicks land on X" - the
    /// occlusion diagnostic.
    #[serde(default, rename = "top_desc")]
    top_desc: Option<String>,
    /// Text content of the element (for "read" mode). For "check"/"prepare"
    /// mode this is the readback of the EFFECTIVE editing host - the focused
    /// editor when the framework moved focus away from the addressed wrapper
    /// (a facade composer's real contenteditable), else the element itself.
    #[serde(default)]
    text: Option<String>,
    /// "check"/"prepare": kind of the effective editing host
    /// ("ce" = contenteditable, "input", "textarea", ...).
    #[serde(default, rename = "hostKind")]
    host_kind: Option<String>,
    /// "check"/"prepare": the effective host is the addressed element.
    #[serde(default, rename = "hostIsTgt")]
    host_is_tgt: Option<bool>,
    /// "check"/"selhost": selected character count in the host.
    #[serde(default)]
    sel: Option<u64>,
    /// "check"/"prepare": the addressed element is gone (framework
    /// remounted it); `text`/`hostKind` still describe the live editor.
    #[serde(default, rename = "tgtMissing")]
    tgt_missing: Option<bool>,
    /// Live options of a select whose pick failed (#21): `text` or
    /// `text=value` tokens, capped at 80.
    #[serde(default)]
    options: Option<Vec<String>>,
    /// Total option count on the live element at failure time.
    #[serde(default, rename = "ototal")]
    options_total: Option<usize>,
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
    /// Selector addressing: the match exists but fails the visibility gate.
    /// Hidden matches are never addressed directly (a mouse cannot reach
    /// them), but they are returned so callers can say WHY a selector
    /// missed instead of reporting a bare "not found".
    #[serde(default)]
    pub hidden: bool,
    /// Why the match is hidden: `display:none (ancestor ...)`,
    /// `visibility:hidden`, `zero-size`, `not visible`.
    #[serde(default)]
    pub reason: String,
}

/// Build the find-by-text page script. `include_hidden` keeps invisible
/// matches in the results - the ref HEAL path needs them (a facade
/// composer's hidden wrapper resolves to its live editor downstream),
/// while text addressing and `see find` stay visible-only.
fn find_text_expr(query: &str, role_filter: Option<&str>, include_hidden: bool) -> Result<String> {
    let query_js = serde_json::to_string(query)?;
    let role_js = match role_filter {
        Some(r) => serde_json::to_string(r)?,
        None => "null".to_string(),
    };
    let ih_js = if include_hidden { "true" } else { "false" };

    Ok("((query,rf,ih)=>{"
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
        + "if(!vis(n)&&!ih)continue;"
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
        + "results.sort((a,b)=>b.score-a.score);return results.slice(0,30);})"
        + "(" + &query_js + "," + &role_js + "," + ih_js + ")")
}

/// Find actionable elements by text/label query. Returns up to 30 matches
/// sorted by relevance score. Used by text addressing (M3): `act click text="Sign in"`
/// resolves the text to an element ref without a prior `see` call.
pub async fn find_by_text(
    cdp: &CdpSession,
    query: &str,
    role_filter: Option<&str>,
    include_hidden: bool,
) -> Result<Vec<TextMatch>> {
    if query.trim().is_empty() {
        return Err(BladeError::Other("find query must not be empty".into()));
    }
    let expression = find_text_expr(query, role_filter, include_hidden)?;

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

/// Build the find-by-selector page script. The selector is matched with
/// `Element.matches()` against every ACTIONABLE element (the same deepAll
/// walk the capture uses, so open shadow roots are searched), and the
/// count/rank math is identical to the capture script - returned sigs are
/// directly adoptable into the model. Hidden matches come back flagged with
/// a reason instead of being dropped: a selector miss must be able to say
/// it found the element but could not reach it.
fn find_selector_expr(selector: &str) -> Result<String> {
    let sel_js = serde_json::to_string(selector)?;
    Ok("((sel_user)=>{ "
        .to_string()
        + "const d=document;if(!d||!d.body)return[];"
        + &JS_PREAMBLE
        + "const _vrs=function(n){let e=n;for(let i=0;i<14&&e;i++){try{const cs=getComputedStyle(e);if(cs.display==='none')return 'display:none'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');if(cs.visibility==='hidden')return 'visibility:hidden'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');}catch(_e){}e=e.parentElement||(e.getRootNode&&e.getRootNode().host);}const r=n.getBoundingClientRect();if(!r.width||!r.height)return 'zero-size';return 'not visible';};"
        + "const all=deepAll(d,sel);const results=[];const counts={};"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const snm=name(n,false);"
        + "const key=r+'\\u0000'+snm;counts[key]=(counts[key]||0)+1;"
        + "let m=false;try{m=!!(n.matches&&n.matches(sel_user));}catch(_e){m=false;}"
        + "if(!m)continue;"
        + "const v=vis(n);"
        + "results.push({sig:'|'+r+'|'+snm+'|'+counts[key],score:0,role:r,name:name(n,true),frame:[],hidden:!v,reason:v?'':_vrs(n)});"
        + "}"
        + "return results.slice(0,30);})("
        + &sel_js
        + ")")
}

/// Resolve a CSS selector to actionable elements (light DOM + open shadow
/// roots). Hidden matches are included (flagged) so callers can explain a
/// miss instead of reporting a bare "not found" - the reddit comment-menu
/// case, where the trigger exists but is desktop-hidden.
pub async fn find_by_selector(cdp: &CdpSession, selector: &str) -> Result<Vec<TextMatch>> {
    if selector.trim().is_empty() {
        return Err(BladeError::Other("selector must not be empty".into()));
    }
    let expression = find_selector_expr(selector)?;
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
            .unwrap_or("unknown selector-match error");
        return Err(BladeError::Other(format!("selector match failed: {msg}")));
    }
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| BladeError::Other("selector match returned no value".to_string()))?;
    let matches: Vec<TextMatch> = serde_json::from_value(value.clone())?;
    Ok(matches)
}

/// One hidden-match example from the find-miss diagnostic.
#[derive(Debug, Clone, Deserialize)]
pub struct MissExample {
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub shadow: bool,
}

/// What a find-by-text miss left on the table: matches that exist but were
/// not returned (invisible), with reasons, and how many live in shadow
/// roots. Lets the miss message explain itself instead of reading as
/// "nothing here".
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MissDiag {
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub hidden: usize,
    #[serde(default)]
    pub shadow: usize,
    #[serde(default)]
    pub examples: Vec<MissExample>,
}

/// Build the find-miss diagnostic script: the same name matching as
/// find_by_text, run over deepAll (open shadow roots included), counting
/// invisible matches with a reason for each.
fn find_miss_expr(query: &str) -> Result<String> {
    let q_js = serde_json::to_string(query)?;
    Ok("((query)=>{ "
        .to_string()
        + "const d=document;if(!d||!d.body)return{total:0,hidden:0,shadow:0,examples:[]};"
        + &JS_PREAMBLE
        + "const _vrs=function(n){let e=n;for(let i=0;i<14&&e;i++){try{const cs=getComputedStyle(e);if(cs.display==='none')return 'display:none'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');if(cs.visibility==='hidden')return 'visibility:hidden'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');}catch(_e){}e=e.parentElement||(e.getRootNode&&e.getRootNode().host);}const r=n.getBoundingClientRect();if(!r.width||!r.height)return 'zero-size';return 'not visible';};"
        + "const q=query.toLowerCase();const all=deepAll(d,sel);"
        + "let total=0;let hidden=0;let shadow=0;const examples=[];"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const nm=name(n,true);if(!nm)continue;"
        + "const nh=nm.toLowerCase();const al=(n.getAttribute('aria-label')||'').toLowerCase();const ph=(n.placeholder||'').toLowerCase();const ti=(n.title||'').toLowerCase();"
        + "if(!(nh.includes(q)||al.includes(q)||ph.includes(q)||ti.includes(q)))continue;"
        + "total++;const sh=n.getRootNode()!==d;"
        + "if(sh)shadow++;"
        + "if(!vis(n)){hidden++;if(examples.length<3)examples.push({role:r,name:nm,reason:_vrs(n),shadow:sh});}"
        + "}"
        + "return{total:total,hidden:hidden,shadow:shadow,examples:examples};})("
        + &q_js
        + ")")
}

/// Run [`find_miss_expr`] against the page. Returns zeroed diagnostics on
/// any transport hiccup - this is a best-effort explainer for a miss, never
/// a new failure of its own.
pub async fn find_miss_diag(cdp: &CdpSession, query: &str) -> Result<MissDiag> {
    if query.trim().is_empty() {
        return Ok(MissDiag::default());
    }
    let expression = find_miss_expr(query)?;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or_default();
    if !value.is_object() {
        return Ok(MissDiag::default());
    }
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Build the coordinate hit-probe script: what element actually sits at
/// viewport (x, y) - descending open shadow roots, since
/// `document.elementFromPoint` returns the shadow HOST.
fn hit_probe_expr(x: f64, y: f64) -> String {
    "((x,y)=>{const d=document;if(!d)return null;const vw=window.innerWidth||0;const vh=window.innerHeight||0;let el=d.elementFromPoint(x,y);if(!el)return (x<0||y<0||x>vw||y>vh)?('outside the viewport ('+Math.round(vw)+'x'+Math.round(vh)+')'):'nothing at that point';while(el&&el.shadowRoot){let inner=null;try{inner=el.shadowRoot.elementFromPoint(x,y);}catch(_e){}if(!inner||inner===el)break;el=inner;}const t=el.tagName.toLowerCase();const idd=el.id?('#'+el.id):'';const cl=(typeof el.className==='string'&&el.className.trim())?('.'+el.className.trim().split(/\\s+/).slice(0,2).join('.')):'';const ala=(el.getAttribute&&(el.getAttribute('aria-label')||el.getAttribute('title')))||'';const tx=(el.textContent||'').replace(/\\s+/g,' ').trim().slice(0,30);let desc=t+idd+cl+(ala?(' ['+ala+']'):(tx?(' \"'+tx+'\"'):''));try{const cs=getComputedStyle(el);if(cs.pointerEvents==='none')desc+=' (pointer-events:none)';}catch(_e){}return desc;})"
        .to_string()
        + &format!("({x},{y})")
}

/// Describe the topmost element at viewport (x, y) - the diagnostic a
/// coordinate click owes when it does nothing: WHAT is actually there.
pub async fn hit_probe(cdp: &CdpSession, x: f64, y: f64) -> Result<Option<String>> {
    let expression = hit_probe_expr(x, y);
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;
    Ok(res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from))
}

/// Summarize a delta's DOM evidence. Node changes read as `+2 \u{2212}1`
/// (strong evidence a handler ran). State-only changes (surviving elements
/// whose value/checked/disabled flipped) read as `state-only (...)` - they
/// are attribute-level and can be focus/hover paint, so a verdict must not
/// sell them as a real effect. None = nothing observable changed.
fn dom_effect_summary(delta: &PageDelta) -> Option<String> {
    if !delta.added.is_empty() || !delta.removed.is_empty() {
        Some(format!("+{} \u{2212}{}", delta.added.len(), delta.removed.len()))
    } else if !delta.changed.is_empty() {
        let refs: Vec<&str> = delta.changed.iter().take(3).map(|(r, _)| r.as_str()).collect();
        let more = if delta.changed.len() > 3 { ", ..." } else { "" };
        Some(format!(
            "state-only: {} ref{} ({}{}) - no nodes added/removed",
            delta.changed.len(),
            if delta.changed.len() == 1 { "" } else { "s" },
            refs.join(", "),
            more
        ))
    } else {
        None
    }
}

/// Compute a one-line outcome verdict from the action + delta + click info.
/// This is the M1 verdict — every act tells the agent what happened.
fn compute_verdict(
    action: &Action,
    delta: &PageDelta,
    lpm: &LivePageModel,
    click_via: Option<(&str, &[&str], &str)>,
    edit: Option<&EditReport>,
    coord_hit: Option<&str>,
) -> String {
    match action {
        Action::ClickCoord { x, y } => {
            if delta.navigated {
                format!("outcome: navigated \u{2192} {} via coord-click({x:.0},{y:.0})", shorten_url(&delta.url))
            } else if let Some(eff) = dom_effect_summary(delta) {
                format!("outcome: dom-changed ({eff}) via coord-click({x:.0},{y:.0})")
            } else if delta.content_changed {
                format!("outcome: dom-changed (content-only) via coord-click({x:.0},{y:.0})")
            } else {
                match coord_hit {
                    Some(h) => format!(
                        "outcome: no-effect (coord-click at {x:.0},{y:.0} - page did not respond; topmost there: {h})"
                    ),
                    None => format!(
                        "outcome: no-effect (coord-click at {x:.0},{y:.0} - page did not respond)"
                    ),
                }
            }
        }
        Action::Click { .. } => {
            let (via, tried, tgt_meta) = click_via.unwrap_or(("", &[][..], ""));
            if delta.navigated {
                format!("outcome: navigated \u{2192} {} via {}", shorten_url(&delta.url), via)
            } else if let Some(eff) = dom_effect_summary(delta) {
                format!("outcome: dom-changed ({eff}) via {via}")
            } else if delta.content_changed {
                // Mutation watcher saw DOM effects on non-actionable
                // content - text swaps, counters, live regions.
                format!("outcome: dom-changed (content-only) via {via}")
            } else if tgt_meta.is_empty() {
                no_effect_verdict(tried, "")
            } else {
                // The resolved click target is named so consumers can tell a
                // wrong-target / avenue problem from a page that rejected a
                // well-aimed click.
                no_effect_verdict(tried, tgt_meta)
            }
        }
        Action::Type { ref_id, text } => {
            if let Some(rep) = edit {
                return type_verdict_text(ref_id, text, rep, lpm);
            }
            let actual = lpm
                .element(ref_id)
                .and_then(|e| e.raw.value.as_deref())
                .unwrap_or("");
            if actual == text {
                format!("outcome: typed \u{2192} value=\"{}\"", clip(text, 40))
            } else if !actual.is_empty() {
                format!(
                    "outcome: typed \"{}\" \u{2192} value=\"{}\" (incomplete - framework may mask input)",
                    clip(text, 40), clip(actual, 40)
                )
            } else {
                format!("outcome: typed \"{}\" \u{2192} value empty (input may be framework-controlled)", clip(text, 40))
            }
        }
        Action::Clear { ref_id } => match edit {
            Some(rep) => clear_verdict_text(ref_id, rep),
            None => format!("outcome: cleared {ref_id}"),
        },
        Action::Select { option, .. } => format!("outcome: selected \"{}\"", clip(option, 40)),
        Action::Press { key } => {
            if delta.navigated {
                format!("outcome: pressed {key} \u{2192} navigated \u{2192} {}", shorten_url(&delta.url))
            } else if let Some(eff) = dom_effect_summary(delta) {
                format!("outcome: pressed {key} \u{2192} dom-changed ({eff})")
            } else if delta.content_changed {
                format!("outcome: pressed {key} \u{2192} dom-changed (content-only)")
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
            if delta.navigated {
                format!("outcome: hovered {ref_id} \u{2192} navigated \u{2192} {}", shorten_url(&delta.url))
            } else if let Some(eff) = dom_effect_summary(delta) {
                format!("outcome: hovered {ref_id} \u{2192} dom-changed ({eff})")
            } else if delta.content_changed {
                format!("outcome: hovered {ref_id} \u{2192} dom-changed (content-only)")
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
        // Char-safe: page URLs can contain raw multi-byte UTF-8 and byte
        // slicing panics mid-char (crash-by-URL).
        format!("{}\u{2026}", crate::platform::truncate_utf8(s, 77))
    } else {
        s.to_string()
    }
}

/// Confirmed-absence window for `if`/`while` condition checks (v3.10). Once
/// the condition has been false this long AND the page has no requests in
/// flight, the check concludes "not present" instead of polling to the full
/// timeout — a false `if` guard or a finished `while` loop exits in ~0.8s.
const ABSENCE_CONFIRM: Duration = Duration::from_millis(800);

/// See [`ABSENCE_CONFIRM`]. `probe = None` disables the fast path (used by
/// the `wait` action, whose timeout is a deliberate wait budget).
fn absence_confirmed(
    absent_since: &mut Option<std::time::Instant>,
    probe: Option<&std::sync::atomic::AtomicUsize>,
) -> bool {
    let Some(counter) = probe else {
        return false;
    };
    let since = *absent_since.get_or_insert_with(std::time::Instant::now);
    since.elapsed() >= ABSENCE_CONFIRM && counter.load(std::sync::atomic::Ordering::Relaxed) == 0
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
    absence_probe: Option<&std::sync::atomic::AtomicUsize>,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut absent_since: Option<std::time::Instant> = None;
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
                if matches!(res, Err(BladeError::Closed)) {
                    return false; // browser died — bail instead of spinning the full timeout
                }
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
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "element" => {
            let needle_js = serde_json::to_string(&text.to_lowercase())
                .unwrap_or_else(|_| "\"\"".to_string());
            let check = format!(
                "(()=>{{const d=document;if(!d||!d.body)return false;const sel='{selector}';const all=[...d.querySelectorAll(sel)];const vis=n=>{{const r=n.getBoundingClientRect();if(r.width===0||r.height===0)return false;const s=getComputedStyle(n);if(s.display==='none'||s.visibility==='hidden'||s.opacity==='0')return false;return true;}};const nodes=all.filter(vis);const t={needle_js}.toLowerCase();return nodes.some(n=>{{const role=(n.getAttribute('role')||n.tagName.toLowerCase());const name=(n.getAttribute('aria-label')||n.textContent||n.placeholder||'').trim();return role.toLowerCase().includes(t)||name.toLowerCase().includes(t);}});}})()",
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
                if matches!(res, Err(BladeError::Closed)) {
                    return false;
                }
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
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "url" => {
            let needle = text.to_lowercase();
            loop {
                let res = cdp
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": "location.href",
                            "returnByValue": true,
                        })),
                    )
                    .await;
                if matches!(res, Err(BladeError::Closed)) {
                    return false;
                }
                let url = res
                    .ok()
                    .and_then(|r| {
                        r.get("result")
                            .and_then(|r| r.get("value"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_default();
                if url.to_lowercase().contains(&needle) {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "text" => {
            let needle_js = serde_json::to_string(&text.to_lowercase())
                .unwrap_or_else(|_| "\"\"".to_string());
            let expr = format!(
                "(document.body&&document.body.innerText||'').toLowerCase().includes({needle_js})"
            );
            loop {
                let res = cdp
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": &expr,
                            "returnByValue": true,
                        })),
                    )
                    .await;
                if matches!(res, Err(BladeError::Closed)) {
                    return false;
                }
                let found = res
                    .ok()
                    .and_then(|r| {
                        r.get("result")
                            .and_then(|r| r.get("value"))
                            .and_then(|v| v.as_bool())
                    })
                    .unwrap_or(false);
                if found {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "js" => {
            // Evaluate the user's JS expression and check truthiness.
            loop {
                let res = cdp
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": text,
                            "returnByValue": true,
                            "awaitPromise": true,
                        })),
                    )
                    .await;
                if matches!(res, Err(BladeError::Closed)) {
                    return false;
                }
                let truthy = res
                    .ok()
                    .and_then(|r| {
                        r.get("result")
                            .and_then(|r| r.get("value"))
                            .map(|v| match v {
                                serde_json::Value::Bool(b) => *b,
                                serde_json::Value::Null => false,
                                serde_json::Value::Number(n) => {
                                    n.as_f64().map(|f| f != 0.0).unwrap_or(false)
                                }
                                serde_json::Value::String(s) => !s.is_empty(),
                                _ => true,
                            })
                    })
                    .unwrap_or(false);
                if truthy {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "settle" | "network" => {
            let _ = wait_for_settle(cdp, timeout).await;
            true
        }
        _ => {
            // Unknown condition name — return false so the agent sees
            // the condition wasn't met instead of a false positive.
            false
        }
    }
}

/// Box-mode leaf-targeting: a text-resolved match is often a container (nav,
/// row, role=button wrapper) whose geometric center is empty. If the center
/// point does not land on a native control, find the nearest exposed native
/// control inside the match and click there instead. CL3 fix: the YouTube
/// account-menu / nav-wrapper no-effect bug (#15).
const LEAF_TARGET_JS: &str = concat!(
    "var _lClick=_lbx(n);var _lht=(n.getAttribute&&(n.getAttribute('aria-label')||n.getAttribute('title')))||'';var _lhr=(n.getAttribute&&n.getAttribute('role'))||n.tagName.toLowerCase();",
    "var _ltopN=doc.elementFromPoint(cx,cy);var _lNative=_lnb(_ltopN);",
    "if(!_lNative){var _lbest=null,_lbd=Infinity,_lcands=[];try{_lcands=n.querySelectorAll('button,a[href],input:not([type=hidden]),select,textarea');}catch(e){}",
    "for(var _lk=0;_lk<_lcands.length;_lk++){var _lc=_lcands[_lk];var _lrc=_lc.getBoundingClientRect();if(_lrc.width<2||_lrc.height<2)continue;var _lbn=n.getBoundingClientRect();if(!(_lrc.left<=_lbn.right&&_lrc.right>=_lbn.left))continue;var _lccx=_lrc.x+_lrc.width/2,_lccy=_lrc.y+_lrc.height/2;var _ltc=doc.elementFromPoint(_lccx,_lccy);if(!(_ltc===_lc||(_lc.contains&&_lc.contains(_ltc))))continue;var _ldd=Math.hypot(_lccx-cx,_lccy-cy);if(_ldd<_lbd){_lbd=_ldd;_lbest=_lc;}}",
    "if(_lbest){_lClick=_lbx(_lbest);_lht=(_lbest.getAttribute&&(_lbest.getAttribute('aria-label')||_lbest.textContent||_lbest.getAttribute('title')))||'';_lhr=(_lbest.getAttribute&&_lbest.getAttribute('role'))||_lbest.tagName.toLowerCase();}}",
);

/// Build the verdict string for a no-effect click that names the resolved
/// click target, so consumers can tell a wrong-target/avenue problem from a
/// page that rejected a well-aimed click. (CL3, #15.) Says what was tried
/// and that dispatch DID happen - a plausible-looking success string on a
/// silent no-op was the original sin here.
fn no_effect_verdict(tried: &[&str], target_meta: &str) -> String {
    if target_meta.is_empty() {
        format!(
            "outcome: no-effect (click dispatched via {} - no navigation, no DOM or state change; the element may be disabled, hidden, or hover-gated)",
            tried.join(", ")
        )
    } else {
        format!(
            "outcome: no-effect (click dispatched via {} on {} - no navigation, no DOM or state change)",
            tried.join(", "),
            target_meta
        )
    }
}

/// Build the find-by-sig page script: re-locates an element by signature in
/// the live DOM and returns its box (`box`), performs an in-page action
/// (`prepare`/`focus`/`clear`/`type`/`select`/`click`), or reads it
/// (`check`/`read`/`hover`). Separate from `find_by_sig` so tests can
/// syntax-check it without a live browser.
fn find_sig_expr(sig: &str, mode: &str, text: Option<&str>, frame: &[usize]) -> Result<String> {
    let sig_js = serde_json::to_string(sig)?;
    let mode_js = serde_json::to_string(mode)?;
    let text_js = match text {
        Some(t) => serde_json::to_string(t)?,
        None => "null".to_string(),
    };
    let frame_js = serde_json::to_string(frame)?;
    Ok("((sig,mode,text,frame)=>{".to_string()
        + "const d=document;if(!d||!d.body)return null;"
        + &JS_PREAMBLE
        + "let doc=d;let ox=0,oy=0;"
        + "for(let i=0;i<frame.length;i++){"
        + "const ifs=doc.querySelectorAll('iframe');const f=ifs[frame[i]];"
        + "if(!f)return{ok:false,reason:'frame gone'};"
        + "try{doc=f.contentDocument;if(!doc)return{ok:false,reason:'frame inaccessible'};}catch(e){return{ok:false,reason:'cross-origin'};}"
        + "const ir=f.getBoundingClientRect();ox+=ir.x;oy+=ir.y;}"
        + "const all=deepAll(doc,sel);"
        + "const fps=frame.join(',');const counts={};"
        // Editing-host helpers, shared by prepare/clear/check/selhost/type:
        // every input path resolves the EFFECTIVE editing host first - the
        // focused editor when a framework moved focus (facade composers),
        // else the addressed element or its inner editable field.
        + "var _ced=function(e){return !!(e&&(e.isContentEditable||(e.getAttribute&&e.getAttribute('contenteditable')==='true')));};"
        + "var _clr=function(e){try{var _s=window.getSelection();_s.selectAllChildren(e);document.execCommand('delete',false);}catch(_e){}e.dispatchEvent(new Event('input',{bubbles:true}));};"
        + "var _tvr=function(v){return String(v==null?'':v).replace(/\\s+/g,' ').trim();};"
        + "var _read=function(e){if(!e)return '';if('value' in e&&typeof e.value==='string')return _tvr(e.value);return _tvr(e.innerText||e.textContent||'');};"
        + "var _isd=function(e){return !!(e&&(_ced(e)||((e.tagName==='INPUT'||e.tagName==='TEXTAREA')&&e.type!=='hidden')));};"
        + "var _hostOf=function(t){var a=doc.activeElement;if(_isd(a))return a;if(t&&t.querySelector){var ii=_ced(t)?null:t.querySelector('textarea,input:not([type=hidden]),[contenteditable]:not([contenteditable=false])');if(ii)return ii;}return t;};"
        + "var _selLen=function(h){try{if(h&&('selectionStart' in h)&&'value' in h)return Math.abs((h.selectionEnd||0)-(h.selectionStart||0));var s=doc.getSelection&&doc.getSelection();return (s&&!s.isCollapsed)?String(s).length:0;}catch(_e){return 0;}};"
        + "var _nearish=function(n,a){if(!n||!a)return false;if(n.contains(a)||a.contains(n))return true;try{var f1=n.closest?n.closest('form'):null;if(f1&&f1===(a.closest?a.closest('form'):null))return true;}catch(_e){}var p=n.parentElement;for(var i=0;i<4&&p;i++){if(p.contains(a))return true;p=p.parentElement;}return false;};"
        + "var _hiddenHost=function(n){var a=doc.activeElement;if(a&&a!==n&&_isd(a)&&vis(a)&&_nearish(n,a))return a;var p=n.parentElement;for(var i=0;i<3&&p;i++){var q=p.querySelectorAll('[contenteditable]:not([contenteditable=false]),textarea,input:not([type=hidden])');for(var j=0;j<q.length;j++){var r=q[j];if(r!==n&&_isd(r)&&vis(r))return r;}p=p.parentElement;}return null;};"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const nm=name(n,false);"
        + "const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;"
        + "const s=fps+'|'+r+'|'+nm+'|'+counts[key];"
        + "if(s==="
        + &sig_js
        + "){if(!vis(n)){var _vd=(mode==='check'||mode==='prepare'||mode==='focus')?_hiddenHost(n):null;if(_vd){if(mode!=='check'){_vd.scrollIntoView({block:'center'});_vd.focus();}if(mode==='focus')return{ok:true};return{ok:true,text:_read(_vd),hostKind:(_ced(_vd)?'ce':((_vd.tagName)?_vd.tagName.toLowerCase():'')),hostIsTgt:false,sel:_selLen(_vd),tgtMissing:true};}return{ok:false,reason:'element hidden'};}const rect=n.getBoundingClientRect();"
        + "const cx=rect.x+rect.width/2;const cy=rect.y+rect.height/2;"
        + "const top=doc.elementFromPoint(cx,cy);"
        + "const isTopmost=top===n||n.contains(top);"
        + "var tgt=n;if(n.getAttribute&&n.getAttribute('role')==='combobox'&&n.tagName!=='SELECT'){var ii=n.querySelector('textarea,input:not([type=hidden])');if(ii)tgt=ii;}"
        + "if(mode==='box'){"
        + "function _lnb(e){return !!(e&&e.tagName&&(e.tagName==='BUTTON'||e.tagName==='A'||e.tagName==='SELECT'||e.tagName==='TEXTAREA'||(e.tagName==='INPUT'&&e.type!=='hidden'))&&(e.tagName!=='A'||!!e.href));}"
        + "function _lbx(e){var _lr=e.getBoundingClientRect();return [Math.round(_lr.x+ox)||0,Math.round(_lr.y+oy)||0,Math.round(_lr.width)||0,Math.round(_lr.height)||0];}"
        + LEAF_TARGET_JS
        + "var _lcbx=_lClick[0]+_lClick[2]/2,_lcby=_lClick[1]+_lClick[3]/2;var _lfc=doc.elementFromPoint(_lcbx-ox,_lcby-oy);"
        + "var _desc=function(e){if(!e)return null;var t=e.tagName.toLowerCase();var idd=e.id?('#'+e.id):'';var cl=(typeof e.className==='string'&&e.className.trim())?('.'+e.className.trim().split(/\\s+/).slice(0,2).join('.')):'';var ala=(e.getAttribute&&(e.getAttribute('aria-label')||e.getAttribute('title')))||'';var tx=(e.textContent||'').replace(/\\s+/g,' ').trim().slice(0,30);return t+idd+cl+(ala?(' ['+ala+']'):(tx?(' \"'+tx+'\"'):''));};"
        + "var _ltop=(_lfc===n||(n.contains&&_lfc&&n.contains(_lfc)))?null:(_lfc?_desc(_lfc):'outside the viewport');"
        + "return{ok:true,box:_lClick,tag:n.tagName.toLowerCase(),type:n.type||null,disabled:!!n.disabled,isTopmost:(_lfc===n||(n.contains?n.contains(_lfc):false)),hit_tgt:(_lhr+' ['+String(_lht).slice(0,60)+']'),top_desc:_ltop};}"
        + "if(mode==='prepare'){tgt.scrollIntoView({block:'center'});tgt.focus();var _ph=_hostOf(tgt);const r3=tgt.getBoundingClientRect();return{ok:true,box:[Math.round(r3.x+ox)||0,Math.round(r3.y+oy)||0,Math.round(r3.width)||0,Math.round(r3.height)||0],disabled:!!tgt.disabled,text:_read(_ph),hostKind:(_ced(_ph)?'ce':((_ph&&_ph.tagName)?_ph.tagName.toLowerCase():'')),hostIsTgt:_ph===tgt,sel:_selLen(_ph),tgtMissing:false};}"
        + "if(mode==='focus'){tgt.focus();return{ok:true};}"
        + "if(mode==='clear'){var _ch=_hostOf(tgt);if(!_ch)return{ok:false,reason:'no editable host'};if(_ced(_ch)){_clr(_ch);}else if('value' in _ch){var _cpr=_ch.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;var _cd=Object.getOwnPropertyDescriptor(_cpr,'value');if(_cd&&_cd.set){_cd.set.call(_ch,'');}else{_ch.value='';}_ch.dispatchEvent(new Event('input',{bubbles:true}));_ch.dispatchEvent(new Event('change',{bubbles:true}));}else{return{ok:false,reason:'not an editable field'};}return{ok:true,text:_read(_ch)};}"
        + "if(mode==='click'){n.click();return{ok:true};}"
        + "if(mode==='check'){var _kh=_hostOf(tgt);return{ok:true,text:_read(_kh),hostKind:(_ced(_kh)?'ce':((_kh&&_kh.tagName)?_kh.tagName.toLowerCase():'')),hostIsTgt:_kh===tgt,sel:_selLen(_kh),tgtMissing:(tgt&&tgt.isConnected===false)?true:false};}"
        + "if(mode==='selhost'){var _sh=_hostOf(tgt);if(!_sh)return{ok:false,reason:'no editable host'};try{_sh.focus();if('setSelectionRange' in _sh&&'value' in _sh){_sh.setSelectionRange(0,(_sh.value||'').length);}else{var _s3=doc.getSelection();var _r3=doc.createRange();_r3.selectNodeContents(_sh);_s3.removeAllRanges();_s3.addRange(_r3);}}catch(_e){}return{ok:true,sel:_selLen(_sh)};}"
        + "if(mode==='type'){var _th=_hostOf(tgt)||tgt;if(!_th||_ced(_th)||!('value' in _th))return{ok:false,reason:'js-type only applies to value fields'};_th.focus();var _tx="
        + &text_js
        + ";var _tpr=_th.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;var _tsd=Object.getOwnPropertyDescriptor(_tpr,'value');if(_tsd&&_tsd.set){_tsd.set.call(_th,_tx);}else{_th.value=_tx;}_th.dispatchEvent(new Event('input',{bubbles:true}));_th.dispatchEvent(new Event('change',{bubbles:true}));return{ok:true,text:_read(_th)};}"
        + "if(mode==='select'){"
        + "if(n.tagName==='SELECT'){"
        + "var opts=[...n.options];var match=opts.find(o=>o.value===" + &text_js + ")||opts.find(o=>o.text.trim()===" + &text_js + ")||opts.find(o=>o.text.trim().toLowerCase()===(" + &text_js + ").toLowerCase())||opts.find(o=>o.value.toLowerCase()===(" + &text_js + ").toLowerCase());"
        + "if(match){n.value=match.value;n.dispatchEvent(new Event('change',{bubbles:true}));n.dispatchEvent(new Event('input',{bubbles:true}));return{ok:true};}"
        + "var __lo=[...n.options].slice(0,80).map(o=>{var t=(o.label||o.text||'').trim().replace(/\\s+/g,' ').split('|').join('¦');var v=(o.value||'').trim().split('|').join('¦');if(t&&v&&t!==v)return t+'='+v;return t||(v?'='+v:'');}).filter(Boolean);return{ok:false,reason:'option not found in select',options:__lo,ototal:n.options.length};"
        + "}"
        + "n.value=" + &text_js + ";n.dispatchEvent(new Event('change',{bubbles:true}));n.dispatchEvent(new Event('input',{bubbles:true}));return{ok:true};}"
        + "if(mode==='read'){var txt=n.innerText||n.textContent||'';if(!txt&&('value' in n)&&n.value)txt=n.value;return{ok:true,text:txt.slice(0,5000)};}"
        + "if(mode==='hover'){n.scrollIntoView({block:'center'});const rect=n.getBoundingClientRect();const cx=rect.x+rect.width/2;const cy=rect.y+rect.height/2;const top=doc.elementFromPoint(cx,cy);const isTopmost=top===n||n.contains(top);return{ok:true,box:[Math.round(rect.x+ox)||0,Math.round(rect.y+oy)||0,Math.round(rect.width)||0,Math.round(rect.height)||0],isTopmost:isTopmost};}"
        + "return{ok:false,reason:'unknown mode'};}}"
        + "if(mode==='check'||mode==='prepare'){var _a3=doc.activeElement;var _h3=_isd(_a3)?_a3:null;if(_h3){if(mode==='prepare')_h3.focus();return{ok:true,text:_read(_h3),hostKind:(_ced(_h3)?'ce':((_h3.tagName)?_h3.tagName.toLowerCase():'')),hostIsTgt:false,sel:_selLen(_h3),tgtMissing:true};}}"
        + "return{ok:false,reason:'not found'};})("
        + &sig_js
        + ","
        + &mode_js
        + ","
        + &text_js
        + ","
        + &frame_js
        + ")")
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
    let expression = find_sig_expr(sig, mode, text, frame)?;

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

/// Kind of edit an `EditReport` describes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EditKind {
    Type,
    Clear,
}

/// What actually happened to an edited field - the authority behind the
/// verdict. The Type/Clear arms fill it; `finalize_edit` re-reads after
/// settle (and runs at most one bounded corrective pass); `compute_verdict`
/// formats it. Every note in the verdict must be backed by a readback here.
#[derive(Debug, Clone)]
struct EditReport {
    kind: EditKind,
    /// Typed text (empty for clears).
    text: String,
    /// Readback of the effective editing host at verdict time.
    final_text: String,
    /// Readback right after the action (diffed against `final_text`).
    branch_text: String,
    /// "ce" | "input" | "textarea" | "" - host kind at the last read.
    host_kind: String,
    /// The host IS the addressed element (no framework redirect).
    host_is_tgt: bool,
    /// The addressed element is gone (framework remounted it); the host
    /// data still describes the live editor.
    tgt_missing: bool,
    /// Chars the field held before a replace-clear ran.
    pre_text_len: usize,
    /// The field was empty (or verified-cleared) before typing.
    pre_cleared: bool,
    /// Exact match (type) / empty (clear) at the last readback.
    verified: bool,
    /// A JS setter wrote the value because key events did not register.
    set_via_js: bool,
    /// A corrective pass ran (late content / failed clear retried once).
    corrected: bool,
}

/// Outcome of a verified clear.
struct ClearResult {
    /// The host read empty at the last readback.
    ok: bool,
    /// Last readback of the host.
    text: String,
    /// The host was already empty (nothing to do).
    was_empty: bool,
    /// Neither the addressed element nor a live editor host was reachable.
    missing: bool,
}

/// Normalize text for readback comparison (mirrors the JS `_tvr`).
fn norm_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Readback of the effective editing host via the "check" mode. `None`
/// when the eval itself fails - callers treat that as "unverified", never
/// as "empty".
async fn check_editor(cdp: &CdpSession, sig: &str, frame: &[usize]) -> Option<FoundElement> {
    find_by_sig(cdp, sig, frame, "check", None).await.ok().filter(|f| f.ok)
}

/// Clear the addressed element's effective editing host, verified at every
/// rung. Ladder: JS setter (value fields) -> trusted Ctrl+A + Backspace ->
/// Ctrl+A carrying the selectAll editing command -> JS range-select +
/// trusted Backspace -> execCommand. The result carries the final readback
/// so no caller can claim a clear that did not happen.
async fn clear_editable(cdp: &CdpSession, sig: &str, frame: &[usize]) -> Result<ClearResult> {
    // Focus first: framework editors mount and take focus here, and the
    // trusted-key rungs act on whatever is focused.
    let _ = find_by_sig(cdp, sig, frame, "focus", None).await;
    let mut cur = check_editor(cdp, sig, frame).await;
    let Some(first) = cur.as_ref() else {
        return Ok(ClearResult { ok: false, text: String::new(), was_empty: false, missing: true });
    };
    let mut text = first.text.clone().unwrap_or_default();
    if text.is_empty() {
        return Ok(ClearResult { ok: true, text, was_empty: true, missing: false });
    }
    let kind = first.host_kind.clone().unwrap_or_default();
    if kind == "input" || kind == "textarea" || kind == "select" {
        // Fast path for value fields: native setter + input/change events
        // (React/Vue compatible), then readback.
        let _ = find_by_sig(cdp, sig, frame, "clear", None).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        cur = check_editor(cdp, sig, frame).await;
        text = cur.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
        if text.is_empty() {
            return Ok(ClearResult { ok: true, text, was_empty: false, missing: false });
        }
    }
    // Trusted select-all + Backspace; then the command variant; then a
    // programmatic selection with the same trusted delete.
    for rung in 0..3u8 {
        let after = match rung {
            0 => trusted_select_all(cdp, sig, frame, false).await?,
            1 => trusted_select_all(cdp, sig, frame, true).await?,
            _ => js_select_all(cdp, sig, frame).await?,
        };
        if let Some(t) = after {
            text = t;
            if text.is_empty() {
                return Ok(ClearResult { ok: true, text, was_empty: false, missing: false });
            }
        }
    }
    // Legacy editors: the execCommand path, then a final readback.
    let _ = find_by_sig(cdp, sig, frame, "clear", None).await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    cur = check_editor(cdp, sig, frame).await;
    text = cur.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
    Ok(ClearResult { ok: text.is_empty(), text, was_empty: false, missing: false })
}

/// Rung helper: trusted Ctrl+A (optionally carrying the selectAll editing
/// command) + trusted Backspace. `Some(readback)` when a selection was
/// made and deleted; `None` when the shortcut did not select anything.
async fn trusted_select_all(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
    with_cmd: bool,
) -> Result<Option<String>> {
    let combo = KeyCombo { ctrl: true, alt: false, shift: false, meta: false, key: "a".to_string() };
    let cmds = if with_cmd { Some(vec!["selectAll".to_string()]) } else { None };
    dispatch_combo(cdp, &combo, cmds).await?;
    tokio::time::sleep(Duration::from_millis(25)).await;
    let cur = check_editor(cdp, sig, frame).await;
    let sel = cur.as_ref().and_then(|c| c.sel).unwrap_or(0);
    if sel == 0 {
        return Ok(None);
    }
    dispatch_key(cdp, "Backspace").await?;
    tokio::time::sleep(Duration::from_millis(35)).await;
    let after = check_editor(cdp, sig, frame).await;
    Ok(Some(after.as_ref().and_then(|c| c.text.clone()).unwrap_or_default()))
}

/// Rung helper: JS range-select (selhost) + the same trusted Backspace.
async fn js_select_all(cdp: &CdpSession, sig: &str, frame: &[usize]) -> Result<Option<String>> {
    let sh = find_by_sig(cdp, sig, frame, "selhost", None).await?;
    if !sh.ok || sh.sel.unwrap_or(0) == 0 {
        return Ok(None);
    }
    dispatch_key(cdp, "Backspace").await?;
    tokio::time::sleep(Duration::from_millis(35)).await;
    let after = check_editor(cdp, sig, frame).await;
    Ok(Some(after.as_ref().and_then(|c| c.text.clone()).unwrap_or_default()))
}

/// Dispatch the typing itself: humanized per-char key events for short text
/// (the biometrics path - keydown/keyup pairs, Shift wrapping, the full
/// log-normal cadence), one `Input.insertText` for long text (a paste/IME
/// commit - human-plausible and fast). Returns true when a dispatch path
/// reported success; the readback remains the authority on what landed.
async fn type_text(cdp: &CdpSession, text: &str) -> bool {
    const PER_CHAR_MAX: usize = 120;
    let mut typed = false;
    if text.chars().count() <= PER_CHAR_MAX {
        typed = type_per_char(cdp, text).await;
    }
    if !typed {
        typed = cdp.send("Input.insertText", Some(json!({ "text": text }))).await.is_ok();
    }
    if !typed {
        let _ = type_per_char(cdp, text).await;
    }
    typed
}

/// Post-settle finalization for editor actions: one last readback of the
/// host plus at most ONE bounded corrective pass - retype when the field
/// shows content that is not exactly the typed text, re-clear when a draft
/// restored after a verified clear. Corrections need a readable state; a
/// readback we cannot see is reported, never fought (a blind retry could
/// double-type).
async fn finalize_edit(rep: &mut EditReport, cdp: &CdpSession, sig: &str, frame: &[usize]) -> Result<()> {
    let read = check_editor(cdp, sig, frame).await;
    let mut final_text = read.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
    if let Some(c) = &read {
        rep.host_kind = c.host_kind.clone().unwrap_or_default();
        rep.host_is_tgt = c.host_is_tgt.unwrap_or(false);
        rep.tgt_missing = c.tgt_missing.unwrap_or(false);
    }
    match rep.kind {
        EditKind::Type => {
            let want = norm_text(&rep.text);
            rep.verified = norm_text(&final_text) == want;
            if !rep.verified {
                // Visible-but-wrong states can be corrected safely; value
                // fields are readable by construction; blind CE readbacks
                // stay as reported.
                let retry_ok = !norm_text(&final_text).is_empty()
                    || matches!(rep.host_kind.as_str(), "input" | "textarea");
                if retry_ok {
                    let cl = clear_editable(cdp, sig, frame).await?;
                    if cl.ok {
                        let _ = type_text(cdp, &rep.text).await;
                        for _ in 0..3u8 {
                            tokio::time::sleep(Duration::from_millis(120)).await;
                            let after = check_editor(cdp, sig, frame).await;
                            final_text = after.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
                            if norm_text(&final_text) == want {
                                break;
                            }
                        }
                        rep.corrected = true;
                        rep.verified = norm_text(&final_text) == want;
                    }
                }
            }
            rep.final_text = final_text;
        }
        EditKind::Clear => {
            if !final_text.is_empty() && rep.branch_text.is_empty() {
                // Content restored after a verified clear (draft hydration).
                let cl = clear_editable(cdp, sig, frame).await?;
                final_text = cl.text;
                rep.corrected = true;
                rep.verified = cl.ok;
            } else {
                rep.verified = final_text.is_empty();
            }
            rep.final_text = final_text;
        }
    }
    Ok(())
}

/// Find a ref (different from `target`) whose captured value equals
/// `final_text` - the "where did the text actually land" lookup for
/// framework editors whose wrapper and editor are separate elements.
fn landed_ref_excluding(lpm: &LivePageModel, target: &str, final_text: &str) -> Option<String> {
    let want = norm_text(final_text);
    if want.is_empty() {
        return None;
    }
    lpm.elements()
        .iter()
        .find(|e| {
            e.ref_id != target
                && e.raw.role == "textbox"
                && norm_text(e.raw.value.as_deref().unwrap_or("")) == want
        })
        .map(|e| e.ref_id.clone())
}

/// Type verdict from the verified report. `edit: None` callers keep the
/// capture-based fallback in `compute_verdict`.
fn type_verdict_text(ref_id: &str, text: &str, rep: &EditReport, lpm: &LivePageModel) -> String {
    let want = norm_text(text);
    let got = norm_text(&rep.final_text);
    if rep.verified && got == want {
        let mut s = format!("outcome: typed \"{}\" → value=\"{}\"", clip(text, 40), clip(&rep.final_text, 40));
        if rep.pre_text_len > 0 {
            s.push_str(&format!(" (replaced {} chars)", rep.pre_text_len));
        }
        if rep.corrected {
            s.push_str(" (after a retry)");
        }
        if rep.set_via_js {
            s.push_str(" (set via JS - key events did not register)");
        } else if rep.tgt_missing || !rep.host_is_tgt {
            match landed_ref_excluding(lpm, ref_id, &rep.final_text) {
                Some(l) => s.push_str(&format!(" (landed in {l}: the live editor)")),
                None => s.push_str(" (landed in the focused editor)"),
            }
        }
        if !rep.corrected && norm_text(&rep.branch_text) != got {
            s.push_str(" (settled late)");
        }
        s
    } else if !got.is_empty() && got.contains(&want) {
        let why = if rep.pre_cleared {
            "content changed after typing (late restore?)"
        } else {
            "field had existing content (clear did not empty it)"
        };
        format!(
            "outcome: typed \"{}\" → value=\"{}\" ({why})",
            clip(text, 40),
            clip(&rep.final_text, 40)
        )
    } else if !got.is_empty() {
        format!(
            "outcome: typed \"{}\" → value=\"{}\" (readback mismatch)",
            clip(text, 40),
            clip(&rep.final_text, 40)
        )
    } else {
        format!(
            "outcome: typed \"{}\" → readback unverified (the editor shows no readable text; it may hydrate late)",
            clip(text, 40)
        )
    }
}

/// Clear verdict from the verified report.
fn clear_verdict_text(ref_id: &str, rep: &EditReport) -> String {
    if rep.verified {
        let mut s = format!("outcome: cleared {ref_id} (verified empty)");
        if rep.corrected {
            s.push_str(" (draft restored and was cleared again)");
        } else if !rep.host_is_tgt || rep.tgt_missing {
            s.push_str(" (live editor)");
        }
        s
    } else {
        format!(
            "outcome: clear failed - still contains \"{}\" ({} chars)",
            clip(&rep.final_text, 40),
            rep.final_text.chars().count()
        )
    }
}

/// Dispatch a human-like mouse move from a random start point to `target`.
/// Returns the final (click) position after bezier path + overshoot correction.
/// Shared by click (adds press/release after) and hover (no click).
async fn dispatch_mouse_move(
    cdp: &CdpSession,
    target: (f64, f64),
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<(f64, f64)> {
    let mut rng = crate::stealth::Rng::new();
    // Random start point — offset from the target by 100-300px.
    // If we have a last position, start from there instead (continuity).
    let start = {
        let lm = last_mouse.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((lx, ly)) = *lm {
            (lx, ly)
        } else {
            let angle = rng.uniform() * 2.0 * std::f64::consts::PI;
            let start_dist = 100.0 + rng.uniform() * 200.0;
            (target.0 + angle.cos() * start_dist, target.1 + angle.sin() * start_dist)
        }
    };

    // Generate the bezier path with overshoot + gaussian click offset.
    let path = crate::stealth::mouse_path(start, target, &mut rng);

    // Move along the path, dispatching mouseMoved events with natural delays.
    // Include movementX/movementY deltas — PerimeterX/HUMAN tracks these
    // coordinate deltas as behavioral biometrics; missing or zero values
    // are an instant bot flag.
    // 5s timeout: if Chrome is stuck (e.g. new tab loading), don't hang 30s.
    let to = Duration::from_secs(5);
    for pt in &path {
        let (mx, my) = {
            let lm = last_mouse.lock().unwrap_or_else(|e| e.into_inner());
            lm.map(|(lx, ly)| (
                (pt.x - lx).round() as i64,
                (pt.y - ly).round() as i64,
            )).unwrap_or((0, 0))
        };
        cdp.send_with_timeout(
            "Input.dispatchMouseEvent",
            Some(json!({
                "type": "mouseMoved",
                "x": pt.x, "y": pt.y,
                "movementX": mx, "movementY": my,
            })),
            to,
        )
        .await?;
        *last_mouse.lock().unwrap_or_else(|e| e.into_inner()) = Some((pt.x, pt.y));
        tokio::time::sleep(pt.delay).await;
    }

    // Return the final click point (after overshoot correction).
    Ok(crate::stealth::click_target(&path))
}

/// Dispatch a real mouse click at (x, y) via `Input.dispatchMouseEvent`,
/// with a human-like bezier mouse path from a random start point.
pub(crate) async fn dispatch_mouse_click(
    cdp: &CdpSession,
    x: f64,
    y: f64,
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<()> {
    let (cx, cy) = dispatch_mouse_move(cdp, (x, y), last_mouse).await?;

    let mut rng = crate::stealth::Rng::new();
    let to = Duration::from_secs(5);
    // Micro-tremors: 1-3 tiny jitter points (1-3px) before clicking.
    // Real humans have involuntary hand tremors even when "holding still".
    // A perfectly stationary cursor before a click is a bot signal.
    let tremor_count = rng.range(1, 3) as usize;
    for _ in 0..tremor_count {
        let jx = gaussian(&mut rng, 0.0, 1.5).clamp(-3.0, 3.0);
        let jy = gaussian(&mut rng, 0.0, 1.5).clamp(-3.0, 3.0);
        let tx = cx + jx;
        let ty = cy + jy;
        let (mx, my) = {
            let lm = last_mouse.lock().unwrap_or_else(|e| e.into_inner());
            lm.map(|(lx, ly)| (
                (tx - lx).round() as i64,
                (ty - ly).round() as i64,
            )).unwrap_or((0, 0))
        };
        cdp.send_with_timeout(
            "Input.dispatchMouseEvent",
            Some(json!({
                "type": "mouseMoved",
                "x": tx, "y": ty,
                "movementX": mx, "movementY": my,
            })),
            to,
        )
        .await?;
        *last_mouse.lock().unwrap_or_else(|e| e.into_inner()) = Some((tx, ty));
        tokio::time::sleep(Duration::from_millis(rng.range(8, 24) as u64)).await;
    }
    // Press + release at the final position.
    cdp.send_with_timeout(
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mousePressed", "x": cx, "y": cy, "button": "left", "clickCount": 1,
            "movementX": 0, "movementY": 0,
        })),
        to,
    )
    .await?;
    // Small delay between press and release (28-70ms).
    tokio::time::sleep(Duration::from_millis(28 + rng.range(0, 42) as u64)).await;
    cdp.send_with_timeout(
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mouseReleased", "x": cx, "y": cy, "button": "left", "clickCount": 1,
            "movementX": 0, "movementY": 0,
        })),
        to,
    )
    .await?;
    Ok(())
}

/// A parsed key chord ("Control+a", "Meta+Enter", "Shift+Tab").
#[derive(Debug, Clone, PartialEq)]
struct KeyCombo {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    key: String,
}

/// Parse chord syntax. `Ok(None)` when the input is not a chord (no '+' at
/// all); `Err` for malformed chords so the caller can name the problem.
fn parse_key_combo(input: &str) -> std::result::Result<Option<KeyCombo>, String> {
    // A single character (including '+' itself) is never chord syntax.
    if input.chars().count() <= 1 || !input.contains('+') {
        return Ok(None);
    }
    let parts: Vec<&str> = input.split('+').collect();
    let mut combo = KeyCombo {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
        key: String::new(),
    };
    for (i, raw) in parts.iter().enumerate() {
        let p = raw.trim();
        if p.is_empty() {
            return Err(format!("invalid key combo '{input}'"));
        }
        if i == parts.len() - 1 {
            combo.key = p.to_string();
        } else {
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => combo.ctrl = true,
                "alt" => combo.alt = true,
                "shift" => combo.shift = true,
                "meta" | "cmd" | "command" | "super" | "win" => combo.meta = true,
                other => return Err(format!("unsupported modifier '{other}' in '{input}'")),
            }
        }
    }
    Ok(Some(combo))
}

/// Named keys (case-insensitive) -> (key name, code, windowsVirtualKeyCode).
fn named_key(key: &str) -> Option<(&'static str, &'static str, u32)> {
    match key.to_ascii_lowercase().as_str() {
        "enter" => Some(("Enter", "Enter", 13)),
        "tab" => Some(("Tab", "Tab", 9)),
        "escape" | "esc" => Some(("Escape", "Escape", 27)),
        "backspace" => Some(("Backspace", "Backspace", 8)),
        "delete" => Some(("Delete", "Delete", 46)),
        "arrowdown" | "down" => Some(("ArrowDown", "ArrowDown", 40)),
        "arrowup" | "up" => Some(("ArrowUp", "ArrowUp", 38)),
        "arrowleft" | "left" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "arrowright" | "right" => Some(("ArrowRight", "ArrowRight", 39)),
        "home" => Some(("Home", "Home", 36)),
        "end" => Some(("End", "End", 35)),
        "pagedown" => Some(("PageDown", "PageDown", 34)),
        "pageup" => Some(("PageUp", "PageUp", 33)),
        "space" | " " => Some((" ", "Space", 32)),
        _ => None,
    }
}

/// Build one CDP key event.
fn key_event(
    kind: &str,
    key: &str,
    code: &str,
    vk: u32,
    modifiers: u8,
    text: Option<&str>,
) -> serde_json::Value {
    let mut ev = json!({
        "type": kind,
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": vk,
        "modifiers": modifiers,
    });
    if let Some(t) = text {
        ev["text"] = json!(t);
    }
    ev
}

/// Resolve a chord's main key to (key, code, vk, text).
fn resolve_combo_key(combo: &KeyCombo) -> std::result::Result<(String, String, u32, Option<String>), String> {
    let key = combo.key.as_str();
    if key.chars().count() == 1 {
        let mut ch = key.chars().next().unwrap();
        // Shift over a letter reports the shifted glyph, as real keyboards do.
        if combo.shift && ch.is_ascii_alphabetic() {
            ch = ch.to_ascii_uppercase();
        }
        let (code, vk, _) = char_to_key_code(ch);
        if code == "Unidentified" {
            return Err(format!("unsupported key '{key}' in combo"));
        }
        Ok((ch.to_string(), code, vk, Some(ch.to_string())))
    } else {
        let (kname, code, vk) = named_key(key)
            .ok_or_else(|| format!("unsupported key '{key}' in combo"))?;
        let text = match kname {
            "Enter" => Some("\r".to_string()),
            "Tab" => Some("\t".to_string()),
            " " => Some(" ".to_string()),
            _ => None,
        };
        Ok((kname.to_string(), code.to_string(), vk, text))
    }
}

/// The CDP event sequence for a chord: modifier downs, main down, main up,
/// modifier ups (each release drops its own bit, in reverse order). Returns
/// the events and the index of the main keydown so the dispatcher can hold
/// the key like the plain path does.
fn combo_events(combo: &KeyCombo) -> std::result::Result<(Vec<serde_json::Value>, usize), String> {
    let (kname, kcode, kvk, ktext) = resolve_combo_key(combo)?;
    let mods = [
        ("Control", "ControlLeft", 17u32, 2u8, combo.ctrl),
        ("Alt", "AltLeft", 18, 1, combo.alt),
        ("Shift", "ShiftLeft", 16, 8, combo.shift),
        ("Meta", "MetaLeft", 91, 4, combo.meta),
    ];
    let mut bits: u8 = 0;
    let mut evs: Vec<serde_json::Value> = Vec::new();
    for (name, code, vk, bit, on) in mods {
        if on {
            bits |= bit;
            evs.push(key_event("keyDown", name, code, vk, bits, None));
        }
    }
    // Ctrl/Alt/Meta chords are shortcuts, never text; Shift keeps the glyph.
    let text = if combo.ctrl || combo.alt || combo.meta { None } else { ktext };
    let kind = if text.is_some() { "keyDown" } else { "rawKeyDown" };
    let main_idx = evs.len();
    evs.push(key_event(kind, &kname, &kcode, kvk, bits, text.as_deref()));
    evs.push(key_event("keyUp", &kname, &kcode, kvk, bits, None));
    for (name, code, vk, bit, on) in mods.into_iter().rev() {
        if on {
            bits &= !bit;
            evs.push(key_event("keyUp", name, code, vk, bits, None));
        }
    }
    Ok((evs, main_idx))
}

/// Dispatch a key press (keyDown + keyUp) via `Input.dispatchKeyEvent`.
/// Accepts single characters, named keys, and chords ("Control+a").
async fn dispatch_key(cdp: &CdpSession, key: &str) -> Result<()> {
    if let Some(combo) = parse_key_combo(key).map_err(BladeError::Other)? {
        return dispatch_combo(cdp, &combo, None).await;
    }
    // Printable single character: full char event (code + VK + text).
    // `act press key="a"` used to dispatch key:"a" code:"Unidentified"
    // vk:0 - a synthetic-looking no-op no page would act on.
    if key.chars().count() == 1 {
        let ch = key.chars().next().unwrap();
        let (code, vk, shift) = char_to_key_code(ch);
        let modifiers: u8 = if shift { 8 } else { 0 };
        if shift {
            cdp.send("Input.dispatchKeyEvent", Some(json!({
                "type": "keyDown", "key": "Shift", "code": "ShiftLeft",
                "windowsVirtualKeyCode": 16, "modifiers": 8,
            }))).await?;
        }
        cdp.send("Input.dispatchKeyEvent", Some(json!({
            "type": "keyDown", "key": key, "code": code,
            "windowsVirtualKeyCode": vk, "text": key, "modifiers": modifiers,
            "keyChar": key,
        }))).await?;
        // Key press duration: 25-70ms - real humans hold before releasing.
        let mut rng = crate::stealth::Rng::new();
        tokio::time::sleep(Duration::from_millis(25 + rng.range(0, 45) as u64)).await;
        cdp.send("Input.dispatchKeyEvent", Some(json!({
            "type": "keyUp", "key": key, "code": code,
            "windowsVirtualKeyCode": vk, "modifiers": modifiers,
        }))).await?;
        if shift {
            cdp.send("Input.dispatchKeyEvent", Some(json!({
                "type": "keyUp", "key": "Shift", "code": "ShiftLeft",
                "windowsVirtualKeyCode": 16, "modifiers": 0,
            }))).await?;
        }
        return Ok(());
    }

    let (kname, code, vk) = named_key(key).ok_or_else(|| BladeError::Other(format!(
        "unsupported key: '{key}'. Supported: single characters, named keys (Enter, Tab, Escape, Backspace, Delete, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, Space), and chords (Control+a, Meta+Enter, Shift+Tab)"
    )))?;
    // The `text` field is critical for keys that produce text - without it,
    // the browser fires keydown but doesn't process the default action
    // (e.g. Enter won't submit forms, Space won't scroll/click).
    let text = match kname {
        "Enter" => Some("\r"),
        "Tab" => Some("\t"),
        " " => Some(" "),
        _ => None,
    };
    let mut key_down = json!({
        "type": "keyDown",
        "key": kname,
        "code": code,
        "windowsVirtualKeyCode": vk,
    });
    if let Some(t) = text {
        key_down["text"] = json!(t);
    }
    let key_up = json!({
        "type": "keyUp",
        "key": kname,
        "code": code,
        "windowsVirtualKeyCode": vk,
    });
    cdp.send("Input.dispatchKeyEvent", Some(key_down)).await?;
    // Key press duration: 40-110ms - real humans hold before releasing.
    let mut rng = crate::stealth::Rng::new();
    tokio::time::sleep(Duration::from_millis(40 + rng.range(0, 70) as u64)).await;
    cdp.send("Input.dispatchKeyEvent", Some(key_up)).await?;
    Ok(())
}

/// Dispatch a parsed chord, optionally attaching Chrome editing commands
/// (e.g. ["selectAll"]) to the main keydown - the deterministic fallback
/// when a framework swallows the plain shortcut.
async fn dispatch_combo(
    cdp: &CdpSession,
    combo: &KeyCombo,
    commands: Option<Vec<String>>,
) -> Result<()> {
    let (mut evs, main_idx) = combo_events(combo).map_err(BladeError::Other)?;
    if let Some(cmds) = commands {
        if let Some(obj) = evs.get_mut(main_idx).and_then(|v| v.as_object_mut()) {
            obj.insert("commands".to_string(), json!(cmds));
        }
    }
    for (i, ev) in evs.into_iter().enumerate() {
        cdp.send("Input.dispatchKeyEvent", Some(ev)).await?;
        if i == main_idx {
            // Hold the key like real fingers do (matches the plain path).
            let mut rng = crate::stealth::Rng::new();
            tokio::time::sleep(Duration::from_millis(25 + rng.range(0, 45) as u64)).await;
        }
    }
    Ok(())
}

/// Type text with per-character key events and human cadence.
/// Returns false when a dispatch fails (caller falls back to insertText).
///
/// v3.9 realism (M6): the inter-key cadence sleep happens BETWEEN keyDown
/// and keyUp — the key is HELD for the interval, as real fingers do (the
/// old code slept after keyUp: hold time was a constant ~1-5ms). Uppercase
/// and shifted symbols get a real Shift keyDown/up with modifiers:8.
/// Non-ASCII chars dispatch with VK 229 (IME), matching real IME input.
async fn type_per_char(cdp: &CdpSession, text: &str) -> bool {
    let mut rng = crate::stealth::Rng::new();
    let cadence = crate::stealth::typing_cadence(text, &mut rng);
    for (i, ch) in text.chars().enumerate() {
        let ch_str = ch.to_string();
        let (code, vk, shift) = char_to_key_code(ch);
        let modifiers: u8 = if shift { 8 } else { 0 };
        if shift && cdp.send("Input.dispatchKeyEvent", Some(json!({
            "type": "keyDown", "key": "Shift", "code": "ShiftLeft",
            "windowsVirtualKeyCode": 16, "modifiers": 8,
        }))).await.is_err() {
            return false;
        }
        let mut down = json!({
            "type": "keyDown",
            "key": ch_str,
            "code": code,
            "windowsVirtualKeyCode": vk,
            "modifiers": modifiers,
        });
        // `text` produces the char + keypress; non-ASCII uses IME VK 229
        // without text (the input event comes from the IME commit path).
        if ch.is_ascii() {
            down["text"] = json!(ch_str);
        }
        if cdp.send("Input.dispatchKeyEvent", Some(down)).await.is_err() {
            return false;
        }
        // HOLD the key for the inter-key interval.
        if i < cadence.len() {
            tokio::time::sleep(cadence[i]).await;
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if cdp.send("Input.dispatchKeyEvent", Some(json!({
            "type": "keyUp",
            "key": ch_str,
            "code": code,
            "windowsVirtualKeyCode": vk,
            "modifiers": modifiers,
        }))).await.is_err() {
            return false;
        }
        if shift && cdp.send("Input.dispatchKeyEvent", Some(json!({
            "type": "keyUp", "key": "Shift", "code": "ShiftLeft",
            "windowsVirtualKeyCode": 16, "modifiers": 0,
        }))).await.is_err() {
            return false;
        }
    }
    true
}

/// Map a printable character to (code, windowsVirtualKeyCode, needsShift)
/// for CDP key events. Shifted symbols resolve to their BASE key + Shift
/// ('!' → Digit1 + Shift), exactly as a real keyboard reports them; the
/// old map gave symbols code:"Unidentified" and vk:<unicode codepoint> —
/// a synthetic signature no real keyboard produces. Non-ASCII returns
/// VK 229 (IME composition), matching real IME input.
fn char_to_key_code(ch: char) -> (String, u32, bool) {
    if !ch.is_ascii() {
        return ("Unidentified".to_string(), 229, false);
    }
    let shift = ch.is_uppercase() || "!@#$%^&*()_+{}|:\"<>?~".contains(ch);
    let base = ch.to_ascii_lowercase();
    let (code, vk): (String, u32) = match base {
        'a'..='z' => {
            let up = base.to_ascii_uppercase();
            (format!("Key{up}"), up as u32)
        }
        '0'..='9' => (format!("Digit{base}"), base as u32),
        ' ' => ("Space".to_string(), 32),
        '.' | '>' => ("Period".to_string(), 190),
        ',' | '<' => ("Comma".to_string(), 188),
        '-' | '_' => ("Minus".to_string(), 189),
        '=' | '+' => ("Equal".to_string(), 187),
        '/' | '?' => ("Slash".to_string(), 191),
        '\\' | '|' => ("Backslash".to_string(), 220),
        ';' | ':' => ("Semicolon".to_string(), 186),
        '\'' | '"' => ("Quote".to_string(), 222),
        '`' | '~' => ("Backquote".to_string(), 192),
        '[' | '{' => ("BracketLeft".to_string(), 219),
        ']' | '}' => ("BracketRight".to_string(), 221),
        '!' => ("Digit1".to_string(), 49),
        '@' => ("Digit2".to_string(), 50),
        '#' => ("Digit3".to_string(), 51),
        '$' => ("Digit4".to_string(), 52),
        '%' => ("Digit5".to_string(), 53),
        '^' => ("Digit6".to_string(), 54),
        '&' => ("Digit7".to_string(), 55),
        '*' => ("Digit8".to_string(), 56),
        '(' => ("Digit9".to_string(), 57),
        ')' => ("Digit0".to_string(), 48),
        _ => ("Unidentified".to_string(), 0),
    };
    (code, vk, shift)
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
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<(PageDelta, String)> {
    perform_with_network(cdp, lpm, action, None, last_mouse).await
}

/// Mutation watcher: installed before an action dispatch so the next
/// capture can report whether the DOM actually changed — including changes
/// to non-actionable content (text swaps, counters, live regions) that the
/// actionable-element delta cannot see. Reset happens inside the capture
/// script after it reads the count. No attributes: class/animation churn
/// would flood the counter on animated pages.
///
/// v3.9: state lives under Symbol-keyed window slots (invisible to
/// Object.keys / for-in / `'name' in window` probes). The old
/// `window.__blade_muts` / `__blade_mo` string properties named the tool
/// outright on every page after the first action — a one-line detection.
pub const MUT_WATCH: &str = "(function(){var K=Symbol.for('m'),O=Symbol.for('n');var c=window[K];if(!c){c={n:0};try{window[K]=c;}catch(e){return;}if(!window[O]){try{var mo=new MutationObserver(function(l){c.n+=l.length;});window[O]=mo;mo.observe(document.documentElement||document,{childList:true,subtree:true,characterData:true});}catch(e){}}}c.n=0;})();";

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
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<(PageDelta, String)> {
    // Resolve ref → (sig, frame) for ref-targeted actions.
    let sig_frame = match action.ref_id() {
        Some(ref_id) => Some(resolve_ref(lpm, ref_id)?),
        None => None,
    };

    // Edit report (Type/Clear): what actually happened to the field. Filled
    // by the arms; finalized after settle by `finalize_edit`; consumed by
    // `compute_verdict` - the verdict never outclaims the readback.
    let mut edit_report: Option<EditReport> = None;

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
            dispatch_mouse_click(cdp, *x, *y, last_mouse).await?;
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
            // M8: dispatch-level failures (transport) are counted separately
            // from "clicked but no effect" — if EVERY strategy failed at the
            // transport level, reporting "element may be disabled" sends the
            // agent debugging the page when the driver was the problem.
            let mut dispatch_errors = 0usize;
            let mut last_dispatch_err: Option<BladeError> = None;

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
                            if let Err(e) = dispatch_mouse_click(cdp, cx, cy, last_mouse).await {
                                dispatch_errors += 1;
                                last_dispatch_err = Some(e);
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
                    "js" => {
                        match find_by_sig(cdp, sig, frame, "click", None).await {
                            Ok(f) if f.ok => {}
                            Ok(_) => continue,
                            Err(e) => {
                                dispatch_errors += 1;
                                last_dispatch_err = Some(e);
                                continue;
                            }
                        }
                    }
                    "enter" => {
                        let _ = find_by_sig(cdp, sig, frame, "focus", None).await;
                        if let Err(e) = dispatch_key(cdp, "Enter").await {
                            dispatch_errors += 1;
                            last_dispatch_err = Some(e);
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
                match tokio::time::timeout(Duration::from_millis(150), early).await {
                    Ok(true) => dialog_fired = true,
                    Ok(false) => {
                        // Small delay to let the new execution context
                        // be created before we send Runtime.evaluate
                        // commands. Without this, the evaluate can hang
                        // on JSON/binary response pages (e.g. httpbin.org/post).
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        crate::page::wait_for_load(cdp, Duration::from_secs(10)).await?;
                    }
                    _ => {}
                }
                wait_for_settle_with_network(cdp, Duration::from_millis(1500), in_flight).await?;
                let cap = capture(cdp).await?;
                delta = lpm.ingest(cap);

                if delta.navigated || !delta.is_empty() || delta.content_changed || dialog_fired {
                    break;
                }
            }

            // All strategies errored at the DISPATCH level: the driver
            // failed, not the page — surface the transport error.
            if dispatch_errors == tried.len() && !tried.is_empty() {
                if let Some(e) = last_dispatch_err {
                    return Err(e);
                }
            }
            // Expose the resolved click target so no-effect verdicts can
            // distinguish a bad selector from a page that rejected a well-
            // aimed click (issue #15). When the target is NOT topmost, name
            // what actually receives the click there (occlusion diagnostic).
            let mut tgt_meta = found.hit_tgt.as_deref().unwrap_or("").to_string();
            if !tgt_meta.is_empty() {
                if found.is_topmost == Some(false) {
                    match found.top_desc.as_deref() {
                        Some(top) => tgt_meta.push_str(&format!(" (topmost=false - clicks land on {top})")),
                        None => tgt_meta.push_str(" (topmost=false)"),
                    }
                } else {
                    tgt_meta.push_str(&format!(
                        " (topmost={},disabled={})",
                        found.is_topmost.unwrap_or(false),
                        found.disabled.unwrap_or(false)
                    ));
                }
            }
            let mut verdict = compute_verdict(action, &delta, lpm, Some((via, &tried, &tgt_meta)), None, None);
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
            // Hard cap: a multi-MB text into the per-char path would hang
            // the daemon for hours; even insertText serializes MBs of JSON
            // through CDP for no agent benefit.
            const MAX_TYPE_CHARS: usize = 100_000;
            if text.chars().count() > MAX_TYPE_CHARS {
                return Err(BladeError::Other(format!(
                    "text too long ({} chars, max {MAX_TYPE_CHARS}) — split the input or use a file upload",
                    text.chars().count()
                )));
            }
            // Focus the target. Framework composers (facade textarea + a
            // late-mounted contenteditable) mount their real editor here and
            // may move focus into it - typing then lands where a human's
            // would, and every readback below resolves the effective host.
            let focus = find_by_sig(cdp, sig, frame, "prepare", None).await?;
            let pre = check_editor(cdp, sig, frame).await;
            if !focus.ok && pre.as_ref().and_then(|p| p.host_kind.clone()).unwrap_or_default().is_empty() {
                // The addressed element is gone and no live editor took over.
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            let pre_read = pre.as_ref().and_then(|p| p.text.clone()).unwrap_or_default();
            // Replace semantics: clear existing content first, VERIFIED. A
            // clear that cannot empty a framework editor is not claimed -
            // it flows into the report and the verdict says so.
            let mut pre_cleared = pre_read.is_empty();
            if !pre_read.is_empty() {
                let cl = clear_editable(cdp, sig, frame).await?;
                pre_cleared = cl.ok;
            }
            // Type: short text via per-char key events (the biometrics
            // path: Shift wrapping, hold-time cadence), long text via one
            // insertText (a paste/IME commit - human-plausible and fast).
            let _ = type_text(cdp, text).await;
            // Readback with a bounded poll: framework editors (and late
            // drafts) can surface their content after the keystrokes return.
            let want = norm_text(text);
            let mut last = check_editor(cdp, sig, frame).await;
            let mut branch_text = last.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
            if norm_text(&branch_text) != want {
                for _ in 0..6u8 {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    last = check_editor(cdp, sig, frame).await;
                    branch_text = last.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
                    if norm_text(&branch_text) == want {
                        break;
                    }
                }
            }
            // Rescue for stubborn value fields: JS setter when key events
            // did not register at all. Never for contenteditables - a DOM
            // write desyncs a framework editor's internal state.
            let mut set_via_js = false;
            if norm_text(&branch_text) != want {
                let kind = last.as_ref().and_then(|c| c.host_kind.clone()).unwrap_or_default();
                if (kind == "input" || kind == "textarea")
                    && norm_text(&branch_text) == norm_text(&pre_read)
                {
                    let js = find_by_sig(cdp, sig, frame, "type", Some(text)).await?;
                    if js.ok {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        last = check_editor(cdp, sig, frame).await;
                        branch_text = last.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
                        set_via_js = norm_text(&branch_text) == want;
                    }
                }
            }
            let host = last.as_ref();
            let verified = norm_text(&branch_text) == want;
            edit_report = Some(EditReport {
                kind: EditKind::Type,
                text: text.clone(),
                final_text: branch_text.clone(),
                branch_text,
                host_kind: host.and_then(|c| c.host_kind.clone()).unwrap_or_default(),
                host_is_tgt: host.and_then(|c| c.host_is_tgt).unwrap_or(false),
                tgt_missing: host.and_then(|c| c.tgt_missing).unwrap_or(false),
                pre_text_len: pre_read.chars().count(),
                pre_cleared,
                verified,
                set_via_js,
                corrected: false,
            });
        }
        Action::Clear { ref_id } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Verified clear: the ladder (setter -> trusted keys -> JS path)
            // reads back at every rung; the report carries the truth into
            // the verdict - a clear that did not empty the field never
            // reads "cleared".
            let cl = clear_editable(cdp, sig, frame).await?;
            if cl.missing {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            let cur = check_editor(cdp, sig, frame).await;
            let host = cur.as_ref();
            edit_report = Some(EditReport {
                kind: EditKind::Clear,
                text: String::new(),
                final_text: cl.text.clone(),
                branch_text: cl.text,
                host_kind: host.and_then(|c| c.host_kind.clone()).unwrap_or_default(),
                host_is_tgt: host.and_then(|c| c.host_is_tgt).unwrap_or(false),
                tgt_missing: host.and_then(|c| c.tgt_missing).unwrap_or(false),
                pre_text_len: 0,
                pre_cleared: cl.was_empty,
                verified: cl.ok,
                set_via_js: false,
                corrected: false,
            });
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
                let reason = found.reason.as_deref().unwrap_or("not found");
                if reason == "option not found in select" {
                    // #21: the PICK failed, not the element. List the real
                    // live options so one retry succeeds, and don't raise
                    // ElementNotFound — the DOM-drift heal would retry the
                    // same losing pick and drop this list on the way.
                    let mut msg = format!("{ref_id} ({sig}) — option \"{option}\" not found");
                    let live = found.options.clone().unwrap_or_default();
                    if !live.is_empty() {
                        let hidden = found
                            .options_total
                            .unwrap_or(live.len())
                            .saturating_sub(live.len());
                        msg.push_str(&format!(". available: {}", live.join(" | ")));
                        if hidden > 0 {
                            msg.push_str(&format!(" (+{hidden} more)"));
                        }
                    } else if let Some(opts) = &el.raw.options {
                        // Fallback: the captured copy (covers frames the
                        // live read couldn't reach).
                        let (tokens, hidden) = opts.tokens(80);
                        if !tokens.is_empty() {
                            msg.push_str(&format!(". available: {}", tokens.join(" | ")));
                            if hidden > 0 {
                                msg.push_str(&format!(" (+{hidden} more)"));
                            }
                        }
                    }
                    return Err(BladeError::Other(msg));
                }
                // Element hidden / frame gone / not found — healing may
                // legitimately help; keep ElementNotFound.
                return Err(BladeError::ElementNotFound(format!(
                    "{ref_id} ({sig}) — select failed: {reason}"
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
            let steps = (((total_x.abs() + total_y.abs()) / 150.0).round() as usize)
                .clamp(4, 12);

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

                // 8-18ms jitter between steps.
                let jitter = 8 + rng.range(0, 10) as u64;
                tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
            }
        }
        Action::Read { .. } => {
            // Read is handled by handle_act directly (returns text, not delta).
            // This arm exists for exhaustiveness.
        }
        Action::Wait { condition, text, timeout } => {
            // check_condition polls until the condition is met or timeout.
            // On timeout, error so the agent gets the current page state to
            // recover (a silent "waited" would be a lie — the condition failed).
            let met = check_condition(cdp, condition, text, *timeout, None).await;
            if !met {
                return Err(BladeError::Other(format!(
                    "wait timeout: condition '{condition}' not met within {}s",
                    timeout.as_secs()
                )));
            }
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
            if dispatch_mouse_move(cdp, (cx, cy), last_mouse).await.is_err() {
                let retry = find_by_sig(cdp, sig, frame, "hover", None).await?;
                if retry.ok {
                    if let Some(b2) = retry.box_ {
                        dispatch_mouse_move(cdp, (b2[0] + b2[2] / 2.0, b2[1] + b2[3] / 2.0), last_mouse).await?;
                    }
                }
            }
            // Hover-driven dropdowns appear within ~150ms — the settle
            // quiet threshold (150ms) catches them. No extra sleep needed.
            // (Removed 300ms pre-settle sleep; settle already waits for DOM quiet.)
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

    // Action-dependent timeouts: type/clear/scroll don't trigger navigation,
    // and their DOM settles fast. Shorter nav check + shorter settle = faster.
    // A wait's condition check already settled the state it waited on; the
    // extra nav-check + settle would be a pure 150-300ms tax on every wait.
    // Other waits keep one settle (the condition can match mid-mutation).
    let (nav_check_ms, settle_ms) = match action {
        Action::Type { .. } | Action::Clear { .. } | Action::Scroll { .. } => (120, 900),
        Action::Press { .. } | Action::Select { .. } | Action::Hover { .. } => (150, 900),
        Action::Wait { condition, .. } if condition.as_str() == "settle" || condition.as_str() == "network" => (0, 0),
        Action::Wait { .. } => (0, 500),
        _ => (150, 2000),
    };

    // Check if navigation was triggered (with a short timeout).
    let nav_result = tokio::time::timeout(
        Duration::from_millis(nav_check_ms),
        sub_fires(&mut nav_sub, "Page.frameNavigated"),
    ).await;
    if let Ok(true) = nav_result {
        crate::page::wait_for_load(cdp, Duration::from_secs(10)).await?;
    }
    wait_for_settle_with_network(cdp, Duration::from_millis(settle_ms), in_flight).await?;

    // Final readback for editor actions (+ at most one bounded corrective
    // pass) before the verdict: catches late draft hydration and clears
    // whose content was restored after the action returned.
    if let (Some(mut rep), Some((sig, frame))) = (edit_report.take(), sig_frame.as_ref()) {
        finalize_edit(&mut rep, cdp, sig, frame).await?;
        edit_report = Some(rep);
    }

    // Recapture → delta.
    let cap = capture(cdp).await?;
    let delta = lpm.ingest(cap);
    // Coordinate click with no observable effect: ask the page WHAT is
    // actually at (x,y), so the verdict carries the reason (occluded point,
    // element elsewhere, outside the viewport) instead of a bare fact.
    let mut coord_hit: Option<String> = None;
    if let Action::ClickCoord { x, y } = action {
        if !delta.navigated && delta.is_empty() && !delta.content_changed {
            coord_hit = hit_probe(cdp, *x, *y).await.unwrap_or(None);
        }
    }
    let verdict = compute_verdict(action, &delta, lpm, None, edit_report.as_ref(), coord_hit.as_deref());
    Ok((delta, verdict))
}


#[cfg(test)]
mod action_tests {
    // Regression tests for the CL3 / #15 click fix: leaf-targeting of
    // container-matched controls and the no-effect diagnostic that names the
    // resolved target. These lock the message format (which consumers parse)
    // and the em-dash-free guarantee (public-facing strings use hyphens).

    #[test]
    fn no_effect_verdict_names_target_without_em_dashes() {
        let msg = super::no_effect_verdict(
            &["mouse", "js", "enter"],
            "button [Account menu] (topmost=true,disabled=false)",
        );
        assert_eq!(
            msg,
            "outcome: no-effect (click dispatched via mouse, js, enter on button [Account menu] (topmost=true,disabled=false) - no navigation, no DOM or state change)"
        );
        assert!(!msg.contains('\u{2014}'), "no em-dash in public verdict");
    }

    #[test]
    fn dom_effect_summary_distinguishes_state_only_changes() {
        use super::{dom_effect_summary, PageDelta};
        use crate::page::refs::StateChange;
        // Nothing changed → None (the no-effect path).
        assert!(dom_effect_summary(&PageDelta::default()).is_none());
        // Node change → real counts.
        let mut d = PageDelta::default();
        d.removed.push("e9".into());
        assert_eq!(dom_effect_summary(&d).unwrap(), "+0 \u{2212}1");
        // State-only change (value/checked/disabled flip) → explicitly weak:
        // this is the class that used to print a self-contradictory "(+0 -0)".
        let mut d = PageDelta::default();
        d.changed.push(("e2".into(), StateChange { value: None, disabled: None, checked: Some(true) }));
        let s = dom_effect_summary(&d).unwrap();
        assert!(s.starts_with("state-only:"), "{s}");
        assert!(s.contains("e2") && s.contains("no nodes added/removed"), "{s}");
    }

    #[test]
    fn no_effect_verdict_falls_back_when_target_unknown() {
        let msg = super::no_effect_verdict(&["mouse"], "");
        assert!(
            msg.ends_with("- no navigation, no DOM or state change; the element may be disabled, hidden, or hover-gated)"),
            "unexpected fallback: {msg}"
        );
        assert!(!msg.contains('\u{2014}'), "no em-dash in fallback verdict");
    }

    // Guards the leaf-targeting JS fragment: the box-mode resolver must stay
    // wired into the injected script, or container-matched clicks silently
    // regress to the no-effect they were built to fix.
    #[test]
    fn leaf_target_fragment_is_present_and_em_dash_free() {
        let js = super::LEAF_TARGET_JS;
        assert!(js.contains("elementFromPoint"), "leaf-target uses hit-testing");
        assert!(js.contains("querySelectorAll('button,a[href]"), "leaf-target finds native controls");
        assert!(js.contains("_lbest"), "leaf-target selects nearest candidate");
        assert!(js.contains("_lClick"), "leaf-target rewrites the click box");
        assert!(js.contains("_lhr"), "leaf-target rewrites the target label");
        assert!(!js.contains('\u{2014}'), "no em-dash in injected JS");
    }

    // The if/while absence fast path (v3.10): only a QUIET page counts as
    // confirmed absence, and only when a probe is supplied (wait passes None
    // and keeps its full time budget).
    #[test]
    fn absence_confirms_only_when_quiet_and_past_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut since = None;
        assert!(!super::absence_confirmed(&mut since, None));
        assert!(since.is_none(), "no probe must not start the absence clock");
        let counter = AtomicUsize::new(1);
        since = Some(std::time::Instant::now() - std::time::Duration::from_millis(900));
        assert!(!super::absence_confirmed(&mut since, Some(&counter)), "busy page never confirms");
        counter.store(0, Ordering::Relaxed);
        assert!(super::absence_confirmed(&mut since, Some(&counter)), "quiet + past window confirms");
    }

    // The find-by-sig script is built at runtime; a syntax error in it
    // disables every ref-targeted action at once. `node --check` guards it,
    // following the perception.rs script-check pattern (skips without node).
    #[test]
    fn find_sig_script_is_valid_js() {
        let has_node = std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping find-sig syntax check");
            return;
        }
        for (mode, text) in [
            ("box", None),
            ("prepare", None),
            ("check", None),
            ("type", Some("hello 'quoted' text")),
            ("clear", None),
            ("selhost", None),
            ("select", Some("opt")),
            ("read", None),
            ("hover", None),
        ] {
            let js = super::find_sig_expr("0|textbox|Post text|1", mode, text, &[0])
                .expect("find-sig expr builds");
            let path = std::env::temp_dir().join(format!("bladebro-findsig-{mode}.js"));
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "find-sig ({mode}) has a JS syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // The find-by-text script powers text addressing AND ref healing;
        // guard it the same way.
        for (q, rf, ih) in [
            ("Post text", None, false),
            ("Join the conversation", Some("textbox"), true),
        ] {
            let js = super::find_text_expr(q, rf, ih).expect("find-text expr builds");
            let path = std::env::temp_dir().join("bladebro-findtext.js");
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "find-text has a JS syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    // The selector matcher, the find-miss diagnostic and the coordinate
    // hit probe are injected into live pages the same way; a syntax error in
    // any of them would silently break addressing or misreport misses.
    // Guarded with node --check (skips without node).
    #[test]
    fn selector_diag_and_probe_scripts_are_valid_js() {
        let has_node = std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping selector/diag/probe syntax checks");
            return;
        }
        let cases: Vec<(&str, String)> = vec![
            ("bd-select", super::find_selector_expr("#overflow-trigger").expect("selector expr")),
            ("bd-select2", super::find_selector_expr("[role=menuitem]").expect("selector expr")),
            ("bd-missdiag", super::find_miss_expr("Open user actions \"quoted\"").expect("miss expr")),
            ("bd-hitprobe", super::hit_probe_expr(216.0, 616.5)),
        ];
        for (name, js) in cases {
            let path = std::env::temp_dir().join(format!("bladebro-{name}.js"));
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "{name} has a JS syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn key_combos_parse_and_reject_junk() {
        use super::{parse_key_combo, KeyCombo};
        let c = parse_key_combo("Control+a").unwrap().expect("combo");
        assert_eq!(c, KeyCombo { ctrl: true, alt: false, shift: false, meta: false, key: "a".into() });
        let c = parse_key_combo("Meta+Enter").unwrap().expect("combo");
        assert!(c.meta && !c.ctrl && c.key == "Enter");
        let c = parse_key_combo("ctrl+shift+z").unwrap().expect("combo");
        assert!(c.ctrl && c.shift && c.key == "z");
        // Single characters (including '+' itself) are never chords.
        assert!(parse_key_combo("a").unwrap().is_none());
        assert!(parse_key_combo("+").unwrap().is_none());
        // Malformed chords are loud, not silently mis-dispatched.
        assert!(parse_key_combo("Control+").is_err());
        assert!(parse_key_combo("Foo+a").is_err());
    }

    #[test]
    fn combo_events_shape_native_chords() {
        use super::{combo_events, parse_key_combo};
        // Ctrl+A: control down, 'a' rawKeyDown (shortcuts carry no text),
        // 'a' up, control up - modifiers as a real keyboard reports them.
        let c = parse_key_combo("Control+a").unwrap().expect("combo");
        let (evs, main) = combo_events(&c).unwrap();
        assert_eq!(evs.len(), 4);
        assert_eq!(evs[0]["key"].as_str(), Some("Control"));
        assert_eq!(evs[0]["modifiers"].as_u64(), Some(2));
        assert_eq!(evs[main]["type"].as_str(), Some("rawKeyDown"));
        assert_eq!(evs[main]["key"].as_str(), Some("a"));
        assert_eq!(evs[main]["code"].as_str(), Some("KeyA"));
        assert_eq!(evs[main]["windowsVirtualKeyCode"].as_u64(), Some(65));
        assert_eq!(evs[main]["modifiers"].as_u64(), Some(2));
        assert!(evs[main].get("text").is_none());
        assert_eq!(evs[3]["key"].as_str(), Some("Control"));
        assert_eq!(evs[3]["modifiers"].as_u64(), Some(0));
        // Shift+A types the shifted glyph.
        let c = parse_key_combo("Shift+a").unwrap().expect("combo");
        let (evs, main) = combo_events(&c).unwrap();
        assert_eq!(evs[main]["type"].as_str(), Some("keyDown"));
        assert_eq!(evs[main]["key"].as_str(), Some("A"));
        assert_eq!(evs[main]["text"].as_str(), Some("A"));
        assert_eq!(evs[main]["modifiers"].as_u64(), Some(8));
        // Meta+Enter is a shortcut: no text, meta bit set.
        let c = parse_key_combo("Meta+Enter").unwrap().expect("combo");
        let (evs, main) = combo_events(&c).unwrap();
        assert_eq!(evs[main]["key"].as_str(), Some("Enter"));
        assert_eq!(evs[main]["type"].as_str(), Some("rawKeyDown"));
        assert!(evs[main].get("text").is_none());
        assert_eq!(evs[main]["modifiers"].as_u64(), Some(4));
    }

    #[test]
    fn verdicts_never_outclaim_the_readback() {
        use super::{clear_verdict_text, type_verdict_text, EditKind, EditReport};
        let lpm = super::LivePageModel::new();
        let base = EditReport {
            kind: EditKind::Type,
            text: "hi".into(),
            final_text: "hi".into(),
            branch_text: "hi".into(),
            host_kind: "ce".into(),
            host_is_tgt: false,
            tgt_missing: false,
            pre_text_len: 4,
            pre_cleared: true,
            verified: true,
            set_via_js: false,
            corrected: false,
        };
        // Clean replace on a framework editor: honest value + where it landed.
        let v = type_verdict_text("e4", "hi", &base, &lpm);
        assert!(v.contains("value=\"hi\""), "{v}");
        assert!(v.contains("replaced 4 chars"), "{v}");
        assert!(v.contains("landed in the focused editor"), "{v}");
        // Unverified: says so, never claims a value.
        let rep = EditReport { final_text: String::new(), verified: false, ..base.clone() };
        let v = type_verdict_text("e4", "hi", &rep, &lpm);
        assert!(v.contains("unverified"), "{v}");
        assert!(!v.contains("value=\"hi\""), "{v}");
        // Late restore caught by the final readback.
        let rep = EditReport { final_text: "DRAFT hi".into(), verified: false, ..base.clone() };
        let v = type_verdict_text("e4", "hi", &rep, &lpm);
        assert!(v.contains("DRAFT hi"), "{v}");
        assert!(v.contains("late restore"), "{v}");
        // A clear that failed never reads "cleared".
        let rep = EditReport {
            kind: EditKind::Clear,
            text: String::new(),
            final_text: "abc".into(),
            branch_text: "abc".into(),
            host_is_tgt: true,
            pre_text_len: 0,
            pre_cleared: false,
            verified: false,
            ..base.clone()
        };
        let v = clear_verdict_text("e3", &rep);
        assert!(v.contains("clear failed"), "{v}");
        assert!(!v.contains("cleared e3"), "{v}");
        // Verified clear.
        let rep = EditReport {
            final_text: String::new(),
            branch_text: String::new(),
            verified: true,
            ..base.clone()
        };
        let v = clear_verdict_text("e3", &rep);
        assert!(v.contains("cleared e3 (verified empty)"), "{v}");
    }
}

#[cfg(test)]
mod pause_gate_tests {
    use super::*;

    #[test]
    fn pause_refuses_input_and_page_moves_but_not_reads() {
        // Input dispatch.
        assert!(Action::Click { ref_id: "e1".into() }.disrupts_page());
        assert!(Action::ClickCoord { x: 1.0, y: 2.0 }.disrupts_page());
        assert!(Action::Type { ref_id: "e1".into(), text: "x".into() }.disrupts_page());
        assert!(Action::Upload { ref_id: "e1".into(), path: "/tmp/x".into() }.disrupts_page());
        // Page-level moves (history/reload) — they replace what the person
        // using the browser is looking at.
        assert!(Action::Back.disrupts_page());
        assert!(Action::Forward.disrupts_page());
        assert!(Action::Reload.disrupts_page());
        // Reads and waits stay available while paused.
        assert!(!Action::Read { ref_id: "e1".into() }.disrupts_page());
        assert!(!Action::Wait {
            condition: "settle".into(),
            text: String::new(),
            timeout: std::time::Duration::from_secs(1),
        }
        .disrupts_page());
        // `injects_input` stays exactly the input set (no navigation).
        assert!(!Action::Back.injects_input());
        assert!(!Action::Reload.injects_input());
    }
}
