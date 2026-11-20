//! Selection — pick the browser + profile the lane uses, gate binary overrides,
//! and the drift decision shared by the long-lived surfaces.

use super::*;
use crate::error::{BladeError, Result};
use crate::platform;
use std::path::{Path, PathBuf};

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
    // Store ABSOLUTE: a relative override resolved against the spawning
    // shell's cwd would silently break in another process (daemon, MCP) that
    // runs with a different cwd. Never resolve symlinks: launchers that
    // dispatch on argv[0] (`/snap/bin/chromium` → `/usr/bin/snap`) only work
    // under the name they were invoked by.
    let p = std::path::absolute(&p).unwrap_or(p);
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
    let binary_override = cfg
        .binary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut spec = if browsers.is_empty() {
        // Portable/AppImage-only hosts: if the user explicitly set `--binary`
        // (and typically an absolute `rb profile <dir>`), allow it instead of
        // refusing because none of the standard system paths matched.
        if let Some(ov) = binary_override {
            let bin = validate_binary_override(ov)?;
            let brand = brand_from_binary_path(&bin).unwrap_or(Brand::Chromium);
            let root = cfg
                .profile
                .as_deref()
                .filter(|p| Path::new(p).is_absolute())
                .map(PathBuf::from)
                .unwrap_or_default();
            BrowserSpec {
                id: cfg.browser.clone().unwrap_or_else(|| "custom".into()),
                name: format!("{} (custom)", brand.as_str()),
                brand,
                binary: bin,
                profile_root: root,
            }
        } else {
            return Err(BladeError::Other(
                "no Chromium-family browser found. Install one (Chromium, Chrome, Brave, Helium, Arc, Edge, Vivaldi, Opera, Thorium) or set a custom binary via `bladebro rb use --binary <path>`.".into(),
            ));
        }
    } else {
        match cfg.browser.as_deref() {
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
            None => {
                // Prefer browsers that are actually usable (have a launchable
                // binary, an explicit `--binary` override, or a live debug
                // endpoint) over uninstalled ghost profile dirs left on disk.
                let usable: Vec<&BrowserSpec> = browsers
                    .iter()
                    .filter(|b| {
                        binary_override.is_some()
                            || !b.binary.as_os_str().is_empty()
                            || devtools_port_live(&b.profile_root).is_some()
                    })
                    .collect();
                let pool: Vec<&BrowserSpec> = if usable.is_empty() {
                    browsers.iter().collect()
                } else {
                    usable
                };
                // Most recently used wins; equal recency keeps the FIRST
                // browser in table order (chromium first). `max_by_key`
                // returns the LAST maximum — on a machine where every profile
                // is equally fresh that silently picked the last table browser.
                let recency: Vec<u64> = pool
                    .iter()
                    .map(|b| {
                        list_profiles(&b.profile_root)
                            .first()
                            .map(|p| p.last_used)
                            .unwrap_or(0)
                    })
                    .collect();
                let idx = pick_recency_index(&recency).expect("non-empty");
                pool[idx].clone()
            }
        }
    };

    // `rb use --binary`: an explicit override always wins over discovery.
    // Re-validated here so a binary deleted since it was set fails with the
    // path named, not with a spawn error inside a launch.
    if let Some(ov) = binary_override {
        let bin = validate_binary_override(ov)?;
        if let Some(inferred) = brand_from_binary_path(&bin) {
            spec.brand = inferred;
        }
        spec.binary = bin;
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
                    if avail.is_empty() {
                        "(none)".to_string()
                    } else {
                        avail.join(", ")
                    }
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

/// Index of the first maximum in `values` — ties keep the EARLIER entry, so
/// the browser table order (chromium first) is the preference order. Rust's
/// `max_by_key` returns the LAST maximum, which silently picked the last
/// table browser whenever every profile was equally fresh.
pub(super) fn pick_recency_index(values: &[u64]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (i, &v) in values.iter().enumerate() {
        if best.map(|(_, bv)| v > bv).unwrap_or(true) {
            best = Some((i, v));
        }
    }
    best.map(|(i, _)| i)
}

/// Drift decision shared by the long-lived surfaces (daemon + MCP): a LIVE
/// session (`session_live` — a page attached, owned or not) whose lane or
/// launch inputs changed since its launch must be switched at the next
/// action. Attach sessions have no owned browser but are still driven by
/// bladebro — `rb off` must detach them, not keep steering the user's browser.
/// Settled sessions (nothing changed) and sessions with no live page are
/// exempt, so agent-lane sessions never pay a pointless relaunch.
pub fn session_drifted(
    session_live: bool,
    lane_switched: bool,
    fingerprint: u64,
    launched: u64,
) -> bool {
    session_live && (lane_switched || fingerprint != launched)
}

/// Resolve `auto`: attach when a live debug endpoint already exists on the
/// selected profile's user-data root (best fidelity, zero disruption, never
/// owned), else clone. Profile mode is never picked silently — it needs the
/// user's browser closed, so it stays an explicit choice.
pub fn effective_mode(cfg: &Config, user_data_root: &Path) -> Mode {
    match cfg.mode {
        Mode::Auto => {
            // SECURITY & RELIABILITY: `Auto` must not attach on a dead flag
            // file — `devtools_port_live` requires both a live browser of this
            // user (Linux) and a reachable loopback port (all OSes) so stale
            // DevToolsActivePort files fall back cleanly to Clone.
            if devtools_port_live(user_data_root).is_some() {
                Mode::Attach
            } else {
                Mode::Clone
            }
        }
        m => m,
    }
}

/// Import unless the clone already holds the selected profile's user-data
/// root AND is still fresh relative to the user's live profile — shared by
/// `rb on` and the launch path so their behavior cannot drift apart.
///
/// Re-imports automatically when:
/// 1. No clone exists yet.
/// 2. The clone was imported from a different user-data root (`rb profile <abs>`).
/// 3. The user's real browser wrote newer auth/storage state (`Cookies`,
///    `Local Storage`, `Preferences`, `Local State`, etc.) since the last import.
///
/// Returns `Ok(Some(stats))` when an import ran, `Ok(None)` when the existing
/// clone was already up to date.
pub fn ensure_current_import(
    spec: &BrowserSpec,
    profile: &ProfileInfo,
) -> Result<Option<ImportStats>> {
    if template_is_fresh(&spec.id, &profile.root, &profile.key) {
        return Ok(None);
    }
    if has_template(&spec.id) {
        if !template_matches(&spec.id, &profile.root) {
            if let Some(old) = template_source(&spec.id) {
                eprintln!(
                    "{} the clone holds {old}, but the selected profile lives in {} — re-importing",
                    crate::ui::err::dim("[realbrowser]"),
                    profile.root.display()
                );
            }
        } else {
            eprintln!(
                "{} source profile `{}` has newer logins/state since the last import — refreshing clone",
                crate::ui::err::dim("[realbrowser]"),
                profile.name
            );
        }
    }
    ensure_import(spec, profile).map(Some)
}

/// Import (or re-import) the selected profile's root into the clone.
pub fn ensure_import(spec: &BrowserSpec, profile: &ProfileInfo) -> Result<ImportStats> {
    let running_owner = profile_in_use(&profile.root);
    if let Some(owner) = &running_owner {
        eprintln!(
            "{} your browser ({owner}) is open — importing its profile now. Very recent \
             in-memory cookie writes may not have flushed to disk yet; the source is never \
             written to. Close it and run `bladebro rb refresh` for a guaranteed clean snapshot.",
            crate::ui::err::dim("[realbrowser]")
        );
    }
    eprintln!(
        "{} importing {} profile `{}` ({})...",
        crate::ui::err::dim("[realbrowser]"),
        spec.name,
        profile.name,
        profile.path.display()
    );
    let stats = import_template(&spec.id, &profile.root)?;
    eprintln!(
        "{} imported {} files ({}) in {}ms",
        crate::ui::err::dim("[realbrowser]"),
        stats.files,
        human_bytes(stats.bytes),
        stats.ms
    );
    if !stats.failed_critical.is_empty() {
        let joined = stats.failed_critical.join(", ");
        let msg = format!(
            "real-browser clone could not copy locked profile file(s): {joined}. \
             Logins/cookies may be missing — close {} and run `bladebro rb refresh`, \
             or attach via `chrome://inspect#remote-debugging` / `--remote-debugging-port`.",
            spec.name
        );
        eprintln!("{} WARNING: {msg}", crate::ui::err::dim("[realbrowser]"));
        crate::browser::set_rb_note(format!("warning: {msg}"));
    }
    Ok(stats)
}
