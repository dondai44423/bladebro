//! Clone-lane template management: atomic import of a user profile root,
//! provenance metadata, stats, forget.

use crate::error::{BladeError, Result};
use crate::platform;
use std::path::{Path, PathBuf};

/// `<data-dir>/realbrowser/<browser-id>/`.
pub fn root_for(id: &str) -> PathBuf {
    platform::blade_dir().join("realbrowser").join(id)
}

/// The imported profile template for a browser id.
pub fn template_dir(id: &str) -> PathBuf {
    root_for(id).join("template")
}

pub fn has_template(id: &str) -> bool {
    template_dir(id).is_dir()
}

/// `<root>/profile-source.json` — where a template came from (provenance).
pub fn source_meta_path(id: &str) -> PathBuf {
    root_for(id).join("profile-source.json")
}

/// Import stats for user feedback.
#[derive(Clone, Debug)]
pub struct ImportStats {
    pub files: u64,
    pub bytes: u64,
    pub ms: u128,
}

/// Copy a live user-data-dir ROOT into the template for `id`, atomically:
/// the copy lands in `<root>/template.tmp`, then swaps in. Session-restore
/// files are excluded — the clone must open a fresh window, not the user's
/// tab set. The SOURCE is only ever read.
pub fn import_template(id: &str, src: &Path) -> Result<ImportStats> {
    if !src.is_dir() {
        return Err(BladeError::Other(format!(
            "profile dir not found: {}",
            src.display()
        )));
    }
    let root = root_for(id);
    platform::secure_create_dir_all(&root)
        .map_err(|e| BladeError::Other(format!("cannot create {}: {e}", root.display())))?;
    let _lock =
        crate::session_profile::SessionProfile::acquire_lock_at(&root).ok_or_else(|| {
            BladeError::Other(
                "browser template is busy; retry import after the current session finishes syncing"
                    .into(),
            )
        })?;
    let tmp = root.join("template.tmp");
    let _ = std::fs::remove_dir_all(&tmp);

    let started = std::time::Instant::now();
    crate::session_profile::copy_profile_ex(
        src,
        &tmp,
        &[
            "Current Session",
            "Current Tabs",
            "Last Session",
            "Last Tabs",
        ],
    );
    if !tmp.is_dir() {
        return Err(BladeError::Other(
            "profile copy produced no directory".into(),
        ));
    }

    // Atomic-ish swap (same discipline as the agent-lane template).
    let template = root.join("template");
    let old = root.join("template.old");
    let _ = std::fs::remove_dir_all(&old);
    if template.exists() {
        std::fs::rename(&template, &old)
            .map_err(|e| BladeError::Other(format!("cannot move old template aside: {e}")))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &template) {
        if old.exists() {
            let _ = std::fs::rename(&old, &template);
        }
        return Err(BladeError::Other(format!("cannot install template: {e}")));
    }
    let _ = std::fs::remove_dir_all(&old);

    let (files, bytes) = dir_stats(&template);
    let meta = serde_json::json!({
        "source": src.display().to_string(),
        "imported_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "files": files,
        "bytes": bytes,
    });
    let _ = platform::secure_write_file(
        &root.join("profile-source.json"),
        meta.to_string().as_bytes(),
    );

    Ok(ImportStats {
        files,
        bytes,
        ms: started.elapsed().as_millis(),
    })
}

/// Name this import's source path, for `rb status`/diagnostics.
pub fn template_source(id: &str) -> Option<String> {
    let txt = std::fs::read_to_string(source_meta_path(id)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("source").and_then(|s| s.as_str()).map(String::from)
}

/// Wipe a browser's template + any leftover session dirs.
pub fn forget(id: &str) -> Result<bool> {
    // Config and on-disk orphan names reach this destructive boundary without
    // browser discovery. Never let a malformed id become a filesystem path.
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(BladeError::Other(format!(
            "invalid browser id `{id}` — select a browser with `bladebro rb use <id>`"
        )));
    }
    let root = root_for(id);
    if !root.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&root)
        .map_err(|e| BladeError::Other(format!("cannot remove {}: {e}", root.display())))?;
    Ok(true)
}

fn dir_stats(dir: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    files += 1;
                    bytes += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    (files, bytes)
}

/// File count + byte size of a browser's template (for `rb status`).
pub fn template_stats(id: &str) -> Option<(u64, u64)> {
    let t = template_dir(id);
    if !t.is_dir() {
        return None;
    }
    Some(dir_stats(&t))
}

/// Human size for status output.
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
