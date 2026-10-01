//! Stealth injection — the script that runs before every page load.
//!
//! Injected via `Page.addScriptToEvaluateOnNewDocument`. Runs at the
//! `document_start` phase, before any page JavaScript executes.
//!
//! 6-layer stealth architecture:
//! 1. Protocol: No Runtime.enable (defuses DataDome console trap) — in CdpSession.
//! 2. Launch: navigator.webdriver is false by Chrome's own default without
//!    --enable-automation; the old blink-flag workaround was removed in
//!    v3.9.12 (it triggered the unsupported-flag infobar — a visible tell).
//! 3. UA: Network.setUserAgentOverride with full Client Hints — in Page::attach.
//! 4. Injection (this file): native-lie toString masking (S9), cdc_ removal,
//!    outer dims, WebGL1+2, screen geometry, Web API polyfills
//!    (WebShare, ContentIndex, ContactsManager, downlinkMax),
//!    seeded canvas noise, seeded audio noise,
//!    window.chrome object, battery API, WebRTC IP leak prevention.
//!    The permissions rewrite + media-devices patch are conditional
//!    segments — only injected when the environment actually needs them.
//! 5. Biometrics: Bezier mouse paths, log-normal typing cadence — in action.rs.
//! 6. Xvfb: Headful mode on virtual display (eliminates headless signals at root).
//!
//! The JS payloads live in `js/` — one file per block, assembled here with
//! `include_str!`; the test suite `node --check`s the assembled script.
//!
//! Phase 3 changes (S9):
//! - Every override is registered in a "native lie" registry and one patched
//!   Function.prototype.toString serves "function name() { [native code] }"
//!   for all of them. Spoofed getters are masked too.
//! - Property locations match real Chrome: spoofs live on Screen.prototype /
//!   Navigator.prototype (not own properties on the instance) with WebIDL
//!   enumerability (true for attributes/methods, false for interface objects).
//! - Worker Proxy REMOVED: it hung blob:/data: workers (opaque-origin
//!   importScripts fails), which broke real sites' worker-based collectors
//!   and was itself a detection surface. On Xvfb worker WebGL fails anyway
//!   (no hasBadWebGL signal); on real displays the real GPU needs no spoof.
//! - hardwareConcurrency / deviceMemory spoofs REMOVED: real machine values
//!   are more coherent than normalized ones (D14 — coherence over noise).
//!
//! S10: no Function.prototype.toString patch anywhere. Every installed
//! function is a Proxy over a native target (the original accessor/method,
//! or a bound non-constructible placeholder for polyfills). V8 gives a Proxy
//! no [[SourceText]], so it stringifies as "function () { [native code] }" in
//! EVERY realm — including the phantom nested-iframe realm CreepJS-class lie
//! engines stringify from. The trap closure is unreachable from page code
//! (own keys stay {length,name}, no 'prototype', non-constructible), and
//! delegation to the captured native runs FIRST, so receiver/argument
//! semantics ('Illegal invocation', TypeError text, promise timing) stay
//! byte-native.
//!
//! Module map: this file is the injection core — script assembly (`apply`),
//! the full-script / worker registry, and the conditional segments with
//! their patches; the child `gpu` holds GPU detection and the page/worker
//! GL spoof builders.

use crate::cdp::CdpSession;
use crate::error::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

mod gpu;

use self::gpu::{build_gl_spoof, build_worker_gl_spoof, get_gpu_profile};

/// Whether GL spoofing was applied to the main page. Set by `apply()`, read
/// by `worker_gl_spoof()` to decide if worker injection is needed.
static GL_SPOOFED: AtomicBool = AtomicBool::new(false);

/// The most recently assembled full stealth script (v3.9). The OOPIF
/// auto-attach handler injects it into out-of-process iframe sessions —
/// `Page.addScriptToEvaluateOnNewDocument` on the main session never
/// reaches those (separate targets, separate processes).
static FULL_SCRIPT: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// The assembled full stealth script, if one has been registered.
pub fn full_script() -> Option<String> {
    let guard = FULL_SCRIPT.read().ok()?;
    if guard.is_empty() {
        None
    } else {
        Some(guard.clone())
    }
}

/// True once a full stealth script has been registered (drives whether the
/// auto-attach handler needs to run at all).
pub fn has_full_script() -> bool {
    FULL_SCRIPT.read().map(|s| !s.is_empty()).unwrap_or(false)
}

pub type ScriptId = String;

/// Core block (always applied): seed, proxy-mask helpers (PROXY_HELPERS),
/// cdc_ removal, outer dims, screen geometry, polyfills. `__SEED__` is
/// replaced with a random u32 at injection time (stable per session).
/// Launch flags handle navigator.webdriver, window.chrome, and
/// navigator.plugins — this script never touches them.
///
/// Shared proxy-mask helper block. Used verbatim by the page core and by the
/// worker scripts (via `__PROXY_HELPERS__` / direct prefix) so the two realms
/// can never drift apart.
const PROXY_HELPERS: &str = include_str!("js/proxy_helpers.js");

/// Core block (always applied): seed, proxy-mask helpers, cdc_ removal, outer
/// dims, screen geometry, permissions, polyfills.
/// `__SEED__` is replaced with a random u32 at injection time (stable per
/// session). Launch flags handle navigator.webdriver, window.chrome, and
/// navigator.plugins — this script never touches them.
const STEALTH_CORE: &str = include_str!("js/stealth_core.js");

/// Tail: cdc_ watcher + IIFE close. GL_SPOOF / NOISE assemble between HEAD and TAIL.
const STEALTH_TAIL: &str = include_str!("js/stealth_tail.js");

/// Media devices patch — applied ONLY when the real machine reports zero
/// devices (a server tell; real desktops always have audio in/out). Returns
/// the exact pre-permission shape of real Chrome: devices present, ids and
/// labels empty. Passthrough whenever real devices exist.
const MEDIA_PATCH: &str = include_str!("js/media_patch.js");

/// permissions.query('notifications') rewrite — always installed; the
/// rewrite itself fires only for genuinely-origined documents
/// (`location.origin !== 'null'`) whose result is 'denied'. On opaque
/// origins (about:blank) real Chrome also reports 'denied', so the native
/// result stands; on real origins the only divergent environment
/// (headless-new: 'denied' everywhere) is masked to 'prompt'. The wrapper
/// relays with the caller's exact receiver and arguments — native validation
/// runs first, so zero-arg TypeErrors, non-object TypeErrors, wrong-receiver
/// 'Illegal invocation' and promise timing stay byte-native.
const PERMISSIONS_PATCH: &str = include_str!("js/permissions_patch.js");

/// `--remote-debugging-pipe` is Chrome's automation transport, and Chrome
/// enables the blink AutomationControlled feature for it: `navigator.webdriver`
/// is `true` on that lane while the WS lane reports `false` (measured on
/// Chrome 151 against a stock control). The launch flag that clears it
/// natively (`--disable-blink-features=AutomationControlled`) also triggers
/// Chrome's "unsupported command-line flag" infobar — a visible 56px tell that
/// skews innerHeight (measured: 932 vs 988) — so the value is masked instead,
/// exactly like every other environment override: a proxy over the native
/// getter that delegates first (receiver validation stays byte-native) and
/// returns `false`. Residual: the proxy shape is lie-engine-visible (measured:
/// CreepJS `webDriverIsOn: true` → 33% headless), and
/// `Emulation.setAutomationOverride{enabled:false}` does NOT clear the flag
/// (accepted, no effect — measured 2026-09-26, page- and browser-level). That
/// is why the MCP now defaults to WS; this patch serves only the explicit
/// `BLADE_TRANSPORT=pipe` opt-in.
const WEBDRIVER_PATCH: &str = include_str!("js/webdriver_patch.js");

/// Locale override — navigator.language/languages must match BLADE_LOCALE.
/// Applied when BLADE_LOCALE is set (S6: geo-consistent identity).
const LOCALE_OVERRIDE: &str = include_str!("js/locale_override.js");

/// Seeded canvas+audio noise block — opt-in via BLADE_NOISE=1 (D14: noise
/// injection is ML-detectable as browser tampering on FingerprintJS-class
/// detectors; real hardware fingerprints are stable and coherent without it).
const NOISE: &str = include_str!("js/noise.js");

/// WebRTC ICE filtering — applied ONLY when BLADE_PROXY is set. Without a
/// proxy, stripping candidates breaks legit WebRTC for zero privacy gain
/// (the page learns the same IP from HTTP; host candidates are mDNS-
/// obfuscated by Chrome itself). With a proxy, three layers keep the real IP
/// out of page reach: the `--force-webrtc-ip-handling-policy` launch flag
/// (network), SDP filtering (what the remote peer sees), and LOCAL
/// candidate-event filtering — a page enumerating its OWN ICE candidates
/// reads srflx addresses directly, and that is the vector fingerprinting
/// scripts actually use. Verified live: flag + SDP filter together still let
/// `onicecandidate` expose the real egress IP; the event filter removes srflx
/// and raw-IP host candidates while keeping mDNS `.local` hosts, relay and
/// prflx (a coherent proxy-user ICE profile). Residual, documented: `getStats()`
/// local-candidate entries can still name addresses.
const RTC_PATCH: &str = include_str!("js/rtc_patch.js");

/// What the attach-time environment probe learned about the real machine.
struct EnvProbe {
    gl_renderer: Option<String>,
    media_devices: i64,
}

/// Probe the REAL environment of the current page before any spoofing is
/// registered: WebGL renderer (fallback for externally-attached browsers —
/// launched browsers use the launch healthcheck), media device count, and
/// the notifications permission state. One evaluate round-trip.
async fn probe_environment(cdp: &CdpSession) -> EnvProbe {
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": "(async function(){var out={gl:null,media:-1};try{var c=document.createElement('canvas');var g=c.getContext('webgl');if(g){var e=g.getExtension('WEBGL_debug_renderer_info');out.gl=e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):null;}}catch(e){}try{var d=await navigator.mediaDevices.enumerateDevices();out.media=d.length;}catch(e){}return out;})()",
                "returnByValue": true,
                "awaitPromise": true,
            })),
        )
        .await;
    match res {
        Ok(v) => {
            let val = v
                .get("result")
                .and_then(|r| r.get("value"))
                .cloned()
                .unwrap_or(Value::Null);
            EnvProbe {
                gl_renderer: val
                    .get("gl")
                    .and_then(|g| g.as_str())
                    .map(|s| s.to_string()),
                media_devices: val.get("media").and_then(|m| m.as_i64()).unwrap_or(-1),
            }
        }
        Err(_) => EnvProbe {
            gl_renderer: None,
            media_devices: -1,
        },
    }
}

/// True when a GL renderer string is a headless/server artifact that must
/// be hidden. Delegates to the shared definition in `browser` so the
/// launch healthcheck and the spoof decision can never disagree.
fn is_software_gl(renderer: &str) -> bool {
    crate::browser::is_software_renderer(renderer)
}

/// Legacy alias kept for external references (doc/examples).
pub const STEALTH_SCRIPT_TEMPLATE: &str = STEALTH_CORE;

/// Apply the stealth script to a CDP client via `Page.addScriptToEvaluateOnNewDocument`.
/// Adaptive: the launch healthcheck's GL verdict (browser.rs) drives the
/// WebGL spoof — a real GPU is reported honestly (no mask), software GL
/// gets the spoof (D14 — coherence over noise). Externally-attached
/// browsers fall back to a live probe here. Canvas/audio noise is opt-in
/// via BLADE_NOISE=1. BLADE_WEBGL=spoof|real forces the GL decision.
/// Returns the script identifier.
pub async fn apply(cdp: &CdpSession, locale_override: Option<&str>) -> Result<ScriptId> {
    // Real-browser lane: the injection layer is OFF by design. The user's
    // own browser on its real environment has no manufactured coherence to
    // maintain — truth has no tells to catch, and every proxy/getter this
    // module installs is a measurable risk. The lane's stealth is the real
    // environment plus the driver-side behavior layer; nothing page-visible.
    if crate::realbrowser::real_lane() {
        return Ok(String::new());
    }

    // Persistent seed: stable across sessions (canvas/audio fingerprint
    // consistency). Generated once, stored in ~/.blade/.fingerprint.json.
    let seed: u32 = crate::fingerprint::load_or_create_seed();

    // One environment probe drives every adaptive decision (S8/S15).
    let env = probe_environment(cdp).await;

    // Adaptive GL decision. The launch healthcheck (browser.rs) already
    // probed the real backend when Bladebro launched the browser itself —
    // trust it: hardware GL is reported honestly (nothing to mask), software
    // GL gets the spoof. Only an externally-attached browser (no healthcheck
    // in this process) falls back to a live probe here.
    let gl_mode = std::env::var("BLADE_WEBGL").unwrap_or_else(|_| "auto".to_string());
    let spoof_gl = match gl_mode.as_str() {
        "spoof" => true,
        "real" => false,
        _ => {
            match crate::browser::gpu_state() {
                Some(crate::browser::GpuState::Hardware(renderer)) => {
                    eprintln!(
                        "[stealth] GL healthcheck says hardware ({renderer}) — no WebGL spoof"
                    );
                    false
                }
                Some(crate::browser::GpuState::Software(renderer)) => {
                    eprintln!("[stealth] GL healthcheck says software ({renderer}) — registering WebGL spoof");
                    true
                }
                // No context existed at launch — the spoof is inert either way;
                // keep the fail-safe default.
                Some(crate::browser::GpuState::Missing) => true,
                None => match &env.gl_renderer {
                    Some(renderer) => {
                        let software = is_software_gl(renderer);
                        if software {
                            eprintln!("[stealth] real GL is software ({renderer}) — registering WebGL spoof");
                        }
                        software
                    }
                    // Probe failed — safest default is to spoof (hides SwiftShader).
                    None => true,
                },
            }
        }
    };
    // S15: mediaDevices patch only when the machine reports zero devices.
    let patch_media = match std::env::var("BLADE_MEDIA").as_deref() {
        Ok("patch") => true,
        Ok("real") => false,
        _ => env.media_devices == 0,
    };
    if patch_media {
        eprintln!("[stealth] registering mediaDevices patch");
    }
    let noise = std::env::var("BLADE_NOISE")
        .map(|v| v == "1")
        .unwrap_or(false);
    // BCP-47 validate: the locale is interpolated into a JS string literal —
    // a quote or backslash would break out and silently kill the whole
    // stealth injection. Letters, digits, '-', '_' only.
    // Explicit override (per-domain profile, S11) beats the env var.
    let raw_locale = locale_override
        .map(String::from)
        .or_else(|| std::env::var("BLADE_LOCALE").ok());
    let locale = raw_locale.filter(|s| !s.is_empty()).filter(|s| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    if std::env::var("BLADE_LOCALE").is_ok() && locale.is_none() {
        eprintln!("[stealth] WARNING: BLADE_LOCALE rejected (invalid characters) — locale override skipped");
    }
    if let Some(ref l) = locale {
        eprintln!("[stealth] registering locale override: {l}");
    }

    let mut script = String::with_capacity(
        STEALTH_CORE.len()
            + 2048
            + MEDIA_PATCH.len()
            + PERMISSIONS_PATCH.len()
            + LOCALE_OVERRIDE.len()
            + NOISE.len()
            + RTC_PATCH.len()
            + STEALTH_TAIL.len(),
    );
    script.push_str(STEALTH_CORE);
    // WebRTC candidate filtering only under a proxy (see RTC_PATCH docs).
    if std::env::var("BLADE_PROXY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        script.push_str(RTC_PATCH);
    }
    GL_SPOOFED.store(spoof_gl, Ordering::Relaxed);
    if spoof_gl {
        let profile = get_gpu_profile();
        eprintln!(
            "[stealth] registering WebGL spoof: {} (page + worker)",
            profile.gl_renderer
        );
        script.push_str(&build_gl_spoof(&profile));
        // SharedWorker constructor wrapper (issue #8): CDP doesn't emit
        // attachedToTarget for shared_worker targets, so we intercept the
        // constructor on the main page and inject GL spoof via blob URL.
        if let Some(sw) = sharedworker_wrapper(locale.as_deref()) {
            script.push_str(&sw);
        }
    }
    if patch_media {
        script.push_str(MEDIA_PATCH);
    }
    // Notifications relay: only the headless lane needs it. Headless-New
    // reports 'denied' for notifications on real origins — a server tell —
    // while a headful lane reports the honest state (verified against stock
    // on the same display: headful-on-Xvfb says 'prompt'). Installing it
    // otherwise is both a lie on honest desktops and a patched function a
    // lie engine can inspect. BLADE_PERMS=patch|real overrides.
    let patch_perms = match std::env::var("BLADE_PERMS").as_deref() {
        Ok("patch") => true,
        Ok("real") => false,
        _ => crate::browser::launched_headless(),
    };
    if patch_perms {
        script.push_str(PERMISSIONS_PATCH);
    }
    // Pipe transport: the automation flag makes navigator.webdriver true
    // (see WEBDRIVER_PATCH).
    if crate::browser::launched_pipe() {
        script.push_str(WEBDRIVER_PATCH);
    }
    if locale.is_some() {
        script.push_str(LOCALE_OVERRIDE);
    }
    if noise {
        script.push_str(NOISE);
    }
    script.push_str(STEALTH_TAIL);
    let script = script.replace("__SEED__", &seed.to_string());
    // Shared proxy-mask helpers (also used by the worker scripts).
    let script = script.replace("__PROXY_HELPERS__", PROXY_HELPERS);
    let script = if let Some(ref l) = locale {
        let base = l.split('-').next().unwrap_or(l).to_string();
        script
            .replace("__LOCALE__", l)
            .replace("__LOCALE_BASE__", &base)
    } else {
        script
    };

    // Register for the OOPIF auto-attach handler (see FULL_SCRIPT).
    if let Ok(mut guard) = FULL_SCRIPT.write() {
        *guard = script.clone();
    }

    // BLADE_DUMP_INJECT=<path>: write the exact assembled script to a file.
    // Used by the release checklist / regression work to syntax-lint the
    // injection (`node --check`): a parse error silently disables EVERY patch
    // (the audit's vector score drops, but a lint catches it before it ships).
    if let Ok(path) = std::env::var("BLADE_DUMP_INJECT") {
        let _ = std::fs::write(&path, &script);
        if let Some(w) = worker_gl_spoof(locale.as_deref()) {
            let _ = std::fs::write(format!("{path}.worker.js"), w);
        }
    }

    let res = cdp
        .send(
            "Page.addScriptToEvaluateOnNewDocument",
            Some(json!({
                "source": script,
                // Also run in the CURRENT document. Without this, a
                // bladebro attaching to an already-loaded page (the
                // --port connect path, or a daemon attach mid-session)
                // drives an UNPATCHED document until the first navigation.
                "runImmediately": true,
            })),
        )
        .await?;

    let id = res
        .get("identifier")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(id)
}

/// Worker GL spoof script — the same proxy architecture as the page
/// (PROXY_HELPERS is the shared helper block, so the two realms cannot
/// drift). Returns None when neither the GL spoof is active nor a locale
/// override is set (M11: with BLADE_LOCALE, WorkerNavigator.language must
/// match the main frame or the mismatch is itself a fingerprint).
/// D22: injected via CDP Target.setAutoAttach into each worker target.
pub fn worker_gl_spoof(locale: Option<&str>) -> Option<String> {
    // Real-browser lane: no worker patches either (see `apply`).
    if crate::realbrowser::real_lane() {
        return None;
    }
    if !GL_SPOOFED.load(Ordering::Relaxed) {
        if locale.is_some() {
            let patch = worker_locale_patch(locale);
            return Some(format!("{}\ntry{{{patch}}}catch(e){{}}", PROXY_HELPERS));
        }
        return None;
    }
    let profile = get_gpu_profile();
    Some(build_worker_gl_spoof(&profile, locale))
}

/// The WorkerNavigator locale patch as a standalone JS statement list —
/// proxy getters over the native accessors (needs PROXY_HELPERS).
fn worker_locale_patch(locale: Option<&str>) -> String {
    if let Some(l) = locale {
        let base = l.split('-').next().unwrap_or(l);
        format!(
            r#"if(typeof WorkerNavigator!=='undefined'){{
  var _wl='{l}';var _wls=['{l}','{base}'];
  _defGet(WorkerNavigator.prototype,'language',_ogs(WorkerNavigator.prototype,'language'),function(th,a,og){{og.apply(th,a);return _wl;}});
  _defGet(WorkerNavigator.prototype,'languages',_ogs(WorkerNavigator.prototype,'languages'),function(th,a,og){{og.apply(th,a);return _wls;}});
}}"#,
            l = l,
            base = base
        )
    } else {
        String::new()
    }
}

/// SharedWorker constructor wrapper for the main page injection.
/// CDP doesn't emit Target.attachedToTarget for shared_worker targets, so we
/// intercept the SharedWorker constructor and inject the GL spoof via a blob
/// URL: <gl_spoof + locale patch + importScripts(absolute_url)>. The URL is
/// resolved against the document first — relative paths cannot resolve inside
/// a blob: worker's scope (importScripts throws), which is exactly how
/// CreepJS's shared-worker tier was broken. A Proxy construct trap keeps the
/// interface object native-shaped (toString, own keys, prototype forwarding)
/// — the old plain-wrapper function was itself a differential. Any
/// construction failure falls back to the untouched constructor. Returns
/// None when GL spoof is not active.
pub fn sharedworker_wrapper(locale: Option<&str>) -> Option<String> {
    if !GL_SPOOFED.load(Ordering::Relaxed) {
        return None;
    }
    let profile = get_gpu_profile();
    let worker_code = build_worker_gl_spoof(&profile, locale);
    // Escape for embedding in a JS string literal.
    let escaped: String = worker_code
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    Some(format!(
        r#"
// SharedWorker GL spoof — CDP doesn't emit attachedToTarget for shared_worker.
try{{
  var _swCode='{escaped}';
  var _OrigSW=window.SharedWorker;
  if(_OrigSW){{
    var _SWProxy=new Proxy(_OrigSW,{{construct:function(t,a,nt){{
      try{{
        var url=a[0],options=a[1];
        try{{url=new URL(url, document.baseURI).href;}}catch(e){{}}
        var full=_swCode+'\nimportScripts('+JSON.stringify(url)+')';
        var blob=new Blob([full],{{type:'application/javascript'}});
        var blobUrl=URL.createObjectURL(blob);
        var sw=Reflect.construct(t,[blobUrl,options],nt);
        setTimeout(function(){{URL.revokeObjectURL(blobUrl);}},10000);
        return sw;
      }}catch(e){{
        return Reflect.construct(t,a,nt);
      }}
    }}}});
    Object.defineProperty(window,'SharedWorker',{{value:_SWProxy,writable:true,configurable:true,enumerable:false}});
  }}
}}catch(e){{}}
"#,
        escaped = escaped
    ))
}
