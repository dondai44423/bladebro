//! Launch paths — the agent-lane launch ladder (WS), the real-lane launch,
//! the lane dispatcher and the font audit.

use super::discover::{endpoint_owned_by_pid, find_chrome, free_port};
#[cfg(target_os = "linux")]
use super::display::{apply_xvfb_env, VirtualDisplay};
use super::flags::{
    classify_gl, gl_stages, launch_args, launch_args_real, stage_label, LaunchCfg, RealLaunchCfg,
};
use super::probe::probe_gl_ws;
use super::*;

impl Browser {
    /// Find Chrome, launch it with stealth flags on `port` (0 = auto-pick a
    /// free port), and wait for the CDP debug endpoint to respond.
    ///
    /// Linux: Xvfb headful if available, headless fallback.
    /// macOS/Windows: headful natively (native window server).
    ///
    /// SECURITY: the Chrome sandbox is kept ON. If Chrome dies at startup
    /// (root, unprivileged-user-namespace containers), we retry once with
    /// `--no-sandbox` — availability preserved, sandbox used whenever the
    /// environment allows it.
    pub async fn launch(port: u16) -> Result<Self> {
        #[cfg(windows)]
        platform::guard_browser_process_tree()?;
        let auto = port == 0;
        let mut last_err = None;
        // Attempt sequence: sandboxed first; then --no-sandbox (sandbox
        // unavailable: root, restricted container); then one more fresh-port
        // retry with --no-sandbox (port-steal race). Only fast startup
        // exits are retried — endpoint timeouts abort immediately (same
        // policy as before; avoids tripling a 20s wait).
        let attempts: [(u8, bool); 3] = [(0, false), (1, true), (2, true)];
        for (attempt, no_sandbox) in attempts {
            let p = if auto { free_port() } else { Some(port) };
            let Some(p) = p else {
                last_err = Some(BladeError::Other(
                    "cannot allocate a loopback debug port (ephemeral range exhausted)".into(),
                ));
                break;
            };
            match Self::launch_inner(p, no_sandbox).await {
                Ok(b) => return Ok(b),
                Err(e) => {
                    let startup_exit = e.to_string().contains("exited during startup");
                    last_err = Some(e);
                    if !auto || !startup_exit {
                        break;
                    }
                    if attempt == 0 {
                        eprintln!(
                            "[bladebro] sandboxed Chrome died at startup (root or restricted container?) — retrying with --no-sandbox"
                        );
                    } else if attempt == 1 {
                        eprintln!("[bladebro] Chrome died at startup (port race?), retrying on a fresh port");
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| BladeError::Other("launch failed".into())))
    }

    /// Launch with the GL ladder: walk `gl_stages()`, keep the first stage
    /// that yields a live WebGL context, and record the result for the
    /// stealth layer. A stage that comes up without GL is shut down and the
    /// next stage is tried (bounded — the ladder has two stages) so a
    /// GL-less browser is never what pages see.
    async fn launch_inner(port: u16, no_sandbox: bool) -> Result<Self> {
        let stages = gl_stages();
        let mut last: Option<Self> = None;
        for (i, stage) in stages.iter().enumerate() {
            if let Some(b) = last.take() {
                // No GL in the previous stage — tear it down before the
                // relaunch (fresh Xvfb + profile; the WS transport re-binds
                // the same port, so the old Chrome must be gone first).
                let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
            }
            let (browser, probe) = Self::launch_inner_stage(port, no_sandbox, *stage).await?;
            match probe {
                Some(renderer) => {
                    let state = classify_gl(&renderer);
                    eprintln!(
                        "[stealth] GL healthcheck: {renderer} ({}) via {}",
                        if matches!(state, GpuState::Hardware(_)) {
                            "hardware"
                        } else {
                            "software"
                        },
                        stage_label(*stage)
                    );
                    set_gpu_state(Some(state));
                    return Ok(browser);
                }
                None => {
                    if i + 1 < stages.len() {
                        let next = stages[i + 1];
                        if next == *stage {
                            eprintln!(
                                "[bladebro] GL healthcheck: no WebGL context via {} — retrying it once (fresh launch)",
                                stage_label(*stage)
                            );
                        } else {
                            eprintln!(
                                "[bladebro] GL healthcheck: no WebGL context via {} — escalating to {}",
                                stage_label(*stage),
                                stage_label(next)
                            );
                        }
                        last = Some(browser);
                    } else {
                        eprintln!(
                            "[bladebro] WARNING: no WebGL context after the full GL ladder — \
                             pages will see `getContext('webgl') === null` (stock Chrome on a \
                             GL-less host behaves identically; no GL mask is applied). Run `bladebro audit`."
                        );
                        set_gpu_state(Some(GpuState::Missing));
                        return Ok(browser);
                    }
                }
            }
        }
        Err(BladeError::Other("no GL stages configured".into()))
    }

    /// One launch attempt with a pinned GL stage (driven by `launch_inner`).
    async fn launch_inner_stage(
        port: u16,
        no_sandbox: bool,
        stage: GlStage,
    ) -> Result<(Self, Option<String>)> {
        let timing = std::env::var("NAV_TIMING").is_ok();
        let t0 = std::time::Instant::now();
        let chrome_path = find_chrome()?;
        let profile = crate::session_profile::SessionProfile::create()?;
        if timing {
            eprintln!("[launch-timing] profile: {:?}", t0.elapsed());
        }
        let user_data_dir = profile.dir().to_path_buf();
        font_audit();

        #[cfg(target_os = "linux")]
        let xvfb = VirtualDisplay::start().ok();
        #[cfg(target_os = "linux")]
        if xvfb.is_none() {
            // Stealth downgrade: headless=new carries real detection
            // surface. Never fail silently — the operator should know.
            eprintln!(
                "[bladebro] WARNING: Xvfb unavailable — falling back to headless mode (reduced stealth). Install xvfb for headful-on-virtual-display."
            );
        }
        #[cfg(target_os = "linux")]
        if timing {
            eprintln!("[launch-timing] display: {:?}", t0.elapsed());
        }
        #[cfg(target_os = "linux")]
        let headful = xvfb.is_some();

        #[cfg(not(target_os = "linux"))]
        let headful = true; // macOS/Windows have native window servers

        // After both bindings: the recording must compile on every target.
        set_launched_headless(!headful);
        set_launched_no_sandbox(no_sandbox);

        // M18: Proxy support via BLADE_PROXY env var. Validated here even
        // when nothing logs: a malformed value fails the launch with a
        // redacted reason instead of silently producing a broken-proxy
        // browser. The raw value may embed credentials — only the redacted
        // display form ever reaches stderr.
        let proxy = match crate::browser::proxy::env_blade_proxy() {
            Ok(p) => p,
            Err(reason) => {
                return Err(BladeError::Other(format!(
                    "invalid BLADE_PROXY value: {reason} (value redacted)"
                )));
            }
        };
        if let Some(spec) = &proxy {
            eprintln!("[bladebro] using proxy: {}", spec.display);
        }
        // Power-user escape hatch: append raw Chrome flags. Useful for
        // diagnosing GL/WebGL backend issues on odd displays and for
        // users who need a specific Chromium switch. Whitespace-split.
        let extra: Vec<String> = std::env::var("BLADE_CHROME_FLAGS")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let args = launch_args(&LaunchCfg {
            stage,
            headful,
            no_sandbox,
            transport: Transport::Ws,
            port,
            user_data_dir: &user_data_dir,
            proxy: proxy.as_ref().map(|s| s.server.as_str()),
            extra: &extra,
        });

        #[cfg(target_os = "linux")]
        let mode_str = if headful {
            "headful (Xvfb)"
        } else {
            "headless"
        };
        #[cfg(not(target_os = "linux"))]
        let mode_str = "headful";
        eprintln!("[bladebro] launching Chrome from {chrome_path} on port {port} ({mode_str})");

        let mut cmd = Command::new(&chrome_path);
        browser_temp_env(&mut cmd);
        // G11: bounded startup-stderr capture — a failed launch must be
        // diagnosable instead of guessed at (the runner investigation lost
        // Chrome's stderr to /dev/null).
        cmd.args(&args).stdout(Stdio::null());
        match chrome_stderr_sink() {
            Some(f) => {
                cmd.stderr(Stdio::from(f));
            }
            None => {
                cmd.stderr(Stdio::null());
            }
        }

        // Set DISPLAY env var for headful mode (Linux only).
        // CRITICAL on Wayland sessions: Chrome 110+ defaults to the
        // Wayland ozone platform when WAYLAND_DISPLAY is inherited
        // from the user's session — it opens on the USER'S real
        // screen, ignoring the Xvfb DISPLAY. Strip Wayland env and
        // force X11 so Chrome stays invisible on the virtual display.
        #[cfg(target_os = "linux")]
        if let Some(ref xvfb) = xvfb {
            apply_xvfb_env(&mut cmd, xvfb);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| BladeError::Other(format!("failed to launch Chrome: {e}")))?;

        let base = format!("127.0.0.1:{port}");
        if timing {
            eprintln!("[launch-timing] chrome-spawn: {:?}", t0.elapsed());
        }

        // Poll the debug endpoint until it responds or we time out.
        let deadline = Instant::now() + Duration::from_secs(20);
        let started = Instant::now();
        loop {
            match crate::cdp::version(&base).await {
                Ok(v) => {
                    // SECURITY: a parseable-JSON responder is not proof the
                    // endpoint belongs to the Chrome we spawned —
                    // free_port's bind-release window can be won by a local
                    // impostor. On Linux require our child to hold the
                    // listening socket; keep polling otherwise (the window
                    // resolves once Chrome finishes binding). Elsewhere the
                    // check is skipped.
                    if !endpoint_owned_by_pid(port, child.id()) {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            return Err(BladeError::Other(
                                "the debug endpoint on this port is not served by the browser process — refusing it (possible local port hijack)".into(),
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    eprintln!(
                        "[bladebro] Chrome ready: {} (protocol {})",
                        v.browser, v.protocol_version
                    );
                    if timing {
                        eprintln!("[launch-timing] chrome-ready: {:?}", t0.elapsed());
                    }
                    let probe = probe_gl_ws(&base).await;
                    if timing {
                        eprintln!("[launch-timing] healthcheck: {:?}", t0.elapsed());
                    }
                    return Ok((
                        Self {
                            child,
                            #[cfg(target_os = "linux")]
                            xvfb,
                            port,
                            profile,
                        },
                        probe,
                    ));
                }
                Err(_) => {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            return Err(BladeError::Other(format!(
                                "Chrome exited during startup: {status}{}",
                                chrome_stderr_tail()
                            )));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return Err(BladeError::Other(format!(
                                "failed to poll Chrome status: {e}"
                            )));
                        }
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(BladeError::Other(format!(
                            "Chrome debug endpoint not responding after 20s{}",
                            chrome_stderr_tail()
                        )));
                    }
                    // Adaptive poll: Chrome answers well under a second in the
                    // normal case, so a flat 300ms interval quantized the
                    // whole readiness wait up — go tight early, coarse late.
                    if started.elapsed() < Duration::from_secs(2) {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
            }
        }
    }

    /// Launch the real-browser lane: the user's own Chromium-family binary
    /// on a copied (clone) or live (profile) user-data-dir. No Xvfb, no GL
    /// ladder, no stealth flags — this lane's whole point is that the
    /// environment and the page-visible surface are the real thing.
    pub async fn launch_real(
        binary: &std::path::Path,
        profile: crate::session_profile::SessionProfile,
        visible: bool,
        profile_directory: Option<&str>,
    ) -> Result<Self> {
        #[cfg(windows)]
        platform::guard_browser_process_tree()?;
        let user_data_dir = profile.dir().to_path_buf();

        // Visible needs a display on Linux. No display → an honest
        // headless fallback with a loud note. Never a virtual display:
        // that would re-create the mock environment this lane deletes.
        // On non-Linux targets there is no display probing below, so these
        // two stay untouched — the `mut` is Linux-only.
        #[allow(unused_mut)]
        let mut headless = !visible;
        #[allow(unused_mut)]
        let mut ozone_x11 = false;
        #[cfg(target_os = "linux")]
        {
            let has_wayland = std::env::var_os("WAYLAND_DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            let has_x11 = std::env::var_os("DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !headless {
                if !has_wayland && !has_x11 {
                    eprintln!(
                        "[realbrowser] WARNING: no display in this environment — falling back to \
                         --headless=new (reduced environment realism). Run from a desktop session \
                         for the real lane's full power, or set `rb visible off` to make this \
                         explicit."
                    );
                    headless = true;
                } else if !has_wayland && has_x11 {
                    // X11 is the only reachable display — pin it (see
                    // `launch_args_real` for the failure this prevents).
                    ozone_x11 = true;
                }
            }
        }
        set_launched_headless(headless);
        set_launched_pipe(false);

        // Real-lane proxy: validated the same way as the managed lane (a
        // malformed value fails loudly, redacted); the real lane does not
        // log the endpoint, so nothing here can leak credentials.
        let proxy = match crate::browser::proxy::env_blade_proxy() {
            Ok(p) => p,
            Err(reason) => {
                return Err(BladeError::Other(format!(
                    "invalid BLADE_PROXY value: {reason} (value redacted)"
                )));
            }
        };
        let extra: Vec<String> = std::env::var("BLADE_CHROME_FLAGS")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();

        let mut last_err = None;
        // A quick clean exit means Chrome handed the command line to an
        // already-running instance on this profile (process singleton) and
        // quit — retrying cannot help, and the bare "exited" message would
        // hide the actual cause. An endpoint timeout is equally not a startup
        // death (the browser is RUNNING and not answering): only a genuine
        // startup exit gets the --no-sandbox retry — anything else would just
        // burn another 20s and mislabel the failure.
        for (attempt, no_sandbox) in [(0u8, false), (1u8, true)] {
            let mut startup_exit = false;
            let spawned_at = Instant::now();
            let Some(port) = free_port() else {
                last_err = Some(BladeError::Other(
                    "cannot allocate a loopback debug port (ephemeral range exhausted)".into(),
                ));
                break;
            };
            set_launched_no_sandbox(no_sandbox);
            let args = launch_args_real(&RealLaunchCfg {
                headless,
                no_sandbox,
                ozone_x11,
                port,
                user_data_dir: &user_data_dir,
                profile_directory,
                proxy: proxy.as_ref().map(|s| s.server.as_str()),
                extra: &extra,
            });
            eprintln!(
                "[realbrowser] launching {} ({}) on port {port}",
                binary.display(),
                if headless { "headless" } else { "visible" }
            );

            let mut cmd = Command::new(binary);
            browser_temp_env(&mut cmd);
            // BLADE_RB_DEBUG=1 surfaces the browser's own stderr — the only
            // way to see WHY a real-lane launch died (X11/Wayland/GL errors
            // are otherwise discarded into /dev/null).
            let child_stderr = if std::env::var("BLADE_RB_DEBUG")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                Stdio::inherit()
            } else {
                Stdio::null()
            };
            cmd.args(&args).stdout(Stdio::null()).stderr(child_stderr);
            #[cfg(target_os = "linux")]
            if ozone_x11 {
                // Belt and braces with --ozone-platform=x11: drop a possibly
                // stale session-type claim from the child env.
                cmd.env_remove("XDG_SESSION_TYPE");
            }
            let mut child = cmd.spawn().map_err(|e| {
                BladeError::Other(format!("failed to launch {}: {e}", binary.display()))
            })?;

            let base = format!("127.0.0.1:{port}");
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match crate::cdp::version(&base).await {
                    Ok(v) => {
                        // SECURITY: same ownership requirement as the agent
                        // lane — the endpoint must be held by the browser
                        // process we spawned.
                        if !endpoint_owned_by_pid(port, child.id()) {
                            if Instant::now() >= deadline {
                                let _ = child.kill();
                                last_err = Some(BladeError::Other(
                                    "the debug endpoint on this port is not served by the browser process — refusing it (possible local port hijack)".into(),
                                ));
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                        eprintln!(
                            "[realbrowser] browser ready: {} (protocol {})",
                            v.browser, v.protocol_version
                        );
                        return Ok(Self {
                            child,
                            #[cfg(target_os = "linux")]
                            xvfb: None,
                            port,
                            profile,
                        });
                    }
                    Err(_) => {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                if spawned_at.elapsed() < Duration::from_secs(5) && status.success()
                                {
                                    last_err = Some(BladeError::Other(
                                        "the browser exited immediately (status 0) — another \
                                         instance is almost certainly running on this profile \
                                         and Chrome handed the launch off to it. Close it and \
                                         retry, or use `rb mode clone` (works while it stays \
                                         open), or `rb mode attach`."
                                            .into(),
                                    ));
                                } else {
                                    startup_exit = true;
                                    last_err = Some(BladeError::Other(format!(
                                        "browser exited during startup: {status}"
                                    )));
                                }
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                return Err(BladeError::Other(format!(
                                    "failed to poll browser status: {e}"
                                )));
                            }
                        }
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            last_err = Some(BladeError::Other(
                                "browser debug endpoint not responding after 20s".into(),
                            ));
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
            }
            // Only a genuine startup death gets the --no-sandbox retry.
            if !startup_exit {
                break;
            }
            if attempt == 0 {
                eprintln!(
                    "[realbrowser] browser died at startup — retrying once with --no-sandbox"
                );
            }
        }
        let failure =
            last_err.unwrap_or_else(|| BladeError::Other("real-browser launch failed".into()));
        if headless {
            Err(failure)
        } else {
            // Visible startup deaths are almost always display problems —
            // point at the diagnostic switch instead of guessing.
            Err(BladeError::Other(format!(
                "{failure} — visible launches need a reachable display; re-run with \
                 BLADE_RB_DEBUG=1 to see the browser's own error output"
            )))
        }
    }
}

/// Launch (or attach to) the lane's browser. Agent lane → the isolated
/// Xvfb browser. Real lane → the user's own browser per config: clone or
/// profile launch (owned, `Some`), or attach to a running debuggable
/// browser (never owned, `None`). Returns that browser and the CDP base.
pub async fn launch_lane() -> Result<(Option<Browser>, String)> {
    if !crate::realbrowser::real_lane() {
        let browser = Browser::launch(0).await?;
        let base = browser.base();
        return Ok((Some(browser), base));
    }

    let cfg = crate::realbrowser::config();
    let (spec, profile) = crate::realbrowser::resolve_selection(&cfg)?;
    let mode = crate::realbrowser::effective_mode(&cfg, &profile.root);

    match mode {
        crate::realbrowser::Mode::Attach => {
            let port = crate::realbrowser::devtools_port(&profile.root).ok_or_else(|| {
                BladeError::Other(format!(
                    "no live debug endpoint found for `{}`. Auto-discovery reads Chrome's \
                     DevToolsActivePort file, which browsers write when started with \
                     `--remote-debugging-port=0` (or via chrome://inspect#remote-debugging on \
                     Chrome 144+). For a browser on a FIXED debug port, point bladebro at it \
                     directly: `bladebro nav <url> --port <N>` — a fixed port is also the \
                     stealthier arm: Chrome reports navigator.webdriver=true for an ephemeral \
                     port (measured, 151), and the lane never masks Chrome's own value. Or \
                     switch mechanism: `bladebro rb mode clone`.",
                    profile.path.display()
                ))
            })?;
            // SECURITY: the DevToolsActivePort file survives unclean exits
            // (SIGKILL/OOM) and the port it names can be squatted — verify a
            // live browser of this user actually holds the endpoint before
            // driving it (Linux; elsewhere the file's existence is the best
            // available signal).
            if !crate::browser::endpoint_owned_by_own_browser(port) {
                return Err(BladeError::Other(format!(
                    "the debug endpoint on port {port} (from {}'s DevToolsActivePort) is not \
                     served by any live browser of this user — the file is stale or something \
                     else took the port. Start the browser with `--remote-debugging-port=0`, \
                     or use `rb mode clone`.",
                    profile.root.display()
                )));
            }
            eprintln!(
                "[realbrowser] attaching to {} (`{}`) on port {port} — the browser stays yours \
                 (no launch, no shutdown)",
                spec.name, profile.name
            );
            Ok((None, format!("127.0.0.1:{port}")))
        }
        crate::realbrowser::Mode::Clone => {
            if spec.binary.as_os_str().is_empty() {
                return Err(BladeError::Other(format!(
                    "{} has a profile on this machine but no launchable binary was found — \
                     install it, pick another browser (`bladebro rb use`), or attach to a \
                     running instance (`rb mode attach`).",
                    spec.name
                )));
            }
            if !crate::realbrowser::has_template(&spec.id) {
                crate::realbrowser::ensure_import(&spec, &profile)?;
            }
            let root = crate::realbrowser::root_for(&spec.id);
            let sp = crate::session_profile::SessionProfile::create_real(&root)?;
            let browser =
                Browser::launch_real(&spec.binary, sp, cfg.visible, Some(profile.key.as_str()))
                    .await?;
            let base = browser.base();
            Ok((Some(browser), base))
        }
        crate::realbrowser::Mode::Profile => {
            if spec.binary.as_os_str().is_empty() {
                return Err(BladeError::Other(format!(
                    "{} has a profile on this machine but no launchable binary was found — \
                     install it, pick another browser (`bladebro rb use`), or attach to a \
                     running instance (`rb mode attach`).",
                    spec.name
                )));
            }
            if crate::realbrowser::requires_non_default_dir(spec.brand)
                && crate::realbrowser::same_dir(&profile.root, &spec.profile_root)
            {
                return Err(BladeError::Other(format!(
                    "Google Chrome (136+) refuses remote debugging on the DEFAULT profile dir \
                     (`{}`) — the debug endpoint is never exposed, so profile mode cannot work \
                     here. Options: (a) `bladebro rb mode clone` (recommended; works and your \
                     browser may stay open), (b) attach via chrome://inspect#remote-debugging \
                     on Chrome 144+, or (c) relaunch Chrome with a non-default \
                     --user-data-dir.",
                    profile.path.display()
                )));
            }
            if let Some(owner) = crate::realbrowser::profile_in_use(&profile.root) {
                return Err(BladeError::Other(format!(
                    "your browser is running on this profile ({owner}) — Chrome would hand off \
                     to it and swallow the debug flag. Close it first, or use `rb mode clone` \
                     (works while it stays open), or `rb mode attach`."
                )));
            }
            let sp = crate::session_profile::SessionProfile::adopt(&profile.root)?;
            let browser =
                Browser::launch_real(&spec.binary, sp, cfg.visible, Some(profile.key.as_str()))
                    .await?;
            let base = browser.base();
            Ok((Some(browser), base))
        }
        crate::realbrowser::Mode::Auto => unreachable!("effective_mode resolves Auto"),
    }
}

/// S15: warn when no emoji font is installed. Kasada/Akamai render emoji on
/// hidden canvases and hash the pixels; a missing emoji font produces a hash
/// no real browser generates. Linux-only (fc-list). Best-effort, never fatal.
pub(super) fn font_audit() {
    #[cfg(target_os = "linux")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let found = std::process::Command::new("fc-list")
                .args([":lang=und-zsye", "family"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or_else(|_| {
                    [
                        "/usr/share/fonts/noto/NotoColorEmoji.ttf",
                        "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
                        "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
                        "/usr/share/fonts/TTF/NotoColorEmoji.ttf",
                    ]
                    .iter()
                    .any(|p| std::path::Path::new(p).exists())
                });
            if !found {
                eprintln!(
                    "[bladebro] WARNING: no emoji font found — anti-bot canvas emoji hashes \
                     will mismatch (Kasada/Akamai). Install: noto-fonts-emoji (Arch) / \
                     fonts-noto-color-emoji (Debian)"
                );
            }
        });
    }
    // macOS/Windows: system fonts are always present, no audit needed.
}

/// G11: bounded startup-stderr capture. The runner investigation could not
/// diagnose a startup timeout because Chrome's stderr went to /dev/null;
/// keep the LAST launch's stderr on disk (truncated per launch) and read
/// back only a 4 KiB tail on startup failure. The file lives under the
/// blade home's logs dir (created 0700, file 0600 on Unix).
pub(super) fn chrome_stderr_sink() -> Option<std::fs::File> {
    let dir = crate::platform::blade_dir().join("logs");
    crate::platform::secure_create_dir_all(&dir).ok()?;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(dir.join("chrome-last-launch.log")).ok()
}

/// Tail of the last launch's Chrome stderr ("" when nothing was captured).
pub(super) fn chrome_stderr_tail() -> String {
    let path = crate::platform::blade_dir()
        .join("logs")
        .join("chrome-last-launch.log");
    let Ok(data) = std::fs::read(&path) else {
        return String::new();
    };
    if data.is_empty() {
        return String::new();
    }
    let start = data.len().saturating_sub(4096);
    let tail = String::from_utf8_lossy(&data[start..]);
    format!("\n--- chrome stderr (tail of last launch) ---\n{tail}")
}
