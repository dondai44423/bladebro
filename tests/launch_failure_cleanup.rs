//! A failed launch attempt must leave nothing behind: not the browser's
//! process tree (a forked helper, like Chrome's zygote, outlives a launcher
//! that died at startup) and not the session profile (with BLADE_FRESH=1
//! every attempt — up to three per launch — creates its own
//! `bladebro-chrome-*` dir under the temp root). Both transports.
//!
//! The same holds when a termination signal (Ctrl-C) reaches an in-process
//! run's process group mid-launch: the browser and Xvfb each run in their own
//! group, so bladebro dying on the signal's default action used to orphan
//! them instead of cleaning up.
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

/// A stand-in browser that records its own pid and a forked helper's, then
/// lingers at startup (no debug endpoint) long enough for a signal to land
/// mid-launch, and dies.
fn lingering_browser(root: &Path) -> (PathBuf, PathBuf) {
    let pids = root.join("helpers.pid");
    let fake = root.join("fake-chrome");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho $$ >> '{p}'\nsleep 60 &\necho $! >> '{p}'\nsleep 1\nexit 1\n",
            p = pids.display()
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

/// Linux: live processes still tied to bladebro `pid`'s virtual display.
/// Xvfb names the session's Xauthority file (`…-xauth-<pid>-…`) in its
/// command line; the WM and the browser tree carry it in XAUTHORITY. Killed
/// here either way. Elsewhere there is no Xvfb: nothing to find.
fn display_leftovers(pid: u32) -> Vec<i32> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let marker = format!("-xauth-{pid}-");
    let scan = || -> Vec<i32> {
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
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while !scan().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors = scan();
    for pid in &survivors {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    survivors
}

/// SIGINT to an in-process run's process group while its browser is still
/// starting (the user's Ctrl-C): bladebro must not die on the spot. It has to
/// outlive the signal long enough to tear down the browser tree, the profile
/// and the virtual display it started.
fn interrupt_mid_launch(name: &str, args: &[&str]) {
    let _serial = serial();
    let (root, temp, fake, pids) = scenario_with(name, lingering_browser);
    let errlog = root.join("stderr.log");
    let mut child = base_command(&root, &fake, &temp)
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&errlog).unwrap())
        .spawn()
        .unwrap();
    let recorded = || {
        std::fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .count()
    };
    // The browser and its helper are up; the launch is waiting on the
    // debug endpoint.
    let deadline = Instant::now() + Duration::from_secs(30);
    while recorded() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGINT);
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
    let stderr = std::fs::read_to_string(&errlog).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);
    let status =
        status.unwrap_or_else(|| panic!("bladebro still running 60s after SIGINT\n{stderr}"));
    assert!(
        survivors.is_empty() && profiles.is_empty() && display.is_empty(),
        "SIGINT mid-launch left browser processes {survivors:?}, display processes {display:?} and profiles {profiles:?} ({status:?})\n{stderr}"
    );
    assert_eq!(
        status.signal(),
        None,
        "bladebro died of the signal instead of cleaning up\n{stderr}"
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
