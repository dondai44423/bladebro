//! Post-action evidence: measured verdicts, condition checks, change probes.
//!
//! `compute_verdict` classifies what an action actually did (navigation /
//! DOM nodes / state-only / no observable change) from before/after probes;
//! `eval_condition` backs `wait`/`if`/`while`; hit probes and the absence
//! confirm exist so a no-op can never be sold as success.

use serde_json::json;
use std::time::Duration;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::page::perception::{JS_PREAMBLE, JS_READ_TREE};
use crate::page::{wait_for_settle, LivePageModel, PageDelta};

use super::edit::{clear_verdict_text, type_verdict_text, EditReport};
use super::Action;

/// Build the coordinate hit-probe script: what element actually sits at
/// viewport (x, y) - descending open shadow roots, since
/// `document.elementFromPoint` returns the shadow HOST.
pub(super) fn hit_probe_expr(x: f64, y: f64) -> String {
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

/// G09 coordinate-click toggle probe: the observable toggle state of the
/// element under (x, y) - descending open shadow roots, then walking composed
/// ancestors (a click on a label's inner span still flips the label's own
/// control). Null when nothing toggle-like sits under the point.
pub(super) fn coord_toggle_expr(x: f64, y: f64) -> String {
    "((x,y)=>{const d=document;if(!d)return null;"
        .to_string()
        + &JS_PREAMBLE
        + "let el=null;try{el=d.elementFromPoint(x,y);}catch(e){}if(!el)return null;"
        + "while(el&&el.shadowRoot){let inner=null;try{inner=el.shadowRoot.elementFromPoint(x,y);}catch(e){}if(!inner||inner===el)break;el=inner;}"
        + "for(let i=0;i<40&&el;i++){const st=stateOf(el);if(st)return st;el=el.parentElement||(el.getRootNode&&el.getRootNode().host)||null;}"
        + "return null;})("
        + &format!("{x},{y})")
}

/// Run [`coord_toggle_expr`]; best-effort (None on any hiccup or when the
/// point carries no toggle state).
pub(super) async fn coord_toggle_probe(cdp: &CdpSession, x: f64, y: f64) -> Option<String> {
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": coord_toggle_expr(x, y),
                "returnByValue": true,
            })),
        )
        .await
        .ok()?;
    res.get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Summarize a delta's DOM evidence. Node changes read as `+2 \u{2212}1`
/// (strong evidence a handler ran). State-only changes (surviving elements
/// whose value/checked/disabled flipped) read as `state-only (...)` - they
/// are attribute-level and can be focus/hover paint, so a verdict must not
/// sell them as a real effect. None = nothing observable changed.
pub(super) fn dom_effect_summary(delta: &PageDelta) -> Option<String> {
    if !delta.added.is_empty() || !delta.removed.is_empty() {
        Some(format!(
            "+{} \u{2212}{}",
            delta.added.len(),
            delta.removed.len()
        ))
    } else if !delta.changed.is_empty() {
        let refs: Vec<&str> = delta
            .changed
            .iter()
            .take(3)
            .map(|(r, _)| r.as_str())
            .collect();
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
/// Measured evidence of what a scroll actually moved: window scroller plus
/// the scrollable element under the wheel point, sampled before and after
/// the burst. The verdict uses it to stop claiming "scrolled" on a no-op
/// (a wheel burst at the page bottom reported success and burned agent
/// steps doubting the tool).
#[derive(Debug, Clone, Default)]
pub struct ScrollReport {
    pub before_y: f64,
    pub after_y: f64,
    pub max_y: f64,
    /// Descriptor of the scrollable element under the wheel point ("" when
    /// the point is over the page only).
    pub scroller: String,
    pub scroller_before: Option<f64>,
    pub scroller_after: Option<f64>,
}

/// Build the scroll-position probe: window scroller state + the deepest
/// scrollable element under (cx, cy). One evaluate, best-effort.
pub(super) fn scroll_probe_expr(cx: f64, cy: f64) -> String {
    let js = "(()=>{var d=document;var se=d.scrollingElement||d.documentElement;var out={y:se?se.scrollTop:0,max:Math.max(0,(se?se.scrollHeight:0)-(window.innerHeight||0)),el:'',elTop:null};try{var hit=d.elementFromPoint(__X__,__Y__);var hops=0;while(hit&&hops<40){if(hit!==d.body&&hit!==d.documentElement){var cs=getComputedStyle(hit);if((cs.overflowY==='auto'||cs.overflowY==='scroll'||cs.overflowY==='overlay')&&hit.scrollHeight>hit.clientHeight+1){out.el=hit.tagName.toLowerCase()+((hit.getAttribute&&hit.getAttribute('id'))?('#'+String(hit.getAttribute('id')).slice(0,24)):'');out.elTop=hit.scrollTop;break;}}hit=hit.parentElement||(hit.getRootNode&&hit.getRootNode().host)||null;hops++;}}catch(e){}return out;})()";
    js.replace("__X__", &format!("{cx}"))
        .replace("__Y__", &format!("{cy}"))
}

/// G01/G09: observable toggle state (checked / aria-selected / aria-checked)
/// sampled around a click dispatch. `changed` requires both sides; a
/// one-sided probe still feeds the "targeted state unchanged (…)" note.
#[derive(Debug, Clone, Default)]
pub struct ToggleProbe {
    pub before: Option<String>,
    pub after: Option<String>,
}

impl ToggleProbe {
    pub(super) fn changed(&self) -> bool {
        match (&self.before, &self.after) {
            (Some(b), Some(a)) => a != b,
            _ => false,
        }
    }
}

/// Click evidence threaded into the verdict: the activation lane + attempts +
/// resolved target (ref clicks), the element under the point (coord clicks),
/// whether any lane actually dispatched (R2), and the observable toggle state
/// around the dispatch (G01/G09).
#[derive(Default)]
pub struct ClickEvidence<'a> {
    pub via: Option<(&'a str, &'a [&'a str], &'a str)>,
    pub coord_hit: Option<&'a str>,
    pub toggle: Option<&'a ToggleProbe>,
    /// True when a click was actually dispatched (mouse/js/enter/space/coord).
    pub dispatched: bool,
}

/// Compute a one-line outcome verdict from the action + delta + click info.
/// This is the M1 verdict — every act tells the agent what happened.
pub(super) fn compute_verdict(
    action: &Action,
    delta: &PageDelta,
    lpm: &LivePageModel,
    click: ClickEvidence<'_>,
    edit: Option<&EditReport>,
    scroll: Option<&ScrollReport>,
) -> String {
    let (click_via, coord_hit, toggle, dispatched) =
        (click.via, click.coord_hit, click.toggle, click.dispatched);
    match action {
        Action::ClickCoord { x, y } => {
            if delta.navigated {
                format!("outcome: navigated \u{2192} {} via coord-click({x:.0},{y:.0})", shorten_url(&delta.url))
            } else if let Some(eff) = dom_effect_summary(delta) {
                let note = match toggle.filter(|t| t.changed()) {
                    Some(t) => format!(
                        " - control state {} -> {}",
                        t.before.as_deref().unwrap_or("?"),
                        t.after.as_deref().unwrap_or("?")
                    ),
                    None => String::new(),
                };
                format!("outcome: dom-changed ({eff}) via coord-click({x:.0},{y:.0}){note}")
            } else if delta.content_changed {
                format!("outcome: dom-changed (content-only) via coord-click({x:.0},{y:.0})")
            } else if toggle.map(|t| t.changed()).unwrap_or(false) {
                // G09: the click flipped a control under the point (a visible
                // label toggling its hidden checkbox) with no visible DOM
                // change - the measured state IS the effect.
                let t = toggle.unwrap();
                format!(
                    "outcome: state changed ({} -> {}) via coord-click({x:.0},{y:.0}) - no visible DOM change",
                    t.before.as_deref().unwrap_or("?"),
                    t.after.as_deref().unwrap_or("?")
                )
            } else {
                let state_note = toggle
                    .and_then(|t| t.after.as_deref())
                    .map(|s| format!("; targeted state unchanged ({s})"))
                    .unwrap_or_default();
                match coord_hit {
                    Some(h) => format!(
                        "outcome: clicked (no observable DOM change) via coord-click at {x:.0},{y:.0}{state_note}; topmost there: {h}"
                    ),
                    None => format!(
                        "outcome: clicked (no observable DOM change) via coord-click at {x:.0},{y:.0}{state_note}"
                    ),
                }
            }
        }
        Action::Click { .. } => {
            let (via, tried, tgt_meta) = click_via.unwrap_or(("", &[][..], ""));
            if delta.navigated {
                format!("outcome: navigated \u{2192} {} via {}", shorten_url(&delta.url), via)
            } else if let Some(eff) = dom_effect_summary(delta) {
                let note = match toggle.filter(|t| t.changed()) {
                    Some(t) => format!(
                        " - control state {} -> {}",
                        t.before.as_deref().unwrap_or("?"),
                        t.after.as_deref().unwrap_or("?")
                    ),
                    None => String::new(),
                };
                format!("outcome: dom-changed ({eff}) via {via}{note}")
            } else if delta.content_changed {
                // Mutation watcher saw DOM effects on non-actionable
                // content - text swaps, counters, live regions.
                format!("outcome: dom-changed (content-only) via {via}")
            } else if toggle.map(|t| t.changed()).unwrap_or(false) {
                // G09: the page showed no visible change, but the target's
                // own state flipped (a hidden checkbox toggled through its
                // label, a styled radio selected). That IS the effect.
                let t = toggle.unwrap();
                format!(
                    "outcome: state changed ({} -> {}) via {via} - no visible DOM change",
                    t.before.as_deref().unwrap_or("?"),
                    t.after.as_deref().unwrap_or("?")
                )
            } else if !dispatched {
                // R2: nothing was dispatched at all (every lane skipped: no
                // box, no verified focus) - the one case that earns the word
                // "no-effect". A retry is safe and may be the right move.
                no_effect_verdict(tried, tgt_meta, toggle.and_then(|t| t.after.as_deref()))
            } else if tgt_meta.is_empty() {
                clicked_quiet_verdict(tried, "", toggle.and_then(|t| t.after.as_deref()))
            } else {
                // The resolved click target is named so consumers can tell a
                // wrong-target / avenue problem from a page that rejected a
                // well-aimed click.
                clicked_quiet_verdict(tried, tgt_meta, toggle.and_then(|t| t.after.as_deref()))
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
        Action::Scroll { dx, dy } => match scroll {
            // Report MEASURED movement. The old unconditional "scrolled
            // (dx, dy)" claimed success when the wheel hit a boundary and
            // nothing moved - agents read the lie as a broken tool.
            Some(s) => {
                let moved = s.after_y - s.before_y;
                let el_moved = match (s.scroller_before, s.scroller_after) {
                    (Some(a), Some(b)) => b - a,
                    _ => 0.0,
                };
                if moved.abs() >= 1.0 {
                    format!(
                        "outcome: scrolled page delta{moved:+.0} (y {:.0} -> {:.0})",
                        s.before_y, s.after_y
                    )
                } else if el_moved.abs() >= 1.0 {
                    let who = if s.scroller.is_empty() {
                        "nested scroller".to_string()
                    } else {
                        s.scroller.clone()
                    };
                    format!(
                        "outcome: scrolled {who} delta{el_moved:+.0} (page unchanged at y {:.0})",
                        s.after_y
                    )
                } else if s.max_y > 1.0 && (s.after_y - s.max_y).abs() < 2.0 {
                    format!(
                        "outcome: no movement - page already at the bottom (y {:.0} of {:.0})",
                        s.after_y, s.max_y
                    )
                } else if s.after_y <= 0.5 {
                    "outcome: no movement - page already at the top".to_string()
                } else {
                    format!(
                        "outcome: no movement - wheel dispatched, nothing scrolled (y {:.0} of {:.0})",
                        s.after_y, s.max_y
                    )
                }
            }
            // Probe failed on this burst: claim the dispatch, not movement.
            None => format!(
                "outcome: scroll dispatched ({dx}, {dy}) - movement not measured (probe unavailable)"
            ),
        },
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

pub(super) fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('\u{2026}');
        t
    }
}

pub(super) fn shorten_url(u: &str) -> String {
    let s = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
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
pub(super) const ABSENCE_CONFIRM: Duration = Duration::from_millis(800);

/// See [`ABSENCE_CONFIRM`]. `probe = None` disables the fast path (used by
/// the `wait` action, whose timeout is a deliberate wait budget).
pub(super) fn absence_confirmed(
    absent_since: &mut Option<std::time::Instant>,
    probe: Option<&std::sync::atomic::AtomicUsize>,
) -> bool {
    let Some(counter) = probe else {
        return false;
    };
    let since = *absent_since.get_or_insert_with(std::time::Instant::now);
    since.elapsed() >= ABSENCE_CONFIRM && counter.load(std::sync::atomic::Ordering::Relaxed) == 0
}

/// Element conditions traverse visible light DOM, shadows and same-origin
/// frames. A control shown in the model must not read as absent in a branch.
pub(crate) fn element_condition_expr(needle: &str) -> String {
    let needle_js =
        serde_json::to_string(&needle.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
    "(()=>{const d=document;if(!d||!d.body)return false;"
        .to_string()
        + &JS_PREAMBLE
        + JS_READ_TREE
        + "const t="
        + &needle_js
        + ";let found=false;walkReadTree(d.body,n=>{if(n.nodeType!==1||!n.matches(sel)||!vis(n))return;const r=(n.getAttribute('role')||n.tagName.toLowerCase());const nm=(n.getAttribute('aria-label')||n.innerText||n.placeholder||'').trim();if(r.toLowerCase().includes(t)||nm.toLowerCase().includes(t)){found=true;return false;}});return found;})()"
}

/// Text conditions use rendered light-DOM text first; the bounded fallback
/// traverses visible shadow/iframe text, never script/style or hidden content.
pub(crate) fn text_condition_expr(needle: &str) -> String {
    let needle_js =
        serde_json::to_string(&needle.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
    "(()=>{const d=document;if(!d||!d.body)return false;const nd="
        .to_string()
        + &needle_js
        + ";if((d.body.innerText||'').toLowerCase().includes(nd))return true;"
        + JS_READ_TREE
        + "const parts=[];walkReadTree(d.body,n=>{if(n.nodeType===3)parts.push(n.textContent);});return parts.join(' ').replace(/\\s+/g,' ').toLowerCase().includes(nd.replace(/\\s+/g,' '));})()"
}

/// Outcome of an [`eval_condition`] run (G02).
#[derive(Debug)]
pub enum CondOutcome {
    /// The condition was met within the timeout.
    Met,
    /// The timeout elapsed without the condition being met.
    Timeout,
    /// The condition cannot be evaluated deterministically: a js expression
    /// with a syntax error, or one that threw on every attempt. The message
    /// carries the underlying error — callers surface it instead of letting
    /// it masquerade as a plain timeout.
    Error(String),
}

/// First line + syntax classification of a Runtime.evaluate exception.
fn js_exception(details: &serde_json::Value) -> Option<(bool, String)> {
    let class = details
        .get("exception")
        .and_then(|e| e.get("className"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let desc = details
        .get("exception")
        .and_then(|e| e.get("description"))
        .and_then(|d| d.as_str())
        .or_else(|| details.get("text").and_then(|t| t.as_str()))
        .unwrap_or("unknown js error");
    let first = desc.lines().next().unwrap_or(desc).to_string();
    Some((
        class == "SyntaxError" || first.starts_with("SyntaxError"),
        first,
    ))
}

/// The one condition evaluator behind `wait`, `if`, `while` and `wait+else`
/// (G02). Polls until the condition is met or `timeout` elapses.
///
/// Conditions:
/// - `"title"`: page title contains `text` (case-insensitive).
/// - `"element"`: a visible actionable element whose role or name contains
///   `text` exists in the DOM.
/// - `"url"`: current URL contains `text` (case-insensitive).
/// - `"text"`: rendered page text contains `text` (case-insensitive).
/// - `"js"`: `text` evaluated as an expression is truthy. A SYNTAX error can
///   never become true and surfaces immediately ([`CondOutcome::Error`]); a
///   runtime throw before hydration is polled through — if the wait ends
///   with throws and no success, the last error is reported in the failure.
/// - `"settle"` / `"network"`: wait for DOM/network quiet; always met.
/// - anything else: not met (the agent sees an honest timeout, never a
///   false positive).
///
/// `Err` carries transport failures; `Closed` propagates so the MCP
/// self-heal can fire.
pub async fn eval_condition(
    cdp: &CdpSession,
    condition: &str,
    text: &str,
    timeout: Duration,
    absence_probe: Option<&std::sync::atomic::AtomicUsize>,
) -> Result<CondOutcome> {
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
                    // browser died — bail instead of spinning the full timeout
                    return Err(BladeError::Closed);
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
                    return Ok(CondOutcome::Met);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(CondOutcome::Timeout);
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return Ok(CondOutcome::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "element" => {
            let check = element_condition_expr(text);
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
                    return Err(BladeError::Closed);
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
                    return Ok(CondOutcome::Met);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(CondOutcome::Timeout);
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return Ok(CondOutcome::Timeout);
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
                    return Err(BladeError::Closed);
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
                    return Ok(CondOutcome::Met);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(CondOutcome::Timeout);
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return Ok(CondOutcome::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "text" => {
            let expr = text_condition_expr(text);
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
                    return Err(BladeError::Closed);
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
                    return Ok(CondOutcome::Met);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(CondOutcome::Timeout);
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return Ok(CondOutcome::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "js" => {
            // A js wait ends one of four ways: truthy (met), a syntax error
            // (deterministic — reported immediately), throws + deadline
            // (reported WITH the last throw), or plain falsy + deadline.
            let mut last_throw: Option<String> = None;
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
                    return Err(BladeError::Closed);
                }
                let resp = res.ok();
                match resp
                    .as_ref()
                    .and_then(|r| r.get("exceptionDetails"))
                    .and_then(js_exception)
                {
                    Some((true, desc)) => {
                        // A syntax error can never become true — failing fast
                        // with the real reason beats a timeout that teaches
                        // nothing.
                        return Ok(CondOutcome::Error(format!(
                            "js expression is not valid JavaScript ({})",
                            clip(&desc, 160)
                        )));
                    }
                    Some((false, desc)) => last_throw = Some(desc),
                    None => {
                        let truthy = resp
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
                            return Ok(CondOutcome::Met);
                        }
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(match last_throw {
                        Some(t) => CondOutcome::Error(format!(
                            "js expression never became truthy and threw - last error: {}",
                            clip(&t, 160)
                        )),
                        None => CondOutcome::Timeout,
                    });
                }
                // v3.10: confirmed-absence fast path — `if`/`while` pass the
                // in-flight counter as the probe; `wait` passes None.
                if absence_confirmed(&mut absent_since, absence_probe) {
                    return Ok(match last_throw {
                        Some(t) => CondOutcome::Error(format!(
                            "js expression never became truthy and threw - last error: {}",
                            clip(&t, 160)
                        )),
                        None => CondOutcome::Timeout,
                    });
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        "settle" | "network" => {
            let _ = wait_for_settle(cdp, timeout).await;
            Ok(CondOutcome::Met)
        }
        _ => {
            // Unknown condition name — not met, so the agent sees an honest
            // timeout instead of a false positive.
            Ok(CondOutcome::Timeout)
        }
    }
}

/// Box-mode leaf-targeting: a text-resolved match is often a container (nav,
/// row, role=button wrapper) whose geometric center is empty. If the center
/// point does not land on a native control, find the nearest exposed native
/// control inside the match and click there instead. CL3 fix: the YouTube
/// account-menu / nav-wrapper no-effect bug (#15).
pub(super) const LEAF_TARGET_JS: &str = concat!(
    "var _lClick=_lbx(n);var _lht=(n.getAttribute&&(n.getAttribute('aria-label')||n.getAttribute('title')))||'';var _lhr=(n.getAttribute&&n.getAttribute('role'))||n.tagName.toLowerCase();",
    "var _ltopN=doc.elementFromPoint(cx,cy);var _lNative=_lnb(_ltopN);",
    "if(!_lNative){var _lbest=null,_lbd=Infinity,_lcands=[];try{_lcands=deepAll(n,'button,a[href],input:not([type=hidden]),select,textarea');}catch(e){}",
    "for(var _lk=0;_lk<_lcands.length;_lk++){var _lc=_lcands[_lk];var _lrc=_lc.getBoundingClientRect();if(_lrc.width<2||_lrc.height<2)continue;var _lbn=n.getBoundingClientRect();if(!(_lrc.left<=_lbn.right&&_lrc.right>=_lbn.left))continue;var _lccx=_lrc.x+_lrc.width/2,_lccy=_lrc.y+_lrc.height/2;var _ltc=doc.elementFromPoint(_lccx,_lccy);if(!(_ltc===_lc||(_lc.contains&&_lc.contains(_ltc))))continue;var _ldd=Math.hypot(_lccx-cx,_lccy-cy);if(_ldd<_lbd){_lbd=_ldd;_lbest=_lc;}}",
    "if(_lbest){_lClick=_lbx(_lbest);_lht=(_lbest.getAttribute&&(_lbest.getAttribute('aria-label')||_lbest.textContent||_lbest.getAttribute('title')))||'';_lhr=(_lbest.getAttribute&&_lbest.getAttribute('role'))||_lbest.tagName.toLowerCase();}}",
);

/// Build the verdict for a click where NO activation lane dispatched (every
/// lane was skipped: no box, no verified focus). "no-effect" is reserved for
/// exactly this case (R2) - the dispatch never happened, so a retry is safe.
/// Names the resolved click target so consumers can tell a wrong-target /
/// avenue problem from a page that rejected a well-aimed click.
pub(super) fn no_effect_verdict(
    tried: &[&str],
    target_meta: &str,
    state_after: Option<&str>,
) -> String {
    let state_note = state_after
        .map(|s| format!("; targeted state unchanged ({s})"))
        .unwrap_or_default();
    if target_meta.is_empty() {
        format!(
            "outcome: no-effect (no click was dispatched: tried {}{state_note} - every activation lane was skipped)",
            tried.join(", ")
        )
    } else {
        format!(
            "outcome: no-effect (no click was dispatched: tried {} on {}{state_note} - every activation lane was skipped)",
            tried.join(", "),
            target_meta
        )
    }
}

/// Build the verdict for a click that DID dispatch but produced no observable
/// DOM change (R2). The dispatch is a fact, so the verdict says "clicked" -
/// the quiet DOM is reported as such instead of reading like the click
/// failed. Carries the dispatch-once caution: a blind re-click may be a
/// duplicate submit. The target meta doubles as the occlusion diagnostic.
pub(super) fn clicked_quiet_verdict(
    tried: &[&str],
    target_meta: &str,
    state_after: Option<&str>,
) -> String {
    let state_note = state_after
        .map(|s| format!("; targeted state unchanged ({s})"))
        .unwrap_or_default();
    if target_meta.is_empty() {
        format!(
            "outcome: clicked (no observable DOM change) via {}{state_note} - dispatched once; the element may be disabled, occluded, or hover-gated",
            tried.join(", ")
        )
    } else {
        format!(
            "outcome: clicked (no observable DOM change) via {} on {}{state_note} - dispatched once; verify state before re-clicking",
            tried.join(", "),
            target_meta
        )
    }
}
