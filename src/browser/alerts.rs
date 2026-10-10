//! One-shot degradation advisories for the agent lane: page-visible stealth
//! states that must surface to the agent exactly once — never silently
//! absorbed. Both long-lived lanes (the MCP server and the CLI daemon) drain
//! [`pending_notes`] once per tool response and prepend the notes to the
//! result. Claiming is process-global and one-shot; the GL note re-arms when
//! a fresh healthy GL verdict records (see `flags::set_gpu_state`).

use std::sync::atomic::{AtomicBool, Ordering};

/// One-shot latches, claimed by `pending_notes`.
static GL_NOTE: AtomicBool = AtomicBool::new(false);
static HEADLESS_NOTE: AtomicBool = AtomicBool::new(false);
static NO_SANDBOX_NOTE: AtomicBool = AtomicBool::new(false);
static PROXY_TZ_NOTE: AtomicBool = AtomicBool::new(false);
static INJECT_NOTE: AtomicBool = AtomicBool::new(false);
static UA_NOTE: AtomicBool = AtomicBool::new(false);

/// Failed stealth injection (page patches absent). Set by `Page::attach`.
static STEALTH_INJECT_FAILED: AtomicBool = AtomicBool::new(false);

/// Failed headless UA override (pages may see `HeadlessChrome`).
static UA_OVERRIDE_FAILED: AtomicBool = AtomicBool::new(false);

/// Record a failed stealth injection (`Page::attach`).
pub fn set_stealth_inject_failed() {
    STEALTH_INJECT_FAILED.store(true, Ordering::Relaxed);
}

/// Record a failed UA override (`Page::attach`, headless lane only).
pub fn set_ua_override_failed() {
    UA_OVERRIDE_FAILED.store(true, Ordering::Relaxed);
}

/// Re-arm the GL advisory after a fresh healthy GL verdict — a new browser
/// generation gets a fresh advisement cycle.
pub fn reset_gl_alert() {
    GL_NOTE.store(false, Ordering::Relaxed);
}

/// Claim `flag` once: true only for the first caller.
fn claim(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

/// `BLADE_PROXY` set but `BLADE_TZ` missing/empty: the page-visible timezone
/// cannot match the proxy's region. Pure for testing.
pub fn proxy_tz_mismatch(proxy: Option<&str>, tz: Option<&str>) -> bool {
    proxy.map(|p| !p.is_empty()).unwrap_or(false) && tz.map(|t| t.is_empty()).unwrap_or(true)
}

/// Drain the one-shot degradation advisories that currently apply.
pub fn pending_notes() -> Vec<String> {
    let mut notes = Vec::new();
    if !crate::realbrowser::real_lane() {
        if let Some(crate::browser::GpuState::Missing) = crate::browser::gpu_state() {
            if claim(&GL_NOTE) {
                notes.push(if crate::browser::gl_mid_session_loss() {
                    "note: this browser LOST WebGL mid-session (the GPU process degraded — getContext('webgl') returns null on every page right now). This is often transient: the driver keeps re-probing and restores the GL state (and its mask) automatically when contexts return. While it lasts, pages see no WebGL.".into()
                } else {
                    "note: this browser has no WebGL (getContext('webgl') returns null — the same as stock Chrome on this host); no GL mask is applied.".into()
                });
            }
        }
        if crate::browser::launched_headless() && claim(&HEADLESS_NOTE) {
            notes.push(
                "note: no Xvfb display was available — this browser fell back to --headless=new \
                 (reduced stealth). Install xvfb for headful-on-virtual-display."
                    .into(),
            );
        }
        if proxy_tz_mismatch(
            std::env::var("BLADE_PROXY").ok().as_deref(),
            std::env::var("BLADE_TZ").ok().as_deref(),
        ) && claim(&PROXY_TZ_NOTE)
        {
            notes.push(
                "note: BLADE_PROXY is set but BLADE_TZ is not — the page-visible timezone will not \
                 match the proxy's region. Set BLADE_TZ (e.g. `America/New_York`) to match the proxy."
                    .into(),
            );
        }
        if STEALTH_INJECT_FAILED.load(Ordering::Relaxed) && claim(&INJECT_NOTE) {
            notes.push(
                "note: the stealth injection failed to register on this browser (see server stderr) \
                 — pages may observe unpatched CDP artifacts."
                    .into(),
            );
        }
        if UA_OVERRIDE_FAILED.load(Ordering::Relaxed) && claim(&UA_NOTE) {
            notes.push(
                "note: the User-Agent override failed — this headless browser may still report \
                 'HeadlessChrome' to pages."
                    .into(),
            );
        }
    }
    if crate::browser::launched_no_sandbox() && claim(&NO_SANDBOX_NOTE) {
        notes.push(
            "note: Chrome is running with --no-sandbox (sandboxed startup failed — root or \
             restricted container); the renderer sandbox is OFF."
                .into(),
        );
    }
    notes
}

#[cfg(test)]
mod alert_tests {
    use super::*;

    /// One test drives the whole latch machine — the flags are process-wide,
    /// so splitting across tests would race the test harness.
    #[test]
    fn latches_are_one_shot_and_gl_rearms() {
        // Conditions: headless + no-sandbox → two notes in ONE drain (the
        // old single-slot advisory could drop one).
        crate::browser::set_launched_headless(true);
        crate::browser::set_launched_no_sandbox(true);
        let notes = pending_notes();
        assert_eq!(notes.len(), 2, "both degradations must surface: {notes:?}");
        assert!(notes.iter().any(|n| n.contains("--headless=new")));
        assert!(notes.iter().any(|n| n.contains("--no-sandbox")));
        // Claimed once — a second drain is empty.
        assert!(pending_notes().is_empty());
        crate::browser::set_launched_headless(false);
        crate::browser::set_launched_no_sandbox(false);

        // GL: a mid-session loss notes once; a healthy verdict re-arms it.
        crate::browser::set_gpu_state(Some(crate::browser::GpuState::Missing));
        crate::browser::mark_gl_mid_session_loss();
        let notes = pending_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("LOST WebGL mid-session"));
        assert!(pending_notes().is_empty());
        crate::browser::set_gpu_state(Some(crate::browser::GpuState::Software(
            "ANGLE (Mesa, llvmpipe (LLVM 21.1.7 256 bits), OpenGL 4.6)".into(),
        )));
        crate::browser::set_gpu_state(Some(crate::browser::GpuState::Missing));
        let notes = pending_notes();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("has no WebGL"),
            "at-launch wording: {notes:?}"
        );
        crate::browser::set_gpu_state(None);

        // Failed injection: notes once.
        set_stealth_inject_failed();
        let notes = pending_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("stealth injection failed"));
        assert!(pending_notes().is_empty());
    }

    #[test]
    fn proxy_tz_mismatch_is_precise() {
        assert!(proxy_tz_mismatch(Some("http://127.0.0.1:8080"), None));
        assert!(proxy_tz_mismatch(Some("http://127.0.0.1:8080"), Some("")));
        assert!(!proxy_tz_mismatch(
            Some("http://127.0.0.1:8080"),
            Some("UTC")
        ));
        assert!(!proxy_tz_mismatch(None, None));
        assert!(!proxy_tz_mismatch(Some(""), None));
    }
}
