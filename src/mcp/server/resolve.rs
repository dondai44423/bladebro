//! Addressing resolution for the tool handlers.
//!
//! Text queries resolve three-tier (exact/contains → alias groups → input
//! type) against the LPM first, with a live-DOM fallback; selectors resolve
//! over the light DOM + open shadow roots. Misses carry their diagnostics —
//! hidden/unaddressable matches, near-miss selectors — because a bare
//! "not found" is the bug class this file exists to kill.

use crate::error::{BladeError, Result};
use crate::page::Page;

/// M3: Resolve a text query to an element ref. Searches the LPM (page model)
/// first — it has all elements from all frames, so label addressing works
/// for iframe content too. Falls back to live DOM search via find_by_text
/// if the LPM has no matches (e.g. the page changed since last capture).
///
/// Three-tier matching: exact/contains name → alias group → type-based.
/// Alias groups map common synonyms (username→acct, password→pw, etc.).
/// Type-based uses the HTML input type as a universal hint.
pub(super) async fn resolve_text_target(
    page: &mut Page,
    query: &str,
    role_filter: Option<&str>,
    nth: Option<usize>,
) -> Result<String> {
    let q = query.to_lowercase();

    // Alias groups: all names in a group are interchangeable.
    // When the query matches any name in a group, any element whose
    // name is also in that group is a candidate.
    const FIELD_ALIASES: &[&[&str]] = &[
        &[
            "username",
            "user",
            "login",
            "acct",
            "account",
            "userid",
            "user id",
            "uid",
            "uname",
            "login name",
            "loginid",
            "login id",
            "signin",
            "sign in",
            "user name",
            "member",
            "handle",
            "nick",
            "nickname",
        ],
        &[
            "password",
            "pw",
            "passwd",
            "pwd",
            "pass",
            "secret",
            "current password",
            "new password",
            "confirm password",
        ],
        &[
            "email",
            "mail",
            "e mail",
            "e-mail",
            "emailaddress",
            "email address",
            "eml",
            "emailaddr",
        ],
        &["search", "query", "find", "filter", "keyword", "q", "s"],
        &[
            "phone",
            "tel",
            "mobile",
            "phone number",
            "mobile number",
            "telephone",
            "contact",
            "cell",
            "cellphone",
        ],
        &[
            "name",
            "fullname",
            "full name",
            "first name",
            "firstname",
            "given name",
            "family name",
            "last name",
            "lastname",
            "display name",
        ],
    ];

    // Find which alias group the query belongs to (if any).
    let alias_group: Option<&[&str]> = FIELD_ALIASES
        .iter()
        .find(|g| g.iter().any(|a| *a == q))
        .copied();

    // Phase 1: Search the LPM directly.
    let mut lpm_matches: Vec<(String, String, String, Vec<usize>, i64)> = Vec::new();
    for el in page.model().elements() {
        let role = &el.raw.role;
        if role == "hidden" {
            continue;
        }
        if let Some(rf) = role_filter {
            if role != rf {
                continue;
            }
        }
        let name = &el.raw.name;
        let name_lower = name.to_lowercase();
        let mut score = 0i64;
        if name == query {
            score = 100;
        } else if name_lower == q {
            score = 80;
        } else if name.contains(query) {
            score = 70;
        } else if name_lower.contains(&q) {
            score = 60;
        } else {
            // Check placeholder as fallback.
            let al = el.raw.placeholder.as_deref().unwrap_or("");
            if !al.is_empty() && al.to_lowercase().contains(&q) {
                score = 30;
            }
        }
        // Alias group matching: if both query and element name are in
        // the same alias group, it's a strong match (score 55).
        if score == 0 {
            if let Some(group) = alias_group {
                if group.iter().any(|a| *a == name_lower) {
                    score = 55;
                }
            }
        }
        // Type-based matching: if query matches the HTML input type.
        if score == 0 {
            if let Some(ref ty) = el.raw.element_type {
                let ty_lower = ty.to_lowercase();
                let type_match = (q == "password" && ty_lower == "password")
                    || (q == "email" && ty_lower == "email")
                    || (q == "search" && ty_lower == "search")
                    || (q == "phone" && ty_lower == "tel")
                    || (q == "url" && ty_lower == "url");
                if type_match {
                    score = 50;
                }
            }
        }
        if score > 0 {
            lpm_matches.push((
                el.ref_id.clone(),
                role.clone(),
                name.clone(),
                el.raw.frame.clone(),
                score,
            ));
        }
    }
    if !lpm_matches.is_empty() {
        lpm_matches.sort_by_key(|b| std::cmp::Reverse(b.4));
        match nth {
            Some(n) if n >= 1 && n <= lpm_matches.len() => {
                return Ok(lpm_matches[n - 1].0.clone());
            }
            Some(_) => {
                // Out of range against the captured model - the page may have
                // grown since the capture (late hydration, lazy render), so
                // fall through to the live DOM search and error only if the
                // live page is short too. Never silently pick the first match.
            }
            None => return Ok(lpm_matches[0].0.clone()),
        }
    }

    // Phase 2: Positional fallback for forms. If the query is a common
    // field type (username/password/email) and there are textboxes in
    // the model, pick the first textbox for username/email and the
    // password-typed one for password. Guarded to nth-less calls: a
    // positional guess is a "no matches, cope" heuristic, and answering
    // nth=3 with the first password box would be a silent wrong target.
    if let Some(group) = alias_group.filter(|_| nth.is_none()) {
        let is_username_like = group
            .iter()
            .any(|a| *a == "username" || *a == "user" || *a == "login" || *a == "acct");
        let is_password_like = group.iter().any(|a| *a == "password" || *a == "pw");
        if is_username_like || is_password_like {
            let textboxes: Vec<_> = page
                .model()
                .elements()
                .iter()
                .filter(|e| e.raw.role == "textbox" || e.raw.role == "combobox")
                .collect();
            if is_password_like {
                // Prefer password-typed inputs.
                if let Some(pw) = textboxes
                    .iter()
                    .find(|e| e.raw.element_type.as_deref() == Some("password"))
                {
                    return Ok(pw.ref_id.clone());
                }
            }
            if is_username_like && !textboxes.is_empty() {
                // First non-password textbox is the username field.
                if let Some(tb) = textboxes
                    .iter()
                    .find(|e| e.raw.element_type.as_deref() != Some("password"))
                {
                    return Ok(tb.ref_id.clone());
                }
            }
        }
    }

    // Phase 3: Live DOM search via find_by_text. Used when the LPM
    // has no matches (page changed since last capture).
    let matches = crate::action::find_by_text(page.cdp_ref(), query, role_filter, false).await?;
    if matches.is_empty() {
        // Same explainer as `see find`: a flat "not found" while a hidden or
        // shadow-root match exists is exactly the diagnostic gap that sent
        // an agent hunting through raw eval.
        let note = miss_diag_note(
            crate::action::find_miss_diag(page.cdp_ref(), query)
                .await
                .ok(),
        );
        // W4: a miss on a localized UI (the Google-Nepali case) otherwise
        // reads as "blocked" or "gone"; one extra evaluate names the page
        // language so the fix is one step (localized label / selector=).
        let lang = page_lang_note(page.cdp_ref()).await;
        let view = page.view(2000);
        return Err(BladeError::Other(format!(
            "no element matching \"{}\" found{}{}\n\n--- current page ---\n{}",
            query, note, lang, view
        )));
    }
    if let Some(n) = nth {
        if n >= 1 && n <= matches.len() {
            let m = &matches[n - 1];
            return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
        }
        return Err(BladeError::Other(format!(
            "nth={n} requested but \"{query}\" has only {} visible match(es) (nth is 1-based; omit nth to use the top match)",
            matches.len()
        )));
    }
    if matches.len() == 1 {
        let m = &matches[0];
        return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
    }
    let top = &matches[0];
    let id = page
        .model_mut()
        .adopt(&top.sig, &top.role, &top.name, &top.frame);
    for m in &matches[1..] {
        let _ = page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame);
    }
    Ok(id)
}

/// Format the hidden/unaddressable-match note shared by `see find` and text
/// addressing misses (`act click text=...`): a miss must say WHY it missed.
pub(super) fn miss_diag_note(diag: Option<crate::action::MissDiag>) -> String {
    match diag {
        Some(d) if d.total > 0 => {
            let mut s = format!(" ({} match(es) exist but were not addressable", d.total);
            if !d.examples.is_empty() {
                let ex: Vec<String> = d
                    .examples
                    .iter()
                    .map(|e| format!("{} \"{}\" [{}]", e.role, e.name, e.reason))
                    .collect();
                s.push_str(&format!(": {}", ex.join("; ")));
            } else if d.hidden > 0 {
                s.push_str(&format!(": {} hidden", d.hidden));
            }
            if d.shadow > 0 {
                s.push_str(&format!("; {} in open shadow roots", d.shadow));
            }
            s.push_str(" - hidden controls are not clickable by a mouse; if the site wires them programmatically use act eval (el.click())");
            s
        }
        _ => String::new(),
    }
}

/// W4: page-language hint for a text/label miss. A miss on a localized UI
/// (the Google-served-Nepali case) reads as "blocked" or "gone" without
/// this; the page's own `lang` names the real problem in one line. Quiet on
/// English/absent-lang pages - the common case adds no tokens.
pub(super) async fn page_lang_note(cdp: &crate::cdp::CdpSession) -> String {
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "document.documentElement.lang||''",
                "returnByValue": true
            })),
        )
        .await;
    let lang = res
        .ok()
        .and_then(|r| {
            r.get("result")
                .and_then(|x| x.get("value"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let lang = lang.trim().to_ascii_lowercase();
    if lang.is_empty() || lang.starts_with("en") {
        return String::new();
    }
    format!(" (page lang=\"{lang}\" - the UI may be localized; try the page's own wording, selector=, or a locale override)")
}

/// Format the near-miss diagnostic for a selector that matched nothing
/// actionable: the raw light-DOM count, or the closest live matches for the
/// looser suffix that DID match - one-step recovery instead of a dead end.
fn selector_diag_note(diag: Option<&crate::action::SelectorDiag>) -> String {
    let d = match diag {
        Some(d) => d,
        None => return String::new(),
    };
    if d.raw > 0 {
        let ex = if d.raw_samples.is_empty() {
            String::new()
        } else {
            format!(": {}", d.raw_samples.join("; "))
        };
        return format!(
            " - {} element(s) match the full selector but are not actionable{ex}",
            d.raw
        );
    }
    if d.sub_count > 0 {
        let ex = if d.samples.is_empty() {
            String::new()
        } else {
            format!(": {}", d.samples.join("; "))
        };
        return format!(
            " - closest live matches for \"{}\" ({} found){ex} - adjust the selector (a tag or class in the path may have changed)",
            d.sub, d.sub_count
        );
    }
    // W3: the selector matched nothing anywhere - say the counts outright so
    // "the selector is wrong" and "bladebro cannot match" are distinguishable
    // (the HN `a.titleline` detour: only a manual querySelectorAll proved 0).
    " - 0 raw matches, 0 actionable: the selector matched nothing - check its spelling against the live page (see mode=model lists the real tags/classes)".to_string()
}

/// Resolve a CSS selector to a ref. Mirrors [`resolve_text_target`]: match
/// against actionable elements (open shadow roots included), adopt the live
/// sig into the model, return the ref - every action downstream (real mouse
/// input, healing) then works exactly as with a text- or ref-addressed
/// element. Hidden-only matches error with the reason: a mouse cannot reach
/// a display:none control, and pretending otherwise would be the silent
/// no-op class this addressing exists to kill.
pub(super) async fn resolve_selector_target(
    page: &mut Page,
    selector: &str,
    nth: Option<usize>,
) -> Result<String> {
    let lookup = crate::action::find_by_selector(page.cdp_ref(), selector).await?;
    let matches = &lookup.matches;
    let visible: Vec<&crate::action::TextMatch> = matches.iter().filter(|m| !m.hidden).collect();
    if visible.is_empty() {
        if !matches.is_empty() {
            // G01: a single hidden checkbox/radio with ONE visible label
            // proxy is genuinely addressable - the click lands on the label
            // and toggles the control natively. Only this narrow shape
            // reroutes; every other hidden match keeps the honest error.
            if matches.len() == 1 {
                let m = &matches[0];
                if m.role == "checkbox" || m.role == "radio" {
                    let route =
                        crate::action::find_label_route(page.cdp_ref(), &m.sig, &m.frame).await?;
                    if let Some(route) = route {
                        return Ok(page.model_mut().adopt(
                            &route.sig,
                            &route.role,
                            &route.name,
                            &m.frame,
                        ));
                    }
                }
            }
            let ex: Vec<String> = matches
                .iter()
                .take(3)
                .map(|m| {
                    let ctx = if m.ctx.is_empty() {
                        String::new()
                    } else {
                        format!(" (in {})", m.ctx)
                    };
                    format!("{} \"{}\" [{}]{}", m.role, m.name, m.reason, ctx)
                })
                .collect();
            return Err(BladeError::Other(format!(
                "selector \"{selector}\" matched {} element(s) but none is visible: {} - a hidden control cannot be clicked by a mouse; if the site wires it programmatically, drive it with act eval (el.click())",
                matches.len(),
                ex.join("; ")
            )));
        }
        let diag_note = selector_diag_note(lookup.diag.as_ref());
        return Err(BladeError::Other(format!(
            "no actionable element matches selector \"{selector}\" (searched light DOM + open shadow roots){diag_note}"
        )));
    }
    if let Some(n) = nth {
        if n >= 1 && n <= visible.len() {
            let m = visible[n - 1];
            return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
        }
        // An out-of-range ordinal must not silently fall back to the first
        // match: answering "7" with position 1 is the silent-wrong-target
        // class this addressing exists to kill.
        let hidden_note = if matches.len() > visible.len() {
            format!(
                "; {} more match(es) exist but are hidden",
                matches.len() - visible.len()
            )
        } else {
            String::new()
        };
        return Err(BladeError::Other(format!(
            "nth={n} requested but selector \"{selector}\" has only {} visible match(es){hidden_note} (nth is 1-based among visible matches)",
            visible.len()
        )));
    }
    if visible.len() == 1 {
        let m = visible[0];
        return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
    }
    // Multiple visible matches are NOT silently resolved to the first: an
    // agent that wrote "a.yes" while two different yes-confirmations exist
    // (disable-inbox vs delete) must choose deliberately - the silent
    // top-pick clicked the wrong action with no signal. List them; nth picks.
    let mut lines: Vec<String> = Vec::new();
    for (i, m) in visible.iter().enumerate().take(5) {
        let ctx = if m.ctx.is_empty() {
            String::new()
        } else {
            format!(" (in {})", m.ctx)
        };
        lines.push(format!("{}) {} \"{}\"{}", i + 1, m.role, m.name, ctx));
    }
    if visible.len() > 5 {
        lines.push(format!("…(+{} more)", visible.len() - 5));
    }
    Err(BladeError::Other(format!(
        "selector \"{selector}\" matches {} visible elements - add nth=N (1-based) to pick one:\n  {}",
        visible.len(),
        lines.join("\n  ")
    )))
}
