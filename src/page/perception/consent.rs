//! Consent / cookie-wall handling: reject-first click ladders, the
//! `BLADE_CONSENT` accept/off policy, and the stored-selector fast path.
//! Split from the `perception` core.

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;

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
        + ";const rs=['#onetrust-reject-all-handler','#CybotCookiebotDialogBodyButtonDecline','#didomi-notice-disagree-button','.qc-cmp2-summary-buttons button[mode=secondary]','#truste-consent-reject'];"
        + "const as=['#onetrust-accept-btn-handler','#CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll','#didomi-notice-agree-button','.qc-cmp2-summary-buttons button[mode=primary]','#truste-consent-button'];"
        + "const sels=reject?rs:as;for(const sel of sels){const btn=document.querySelector(sel);if(btn&&btn.offsetWidth+btn.offsetHeight>0){btn.click();return sel;}}"
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

/// Try a stored consent selector first (one cheap querySelector+click),
/// fall back to full [`dismiss_consent`] detection if it doesn't match.
///
/// This is the knowledge-base integration point: on known sites, a trusted
/// CSS selector (confidence >= 0.7) skips the full 20-line detection JS,
/// saving one large `Runtime.evaluate`. On unknown or changed sites, the
/// full detection runs as usual — zero regression for cold starts.
///
/// Returns the selector that was clicked (stored selector, new selector, or
/// `"generic"`), or `None` if no consent dialog was found.
pub async fn dismiss_consent_with_stored(
    cdp: &CdpSession,
    stored: Option<&str>,
) -> Result<Option<String>> {
    let policy = std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".to_string());
    if policy == "off" {
        return Ok(None);
    }

    // Try the stored selector first — skip the full detection JS if it works.
    if let Some(sel) = stored.filter(|s| !s.is_empty() && *s != "generic") {
        let sel_json = serde_json::to_string(sel).unwrap_or_default();
        // SECURITY: re-validate before clicking. The store is file-backed
        // state whose integrity rests on directory permissions — but the
        // browser rendering this page can itself write those files (CDP
        // download routing), so a stored selector is not proof of consent
        // context. Require the same gates as the generic pass: a visible
        // match inside a consent-looking container with consent vocabulary.
        // A stale or poisoned selector falls through to full detection
        // instead of clicking.
        let expr = format!(
            "(()={{const b=document.querySelector({sel_json});if(b&&b.offsetWidth+b.offsetHeight>0){{const d=b.closest('[role=dialog],[class*=cookie i],[id*=cookie i],[class*=consent i],[id*=consent i],[class*=gdpr i],[id*=gdpr i],[class*=onetrust i],[class*=didomi i],[class*=cmp i]');if(d&&/cookie|consent|gdpr/.test((d.textContent||'').toLowerCase())){{b.click();return {sel_json};}}}}return null;}})()"
        );
        let res = cdp
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expr,
                    "returnByValue": true,
                })),
            )
            .await?;
        if res.get("exceptionDetails").is_none() {
            if let Some(v) = res
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
            {
                if !v.is_empty() {
                    return Ok(Some(v.to_string()));
                }
            }
        }
    }

    // Fall through to full detection.
    dismiss_consent(cdp).await
}
