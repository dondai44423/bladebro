//! A failed launch attempt must leave nothing behind: not the browser's
//! process tree (a forked helper, like Chrome's zygote, outlives a launcher
//! that died at startup) and not the session profile (with BLADE_FRESH=1
//! every attempt — up to three per launch — creates its own
//! `bladebro-chrome-*` dir under the temp root). Both transports.
//!
//! The same holds when a termination signal (Ctrl-C) reaches an in-process
//! run's process group, mid-launch or while the command runs: the browser runs
//! in its own group, so bladebro dying on the signal's default action used to
//! orphan it instead of cleaning up. A signal the run inherited as ignored
//! (`nohup`, a non-interactive shell's `cmd &`) must stay ignored: the command
//! finishes as it did before signals were caught. The virtual display must
//! outlive a Ctrl-C to the run's group until the browser is closed, yet a
//! SIGKILL, which nothing can catch, to the group or to bladebro alone must
//! still take it down.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A stand-in browser that forks a long-lived helper (same process group,
/// like Chrome's zygote; its pid is recorded) and then dies at startup.
fn fake_browser(root: &Path) -> (PathBuf, PathBuf) {
    let pids = root.join("helpers.pid");
    let fake = root.join("fake-chrome");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nsleep 60 &\necho $! >> '{}'\nexit 1\n",
            pids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    (fake, pids)
}

/// A stand-in browser that records its start (`starts`), its own pid and a
/// forked helper's, then lingers at startup (no debug endpoint) long enough
/// for a signal to land mid-launch, and dies.
fn lingering_browser(root: &Path) -> (PathBuf, PathBuf) {
    let pids = root.join("helpers.pid");
    let fake = root.join("fake-chrome");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho start >> '{s}'\necho $$ >> '{p}'\nsleep 60 &\necho $! >> '{p}'\nsleep 1\nexit 1\n",
            s = root.join("starts").display(),
            p = pids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    (fake, pids)
}

/// A stand-in browser that serves just enough of the DevTools HTTP endpoint
/// (python3, on the `--remote-debugging-port` it is handed) for the launch to
/// succeed: `/json/version` answers and `/json` lists no targets, so the GL
/// ladder walks its stages and keeps the last one. Once the launch's GL probe
/// has listed targets, the next `/json/version` (`probe`'s first request)
/// writes `running` and takes 3s to answer: the command itself, not the
/// launch, is in flight. Records its start and pid like the other stand-ins.
fn endpoint_browser(root: &Path) -> (PathBuf, PathBuf) {
    endpoint_browser_with(root, "")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// `endpoint_browser` that, when bladebro stops it (SIGTERM), first records in
/// `display-at-close` whether its X display is still served: `up`, `down`, or
/// `none` without a virtual display. Real Chrome exits on losing its display,
/// so `down` means a real browser would already be gone, unflushed. The pause
/// lets a display that is dying anyway finish dying.
fn display_checking_browser(root: &Path) -> (PathBuf, PathBuf) {
    let hook = format!(
        r#"
import signal, socket
def closing(*_):
    time.sleep(0.3)
    display = os.environ.get('DISPLAY', '')
    state = 'none'
    if display.startswith(':'):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.connect('/tmp/.X11-unix/X' + display[1:].split('.')[0])
            state = 'up'
        except OSError:
            state = 'down'
        s.close()
    with open({out:?}, 'w') as f:
        f.write(state)
    os._exit(0)
signal.signal(signal.SIGTERM, closing)
"#,
        out = root.join("display-at-close").to_string_lossy()
    );
    endpoint_browser_with(root, &hook)
}

/// `endpoint_browser` with `hook` (python) run before it starts serving.
fn endpoint_browser_with(root: &Path, hook: &str) -> (PathBuf, PathBuf) {
    let pids = root.join("helpers.pid");
    let starts = root.join("starts");
    let running = root.join("running");
    let fake = root.join("fake-chrome");
    std::fs::write(
        &fake,
        format!(
            r#"#!/usr/bin/env python3
import http.server, json, os, sys, time
port = int(next(a for a in sys.argv if a.startswith('--remote-debugging-port=')).split('=', 1)[1])
with open({starts:?}, 'a') as f:
    f.write('start\n')
with open({pids:?}, 'a') as f:
    f.write('%d\n' % os.getpid())
listed = []
class Endpoint(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/json/version':
            if listed:
                with open({running:?}, 'a') as f:
                    f.write('run\n')
                time.sleep(3)
            body = {{'Browser': 'FakeChrome/1.0', 'Protocol-Version': '1.3'}}
        else:
            listed.append(self.path)
            body = []
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *args):
        pass
{hook}
http.server.ThreadingHTTPServer(('127.0.0.1', port), Endpoint).serve_forever()
"#,
            starts = starts.to_string_lossy(),
            pids = pids.to_string_lossy(),
            running = running.to_string_lossy(),
            hook = hook,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    (fake, pids)
}

fn base_command(root: &Path, fake: &Path, temp: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_bladebro"));
    cmd.env("BLADE_HOME", root.join("home"))
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env("BLADE_NO_WARMING", "1")
        .env("BLADE_FRESH", "1")
        .env("TMPDIR", temp)
        .env("CHROME_PATH", fake)
        .env("BLADE_LANE", "agent")
        .env_remove("BLADE_PROXY")
        .env_remove("BLADE_CHROME_FLAGS")
        .env_remove("BLADE_PROFILE_DIR");
    cmd
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Leftover profiles and still-running helpers (killed here either way so
/// a failing run leaves nothing behind either).
fn leftovers(temp: &Path, pids: &Path) -> (Vec<String>, Vec<i32>) {
    let profiles: Vec<String> = std::fs::read_dir(temp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("bladebro-chrome-"))
        .collect();
    let helpers: Vec<i32> = std::fs::read_to_string(pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(!helpers.is_empty(), "the fake browser never ran");
    // A killed helper is reaped by its new parent shortly after the signal.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while helpers.iter().any(|p| alive(*p)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let survivors: Vec<i32> = helpers.into_iter().filter(|p| alive(*p)).collect();
    for pid in &survivors {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    (profiles, survivors)
}

/// Every launch runs bladebro's orphan reaper, which kills an Xvfb whose
/// bladebro is dead: run these tests one at a time, or a concurrent test's
/// launch reaps the display a SIGINT'd run leaked before the check sees it.
static LAUNCHES: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    LAUNCHES.lock().unwrap_or_else(|e| e.into_inner())
}

fn scenario(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    scenario_with(name, fake_browser)
}

fn scenario_with(
    name: &str,
    browser: fn(&Path) -> (PathBuf, PathBuf),
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("blade-leak-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let temp = root.join("tmp");
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (fake, pids) = browser(&root);
    (root, temp, fake, pids)
}

/// Linux: live processes tied to bladebro `pid`'s virtual display (see
/// `display_leftovers`). Elsewhere there is no Xvfb: nothing to find.
fn display_processes(pid: u32) -> Vec<i32> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let marker = format!("-xauth-{pid}-");
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|p| {
            ["cmdline", "environ"].iter().any(|f| {
                let raw = std::fs::read(format!("/proc/{p}/{f}")).unwrap_or_default();
                String::from_utf8_lossy(&raw).contains(&marker)
            })
        })
        .collect()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Whether bladebro finds an Xvfb here (its fixed locations, then PATH). A
/// display test then fails, rather than skips, when no display came up.
fn xvfb_installed() -> bool {
    let fixed = [
        "/run/current-system/sw/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/opt/Xvfb/bin",
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    cfg!(target_os = "linux")
        && fixed
            .into_iter()
            .map(PathBuf::from)
            .chain(std::env::split_paths(&path))
            .any(|dir| dir.join("Xvfb").is_file())
}

/// Linux: live processes still tied to bladebro `pid`'s virtual display.
/// Xvfb names the session's Xauthority file (`…-xauth-<pid>-…`) in its
/// command line; the WM and the browser tree carry it in XAUTHORITY. Killed
/// here either way. Elsewhere there is no Xvfb: nothing to find.
fn display_leftovers(pid: u32) -> Vec<i32> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !display_processes(pid).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors = display_processes(pid);
    for pid in &survivors {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    survivors
}

/// The browser and its forked helper are up: the launch is waiting on the
/// debug endpoint (`lingering_browser`).
fn launching(root: &Path) -> bool {
    std::fs::read_to_string(root.join("helpers.pid"))
        .unwrap_or_default()
        .lines()
        .count()
        >= 2
}

/// The launch is done and the command's own request is in flight
/// (`endpoint_browser`). Linux-only with the tests that need it: those wait
/// for bladebro to reach a stand-in DevTools endpoint on a virtual display it
/// started, and CI showed the endpoint poll timing out on macOS (no Xvfb; the
/// launch path differs), so running them there would assert a scenario that
/// does not exist on that platform.
#[cfg(target_os = "linux")]
fn running(root: &Path) -> bool {
    root.join("running").exists()
}

/// How many times the launch started the stand-in browser.
fn starts(root: &Path) -> usize {
    std::fs::read_to_string(root.join("starts"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The first browser of the launch has started (`endpoint_browser`: its GL
/// probe, which finds no WebGL and walks the GL ladder, is still ahead).
fn started(root: &Path) -> bool {
    starts(root) >= 1
}

struct SignalledRun {
    status: std::process::ExitStatus,
    /// Read by the Linux-only `signals_ignored_on_entry_stay_ignored`; every
    /// other platform keeps the field for a uniform constructor.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    stdout: String,
    stderr: String,
    /// Browser starts over the whole run (`starts`).
    starts: usize,
    /// What `display_checking_browser` saw when it was stopped. Read by the
    /// Linux-only display test; kept elsewhere for a uniform constructor.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    display_at_close: Option<String>,
}

/// Start an in-process run the way a shell starts a job: in its own process
/// group, with the `ignored` signals handed down as SIG_IGN (what `nohup`
/// does for SIGHUP, and a non-interactive shell's `cmd &` for SIGINT). Once
/// `ready`, send each of `signals` to that group (the user's Ctrl-C). Whatever
/// the outcome, nothing the run started may be left: not the browser tree,
/// not the profile, not the virtual display.
fn signal_run(
    name: &str,
    browser: fn(&Path) -> (PathBuf, PathBuf),
    args: &[&str],
    ignored: &'static [libc::c_int],
    ready: fn(&Path) -> bool,
    signals: &[libc::c_int],
) -> SignalledRun {
    let _serial = serial();
    let (root, temp, fake, pids) = scenario_with(name, browser);
    let outlog = root.join("stdout.log");
    let errlog = root.join("stderr.log");
    let mut cmd = base_command(&root, &fake, &temp);
    cmd.args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&outlog).unwrap())
        .stderr(std::fs::File::create(&errlog).unwrap());
    // SAFETY: signal(2) is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            for &sig in ignored {
                libc::signal(sig, libc::SIG_IGN);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready(&root) && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(25));
    }
    let was_ready = ready(&root);
    for &sig in signals {
        unsafe {
            libc::kill(-(child.id() as i32), sig);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (profiles, survivors) = leftovers(&temp, &pids);
    let display = display_leftovers(child.id());
    let stdout = std::fs::read_to_string(&outlog).unwrap_or_default();
    let stderr = std::fs::read_to_string(&errlog).unwrap_or_default();
    let starts = starts(&root);
    let display_at_close = std::fs::read_to_string(root.join("display-at-close")).ok();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        was_ready,
        "the run never reached the signal point\n{stderr}"
    );
    let status =
        status.unwrap_or_else(|| panic!("bladebro still running 60s after {signals:?}\n{stderr}"));
    assert!(
        survivors.is_empty() && profiles.is_empty() && display.is_empty(),
        "signals {signals:?} left browser processes {survivors:?}, display processes {display:?} and profiles {profiles:?} ({status:?})\n{stderr}"
    );
    SignalledRun {
        status,
        stdout,
        stderr,
        starts,
        display_at_close,
    }
}

/// SIGINT while the browser is still starting: bladebro must not die on the
/// spot. It has to outlive the signal long enough to tear down the browser
/// tree, the profile and the virtual display it started (`signal_run` checks
/// that), and then die of the signal after all: a shell running it in a loop
/// or a script stops at Ctrl-C only when its child died of SIGINT. The
/// browser dies at startup, but the launch must not retry it (--no-sandbox,
/// a fresh port) once the signal is in.
fn interrupt_mid_launch(name: &str, args: &[&str]) {
    let run = signal_run(
        name,
        lingering_browser,
        args,
        &[],
        launching,
        &[libc::SIGINT],
    );
    assert_eq!(
        run.status.signal(),
        Some(libc::SIGINT),
        "bladebro did not die of the SIGINT it cleaned up after ({:?})\n{}",
        run.status,
        run.stderr
    );
    assert_eq!(
        run.starts, 1,
        "the launch kept starting browsers after the signal\n{}",
        run.stderr
    );
}

#[test]
fn failed_ws_launch_attempts_clean_up_tree_and_profile() {
    let _serial = serial();
    let (root, temp, fake, pids) = scenario("ws");
    let output = base_command(&root, &fake, &temp)
        .arg("probe")
        .output()
        .unwrap();
    let (profiles, survivors) = leftovers(&temp, &pids);
    let _ = std::fs::remove_dir_all(&root);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("exited during startup"), "{stderr}");
    assert!(
        survivors.is_empty() && profiles.is_empty(),
        "failed launch left browser helpers {survivors:?} and profiles {profiles:?}"
    );
}

#[test]
fn failed_pipe_launch_attempts_clean_up_tree_and_profile() {
    let _serial = serial();
    let (root, temp, fake, pids) = scenario("pipe");
    let mut child = base_command(&root, &fake, &temp)
        .arg("mcp")
        .env("BLADE_TRANSPORT", "pipe")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"see","arguments":{"mode":"content"}}}"#;
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{request}").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    let (profiles, survivors) = leftovers(&temp, &pids);
    let _ = std::fs::remove_dir_all(&root);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("exited during startup"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        survivors.is_empty() && profiles.is_empty(),
        "failed launch left browser helpers {survivors:?} and profiles {profiles:?}"
    );
}

#[test]
fn sigint_mid_launch_of_a_one_shot_command_leaves_nothing_behind() {
    interrupt_mid_launch("sigint-nav", &["nav", "http://127.0.0.1:9/", "--no-daemon"]);
}

#[test]
fn sigint_mid_launch_of_probe_leaves_nothing_behind() {
    interrupt_mid_launch("sigint-probe", &["probe"]);
}

/// Ctrl-C while the command itself runs (the launch is done): the run is
/// dropped, the browser bladebro launched is closed, the command reports the
/// interruption, and only then does bladebro die of the signal, instead of
/// dying at once with Chrome left running.
#[cfg(target_os = "linux")]
#[test]
fn sigint_while_probe_runs_closes_the_browser_it_launched() {
    let run = signal_run(
        "sigint-run",
        endpoint_browser,
        &["probe"],
        &[],
        running,
        &[libc::SIGINT],
    );
    assert_eq!(
        run.status.signal(),
        Some(libc::SIGINT),
        "{:?}\n{}",
        run.status,
        run.stderr
    );
    assert!(
        run.stderr
            .contains("interrupted by a termination signal before the command finished"),
        "{}",
        run.stderr
    );
}

/// Ctrl-C while the launch walks its GL ladder (`endpoint_browser` offers no
/// WebGL, so every stage is a fresh browser): the ladder stops at the browser
/// it has, which is then closed, instead of starting the remaining stages'
/// browsers first.
#[test]
fn sigint_during_the_gl_ladder_stops_relaunching() {
    let run = signal_run(
        "sigint-gl",
        endpoint_browser,
        &["probe"],
        &[],
        started,
        &[libc::SIGINT],
    );
    assert_eq!(
        run.status.signal(),
        Some(libc::SIGINT),
        "{:?}\n{}",
        run.status,
        run.stderr
    );
    assert_eq!(
        run.starts, 1,
        "the GL ladder kept starting browsers after the signal\n{}",
        run.stderr
    );
}

/// Under `nohup` (SIGHUP ignored) or as a non-interactive shell's background
/// job (SIGINT ignored) those signals never ended bladebro; catching them must
/// not start to. The run finishes normally.
#[cfg(target_os = "linux")]
#[test]
fn signals_ignored_on_entry_stay_ignored() {
    let run = signal_run(
        "nohup",
        endpoint_browser,
        &["probe"],
        &[libc::SIGHUP, libc::SIGINT],
        running,
        &[libc::SIGHUP, libc::SIGINT],
    );
    assert!(
        run.status.success(),
        "an ignored signal aborted the run ({:?})\n{}",
        run.status,
        run.stderr
    );
    assert!(
        run.stdout.contains("nothing to drive"),
        "probe did not finish\nstdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
}

/// Ctrl-C to the job's process group (the terminal's ^C) reaches bladebro and,
/// unless kept out of that group, the virtual display: Xvfb died on the spot
/// while bladebro was still closing the browser, and a real Chrome exited on
/// losing its display before its graceful close could flush storage. The
/// display must still be up when bladebro stops the browser.
/// (`tools/reliability_probe/persistence.py` checks the flush itself with a
/// real Chrome.)
#[cfg(target_os = "linux")]
#[test]
fn sigint_to_the_process_group_keeps_the_display_up_until_the_browser_is_closed() {
    let run = signal_run(
        "sigint-display",
        display_checking_browser,
        &["probe"],
        &[],
        running,
        &[libc::SIGINT],
    );
    assert_eq!(
        run.status.signal(),
        Some(libc::SIGINT),
        "{:?}\n{}",
        run.status,
        run.stderr
    );
    if run.display_at_close.as_deref() == Some("none") && !xvfb_installed() {
        eprintln!("no virtual display here (no Xvfb); skipping the display check");
        return;
    }
    assert_eq!(
        run.display_at_close.as_deref(),
        Some("up"),
        "the browser lost its display before bladebro closed it\n{}",
        run.stderr
    );
}

#[cfg(target_os = "linux")]
struct KilledRun {
    status: std::process::ExitStatus,
    /// Display processes (Xvfb, window manager, browser) just before the kill.
    display_before: usize,
    /// Display processes still alive after it (`display_leftovers`).
    display_after: Vec<i32>,
    stderr: String,
}

#[cfg(target_os = "linux")]
/// SIGKILL an in-process `probe` while the command runs: to its whole process
/// group (`whole_group`) or to bladebro's pid alone.
fn sigkill_mid_run(name: &str, whole_group: bool) -> KilledRun {
    let _serial = serial();
    let (root, temp, fake, pids) = scenario_with(name, endpoint_browser);
    let errlog = root.join("stderr.log");
    let mut child = base_command(&root, &fake, &temp)
        .arg("probe")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&errlog).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !running(&root) && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(25));
    }
    let was_running = running(&root);
    let display_before = display_processes(child.id()).len();
    let target = if whole_group {
        -(child.id() as i32)
    } else {
        child.id() as i32
    };
    unsafe {
        libc::kill(target, libc::SIGKILL);
    }
    let status = child.wait().unwrap();
    let _ = leftovers(&temp, &pids);
    let display_after = display_leftovers(child.id());
    let stderr = std::fs::read_to_string(&errlog).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        was_running,
        "the run never reached the signal point\n{stderr}"
    );
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{stderr}");
    KilledRun {
        status,
        display_before,
        display_after,
        stderr,
    }
}

/// SIGKILL to an in-process run's process group (`timeout -s KILL`, a
/// harness's killpg, a cancelled CI job) leaves bladebro no chance to clean
/// up. The virtual display must die with it: Xvfb and the window manager run
/// in their own process group (out of reach of the group's Ctrl-C), so it is
/// the kernel's parent-death signal that takes them down; a real Chrome then
/// exits on losing its display. (This stand-in browser does not, and is not
/// what is checked: it runs in its own group by design and is cleared first.)
#[cfg(target_os = "linux")]
#[test]
fn sigkill_to_the_process_group_takes_the_virtual_display_with_it() {
    let run = sigkill_mid_run("sigkill", true);
    assert!(
        run.display_after.is_empty(),
        "SIGKILL to bladebro's process group left display processes {:?} running\n{}",
        run.display_after,
        run.stderr
    );
}

/// SIGKILL to bladebro's pid alone (a harness's kill(pid), the OOM killer)
/// reaches nothing else: the virtual display must still die with bladebro
/// instead of running on until some later launch's orphan reaper finds it.
#[cfg(target_os = "linux")]
#[test]
fn sigkill_to_bladebro_alone_takes_the_virtual_display_with_it() {
    let run = sigkill_mid_run("sigkill-alone", false);
    if run.display_before == 0 && !xvfb_installed() {
        eprintln!(
            "no virtual display here (no Xvfb); skipping ({:?})",
            run.status
        );
        return;
    }
    assert!(
        run.display_before > 0,
        "no virtual display came up although Xvfb is installed\n{}",
        run.stderr
    );
    assert!(
        run.display_after.is_empty(),
        "SIGKILL to bladebro alone left display processes {:?} running\n{}",
        run.display_after,
        run.stderr
    );
}
