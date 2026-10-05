//! Profile-tree copying: the runtime-file skip list and the bounded,
//! error-tolerant recursive copy. Split from the `session_profile` core.

use std::path::Path;

/// Files/dirs Chrome locks while running — never copy these
/// between a live profile and the template.
const SKIP_ON_COPY: &[&str] = &[
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
    "lockfile",
    ".blade-owner",
    "Crashpad",
    "RunningChromeVersion",
    // Performance caches: large, regenerated on demand, and not needed
    // for seasoning (trust signals). Skipping these cuts the profile copy
    // from ~hundreds of MB to ~tens of MB — much faster launch.
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "blob_storage",
];

/// Copy a Chrome profile tree, skipping locked/runtime files.
/// Shallow-but-recursive: Chrome's profile nests (Default/,
/// Local Storage/, etc.) — a full recursive copy with per-file
/// error tolerance. Dirs are created 0700 (cookie-bearing data).
pub(super) fn copy_profile(src: &Path, dst: &Path) {
    copy_profile_ex(src, dst, &[]);
}

/// Copy with an extra skip list — the real-lane import excludes session
/// restore files so the clone opens a fresh window, not the user's tab set.
pub(crate) fn copy_profile_ex(src: &Path, dst: &Path, extra_skip: &[&str]) {
    let _ = crate::platform::secure_create_dir_all(dst);
    copy_dir_filtered(src, dst, 0, extra_skip);
}

fn copy_dir_filtered(src: &Path, dst: &Path, depth: usize, extra_skip: &[&str]) {
    // Bound recursion — Chrome profiles nest cache dirs deeply.
    // 6 levels covers the deepest seasoning-relevant structure
    // (Default/WebStorage/<id>/CacheStorage/<origin>/files).
    if depth > 6 {
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
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let _ = crate::platform::secure_create_dir_all(&d);
            copy_dir_filtered(&s, &d, depth + 1, extra_skip);
        } else if ft.is_file() {
            // Skip sockets/fifos implicitly (not files). Copy
            // errors (locked SQLite, etc.) are tolerated.
            let _ = std::fs::copy(&s, &d);
        }
    }
}
