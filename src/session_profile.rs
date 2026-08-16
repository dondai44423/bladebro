//! Session-scoped Chrome profiles — the fix for cross-session
//! Chrome murder, orphaned processes, and profile-lock races.
//!
//! ## The problem
//!
//! Every bladebro process used to share `~/.blade/profile`.
//! Chrome's SingletonLock meant a second session's launch had
//! to KILL the first session's live Chrome — and if both were
//! alive, they murdered each other's browsers in a loop. On
//! SIGKILL/SIGTERM the Chrome + Xvfb processes were orphaned
//! forever (11 leaked Xvfb processes observed on one machine
//! after a day of testing).
//!
//! ## The model
//!
//! ```text
//! ~/.blade/
//!   profile/              ← seasoned TEMPLATE (never held by a running Chrome)
//!   profiles/
//!     sess-<pid>/         ← per-process live profile (Chrome's user-data-dir)
//!       .blade-owner      ← bladebro's pid (ownership for the reaper)
//! ```
//!
//! - **Launch**: reap orphans → copy template → session dir →
//!   Chrome launches with the session dir. Two sessions NEVER
//!   share a profile, so no lock conflict is possible.
//! - **Graceful exit / idle shutdown**: Chrome is SIGTERMed
//!   (flushes cookies/storage), the session dir is copied back
//!   over the template (sole survivor only), then removed.
//! - **SIGKILL**: nothing runs — the next launch's reaper finds
//!   the dead-owner session dir, kills its orphaned Chrome, and
//!   deletes it.
//!
//! Seasoning (S7: returning-visitor trust) is preserved via the
//! template copy in both directions.

use std::path::{Path, PathBuf};

use crate::error::{BladeError, Result};
use crate::platform;

/// Files/dirs Chrome locks while running — never copy these
/// between a live profile and the template.
const SKIP_ON_COPY: &[&str] = &[
    "SingletonLock",
    "SingletonSocket",
    "SingletonCookie",
    "lockfile",
    ".blade-owner",
    "Crashpad",
    "RunningChromeVersion",
    // Performance caches: large, regenerated on demand, and not needed
    // for seasoning (trust signals). Skipping these cuts the profile copy
    // from ~hundreds of MB to ~tens of MB — much faster launch.
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "blob_storage",
];

/// A per-process session profile. Cleaned up by [`cleanup`]
/// after Chrome has exited (never while Chrome holds it).
pub struct SessionProfile {
    dir: PathBuf,
    /// Whether to copy this profile back over the template
    /// on cleanup (false for BLADE_FRESH ephemeral sessions).
    seasoned: bool,
}

impl SessionProfile {
    /// Create the session profile for this process: reap
    /// orphans from dead bladebro processes, then copy the
    /// seasoned template into a fresh per-process dir.
    pub fn create() -> Result<Self> {
        reap_orphans();

        let seasoned = !std::env::var("BLADE_FRESH").map(|v| v == "1").unwrap_or(false);

        let dir = if let Ok(custom) = std::env::var("BLADE_PROFILE_DIR") {
            // Explicit override: use it as-is (the caller owns
            // the consequences — this is the escape hatch).
            if !custom.is_empty() {
                let d = PathBuf::from(custom);
                std::fs::create_dir_all(&d)
                    .map_err(|e| BladeError::Other(format!("cannot create profile dir: {e}")))?;
                return Ok(Self { dir: d, seasoned: false });
            }
            session_dir()
        } else if seasoned {
            session_dir()
        } else {
            // BLADE_FRESH=1: ephemeral temp dir. 0700 — it holds a full
            // Chrome profile (cookies, storage); a predictable
            // world-readable /tmp/bladebro-chrome-<pid> leaked it to
            // every local user.
            std::env::temp_dir().join(format!("bladebro-chrome-{}", std::process::id()))
        };

        // SECURITY: 0700 — the profile contains cookies and localStorage.
        // Plain create_dir_all gave umask perms (755 on default setups).
        crate::platform::secure_create_dir_all(&dir)
            .map_err(|e| BladeError::Other(format!("cannot create profile dir: {e}")))?;

        if seasoned {
            // Copy the seasoned template in (returning-visitor
            // trust). Best-effort: locked/missing files skip.
            let template = platform::blade_dir().join("profile");
            if template.is_dir() {
                copy_profile(&template, &dir);
            }
            // Mark ownership for the orphan reaper.
            let _ = std::fs::write(
                dir.join(".blade-owner"),
                std::process::id().to_string(),
            );
        }

        Ok(Self { dir, seasoned })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Called AFTER Chrome has fully exited. Copies the session
    /// profile back over the template (sole survivor only, so
    /// concurrent sessions don't clobber each other), then
    /// removes the session dir.
    pub fn cleanup(&self) {
        if !self.seasoned {
            // Ephemeral or custom dir: just remove if it's ours.
            if self.dir.starts_with(std::env::temp_dir()) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
            return;
        }
        Self::sync_back_and_remove(&self.dir);
    }

    /// Non-destructive periodic sync-back: copies the session profile to
    /// the template WITHOUT removing the session dir. Called every 60s
    /// during the MCP serve loop for SIGKILL resilience — if the process
    /// is force-killed, the template has a recent snapshot instead of the
    /// state from the last graceful shutdown.
    ///
    /// Uses the same sole-survivor + lockfile logic as the final cleanup.
    pub fn sync_back_only(dir: &Path) {
        if !other_live_sessions_exist() {
            let template = platform::blade_dir().join("profile");
            let lock = platform::blade_dir().join(".template.lock");
            if std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock)
                .is_ok()
            {
                let tmp = platform::blade_dir().join(".profile.sync");
                let _ = std::fs::remove_dir_all(&tmp);
                copy_profile(dir, &tmp);
                if tmp.is_dir() {
                    let _ = std::fs::remove_dir_all(&template);
                    let _ = std::fs::rename(&tmp, &template);
                }
                let _ = std::fs::remove_file(&lock);
            }
        }
    }

    /// Claim first-run warming via an O_EXCL marker file. Returns true if
    /// this process should warm the profile (first run ever, or previous
    /// warming failed and the marker was released). Also re-warms if the
    /// template profile is empty/missing (e.g. manually deleted).
    /// BLADE_NO_WARMING=1 disables warming entirely (privacy: no default
    /// visits to google.com/github.com/wikipedia.org).
    pub fn claim_warming() -> bool {
        if std::env::var("BLADE_NO_WARMING").map(|v| v == "1").unwrap_or(false) {
            return false;
        }
        let marker = platform::blade_dir().join(".warmed");
        // If the template profile is empty or missing, warming is needed
        // regardless of the marker (the profile was deleted/reset).
        let template = platform::blade_dir().join("profile");
        let template_empty = !template.is_dir()
            || template.read_dir().map(|mut d| d.next().is_none()).unwrap_or(true);
        if template_empty {
            let _ = std::fs::remove_file(&marker);
        }
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker)
            .is_ok()
    }

    /// Release the warming marker on failure so the next launch retries.
    pub fn release_warming() {
        let marker = platform::blade_dir().join(".warmed");
        let _ = std::fs::remove_file(&marker);
    }

    /// Static cleanup for after the Browser (and its profile)
    /// has been consumed by Drop. Detects seasoning from the
    /// path: only `~/.blade/profiles/sess-*` dirs sync back.
    pub fn cleanup_dir(dir: &Path) {
        let profiles = platform::blade_dir().join("profiles");
        let is_session = dir.starts_with(&profiles)
            && dir.file_name()
                .map(|n| n.to_string_lossy().starts_with("sess-"))
                .unwrap_or(false);
        if is_session {
            Self::sync_back_and_remove(dir);
        } else if dir.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn sync_back_and_remove(dir: &Path) {
        // Sole-survivor copy-back: if another live session
        // exists, skip — its state wins when IT exits.
        if !other_live_sessions_exist() {
            let template = platform::blade_dir().join("profile");
            // Atomic-ish: write to a sibling, then swap. A
            // lockfile guards against two processes copying
            // back simultaneously.
            let lock = platform::blade_dir().join(".template.lock");
            if std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock)
                .is_ok()
            {
                let tmp = platform::blade_dir().join(".profile.sync");
                let _ = std::fs::remove_dir_all(&tmp);
                copy_profile(dir, &tmp);
                if tmp.is_dir() {
                    let _ = std::fs::remove_dir_all(&template);
                    let _ = std::fs::rename(&tmp, &template);
                }
                let _ = std::fs::remove_file(&lock);
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// This process's session profile dir.
fn session_dir() -> PathBuf {
    platform::blade_dir()
        .join("profiles")
        .join(format!("sess-{}", std::process::id()))
}

/// Are there OTHER session dirs whose owner bladebro is alive?
fn other_live_sessions_exist() -> bool {
    let profiles = platform::blade_dir().join("profiles");
    let my_pid = std::process::id();
    let entries = match std::fs::read_dir(&profiles) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("sess-") {
            continue;
        }
        let owner = std::fs::read_to_string(entry.path().join(".blade-owner"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if let Some(pid) = owner {
            if pid != my_pid && platform::process_alive(pid) {
                return true;
            }
        }
    }
    false
}

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
    // 1. Dead session profiles + their Chromes.
    let profiles = platform::blade_dir().join("profiles");
    let my_pid = std::process::id();
    if let Ok(entries) = std::fs::read_dir(&profiles) {
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
                // No owner file = pre-session-profile artifact or
                // interrupted creation. Treat sess-<pid> name as owner.
                None => name
                    .strip_prefix("sess-")
                    .and_then(|p| p.parse::<u32>().ok())
                    .map(|pid| pid != my_pid && !platform::process_alive(pid))
                    .unwrap_or(false),
            };
            if !owner_dead {
                continue;
            }
            // Kill the orphaned Chrome holding this profile.
            // Verify the pid is Chrome AND its cmdline mentions
            // THIS profile dir — a recycled pid must never be
            // killed.
            if let Some(chrome_pid) = read_singleton_pid(&dir) {
                if platform::process_alive(chrome_pid)
                    && platform::process_is_chrome(chrome_pid)
                    && process_uses_dir(chrome_pid, &dir)
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
            // Sync the dead session's state (cookies, localStorage)
            // back to the template BEFORE removing it. Without this,
            // sessions that were killed without graceful shutdown
            // lose all their state — the reaper just deleted the dir.
            SessionProfile::sync_back_only(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("[bladebro] reaped dead session profile {}", name);
        }
    }

    // 2. Stale Xvfb locks + orphan Xvfb processes (Linux).
    #[cfg(target_os = "linux")]
    {
        reap_xvfb();
    }
}

/// Read the pid from a profile dir's SingletonLock (symlink
/// "hostname-pid" on Unix, pid text file on Windows).
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
    #[cfg(unix)]
    {
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .map(|c| c.contains(&dir.display().to_string()))
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // wmic is deprecated but universally present; fall back
        // to trusting process_is_chrome for the profile kill.
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
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap_or_default();
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

/// Copy a Chrome profile tree, skipping locked/runtime files.
/// Shallow-but-recursive: Chrome's profile nests (Default/,
/// Local Storage/, etc.) — a full recursive copy with per-file
/// error tolerance. Dirs are created 0700 (cookie-bearing data).
fn copy_profile(src: &Path, dst: &Path) {
    let _ = crate::platform::secure_create_dir_all(dst);
    copy_dir_filtered(src, dst, 0);
}

fn copy_dir_filtered(src: &Path, dst: &Path, depth: usize) {
    // Bound recursion — Chrome profiles nest cache dirs deeply.
    // 6 levels covers the deepest seasoning-relevant structure
    // (Default/WebStorage/<id>/CacheStorage/<origin>/files).
    if depth > 6 {
        return;
    }
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_ON_COPY.contains(&name.as_str()) {
            continue;
        }
        let s = entry.path();
        let d = dst.join(&name);
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let _ = crate::platform::secure_create_dir_all(&d);
            copy_dir_filtered(&s, &d, depth + 1);
        } else if ft.is_file() {
            // Skip sockets/fifos implicitly (not files). Copy
            // errors (locked SQLite, etc.) are tolerated.
            let _ = std::fs::copy(&s, &d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_skips_singleton_files() {
        let src = std::env::temp_dir().join("bladebro-test-copy-src");
        let dst = std::env::temp_dir().join("bladebro-test-copy-dst");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(src.join("Default")).unwrap();
        std::fs::write(src.join("Default/Cookies"), b"cookie-data").unwrap();
        std::fs::write(src.join("SingletonLock"), b"host-1234").unwrap();
        std::fs::write(src.join("Preferences"), b"{}").unwrap();

        copy_profile(&src, &dst);

        assert!(dst.join("Default/Cookies").exists());
        assert!(dst.join("Preferences").exists());
        assert!(!dst.join("SingletonLock").exists());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn copy_tolerates_missing_src() {
        let src = std::env::temp_dir().join("bladebro-test-nonexistent");
        let dst = std::env::temp_dir().join("bladebro-test-copy-dst2");
        let _ = std::fs::remove_dir_all(&dst);
        copy_profile(&src, &dst); // must not panic
    }

    /// Verify the reaper syncs a dead session's state back to the
    /// template BEFORE deleting it. This is the fix for issue #5:
    /// cookies were lost when sessions were killed without graceful
    /// shutdown, because the reaper just deleted the session dir.
    #[test]
    fn reaper_syncs_back_before_delete() {
        let blade_dir = platform::blade_dir();
        let profiles_dir = blade_dir.join("profiles");
        let template_dir = blade_dir.join("profile");

        // Clean ALL stale session dirs + locks so they don't interfere.
        // (Live testing leaves orphaned session dirs with dead owners.)
        if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let _ = std::fs::remove_file(blade_dir.join(".template.lock"));
        let _ = std::fs::remove_dir_all(blade_dir.join(".profile.sync"));

        // Snapshot the entire template so we can restore it after.
        let template_backup = std::env::temp_dir().join("bladebro-test-template-backup");
        let _ = std::fs::remove_dir_all(&template_backup);
        if template_dir.is_dir() {
            copy_profile(&template_dir, &template_backup);
        }

        // Create a fake dead session with a marker file.
        let test_pid = 999_999_999u32; // guaranteed dead pid
        let sess_dir = profiles_dir.join(format!("sess-{test_pid}"));
        std::fs::create_dir_all(sess_dir.join("Default")).unwrap();
        std::fs::write(sess_dir.join(".blade-owner"), test_pid.to_string()).unwrap();
        std::fs::write(
            sess_dir.join("Default/Cookies"),
            b"fake-cookie-db-data",
        ).unwrap();

        // Run the reaper.
        reap_orphans();

        // Session dir should be gone.
        assert!(!sess_dir.exists(), "reaper should have removed session dir");

        // Template should have the marker file from the session,
        // proving sync_back ran before deletion.
        let template_cookie = template_dir.join("Default/Cookies");
        assert!(
            template_cookie.exists(),
            "template should have Cookies file after reaper synced"
        );
        let content = std::fs::read(&template_cookie).unwrap_or_default();
        assert_eq!(
            content, b"fake-cookie-db-data",
            "template cookie DB should contain session data"
        );

        // Restore the original template.
        let _ = std::fs::remove_dir_all(&template_dir);
        if template_backup.is_dir() {
            let tmp = blade_dir.join(".profile.restore");
            let _ = std::fs::remove_dir_all(&tmp);
            copy_profile(&template_backup, &tmp);
            let _ = std::fs::rename(&tmp, &template_dir);
        }
        let _ = std::fs::remove_dir_all(&template_backup);
    }
}
