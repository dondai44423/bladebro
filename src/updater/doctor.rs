//! System diagnostics — the "doctor" that checks everything.

use crate::error::Result;
use super::ui;
use std::fmt;

/// A single diagnostic check result.
struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    fix: Option<String>,
}

#[derive(Clone, PartialEq)]
enum Status {
    Pass,
    Warn,
    Fail,
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

/// Run all diagnostic checks and print the report.
pub async fn run() -> Result<()> {
    ui::header("Bladebro Doctor");

    let mut checks: Vec<Check> = Vec::new();

    // 1. Operating system
    checks.push(check_os());

    // 2. Data directory (where state resolves: BLADE_HOME/XDG/legacy)
    checks.push(check_data_dir());

    // 3. Chrome/Chromium
    checks.push(check_chrome());

    // 3. Chrome version (if found)
    if checks.last().map(|c| c.status == Status::Pass).unwrap_or(false) {
        checks.push(check_chrome_version().await);
    }

    // 4. Xvfb (Linux only)
    #[cfg(target_os = "linux")]
    checks.push(check_xvfb());

    // 5. Profile directory
    checks.push(check_profile_dir());

    // 6. Login sidecar (the logins that survive reboots/SIGKILL)
    checks.push(check_login_sidecar());

    // 7. Session/profiles hygiene (orphans, disk usage)
    checks.push(check_profile_hygiene());

    // 8. Stale locks
    checks.push(check_stale_locks());

    // 9. Network connectivity
    checks.push(check_network().await);

    // 10. Binary integrity + install method
    checks.push(check_binary());

    // 11. Disk space
    checks.push(check_disk_space());

    // 12. Version vs latest
    checks.push(check_version().await);

    // Print results.
    let mut passes = 0;
    let mut warns = 0;
    let mut fails = 0;

    for check in &checks {
        let icon = match check.status {
            Status::Pass => ui::green("  ✓"),
            Status::Warn => ui::yellow("  ⚠"),
            Status::Fail => ui::red("  ✗"),
        };
        println!("{icon} {:<24} {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("    {} {}", ui::dim("fix:"), fix);
        }
        match check.status {
            Status::Pass => passes += 1,
            Status::Warn => warns += 1,
            Status::Fail => fails += 1,
        }
    }

    // Summary.
    println!();
    let total = checks.len();
    if fails == 0 && warns == 0 {
        ui::success(&format!("All {total} checks passed. Bladebro is healthy."));
    } else if fails == 0 {
        println!(
            "  {} {}/{} passed, {} warning{}",
            ui::yellow("⚠"),
            passes,
            total,
            warns,
            if warns == 1 { "" } else { "s" }
        );
    } else {
        println!(
            "  {} {}/{} passed, {} failed, {} warning{}",
            ui::red("✗"),
            passes,
            total,
            fails,
            warns,
            if warns == 1 { "" } else { "s" }
        );
        println!();
        ui::hint("Fix the failures above, then run bladebro doctor again.");
    }

    Ok(())
}

fn check_os() -> Check {
    let os = if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    };
    Check {
        name: "Operating system",
        status: Status::Pass,
        detail: format!("{os} ({arch})"),
        fix: None,
    }
}

fn check_data_dir() -> Check {
    let dir = crate::platform::blade_dir();
    let home = crate::platform::home_dir().join(".blade");
    let reason = if std::env::var("BLADE_HOME").map(|v| !v.trim().is_empty()).unwrap_or(false) {
        "BLADE_HOME"
    } else if std::env::var("XDG_STATE_HOME").map(|v| !v.trim().is_empty()).unwrap_or(false) {
        if dir == home { "existing install kept on legacy dir" } else { "XDG_STATE_HOME" }
    } else if dir == home {
        // Legacy dir selected: either nothing state-worthy exists yet or the
        // migration-free fallback kept an existing install put.
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
        name: "Data directory",
        status: Status::Pass,
        detail: format!("{} ({reason}; override with BLADE_HOME)", dir.display()),
        fix: None,
    }
}

fn check_chrome() -> Check {
    match find_chrome_path() {
        Some(path) => Check {
            name: "Chrome/Chromium",
            status: Status::Pass,
            detail: path,
            fix: None,
        },
        None => Check {
            name: "Chrome/Chromium",
            status: Status::Fail,
            detail: "not found".into(),
            fix: Some(chrome_install_hint()),
        },
    }
}

async fn check_chrome_version() -> Check {
    let path = match find_chrome_path() {
        Some(p) => p,
        None => {
            return Check {
                name: "Chrome version",
                status: Status::Fail,
                detail: "Chrome not found".into(),
                fix: None,
            }
        }
    };

    let output = std::process::Command::new(&path)
        .arg("--version")
        .output();

    match output {
        Ok(o) => {
            let version_str = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // Extract the first token that looks like a version (contains digits and dots).
            // "Chromium 150.0.7871.186 Arch Linux" → "150.0.7871.186"
            // "Google Chrome 150.0.7871.186" → "150.0.7871.186"
            let version_token = version_str
                .split_whitespace()
                .find(|t| t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
                    && t.contains('.'));
            let major = version_token
                .and_then(|v| v.split('.').next())
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);

            let display = version_token.unwrap_or(&version_str);

            if major >= 100 {
                Check {
                    name: "Chrome version",
                    status: Status::Pass,
                    detail: display.to_string(),
                    fix: None,
                }
            } else if major > 0 {
                Check {
                    name: "Chrome version",
                    status: Status::Warn,
                    detail: format!("{display} (v100+ recommended)"),
                    fix: Some("Update Chrome to the latest version".into()),
                }
            } else {
                Check {
                    name: "Chrome version",
                    status: Status::Warn,
                    detail: format!("{version_str} (could not parse version)"),
                    fix: None,
                }
            }
        }
        Err(e) => Check {
            name: "Chrome version",
            status: Status::Warn,
            detail: format!("could not check: {e}"),
            fix: None,
        },
    }
}

#[cfg(target_os = "linux")]
fn check_xvfb() -> Check {
    let candidates = [
        "/usr/bin/Xvfb",
        "/usr/local/bin/Xvfb",
        "/run/current-system/sw/bin/Xvfb",
    ];
    let found = candidates.iter().any(|p| std::path::Path::new(p).exists())
        || std::process::Command::new("which")
            .arg("Xvfb")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

    if found {
        Check {
            name: "Xvfb (headful stealth)",
            status: Status::Pass,
            detail: "available".into(),
            fix: None,
        }
    } else {
        Check {
            name: "Xvfb (headful stealth)",
            status: Status::Warn,
            detail: "not found (headless fallback will be used)".into(),
            fix: Some("Install: sudo pacman -S xorg-server-xvfb (Arch) / sudo apt install xvfb (Debian)".into()),
        }
    }
}

fn check_profile_dir() -> Check {
    // Honor BLADE_PROFILE_DIR: the effective profile root, not just the
    // template inside the data dir.
    let dir = std::env::var("BLADE_PROFILE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| crate::platform::blade_dir().join("profile"));
    if !dir.exists() {
        return Check {
            name: "Profile directory",
            status: Status::Warn,
            detail: format!("{} (will be created on first run)", dir.display()),
            fix: None,
        };
    }
    // Check writability.
    let test_file = dir.join(".bladebro-write-test");
    match std::fs::write(&test_file, b"test") {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            Check {
                name: "Profile directory",
                status: Status::Pass,
                detail: format!("{} (writable)", dir.display()),
                fix: None,
            }
        }
        Err(e) => Check {
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
                    name: "Login persistence",
                    status: Status::Pass,
                    detail: format!("{n} cookie(s) saved for next session"),
                    fix: None,
                }
            }
            Err(_) => Check {
                name: "Login persistence",
                status: Status::Warn,
                detail: format!("{} is unreadable JSON", path.display()),
                fix: Some("It will be rebuilt on next clean snapshot".into()),
            },
        },
        Err(e) => Check {
            name: "Login persistence",
            status: Status::Warn,
            detail: format!("cannot read {}: {e}", path.display()),
            fix: None,
        },
    }
}

fn check_profile_hygiene() -> Check {
    let dir = crate::platform::blade_dir();
    // Count per-process profile dirs: every `sess-*` dir whose owning pid is
    // dead. The reaper clears these on next launch; doctor reports them so
    // disk isn't silently eaten between runs.
    let mut orphans = Vec::new();
    let mut total_orphan_bytes: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir.join("profiles")) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let pid = name
                .strip_prefix("sess-")
                .or_else(|| name.strip_prefix("pid-"))
                .and_then(|p| p.split('-').next())
                .and_then(|p| p.parse::<u32>().ok());
            let dead = pid.map(|p| !crate::platform::process_alive(p)).unwrap_or(true);
            if dead {
                // size (best effort, bounded)
                let mut size: u64 = 0;
                if let Ok(it) = fs_items(&path) {
                    for f in it {
                        size += f.len();
                        if size > 200_000_000 { break; }
                    }
                }
                total_orphan_bytes += size;
                orphans.push(format!("{name} ({:.0}MB)", size as f64 / 1_048_576.0));
            }
        }
    }
    if orphans.is_empty() {
        return Check {
            name: "Profile hygiene",
            status: Status::Pass,
            detail: "no orphaned session dirs".into(),
            fix: None,
        };
    }
    let mb = total_orphan_bytes as f64 / 1_048_576.0;
    Check {
        name: "Profile hygiene",
        status: Status::Warn,
        detail: format!("{} orphaned profile dir{} ({mb:.0}MB); auto-reaped on next launch", orphans.len(), if orphans.len() == 1 { "" } else { "s" }),
        fix: Some("Next launch cleans them automatically".into()),
    }
}

/// Walk a dir shallowly collecting file sizes (no symlink following).
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
    let dir = std::env::var("BLADE_PROFILE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| crate::platform::blade_dir().join("profile"));
    let lock = dir.join("SingletonLock");
    if !lock.exists() {
        return Check {
            name: "Profile locks",
            status: Status::Pass,
            detail: "no stale locks".into(),
            fix: None,
        };
    }
    // There's a lock. Check if it's stale (PID is dead).
    let pid = read_lock_pid(&lock);
    match pid {
        Some(p) => {
            if crate::platform::process_alive(p) {
                Check {
                    name: "Profile locks",
                    status: Status::Warn,
                    detail: format!("lock held by PID {p} (another bladebro may be running)"),
                    fix: Some("Close other bladebro instances, or use BLADE_PROFILE_DIR for a separate profile".into()),
                }
            } else {
                Check {
                    name: "Profile locks",
                    status: Status::Warn,
                    detail: format!("stale lock from dead PID {p} (will be auto-cleared on next launch)"),
                    fix: None,
                }
            }
        }
        None => Check {
            name: "Profile locks",
            status: Status::Pass,
            detail: "no stale locks".into(),
            fix: None,
        },
    }
}

async fn check_network() -> Check {
    match super::version::fetch_latest().await {
        Ok(release) => Check {
            name: "GitHub connectivity",
            status: Status::Pass,
            detail: format!("reachable (latest: {})", release.tag_name),
            fix: None,
        },
        Err(e) => {
            Check {
                name: "GitHub connectivity",
                status: Status::Fail,
                detail: format!("cannot reach GitHub: {e}"),
                fix: Some("Check your internet connection and firewall settings".into()),
            }
        }
    }
}

fn check_binary() -> Check {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return Check {
                name: "Binary integrity",
                status: Status::Warn,
                detail: format!("cannot check: {e}"),
                fix: None,
            }
        }
    };

    let size = std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
    let size_mb = size as f64 / 1_048_576.0;
    let method = super::version::install_method();

    let size_ok = size > 1_000_000 && size < 500_000_000;
    let detail = format!(
        "v{} ({size_mb:.1}MB) [{}]",
        super::CURRENT_VERSION,
        match method {
            "npm" => "npm",
            "source" => "source build",
            _ => "binary",
        }
    );

    if size_ok {
        Check {
            name: "Binary integrity",
            status: Status::Pass,
            detail,
            fix: None,
        }
    } else {
        Check {
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
        Err(_) => return Check {
            name: "Disk space",
            status: Status::Warn,
            detail: "cannot check".into(),
            fix: None,
        },
    };
    // Suppress unused warning on non-Unix (exe only used in cfg(unix) block).
    let _ = &exe;

    #[cfg(unix)]
    {
        let dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let output = std::process::Command::new("df")
            .arg("-k")
            .arg(dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if let Some(line) = stdout.lines().last() {
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    let use_idx = fields.iter().position(|f| f.ends_with('%'));
                    let avail_idx = use_idx.and_then(|i| if i > 0 { Some(i - 1) } else { None });
                    if let Some(idx) = avail_idx {
                        if let Ok(avail_kb) = fields[idx].parse::<u64>() {
                            let avail_mb = avail_kb as f64 / 1024.0;
                            if avail_mb < 50.0 {
                                return Check {
                                    name: "Disk space",
                                    status: Status::Warn,
                                    detail: format!("{avail_mb:.0} MB free (updates need ~20MB)"),
                                    fix: Some("Free up disk space before updating".into()),
                                };
                            }
                            return Check {
                                name: "Disk space",
                                status: Status::Pass,
                                detail: format!("{avail_mb:.0} MB free"),
                                fix: None,
                            };
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Check {
        name: "Disk space",
        status: Status::Pass,
        detail: "sufficient".into(),
        fix: None,
    }
}

async fn check_version() -> Check {
    match super::version::fetch_latest().await {
        Ok(latest) => {
            if super::version::is_newer(latest.tag(), super::CURRENT_VERSION) {
                Check {
                    name: "Version",
                    status: Status::Warn,
                    detail: format!(
                        "v{} (update available: {})",
                        super::CURRENT_VERSION,
                        latest.tag_name
                    ),
                    fix: Some("Run: bladebro -u".into()),
                }
            } else {
                Check {
                    name: "Version",
                    status: Status::Pass,
                    detail: format!("v{} (up to date)", super::CURRENT_VERSION),
                    fix: None,
                }
            }
        }
        Err(_) => Check {
            name: "Version",
            status: Status::Warn,
            detail: format!("v{} (could not check for updates)", super::CURRENT_VERSION),
            fix: None,
        },
    }
}

/// Find Chrome path for the doctor (doesn't launch anything).
fn find_chrome_path() -> Option<String> {
    if let Ok(path) = std::env::var("CHROME_PATH") {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }

    let names: &[&str] = if cfg!(target_os = "macos") {
        &["google-chrome", "chromium"]
    } else if cfg!(target_os = "windows") {
        &["chrome", "chromium"]
    } else {
        &["chromium", "google-chrome", "google-chrome-stable", "chromium-browser"]
    };

    for name in names {
        // Try PATH first.
        if let Ok(output) = std::process::Command::new("which").arg(name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }

    // Try common absolute paths.
    let paths: Vec<&str> = if cfg!(target_os = "linux") {
        vec![
            "/usr/bin/chromium",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/snap/bin/chromium",
            "/run/current-system/sw/bin/chromium",
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else if cfg!(windows) {
        vec![
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    } else {
        vec![]
    };

    for path in paths {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }

    None
}

fn chrome_install_hint() -> String {
    if cfg!(target_os = "linux") {
        "Install: sudo pacman -S chromium (Arch) / sudo apt install chromium-browser (Debian) / nix-shell -p chromium (Nix)".into()
    } else if cfg!(target_os = "macos") {
        "Install: brew install --cask google-chrome".into()
    } else if cfg!(windows) {
        "Install: https://www.google.com/chrome/".into()
    } else {
        "Install Chrome or Chromium from your package manager".into()
    }
}

/// Read PID from SingletonLock (same logic as browser.rs).
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
