//! Stealth injection — the script that runs before every page load.
//!
//! Injected via `Page.addScriptToEvaluateOnNewDocument`. Runs at the
//! `document_start` phase, before any page JavaScript executes.
//!
//! 6-layer stealth architecture:
//! 1. Protocol: No Runtime.enable (defuses DataDome console trap) — in CdpSession.
//! 2. Launch: --disable-blink-features=AutomationControlled (webdriver=false at Blink level).
//! 3. UA: Network.setUserAgentOverride with full Client Hints — in Page::attach.
//! 4. Injection (this file): native-lie toString masking (S9), cdc_ removal,
//!    outer dims, WebGL1+2, screen geometry, permissions, Web API polyfills
//!    (WebShare, ContentIndex, ContactsManager, downlinkMax),
//!    seeded canvas noise, seeded audio noise,
//!    window.chrome object, speech synthesis voices, battery API,
//!    WebRTC IP leak prevention, error stack normalization.
//! 5. Biometrics: Bezier mouse paths, log-normal typing cadence — in action.rs.
//! 6. Xvfb: Headful mode on virtual display (eliminates headless signals at root).
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
//! Residual: Function.prototype.toString replacement is a plain function
//! (not a Proxy, so hasToStringProxy passes) but owns a .prototype property
//! that native methods lack — accepted, ultra-rare check.

use crate::cdp::CdpSession;
use crate::error::Result;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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
    if guard.is_empty() { None } else { Some(guard.clone()) }
}

/// True once a full stealth script has been registered (drives whether the
/// auto-attach handler needs to run at all).
pub fn has_full_script() -> bool {
    FULL_SCRIPT.read().map(|s| !s.is_empty()).unwrap_or(false)
}

pub type ScriptId = String;

/// Core block (always applied): seed, native-lie registry, helpers, cdc_
/// removal, outer dims, screen geometry, permissions, polyfills.
/// `__SEED__` is replaced with a random u32 at injection time (stable per
/// session). Launch flags handle navigator.webdriver, window.chrome, and
/// navigator.plugins — this script never touches them.
const STEALTH_CORE: &str = r#"(function(){
var S=__SEED__;
function R(){S^=S<<13;S^=S>>>17;S^=S<<5;return((S>>>0)%256);}
var cn=R()&1,an=R(),cn2=R()&1;

// --- S9: native-lie registry. One patched toString masks every override. ---
var _lies=[];
function _lie(fn,name){_lies.push([fn,name]);}
var _ots=Function.prototype.toString;
var _nts=function toString(){
  for(var i=0;i<_lies.length;i++){if(this===_lies[i][0])return'function '+_lies[i][1]+'() { [native code] }';}
  return _ots.apply(this,arguments);
};
_lie(_nts,'toString');
Function.prototype.toString=_nts;
// Masked helpers: WebIDL attributes/methods are enumerable:true on prototypes;
// interface objects (constructors) on window are enumerable:false.
// NOTE on getter naming: real V8 native accessors stringify WITHOUT a
// "get " prefix — Object.getOwnPropertyDescriptor(Window.prototype,'outerWidth')
// .get.toString() === 'function outerWidth() { [native code] }'. Masking
// getters as 'get outerWidth' was itself a detectable toString-format tell.
function _defGet(obj,name,fn){try{Object.defineProperty(obj,name,{get:fn,configurable:true,enumerable:true});_lie(fn,name);}catch(e){}}
function _defFn(obj,name,fn){try{Object.defineProperty(obj,name,{value:fn,writable:true,configurable:true,enumerable:true});_lie(fn,name);}catch(e){}}
function _defCtor(name,fn){try{Object.defineProperty(window,name,{value:fn,writable:true,configurable:true,enumerable:false});_lie(fn,name);}catch(e){}}

// cdc_ residue removal (chromedriver artifact — belt and suspenders).
var p=Object.getOwnPropertyNames(document).concat(Object.getOwnPropertyNames(window));
for(var i=0;i<p.length;i++){if(p[i].indexOf('cdc_')===0){try{delete document[p[i]];delete window[p[i]];}catch(e){}}}

// Window outer dims (Xvfb has no WM, so outer===inner without this).
// Defined on Window.prototype — real Chrome exposes these as accessors
// on the prototype; an OWN accessor on the window instance makes
// Object.getOwnPropertyDescriptor(window,'outerWidth') non-undefined,
// which no real Chrome ever is.
try{
var _wp=(typeof Window!=='undefined'&&Window.prototype)?Window.prototype:Object.getPrototypeOf(window);
_defGet(_wp,'outerWidth',function outerWidth(){return window.innerWidth+16;});
_defGet(_wp,'outerHeight',function outerHeight(){return window.innerHeight+88;});
}catch(e){}

// Screen geometry on Screen.prototype (real location, not the instance).
try{
var _sp=(typeof Screen!=='undefined'&&Screen.prototype)?Screen.prototype:Object.getPrototypeOf(screen);
_defGet(_sp,'availWidth',function(){return screen.width;});
_defGet(_sp,'availHeight',function(){return screen.height-40;});
}catch(e){}

// permissions.query('notifications') — headless returns 'denied', real
// desktop 'prompt'. The wrapper delegates to the REAL query (so the
// result is a genuine PermissionStatus: instanceof + EventTarget methods
// work) and rewrites only the headless 'denied' tell to 'prompt'. On
// headful Xvfb the real state already is 'prompt' — the rewrite never
// fires, zero added surface.
try{
var _opq=navigator.permissions.query;
var _pq=function query(d){
  var p=_opq.call(navigator.permissions,d);
  if(d&&d.name==='notifications'){
    return p.then(function(s){
      if(s&&s.state==='denied'){try{Object.defineProperty(s,'state',{value:'prompt',configurable:true});}catch(e){}}
      return s;
    });
  }
  return p;
};
var _pp=Object.getPrototypeOf(navigator.permissions);
try{Object.defineProperty(_pp,'query',{value:_pq,writable:true,configurable:true,enumerable:true});}catch(e){navigator.permissions.query=_pq;}
_lie(_pq,'query');
}catch(e){}

// Web API polyfills (Linux-headless absence signals).
try{
var _np=(typeof Navigator!=='undefined'&&Navigator.prototype)?Navigator.prototype:Object.getPrototypeOf(navigator);
if(!('share' in navigator)){_defFn(_np,'share',function share(){return Promise.reject(new TypeError('Not supported'));});}
if(!('canShare' in navigator)){_defFn(_np,'canShare',function canShare(){return false;});}
if(!('ContentIndex' in window)){_defCtor('ContentIndex',function ContentIndex(){throw new TypeError('Illegal constructor');});}
if(!('ContactsManager' in window)){_defCtor('ContactsManager',function ContactsManager(){throw new TypeError('Illegal constructor');});}
if(navigator.connection){
  var _pr=Object.getPrototypeOf(navigator.connection);
  if(_pr&&!('downlinkMax' in _pr)){_defGet(_pr,'downlinkMax',function(){return Infinity;});}
  if(typeof window.NetworkInformation==='undefined'){_defCtor('NetworkInformation',navigator.connection.constructor);}
}
}catch(e){}

// ── Additional stealth layers ───────────────────────────────

// window.chrome object: real Chrome has chrome.app, chrome.runtime,
// chrome.csi(), chrome.loadTimes(). Headless may be missing these.
try{
if(!window.chrome){window.chrome={};}
if(!window.chrome.app){window.chrome.app={isInstalled:false,InstallState:{DISABLED:'disabled',INSTALLED:'installed',NOT_INSTALLED:'not_installed'},RunningState:{CANNOT_RUN:'cannot_run',READY_TO_RUN:'ready_to_run',RUNNING:'running'}};}
if(!window.chrome.runtime){window.chrome.runtime={OnInstalledReason:{CHROME_UPDATE:'chrome_update',INSTALL:'install',SHARED_MODULE_UPDATE:'shared_module_update',UPDATE:'update'},OnRestartRequiredReason:{APP_UPDATE:'app_update',OS_UPDATE:'os_update',PERIODIC:'periodic'},PlatformArch:{ARM:'arm',MIPS:'mips',MIPS64:'mips64',X86_32:'x86-32',X86_64:'x86-64'},PlatformNaclArch:{ARM:'arm',MIPS:'mips',MIPS64:'mips64',X86_32:'x86-32',X86_64:'x86-64'},PlatformOs:{ANDROID:'android',CROS:'cros',FUCHSIA:'fuchsia',LINUX:'linux',MAC:'mac',OPENBSD:'openbsd',WIN:'win'},RequestUpdateCheckStatus:{NO_UPDATE:'no_update',THROTTLED:'throttled',UPDATE_AVAILABLE:'update_available'}};}
if(!window.chrome.csi){window.chrome.csi=function csi(){return{startE:Date.now(),onloadT:Date.now()+100,pageT:1000,tran:15};};_lie(window.chrome.csi,'csi');}
if(!window.chrome.loadTimes){window.chrome.loadTimes=function loadTimes(){return{commitLoadTime:Date.now()/1000,connectionInfo:'h2',finishDocumentLoadTime:Date.now()/1000+0.1,finishLoadTime:Date.now()/1000+0.2,firstPaintAfterLoadTime:0,firstPaintTime:Date.now()/1000+0.05,navigationType:'Other',npnNegotiatedProtocol:'h2',requestTime:Date.now()/1000-0.5,startLoadTime:Date.now()/1000-0.5,wasAlternateProtocolAvailable:false,wasFetchedViaSpdy:true,wasNpnNegotiated:true};};_lie(window.chrome.loadTimes,'loadTimes');}
}catch(e){}

// Speech synthesis voices: headless returns empty array.
// Real Linux Chrome has at least a few voices.
// Patched on SpeechSynthesis.prototype (the real location — the
// speechSynthesis instance has zero own properties) and returns a
// FRESH array per call (getVoices() === getVoices() must be false,
// as in real Chrome).
try{
if(typeof speechSynthesis!=='undefined'&&speechSynthesis.getVoices().length===0){
  var _voices=[
    {voiceURI:'Google US English',name:'Google US English',lang:'en-US',localService:false,default:true},
    {voiceURI:'Google UK English Female',name:'Google UK English Female',lang:'en-GB',localService:false,default:false},
    {voiceURI:'Google UK English Male',name:'Google UK English Male',lang:'en-GB',localService:false,default:false}
  ];
  var _ssp=(typeof SpeechSynthesis!=='undefined'&&SpeechSynthesis.prototype)?SpeechSynthesis.prototype:Object.getPrototypeOf(speechSynthesis);
  var _gv=function getVoices(){return _voices.slice();};
  try{Object.defineProperty(_ssp,'getVoices',{value:_gv,writable:true,configurable:true,enumerable:true});}catch(e){speechSynthesis.getVoices=_gv;}
  _lie(_gv,'getVoices');
}
}catch(e){}

// Battery API: headless may not have navigator.getBattery.
try{
if(!navigator.getBattery){
  var _gb=function getBattery(){return Promise.resolve({charging:true,chargingTime:0,dischargingTime:Infinity,level:1,onchargingchange:null,onchargingtimechange:null,ondischargingtimechange:null,onlevelchange:null});};
  _defFn(Navigator.prototype,'getBattery',_gb);
}
}catch(e){}

// WebRTC ICE/SDP filtering moved OUT of the core (v3.9): modern Chrome
// already replaces host candidates with mDNS names, and the
// --force-webrtc-ip-handling-policy launch flag handles the network
// layer. Stripping srflx/host candidates from every session BREAKS
// legit WebRTC (empty candidate lists are themselves a detection
// surface) for zero benefit without a proxy. The patch is now applied
// ONLY when BLADE_PROXY is set — the one case where the real IP must
// not appear in candidates. See RTC_PATCH in apply().

// Error-stack normalization REMOVED (v3.9): the wrapper broke every
// `class X extends Error` subclass (instanceof failed), forced early
// stack materialization (defeating page-set prepareStackTrace /
// stackTraceLimit), and copied stackTraceLimit by value. On headful
// Xvfb Chrome, page errors never contain devtools/chrome-extension
// frames anyway — the patch bought nothing at real functional cost.

// Document.visibilityState: should be 'visible' in headful mode.
// Some headless configurations report 'hidden' or 'prerender'.
// Defined on Document.prototype (the real WebIDL location) — an own
// property on the document instance is a descriptor-shape tell.
try{
if(document.visibilityState!=='visible'){
  var _dp=(typeof Document!=='undefined'&&Document.prototype)?Document.prototype:Object.getPrototypeOf(document);
  _defGet(_dp,'visibilityState',function visibilityState(){return 'visible';});
  _defGet(_dp,'hidden',function hidden(){return false;});
}
}catch(e){}

// performance.timing polyfill REMOVED (v3.9): the fabricated timeline
// was internally incoherent (navigationStart !== performance.timeOrigin,
// loadEventEnd timestamped before load). Modern Chrome's absence or
// presence of performance.timing is what a real Chrome of the same
// version shows — polyfilling it was the anomaly.

// Notification.permission: should be 'default' (not 'denied').
try{
if(typeof Notification!=='undefined'&&Notification.permission==='denied'){
  _defGet(Notification,'permission',function permission(){return 'default';});
}
}catch(e){}

// navigator.pdfViewerEnabled: real Chrome has this as true.
// --disable-features=PdfPlugin makes it false (for download support),
// so patch it back to true to maintain the fingerprint.
try{
if(navigator.pdfViewerEnabled===false){
  _defGet(Navigator.prototype,'pdfViewerEnabled',function pdfViewerEnabled(){return true;});
}
}catch(e){}

// navigator.presentation: REMOVED — adding a fake {} via _defFn creates
// a detectable data descriptor. Real Chrome only exposes this on HTTPS.
// Missing it is normal and less suspicious than a wrong-shaped object.

// navigator.connection: more realistic values for a broadband connection.
// Headless may report unrealistic values or missing properties.
try{
if(navigator.connection){
  var _cp=Object.getPrototypeOf(navigator.connection);
  if(_cp){
    if(!('effectiveType' in _cp))_defGet(_cp,'effectiveType',function(){return '4g';});
    if(!('rtt' in _cp))_defGet(_cp,'rtt',function(){return 50;});
    if(!('downlink' in _cp))_defGet(_cp,'downlink',function(){return 10;});
    if(!('saveData' in _cp))_defGet(_cp,'saveData',function(){return false;});
  }
}
}catch(e){}

// navigator.scheduling: Chrome has the Scheduling API.
// Use getter (not data property) — real Chrome exposes it as an accessor.
try{
if(!navigator.scheduling&&!('scheduling' in navigator)){
  var _sch={isInputPending:function isInputPending(){return false;},isInputPendingOrAvailable:function isInputPendingOrAvailable(){return false;}};
  _lie(_sch.isInputPending,'isInputPending');
  _lie(_sch.isInputPendingOrAvailable,'isInputPendingOrAvailable');
  _defGet(Navigator.prototype,'scheduling',function scheduling(){return _sch;});
}
}catch(e){}

// navigator.cookieEnabled: should be true (Chrome default).
try{
if(!navigator.cookieEnabled){
  _defGet(Navigator.prototype,'cookieEnabled',function cookieEnabled(){return true;});
}
}catch(e){}

// Screen colorDepth/pixelDepth: should be 24 (standard).
try{
var _sp2=(typeof Screen!=='undefined'&&Screen.prototype)?Screen.prototype:Object.getPrototypeOf(screen);
if(screen.colorDepth!==24){_defGet(_sp2,'colorDepth',function colorDepth(){return 24;});}
if(screen.pixelDepth!==24){_defGet(_sp2,'pixelDepth',function pixelDepth(){return 24;});}
}catch(e){}

// V8: console capture for driver introspection (see logs=console).
// Hooked methods are masked by the _lie registry (toString shows
// native code) and carry their proper .name. The ring buffer lives
// under a Symbol-keyed NON-ENUMERABLE window slot: invisible to
// for-in, Object.keys, and getOwnPropertyNames — the old string-keyed
// `__uxa` own property was readable by any page with `'__uxa' in window`.
// Ring buffer, 200 entries, resets per document (correct: logs are
// per load).
try{
var _uk=Symbol.for('q');
var _uxa=[];
function _uxp(l,a){try{var p=[];for(var i=0;i<a.length;i++){var v=a[i];try{p.push(typeof v==='string'?v:JSON.stringify(v));}catch(e){p.push(String(v));}}_uxa.push({l:l,m:p.join(' ').slice(0,400),t:Date.now()});if(_uxa.length>200)_uxa.shift();}catch(e){}}
['log','info','warn','error','debug'].forEach(function(m){
  var _o=console[m];
  var _h=function(){_uxp(m,arguments);return _o.apply(console,arguments);};
  // Real console methods have matching .name ('log' etc.) — an
  // anonymous hook's inferred name is a separate observable from
  // toString.
  try{Object.defineProperty(_h,'name',{value:m,configurable:true});}catch(e){}
  // Override on Console.prototype, not the console instance.
  // Own properties on the console instance are a detection vector
  // (Object.getOwnPropertyDescriptor(console,'log') should be undefined).
  try{var _cp=Object.getPrototypeOf(console);Object.defineProperty(_cp,m,{value:_h,writable:true,configurable:true,enumerable:true});}catch(e){console[m]=_h;}
  _lie(_h,m);
});
window.addEventListener('error',function(e){_uxp('exception',[String(e.message||'')+' @'+String(e.filename||'')+':'+String(e.lineno||'')]);});
window.addEventListener('unhandledrejection',function(e){_uxp('unhandledrejection',[String(e.reason)]);});
try{Object.defineProperty(window,_uk,{value:_uxa,writable:true,configurable:true,enumerable:false});}catch(e){try{window[_uk]=_uxa;}catch(e2){}}

// Issue #9: macOS shows a system dialog "Chrome Helper needs to download
// the font 'Osaka'/'STHeiti'" when on-demand CJK fonts are referenced in
// page CSS but not installed. We inject a stylesheet that defines
// @font-face aliases mapping these font names to system fonts that are
// always available. This intercepts the font request before it reaches
// macOS's CoreText download system. --disable-remote-fonts (launch flag)
// handles the web-font side; this handles the CSS font-family side.
// Note: addScriptToEvaluateOnNewDocument runs before HTML parsing, so
// documentElement/head may be null. Create the element now, append when
// the DOM is ready.
try{
var _fs=document.createElement('style');
_fs.textContent="@font-face{font-family:'Osaka';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'STHeiti';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'STHeiti Light';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'Hiragino Sans';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'Hiragino Mincho ProN';src:local('Times New Roman'),local('serif');}";
if(document.documentElement){(document.head||document.documentElement).appendChild(_fs);}
else{document.addEventListener('DOMContentLoaded',function(){try{(document.head||document.documentElement).appendChild(_fs);}catch(e){}});}
}catch(e){}
}catch(e){}

"#;

/// Tail: cdc_ watcher + IIFE close. GL_SPOOF / NOISE assemble between HEAD and TAIL.
const STEALTH_TAIL: &str = r#"
// cdc_ late-injection watcher (first 3s only, then disconnects).
// Throttled: getOwnPropertyNames on EVERY mutation was measurable
// main-thread jank in the first 3s — itself a timing fingerprint.
var _cdcLast=0;
var obs=new MutationObserver(function(){
  var now=Date.now();if(now-_cdcLast<500)return;_cdcLast=now;
  var q=Object.getOwnPropertyNames(document);
  for(var j=0;j<q.length;j++){if(q[j].indexOf('cdc_')===0){try{delete document[q[j]];}catch(e){}}}
});obs.observe(document,{childList:true,subtree:true});setTimeout(function(){obs.disconnect();},3000);
})();"#;

/// GPU profile for WebGL spoofing — detected from host hardware (lspci on
/// Linux) so the spoofed renderer string and GL capability limits match the
/// real GPU. Falls back to Intel UHD 630 when detection fails (Docker without
/// pciutils, macOS, Windows). Override: BLADE_GPU=intel|amd|nvidia.
#[derive(Clone)]
struct GpuProfile {
    gl_vendor: String,
    gl_renderer: String,
    max_texture_size: i32,      // 3379 (MAX_TEXTURE_SIZE)
    max_renderbuffer_size: i32, // 34024
    max_cube_map_size: i32,      // 34076
    max_vtx_tex_units: i32,     // 35661
    max_viewport_dims: [i32; 2], // 3386
    point_size_range: [f32; 2],  // 33902
    line_width_range: [f32; 2],  // 33901
}

static DETECTED_GPU: OnceLock<GpuProfile> = OnceLock::new();

fn get_gpu_profile() -> GpuProfile {
    DETECTED_GPU.get_or_init(detect_gpu).clone()
}

fn detect_gpu() -> GpuProfile {
    if let Ok(gpu) = std::env::var("BLADE_GPU") {
        match gpu.as_str() {
            "amd" => { eprintln!("[stealth] BLADE_GPU=amd — using AMD Radeon profile"); return amd_profile(); }
            "nvidia" => { eprintln!("[stealth] BLADE_GPU=nvidia — using NVIDIA GeForce profile"); return nvidia_profile(); }
            "mali" => { eprintln!("[stealth] BLADE_GPU=mali — using Mali-G78 profile"); return mali_profile(); }
            "adreno" => { eprintln!("[stealth] BLADE_GPU=adreno — using Adreno 730 profile"); return adreno_profile(); }
            _ => {
                // Default override: match the native arch
                #[cfg(target_arch = "aarch64")]
                { eprintln!("[stealth] BLADE_GPU={gpu} — using Mali-G78 profile"); return mali_profile(); }
                #[cfg(not(target_arch = "aarch64"))]
                { eprintln!("[stealth] BLADE_GPU={gpu} — using Intel UHD 630 profile"); return intel_profile("Mesa Intel(R) UHD Graphics 630 (CFL GT2)"); }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(p) = detect_gpu_lspci() {
            eprintln!("[stealth] detected GPU: {}", p.gl_renderer);
            return p;
        }
    }
    // Architecture-aware fallback: Mali on ARM, Intel on x86
    #[cfg(target_arch = "aarch64")]
    eprintln!("[stealth] GPU detection failed — using Mali-G78 fallback (aarch64)");
    #[cfg(not(target_arch = "aarch64"))]
    eprintln!("[stealth] GPU detection failed — using Intel UHD 630 fallback");

    #[cfg(target_arch = "aarch64")]
    { mali_profile() }
    #[cfg(not(target_arch = "aarch64"))]
    { intel_profile("Mesa Intel(R) UHD Graphics 630 (CFL GT2)") }
}

#[cfg(target_os = "linux")]
fn detect_gpu_lspci() -> Option<GpuProfile> {
    let output = std::process::Command::new("lspci").arg("-nn").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if !lower.contains("vga") && !lower.contains("3d") && !lower.contains("display") {
            continue;
        }
        // Intel
        if lower.contains("intel") {
            if lower.contains("alder lake-p") || lower.contains("adl-p") {
                return Some(intel_profile("Mesa Intel(R) Graphics (ADL-P GT2)"));
            }
            if lower.contains("alder lake-s") || lower.contains("adl-s") || lower.contains("uhd 770") {
                return Some(intel_profile("Mesa Intel(R) Graphics (ADL-S GT1)"));
            }
            if lower.contains("tiger lake") || lower.contains("tgl") {
                return Some(intel_profile("Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2)"));
            }
            if lower.contains("coffee lake") || lower.contains("uhd 630") || lower.contains("cfl") {
                return Some(intel_profile("Mesa Intel(R) UHD Graphics 630 (CFL GT2)"));
            }
            if lower.contains("skylake") || lower.contains("hd 530") || lower.contains("skl") {
                return Some(intel_profile("Mesa Intel(R) HD Graphics 530 (SKL GT2)"));
            }
            if lower.contains("haswell") || lower.contains("hsw") {
                return Some(intel_profile("Mesa Intel(R) HD Graphics 4600 (HSW GT2)"));
            }
            return Some(intel_profile("Mesa Intel(R) UHD Graphics 630 (CFL GT2)"));
        }
        // AMD/ATI
        if lower.contains("amd") || lower.contains("ati") || lower.contains("radeon") {
            return Some(amd_profile());
        }
        // NVIDIA
        if lower.contains("nvidia") || lower.contains("geforce") || lower.contains("quadro") {
            return Some(nvidia_profile());
        }
        // ARM Mali (common on ARM SoCs with PCI)
        if lower.contains("mali") || lower.contains("arm") && lower.contains("gpu") {
            return Some(mali_profile());
        }
        // Qualcomm Adreno
        if lower.contains("adreno") || lower.contains("qualcomm") {
            return Some(adreno_profile());
        }
    }
    None
}

#[allow(dead_code)]
fn intel_profile(mesa_name: &str) -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (Intel)".into(),
        gl_renderer: format!("ANGLE (Intel, {mesa_name}, OpenGL ES 3.2)"),
        max_texture_size: 16384,
        max_renderbuffer_size: 16384,
        max_cube_map_size: 16384,
        max_vtx_tex_units: 16,
        max_viewport_dims: [16384, 16384],
        point_size_range: [1.0, 255.0],
        line_width_range: [1.0, 1024.0],
    }
}

fn amd_profile() -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (AMD)".into(),
        gl_renderer: "ANGLE (AMD, Mesa AMD Radeon RX 6700 XT (navi22, LLVM 15.0.7), OpenGL ES 3.2)".into(),
        max_texture_size: 16384,
        max_renderbuffer_size: 16384,
        max_cube_map_size: 16384,
        max_vtx_tex_units: 32,
        max_viewport_dims: [16384, 16384],
        point_size_range: [1.0, 8192.0],
        line_width_range: [1.0, 8192.0],
    }
}

fn nvidia_profile() -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (NVIDIA)".into(),
        gl_renderer: "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060, OpenGL ES 3.2)".into(),
        max_texture_size: 32768,
        max_renderbuffer_size: 32768,
        max_cube_map_size: 32768,
        max_vtx_tex_units: 32,
        max_viewport_dims: [32768, 32768],
        point_size_range: [1.0, 2048.0],
        line_width_range: [1.0, 10.0],
    }
}

/// Mali GPU profile (common on ARM SoCs: Exynos, MediaTek Dimensity).
/// Used as the default fallback on aarch64 when lspci is unavailable.
fn mali_profile() -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (ARM)".into(),
        gl_renderer: "ANGLE (ARM, Mali-G78, OpenGL ES 3.2)".into(),
        max_texture_size: 8192,
        max_renderbuffer_size: 8192,
        max_cube_map_size: 8192,
        max_vtx_tex_units: 16,
        max_viewport_dims: [8192, 8192],
        point_size_range: [1.0, 1024.0],
        line_width_range: [1.0, 1024.0],
    }
}

/// Adreno GPU profile (Qualcomm Snapdragon SoCs).
fn adreno_profile() -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (Qualcomm)".into(),
        gl_renderer: "ANGLE (Qualcomm, Adreno (TM) 730, OpenGL ES 3.2)".into(),
        max_texture_size: 16384,
        max_renderbuffer_size: 16384,
        max_cube_map_size: 16384,
        max_vtx_tex_units: 16,
        max_viewport_dims: [16384, 16384],
        point_size_range: [1.0, 1024.0],
        line_width_range: [1.0, 1024.0],
    }
}

/// Build GL spoof JS for the main page (has _lie from STEALTH_CORE).
fn build_gl_spoof(p: &GpuProfile) -> String {
    format!(
        r#"try{{
var _fv='{vendor}',_fr='{renderer}';
var _glLimits={{3379:{mts},34024:{mrs},34076:{mcs},35661:{mvtu}}};
var _glVP=new Int32Array([{vp0},{vp1}]);
var _glPtR=new Float32Array([{psr0},{psr1}]);
var _glLwR=new Float32Array([{lwr0},{lwr1}]);
function _mkGP(proto){{
  var orig=proto.getParameter;
  var f=function getParameter(p){{
    if(p===37445)return _fv;
    if(p===37446)return _fr;
    if(p===3386)return _glVP;
    if(p===33902)return _glPtR;
    if(p===33901)return _glLwR;
    if(_glLimits[p]!==undefined)return _glLimits[p];
    return orig.call(this,p);
  }};
  try{{Object.defineProperty(proto,'getParameter',{{value:f,writable:true,configurable:true,enumerable:true}});_lie(f,'getParameter');}}catch(e){{}}
}}
_mkGP(WebGLRenderingContext.prototype);
if(typeof WebGL2RenderingContext!=='undefined'){{_mkGP(WebGL2RenderingContext.prototype);}}
}}catch(e){{}}"#,
        vendor = p.gl_vendor,
        renderer = p.gl_renderer,
        mts = p.max_texture_size,
        mrs = p.max_renderbuffer_size,
        mcs = p.max_cube_map_size,
        mvtu = p.max_vtx_tex_units,
        vp0 = p.max_viewport_dims[0],
        vp1 = p.max_viewport_dims[1],
        psr0 = p.point_size_range[0],
        psr1 = p.point_size_range[1],
        lwr0 = p.line_width_range[0],
        lwr1 = p.line_width_range[1],
    )
}

/// Build GL spoof JS for Worker contexts (self-contained, no _lie).
fn build_worker_gl_spoof(p: &GpuProfile, locale: Option<&str>) -> String {
    let locale_patch = if let Some(l) = locale {
        let base = l.split('-').next().unwrap_or(l);
        format!(r#"
try{{
  if(typeof WorkerNavigator!=='undefined'){{
    var _wl='{l}';var _wls=['{l}','{base}'];
    Object.defineProperty(WorkerNavigator.prototype,'language',{{get:function(){{return _wl;}},configurable:true,enumerable:true}});
    Object.defineProperty(WorkerNavigator.prototype,'languages',{{get:function(){{return _wls;}},configurable:true,enumerable:true}});
  }}
}}catch(e){{}}
"#, l=l, base=base)
    } else {
        String::new()
    };

    format!(
        r#"try{{
var _fv='{vendor}',_fr='{renderer}';
var _glLimits={{3379:{mts},34024:{mrs},34076:{mcs},35661:{mvtu}}};
var _glVP=new Int32Array([{vp0},{vp1}]);
var _glPtR=new Float32Array([{psr0},{psr1}]);
var _glLwR=new Float32Array([{lwr0},{lwr1}]);
function _mkGP(proto){{
  var orig=proto.getParameter;
  var f=function getParameter(p){{
    if(p===37445)return _fv;
    if(p===37446)return _fr;
    if(p===3386)return _glVP;
    if(p===33902)return _glPtR;
    if(p===33901)return _glLwR;
    if(_glLimits[p]!==undefined)return _glLimits[p];
    return orig.call(this,p);
  }};
  _wmasked.push(f);
  try{{Object.defineProperty(proto,'getParameter',{{value:f,writable:true,configurable:true,enumerable:true}});}}catch(e){{}}
}}
var _wmasked=[];
if(typeof WebGLRenderingContext!=='undefined')_mkGP(WebGLRenderingContext.prototype);
if(typeof WebGL2RenderingContext!=='undefined')_mkGP(WebGL2RenderingContext.prototype);
// toString mask: workers get a pristine Function.prototype.toString,
// so without this the patched getParameter stringifies as JS source —
// exactly the check Pixelscan/Creepjs run in workers to catch masking.
try{{
var _wots=Function.prototype.toString;
Function.prototype.toString=function toString(){{
  for(var _wi=0;_wi<_wmasked.length;_wi++){{if(this===_wmasked[_wi])return'function getParameter() {{ [native code] }}';}}
  return _wots.apply(this,arguments);
}};
}}catch(e){{}}
}}catch(e){{}}
{locale_patch}"#,
        vendor = p.gl_vendor,
        renderer = p.gl_renderer,
        mts = p.max_texture_size,
        mrs = p.max_renderbuffer_size,
        mcs = p.max_cube_map_size,
        mvtu = p.max_vtx_tex_units,
        vp0 = p.max_viewport_dims[0],
        vp1 = p.max_viewport_dims[1],
        psr0 = p.point_size_range[0],
        psr1 = p.point_size_range[1],
        lwr0 = p.line_width_range[0],
        lwr1 = p.line_width_range[1],
        locale_patch = locale_patch,
    )
}

/// Media devices patch — applied ONLY when the real machine reports zero
/// devices (a server tell; real desktops always have audio in/out). Returns
/// the exact pre-permission shape of real Chrome: devices present, ids and
/// labels empty. Passthrough whenever real devices exist.
const MEDIA_PATCH: &str = r#"
// mediaDevices: a machine with zero audio/video devices is a server tell.
try{
var _mdp=(typeof MediaDevices!=='undefined'&&MediaDevices.prototype)?MediaDevices.prototype:Object.getPrototypeOf(navigator.mediaDevices);
if(_mdp){
  var _ed=_mdp.enumerateDevices;
  var _fed=function enumerateDevices(){
    return _ed.call(this).then(function(d){
      if(d&&d.length>0)return d;
      function mk(k){var o={deviceId:'',kind:k,label:'',groupId:''};o.toJSON=function toJSON(){return{deviceId:'',kind:k,label:'',groupId:''};};_lie(o.toJSON,'toJSON');return o;}
      return[mk('audioinput'),mk('audiooutput')];
    });
  };
  try{Object.defineProperty(_mdp,'enumerateDevices',{value:_fed,writable:true,configurable:true,enumerable:true});}catch(e){_mdp.enumerateDevices=_fed;}
  _lie(_fed,'enumerateDevices');
}
}catch(e){}
"#;

/// Locale override — navigator.language/languages must match BLADE_LOCALE.
/// Applied when BLADE_LOCALE is set (S6: geo-consistent identity).
const LOCALE_OVERRIDE: &str = r#"
// S6: navigator.language consistency with timezone/locale.
try{
  var _lang='__LOCALE__';
  var _langs=['__LOCALE__','__LOCALE_BASE__'];
  _defGet(Navigator.prototype,'language',function language(){return _lang;});
  _defGet(Navigator.prototype,'languages',function languages(){return _langs;});
}catch(e){}
"#;

/// Seeded canvas+audio noise block — opt-in via BLADE_NOISE=1 (D14: noise
/// injection is ML-detectable as browser tampering on FingerprintJS-class
/// detectors; real hardware fingerprints are stable and coherent without it).
const NOISE: &str = r#"
// Seeded canvas noise (stable per session — random-per-load is itself a signal).
try{
var _otd=HTMLCanvasElement.prototype.toDataURL;var _ogi=CanvasRenderingContext2D.prototype.getImageData;var _m=new WeakMap();
var _tdu=function toDataURL(){if(!_m.has(this)){try{var c=this.getContext('2d');if(c&&this.width>0&&this.height>0){var px=_ogi.call(c,0,0,1,1);px.data[0]=(px.data[0]+cn)%256;px.data[1]=(px.data[1]+cn2)%256;c.putImageData(px,0,0);_m.set(this,true);}}catch(e){}}return _otd.apply(this,arguments);};
try{Object.defineProperty(HTMLCanvasElement.prototype,'toDataURL',{value:_tdu,writable:true,configurable:true,enumerable:true});}catch(e){HTMLCanvasElement.prototype.toDataURL=_tdu;}
_lie(_tdu,'toDataURL');
}catch(e){}

// Seeded audio noise.
try{
var _ogcd=AudioBuffer.prototype.getChannelData;
var _gcd=function getChannelData(){var d=_ogcd.apply(this,arguments);if(d.length>0){d[0]+=an*1e-7;}return d;};
try{Object.defineProperty(AudioBuffer.prototype,'getChannelData',{value:_gcd,writable:true,configurable:true,enumerable:true});}catch(e){AudioBuffer.prototype.getChannelData=_gcd;}
_lie(_gcd,'getChannelData');
}catch(e){}
"#;

/// WebRTC ICE filtering — applied ONLY when BLADE_PROXY is set. Without a
/// proxy, stripping candidates breaks legit WebRTC for zero privacy gain
/// (the page learns the same IP from HTTP; host candidates are mDNS-
/// obfuscated by Chrome itself). With a proxy, srflx/host candidates
/// would leak the real IP, so they are filtered here and by the
/// `--force-webrtc-ip-handling-policy` launch flag.
const RTC_PATCH: &str = r#"
try{
if(typeof RTCPeerConnection!=='undefined'){
  var _origRTC=RTCPeerConnection.prototype;
  var _origCreateOffer=_origRTC.createOffer;
  var _origCreateAnswer=_origRTC.createAnswer;
  function _filterSDP(sdp){
    if(!sdp)return sdp;
    return sdp.replace(/a=candidate:[^\r\n]*typ host[^\r\n]*/g,'').replace(/a=candidate:[^\r\n]*typ srflx[^\r\n]*/g,'');
  }
  var _co=function createOffer(opts){
    return _origCreateOffer.call(this,opts).then(function(offer){
      if(offer&&offer.sdp){offer.sdp=_filterSDP(offer.sdp);}
      return offer;
    });
  };
  var _ca=function createAnswer(opts){
    return _origCreateAnswer.call(this,opts).then(function(answer){
      if(answer&&answer.sdp){answer.sdp=_filterSDP(answer.sdp);}
      return answer;
    });
  };
  var _origAIC=_origRTC.addIceCandidate;
  var _aic=function addIceCandidate(candidate){
    if(candidate&&candidate.candidate){
      if(candidate.candidate.indexOf('typ host')!==-1){return Promise.resolve();}
    }
    return _origAIC.call(this,candidate);
  };
  try{Object.defineProperty(_origRTC,'createOffer',{value:_co,writable:true,configurable:true,enumerable:true});}catch(e){}
  try{Object.defineProperty(_origRTC,'createAnswer',{value:_ca,writable:true,configurable:true,enumerable:true});}catch(e){}
  try{Object.defineProperty(_origRTC,'addIceCandidate',{value:_aic,writable:true,configurable:true,enumerable:true});}catch(e){}
  _lie(_co,'createOffer');
  _lie(_ca,'createAnswer');
  _lie(_aic,'addIceCandidate');
}
}catch(e){}
"#;

/// What the attach-time environment probe learned about the real machine.
struct EnvProbe {
    gl_renderer: Option<String>,
    media_devices: i64,
}

/// Probe the REAL environment of the current page before any spoofing is
/// registered: WebGL renderer + media device count. One evaluate round-trip.
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
            let val = v.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
            EnvProbe {
                gl_renderer: val.get("gl").and_then(|g| g.as_str()).map(|s| s.to_string()),
                media_devices: val.get("media").and_then(|m| m.as_i64()).unwrap_or(-1),
            }
        }
        Err(_) => EnvProbe { gl_renderer: None, media_devices: -1 },
    }
}

/// True when a GL renderer string is a headless/server artifact that must be hidden.
fn is_software_gl(renderer: &str) -> bool {
    let r = renderer.to_lowercase();
    r.contains("swiftshader") || r.contains("llvmpipe") || r.contains("softpipe") || r.contains("software")
}

/// Legacy alias kept for external references (doc/examples).
pub const STEALTH_SCRIPT_TEMPLATE: &str = STEALTH_CORE;

/// Apply the stealth script to a CDP client via `Page.addScriptToEvaluateOnNewDocument`.
/// Adaptive: probes the real WebGL renderer first; the GL spoof is only
/// registered when the real value is a software/headless artifact (D14 —
/// coherence over noise; a page-scope-only spoof loses to hasBadWebGL).
/// Canvas/audio noise is opt-in via BLADE_NOISE=1. BLADE_WEBGL=spoof|real
/// forces the GL decision. Returns the script identifier.
pub async fn apply(cdp: &CdpSession, locale_override: Option<&str>) -> Result<ScriptId> {
    // Persistent seed: stable across sessions (canvas/audio fingerprint
    // consistency). Generated once, stored in ~/.blade/.fingerprint.json.
    let seed: u32 = crate::fingerprint::load_or_create_seed();

    // One environment probe drives every adaptive decision (S8/S15).
    let env = probe_environment(cdp).await;

    // Adaptive GL decision.
    let gl_mode = std::env::var("BLADE_WEBGL").unwrap_or_else(|_| "auto".to_string());
    let spoof_gl = match gl_mode.as_str() {
        "spoof" => true,
        "real" => false,
        _ => match &env.gl_renderer {
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
    let noise = std::env::var("BLADE_NOISE").map(|v| v == "1").unwrap_or(false);
    // BCP-47 validate: the locale is interpolated into a JS string literal —
    // a quote or backslash would break out and silently kill the whole
    // stealth injection. Letters, digits, '-', '_' only.
    // Explicit override (per-domain profile, S11) beats the env var.
    let raw_locale = locale_override.map(String::from)
        .or_else(|| std::env::var("BLADE_LOCALE").ok());
    let locale = raw_locale
        .filter(|s| !s.is_empty())
        .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    if std::env::var("BLADE_LOCALE").is_ok() && locale.is_none() {
        eprintln!("[stealth] WARNING: BLADE_LOCALE rejected (invalid characters) — locale override skipped");
    }
    if let Some(ref l) = locale {
        eprintln!("[stealth] registering locale override: {l}");
    }

    let mut script = String::with_capacity(STEALTH_CORE.len() + 2048 + MEDIA_PATCH.len() + LOCALE_OVERRIDE.len() + NOISE.len() + RTC_PATCH.len() + STEALTH_TAIL.len());
    script.push_str(STEALTH_CORE);
    // WebRTC candidate filtering only under a proxy (see RTC_PATCH docs).
    if std::env::var("BLADE_PROXY").map(|v| !v.is_empty()).unwrap_or(false) {
        script.push_str(RTC_PATCH);
    }
    GL_SPOOFED.store(spoof_gl, Ordering::Relaxed);
    if spoof_gl {
        let profile = get_gpu_profile();
        eprintln!("[stealth] registering WebGL spoof: {} (page + worker)", profile.gl_renderer);
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
    if locale.is_some() {
        script.push_str(LOCALE_OVERRIDE);
    }
    if noise {
        script.push_str(NOISE);
    }
    script.push_str(STEALTH_TAIL);
    let script = script.replace("__SEED__", &seed.to_string());
    let script = if let Some(ref l) = locale {
        let base = l.split('-').next().unwrap_or(l).to_string();
        script.replace("__LOCALE__", l).replace("__LOCALE_BASE__", &base)
    } else {
        script
    };

    // Register for the OOPIF auto-attach handler (see FULL_SCRIPT).
    if let Ok(mut guard) = FULL_SCRIPT.write() {
        *guard = script.clone();
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

/// Worker GL spoof script — self-contained (no _lie dependency) for Worker
/// contexts. Returns None when neither the GL spoof is active nor a locale
/// override is set (M11: with BLADE_LOCALE, WorkerNavigator.language must
/// match the main frame or the mismatch is itself a fingerprint).
/// D22: Injected via CDP Target.setAutoAttach into each worker target.
pub fn worker_gl_spoof(locale: Option<&str>) -> Option<String> {
    if !GL_SPOOFED.load(Ordering::Relaxed) {
        if locale.is_some() {
            return Some(format!("try{{{{{}}}}}catch(e){{}}", worker_locale_patch(locale)));
        }
        return None;
    }
    let profile = get_gpu_profile();
    Some(build_worker_gl_spoof(&profile, locale))
}

/// The WorkerNavigator locale patch as a standalone JS statement list.
fn worker_locale_patch(locale: Option<&str>) -> String {
    if let Some(l) = locale {
        let base = l.split('-').next().unwrap_or(l);
        format!(
            r#"if(typeof WorkerNavigator!=='undefined'){{
  var _wl='{l}';var _wls=['{l}','{base}'];
  Object.defineProperty(WorkerNavigator.prototype,'language',{{get:function(){{return _wl;}},configurable:true,enumerable:true}});
  Object.defineProperty(WorkerNavigator.prototype,'languages',{{get:function(){{return _wls;}},configurable:true,enumerable:true}});
}}"#,
            l = l,
            base = base
        )
    } else {
        String::new()
    }
}

/// SharedWorker constructor wrapper for the main page injection.
/// CDP doesn't emit Target.attachedToTarget for shared_worker targets,
/// so we intercept the SharedWorker constructor and inject the GL spoof
/// via a blob URL: <gl_spoof + locale patch + importScripts(original_url)>.
/// Falls back to the original URL for cross-origin workers (importScripts throws).
/// Returns None when GL spoof is not active.
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
    var _SWProxy=function SharedWorker(url,options){{
      try{{
        var full=_swCode+'\nimportScripts('+JSON.stringify(url)+')';
        var blob=new Blob([full],{{type:'application/javascript'}});
        var blobUrl=URL.createObjectURL(blob);
        var sw=new _OrigSW(blobUrl,options);
        setTimeout(function(){{URL.revokeObjectURL(blobUrl);}},10000);
        return sw;
      }}catch(e){{
        return new _OrigSW(url,options);
      }}
    }};
    _SWProxy.prototype=_OrigSW.prototype;
    Object.setPrototypeOf(_SWProxy,_OrigSW);
    _lie(_SWProxy,'SharedWorker');
    Object.defineProperty(window,'SharedWorker',{{value:_SWProxy,writable:true,configurable:true,enumerable:false}});
  }}
}}catch(e){{}}
"#,
        escaped = escaped
    ))
}
