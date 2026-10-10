//! Browser GL health revalidation and self-heal for the long-lived lanes:
//! the launch-time GL verdict can go stale in BOTH directions — a
//! GPU-process restart makes Chrome serve null contexts (loss), and contexts
//! can come back (recovery), possibly on a different backend (a crashed
//! hardware GPU commonly returns as software). Tool calls re-check GL
//! liveness on a bounded cadence, update the recorded state, and REBUILD the
//! injection when a transition ADDS the mask — never to remove it (the live
//! probe reads through a page's own mask; a removal trigger measurably
//! stripped documents loaded after it) — so the mask covers what pages
//! actually see and no client restart is needed (the old
//! "restart the client" advice was wrong: a degraded GPU recovered on its
//! own). The one-shot advisory (browser::alerts) surfaces a loss transiently
//! on the same response. Wording reports what was OBSERVED (main-thread
//! null; worker evidence when it differs) — the cause (GPU crash, driver
//! reset, context limit) is explicitly unconfirmed; the driver never
//! fabricates GL on its own to improve a detector score.

use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cadence latch (process-global — GL is a per-browser property). Claims the
/// slot before probing so a degraded check cannot re-probe on every call.
static GL_RECHECK: Mutex<Option<Instant>> = Mutex::new(None);
const GL_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

impl Page {
    /// Cheap periodic GL liveness check (agent lane, at most once per
    /// minute), also driven by the serve loop's idle tick so recovery is
    /// noticed between tool calls. Runs while the recorded verdict claims GL
    /// (loss detection) AND while it reads Missing (recovery detection —
    /// G04: the Missing state used to be a dead end that could never come
    /// back; a GPU that returns must be picked up so the mask/state stop
    /// lying in either direction). A transition that ADDS the mask rebuilds
    /// the injection (S2 self-heal): a software fallback gains it, and a
    /// document loaded during a null-context window carries the fail-safe
    /// mask. The mask is never removed automatically — the live probe reads
    /// through the page's own mask (a masked page answers with the claimed
    /// GPU and classifies as hardware), so removal is the one direction that
    /// can strip stealth from documents loaded after it (observed live).
    pub async fn gl_health_refresh(&mut self) {
        use crate::browser::{
            gpu_state, probe_gl_live, reconcile_gl, set_gpu_state, GlLive, GlReconcile, GpuState,
            WorkerEvidence,
        };
        if crate::realbrowser::real_lane() {
            return;
        }
        let before = gpu_state();
        // An operator-forced decision (`BLADE_WEBGL`) is never rearmed
        // against — the refresh must not fight an explicit override.
        let forced = std::env::var_os("BLADE_WEBGL").is_some();
        {
            let Ok(mut last) = GL_RECHECK.lock() else {
                return;
            };
            let due = last
                .map(|t| t.elapsed() >= GL_RECHECK_INTERVAL)
                .unwrap_or(true);
            if !due {
                return;
            }
            *last = Some(Instant::now());
        }
        match probe_gl_live(&self.cdp).await {
            GlLive::Live(renderer) => {
                if let GlReconcile::Live {
                    spoof,
                    state: st,
                    changed,
                } = reconcile_gl(before.as_ref(), Some(&renderer))
                {
                    if changed {
                        if matches!(before, Some(GpuState::Missing)) {
                            eprintln!(
                                "[stealth] GL recovered: a WebGL context reports {renderer} again - state updated"
                            );
                        } else {
                            eprintln!("[stealth] GL reports {renderer} — state updated");
                        }
                        set_gpu_state(Some(st));
                    }
                    if gl_rearm_needed(spoof, crate::stealth::gl_spoofed(), forced) {
                        self.rearm_gl_injection(spoof).await;
                    }
                }
            }
            GlLive::NoContext { worker } => {
                if let GlReconcile::Dead { spoof, changed, .. } =
                    reconcile_gl(before.as_ref(), None)
                {
                    if changed {
                        // Report what was OBSERVED; the cause is explicitly
                        // unconfirmed — a GPU-process restart, a driver reset
                        // and a context-specific limit all look like this.
                        match &worker {
                            WorkerEvidence::Live(r) => eprintln!(
                                "[stealth] WARNING: main-thread WebGL now returns null on this browser, but a WORKER context still reports {r} - partial GL loss (cause unconfirmed: GPU-process restart, driver reset or context-specific limit). Main-thread pages see no WebGL and the GL mask no longer applies to them; nothing is fabricated, and the driver keeps re-probing until contexts return."
                            ),
                            WorkerEvidence::Null => eprintln!(
                                "[stealth] WARNING: WebGL context creation now returns null on this browser (main thread AND worker; cause unconfirmed - a GPU-process restart, driver reset or context limit all look like this). Pages see no WebGL while it lasts and nothing is fabricated; the driver keeps re-probing until contexts return."
                            ),
                            WorkerEvidence::Unavailable => eprintln!(
                                "[stealth] WARNING: WebGL context creation now returns null on this browser (observed live; cause unconfirmed - a GPU-process restart, driver reset or context limit all look like this). Pages see no WebGL while it lasts and nothing is fabricated; the driver keeps re-probing until contexts return."
                            ),
                        }
                        set_gpu_state(Some(GpuState::Missing));
                        crate::browser::mark_gl_mid_session_loss();
                    }
                    // Fail-safe: bake the spoof into the registration while
                    // contexts are absent, so a document loaded during the
                    // outage is masked the moment GL returns (the adds-only
                    // rearm then keeps it in place).
                    if gl_rearm_needed(spoof, crate::stealth::gl_spoofed(), forced) {
                        self.rearm_gl_injection(spoof).await;
                    }
                }
            }
            GlLive::Unknown => {}
        }
    }

    /// Rebuild the stealth injection so the GL mask matches the live backend
    /// (S2 self-heal). Mirrors the locale swap in `navigate`: replace the
    /// registration, never stack; `runImmediately` re-applies to the current
    /// document, so contexts created from now on are masked without a
    /// restart. Called only to ADD the mask (see `gl_rearm_needed`) — the
    /// decision is never reversed automatically, and an operator-forced
    /// `BLADE_WEBGL` is never overridden.
    async fn rearm_gl_injection(&mut self, spoof: bool) {
        eprintln!(
            "[stealth] GL transition: rebuilding the injection (mask {})",
            if spoof { "on" } else { "off" }
        );
        if let Some(id) = self.stealth_script_id.take() {
            let _ = self
                .cdp
                .send(
                    "Page.removeScriptToEvaluateOnNewDocument",
                    Some(serde_json::json!({ "identifier": id })),
                )
                .await;
        }
        match crate::stealth::apply_stealth(&self.cdp, self.active_locale.as_deref()).await {
            Ok(id) => self.stealth_script_id = Some(id),
            Err(e) => eprintln!("[stealth] WARNING: GL injection rebuild failed: {e}"),
        }
    }
}

/// Whether the registration must be rebuilt to re-arm the WebGL mask.
///
/// Only ever ADDS the mask (`spoof && !spoofed`, unless forced): an
/// automatic removal is unsound — the mid-session live probe reads through
/// the page's own mask (a masked page answers with the claimed GPU and
/// classifies as "hardware"), so a removal trigger misfires on every masked
/// session and strips the mask from documents loaded afterwards (measured:
/// an about:blank iframe read the raw software renderer). Leaving an
/// existing mask in place is the conservative direction; the recorded state
/// still updates honestly either way.
fn gl_rearm_needed(spoof: bool, spoofed: bool, forced: bool) -> bool {
    !forced && spoof && !spoofed
}

#[cfg(test)]
mod gl_rearm_tests {
    use super::gl_rearm_needed;

    #[test]
    fn rearm_only_ever_adds_the_mask() {
        // Software backend + unmasked registration: add the mask.
        assert!(gl_rearm_needed(true, false, false));
        // Masked registration stays masked: the probe can misread a masked
        // page as "hardware" — never rebuild in that direction.
        assert!(!gl_rearm_needed(false, true, false));
        // Already-matching states: nothing to do.
        assert!(!gl_rearm_needed(true, true, false));
        assert!(!gl_rearm_needed(false, false, false));
        // An operator-forced BLADE_WEBGL is never rearmed against.
        assert!(!gl_rearm_needed(true, false, true));
    }
}
