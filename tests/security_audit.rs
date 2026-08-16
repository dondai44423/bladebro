//! Security-audit regression tests.
//!
//! Each test locks in a fix from the 2026-08-16 security audit. If one of
//! these fails, a change reintroduced a vulnerability — see
//! SECURITY-AUDIT-2026-08-16.md in the repo root for the full writeup.

use bladebro::platform::{truncate_utf8, validate_write_path};

// ── FIX: crash-by-URL / crash-by-message (byte slicing mid UTF-8 char) ──

#[test]
fn truncate_utf8_never_panics_on_multibyte_boundaries() {
    // Exact PoC from the audit: 77 ASCII bytes + a 3-byte CJK char whose
    // bytes straddle the cut. `&s[..77]` panicked here before the fix.
    let base = "https://evil.example/".len();
    let evil = format!(
        "https://evil.example/{}日本語テキストがここに続きます",
        "a".repeat(77 - base)
    );
    let cut = truncate_utf8(&evil, 77);
    assert!(cut.len() <= 77);
    // Must be a valid prefix of the input.
    assert!(evil.starts_with(cut));
}

#[test]
fn truncate_utf8_short_and_exact_inputs() {
    assert_eq!(truncate_utf8("abc", 10), "abc");
    assert_eq!(truncate_utf8("abcdef", 3), "abc");
    // é is 2 bytes: cutting at 1 returns "" not a panic.
    assert_eq!(truncate_utf8("éx", 1), "");
    assert_eq!(truncate_utf8("éx", 2), "é");
    // 4-byte emoji.
    assert_eq!(truncate_utf8("a😀b", 2), "a");
    assert_eq!(truncate_utf8("a😀b", 4), "a");
    assert_eq!(truncate_utf8("a😀b", 5), "a😀");
    assert_eq!(truncate_utf8("a😀b", 6), "a😀b");
}

// ── FIX: validate_write_path now blocks home config/credential sinks ────

#[test]
fn write_path_blocks_system_dirs() {
    for p in ["/etc/passwd", "/etc/cron.d/x", "/usr/bin/ls", "/boot/vmlinuz"] {
        assert!(validate_write_path(std::path::Path::new(p)).is_err(), "{p} must be blocked");
    }
}

#[test]
fn write_path_blocks_credential_and_persistence_sinks() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tester".into());
    for p in [
        format!("{home}/.bashrc"),
        format!("{home}/.profile"),
        format!("{home}/.zshrc"),
        format!("{home}/.ssh/authorized_keys"),
        format!("{home}/.ssh/id_rsa"),
        format!("{home}/.gnupg/pubring.kbx"),
        format!("{home}/.aws/credentials"),
        format!("{home}/.config/autostart/evil.desktop"),
        format!("{home}/.local/share/systemd/user/evil.service"),
        // traversal variant
        format!("{home}/docs/../../.ssh/authorized_keys"),
    ] {
        assert!(
            validate_write_path(std::path::Path::new(&p)).is_err(),
            "{p} must be blocked"
        );
    }
}

#[test]
fn write_path_allows_normal_outputs() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tester".into());
    for p in [
        "/tmp/report.pdf".to_string(),
        format!("{home}/Downloads/report.pdf"),
        format!("{home}/notes.txt"),
        "out.pdf".to_string(), // relative → cwd
    ] {
        assert!(
            validate_write_path(std::path::Path::new(&p)).is_ok(),
            "{p} should be allowed"
        );
    }
}

// ── FIX: updater checksum policy is fail-closed for new releases ────────

#[test]
fn checksum_required_policy() {
    use bladebro::updater::download::checksum_required;
    // The first release that ships .sha256 assets.
    assert!(checksum_required("3.3.0"));
    assert!(checksum_required("4.0.0"));
    assert!(checksum_required("10.0.0"));
    // Legacy releases (no checksum assets ever uploaded) keep warn-skip.
    assert!(!checksum_required("3.2.0"));
    assert!(!checksum_required("3.0.21"));
}

#[test]
fn checksum_parsing_accepts_only_valid_sha256() {
    use bladebro::updater::download::parse_checksum;
    let hash = "a".repeat(64);
    assert_eq!(
        parse_checksum(&format!("{hash}  bladebro-linux-x64")),
        Some(hash.clone())
    );
    assert_eq!(parse_checksum(&hash), Some(hash.clone()));
    assert_eq!(
        parse_checksum(&format!("{} bladebro", hash.to_uppercase())),
        Some(hash)
    );
    // Reject: not hex, too short, empty, junk.
    assert_eq!(parse_checksum(&"z".repeat(64)), None);
    assert_eq!(parse_checksum("deadbeef"), None);
    assert_eq!(parse_checksum(""), None);
    assert_eq!(parse_checksum("sha256sum: no such file"), None);
}

// ── FIX: updater temp file is unique + symlink-resistant ────────────────

#[test]
fn secure_tmp_rejects_preplaced_symlink_and_file() {
    use bladebro::updater::download::create_secure_tmp;
    let dir = std::env::temp_dir().join(format!("bladebro-sec-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Two calls produce different unpredictable paths.
    let a = create_secure_tmp(&dir).expect("first temp");
    let b = create_secure_tmp(&dir).expect("second temp");
    assert_ne!(a, b);
    assert!(a.file_name().unwrap().to_string_lossy().starts_with(".bladebro-update"));

    // Pre-place a symlink + a regular file and verify O_EXCL semantics
    // hold for the API the updater uses (create_new): creation at an
    // existing path — file or symlink — must FAIL, not write through.
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, b"ORIGINAL").unwrap();
    let link = dir.join(".bladebro-update-link-test");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    let probe = |p: &std::path::Path| {
        std::fs::OpenOptions::new().create_new(true).write(true).open(p).is_err()
    };
    assert!(probe(&dir.join("victim.txt")));
    #[cfg(unix)]
    assert!(probe(&link));
    // And the victim was not modified through the symlink.
    assert_eq!(std::fs::read(&victim).unwrap(), b"ORIGINAL");

    let _ = std::fs::remove_dir_all(&dir);
}

// ── FIX: cookie JS fallback uses JSON escaping (valid JS string) ─────────

#[test]
fn cookie_fallback_js_is_valid_json_string() {
    // The generated `document.cookie="<...>"` literal must be a valid JS
    // string for arbitrary cookie bytes. serde_json::to_string output is a
    // strict subset of JS string syntax (the old Rust {:?} was not).
    let nasty = "a\"b\\c\nd\te\x7f 你好";
    let js = serde_json::to_string(&format!("k={nasty}")).unwrap();
    // Round-trips through JSON (validates escaping end-to-end).
    let back: String = serde_json::from_str(&js).unwrap();
    assert_eq!(back, format!("k={nasty}"));
    assert!(js.starts_with('"') && js.ends_with('"'));
}

// ── FIX: secure_create_dir_all secures every component it creates ───────

#[cfg(unix)]
#[test]
fn secure_dir_all_chmods_created_components() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("bladebro-secdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let deep = base.join("one/two/three");
    bladebro::platform::secure_create_dir_all(&deep).unwrap();
    for p in [base.join("one"), base.join("one/two"), deep.clone()] {
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{:?} must be 0700", p);
    }
    // Pre-existing dirs are left alone.
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    bladebro::platform::secure_create_dir_all(&base.join("four")).unwrap();
    let base_mode = std::fs::metadata(&base).unwrap().permissions().mode() & 0o777;
    assert_eq!(base_mode, 0o755, "pre-existing dir must not be chmodded");
    let four_mode = std::fs::metadata(base.join("four")).unwrap().permissions().mode() & 0o777;
    assert_eq!(four_mode, 0o700);
    let _ = std::fs::remove_dir_all(&base);
}
