//! Pipe transport — `--remote-debugging-pipe` launches (no debug port).
//! Opt-in lane; the WS transport is the default.

#[cfg(unix)]
use super::discover::find_chrome;
#[cfg(target_os = "linux")]
use super::display::{apply_xvfb_env, VirtualDisplay};
#[cfg(unix)]
use super::flags::{classify_gl, gl_stages, launch_args, stage_label, LaunchCfg};
#[cfg(unix)]
use super::launch::font_audit;
#[cfg(unix)]
use super::probe::probe_gl_pipe;
use super::*;

impl Browser {
    /// Launch Chrome with CDP over `--remote-debugging-pipe` (S1: zero-port
    /// CDP). No TCP listener exists, so page JavaScript cannot probe for an
    /// open debugging port and no WebSocket handshake residue exists.
    /// Chrome reads commands from fd 3 and writes responses to fd 4.
    ///
    /// Returns the Browser handle (kills Chrome + Xvfb on drop) plus a
    /// connected browser-level CDP client.
    #[cfg(unix)]
    pub async fn launch_pipe() -> Result<(Self, crate::cdp::CdpClient)> {
        // Sandbox-first, --no-sandbox fallback (same policy as launch()).
        let mut last_err = None;
        for (attempt, no_sandbox) in [(0u8, false), (1u8, true), (2u8, true)] {
            match Self::launch_pipe_inner(no_sandbox).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let startup_exit = e.to_string().contains("exited during startup");
                    last_err = Some(e);
                    if !startup_exit {
                        break;
                    }
                    if attempt == 0 {
                        eprintln!(
                            "[bladebro] sandboxed Chrome died at startup (root or restricted container?) — retrying with --no-sandbox"
                        );
                    } else if attempt == 1 {
                        eprintln!("[bladebro] Chrome died at startup, retrying");
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| BladeError::Other("pipe launch failed".into())))
    }

    /// Launch over the pipe transport with the same GL ladder as the WS
    /// path (shared `gl_stages()` — lane parity is structural, not a
    /// promise).
    #[cfg(unix)]
    async fn launch_pipe_inner(no_sandbox: bool) -> Result<(Self, crate::cdp::CdpClient)> {
        let stages = gl_stages();
        let mut last: Option<(Self, crate::cdp::CdpClient)> = None;
        for (i, stage) in stages.iter().enumerate() {
            if let Some((b, c)) = last.take() {
                drop(c);
                let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
            }
            let (browser, client, probe) =
                Self::launch_pipe_inner_stage(no_sandbox, *stage).await?;
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
                    return Ok((browser, client));
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
                        last = Some((browser, client));
                    } else {
                        eprintln!(
                            "[bladebro] WARNING: no WebGL context after the full GL ladder — \
                             pages will see `getContext('webgl') === null` (stock Chrome on a \
                             GL-less host behaves identically; no GL mask is applied). Run `bladebro audit`."
                        );
                        set_gpu_state(Some(GpuState::Missing));
                        return Ok((browser, client));
                    }
                }
            }
        }
        Err(BladeError::Other("no GL stages configured".into()))
    }

    /// One pipe launch attempt with a pinned GL stage (driven by
    /// `launch_pipe_inner`).
    #[cfg(unix)]
    async fn launch_pipe_inner_stage(
        no_sandbox: bool,
        stage: GlStage,
    ) -> Result<(Self, crate::cdp::CdpClient, Option<String>)> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use tokio::net::unix::pipe;

        let chrome_path = find_chrome()?;
        let profile = crate::session_profile::SessionProfile::create()?;
        let user_data_dir = profile.dir().to_path_buf();
        font_audit();

        #[cfg(target_os = "linux")]
        let xvfb = VirtualDisplay::start().ok();
        #[cfg(target_os = "linux")]
        if xvfb.is_none() {
            eprintln!(
                "[bladebro] WARNING: Xvfb unavailable — falling back to headless mode (reduced stealth). Install xvfb for headful-on-virtual-display."
            );
        }
        #[cfg(target_os = "linux")]
        let headful = xvfb.is_some();

        #[cfg(not(target_os = "linux"))]
        let headful = true; // macOS/Windows have native window servers

        // After both bindings: the recording must compile on every target.
        set_launched_headless(!headful);
        set_launched_no_sandbox(no_sandbox);

        // M18: Proxy support via BLADE_PROXY env var. Validated + redacted
        // exactly like the WS lane — same failure mode, same log guarantee.
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
        // Power-user escape hatch (same as the WS path — this used to be
        // WS-only, another quiet lane divergence).
        let extra: Vec<String> = std::env::var("BLADE_CHROME_FLAGS")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        set_launched_pipe(true);
        let args = launch_args(&LaunchCfg {
            stage,
            headful,
            no_sandbox,
            transport: Transport::Pipe,
            port: 0,
            user_data_dir: &user_data_dir,
            proxy: proxy.as_ref().map(|s| s.server.as_str()),
            extra: &extra,
        });

        // Pipe pairs: out = us→chrome (chrome reads fd 3), in = chrome→us
        // (chrome writes fd 4). We keep out_tx/in_rx; the child-side ends
        // become fds 3/4 in the child via pre_exec dup2.
        let (out_tx, out_rx) =
            pipe::pipe().map_err(|e| BladeError::Other(format!("pipe create: {e}")))?;
        let (in_tx, in_rx) =
            pipe::pipe().map_err(|e| BladeError::Other(format!("pipe create: {e}")))?;

        // Child-side ends: blocking fds (Chrome does blocking IO on 3/4).
        let child_read_fd = out_rx
            .into_blocking_fd()
            .map_err(|e| BladeError::Other(format!("pipe fd: {e}")))?;
        let child_write_fd = in_tx
            .into_blocking_fd()
            .map_err(|e| BladeError::Other(format!("pipe fd: {e}")))?;
        if child_read_fd.as_raw_fd() <= 4 || child_write_fd.as_raw_fd() <= 4 {
            return Err(BladeError::Other(
                "pipe fds collided with stdio — unset BLADE_TRANSPORT (the MCP defaults to WebSocket) or set BLADE_TRANSPORT=ws to force it".into(),
            ));
        }

        #[cfg(target_os = "linux")]
        let mode_str = if headful {
            "headful (Xvfb)"
        } else {
            "headless"
        };
        #[cfg(not(target_os = "linux"))]
        let mode_str = "headful";
        eprintln!("[bladebro] launching Chrome from {chrome_path} on CDP pipe ({mode_str})");

        let mut cmd = Command::new(&chrome_path);
        browser_temp_env(&mut cmd);
        // G11: bounded startup-stderr capture (shared with the WS lane) —
        // a failed launch must be diagnosable instead of guessed at.
        cmd.args(&args).stdout(Stdio::null());
        match super::launch::chrome_stderr_sink() {
            Some(f) => {
                cmd.stderr(Stdio::from(f));
            }
            None => {
                cmd.stderr(Stdio::null());
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(ref xvfb) = xvfb {
            apply_xvfb_env(&mut cmd, xvfb);
        }
        // In the child (post-fork, pre-exec): our pipe ends become fds 3/4.
        // The OwnedFds are moved into the closure — the parent's copies close
        // when the closure drops after spawn; the child's dup2'd copies
        // survive exec (dup2 clears CLOEXEC).
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(child_read_fd.as_raw_fd(), 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(child_write_fd.as_raw_fd(), 4) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| BladeError::Other(format!("failed to launch Chrome: {e}")))?;

        let client = crate::cdp::CdpClient::from_pipe(in_rx, out_tx)?;

        // Readiness probe: Browser.getVersion over the pipe, with retries.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match client
                .send_with_timeout("Browser.getVersion", None, Duration::from_secs(2))
                .await
            {
                Ok(v) => {
                    let product = v
                        .get("product")
                        .and_then(|p| p.as_str())
                        .unwrap_or("unknown");
                    eprintln!("[bladebro] Chrome ready: {product} (pipe transport)");
                    let probe = probe_gl_pipe(&client).await;
                    return Ok((
                        Self {
                            child,
                            #[cfg(target_os = "linux")]
                            xvfb,
                            port: 0,
                            profile,
                        },
                        client,
                        probe,
                    ));
                }
                Err(_) => {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            return Err(BladeError::Other(format!(
                                "Chrome exited during startup: {status}{}",
                                super::launch::chrome_stderr_tail()
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
                            "Chrome pipe not responding after 20s{}",
                            super::launch::chrome_stderr_tail()
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }
}
