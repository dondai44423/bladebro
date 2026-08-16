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
/// SECURITY: the Chrome renderer sandbox stays ON. `--no-sandbox` used to
/// be unconditional — any renderer exploit on a hostile page escaped straight
/// into the user's account. The flag is now added only as an automatic
/// fallback when sandboxed startup fails (root, restrictive containers).
const STEALTH_FLAGS: &[&str] = &[
    "--disable-extensions",
    "--no-first-run",
    "--disable-blink-features=AutomationControlled",
    "--disable-dev-shm-usage",
    "--disable-background-networking",
    "--disable-sync",
    // Allow window.open popups (OAuth, payment flows). Without this,
    // Chrome blocks popups and the agent gets no feedback.
    "--disable-popup-blocking",
    // Download PDFs instead of opening in Chrome's viewer.
    // Without this, PDF URLs open inline and can't be captured as downloads.
    "--disable-features=PdfPlugin",
    // S17: force WebRTC to only use proxied UDP — prevents ICE candidate
    // leaks of the real IP when a proxy is active. No effect without proxy.
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    // Issue #9: macOS shows a system dialog "Chrome Helper needs to download
    // the font 'Osaka'/'STHeiti'" when on-demand CJK fonts are requested by
    // page CSS but not installed. --disable-remote-fonts prevents Chrome from
    // requesting non-local fonts, using fallbacks instead. Also applies to
    // worker contexts per Chromium docs.
    "--disable-remote-fonts",
];

/// Additional flags for headless mode only.
const HEADLESS_FLAGS: &[&str] = &["--headless=new", "--disable-gpu"];

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
    ///
    /// Race-free display selection: an atomic claim file
    /// (`/tmp/.blade-x<n>-claim`, O_EXCL create) marks the
    /// display as OURS before Xvfb even spawns. Timing-based
    /// verification ("is Xvfb still alive after 200ms") was
    /// not enough — a losing Xvfb can take >200ms to exit,
    /// letting two bladebros share one display; when the
    /// owner exits, the survivor's Chrome loses its display
    /// and dies (observed live: SIGTERM on session A killed
    /// session B's Chrome via a shared Xvfb).
    fn start() -> Result<Self> {
        let xvfb_path = find_xvfb().ok_or_else(|| {
            BladeError::Other("Xvfb not found".into())
        })?;
        let mut last_err = String::new();
        for _attempt in 0..3 {
            let Some(display_num) = claim_display_num() else {
                last_err = "no free display claim".into();
                break;
            };
            let child = Command::new(&xvfb_path)
                .args([
                    &format!(":{display_num}"),
                    "-screen", "0", "1920x1080x24",
                    "-ac",           // disable access control (headless server)
                    "-nolisten", "tcp",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    release_display_claim(display_num);
                    last_err = format!("spawn: {e}");
                    continue;
                }
            };
            // Verify survival: Xvfb exits when the display is
            // held by a FOREIGN X server (the user's real one).
            // Our claim prevents bladebro-vs-bladebro races;
            // this check catches foreign owners.
            std::thread::sleep(Duration::from_millis(300));
            match child.try_wait() {
                Ok(None) => {
                    eprintln!("[bladebro] Xvfb virtual display on :{display_num}");
                    return Ok(Self { child, display_num });
                }
                Ok(Some(status)) => {
                    last_err = format!("Xvfb :{display_num} exited ({status})");
                    eprintln!("[bladebro] Xvfb :{display_num} died ({status}), retrying");
                    release_display_claim(display_num);
                }
                Err(e) => {
                    release_display_claim(display_num);
                    last_err = format!("poll: {e}");
                }
            }
        }
        Err(BladeError::Other(format!("Xvfb failed after 3 attempts: {last_err}")))
    }

    fn display_env(&self) -> String {
        format!(":{}", self.display_num)
    }
}

/// Atomically claim a free display number via O_EXCL file
/// creation. Returns None when 99..200 are all claimed.
#[cfg(target_os = "linux")]
fn claim_display_num() -> Option<u16> {
    for n in 99..200 {
        // Skip displays with a live foreign X server lock.
        if std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists() {
            continue;
        }
        let claim = format!("/tmp/.blade-x{n}-claim");
        if std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&claim)
            .and_then(|mut f| {
                use std::io::Write;
                write!(f, "{}", std::process::id())
            })
            .is_ok()
        {
            return Some(n);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn release_display_claim(display_num: u16) {
    let _ = std::fs::remove_file(format!("/tmp/.blade-x{display_num}-claim"));
}

#[cfg(target_os = "linux")]
impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Clean up the lock file + our claim.
        let lock = format!("/tmp/.X{}-lock", self.display_num);
        let _ = std::fs::remove_file(lock);
        release_display_claim(self.display_num);
    }
}

/// Point a Chrome child command at the Xvfb display and pin it
/// to X11. On Wayland sessions, Chrome would otherwise inherit
/// WAYLAND_DISPLAY and open on the user's real screen.
#[cfg(target_os = "linux")]
fn apply_xvfb_env(cmd: &mut Command, xvfb: &VirtualDisplay) {
    cmd.env("DISPLAY", xvfb.display_env());
    cmd.env_remove("WAYLAND_DISPLAY");
    cmd.env_remove("XDG_SESSION_TYPE");
    cmd.arg("--ozone-platform=x11");
}

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
        let auto = port == 0;
        let mut last_err = None;
        // Attempt sequence: sandboxed first; then --no-sandbox (sandbox
        // unavailable: root, restricted container); then one more fresh-port
        // retry with --no-sandbox (port-steal race). Only fast startup
        // exits are retried — endpoint timeouts abort immediately (same
        // policy as before; avoids tripling a 20s wait).
        let attempts: [(u8, bool); 3] = [(0, false), (1, true), (2, true)];
        for (attempt, no_sandbox) in attempts {
            let p = if auto { free_port() } else { port };
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

    async fn launch_inner(port: u16, no_sandbox: bool) -> Result<Self> {
        let chrome_path = find_chrome()?;
        let profile = crate::session_profile::SessionProfile::create()?;
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
        let headful = xvfb.is_some();

        #[cfg(not(target_os = "linux"))]
        let headful = true; // macOS/Windows have native window servers

        let mut args: Vec<String> = STEALTH_FLAGS.iter().map(|s| s.to_string()).collect();
        if no_sandbox {
            args.push("--no-sandbox".into());
        }
        if !headful {
            args.extend(HEADLESS_FLAGS.iter().map(|s| s.to_string()));
        }
        // On Xvfb there is no GPU: Chrome 139+ refuses software
        // WebGL unless this flag is set (contexts return null).
        // The stealth GL spoof masks the SwiftShader strings, so
        // pages see a coherent hardware identity either way.
        #[cfg(target_os = "linux")]
        if headful {
            args.push("--enable-unsafe-swiftshader".into());
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
                        profile,
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

    #[cfg(unix)]
    async fn launch_pipe_inner(no_sandbox: bool) -> Result<(Self, crate::cdp::CdpClient)> {
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

        let mut args: Vec<String> = STEALTH_FLAGS.iter().map(|s| s.to_string()).collect();
        if no_sandbox {
            args.push("--no-sandbox".into());
        }
        if !headful {
            args.extend(HEADLESS_FLAGS.iter().map(|s| s.to_string()));
        }
        // On Xvfb there is no GPU: Chrome 139+ refuses software
        // WebGL unless this flag is set (contexts return null).
        // The stealth GL spoof masks the SwiftShader strings.
        #[cfg(target_os = "linux")]
        if headful {
            args.push("--enable-unsafe-swiftshader".into());
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
                    let product = v.get("product").and_then(|p| p.as_str()).unwrap_or("unknown");
                    eprintln!("[bladebro] Chrome ready: {product} (pipe transport)");
                    return Ok((Self {
                        child,
                        #[cfg(target_os = "linux")]
                        xvfb,
                        port: 0,
                        profile,
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

    // Windows per-user install (default when installing without admin rights).
    #[cfg(target_os = "windows")]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = format!("{local}\\Google\\Chrome\\Application\\chrome.exe");
        if std::path::Path::new(&p).exists() {
            return Ok(p);
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
