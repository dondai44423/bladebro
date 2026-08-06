//! Request interception and stealth-aware resource blocking.
//!
//! The single biggest real-site speed win available: blocking
//! images, fonts, media, and third-party trackers cuts page load
//! 2-5x on heavy sites and removes DOM noise (fewer tokens). Done
//! via the CDP Fetch domain — every request pauses until we allow
//! or fail it.
//!
//! STEALTH IS THE HARD CONSTRAINT. A naive blocklist makes us
//! MORE detectable, not less. The rules:
//!
//! 1. NEVER block first-party (same-site) JavaScript or XHR. The
//!    site's own bot-detector (inline or same-origin) MUST run.
//!    Blocking it is the loudest possible "I am a bot" signal.
//! 2. NEVER block known bot-detection vendors (DataDome, Cloudflare
//!    challenges, PerimeterX/HUMAN, Imperva, Akamai Bot Manager),
//!    even third-party. These gate the page; killing them breaks
//!    the challenge AND flags us.
//! 3. Resource-TYPE blocks (image/font/media) are ALWAYS safe —
//!    they are inert, cannot execute, and cannot be a detector.
//! 4. Tracker blocking applies ONLY to third-party (eTLD+1 differs
//!    from the page origin). Real users run ad blockers (40%+), so
//!    blocking third-party analytics is human-like.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crate::cdp::session::{CdpSession, SessionSubscription};

// Block-class bitmask (lock-free reads in the hot interception path).
pub const BLOCK_IMAGES: u32 = 1;
pub const BLOCK_FONTS: u32 = 2;
pub const BLOCK_MEDIA: u32 = 4;
pub const BLOCK_TRACKERS: u32 = 8;

/// Shared interception state. Clone-cheap (Arc); the Page holds one
/// handle, the interception task holds another.
#[derive(Clone)]
pub struct InterceptState {
    /// Active block-class bitmask.
    pub rules: Arc<AtomicU32>,
    /// Registrable domain (eTLD+1) of the current page, for
    /// third-party detection. Updated on every navigation.
    pub page_domain: Arc<RwLock<String>>,
    /// requestIds currently paused and awaiting a verdict, so a
    /// Fetch.disable can drain them instead of leaving requests hung.
    pending: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl Default for InterceptState {
    fn default() -> Self {
        Self {
            rules: Arc::new(AtomicU32::new(0)),
            page_domain: Arc::new(RwLock::new(String::new())),
            pending: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }
}

impl InterceptState {
    /// Parse "images,fonts,media,trackers" into the bitmask.
    /// Returns the mask. Unknown classes are ignored (lenient).
    pub fn parse_classes(spec: &str) -> u32 {
        let mut mask = 0u32;
        for tok in spec.split(',') {
            match tok.trim().to_ascii_lowercase().as_str() {
                "images" | "image" | "img" => mask |= BLOCK_IMAGES,
                "fonts" | "font" => mask |= BLOCK_FONTS,
                "media" | "video" | "audio" => mask |= BLOCK_MEDIA,
                "trackers" | "tracker" | "analytics" | "ads" => mask |= BLOCK_TRACKERS,
                _ => {}
            }
        }
        mask
    }

    /// Human-readable list of active classes, for `state op=block get`.
    pub fn describe(mask: u32) -> String {
        let mut parts = Vec::new();
        if mask & BLOCK_IMAGES != 0 { parts.push("images"); }
        if mask & BLOCK_FONTS != 0 { parts.push("fonts"); }
        if mask & BLOCK_MEDIA != 0 { parts.push("media"); }
        if mask & BLOCK_TRACKERS != 0 { parts.push("trackers"); }
        if parts.is_empty() { "none".to_string() } else { parts.join(",") }
    }

    pub fn set_rules(&self, mask: u32) {
        self.rules.store(mask, Ordering::Relaxed);
    }

    pub fn rules(&self) -> u32 {
        self.rules.load(Ordering::Relaxed)
    }

    pub fn set_page_url(&self, url: &str) {
        let host = url_host(url);
        let domain = registrable_domain(&host);
        if let Ok(mut d) = self.page_domain.write() {
            *d = domain;
        }
    }

    fn page_domain(&self) -> String {
        self.page_domain.read().map(|d| d.clone()).unwrap_or_default()
    }

    fn add_pending(&self, id: &str) {
        if let Ok(mut p) = self.pending.lock() { p.insert(id.to_string()); }
    }

    fn remove_pending(&self, id: &str) {
        if let Ok(mut p) = self.pending.lock() { p.remove(id); }
    }

    fn drain_pending(&self) -> Vec<String> {
        if let Ok(mut p) = self.pending.lock() {
            p.drain().collect()
        } else {
            Vec::new()
        }
    }
}

/// Extract the host (no port, no path) from a URL. Lowercased.
pub fn url_host(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = after_scheme.split('/').next().unwrap_or("");
    let host = host_port.split(':').next().unwrap_or("");
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Known two-label public suffixes (ccTLD second levels). When the
/// last two labels match one of these, the registrable domain is the
/// last THREE labels. Not exhaustive (full PSL is heavy) but covers
/// the common cases that matter for third-party detection.
const TWO_PART_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "net.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au",
    "co.jp", "or.jp", "ne.jp", "ac.jp", "go.jp",
    "com.br", "net.br", "org.br", "gov.br",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
    "com.tw", "org.tw", "edu.tw", "gov.tw",
    "co.in", "net.in", "org.in", "gen.in", "firm.in",
    "co.nz", "net.nz", "org.nz", "ac.nz", "govt.nz",
    "co.kr", "or.kr", "ne.kr", "ac.kr", "go.kr",
    "com.sg", "net.sg", "org.sg", "edu.sg", "gov.sg",
    "com.mx", "org.mx", "gob.mx", "edu.mx",
    "com.tr", "org.tr", "net.tr", "gen.tr",
    "co.za", "org.za", "net.za", "ac.za",
    "com.hk", "org.hk", "net.hk", "edu.hk", "gov.hk",
    "com.ar", "com.co", "com.pe", "com.ve", "com.ph", "com.my",
    "co.id", "co.th", "co.il", "com.ng", "com.pk", "com.bd",
    "com.eg", "com.sa", "com.ae", "com.ua", "com.pl", "com.vn",
];

/// Registrable domain (eTLD+1) heuristic. "www.cdn.example.co.uk"
/// → "example.co.uk". IP addresses and short hosts return as-is.
pub fn registrable_domain(host: &str) -> String {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    if n <= 2 {
        return host.to_string();
    }
    let last2 = format!("{}.{}", labels[n - 2], labels[n - 1]);
    if TWO_PART_SUFFIXES.contains(&last2.as_str()) && n >= 3 {
        return format!("{}.{}", labels[n - 3], last2);
    }
    last2
}

/// Third-party tracker/analytics/ad domains. Matched against the
/// registrable domain of a THIRD-PARTY request only. Deliberately
/// excludes bot-detection vendors (see NEVER_BLOCK).
const TRACKER_DOMAINS: &[&str] = &[
    // Google ads/analytics (not the bot-detection side)
    "doubleclick.net", "googlesyndication.com", "google-analytics.com",
    "googletagmanager.com", "googleadservices.com", "analytics.google.com",
    "adservice.google.com", "pagead2.googlesyndication.com",
    // Facebook/Meta
    "facebook.net", "connect.facebook.net", "fbcdn.net",
    // Twitter/X
    "ads.twitter.com", "analytics.twitter.com", "ads-twitter.com",
    // Microsoft Clarity / LinkedIn
    "clarity.ms", "snap.licdn.com",
    // Session replay / heatmaps (third-party trackers users block)
    "hotjar.com", "fullstory.com", "mouseflow.com", "luckyorange.com",
    "crazyegg.com", "smartlook.com", "logrocket.com",
    // Product analytics
    "mixpanel.com", "segment.io", "segment.com", "amplitude.com",
    "heap.io", "heapanalytics.com", "posthog.com", "pendo.io",
    "kissmetrics.com", "chartbeat.com", "parsely.com",
    // APM / error tracking
    "newrelic.com", "nr-data.net", "sentry.io", "bugsnag.com",
    "datadoghq.com", "rollbar.com",
    // Ad networks / exchanges
    "amazon-adsystem.com", "adnxs.com", "adsrvr.org", "rubiconproject.com",
    "pubmatic.com", "openx.net", "criteo.com", "criteo.net", "taboola.com",
    "outbrain.com", "scorecardresearch.com", "quantserve.com", "quantcount.com",
    "moatads.com", "advertising.com", "bidswitch.net", "casalemedia.com",
    "indexww.com", "lijit.com", "sharethrough.com", "teads.tv", "yieldmo.com",
    "33across.com", "adform.net", "smartadserver.com", "spotxchange.com",
    "contextweb.com", "gumgum.com", "media.net", "sovrn.com",
    // Tag managers / beacons
    "tiqcdn.com", "demdex.net", "omtrdc.net", "everesttech.net",
    "krxd.net", "bluekai.com", "exelator.com", "mathtag.com",
    // Consent/analytics extras
    "onetrust.io", "cookielaw.org", "trustarc.com",
];

/// Bot-detection / challenge vendors that must NEVER be blocked.
/// Killing these breaks the challenge and flags us instantly. This
/// list WINS over TRACKER_DOMAINS and over resource-type blocks for
/// scripts/xhr (resource-type blocks only hit inert assets anyway).
const NEVER_BLOCK_DOMAINS: &[&str] = &[
    "datadome.co", "datadome.eu",
    "challenges.cloudflare.com", "cloudflare.com", "cdn-cgi",
    "px-cdn.net", "perimeterx.net", "humansecurity.com", "px-cloud.net",
    "imperva.com", "incapsula.com", "incech.com",
    "akamaihd.net", "akamaized.net", "akamai.com",
    "distil.networks", "shieldsquare.com", "kasada.io", "arkoselabs.com",
    "hcaptcha.com", "recaptcha.net", "google.com", // reCAPTCHA lives under google.com/recaptcha
    "funcaptcha.com", "geetest.com",
];

/// Decide whether a request should be blocked.
///
/// `resource_type`: CDP Network.ResourceType string ("Image", "Script", ...).
/// `url`: the request URL.
/// `rules`: the active block-class bitmask.
/// `page_domain`: registrable domain of the current page ("" = unknown).
pub fn should_block(resource_type: &str, url: &str, rules: u32, page_domain: &str) -> bool {
    if rules == 0 {
        return false;
    }
    let host = url_host(url);
    if host.is_empty() {
        return false;
    }
    let req_domain = registrable_domain(&host);

    // NEVER block bot-detection vendors, regardless of class.
    for nb in NEVER_BLOCK_DOMAINS {
        if req_domain == *nb || host.ends_with(nb) || host.contains(nb) {
            return false;
        }
    }

    // Inert resource-type blocks: always safe (cannot be a detector).
    let type_blocked = match resource_type {
        "Image" => rules & BLOCK_IMAGES != 0,
        "Font" => rules & BLOCK_FONTS != 0,
        "Media" => rules & BLOCK_MEDIA != 0,
        _ => false,
    };
    if type_blocked {
        return true;
    }

    // Tracker blocking: ANY resource type (a tracking pixel is an
    // Image; an analytics beacon is XHR; a tag is a Script), but
    // only third-party, never same-site.
    if rules & BLOCK_TRACKERS != 0 {
        let third_party = !page_domain.is_empty() && req_domain != page_domain;
        if third_party && TRACKER_DOMAINS.contains(&req_domain.as_str()) {
            return true;
        }
    }

    false
}

/// The interception task. Subscribes to the CDP event bus and answers
/// every `Fetch.requestPaused` immediately with a block/continue
/// verdict. Runs for the life of the session; only receives events
/// while Fetch is enabled (i.e. while blocking is active).
pub async fn run_interception(
    cdp: CdpSession,
    state: InterceptState,
    mut events: SessionSubscription,
) {
    loop {
        let event = match events.recv().await {
            Ok(ev) => ev,
            // Lagged is RECOVERABLE: we skipped some events. Continue
            // listening. (Missed requestPaused events are unfortunate
            // but rare at 4096 capacity; Chrome times out hung pauses.)
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            // Channel closed: session is gone.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        if event.method != "Fetch.requestPaused" {
            continue;
        }
        let params = &event.params;
        let request_id = params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
        if request_id.is_empty() {
            continue;
        }
        let url = params
            .get("request")
            .and_then(|r| r.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("");
        let resource_type = params.get("resourceType").and_then(|v| v.as_str()).unwrap_or("");

        state.add_pending(request_id);
        let rules = state.rules();
        let page_domain = state.page_domain();
        let block = should_block(resource_type, url, rules, &page_domain);

        let verdict = if block {
            cdp.send(
                "Fetch.failRequest",
                Some(serde_json::json!({
                    "requestId": request_id,
                    "errorReason": "BlockedByClient",
                })),
            )
            .await
        } else {
            cdp.send(
                "Fetch.continueRequest",
                Some(serde_json::json!({ "requestId": request_id })),
            )
            .await
        };
        state.remove_pending(request_id);

        // A Closed session means the browser died; the serve() loop
        // relaunches and this task is dropped with the old session.
        if verdict.is_err() {
            let is_closed = matches!(
                verdict.as_ref().err(),
                Some(crate::error::BladeError::Closed)
            );
            if is_closed {
                return;
            }
        }
    }
}

impl InterceptState {
    /// Re-allow any still-paused requests (used before Fetch.disable
    /// so nothing is left hung). Best-effort.
    pub async fn drain_pending_requests(&self, cdp: &CdpSession) {
        for id in self.drain_pending() {
            let _ = cdp
                .send(
                    "Fetch.continueRequest",
                    Some(serde_json::json!({ "requestId": id })),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_classes_mask() {
        assert_eq!(InterceptState::parse_classes("images,fonts"), BLOCK_IMAGES | BLOCK_FONTS);
        assert_eq!(InterceptState::parse_classes("media"), BLOCK_MEDIA);
        assert_eq!(InterceptState::parse_classes("trackers"), BLOCK_TRACKERS);
        assert_eq!(InterceptState::parse_classes("images,garbage,fonts"), BLOCK_IMAGES | BLOCK_FONTS);
        assert_eq!(InterceptState::parse_classes(""), 0);
    }

    #[test]
    fn registrable_domain_basic() {
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("www.example.com"), "example.com");
        assert_eq!(registrable_domain("a.b.example.com"), "example.com");
        assert_eq!(registrable_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("cdn.example.co.jp"), "example.co.jp");
        assert_eq!(registrable_domain("1.2.3.4"), "1.2.3.4");
        assert_eq!(registrable_domain("localhost"), "localhost");
    }

    #[test]
    fn block_images_by_type() {
        let r = BLOCK_IMAGES;
        assert!(should_block("Image", "https://x.com/a.png", r, "x.com"));
        assert!(!should_block("Script", "https://x.com/a.js", r, "x.com"));
        // off when class not set
        assert!(!should_block("Image", "https://x.com/a.png", 0, "x.com"));
    }

    #[test]
    fn never_block_first_party_script() {
        let r = BLOCK_IMAGES | BLOCK_FONTS | BLOCK_MEDIA | BLOCK_TRACKERS;
        // same-site script: never
        assert!(!should_block("Script", "https://www.example.com/app.js", r, "example.com"));
        // same-site xhr: never
        assert!(!should_block("XHR", "https://api.example.com/data", r, "example.com"));
    }

    #[test]
    fn block_third_party_tracker_only() {
        let r = BLOCK_TRACKERS;
        // third-party tracker script: block
        assert!(should_block("Script", "https://www.google-analytics.com/analytics.js", r, "example.com"));
        // third-party tracker xhr: block
        assert!(should_block("XHR", "https://stats.mixpanel.com/track", r, "example.com"));
        // same-site tracker-looking host is NOT in list anyway, but third_party check:
        assert!(!should_block("Script", "https://example.com/analytics.js", r, "example.com"));
        // subdomain of tracker still resolves to tracker domain
        assert!(should_block("Image", "https://pixel.doubleclick.net/x.gif", r, "example.com"));
    }

    #[test]
    fn never_block_bot_detectors() {
        let r = BLOCK_IMAGES | BLOCK_FONTS | BLOCK_MEDIA | BLOCK_TRACKERS;
        // DataDome third-party script: never block
        assert!(!should_block("Script", "https://js.datadome.co/tags.js", r, "example.com"));
        // Cloudflare challenge: never block
        assert!(!should_block("Script", "https://challenges.cloudflare.com/turnstile.js", r, "example.com"));
        // PerimeterX: never
        assert!(!should_block("Script", "https://px-cdn.net/px.js", r, "example.com"));
    }

    #[test]
    fn images_blocked_even_first_party() {
        // Inert assets are always safe to block, even same-origin.
        let r = BLOCK_IMAGES;
        assert!(should_block("Image", "https://example.com/logo.png", r, "example.com"));
    }

    #[test]
    fn empty_page_domain_no_tracker_block() {
        // With unknown page origin we cannot prove third-party; be safe, don't block.
        let r = BLOCK_TRACKERS;
        assert!(!should_block("Script", "https://www.google-analytics.com/a.js", r, ""));
    }

    #[test]
    fn describe_roundtrip() {
        assert_eq!(InterceptState::describe(0), "none");
        assert_eq!(InterceptState::describe(BLOCK_IMAGES | BLOCK_MEDIA), "images,media");
    }
}
