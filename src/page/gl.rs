//! Browser GL health revalidation for the long-lived lanes: the launch-time
//! GL verdict can go stale in BOTH directions — a GPU-process restart makes
//! Chrome serve null contexts (loss), and contexts can come back (recovery).
//! Tool calls re-check GL liveness on a bounded cadence and update the
//! recorded state; the one-shot advisory (browser::alerts) surfaces
//! confirmed changes to the agent on the same response. Wording reports
//! what was OBSERVED (main-thread null; worker evidence when it differs) —
//! the cause (GPU crash, driver reset, context limit) is explicitly
//! unconfirmed; the driver never restarts a session or fabricates GL on
//! its own to improve a detector score.

use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cadence latch (process-global — GL is a per-browser property). Claims the
/// slot before probing so a degraded check cannot re-probe on every call.
static GL_RECHECK: Mutex<Option<Instant>> = Mutex::new(None);
const GL_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

impl Page {
    /// Cheap periodic GL liveness check (agent lane, at most once per
    /// minute). Runs while the recorded verdict claims GL (loss detection)
    /// AND while it reads Missing (recovery detection — G04: the Missing
    /// state used to be a dead end that could never come back; a GPU that
    /// returns must be picked up so the mask/state stop lying in either
    /// direction).
    pub async fn gl_health_refresh(&self) {
        use crate::browser::{
            gpu_state, probe_gl_live, reconcile_gl, set_gpu_state, GlLive, GlReconcile, GpuState,
            WorkerEvidence,
        };
        if crate::realbrowser::real_lane() {
            return;
        }
        let before = gpu_state();
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
                    state: st, changed, ..
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
                }
            }
            GlLive::NoContext { worker } => {
                if let GlReconcile::Dead { changed, .. } = reconcile_gl(before.as_ref(), None) {
                    if changed {
                        // Report what was OBSERVED; the cause is explicitly
                        // unconfirmed — a GPU-process restart, a driver reset
                        // and a context-specific limit all look like this.
                        match &worker {
                            WorkerEvidence::Live(r) => eprintln!(
                                "[stealth] WARNING: main-thread WebGL now returns null on this browser, but a WORKER context still reports {r} - partial GL loss (cause unconfirmed: GPU-process restart, driver reset or context-specific limit). Main-thread pages see no WebGL and the GL mask no longer applies to them; nothing is being fabricated. A browser restart reestablishes a clean GL state."
                            ),
                            WorkerEvidence::Null => eprintln!(
                                "[stealth] WARNING: WebGL context creation now returns null on this browser (main thread AND worker; cause unconfirmed - a GPU-process restart, driver reset or context limit all look like this). Pages will see no WebGL; no GL mask applies. A browser restart reestablishes a clean GL state."
                            ),
                            WorkerEvidence::Unavailable => eprintln!(
                                "[stealth] WARNING: WebGL context creation now returns null on this browser (observed live; cause unconfirmed - a GPU-process restart, driver reset or context limit all look like this). Pages will see no WebGL; no GL mask applies. A browser restart reestablishes a clean GL state."
                            ),
                        }
                        set_gpu_state(Some(GpuState::Missing));
                        crate::browser::mark_gl_mid_session_loss();
                    }
                }
            }
            GlLive::Unknown => {}
        }
    }
}
