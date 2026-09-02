//! Data-dir resolution integration tests (issue #20: XDG + BLADE_HOME).
//!
//! These spawn the real binary with controlled env in a SUBPROCESS so the
//! process-global env vars can never race with parallel in-process tests.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_bladebro")
}

/// Run `bladebro doctor` with extra env, return stdout.
fn doctor_with(envs: &[(&str, &str)], home: Option<&str>) -> String {
    let tmp_home = std::env::temp_dir().join(format!("bladebro-xdg-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_home);
    std::fs::create_dir_all(&tmp_home).unwrap();
    // Give the fake home a .local so the XDG-default branch is reachable,
    // while keeping it free of any legacy .blade state.
    std::fs::create_dir_all(tmp_home.join(".local")).unwrap();

    let mut cmd = Command::new(bin());
    cmd.arg("doctor")
        .env_remove("BLADE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", home.unwrap_or(tmp_home.to_str().unwrap()))
        .env("BLADE_PROFILE_DIR", tmp_home.join("p").to_str().unwrap());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("doctor must run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&tmp_home);
    stdout.to_string()
}

#[test]
fn blade_home_override_wins() {
    let out = doctor_with(&[("BLADE_HOME", "/tmp/bladebro-blade-home")], None);
    let data_line = out
        .lines()
        .find(|l| l.contains("Data directory"))
        .expect("doctor must print Data directory");
    assert!(
        data_line.contains("/tmp/bladebro-blade-home"),
        "BLADE_HOME must be honored: {data_line}"
    );
    assert!(data_line.contains("BLADE_HOME"), "reason label: {data_line}");
}

#[test]
fn xdg_state_home_resolves_under_it() {
    let out = doctor_with(&[("XDG_STATE_HOME", "/tmp/bladebro-xdg-state")], None);
    let data_line = out
        .lines()
        .find(|l| l.contains("Data directory"))
        .expect("doctor must print Data directory");
    assert!(
        data_line.contains("/tmp/bladebro-xdg-state/blade"),
        "XDG_STATE_HOME must be honored: {data_line}"
    );
}

#[test]
fn missing_local_dir_falls_back_to_legacy() {
    // Home WITHOUT a .local dir at all: resolution falls to ~/.blade.
    let bare = std::env::temp_dir().join(format!("bladebro-bare-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bare);
    std::fs::create_dir_all(&bare).unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("doctor")
        .env_remove("BLADE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", bare.to_str().unwrap())
        .env("BLADE_PROFILE_DIR", bare.join("p").to_str().unwrap());
    let out = cmd.output().expect("doctor must run");
    let _ = std::fs::remove_dir_all(&bare);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let data_line = stdout
        .lines()
        .find(|l| l.contains("Data directory"))
        .expect("doctor must print Data directory");
    assert!(
        data_line.contains(".blade"),
        "no .local -> legacy ~/.blade: {data_line}"
    );
}