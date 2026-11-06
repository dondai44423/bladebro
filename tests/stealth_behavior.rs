//! Regression coverage for native call/argument/promise semantics. Browser
//! conformance and exact Chromium error messages live in audit + diff_oracle.
fn run(scenario: &str) -> std::process::Output {
    std::process::Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/stealth_behavior.cjs"
        ))
        .arg(scenario)
        .output()
        .expect("node is required for injected-script tests")
}

fn regression(scenario: &str) {
    let result = run(scenario);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

// A misspelled scenario name used to fall back to `permissions` and pass.
#[test]
fn unknown_scenario_fails_instead_of_running_a_default() {
    let result = run("no-such-scenario");
    assert!(!result.status.success(), "an unknown scenario must fail");
    assert!(String::from_utf8_lossy(&result.stderr).contains("unknown scenario"));
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

#[test]
fn noise_never_creates_a_canvas_context_and_audio_offset_is_idempotent() {
    regression("noise");
}

#[test]
fn console_capture_runs_no_page_code_and_is_bounded() {
    regression("console");
}
