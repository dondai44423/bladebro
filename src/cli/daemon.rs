//! CLI daemon: lifecycle (spawn/stop/pidfile) and the socket server loop.
//!
//! Same lifecycle guarantees as the MCP server: lazy launch, self-healing
//! mid-session (crash relaunch + lane/settings drift), idle timeout
//! (`BLADE_IDLE_TIMEOUT`, 0 disables), periodic login snapshot + knowledge
//! sync, and pidfile-ownership teardown so a ghost daemon can never unlink a
//! live daemon's socket.

use crate::error::{BladeError, Result};

#[cfg(unix)]
use super::{daemon_running, dispatch, ignore_sighup, socket_path, warm_profile};
#[cfg(unix)]
use crate::page::Page;
#[cfg(unix)]
use serde_json::{json, Value};

/// Wait for a termination signal (SIGTERM/SIGINT/SIGHUP on Unix, Ctrl+C on Windows).
/// Same as the MCP server's signal handler.
#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Only catch SIGTERM and SIGINT. NOT SIGHUP — SIGHUP is sent
        // when the parent process exits, which would kill the daemon
        // immediately after the first CLI command. Daemons ignore SIGHUP.
        let mut term = signal(SignalKind::terminate()).ok();
        let mut int = signal(SignalKind::interrupt()).ok();
        tokio::select! {
            _ = async { if let Some(s) = &mut term { s.recv().await } else { std::future::pending().await } } => {}
            _ = async { if let Some(s) = &mut int { s.recv().await } else { std::future::pending().await } } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Best-effort daemon restart so a lane switch takes effect immediately:
/// a running daemon holds the OLD lane's browser. Unix only (daemon mode is).
pub(super) async fn restart_daemon_for_lane() {
    #[cfg(unix)]
    {
        if daemon_running() {
            let _ = stop_daemon().await;
        }
    }
}

/// Start the CLI daemon: persistent Chrome + Unix socket server.
/// Same lifecycle as MCP (lazy launch, self-healing, idle timeout, reaper).
#[cfg(unix)]
pub async fn run_daemon() -> Result<()> {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    // Ignore SIGHUP — the daemon must survive the parent CLI exiting.
    #[cfg(unix)]
    ignore_sighup();

    let path = socket_path();
    // Hold an OS lock for the daemon lifetime. A connect/remove/bind sequence
    // alone races two cold starts and can unlink the winner's live socket.
    if let Some(parent) = path.parent() {
        crate::platform::secure_create_dir_all(parent)?;
    }
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path.with_extension("lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            eprintln!(
                "[bladebro] daemon already running or starting on {}",
                path.display()
            );
            return Ok(());
        }
        return Err(error.into());
    }
    // Never steal a LIVE daemon's socket: connect to check first. Without
    // this, a second `bladebro daemon` rebinds over the active one and
    // orphans it — a ghost that keeps its Chrome but that `stop` can no
    // longer reach (and whose death later unlinks the new daemon's socket).
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        eprintln!(
            "[bladebro] daemon already running on {} — exiting",
            path.display()
        );
        return Ok(());
    }
    // Remove stale socket (connect failed — nobody is listening).
    let _ = std::fs::remove_file(&path);
    // Create parent dir with secure permissions.
    if let Some(parent) = path.parent() {
        let _ = crate::platform::secure_create_dir_all(parent);
    }

    let listener = UnixListener::bind(&path)
        .map_err(|e| BladeError::Other(format!("failed to bind socket {}: {e}", path.display())))?;

    // SECURITY: Restrict socket to owner-only. Without this, the socket
    // inherits umask permissions (often 755), which on misconfigured systems
    // (umask 000) lets any local user connect and control the browser.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    eprintln!("[bladebro] daemon listening on {}", path.display());
    let _ = std::fs::write(pid_path(), std::process::id().to_string());

    let mut browser: Option<crate::browser::Browser> = None;
    let mut page: Option<Page> = None;
    // Launch-input fingerprint of the live browser + one-shot stale-binary
    // latch (see the per-command drift check below).
    let mut launched = 0u64;
    let mut stale_warned = false;
    // One-shot latch: has the GL-less advisory been delivered?
    let mut gl_warned = false;
    let mut last_activity = std::time::Instant::now();
    let idle_secs: u64 = std::env::var("BLADE_IDLE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let mut idle_check = tokio::time::interval(std::time::Duration::from_secs(15));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let knowledge = crate::knowledge::load_shared();
    // Periodic profile sync: every 60s while Chrome is alive,
    // sync the session profile to the template. SIGKILL resilience.
    let mut last_sync = std::time::Instant::now();
    let sync_interval = std::time::Duration::from_secs(60);

    loop {
        tokio::select! {
            // Graceful shutdown on SIGTERM/SIGINT/SIGHUP.
            _ = wait_for_shutdown_signal() => {
                eprintln!("[bladebro] termination signal \u{2014} shutting down Chrome gracefully");
                break;
            }
            // Accept new connections.
            accept = listener.accept() => {
                let (stream, _) = match accept {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[bladebro] accept error: {e}");
                        continue;
                    }
                };

                let mut reader = BufReader::new(stream);
                let mut input = Vec::new();
                // Bounded read: a client that connects and never writes (or
                // dies mid-request) must not stall the whole daemon — accepts,
                // signals and the idle timer all sit behind this await.
                let read = tokio::select! {
                    _ = wait_for_shutdown_signal() => break,
                    read = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    crate::framing::read_until_limited(&mut reader, b'\n', &mut input, 8 * 1024 * 1024),
                )
                    => read,
                };
                match read {
                    Ok(Ok(_)) => {}
                    _ => continue, // io error, or a silent client — drop it
                }
                let line = match std::str::from_utf8(&input) {
                    Ok(line) => line,
                    Err(_) => continue,
                };
                let line = line.trim();
                if line.is_empty() { continue; }

                let req: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let tool = req.get("tool").and_then(|t| t.as_str()).unwrap_or("");
                let args = req.get("args").cloned().unwrap_or(json!({}));

                // Stop command.
                if tool == "stop" {
                    // Flush the knowledge base BEFORE acknowledging: once
                    // `bladebro stop` returns, consent/visits/timing/block
                    // knowledge must already be durable on disk.
                    if let Ok(mut kb) = knowledge.lock() {
                        kb.prune();
                        kb.sync();
                    }
                    let resp = json!({ "ok": true, "text": "daemon stopped" });
                    let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                    let _ = reader.get_mut().write_all(resp_str.as_bytes()).await;
                    let _ = reader.get_mut().write_all(b"\n").await;
                    eprintln!("[bladebro] daemon stopping (stop command)");
                    break;
                }

                // Lazy launch + self-heal + lane/settings drift (same as the
                // MCP server): a running browser belongs to the lane it was
                // launched on, so `rb on|off` (or a hand-edited config) must
                // relaunch it — otherwise the switch is silently ignored.
                // A live session can also be ATTACHED (page alive, browser
                // None): the same switch must DETACH it — a lane-flag flip
                // alone would keep steering the user's browser, and a tab
                // opened after it would get agent-lane page patches.
                let lane_switched = crate::realbrowser::refresh_lane();
                let session_live = browser.is_some()
                    || page.as_ref().map(|p| !p.is_closed()).unwrap_or(false);
                let drifted = crate::realbrowser::session_drifted(
                    session_live,
                    lane_switched,
                    crate::realbrowser::launch_fingerprint(),
                    launched,
                );
                let need_launch = drifted
                    || page.is_none()
                    || page.as_ref().map(|p| p.is_closed()).unwrap_or(true);
                let mut detached_attach = false;
                if need_launch {
                    if browser.is_some() {
                        eprintln!(
                            "[bladebro] browser relaunch: {}",
                            if drifted { "lane/settings changed" } else { "connection lost" }
                        );
                        if let Some(b) = browser.take() {
                            let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    } else if drifted {
                        // Attach session (never owned): detach — drop the
                        // attached page; the user's browser is not ours to
                        // close. The launch below opens the browser the
                        // current lane/settings call for.
                        eprintln!(
                            "[bladebro] lane/settings changed — detaching from the attached browser"
                        );
                        detached_attach = true;
                        page = None;
                    }
                    match launch_browser().await {
                        Ok((new_page, new_browser)) => {
                            browser = new_browser;
                            page = Some(new_page);
                            launched = crate::realbrowser::launch_fingerprint();
                            if let Some(ref mut p) = page {
                                p.set_knowledge(knowledge.clone());
                            }
                            // Never warm the real lane: it would navigate the
                            // user's own browser to seed sites.
                            if !crate::realbrowser::real_lane()
                                && crate::session_profile::SessionProfile::claim_warming()
                            {
                                if let Some(ref mut p) = page {
                                    warm_profile(p).await;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[bladebro] Chrome launch failed: {e}");
                            let resp = json!({ "ok": false, "error": format!("Chrome launch failed: {e}") });
                            let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                            let _ = reader.get_mut().write_all(resp_str.as_bytes()).await;
                            let _ = reader.get_mut().write_all(b"\n").await;
                            continue;
                        }
                    }
                }

                // Dispatch.
                let result = {
                    let p = page.as_mut().unwrap();
                    dispatch(tool, &args, p).await
                };

                // One-line advisories for the human: a mid-session lane switch
                // actually landed, or this daemon runs a binary that was
                // replaced on disk (a restart is one `bladebro stop` away).
                let mut advisories: Vec<String> = Vec::new();
                if drifted {
                    advisories.push(match (detached_attach, crate::realbrowser::real_lane()) {
                        (false, true) => "note: real-browser lane ON — relaunched as your own browser (page state reset)".into(),
                        (false, false) => "note: real-browser lane OFF — relaunched as the isolated agent browser (page state reset)".into(),
                        (true, true) => "note: real-browser settings changed — detached from the attached browser and reopened per the current settings (page state reset)".into(),
                        (true, false) => "note: real-browser lane OFF — detached from the attached browser (left running, untouched) and relaunched as the isolated agent browser (page state reset)".into(),
                    });
                }
                if !stale_warned && crate::platform::stale_binary() {
                    stale_warned = true;
                    advisories.push(
                        "note: this daemon runs a binary that was replaced on disk — `bladebro stop` to pick up the new build".into(),
                    );
                }
                if !gl_warned {
                    if let Some(crate::browser::GpuState::Missing) = crate::browser::gpu_state() {
                        gl_warned = true;
                        advisories.push(
                            "note: this browser has no WebGL (getContext('webgl') returns null — stock-equivalent on this host); no GL mask is applied".into(),
                        );
                    }
                }
                let with_notes = |text: String| -> String {
                    if advisories.is_empty() {
                        text
                    } else {
                        format!("{}\n{}", advisories.join("\n"), text)
                    }
                };

                let resp = match result {
                    Ok(r) => json!({
                        "ok": true,
                        "text": with_notes(r.text),
                        "image": r.image,
                        "is_error": r.is_error,
                    }),
                    Err(BladeError::Closed) => {
                        // Self-heal: relaunch and retry.
                        eprintln!("[bladebro] browser closed during tool call, reconnecting...");
                        if let Some(b) = browser.take() {
                            let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                        }
                        page = None;
                        match launch_browser().await {
                            Ok((new_page, new_browser)) => {
                                browser = new_browser;
                                page = Some(new_page);
                                if let Some(ref mut p) = page {
                                    p.set_knowledge(knowledge.clone());
                                }
                                let p = page.as_mut().unwrap();
                                if !crate::mcp::server::retry_safe(tool, &args) {
                                    json!({"ok":false,"error":"Browser connection lost mid-action; outcome UNKNOWN. Chrome restarted. Inspect the destination before repeating the action."})
                                } else { match dispatch(tool, &args, p).await {
                                    Ok(r) => json!({
                                        "ok": true,
                                        "text": with_notes(r.text),
                                        "image": r.image,
                                        "is_error": r.is_error,
                                    }),
                                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                                } }
                            }
                            Err(e) => json!({ "ok": false, "error": format!("reconnect failed: {e}") }),
                        }
                    }
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                };

                let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                let _ = reader.get_mut().write_all(resp_str.as_bytes()).await;
                let _ = reader.get_mut().write_all(b"\n").await;

                last_activity = std::time::Instant::now();
            }
            _ = idle_check.tick() => {
                // Real lane: the browser is the user's — never yanked by the
                // idle timer unless `idle_shutdown` opts in.
                if idle_secs > 0
                    && browser.is_some()
                    && last_activity.elapsed().as_secs() > idle_secs
                    && crate::realbrowser::should_idle_shutdown()
                {
                    eprintln!("[bladebro] idle timeout ({idle_secs}s), shutting down Chrome");
                    if let Some(b) = browser.take() {
                        if let Some(ref p) = page {
                            if !crate::realbrowser::real_lane() {
                                if let Err(e) = crate::logins::snapshot(p.cdp_ref()).await { eprintln!("[bladebro] login snapshot failed: {e}"); }
                            }
                        }
                        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                    }
                    page = None;
                }
                // Periodic login snapshot (same as MCP server). Persist the
                // authoritative live cookie store, never a hot profile copy.
                if browser.is_some() && last_sync.elapsed() > sync_interval {
                    last_sync = std::time::Instant::now();
                    // Never snapshot on the real lane: the user's live cookie
                    // store is not ours to hoard into the data dir.
                    if !crate::realbrowser::real_lane() {
                        if let Some(ref p) = page {
                            if !p.cdp_ref().is_closed() {
                                if let Err(e) = crate::logins::snapshot(p.cdp_ref()).await { eprintln!("[bladebro] login snapshot failed: {e}"); }
                            }
                        }
                    }
                    // Sync the knowledge base to disk (prune + write) —
                    // consent/visits/timing/block-risk compound across sessions.
                    let kb = knowledge.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(mut kb) = kb.lock() {
                            kb.prune();
                            kb.sync();
                        }
                    }).await;
                }
            }
        }
    }

    // Cleanup: persist live logins, then kill Chrome gracefully.
    // (Never on the real lane — not our cookie store.)
    if let Some(ref p) = page {
        if !crate::realbrowser::real_lane() {
            if let Err(e) = crate::logins::snapshot(p.cdp_ref()).await {
                eprintln!("[bladebro] login snapshot failed: {e}");
            }
        }
    }
    if let Some(b) = browser {
        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
    }
    // Flush the knowledge base before exit (consent, visits, timing, risk).
    if let Ok(mut kb) = knowledge.lock() {
        kb.prune();
        kb.sync();
    }
    // Clean up only while WE still own the socket + pidfile. If a newer
    // daemon has taken over (this one is a "ghost"), unlinking would
    // delete the LIVE daemon's files and orphan it in turn — the
    // self-perpetuating ghost cycle. Ownership test: the pidfile says us.
    if read_pid_file() == Some(std::process::id() as i32) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(pid_path());
    }
    eprintln!("[bladebro] daemon stopped");
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_daemon() -> Result<()> {
    Err(BladeError::Other(
        "daemon mode is Unix-only (requires Unix sockets)".into(),
    ))
}

/// Launch the lane's browser and create a Page for the daemon.
/// Cleans up the browser if any step after launch fails.
#[cfg(unix)]
async fn launch_browser() -> Result<(Page, Option<crate::browser::Browser>)> {
    // Re-read the lane at every launch: `rb on|off` (or a hand-edited
    // config) takes effect on the next launch, even inside a long-lived
    // daemon or MCP process. The daemon/MCP loops additionally detect the
    // change mid-session and relaunch a live browser (see run_daemon).
    crate::realbrowser::init_lane();
    let (browser, base) = crate::browser::launch_lane().await?;
    let result = async {
        let target = crate::cdp::first_page_target(&base).await?;
        let client = crate::cdp::CdpClient::connect(target.ws_url()?).await?;
        let page = Page::attach(crate::cdp::CdpSession::root(client), &base, None).await?;
        if browser.is_some() && !crate::realbrowser::real_lane() {
            crate::logins::restore(page.cdp_ref()).await?;
        }
        Ok(page)
    }
    .await;

    match result {
        Ok(page) => Ok((page, browser)),
        Err(e) => {
            // Clean up a browser we launched (attach lane: never ours).
            if let Some(b) = browser {
                let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
            }
            Err(e)
        }
    }
}

// ── Daemon lifecycle helpers ───────────────────────────────────────────

/// Path of the daemon pid file. Stale files (SIGKILL) are detected by
/// checking whether the pid is alive, and only then killed.
#[cfg(unix)]
fn pid_path() -> std::path::PathBuf {
    crate::platform::blade_dir().join("cli.pid")
}

#[cfg(unix)]
fn read_pid_file() -> Option<i32> {
    std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    pid > 1 && crate::platform::process_alive(pid as u32)
}

/// Verify the pid is actually a bladebro process before killing it — pids
/// get recycled and a blind kill could hit an innocent process. macOS uses
/// ps executable identity; an unreadable identity never authorizes a kill.
#[cfg(unix)]
fn looks_like_bladebro(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim() == "bladebro")
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .is_ok_and(|output| {
                std::path::Path::new(String::from_utf8_lossy(&output.stdout).trim())
                    .file_name()
                    .is_some_and(|name| name == "bladebro")
            })
    }
}

/// Stop the daemon: graceful over the socket; idempotent when nothing runs.
///
/// Reliability: if the socket is gone but the pid file says a daemon is
/// alive (wedged, or SIGKILLed mid-cleanup), terminate it — otherwise
/// `stop` would lie "not running" while an orphan Chrome kept running.
#[cfg(unix)]
pub async fn stop_daemon() -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let path = socket_path();

    // Graceful path: a live daemon answers the stop command itself
    // (it flushes logins + knowledge before acknowledging).
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        writeln!(stream, "{{\"tool\":\"stop\"}}")?;
        stream.flush()?;
        let mut resp = String::new();
        stream.read_to_string(&mut resp)?;
        let response: Value = serde_json::from_str(resp.trim())?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(BladeError::Other(
                "daemon did not acknowledge shutdown".into(),
            ));
        }
        // Wait for the daemon to finish teardown — it removes the socket
        // file last. Without this, an immediate second `stop` could connect
        // to the dying daemon's still-bound socket and report "stopped"
        // again instead of "not running".
        for _ in 0..50 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if path.exists() {
            return Err(BladeError::Other("daemon acknowledged stop but cleanup is still running; check again before restarting".into()));
        }
        return Ok("daemon stopped".into());
    }

    // No socket. A daemon process may still linger — check the pid file.
    if let Some(pid) = read_pid_file() {
        if process_alive(pid) && looks_like_bladebro(pid) {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            for _ in 0..50 {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if process_alive(pid) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let _ = std::fs::remove_file(pid_path());
            let _ = std::fs::remove_file(&path);
            return Ok(format!("daemon (pid {pid}) was unresponsive — terminated"));
        }
        let _ = std::fs::remove_file(pid_path());
        let _ = std::fs::remove_file(&path);
        return Ok("daemon not running".into());
    }

    let _ = std::fs::remove_file(&path);
    Ok("daemon not running".into())
}

#[cfg(not(unix))]
pub async fn stop_daemon() -> Result<String> {
    Ok("daemon mode is Unix-only — commands run one-shot on this platform".into())
}
