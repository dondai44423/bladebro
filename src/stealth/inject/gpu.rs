//! GPU detection and the WebGL spoof builders: the per-vendor profile
//! tables (Intel / AMD / NVIDIA / Mali / Adreno), `lspci` + env detection,
//! and the page/worker GL spoof script assembly. Split from the `inject`
//! core.

use std::sync::OnceLock;

use super::PROXY_HELPERS;

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
pub(super) struct GpuProfile {
    gl_vendor: String,
    pub(super) gl_renderer: String,
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

pub(super) fn get_gpu_profile() -> GpuProfile {
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
pub(super) fn build_gl_spoof(p: &GpuProfile) -> String {
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
pub(super) fn build_worker_gl_spoof(p: &GpuProfile, locale: Option<&str>) -> String {
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
