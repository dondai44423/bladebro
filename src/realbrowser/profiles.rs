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

/// Latest modification timestamp (seconds since epoch) across the profile's
/// active state files (`Preferences`, `Cookies`, `Network/Cookies`, `History`,
/// `Local Storage/leveldb`). Chrome writes Cookies/History when logging into
/// sites without always rewriting `Preferences` immediately.
fn profile_last_used_secs(profile_dir: &Path) -> u64 {
    let candidates = [
        profile_dir.join("Preferences"),
        profile_dir.join("Cookies"),
        profile_dir.join("Cookies-wal"),
        profile_dir.join("Network").join("Cookies"),
        profile_dir.join("Network").join("Cookies-wal"),
        profile_dir.join("History"),
        profile_dir.join("Local Storage").join("leveldb"),
    ];
    candidates
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(0)
}

/// Enumerate profiles under a root: `Local State`'s `info_cache` names,
/// unioned with any dir that holds a `Preferences` file (covers odd
/// installs), plus flat-root profiles (Opera / Opera GX, where `Preferences`
/// and `Cookies` sit directly inside `root` with no `Default/` subdir).
/// Sorted most-recently-used first (respecting `Local State`'s active profile
/// on ties).
pub fn list_profiles(root: &Path) -> Vec<ProfileInfo> {
    use std::collections::{BTreeMap, HashSet};

    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut active_keys: HashSet<String> = HashSet::new();
    if let Ok(txt) = std::fs::read_to_string(root.join("Local State")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(profile_obj) = v.get("profile") {
                if let Some(last) = profile_obj.get("last_used").and_then(|s| s.as_str()) {
                    if !last.is_empty() {
                        active_keys.insert(last.to_string());
                    }
                }
                if let Some(arr) = profile_obj
                    .get("last_active_profiles")
                    .and_then(|a| a.as_array())
                {
                    for item in arr {
                        if let Some(k) = item.as_str().filter(|s| !s.is_empty()) {
                            active_keys.insert(k.to_string());
                        }
                    }
                }
                if let Some(cache) = profile_obj.get("info_cache").and_then(|c| c.as_object()) {
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
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let key = e.file_name().to_string_lossy().to_string();
            // A user profile dir carries Preferences (or Cookies/History on a
            // live first-run profile before Chromium's first Preferences flush).
            if p.join("Preferences").is_file()
                || p.join("Cookies").is_file()
                || p.join("Network").join("Cookies").is_file()
                || p.join("History").is_file()
            {
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
        // Local State can retain deleted profiles. Its keys are directory
        // names, never paths: don't select a ghost or let metadata escape
        // the user-data root via an absolute name or parent traversal.
        .filter(|(key, _)| {
            !key.is_empty()
                && key != "."
                && key != ".."
                && !key.contains(['/', '\\', ':', '\0'])
                && std::fs::symlink_metadata(root.join(key)).is_ok_and(|m| m.is_dir())
        })
        .map(|(key, name)| {
            let path = root.join(&key);
            let last_used = profile_last_used_secs(&path);
            ProfileInfo {
                name: if name.is_empty() { key.clone() } else { name },
                key,
                path,
                root: root.to_path_buf(),
                last_used,
            }
        })
        .collect();

    // Opera and Opera GX store `Preferences` and `Cookies` directly in the
    // user-data root with no `Default/` subdirectory. Surface that flat root
    // with sentinel key `.` (omitted from `--profile-directory` at launch).
    if out.is_empty() && root.join("Preferences").is_file() {
        out.push(ProfileInfo {
            key: ".".into(),
            name: "Default".into(),
            path: root.to_path_buf(),
            root: root.to_path_buf(),
            last_used: profile_last_used_secs(root),
        });
    }

    out.sort_by_key(|p| {
        (
            std::cmp::Reverse(p.last_used),
            std::cmp::Reverse(active_keys.contains(&p.key)),
        )
    });
    out
}

/// Parse a `DevToolsActivePort` file body: line 1 = port, line 2 = browser
/// ws path. Chrome writes this into the profile dir whenever a debug
/// endpoint is live (launch flag or the chrome://inspect approval flow).
pub fn parse_devtools_active_port(body: &str) -> Option<u16> {
    body.lines()
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
}

/// Parse `--remote-debugging-port=<N>` (N > 0) from a NUL- or space-separated
/// browser command line. Used as a Linux fallback when a browser was armed
/// with a FIXED port (which does not always write `DevToolsActivePort`).
pub fn parse_remote_debugging_port_arg(cmdline: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(cmdline);
    for token in text.split(['\0', ' ', '\t', '\n', '\r']) {
        if let Some(val) = token.strip_prefix("--remote-debugging-port=") {
            if let Ok(port) = val.parse::<u16>() {
                if port > 0 {
                    return Some(port);
                }
            }
        }
    }
    None
}

/// The debug port advertised for a user-data root: `DevToolsActivePort`
/// (ephemeral `--remote-debugging-port=0` or Chrome 144+ `chrome://inspect`
/// approval flow), or on Linux a live owner browser's fixed
/// `--remote-debugging-port=<N>` command-line flag.
pub fn devtools_port(root: &Path) -> Option<u16> {
    if let Some(port) = std::fs::read_to_string(root.join("DevToolsActivePort"))
        .ok()
        .and_then(|b| parse_devtools_active_port(&b))
    {
        return Some(port);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(pid) = profile_owner_pid(root) {
        if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            if let Some(port) = parse_remote_debugging_port_arg(&cmdline) {
                return Some(port);
            }
        }
    }
    None
}

/// True when `127.0.0.1:<port>` (or `[::1]:<port>`) accepts a loopback TCP
/// connection within a tight local budget — catches stale `DevToolsActivePort`
/// files left behind by crashed browsers on macOS/Windows too.
fn loopback_port_reachable(port: u16) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    let timeout = Duration::from_millis(80);
    let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    if TcpStream::connect_timeout(&v4, timeout).is_ok() {
        return true;
    }
    let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
    TcpStream::connect_timeout(&v6, timeout).is_ok()
}

/// Live debug port owned by a real browser of this user on `root`. Filters
/// out stale `DevToolsActivePort` files on every OS (Linux verifies process
/// ownership; macOS/Windows verify the loopback port is actually listening).
pub fn devtools_port_live(root: &Path) -> Option<u16> {
    let port = devtools_port(root)?;
    if crate::browser::endpoint_owned_by_own_browser(port) && loopback_port_reachable(port) {
        Some(port)
    } else {
        None
    }
}

/// Parse a Chrome `SingletonLock` symlink target (`<hostname>-<pid>`).
pub fn parse_singleton_owner(target: &str) -> Option<u32> {
    target.rsplit('-').next()?.parse::<u32>().ok()
}

/// The pid holding this profile, if a live Chromium browser process does
/// (stale locks — dead pids, recycled non-browser pids, or sandbox-namespace
/// pids like Flatpak's `steamdeck-2` colliding with host `[kthreadd]` — read
/// as not running unless a live host browser holds `root`).
pub fn profile_owner_pid(root: &Path) -> Option<u32> {
    let target = std::fs::read_link(root.join("SingletonLock")).ok()?;
    let pid = parse_singleton_owner(&target.to_string_lossy())?;
    if pid > 1 && platform::process_alive(pid) && platform::process_is_chrome(pid) {
        return Some(pid);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(host_pid) = sandbox_browser_holding_root(root) {
        return Some(host_pid);
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sandbox_browser_holding_root(root: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let me = std::process::id();
    let canon = root.canonicalize().ok();
    let root_dev_ino = std::fs::metadata(root).ok().map(|m| (m.dev(), m.ino()));
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid <= 2 || pid == me {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if cmdline.is_empty()
            || cmdline
                .split(|b| *b == 0 || *b == b' ')
                .any(|arg| arg.starts_with(b"--type="))
            || !platform::cmdline_is_chromium(&cmdline)
        {
            continue;
        }
        if cmdline_names_root(&cmdline, root)
            || canon
                .as_deref()
                .is_some_and(|c| cmdline_names_root(&cmdline, c))
            || proc_fds_touch_root(pid, root, canon.as_deref(), root_dev_ino)
        {
            return Some(pid);
        }
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn cmdline_names_root(cmdline: &[u8], dir: &Path) -> bool {
    let needle = format!("--user-data-dir={}", dir.display());
    let needle = needle.as_bytes();
    let bounded = |b: Option<&u8>| matches!(b, None | Some(0) | Some(b' '));
    cmdline
        .windows(needle.len())
        .enumerate()
        .any(|(at, window)| {
            window == needle
                && bounded(at.checked_sub(1).and_then(|i| cmdline.get(i)))
                && bounded(cmdline.get(at + needle.len()))
        })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_fds_touch_root(
    pid: u32,
    root: &Path,
    canon: Option<&Path>,
    root_dev_ino: Option<(u64, u64)>,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    for fd in fds.flatten() {
        let fd_path = fd.path();
        if let Ok(link) = std::fs::read_link(&fd_path) {
            if link.starts_with(root) || canon.is_some_and(|c| link.starts_with(c)) {
                return true;
            }
        }
        if let Some((rdev, rino)) = root_dev_ino {
            if let Ok(meta) = std::fs::metadata(&fd_path) {
                if meta.dev() == rdev && meta.ino() == rino {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(unix)]
fn singleton_socket_active(root: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(root.join("SingletonSocket")).is_ok()
}

/// Human description of who holds this profile, if a live process does.
/// POSIX/macOS: the `SingletonLock` browser pid, a sandboxed browser holding
/// `root`, or a live `SingletonSocket` listener. Windows: Chromium's
/// singleton is a `lockfile` opened with `FILE_FLAG_DELETE_ON_CLOSE`
/// (chrome/browser/process_singleton_win.cc — there is no `SingletonLock`
/// file at all), so the file's existence means a running browser holds the
/// profile; the kernel deletes it when that process dies, stale ones cannot
/// persist across a crash.
pub fn profile_in_use(root: &Path) -> Option<String> {
    if let Some(pid) = profile_owner_pid(root) {
        return Some(format!("pid {pid}"));
    }
    #[cfg(unix)]
    if singleton_socket_active(root) {
        let label = std::fs::read_link(root.join("SingletonLock"))
            .ok()
            .and_then(|t| parse_singleton_owner(&t.to_string_lossy()))
            .map(|pid| format!("pid {pid}"))
            .unwrap_or_else(|| "SingletonSocket active".to_string());
        return Some(label);
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
