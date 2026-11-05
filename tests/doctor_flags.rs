//! `bladebro doctor` must name the setting that is actually broken. A
//! BLADE_CHROME_FLAGS value with an unterminated quote fails every launch;
//! it used to be reported under "Agent browser mode" with the fix "Set
//! BLADE_INCOGNITO to auto, 0 or 1", a setting that was fine.
//!
//! Spawns the real binary: the env vars are process-global.

#[test]
fn malformed_chrome_flags_are_reported_as_chrome_flags() {
    let home = std::env::temp_dir().join(format!("bladebro-doctor-flags-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bladebro"))
        .arg("doctor")
        .env("BLADE_HOME", &home)
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env("BLADE_CHROME_FLAGS", "--user-agent=\"Mozilla/5.0 (X11")
        .env_remove("BLADE_INCOGNITO")
        .env_remove("BLADE_PROFILE_DIR")
        .output()
        .expect("doctor must run");
    let _ = std::fs::remove_dir_all(&home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains("unterminated quote"))
        .unwrap_or_else(|| panic!("doctor did not report the malformed flags:\n{stdout}"));
    let check = lines[at];
    let fix = lines.get(at + 1).copied().unwrap_or_default();
    assert!(check.contains("Chrome flags"), "check label: {check}");
    assert!(
        fix.contains("fix:")
            && fix.contains("BLADE_CHROME_FLAGS")
            && !fix.contains("BLADE_INCOGNITO"),
        "fix hint: {fix}"
    );
}
