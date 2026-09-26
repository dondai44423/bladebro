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

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{BladeError, Result};
use crate::platform;
use std::sync::atomic::{AtomicBool, Ordering};
use serde_json::json;

/// Flags applied to every Chrome launch (both headless and headful).
/// SECURITY: the Chrome renderer sandbox stays ON. `--no-sandbox` used to
/// be unconditional — any renderer exploit on a hostile page escaped straight
/// into the user's account. The flag is now added only as an automatic
/// fallback when sandboxed startup fails (root, restrictive containers).
const STEALTH_FLAGS: &[&str] = &[
    "--disable-extensions",
    "--no-first-run",
    // (v3.9.12) --disable-blink-features=AutomationControlled REMOVED: on
    // Chrome 151 it is a no-op for navigator.webdriver unless the browser is
    // launched with --enable-automation (we never pass it) — verified
    // with/without in headful AND headless — and it triggers Chrome's
    // "unsupported command-line flag" infobar: a visible 56px in-window tell
    // that also skews innerHeight (caught by tools/diff_oracle).
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

/// GL backend ladder stage. The launch healthcheck walks these in order and
/// keeps the first stage that yields a live WebGL context; a stage that comes
/// up without one is shut down and the next is tried (bounded).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlStage {
    /// Native-GL ANGLE backend. Under Xvfb this lands on Mesa (llvmpipe —
    /// software, but a live context) which the stealth GL mask normalizes.
    NativeGl,
    /// SwiftShader ANGLE backend — last resort for stacks where the
    /// native-GL backend cannot initialize (observed live: SwANGLE
    /// `eglInitialize` failing with a Vulkan init error is the *default*
    /// ANGLE choice on some Mesa builds).
    SwiftShader,
}

/// CDP transport — the ONLY difference a launcher is allowed to make
/// between the WS and pipe command lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Ws,
    Pipe,
}

/// Launch-time GL healthcheck result. Consumed by `stealth::apply`: the
/// WebGL spoof registers only when the real backend is software (D14 —
/// coherence over noise; a real GPU is reported honestly, nothing to mask).
#[derive(Clone, Debug)]
pub enum GpuState {
    Hardware(String),
    Software(String),
    /// No WebGL context after the whole ladder — inert spoof, loud warning.
    Missing,
}

/// Launch-time GL state, read by `stealth::apply` at attach time.
static GPU_STATE: std::sync::RwLock<Option<GpuState>> = std::sync::RwLock::new(None);

/// Record the GL healthcheck result (called by the launch paths).
pub fn set_gpu_state(state: Option<GpuState>) {
    if let Ok(mut g) = GPU_STATE.write() {
        *g = state;
    }
}

/// The launch-time GL state, if a healthcheck ran in this process.
pub fn gpu_state() -> Option<GpuState> {
    match GPU_STATE.read() {
        Ok(g) => g.clone(),
        Err(_) => None,
    }
}

/// Launch-mode flag: true when this process launched the browser with
/// `--headless=new` (no virtual display available). The stealth layer uses it
/// to gate environment-specific masks that are only *needed* in headless
/// mode — e.g. the notifications permission relay (headless-New reports
/// 'denied' on real origins; a headful lane reports the honest 'prompt',
/// verified against stock on the same display). Unknown (external attach)
/// reads as false = honest: never lie to a browser we did not launch.
static LAUNCH_HEADLESS: AtomicBool = AtomicBool::new(false);

/// Record whether the launch fell back to `--headless=new`.
pub fn set_launched_headless(headless: bool) {
    LAUNCH_HEADLESS.store(headless, Ordering::Relaxed);
}

/// True when this process launched a headless (`--headless=new`) browser.
pub fn launched_headless() -> bool {
    LAUNCH_HEADLESS.load(Ordering::Relaxed)
}

/// Launch-transport flag: true when the browser was launched with
/// `--remote-debugging-pipe`. Chrome treats that transport as its automation
/// transport and enables the blink AutomationControlled feature, so
/// `navigator.webdriver` is `true` on this lane while the WS lane reports
/// `false` (measured on Chrome 151, both transports, stock control). The
/// stealth layer masks it there — see `WEBDRIVER_PATCH`.
static LAUNCH_PIPE: AtomicBool = AtomicBool::new(false);

/// Record whether the launch used the pipe transport.
pub fn set_launched_pipe(pipe: bool) {
    LAUNCH_PIPE.store(pipe, Ordering::Relaxed);
}

/// True when this process launched the browser over `--remote-debugging-pipe`.
pub fn launched_pipe() -> bool {
    LAUNCH_PIPE.load(Ordering::Relaxed)
}

/// True for software/headless renderer artifacts (llvmpipe, SwiftShader,
/// softpipe). Single definition shared with `stealth::inject` so the launch
/// healthcheck and the spoof decision can never disagree.
pub fn is_software_renderer(renderer: &str) -> bool {
    let r = renderer.to_lowercase();
    r.contains("swiftshader") || r.contains("llvmpipe") || r.contains("softpipe") || r.contains("software")
}

/// Classify a live renderer string into the launch-time [`GpuState`].
fn classify_gl(renderer: &str) -> GpuState {
    if is_software_renderer(renderer) {
        GpuState::Software(renderer.to_string())
    } else {
        GpuState::Hardware(renderer.to_string())
    }
}

/// Human label for a ladder stage (logs).
fn stage_label(stage: GlStage) -> &'static str {
    match stage {
        GlStage::NativeGl => "native-gl",
        GlStage::SwiftShader => "swiftshader",
    }
}

/// The GL ladder, in attempt order. Both transports walk the same list.
fn gl_stages() -> &'static [GlStage] {
    &[GlStage::NativeGl, GlStage::SwiftShader]
}

/// Everything a Chrome command line depends on. Both transports build from
/// [`launch_args`] — the v3.9.11 pipe path silently missing
/// `--ignore-gpu-blocklist` is exactly the class of bug this prevents:
/// Chrome then answers "WebGL{1,2} blocklisted" and every context is null
/// (stealth was fine; the launch wasn't).
struct LaunchCfg<'a> {
    stage: GlStage,
    headful: bool,
    no_sandbox: bool,
    transport: Transport,
    port: u16,
    user_data_dir: &'a std::path::Path,
    proxy: Option<&'a str>,
    extra: &'a [String],
}

/// The single source of truth for a Chrome command line. Pure — unit tests
/// lock the WS/pipe delta to exactly the transport flag.
fn launch_args(cfg: &LaunchCfg<'_>) -> Vec<String> {
    let mut args: Vec<String> = STEALTH_FLAGS.iter().map(|s| s.to_string()).collect();
    if cfg.no_sandbox {
        args.push("--no-sandbox".into());
    }
    if !cfg.headful {
        args.extend(HEADLESS_FLAGS.iter().map(|s| s.to_string()));
    }
    // Chrome 139+ only hands out software WebGL contexts when this is set
    // (hardware GL is unaffected). Without it, GPU-less environments get
    // `getContext('webgl') === null` — the loudest automation tell there is.
    args.push("--enable-unsafe-swiftshader".into());
    // Software/virtual renderers (llvmpipe, SwiftShader) sit on Chrome's GPU
    // blocklist; without the override Chrome reports "WebGL1/2 blocklisted"
    // and contexts are null. The override turns a dead GL stack into a
    // working, mask-normalized one. BOTH transports must carry it.
    args.push("--ignore-gpu-blocklist".into());
    #[cfg(target_os = "linux")]
    if cfg.headful {
        // Pin the window to the Xvfb display (the caller strips Wayland env
        // first). Explicit so Chrome can never drift onto the user's real
        // session via platform auto-detection.
        args.push("--ozone-platform=x11".into());
    }
    match cfg.stage {
        GlStage::NativeGl => {
            #[cfg(target_os = "linux")]
            if cfg.headful {
                args.push("--use-angle=gl".into());
            }
        }
        GlStage::SwiftShader => {
            args.push("--use-angle=swiftshader".into());
        }
    }
    match cfg.transport {
        Transport::Ws => args.push(format!("--remote-debugging-port={}", cfg.port)),
        Transport::Pipe => args.push("--remote-debugging-pipe".into()),
    }
    args.push(format!("--user-data-dir={}", cfg.user_data_dir.display()));
    if let Some(proxy) = cfg.proxy {
        args.push(format!("--proxy-server={proxy}"));
    }
    if cfg.headful {
        args.push("--window-size=1920,1080".into());
    }
    args.extend(cfg.extra.iter().cloned());
    args
}

/// The healthcheck expression — evaluated in a normal document (the probe
/// navigates the target to about:blank first: the startup tab may be a
/// WebUI where canvas access can be restricted).
const GL_PROBE_EXPR: &str = "(function(){try{var c=document.createElement('canvas');var g=c.getContext('webgl');if(!g)return '';var e=g.getExtension('WEBGL_debug_renderer_info');return e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):String(g.getParameter(g.RENDERER));}catch(err){return ''}})()";

/// Healthcheck the WS transport: evaluate the GL probe in the first page
/// target with bounded retries. Returns the renderer string, or None when
/// no context ever appears.
async fn probe_gl_ws(base: &str) -> Option<String> {
    for _ in 0..3 {
        if let Some(renderer) = probe_gl_ws_once(base).await {
            return Some(renderer);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    None
}

async fn probe_gl_ws_once(base: &str) -> Option<String> {
    let target = crate::cdp::first_page_target(base).await.ok()?;
    let client = crate::cdp::CdpClient::connect(target.ws_url().ok()?).await.ok()?;
    let session = crate::cdp::CdpSession::root(client);
    let _ = session
        .send("Page.navigate", Some(json!({ "url": "about:blank" })))
        .await;
    for _ in 0..10 {
        if let Ok(v) = session
            .send(
                "Runtime.evaluate",
                Some(json!({ "expression": GL_PROBE_EXPR, "returnByValue": true })),
            )
            .await
        {
            if let Some(s) = v.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Healthcheck the pipe transport over the browser-level connection
/// (`Target.getTargets` → `attachToTarget` flatten → evaluate → detach).
#[cfg(unix)]
async fn probe_gl_pipe(client: &crate::cdp::CdpClient) -> Option<String> {
    for _ in 0..3 {
        if let Some(renderer) = probe_gl_pipe_once(client).await {
            return Some(renderer);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    None
}

#[cfg(unix)]
async fn probe_gl_pipe_once(client: &crate::cdp::CdpClient) -> Option<String> {
    let targets = client.send("Target.getTargets", None).await.ok()?;
    let empty = Vec::new();
    let infos = targets
        .get("targetInfos")
        .and_then(|t| t.as_array())
        .unwrap_or(&empty);
    let target_id = infos
        .iter()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .and_then(|t| t.get("targetId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    let res = client
        .send(
            "Target.attachToTarget",
            Some(json!({ "targetId": target_id, "flatten": true })),
        )
        .await
        .ok()?;
    let session_id = res.get("sessionId").and_then(|v| v.as_str())?.to_string();
    let session = crate::cdp::CdpSession::child(client.clone(), session_id.clone());
    let _ = session
        .send("Page.navigate", Some(json!({ "url": "about:blank" })))
        .await;
    let mut found = None;
    for _ in 0..10 {
        if let Ok(v) = session
            .send(
                "Runtime.evaluate",
                Some(json!({ "expression": GL_PROBE_EXPR, "returnByValue": true })),
            )
            .await
        {
            if let Some(s) = v.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    found = Some(s.to_string());
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = client
        .send(
            "Target.detachFromTarget",
            Some(json!({ "sessionId": session_id })),
        )
        .await;
    found
}

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
                    "-screen", "0", &format!("{XVFB_SCREEN_WIDTH}x{XVFB_SCREEN_HEIGHT}x24"),
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
                    let wm = spawn_session_chrome(display_num);
                    eprintln!("[bladebro] Xvfb virtual display on :{display_num}");
                    return Ok(Self { child, wm, display_num });
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
        std::thread::sleep(Duration::from_millis(400));
        for _ in 0..25 {
            if set_work_area(display_num) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    wm
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
fn apply_xvfb_env(cmd: &mut Command, xvfb: &VirtualDisplay) {
    cmd.env("DISPLAY", xvfb.display_env());
    cmd.env_remove("WAYLAND_DISPLAY");
    cmd.env_remove("XDG_SESSION_TYPE");
    // The --ozone-platform=x11 pin itself lives in `launch_args` (both
    // transports build from that single source).
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

    /// Launch with the GL ladder: walk `gl_stages()`, keep the first stage
    /// that yields a live WebGL context, and record the result for the
    /// stealth layer. A stage that comes up without GL is shut down and the
    /// next stage is tried (bounded — the ladder has two stages) so a
    /// GL-less browser is never what pages see.
    async fn launch_inner(port: u16, no_sandbox: bool) -> Result<Self> {
        let stages = gl_stages();
        let mut last: Option<Self> = None;
        for (i, stage) in stages.iter().enumerate() {
            if let Some(b) = last.take() {
                // No GL in the previous stage — tear it down before the
                // relaunch (fresh Xvfb + profile; the WS transport re-binds
                // the same port, so the old Chrome must be gone first).
                let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
            }
            let (browser, probe) = Self::launch_inner_stage(port, no_sandbox, *stage).await?;
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
                    return Ok(browser);
                }
                None => {
                    if i + 1 < stages.len() {
                        eprintln!(
                            "[bladebro] GL healthcheck: no WebGL context via {} — escalating to {}",
                            stage_label(*stage),
                            stage_label(stages[i + 1])
                        );
                        last = Some(browser);
                    } else {
                        eprintln!(
                            "[bladebro] WARNING: no WebGL context after the full GL ladder — \
                             pages will see `getContext('webgl') === null`. Run `bladebro audit`."
                        );
                        set_gpu_state(Some(GpuState::Missing));
                        return Ok(browser);
                    }
                }
            }
        }
        Err(BladeError::Other("no GL stages configured".into()))
    }

    /// One launch attempt with a pinned GL stage (driven by `launch_inner`).
    async fn launch_inner_stage(port: u16, no_sandbox: bool, stage: GlStage) -> Result<(Self, Option<String>)> {
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

        // After both bindings: the recording must compile on every target.
        set_launched_headless(!headful);

        // M18: Proxy support via BLADE_PROXY env var.
        let proxy = std::env::var("BLADE_PROXY").ok().filter(|p| !p.is_empty());
        if let Some(p) = &proxy {
            eprintln!("[bladebro] using proxy: {p}");
        }
        // Power-user escape hatch: append raw Chrome flags. Useful for
        // diagnosing GL/WebGL backend issues on odd displays and for
        // users who need a specific Chromium switch. Whitespace-split.
        let extra: Vec<String> = std::env::var("BLADE_CHROME_FLAGS")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let args = launch_args(&LaunchCfg {
            stage,
            headful,
            no_sandbox,
            transport: Transport::Ws,
            port,
            user_data_dir: &user_data_dir,
            proxy: proxy.as_deref(),
            extra: &extra,
        });

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
                    let probe = probe_gl_ws(&base).await;
                    return Ok((
                        Self {
                            child,
                            #[cfg(target_os = "linux")]
                            xvfb,
                            port,
                            profile,
                        },
                        probe,
                    ));
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
                        eprintln!(
                            "[bladebro] GL healthcheck: no WebGL context via {} — escalating to {}",
                            stage_label(*stage),
                            stage_label(stages[i + 1])
                        );
                        last = Some((browser, client));
                    } else {
                        eprintln!(
                            "[bladebro] WARNING: no WebGL context after the full GL ladder — \
                             pages will see `getContext('webgl') === null`. Run `bladebro audit`."
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

        // M18: Proxy support via BLADE_PROXY env var.
        let proxy = std::env::var("BLADE_PROXY").ok().filter(|p| !p.is_empty());
        if let Some(p) = &proxy {
            eprintln!("[bladebro] using proxy: {p}");
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
            proxy: proxy.as_deref(),
            extra: &extra,
        });

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

#[cfg(test)]
mod launch_flag_tests {
    use super::*;
    use std::path::Path;

    const EMPTY_EXTRA: &[String] = &[];

    fn cfg(stage: GlStage, headful: bool, transport: Transport) -> LaunchCfg<'static> {
        LaunchCfg {
            stage,
            headful,
            no_sandbox: false,
            transport,
            port: 9222,
            user_data_dir: Path::new("/tmp/bb-launch-args-test"),
            proxy: None,
            extra: EMPTY_EXTRA,
        }
    }

    /// The v3.9.11 bug: the pipe path was missing `--ignore-gpu-blocklist` and
    /// Chrome answered "WebGL{1,2} blocklisted" with *null* contexts on every
    /// soft-GL display (stealth was fine; the launch wasn't). The stealth core
    /// must be identical across transports — always.
    #[test]
    fn both_transports_share_the_stealth_core() {
        let ws = launch_args(&cfg(GlStage::NativeGl, true, Transport::Ws));
        let pipe = launch_args(&cfg(GlStage::NativeGl, true, Transport::Pipe));
        let strip = |v: &[String]| -> Vec<String> {
            v.iter()
                .filter(|a| !a.starts_with("--remote-debugging"))
                .cloned()
                .collect()
        };
        assert_eq!(strip(&ws), strip(&pipe));
    }

    /// Both overrides are mandatory on every platform/stage/headful combo —
    /// without them GPU-less environments get null WebGL contexts.
    #[test]
    fn blocklist_and_swiftshader_overrides_are_always_present() {
        for stage in gl_stages() {
            for headful in [true, false] {
                for transport in [Transport::Ws, Transport::Pipe] {
                    let a = launch_args(&cfg(*stage, headful, transport));
                    assert!(
                        a.iter().any(|f| f == "--ignore-gpu-blocklist"),
                        "missing --ignore-gpu-blocklist for {stage:?} headful={headful}"
                    );
                    assert!(
                        a.iter().any(|f| f == "--enable-unsafe-swiftshader"),
                        "missing --enable-unsafe-swiftshader for {stage:?} headful={headful}"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_headful_pins_x11_and_the_stage_backend() {
        let native = launch_args(&cfg(GlStage::NativeGl, true, Transport::Pipe));
        assert!(native.iter().any(|f| f == "--ozone-platform=x11"));
        assert!(native.iter().any(|f| f == "--use-angle=gl"));
        let sw = launch_args(&cfg(GlStage::SwiftShader, true, Transport::Pipe));
        assert!(sw.iter().any(|f| f == "--ozone-platform=x11"));
        assert!(sw.iter().any(|f| f == "--use-angle=swiftshader"));
        assert!(!sw.iter().any(|f| f == "--use-angle=gl"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_never_pins_a_display_or_native_angle() {
        let a = launch_args(&cfg(GlStage::NativeGl, false, Transport::Ws));
        assert!(!a.iter().any(|f| f == "--ozone-platform=x11"));
        assert!(!a.iter().any(|f| f == "--use-angle=gl"));
        assert!(a.iter().any(|f| f == "--headless=new"));
    }

    #[test]
    fn ladder_order_is_native_first() {
        assert_eq!(gl_stages()[0], GlStage::NativeGl);
        assert!(gl_stages().len() >= 2);
    }

    #[test]
    fn classifier_flags_software_renders() {
        assert!(is_software_renderer(
            "ANGLE (Mesa, llvmpipe (LLVM 21.1.7 256 bits), OpenGL 4.6)"
        ));
        assert!(is_software_renderer(
            "ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero)), SwiftShader driver)"
        ));
        assert!(!is_software_renderer(
            "ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)"
        ));
        assert!(matches!(
            classify_gl("ANGLE (Mesa, llvmpipe (LLVM 21.1.7 256 bits), OpenGL 4.6)"),
            GpuState::Software(_)
        ));
        assert!(matches!(
            classify_gl("ANGLE (Intel, Mesa Intel(R) Graphics (ADL GT2), OpenGL ES 3.2)"),
            GpuState::Hardware(_)
        ));
    }

    #[test]
    fn window_size_and_user_data_are_shared() {
        let a = launch_args(&cfg(GlStage::NativeGl, true, Transport::Ws));
        assert!(a.iter().any(|f| f == "--window-size=1920,1080"));
        assert!(a.iter().any(|f| f.starts_with("--user-data-dir=")));
        let h = launch_args(&cfg(GlStage::NativeGl, false, Transport::Ws));
        assert!(!h.iter().any(|f| f == "--window-size=1920,1080"));
    }
}
