//! Xvfb virtual display + window manager — the headful-on-a-virtual-display
//! foundation (Linux only; macOS/Windows use the native window server).

use super::discover::{find_in_path, find_xvfb};
use super::*;

/// A virtual X display managed by Xvfb. Killed + cleaned up on Drop.
/// Linux-only: macOS and Windows have native window servers.
#[cfg(target_os = "linux")]
pub struct VirtualDisplay {
    child: Child,
    /// Window manager on the virtual display, when available: real window
    /// decorations (`outerWidth > innerWidth`) and a declared work area
    /// (`availHeight < height`) so the JS geometry masks stay off. Absent when
    /// no WM binary is installed — the JS masks are then the fallback.
    wm: Option<Child>,
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
    pub(super) fn start() -> Result<Self> {
        let xvfb_path = find_xvfb().ok_or_else(|| BladeError::Other("Xvfb not found".into()))?;
        Self::start_with_path(&xvfb_path)
    }

    fn start_with_path(xvfb_path: &str) -> Result<Self> {
        let mut last_err = String::new();
        for _attempt in 0..3 {
            let Some(display_num) = claim_display_num() else {
                last_err = "no free display claim".into();
                break;
            };
            let child = Command::new(xvfb_path)
                .args([
                    &format!(":{display_num}"),
                    "-screen",
                    "0",
                    &format!("{XVFB_SCREEN_WIDTH}x{XVFB_SCREEN_HEIGHT}x24"),
                    "-ac", // disable access control (headless server)
                    "-nolisten",
                    "tcp",
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
            // Readiness is observable: the X socket accepts connections as
            // soon as the server is up (tens of ms). Poll it and check
            // survival on a 25ms cadence — a foreign owner (Xvfb exits
            // quickly) is caught just as well as with the old single 300ms
            // check, and the normal start no longer pays a flat 300ms. Cap
            // at 500ms so a pathological case still fails over.
            let sock = format!("/tmp/.X11-unix/X{display_num}");
            let ready_deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        last_err = format!("Xvfb :{display_num} exited ({status})");
                        eprintln!("[bladebro] Xvfb :{display_num} died ({status}), retrying");
                        release_display_claim(display_num);
                        break;
                    }
                    Ok(None) => {
                        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                            let wm = spawn_session_chrome(display_num);
                            eprintln!("[bladebro] Xvfb virtual display on :{display_num}");
                            return Ok(Self {
                                child,
                                wm,
                                display_num,
                            });
                        }
                        if std::time::Instant::now() >= ready_deadline {
                            last_err =
                                format!("Xvfb :{display_num} did not open its socket within 500ms");
                            let _ = child.kill();
                            let _ = child.wait();
                            release_display_claim(display_num);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        release_display_claim(display_num);
                        last_err = format!("poll: {e}");
                        break;
                    }
                }
            }
        }
        Err(BladeError::Other(format!(
            "Xvfb failed after 3 attempts: {last_err}"
        )))
    }

    fn display_env(&self) -> String {
        format!(":{}", self.display_num)
    }
}

/// Start a window manager on the virtual display and give the screen an
/// honest work area. An X screen with no WM has no work area at all:
/// `screen.availHeight` equals `screen.height`, `outerWidth` equals
/// `innerWidth` and windows carry no decorations — an Xvfb signature that
/// would otherwise have to be masked in JS (and every JS mask is a patched
/// function a lie engine can inspect). With a WM the decorations are real and
/// `_NET_WORKAREA` — which Chromium reads for `screen.availHeight`, the
/// `avail*` family and window maximization — declares a plausible 40px bottom
/// taskbar. No panel process is spawned: xfce4-panel is a per-user singleton
/// ("There is already a running instance") so a second lane cannot get a
/// strut from it, while the property belongs to no one once the WM is up.
/// Degrades silently when the tools are missing — the JS geometry masks are
/// then the fallback.
#[cfg(target_os = "linux")]
fn spawn_session_chrome(display_num: u16) -> Option<Child> {
    fn find_bin(name: &str, extra: &[&str]) -> Option<String> {
        for path in extra {
            if std::path::Path::new(path).exists() {
                return Some((*path).to_string());
            }
        }
        find_in_path(name)
    }
    let wm = find_bin("xfwm4", &["/usr/bin/xfwm4", "/usr/local/bin/xfwm4"]).and_then(|path| {
        let mut cmd = Command::new(path);
        cmd.args(["--compositor=off", "--replace"])
            .env("DISPLAY", format!(":{display_num}"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("XDG_SESSION_TYPE")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.spawn().ok()
    });
    // xfwm4 publishes the full screen first, then stops touching the property
    // (there are no struts to react to) — so declare the taskbar after a short
    // settle and verify the read-back before the caller launches Chrome, whose
    // window caches this geometry at creation time.
    if wm.is_some() {
        // Wait for the WM to own the root instead of a blind 400ms sleep,
        // then set the taskbar property and verify the read-back. xfwm4
        // publishes the full screen at startup and stops touching
        // _NET_WORKAREA once it is up — readiness is observable
        // (_NET_SUPPORTING_WM_CHECK), so poll it tightly (50ms) up to 3s.
        let mut done = false;
        for _ in 0..60 {
            if wm_ready(display_num) && set_work_area(display_num) {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !done {
            // A WM that never publishes the atom: try once blindly — the
            // read-back inside set_work_area is still the instrument.
            let _ = set_work_area(display_num);
        }
    }
    wm
}

/// True once a window manager owns the root window (`_NET_SUPPORTING_WM_CHECK`
/// resolves to a window). True when `xprop` is unavailable — `set_work_area`'s
/// own read-back is then the fallback instrument, as before.
#[cfg(target_os = "linux")]
fn wm_ready(display_num: u16) -> bool {
    match Command::new("xprop")
        .args(["-root", "-notype", "_NET_SUPPORTING_WM_CHECK"])
        .env("DISPLAY", format!(":{display_num}"))
        .output()
    {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            !s.is_empty() && !s.contains("no such atom")
        }
        Err(_) => true,
    }
}

/// Declare a 40px bottom taskbar on `_NET_WORKAREA` (x, y, w, h per desktop)
/// via `xprop -set`. Returns true once the property reads back with the
/// expected height, or when `xprop` is unavailable (the JS mask's own
/// self-correcting guard is then the fallback — never spin without an
/// instrument).
#[cfg(target_os = "linux")]
fn set_work_area(display_num: u16) -> bool {
    let disp = format!(":{display_num}");
    let want = XVFB_SCREEN_HEIGHT - 40;
    let value = format!("0, 0, {XVFB_SCREEN_WIDTH}, {want}");
    let set = Command::new("xprop")
        .args([
            "-display",
            &disp,
            "-root",
            "-f",
            "_NET_WORKAREA",
            "32c",
            "-set",
            "_NET_WORKAREA",
            &value,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(set, Ok(s) if s.success()) {
        return true;
    }
    let out = Command::new("xprop")
        .args(["-display", &disp, "-root", "-notype", "_NET_WORKAREA"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let nums: Vec<u64> = s
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|t| t.parse().ok())
                .collect();
            nums.len() >= 4 && nums.chunks(4).any(|c| c.len() == 4 && c[3] == want)
        }
        Err(_) => true,
    }
}

/// The virtual display's geometry (one source for the Xvfb args and the
/// work-area check).
#[cfg(target_os = "linux")]
const XVFB_SCREEN_WIDTH: u64 = 1920;
#[cfg(target_os = "linux")]
const XVFB_SCREEN_HEIGHT: u64 = 1080;

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
        if let Some(mut w) = self.wm.take() {
            let _ = w.kill();
            let _ = w.wait();
        }
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
pub(super) fn apply_xvfb_env(cmd: &mut Command, xvfb: &VirtualDisplay) {
    cmd.env("DISPLAY", xvfb.display_env());
    cmd.env_remove("WAYLAND_DISPLAY");
    cmd.env_remove("XDG_SESSION_TYPE");
    // The --ozone-platform=x11 pin itself lives in `launch_args` (both
    // transports build from that single source).
}

#[cfg(all(test, target_os = "linux"))]
mod readiness_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn a_live_process_without_a_display_socket_is_not_ready() {
        let dir = std::env::temp_dir().join(format!("blade-xvfb-ready-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-xvfb");
        let pids = dir.join("pids");
        std::fs::write(&script, format!("#!/usr/bin/env python3\nimport os,time\nwith open({:?},'a') as f:f.write(str(os.getpid())+'\\n')\ntime.sleep(60)\n", pids.to_string_lossy())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = VirtualDisplay::start_with_path(script.to_str().unwrap());
        let recorded = std::fs::read_to_string(&pids).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err(), "process survival is not display readiness");
        let pids: Vec<u32> = recorded.lines().map(|s| s.parse().unwrap()).collect();
        assert_eq!(
            pids.len(),
            3,
            "all bounded attempts reached the fake server"
        );
        assert!(
            pids.iter().all(|pid| !crate::platform::process_alive(*pid)),
            "timed-out servers must be reaped"
        );
    }
}
