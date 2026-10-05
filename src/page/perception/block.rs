//! Block / challenge detection: the small-page gated detector and the
//! per-type remediation ladder the surfaces append to ambient events.
//! Split from the `perception` core.

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;

/// M6: Detect block/challenge pages (Cloudflare, DataDome, PerimeterX, reCAPTCHA, Akamai, Reddit).
/// Returns the block type if detected, or None.
/// S12: remediation ladder — actionable steps for each block type.
/// Appended to ambient events so the agent knows what to try next.
pub fn remediation_ladder(block_type: &str) -> Vec<String> {
    match block_type {
        "cloudflare" => vec![
            "wait 10s \u{2014} Turnstile often auto-passes non-interactively".into(),
            "see \u{2014} check for a visible checkbox, use act click x=X y=Y if found".into(),
            "if interactive challenge: solve manually or use coordinate click on checkbox".into(),
        ],
        "datadome" => vec![
            "blocked by DataDome (ML-based per-site detection)".into(),
            "try: navigate away, wait 30s, return (rate cooldown)".into(),
            "if captcha wall: needs external solver".into(),
        ],
        "perimeterx" => vec![
            "blocked by PerimeterX (behavioral analysis)".into(),
            "try: slower pacing, longer idle hum, more natural session".into(),
            "if captcha: needs external solver".into(),
        ],
        "recaptcha" => vec![
            "reCAPTCHA challenge (v3 score-based or v2 checkbox)".into(),
            "v3: improve score via longer session with human-like behavior".into(),
            "v2: click checkbox via act click x=X y=Y".into(),
        ],
        "akamai" => vec![
            "blocked by Akamai (IP reputation + TLS fingerprint)".into(),
            "try: BLADE_PROXY for a different IP, BLADE_TZ for matching timezone".into(),
        ],
        "rate-limit" => vec![
            "rate limited \u{2014} wait 30-60s before retrying".into(),
            "consider: BLADE_PROXY for a different IP".into(),
        ],
        "reddit-humanity" => vec![
            "reddit's one-time humanity check (reCAPTCHA v2 checkbox)".into(),
            "solved automatically when detected \u{2014} one humanized click grants the profile token".into(),
            "if it persists: an image challenge may be showing \u{2014} solve it manually once in the browser, or retry later; the wall does not return after a pass".into(),
        ],
        "reddit" => vec![
            "reddit's network-security wall (soft, transient \u{2014} its own retry-after is 0)".into(),
            "auto-recovery already reloaded; if it persists, wait ~30s and retry".into(),
            "persistent walls usually mean a flagged IP (VPN/datacenter) \u{2014} use a residential connection; signed-in sessions are trusted more".into(),
        ],
        "js-challenge" => vec![
            "reddit's JS challenge did not auto-resolve \u{2014} a reload usually completes it".into(),
            "retry the same navigation; a cold profile gets the challenge once, then `loid` is stored".into(),
        ],
        _ => vec!["unknown block \u{2014} try waiting and retrying".into()],
    }
}

/// v3.9: block heuristics are gated to avoid false "blocked:" verdicts on
/// perfectly good pages. The old rules matched bare substrings anywhere in
/// the first 2000 chars of body text:
///
/// - any docs page mentioning "rate limit" → blocked:rate-limit
/// - any site embedding a Turnstile widget (login/signup forms do this
///   LEGITIMATELY) → blocked:cloudflare
/// - any page mentioning "datadome" (tech blogs!) → blocked:datadome
///
/// Real block/challenge pages are SMALL (a title, a spinner, a form) — the
/// body-length gate is the strongest discriminator between "the page is a
/// wall" and "the page discusses walls".
pub(super) const DETECT_BLOCK_SCRIPT: &str = include_str!("../js/detect_block.js");

/// M6: Detect block/challenge pages from their live DOM (small-page gated —
/// the body-length gate is the strongest wall-vs-prose discriminator).
/// Returns the block type if detected, or None.
pub async fn detect_block(cdp: &CdpSession) -> Result<Option<String>> {
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": DETECT_BLOCK_SCRIPT,
                "returnByValue": true,
            })),
        )
        .await?;

    if res.get("exceptionDetails").is_some() {
        return Ok(None);
    }
    let block = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(block)
}
