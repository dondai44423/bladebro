//! `BLADE_PROXY=http://user:pass@host:port` must never reach Chrome's argv.
//! Chrome's `--proxy-server` cannot carry credentials (every page load then
//! fails with ERR_NO_SUPPORTED_PROXIES while the launch "succeeds"), and a
//! command line is readable by every local user via the process list. The
//! launch must refuse such a value up front, with a redacted, actionable
//! reason, before any browser process exists.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

#[test]
fn credentialed_proxy_is_refused_before_chrome_sees_it() {
    let root = std::env::temp_dir().join(format!("blade-proxy-creds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let argv_log = root.join("argv.log");
    // Stand-in browser: records its command line, then dies at startup.
    let fake = root.join("fake-chrome");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n",
            argv_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bladebro"))
        .arg("probe")
        .env("BLADE_HOME", root.join("home"))
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env("BLADE_NO_WARMING", "1")
        .env("CHROME_PATH", &fake)
        .env("BLADE_PROXY", "http://user:secretpw@127.0.0.1:9")
        .env_remove("BLADE_CHROME_FLAGS")
        .env_remove("BLADE_TRANSPORT")
        .output()
        .unwrap();
    let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !argv.contains("secretpw"),
        "proxy credentials reached the browser command line: {argv}"
    );
    assert!(
        !output.status.success(),
        "a launch that cannot use its proxy must fail"
    );
    assert!(
        stderr.contains("credentials are not supported"),
        "the refusal must say why: {stderr}"
    );
    assert!(
        !stderr.contains("secretpw") && !stdout.contains("secretpw"),
        "the refusal must not echo the secret"
    );
}
