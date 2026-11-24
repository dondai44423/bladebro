//! Profile-tree copying: the runtime-file skip list and the bounded,
//! error-tolerant recursive copy. Split from the `session_profile` core.

use std::path::{Path, PathBuf};

/// Files/dirs Chrome locks while running or regenerates on demand — never copy
/// these between a live profile and the template. User session state (Cookies,
/// Network, Local Storage, Session Storage, IndexedDB, WebStorage, Extensions,
/// Preferences, History, WidevineCdm) is always kept.
const SKIP_ON_COPY: &[&str] = &[
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
    "lockfile",
    ".blade-owner",
    "Crashpad",
    "Crash Reports",
    "RunningChromeVersion",
    // Performance & component caches: large, regenerated on demand, and never
    // hold user logins, cookies, storage, or extensions. Skipping these cuts
    // real-browser clone copies from ~170+ MB down to ~10-25 MB.
    "Cache",
    "Code Cache",
    "GPUCache",
    "GPUPersistentCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GraphiteDawnCache",
    "GrShaderCache",
    "ShaderCache",
    "Media Cache",
    "CacheStorage",
    "ScriptCache",
    "blob_storage",
    "component_crx_cache",
    "extensions_crx_cache",
    "WasmTtsEngine",
    "Safe Browsing",
    "BrowserMetrics",
    "OnDeviceHeadSuggestModel",
    "OptGuideOnDeviceModel",
    "OptimizationGuideModelsManifest",
    "OptimizationHints",
    "CertificateRevocation",
    "ActorSafetyLists",
    "Crowd Deny",
    "Subresource Filter",
    "PKIMetadata",
    "ZxcvbnData",
    "SafetyTips",
    "MEIPreload",
    "hyphen-data",
];

/// Files whose copy failure means logins or session decryption may be missing
/// in a cloned profile (e.g., Windows Chrome holding an exclusive lock on
/// `Network/Cookies` while running).
const CRITICAL_AUTH_FILES: &[&str] = &[
    "Cookies",
    "Cookies-wal",
    "Cookies-journal",
    "Local State",
    "Preferences",
];

/// Summary of a profile-tree copy.
#[derive(Clone, Debug, Default)]
pub(crate) struct CopyReport {
    pub truncated_dirs: usize,
    pub copied_cookies: bool,
    pub failed_critical: Vec<String>,
}

/// Nesting bound for the copy. It used to be 6, which silently dropped real
/// profile data: extension packages nest deeper
/// (`Default/Extensions/<id>/<version>/lib/codemirror/addon/fold/*.js`), and
/// a clone missing those files breaks the user's extensions. Symlinks are
/// never followed and the large caches are skipped by name, so the bound only
/// guards against a pathological tree — and what it cuts is reported.
pub(super) const MAX_COPY_DEPTH: usize = 32;

/// Copy a Chrome profile tree, skipping locked/runtime files.
/// Shallow-but-recursive: Chrome's profile nests (Default/,
/// Local Storage/, etc.) — a full recursive copy with per-file
/// error tolerance. Dirs are created 0700 (cookie-bearing data).
pub(super) fn copy_profile(src: &Path, dst: &Path) -> usize {
    copy_profile_ex_report(src, dst, &[]).truncated_dirs
}

/// Copy with an extra skip list — the real-lane import excludes session
/// restore files so the clone opens a fresh window, not the user's tab set.
/// Tracks critical auth-file copy diagnostics so the real-browser lane can
/// warn loudly if a running browser locked `Cookies` or `Local State` during
/// import.
pub(crate) fn copy_profile_ex_report(src: &Path, dst: &Path, extra_skip: &[&str]) -> CopyReport {
    let _ = crate::platform::secure_create_dir_all(dst);
    let mut cut = Truncated::default();
    let mut report = CopyReport::default();
    copy_dir_filtered(
        src,
        dst,
        0,
        extra_skip,
        &mut cut,
        &mut report,
        Path::new(""),
    );
    if let Some(first) = &cut.first {
        eprintln!(
            "[bladebro] profile copy incomplete: {} director{} nested deeper than \
             {MAX_COPY_DEPTH} levels not copied (first: {})",
            cut.count,
            if cut.count == 1 { "y" } else { "ies" },
            first.display()
        );
    }
    report.truncated_dirs = cut.count;
    report
}

/// Directories the depth bound cut (count + the first, for the report).
#[derive(Default)]
struct Truncated {
    count: usize,
    first: Option<PathBuf>,
}

fn copy_dir_filtered(
    src: &Path,
    dst: &Path,
    depth: usize,
    extra_skip: &[&str],
    cut: &mut Truncated,
    report: &mut CopyReport,
    rel: &Path,
) {
    if depth > MAX_COPY_DEPTH {
        cut.count += 1;
        cut.first.get_or_insert_with(|| src.to_path_buf());
        return;
    }
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_ON_COPY.contains(&name.as_str()) || extra_skip.contains(&name.as_str()) {
            continue;
        }
        let s = entry.path();
        let d = dst.join(&name);
        let child_rel = rel.join(&name);
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let _ = crate::platform::secure_create_dir_all(&d);
            copy_dir_filtered(&s, &d, depth + 1, extra_skip, cut, report, &child_rel);
        } else if ft.is_file() {
            // Skip sockets/fifos implicitly (not files). Copy
            // errors (locked SQLite, etc.) are tolerated, but failures on
            // auth-critical files are recorded so the caller can warn loudly.
            match std::fs::copy(&s, &d) {
                Ok(_) => {
                    if name == "Cookies" {
                        report.copied_cookies = true;
                    }
                }
                Err(e) => {
                    if CRITICAL_AUTH_FILES.contains(&name.as_str()) {
                        report
                            .failed_critical
                            .push(format!("{} ({e})", child_rel.display()));
                    }
                }
            }
        }
    }
}
