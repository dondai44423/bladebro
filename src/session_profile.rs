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
use std::io::Write;

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
    /// Whether to copy this profile back over the agent-lane template
    /// on cleanup (false for BLADE_FRESH ephemeral sessions).
    seasoned: bool,
    /// Real-lane sessions only: the realbrowser root
    /// (`<data-dir>/realbrowser/<id>`) whose `template/` this session
    /// syncs back to. `None` on the agent lane.
    real_root: Option<PathBuf>,
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
                return Ok(Self { dir: d, seasoned: false, real_root: None });
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

        Ok(Self { dir, seasoned, real_root: None })
    }

    /// Create a real-lane session profile (clone mechanism): copy the
    /// imported template at `root/template` into a per-process session dir
    /// under `root/profiles/` — the same discipline the agent lane uses for
    /// its own template, so concurrent bladebro processes (daemon + MCP)
    /// never contend Chrome's SingletonLock on the clone, and the clone
    /// still ages through the sole-survivor sync-back.
    pub fn create_real(root: &Path) -> Result<Self> {
        reap_orphans();
        let dir = root.join("profiles").join(format!("sess-{}", std::process::id()));
        if dir.exists() {
            // PID reuse after a crash: never copy on top of a stale dir.
            let _ = std::fs::remove_dir_all(&dir);
        }
        crate::platform::secure_create_dir_all(&dir)
            .map_err(|e| BladeError::Other(format!("cannot create profile dir: {e}")))?;
        let template = root.join("template");
        if template.is_dir() {
            copy_profile(&template, &dir);
        }
        let _ = std::fs::write(dir.join(".blade-owner"), std::process::id().to_string());
        Ok(Self {
            dir,
            seasoned: false,
            real_root: Some(root.to_path_buf()),
        })
    }

    /// Adopt an existing directory as the profile (real-browser profile
    /// mode): used as-is, never synced, never removed — `cleanup` only
    /// removes temp dirs for non-seasoned profiles, so the user's real
    /// profile is untouched on teardown.
    pub fn adopt(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            return Err(BladeError::Other(format!(
                "profile dir not found: {}",
                dir.display()
            )));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            seasoned: false,
            real_root: None,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Called AFTER Chrome has fully exited. Copies the session
    /// profile back over the template (sole survivor only, so
    /// concurrent sessions don't clobber each other), then
    /// removes the session dir.
    pub fn cleanup(&self) {
        if let Some(root) = &self.real_root {
            // Real-lane (clone) session: sync back into this browser's own
            // template under the realbrowser root.
            Self::sync_back_impl(root, "template", "template.sync", ".template.old", &self.dir);
            return;
        }
        if !self.seasoned {
            // Ephemeral or custom dir: just remove if it's ours.
            if self.dir.starts_with(std::env::temp_dir()) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
            return;
        }
        Self::sync_back_and_remove(&self.dir);
    }

    /// Acquire the template-copy lock, unless it is held by a live process.
    /// The lock carries the owner pid + timestamp so a crashed holder (which
    /// would otherwise block every future sync-back and silently lose all
    /// logins) can be detected and broken. Returns true when we hold it.
    /// Acquire the template-copy lock under `root`, unless it is held by a
    /// live process. The lock carries the owner pid + timestamp so a crashed
    /// holder (which would otherwise block every future sync-back and
    /// silently lose all logins) can be detected and broken.
    fn acquire_lock_at(root: &Path) -> bool {
        let lock = root.join(".template.lock");
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&lock) {
            Ok(mut f) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let _ = writeln!(f, "{} {}", std::process::id(), now);
                true
            }
            Err(_) if template_lock_stale(&lock) => {
                let _ = std::fs::remove_file(&lock);
                Self::acquire_lock_at(root)
            }
            Err(_) => false,
        }
    }

    /// Promote a fully-written temp profile into the template atomically:
    /// move the old template aside to `old`, then rename the new one in; if
    /// the promote fails, put the old one back. This never leaves the
    /// template missing.
    fn swap_into_template(tmp: &Path, template: &Path, old: &Path) {
        let _ = std::fs::remove_dir_all(old);
        let _ = std::fs::rename(template, old);
        if std::fs::rename(tmp, template).is_ok() {
            let _ = std::fs::remove_dir_all(old);
        } else {
            let _ = std::fs::rename(old, template);
        }
    }

    /// Copy a session profile into the template without removing the session
    /// dir. Used by the orphan reaper to rescue a dead session's state on the
    /// next launch (graceful-kill the orphan, flush, then copy). The template
    /// is only replaced after Chrome is dead, so the copy is never taken from
    /// a live, un-flushed profile.
    pub fn sync_back_only(dir: &Path) {
        let root = platform::blade_dir();
        if other_live_sessions_at(&root.join("profiles")) {
            return;
        }
        if !Self::acquire_lock_at(&root) {
            return;
        }
        let tmp = root.join(".profile.sync");
        let _ = std::fs::remove_dir_all(&tmp);
        copy_profile(dir, &tmp);
        if tmp.is_dir() {
            Self::swap_into_template(&tmp, &root.join("profile"), &root.join(".profile.old"));
        }
        let _ = std::fs::remove_file(root.join(".template.lock"));
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
        if let Some(root) = real_root_of_session(dir) {
            // Real-lane (clone) session: sync back to the browser's own
            // template under the realbrowser root.
            Self::sync_back_impl(&root, "template", "template.sync", ".template.old", dir);
            return;
        }
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
        Self::sync_back_impl(&platform::blade_dir(), "profile", ".profile.sync", ".profile.old", dir);
    }

    /// Real-lane static teardown: sync a clone session back to its
    /// realbrowser root's template and remove it.
    pub fn sync_back_real(root: &Path, dir: &Path) {
        Self::sync_back_impl(root, "template", "template.sync", ".template.old", dir);
    }

    /// Sole-survivor copy-back: if another live session exists, skip — its
    /// state wins when IT exits. Shared by the agent lane (`profile`) and
    /// the real lane (`template` under the realbrowser root).
    fn sync_back_impl(root: &Path, template_name: &str, tmp_name: &str, old_name: &str, dir: &Path) {
        if !other_live_sessions_at(&root.join("profiles")) && Self::acquire_lock_at(root) {
            let tmp = root.join(tmp_name);
            let _ = std::fs::remove_dir_all(&tmp);
            copy_profile(dir, &tmp);
            if tmp.is_dir() {
                Self::swap_into_template(&tmp, &root.join(template_name), &root.join(old_name));
            }
            let _ = std::fs::remove_file(root.join(".template.lock"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Is the template-copy lock stale (owner dead or older than a grace
/// period)? A crashed holder must never block sync-backs forever.
fn template_lock_stale(lock: &Path) -> bool {
    let text = std::fs::read_to_string(lock).unwrap_or_default();
    let mut it = text.split_whitespace();
    match (it.next(), it.next()) {
        (Some(pid_s), Some(ts_s)) => {
            if let Ok(pid) = pid_s.parse::<u32>() {
                return !platform::process_alive(pid);
            }
            // Unparseable pid: fall through to age check.
            if let Ok(ts) = ts_s.parse::<u64>() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                return now.saturating_sub(ts) > 120;
            }
            true
        }
        _ => {
            // Legacy/empty lock: treat as stale after a grace period so a
            // half-written pre-update lock cannot wedge sync forever.
            std::fs::metadata(lock)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age.as_secs() > 120)
                .unwrap_or(true)
        }
    }
}

/// This process's session profile dir.
fn session_dir() -> PathBuf {
    platform::blade_dir()
        .join("profiles")
        .join(format!("sess-{}", std::process::id()))
}

/// Root of a real-lane session dir (`<root>/profiles/sess-<pid>` →
/// `<root>`), or None when `dir` is not a real-lane session.
pub fn real_root_of_session(dir: &Path) -> Option<PathBuf> {
    let profiles = dir.parent()?;
    if profiles.file_name().map(|n| n != "profiles").unwrap_or(true) {
        return None;
    }
    let root = profiles.parent()?;
    let in_realbrowser = root
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n == "realbrowser")
        .unwrap_or(false);
    let is_session = dir
        .file_name()
        .map(|n| n.to_string_lossy().starts_with("sess-"))
        .unwrap_or(false);
    if in_realbrowser && is_session && root.starts_with(platform::blade_dir()) {
        Some(root.to_path_buf())
    } else {
        None
    }
}

/// Are there OTHER session dirs under `profiles` whose owner bladebro is
/// alive?
fn other_live_sessions_at(profiles: &Path) -> bool {
    let my_pid = std::process::id();
    let entries = match std::fs::read_dir(profiles) {
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
/// Restore a template that a crashed sync-swap left displaced, and clean any
/// swap staging leftovers. Called by [`reap_orphans`] on every launch.
///
/// `swap_into_template` moves the live `profile` aside to `.profile.old`
/// BEFORE renaming the new copy in. If that swap is SIGKILLed between the two
/// renames, `profile` is missing and `.profile.old` is the ONLY surviving copy
/// of the whole profile. Deleting it (as reap_orphans used to) loses all
/// seasoning on exactly the power-loss path the sidecar protects against.
/// Returns true when a displaced template was resurrected.
fn restore_interrupted_swap(blade_dir: &Path) -> bool {
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

pub fn reap_orphans() {
    let blade_dir = platform::blade_dir();
    // `.profile.sync` is just the staging copy, safe to drop.
    let _ = std::fs::remove_dir_all(blade_dir.join(".profile.sync"));
    restore_interrupted_swap(&blade_dir);

    // 1. Dead session profiles + their Chromes (agent lane).
    reap_session_root(&blade_dir.join("profiles"), None);

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
            reap_session_root(&root.join("profiles"), Some(&root));
        }
    }

    // 2. Stale Xvfb locks + orphan Xvfb processes (Linux).
    #[cfg(target_os = "linux")]
    {
        reap_xvfb();
    }
}

/// Reap dead session dirs under `profiles`; `real_root` selects the
/// sync-back destination (`None` = the agent-lane template).
fn reap_session_root(profiles: &Path, real_root: Option<&Path>) {
    let my_pid = std::process::id();
    let entries = match std::fs::read_dir(profiles) {
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
            Some(root) => SessionProfile::sync_back_real(root, &dir),
            None => SessionProfile::sync_back_only(&dir),
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
    copy_profile_ex(src, dst, &[]);
}

/// Copy with an extra skip list — the real-lane import excludes session
/// restore files so the clone opens a fresh window, not the user's tab set.
pub(crate) fn copy_profile_ex(src: &Path, dst: &Path, extra_skip: &[&str]) {
    let _ = crate::platform::secure_create_dir_all(dst);
    copy_dir_filtered(src, dst, 0, extra_skip);
}

fn copy_dir_filtered(src: &Path, dst: &Path, depth: usize, extra_skip: &[&str]) {
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
        if SKIP_ON_COPY.contains(&name.as_str()) || extra_skip.contains(&name.as_str()) {
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
            copy_dir_filtered(&s, &d, depth + 1, extra_skip);
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
    fn stale_lock_with_dead_owner_is_broken_and_acquired() {
        let _ = std::fs::create_dir_all(platform::blade_dir());
        let lock = platform::blade_dir().join(".template.lock.test-dead");
        let _ = std::fs::remove_file(&lock);
        // A dead pid must never wedge sync: acquiring should break the lock.
        std::fs::write(&lock, "999999999 0\n").unwrap();
        // Redirect acquire to the test path by temporarily using a helper
        // that reads this exact file name.
        let stale = template_lock_stale(&lock);
        assert!(stale, "lock owned by a dead pid must be stale");
        let _ = std::fs::remove_file(&lock);
    }

    #[test]
    fn stale_lock_with_live_owner_is_not_stale() {
        let _ = std::fs::create_dir_all(platform::blade_dir());
        let lock = platform::blade_dir().join(".template.lock.test-live");
        let _ = std::fs::remove_file(&lock);
        let me = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        std::fs::write(&lock, format!("{me} {now}\n")).unwrap();
        assert!(!template_lock_stale(&lock), "live owner lock must not be stale");
        let _ = std::fs::remove_file(&lock);
    }

    #[test]
    fn reaper_syncs_back_before_delete() {
        let blade_dir = platform::blade_dir();        let profiles_dir = blade_dir.join("profiles");
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

    /// The reaper must not delete the template when a sync-swap was SIGKILLed
    /// mid-way (profile moved to .profile.old but the new copy not yet in).
    /// .profile.old is then the ONLY surviving profile; delete = data loss.
    #[test]
    fn reaper_restores_template_displaced_by_interrupted_swap() {
        // Hermetic: run against a temp blade dir, not the shared ~/.blade that
        // other reaper tests mutate in parallel.
        let blade_dir = std::env::temp_dir().join("bladebro-test-swap");
        let _ = std::fs::remove_dir_all(&blade_dir);

        // Simulate the crash state: template was moved aside, new copy never
        // renamed in. `profile` is missing; `.profile.old` holds the profile.
        std::fs::create_dir_all(blade_dir.join(".profile.old").join("Default")).unwrap();
        std::fs::write(blade_dir.join(".profile.old").join("Default/Cookies"), b"displaced-cookie").unwrap();

        // The displaced profile must be resurrected, not deleted.
        assert!(restore_interrupted_swap(&blade_dir), "swap-restore should trigger");
        assert_eq!(
            std::fs::read(blade_dir.join("profile/Default/Cookies")).unwrap(),
            b"displaced-cookie"
        );
        assert!(!blade_dir.join(".profile.old").exists(), ".profile.old consumed by restore");

        // When the template is already present, .profile.old is just staging
        // and must be dropped without disturbing `profile`.
        std::fs::write(blade_dir.join("profile/Default/Cookies"), b"live").unwrap();
        std::fs::create_dir_all(blade_dir.join(".profile.old")).unwrap();
        assert!(!restore_interrupted_swap(&blade_dir), "no restore when profile present");
        assert_eq!(
            std::fs::read(blade_dir.join("profile/Default/Cookies")).unwrap(),
            b"live",
            "present template must be left untouched"
        );
        assert!(!blade_dir.join(".profile.old").exists(), "stale .profile.old cleaned");

        let _ = std::fs::remove_dir_all(&blade_dir);
    }
}
