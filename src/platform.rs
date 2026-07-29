//! Cross-platform process management and paths.
//!
//! Every OS-specific operation lives here. The rest of the codebase
//! calls these helpers — no `#[cfg]` outside this module (except for
//! transport-level differences that can't be abstracted).

use std::path::PathBuf;
use std::process::Child;
#[cfg(unix)]
use std::time::Duration;

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\Users\\Default"))
    }
}

/// The Bladebro data directory (`~/.blade` on Unix, `%USERPROFILE%\.blade` on Windows).
pub fn blade_dir() -> PathBuf {
    home_dir().join(".blade")
}

/// Is this process alive?
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        // tasklist /FI "PID eq 1234" /NH — if the process exists, output
        // contains the PID; if not, output says "No tasks".
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                !out.contains("No tasks") && out.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

/// Is this process a Chrome/Chromium process?
pub fn process_is_chrome(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .contains("chrom")
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("chrom"))
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("chrome"))
            .unwrap_or(false)
    }
}

/// Send SIGTERM to a process by PID (Unix) or graceful-terminate (Windows).
pub fn kill_process_graceful(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        // Windows has no SIGTERM. /T kills the process tree, without /F
        // it's a graceful request (WM_CLOSE to console apps).
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .output();
    }
}

/// Send SIGKILL to a process by PID (Unix) or force-terminate (Windows).
pub fn kill_process_force(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Shut down a child process gracefully: SIGTERM first (Unix), then
/// SIGKILL after a grace period. On Windows, TerminateProcess directly.
pub fn shutdown_child(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        for _ in 0..30 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(windows)]
    {
        // Windows: no graceful signal. TerminateProcess is the only option.
        let _ = child.kill();
        let _ = child.wait();
    }
}
