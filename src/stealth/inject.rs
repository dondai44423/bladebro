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

use crate::cdp::CdpSession;
use crate::error::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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
    max_texture_size: i32,       // 3379 (MAX_TEXTURE_SIZE)
    max_renderbuffer_size: i32,  // 34024
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
            "amd" => {
                eprintln!("[stealth] BLADE_GPU=amd — using AMD Radeon profile");
                return amd_profile();
            }
            "nvidia" => {
                eprintln!("[stealth] BLADE_GPU=nvidia — using NVIDIA GeForce profile");
                return nvidia_profile();
            }
            "mali" => {
                eprintln!("[stealth] BLADE_GPU=mali — using Mali-G78 profile");
                return mali_profile();
            }
            "adreno" => {
                eprintln!("[stealth] BLADE_GPU=adreno — using Adreno 730 profile");
                return adreno_profile();
            }
            _ => {
                // Default override: match the native arch
                #[cfg(target_arch = "aarch64")]
                {
                    eprintln!("[stealth] BLADE_GPU={gpu} — using Mali-G78 profile");
                    return mali_profile();
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    eprintln!("[stealth] BLADE_GPU={gpu} — using Intel UHD 630 profile");
                    return intel_profile("Mesa Intel(R) UHD Graphics 630 (CFL GT2)");
                }
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
    {
        mali_profile()
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        intel_profile("Mesa Intel(R) UHD Graphics 630 (CFL GT2)")
    }
}

#[cfg(target_os = "linux")]
fn detect_gpu_lspci() -> Option<GpuProfile> {
    let output = std::process::Command::new("lspci")
        .arg("-nn")
        .output()
        .ok()?;
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
            if lower.contains("alder lake-s")
                || lower.contains("adl-s")
                || lower.contains("uhd 770")
            {
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
        gl_renderer: "ANGLE (AMD, Mesa AMD Radeon RX 6700 XT (navi22, LLVM 15.0.7), OpenGL ES 3.2)"
            .into(),
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
        format!(
            r#"
try{{
  if(typeof WorkerNavigator!=='undefined'){{
    var _wl='{l}';var _wls=['{l}','{base}'];
    _defGet(WorkerNavigator.prototype,'language',_ogs(WorkerNavigator.prototype,'language'),function(th,a,og){{og.apply(th,a);return _wl;}});
    _defGet(WorkerNavigator.prototype,'languages',_ogs(WorkerNavigator.prototype,'languages'),function(th,a,og){{og.apply(th,a);return _wls;}});
  }}
}}catch(e){{}}
"#,
            l = l,
            base = base
        )
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
