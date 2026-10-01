//! Orphan reaper: dead session profiles (+ their orphaned Chrome), stale
//! Xvfb locks and display claims, and interrupted sync-swap recovery.
//! Split from the `session_profile` core.

use std::path::Path;

use crate::platform;

use super::SessionProfile;

/// Reap the corpses of dead bladebro processes:
/// 1. Session profile dirs whose owner pid is dead → kill
///    their orphaned Chrome (via SingletonLock pid), rm dir.
/// 2. Xvfb lock files (/tmp/.X<n>-lock) whose pid is dead →
///    remove lock, kill the orphaned Xvfb if still running.
///
/// Runs on every Chrome launch. Self-healing: no matter how
/// bladebro died (SIGKILL, OOM, panic), the next launch
/// cleans up after it.
pub fn reap_orphans() {
    reap_orphans_at(&platform::blade_dir());

    // Stale Xvfb locks + orphan Xvfb processes (Linux).
    #[cfg(target_os = "linux")]
    {
        reap_xvfb();
    }
}

/// Restore a template that a crashed sync-swap left displaced, and clean any
/// swap staging leftovers. Called by [`reap_orphans`] on every launch.
///
/// `swap_into_template` moves the live `profile` aside to `.profile.old`
/// BEFORE renaming the new copy in. If that swap is SIGKILLed between the two
/// renames, `profile` is missing and `.profile.old` is the ONLY surviving copy
/// of the whole profile. Deleting it (as reap_orphans used to) loses all
/// seasoning on exactly the power-loss path the sidecar protects against.
/// Returns true when a displaced template was resurrected.
pub(super) fn restore_interrupted_swap(blade_dir: &Path) -> bool {
    let old = blade_dir.join(".profile.old");
    let template = blade_dir.join("profile");
    if old.is_dir() && !template.exists() {
        let _ = std::fs::rename(&old, &template);
        eprintln!("[bladebro] restored template from .profile.old (sync swap was interrupted)");
        return true;
    }
    // Normal (template present) or leftover staging: drop it.
    let _ = std::fs::remove_dir_all(&old);
    false
}

/// The reaper core, parameterized on the data root. The root is resolved
/// ONCE by the caller: everything below must operate on the same directory —
/// a mid-reap `blade_dir()` re-resolution could be redirected by the
/// process-global env changing on another thread (tests flip BLADE_HOME in
/// parallel), which is the split-brain this signature exists to prevent.
pub(super) fn reap_orphans_at(blade_dir: &Path) {
    // `.profile.sync` is just the staging copy, safe to drop.
    let _ = std::fs::remove_dir_all(blade_dir.join(".profile.sync"));
    restore_interrupted_swap(blade_dir);

    // 1. Dead session profiles + their Chromes (agent lane).
    reap_session_root(blade_dir, None);

    // 1b. Real-lane (clone) roots: swap leftovers + dead sessions. A dead
    // MCP/daemon process must not leak its Chrome or lose the clone's state.
    let rb = blade_dir.join("realbrowser");
    if let Ok(entries) = std::fs::read_dir(&rb) {
        for entry in entries.flatten() {
            let root = entry.path();
            if !root.is_dir() {
                continue;
            }
            restore_interrupted_real_swap(&root);
            let _ = std::fs::remove_dir_all(root.join("template.sync"));
            reap_session_root(&root, Some(&root));
        }
    }
}

/// Reap dead session dirs under `root/profiles`; `real_root` selects the
/// sync-back destination (`None` = the agent-lane template at `root`,
/// `Some` = the browser-root's own `template/`).
fn reap_session_root(root: &Path, real_root: Option<&Path>) {
    let my_pid = std::process::id();
    let entries = match std::fs::read_dir(root.join("profiles")) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("sess-") {
            continue;
        }
        let dir = entry.path();
        let owner = std::fs::read_to_string(dir.join(".blade-owner"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let owner_dead = match owner {
            Some(pid) => pid != my_pid && !platform::process_alive(pid),
            // No owner file = pre-session-profile artifact or interrupted
            // creation. Treat sess-<pid> name as owner.
            None => name
                .strip_prefix("sess-")
                .and_then(|p| p.parse::<u32>().ok())
                .map(|pid| pid != my_pid && !platform::process_alive(pid))
                .unwrap_or(false),
        };
        if !owner_dead {
            continue;
        }
        kill_orphan_chrome(&dir);
        // Sync the dead session's state (cookies, localStorage) back to the
        // template BEFORE removing it. Without this, sessions killed without
        // graceful shutdown lose all their state — the reaper just deleted
        // the dir.
        match real_root {
            Some(real_root) => SessionProfile::sync_back_real(real_root, &dir),
            None => SessionProfile::sync_back_only_at(root, &dir),
        }
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("[bladebro] reaped dead session profile {name}");
    }
}

/// Kill the orphaned Chrome holding a dead session's profile. The pid is
/// verified to be Chrome AND to mention THIS profile dir — a recycled pid
/// must never be killed.
fn kill_orphan_chrome(dir: &Path) {
    if let Some(chrome_pid) = read_singleton_pid(dir) {
        if platform::process_alive(chrome_pid)
            && platform::process_is_chrome(chrome_pid)
            && process_uses_dir(chrome_pid, dir)
        {
            platform::kill_process_graceful(chrome_pid);
            for _ in 0..20 {
                if !platform::process_alive(chrome_pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if platform::process_alive(chrome_pid) {
                platform::kill_process_force(chrome_pid);
            }
        }
    }
}

/// Restore/clean a realbrowser template after a crashed import or sync
/// swap. Mirrors [`restore_interrupted_swap`] for the per-browser root.
fn restore_interrupted_real_swap(root: &Path) {
    let template = root.join("template");
    for (name, what) in [("template.old", "sync swap"), ("template.tmp", "import")] {
        let dir = root.join(name);
        if dir.is_dir() {
            if !template.exists() {
                let _ = std::fs::rename(&dir, &template);
                eprintln!(
                    "[bladebro] restored real-browser template from {name} (interrupted {what})"
                );
            } else {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

/// Read the pid from a profile dir's SingletonLock (a `hostname-pid`
/// symlink on Linux/macOS). Windows has no such file — its singleton is a
/// `lockfile` handle with no pid in it — so this returns None there and the
/// orphan kill is Unix-only (documented residual).
fn read_singleton_pid(dir: &Path) -> Option<u32> {
    let lock = dir.join("SingletonLock");
    #[cfg(unix)]
    {
        let target = std::fs::read_link(&lock).ok()?;
        target
            .to_string_lossy()
            .rsplit('-')
            .next()
            .and_then(|p| p.parse::<u32>().ok())
    }
    #[cfg(windows)]
    {
        std::fs::read_to_string(&lock)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }
}

/// Does process `pid` have `dir` in its command line?
/// Guards against killing a recycled pid.
fn process_uses_dir(pid: u32, dir: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .map(|c| c.contains(&dir.display().to_string()))
            .unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        // No /proc on macOS — ask ps for the full command line.
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&dir.display().to_string()))
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // No cheap command-line read on Windows (wmic is gone on recent
        // builds). Trust process_is_chrome — the pid comes from OUR session
        // dir's lock, so a recycled-pid kill window is narrow.
        let _ = (pid, dir);
        true
    }
}

/// Reap stale Xvfb locks and orphaned Xvfb processes.
///
/// The X lock file /tmp/.X<n>-lock contains the server
/// pid, 10 chars, space-padded. Two reap cases:
/// - pid dead: stale lock, remove + kill any orphan on
///   that display.
/// - pid alive but ORPHANED (ppid 1): its bladebro died
///   without cleanup; kill it and remove the lock.
///
/// Never touches :0/:1 (the user's real displays) or any
/// Xvfb with a live parent (an active session owns it).
#[cfg(target_os = "linux")]
fn reap_xvfb() {
    for n in 2..200 {
        let lock = format!("/tmp/.X{n}-lock");
        let content = match std::fs::read_to_string(&lock) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let pid: u32 = match content.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if platform::process_alive(pid) {
            // Alive — but orphaned? ppid 1 means the owning
            // bladebro died and init adopted it.
            if proc_ppid(pid) <= 1 && proc_is_xvfb(pid) {
                platform::kill_process_force(pid);
                let _ = std::fs::remove_file(&lock);
                eprintln!("[bladebro] reaped orphaned Xvfb :{n} (pid {pid})");
            }
            continue;
        }
        // Dead pid: the lock is stale. Remove it, then kill
        // any orphaned Xvfb still bound to this display.
        let _ = std::fs::remove_file(&lock);
        kill_xvfb_on_display(n);
        eprintln!("[bladebro] reaped stale Xvfb display :{n}");
    }
    // Stale display CLAIMS: /tmp/.blade-x<n>-claim holds the claiming
    // bladebro's pid. A SIGKILLed bladebro never releases its claim, so
    // that display number would be lost forever (until /tmp is wiped).
    // Free every claim whose owner is dead.
    for n in 99..200 {
        let claim = format!("/tmp/.blade-x{n}-claim");
        let pid: u32 = match std::fs::read_to_string(&claim) {
            Ok(c) => match c.trim().parse() {
                Ok(p) => p,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        if !platform::process_alive(pid) {
            let _ = std::fs::remove_file(&claim);
        }
    }
}

/// Parent pid of a process (0/1 = orphaned or init).
#[cfg(target_os = "linux")]
fn proc_ppid(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Format: pid (comm) state ppid ... — comm can contain
    // spaces/parens, so parse after the last ')'.
    stat.rsplit(')')
        .next()
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// Is this pid an Xvfb process?
#[cfg(target_os = "linux")]
fn proc_is_xvfb(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|c| c.contains("Xvfb"))
        .unwrap_or(false)
}

/// Kill any Xvfb process serving display `:<n>` whose parent
/// is dead (orphaned). Identified via /proc cmdline scan.
#[cfg(target_os = "linux")]
fn kill_xvfb_on_display(display: u16) {
    let want = format!(":{display}");
    let procs = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return,
    };
    for entry in procs.flatten() {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline = match std::fs::read_to_string(entry.path().join("cmdline")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !cmdline.contains("Xvfb") || !cmdline.contains(&want) {
            continue;
        }
        if proc_ppid(pid) <= 1 {
            platform::kill_process_force(pid);
        }
    }
}
