//! Page construction: attach to a CDP target and wire the background tasks.
//!
//! `Page::attach` sets up the session, the dialog / network / download / XHR
//! interceptors, stealth apply, the idle hum and the worker patch path —
//! everything a live Page owns for its lifetime.

use super::logs::{is_media_url, xhr_key};
use super::*;

impl Page {
    /// Attach to an existing page target over `cdp`, enable the core domains,
    /// and run an initial capture to seed the model.
    /// `browser_client` is the browser-level connection in pipe mode (S1) —
    /// used for tab listing since pipe mode has no HTTP debug endpoint.
    pub async fn attach(
        cdp: CdpSession,
        base: &str,
        browser_client: Option<CdpClient>,
    ) -> Result<Self> {
        #[allow(unused_assignments)]
        let mut worker_task: Option<tokio::task::JoinHandle<()>> = None;
        cdp.enable("Page").await?;
        // DO NOT enable Runtime — DataDome's detection leverages the fact that
        // `Runtime.enable` changes console buffering behavior, making
        // `console.log(new Error())` trigger serialization (and thus getter
        // calls on Error.stack) only when CDP is connected. We can still use
        // `Runtime.evaluate` without enabling the domain — evaluate is a
        // standalone command, enable only turns on event notifications.
        cdp.enable("Network").await?;
        cdp.enable("DOM").await?;
        // UA override: only needed in headless mode where the UA contains
        // "HeadlessChrome". In headful mode (Xvfb or native), the real UA
        // is already correct. Read the real UA + platform, and if headless,
        // replace just "HeadlessChrome" with "Chrome" — preserving the real
        // Chrome version and OS. This is a CDP-level override, not JS.
        let ua_info = cdp.send("Runtime.evaluate", Some(serde_json::json!({
            "expression": "JSON.stringify({ua:navigator.userAgent,plt:navigator.platform,wd:navigator.webdriver})",
            "returnByValue": true,
        }))).await.ok()
            .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from))
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

        // Real-browser lane honesty probe (measured, Chrome 151.0.7922.108):
        // a browser armed with an EPHEMERAL `--remote-debugging-port=0` (or
        // via Chrome's chrome://inspect approval flow) reports
        // `navigator.webdriver === true` natively — Chromium's own behavior,
        // not a bladebro patch (fixed-port arms report false). The lane's
        // contract forbids masking it, so surface it once instead of letting
        // the agent discover it from a page.
        if crate::realbrowser::real_lane()
            && ua_info
                .as_ref()
                .and_then(|v| v.get("wd").and_then(|w| w.as_bool()))
                == Some(true)
        {
            eprintln!(
                "{} note: this browser reports navigator.webdriver=true — Chrome does \
                 that itself for an ephemeral debug port (`--remote-debugging-port=0`) or the \
                 chrome://inspect approval flow; the lane never masks it. For stealth-critical \
                 work, arm with a FIXED --remote-debugging-port, or use clone/profile mode.",
                crate::ui::dim("[realbrowser]")
            );
        }

        let need_override = ua_info
            .as_ref()
            .and_then(|v| v.get("ua").and_then(|u| u.as_str()))
            .map(|ua| ua.contains("HeadlessChrome"))
            .unwrap_or(true);

        // Real-browser lane: no UA rewriting — if the lane runs headless it
        // honestly reports HeadlessChrome. Masks are exactly what this lane
        // deletes.
        if need_override && !crate::realbrowser::real_lane() {
            let real_ua = ua_info.as_ref()
                .and_then(|v| v.get("ua").and_then(|u| u.as_str()))
                .unwrap_or({
                    #[cfg(target_arch = "aarch64")]
                    { "Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36" }
                    #[cfg(not(target_arch = "aarch64"))]
                    { "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36" }
                });
            let fixed_ua = real_ua.replace("HeadlessChrome", "Chrome");
            let real_platform = ua_info
                .as_ref()
                .and_then(|v| v.get("plt").and_then(|p| p.as_str()))
                .unwrap_or({
                    #[cfg(target_arch = "aarch64")]
                    {
                        "Linux aarch64"
                    }
                    #[cfg(not(target_arch = "aarch64"))]
                    {
                        "Linux x86_64"
                    }
                });

            // Override ONLY userAgent + platform. The previous version also
            // sent a hand-built userAgentMetadata with a hardcoded GREASE
            // brand ("Not:A-Brand"/99) that matched no real Chrome build —
            // the real Sec-CH-UA headers would never equal it. Chrome keeps
            // generating coherent metadata (brands, GREASE, full versions)
            // from its true version, which matches the fixed UA string
            // (only "HeadlessChrome" → "Chrome" differs).
            if let Err(e) = cdp
                .send(
                    "Network.setUserAgentOverride",
                    Some(serde_json::json!({
                        "userAgent": fixed_ua,
                        "platform": real_platform,
                    })),
                )
                .await
            {
                crate::browser::set_ua_override_failed();
                eprintln!("[bladebro] WARNING: UA override failed: {e}");
            }
        }
        // S6: geo-consistent identity — timezone and locale must match the
        // proxy's geographic location. Without a proxy, the system timezone
        // is already correct. BLADE_TZ and BLADE_LOCALE override explicitly.
        // Real-browser lane: skipped — both are page-visible masks, and this
        // lane's contract is that nothing page-visible is manufactured.
        if !crate::realbrowser::real_lane() {
            if let Ok(tz) = std::env::var("BLADE_TZ") {
                if !tz.is_empty() {
                    if let Err(e) = cdp
                        .send(
                            "Emulation.setTimezoneOverride",
                            Some(serde_json::json!({ "timezoneId": tz })),
                        )
                        .await
                    {
                        eprintln!("[bladebro] WARNING: timezone override failed: {e}");
                    } else {
                        eprintln!("[bladebro] timezone override: {tz}");
                    }
                }
            } else if std::env::var("BLADE_PROXY").is_ok() {
                eprintln!("[bladebro] WARNING: BLADE_PROXY set but BLADE_TZ not set — timezone/IP mismatch will be detected");
            }
            if let Ok(locale) = std::env::var("BLADE_LOCALE") {
                if !locale.is_empty() {
                    let base = locale.split('-').next().unwrap_or(&locale).to_string();
                    let _ = cdp
                        .send(
                            "Emulation.setLocaleOverride",
                            Some(serde_json::json!({ "locale": locale })),
                        )
                        .await;
                    let _ = cdp
                        .send(
                            "Network.setExtraHTTPHeaders",
                            Some(serde_json::json!({
                                "headers": { "Accept-Language": format!("{locale},{base};q=0.9") }
                            })),
                        )
                        .await;
                    eprintln!("[bladebro] locale override: {locale}");
                }
            }
        }
        // Inject the stealth script before any page loads. This runs at
        // document_start on every new document, removing CDP artifacts.
        let stealth_script_id = match crate::stealth::apply_stealth(&cdp, None).await {
            Ok(id) => Some(id),
            Err(e) => {
                crate::browser::set_stealth_inject_failed();
                eprintln!("[bladebro] WARNING: stealth injection failed: {e}");
                None
            }
        };
        let active_locale = std::env::var("BLADE_LOCALE").ok().filter(|s| !s.is_empty());
        wait_for_load(&cdp, Duration::from_secs(10)).await?;

        // D22: Worker GL spoof — inject the GL spoof into Worker contexts too.
        // The main page GL spoof (via Page.addScriptToEvaluateOnNewDocument)
        // does NOT reach Worker contexts. Without this, Pixelscan/Creepjs
        // compare page GL (spoofed Intel) vs worker GL (real SwiftShader) and
        // flag "masking detected". We use CDP Target.setAutoAttach to inject
        // invisibly — no JS-level Worker constructor patching (which broke
        // sites via blob URLs in the previous attempt).
        //
        // v3.9 CRITICAL fixes here:
        // - EVERY auto-attached target is resumed (Runtime.runIfWaitingForDebugger).
        //   The old handler only resumed workers — cross-origin iframe (OOPIF)
        //   targets were attached with waitForDebuggerOnStart and then paused
        //   FOREVER: every embedded reCAPTCHA/Stripe/YouTube iframe froze.
        // - OOPIF child sessions get the FULL stealth script (they are
        //   separate targets; Page.addScriptToEvaluateOnNewDocument on the
        //   main session never reaches them).
        // - The task handle is stored on Page and aborted on Drop — it used
        //   to leak per switch_tab, with stale handlers racing the new one.
        // - broadcast Lagged is RECOVERABLE (continue), not fatal — a burst
        //   used to kill the handler, freezing every later worker.
        let worker_gl = crate::stealth::worker_gl_spoof(active_locale.as_deref());
        if worker_gl.is_some() || crate::stealth::has_full_script() {
            let _ = cdp
                .send(
                    "Target.setAutoAttach",
                    Some(serde_json::json!({
                        "autoAttach": true,
                        "flatten": true,
                        "waitForDebuggerOnStart": true
                    })),
                )
                .await;
            let client = cdp.client().clone();
            let worker_script = worker_gl.clone();
            let full_script = crate::stealth::full_script();
            let dbg = std::env::var("BLADE_DBG_WORKERS").is_ok();
            worker_task = Some(tokio::spawn(async move {
                let sub = client.subscribe();
                let mut rx = sub;
                loop {
                    match rx.recv().await {
                        Ok(event) if event.method == "Target.attachedToTarget" => {
                            let target_info = event.params.get("targetInfo");
                            let target_type = target_info
                                .and_then(|t| t.get("type"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            let session_id = event
                                .params
                                .get("sessionId")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            if session_id.is_empty() {
                                continue;
                            }
                            if dbg {
                                eprintln!("[workers] attach type={target_type} sid={session_id}");
                            }
                            // SECURITY: handle each attach in its OWN task. A
                            // sequential handler parked in a per-attach send
                            // (e.g. a busy-looping service worker eating the
                            // 3s eval timeout) froze this loop's recv(),
                            // pinning the broadcast ring's retention window at
                            // capacity (16384 × per-event bytes of
                            // page-authored events) and dropping attach/
                            // dialog events — the exact failure the
                            // EVENT_BUS_CAPACITY comment warns about. The
                            // loop itself now only ever awaits recv();
                            // per-target ordering (resume → eval → resume) is
                            // preserved inside the task, and tasks are
                            // bounded by the same 3–5s/30s command timeouts.
                            let client = client.clone();
                            let worker_script = worker_script.clone();
                            let full_script = full_script.clone();
                            tokio::spawn(async move {
                                let worker_session = CdpSession::child(client, session_id);
                                // Runtime.evaluate against a PAUSED service
                                // worker deadlocks — its execution context
                                // only exists once the script runs — and the
                                // 30s command timeout then froze the old
                                // sequential handler (every later target
                                // stayed paused; live effect: hung SW
                                // registration and CreepJS's worker card
                                // reading `blocked`). Service workers are
                                // resumed FIRST, then patched best-effort.
                                let is_sw = target_type == "service_worker";
                                if is_sw {
                                    let res = worker_session
                                        .send("Runtime.runIfWaitingForDebugger", None)
                                        .await;
                                    if dbg {
                                        eprintln!(
                                            "[workers] resume(sw-first) {target_type}: {}",
                                            if res.is_ok() {
                                                "ok".to_string()
                                            } else {
                                                format!("ERR {:?}", res.err())
                                            }
                                        );
                                    }
                                }
                                match target_type.as_str() {
                                    "worker" | "shared_worker" => {
                                        if let Some(ref script) = worker_script {
                                            // Bounded: a pathological target
                                            // must not stall the pipeline.
                                            let res = worker_session
                                                .send_with_timeout(
                                                    "Runtime.evaluate",
                                                    Some(serde_json::json!({
                                                        "expression": script,
                                                        "returnByValue": true,
                                                    })),
                                                    std::time::Duration::from_secs(5),
                                                )
                                                .await;
                                            if dbg {
                                                eprintln!(
                                                    "[workers] eval {target_type}: {}",
                                                    if res.is_ok() {
                                                        "ok".to_string()
                                                    } else {
                                                        format!("ERR {:?}", res.err())
                                                    }
                                                );
                                            }
                                        }
                                    }
                                    "service_worker" => {
                                        // Already resumed above. The SW realm
                                        // has no WebGL; this only matters for
                                        // the locale patch — best-effort,
                                        // bounded.
                                        if let Some(ref script) = worker_script {
                                            let res = worker_session
                                                .send_with_timeout(
                                                    "Runtime.evaluate",
                                                    Some(serde_json::json!({
                                                        "expression": script,
                                                        "returnByValue": true,
                                                    })),
                                                    std::time::Duration::from_secs(3),
                                                )
                                                .await;
                                            if dbg {
                                                eprintln!(
                                                    "[workers] eval {target_type}: {}",
                                                    if res.is_ok() {
                                                        "ok".to_string()
                                                    } else {
                                                        format!("ERR {:?}", res.err())
                                                    }
                                                );
                                            }
                                        }
                                    }
                                    "iframe" | "oopif" => {
                                        // Full stealth into out-of-process
                                        // frames: document_start semantics via
                                        // evaluate before resume, so the
                                        // frame's scripts run against the
                                        // patched environment.
                                        if let Some(ref script) = full_script {
                                            let _ = worker_session
                                                .send_with_timeout(
                                                    "Runtime.evaluate",
                                                    Some(serde_json::json!({
                                                        "expression": script,
                                                        "returnByValue": true,
                                                    })),
                                                    std::time::Duration::from_secs(5),
                                                )
                                                .await;
                                        }
                                    }
                                    _ => {}
                                }
                                // ALWAYS resume — an unresumed target stays
                                // frozen. (Service workers were already
                                // resumed above.)
                                if !is_sw {
                                    let res = worker_session
                                        .send("Runtime.runIfWaitingForDebugger", None)
                                        .await;
                                    if dbg {
                                        eprintln!(
                                            "[workers] resume {target_type}: {}",
                                            if res.is_ok() {
                                                "ok".to_string()
                                            } else {
                                                format!("ERR {:?}", res.err())
                                            }
                                        );
                                    }
                                }
                            });
                        }
                        Ok(_) => {}
                        // Lagged: we skipped events but the connection is
                        // alive. Continue — never leave future targets paused.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            if dbg {
                                eprintln!("[workers] LAGGED {n} events dropped");
                            }
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            }));
        }

        // M17: Set download behavior so downloads don't hang the browser.
        // SECURITY: ~/.blade/downloads (0700) instead of a shared predictable
        // /tmp dir — downloads can contain private documents, and a shared
        // /tmp/bladebro-downloads leaked them across local users (and was a
        // symlink pre-placement target). The absolute path is returned to
        // the agent in the download result, so nothing depends on /tmp.
        let download_dir = crate::platform::blade_dir().join("downloads");
        let _ = crate::platform::secure_create_dir_all(&download_dir);
        let _ = cdp
            .send(
                "Page.setDownloadBehavior",
                Some(serde_json::json!({
                    "behavior": "allow",
                    "downloadPath": download_dir.display().to_string(),
                })),
            )
            .await;

        // Spawn the dialog-handler background task. It subscribes to
        // `Page.javascriptDialogOpening` events and auto-dismisses them so
        // the page never deadlocks on alert()/confirm()/prompt(). Dismissed
        // dialog info is queued for the MCP server to surface to the agent.
        //
        // Auto-dismiss strategy: alert=accept (only option),
        // confirm/prompt/beforeunload=cancel (safer — don't accidentally
        // confirm destructive actions).
        let dialogs: Arc<Mutex<Vec<DialogInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let cdp_for_dialogs = cdp.clone();
        let dq = dialogs.clone();
        let dialog_task = tokio::spawn(async move {
            let mut rx = cdp_for_dialogs.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Page.javascriptDialogOpening" => {
                        let kind = ev
                            .params
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("alert")
                            .to_string();
                        let message = ev
                            .params
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let default_prompt = ev
                            .params
                            .get("defaultPrompt")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        // alert=accept; confirm/prompt=cancel (safe default).
                        // beforeunload=accept: the agent ISSUED the navigation —
                        // cancelling it would silently block every nav away
                        // from a dirty form.
                        let accepted = kind == "alert" || kind == "beforeunload";
                        let _ = cdp_for_dialogs
                            .send(
                                "Page.handleJavaScriptDialog",
                                Some(serde_json::json!({ "accept": accepted })),
                            )
                            .await;
                        if let Ok(mut q) = dq.lock() {
                            q.push(DialogInfo {
                                kind,
                                message,
                                default_prompt,
                                accepted,
                            });
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Spawn the network-tracker task: counts in-flight requests so settle
        // can wait for data to arrive, not just DOM stability.
        let ambient: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        // V19: download-watch task. Page.setDownloadBehavior (M17, above) already
        // routes downloads to download_dir and enables Page.downloadWillBegin /
        // Page.downloadProgress events. Track each download's state and surface
        // a `download started` ambient note so a click that triggers a download
        // is never silent.
        let downloads: Arc<Mutex<Vec<DownloadInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let cdp_for_dl = cdp.clone();
        let dlq = downloads.clone();
        let dl_ambient = ambient.clone();
        let dl_dir = download_dir.clone();
        let download_task = tokio::spawn(async move {
            let mut rx = cdp_for_dl.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Page.downloadWillBegin" => {
                        let guid = ev
                            .params
                            .get("guid")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let url = ev
                            .params
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let filename = ev
                            .params
                            .get("suggestedFilename")
                            .and_then(|v| v.as_str())
                            .unwrap_or("download")
                            .to_string();
                        let path = dl_dir.join(&filename).display().to_string();
                        if let Ok(mut q) = dlq.lock() {
                            q.push(DownloadInfo {
                                guid,
                                url,
                                filename: filename.clone(),
                                state: "inProgress".into(),
                                received_bytes: 0,
                                total_bytes: 0,
                                path,
                            });
                            if q.len() > 50 {
                                let n = q.len() - 50;
                                q.drain(0..n);
                            }
                        }
                        if let Ok(mut a) = dl_ambient.lock() {
                            a.push(format!("download started: {filename}"));
                        }
                    }
                    Ok(ev) if ev.method == "Page.downloadProgress" => {
                        let guid = ev
                            .params
                            .get("guid")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let state = ev
                            .params
                            .get("state")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let received = ev
                            .params
                            .get("receivedBytes")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let total = ev
                            .params
                            .get("totalBytes")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        if let Ok(mut q) = dlq.lock() {
                            if let Some(d) = q.iter_mut().find(|d| d.guid == guid) {
                                d.state = state;
                                d.received_bytes = received;
                                d.total_bytes = total;
                            }
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let in_flight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        // Client-side route-transition epoch: `Page.navigatedWithinDocument`
        // events bump it (a router moved the url before the content painted).
        let route = Arc::new(RouteEpoch::new());
        let net_log: Arc<Mutex<std::collections::VecDeque<NetEntry>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let xhr_log: Arc<Mutex<std::collections::VecDeque<XhrEntry>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let cdp_for_net = cdp.clone();
        let net_counter = in_flight.clone();
        let net_log_t = net_log.clone();
        let xhr_log_t = xhr_log.clone();
        let route_t = route.clone();
        let network_task = tokio::spawn(async move {
            use std::collections::HashMap;
            let mut rx = cdp_for_net.subscribe();
            // Track request IDs with timestamps: requestWillBeSent fires
            // once per REDIRECT HOP for the same requestId while
            // loadingFinished fires once — a counter drifts +1 per hop and
            // eventually every settle waits the full timeout.
            // Timestamps let us sweep stale entries (data URLs, long-poll,
            // server-sent events that never fire loadingFinished).
            let mut open: std::collections::HashMap<String, std::time::Instant> =
                std::collections::HashMap::new();
            // Pending request metadata for the V8 net log.
            let mut pending: HashMap<String, (String, String, i64)> = HashMap::new();
            let mut last_sweep = std::time::Instant::now();
            let mut sweep_tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                // Sweep on a timer too: a stuck request must expire even when
                // no further network events arrive to wake the loop (a pinned
                // counter made every settle pay its drain plateau).
                let msg = tokio::select! {
                    m = rx.recv() => Some(m),
                    _ = sweep_tick.tick() => None,
                };
                match msg {
                    Some(Ok(ev)) if ev.method == "Network.requestWillBeSent" => {
                        // WebSocket/EventSource "requests" never fire
                        // loadingFinished — counting them pinned the in-flight
                        // counter indefinitely on any page holding a socket.
                        let ty = ev.params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let id = ev
                            .params
                            .get("requestId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !id.is_empty() && ty != "WebSocket" && ty != "EventSource" {
                            open.insert(id.to_string(), std::time::Instant::now());
                            let req = ev.params.get("request");
                            let method = req
                                .and_then(|r| r.get("method"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("GET")
                                .to_string();
                            let url = req
                                .and_then(|r| r.get("url"))
                                .and_then(|u| u.as_str())
                                .unwrap_or("")
                                .to_string();
                            if (ty == "XHR" || ty == "Fetch")
                                && !url.is_empty()
                                && !is_media_url(&url)
                            {
                                if url.contains("/i/api/graphql/") {
                                    tracing::debug!("xhr gql: {} {}", method, url);
                                }
                                let mut hdrs: Vec<(String, String)> = Vec::new();
                                if let Some(h) = req
                                    .and_then(|r| r.get("headers"))
                                    .and_then(|h| h.as_object())
                                {
                                    for (k, v) in h {
                                        let kl = k.to_ascii_lowercase();
                                        // Keep every header except transport noise the
                                        // browser regenerates itself (cookie, UA,
                                        // encodings, sizes, sec-*). X validates per-request
                                        // client headers on some ops (search) but not
                                        // others — a partial copy silently 404s replays.
                                        if kl != "cookie"
                                            && kl != "host"
                                            && kl != "content-length"
                                            && kl != "accept-encoding"
                                            && kl != "connection"
                                            && kl != "user-agent"
                                            && !kl.starts_with("sec-")
                                        {
                                            if let Some(s) = v.as_str() {
                                                hdrs.push((k.clone(), s.to_string()));
                                            }
                                        }
                                    }
                                }
                                if let Ok(mut log) = xhr_log_t.lock() {
                                    let key = xhr_key(&url);
                                    if let Some(pos) =
                                        log.iter().position(|e| xhr_key(&e.url) == key)
                                    {
                                        log.remove(pos);
                                    }
                                    log.push_back(XhrEntry {
                                        id: id.to_string(),
                                        method: method.clone(),
                                        url: url.clone(),
                                        status: 0,
                                        done: false,
                                        error: None,
                                        headers: hdrs,
                                    });
                                    if log.len() > 128 {
                                        log.pop_front();
                                    }
                                }
                            }
                            pending.insert(id.to_string(), (method, url, 0));
                        }
                    }
                    Some(Ok(ev)) if ev.method == "Network.responseReceived" => {
                        let id = ev
                            .params
                            .get("requestId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let status = ev
                            .params
                            .get("response")
                            .and_then(|r| r.get("status"))
                            .and_then(|s| s.as_i64())
                            .unwrap_or(0);
                        if let Some(entry) = pending.get_mut(id) {
                            entry.2 = status;
                        }
                        if status > 0 {
                            if let Ok(mut log) = xhr_log_t.lock() {
                                if let Some(e) = log.iter_mut().rev().find(|e| e.id == id) {
                                    e.status = status;
                                }
                            }
                        }
                    }
                    Some(Ok(ev))
                        if ev.method == "Network.loadingFinished"
                            || ev.method == "Network.loadingFailed" =>
                    {
                        let id = ev
                            .params
                            .get("requestId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !id.is_empty() {
                            open.remove(id);
                            if let Ok(mut log) = xhr_log_t.lock() {
                                if let Some(e) = log.iter_mut().rev().find(|e| e.id == id) {
                                    e.done = true;
                                    if ev.method == "Network.loadingFailed" {
                                        e.error = Some(
                                            ev.params
                                                .get("errorText")
                                                .and_then(|x| x.as_str())
                                                .unwrap_or("failed")
                                                .to_string(),
                                        );
                                    }
                                    if e.url.contains("/i/api/graphql/") {
                                        let st =
                                            e.error.clone().unwrap_or_else(|| e.status.to_string());
                                        tracing::debug!(
                                            "xhr gql done: {} {} -> {}",
                                            e.method,
                                            e.url
                                                .split('?')
                                                .next()
                                                .unwrap_or("")
                                                .replace("https://x.com", ""),
                                            st
                                        );
                                    }
                                }
                            }
                            if let Some((method, url, status)) = pending.remove(id) {
                                let error = if ev.method == "Network.loadingFailed" {
                                    Some(
                                        ev.params
                                            .get("errorText")
                                            .and_then(|e| e.as_str())
                                            .unwrap_or("failed")
                                            .to_string(),
                                    )
                                } else {
                                    None
                                };
                                if let Ok(mut log) = net_log_t.lock() {
                                    log.push_back(NetEntry {
                                        method,
                                        url,
                                        status,
                                        error,
                                    });
                                    if log.len() > 50 {
                                        log.pop_front();
                                    }
                                }
                            }
                        }
                    }
                    // A client-side route change: the url has moved but the
                    // previous route's DOM is still mounted. Arm the guard so
                    // the next read waits for the swap instead of answering
                    // with the old route's content.
                    Some(Ok(ev)) if ev.method == "Page.navigatedWithinDocument" => {
                        route_t.note_nav();
                    }
                    Some(Ok(_)) => {}
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        // Events were dropped — the set may now hold stale IDs.
                        // Clear rather than risk a permanently blocked settle.
                        open.clear();
                        pending.clear();
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                    None => {}
                }
                // Periodic sweep: remove entries older than 8 seconds.
                // Data URLs, long-poll connections, and server-sent events
                // may never fire loadingFinished. 8s is the in-flight horizon:
                // a legit resource still loading after that is a download, not
                // a page-load blocker.
                if last_sweep.elapsed().as_secs() >= 1 {
                    let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(8);
                    open.retain(|_, ts| *ts > cutoff);
                    last_sweep = std::time::Instant::now();
                }
                net_counter.store(open.len(), std::sync::atomic::Ordering::Relaxed);
            }
        });

        let lpm = LivePageModel::new();
        // M4+M6: Check for consent banners and block pages after initial load.
        let consent = dismiss_consent(&cdp).await.unwrap_or(None);
        let blocked = detect_block(&cdp).await.unwrap_or(None);
        if let Some(ref fw) = consent {
            if let Ok(mut a) = ambient.lock() {
                a.push(format!(
                    "consent: {} ({})",
                    if std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".into())
                        != "accept"
                    {
                        "rejected"
                    } else {
                        "accepted"
                    },
                    fw
                ));
            }
        }
        if let Some(ref bt) = blocked {
            if let Ok(mut a) = ambient.lock() {
                a.push(format!("blocked: {}", bt));
                for step in crate::page::perception::remediation_ladder(bt) {
                    a.push(format!("  remediation: {}", step));
                }
            }
        }

        // S4+S5: shared action state for pacing + idle hum.
        let last_action_epoch = Arc::new(AtomicU64::new(0));
        let is_busy = Arc::new(AtomicBool::new(false));
        let last_mouse = Arc::new(std::sync::Mutex::new(None));
        let hum_task = crate::stealth::spawn_hum(
            cdp.clone(),
            last_action_epoch.clone(),
            is_busy.clone(),
            last_mouse.clone(),
        );

        // Request-interception task: answers Fetch.requestPaused events
        // with block/continue verdicts. Only receives events while
        // Fetch is enabled (blocking active); idle otherwise.
        let intercept = intercept::InterceptState::default();
        let intercept_task = tokio::spawn(intercept::run_interception(
            cdp.clone(),
            intercept.clone(),
            cdp.subscribe(),
        ));

        // Construct the Page BEFORE the last fallible step (capture):
        // on error the returned Err drops `page`, whose Drop aborts all
        // background tasks. The old `capture(...)?` leaked all five
        // spawned tasks on failure.
        let mut page = Self {
            cdp,
            browser_client,
            lpm,
            dialogs,
            dialog_task: Some(dialog_task),
            in_flight,
            route,
            net_log,
            xhr_log,
            network_task: Some(network_task),
            ambient,
            base: base.to_string(),
            last_action_epoch,
            is_busy,
            hum_task: Some(hum_task),
            worker_task,
            stealth_script_id,
            active_locale,
            intercept,
            intercept_task: Some(intercept_task),
            downloads,
            download_task: Some(download_task),
            knowledge: None,
            last_mouse,
            isolated_ctx: Arc::new(std::sync::Mutex::new(None)),
            act_count: std::sync::atomic::AtomicU32::new(0),
            compress_enabled: std::sync::atomic::AtomicBool::new(
                std::env::var("BLADE_NO_COMPRESS").as_deref() != Ok("1"),
            ),
        };

        let cap = capture(&page.cdp).await?;
        page.lpm.ingest(cap);
        Ok(page)
    }
}
