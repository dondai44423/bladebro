//! Regression coverage for native call/argument/promise semantics. Browser
//! conformance and exact Chromium error messages live in audit + diff_oracle.
fn regression(scenario: &str) {
    let result = std::process::Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/stealth_behavior.cjs"
        ))
        .arg(scenario)
        .output()
        .expect("node is required for injected-script tests");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn permissions_preserve_native_arguments_promises_and_receiver_errors() {
    regression("permissions");
}

#[test]
fn media_preserves_native_rejection_timing_and_real_devices() {
    regression("media");
}

#[test]
fn rtc_icecandidate_listeners_keep_native_dedupe_and_removal() {
    regression("rtc-listeners");
}

#[test]
fn rtc_sdp_filter_removes_whole_lines_and_remote_candidates_stay_native() {
    regression("rtc-sdp");
}
