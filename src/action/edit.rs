//! Verified text editing — type/clear with readbacks.
//!
//! Every rung of the clear ladder and every type is read back from the live
//! editing host (`EditReport`/`ClearResult`); the verdict builders render
//! those readbacks and never claim a value or a clear without one.

use std::time::Duration;

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;
use crate::page::{perception::JS_PREAMBLE, LivePageModel};

use super::find::{find_by_sig, FoundElement};
use super::input::{dispatch_combo, dispatch_key, type_per_char, KeyCombo};
use super::verdict::clip;

/// Kind of edit an `EditReport` describes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum EditKind {
    Type,
    Clear,
}

/// What actually happened to an edited field - the authority behind the
/// verdict. The Type/Clear arms fill it; `finalize_edit` re-reads after
/// settle (and runs at most one bounded corrective pass); `compute_verdict`
/// formats it. Every note in the verdict must be backed by a readback here.
#[derive(Debug, Clone)]
pub(super) struct EditReport {
    pub(super) kind: EditKind,
    /// Typed text (empty for clears).
    pub(super) text: String,
    /// Readback of the effective editing host at verdict time.
    pub(super) final_text: String,
    /// Readback right after the action (diffed against `final_text`).
    pub(super) branch_text: String,
    /// "ce" | "input" | "textarea" | "" - host kind at the last read.
    pub(super) host_kind: String,
    /// The host IS the addressed element (no framework redirect).
    pub(super) host_is_tgt: bool,
    /// The addressed element is gone (framework remounted it); the host
    /// data still describes the live editor.
    pub(super) tgt_missing: bool,
    /// Chars the field held before a replace-clear ran.
    pub(super) pre_text_len: usize,
    /// The field was empty (or verified-cleared) before typing.
    pub(super) pre_cleared: bool,
    /// Exact match (type) / empty (clear) at the last readback.
    pub(super) verified: bool,
    /// A JS setter wrote the value because key events did not register.
    pub(super) set_via_js: bool,
    /// A corrective pass ran (late content / failed clear retried once).
    pub(super) corrected: bool,
}

/// Outcome of a verified clear.
pub(super) struct ClearResult {
    /// The host read empty at the last readback.
    pub(super) ok: bool,
    /// Last readback of the host.
    pub(super) text: String,
    /// The host was already empty (nothing to do).
    pub(super) was_empty: bool,
    /// Neither the addressed element nor a live editor host was reachable.
    pub(super) missing: bool,
}

/// Normalize text for readback comparison (mirrors the JS `_tvr`).
pub(super) fn norm_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Readback of the effective editing host via the "check" mode. `None`
/// when the eval itself fails - callers treat that as "unverified", never
/// as "empty".
pub(super) async fn check_editor(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
) -> Option<FoundElement> {
    find_by_sig(cdp, sig, frame, "check", None)
        .await
        .ok()
        .filter(|f| f.ok)
}

/// Clear the addressed element's effective editing host, verified at every
/// rung. Ladder: JS setter (value fields) -> trusted Ctrl+A + Backspace ->
/// Ctrl+A carrying the selectAll editing command -> JS range-select +
/// trusted Backspace -> execCommand. The result carries the final readback
/// so no caller can claim a clear that did not happen.
pub(super) async fn clear_editable(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
) -> Result<ClearResult> {
    // Focus first: framework editors mount and take focus here, and the
    // trusted-key rungs act on whatever is focused.
    let _ = find_by_sig(cdp, sig, frame, "focus", None).await;
    let mut cur = check_editor(cdp, sig, frame).await;
    let Some(first) = cur.as_ref() else {
        return Ok(ClearResult {
            ok: false,
            text: String::new(),
            was_empty: false,
            missing: true,
        });
    };
    let mut text = first.text.clone().unwrap_or_default();
    if text.is_empty() {
        return Ok(ClearResult {
            ok: true,
            text,
            was_empty: true,
            missing: false,
        });
    }
    let kind = first.host_kind.clone().unwrap_or_default();
    if kind == "input" || kind == "textarea" || kind == "select" {
        // Fast path for value fields: native setter + input/change events
        // (React/Vue compatible), then readback.
        let _ = find_by_sig(cdp, sig, frame, "clear", None).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        cur = check_editor(cdp, sig, frame).await;
        text = cur
            .as_ref()
            .and_then(|c| c.text.clone())
            .unwrap_or_default();
        if text.is_empty() {
            return Ok(ClearResult {
                ok: true,
                text,
                was_empty: false,
                missing: false,
            });
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
                return Ok(ClearResult {
                    ok: true,
                    text,
                    was_empty: false,
                    missing: false,
                });
            }
        }
    }
    // Legacy editors: the execCommand path, then a final readback.
    let _ = find_by_sig(cdp, sig, frame, "clear", None).await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    cur = check_editor(cdp, sig, frame).await;
    text = cur
        .as_ref()
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
    Ok(ClearResult {
        ok: text.is_empty(),
        text,
        was_empty: false,
        missing: false,
    })
}

/// Rung helper: trusted Ctrl+A (optionally carrying the selectAll editing
/// command) + trusted Backspace. `Some(readback)` when a selection was
/// made and deleted; `None` when the shortcut did not select anything.
pub(super) async fn trusted_select_all(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
    with_cmd: bool,
) -> Result<Option<String>> {
    let combo = KeyCombo {
        ctrl: true,
        alt: false,
        shift: false,
        meta: false,
        key: "a".to_string(),
    };
    let cmds = if with_cmd {
        Some(vec!["selectAll".to_string()])
    } else {
        None
    };
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
    Ok(Some(
        after
            .as_ref()
            .and_then(|c| c.text.clone())
            .unwrap_or_default(),
    ))
}

/// Rung helper: JS range-select (selhost) + the same trusted Backspace.
pub(super) async fn js_select_all(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
) -> Result<Option<String>> {
    let sh = find_by_sig(cdp, sig, frame, "selhost", None).await?;
    if !sh.ok || sh.sel.unwrap_or(0) == 0 {
        return Ok(None);
    }
    dispatch_key(cdp, "Backspace").await?;
    tokio::time::sleep(Duration::from_millis(35)).await;
    let after = check_editor(cdp, sig, frame).await;
    Ok(Some(
        after
            .as_ref()
            .and_then(|c| c.text.clone())
            .unwrap_or_default(),
    ))
}

/// Dispatch the typing itself: humanized per-char key events for short text
/// (the biometrics path - keydown/keyup pairs, Shift wrapping, the full
/// log-normal cadence), one `Input.insertText` for long text (a paste/IME
/// commit - human-plausible and fast). Returns true when a dispatch path
/// reported success; the readback remains the authority on what landed.
pub(super) async fn type_text(cdp: &CdpSession, text: &str) -> bool {
    const PER_CHAR_MAX: usize = 120;
    let mut typed = false;
    if text.chars().count() <= PER_CHAR_MAX {
        typed = type_per_char(cdp, text).await;
    }
    if !typed {
        typed = cdp
            .send("Input.insertText", Some(json!({ "text": text })))
            .await
            .is_ok();
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
pub(super) async fn finalize_edit(
    rep: &mut EditReport,
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
) -> Result<()> {
    let read = check_editor(cdp, sig, frame).await;
    let mut final_text = read
        .as_ref()
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
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
                            final_text = after
                                .as_ref()
                                .and_then(|c| c.text.clone())
                                .unwrap_or_default();
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
pub(super) fn landed_ref_excluding(
    lpm: &LivePageModel,
    target: &str,
    final_text: &str,
) -> Option<String> {
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
pub(super) fn type_verdict_text(
    ref_id: &str,
    text: &str,
    rep: &EditReport,
    lpm: &LivePageModel,
) -> String {
    let want = norm_text(text);
    let got = norm_text(&rep.final_text);
    if rep.verified && got == want {
        let mut s = format!(
            "outcome: typed \"{}\" → value=\"{}\"",
            clip(text, 40),
            clip(&rep.final_text, 40)
        );
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
pub(super) fn clear_verdict_text(ref_id: &str, rep: &EditReport) -> String {
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
/// Build the script that derives the CURRENT sig of the focused editable —
/// same algorithm (deepAll order, `role|name|rank`) the model and
/// find-by-sig use, so the result resolves in churn-free terms. Used after a
/// collapsed composer expands: the strip unmounts and rank churn across
/// hydrations would strand the captured strip sig. Descends
/// `shadowRoot.activeElement` chains: a shadow-internal focus resolves to
/// the real editor, not its host.
pub(super) fn editor_sig_expr() -> String {
    "(()=>{const d=document;var a=d.activeElement;var _g=0;while(a&&a.shadowRoot&&a.shadowRoot.activeElement&&_g<20){a=a.shadowRoot.activeElement;_g++;}if(!a)return '';".to_string()
        + &JS_PREAMBLE
        + "try{const ce=a.isContentEditable||(a.getAttribute&&a.getAttribute('contenteditable')==='true');const ta=a.tagName==='TEXTAREA';const ip=a.tagName==='INPUT'&&a.type!=='hidden';if(!ce&&!ta&&!ip)return '';}catch(_e){return '';}"
        + "const all=deepAll(d,sel);const counts={};let hit='';"
        + "for(let i=0;i<all.length;i++){const n=all[i];const r=role(n);if(r==='hidden')continue;const snm=name(n,false);const key=r+'\\u0000'+snm;counts[key]=(counts[key]||0)+1;if(n===a){hit='|'+r+'|'+snm+'|'+counts[key];}}"
        + "return hit;})()"
}
