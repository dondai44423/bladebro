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

/// Core block (always applied): seed, proxy-mask helpers (PROXY_HELPERS),
/// cdc_ removal, outer dims, screen geometry, polyfills. `__SEED__` is
/// replaced with a random u32 at injection time (stable per session).
/// Launch flags handle navigator.webdriver, window.chrome, and
/// navigator.plugins — this script never touches them.
///
/// Shared proxy-mask helper block. Used verbatim by the page core and by the
/// worker scripts (via `__PROXY_HELPERS__` / direct prefix) so the two realms
/// can never drift apart.
const PROXY_HELPERS: &str = r#"// --- S10: proxy masks. Install helper contract: `fn(th, a, orig)` gets the
// call-site receiver, the arguments array, and the native target; it must
// delegate to `orig` (or a no-orig placeholder) so the native validation and
// coercion order run before any rewrite. ---
var _PP=typeof Proxy!=='undefined'?Proxy:null;
var _nop={m(){}}.m.bind(null);
function _mk(orig,fn){return new _PP(orig,{apply:function(t,th,a){return fn(th,a,t);}});}
function _mkN(fn){return new _PP(_nop,{apply:function(t,th,a){return fn(th,a,t);}});}
function _fix(p,name,len){try{Object.defineProperty(p,'name',{value:name,configurable:true});Object.defineProperty(p,'length',{value:len,configurable:true});}catch(e){}return p;}
function _ogs(obj,name){try{return Object.getOwnPropertyDescriptor(obj,name).get;}catch(e){return undefined;}}
// WebIDL attributes/methods are enumerable:true on prototypes; interface
// objects (constructors) on window are enumerable:false. A proxy of a native
// accessor keeps the engine's "get " name automatically.
function _defGet(obj,name,g,fn){if(!g)return;try{Object.defineProperty(obj,name,{get:_mk(g,fn),configurable:true,enumerable:true});}catch(e){}}
function _defGetN(obj,name,fn){try{Object.defineProperty(obj,name,{get:_fix(_mkN(fn),'get '+name,0),configurable:true,enumerable:true});}catch(e){}}
function _defFn(obj,name,orig,fn){if(!orig)return;try{Object.defineProperty(obj,name,{value:_mk(orig,fn),writable:true,configurable:true,enumerable:true});}catch(e){}}
function _defFnN(obj,name,fn,len){try{Object.defineProperty(obj,name,{value:_fix(_mkN(fn),name,len||0),writable:true,configurable:true,enumerable:true});}catch(e){}}
function _defCtor(name,fn){try{Object.defineProperty(window,name,{value:_fix(_mkN(fn),name,0),writable:true,configurable:true,enumerable:false});}catch(e){}}
"#;

/// Core block (always applied): seed, proxy-mask helpers, cdc_ removal, outer
/// dims, screen geometry, permissions, polyfills.
/// `__SEED__` is replaced with a random u32 at injection time (stable per
/// session). Launch flags handle navigator.webdriver, window.chrome, and
/// navigator.plugins — this script never touches them.
const STEALTH_CORE: &str = r#"(function(){
var S=__SEED__;
function R(){S^=S<<13;S^=S>>>17;S^=S<<5;return((S>>>0)%256);}
var cn=R()&1,an=R(),cn2=R()&1;

__PROXY_HELPERS__

// cdc_ residue removal (chromedriver artifact — belt and suspenders).
var p=Object.getOwnPropertyNames(document).concat(Object.getOwnPropertyNames(window));
for(var i=0;i<p.length;i++){if(p[i].indexOf('cdc_')===0){try{delete document[p[i]];delete window[p[i]];}catch(e){}}}

// Window outer dims. Xvfb has no window manager, so the native outerWidth
// collapses to innerWidth (a 0 diff is a bot tell: real desktops reserve
// side chrome). Modern Chrome (129+) exposes outerWidth as an OWN
// configurable accessor on the window INSTANCE — a Window.prototype
// override is shadowed and never read. Only correct the tell: when the diff
// is already positive (real WM), the natural value is coherent and stays
// native (coherence over noise, D14). outerHeight is left native: its
// natural diff (title/tab bar, here ~143px) is realistic.
try{
var _w0=window.outerWidth,_i0=window.innerWidth;
if(_w0-_i0<=0){
var _mow=function(th,a,og){og.apply(th,a);return window.innerWidth+16;};
var _ow=Object.getOwnPropertyDescriptor(window,'outerWidth');
if(_ow&&_ow.configurable&&_ow.get){_defGet(window,'outerWidth',_ow.get,_mow);}
else{var _wp=(typeof Window!=='undefined'&&Window.prototype)?Window.prototype:Object.getPrototypeOf(window);_defGet(_wp,'outerWidth',_ogs(_wp,'outerWidth'),_mow);}
}
}catch(e){}

// Screen geometry on Screen.prototype (real location, not the instance).
// Only the *incoherent* case is corrected: a window manager gives the screen
// a real work area (availHeight < height) and that native value then stays
// untouched (D14 — coherence over noise). availWidth is left native always:
// a horizontal panel keeps it equal to width, and a narrow one is an honest
// side dock — masking it would hide a real desk, not a tell.
try{
var _sp=(typeof Screen!=='undefined'&&Screen.prototype)?Screen.prototype:Object.getPrototypeOf(screen);
if(screen.height-screen.availHeight<16){_defGet(_sp,'availHeight',_ogs(_sp,'availHeight'),function(th,a,og){var v=og.apply(th,a);return (screen.height-v<16)?(screen.height-40):v;});}
}catch(e){}

// permissions.query rewrite lives in PERMISSIONS_PATCH (apply()): a
// perfect native relay; only a real-origin 'denied' notifications result
// is rewritten (the headless tell). Opaque origins keep native results.

// Web API polyfills (Linux-headless absence signals). Polyfill targets use
// the shared bound placeholder, so a polyfilled method is shaped exactly
// like a native one (no 'prototype', {length,name}, native toString).
try{
var _np=(typeof Navigator!=='undefined'&&Navigator.prototype)?Navigator.prototype:Object.getPrototypeOf(navigator);
if(!('share' in navigator)){_defFnN(_np,'share',function(){return Promise.reject(new TypeError('Not supported'));},1);}
if(!('canShare' in navigator)){_defFnN(_np,'canShare',function(){return false;},1);}
if(!('ContentIndex' in window)){_defCtor('ContentIndex',function(){throw new TypeError('Illegal constructor');});}
if(!('ContactsManager' in window)){_defCtor('ContactsManager',function(){throw new TypeError('Illegal constructor');});}
if(navigator.connection){
  var _pr=Object.getPrototypeOf(navigator.connection);
  if(_pr&&!('downlinkMax' in _pr)){_defGetN(_pr,'downlinkMax',function(){return Infinity;});}
  if(typeof window.NetworkInformation==='undefined'){try{Object.defineProperty(window,'NetworkInformation',{value:navigator.connection.constructor,writable:true,configurable:true,enumerable:false});}catch(e){}}
}
}catch(e){}

// ── Additional stealth layers ───────────────────────────────

// window.chrome object: stock Chromium exposes chrome.app, chrome.csi(),
// chrome.loadTimes() — verified headful AND headless via the differential
// oracle. Polyfill only what is genuinely missing; chrome.runtime is NOT
// exposed by this engine, so adding it was a deviation, not coverage
// (removed v3.9.12; re-check with tools/diff_oracle when the engine moves).
try{
if(!window.chrome){window.chrome={};}
if(!window.chrome.app){window.chrome.app={isInstalled:false,InstallState:{DISABLED:'disabled',INSTALLED:'installed',NOT_INSTALLED:'not_installed'},RunningState:{CANNOT_RUN:'cannot_run',READY_TO_RUN:'ready_to_run',RUNNING:'running'}};}
if(!window.chrome.csi){window.chrome.csi=_fix(_mkN(function(){return{startE:Date.now(),onloadT:Date.now()+100,pageT:1000,tran:15};}),'csi',0);}
if(!window.chrome.loadTimes){window.chrome.loadTimes=_fix(_mkN(function(){return{commitLoadTime:Date.now()/1000,connectionInfo:'h2',finishDocumentLoadTime:Date.now()/1000+0.1,finishLoadTime:Date.now()/1000+0.2,firstPaintAfterLoadTime:0,firstPaintTime:Date.now()/1000+0.05,navigationType:'Other',npnNegotiatedProtocol:'h2',requestTime:Date.now()/1000-0.5,startLoadTime:Date.now()/1000-0.5,wasAlternateProtocolAvailable:false,wasFetchedViaSpdy:true,wasNpnNegotiated:true};}),'loadTimes',0);}
}catch(e){}

// Speech synthesis voices: NO PATCH (removed v3.9.12). The fixed 3-voice
// Google set was a cluster signature shared by every Bladebro profile;
// the machine truth (0 voices without speech-dispatcher, the real list
// with it) is coherent and unremarkable — the differential oracle shows
// stock Chrome on the same box reports 0.

// Battery API: headless may not have navigator.getBattery.
try{
if(!navigator.getBattery){
  _defFnN(Navigator.prototype,'getBattery',function(){return Promise.resolve({charging:true,chargingTime:0,dischargingTime:Infinity,level:1,onchargingchange:null,onchargingtimechange:null,ondischargingtimechange:null,onlevelchange:null});},0);
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
  _defGet(_dp,'visibilityState',_ogs(_dp,'visibilityState'),function(th,a,og){og.apply(th,a);return 'visible';});
  _defGet(_dp,'hidden',_ogs(_dp,'hidden'),function(th,a,og){og.apply(th,a);return false;});
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
  _defGet(Notification,'permission',_ogs(Notification,'permission'),function(th,a,og){og.apply(th,a);return 'default';});
}
}catch(e){}

// navigator.pdfViewerEnabled: real Chrome has this as true.
// --disable-features=PdfPlugin makes it false (for download support),
// so patch it back to true to maintain the fingerprint.
try{
if(navigator.pdfViewerEnabled===false){
  _defGet(Navigator.prototype,'pdfViewerEnabled',_ogs(Navigator.prototype,'pdfViewerEnabled'),function(th,a,og){og.apply(th,a);return true;});
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
    if(!('effectiveType' in _cp))_defGetN(_cp,'effectiveType',function(){return '4g';});
    if(!('rtt' in _cp))_defGetN(_cp,'rtt',function(){return 50;});
    if(!('downlink' in _cp))_defGetN(_cp,'downlink',function(){return 10;});
    if(!('saveData' in _cp))_defGetN(_cp,'saveData',function(){return false;});
  }
}
}catch(e){}

// navigator.scheduling: Chrome has the Scheduling API.
// Use getter (not data property) — real Chrome exposes it as an accessor.
try{
if(!navigator.scheduling&&!('scheduling' in navigator)){
  var _sch={isInputPending:_fix(_mkN(function(){return false;}),'isInputPending',0),isInputPendingOrAvailable:_fix(_mkN(function(){return false;}),'isInputPendingOrAvailable',0)};
  _defGetN(Navigator.prototype,'scheduling',function(){return _sch;});
}
}catch(e){}

// navigator.cookieEnabled: should be true (Chrome default).
try{
if(!navigator.cookieEnabled){
  _defGet(Navigator.prototype,'cookieEnabled',_ogs(Navigator.prototype,'cookieEnabled'),function(th,a,og){og.apply(th,a);return true;});
}
}catch(e){}

try{
var _sp2=(typeof Screen!=='undefined'&&Screen.prototype)?Screen.prototype:Object.getPrototypeOf(screen);
if(screen.colorDepth!==24){_defGet(_sp2,'colorDepth',_ogs(_sp2,'colorDepth'),function(th,a,og){og.apply(th,a);return 24;});}
if(screen.pixelDepth!==24){_defGet(_sp2,'pixelDepth',_ogs(_sp2,'pixelDepth'),function(th,a,og){og.apply(th,a);return 24;});}
}catch(e){}

// V8: console capture for driver introspection (see logs=console).
// Chrome 151 owns the console methods on the INSTANCE (Console.prototype has
// no 'log' — verified against stock), so install where the method actually
// lives: adding a prototype property stock doesn't have is itself a
// differential. The hook is a proxy over the native method — masked in every
// realm, native receiver/argument semantics preserved. The ring buffer lives
// under a Symbol-keyed NON-ENUMERABLE window slot: invisible to for-in,
// Object.keys, and getOwnPropertyNames. 200 entries, resets per document.
try{
var _uk=Symbol.for('q');
var _uxa=[];
function _uxp(l,a){try{var p=[];for(var i=0;i<a.length;i++){var v=a[i];try{p.push(typeof v==='string'?v:JSON.stringify(v));}catch(e){p.push(String(v));}}_uxa.push({l:l,m:p.join(' ').slice(0,400),t:Date.now()});if(_uxa.length>200)_uxa.shift();}catch(e){}}
['log','info','warn','error','debug'].forEach(function(m){
  var _cp=Object.getPrototypeOf(console);
  var _own=Object.getOwnPropertyDescriptor(console,m);
  var _host=_own?console:_cp;
  var _o=_own?_own.value:_cp[m];
  if(typeof _o!=='function')return;
  try{Object.defineProperty(_host,m,{value:_fix(_mk(_o,function(th,a,og){_uxp(m,a);return og.apply(console,a);}),m,_o.length),writable:true,configurable:true,enumerable:true});}catch(e){}
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

/// Real-hardware extension lists captured from this machine's Intel i915
/// (ADL GT2, Mesa, Chrome 151): WebGL1 = 36 entries, WebGL2 = 32 entries.
/// Used to filter the software backend's (llvmpipe) supersets down to the
/// claimed GPU's surface — llvmpipe adds EXT_shader_texture_lod and
/// WEBGL_polygon_mode on WebGL1, and OVR_multiview2, WEBGL_polygon_mode and
/// WEBGL_provoking_vertex on WebGL2. Every nominal entry is supported by
/// llvmpipe, so filtering never over-claims.
const INTEL_EXT1_NOMINAL: &[&str] = &[
    "ANGLE_instanced_arrays",
    "EXT_blend_minmax",
    "EXT_clip_control",
    "EXT_color_buffer_half_float",
    "EXT_depth_clamp",
    "EXT_disjoint_timer_query",
    "EXT_float_blend",
    "EXT_frag_depth",
    "EXT_polygon_offset_clamp",
    "EXT_sRGB",
    "EXT_texture_compression_bptc",
    "EXT_texture_compression_rgtc",
    "EXT_texture_filter_anisotropic",
    "EXT_texture_mirror_clamp_to_edge",
    "KHR_parallel_shader_compile",
    "OES_element_index_uint",
    "OES_fbo_render_mipmap",
    "OES_standard_derivatives",
    "OES_texture_float",
    "OES_texture_float_linear",
    "OES_texture_half_float",
    "OES_texture_half_float_linear",
    "OES_vertex_array_object",
    "WEBGL_blend_func_extended",
    "WEBGL_color_buffer_float",
    "WEBGL_compressed_texture_astc",
    "WEBGL_compressed_texture_etc",
    "WEBGL_compressed_texture_etc1",
    "WEBGL_compressed_texture_s3tc",
    "WEBGL_compressed_texture_s3tc_srgb",
    "WEBGL_debug_renderer_info",
    "WEBGL_debug_shaders",
    "WEBGL_depth_texture",
    "WEBGL_draw_buffers",
    "WEBGL_lose_context",
    "WEBGL_multi_draw",
];

const INTEL_EXT2_NOMINAL: &[&str] = &[
    "EXT_clip_control",
    "EXT_color_buffer_float",
    "EXT_color_buffer_half_float",
    "EXT_conservative_depth",
    "EXT_depth_clamp",
    "EXT_disjoint_timer_query_webgl2",
    "EXT_float_blend",
    "EXT_polygon_offset_clamp",
    "EXT_render_snorm",
    "EXT_texture_compression_bptc",
    "EXT_texture_compression_rgtc",
    "EXT_texture_filter_anisotropic",
    "EXT_texture_mirror_clamp_to_edge",
    "EXT_texture_norm16",
    "KHR_parallel_shader_compile",
    "NV_shader_noperspective_interpolation",
    "OES_draw_buffers_indexed",
    "OES_sample_variables",
    "OES_shader_multisample_interpolation",
    "OES_texture_float_linear",
    "WEBGL_blend_func_extended",
    "WEBGL_clip_cull_distance",
    "WEBGL_compressed_texture_astc",
    "WEBGL_compressed_texture_etc",
    "WEBGL_compressed_texture_etc1",
    "WEBGL_compressed_texture_s3tc",
    "WEBGL_compressed_texture_s3tc_srgb",
    "WEBGL_debug_renderer_info",
    "WEBGL_debug_shaders",
    "WEBGL_lose_context",
    "WEBGL_multi_draw",
    "WEBGL_stencil_texturing",
];

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
    max_combined_tex_units: i32, // 35661 (MAX_COMBINED_TEXTURE_IMAGE_UNITS)
    max_samples: i32,            // 36183 (MAX_SAMPLES, WebGL2)
    max_viewport_dims: [i32; 2], // 3386
    point_size_range: [f32; 2],  // 33902
    line_width_range: [f32; 2],  // 33901
    /// i915/ANGLE reports HIGH-class precision values for every level; when
    /// true the mask remaps MEDIUM/LOW precision queries onto the HIGH
    /// query (argument remap — the returned object stays a genuine native
    /// WebGLShaderPrecisionFormat).
    precision_full: bool,
    /// Nominal WebGL1/WebGL2 extension lists captured from real hardware
    /// (Intel ADL GT2, Mesa/Chrome 151). The mask filters the software
    /// backend's superset down to the claimed GPU's surface. None = no filter.
    ext1_nominal: Option<&'static [&'static str]>,
    ext2_nominal: Option<&'static [&'static str]>,
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
                return Some(intel_profile("Mesa Intel(R) Graphics (ADL GT2)"));
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
        max_combined_tex_units: 64,
        max_samples: 16,
        max_viewport_dims: [16384, 16384],
        point_size_range: [1.0, 255.0],
        line_width_range: [1.0, 1024.0],
        precision_full: true,
        ext1_nominal: Some(INTEL_EXT1_NOMINAL),
        ext2_nominal: Some(INTEL_EXT2_NOMINAL),
    }
}

fn amd_profile() -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (AMD)".into(),
        gl_renderer: "ANGLE (AMD, Mesa AMD Radeon RX 6700 XT (navi22, LLVM 15.0.7), OpenGL ES 3.2)".into(),
        max_texture_size: 16384,
        max_renderbuffer_size: 16384,
        max_cube_map_size: 16384,
        max_combined_tex_units: 64,
        max_samples: 16,
        max_viewport_dims: [16384, 16384],
        point_size_range: [1.0, 8192.0],
        line_width_range: [1.0, 8192.0],
        precision_full: false,
        ext1_nominal: None,
        ext2_nominal: None,
    }
}

fn nvidia_profile() -> GpuProfile {
    GpuProfile {
        gl_vendor: "Google Inc. (NVIDIA)".into(),
        gl_renderer: "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060, OpenGL ES 3.2)".into(),
        max_texture_size: 32768,
        max_renderbuffer_size: 32768,
        max_cube_map_size: 32768,
        max_combined_tex_units: 64,
        max_samples: 16,
        max_viewport_dims: [32768, 32768],
        point_size_range: [1.0, 2048.0],
        line_width_range: [1.0, 10.0],
        precision_full: false,
        ext1_nominal: None,
        ext2_nominal: None,
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
        max_combined_tex_units: 32,
        max_samples: 4,
        max_viewport_dims: [8192, 8192],
        point_size_range: [1.0, 1024.0],
        line_width_range: [1.0, 1024.0],
        precision_full: false,
        ext1_nominal: None,
        ext2_nominal: None,
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
        max_combined_tex_units: 32,
        max_samples: 4,
        max_viewport_dims: [16384, 16384],
        point_size_range: [1.0, 1024.0],
        line_width_range: [1.0, 1024.0],
        precision_full: false,
        ext1_nominal: None,
        ext2_nominal: None,
    }
}

/// Build GL spoof JS for the main page. Installed methods are Proxies over
/// the native ones (S10): V8 stringifies a proxy as native in every realm and
/// delegation runs first, so receiver/argument semantics stay byte-native.
/// Coherence additions (v3.9.12, captured against real i915 hardware):
/// - MAX_COMBINED_TEXTURE_IMAGE_UNITS (35661) and MAX_SAMPLES (36183) pinned
///   to the real GPU's values (llvmpipe says 16/8 where i915 says 64/16).
/// - getShaderPrecisionFormat remap: i915 answers [127,127,23] / [31,30,0]
///   at every level; llvmpipe distinguishes medium/low — remapped onto the
///   HIGH query so the returned object stays a genuine native one.
/// - Extension lists filtered to the claimed GPU's real sets.
fn build_gl_spoof(p: &GpuProfile) -> String {
    let precision_patch = precision_remap_patch(p.precision_full);
    let ext_patch = build_ext_patch(p.ext1_nominal, p.ext2_nominal);
    format!(
        r#"try{{
var _fv='{vendor}',_fr='{renderer}';
var _glLimits={{3379:{mts},34024:{mrs},34076:{mcs},35661:{mctu},36183:{msamples}}};
var _glVP=new Int32Array([{vp0},{vp1}]);
var _glPtR=new Float32Array([{psr0},{psr1}]);
var _glLwR=new Float32Array([{lwr0},{lwr1}]);
function _mkGP(proto){{
  var orig=proto.getParameter;
  _defFn(proto,'getParameter',orig,function(th,a,og){{
    var r=og.apply(th,a);
    var p=a[0];
    if(p===37445)return _fv;
    if(p===37446)return _fr;
    if(p===3386)return _glVP;
    if(p===33902)return _glPtR;
    if(p===33901)return _glLwR;
    if(_glLimits[p]!==undefined)return _glLimits[p];
    return r;
  }});
}}
_mkGP(WebGLRenderingContext.prototype);
if(typeof WebGL2RenderingContext!=='undefined'){{_mkGP(WebGL2RenderingContext.prototype);}}
{precision_patch}
{ext_patch}
}}catch(e){{}}"#,
        vendor = p.gl_vendor,
        renderer = p.gl_renderer,
        mts = p.max_texture_size,
        mrs = p.max_renderbuffer_size,
        mcs = p.max_cube_map_size,
        mctu = p.max_combined_tex_units,
        msamples = p.max_samples,
        vp0 = p.max_viewport_dims[0],
        vp1 = p.max_viewport_dims[1],
        psr0 = p.point_size_range[0],
        psr1 = p.point_size_range[1],
        lwr0 = p.line_width_range[0],
        lwr1 = p.line_width_range[1],
        precision_patch = precision_patch,
        ext_patch = ext_patch,
    )
}

/// Precision-format remap for a hardware backend that answers HIGH-class
/// precision at every level (i915/ANGLE): MEDIUM/LOW float queries are
/// answered from the HIGH_FLOAT query, INT likewise from HIGH_INT — via
/// argument remap, so the returned value is a genuine native
/// WebGLShaderPrecisionFormat. Zero-arg / wrong-receiver semantics stay
/// byte-native (slice + apply, native validation runs first).
fn precision_remap_patch(enabled: bool) -> String {
    if !enabled {
        return String::new();
    }
    r#"
function _mkPrec(proto){
  var orig=proto.getShaderPrecisionFormat;
  _defFn(proto,'getShaderPrecisionFormat',orig,function(th,a,og){
    var b=Array.prototype.slice.call(a);
    if(b.length>1){
      if(b[1]===36337||b[1]===36336)b[1]=36338;
      if(b[1]===36340||b[1]===36339)b[1]=36341;
    }
    return og.apply(th,b);
  });
}
_mkPrec(WebGLRenderingContext.prototype);
if(typeof WebGL2RenderingContext!=='undefined'){_mkPrec(WebGL2RenderingContext.prototype);}
"#
        .to_string()
}

/// Extension-list mask: filter the software backend's lists down to the
/// claimed GPU's real sets. getSupportedExtensions returns a fresh filtered
/// array each call (native semantics); getExtension returns null for
/// anything outside the nominal set — checked AFTER the native call, so
/// error/receiver/coercion semantics stay native. Surviving entries keep the
/// backend's own order (plausible as any driver's list).
fn build_ext_patch(
    ext1: Option<&'static [&'static str]>,
    ext2: Option<&'static [&'static str]>,
) -> String {
    if ext1.is_none() && ext2.is_none() {
        return String::new();
    }
    let mk_list = |l: &[&str]| -> String {
        let mut s = String::from("[");
        for (i, n) in l.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('\'');
            s.push_str(n);
            s.push('\'');
        }
        s.push(']');
        s
    };
    let l1 = ext1.map(mk_list).unwrap_or_else(|| "null".into());
    let l2 = ext2.map(mk_list).unwrap_or_else(|| "null".into());
    format!(
        r#"
function _mkExts(proto,list){{
  var set={{}};for(var i=0;i<list.length;i++)set[list[i]]=1;
  var oge=proto.getSupportedExtensions;
  _defFn(proto,'getSupportedExtensions',oge,function(th,a,og){{
    var r=og.apply(th,a);
    if(!r||typeof r.length!=='number')return r;
    var out=[];for(var j=0;j<r.length;j++){{if(set[r[j]])out.push(r[j]);}}
    return out;
  }});
  var ogx=proto.getExtension;
  _defFn(proto,'getExtension',ogx,function(th,a,og){{
    var r=og.apply(th,a);
    if(r!=null&&a.length>0&&!set[a[0]])return null;
    return r;
  }});
}}
var _ex1={l1};
if(_ex1)_mkExts(WebGLRenderingContext.prototype,_ex1);
var _ex2={l2};
if(_ex2&&typeof WebGL2RenderingContext!=='undefined')_mkExts(WebGL2RenderingContext.prototype,_ex2);
"#,
        l1 = l1,
        l2 = l2,
    )
}

/// Build GL spoof JS for Worker contexts. Same proxy architecture as the page
/// (PROXY_HELPERS is the shared helper block), so a worker realm — where the
/// old code needed its own toString mask — has nothing left to patch.
fn build_worker_gl_spoof(p: &GpuProfile, locale: Option<&str>) -> String {
    let locale_patch = if let Some(l) = locale {
        let base = l.split('-').next().unwrap_or(l);
        format!(r#"
try{{
  if(typeof WorkerNavigator!=='undefined'){{
    var _wl='{l}';var _wls=['{l}','{base}'];
    _defGet(WorkerNavigator.prototype,'language',_ogs(WorkerNavigator.prototype,'language'),function(th,a,og){{og.apply(th,a);return _wl;}});
    _defGet(WorkerNavigator.prototype,'languages',_ogs(WorkerNavigator.prototype,'languages'),function(th,a,og){{og.apply(th,a);return _wls;}});
  }}
}}catch(e){{}}
"#, l=l, base=base)
    } else {
        String::new()
    };

    let precision_patch = precision_remap_patch(p.precision_full);
    let ext_patch = build_ext_patch(p.ext1_nominal, p.ext2_nominal);
    format!(
        r#"{helpers}
try{{
var _fv='{vendor}',_fr='{renderer}';
var _glLimits={{3379:{mts},34024:{mrs},34076:{mcs},35661:{mctu},36183:{msamples}}};
var _glVP=new Int32Array([{vp0},{vp1}]);
var _glPtR=new Float32Array([{psr0},{psr1}]);
var _glLwR=new Float32Array([{lwr0},{lwr1}]);
function _mkGP(proto){{
  var orig=proto.getParameter;
  _defFn(proto,'getParameter',orig,function(th,a,og){{
    var r=og.apply(th,a);
    var p=a[0];
    if(p===37445)return _fv;
    if(p===37446)return _fr;
    if(p===3386)return _glVP;
    if(p===33902)return _glPtR;
    if(p===33901)return _glLwR;
    if(_glLimits[p]!==undefined)return _glLimits[p];
    return r;
  }});
}}
if(typeof WebGLRenderingContext!=='undefined')_mkGP(WebGLRenderingContext.prototype);
if(typeof WebGL2RenderingContext!=='undefined')_mkGP(WebGL2RenderingContext.prototype);
{precision_patch}
{ext_patch}
}}catch(e){{}}
{locale_patch}"#,
        helpers = PROXY_HELPERS,
        vendor = p.gl_vendor,
        renderer = p.gl_renderer,
        mts = p.max_texture_size,
        mrs = p.max_renderbuffer_size,
        mcs = p.max_cube_map_size,
        mctu = p.max_combined_tex_units,
        msamples = p.max_samples,
        vp0 = p.max_viewport_dims[0],
        vp1 = p.max_viewport_dims[1],
        psr0 = p.point_size_range[0],
        psr1 = p.point_size_range[1],
        lwr0 = p.line_width_range[0],
        lwr1 = p.line_width_range[1],
        precision_patch = precision_patch,
        ext_patch = ext_patch,
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
  _defFn(_mdp,'enumerateDevices',_ed,function(th,a,og){
    return og.apply(th,a).then(function(d){
      if(d&&d.length>0)return d;
      function mk(k){var o={deviceId:'',kind:k,label:'',groupId:''};o.toJSON=_fix(_mkN(function(){return{deviceId:'',kind:k,label:'',groupId:''};}),'toJSON',0);return o;}
      return[mk('audioinput'),mk('audiooutput')];
    });
  });
}
}catch(e){}

"#;

/// permissions.query('notifications') rewrite — always installed; the
/// rewrite itself fires only for genuinely-origined documents
/// (`location.origin !== 'null'`) whose result is 'denied'. On opaque
/// origins (about:blank) real Chrome also reports 'denied', so the native
/// result stands; on real origins the only divergent environment
/// (headless-new: 'denied' everywhere) is masked to 'prompt'. The wrapper
/// relays with the caller's exact receiver and arguments — native validation
/// runs first, so zero-arg TypeErrors, non-object TypeErrors, wrong-receiver
/// 'Illegal invocation' and promise timing stay byte-native.
const PERMISSIONS_PATCH: &str = r#"
try{
var _opq=navigator.permissions.query;
var _pp=Object.getPrototypeOf(navigator.permissions);
var _pq=function(th,a,og){
  var p=og.apply(th,a);
  try{
    var d=a.length>0?a[0]:undefined;
    if(d&&typeof d==='object'&&d.name==='notifications'&&location.origin!=='null'){
      return p.then(function(s){
        if(s&&s.state==='denied'){try{Object.defineProperty(s,'state',{value:'prompt',configurable:true});}catch(e){}}
        return s;
      });
    }
  }catch(e){}
  return p;
};
_defFn(_pp,'query',_pp.query||_opq,_pq);
}catch(e){}

"#;

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
const WEBDRIVER_PATCH: &str = r#"
try{
var _wdg=_ogs(Navigator.prototype,'webdriver');
if(_wdg){_defGet(Navigator.prototype,'webdriver',_wdg,function(th,a,og){og.apply(th,a);return false;});}
}catch(e){}
"#;

/// Locale override — navigator.language/languages must match BLADE_LOCALE.
/// Applied when BLADE_LOCALE is set (S6: geo-consistent identity).
const LOCALE_OVERRIDE: &str = r#"
// S6: navigator.language consistency with timezone/locale.
try{
  var _lang='__LOCALE__';
  var _langs=['__LOCALE__','__LOCALE_BASE__'];
  _defGet(Navigator.prototype,'language',_ogs(Navigator.prototype,'language'),function(th,a,og){og.apply(th,a);return _lang;});
  _defGet(Navigator.prototype,'languages',_ogs(Navigator.prototype,'languages'),function(th,a,og){og.apply(th,a);return _langs;});
}catch(e){}

"#;

/// Seeded canvas+audio noise block — opt-in via BLADE_NOISE=1 (D14: noise
/// injection is ML-detectable as browser tampering on FingerprintJS-class
/// detectors; real hardware fingerprints are stable and coherent without it).
const NOISE: &str = r#"
// Seeded canvas noise (stable per session — random-per-load is itself a signal).
try{
var _otd=HTMLCanvasElement.prototype.toDataURL;var _ogi=CanvasRenderingContext2D.prototype.getImageData;var _m=new WeakMap();
_defFn(HTMLCanvasElement.prototype,'toDataURL',_otd,function(th,a,og){
  if(!_m.has(th)){try{var c=th.getContext('2d');if(c&&th.width>0&&th.height>0){var px=_ogi.call(c,0,0,1,1);px.data[0]=(px.data[0]+cn)%256;px.data[1]=(px.data[1]+cn2)%256;c.putImageData(px,0,0);_m.set(th,true);}}catch(e){}}
  return og.apply(th,a);
});
}catch(e){}

// Seeded audio noise.
try{
var _ogcd=AudioBuffer.prototype.getChannelData;
_defFn(AudioBuffer.prototype,'getChannelData',_ogcd,function(th,a,og){var d=og.apply(th,a);if(d.length>0){d[0]+=an*1e-7;}return d;});
}catch(e){}

"#;

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
const RTC_PATCH: &str = r#"
try{
if(typeof RTCPeerConnection!=='undefined'){
  var _origRTC=RTCPeerConnection.prototype;
  // Leaky = names a real address: srflx (STUN-reflexive = the egress IP) or
  // a raw-IP host candidate. mDNS `.local` host candidates are obfuscated
  // and pass; relay and prflx pass.
  function _leakyCand(c){if(!c)return false;if(c.indexOf('typ srflx')!==-1)return true;if(c.indexOf('typ host')!==-1&&c.indexOf('.local')===-1)return true;return false;}
  function _filterSDP(sdp){
    if(!sdp)return sdp;
    return sdp.replace(/a=candidate:[^\r\n]*typ host[^\r\n]*/g,'').replace(/a=candidate:[^\r\n]*typ srflx[^\r\n]*/g,'');
  }
  var _oco=_origRTC.createOffer,_oca=_origRTC.createAnswer,_oaic=_origRTC.addIceCandidate;
  _defFn(_origRTC,'createOffer',_oco,function(th,a,og){
    return og.apply(th,a).then(function(offer){
      if(offer&&offer.sdp){offer.sdp=_filterSDP(offer.sdp);}
      return offer;
    });
  });
  _defFn(_origRTC,'createAnswer',_oca,function(th,a,og){
    return og.apply(th,a).then(function(answer){
      if(answer&&answer.sdp){answer.sdp=_filterSDP(answer.sdp);}
      return answer;
    });
  });
  _defFn(_origRTC,'addIceCandidate',_oaic,function(th,a,og){
    var c=a[0];
    if(c&&c.candidate&&String(c.candidate).indexOf('typ host')!==-1){return Promise.resolve();}
    return og.apply(th,a);
  });
  // Local candidate events — the page's own enumeration vector. Installed at
  // the native locations (EventTarget.prototype already owns add/removeEvent
  // Listener; proxy over the native keeps the descriptor shape and the
  // native-shaped toString) and scoped to RTCPeerConnection receivers.
  var _ael=EventTarget.prototype.addEventListener,_rel=EventTarget.prototype.removeEventListener;
  _defFn(EventTarget.prototype,'addEventListener',_ael,function(th,a,og){
    if(a[0]==='icecandidate'&&typeof a[1]==='function'&&!a[1].__ocWrap&&typeof RTCPeerConnection!=='undefined'&&th instanceof RTCPeerConnection){
      var fn=a[1];
      var wrap=function(ev){if(ev&&ev.candidate&&_leakyCand(ev.candidate.candidate))return;return fn.apply(this,arguments);};
      wrap.__ocWrap=fn;a=Array.prototype.slice.call(a);a[1]=wrap;
    }
    return og.apply(th,a);
  });
  _defFn(EventTarget.prototype,'removeEventListener',_rel,function(th,a,og){
    if(a[0]==='icecandidate'&&typeof a[1]==='function'&&a[1].__ocWrap){a=Array.prototype.slice.call(a);a[1]=a[1].__ocWrap;}
    return og.apply(th,a);
  });
  // onicecandidate handler property (own accessor on the prototype, the
  // native location). The WeakMap keeps `pc.onicecandidate === fn` true for
  // whatever the page set.
  try{
    var _ocd=Object.getOwnPropertyDescriptor(_origRTC,'onicecandidate');
    if(_ocd&&_ocd.get&&_ocd.set){
      var _ocmap=new WeakMap();
      var _ocget=function(th,a,og){var f=_ocmap.get(th);return f!==undefined?f:og.call(th);};
      var _ocset=function(th,a,og){
        var fn=a[0];
        if(typeof fn!=='function'){_ocmap.delete(th);return og.call(th,fn);}
        _ocmap.set(th,fn);
        return og.call(th,function(ev){if(ev&&ev.candidate&&_leakyCand(ev.candidate.candidate))return;return fn.call(this,ev);});
      };
      Object.defineProperty(_origRTC,'onicecandidate',{configurable:true,enumerable:!!_ocd.enumerable,get:_mk(_ocd.get,_ocget),set:_mk(_ocd.set,_ocset)});
    }
  }catch(e){}
}
}catch(e){}

"#;

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
            let val = v.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
            EnvProbe {
                gl_renderer: val.get("gl").and_then(|g| g.as_str()).map(|s| s.to_string()),
                media_devices: val.get("media").and_then(|m| m.as_i64()).unwrap_or(-1),
            }
        }
        Err(_) => EnvProbe { gl_renderer: None, media_devices: -1 },
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
        _ => match crate::browser::gpu_state() {
            Some(crate::browser::GpuState::Hardware(renderer)) => {
                eprintln!("[stealth] GL healthcheck says hardware ({renderer}) — no WebGL spoof");
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

    let mut script = String::with_capacity(STEALTH_CORE.len() + 2048 + MEDIA_PATCH.len() + PERMISSIONS_PATCH.len() + LOCALE_OVERRIDE.len() + NOISE.len() + RTC_PATCH.len() + STEALTH_TAIL.len());
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
        script.replace("__LOCALE__", l).replace("__LOCALE_BASE__", &base)
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
