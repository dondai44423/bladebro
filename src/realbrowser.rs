//! Real-browser lane (`bladebro rb`).
//!
//! Bladebro's default lane owns an isolated Chromium + seasoned profile on a
//! virtual display, and manufactures coherence with a page-injection layer.
//! The real-browser lane is the opposite trade, and for protected sites the
//! stronger one: the agent drives the user's OWN Chromium-family browser —
//! their binary, their real profile data, the real display — and the
//! injection layer is switched OFF entirely. Truth has no lies to catch:
//! every mask this crate maintains is a measurable risk (the S10 `toString`
//! episode is the receipt), so on this lane the correct amount of page
//! patching is zero.
//!
//! What still runs on the real lane: everything driver-side — perception,
//! the Live Page Model, refs, adapters, token-efficiency compression,
//! interception, the biometrics/hum behavior layer. None of it is
//! page-visible.
//!
//! Three mechanisms, chosen by [`Mode`]:
//! - **Clone** (default): the user's profile is imported once into a
//!   blade-owned template and per-process session dirs (the exact machinery
//!   the agent lane already uses for seasoning), then launched with the
//!   user's real browser binary. Works while their browser is running,
//!   works for Google-Chrome-branded builds (whose 136+ CDP hardening
//!   refuses remote debugging on the *default* profile dir — a non-default
//!   clone dir sidesteps it), and never touches their live profile.
//! - **Profile**: launch their binary directly on their real profile dir.
//!   Full fidelity, writes persist into their profile — requires their
//!   browser to be closed, and branded Chrome requires a non-default dir.
//! - **Attach**: drive an already-running browser (classic pre-armed
//!   `--remote-debugging-port`, or Chrome >=144's official
//!   `chrome://inspect#remote-debugging` approval flow). No ownership: no
//!   launch, no shutdown.
//!
//! Ground truth this design obeys (verified on the dev machine, Chrome 151):
//! the `default_user_data_dir` CDP refusal is compiled in only for
//! `GOOGLE_CHROME_BRANDING` (chromium source), so plain Chromium accepts a
//! debug port on any dir; a WS-attached browser reports `webdriver=false`
//! natively; `--remote-debugging-pipe` reports `true` (so the real lane uses
//! WS — masking would be a lie, and a lie is exactly what this lane exists
//! to delete).

use crate::error::{BladeError, Result};
use crate::platform;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// ── Lane switch ─────────────────────────────────────────────────────────

static REAL_LANE: AtomicBool = AtomicBool::new(false);

/// Force the lane for this process (tests, harnesses).
pub fn set_real_lane(on: bool) {
    REAL_LANE.store(on, Ordering::Relaxed);
}

/// True when this process must drive the user's real browser instead of
/// launching the isolated agent browser.
pub fn real_lane() -> bool {
    REAL_LANE.load(Ordering::Relaxed)
}

/// Initialise the lane for this process: `BLADE_LANE=real|agent` overrides,
/// otherwise the persisted config decides. Called once at process start.
pub fn init_lane() {
    set_real_lane(lane_from(
        std::env::var("BLADE_LANE").ok().as_deref(),
        config().enabled,
    ));
}

/// Effective lane: the `BLADE_LANE=real|agent` env override wins, else the
/// persisted config. Pure — the decision is unit-testable, and every reader
/// of the lane (init, refresh, fingerprint) goes through it.
pub fn lane_from(env_override: Option<&str>, cfg_enabled: bool) -> bool {
    match env_override {
        Some("real") => true,
        Some("agent") => false,
        _ => cfg_enabled,
    }
}

/// Re-read the effective lane and switch this process when it moved.
/// Returns true when the lane changed — the browser that is running belongs
/// to the OLD lane, so the caller must relaunch it. Without that, `rb on`
/// / `rb off` is silently ignored by a long-lived MCP or daemon session
/// until its browser happens to die (the observed "rb on does nothing").
pub fn refresh_lane() -> bool {
    let want = lane_from(
        std::env::var("BLADE_LANE").ok().as_deref(),
        config().enabled,
    );
    let had = real_lane();
    if want != had {
        set_real_lane(want);
        true
    } else {
        false
    }
}

/// Fingerprint of everything that decides how the next launch behaves.
/// Long-lived surfaces snapshot it after each launch and compare it on every
/// call: a mismatch means the running browser no longer matches the config
/// (`rb on|off`, `rb mode|use|profile|visible`) and must be relaunched.
/// On the agent lane the real-browser fields cannot affect the browser, so
/// the fingerprint is constant there — an agent session never pays a
/// relaunch for config that does not concern it.
pub fn launch_fingerprint() -> u64 {
    launch_fingerprint_from(std::env::var("BLADE_LANE").ok().as_deref(), &config())
}

/// Testable core of [`launch_fingerprint`] (no env/disk reads).
pub fn launch_fingerprint_from(env_override: Option<&str>, cfg: &Config) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let lane = lane_from(env_override, cfg.enabled);
    lane.hash(&mut h);
    if lane {
        cfg.mode.as_str().hash(&mut h);
        cfg.browser.hash(&mut h);
        cfg.profile.hash(&mut h);
        cfg.binary.hash(&mut h);
        cfg.visible.hash(&mut h);
    }
    h.finish()
}

// ── Config ──────────────────────────────────────────────────────────────

/// How the real lane obtains its browser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Clone when nothing else is possible; attach when a debuggable
    /// browser is already running; profile-launch is never picked silently
    /// (it needs the user's browser closed — explicit only).
    #[default]
    Auto,
    /// Import the profile once, run the user's binary on the copy.
    Clone,
    /// Launch the user's binary on their live profile dir (needs it closed).
    Profile,
    /// Drive an already-running, debuggable browser; never own it.
    Attach,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Clone => "clone",
            Mode::Profile => "profile",
            Mode::Attach => "attach",
        }
    }
}

fn default_true() -> bool {
    true
}

/// Persisted real-browser configuration (`<data-dir>/realbrowser.json`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// Master switch (`bladebro rb on|off`).
    #[serde(default)]
    pub enabled: bool,
    /// Mechanism selection.
    #[serde(default)]
    pub mode: Mode,
    /// Browser id from [`discover`] (`chromium`, `chrome`, `brave`, ...),
    /// or `None` = most recently used.
    #[serde(default)]
    pub browser: Option<String>,
    /// Profile key within the browser's profile root, or an absolute path
    /// to a profile dir. `None` = most recently used.
    #[serde(default)]
    pub profile: Option<String>,
    /// Absolute path to a custom browser binary (`rb use --binary`): nix
    /// wrappers, flatpak launcher scripts, dev builds. `None` = the
    /// discovered binary for the selected browser.
    #[serde(default)]
    pub binary: Option<String>,
    /// Launch a visible window (the point of the feature on a desktop). An
    /// invisible lane falls back to `--headless=new` — honest, but a
    /// degraded environment; only for servers.
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Allow the daemon idle timeout to close a real-lane browser. Default
    /// off: the browser is the user's — yanking it away while they might be
    /// using it is exactly what this feature must never do.
    #[serde(default)]
    pub idle_shutdown: bool,
    /// Keep the idle-hum behavior on the real lane (it is driver-side and
    /// page-invisible; it pauses automatically with `rb pause`).
    #[serde(default = "default_true")]
    pub idle_hum: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: Mode::Auto,
            browser: None,
            profile: None,
            binary: None,
            visible: true,
            idle_shutdown: false,
            idle_hum: true,
        }
    }
}

/// `<data-dir>/realbrowser.json`.
pub fn config_path() -> PathBuf {
    platform::blade_dir().join("realbrowser.json")
}

/// Load the persisted config; a missing or malformed file is the default
/// (switch off). Malformed is deliberately NOT fatal: every command reads
/// this, and a corrupted byte must never brick the CLI.
pub fn config() -> Config {
    // Cached by (mtime, size): refresh_lane + launch_fingerprint run on every
    // tool call, and each used to re-read + re-parse the file. One stat per
    // call now; the file is re-read the moment it changes on disk.
    use std::sync::{Mutex, OnceLock};
    type Key = Option<(std::time::SystemTime, u64)>;
    static CACHE: OnceLock<Mutex<Option<(Key, Config)>>> = OnceLock::new();
    let path = config_path();
    let key: Key = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((cached_key, cfg)) = guard.as_ref() {
        if *cached_key == key {
            return cfg.clone();
        }
    }
    let cfg = config_from(&path);
    *guard = Some((key, cfg.clone()));
    cfg
}

/// Load from an explicit path (unit-testable).
pub fn config_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist the config (0600 — it names the user's browser and profile).
pub fn save_config(cfg: &Config) -> Result<()> {
    save_config_to(&config_path(), cfg)
}

pub fn save_config_to(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        platform::secure_create_dir_all(parent)
            .map_err(|e| BladeError::Other(format!("cannot create data dir: {e}")))?;
    }
    let body = serde_json::to_string_pretty(cfg)
        .map_err(|e| BladeError::Other(format!("cannot serialize config: {e}")))?;
    platform::secure_write_file(path, body.as_bytes())
        .map_err(|e| BladeError::Other(format!("cannot write realbrowser.json: {e}")))
}

// ── Browser discovery ───────────────────────────────────────────────────

/// Browser family — drives the CDP-hardening gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brand {
    /// Google Chrome (branded): refuses CDP on the default profile dir (M136+).
    Chrome,
    Chromium,
    Brave,
    Edge,
    Vivaldi,
    Opera,
}

impl Brand {
    pub fn as_str(self) -> &'static str {
        match self {
            Brand::Chrome => "Chrome",
            Brand::Chromium => "Chromium",
            Brand::Brave => "Brave",
            Brand::Edge => "Edge",
            Brand::Vivaldi => "Vivaldi",
            Brand::Opera => "Opera",
        }
    }
}

/// True when this brand is known to refuse `--remote-debugging-*` on the
/// default user-data dir — i.e. profile-mode on the default dir cannot work
/// and clone/attach are the only routes. Compiled in for
/// `GOOGLE_CHROME_BRANDING` only (chromium `remote_debugging_server.cc`),
/// so everything else must NOT be gated by default.
pub fn requires_non_default_dir(brand: Brand) -> bool {
    matches!(brand, Brand::Chrome)
}

/// Infer the brand from a `--version` output line
/// (e.g. "Google Chrome 151.0.…", "Chromium 151.0.…", "Brave Browser 1.7…").
pub fn brand_from_version(out: &str) -> Option<Brand> {
    let l = out.to_lowercase();
    if l.contains("google chrome") {
        Some(Brand::Chrome)
    } else if l.contains("brave") {
        Some(Brand::Brave)
    } else if l.contains("microsoft edge") || l.contains("msedge") {
        Some(Brand::Edge)
    } else if l.contains("vivaldi") {
        Some(Brand::Vivaldi)
    } else if l.contains("opera") {
        Some(Brand::Opera)
    } else if l.contains("chromium") {
        Some(Brand::Chromium)
    } else {
        None
    }
}

/// One installed browser.
#[derive(Clone, Debug)]
pub struct BrowserSpec {
    pub id: String,
    pub name: String,
    pub brand: Brand,
    /// Resolved binary (first existing candidate).
    pub binary: PathBuf,
    /// Profile root (first existing candidate; if none exists yet, the
    /// canonical first candidate for display).
    pub profile_root: PathBuf,
}

/// Candidate browser installs for this OS. Pure-ish (paths only); the
/// caller filters by existence. Flatpak is deliberately NOT a binary
/// candidate: `/usr/bin/flatpak` is a launcher, not a browser — spawning
/// it with Chrome flags just fails. Flatpak profile roots are listed so a
/// flatpak system can still work via `rb use --binary <wrapper>`.
#[allow(clippy::type_complexity)]
fn candidates() -> Vec<(&'static str, &'static str, Brand, Vec<PathBuf>, Vec<PathBuf>)> {
    // Used by the Linux + macOS candidate tables; Windows builds its paths
    // from LOCALAPPDATA/APPDATA/PROGRAMFILES instead.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home = platform::home_dir();
    let mut out: Vec<(&'static str, &'static str, Brand, Vec<PathBuf>, Vec<PathBuf>)> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"));
        let mut push = |id: &'static str, name: &'static str, brand: Brand,
                        bins: Vec<PathBuf>, roots: Vec<PathBuf>| {
            out.push((id, name, brand, bins, roots));
        };
        push(
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![
                PathBuf::from("/usr/sbin/chromium"),
                PathBuf::from("/usr/bin/chromium"),
                PathBuf::from("/usr/bin/chromium-browser"),
                PathBuf::from("/snap/bin/chromium"),
            ],
            vec![
                xdg.join("chromium"),
                home.join("snap/chromium/common/chromium"),
                home.join(".var/app/org.chromium.Chromium/config/chromium"),
            ],
        );
        push(
            "chrome",
            "Google Chrome",
            Brand::Chrome,
            vec![
                PathBuf::from("/usr/bin/google-chrome"),
                PathBuf::from("/usr/bin/google-chrome-stable"),
                PathBuf::from("/opt/google/chrome/chrome"),
            ],
            vec![
                xdg.join("google-chrome"),
                home.join(".var/app/com.google.Chrome/config/google-chrome"),
            ],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                PathBuf::from("/usr/bin/brave"),
                PathBuf::from("/usr/bin/brave-browser"),
                PathBuf::from("/opt/brave.com/brave/brave"),
            ],
            vec![
                xdg.join("BraveSoftware/Brave-Browser"),
                home.join(".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
            ],
        );
        push(
            "edge",
            "Microsoft Edge",
            Brand::Edge,
            vec![
                PathBuf::from("/usr/bin/microsoft-edge"),
                PathBuf::from("/usr/bin/microsoft-edge-stable"),
                PathBuf::from("/opt/microsoft/msedge/msedge"),
            ],
            vec![
                xdg.join("microsoft-edge"),
                home.join(".var/app/com.microsoft.Edge/config/microsoft-edge"),
            ],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![
                PathBuf::from("/usr/bin/vivaldi"),
                PathBuf::from("/usr/bin/vivaldi-stable"),
            ],
            vec![
                xdg.join("vivaldi"),
                home.join(".var/app/com.vivaldi.Vivaldi/config/vivaldi"),
            ],
        );
        push(
            "opera",
            "Opera",
            Brand::Opera,
            vec![
                PathBuf::from("/usr/bin/opera"),
            ],
            vec![xdg.join("opera")],
        );
    }

    #[cfg(target_os = "macos")]
    {
        let apps = PathBuf::from("/Applications");
        let user_apps = home.join("Applications");
        let sup = home.join("Library/Application Support");
        let mut push = |id: &'static str, name: &'static str, brand: Brand,
                        bins: Vec<PathBuf>, roots: Vec<PathBuf>| {
            out.push((id, name, brand, bins, roots));
        };
        push(
            "chrome",
            "Google Chrome",
            Brand::Chrome,
            vec![
                apps.join("Google Chrome.app/Contents/MacOS/Google Chrome"),
                user_apps.join("Google Chrome.app/Contents/MacOS/Google Chrome"),
            ],
            vec![sup.join("Google/Chrome")],
        );
        push(
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![
                apps.join("Chromium.app/Contents/MacOS/Chromium"),
                user_apps.join("Chromium.app/Contents/MacOS/Chromium"),
            ],
            vec![sup.join("Chromium")],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                apps.join("Brave Browser.app/Contents/MacOS/Brave Browser"),
                user_apps.join("Brave Browser.app/Contents/MacOS/Brave Browser"),
            ],
            vec![sup.join("BraveSoftware/Brave-Browser")],
        );
        push(
            "edge",
            "Microsoft Edge",
            Brand::Edge,
            vec![
                apps.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
                user_apps.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            ],
            vec![sup.join("Microsoft Edge")],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![
                apps.join("Vivaldi.app/Contents/MacOS/Vivaldi"),
                user_apps.join("Vivaldi.app/Contents/MacOS/Vivaldi"),
            ],
            vec![sup.join("Vivaldi")],
        );
        push(
            "opera",
            "Opera",
            Brand::Opera,
            vec![
                apps.join("Opera.app/Contents/MacOS/Opera"),
                user_apps.join("Opera.app/Contents/MacOS/Opera"),
            ],
            vec![sup.join("com.operasoftware.Opera")],
        );
    }

    #[cfg(target_os = "windows")]
    {
        // Env-var roots: a missing variable must never produce a RELATIVE
        // candidate (`PathBuf::from("").join(x)` is cwd-relative and could
        // accidentally exist). Absent base → no candidate.
        let ev = |var: &str| {
            std::env::var(var)
                .ok()
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
        };
        let local = ev("LOCALAPPDATA");
        let roaming = ev("APPDATA");
        let pf = ev("PROGRAMFILES");
        let pf86 = ev("PROGRAMFILES(X86)");
        let j = |b: &Option<PathBuf>, rel: &str| b.as_ref().map(|b| b.join(rel));
        let mut push = |id: &'static str, name: &'static str, brand: Brand,
                        bins: Vec<Option<PathBuf>>, roots: Vec<Option<PathBuf>>| {
            out.push((
                id,
                name,
                brand,
                bins.into_iter().flatten().collect(),
                roots.into_iter().flatten().collect(),
            ));
        };
        push(
            "chrome",
            "Google Chrome",
            Brand::Chrome,
            vec![
                j(&local, "Google/Chrome/Application/chrome.exe"),
                j(&pf, "Google/Chrome/Application/chrome.exe"),
                j(&pf86, "Google/Chrome/Application/chrome.exe"),
            ],
            vec![j(&local, "Google/Chrome/User Data")],
        );
        push(
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![j(&local, "Chromium/Application/chrome.exe")],
            vec![j(&local, "Chromium/User Data")],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                j(&local, "BraveSoftware/Brave-Browser/Application/brave.exe"),
                j(&pf, "BraveSoftware/Brave-Browser/Application/brave.exe"),
            ],
            vec![j(&local, "BraveSoftware/Brave-Browser/User Data")],
        );
        push(
            "edge",
            "Microsoft Edge",
            Brand::Edge,
            vec![
                j(&pf86, "Microsoft/Edge/Application/msedge.exe"),
                j(&pf, "Microsoft/Edge/Application/msedge.exe"),
            ],
            vec![j(&local, "Microsoft/Edge/User Data")],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![j(&local, "Vivaldi/Application/vivaldi.exe")],
            vec![j(&local, "Vivaldi/User Data")],
        );
        push(
            "opera",
            "Opera",
            Brand::Opera,
            vec![
                j(&local, "Programs/Opera/opera.exe"),
                j(&pf, "Programs/Opera/opera.exe"),
            ],
            // Opera's profile lives in Roaming (APPDATA) on Windows — its
            // LOCALAPPDATA entry is the install location, not the profile.
            vec![
                j(&roaming, "Opera Software/Opera Stable"),
                j(&local, "Opera Software/Opera Stable"),
            ],
        );
    }

    out
}

/// Installed browsers (binary OR profile root present), in table order.
pub fn discover() -> Vec<BrowserSpec> {
    let mut found = Vec::new();
    for (id, name, brand, bins, roots) in candidates() {
        let binary = bins.iter().find(|p| p.exists()).cloned();
        let root = roots.iter().find(|p| p.exists()).cloned();
        let root = match (root, binary.as_ref()) {
            (Some(r), _) => r,
            // Binary without a profile root: show the canonical path. Never
            // index blindly — a platform table may legitimately leave the
            // root list empty (e.g. a missing %%APPDATA%% on Windows).
            (None, Some(_)) => roots.first().cloned().unwrap_or_default(),
            (None, None) => continue,
        };
        found.push(BrowserSpec {
            id: id.to_string(),
            name: name.to_string(),
            brand,
            binary: binary.unwrap_or_default(),
            profile_root: root,
        });
    }
    found
}

/// Find one browser by id.
pub fn find_browser(id: &str) -> Option<BrowserSpec> {
    discover().into_iter().find(|b| b.id == id)
}

// ── Profiles ────────────────────────────────────────────────────────────

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

// ── Live-browser probes (attach + safety checks) ────────────────────────

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

/// The single pause-refusal error, so every gate speaks identically.
pub fn paused_error() -> BladeError {
    BladeError::Other(
        "paused — manual control is claimed (`bladebro rb resume` to hand it back)".into(),
    )
}

// ── Template management (clone lane) ────────────────────────────────────

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
    let tmp = root.join("template.tmp");
    let _ = std::fs::remove_dir_all(&tmp);

    let started = std::time::Instant::now();
    crate::session_profile::copy_profile_ex(
        src,
        &tmp,
        &["Current Session", "Current Tabs", "Last Session", "Last Tabs"],
    );
    if !tmp.is_dir() {
        return Err(BladeError::Other("profile copy produced no directory".into()));
    }

    // Atomic-ish swap (same discipline as the agent-lane template).
    let template = template_dir(id);
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
    let _ = platform::secure_write_file(&source_meta_path(id), meta.to_string().as_bytes());

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

// ── Selection (which browser + which profile the lane uses) ─────────────

/// Resolve an absolute profile override. Accepts either a Chrome
/// user-data-dir ROOT (contains `Default/` etc.) or a profile subdir that
/// contains `Preferences` directly — the latter is reinterpreted as its
/// parent root + dir name, because `--user-data-dir` and the clone must
/// operate on the root.
fn resolve_abs_profile(path: &Path) -> Result<ProfileInfo> {
    if !path.is_dir() {
        return Err(BladeError::Other(format!(
            "profile dir not found: {}",
            path.display()
        )));
    }
    if path.join("Preferences").is_file() {
        let root = path.parent().unwrap_or(path).to_path_buf();
        let key = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Default".into());
        return Ok(ProfileInfo {
            name: key.clone(),
            key,
            path: path.to_path_buf(),
            root,
            last_used: 0,
        });
    }
    let mut profiles = list_profiles(path);
    if let Some(first) = profiles.drain(..).next() {
        return Ok(ProfileInfo {
            key: first.key,
            name: first.name,
            path: first.path,
            root: path.to_path_buf(),
            last_used: first.last_used,
        });
    }
    // Empty/foreign dir: treat as a root with Chrome's default profile name.
    Ok(ProfileInfo {
        key: "Default".into(),
        name: "Default".into(),
        path: path.join("Default"),
        root: path.to_path_buf(),
        last_used: 0,
    })
}

/// Validate a `rb use --binary` override: the path must exist and (Unix)
/// carry an execute bit. Failing at set time (and again at resolve time)
/// beats a spawn failure inside a later launch.
pub fn validate_binary_override(path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if !p.is_file() {
        return Err(BladeError::Other(format!("binary not found: {path}")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exec = std::fs::metadata(&p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !exec {
            return Err(BladeError::Other(format!(
                "binary is not executable: {path}"
            )));
        }
    }
    Ok(p)
}

/// The id of the only realbrowser root on disk, when exactly one exists —
/// `rb forget` after the browser itself was uninstalled still needs a
/// target, and at that point discovery cannot name one.
pub fn sole_root_id() -> Option<String> {
    let dir = platform::blade_dir().join("realbrowser");
    let mut ids: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    ids.sort();
    if ids.len() == 1 {
        ids.pop()
    } else {
        None
    }
}

/// Resolve the configured (or best) browser + profile.
pub fn resolve_selection(cfg: &Config) -> Result<(BrowserSpec, ProfileInfo)> {
    let browsers = discover();
    if browsers.is_empty() {
        return Err(BladeError::Other(
            "no Chromium-family browser found. Install one (Chromium, Chrome, Brave, Edge, Vivaldi, Opera) or set a custom binary via `bladebro rb use --binary <path>`.".into(),
        ));
    }

    let mut spec = match cfg.browser.as_deref() {
        Some(id) => browsers
            .iter()
            .find(|b| b.id == id)
            .cloned()
            .ok_or_else(|| {
                let avail: Vec<&str> = browsers.iter().map(|b| b.id.as_str()).collect();
                BladeError::Other(format!(
                    "browser `{id}` not found. Available: {}",
                    avail.join(", ")
                ))
            })?,
        None => browsers
            .iter()
            .max_by_key(|b| {
                list_profiles(&b.profile_root)
                    .first()
                    .map(|p| p.last_used)
                    .unwrap_or(0)
            })
            .cloned()
            .expect("non-empty"),
    };

    // `rb use --binary`: an explicit override always wins over discovery.
    // Re-validated here so a binary deleted since it was set fails with the
    // path named, not with a spawn error inside a launch.
    if let Some(ov) = cfg.binary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        spec.binary = validate_binary_override(ov)?;
    }

    // Profile: absolute path override → use as-is; key → lookup; else most
    // recently used.
    let profiles = list_profiles(&spec.profile_root);
    let profile = match cfg.profile.as_deref() {
        Some(p) if Path::new(p).is_absolute() => resolve_abs_profile(Path::new(p))?,
        Some(key) => profiles
            .iter()
            .find(|p| p.key == key)
            .cloned()
            .ok_or_else(|| {
                let avail: Vec<&str> = profiles.iter().map(|p| p.key.as_str()).collect();
                BladeError::Other(format!(
                    "profile `{key}` not found in {}. Available: {}",
                    spec.profile_root.display(),
                    if avail.is_empty() { "(none)".to_string() } else { avail.join(", ") }
                ))
            })?,
        None => profiles.first().cloned().ok_or_else(|| {
            if spec.profile_root.is_dir() {
                BladeError::Other(format!(
                    "no profiles found under {}",
                    spec.profile_root.display()
                ))
            } else {
                BladeError::Other(format!(
                    "profile root {} does not exist yet — launch {} once so it creates one, \
                     or point `bladebro rb profile <absolute path>` at an existing profile",
                    spec.profile_root.display(),
                    spec.name
                ))
            }
        })?,
    };

    Ok((spec, profile))
}

/// Resolve `auto`: attach when a live debug endpoint already exists on the
/// selected profile's user-data root (best fidelity, zero disruption, never
/// owned), else clone. Profile mode is never picked silently — it needs the
/// user's browser closed, so it stays an explicit choice.
pub fn effective_mode(cfg: &Config, user_data_root: &Path) -> Mode {
    match cfg.mode {
        Mode::Auto => {
            if devtools_port(user_data_root).is_some() {
                Mode::Attach
            } else {
                Mode::Clone
            }
        }
        m => m,
    }
}

/// Import-on-first-use for the clone lane — shared by `rb on` and the
/// launch path so their behavior cannot drift apart.
pub fn ensure_import(spec: &BrowserSpec, profile: &ProfileInfo) -> Result<ImportStats> {
    if let Some(owner) = profile_in_use(&profile.root) {
        eprintln!(
            "{} your browser ({owner}) is open — importing its profile now. The \
             copy may miss the last few writes (SQLite snapshot); the source is never written \
             to. `bladebro rb refresh` with the browser closed gives a clean copy.",
            crate::ui::dim("[realbrowser]")
        );
    }
    eprintln!(
        "{} importing {} profile `{}` ({})...",
        crate::ui::dim("[realbrowser]"),
        spec.name,
        profile.name,
        profile.path.display()
    );
    let stats = import_template(&spec.id, &profile.root)?;
    eprintln!(
        "{} imported {} files ({}) in {}ms",
        crate::ui::dim("[realbrowser]"),
        stats.files,
        human_bytes(stats.bytes),
        stats.ms
    );
    Ok(stats)
}

// ── Pause (manual control handover) ─────────────────────────────────────

/// `<data-dir>/realbrowser-pause` — while this file exists, input-
/// dispatching actions refuse to run and the idle hum stays silent, so the
/// user can use the browser without the agent fighting them.
pub fn pause_path() -> PathBuf {
    platform::blade_dir().join("realbrowser-pause")
}

/// True while manual control is claimed. Honored on every lane (harmless
/// on the agent lane; the point is the real one).
pub fn input_paused() -> bool {
    pause_path().exists()
}

pub fn set_paused(paused: bool) -> Result<()> {
    let path = pause_path();
    if paused {
        if let Some(parent) = path.parent() {
            platform::secure_create_dir_all(parent)
                .map_err(|e| BladeError::Other(format!("cannot create data dir: {e}")))?;
        }
        platform::secure_write_file(&path, b"paused")
            .map_err(|e| BladeError::Other(format!("cannot write pause marker: {e}")))?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

// ── Idle policy ─────────────────────────────────────────────────────────

/// Whether the daemon/MCP idle timeout may close the lane's browser. On the
/// real lane the browser is the user's — default no, opt-in via config.
pub fn should_idle_shutdown() -> bool {
    if !real_lane() {
        return true;
    }
    config().idle_shutdown
}

/// Whether the idle-hum behavior layer runs on this lane.
pub fn hum_enabled() -> bool {
    if !real_lane() {
        return true;
    }
    config().idle_hum && !input_paused()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_from_version_parses_every_family() {
        assert_eq!(
            brand_from_version("Google Chrome 151.0.7922.108"),
            Some(Brand::Chrome)
        );
        assert_eq!(brand_from_version("Chromium 151.0.7922.108"), Some(Brand::Chromium));
        assert_eq!(brand_from_version("Brave Browser 1.79.126"), Some(Brand::Brave));
        assert_eq!(brand_from_version("Microsoft Edge 151.0.0.0"), Some(Brand::Edge));
        assert_eq!(brand_from_version("Vivaldi 7.5.3735.58"), Some(Brand::Vivaldi));
        assert_eq!(brand_from_version("Opera 118.0.0.0"), Some(Brand::Opera));
        assert_eq!(brand_from_version("Mozilla Firefox 141"), None);
    }

    #[test]
    fn only_branded_chrome_requires_a_non_default_dir() {
        assert!(requires_non_default_dir(Brand::Chrome));
        for b in [
            Brand::Chromium,
            Brand::Brave,
            Brand::Edge,
            Brand::Vivaldi,
            Brand::Opera,
        ] {
            assert!(!requires_non_default_dir(b), "{b:?} must not be gated");
        }
    }

    #[test]
    fn devtools_active_port_parses_first_line_only() {
        assert_eq!(parse_devtools_active_port("9222\n/devtools/browser/abc\n"), Some(9222));
        assert_eq!(parse_devtools_active_port("0\n"), None);
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("not-a-port\n"), None);
    }

    #[test]
    fn singleton_owner_parses_hostname_pid() {
        assert_eq!(parse_singleton_owner("myhost-4242"), Some(4242));
        assert_eq!(parse_singleton_owner("host-with-dashes-7"), Some(7));
        assert_eq!(parse_singleton_owner("nopid"), None);
    }

    #[cfg(unix)]
    #[test]
    fn profile_in_use_reads_live_and_stale_singleton_locks() {
        let root = std::env::temp_dir().join(format!("blade-rb-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(profile_in_use(&root), None, "no lock → not in use");

        let live = format!("host-{}", std::process::id());
        std::os::unix::fs::symlink(&live, root.join("SingletonLock")).unwrap();
        assert_eq!(
            profile_in_use(&root),
            Some(format!("pid {}", std::process::id())),
            "a lock held by a live pid reads as in use"
        );

        std::fs::remove_file(root.join("SingletonLock")).unwrap();
        // Beyond any pid_max: kill(pid, 0) errors → not alive → stale.
        std::os::unix::fs::symlink("host-99999999", root.join("SingletonLock")).unwrap();
        assert_eq!(profile_in_use(&root), None, "a dead pid is a stale lock");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lane_decision_prefers_env_over_config() {
        assert!(lane_from(Some("real"), false), "env real beats a disabled config");
        assert!(!lane_from(Some("agent"), true), "env agent beats an enabled config");
        assert!(lane_from(None, true));
        assert!(!lane_from(None, false));
        // Anything that is not exactly real|agent falls through to the
        // config — a typo must never silently flip the lane.
        assert!(!lane_from(Some("Real"), false), "a typo does not force the real lane");
        assert!(lane_from(Some("agentx"), true), "a typo does not force the agent lane either");
    }

    #[test]
    fn fingerprint_tracks_launch_inputs_only_on_the_real_lane() {
        let base = Config {
            enabled: true,
            ..Default::default()
        };
        let fp = launch_fingerprint_from(None, &base);
        assert_eq!(fp, launch_fingerprint_from(None, &base), "stable across reads");

        let mut other = base.clone();
        other.visible = false;
        assert_ne!(fp, launch_fingerprint_from(None, &other), "visible is a launch input");
        let mut other = base.clone();
        other.mode = Mode::Profile;
        assert_ne!(fp, launch_fingerprint_from(None, &other), "mode is a launch input");
        let mut other = base.clone();
        other.profile = Some("Work".into());
        assert_ne!(fp, launch_fingerprint_from(None, &other), "profile is a launch input");

        // The lane itself is part of the fingerprint: on→off must drift.
        let off = Config { enabled: false, ..base.clone() };
        assert_ne!(fp, launch_fingerprint_from(None, &off));

        // Agent lane (env override): the real-lane fields cannot affect an
        // agent browser, so the fingerprint stays put — no pointless relaunch.
        let a1 = Config { enabled: true, mode: Mode::Clone, visible: true, profile: Some("Work".into()), ..Default::default() };
        let a2 = Config { enabled: true, mode: Mode::Profile, visible: false, profile: None, ..Default::default() };
        assert_eq!(
            launch_fingerprint_from(Some("agent"), &a1),
            launch_fingerprint_from(Some("agent"), &a2)
        );
        // ...and the agent lane never equals the real lane.
        assert_ne!(
            launch_fingerprint_from(Some("agent"), &a1),
            launch_fingerprint_from(None, &a1)
        );
    }

    #[test]
    fn binary_override_must_exist_and_be_executable() {
        let dir = std::env::temp_dir().join(format!("blade-rb-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("chrome");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(validate_binary_override(&exe.display().to_string()).is_ok());
        assert!(validate_binary_override("/nonexistent/bladebro-browser").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let noexec = dir.join("noexec");
            std::fs::write(&noexec, b"x").unwrap();
            std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                validate_binary_override(&noexec.display().to_string()).is_err(),
                "a non-executable file must be refused"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_round_trips_and_survives_corruption() {
        let dir = std::env::temp_dir().join(format!("blade-rb-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("realbrowser.json");

        let mut cfg = Config::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.mode, Mode::Auto);
        assert!(cfg.visible, "visible must default on");
        assert!(!cfg.idle_shutdown, "idle shutdown must default off");
        assert!(cfg.binary.is_none(), "no binary override by default");
        cfg.enabled = true;
        cfg.mode = Mode::Clone;
        cfg.browser = Some("brave".into());
        save_config_to(&path, &cfg).expect("save");
        let back = config_from(&path);
        assert!(back.enabled && back.mode == Mode::Clone && back.browser.as_deref() == Some("brave"));

        std::fs::write(&path, b"{ not json").unwrap();
        let corrupt = config_from(&path);
        assert!(!corrupt.enabled, "corruption reads as default-off, never a crash");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_profiles_reads_local_state_and_scans_dirs() {
        let root = std::env::temp_dir().join(format!("blade-rb-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Default")).unwrap();
        std::fs::create_dir_all(root.join("Profile 1")).unwrap();
        std::fs::write(root.join("Default/Preferences"), "{}").unwrap();
        std::fs::write(root.join("Profile 1/Preferences"), "{}").unwrap();
        std::fs::write(
            root.join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"Main"},"Profile 1":{"name":"Work"}}}}"#,
        )
        .unwrap();

        let got = list_profiles(&root);
        let keys: Vec<(&str, &str)> = got.iter().map(|p| (p.key.as_str(), p.name.as_str())).collect();
        assert!(keys.contains(&("Default", "Main")), "got {keys:?}");
        assert!(keys.contains(&("Profile 1", "Work")), "got {keys:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pause_marker_round_trips_via_env_home() {
        // blade_dir() is env-driven; point it at a scratch home so the test
        // never touches a real install. (set_var is process-global — same
        // pattern the platform tests use, and this test owns the name.)
        let home = std::env::temp_dir().join(format!("blade-rb-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let key = "BLADE_HOME";
        let prev = std::env::var(key).ok();
        std::env::set_var(key, &home);
        assert!(!input_paused());
        set_paused(true).expect("pause");
        assert!(input_paused());
        set_paused(false).expect("resume");
        assert!(!input_paused());
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
