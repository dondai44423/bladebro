//! Launch flags — the single source of truth for Chrome command lines on
//! both transports, plus the GL ladder/state the stealth layer reads.

use std::sync::atomic::{AtomicBool, Ordering};

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
pub(super) fn classify_gl(renderer: &str) -> GpuState {
    if is_software_renderer(renderer) {
        GpuState::Software(renderer.to_string())
    } else {
        GpuState::Hardware(renderer.to_string())
    }
}

/// Human label for a ladder stage (logs).
pub(super) fn stage_label(stage: GlStage) -> &'static str {
    match stage {
        GlStage::NativeGl => "native-gl",
        GlStage::SwiftShader => "swiftshader",
    }
}

/// The GL ladder, in attempt order. Both transports walk the same list.
/// NativeGl is retried once: a first-launch GL miss is usually a transient
/// init race under load (observed live), while the SwiftShader stage is a
/// dead end on boxes whose Vulkan init is broken — retrying the real backend
/// beats escalating straight into a known-broken one.
pub(super) fn gl_stages() -> &'static [GlStage] {
    &[GlStage::NativeGl, GlStage::NativeGl, GlStage::SwiftShader]
}

/// Everything a Chrome command line depends on. Both transports build from
/// [`launch_args`] — the v3.9.11 pipe path silently missing
/// `--ignore-gpu-blocklist` is exactly the class of bug this prevents:
/// Chrome then answers "WebGL{1,2} blocklisted" and every context is null
/// (stealth was fine; the launch wasn't).
pub(super) struct LaunchCfg<'a> {
    pub(super) stage: GlStage,
    pub(super) headful: bool,
    pub(super) no_sandbox: bool,
    pub(super) transport: Transport,
    pub(super) port: u16,
    pub(super) user_data_dir: &'a std::path::Path,
    pub(super) proxy: Option<&'a str>,
    pub(super) extra: &'a [String],
}

/// The single source of truth for a Chrome command line. Pure — unit tests
/// lock the WS/pipe delta to exactly the transport flag.
pub(super) fn launch_args(cfg: &LaunchCfg<'_>) -> Vec<String> {
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

/// Everything the real-browser lane's command line depends on. Same
/// discipline as [`launch_args`]: one pure builder. The lane's contract is
/// the *absence* of the stealth layer — every flag here is functional
/// (reliable startup, OAuth popups, PDFs as downloads) or the debug
/// transport. Nothing manufactures fingerprint coherence: a real
/// environment needs none, and every mask is a measurable risk.
pub(super) struct RealLaunchCfg<'a> {
    pub(super) headless: bool,
    pub(super) no_sandbox: bool,
    pub(super) ozone_x11: bool,
    pub(super) port: u16,
    pub(super) user_data_dir: &'a std::path::Path,
    pub(super) profile_directory: Option<&'a str>,
    pub(super) proxy: Option<&'a str>,
    pub(super) extra: &'a [String],
}

pub(super) fn launch_args_real(cfg: &RealLaunchCfg<'_>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        // Functional renderer-crash guard in small-/dev/shm containers.
        // Changes nothing a page can observe.
        "--disable-dev-shm-usage".into(),
        // Driver functionality: allow window.open popups (OAuth, payment
        // flows) in agent tabs.
        "--disable-popup-blocking".into(),
        // Driver functionality: PDFs download instead of opening inline.
        "--disable-features=PdfPlugin".into(),
    ];
    if cfg.no_sandbox {
        args.push("--no-sandbox".into());
    }
    if cfg.headless {
        args.extend(HEADLESS_FLAGS.iter().map(|s| s.to_string()));
        args.push("--window-size=1920,1080".into());
        // Functional GL enablers for GPU-less servers: without them Chrome
        // 139+ hands out null WebGL contexts even in software mode. This
        // enables a working stack — the lane never rewrites what the stack
        // then reports (no spoof, no mask).
        args.push("--enable-unsafe-swiftshader".into());
        args.push("--ignore-gpu-blocklist".into());
    }
    // NOTE: no `--disable-extensions` — the user's extensions are part of
    // the real fingerprint surface and stay.
    // Visible mode follows the ambient session: a real Wayland session is
    // used as Wayland (no pin). When only X11 is reachable the caller sets
    // `ozone_x11` and we pin it — auto-selection consults XDG_SESSION_TYPE,
    // and a stale "wayland" claim routes Chrome to Wayland, which EXITS on
    // connect failure instead of falling back (measured on Chrome 151).
    if cfg.ozone_x11 {
        args.push("--ozone-platform=x11".into());
    }
    args.push(format!("--remote-debugging-port={}", cfg.port));
    args.push(format!("--user-data-dir={}", cfg.user_data_dir.display()));
    if let Some(dir) = cfg.profile_directory {
        // Which profile inside the root (`Default`, `Profile 1`, ...) — the
        // root is what gets cloned/adopted, never a bare profile subdir.
        args.push(format!("--profile-directory={dir}"));
    }
    if let Some(proxy) = cfg.proxy {
        args.push(format!("--proxy-server={proxy}"));
        // Only meaningful with a proxy: keep WebRTC off the real IP.
        args.push("--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into());
    }
    args.extend(cfg.extra.iter().cloned());
    args
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
        // NativeGl is retried once before the SwiftShader escalation.
        assert_eq!(gl_stages()[1], GlStage::NativeGl);
        assert_eq!(*gl_stages().last().unwrap(), GlStage::SwiftShader);
        assert!(gl_stages().len() >= 3);
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
