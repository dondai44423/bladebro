//! Profile-tree copying: the runtime-file skip list and the bounded,
//! error-tolerant recursive copy. Split from the `session_profile` core.

use std::path::{Path, PathBuf};

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
    copy_profile_ex(src, dst, &[])
}

/// Copy with an extra skip list — the real-lane import excludes session
/// restore files so the clone opens a fresh window, not the user's tab set.
/// Returns how many directories were left out for nesting deeper than
/// [`MAX_COPY_DEPTH`]; any such cut is also reported on stderr.
pub(crate) fn copy_profile_ex(src: &Path, dst: &Path, extra_skip: &[&str]) -> usize {
    let _ = crate::platform::secure_create_dir_all(dst);
    let mut cut = Truncated::default();
    copy_dir_filtered(src, dst, 0, extra_skip, &mut cut);
    if let Some(first) = &cut.first {
        eprintln!(
            "[bladebro] profile copy incomplete: {} director{} nested deeper than \
             {MAX_COPY_DEPTH} levels not copied (first: {})",
            cut.count,
            if cut.count == 1 { "y" } else { "ies" },
            first.display()
        );
    }
    cut.count
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
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let _ = crate::platform::secure_create_dir_all(&d);
            copy_dir_filtered(&s, &d, depth + 1, extra_skip, cut);
        } else if ft.is_file() {
            // Skip sockets/fifos implicitly (not files). Copy
            // errors (locked SQLite, etc.) are tolerated.
            let _ = std::fs::copy(&s, &d);
        }
    }
}
