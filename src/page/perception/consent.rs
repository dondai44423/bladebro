//! Consent / cookie-wall handling: reject-first click ladders, the
//! `BLADE_CONSENT` accept/off policy, and the stored-selector fast path.
//! Split from the `perception` core.

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;

/// The ladder's known-framework buttons for the reject policy. Also the
/// replay allowlist of the stored-selector fast path under that policy.
const REJECT_SELECTORS: &[&str] = &[
    "#onetrust-reject-all-handler",
    "#CybotCookiebotDialogBodyButtonDecline",
    "#didomi-notice-disagree-button",
    ".qc-cmp2-summary-buttons button[mode=secondary]",
    "#truste-consent-reject",
    // Amazon EU/UK/DE… banner (the knowledge base pre-seeds it for seven
    // TLDs). It must be on this ladder or the reject policy can never click
    // it: the stored-selector fast path replays only ladder selectors (#82),
    // and the cold ladder had no Amazon entry — the pre-seed never ran
    // (issue #85 item 1).
    "#sp-cc-rejectall-link",
    // Major real-world CMPs (CookieYes, Google Funding Choices, Complianz,
    // Osano, Klaro, Usercentrics, WP Cookie Notice).
    ".cky-btn-reject",
    ".fc-cta-do-not-consent",
    ".cmplz-deny",
    ".osano-cm-denyAll",
    ".cm-btn-decline",
    "button[data-testid=\"uc-deny-all-button\"]",
    "#cn-refuse-cookie",
];

/// The ladder's known-framework buttons for `BLADE_CONSENT=accept`.
const ACCEPT_SELECTORS: &[&str] = &[
    "#onetrust-accept-btn-handler",
    "#CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll",
    "#didomi-notice-agree-button",
    ".qc-cmp2-summary-buttons button[mode=primary]",
    "#truste-consent-button",
    "#sp-cc-accept",
    ".cky-btn-accept",
    ".fc-cta-consent",
    ".cmplz-accept",
    ".osano-cm-accept-all",
    ".cm-btn-accept-all",
    "button[data-testid=\"uc-accept-all-button\"]",
    "#cn-accept-cookie",
];

/// Result of a consent dismissal pass (single CDP round-trip).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConsentOutcome {
    /// Selector that dismissed the banner (`"generic"` for heuristic fallback),
    /// or `None` when no consent banner was visible.
    pub matched: Option<String>,
    /// True when a policy-valid stored selector was tried first, did NOT match,
    /// and a different ladder/generic button dismissed the live banner instead
    /// (signals the knowledge base to downgrade or replace the stale selector).
    pub stored_missed: bool,
}

/// A selector list as a JS array literal. Plain strings always serialize;
/// the fallback still parses.
fn js_list(list: &[&str]) -> String {
    serde_json::to_string(list).unwrap_or_else(|_| "[]".to_string())
}

/// M4: Detect and dismiss consent/cookie banners. Policy: reject (default),
/// accept, or off (via BLADE_CONSENT env). Returns the framework name if dismissed.
///
/// v3.9 false-positive hardening: the old generic pass queried
/// `[role=dialog],[role=banner],[class*=cookie],...` — `[role=banner]` is
/// the site HEADER, so any header containing the word "privacy" (a Privacy
/// Policy nav link — extremely common) plus any header button matching
/// /necessary|essential/ ("Essential Books"!) got CLICKED. Now:
///   - candidates must have cookie/consent/gdpr/cmp-flavored class/id, or
///     be a [role=dialog]
///   - candidates must be VISIBLE (zero-size / display:none banners are
///     stale DOM, not a live wall)
///   - text must match cookie/consent/GDPR vocabulary ("privacy" alone is
///     too weak — it's a footer/header staple)
pub async fn dismiss_consent(cdp: &CdpSession) -> Result<Option<String>> {
    let policy = std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".to_string());
    if policy == "off" {
        return Ok(None);
    }
    let reject = policy != "accept";
    let reject_js = if reject { "true" } else { "false" };

    let expression = "(()=>{const reject=".to_string()
        + reject_js
        + ";const rs="
        + &js_list(REJECT_SELECTORS)
        + ";const as="
        + &js_list(ACCEPT_SELECTORS)
        + ";const sels=reject?rs:as;for(const sel of sels){const btn=document.querySelector(sel);if(btn&&btn.offsetWidth+btn.offsetHeight>0){btn.click();return sel;}}"
        // Generic pass — see the doc comment for why each filter exists.
        + "const dialogs=document.querySelectorAll('[role=dialog],[class*=cookie i],[id*=cookie i],[class*=consent i],[id*=consent i],[class*=gdpr i],[id*=gdpr i],[class*=onetrust i],[class*=didomi i],[class*=cmp i]');"
        + "for(const d of dialogs){"
        + "if(d.offsetWidth+d.offsetHeight===0)continue;"
        + "const text=(d.textContent||'').toLowerCase();"
        + "if(!(/cookie|consent|gdpr/.test(text)))continue;"
        + "const buttons=[...d.querySelectorAll('button,a')];"
        + "if(buttons.length>12)continue;" // real consent walls are compact; a matching mega-container is site chrome
        + "const pattern=reject?/reject|decline|refuse|deny|necessary|essential/i:/accept|agree|allow|consent/i;"
        + "const btn=buttons.find(b=>pattern.test(b.textContent||'')&&b.offsetWidth+b.offsetHeight>0);"
        + "if(btn){btn.click();return 'generic';}}"
        + "return null;})()";

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if res.get("exceptionDetails").is_some() {
        return Ok(None);
    }
    let framework = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(framework)
}

/// Try a stored consent selector first, falling back to the full ladder +
/// generic pass in ONE single `Runtime.evaluate` round-trip.
///
/// Previously, a miss on the stored selector (including every normal revisit
/// where cookies already dismissed the banner!) ran TWO sequential CDP evals,
/// and stale stored selectors were never reported back to the knowledge base.
/// Fusing the stored fast-path and fallback into one script guarantees <= 1
/// CDP round-trip on every navigation while preserving all security gates:
///   - Only replays a stored selector present in the current policy's allowlist.
///   - Re-validates the stored match inside a visible consent-flavored container.
///   - Reports `stored_missed` ONLY when a live banner was dismissed by a
///     different button (never downgrades a valid selector just because cookies
///     kept the banner hidden).
pub(crate) async fn dismiss_consent_detailed(
    cdp: &CdpSession,
    stored: Option<&str>,
) -> Result<ConsentOutcome> {
    let policy = std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".to_string());
    if policy == "off" {
        return Ok(ConsentOutcome::default());
    }
    let reject = policy != "accept";
    let replayable = if reject {
        REJECT_SELECTORS
    } else {
        ACCEPT_SELECTORS
    };
    let stored_sel = stored.filter(|s| replayable.contains(s));
    let stored_js = match stored_sel {
        Some(sel) => serde_json::to_string(sel).unwrap_or_else(|_| "null".to_string()),
        None => "null".to_string(),
    };
    let reject_js = if reject { "true" } else { "false" };

    // Single-round-trip fused script:
    // 1. If a policy-valid stored selector is provided, test it first with the
    //    consent-context security gate; on match, click and return immediately.
    // 2. Otherwise walk the policy ladder and then the bounded generic pass.
    //    Prefix fallback matches with "M:" when a stored selector was present
    //    and missed so the caller knows the stored selector went stale.
    let expression = "(()=>{const stored=".to_string()
        + &stored_js
        + ";const reject="
        + reject_js
        + ";const rs="
        + &js_list(REJECT_SELECTORS)
        + ";const as="
        + &js_list(ACCEPT_SELECTORS)
        + ";if(stored){const b=document.querySelector(stored);if(b&&b.offsetWidth+b.offsetHeight>0){const d=b.closest('[role=dialog],[class*=cookie i],[id*=cookie i],[class*=consent i],[id*=consent i],[class*=gdpr i],[id*=gdpr i],[class*=onetrust i],[class*=didomi i],[class*=cmp i]');if(d&&/cookie|consent|gdpr/.test((d.textContent||'').toLowerCase())){b.click();return 'H:'+stored;}}}"
        + "const pfx=stored?'M:':'H:';"
        + "const sels=reject?rs:as;for(const sel of sels){const btn=document.querySelector(sel);if(btn&&btn.offsetWidth+btn.offsetHeight>0){btn.click();return (sel===stored?'H:':pfx)+sel;}}"
        + "const dialogs=document.querySelectorAll('[role=dialog],[class*=cookie i],[id*=cookie i],[class*=consent i],[id*=consent i],[class*=gdpr i],[id*=gdpr i],[class*=onetrust i],[class*=didomi i],[class*=cmp i]');"
        + "for(const d of dialogs){"
        + "if(d.offsetWidth+d.offsetHeight===0)continue;"
        + "const text=(d.textContent||'').toLowerCase();"
        + "if(!(/cookie|consent|gdpr/.test(text)))continue;"
        + "const buttons=[...d.querySelectorAll('button,a')];"
        + "if(buttons.length>12)continue;"
        + "const pattern=reject?/reject|decline|refuse|deny|necessary|essential/i:/accept|agree|allow|consent/i;"
        + "const btn=buttons.find(b=>pattern.test(b.textContent||'')&&b.offsetWidth+b.offsetHeight>0);"
        + "if(btn){btn.click();return pfx+'generic';}}"
        + "return null;})()";

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if res.get("exceptionDetails").is_some() {
        return Ok(ConsentOutcome::default());
    }
    let Some(raw) = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
    else {
        return Ok(ConsentOutcome::default());
    };
    if let Some(sel) = raw.strip_prefix("M:") {
        Ok(ConsentOutcome {
            matched: (!sel.is_empty()).then(|| sel.to_string()),
            stored_missed: true,
        })
    } else if let Some(sel) = raw.strip_prefix("H:") {
        Ok(ConsentOutcome {
            matched: (!sel.is_empty()).then(|| sel.to_string()),
            stored_missed: false,
        })
    } else if !raw.is_empty() {
        Ok(ConsentOutcome {
            matched: Some(raw.to_string()),
            stored_missed: false,
        })
    } else {
        Ok(ConsentOutcome::default())
    }
}

/// Public wrapper around [`dismiss_consent_detailed`] returning the matched
/// selector (`Some(sel)` / `Some("generic")`) or `None`.
pub async fn dismiss_consent_with_stored(
    cdp: &CdpSession,
    stored: Option<&str>,
) -> Result<Option<String>> {
    Ok(dismiss_consent_detailed(cdp, stored).await?.matched)
}
