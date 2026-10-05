//! Post-action evidence: measured verdicts, condition checks, change probes.
//!
//! `compute_verdict` classifies what an action actually did (navigation /
//! DOM nodes / state-only / no observable change) from before/after probes;
//! `check_condition` backs `wait`/`if`/`while`; hit probes and the absence
//! confirm exist so a no-op can never be sold as success.

use serde_json::json;
use std::time::Duration;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
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
    let js = "(()=>{var d=document;var se=d.scrollingElement||d.documentElement;var out={y:se?se.scrollTop:0,max:Math.max(0,(se?se.scrollHeight:0)-(window.innerHeight||0)),el:'',elTop:null};try{var hit=d.elementFromPoint(__X__,__Y__);var hops=0;while(hit&&hops<40){if(hit!==d.body&&hit!==d.documentElement){var cs=getComputedStyle(hit);if((cs.overflowY==='auto'||cs.overflowY==='scroll'||cs.overflowY==='overlay')&&hit.scrollHeight>hit.clientHeight+1){out.el=hit.tagName.toLowerCase()+((hit.getAttribute&&hit.getAttribute('id'))?('#'+String(hit.getAttribute('id')).slice(0,24)):'');out.elTop=hit.scrollTop;break;}}hit=hit.parentElement;hops++;}}catch(e){}return out;})()";
    js.replace("__X__", &format!("{cx}"))
        .replace("__Y__", &format!("{cy}"))
}

/// Compute a one-line outcome verdict from the action + delta + click info.
/// This is the M1 verdict — every act tells the agent what happened.
pub(super) fn compute_verdict(
    action: &Action,
    delta: &PageDelta,
    lpm: &LivePageModel,
    click_via: Option<(&str, &[&str], &str)>,
    edit: Option<&EditReport>,
    coord_hit: Option<&str>,
    scroll: Option<&ScrollReport>,
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
            let needle_js =
                serde_json::to_string(&text.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
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
            let needle_js =
                serde_json::to_string(&text.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
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
pub(super) const LEAF_TARGET_JS: &str = concat!(
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
pub(super) fn no_effect_verdict(tried: &[&str], target_meta: &str) -> String {
    if target_meta.is_empty() {
        format!(
            "outcome: no-effect (click dispatched via {} - no navigation, no observable DOM or state change; the element may be disabled, hidden, or hover-gated)",
            tried.join(", ")
        )
    } else {
        format!(
            "outcome: no-effect (click dispatched via {} on {} - no navigation, no observable DOM or state change)",
            tried.join(", "),
            target_meta
        )
    }
}
