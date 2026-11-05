//! A failed launch attempt must leave nothing behind: not the browser's
//! process tree (a forked helper, like Chrome's zygote, outlives a launcher
//! that died at startup) and not the session profile (with BLADE_FRESH=1
//! every attempt — up to three per launch — creates its own
//! `bladebro-chrome-*` dir under the temp root). Both transports.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

fn scenario(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("blade-leak-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let temp = root.join("tmp");
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (fake, pids) = fake_browser(&root);
    (root, temp, fake, pids)
}

#[test]
fn failed_ws_launch_attempts_clean_up_tree_and_profile() {
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
