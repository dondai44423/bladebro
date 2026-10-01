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
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::page::perception::JS_PREAMBLE;
use crate::page::{capture, wait_for_settle_with_network, LivePageModel, PageDelta};

use self::edit::*;
use self::find::*;
use self::input::*;
use self::verdict::*;

mod edit;
mod find;
mod input;
mod verdict;

pub use self::find::{
    find_by_selector, find_by_text, find_miss_diag, locate_text, read_text, MissDiag, MissExample,
    SelectorDiag, SelectorLookup, TextLocation, TextMatch,
};
pub use self::verdict::check_condition;
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
    Wait {
        condition: String,
        text: String,
        timeout: Duration,
    },
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
            Action::Press { .. }
            | Action::Scroll { .. }
            | Action::Wait { .. }
            | Action::Back
            | Action::Forward
            | Action::Reload => None,
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

    // Measured scroll movement (Scroll actions): sampled around the wheel
    // burst so the verdict can report what actually moved.
    let mut scroll_report: Option<ScrollReport> = None;

    // Start listening for navigation before dispatching. Eager
    // subscribe — a lazy wait_for would miss events fired synchronously
    // during dispatch (see sub_fires).
    let mut nav_sub = cdp.subscribe();

    match action {
        Action::ClickCoord { x, y } => {
            // S10: coordinate-based click — works on cross-origin iframes,
            // canvas, shadow DOM (anything Input-domain can reach).
            let _ = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": MUT_WATCH,
                    })),
                )
                .await;
            dispatch_mouse_click(cdp, *x, *y, last_mouse).await?;
        }
        Action::Click { ref_id } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            let found = find_by_sig(cdp, sig, frame, "box", None).await?;
            if !found.ok {
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            if found.disabled == Some(true) {
                return Err(BladeError::NotInteractable(format!("{ref_id} is disabled")));
            }

            // M2: Click auto-escalation. Try mouse -> JS -> Enter until effect.
            // Install the mutation watcher first: it sees DOM effects on
            // non-actionable content that the element delta cannot.
            let _ = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": MUT_WATCH,
                    })),
                )
                .await;
            // Strategy order is role-aware (W5). Menu items get the KEYBOARD
            // activation lane first: reddit's rpl-dropdown items ignore
            // synthetic clicks (a trusted mouse click only closes the menu)
            // but activate on Space over the item's own focusable element -
            // exactly the lane a keyboard user takes. Everything else keeps
            // mouse-first with Space as the last resort (it activates native
            // buttons too).
            let role = sig.split('|').nth(1).unwrap_or("");
            let strategies: &[&str] = if role == "menuitem" {
                &["space", "js", "mouse", "enter"]
            } else if found.is_topmost == Some(true) {
                &["mouse", "js", "enter", "space"]
            } else {
                &["js", "mouse", "enter", "space"]
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
                    "js" => match find_by_sig(cdp, sig, frame, "click", None).await {
                        Ok(f) if f.ok => {}
                        Ok(_) => continue,
                        Err(e) => {
                            dispatch_errors += 1;
                            last_dispatch_err = Some(e);
                            continue;
                        }
                    },
                    "enter" => {
                        let fr = find_by_sig(cdp, sig, frame, "focus", None).await;
                        let focused = fr.map(|f| f.focused.unwrap_or(false)).unwrap_or(false);
                        if !focused {
                            continue;
                        }
                        if let Err(e) = dispatch_key(cdp, "Enter").await {
                            dispatch_errors += 1;
                            last_dispatch_err = Some(e);
                            continue;
                        }
                    }
                    "space" => {
                        // Space over the focused element - the activation key
                        // for menu items and buttons alike. Focus resolves
                        // through the item to its focusable inner element
                        // (see the 'focus' mode) and is VERIFIED: a failed
                        // focus must not press Space - the key would scroll
                        // the page (observed live) and read as an effect.
                        let fr = find_by_sig(cdp, sig, frame, "focus", None).await;
                        let focused = fr.map(|f| f.focused.unwrap_or(false)).unwrap_or(false);
                        if !focused {
                            continue;
                        }
                        if let Err(e) = dispatch_key(cdp, "Space").await {
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
                        Some(top) => {
                            tgt_meta.push_str(&format!(" (topmost=false - clicks land on {top})"))
                        }
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
            let mut verdict = compute_verdict(
                action,
                &delta,
                lpm,
                Some((via, &tried, &tgt_meta)),
                None,
                None,
                None,
            );
            if dialog_fired && !delta.navigated && delta.is_empty() && !delta.content_changed {
                verdict =
                    format!("outcome: dialog opened via {via} (auto-dismissed — see ambient)");
            }
            return Ok((delta, verdict));
        }
        Action::Type { ref_id, text } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Validate the element is typeable — prevent silently typing
            // into non-text elements (links, buttons, etc.).
            let el = lpm
                .element(ref_id)
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
            // Reddit collapsed-composer expansion (adapter-gated, best-effort,
            // W5): reddit's comment box starts as a naive strip (a shadow
            // textarea) that only exists to be CLICKED - the click expands the
            // real editor and moves focus into it. The strip then UNMOUNTS, so
            // the captured sig is dead; the hook re-derives the LIVE editor's
            // sig from the focused element (same algorithm the model uses)
            // and the flow below resolves against THAT. Any non-match is a
            // no-op and the original sig stands.
            let mut hook_sig: Option<String> = None;
            if lpm.url().contains("reddit.") {
                let is_strip = lpm
                    .element(ref_id)
                    .map(|e| e.raw.tag == "textarea" && e.raw.shadow && e.raw.role == "textbox")
                    .unwrap_or(false);
                if is_strip {
                    if let Ok(f) = find_by_sig(cdp, sig, frame, "box", None).await {
                        if let Some(bx) = f.box_ {
                            let _ = dispatch_mouse_click(
                                cdp,
                                bx[0] + bx[2] / 2.0,
                                bx[1] + bx[3] / 2.0,
                                last_mouse,
                            )
                            .await;
                            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                            if let Ok(v) = cdp
                                .send(
                                    "Runtime.evaluate",
                                    Some(json!({"expression": editor_sig_expr(), "returnByValue": true})),
                                )
                                .await
                            {
                                if let Some(s2) = v
                                    .get("result")
                                    .and_then(|r| r.get("value"))
                                    .and_then(|s| s.as_str())
                                {
                                    if !s2.is_empty() {
                                        hook_sig = Some(s2.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Resolve the rest of the flow against the hook's live-editor sig
            // when the expansion ran; otherwise the captured sig stands.
            let (sig, frame): (&str, &[usize]) = match hook_sig.as_deref() {
                Some(s) => (s, &[]),
                None => {
                    let (a, b) = sig_frame.as_ref().unwrap();
                    (a.as_str(), b.as_slice())
                }
            };
            // Focus the target. Framework composers (facade textarea + a
            // late-mounted contenteditable) mount their real editor here and
            // may move focus into it - typing then lands where a human's
            // would, and every readback below resolves the effective host.
            let focus = find_by_sig(cdp, sig, frame, "prepare", None).await?;
            let pre = check_editor(cdp, sig, frame).await;
            if !focus.ok
                && pre
                    .as_ref()
                    .and_then(|p| p.host_kind.clone())
                    .unwrap_or_default()
                    .is_empty()
            {
                // The addressed element is gone and no live editor took over.
                return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
            }
            let pre_read = pre
                .as_ref()
                .and_then(|p| p.text.clone())
                .unwrap_or_default();
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
            let mut branch_text = last
                .as_ref()
                .and_then(|c| c.text.clone())
                .unwrap_or_default();
            if norm_text(&branch_text) != want {
                for _ in 0..6u8 {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    last = check_editor(cdp, sig, frame).await;
                    branch_text = last
                        .as_ref()
                        .and_then(|c| c.text.clone())
                        .unwrap_or_default();
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
                let kind = last
                    .as_ref()
                    .and_then(|c| c.host_kind.clone())
                    .unwrap_or_default();
                if (kind == "input" || kind == "textarea")
                    && norm_text(&branch_text) == norm_text(&pre_read)
                {
                    let js = find_by_sig(cdp, sig, frame, "type", Some(text)).await?;
                    if js.ok {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        last = check_editor(cdp, sig, frame).await;
                        branch_text = last
                            .as_ref()
                            .and_then(|c| c.text.clone())
                            .unwrap_or_default();
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
            let el = lpm
                .element(ref_id)
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
            let steps = (((total_x.abs() + total_y.abs()) / 150.0).round() as usize).clamp(4, 12);

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

            // Sample the scroll state before the burst: window position +
            // any scrollable element under the wheel point.
            let probe = scroll_probe_expr(cx, cy);
            let before_v = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({"expression": probe.clone(), "returnByValue": true})),
                )
                .await
                .ok()
                .and_then(|r| r.get("result").and_then(|x| x.get("value")).cloned());

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

            // Sample again and record the measured movement for the verdict.
            let after_v = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({"expression": probe, "returnByValue": true})),
                )
                .await
                .ok()
                .and_then(|r| r.get("result").and_then(|x| x.get("value")).cloned());
            let jnum = |v: &Option<serde_json::Value>, k: &str| {
                v.as_ref().and_then(|o| o.get(k)).and_then(|n| n.as_f64())
            };
            // Both samples must be real measurements: a failed probe must not
            // degrade into zeroes, or the verdict would say "already at the
            // top" with no measurement behind it - exactly the class this
            // report exists to kill. Unmeasured stays unmeasured (None arm).
            if let (Some(before_y), Some(after_y)) = (jnum(&before_v, "y"), jnum(&after_v, "y")) {
                scroll_report = Some(ScrollReport {
                    before_y,
                    after_y,
                    max_y: jnum(&after_v, "max").unwrap_or(0.0),
                    scroller: after_v
                        .as_ref()
                        .and_then(|o| o.get("el"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    scroller_before: jnum(&before_v, "elTop"),
                    scroller_after: jnum(&after_v, "elTop"),
                });
            }
        }
        Action::Read { .. } => {
            // Read is handled by handle_act directly (returns text, not delta).
            // This arm exists for exhaustiveness.
        }
        Action::Wait {
            condition,
            text,
            timeout,
        } => {
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
            cdp.send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": "window.history.back()",
                    "returnByValue": true,
                })),
            )
            .await?;
        }
        Action::Forward => {
            cdp.send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": "window.history.forward()",
                    "returnByValue": true,
                })),
            )
            .await?;
        }
        Action::Reload => {
            // Page.reload (CDP) is a real reload: bypasses bfcache,
            // re-fetches resources. ignoreCache=false keeps it a
            // normal F5, not a hard reload.
            cdp.send(
                "Page.reload",
                Some(json!({
                    "ignoreCache": false,
                })),
            )
            .await?;
        }
        Action::Hover { ref_id } => {
            let (sig, frame) = sig_frame.as_ref().unwrap();
            // Install the mutation watcher FIRST — hover menus
            // and tooltips mutate the DOM, and we want the delta
            // to reflect what the hover actually revealed.
            let _ = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": MUT_WATCH,
                    })),
                )
                .await;
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
            if dispatch_mouse_move(cdp, (cx, cy), last_mouse)
                .await
                .is_err()
            {
                let retry = find_by_sig(cdp, sig, frame, "hover", None).await?;
                if retry.ok {
                    if let Some(b2) = retry.box_ {
                        dispatch_mouse_move(
                            cdp,
                            (b2[0] + b2[2] / 2.0, b2[1] + b2[3] / 2.0),
                            last_mouse,
                        )
                        .await?;
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
            let el = lpm
                .element(ref_id)
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
            let res = cdp
                .send(
                    "Runtime.evaluate",
                    Some(serde_json::json!({
                        "expression": expr,
                        "returnByValue": false,
                    })),
                )
                .await?;
            let object_id = res
                .get("result")
                .and_then(|r| r.get("objectId"))
                .and_then(|o| o.as_str());
            let object_id = match object_id {
                Some(id) => id.to_string(),
                None => {
                    return Err(BladeError::Other(format!(
                        "could not get objectId for file input {ref_id}"
                    )))
                }
            };
            // Set the file on the input. DOM.setFileInputFiles accepts
            // objectId directly — no need to convert to nodeId (which can
            // go stale if the DOM tree hasn't been explicitly requested).
            cdp.send(
                "DOM.setFileInputFiles",
                Some(serde_json::json!({
                    "objectId": object_id,
                    "files": [path],
                })),
            )
            .await?;
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
        Action::Wait { condition, .. }
            if condition.as_str() == "settle" || condition.as_str() == "network" =>
        {
            (0, 0)
        }
        Action::Wait { .. } => (0, 500),
        _ => (150, 2000),
    };

    // Check if navigation was triggered (with a short timeout).
    let nav_result = tokio::time::timeout(
        Duration::from_millis(nav_check_ms),
        sub_fires(&mut nav_sub, "Page.frameNavigated"),
    )
    .await;
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
    let verdict = compute_verdict(
        action,
        &delta,
        lpm,
        None,
        edit_report.as_ref(),
        coord_hit.as_deref(),
        scroll_report.as_ref(),
    );
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
            "outcome: no-effect (click dispatched via mouse, js, enter on button [Account menu] (topmost=true,disabled=false) - no navigation, no observable DOM or state change)"
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
        d.changed.push((
            "e2".into(),
            StateChange {
                value: None,
                disabled: None,
                checked: Some(true),
            },
        ));
        let s = dom_effect_summary(&d).unwrap();
        assert!(s.starts_with("state-only:"), "{s}");
        assert!(
            s.contains("e2") && s.contains("no nodes added/removed"),
            "{s}"
        );
    }

    #[test]
    fn no_effect_verdict_falls_back_when_target_unknown() {
        let msg = super::no_effect_verdict(&["mouse"], "");
        assert!(
            msg.ends_with("- no navigation, no observable DOM or state change; the element may be disabled, hidden, or hover-gated)"),
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
        assert!(
            js.contains("elementFromPoint"),
            "leaf-target uses hit-testing"
        );
        assert!(
            js.contains("querySelectorAll('button,a[href]"),
            "leaf-target finds native controls"
        );
        assert!(
            js.contains("_lbest"),
            "leaf-target selects nearest candidate"
        );
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
        assert!(
            !super::absence_confirmed(&mut since, Some(&counter)),
            "busy page never confirms"
        );
        counter.store(0, Ordering::Relaxed);
        assert!(
            super::absence_confirmed(&mut since, Some(&counter)),
            "quiet + past window confirms"
        );
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
            ("click", None),
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
            (
                "bd-select",
                super::find_selector_expr("#overflow-trigger").expect("selector expr"),
            ),
            (
                "bd-select2",
                super::find_selector_expr("[role=menuitem]").expect("selector expr"),
            ),
            (
                "bd-missdiag",
                super::find_miss_expr("Open user actions \"quoted\"").expect("miss expr"),
            ),
            ("bd-hitprobe", super::hit_probe_expr(216.0, 616.5)),
            (
                "bd-textloc",
                super::text_locate_expr("are you sure? yes").expect("text-locate expr"),
            ),
            ("bd-scrollprobe", super::scroll_probe_expr(480.0, 270.0)),
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
        assert_eq!(
            c,
            KeyCombo {
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
                key: "a".into()
            }
        );
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
        let rep = EditReport {
            final_text: String::new(),
            verified: false,
            ..base.clone()
        };
        let v = type_verdict_text("e4", "hi", &rep, &lpm);
        assert!(v.contains("unverified"), "{v}");
        assert!(!v.contains("value=\"hi\""), "{v}");
        // Late restore caught by the final readback.
        let rep = EditReport {
            final_text: "DRAFT hi".into(),
            verified: false,
            ..base.clone()
        };
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
        assert!(Action::Click {
            ref_id: "e1".into()
        }
        .disrupts_page());
        assert!(Action::ClickCoord { x: 1.0, y: 2.0 }.disrupts_page());
        assert!(Action::Type {
            ref_id: "e1".into(),
            text: "x".into()
        }
        .disrupts_page());
        assert!(Action::Upload {
            ref_id: "e1".into(),
            path: "/tmp/x".into()
        }
        .disrupts_page());
        // Page-level moves (history/reload) — they replace what the person
        // using the browser is looking at.
        assert!(Action::Back.disrupts_page());
        assert!(Action::Forward.disrupts_page());
        assert!(Action::Reload.disrupts_page());
        // Reads and waits stay available while paused.
        assert!(!Action::Read {
            ref_id: "e1".into()
        }
        .disrupts_page());
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
pub(crate) use self::input::dispatch_mouse_click;
