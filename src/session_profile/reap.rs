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
    #[cfg(any(target_os = "linux", target_os = "android"))]
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
    // Startup overlaps other sessions' sync-back. Staging is disposable only
    // while we hold the same lock as the copier; otherwise leave it intact.
    if let Some(_lock) = SessionProfile::acquire_lock_at(blade_dir) {
        let _ = std::fs::remove_dir_all(blade_dir.join(".profile.sync"));
        restore_interrupted_swap(blade_dir);
    }

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
            if let Some(_lock) = SessionProfile::acquire_lock_at(&root) {
                restore_interrupted_real_swap(&root);
                let _ = std::fs::remove_dir_all(root.join("template.sync"));
            }
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
        if reap_dead_session(root, &dir, real_root) {
            eprintln!("[bladebro] reaped dead session profile {name}");
        } else {
            eprintln!(
                "[bladebro] dead session profile {name} is still in use by a running browser; left in place (retried on next launch)"
            );
        }
    }
}

/// Reap one dead session dir: stop its orphaned browser, sync its state
/// (cookies, localStorage) back to the template, then delete it — without
/// the sync, sessions killed without a graceful shutdown lose all their
/// state. Returns false, touching nothing, when the profile could not be
/// freed: a sync copies a live browser's torn, unflushed databases over the
/// template, and the delete pulls the profile out from under it.
pub(super) fn reap_dead_session(root: &Path, dir: &Path, real_root: Option<&Path>) -> bool {
    if !kill_orphan_chrome(dir) {
        return false;
    }
    match real_root {
        Some(real_root) => SessionProfile::sync_back_real(real_root, dir),
        None => SessionProfile::sync_back_only_at(root, dir),
    }
    let _ = std::fs::remove_dir_all(dir);
    true
}

/// Stop the orphaned browser on a dead session's profile; true once no
/// process runs on it any more. A pid is only killed when it is a
/// Chromium-family browser whose argv names THIS profile — a recycled pid
/// must never be killed. SIGTERM first: Chrome flushes cookies/storage on
/// it, and the sync-back right after copies what it flushed.
fn kill_orphan_chrome(dir: &Path) -> bool {
    let targets = orphan_browser_pids(dir);
    if targets.is_empty() {
        return !profile_in_use(dir);
    }
    let runs_on = |pid: u32| platform::process_alive(pid) && process_uses_dir(pid, dir);
    for &pid in &targets {
        platform::kill_process_graceful(pid);
    }
    wait_until(|| !targets.iter().any(|&pid| runs_on(pid)));
    for &pid in &targets {
        if runs_on(pid) {
            platform::kill_process_force(pid);
        }
    }
    // Renderers and sandbox launchers exit after their browser.
    wait_until(|| !profile_in_use(dir))
}

/// Poll `done` for up to 2 s; its final answer.
fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..20 {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    done()
}

/// Browser processes to stop for a dead session: the SingletonLock pid when
/// it is a Chromium browser running on `dir`, plus (Linux) every Chromium
/// browser process whose argv names `dir` as its user-data-dir. The scan is
/// what finds a sandboxed browser: Flatpak runs Chromium in its own pid
/// namespace, so SingletonLock holds the in-sandbox pid (measured:
/// `steamdeck-2`), never the host pid. Child processes (`--type=…`) are left
/// to exit with their browser — killing the storage service first would
/// lose the very flush SIGTERM asks for.
fn orphan_browser_pids(dir: &Path) -> Vec<u32> {
    let singleton = read_singleton_pid(dir).filter(|&pid| {
        platform::process_alive(pid)
            && platform::process_is_chrome(pid)
            && process_uses_dir(pid, dir)
    });
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let scanned: Vec<u32> = processes_naming_dir(dir)
        .into_iter()
        .filter(|(pid, cmdline)| {
            let child = cmdline
                .split(|b| *b == 0 || *b == b' ')
                .any(|arg| arg.starts_with(b"--type="));
            Some(*pid) != singleton && !child && platform::cmdline_is_chromium(cmdline)
        })
        .map(|(pid, _)| pid)
        .collect();
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let scanned: Vec<u32> = Vec::new();
    singleton.into_iter().chain(scanned).collect()
}

/// Does any process still run on this profile? Something we could not
/// identify as a browser (so did not kill) counts: deleting a profile from
/// under it is exactly the failure this guards. Shared with `doctor`.
pub(crate) fn profile_in_use(dir: &Path) -> bool {
    if read_singleton_pid(dir)
        .is_some_and(|pid| platform::process_alive(pid) && process_uses_dir(pid, dir))
    {
        return true;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if !processes_naming_dir(dir).is_empty() {
        return true;
    }
    false
}

/// Every other process whose argv names `dir` as its user-data-dir, with
/// its raw cmdline.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn processes_naming_dir(dir: &Path) -> Vec<(u32, Vec<u8>)> {
    let me = std::process::id();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    procs
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            if pid == me {
                return None;
            }
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            cmdline_names_user_data_dir(&cmdline, dir).then_some((pid, cmdline))
        })
        .collect()
}

/// Does this command line (NUL- or space-separated) pass exactly `dir` as
/// `--user-data-dir`? Exact, not a substring: `sess-12` must never match a
/// live `sess-123`'s browser.
#[cfg(any(unix, test))]
fn cmdline_names_user_data_dir(cmdline: &[u8], dir: &Path) -> bool {
    let needle = format!("--user-data-dir={}", dir.display());
    let needle = needle.as_bytes();
    let bounded = |b: Option<&u8>| matches!(b, None | Some(0) | Some(b' '));
    cmdline
        .windows(needle.len())
        .enumerate()
        .any(|(at, window)| {
            window == needle
                && bounded(at.checked_sub(1).and_then(|i| cmdline.get(i)))
                && bounded(cmdline.get(at + needle.len()))
        })
}

/// Restore/clean a realbrowser template after a crashed import or sync
/// swap. Mirrors [`restore_interrupted_swap`] for the per-browser root.
fn restore_interrupted_real_swap(root: &Path) {
    let template = root.join("template");
    for (name, what) in [
        (".template.old", "sync swap"),
        ("template.old", "import swap"),
    ] {
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
    let _ = std::fs::remove_dir_all(root.join("template.tmp"));
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

/// Does process `pid` run with `dir` as its user-data-dir?
/// Guards against killing a recycled pid.
fn process_uses_dir(pid: u32, dir: &Path) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| cmdline_names_user_data_dir(&c, dir))
    }
    #[cfg(target_os = "macos")]
    {
        // No /proc on macOS — ask ps for the full command line.
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .is_ok_and(|o| cmdline_names_user_data_dir(o.stdout.trim_ascii_end(), dir))
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

/// Only a dead Bladebro display claim authorizes cleanup. Orphanhood alone
/// proves no ownership, and neither does the executable: the claim, the exact
/// Xvfb cmdline and the display number must all agree.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_xvfb() {
    reap_xvfb_at(Path::new("/tmp"));
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_xvfb_at(tmp: &Path) {
    for n in 99..200 {
        let claim = tmp.join(format!(".blade-x{n}-claim"));
        let owner = std::fs::read_to_string(&claim)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let Some(_) = owner.filter(|&pid| pid > 1 && !platform::process_alive(pid)) else {
            continue;
        };
        // The sibling Xauthority file (if any) dies with its claim's owner.
        if let Ok(rd) = std::fs::read_dir(tmp) {
            let prefix = format!(".blade-x{n}-xauth-");
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let lock = tmp.join(format!(".X{n}-lock"));
        let content = match std::fs::read_to_string(&lock) {
            Ok(c) => c,
            Err(_) => {
                let _ = std::fs::remove_file(&claim);
                continue;
            }
        };
        let pid: u32 = match content.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if platform::process_alive(pid) {
            // Alive — but orphaned? The owner died and the system manager
            // adopted it (see `is_orphaned`). And the claimer's? A system or
            // user service Xvfb is adopted by the manager too.
            if is_orphaned(pid)
                && proc_is_xvfb_on_display(pid, n)
                && started_with_claim(pid, &claim)
            {
                platform::kill_process_force(pid);
                let _ = std::fs::remove_file(&lock);
                eprintln!("[bladebro] reaped orphaned Xvfb :{n} (pid {pid})");
            }
            // A live foreign server must keep its lock, even if a stale
            // Bladebro claim was left before that server started.
            let _ = std::fs::remove_file(&claim);
            continue;
        }
        // Dead pid: the lock is stale. Remove it, then kill
        // any orphaned Xvfb still bound to this display.
        let _ = std::fs::remove_file(&lock);
        kill_xvfb_on_display(n, &claim);
        let _ = std::fs::remove_file(&claim);
        eprintln!("[bladebro] reaped stale Xvfb display :{n}");
    }
}

/// Is this process an orphan — adopted by the system manager because the
/// process that owned it died?
///
/// On a systemd host an orphan is reparented to the *user* manager
/// (`systemd --user`), NOT to PID 1, so a bare `ppid == 1` test misses every
/// orphan on Arch/Ubuntu/Fedora-class systems. Found live 2026-10-13 on Arch:
/// a deliberately orphaned Xvfb had PPID 1080 = `/usr/lib/systemd/systemd
/// --user`, so `reap_xvfb` removed the claim and left the display running
/// forever (the unit test failed on the same assumption).
///
/// Conservative by construction: `1` (classic init adoption) or a parent whose
/// comm is `systemd` (the manager). A live owner is named `bladebro`,
/// `chromium` or `Xvfb` — never the manager — and an unreadable parent means
/// NOT orphaned.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_orphaned(pid: u32) -> bool {
    let ppid = proc_ppid(pid);
    if ppid == 1 {
        return true;
    }
    if ppid <= 1 {
        // Unreadable / unparseable / self-parented: never authorize a kill.
        return false;
    }
    std::fs::read_to_string(format!("/proc/{ppid}/comm"))
        .map(|c| c.trim() == "systemd")
        .unwrap_or(false)
}

/// Parent pid of a process (0/1 = orphaned or init).
#[cfg(any(target_os = "linux", target_os = "android"))]
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
#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_is_xvfb_on_display(pid: u32, display: u16) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|c| xvfb_cmdline_matches(&c, display))
        .unwrap_or(false)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn xvfb_cmdline_matches(cmdline: &str, display: u16) -> bool {
    let mut args = cmdline.split('\0');
    args.next()
        .and_then(|a| Path::new(a).file_name())
        .is_some_and(|name| name == "Xvfb")
        && args.any(|a| a == format!(":{display}"))
}

/// Did `pid` start no later than `claim` was written (plus slack)?
/// Bladebro writes its claim and spawns Xvfb milliseconds later, so its
/// server is never much younger than its claim. One that started well after
/// a stale claim was launched by someone else — a system or user service
/// takes a display the dead claimer had freed. Unreadable data never
/// authorizes a kill.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn started_with_claim(pid: u32, claim: &Path) -> bool {
    const SLACK_SECS: f64 = 5.0;
    let Some(claim_age) = std::fs::metadata(claim)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
    else {
        return false;
    };
    process_age_secs(pid).is_some_and(|age| age + SLACK_SECS >= claim_age.as_secs_f64())
}

/// Seconds since `pid` started, on the boot clock (`/proc/uptime`'s clock).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_age_secs(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Parse after the last ')' (comm may hold spaces/parens): that field is
    // #3 (state), so #22 (starttime, in clock ticks since boot) is index 19.
    let start_ticks: f64 = stat
        .rsplit(')')
        .next()?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (ticks_per_sec > 0).then(|| uptime - start_ticks / ticks_per_sec as f64)
}

/// Kill any orphaned Xvfb serving display `:<n>` that started with the dead
/// owner's `claim`. Identified via /proc cmdline scan.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn kill_xvfb_on_display(display: u16, claim: &Path) {
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
        if !xvfb_cmdline_matches(&cmdline, display) {
            continue;
        }
        if is_orphaned(pid) && started_with_claim(pid, claim) {
            platform::kill_process_force(pid);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod xvfb_tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct OrphanDisplay {
        pid: u32,
        number: u16,
        root: std::path::PathBuf,
    }

    impl Drop for OrphanDisplay {
        fn drop(&mut self) {
            platform::kill_process_force(self.pid);
            let lock = format!("/tmp/.X{}-lock", self.number);
            if std::fs::read_to_string(&lock)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                == Some(self.pid)
            {
                let _ = std::fs::remove_file(lock);
                let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{}", self.number));
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The test spawns `Xvfb` from PATH. CI installs it; where it is absent
    /// the scenario cannot be staged, which is not a reaper failure.
    fn orphan_display() -> Option<OrphanDisplay> {
        let on_path = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("Xvfb").is_file()));
        if !on_path {
            eprintln!("Xvfb not on PATH — skipping orphan-display reaper test");
            return None;
        }
        let number = (150..200)
            .find(|n| {
                !Path::new(&format!("/tmp/.X{n}-lock")).exists()
                    && !Path::new(&format!("/tmp/.blade-x{n}-claim")).exists()
                    && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
            })
            .expect("a disposable test display must be free");
        // Only this expendable server is orphaned. The reaper's lock/claim
        // root is private, so it can never enumerate a real user's locks.
        let output = Command::new("sh")
            .args(["-c", &format!("Xvfb :{number} -screen 0 640x480x24 -nolisten tcp </dev/null >/dev/null 2>&1 & echo $!")])
            .output().unwrap();
        let pid = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let root = std::env::temp_dir().join(format!("blade-xvfb-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let display = OrphanDisplay { pid, number, root };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists() || !is_orphaned(pid) {
            assert!(
                Instant::now() < deadline,
                "test Xvfb must start and be adopted by the system manager"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        std::fs::write(
            display.root.join(format!(".X{number}-lock")),
            pid.to_string(),
        )
        .unwrap();
        Some(display)
    }

    #[test]
    fn xvfb_identity_requires_exact_executable_and_display_arguments() {
        let cmdline = format!("{}\0", ["/usr/bin/Xvfb", ":99", "-screen", "0"].join("\0"));
        assert!(xvfb_cmdline_matches(&cmdline, 99));
        assert!(!xvfb_cmdline_matches("/usr/bin/Xvfb\0:199\0", 19));
        assert!(!xvfb_cmdline_matches("/usr/bin/sh\0Xvfb\0:99\0", 99));
        assert!(!xvfb_cmdline_matches(
            "/usr/bin/Xvfb\0-logfile\0/tmp/:99\0",
            99
        ));
    }

    /// The tests below share the private test root and pick display numbers
    /// by probing /tmp: run them one at a time.
    static DISPLAY_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn xvfb_started_after_a_stale_claim_survives() {
        let _serial = DISPLAY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(display) = orphan_display() else {
            return;
        };
        let lock = display.root.join(format!(".X{}-lock", display.number));
        let claim = display
            .root
            .join(format!(".blade-x{}-claim", display.number));
        // A dead bladebro left its claim an hour ago; this server (a system
        // or user service, adopted by the manager) took the display later.
        std::fs::write(&claim, "2000000000").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&claim)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        reap_xvfb_at(&display.root);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            proc_is_xvfb_on_display(display.pid, display.number),
            "an Xvfb that started after the claim was never the claimer's"
        );
        assert!(lock.exists(), "its display lock must survive");
        assert!(!claim.exists(), "the stale claim itself is cleared");
    }

    #[test]
    fn xvfb_reaper_requires_a_dead_blade_claim() {
        let _serial = DISPLAY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(display) = orphan_display() else {
            return;
        };
        let lock = display.root.join(format!(".X{}-lock", display.number));
        let claim = display
            .root
            .join(format!(".blade-x{}-claim", display.number));
        reap_xvfb_at(&display.root);
        assert!(
            proc_is_xvfb_on_display(display.pid, display.number),
            "an unclaimed system-service Xvfb must survive"
        );
        assert!(lock.exists(), "foreign display lock must survive");

        // Malformed claims and live owners are never authority to kill.
        for owner in [
            "bad".to_string(),
            "0".to_string(),
            std::process::id().to_string(),
        ] {
            std::fs::write(&claim, owner).unwrap();
            reap_xvfb_at(&display.root);
            assert!(proc_is_xvfb_on_display(display.pid, display.number));
            assert!(claim.exists());
        }
        std::fs::write(&claim, "2000000000").unwrap();
        reap_xvfb_at(&display.root);
        let deadline = Instant::now() + Duration::from_secs(5);
        while proc_is_xvfb_on_display(display.pid, display.number) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !proc_is_xvfb_on_display(display.pid, display.number),
            "a claimed dead-owner Xvfb must be reaped"
        );
        assert!(!lock.exists());
        assert!(!claim.exists());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod orphan_browser_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// A dead session (owner pid can never exist) under a private root, plus
    /// a template whose content proves whether a sync-back happened.
    fn dead_session(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("blade-orphan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("profile/Default")).unwrap();
        std::fs::write(root.join("profile/Default/Cookies"), "template").unwrap();
        let dir = root.join("profiles/sess-999999998");
        std::fs::create_dir_all(dir.join("Default")).unwrap();
        std::fs::write(dir.join(".blade-owner"), "999999998").unwrap();
        std::fs::write(dir.join("Default/Cookies"), "session").unwrap();
        (root, dir)
    }

    /// A live process whose first /proc cmdline entry is `argv0` — with
    /// spaces, the shape Chromium leaves after rewriting its argv into one
    /// space-joined string (measured on Flatpak Chromium's browser process).
    /// `sh` + a blocking builtin: multi-call binaries (coreutils on NixOS)
    /// dispatch on argv[0] and would exit at once.
    /// spawn() returns once the child is inside execve, before the kernel
    /// has published its new argv: wait until /proc shows it.
    fn holder(argv0: &str) -> Child {
        let child = Command::new("sh")
            .arg0(argv0)
            .args(["-c", "read -r _"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let cmdline = format!("/proc/{}/cmdline", child.id());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !std::fs::read(&cmdline).is_ok_and(|c| c.starts_with(argv0.as_bytes())) {
            assert!(Instant::now() < deadline, "argv never published");
            std::thread::sleep(Duration::from_millis(5));
        }
        child
    }

    fn singleton_lock(dir: &Path, target: &str) {
        std::os::unix::fs::symlink(target, dir.join("SingletonLock")).unwrap();
    }

    fn exited_within(child: &mut Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn finish(mut child: Child, root: &Path) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reaper_stops_a_rewritten_argv_orphan_before_syncing() {
        let (root, dir) = dead_session("rewritten");
        let mut chrome = holder(&format!(
            "/app/chromium/chrome --enable-features=Foo --user-data-dir={}",
            dir.display()
        ));
        singleton_lock(&dir, &format!("host-{}", chrome.id()));
        reap_orphans_at(&root);
        let stopped = exited_within(&mut chrome, Duration::from_secs(5));
        let template = std::fs::read_to_string(root.join("profile/Default/Cookies"));
        let removed = !dir.exists();
        finish(chrome, &root);
        assert!(stopped, "the orphaned browser must be stopped");
        assert_eq!(template.unwrap(), "session", "state rescued after the stop");
        assert!(removed);
    }

    #[test]
    fn reaper_stops_an_orphan_whose_lock_names_a_sandbox_pid() {
        // Flatpak runs Chromium in its own pid namespace: SingletonLock holds
        // the in-sandbox pid (measured: `steamdeck-2`), never the host pid.
        let (root, dir) = dead_session("sandboxed");
        let mut chrome = holder(&format!(
            "/app/chromium/chrome --user-data-dir={}",
            dir.display()
        ));
        singleton_lock(&dir, "steamdeck-2");
        reap_orphans_at(&root);
        let stopped = exited_within(&mut chrome, Duration::from_secs(5));
        let template = std::fs::read_to_string(root.join("profile/Default/Cookies"));
        finish(chrome, &root);
        assert!(stopped, "the orphan must be found by its user-data-dir");
        assert_eq!(template.unwrap(), "session");
    }

    #[test]
    fn reaper_never_syncs_or_deletes_a_profile_it_could_not_free() {
        let (root, dir) = dead_session("held");
        let mut other = holder(&format!(
            "blade-test-holder --user-data-dir={}",
            dir.display()
        ));
        singleton_lock(&dir, &format!("host-{}", other.id()));
        reap_orphans_at(&root);
        let alive = !exited_within(&mut other, Duration::from_millis(200));
        let template = std::fs::read_to_string(root.join("profile/Default/Cookies"));
        let session = std::fs::read_to_string(dir.join("Default/Cookies"));
        finish(other, &root);
        assert!(alive, "an unidentified process is never killed");
        assert_eq!(
            session.ok().as_deref(),
            Some("session"),
            "a profile still in use must not be deleted"
        );
        assert_eq!(
            template.unwrap(),
            "template",
            "a live profile must not be synced"
        );
    }
}

#[cfg(test)]
mod template_recovery_tests {
    use super::*;

    #[test]
    fn user_data_dir_match_is_exact() {
        let dir = Path::new("/home/u/.blade/profiles/sess-12");
        let cases: &[(&[u8], bool)] = &[
            (
                b"chrome\0--user-data-dir=/home/u/.blade/profiles/sess-12\0",
                true,
            ),
            (
                b"chrome --user-data-dir=/home/u/.blade/profiles/sess-12 --x",
                true,
            ),
            (
                b"chrome --user-data-dir=/home/u/.blade/profiles/sess-12",
                true,
            ),
            // A live neighbour whose name extends ours is not ours.
            (
                b"chrome --user-data-dir=/home/u/.blade/profiles/sess-123 --x",
                false,
            ),
            (
                b"chrome --database=/home/u/.blade/profiles/sess-12/Crashpad",
                false,
            ),
            (
                b"chrome --x--user-data-dir=/home/u/.blade/profiles/sess-12",
                false,
            ),
        ];
        for &(cmdline, expected) in cases {
            assert_eq!(
                cmdline_names_user_data_dir(cmdline, dir),
                expected,
                "{:?}",
                String::from_utf8_lossy(cmdline)
            );
        }
    }
    #[test]
    fn real_sync_backup_recovers_but_incomplete_import_is_discarded() {
        let root = std::env::temp_dir().join(format!("blade-real-swap-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".template.old/Default")).unwrap();
        std::fs::create_dir_all(root.join("template.tmp")).unwrap();
        std::fs::write(root.join(".template.old/Default/state"), "last-good").unwrap();
        std::fs::write(root.join("template.tmp/partial"), "not-complete").unwrap();
        restore_interrupted_real_swap(&root);
        assert_eq!(
            std::fs::read_to_string(root.join("template/Default/state")).unwrap(),
            "last-good"
        );
        assert!(!root.join("template.tmp").exists());
        std::fs::remove_dir_all(root.join("template")).unwrap();
        std::fs::create_dir_all(root.join("template.tmp")).unwrap();
        std::fs::write(root.join("template.tmp/partial"), "not-complete").unwrap();
        restore_interrupted_real_swap(&root);
        assert!(
            !root.join("template").exists(),
            "a half-copied import must never become a template"
        );
        assert!(!root.join("template.tmp").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
