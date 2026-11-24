//! Clone-lane template management: atomic import of a user profile root,
//! freshness detection, crash-bubble sanitization, provenance metadata, stats,
//! and forget.

use crate::error::{BladeError, Result};
use crate::platform;
use std::collections::BTreeMap;
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

/// `<root>/profile-source.json` — where a template came from (provenance +
/// source freshness stamps).
pub fn source_meta_path(id: &str) -> PathBuf {
    root_for(id).join("profile-source.json")
}

/// The import-generation marker inside `template/` (`.blade-import`), used by
/// `launch_fingerprint` so a running MCP/daemon session notices a fresh import
/// and relaunches cleanly onto the updated clone.
pub fn template_import_generation(id: &str) -> Option<String> {
    std::fs::read_to_string(template_dir(id).join(crate::session_profile::IMPORT_MARKER))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Compact freshness stamp for one profile directory (auth/storage files).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileStamp {
    pub max_mtime_ns: u128,
    pub cookies_bytes: u64,
}

fn file_mtime_ns(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

fn shallow_dir_max_mtime_ns(dir: &Path) -> u128 {
    let mut max_ns = file_mtime_ns(dir).unwrap_or(0);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(128) {
            if let Some(ns) = file_mtime_ns(&entry.path()) {
                if ns > max_ns {
                    max_ns = ns;
                }
            }
        }
    }
    max_ns
}

/// Compute a fast freshness stamp for one profile directory: max nanosecond
/// mtime across cookies, prefs, and local/session storage, plus cookie
/// database size (detects in-place writes even on coarse filesystems).
pub fn profile_state_stamp(profile_dir: &Path) -> ProfileStamp {
    let direct_files = [
        profile_dir.join("Cookies"),
        profile_dir.join("Cookies-wal"),
        profile_dir.join("Cookies-journal"),
        profile_dir.join("Network").join("Cookies"),
        profile_dir.join("Network").join("Cookies-wal"),
        profile_dir.join("Network").join("Cookies-journal"),
        profile_dir.join("Preferences"),
    ];
    let mut max_mtime_ns = 0u128;
    for path in &direct_files {
        if let Some(ns) = file_mtime_ns(path) {
            if ns > max_mtime_ns {
                max_mtime_ns = ns;
            }
        }
    }
    for dir in [
        profile_dir.join("Local Storage").join("leveldb"),
        profile_dir.join("Session Storage"),
        profile_dir.join("WebStorage"),
    ] {
        let ns = shallow_dir_max_mtime_ns(&dir);
        if ns > max_mtime_ns {
            max_mtime_ns = ns;
        }
    }
    let cookies_bytes = std::fs::metadata(profile_dir.join("Network").join("Cookies"))
        .or_else(|_| std::fs::metadata(profile_dir.join("Cookies")))
        .map(|m| m.len())
        .unwrap_or(0);
    ProfileStamp {
        max_mtime_ns,
        cookies_bytes,
    }
}

fn source_profile_stamps(root: &Path) -> (u128, BTreeMap<String, ProfileStamp>) {
    let local_state_mtime_ns = file_mtime_ns(&root.join("Local State")).unwrap_or(0);
    let mut stamps = BTreeMap::new();
    for p in super::list_profiles(root) {
        stamps.insert(p.key, profile_state_stamp(&p.path));
    }
    (local_state_mtime_ns, stamps)
}

fn profile_dir_for_key(root: &Path, key: &str) -> PathBuf {
    if key.is_empty() || key == "." {
        root.to_path_buf()
    } else {
        root.join(key)
    }
}

/// Clear Chrome's `"exit_type": "Crashed"` / `"exited_cleanly": false` markers
/// inside a CLONED template directory so importing an open browser does not
/// pop up the "Restore pages?" crash bubble on launch. Never touches the
/// user's source profile.
fn sanitize_cloned_preferences(template_root: &Path) {
    let mut prefs_files = vec![template_root.join("Preferences")];
    if let Ok(entries) = std::fs::read_dir(template_root) {
        for entry in entries.flatten() {
            let p = entry.path().join("Preferences");
            if p.is_file() {
                prefs_files.push(p);
            }
        }
    }
    for pref in prefs_files {
        if !pref.is_file() {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&pref) else {
            continue;
        };
        let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&txt) else {
            continue;
        };
        let Some(profile_obj) = val.get_mut("profile").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        let mut changed = false;
        if profile_obj.get("exit_type").and_then(|v| v.as_str()) == Some("Crashed") {
            profile_obj.insert("exit_type".into(), serde_json::json!("Normal"));
            changed = true;
        }
        if profile_obj.get("exited_cleanly").and_then(|v| v.as_bool()) == Some(false) {
            profile_obj.insert("exited_cleanly".into(), serde_json::json!(true));
            changed = true;
        }
        if changed {
            if let Ok(body) = serde_json::to_vec(&val) {
                let _ = platform::secure_write_file(&pref, &body);
            }
        }
    }
}

/// Import stats for user feedback.
#[derive(Clone, Debug)]
pub struct ImportStats {
    pub files: u64,
    pub bytes: u64,
    pub ms: u128,
    pub copied_cookies: bool,
    pub failed_critical: Vec<String>,
}

/// Copy a live user-data-dir ROOT into the template for `id`, atomically:
/// the copy lands in `<root>/template.tmp`, then swaps in. Session-restore
/// files/dirs (`Current/Last Session|Tabs`, `Sessions`, `Sessions_Encrypted`)
/// are excluded — the clone must open a fresh window, not the user's tab set.
/// The SOURCE is only ever read.
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

    // Snapshot source freshness stamps BEFORE copying so any write arriving
    // during or after the copy will be detected as fresher on the next launch.
    let (local_state_mtime_ns, profile_stamps) = source_profile_stamps(src);

    let started = std::time::Instant::now();
    let copy_report = crate::session_profile::copy_profile_ex_report(
        src,
        &tmp,
        &[
            "Current Session",
            "Current Tabs",
            "Last Session",
            "Last Tabs",
            "Sessions",
            "Sessions_Encrypted",
        ],
    );
    if !tmp.is_dir() {
        return Err(BladeError::Other(
            "profile copy produced no directory".into(),
        ));
    }
    sanitize_cloned_preferences(&tmp);
    // A fresh import generation: sessions cloned from an EARLIER import
    // carry the old value, and their sync-back refuses to overwrite this
    // template with their stale copy.
    let generation = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id()
    );
    platform::secure_write_file(
        &tmp.join(crate::session_profile::IMPORT_MARKER),
        generation.as_bytes(),
    )
    .map_err(|e| BladeError::Other(format!("cannot mark the imported template: {e}")))?;

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
        "copied_cookies": copy_report.copied_cookies,
        "failed_critical": copy_report.failed_critical,
        "local_state_mtime_ns": local_state_mtime_ns.to_string(),
        "profile_stamps": profile_stamps.iter().map(|(k, st)| (
            k.clone(),
            serde_json::json!({
                "max_mtime_ns": st.max_mtime_ns.to_string(),
                "cookies_bytes": st.cookies_bytes,
            })
        )).collect::<serde_json::Map<String, serde_json::Value>>(),
    });
    let _ = platform::secure_write_file(
        &root.join("profile-source.json"),
        meta.to_string().as_bytes(),
    );

    Ok(ImportStats {
        files,
        bytes,
        ms: started.elapsed().as_millis(),
        copied_cookies: copy_report.copied_cookies,
        failed_critical: copy_report.failed_critical,
    })
}

/// Name this import's source path, for `rb status`/diagnostics.
pub fn template_source(id: &str) -> Option<String> {
    let txt = std::fs::read_to_string(source_meta_path(id)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("source").and_then(|s| s.as_str()).map(String::from)
}

/// True when the clone template for `id` exists AND was imported from the
/// user-data root `root`. The template is keyed by browser id only, so a
/// profile selected in ANOTHER root (`rb profile <abs path>`) must not reuse
/// it. Missing provenance counts as a mismatch: nothing then proves whose
/// profile the clone holds.
pub fn template_matches(id: &str, root: &Path) -> bool {
    has_template(id)
        && template_source(id).is_some_and(|src| super::same_dir(Path::new(&src), root))
}

/// True when the clone template for `id` matches `root` AND the user's live
/// source profile has no newer auth/storage state than what was imported.
///
/// If the user logged into a site (e.g., X, GitHub, Gmail) in their real
/// browser after the last import, `Cookies`/`Local Storage`/`Preferences`/
/// `Local State` mtime or cookie DB size changes and this returns `false`,
/// triggering an automatic refresh instead of launching a logged-out clone.
/// When the source is unchanged, this returns `true` so agent-made clone state
/// survives and switching profile keys inside the same root never re-imports.
pub fn template_is_fresh(id: &str, root: &Path, profile_key: &str) -> bool {
    if !template_matches(id, root) {
        return false;
    }
    let Ok(txt) = std::fs::read_to_string(source_meta_path(id)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return false;
    };
    let current_local = file_mtime_ns(&root.join("Local State")).unwrap_or(0);
    let current_profile = profile_state_stamp(&profile_dir_for_key(root, profile_key));

    // New format (4.8.2+): exact nanosecond mtime + cookie byte size stamps.
    if let Some(stamps) = v.get("profile_stamps").and_then(|s| s.as_object()) {
        if let Some(saved_local) = v
            .get("local_state_mtime_ns")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<u128>().ok())
        {
            if current_local > saved_local {
                return false;
            }
        }
        if let Some(entry) = stamps.get(profile_key) {
            let saved_ns = entry
                .get("max_mtime_ns")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u128>().ok())
                .unwrap_or(0);
            let saved_cookies = entry
                .get("cookies_bytes")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            return current_profile.max_mtime_ns <= saved_ns
                && current_profile.cookies_bytes == saved_cookies;
        }
        // Selected profile was not in the previous snapshot (e.g. newly created).
        return false;
    }

    // Legacy `profile-source.json` (pre-4.8.2): compare against `imported_at`
    // in whole seconds so any existing stale template is caught immediately.
    let imported_at_ns = v
        .get("imported_at")
        .and_then(|x| x.as_u64())
        .map(|s| u128::from(s) * 1_000_000_000)
        .unwrap_or(0);
    current_local <= imported_at_ns && current_profile.max_mtime_ns <= imported_at_ns
}

/// True when the last import of `id` recorded locked/failed critical auth files
/// (e.g. `Network/Cookies` locked by Chrome on Windows).
pub fn template_failed_critical(id: &str) -> Vec<String> {
    let Ok(txt) = std::fs::read_to_string(source_meta_path(id)) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return Vec::new();
    };
    v.get("failed_critical")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Pids of live bladebro processes running a clone session of `id`. Refreshing
/// under one gets its stale copy synced back over the import; wiping under
/// one deletes the profile its browser is running on.
pub fn live_clone_sessions(id: &str) -> Vec<u32> {
    crate::session_profile::live_session_owners(&root_for(id).join("profiles"))
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
