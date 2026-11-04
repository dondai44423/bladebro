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
        // Xvfb is dropped here too (field order: child first, then xvfb).
    }
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
