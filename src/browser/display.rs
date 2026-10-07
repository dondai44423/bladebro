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
    /// Per-session Xauthority file (0600, O_EXCL). Xvfb runs with
    /// `-auth <file>` instead of `-ac`: with access control disabled, any
    /// local co-user could connect to `/tmp/.X11-unix/X<n>` (or its abstract
    /// twin, which carries no permission bits at all) and read every
    /// rendered page / inject XTEST input into the live browser.
    auth_file: std::path::PathBuf,
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
    ///
    /// SECURITY: the display is cookie-authorized (`-auth`, never `-ac`)
    /// and ready only when the socket is served by the child we spawned —
    /// connect-success alone would accept a co-user's pre-placed listener
    /// in the world-writable `/tmp/.X11-unix` (see `xvfb_socket_owned_by`).
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
            // SECURITY: authorize the display with a fresh per-session
            // MIT-MAGIC-COOKIE-1 instead of `-ac`. `-ac` disabled ALL
            // access control: on a multi-user host any local user could
            // attach to the display and capture/inject at will. The cookie
            // file is 0600 and O_EXCL-created under an unpredictable name;
            // only children of this process (handed XAUTHORITY below) can
            // open the display.
            let Some(auth_file) = write_xvfb_auth_file(display_num) else {
                release_display_claim(display_num);
                last_err = "cannot write Xauthority file".into();
                continue;
            };
            let auth_arg = auth_file.to_string_lossy().into_owned();
            let child = Command::new(xvfb_path)
                .args([
                    &format!(":{display_num}"),
                    "-screen",
                    "0",
                    &format!("{XVFB_SCREEN_WIDTH}x{XVFB_SCREEN_HEIGHT}x24"),
                    "-auth",
                    &auth_arg, // cookie-only access control (never -ac)
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
                    let _ = std::fs::remove_file(&auth_file);
                    last_err = format!("spawn: {e}");
                    continue;
                }
            };
            // Readiness: the X socket accepts connections as soon as the
            // server is up (tens of ms), but connect-success on a
            // predictable path in the world-writable /tmp/.X11-unix proves
            // only that SOMETHING listens — a co-user's pre-placed socket
            // satisfies it just as well as our child (whose own bind
            // failure is invisible: stdio → /dev/null). Require the peer
            // credentials of the connected listener to be the child we
            // spawned, on BOTH the pathname and abstract sockets (Chrome
            // tries the abstract name first), and check survival on a
            // 25ms cadence. Deadline expiry is a FAILURE, never readiness.
            let ready_deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        last_err = format!("Xvfb :{display_num} exited ({status})");
                        eprintln!("[bladebro] Xvfb :{display_num} died ({status}), retrying");
                        release_display_claim(display_num);
                        let _ = std::fs::remove_file(&auth_file);
                        break;
                    }
                    Ok(None) => {
                        if xvfb_socket_owned_by(child.id(), display_num) {
                            let wm = spawn_session_chrome(display_num, &auth_file);
                            eprintln!("[bladebro] Xvfb virtual display on :{display_num}");
                            return Ok(Self {
                                child,
                                wm,
                                display_num,
                                auth_file,
                            });
                        }
                        if std::time::Instant::now() >= ready_deadline {
                            last_err = format!(
                                "Xvfb :{display_num} never served a socket owned by our process within 500ms"
                            );
                            let _ = child.kill();
                            let _ = child.wait();
                            release_display_claim(display_num);
                            let _ = std::fs::remove_file(&auth_file);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        release_display_claim(display_num);
                        let _ = std::fs::remove_file(&auth_file);
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
fn spawn_session_chrome(display_num: u16, xauth: &std::path::Path) -> Option<Child> {
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
            .env("XAUTHORITY", xauth)
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
            if wm_ready(display_num, xauth) && set_work_area(display_num, xauth) {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !done {
            // A WM that never publishes the atom: try once blindly — the
            // read-back inside set_work_area is still the instrument.
            let _ = set_work_area(display_num, xauth);
        }
    }
    wm
}

/// True once a window manager owns the root window (`_NET_SUPPORTING_WM_CHECK`
/// resolves to a window). True when `xprop` is unavailable — `set_work_area`'s
/// own read-back is then the fallback instrument, as before.
#[cfg(target_os = "linux")]
fn wm_ready(display_num: u16, xauth: &std::path::Path) -> bool {
    match Command::new("xprop")
        .args(["-root", "-notype", "_NET_SUPPORTING_WM_CHECK"])
        .env("DISPLAY", format!(":{display_num}"))
        .env("XAUTHORITY", xauth)
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
fn set_work_area(display_num: u16, xauth: &std::path::Path) -> bool {
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
        .env("XAUTHORITY", xauth)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(set, Ok(s) if s.success()) {
        return true;
    }
    let out = Command::new("xprop")
        .args(["-display", &disp, "-root", "-notype", "_NET_WORKAREA"])
        .env("XAUTHORITY", xauth)
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

/// True when the display's X sockets are served by the process `child_pid`:
/// the pathname socket `/tmp/.X11-unix/X<n>` must report OUR child via
/// SO_PEERCRED, and the abstract twin (`\0/tmp/.X11-unix/X<n>`, which Chrome
/// tries FIRST) must either be absent or served by our child too. This is
/// the ownership proof the readiness gate needs: ANY listener in the
/// world-writable /tmp/.X11-unix makes a bare connect() succeed.
#[cfg(target_os = "linux")]
fn xvfb_socket_owned_by(child_pid: u32, display_num: u16) -> bool {
    let path_owned =
        std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{display_num}"))
            .ok()
            .and_then(|s| socket_peer_pid(&s))
            .is_some_and(|peer| peer == child_pid);
    if !path_owned {
        return false;
    }
    let abs_name = format!("\0/tmp/.X11-unix/X{display_num}");
    match connect_abstract(abs_name.as_bytes()) {
        Ok(s) => socket_peer_pid(&s).is_some_and(|peer| peer == child_pid),
        Err(_) => true, // no abstract listener — the pathname socket is the only channel
    }
}

/// Peer pid of a connected AF_UNIX stream (SO_PEERCRED): for a client, the
/// credentials of the process that created the listening socket — the only
/// trustworthy answer to "is this display ours?" in a world-writable
/// directory.
#[cfg(target_os = "linux")]
fn socket_peer_pid(sock: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut creds = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut creds as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        Some(creds.pid as u32)
    } else {
        None
    }
}

/// Connect to a Linux abstract-namespace unix socket by name (the leading
/// NUL selects the abstract namespace, which has no filesystem permission
/// checks). std's `UnixStream::connect` needs a real path, so this builds
/// the sockaddr directly.
#[cfg(target_os = "linux")]
fn connect_abstract(name: &[u8]) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::FromRawFd;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let cap = addr.sun_path.len();
        if name.len() > cap {
            libc::close(fd);
            return Err(std::io::Error::other("abstract socket name too long"));
        }
        for (i, b) in name.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + name.len()) as libc::socklen_t;
        if libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        ) != 0
        {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }
        Ok(std::os::unix::net::UnixStream::from_raw_fd(fd))
    }
}

/// Write a per-display Xauthority file holding a fresh MIT-MAGIC-COOKIE-1
/// (16 bytes from /dev/urandom): 0600, O_EXCL, unpredictable name — the
/// same scheme xvfb-run uses. Xvfb is started with `-auth <file>` so only
/// processes of this uid (handed the cookie via XAUTHORITY) can open the
/// display; a local co-user connecting to the unix or abstract socket is
/// rejected by the X server's access control.
#[cfg(target_os = "linux")]
fn write_xvfb_auth_file(display_num: u16) -> Option<std::path::PathBuf> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let mut cookie = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut cookie))
        .ok()?;
    let mut host_buf = [0 as libc::c_char; 256];
    let host = match unsafe { libc::gethostname(host_buf.as_mut_ptr(), host_buf.len()) } {
        0 => {
            let bytes: Vec<u8> = host_buf
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => "localhost".into(),
    };
    let disp = display_num.to_string();
    let name = b"MIT-MAGIC-COOKIE-1";
    // Xauthority record: 2-byte big-endian family, then (2-byte big-endian
    // length + bytes) for address, display number, auth name, auth data.
    // FamilyLocal + this host + "<n>" matches what xvfb-run's `xauth add`
    // stores and what libX11/libxcb (clients) and the X server look up.
    let mut rec = Vec::with_capacity(48 + host.len() + disp.len());
    rec.extend_from_slice(&256u16.to_be_bytes()); // FamilyLocal
    rec.extend_from_slice(&(host.len() as u16).to_be_bytes());
    rec.extend_from_slice(host.as_bytes());
    rec.extend_from_slice(&(disp.len() as u16).to_be_bytes());
    rec.extend_from_slice(disp.as_bytes());
    rec.extend_from_slice(&(name.len() as u16).to_be_bytes());
    rec.extend_from_slice(name);
    rec.extend_from_slice(&(cookie.len() as u16).to_be_bytes());
    rec.extend_from_slice(&cookie);
    // Unpredictable name (pid + clock) — a fixed name in /tmp would let a
    // co-user pre-create it and (best case) only DoS every launch.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::path::PathBuf::from(format!(
        "/tmp/.blade-x{display_num}-xauth-{}-{nanos}",
        std::process::id()
    ));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true) // O_EXCL — no pre-placement, no clobbering
        .write(true)
        .mode(0o600) // the cookie is readable by this uid only
        .open(&path)
        .ok()?;
    f.write_all(&rec).ok()?;
    Some(path)
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
        // Clean up the lock file + our claim + our Xauthority cookie.
        let lock = format!("/tmp/.X{}-lock", self.display_num);
        let _ = std::fs::remove_file(lock);
        let _ = std::fs::remove_file(&self.auth_file);
        release_display_claim(self.display_num);
    }
}

/// Point a Chrome child command at the Xvfb display and pin it
/// to X11. On Wayland sessions, Chrome would otherwise inherit
/// WAYLAND_DISPLAY and open on the user's real screen.
#[cfg(target_os = "linux")]
pub(super) fn apply_xvfb_env(cmd: &mut Command, xvfb: &VirtualDisplay) {
    cmd.env("DISPLAY", xvfb.display_env());
    // The display is cookie-authorized (`-auth`, never `-ac`): Chrome
    // presents the same MIT-MAGIC-COOKIE-1 to attach; without this it
    // would be rejected by our own access control.
    cmd.env("XAUTHORITY", &xvfb.auth_file);
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
        // The per-session Xauthority files must not outlive the attempts.
        let mine = format!("xauth-{}- ", std::process::id()).replace(' ', "");
        let leftovers: Vec<_> = std::fs::read_dir("/tmp")
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(".blade-x") && n.contains(&mine)
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "auth files must be cleaned on failure"
        );
    }

    #[test]
    fn auth_file_is_private_and_well_formed() {
        let path = write_xvfb_auth_file(199).expect("auth file");
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..2], &256u16.to_be_bytes(), "FamilyLocal");
        let mut o = 2usize;
        let mut rd = || -> Vec<u8> {
            let l = u16::from_be_bytes([bytes[o], bytes[o + 1]]) as usize;
            o += 2;
            let s = bytes[o..o + l].to_vec();
            o += l;
            s
        };
        let addr = rd();
        let disp = rd();
        let name = rd();
        let data = rd();
        assert!(!addr.is_empty(), "local hostname");
        assert_eq!(disp, b"199".to_vec());
        assert_eq!(name, b"MIT-MAGIC-COOKIE-1".to_vec());
        assert_eq!(data.len(), 16);
        assert_eq!(o, bytes.len(), "no trailing bytes");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn peer_credentials_match_the_listening_process() {
        let dir = std::env::temp_dir().join(format!("blade-peer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let client = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        assert_eq!(socket_peer_pid(&client), Some(std::process::id()));
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
