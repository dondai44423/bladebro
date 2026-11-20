//! System diagnostics — the Mafia Don "System Dossier" (`bladebro doctor`).
//!
//! Runs local filesystem/environment probes immediately and executes async
//! checks (Chrome `--version` + one shared release-registry check) **in
//! parallel**, cutting wall-clock latency roughly in half while avoiding
//! redundant GitHub requests and subprocesses (`statvfs` / `GetDiskFreeSpaceExW`
//! for disk space, pure Rust `PATH` scan for Xvfb).

use std::fmt;
use std::time::Instant;

use super::ui;
use super::version;
use crate::error::Result;

/// Diagnostic section grouping for the dossier.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Environment,
    Browser,
    Vault,
    Network,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::Environment => "Environment",
            Group::Browser => "Browser & Display",
            Group::Vault => "Vault & Hygiene",
            Group::Network => "Network & Release",
        }
    }
}

/// A single diagnostic check result.
struct Check {
    group: Group,
    name: &'static str,
    status: Status,
    detail: String,
    fix: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn tone(self) -> ui::Tone {
        match self {
            Status::Pass => ui::Tone::Success,
            Status::Warn => ui::Tone::Warn,
            Status::Fail => ui::Tone::Fail,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Pass => write!(f, "{}", ui::green("PASS")),
            Status::Warn => write!(f, "{}", ui::yellow("WARN")),
            Status::Fail => write!(f, "{}", ui::red("FAIL")),
        }
    }
}

/// Run all diagnostic checks and print the Mafia Don System Dossier.
pub async fn run() -> Result<()> {
    let started = Instant::now();
    let meta = format!(
        "{} · {}",
        version::platform_label(),
        version::install_channel_label()
    );
    ui::masthead("System Dossier", &meta);

    let spin = ui::Spinner::start("Inspecting environment, browser, vault & release registry...");

    // Synchronous local checks (microseconds — zero subprocesses).
    let os_check = check_os();
    let data_dir_check = check_data_dir();
    let binary_check = check_binary();
    let disk_check = check_disk_space();

    let chrome_check = check_chrome();
    let chrome_found = chrome_check.status == Status::Pass;
    let browser_mode_check = check_browser_mode();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let xvfb_check = check_xvfb();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let wm_check_res = check_window_manager();

    let profile_dir_check = check_profile_dir();
    let login_check = check_login_sidecar();
    let knowledge_check = check_knowledge_and_cache();
    let hygiene_check = check_profile_hygiene();
    let locks_check = check_stale_locks();

    // Run async checks IN PARALLEL — and share ONE release check between
    // GitHub connectivity and Version (instead of two sequential HTTP calls).
    let (chrome_ver_opt, latest_res) = tokio::join!(
        async {
            if chrome_found {
                Some(check_chrome_version().await)
            } else {
                None
            }
        },
        version::fetch_latest_tag(),
    );

    let network_check = check_network_from(&latest_res);
    let version_check = check_version_from(&latest_res);

    spin.finish_clear();

    let mut checks: Vec<Check> = vec![
        os_check,
        data_dir_check,
        binary_check,
        disk_check,
        chrome_check,
    ];
    if let Some(cv) = chrome_ver_opt {
        checks.push(cv);
    }
    checks.push(browser_mode_check);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        checks.push(xvfb_check);
        checks.push(wm_check_res);
    }
    checks.push(profile_dir_check);
    checks.push(login_check);
    checks.push(knowledge_check);
    checks.push(hygiene_check);
    checks.push(locks_check);
    checks.push(network_check);
    checks.push(version_check);

    // Compute exact aligned column width from all active checks (never overflows).
    let label_width = checks
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(20);

    let mut passes = 0usize;
    let mut warns = 0usize;
    let mut fails = 0usize;

    for group in [
        Group::Environment,
        Group::Browser,
        Group::Vault,
        Group::Network,
    ] {
        let group_checks: Vec<&Check> = checks.iter().filter(|c| c.group == group).collect();
        if group_checks.is_empty() {
            continue;
        }
        ui::section(group.title());
        for check in group_checks {
            ui::dossier_row(
                check.status.tone(),
                check.name,
                &check.detail,
                check.fix.as_deref(),
                label_width,
            );
            match check.status {
                Status::Pass => passes += 1,
                Status::Warn => warns += 1,
                Status::Fail => fails += 1,
            }
        }
    }

    let total = checks.len();
    let elapsed = ui::format_elapsed(started.elapsed());
    if fails == 0 && warns == 0 {
        ui::verdict_card(
            ui::Tone::Success,
            &format!("ALL {total} CHECKS CLEAR · BLADEBRO IS HEALTHY"),
            &elapsed,
            &[],
        );
    } else if fails == 0 {
        ui::verdict_card(
            ui::Tone::Warn,
            &format!(
                "{passes}/{total} PASSED · {warns} ADVISOR{}",
                if warns == 1 { "Y" } else { "IES" }
            ),
            &elapsed,
            &[(
                "note",
                "Bladebro is operational; review advisories above".into(),
            )],
        );
    } else {
        ui::verdict_card(
            ui::Tone::Fail,
            &format!(
                "{passes}/{total} PASSED · {fails} FAILED · {warns} WARNING{}",
                if warns == 1 { "" } else { "S" }
            ),
            &elapsed,
            &[(
                "next",
                "Fix the failures above, then run `bladebro doctor` again".into(),
            )],
        );
        return Err(crate::error::BladeError::Other(format!(
            "doctor: {fails} failed check(s)"
        )));
    }
    Ok(())
}

fn check_os() -> Check {
    Check {
        group: Group::Environment,
        name: "Operating system",
        status: Status::Pass,
        detail: version::platform_label(),
        fix: None,
    }
}

fn check_knowledge_and_cache() -> Check {
    let kb = crate::knowledge::KnowledgeBase::load();
    let s = kb.summary();
    let cache_root = crate::platform::blade_dir().join("http-cache");
    let cache_slots = std::fs::read_dir(&cache_root)
        .ok()
        .map(|it| {
            it.flatten()
                .filter(|e| {
                    e.file_type().is_ok_and(|t| t.is_dir())
                        && e.file_name().to_string_lossy().starts_with("slot-")
                })
                .count()
        })
        .unwrap_or(0);
    let detail = format!(
        "{} domains ({} consent, {} site-chrome, {} chirpy, {} heavy-risk; {} links folded); HTTP cache: {} warm slot(s)",
        s.domains,
        s.consent_rules,
        s.chrome_domains,
        s.chirpy_domains,
        s.heavy_risk_domains,
        s.total_chrome_folded,
        cache_slots,
    );
    Check {
        group: Group::Vault,
        name: "Knowledge & cache",
        status: Status::Pass,
        detail,
        fix: None,
    }
}

fn check_data_dir() -> Check {
    let dir = crate::platform::blade_dir();
    let home = crate::platform::home_dir().join(".blade");
    let reason = if std::env::var("BLADE_HOME")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        "BLADE_HOME"
    } else if std::env::var("XDG_STATE_HOME")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        if dir == home {
            "existing install kept on legacy dir"
        } else {
            "XDG_STATE_HOME"
        }
    } else if dir == home {
        if home.join("profile").exists()
            || home.join("logins.json").exists()
            || home.join("knowledge").exists()
            || home.join(".fingerprint.json").exists()
        {
            "existing install kept on legacy dir"
        } else {
            "legacy default (no .local/state)"
        }
    } else {
        "XDG default"
    };
    Check {
        group: Group::Environment,
        name: "Data directory",
        status: Status::Pass,
        detail: format!("{} ({reason}; override with BLADE_HOME)", dir.display()),
        fix: None,
    }
}

fn check_browser_mode() -> Check {
    if let Err(error) = crate::browser::env_chrome_flags() {
        return Check {
            group: Group::Browser,
            name: "Chrome flags",
            status: Status::Fail,
            detail: error.to_string(),
            fix: Some(
                "Fix or unset BLADE_CHROME_FLAGS; every browser launch fails until then".into(),
            ),
        };
    }
    match crate::browser::incognito_policy() {
        Ok(true) => Check {
            group: Group::Browser,
            name: "Agent browser mode",
            status: Status::Warn,
            detail: "incognito compatibility (cookies/named sessions supported; history/cache and automatic localStorage persistence unavailable)".into(),
            fix: Some("BLADE_INCOGNITO=0 disables automatic Raspberry Pi OS trixie compatibility; restart the client/daemon".into()),
        },
        Ok(false) => Check {
            group: Group::Browser,
            name: "Agent browser mode",
            status: Status::Pass,
            detail: "persistent profile".into(),
            fix: None,
        },
        Err(error) => Check {
            group: Group::Browser,
            name: "Agent browser mode",
            status: Status::Fail,
            detail: error.to_string(),
            fix: Some("Set BLADE_INCOGNITO to auto, 0 or 1".into()),
        },
    }
}

fn check_chrome() -> Check {
    match crate::browser::find_chrome() {
        Ok(path) => Check {
            group: Group::Browser,
            name: "Chrome/Chromium",
            status: Status::Pass,
            detail: path,
            fix: None,
        },
        Err(error) => Check {
            group: Group::Browser,
            name: "Chrome/Chromium",
            status: Status::Fail,
            detail: error.to_string(),
            fix: Some(chrome_install_hint()),
        },
    }
}

async fn check_chrome_version() -> Check {
    let path = match find_chrome_path() {
        Some(p) => p,
        None => {
            return Check {
                group: Group::Browser,
                name: "Chrome version",
                status: Status::Fail,
                detail: "Chrome not found".into(),
                fix: None,
            }
        }
    };

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(&path)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match output {
        Ok(Ok(o)) if o.status.success() => {
            let version_str = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let version_token = version_str.split_whitespace().find(|t| {
                t.chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
                    && t.contains('.')
            });
            let major = version_token
                .and_then(|v| v.split('.').next())
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);

            let display = version_token.unwrap_or(&version_str);

            if major >= 100 {
                Check {
                    group: Group::Browser,
                    name: "Chrome version",
                    status: Status::Pass,
                    detail: display.to_string(),
                    fix: None,
                }
            } else if major > 0 {
                Check {
                    group: Group::Browser,
                    name: "Chrome version",
                    status: Status::Warn,
                    detail: format!("{display} (v100+ recommended)"),
                    fix: Some("Update Chrome to the latest version".into()),
                }
            } else {
                Check {
                    group: Group::Browser,
                    name: "Chrome version",
                    status: Status::Warn,
                    detail: format!("{version_str} (could not parse version)"),
                    fix: None,
                }
            }
        }
        _ => Check {
            group: Group::Browser,
            name: "Chrome version",
            status: Status::Fail,
            detail: "--version failed or did not answer within 5s".into(),
            fix: Some("Check CHROME_PATH and run the browser --version directly".into()),
        },
    }
}

/// Pure-Rust lookup for Xvfb (standard paths + `PATH`, zero subprocesses —
/// works on minimal containers without `which`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn check_xvfb() -> Check {
    let candidates = [
        "/usr/bin/Xvfb",
        "/usr/local/bin/Xvfb",
        "/run/current-system/sw/bin/Xvfb",
    ];
    let found = candidates
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join("Xvfb"))
                    .find(|p| p.is_file())
            })
        });

    if let Some(p) = found {
        Check {
            group: Group::Browser,
            name: "Xvfb (headful stealth)",
            status: Status::Pass,
            detail: format!("{} (available)", p.display()),
            fix: None,
        }
    } else {
        Check {
            group: Group::Browser,
            name: "Xvfb (headful stealth)",
            status: Status::Warn,
            detail: "not found (headless fallback will be used)".into(),
            fix: Some(
                "Install: sudo pacman -S xorg-server-xvfb (Arch) / sudo apt install xvfb (Debian)"
                    .into(),
            ),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn check_window_manager() -> Check {
    wm_check(crate::browser::display::window_manager())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn wm_check(found: Option<String>) -> Check {
    match found {
        Some(path) => Check {
            group: Group::Browser,
            name: "Window manager",
            status: Status::Pass,
            detail: format!("{path} (real decorations, coherent outer/inner geometry)"),
            fix: None,
        },
        None => Check {
            group: Group::Browser,
            name: "Window manager",
            status: Status::Warn,
            detail: "xfwm4 not found — Xvfb runs headful with no decorations or frame extents \
                 (outerWidth == innerWidth, availHeight == height); pages see an Xvfb signature"
                .into(),
            fix: Some(
                "Install xfwm4: sudo pacman -S xfwm4 (Arch) / sudo apt install xfwm4 (Debian)"
                    .into(),
            ),
        },
    }
}

fn check_profile_dir() -> Check {
    let dir = std::env::var("BLADE_PROFILE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| crate::platform::blade_dir().join("profile"));
    if !dir.exists() {
        return Check {
            group: Group::Vault,
            name: "Profile directory",
            status: Status::Warn,
            detail: format!("{} (will be created on first run)", dir.display()),
            fix: None,
        };
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let test_file = dir.join(format!(
        ".bladebro-write-test-{}-{nanos}",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&test_file)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            Check {
                group: Group::Vault,
                name: "Profile directory",
                status: Status::Pass,
                detail: format!("{} (writable)", dir.display()),
                fix: None,
            }
        }
        Err(e) => Check {
            group: Group::Vault,
            name: "Profile directory",
            status: Status::Fail,
            detail: format!("{} (not writable: {e})", dir.display()),
            fix: Some(format!("Fix permissions: chmod 700 {}", dir.display())),
        },
    }
}

fn check_login_sidecar() -> Check {
    let path = crate::platform::blade_dir().join("logins.json");
    if !path.exists() {
        return Check {
            group: Group::Vault,
            name: "Login persistence",
            status: Status::Pass,
            detail: "no saved logins yet (created on first login)".into(),
            fix: None,
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.permissions().mode() & 0o077 != 0 {
                return Check {
                    group: Group::Vault,
                    name: "Login persistence",
                    status: Status::Warn,
                    detail: format!("{} has non-private permissions", path.display()),
                    fix: Some(format!("Fix: chmod 600 {}", path.display())),
                };
            }
        }
    }
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => {
                let n = v.as_array().map(|a| a.len()).unwrap_or(0);
                Check {
                    group: Group::Vault,
                    name: "Login persistence",
                    status: Status::Pass,
                    detail: format!("{n} cookie(s) saved for next session"),
                    fix: None,
                }
            }
            Err(_) => Check {
                group: Group::Vault,
                name: "Login persistence",
                status: Status::Warn,
                detail: format!("{} is unreadable JSON", path.display()),
                fix: Some("It will be rebuilt on next clean snapshot".into()),
            },
        },
        Err(e) => Check {
            group: Group::Vault,
            name: "Login persistence",
            status: Status::Warn,
            detail: format!("cannot read {}: {e}", path.display()),
            fix: None,
        },
    }
}

fn check_profile_hygiene() -> Check {
    profile_hygiene_at(&crate::platform::blade_dir())
}

fn profile_hygiene_at(dir: &std::path::Path) -> Check {
    let mut orphans = Vec::new();
    let mut total_orphan_bytes: u64 = 0;
    for root in crate::session_profile::session_roots(dir) {
        let Ok(entries) = std::fs::read_dir(root.join("profiles")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if crate::session_profile::session_owner_dead(&path) {
                let mut size: u64 = 0;
                if let Ok(it) = fs_items(&path) {
                    for f in it {
                        size += f.len();
                        if size > 200_000_000 {
                            break;
                        }
                    }
                }
                total_orphan_bytes += size;
                orphans.push(format!("{name} ({:.0}MB)", size as f64 / 1_048_576.0));
            }
        }
    }
    if orphans.is_empty() {
        return Check {
            group: Group::Vault,
            name: "Profile hygiene",
            status: Status::Pass,
            detail: "no orphaned session dirs".into(),
            fix: None,
        };
    }
    let mb = total_orphan_bytes as f64 / 1_048_576.0;
    Check {
        group: Group::Vault,
        name: "Profile hygiene",
        status: Status::Warn,
        detail: format!(
            "{} orphaned profile dir{} ({mb:.0}MB); auto-reaped on next launch",
            orphans.len(),
            if orphans.len() == 1 { "" } else { "s" }
        ),
        fix: Some("Next launch cleans them automatically".into()),
    }
}

fn fs_items(dir: &std::path::Path) -> std::io::Result<Vec<std::fs::Metadata>> {
    let mut out = Vec::new();
    if let Ok(e) = std::fs::read_dir(dir) {
        for f in e.flatten() {
            if let Ok(m) = f.metadata() {
                if m.is_file() {
                    out.push(m);
                }
            }
        }
    }
    Ok(out)
}

fn check_stale_locks() -> Check {
    let custom = std::env::var("BLADE_PROFILE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    profile_locks_at(&crate::platform::blade_dir(), custom.as_deref())
}

fn profile_locks_at(blade_dir: &std::path::Path, custom: Option<&std::path::Path>) -> Check {
    if let Some(dir) = custom {
        return custom_profile_lock(dir);
    }
    let mut running = Vec::new();
    let mut stale = Vec::new();
    for root in crate::session_profile::session_roots(blade_dir) {
        let Ok(entries) = std::fs::read_dir(root.join("profiles")) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !crate::session_profile::session_owner_dead(&dir) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let lock = dir.join("SingletonLock");
            if crate::session_profile::profile_in_use(&dir) {
                running.push(name);
            } else if std::fs::symlink_metadata(&lock).is_ok() {
                match read_lock_pid(&lock) {
                    Some(p) => stale.push(format!("{name} from dead PID {p}")),
                    None => stale.push(name),
                }
            }
        }
    }
    if !running.is_empty() {
        return Check {
            group: Group::Vault,
            name: "Profile locks",
            status: Status::Warn,
            detail: format!(
                "a process (browser or launcher) still runs on dead session {}",
                running.join(", ")
            ),
            fix: Some(
                "The next launch stops a browser on it before reaping; stop any other process started with its --user-data-dir yourself".into(),
            ),
        };
    }
    if !stale.is_empty() {
        return Check {
            group: Group::Vault,
            name: "Profile locks",
            status: Status::Warn,
            detail: format!(
                "stale lock in {} (auto-cleared on next launch)",
                stale.join(", ")
            ),
            fix: None,
        };
    }
    Check {
        group: Group::Vault,
        name: "Profile locks",
        status: Status::Pass,
        detail: "no stale locks".into(),
        fix: None,
    }
}

fn custom_profile_lock(dir: &std::path::Path) -> Check {
    let lock = dir.join("SingletonLock");
    if std::fs::symlink_metadata(&lock).is_err() {
        return Check {
            group: Group::Vault,
            name: "Profile locks",
            status: Status::Pass,
            detail: "no stale locks".into(),
            fix: None,
        };
    }
    let pid = read_lock_pid(&lock);
    match pid {
        Some(p) => {
            if crate::platform::process_alive(p) {
                Check {
                    group: Group::Vault,
                    name: "Profile locks",
                    status: Status::Warn,
                    detail: format!("lock held by PID {p} (another bladebro may be running)"),
                    fix: Some("Close other bladebro instances, or use BLADE_PROFILE_DIR pointing at an empty directory only you can write".into()),
                }
            } else {
                Check {
                    group: Group::Vault,
                    name: "Profile locks",
                    status: Status::Warn,
                    detail: format!(
                        "stale lock from dead PID {p} (will be auto-cleared on next launch)"
                    ),
                    fix: None,
                }
            }
        }
        None => Check {
            group: Group::Vault,
            name: "Profile locks",
            status: Status::Pass,
            detail: "no stale locks".into(),
            fix: None,
        },
    }
}

fn check_network_from(latest: &Result<String>) -> Check {
    match latest {
        Ok(tag) => Check {
            group: Group::Network,
            name: "Release registry",
            status: Status::Pass,
            detail: format!("reachable (latest: v{tag})"),
            fix: None,
        },
        Err(e) => {
            let disabled = std::env::var("BLADE_NO_UPDATE_CHECK")
                .map(|v| v == "1")
                .unwrap_or(false);
            if disabled {
                Check {
                    group: Group::Network,
                    name: "Release registry",
                    status: Status::Warn,
                    detail: "skipped (BLADE_NO_UPDATE_CHECK=1)".into(),
                    fix: None,
                }
            } else {
                Check {
                    group: Group::Network,
                    name: "Release registry",
                    status: Status::Fail,
                    detail: format!("cannot reach GitHub or npm: {e}"),
                    fix: Some("Check your internet connection and firewall/proxy settings".into()),
                }
            }
        }
    }
}

fn check_binary() -> Check {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return Check {
                group: Group::Environment,
                name: "Binary integrity",
                status: Status::Warn,
                detail: format!("cannot check: {e}"),
                fix: None,
            }
        }
    };

    let size = std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
    let size_mb = size as f64 / 1_048_576.0;
    let channel = version::install_channel_label();

    let size_ok = size > 1_000_000 && size < 500_000_000;
    let detail = format!("v{} ({size_mb:.1} MB) [{channel}]", super::CURRENT_VERSION);

    if size_ok {
        Check {
            group: Group::Environment,
            name: "Binary integrity",
            status: Status::Pass,
            detail,
            fix: None,
        }
    } else {
        Check {
            group: Group::Environment,
            name: "Binary integrity",
            status: Status::Warn,
            detail: format!("{detail} unusual size"),
            fix: Some("Reinstall: npm install -g bladebro".into()),
        }
    }
}

fn check_disk_space() -> Check {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => {
            return Check {
                group: Group::Environment,
                name: "Disk space",
                status: Status::Warn,
                detail: "cannot check".into(),
                fix: None,
            }
        }
    };
    let dir = exe.parent().unwrap_or(std::path::Path::new("."));
    match crate::platform::available_disk_bytes(dir) {
        Some(avail) => {
            let avail_mb = avail as f64 / 1_048_576.0;
            if avail_mb < 50.0 {
                Check {
                    group: Group::Environment,
                    name: "Disk space",
                    status: Status::Warn,
                    detail: format!("{} free (updates need ~20 MB)", ui::format_bytes(avail)),
                    fix: Some("Free up disk space before updating".into()),
                }
            } else {
                Check {
                    group: Group::Environment,
                    name: "Disk space",
                    status: Status::Pass,
                    detail: format!("{} free", ui::format_bytes(avail)),
                    fix: None,
                }
            }
        }
        None => Check {
            group: Group::Environment,
            name: "Disk space",
            status: Status::Warn,
            detail: "could not measure available space".into(),
            fix: None,
        },
    }
}

fn check_version_from(latest: &Result<String>) -> Check {
    match latest {
        Ok(tag) => match version::compare_versions(tag, super::CURRENT_VERSION) {
            std::cmp::Ordering::Greater => Check {
                group: Group::Network,
                name: "Version",
                status: Status::Warn,
                detail: format!("v{} (update available: v{tag})", super::CURRENT_VERSION),
                fix: Some(format!("Run: {}", version::update_command_hint())),
            },
            std::cmp::Ordering::Equal => Check {
                group: Group::Network,
                name: "Version",
                status: Status::Pass,
                detail: format!("v{} (on the latest release)", super::CURRENT_VERSION),
                fix: None,
            },
            std::cmp::Ordering::Less => Check {
                group: Group::Network,
                name: "Version",
                status: Status::Pass,
                detail: format!(
                    "v{} (ahead of latest release v{tag})",
                    super::CURRENT_VERSION
                ),
                fix: None,
            },
        },
        Err(_) => Check {
            group: Group::Network,
            name: "Version",
            status: Status::Warn,
            detail: format!("v{} (could not check for updates)", super::CURRENT_VERSION),
            fix: None,
        },
    }
}

fn find_chrome_path() -> Option<String> {
    crate::browser::find_chrome().ok()
}

fn chrome_install_hint() -> String {
    if cfg!(target_os = "linux") {
        "Install: sudo pacman -S chromium (Arch) / sudo apt install chromium-browser (Debian) / nix-shell -p chromium (Nix) / flatpak install flathub org.chromium.Chromium (SteamOS, Bazzite, Silverblue: found automatically), or set CHROME_PATH".into()
    } else if cfg!(target_os = "macos") {
        "Install: brew install --cask google-chrome".into()
    } else if cfg!(windows) {
        "Install: https://www.google.com/chrome/".into()
    } else {
        "Install Chrome or Chromium from your package manager".into()
    }
}

fn read_lock_pid(lock: &std::path::Path) -> Option<u32> {
    #[cfg(unix)]
    {
        let target = std::fs::read_link(lock).ok()?;
        target
            .to_string_lossy()
            .rsplit('-')
            .next()
            .and_then(|p| p.parse::<u32>().ok())
    }
    #[cfg(windows)]
    {
        let content = std::fs::read_to_string(lock).ok()?;
        content.trim().parse::<u32>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("blade-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn session(root: &Path, name: &str, owner: u32) -> PathBuf {
        let dir = root.join("profiles").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".blade-owner"), owner.to_string()).unwrap();
        dir
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_missing_window_manager_is_reported_with_its_install_hint() {
        let missing = wm_check(None);
        assert!(matches!(missing.status, Status::Warn));
        assert!(
            missing.detail.contains("outerWidth == innerWidth"),
            "{}",
            missing.detail
        );
        assert!(missing
            .fix
            .unwrap_or_default()
            .contains("sudo pacman -S xfwm4"));
        let found = wm_check(Some("/usr/bin/xfwm4".into()));
        assert!(matches!(found.status, Status::Pass));
        assert!(found.detail.contains("/usr/bin/xfwm4"));
    }

    #[test]
    fn version_check_distinguishes_behind_equal_and_ahead() {
        let behind = check_version_from(&Ok("99.0.0".into()));
        assert!(behind.status == Status::Warn);
        assert!(behind.detail.contains("update available"));
        assert!(behind.fix.is_some());

        let equal = check_version_from(&Ok(super::super::CURRENT_VERSION.into()));
        assert!(equal.status == Status::Pass);
        assert!(equal.detail.contains("on the latest release"));

        let ahead = check_version_from(&Ok("0.0.1".into()));
        assert!(ahead.status == Status::Pass);
        assert!(ahead.detail.contains("ahead of latest release"));
    }

    #[test]
    fn disk_space_check_succeeds_natively() {
        let check = check_disk_space();
        assert!(
            check.status == Status::Pass,
            "native disk space check should pass: {}",
            check.detail
        );
    }

    #[test]
    fn hygiene_counts_exactly_what_the_reaper_reaps() {
        let root = scratch("hygiene");
        let me = std::process::id();
        session(&root, "sess-999999998", 999_999_998);
        session(&root, &format!("sess-{me}"), me);

        std::fs::create_dir_all(root.join("profiles/pid-999999996")).unwrap();
        std::fs::create_dir_all(root.join("profiles/junk")).unwrap();
        let real = root.join("realbrowser/chrome");
        session(&real, "sess-999999997", 999_999_997);
        let check = profile_hygiene_at(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(check.status == Status::Warn);
        assert!(
            check.detail.starts_with("2 orphaned profile dirs"),
            "got: {}",
            check.detail
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_locks_see_dangling_singleton_symlinks_in_live_dirs() {
        let custom = scratch("locks-custom");
        std::os::unix::fs::symlink("host-999999999", custom.join("SingletonLock")).unwrap();
        let custom_check = profile_locks_at(Path::new("/nonexistent-blade"), Some(&custom));

        let root = scratch("locks-sessions");
        let me = std::process::id();
        let dead = session(&root, "sess-999999998", 999_999_998);
        std::os::unix::fs::symlink("host-999999999", dead.join("SingletonLock")).unwrap();
        let sessions_check = profile_locks_at(&root, None);

        let live_root = scratch("locks-live");
        let live = session(&live_root, &format!("sess-{me}"), me);
        std::os::unix::fs::symlink(format!("host-{me}"), live.join("SingletonLock")).unwrap();
        let live_check = profile_locks_at(&live_root, None);

        for dir in [&custom, &root, &live_root] {
            let _ = std::fs::remove_dir_all(dir);
        }
        assert!(
            custom_check.status == Status::Warn
                && custom_check.detail.contains("dead PID 999999999"),
            "custom dir: {}",
            custom_check.detail
        );
        assert!(
            sessions_check.status == Status::Warn
                && sessions_check.detail.contains("sess-999999998"),
            "session dirs: {}",
            sessions_check.detail
        );
        assert!(
            live_check.status == Status::Pass,
            "a live session's own lock is normal: {}",
            live_check.detail
        );
    }
}
