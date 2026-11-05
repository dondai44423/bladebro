//! Browser process management — find Chrome, launch it with stealth flags,
//! wait for the CDP debug endpoint, and clean up on drop.
//!
//! The "one binary, works out of the box" promise (D2/D8): Bladebro finds
//! Chrome itself — no manual `--remote-debugging-port` setup. On NixOS it
//! scans the nix store via `fd`; on mainstream distros it checks PATH and
//! common install paths; everywhere it respects `CHROME_PATH`.
//!
//! Stealth mode: if Xvfb (virtual X display) is available, Chrome runs in
//! headful mode on a virtual display. This eliminates most headless-detection
//! signals at the root (real CSS rendering, a live GL stack, real UA in workers).
//! If Xvfb isn't available, falls back to `--headless=new`.
//!
//! Module map: this file is the `Browser` struct, its accessors and the
//! shutdown/drop contract; `flags` builds the command line, `probe` runs the
//! GL healthcheck, `display` owns the Xvfb + WM layer (Linux), `launch` is the
//! launch ladder + lane dispatch, `pipe` is the pipe-transport launch, and
//! `discover` finds Chrome/Xvfb on this system.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{BladeError, Result};
use crate::platform;

mod alerts;
mod compat;
mod discover;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod display;
mod flags;
mod launch;
mod pipe;
mod probe;
pub mod proxy;

pub use self::alerts::{
    pending_notes, reset_gl_alert, set_stealth_inject_failed, set_ua_override_failed,
};
pub(crate) use self::compat::{incognito_policy, launched_incognito};
pub(crate) use self::discover::endpoint_owned_by_own_browser;
pub(crate) use self::discover::find_chrome;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use self::display::VirtualDisplay;
pub use self::flags::{
    gl_mid_session_loss, gpu_state, is_software_renderer, launched_headless, launched_no_sandbox,
    launched_pipe, mark_gl_mid_session_loss, reconcile_gl, set_gpu_state, set_launched_headless,
    set_launched_no_sandbox, set_launched_pipe, GlReconcile, GlStage, GpuState, Transport,
};
pub use self::launch::launch_lane;
pub use self::probe::{probe_gl_live, GlLive, WorkerEvidence};
// Chromium appends a singleton socket name under TMPDIR. Hermes and other
// harnesses may supply a long workspace path, exceeding even macOS's 104-byte
// sockaddr_un limit and fatally aborting Chrome. Only the browser child uses
// the short system temp root; Chromium creates its own private random directory.
fn browser_temp_env(cmd: &mut Command) {
    #[cfg(unix)]
    if std::env::temp_dir().as_os_str().as_encoded_bytes().len() > 48 {
        cmd.env("TMPDIR", "/tmp");
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// A launched Chrome process + virtual display + session
/// profile. Chrome killed on Drop; the session profile is
/// synced back to the template and removed on explicit
/// [`Browser::shutdown`] (graceful) or by the next launch's
/// orphan reaper (ungraceful death).
pub struct Browser {
    child: Child,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[allow(dead_code)]
    xvfb: Option<VirtualDisplay>,
    port: u16,
    profile: crate::session_profile::SessionProfile,
    shutdown_connection: Option<ShutdownConnection>,
    close_requested: bool,
}

enum ShutdownConnection {
    WebSocket(String),
    #[cfg(unix)]
    Pipe(crate::cdp::CdpClient),
}

impl Browser {
    /// The port Chrome's debug endpoint is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The session profile directory (for periodic sync-back).
    pub fn profile_dir(&self) -> &std::path::Path {
        self.profile.dir()
    }

    /// `host:port` string for CDP HTTP discovery calls.
    pub fn base(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // A requested Browser.close gets a flush grace before forced cleanup.
        // Unwinding/failed launches retain the emergency process-tree path.
        platform::finish_child_shutdown(&mut self.child, self.close_requested);
        stop_profile_processes(&self.profile, self.close_requested);
        // Xvfb is dropped here too (field order: child first, then xvfb).
    }
}

/// Stop browser processes still running on this session's own profile once
/// the launcher's process group is gone. Under Flatpak the portal spawns
/// Chromium's zygote and utility sandboxes in its own session, so the group
/// signal never reaches them, and where the portal does not nest them in the
/// browser's pid namespace they outlive it (observed on SteamOS after failed
/// launches). Only bladebro's own agent-lane session dirs qualify: a
/// borrowed or real-lane profile may be shared with the user's own browser.
/// After a requested close the stragglers get a flush grace first.
pub(crate) fn stop_profile_processes(
    profile: &crate::session_profile::SessionProfile,
    close_requested: bool,
) {
    if !profile.is_owned_agent_session() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let left = || discover::processes_on_user_data_dir(profile.dir());
        let gone_within = |wait: Duration| {
            let deadline = Instant::now() + wait;
            loop {
                if left().is_empty() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        if left().is_empty() || (close_requested && gone_within(Duration::from_secs(2))) {
            return;
        }
        // Each round re-reads /proc, so only processes that still carry our
        // profile flag are signalled.
        for pid in left() {
            platform::kill_process_graceful(pid);
        }
        if gone_within(Duration::from_secs(2)) {
            return;
        }
        for pid in left() {
            platform::kill_process_force(pid);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = close_requested;
}

impl Browser {
    /// Let Chrome flush its storage services before process-tree cleanup.
    /// Signalling the entire process group first also kills those services,
    /// losing localStorage even when Chrome's main process exits cleanly.
    pub async fn shutdown_gracefully(mut self) {
        let client = match self.shutdown_connection.take() {
            Some(ShutdownConnection::WebSocket(url)) => {
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    crate::cdp::CdpClient::connect(&url),
                )
                .await
                {
                    Ok(Ok(client)) => Some(client),
                    _ => None,
                }
            }
            #[cfg(unix)]
            Some(ShutdownConnection::Pipe(client)) => Some(client),
            None => None,
        };
        if let Some(client) = client {
            let owned = client
                .send_with_timeout("SystemInfo.getProcessInfo", None, Duration::from_secs(1))
                .await
                .ok()
                .and_then(|v| v.get("processInfo").and_then(|p| p.as_array()).cloned())
                .is_some_and(|rows| {
                    rows.iter().any(|p| {
                        p["type"] == "browser"
                            && p["id"]
                                .as_u64()
                                .and_then(|id| u32::try_from(id).ok())
                                .is_some_and(|id| {
                                    platform::owns_browser_process(self.child.id(), id)
                                })
                    })
                });
            if owned {
                // Close may disconnect before acknowledging. In either case
                // give the owned tree its flush grace before forced cleanup.
                self.close_requested = true;
                let _ = client
                    .send_with_timeout("Browser.close", None, Duration::from_secs(2))
                    .await;
            }
        }
        if !self.close_requested {
            eprintln!("[bladebro] graceful close of the owned browser was unavailable; using process cleanup, recent storage flush is unverified");
        }
        let _ = tokio::task::spawn_blocking(move || self.shutdown()).await;
    }

    /// Synchronous process cleanup + profile sync. Normal async callers use
    /// shutdown_gracefully first so Chrome can flush its storage services.
    /// On SIGKILL nothing runs; the next launch's orphan reaper cleans up.
    pub fn shutdown(self) {
        let profile = self.profile.clone();
        drop(self); // kills Chrome + Xvfb
                    // Retain ownership metadata: a path under /tmp is not proof that
                    // an adopted user profile belongs to Bladebro.
        profile.cleanup();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod cleanup_tests {
    use super::*;

    /// A long-lived process whose own argv carries `flag`, like a sandboxed
    /// Chromium helper. The loop keeps `sh` itself alive (a lone command could
    /// be exec'd, dropping the flag) and leaves at most a 0.2 s sleep behind.
    fn spawn_sleeper(flag: &str) -> Child {
        use std::os::unix::process::CommandExt;
        Command::new("sh")
            .args(["-c", "while :; do sleep 0.2; done", "fixture", flag])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn fixture process")
    }

    fn exits_within(child: &mut Child, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if child.try_wait().is_ok_and(|s| s.is_some()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Flatpak's portal (zypak) runs Chromium's zygote and utility sandboxes in
    /// the portal's own session, outside the launcher's process group. Where
    /// the portal does not nest them in the browser's pid namespace they
    /// outlive the launcher (observed on SteamOS). Tearing the Browser down
    /// must stop every process running on its own session profile — and
    /// nothing that merely shares a path prefix.
    #[test]
    fn teardown_stops_profile_processes_outside_the_launcher_group() {
        let root = std::env::temp_dir().join(format!(
            "blade-sweep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("sess-1");
        std::fs::create_dir_all(&dir).unwrap();
        let mut zygote = spawn_sleeper(&format!("--user-data-dir={}", dir.display()));
        let mut neighbour = spawn_sleeper(&format!(
            "--user-data-dir={}",
            root.join("sess-10").display()
        ));
        let launcher = spawn_sleeper("launcher");

        drop(Browser {
            child: launcher,
            xvfb: None,
            port: 0,
            profile: crate::session_profile::SessionProfile::owned_for_test(&dir),
            shutdown_connection: None,
            close_requested: false,
        });

        let zygote_stopped = exits_within(&mut zygote, Duration::from_secs(3));
        let neighbour_alive = neighbour.try_wait().is_ok_and(|s| s.is_none());
        // Single-process fixtures: std's kill is a no-op once reaped.
        for fixture in [&mut zygote, &mut neighbour] {
            let _ = fixture.kill();
            let _ = fixture.wait();
        }
        std::fs::remove_dir_all(&root).unwrap();

        assert!(
            zygote_stopped,
            "a browser process on our session profile must not outlive teardown"
        );
        assert!(
            neighbour_alive,
            "a process on a different profile (shared prefix) must never be signalled"
        );
    }
}
