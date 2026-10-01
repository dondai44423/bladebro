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
//!
//! Module map: this file is the `SessionProfile` lifecycle — create/adopt,
//! teardown and the sole-survivor sync-back, template locks, and the path
//! helpers; children: `reap` (orphan reaper + Xvfb cleanup), `copy`
//! (profile-tree copy + its skip list).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{BladeError, Result};
use crate::platform;

mod copy;
mod reap;

use self::copy::copy_profile;
pub(crate) use self::copy::copy_profile_ex;
pub use self::reap::reap_orphans;

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

        let seasoned = !std::env::var("BLADE_FRESH")
            .map(|v| v == "1")
            .unwrap_or(false);

        let dir = if let Ok(custom) = std::env::var("BLADE_PROFILE_DIR") {
            // Explicit override: use it as-is (the caller owns
            // the consequences — this is the escape hatch).
            if !custom.is_empty() {
                let d = PathBuf::from(custom);
                std::fs::create_dir_all(&d)
                    .map_err(|e| BladeError::Other(format!("cannot create profile dir: {e}")))?;
                return Ok(Self {
                    dir: d,
                    seasoned: false,
                    real_root: None,
                });
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
            let _ = std::fs::write(dir.join(".blade-owner"), std::process::id().to_string());
        }

        Ok(Self {
            dir,
            seasoned,
            real_root: None,
        })
    }

    /// Create a real-lane session profile (clone mechanism): copy the
    /// imported template at `root/template` into a per-process session dir
    /// under `root/profiles/` — the same discipline the agent lane uses for
    /// its own template, so concurrent bladebro processes (daemon + MCP)
    /// never contend Chrome's SingletonLock on the clone, and the clone
    /// still ages through the sole-survivor sync-back.
    pub fn create_real(root: &Path) -> Result<Self> {
        reap_orphans();
        let dir = root
            .join("profiles")
            .join(format!("sess-{}", std::process::id()));
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
            Self::sync_back_impl(
                root,
                "template",
                "template.sync",
                ".template.old",
                &self.dir,
            );
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
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock)
        {
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

    /// Copy a session profile into the template at `root` without removing
    /// the session dir. Used by the orphan reaper to rescue a dead session's
    /// state on the next launch (graceful-kill the orphan, flush, then copy).
    /// The template is only replaced after Chrome is dead, so the copy is
    /// never taken from a live, un-flushed profile.
    ///
    /// `root` is passed in rather than re-resolved: `blade_dir()` reads the
    /// process-global env, and a second resolution could land in a different
    /// directory than the reap that called us (the test suite flips
    /// BLADE_HOME on other threads).
    fn sync_back_only_at(root: &Path, dir: &Path) {
        if other_live_sessions_at(&root.join("profiles")) {
            return;
        }
        if !Self::acquire_lock_at(root) {
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
        if std::env::var("BLADE_NO_WARMING")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            return false;
        }
        let marker = platform::blade_dir().join(".warmed");
        // If the template profile is empty or missing, warming is needed
        // regardless of the marker (the profile was deleted/reset).
        let template = platform::blade_dir().join("profile");
        let template_empty = !template.is_dir()
            || template
                .read_dir()
                .map(|mut d| d.next().is_none())
                .unwrap_or(true);
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
            && dir
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("sess-"))
                .unwrap_or(false);
        if is_session {
            Self::sync_back_and_remove(dir);
        } else if dir.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn sync_back_and_remove(dir: &Path) {
        Self::sync_back_impl(
            &platform::blade_dir(),
            "profile",
            ".profile.sync",
            ".profile.old",
            dir,
        );
    }

    /// Real-lane static teardown: sync a clone session back to its
    /// realbrowser root's template and remove it.
    pub fn sync_back_real(root: &Path, dir: &Path) {
        Self::sync_back_impl(root, "template", "template.sync", ".template.old", dir);
    }

    /// Sole-survivor copy-back: if another live session exists, skip — its
    /// state wins when IT exits. Shared by the agent lane (`profile`) and
    /// the real lane (`template` under the realbrowser root).
    fn sync_back_impl(
        root: &Path,
        template_name: &str,
        tmp_name: &str,
        old_name: &str,
        dir: &Path,
    ) {
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
    if profiles
        .file_name()
        .map(|n| n != "profiles")
        .unwrap_or(true)
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    use super::copy::copy_profile;
    use super::reap::{reap_orphans_at, restore_interrupted_swap};

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
        // Hermetic: locks live in a temp dir, never the shared real one.
        let dir = std::env::temp_dir().join(format!("bladebro-lock-dead-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let lock = dir.join(".template.lock.test-dead");
        let _ = std::fs::remove_file(&lock);
        // A dead pid must never wedge sync: the lock must read as stale.
        std::fs::write(&lock, "999999999 0\n").unwrap();
        assert!(
            template_lock_stale(&lock),
            "lock owned by a dead pid must be stale"
        );
        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_with_live_owner_is_not_stale() {
        // Hermetic: locks live in a temp dir, never the shared real one.
        let dir = std::env::temp_dir().join(format!("bladebro-lock-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let lock = dir.join(".template.lock.test-live");
        let _ = std::fs::remove_file(&lock);
        let me = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(&lock, format!("{me} {now}\n")).unwrap();
        assert!(
            !template_lock_stale(&lock),
            "live owner lock must not be stale"
        );
        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reaper_syncs_back_before_delete() {
        // Hermetic: a temp root, never the shared real data dir. The env is
        // process-global and parallel tests flip BLADE_HOME; the pre-fix
        // version resolved the root twice (reap entry + sync) and could be
        // split across two directories by a flip — and it mutated the real
        // install (deleted its session dirs, swapped its template).
        let root =
            std::env::temp_dir().join(format!("bladebro-test-reaper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let profiles_dir = root.join("profiles");
        let template_dir = root.join("profile");

        // Template with a marker file the sync must REPLACE, not merge into.
        std::fs::create_dir_all(template_dir.join("Default")).unwrap();
        std::fs::write(template_dir.join("Default/Cookies"), b"template-cookie-db").unwrap();
        std::fs::write(template_dir.join("stale-marker"), b"old-template").unwrap();

        // Create a fake dead session with a marker file.
        let test_pid = 999_999_999u32; // guaranteed dead pid
        let sess_dir = profiles_dir.join(format!("sess-{test_pid}"));
        std::fs::create_dir_all(sess_dir.join("Default")).unwrap();
        std::fs::write(sess_dir.join(".blade-owner"), test_pid.to_string()).unwrap();
        std::fs::write(sess_dir.join("Default/Cookies"), b"fake-cookie-db-data").unwrap();

        // Run the reaper against the temp root.
        reap_orphans_at(&root);

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
        assert!(
            !template_dir.join("stale-marker").exists(),
            "template must be replaced by the sync, not merged into"
        );

        let _ = std::fs::remove_dir_all(&root);
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
        std::fs::write(
            blade_dir.join(".profile.old").join("Default/Cookies"),
            b"displaced-cookie",
        )
        .unwrap();

        // The displaced profile must be resurrected, not deleted.
        assert!(
            restore_interrupted_swap(&blade_dir),
            "swap-restore should trigger"
        );
        assert_eq!(
            std::fs::read(blade_dir.join("profile/Default/Cookies")).unwrap(),
            b"displaced-cookie"
        );
        assert!(
            !blade_dir.join(".profile.old").exists(),
            ".profile.old consumed by restore"
        );

        // When the template is already present, .profile.old is just staging
        // and must be dropped without disturbing `profile`.
        std::fs::write(blade_dir.join("profile/Default/Cookies"), b"live").unwrap();
        std::fs::create_dir_all(blade_dir.join(".profile.old")).unwrap();
        assert!(
            !restore_interrupted_swap(&blade_dir),
            "no restore when profile present"
        );
        assert_eq!(
            std::fs::read(blade_dir.join("profile/Default/Cookies")).unwrap(),
            b"live",
            "present template must be left untouched"
        );
        assert!(
            !blade_dir.join(".profile.old").exists(),
            "stale .profile.old cleaned"
        );

        let _ = std::fs::remove_dir_all(&blade_dir);
    }
}
