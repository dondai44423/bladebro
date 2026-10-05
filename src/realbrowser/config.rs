//! Real-browser config (`realbrowser.json`): the lane switch, mode, browser/
//! profile/binary overrides, visibility and idle settings — mtime-cached so
//! per-call readers don't re-parse.

use crate::error::{BladeError, Result};
use crate::platform;
use std::path::{Path, PathBuf};

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
