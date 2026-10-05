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

mod discover;
#[cfg(target_os = "linux")]
mod display;
mod flags;
mod launch;
mod pipe;
mod probe;

#[cfg(target_os = "linux")]
pub use self::display::VirtualDisplay;
pub use self::flags::{
    gpu_state, is_software_renderer, launched_headless, launched_pipe, set_gpu_state,
    set_launched_headless, set_launched_pipe, GlStage, GpuState, Transport,
};
pub use self::launch::launch_lane;
/// A launched Chrome process + virtual display + session
/// profile. Chrome killed on Drop; the session profile is
/// synced back to the template and removed on explicit
/// [`Browser::shutdown`] (graceful) or by the next launch's
/// orphan reaper (ungraceful death).
pub struct Browser {
    child: Child,
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    xvfb: Option<VirtualDisplay>,
    port: u16,
    profile: crate::session_profile::SessionProfile,
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
        // Graceful shutdown: SIGTERM first (lets Chrome flush
        // localStorage/cookies to the profile), then SIGKILL
        // after 3s if it hasn't exited. On Windows,
        // TerminateProcess directly.
        platform::shutdown_child(&mut self.child);
        // Xvfb is dropped here too (field order: child first, then xvfb).
    }
}

impl Browser {
    /// Graceful shutdown: kill Chrome (Drop), then sync the
    /// session profile back to the template and remove it.
    /// Call this on every deliberate teardown path (stdin EOF,
    /// signal, idle timeout). On SIGKILL nothing runs — the
    /// next launch's orphan reaper cleans up instead.
    pub fn shutdown(self) {
        let profile_dir = self.profile.dir().to_path_buf();
        drop(self); // kills Chrome + Xvfb
                    // Chrome is dead — the profile is flushed and safe to sync.
        crate::session_profile::SessionProfile::cleanup_dir(&profile_dir);
    }
}
