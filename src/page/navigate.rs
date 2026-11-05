//! Navigation: scheme normalization, load+settle, domain profiles, and the
//! reddit humanity gate (the one adapter-only, human-trivial exception).

use super::*;

impl Page {
    /// Navigate to a URL. Re-registers the stealth script for the new
    /// document, sends `Page.navigate`, waits for load + settle, then
    /// recaptures and returns the delta. Shared by `act navigate`, `run`
    /// navigate steps, and the CLI `nav` command.
    pub async fn navigate(&mut self, url: &str) -> Result<PageDelta> {
        self.activate().await?;
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
        // R1: reject schemes that cannot load a page BEFORE any CDP work.
        // ftp://, javascript:, chrome:// etc. used to sail into
        // Page.navigate, where the browser silently refused: the driver
        // paid the full 10s frameNavigated wait and then reported the
        // unchanged page as "already here". The gate answers instantly.
        validate_nav_url(url)?;
        // M16: Idempotent navigate \u{2014} if already on this URL, skip reload.
        if already_here(&self.lpm, url) {
            let cap = capture(&self.cdp).await?;
            return Ok(self.lpm.ingest(cap));
        }
        let from_unanswered = self.lpm.read_unanswered();
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
        let nav_res = self
            .cdp
            .send("Page.navigate", Some(serde_json::json!({ "url": &target })))
            .await?;
        let nav_error: Option<String> = nav_res
            .get("errorText")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        _t("sent");
        // R1: when the browser reports WHY it refused the navigation, give a
        // superseded page-side navigation a short grace to fire its own
        // frameNavigated. If nothing navigated, surface the browser's reason
        // instead of waiting the full 10s and then reporting the unchanged
        // page as "already here" - a confident lie that branch-switched
        // agents into wrong work.
        let grace = if nav_error.is_some() {
            Duration::from_millis(1500)
        } else {
            Duration::from_secs(10)
        };
        if nav_res.get("isDownload").and_then(Value::as_bool) == Some(true) {
            return Err(BladeError::Other(format!("navigation to {target} started a download — use act download url=... to retrieve the file")));
        }
        // CDP omits loaderId for same-document navigations; those emit
        // navigatedWithinDocument, never frameNavigated. Waiting for the
        // latter adds ten seconds to every hash-router transition.
        let same_document = nav_error.is_none() && nav_res.get("loaderId").is_none();
        let nav_event = if same_document {
            None
        } else {
            Some(tokio::time::timeout(grace, wait).await)
        };
        _t("frameNavigated");
        if matches!(nav_event, Some(result) if !matches!(result, Ok(Ok(_)))) {
            if let Some(err) = nav_error.as_deref() {
                let hint = if err.contains("ERR_ABORTED") {
                    " (the URL may be a file download - try act download url=...)"
                } else {
                    ""
                };
                return Err(crate::error::BladeError::Other(format!(
                    "navigation to {target} failed: {err}{hint}"
                )));
            }
        }
        // L7: an unreachable target (refused connection, DNS, TLS, unsafe
        // port, dead proxy) is not refused - Chrome COMMITS its own error
        // page in its place, so frameNavigated fires inside the grace above
        // and this used to report "navigated" with "This site can't be
        // reached" as the site's content (exit 0). A page-side navigation
        // that superseded ours (ERR_ABORTED, then a real document) still
        // proceeds: only Chrome's error document in the main frame fails.
        if let Some(err) = nav_error.as_deref() {
            let tree = self.cdp.send("Page.getFrameTree", None).await.ok();
            let frame = tree.as_ref().and_then(|t| t.pointer("/frameTree/frame"));
            // Name BLADE_PROXY only when it is the proxy this browser was
            // launched with (the real lane runs the person's own Chrome).
            let blade_proxy = !crate::realbrowser::real_lane()
                && matches!(crate::browser::proxy::env_blade_proxy(), Ok(Some(_)));
            if let Some(msg) = error_page_failure(&target, err, frame, blade_proxy) {
                // Recapture so the error's page state is the tab's real
                // state (Chrome's error page), not the page we left.
                let _ = wait_for_load(&self.cdp, Duration::from_secs(3)).await;
                let _ = self.recapture().await;
                return Err(crate::error::BladeError::Other(msg));
            }
        }
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
        let r = self.recapture().await.map(|d| landed(d, from_unanswered));
        _t("recapture");
        // P1: query parameters the caller asked for that the final URL does not
        // carry. Sites silently rewrite their own URLs (Amazon drops rh=p_36,
        // search engines strip filters); the page then renders happily and the
        // agent trusts results that never had the filter applied. Observation
        // only — it names what is missing, never why.
        if let Ok(delta) = &r {
            if delta.navigated {
                if let Some(note) = nav_params_note(url, self.lpm.url()) {
                    if let Ok(mut a) = self.ambient.lock() {
                        a.push(note);
                    }
                }
            }
        }
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

/// L7: the error for a navigation whose main frame committed Chrome's own
/// error page (`chrome-error://chromewebdata/`, `unreachableUrl` set) in
/// place of the target, or None when a real document is showing.
/// `blade_proxy`: the browser runs behind BLADE_PROXY (names the recovery
/// path for a proxy/tunnel failure).
fn error_page_failure(
    target: &str,
    err: &str,
    frame: Option<&Value>,
    blade_proxy: bool,
) -> Option<String> {
    let frame = frame?;
    let unreachable = frame
        .get("unreachableUrl")
        .and_then(Value::as_str)
        .is_some_and(|u| !u.is_empty());
    let error_doc = frame
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|u| u.starts_with("chrome-error://"));
    if !(unreachable || error_doc) {
        return None;
    }
    let hint = if err.contains("PROXY") || err.contains("TUNNEL") {
        if blade_proxy {
            "; the proxy in BLADE_PROXY refused or is down"
        } else {
            "; the browser's proxy refused or is down"
        }
    } else if err.contains("ERR_HTTP_RESPONSE_CODE_FAILURE") {
        // The server did answer, but with an error status and no body of
        // its own, so Chrome rendered its "HTTP ERROR" page instead.
        "; the server answered with an HTTP error status and an empty body"
    } else {
        ""
    };
    Some(format!(
        "navigation to {target} failed: {err} (the tab shows Chrome's error page, not the site{hint})"
    ))
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
    // Read the host the navigation will actually open: a bare host gets its
    // scheme first, so a `://` inside the query (`?next=http://x`) is never
    // mistaken for this URL's own scheme separator.
    let url = with_scheme(url);
    after_scheme_slashes(&url)
        .unwrap_or(&url)
        .split(['/', '?', '#'])
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
        || after_scheme_slashes(u).is_some()
        || scheme_prefix(u).is_some_and(|s| {
            matches!(
                s.to_ascii_lowercase().as_str(),
                "about" | "file" | "data" | "blob" | "javascript"
            )
        })
    {
        return u.to_string();
    }
    let parsed = url::Url::parse(&format!("http://{u}")).ok();
    let host = parsed.as_ref().and_then(|url| url.host_str()).unwrap_or("");
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
        || u.split(['/', '?', '#'])
            .next()
            .and_then(|authority| authority.rsplit_once(':'))
            .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        || host.contains(':');
    if is_local {
        format!("http://{u}")
    } else {
        format!("https://{u}")
    }
}

/// R1: reject a navigation target whose scheme cannot produce a page, before
/// any browser work. `ftp://`, `javascript:`, `mailto:` etc. used to reach
/// `Page.navigate`, where Chrome refused silently: the driver then burned
/// the full 10s frameNavigated wait and reported the unchanged page as
/// "already here" (10.3s + a false verdict, reproduced twice). The gate
/// answers in microseconds with the supported set.
///
/// Allowed: http(s) (and about/data/file/blob, which can load documents).
/// A bare host or `host:port` (all-digit port) is not a scheme - it passes
/// through for [`with_scheme`]. The scheme names that conflict with the
/// `host:port` shape (javascript:1, ftp:...) are rejected by name so a
/// digit-only tail cannot smuggle them through.
pub(crate) fn validate_nav_url(url: &str) -> Result<()> {
    let u = url.trim();
    if u.is_empty() {
        return Err(crate::error::BladeError::Other(
            "navigate: empty URL - pass the page to open".into(),
        ));
    }
    let Some(scheme) = scheme_prefix(u) else {
        return Ok(()); // bare host or path form - with_scheme handles it
    };
    let s = scheme.to_ascii_lowercase();
    const NAVIGABLE: &[&str] = &["http", "https", "about", "data", "file", "blob"];
    if NAVIGABLE.contains(&s.as_str()) {
        return Ok(());
    }
    // Known non-page schemes must be rejected even when their tail looks
    // like a port (`javascript:1`, `tel:555`). Everything else that is not
    // host:port falls through to the same explicit error.
    let rest = &u[scheme.len() + 1..];
    let portish = !rest.is_empty()
        && rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .bytes()
            .all(|b| b.is_ascii_digit());
    if portish && !is_known_scheme_name(&s) {
        return Ok(()); // `localhost:3000`, `example.com:8080/x`
    }
    Err(crate::error::BladeError::Other(format!(
        "unsupported URL scheme \"{s}:\" - navigable schemes are http(s), about, data, file and blob (this target was rejected before any browser work)"
    )))
}

/// The scheme prefix of a URL (`scheme:` before any `/`, `?`, `#`), or None.
/// Scheme grammar: ASCII alpha, then alnum/`+`/`-`/`.`, then `:`.
fn scheme_prefix(url: &str) -> Option<&str> {
    let bytes = url.as_bytes();
    if !bytes.first().is_some_and(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.' => {}
            b':' => return Some(&url[..i]),
            _ => return None, // '/' '?' '#' or invalid scheme byte
        }
    }
    None
}

/// The rest of a URL that STARTS with `scheme://`, or None. Only a leading
/// scheme counts: a bare host carrying a URL in its query or fragment
/// (`127.0.0.1:8080/?next=http://x`) used to read as already-schemed, went
/// to Chrome without one and failed as "Cannot navigate to invalid URL".
fn after_scheme_slashes(url: &str) -> Option<&str> {
    let scheme = scheme_prefix(url)?;
    url[scheme.len() + 1..].strip_prefix("//")
}

/// Scheme names that look like they could be `host:port` but are schemes.
fn is_known_scheme_name(s: &str) -> bool {
    matches!(
        s,
        "javascript"
            | "ftp"
            | "ftps"
            | "ws"
            | "wss"
            | "mailto"
            | "tel"
            | "sms"
            | "callto"
            | "chrome"
            | "chrome-extension"
            | "devtools"
            | "edge"
            | "view-source"
            | "magnet"
            | "intent"
            | "market"
            | "itms-apps"
    )
}

pub(crate) fn query_params(url: &str) -> Option<Vec<(String, String)>> {
    let q = url.split_once('?')?.1.split('#').next().unwrap_or("");
    if q.is_empty() {
        return None;
    }
    Some(
        q.split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (p.to_string(), String::new()),
            })
            .collect(),
    )
}

/// Query parameters that exist for ATTRIBUTION only. A site dropping these is
/// not a lost filter, and reporting them would train the caller to ignore the
/// real signal.
const TRACKING_PARAMS: &[&str] = &[
    "gclid",
    "fbclid",
    "msclkid",
    "yclid",
    "twclid",
    "srsltid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "ref",
    "ref_",
    "referrer",
    "source",
    "spm",
    "psc",
    "th",
    "ascsubtag",
    "linkcode",
    "creative",
    "si",
];

fn is_tracking_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("utm_") || TRACKING_PARAMS.contains(&n.as_str())
}

/// The note for requested query parameters the final URL does not carry, or
/// None when every meaningful parameter survived.
fn nav_params_note(requested: &str, final_url: &str) -> Option<String> {
    let req = query_params(requested)?;
    let fin = query_params(final_url).unwrap_or_default();
    let mut missing: Vec<String> = Vec::new();
    let mut rewritten: Vec<String> = Vec::new();
    for (k, v) in &req {
        if is_tracking_param(k) {
            continue;
        }
        match fin.iter().find(|(fk, _)| fk == k) {
            None => missing.push(k.clone()),
            Some((_, fv)) => {
                if !v.is_empty() && fv != v {
                    rewritten.push(k.clone());
                }
            }
        }
    }
    if missing.is_empty() && rewritten.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("dropped: {}", missing.join(", ")));
    }
    if !rewritten.is_empty() {
        parts.push(format!("value changed: {}", rewritten.join(", ")));
    }
    Some(format!(
        "url params: the final URL does not carry the requested query parameter(s) ({}) — the site rewrote the request, so do not assume those filters or options were applied",
        parts.join("; ")
    ))
}

/// M16 shortcut gate: the model's url is the page `url` points at. Only a
/// url the page itself answered with counts. After an unanswered read the
/// model keeps the previous page while the tab may already be elsewhere;
/// taking the shortcut then would skip the real navigation and report
/// wherever the tab is as the destination.
fn already_here(lpm: &LivePageModel, url: &str) -> bool {
    !lpm.read_unanswered()
        && !lpm.url().is_empty()
        && normalize_url(url) == normalize_url(lpm.url())
}

/// The delta of a navigation that was really issued. When it started from an
/// unanswered read, the model's url is the last ANSWERED page, not where the
/// tab was, so landing on that same url proves nothing: the tab may have
/// been elsewhere (a click that reached a busy page). Report the navigation
/// that happened instead of "already here".
fn landed(mut delta: PageDelta, from_unanswered: bool) -> PageDelta {
    if from_unanswered && !delta.unanswered {
        delta.navigated = true;
    }
    delta
}

/// Canonical comparison preserves the origin, fragment and significant path.
/// Only URL-standard equivalences (host case, default ports, root slash) may
/// suppress a navigation; hash routers and /x versus /x/ are different pages.
fn normalize_url(value: &str) -> String {
    let value = with_scheme(value);
    url::Url::parse(&value)
        .map(|url| url.to_string())
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::{
        already_here, error_page_failure, extract_domain, landed, nav_params_note, normalize_url,
        query_params, validate_nav_url,
    };
    use serde_json::json;

    /// L7: an unreachable target (refused, DNS, TLS, unsafe port, dead
    /// proxy) makes Chrome commit its own error page: errorText is set AND
    /// frameNavigated fires. That main frame (CDP `Page.Frame`) is a failed
    /// navigation, not "navigated → chrome-error://chromewebdata/".
    #[test]
    fn chrome_error_page_in_the_main_frame_fails_the_navigation() {
        let error_page = json!({
            "id": "MAIN",
            "url": "chrome-error://chromewebdata/",
            "unreachableUrl": "http://127.0.0.1:9/",
        });
        let msg = error_page_failure(
            "http://127.0.0.1:9/",
            "net::ERR_UNSAFE_PORT",
            Some(&error_page),
            false,
        )
        .expect("Chrome's error page must fail the navigation");
        assert!(
            msg.starts_with("navigation to http://127.0.0.1:9/ failed: net::ERR_UNSAFE_PORT"),
            "{msg}"
        );
        assert!(msg.contains("Chrome's error page"), "{msg}");
        // Either signal alone is enough.
        let url_only = json!({"id": "MAIN", "url": "chrome-error://chromewebdata/"});
        let msg = error_page_failure(
            "http://site.test/",
            "net::ERR_PROXY_CONNECTION_FAILED",
            Some(&url_only),
            true,
        )
        .expect("error document by url");
        assert!(
            msg.contains("BLADE_PROXY"),
            "a proxy failure names the proxy: {msg}"
        );
        // Without BLADE_PROXY (the real lane, a system proxy) the hint must
        // not send the agent to a variable that is not in play.
        let msg = error_page_failure(
            "http://site.test/",
            "net::ERR_TUNNEL_CONNECTION_FAILED",
            Some(&url_only),
            false,
        )
        .expect("error document by url");
        assert!(
            msg.contains("the browser's proxy") && !msg.contains("BLADE_PROXY"),
            "{msg}"
        );
        // An error status with an empty body also lands on Chrome's page.
        let msg = error_page_failure(
            "http://h/empty",
            "net::ERR_HTTP_RESPONSE_CODE_FAILURE",
            Some(&url_only),
            false,
        )
        .expect("error document by url");
        assert!(msg.contains("HTTP error status"), "{msg}");
        // A page-side navigation that superseded ours committed a REAL
        // document: that is still a successful navigation.
        let real = json!({"id": "MAIN", "url": "http://127.0.0.1:8080/next"});
        assert_eq!(
            error_page_failure(
                "http://127.0.0.1:8080/a",
                "net::ERR_ABORTED",
                Some(&real),
                false
            ),
            None
        );
        // Frame tree unavailable: no claim either way.
        assert_eq!(
            error_page_failure("http://h/", "net::ERR_ABORTED", None, false),
            None
        );
    }

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
    fn dropped_query_params_are_reported_and_tracking_noise_is_not() {
        // The Amazon case: the site rewrote the URL without the price filter.
        let note = nav_params_note(
            "https://www.amazon.in/s?k=ssd&rh=p_36%3A35000-45000",
            "https://www.amazon.in/s?k=ssd",
        )
        .expect("a dropped filter must be reported");
        assert!(note.contains("rh"), "{note}");
        assert!(note.contains("dropped"), "{note}");
        // Surviving params stay quiet — no news is the normal case.
        assert!(nav_params_note(
            "https://example.com/s?q=x&page=2",
            "https://example.com/s?q=x&page=2"
        )
        .is_none());
        // A rewritten value is named as such, not as a drop.
        let note = nav_params_note("https://e.com/s?page=2", "https://e.com/s?page=1")
            .expect("rewritten value must be reported");
        assert!(
            note.contains("value changed") && note.contains("page"),
            "{note}"
        );
        // Attribution-only params never fire (that noise trains callers to
        // ignore the real signal).
        assert!(nav_params_note(
            "https://e.com/s?q=x&utm_source=nl&gclid=1",
            "https://e.com/s?q=x"
        )
        .is_none());
        // No query at all → nothing to compare.
        assert!(nav_params_note("https://e.com/", "https://e.com/").is_none());
    }

    #[test]
    fn query_params_parses_valueless_and_fragmented_forms() {
        let p = query_params("https://e.com/s?flag&x=1#frag").expect("parse");
        assert_eq!(
            p,
            vec![
                ("flag".to_string(), String::new()),
                ("x".to_string(), "1".to_string())
            ]
        );
        assert!(query_params("https://e.com/s").is_none());
        assert!(query_params("https://e.com/s?").is_none());
    }

    /// The idempotent-navigate shortcut trusted the model's url even after
    /// an unanswered read, when that url is the last ANSWERED page, not
    /// where the tab is: `navigate A` after a click that went to B (read
    /// unanswered) skipped the navigation and reported B as A's result.
    #[test]
    fn idempotent_shortcut_needs_an_answered_url() {
        use crate::page::{LivePageModel, PageCapture};
        let page = |url: &str| PageCapture {
            url: url.into(),
            title: String::new(),
            ready_state: "complete".into(),
            muts: 0,
            elements: Vec::new(),
        };
        let mut lpm = LivePageModel::new();
        assert!(!already_here(&lpm, "https://a.test/"));
        lpm.ingest(page("https://a.test/"));
        assert!(already_here(&lpm, "https://a.test/"));
        lpm.ingest(PageCapture::unanswered());
        assert_eq!(lpm.url(), "https://a.test/");
        assert!(
            !already_here(&lpm, "https://a.test/"),
            "an unanswered read must not satisfy the already-here shortcut"
        );
        lpm.ingest(page("https://a.test/"));
        assert!(already_here(&lpm, "https://a.test/"));
    }

    /// With the shortcut skipped, `navigate A` from a tab whose last read
    /// went unanswered (really on B) lands on A while the model still says
    /// A: the delta must report the navigation, not "already here".
    #[test]
    fn navigation_from_an_unanswered_read_reports_the_landing() {
        use crate::page::{LivePageModel, PageCapture};
        let page = |url: &str| PageCapture {
            url: url.into(),
            title: String::new(),
            ready_state: "complete".into(),
            muts: 0,
            elements: Vec::new(),
        };
        let mut lpm = LivePageModel::new();
        lpm.ingest(page("https://a.test/"));
        lpm.ingest(PageCapture::unanswered());
        let from_unanswered = lpm.read_unanswered();
        let d = landed(lpm.ingest(page("https://a.test/")), from_unanswered);
        assert!(
            d.navigated,
            "a navigation from an unanswered read was reported as already here"
        );
        // An answered start keeps the plain comparison.
        let d = landed(lpm.ingest(page("https://a.test/")), lpm.read_unanswered());
        assert!(!d.navigated);
        // A landing that itself went unanswered stays unverified.
        lpm.ingest(PageCapture::unanswered());
        let d = landed(lpm.ingest(PageCapture::unanswered()), true);
        assert!(!d.navigated && d.unanswered);
    }

    #[test]
    fn navigation_identity_preserves_origin_hash_and_path() {
        assert_eq!(
            normalize_url("https://EXAMPLE.com:443"),
            "https://example.com/"
        );
        assert_eq!(normalize_url("example.com"), "https://example.com/");
        assert_eq!(
            normalize_url("http://example.com:80"),
            "http://example.com/"
        );
        for (a, b) in [
            ("http://example.com/", "https://example.com/"),
            ("https://example.com/#old", "https://example.com/#new"),
            ("https://example.com/x", "https://example.com/x/"),
            ("https://example.com/?q=1", "https://example.com/?q=2"),
            ("http://example.com:8080/", "http://example.com:8081/"),
        ] {
            assert_ne!(
                normalize_url(a),
                normalize_url(b),
                "{a} and {b} must navigate"
            );
        }
    }

    #[test]
    fn bare_local_urls_keep_http_with_queries_and_fragments() {
        use super::with_scheme;
        for u in [
            "localhost?x=1",
            "localhost#view",
            "127.0.0.1?x=1",
            "localhost:80/x",
            "example.com:80/x",
        ] {
            assert_eq!(with_scheme(u), format!("http://{u}"));
        }
        assert_eq!(with_scheme("DATA:text/html,hello"), "DATA:text/html,hello");
        assert_eq!(with_scheme("ABOUT:blank"), "ABOUT:blank");
        assert_eq!(with_scheme("example.com?x=1"), "https://example.com?x=1");
    }

    /// A `://` inside the query or path (a return-URL parameter) is not the
    /// URL's own scheme: the bare host must still get one, or Chrome rejects
    /// the target as "Cannot navigate to invalid URL".
    #[test]
    fn bare_urls_carrying_a_url_in_the_query_still_get_a_scheme() {
        use super::with_scheme;
        assert_eq!(
            with_scheme("127.0.0.1:8080/?next=http://x"),
            "http://127.0.0.1:8080/?next=http://x"
        );
        assert_eq!(
            with_scheme("example.com/login?return=https://example.com/a"),
            "https://example.com/login?return=https://example.com/a"
        );
        assert_eq!(
            with_scheme("localhost/cb#u=ws://h"),
            "http://localhost/cb#u=ws://h"
        );
        // A real leading scheme is still left alone.
        assert_eq!(
            with_scheme("https://e.com/?next=http://x"),
            "https://e.com/?next=http://x"
        );
        // The domain-profile lookup reads the same input: it must name the
        // host being opened, not the host inside the query.
        assert_eq!(
            extract_domain("127.0.0.1:8080/?next=http://x.example"),
            "127.0.0.1"
        );
    }

    /// R1: the scheme gate. The rejected set is exactly the class that used
    /// to burn 10s and then report "already here" (ftp/javascript/mailto);
    /// host:port forms and the document-capable schemes must pass through.
    #[test]
    fn nav_scheme_gate_rejects_non_page_schemes() {
        for bad in [
            "ftp://example.com/",
            "javascript:1",
            "javascript:alert(1)",
            "mailto:user@example.com",
            "tel:5551234",
            "chrome://settings",
            "view-source:https://example.com",
            "ws://example.com/socket",
        ] {
            let err = validate_nav_url(bad).unwrap_err().to_string();
            assert!(err.contains("unsupported URL scheme"), "{bad}: {err}");
        }
        assert!(validate_nav_url("").is_err());
        assert!(validate_nav_url("   ").is_err());
        // Accepted: page-capable schemes, bare hosts, host:port forms.
        for ok in [
            "https://example.com",
            "http://127.0.0.1:8080/fixture.html",
            "example.com/path?q=1",
            "localhost:3000",
            "127.0.0.1:9222",
            "about:blank",
            "data:text/html,<b>hi</b>",
            "file:///tmp/page.html",
        ] {
            assert!(validate_nav_url(ok).is_ok(), "{ok} must pass the gate");
        }
    }

    /// The scheme grammar splitter: only a real `scheme:` prefix counts;
    /// `host:port`, paths with colons and fragments must not be misread.
    #[test]
    fn scheme_prefix_reads_only_a_real_scheme() {
        use super::scheme_prefix;
        assert_eq!(scheme_prefix("https://x"), Some("https"));
        assert_eq!(scheme_prefix("javascript:1"), Some("javascript"));
        assert_eq!(scheme_prefix("localhost:3000/x"), Some("localhost"));
        assert_eq!(scheme_prefix("example.com:8080"), Some("example.com"));
        assert_eq!(scheme_prefix("/path:with:colons"), None);
        assert_eq!(scheme_prefix("example.com/path"), None);
        assert_eq!(scheme_prefix("sub/path:1"), None);
        assert_eq!(scheme_prefix(""), None);
    }
}
