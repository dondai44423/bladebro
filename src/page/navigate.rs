//! Navigation: scheme normalization, load+settle, domain profiles, and the
//! reddit humanity gate (the one adapter-only, human-trivial exception).

use super::*;

impl Page {
    /// Navigate to a URL. Re-registers the stealth script for the new
    /// document, sends `Page.navigate`, waits for load + settle, then
    /// recaptures and returns the delta. Shared by `act navigate`, `run`
    /// navigate steps, and the CLI `nav` command.
    pub async fn navigate(&mut self, url: &str) -> Result<PageDelta> {
        // S4+S5: track action timing for pacing + idle hum.
        self.is_busy
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let result = self.navigate_inner(url).await;
        self.is_busy
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_action_epoch
            .store(now, std::sync::atomic::Ordering::Relaxed);
        result
    }

    async fn navigate_inner(&mut self, url: &str) -> Result<PageDelta> {
        // M16: Idempotent navigate \u{2014} if already on this URL, skip reload.
        if !self.lpm.url().is_empty() && normalize_url(url) == normalize_url(self.lpm.url()) {
            let cap = capture(&self.cdp).await?;
            return Ok(self.lpm.ingest(cap));
        }
        // NOTE: no stealth re-apply here — addScriptToEvaluateOnNewDocument
        // registrations persist for the target's lifetime. Re-applying on
        // every navigation used to STACK another identical script each time
        // (N navigations = N document_start scripts). Locale changes are
        // handled by apply_domain_profile swapping the registration.
        // S11: apply per-domain stealth settings from ~/.blade/profiles.json.
        self.apply_domain_profile(url).await;
        // Reset isolated world on navigation — old context is destroyed.
        self.reset_isolated();
        // Knowledge: per-domain resource-block config the agent set before.
        // Applied only when nothing is active — an explicit session choice
        // always wins; `state block clear` erases the stored config.
        let domain = crate::knowledge::domain_from_url(url);
        let (stored_block, learned_settle) = self
            .knowledge
            .as_ref()
            .and_then(|kb| kb.lock().ok())
            .map(|kb| {
                (
                    kb.get_block_config(&domain).map(|s| s.to_string()),
                    kb.get_settle_ms(&domain),
                )
            })
            .unwrap_or((None, None));
        if let Some(spec) = stored_block.filter(|s| !s.is_empty()) {
            if self.block_rules() == 0 {
                if let Err(e) = self.set_block_classes(&spec).await {
                    eprintln!("[bladebro] stored block config failed to apply: {e}");
                }
            }
        }
        let settle_cap = crate::knowledge::nav_settle_cap_ms(learned_settle);
        let _nav_t = std::time::Instant::now();
        let _t = |label: &str| {
            if std::env::var("NAV_TIMING").is_ok() {
                eprintln!("[nav-timing] {label}: {:?}", _nav_t.elapsed());
            }
        };
        let wait = self
            .cdp
            .wait_for("Page.frameNavigated", Duration::from_secs(10));
        let target = with_scheme(url);
        self.cdp
            .send("Page.navigate", Some(serde_json::json!({ "url": target })))
            .await?;
        _t("sent");
        let _ = tokio::time::timeout(Duration::from_secs(10), wait).await;
        _t("frameNavigated");
        wait_for_load(&self.cdp, Duration::from_secs(10)).await?;
        _t("load");
        let _settle_t = std::time::Instant::now();
        wait_for_settle_with_network(
            &self.cdp,
            Duration::from_millis(settle_cap),
            Some(&self.in_flight),
        )
        .await?;
        // Bounded post-drain re-quiet: a late fetch resolving after the
        // network plateau mounts its content a moment later; this catches
        // that mount without taxing interactions (nav-only).
        let _ = re_settle(&self.cdp).await;
        _t("settle");
        // Knowledge: learn this domain's real settle duration (only when it
        // finished early — a cap timeout is not a settle sample).
        if !domain.is_empty() {
            let elapsed = _settle_t.elapsed().as_millis() as u64;
            if elapsed + 150 < settle_cap {
                if let Some(kb) = self.knowledge.as_ref() {
                    if let Ok(mut kb) = kb.lock() {
                        kb.update_timing(&domain, elapsed);
                    }
                }
            }
        }
        // M4+M6: Check for consent banners and block pages after navigation.
        // Knowledge-base integration: try stored consent selector first,
        // learn from successful dismissals, record the visit.
        let stored_consent = self
            .knowledge
            .as_ref()
            .and_then(|kb| kb.lock().ok())
            .and_then(|kb| kb.get_consent(&domain).map(|c| c.selector.clone()));
        let consent = dismiss_consent_with_stored(&self.cdp, stored_consent.as_deref())
            .await
            .unwrap_or(None);
        let blocked = detect_block(&self.cdp).await.unwrap_or(None);
        // JS challenge handling: many anti-bot systems (Reddit, Cloudflare)
        // serve a JS challenge page that a real browser solves automatically.
        // The challenge page is simple HTML and settles fast, so detect_block
        // fires BEFORE the challenge JS has time to compute + redirect.
        // Wait up to 5s polling for either a URL change OR the block
        // disappearing (some challenges solve in-place without redirect).
        // If either happens, the challenge was solved — do NOT report a block.
        // Only JS-challenge types can self-solve; rate-limit/akamai/recaptcha
        // walls never do (waiting there only added 5s latency to a final verdict).
        // Knowledge: heavier vendors get a longer self-solve window (learned
        // per domain, raised by every real block we hit there).
        let domain_risk = self
            .knowledge
            .as_ref()
            .and_then(|kb| kb.lock().ok())
            .map(|kb| kb.get_bot_risk(&domain))
            .unwrap_or_default();
        let challenge_polls: u32 = if domain_risk >= crate::knowledge::BotRiskLevel::Heavy {
            16
        } else {
            10
        };
        let mut challenge_seen = false;
        let blocked = match blocked.as_deref() {
            Some("cloudflare") | Some("datadome") | Some("perimeterx") => {
                challenge_seen = true;
                let pre_url = eval_location_href(&self.cdp).await;
                let mut solved = false;
                for _ in 0..challenge_polls {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let post_url = eval_location_href(&self.cdp).await;
                    if post_url != pre_url && !post_url.is_empty() {
                        solved = true;
                        break;
                    }
                    // Also check if the block disappeared without a URL change
                    // (some challenges solve in-place, replacing page content).
                    if detect_block(&self.cdp).await.unwrap_or(None).is_none() {
                        solved = true;
                        break;
                    }
                }
                if solved {
                    wait_for_settle_with_network(
                        &self.cdp,
                        Duration::from_millis(2500),
                        Some(&self.in_flight),
                    )
                    .await?;
                    let _ = re_settle(&self.cdp).await;
                    None // clear block — was a JS challenge, not a real block
                } else {
                    blocked
                }
            }
            Some("js-challenge") => {
                // Reddit's browser-solvable challenge: a hidden form that
                // auto-submits `solution=<token><token>` and sets the `loid`
                // token on the solved response. The page navigates itself in
                // ~1s; wait bounded for it (URL change or wall gone), then
                // settle on the real content. No interaction needed.
                challenge_seen = true;
                let pre_url = eval_location_href(&self.cdp).await;
                let mut solved = false;
                for _ in 0..challenge_polls {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let post_url = eval_location_href(&self.cdp).await;
                    if post_url != pre_url && !post_url.is_empty() {
                        solved = true;
                        break;
                    }
                    if detect_block(&self.cdp).await.unwrap_or(None).is_none() {
                        solved = true;
                        break;
                    }
                }
                if solved {
                    wait_for_settle_with_network(
                        &self.cdp,
                        Duration::from_millis(2500),
                        Some(&self.in_flight),
                    )
                    .await?;
                    let _ = re_settle(&self.cdp).await;
                    None // clear block — the challenge solved itself
                } else {
                    blocked
                }
            }
            Some("reddit-humanity") => {
                // Reddit's one-time humanity check (reCAPTCHA v2 checkbox)
                // for sessions without the `loid` token. One humanized click
                // on the checkbox passes it; the solve grants `loid` and the
                // wall does not return for this profile. An image grid (if
                // Google serves one) is not solvable in-house — report it.
                challenge_seen = true;
                match solve_reddit_humanity(&self.cdp).await {
                    HumanityOutcome::Solved => {
                        wait_for_settle_with_network(
                            &self.cdp,
                            Duration::from_millis(2500),
                            Some(&self.in_flight),
                        )
                        .await?;
                        let _ = re_settle(&self.cdp).await;
                        if let Ok(mut a) = self.ambient.lock() {
                            a.push(
                                "reddit: humanity check solved automatically (one humanized click — grant stored for this profile)".into(),
                            );
                        }
                        None
                    }
                    HumanityOutcome::Grid => {
                        if let Ok(mut a) = self.ambient.lock() {
                            a.push(
                                "reddit: the humanity check escalated to an image grid — not solvable automatically; solve it once manually in the browser (the grant then persists for this profile), or retry later".into(),
                            );
                        }
                        blocked
                    }
                    HumanityOutcome::Failed => blocked,
                }
            }
            Some("reddit") => {
                // The network-security wall is a soft, transient flag (its
                // own 403 carries `retry-after: 0`) — a reload clears it in
                // most cases. Walk a small jittered reload ladder before
                // reporting a block; a domain already known Heavy gets one
                // attempt instead of two.
                challenge_seen = true;
                let attempts: u32 = if domain_risk >= crate::knowledge::BotRiskLevel::Heavy {
                    1
                } else {
                    2
                };
                let mut cleared = false;
                for attempt in 0..attempts {
                    let jitter = 1100
                        + attempt as u64 * 900
                        + std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_millis() as u64)
                            .unwrap_or(200)
                            % 350;
                    tokio::time::sleep(Duration::from_millis(jitter)).await;
                    let _ = self
                        .cdp
                        .send(
                            "Page.reload",
                            Some(serde_json::json!({ "ignoreCache": false })),
                        )
                        .await;
                    let _ = wait_for_load(&self.cdp, Duration::from_secs(10)).await;
                    let _ = wait_for_settle_with_network(
                        &self.cdp,
                        Duration::from_millis(2500),
                        Some(&self.in_flight),
                    )
                    .await;
                    let _ = re_settle(&self.cdp).await;
                    let now = detect_block(&self.cdp).await.unwrap_or(None);
                    if !matches!(now.as_deref(), Some("reddit")) {
                        cleared = true;
                        break;
                    }
                }
                if cleared {
                    if let Ok(mut a) = self.ambient.lock() {
                        a.push(
                            "reddit: transient network-security wall — auto-cleared on reload"
                                .into(),
                        );
                    }
                    None
                } else {
                    blocked
                }
            }
            other => other.map(String::from),
        };
        // Knowledge: persist what this domain does to us — a real block wall
        // counts in stats and raises the domain's risk level; a solved JS
        // challenge marks the domain as challenge-serving (medium).
        if !domain.is_empty() {
            if let Some(kb) = self.knowledge.as_ref() {
                if let Ok(mut kb) = kb.lock() {
                    if let Some(ref bt) = blocked {
                        kb.record_block_detected();
                        kb.raise_bot_risk(&domain, crate::knowledge::vendor_risk(bt));
                    } else if challenge_seen {
                        kb.raise_bot_risk(&domain, crate::knowledge::BotRiskLevel::Medium);
                    }
                }
            }
        }
        // Learn from successful consent dismissal (only specific selectors, not "generic").
        if let Some(ref result) = consent {
            if result != "generic" && !result.is_empty() && !domain.is_empty() {
                if let Some(kb) = self.knowledge.as_ref() {
                    if let Ok(mut kb) = kb.lock() {
                        kb.learn_consent_result(&domain, result);
                        kb.record_consent_dismissed();
                    }
                }
            }
            if let Ok(mut a) = self.ambient.lock() {
                a.push(format!(
                    "consent: {} ({})",
                    if std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".into())
                        != "accept"
                    {
                        "rejected"
                    } else {
                        "accepted"
                    },
                    result
                ));
            }
        }
        if let Some(ref bt) = blocked {
            if let Ok(mut a) = self.ambient.lock() {
                a.push(format!("blocked: {}", bt));
                for step in crate::page::perception::remediation_ladder(bt) {
                    a.push(format!("  remediation: {}", step));
                }
            }
        }
        // Record visit + navigation for this domain.
        if !domain.is_empty() {
            if let Some(kb) = self.knowledge.as_ref() {
                if let Ok(mut kb) = kb.lock() {
                    kb.record_visit(&domain);
                    kb.record_navigation();
                }
            }
        }
        _t("consent/block");
        let r = self.recapture().await;
        _t("recapture");
        r
    }

    /// S11: apply per-domain stealth settings from ~/.blade/profiles.json.
    /// Stores timezone and locale overrides per-domain so the driver remembers
    /// which settings work for each site. The agent can edit the file directly.
    async fn apply_domain_profile(&mut self, url: &str) {
        // Real-browser lane: per-domain tz/locale overrides are page-visible
        // masks, and this lane's contract is that nothing page-visible is
        // manufactured. Return before any CDP call.
        if crate::realbrowser::real_lane() {
            return;
        }
        let domain = extract_domain(url);
        if domain.is_empty() {
            return;
        }
        let path = crate::platform::blade_dir().join("profiles.json");
        let profiles: std::collections::HashMap<String, DomainProfile> =
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
        if let Some(profile) = profiles.get(&domain) {
            if let Some(ref tz) = profile.tz {
                let _ = self
                    .cdp
                    .send(
                        "Emulation.setTimezoneOverride",
                        Some(serde_json::json!({ "timezoneId": tz })),
                    )
                    .await;
                eprintln!("[bladebro] domain profile {domain}: tz={tz}");
            }
            if let Some(ref locale) = profile.locale {
                let _ = self
                    .cdp
                    .send(
                        "Emulation.setLocaleOverride",
                        Some(serde_json::json!({ "locale": locale })),
                    )
                    .await;
                // Keep Accept-Language in sync — it was set once at attach;
                // a swapped locale with a stale header is a cross-layer
                // mismatch fingerprint.
                let base = locale.split('-').next().unwrap_or(locale);
                let _ = self
                    .cdp
                    .send(
                        "Network.setExtraHTTPHeaders",
                        Some(serde_json::json!({
                            "headers": { "Accept-Language": format!("{locale},{base};q=0.9") }
                        })),
                    )
                    .await;
                eprintln!("[bladebro] domain profile {domain}: locale={locale}");
            }

            // S11 coherence: navigator.language comes from the INJECTED
            // script, not the CDP override. If the injection bakes a
            // different locale, navigator.language and Intl disagree — a
            // fingerprint-visible mismatch. Swap the registration (remove
            // + re-add, never stack) so both layers speak the same locale.
            let want_locale = profile
                .locale
                .clone()
                .or_else(|| std::env::var("BLADE_LOCALE").ok().filter(|s| !s.is_empty()));
            if want_locale != self.active_locale {
                if let Some(id) = self.stealth_script_id.take() {
                    let _ = self
                        .cdp
                        .send(
                            "Page.removeScriptToEvaluateOnNewDocument",
                            Some(serde_json::json!({ "identifier": id })),
                        )
                        .await;
                }
                match crate::stealth::apply_stealth(&self.cdp, profile.locale.as_deref()).await {
                    Ok(id) => {
                        self.stealth_script_id = Some(id);
                        self.active_locale = want_locale;
                    }
                    Err(e) => eprintln!("[bladebro] WARNING: locale swap injection failed: {e}"),
                }
            }
        }
    }
}

/// S11: per-domain stealth settings stored in ~/.blade/profiles.json.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct DomainProfile {
    tz: Option<String>,
    locale: Option<String>,
}

/// Extract the registrable domain from a URL for profile lookup.
/// Reddit's "Prove your humanity" wall is a reCAPTCHA v2 checkbox. One
/// humanized click on the (cross-origin) anchor iframe passes it — the
/// solve auto-submits to `?captcha=1` and grants the `loid` token, after
/// which the wall does not return for that profile.
///
/// Outcomes: `Solved` (grant obtained), `Grid` (Google escalated to an
/// image challenge — not solvable in-house, report honestly), `Failed`
/// (no widget to click, dispatch error, or no verdict in the window).
async fn solve_reddit_humanity(cdp: &CdpSession) -> HumanityOutcome {
    // The widget loads async (recaptcha scripts come from google) — wait
    // bounded for the anchor iframe to exist at a sane size, then one short
    // beat so the widget's own JS is listening before the click lands (an
    // early click is swallowed silently — observed live).
    let mut point: Option<(f64, f64)> = None;
    for _ in 0..12 {
        point = probe_click_point(cdp).await;
        if point.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let Some((x, y)) = point else {
        return HumanityOutcome::Failed;
    };
    tokio::time::sleep(Duration::from_millis(700)).await;
    let last_mouse = std::sync::Arc::new(std::sync::Mutex::new(None));
    if crate::action::dispatch_mouse_click(cdp, x, y, &last_mouse)
        .await
        .is_err()
    {
        return HumanityOutcome::Failed;
    }
    // Poll bounded for the verdict: URL change or a filled token = solved;
    // a visible b-frame twice in a row = an image grid is showing (bail).
    // One click only — re-clicking could disturb a slow-but-passing
    // verification, and a swallowed click is reported honestly instead.
    let pre_url = eval_location_href(cdp).await;
    let check = r#"(function(){var t=document.querySelector('#g-recaptcha-response');if(t&&t.value)return 'solved';var fs=document.querySelectorAll('iframe');for(var i=0;i<fs.length;i++){var s=fs[i].src||'';if(s.indexOf('bframe')>=0){var r=fs[i].getBoundingClientRect();if(r.y>-200&&r.width>0)return 'grid';}}return 'wait';})()"#;
    let mut grid_seen = 0u32;
    let mut re_clicks = 0u32;
    for i in 0..60 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let post_url = eval_location_href(cdp).await;
        if post_url != pre_url && !post_url.is_empty() {
            return HumanityOutcome::Solved;
        }
        let state = cdp
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({ "expression": check, "returnByValue": true })),
            )
            .await
            .ok()
            .and_then(|r| {
                r.get("result")
                    .and_then(|x| x.get("value"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();
        if state == "solved" {
            return HumanityOutcome::Solved;
        }
        if state == "grid" {
            grid_seen += 1;
            if grid_seen >= 2 {
                return HumanityOutcome::Grid;
            }
        } else {
            grid_seen = 0;
        }
        // An early click can be swallowed before the widget listens; two
        // spaced re-clicks recover exactly that case. A click that IS
        // being verified also keeps state 'wait' — a checking widget
        // ignores extra clicks, and this hedge is bounded at two.
        if state == "wait" && (i == 8 || i == 24) && re_clicks < 2 {
            re_clicks += 1;
            if let Some((x2, y2)) = probe_click_point(cdp).await {
                let lm = std::sync::Arc::new(std::sync::Mutex::new(None));
                let _ = crate::action::dispatch_mouse_click(cdp, x2, y2, &lm).await;
            }
        }
    }
    HumanityOutcome::Failed
}

/// Outcome of a humanity-check solve attempt.
enum HumanityOutcome {
    Solved,
    Grid,
    Failed,
}

/// Resolve the recaptcha anchor checkbox click point (left-center of the
/// anchor iframe), or None while the widget is not mounted at a sane size.
async fn probe_click_point(cdp: &CdpSession) -> Option<(f64, f64)> {
    let probe = r#"(function(){if(typeof window.grecaptcha==='undefined'||typeof window.grecaptcha.getResponse!=='function')return null;var fs=document.querySelectorAll('iframe');for(var i=0;i<fs.length;i++){var s=fs[i].src||'';if(s.indexOf('/recaptcha/api2/anchor')>=0){var r=fs[i].getBoundingClientRect();if(r.width<60||r.height<30)return null;return JSON.stringify({x:Math.round(r.x+30),y:Math.round(r.y+r.height/2)});}}return null;})()"#;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({ "expression": probe, "returnByValue": true })),
        )
        .await
        .ok()?;
    let point = res.get("result")?.get("value")?.as_str()?.to_string();
    let p: serde_json::Value = serde_json::from_str(&point).ok()?;
    let (x, y) = (p.get("x")?.as_f64()?, p.get("y")?.as_f64()?);
    if x <= 0.0 || y <= 0.0 {
        return None;
    }
    Some((x, y))
}

/// Get the current page URL via CDP. Used by JS challenge detection
/// to detect redirects after a challenge page is served.
async fn eval_location_href(cdp: &CdpSession) -> String {
    cdp.send(
        "Runtime.evaluate",
        Some(json!({
            "expression": "location.href",
            "returnByValue": true,
        })),
    )
    .await
    .ok()
    .and_then(|r| {
        r.get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .map(String::from)
    })
    .unwrap_or_default()
}

fn extract_domain(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_string()
}

/// Add a scheme to user-supplied URLs so bare hosts work everywhere
/// (`example.com`, `localhost:3000`). Local/private hosts and IPs default to
/// http:// (dev servers rarely have certs), public hosts to https://.
/// URLs that already carry a scheme (http/https/about/file/data/blob/...) are
/// left untouched.
pub(crate) fn with_scheme(url: &str) -> String {
    let u = url.trim();
    if u.is_empty()
        || u.contains("://")
        || u.starts_with("about:")
        || u.starts_with("file:")
        || u.starts_with("data:")
        || u.starts_with("blob:")
        || u.starts_with("javascript:")
    {
        return u.to_string();
    }
    let host = u.split('/').next().unwrap_or(u);
    // localhost, loopback/private IPs, or an explicit port (dev-server
    // signal) default to http://; public hosts to https://.
    let is_local = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| match ip {
                std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
                std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local(),
            })
            .unwrap_or(false)
        || host.contains(':');
    if is_local {
        format!("http://{u}")
    } else {
        format!("https://{u}")
    }
}

/// Normalize a URL for comparison: strip scheme, fragment, trailing slash.
fn normalize_url(url: &str) -> String {
    let (s, https) = url
        .strip_prefix("https://")
        .map(|s| (s, true))
        .or_else(|| url.strip_prefix("http://").map(|s| (s, false)))
        .unwrap_or((url, false));
    let s = s.split('#').next().unwrap_or(s);
    // Strip default ports: :443 on https, :80 on http (host part only).
    let default_port = if https { ":443" } else { ":80" };
    let s = if let Some(slash) = s.find('/') {
        let (host, path) = s.split_at(slash);
        let host = host.strip_suffix(default_port).unwrap_or(host);
        format!("{host}{path}")
    } else {
        s.strip_suffix(default_port).unwrap_or(s).to_string()
    };
    s.strip_suffix('/').unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::{extract_domain, normalize_url};

    #[test]
    fn extract_domain_strips_scheme_port_and_www() {
        assert_eq!(
            extract_domain("https://www.example.com/a/b?q=1"),
            "example.com"
        );
        assert_eq!(extract_domain("http://localhost:3000/x"), "localhost");
        assert_eq!(extract_domain("example.com/path"), "example.com");
    }

    #[test]
    fn normalize_url_strips_scheme_fragment_and_default_port() {
        assert_eq!(normalize_url("https://example.com/"), "example.com");
        assert_eq!(
            normalize_url("https://example.com:443/x#frag"),
            "example.com/x"
        );
        assert_eq!(normalize_url("http://example.com:80"), "example.com");
        assert_eq!(
            normalize_url("http://example.com:8080/x/"),
            "example.com:8080/x"
        );
    }
}
