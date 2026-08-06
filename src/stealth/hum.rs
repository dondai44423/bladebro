//! S4: Idle-hum — background mouse drift during agent think-time.
//!
//! A perfectly idle browser says "no human present." Real users fidget:
//! small mouse movements while reading, thinking, or deciding. The hum
//! thread dispatches occasional `mouseMoved` events during idle periods
//! (no action for >3s), generating the behavioral noise of presence.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::cdp::CdpSession;
use crate::stealth::biometrics::{log_normal, Rng};

/// Idle threshold in ms — start humming after this much inactivity.
const IDLE_THRESHOLD_MS: u64 = 3000;

/// Spawn the hum background task. Returns a handle that should be aborted
/// when the Page is dropped.
pub fn spawn_hum(
    cdp: CdpSession,
    last_action_epoch: Arc<AtomicU64>,
    is_busy: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        hum_loop(cdp, last_action_epoch, is_busy).await;
    })
}

async fn hum_loop(cdp: CdpSession, last_epoch: Arc<AtomicU64>, is_busy: Arc<AtomicBool>) {
    let mut rng = Rng::new();
    // Random-walk starting position — center of a typical viewport.
    let (mut mx, mut my) = (480.0_f64, 270.0_f64);
    // Cached viewport dims. Probing every cycle would put a metronome-
    // regular Runtime.evaluate on the wire — evaluation-timing is an L1
    // fingerprint surface. Probe rarely (first hum, then every ~12 cycles);
    // viewport size changes are rare mid-session.
    let (mut vw, mut vh) = (960.0_f64, 540.0_f64);
    let mut cycle: u32 = 0;

    loop {
        // Check interval — don't spin.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Pause during actions.
        if is_busy.load(Ordering::Relaxed) {
            continue;
        }

        // Check idle duration.
        let last = last_epoch.load(Ordering::Relaxed);
        if last == 0 {
            continue; // no action yet — silent startup
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let elapsed = now.saturating_sub(last);
        if elapsed < IDLE_THRESHOLD_MS {
            continue;
        }

        cycle = cycle.wrapping_add(1);
        if cycle == 1 || cycle % 12 == 0 {
            if let Some(dims) = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": "[window.innerWidth, window.innerHeight]",
                        "returnByValue": true,
                    })),
                )
                .await
                .ok()
                .and_then(|r| {
                    let arr = r.get("result")?.get("value")?.as_array()?;
                    Some((arr[0].as_f64()?, arr[1].as_f64()?))
                })
            {
                vw = dims.0;
                vh = dims.1;
            }
        }

        // Random walk: small jitter from current position. Guard the
        // clamp bounds: a tiny viewport (<40px) would make min>max and
        // panic-clamp, silently killing the hum task.
        let jx = rng.range(-12, 12) as f64;
        let jy = rng.range(-12, 12) as f64;
        let xmax = (vw - 20.0).max(21.0);
        let ymax = (vh - 20.0).max(21.0);
        mx = (mx + jx).clamp(20.0, xmax);
        my = (my + jy).clamp(20.0, ymax);

        let _ = cdp
            .send(
                "Input.dispatchMouseEvent",
                Some(json!({
                    "type": "mouseMoved",
                    "x": mx,
                    "y": my,
                })),
            )
            .await;

        // Next hum (log-normal from persistent profile — same fidgeting pattern every session).
        let interval = log_normal(&mut rng, crate::knowledge::BEHAVIOR.hum_interval_ms, 0.5);
        tokio::time::sleep(interval).await;
    }
}
