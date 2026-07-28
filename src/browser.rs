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
//! signals at the root (real CSS rendering, real GPU, real UA in workers).
//! If Xvfb isn't available, falls back to `--headless=new`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{BladeError, Result};
use crate::platform;

/// Flags applied to every Chrome launch (both headless and headful).
const STEALTH_FLAGS: &[&str] = &[
    "--no-sandbox",
    "--disable-extensions",
    "--no-first-run",
    "--disable-blink-features=AutomationControlled",
    "--disable-dev-shm-usage",
    "--disable-background-networking",
    "--disable-sync",
    // S17: force WebRTC to only use proxied UDP — prevents ICE candidate
    // leaks of the real IP when a proxy is active. No effect without proxy.
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
];

/// Additional flags for headless mode only.
const HEADLESS_FLAGS: &[&str] = &["--headless=new", "--disable-gpu"];

/// A launched Chrome process + optional virtual display. Both killed on Drop.
pub struct Browser {
    child: Child,
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    xvfb: Option<VirtualDisplay>,
    port: u16,
}

/// A virtual X display managed by Xvfb. Killed + cleaned up on Drop.
/// Linux-only: macOS and Windows have native window servers.
#[cfg(target_os = "linux")]
pub struct VirtualDisplay {
    child: Child,
    display_num: u16,
}

#[cfg(target_os = "linux")]
impl VirtualDisplay {
    /// Start an Xvfb virtual display on a free display number.
    /// Returns the display number (e.g., 99 for `:99`).
    fn start() -> Result<Self> {
        let xvfb_path = find_xvfb().ok_or_else(|| {
            BladeError::Other("Xvfb not found".into())
        })?;
        let display_num = free_display_num();

        let child = Command::new(&xvfb_path)
            .args([
                &format!(":{display_num}"),
                "-screen", "0", "1920x1080x24",
                "-ac",           // disable access control (headless server)
                "-nolisten", "tcp",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| BladeError::Other(format!("failed to launch Xvfb: {e}")))?;

        eprintln!("[bladebro] Xvfb virtual display on :{display_num}");

        Ok(Self { child, display_num })
    }

    fn display_env(&self) -> String {
        format!(":{}", self.display_num)
    }
}

#[cfg(target_os = "linux")]
impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Clean up the lock file.
        let lock = format!("/tmp/.X{}-lock", self.display_num);
        let _ = std::fs::remove_file(lock);
    }
}

impl Browser {
    /// Find Chrome, launch it with stealth flags on `port` (0 = auto-pick a
    /// free port), and wait for the CDP debug endpoint to respond.
    ///
    /// Linux: Xvfb headful if available, headless fallback.
    /// macOS/Windows: headful natively (native window server).
    pub async fn launch(port: u16) -> Result<Self> {
        let chrome_path = find_chrome()?;
        let port = if port == 0 { free_port() } else { port };
        let user_data_dir = profile_dir();
        std::fs::create_dir_all(&user_data_dir)
            .map_err(|e| BladeError::Other(format!("cannot create profile dir: {e}")))?;
        clear_stale_profile_lock(&user_data_dir)?;
        font_audit();

        #[cfg(target_os = "linux")]
        let xvfb = VirtualDisplay::start().ok();
        #[cfg(target_os = "linux")]
        let headful = xvfb.is_some();

        #[cfg(not(target_os = "linux"))]
        let headful = true; // macOS/Windows have native window servers

        let mut args: Vec<String> = STEALTH_FLAGS.iter().map(|s| s.to_string()).collect();
        if !headful {
            args.extend(HEADLESS_FLAGS.iter().map(|s| s.to_string()));
        }
        args.push(format!("--remote-debugging-port={port}"));
        args.push(format!("--user-data-dir={}", user_data_dir.display()));

        // M18: Proxy support via BLADE_PROXY env var.
        if let Ok(proxy) = std::env::var("BLADE_PROXY") {
            if !proxy.is_empty() {
                args.push(format!("--proxy-server={proxy}"));
                eprintln!("[bladebro] using proxy: {proxy}");
            }
        }

        // Set a realistic window size for headful mode.
        if headful {
            args.push("--window-size=1920,1080".into());
        }

        #[cfg(target_os = "linux")]
        let mode_str = if headful { "headful (Xvfb)" } else { "headless" };
        #[cfg(not(target_os = "linux"))]
        let mode_str = "headful";
        eprintln!(
            "[bladebro] launching Chrome from {chrome_path} on port {port} ({mode_str})"
        );

        let mut cmd = Command::new(&chrome_path);
        cmd.args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Set DISPLAY env var for headful mode (Linux only).
        #[cfg(target_os = "linux")]
        if let Some(ref xvfb) = xvfb {
            cmd.env("DISPLAY", xvfb.display_env());
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| BladeError::Other(format!("failed to launch Chrome: {e}")))?;

        let base = format!("127.0.0.1:{port}");

        // Poll the debug endpoint until it responds or we time out.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match crate::cdp::version(&base).await {
                Ok(v) => {
                    eprintln!(
                        "[bladebro] Chrome ready: {} (protocol {})",
                        v.browser, v.protocol_version
                    );
                    return Ok(Self {
                        child,
                        #[cfg(target_os = "linux")]
                        xvfb,
                        port,
                    });
                }
                Err(_) => {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            return Err(BladeError::Other(format!(
                                "Chrome exited during startup: {status}"
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
                        return Err(BladeError::Other(
                            "Chrome debug endpoint not responding after 20s".into(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }

    /// The port Chrome's debug endpoint is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// `host:port` string for CDP HTTP discovery calls.
    pub fn base(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Graceful shutdown: SIGTERM first (lets Chrome flush
        // localStorage/cookies to the persistent profile), then
        // SIGKILL after 3s if it hasn't exited. On Windows,
        // TerminateProcess directly.
        platform::shutdown_child(&mut self.child);
        // Xvfb is dropped here too (field order: child first, then xvfb).
    }
}

/// S7: the seasoned persistent profile. Defaults to `~/.blade/profile` so
/// cookies, history, cache, and service workers accumulate across runs —
/// returning-visitor trust instead of a newborn profile every launch.
/// `BLADE_FRESH=1` forces an ephemeral temp profile; `BLADE_PROFILE_DIR`
/// overrides the location entirely.
fn profile_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("BLADE_PROFILE_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    if std::env::var("BLADE_FRESH").map(|v| v == "1").unwrap_or(false) {
        return std::env::temp_dir().join(format!("bladebro-chrome-{}", std::process::id()));
    }
    let home = platform::blade_dir();
    home.join("profile")
}

/// Remove Chrome's SingletonLock/SingletonSocket/SingletonCookie when the
/// pid they point at is dead (stale after a crash). Errors out when another
/// live process holds the profile — corrupting a live profile is worse.
fn clear_stale_profile_lock(dir: &std::path::Path) -> Result<()> {
    let lock = dir.join("SingletonLock");
    let target = match std::fs::read_link(&lock) {
        Ok(t) => t,
        Err(_) => return Ok(()), // no lock, or not a symlink — nothing to do
    };
    // Lock target looks like "hostname-12345".
    let pid = target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|p| p.parse::<u32>().ok());
    let pid_alive = pid.map(platform::process_alive).unwrap_or(false);

    if pid_alive {
        let pid = pid.unwrap();
        // Check if the PID is actually a Chrome process (PID could have been recycled).
        if platform::process_is_chrome(pid) {
            // Previous Chrome is still dying (orphaned by SIGKILL of the MCP server).
            // Send SIGTERM and wait up to 2s for it to exit.
            platform::kill_process_graceful(pid);
            for _ in 0..20 {
                if !platform::process_alive(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if platform::process_alive(pid) {
                // Still alive after SIGTERM — escalate to SIGKILL.
                platform::kill_process_force(pid);
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        // If PID recycled (not Chrome) or Chrome is now dead, the lock is stale → clear it.
        // If Chrome is STILL alive (refused to die), only then error.
        if platform::process_alive(pid) && platform::process_is_chrome(pid) {
            return Err(BladeError::Other(format!(
                "profile {} is locked by a live Chrome (another bladebro running?). \
                 Use BLADE_PROFILE_DIR for a separate profile or BLADE_FRESH=1 for ephemeral.",
                dir.display()
            )));
        }
    }
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    eprintln!("[bladebro] cleared stale profile lock in {}", dir.display());
    Ok(())
}

/// S15: warn when no emoji font is installed. Kasada/Akamai render emoji on
/// hidden canvases and hash the pixels; a missing emoji font produces a hash
/// no real browser generates. Linux-only (fc-list). Best-effort, never fatal.
fn font_audit() {
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
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use tokio::net::unix::pipe;

        let chrome_path = find_chrome()?;
        let user_data_dir = profile_dir();
        std::fs::create_dir_all(&user_data_dir)
            .map_err(|e| BladeError::Other(format!("cannot create profile dir: {e}")))?;
        clear_stale_profile_lock(&user_data_dir)?;
        font_audit();

        #[cfg(target_os = "linux")]
        let xvfb = VirtualDisplay::start().ok();
        #[cfg(target_os = "linux")]
        let headful = xvfb.is_some();

        #[cfg(not(target_os = "linux"))]
        let headful = true; // macOS/Windows have native window servers

        let mut args: Vec<String> = STEALTH_FLAGS.iter().map(|s| s.to_string()).collect();
        if !headful {
            args.extend(HEADLESS_FLAGS.iter().map(|s| s.to_string()));
        }
        args.push("--remote-debugging-pipe".into());
        args.push(format!("--user-data-dir={}", user_data_dir.display()));

        if let Ok(proxy) = std::env::var("BLADE_PROXY") {
            if !proxy.is_empty() {
                args.push(format!("--proxy-server={proxy}"));
                eprintln!("[bladebro] using proxy: {proxy}");
            }
        }
        if headful {
            args.push("--window-size=1920,1080".into());
        }

        // Pipe pairs: out = us→chrome (chrome reads fd 3), in = chrome→us
        // (chrome writes fd 4). We keep out_tx/in_rx; the child-side ends
        // become fds 3/4 in the child via pre_exec dup2.
        let (out_tx, out_rx) = pipe::pipe().map_err(|e| BladeError::Other(format!("pipe create: {e}")))?;
        let (in_tx, in_rx) = pipe::pipe().map_err(|e| BladeError::Other(format!("pipe create: {e}")))?;

        // Child-side ends: blocking fds (Chrome does blocking IO on 3/4).
        let child_read_fd = out_rx
            .into_blocking_fd()
            .map_err(|e| BladeError::Other(format!("pipe fd: {e}")))?;
        let child_write_fd = in_tx
            .into_blocking_fd()
            .map_err(|e| BladeError::Other(format!("pipe fd: {e}")))?;
        if child_read_fd.as_raw_fd() <= 4 || child_write_fd.as_raw_fd() <= 4 {
            return Err(BladeError::Other(
                "pipe fds collided with stdio — set BLADE_TRANSPORT=ws to use the WebSocket transport".into(),
            ));
        }

        #[cfg(target_os = "linux")]
        let mode_str = if headful { "headful (Xvfb)" } else { "headless" };
        #[cfg(not(target_os = "linux"))]
        let mode_str = "headful";
        eprintln!("[bladebro] launching Chrome from {chrome_path} on CDP pipe ({mode_str})");

        let mut cmd = Command::new(&chrome_path);
        cmd.args(&args).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(target_os = "linux")]
        if let Some(ref xvfb) = xvfb {
            cmd.env("DISPLAY", xvfb.display_env());
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
                    let product = v.get("product").and_then(|p| p.as_str()).unwrap_or("unknown");
                    eprintln!("[bladebro] Chrome ready: {product} (pipe transport)");
                    return Ok((Self {
                        child,
                        #[cfg(target_os = "linux")]
                        xvfb,
                        port: 0,
                    }, client));
                }
                Err(_) => {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            return Err(BladeError::Other(format!("Chrome exited during startup: {status}")));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return Err(BladeError::Other(format!("failed to poll Chrome status: {e}")));
                        }
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(BladeError::Other("Chrome pipe not responding after 20s".into()));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }
}

/// Find the Xvfb binary on this system. Linux-only.
#[cfg(target_os = "linux")]
fn find_xvfb() -> Option<String> {
    // Common locations (NixOS system profile, standard paths).
    let candidates: &[&str] = &[
        "/run/current-system/sw/bin/Xvfb",
        "/usr/bin/Xvfb",
        "/usr/local/bin/Xvfb",
        "/opt/Xvfb/bin/Xvfb",
    ];
    for path in candidates {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    // Try PATH.
    find_in_path("Xvfb")
}

/// Find a free X display number. Checks for existing lock files. Linux-only.
#[cfg(target_os = "linux")]
fn free_display_num() -> u16 {
    for n in 99..200 {
        let lock = format!("/tmp/.X{n}-lock");
        if !std::path::Path::new(&lock).exists() {
            return n;
        }
    }
    99 // Fallback; collision unlikely.
}

/// Find the Chrome/Chromium binary on this system.
fn find_chrome() -> Result<String> {
    if let Ok(path) = std::env::var("CHROME_PATH") {
        if std::path::Path::new(&path).exists() {
            return Ok(path);
        }
    }

    let names = if cfg!(target_os = "macos") {
        &["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"][..]
    } else if cfg!(target_os = "windows") {
        &["chrome", "chromium"][..]
    } else {
        &["chromium", "google-chrome", "google-chrome-stable", "chromium-browser"][..]
    };
    for name in names {
        if let Some(path) = find_in_path(name) {
            return Ok(path);
        }
    }

    let paths = common_paths();
    for path in &paths {
        if std::path::Path::new(path).exists() {
            return Ok(path.to_string());
        }
    }

    if std::path::Path::new("/nix/store").exists() {
        if let Some(path) = find_in_nix_store() {
            return Ok(path);
        }
        if let Some(path) = find_via_nix_shell() {
            return Ok(path);
        }
    }

    Err(BladeError::Other(
        "Chrome/Chromium not found. Set CHROME_PATH env var, add chromium to PATH, or install Chrome.".into(),
    ))
}

fn common_paths() -> Vec<&'static str> {
    let mut paths = Vec::new();
    if cfg!(target_os = "linux") {
        paths.extend([
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
            "/opt/google/chrome/chrome",
            "/run/current-system/sw/bin/chromium",
            "/run/current-system/sw/bin/google-chrome",
        ]);
    }
    if cfg!(target_os = "macos") {
        paths.extend([
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/usr/local/bin/google-chrome",
            "/opt/homebrew/bin/chromium",
        ]);
    }
    if cfg!(target_os = "windows") {
        paths.extend([
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]);
    }
    paths
}

fn find_in_path(cmd: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.split(sep) {
        let full = std::path::Path::new(dir).join(cmd);
        if is_executable(&full) {
            return Some(full.to_string_lossy().to_string());
        }
        if cfg!(windows) {
            let with_exe = std::path::Path::new(dir).join(format!("{cmd}.exe"));
            if is_executable(&with_exe) {
                return Some(with_exe.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    if !path.exists() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_in_nix_store() -> Option<String> {
    if let Ok(output) = Command::new("fd")
        .args(["-t", "x", "-1", "chromium$", "/nix/store", "--max-depth", "3"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.ends_with("/bin/chromium") {
                    return Some(line.to_string());
                }
            }
        }
    }
    if let Ok(output) = Command::new("find")
        .args(["/nix/store", "-maxdepth", "3", "-name", "chromium", "-type", "f"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.ends_with("/bin/chromium") {
                return Some(line.to_string());
            }
        }
    }
    None
}

fn find_via_nix_shell() -> Option<String> {
    let output = Command::new("nix-shell")
        .args(["-p", "chromium", "--run", "which chromium"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !path.is_empty() && std::path::Path::new(&path).exists() {
        Some(path)
    } else {
        None
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(9222)
}
