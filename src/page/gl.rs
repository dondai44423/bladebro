//! Browser GL health revalidation for the long-lived lanes: the launch-time
//! GL verdict can go stale (a GPU-process crash makes Chrome restart the GPU
//! with GL disabled — every context then returns null). Tool calls re-check
//! GL liveness on a bounded cadence and demote the recorded state; the
//! one-shot advisory (browser::alerts) surfaces the loss to the agent on the
//! same response instead of the lane serving a silently broken GL profile.

use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cadence latch (process-global — GL is a per-browser property). Claims the
/// slot before probing so a degraded check cannot re-probe on every call.
static GL_RECHECK: Mutex<Option<Instant>> = Mutex::new(None);
const GL_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

impl Page {
    /// Cheap periodic GL liveness check (agent lane, at most once per
    /// minute). No-op unless the recorded verdict claims GL exists.
    pub async fn gl_health_refresh(&self) {
        use crate::browser::{gpu_state, probe_gl_live, set_gpu_state, GlLive, GpuState};
        if crate::realbrowser::real_lane() {
            return;
        }
        if !matches!(
            gpu_state(),
            Some(GpuState::Software(_)) | Some(GpuState::Hardware(_))
        ) {
            return;
        }
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
                if let crate::browser::GlReconcile::Live {
                    state: st, changed, ..
                } = crate::browser::reconcile_gl(gpu_state().as_ref(), Some(&renderer))
                {
                    if changed {
                        eprintln!("[stealth] GL reports {renderer} — state updated");
                        set_gpu_state(Some(st));
                    }
                }
            }
            GlLive::NoContext => {
                eprintln!(
                    "[stealth] WARNING: WebGL context creation now returns null on this browser — \
                     the GPU process degraded mid-session (Chrome restarts it with GL disabled \
                     after a GPU crash). Pages will see no WebGL; no GL mask can apply until the \
                     browser is relaunched."
                );
                set_gpu_state(Some(GpuState::Missing));
                crate::browser::mark_gl_mid_session_loss();
            }
            GlLive::Unknown => {}
        }
    }
}
