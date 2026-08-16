//! Cross-platform process management and paths.
//!
//! Every OS-specific operation lives here. The rest of the codebase
//! calls these helpers — no `#[cfg]` outside this module (except for
//! transport-level differences that can't be abstracted).

use std::path::PathBuf;
use std::process::Child;
#[cfg(unix)]
use std::time::Duration;

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\Users\\Default"))
    }
}

/// The Bladebro data directory (`~/.blade` on Unix, `%USERPROFILE%\.blade` on Windows).
pub fn blade_dir() -> PathBuf {
    home_dir().join(".blade")
}

/// Create a directory and set restrictive permissions (0700 on Unix).
/// SECURITY: Session files, fingerprints, and backups contain sensitive
/// data (cookies, auth tokens). Without explicit permissions, they get
/// the process umask (often 755/644), making them world-readable.
/// Every component this call CREATES is chmodded 0700 — ancestors that
/// already exist (e.g. $HOME) are never touched.
pub fn secure_create_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    // Deepest existing ancestor — creation (and chmodding) starts below it.
    let mut first_missing = path.to_path_buf();
    while !first_missing.exists() {
        match first_missing.parent() {
            Some(p) if p != first_missing => first_missing = p.to_path_buf(),
            _ => break,
        }
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(tail) = path.strip_prefix(first_missing.as_path()) {
            let mut cur = first_missing;
            for comp in tail.components() {
                cur.push(comp.as_os_str());
                let _ = std::fs::set_permissions(&cur, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write a file with restrictive permissions (0600 on Unix).
/// SECURITY: Session files contain cookies and localStorage — world-readable
/// by default (644). This ensures only the owner can read them.
pub fn secure_write_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Validate a file write path to prevent writing to system directories.
/// SECURITY: Blocks path traversal attacks that could overwrite critical
/// system files (e.g., /etc/cron.d, /usr/bin, /boot) via prompt injection,
/// plus credential stores and shell-startup files in the user's home
/// (~/.ssh, ~/.gnupg, ~/.bashrc, ...) — the classic persistence/exfiltration
/// sinks a prompt-injected page would target.
/// Returns Ok(()) if safe, Err(message) if blocked.
pub fn validate_write_path(path: &std::path::Path) -> Result<(), String> {
    let canonical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    // Normalize the path (resolve . and .. without requiring the file to exist).
    let mut normalized = std::path::PathBuf::new();
    for component in canonical.components() {
        match component {
            std::path::Component::ParentDir => { normalized.pop(); }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let path_str = normalized.to_string_lossy().replace('\\', "/");
    let lower = path_str.to_lowercase();

    #[cfg(unix)]
    {
        let blocked_prefixes: &[&str] = &[
            "/etc", "/usr", "/bin", "/sbin", "/boot", "/dev",
            "/proc", "/sys", "/var/log", "/root", "/lib", "/lib64",
            "/run", "/snap",
        ];
        for prefix in blocked_prefixes {
            if path_str.starts_with(prefix) || path_str.starts_with(&format!("{prefix}/")) {
                return Err(format!(
                    "blocked: writing to system directory ({prefix}) is not allowed"
                ));
            }
        }
    }

    #[cfg(windows)]
    {
        let blocked_win: &[&str] = &[
            "c:/windows", "c:/program files", "c:/program files (x86)",
            "c:/programdata/microsoft/windows/start menu",
        ];
        for prefix in blocked_win {
            if lower.starts_with(prefix) {
                return Err(format!(
                    "blocked: writing to system directory ({prefix}) is not allowed"
                ));
            }
        }
        // Autostart persistence: the per-user Startup folder.
        if lower.contains("/microsoft/windows/start menu/programs/startup") {
            return Err(
                "blocked: writing to the Windows Startup folder is not allowed".into(),
            );
        }
    }

    // Credential/config sinks that exist anywhere in the path (any home).
    // A prompt-injected page convincing the agent to write here gains
    // persistence (rc files) or steals credentials (.ssh/.aws/.gnupg).
    const BLOCKED_COMPONENTS: &[&str] = &[
        ".ssh", ".gnupg", ".aws", ".kube", ".docker", ".config",
        ".gnome",
    ];
    for comp in normalized.components().filter_map(|c| c.as_os_str().to_str()) {
        let cl = comp.to_lowercase();
        if BLOCKED_COMPONENTS.contains(&cl.as_str())
            || cl == "authorized_keys"
            || cl == "known_hosts"
            || cl == "id_rsa"
            || cl.starts_with("id_rsa.")
            || cl.starts_with("id_ed25519")
            || cl.starts_with("id_ecdsa")
        {
            return Err(format!(
                "blocked: writing to credential/config location ({comp}) is not allowed"
            ));
        }
    }
    // systemd user-unit persistence spans multiple components.
    if lower.contains("/.local/share/systemd/") {
        return Err(
            "blocked: writing to systemd user units is not allowed".into(),
        );
    }

    // Shell startup files directly in the home directory (~/.bashrc etc.).
    let file_name = normalized
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    const RC_FILES: &[&str] = &[
        ".bashrc", ".bash_profile", ".bash_logout", ".bash_aliases",
        ".profile", ".zshrc", ".zprofile", ".zshenv", ".zlogin",
        ".kshrc", ".cshrc", ".gitconfig", ".tmux.conf", ".xinitrc",
        ".xsession", ".xprofile", ".crontab", ".vimrc", ".exrc",
        ".curlrc", ".wgetrc", ".netrc", ".env",
    ];
    let in_home = std::env::var("HOME")
        .map(|h| {
            let h = h.replace('\\', "/");
            path_str == h || path_str.starts_with(&format!("{h}/"))
        })
        .unwrap_or(false)
        || std::env::var("USERPROFILE")
            .map(|h| {
                let h = h.replace('\\', "/");
                path_str == h || path_str.starts_with(&format!("{h}/"))
            })
            .unwrap_or(false);
    if in_home && RC_FILES.contains(&file_name.as_str()) {
        return Err(format!(
            "blocked: writing to shell/config startup file ({file_name}) is not allowed"
        ));
    }

    Ok(())
}

/// Char-safe string truncation. Byte slicing (`&s[..n]`) panics when `n`
/// lands inside a multi-byte UTF-8 char — and these strings are often
/// page-controlled (URLs, exception messages), so a malicious page could
/// crash the daemon. Returns the longest prefix of at most `n` bytes that
/// ends on a char boundary.
pub fn truncate_utf8(s: &str, n: usize) -> &str {
    if n >= s.len() {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Is this process alive?
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        // tasklist /FI "PID eq 1234" /NH — if the process exists, output
        // contains the PID; if not, output says "No tasks".
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                !out.contains("No tasks") && out.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

/// Is this process a Chrome/Chromium process?
pub fn process_is_chrome(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .contains("chrom")
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("chrom"))
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("chrome"))
            .unwrap_or(false)
    }
}

/// Send SIGTERM to a process by PID (Unix) or graceful-terminate (Windows).
pub fn kill_process_graceful(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        // Windows has no SIGTERM. /T kills the process tree, without /F
        // it's a graceful request (WM_CLOSE to console apps).
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .output();
    }
}

/// Send SIGKILL to a process by PID (Unix) or force-terminate (Windows).
pub fn kill_process_force(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Shut down a child process gracefully: SIGTERM first (Unix), then
/// SIGKILL after a grace period. On Windows, TerminateProcess directly.
pub fn shutdown_child(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        for _ in 0..30 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(windows)]
    {
        // Windows: no graceful signal. TerminateProcess is the only option.
        let _ = child.kill();
        let _ = child.wait();
    }
}
