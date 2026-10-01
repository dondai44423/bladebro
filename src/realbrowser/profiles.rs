//! Profile enumeration (`Local State` + dir scan) and the live-browser probes:
//! DevToolsActivePort, SingletonLock/pid owners, path identity.

use crate::platform;
use std::path::{Path, PathBuf};

/// One profile inside a browser's profile root.
#[derive(Clone, Debug)]
pub struct ProfileInfo {
    /// Dir name key (`Default`, `Profile 1`, ...).
    pub key: String,
    /// Human name from `Local State` (`profile.info_cache`), or the key.
    pub name: String,
    /// The profile subdir (`<root>/<key>`) — where Preferences/Cookies live.
    pub path: PathBuf,
    /// The Chrome user-data-dir ROOT that contains this profile: what
    /// `--user-data-dir` and the clone import actually operate on. A profile
    /// subdir passed as the root would silently produce a fresh empty
    /// profile — cookies live under `<root>/<key>/`, never at the root.
    pub root: PathBuf,
    /// mtime of the profile's `Preferences` file (recency proxy).
    pub last_used: u64,
}

/// Enumerate profiles under a root: `Local State`'s `info_cache` names,
/// unioned with any dir that holds a `Preferences` file (covers odd
/// installs). Sorted most-recently-used first.
pub fn list_profiles(root: &Path) -> Vec<ProfileInfo> {
    use std::collections::BTreeMap;

    let mut names: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(txt) = std::fs::read_to_string(root.join("Local State")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(cache) = v
                .get("profile")
                .and_then(|p| p.get("info_cache"))
                .and_then(|c| c.as_object())
            {
                for (key, entry) in cache {
                    let name = entry
                        .get("name")
                        .and_then(|n| n.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(key.as_str());
                    names.insert(key.clone(), name.to_string());
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let key = e.file_name().to_string_lossy().to_string();
            // A profile dir always carries a Preferences JSON.
            if p.join("Preferences").is_file() {
                names.entry(key).or_default();
            }
        }
    }

    // Chrome's internal dirs carry a Preferences file but are never user
    // profiles: "System Profile" holds system-level prefs; "Guest Profile"
    // is transient. Listing them would let the default pick import an empty,
    // login-less profile.
    names.remove("System Profile");
    names.remove("Guest Profile");

    let mut out: Vec<ProfileInfo> = names
        .into_iter()
        .map(|(key, name)| {
            let path = root.join(&key);
            let last_used = std::fs::metadata(path.join("Preferences"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            ProfileInfo {
                name: if name.is_empty() { key.clone() } else { name },
                key,
                path,
                root: root.to_path_buf(),
                last_used,
            }
        })
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.last_used));
    out
}

/// Parse a `DevToolsActivePort` file body: line 1 = port, line 2 = browser
/// ws path. Chrome writes this into the profile dir whenever a debug
/// endpoint is live (launch flag or the chrome://inspect approval flow).
pub fn parse_devtools_active_port(body: &str) -> Option<u16> {
    body.lines().next()?.trim().parse::<u16>().ok().filter(|p| *p > 0)
}

/// The live debug port of a user-data root, if one is exposed right now
/// (Chrome writes `DevToolsActivePort` into the root).
pub fn devtools_port(root: &Path) -> Option<u16> {
    // Nuance (measured, Chrome 151): the file is written when the browser
    // was started with `--remote-debugging-port=0` (auto-assigned port) or
    // via the chrome://inspect approval flow — NOT for fixed-port launches.
    std::fs::read_to_string(root.join("DevToolsActivePort"))
        .ok()
        .and_then(|b| parse_devtools_active_port(&b))
}

/// Parse a Chrome `SingletonLock` symlink target (`<hostname>-<pid>`).
pub fn parse_singleton_owner(target: &str) -> Option<u32> {
    target.rsplit('-').next()?.parse::<u32>().ok()
}

/// The pid holding this profile, if a live process does (stale locks —
/// dead pids — read as not running). `SingletonLock` lives in the root.
pub fn profile_owner_pid(root: &Path) -> Option<u32> {
    let target = std::fs::read_link(root.join("SingletonLock")).ok()?;
    let pid = parse_singleton_owner(&target.to_string_lossy())?;
    if platform::process_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

/// Human description of who holds this profile, if a live process does.
/// POSIX/macOS: the `SingletonLock` symlink's pid. Windows: Chromium's
/// singleton is a `lockfile` opened with `FILE_FLAG_DELETE_ON_CLOSE`
/// (chrome/browser/process_singleton_win.cc — there is no `SingletonLock`
/// file at all), so the file's existence means a running browser holds the
/// profile; the kernel deletes it when that process dies, stale ones cannot
/// persist across a crash.
pub fn profile_in_use(root: &Path) -> Option<String> {
    if let Some(pid) = profile_owner_pid(root) {
        return Some(format!("pid {pid}"));
    }
    #[cfg(target_os = "windows")]
    {
        if root.join("lockfile").exists() {
            return Some("lock file held".to_string());
        }
    }
    None
}

/// Do two paths name the same directory? Canonicalizing both sides makes
/// path comparisons (the branded-Chrome default-dir gate) immune to
/// spelling: trailing slash, symlinked home, `..` components.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}
