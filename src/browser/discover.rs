//! Chrome/Xvfb binary discovery — PATH, common install locations, the Nix
//! store, and free-port allocation.

use super::*;

/// Find the Xvfb binary on this system. Linux-only.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn find_xvfb() -> Option<String> {
    // Common locations (NixOS system profile, standard paths).
    let candidates: &[&str] = &[
        "/run/current-system/sw/bin/Xvfb",
        "/usr/bin/Xvfb",
        "/usr/local/bin/Xvfb",
        "/opt/Xvfb/bin/Xvfb",
    ];
    for path in candidates {
        if is_executable(std::path::Path::new(path)) {
            return Some(path.to_string());
        }
    }
    // Try PATH.
    find_in_path("Xvfb")
}

/// Find the Chrome/Chromium binary on this system.
pub(crate) fn find_chrome() -> Result<String> {
    if let Ok(path) = std::env::var("CHROME_PATH") {
        if is_executable(std::path::Path::new(&path)) {
            return Ok(path);
        }
        return Err(BladeError::Other("CHROME_PATH is not an executable file; set it to the Chrome/Chromium binary or unset it for automatic discovery".into()));
    }

    let names = if cfg!(target_os = "macos") {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ][..]
    } else if cfg!(target_os = "windows") {
        &["chrome", "chromium"][..]
    } else {
        &[
            "chromium",
            "google-chrome",
            "google-chrome-stable",
            "chromium-browser",
        ][..]
    };
    for name in names {
        if let Some(path) = find_in_path(name) {
            return Ok(path);
        }
    }

    let paths = common_paths();
    for path in &paths {
        if is_executable(std::path::Path::new(path)) {
            return Ok(path.to_string());
        }
    }

    // Windows per-user install (default when installing without admin rights).
    #[cfg(target_os = "windows")]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = format!("{local}\\Google\\Chrome\\Application\\chrome.exe");
        if is_executable(std::path::Path::new(&p)) {
            return Ok(p);
        }
    }

    if std::path::Path::new("/nix/store").exists() {
        if let Some(path) = find_in_nix_store() {
            return Ok(path);
        }
        if let Some(path) = find_via_nix_shell() {
            return Ok(path);
        }
    }

    Err(BladeError::Other(
        "Chrome/Chromium not found. Set CHROME_PATH env var, add chromium to PATH, or install Chrome.".into(),
    ))
}

fn common_paths() -> Vec<&'static str> {
    let mut paths = Vec::new();
    if cfg!(target_os = "linux") {
        paths.extend([
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
            "/opt/google/chrome/chrome",
            "/run/current-system/sw/bin/chromium",
            "/run/current-system/sw/bin/google-chrome",
        ]);
    }
    if cfg!(target_os = "macos") {
        paths.extend([
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/usr/local/bin/google-chrome",
            "/opt/homebrew/bin/chromium",
        ]);
    }
    if cfg!(target_os = "windows") {
        paths.extend([
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]);
    }
    paths
}

pub(super) fn find_in_path(cmd: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.split(sep) {
        let full = std::path::Path::new(dir).join(cmd);
        if is_executable(&full) {
            return Some(full.to_string_lossy().to_string());
        }
        if cfg!(windows) {
            let with_exe = std::path::Path::new(dir).join(format!("{cmd}.exe"));
            if is_executable(&with_exe) {
                return Some(with_exe.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_in_nix_store() -> Option<String> {
    if let Ok(output) = Command::new("fd")
        .args([
            "-t",
            "x",
            "-1",
            "chromium$",
            "/nix/store",
            "--max-depth",
            "3",
        ])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.ends_with("/bin/chromium") {
                    return Some(line.to_string());
                }
            }
        }
    }
    if let Ok(output) = Command::new("find")
        .args([
            "/nix/store",
            "-maxdepth",
            "3",
            "-name",
            "chromium",
            "-type",
            "f",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.ends_with("/bin/chromium") {
                return Some(line.to_string());
            }
        }
    }
    None
}

fn find_via_nix_shell() -> Option<String> {
    let output = Command::new("nix-shell")
        .args(["-p", "chromium", "--run", "which chromium"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !path.is_empty() && is_executable(std::path::Path::new(&path)) {
        Some(path)
    } else {
        None
    }
}

pub(super) fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .ok()
}

/// Inode of the listening socket on 127.0.0.1:`port`, read from
/// /proc/net/tcp. Linux-only; `None` when /proc is unavailable or no
/// listener exists (callers then skip verification).
#[cfg(target_os = "linux")]
pub(crate) fn loopback_listener_inode(port: u16) -> Option<u64> {
    let tcp = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let want_port = format!("{port:04X}");
    for line in tcp.lines().skip(1) {
        let mut f = line.split_whitespace();
        let Some(_sl) = f.next() else { continue };
        let Some(local) = f.next() else { continue };
        let Some(_rem) = f.next() else { continue };
        let Some(state) = f.next() else { continue };
        if state != "0A" {
            continue; // 0A = LISTEN
        }
        // 127.0.0.1 in /proc/net/tcp's byte-swapped hex.
        if !local.starts_with("0100007F:") {
            continue;
        }
        if local.rsplit(':').next() != Some(want_port.as_str()) {
            continue;
        }
        // Column 10 of the row carries the socket inode.
        return line.split_whitespace().nth(9).and_then(|i| i.parse().ok());
    }
    None
}

/// True when the listening socket for `port` is held by the process `pid`
/// or by a descendant of it. The readiness gate uses this to require the
/// debug endpoint to belong to the Chrome we just spawned — a bare JSON
/// responder on the port could be any process that won the free_port
/// bind-release window.
///
/// The holder is not always the spawned pid: launchers that fork instead of
/// exec'ing (an AppImage's AppRun script, a shell wrapper) run the real
/// browser as their child, so the listener lives one level down. Accepting
/// the whole subtree keeps the same ownership proof — only processes we
/// started can be in it — while a listener from another process tree is
/// still refused (issue #57).
#[cfg(target_os = "linux")]
pub(crate) fn endpoint_owned_by_pid(port: u16, pid: u32) -> bool {
    let Some(ino) = loopback_listener_inode(port) else {
        return false;
    };
    if pid_holds_socket(pid, ino) {
        return true;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    procs.flatten().any(|entry| {
        let Ok(candidate) = entry.file_name().to_string_lossy().parse::<u32>() else {
            return false;
        };
        candidate != pid
            && pid_within_process_tree(candidate, pid)
            && pid_holds_socket(candidate, ino)
    })
}

/// True when `pid` is `root` or a direct/indirect child of it, walking
/// /proc's parent links. Bounded so a corrupt or racing chain cannot loop;
/// an unreadable parent fails closed.
#[cfg(target_os = "linux")]
fn pid_within_process_tree(mut pid: u32, root: u32) -> bool {
    // Real launch chains are short (AppRun → wrapper → browser → helper).
    // A deeper chain is not evidence of ownership.
    for _ in 0..64 {
        if pid == root {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        pid = proc_ppid(pid);
    }
    false
}

/// Parent pid of a process (0 when /proc is unreadable or unparseable).
#[cfg(target_os = "linux")]
fn proc_ppid(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Format: pid (comm) state ppid ... — comm can contain spaces/parens,
    // so parse after the last ')'.
    stat.rsplit(')')
        .next()
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// True when ANY live process of this uid holds the loopback listener for
/// `port` AND looks like a Chromium-family browser. The attach lane uses
/// this as its liveness+identity proof: a stale DevToolsActivePort file or
/// an impostor squatting the remembered port fails it.
#[cfg(target_os = "linux")]
pub(crate) fn endpoint_owned_by_own_browser(port: u16) -> bool {
    let dbg = std::env::var("BLADE_DBG_OWNERSHIP").is_ok();
    let Some(ino) = loopback_listener_inode(port) else {
        if dbg {
            eprintln!("[ownership] port {port}: no listener found in /proc/net/tcp");
        }
        return false;
    };
    let uid = unsafe { libc::getuid() };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut holders = 0usize;
    for e in procs.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if process_uid(pid) != Some(uid) {
            continue;
        }
        if pid_holds_socket(pid, ino) {
            holders += 1;
            let chromeish = looks_like_chromium(pid);
            if dbg {
                let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
                    .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
                    .unwrap_or_default();
                eprintln!(
                    "[ownership] port {port} inode {ino}: pid {pid} holds socket; chromium-like={chromeish}; cmd={}",
                    crate::platform::truncate_utf8(&cmd, 120)
                );
            }
            if chromeish {
                return true;
            }
        }
    }
    if dbg && holders == 0 {
        eprintln!("[ownership] port {port} inode {ino}: no same-uid process holds the socket");
    }
    false
}

#[cfg(target_os = "linux")]
fn pid_holds_socket(pid: u32, inode: u64) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten().any(|e| {
        std::fs::metadata(e.path())
            .map(|m| m.file_type().is_socket() && m.ino() == inode)
            .unwrap_or(false)
    })
}

#[cfg(target_os = "linux")]
fn process_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|u| u.parse().ok())
}

#[cfg(target_os = "linux")]
fn looks_like_chromium(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    // First token of the cmdline. The kernel's normal form separates argv
    // entries with NULs, but Chromium rewrites its own argv block into one
    // space-joined string early in startup (measured live: the browser
    // process's /proc/<pid>/cmdline has no NUL bytes at all once rewritten),
    // so splitting on NUL alone would yield the WHOLE command line as one
    // token and every name check below would fail. Split on either byte.
    let exe = cmdline
        .split(|b| *b == 0 || *b == b' ')
        .find(|tok| !tok.is_empty())
        .unwrap_or_default();
    let s = String::from_utf8_lossy(exe);
    let name = std::path::Path::new(s.as_ref())
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    [
        "chrome",
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "brave",
        "brave-browser",
        "helium",
        "microsoft-edge",
        "msedge",
        "vivaldi",
        "opera",
    ]
    .iter()
    .any(|f| name == *f || name.starts_with(&format!("{f}-")))
}

/// Non-Linux: no /proc, so the ownership checks are skipped (verification
/// is Linux-only; documented residual).
#[cfg(not(target_os = "linux"))]
pub(crate) fn endpoint_owned_by_pid(_port: u16, _pid: u32) -> bool {
    true
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn endpoint_owned_by_own_browser(_port: u16) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_candidates_reject_directories_and_non_executable_files() {
        let root = std::env::temp_dir().join(format!("blade-discover-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            !is_executable(&root),
            "a searchable directory is not a browser executable"
        );
        let file = root.join("chrome");
        std::fs::write(&file, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(!is_executable(&file));
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(is_executable(&file));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Issue #57: an AppImage's AppRun forks the real browser instead of
    /// exec'ing it, so the debug port is held by a child of the PID bladebro
    /// spawned. The ownership gate must accept that subtree and still refuse
    /// a listener that belongs to a different process tree.
    #[cfg(target_os = "linux")]
    #[test]
    fn endpoint_ownership_accepts_a_listener_forked_by_the_spawned_wrapper() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let python3 = Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !python3 {
            eprintln!("python3 unavailable; skipping forked-launcher ownership test");
            return;
        }

        // The wrapper forks and waits (AppRun's exact shape): the spawned pid
        // never holds the listener itself.
        let script = "python3 -c 'import socket,time; s=socket.socket(); \
                      s.bind((\"127.0.0.1\",0)); print(s.getsockname()[1], flush=True); \
                      s.listen(1); time.sleep(30)' & wait";
        let mut wrapper = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn forking wrapper");
        let mut port_line = String::new();
        BufReader::new(wrapper.stdout.take().expect("wrapper stdout"))
            .read_line(&mut port_line)
            .expect("read listener port");
        let port: u16 = port_line.trim().parse().expect("listener port");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut accepted = false;
        while std::time::Instant::now() < deadline {
            if endpoint_owned_by_pid(port, wrapper.id()) {
                accepted = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let mut unrelated = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated process");
        let refused_outside = !endpoint_owned_by_pid(port, unrelated.id());

        // Locate the forked listener and prove shutdown reaps it.
        let inode = loopback_listener_inode(port).expect("listener inode");
        let listener_pid = std::fs::read_dir("/proc")
            .expect("scan /proc")
            .flatten()
            .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
            .find(|pid| {
                *pid != wrapper.id()
                    && pid_within_process_tree(*pid, wrapper.id())
                    && pid_holds_socket(*pid, inode)
            })
            .expect("forked listener pid");

        let _ = unrelated.kill();
        let _ = unrelated.wait();
        // A direct-pid kill would leave the forked browser running.
        crate::platform::shutdown_child(&mut wrapper);
        let mut listener_gone = false;
        for _ in 0..40 {
            if !crate::platform::process_alive(listener_pid) {
                listener_gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert!(
            accepted,
            "a listener held by a descendant of the spawned launcher is owned by it"
        );
        assert!(
            refused_outside,
            "a listener outside the spawned process tree must still be refused"
        );
        assert!(
            listener_gone,
            "shutdown must terminate the forked browser, not only the launcher"
        );
    }
}
