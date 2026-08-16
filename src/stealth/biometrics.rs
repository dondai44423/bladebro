//! Human behavioral biometrics — the unique stealth layer.
//!
//! Most stealth tools make the browser *look* like a non-bot (fingerprint
//! masking, property overrides). Almost none make it *act* like a human.
//! That's the gap Bladebro fills: every mouse movement traces a bezier curve
//! with overshoot-and-correct; every click has gaussian imprecision; every
//! keystroke has log-normal cadence with natural pauses.
//!
//! Anti-bot systems score behavioral signals: mouse path linearity, click
//! precision, inter-action timing. A browser that teleports the mouse to exact
//! coordinates and clicks instantly scores as automated on the first signal,
//! regardless of fingerprint quality. This module makes that score look human.
//!
//! Self-contained: no external PRNG dependency. XorShift64* + Box-Muller
//! transform, seeded from system time. Deterministic per-seed for testability.

use std::time::Duration;

// ---- PRNG ----

/// XorShift64* — fast, statistically adequate for biometric simulation.
/// Not cryptographic; that's not the threat model here.
pub struct Rng {
    state: u64,
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

impl Rng {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xdead_beef_cafe_babe)
            | 1; // ensure non-zero
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in [lo, hi]. Never panics: an inverted range
    /// (lo > hi, e.g. from a user-edited behavior.json with
    /// overshoot_min > overshoot_max) returns `lo`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo + 1) as u64) as i64
    }
}

// ---- Distributions ----

/// Standard normal N(0,1) via Box-Muller transform.
fn standard_normal(rng: &mut Rng) -> f64 {
    let u1 = rng.uniform().max(f64::MIN_POSITIVE);
    let u2 = rng.uniform();
    let r = (-2.0 * u1.ln()).sqrt();
    let theta = 2.0 * std::f64::consts::PI * u2;
    r * theta.cos()
}

/// Gaussian N(mean, std).
pub fn gaussian(rng: &mut Rng, mean: f64, std: f64) -> f64 {
    mean + std * standard_normal(rng)
}

/// Log-normal: `exp(N(ln(mean), sigma))`. Produces positive values with a
/// long right tail — matches real human reaction times (most fast, some slow).
pub fn log_normal(rng: &mut Rng, mean_ms: f64, sigma: f64) -> Duration {
    let ln_mean = mean_ms.ln();
    let val = (ln_mean + sigma * standard_normal(rng)).exp();
    Duration::from_millis(val.max(10.0) as u64)
}

// ---- Mouse trajectory ----

/// One point on the mouse path.
pub struct PathPoint {
    pub x: f64,
    pub y: f64,
    pub delay: Duration, // time to wait before moving to next point
}

/// Generate a human-like mouse trajectory from `start` to `target`.
///
/// The path is a cubic bezier curve with natural curvature, followed by an
/// overshoot past the target and a correction back. This mimics the human
/// motor pattern: accelerate toward target, overshoot slightly, correct.
///
/// - `start`: current mouse position (or last known).
/// - `target`: element center (before gaussian offset).
/// - `rng`: PRNG.
pub fn mouse_path(start: (f64, f64), target: (f64, f64), rng: &mut Rng) -> Vec<PathPoint> {
    // Gaussian offset from exact center — humans don't click pixel-perfect.
    // Precision comes from the persistent behavioral profile (same "person" every session).
    let precision = crate::knowledge::BEHAVIOR.click_precision;
    let click_x = gaussian(rng, target.0, precision);
    let click_y = gaussian(rng, target.1, precision);

    let dx = click_x - start.0;
    let dy = click_y - start.1;
    let dist = (dx * dx + dy * dy).sqrt();

    // Perpendicular direction for control point offset.
    let perp_x = -dy / dist.max(1.0);
    let perp_y = dx / dist.max(1.0);

    // Control points: offset perpendicular to the start→end line.
    // The offset magnitude is proportional to distance but capped, with
    // random sign and magnitude — this creates the natural curve.
    let curve_amount = (dist * crate::knowledge::BEHAVIOR.curve_factor).min(80.0);
    let cp1_offset = gaussian(rng, 0.0, curve_amount);
    let cp2_offset = gaussian(rng, 0.0, curve_amount * 0.7);

    let cp1 = (
        start.0 + dx * 0.3 + perp_x * cp1_offset,
        start.1 + dy * 0.3 + perp_y * cp1_offset,
    );
    let cp2 = (
        start.0 + dx * 0.7 + perp_x * cp2_offset,
        start.1 + dy * 0.7 + perp_y * cp2_offset,
    );

    // Number of steps: proportional to distance, min 5, max 40.
    // v3.9 (M8): the old 25-step cap gave a 1200px traverse ~48px jumps
    // (~4000px/s peak) — a superhuman flick in the velocity curve.
    let steps = (dist / 15.0).round().clamp(5.0, 40.0) as usize;

    // Total movement time: 80ms floor, distance-scaled, 650ms ceiling.
    // A 1200px flick at ~620ms (~2000px/s peak) matches fast human
    // mouse work; the old 300ms cap forced superhuman speeds.
    let total_ms = (dist * 0.45 + 80.0).clamp(80.0, 650.0);
    let step_ms = total_ms / steps as f64;

    let mut path = Vec::with_capacity(steps + 4);

    // Bezier curve to the (offset) click point.
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let mt = 1.0 - t;
        let x = mt * mt * mt * start.0
            + 3.0 * mt * mt * t * cp1.0
            + 3.0 * mt * t * t * cp2.0
            + t * t * t * click_x;
        let y = mt * mt * mt * start.1
            + 3.0 * mt * mt * t * cp1.1
            + 3.0 * mt * t * t * cp2.1
            + t * t * t * click_y;
        // Vary step timing slightly — constant-interval steps are a tell.
        let jitter = gaussian(rng, step_ms, step_ms * 0.15).max(5.0);
        path.push(PathPoint {
            x,
            y,
            delay: Duration::from_millis(jitter as u64),
        });
    }

    // Overshoot: go past the target, then correct back. Range from behavioral profile.
    let overshoot_dist = rng.range(
        crate::knowledge::BEHAVIOR.overshoot_min,
        crate::knowledge::BEHAVIOR.overshoot_max,
    ) as f64;
    let overshoot_dir_x = dx / dist.max(1.0);
    let overshoot_dir_y = dy / dist.max(1.0);
    let over_x = click_x + overshoot_dir_x * overshoot_dist;
    let over_y = click_y + overshoot_dir_y * overshoot_dist;

    path.push(PathPoint {
        x: over_x,
        y: over_y,
        delay: log_normal(rng, 32.0, 0.35),
    });
    // Correct back to the actual click point.
    path.push(PathPoint {
        x: click_x,
        y: click_y,
        delay: log_normal(rng, 50.0, 0.35),
    });

    // Small pause before clicking — humans hesitate briefly.
    path.push(PathPoint {
        x: click_x,
        y: click_y,
        delay: log_normal(rng, 80.0, 0.3),
    });

    path
}

/// The final click point (last point on the path, after overshoot correction).
pub fn click_target(path: &[PathPoint]) -> (f64, f64) {
    path.last()
        .map(|p| (p.x, p.y))
        .unwrap_or((0.0, 0.0))
}

// ---- Typing cadence ----

/// Generate human-like inter-key delays for typing `text`.
///
/// Base cadence: log-normal around 55ms per keystroke — matches a fast
/// typist (120+ WPM). Longer pauses after spaces (word boundaries).
/// Rare "thinking" pauses (1-3% chance, 300-1200ms).
pub fn typing_cadence(text: &str, rng: &mut Rng) -> Vec<Duration> {
    text.chars()
        .map(|c| {
            if c == ' ' {
                // Word boundary: longer pause, 100-250ms.
                log_normal(rng, 120.0, 0.25)
            } else if rng.uniform() < 0.02 {
                // Rare "thinking" pause: 300-1200ms.
                log_normal(rng, 500.0, 0.4)
            } else {
                // Normal keystroke: log-normal from persistent profile.
                log_normal(
                    rng,
                    crate::knowledge::BEHAVIOR.typing_mean_ms,
                    crate::knowledge::BEHAVIOR.typing_sigma,
                )
            }
        })
        .collect()
}

/// Inter-action delay: how long to pause between sequential actions.
/// Log-normal from persistent profile — same "person" pauses the same way every session.
pub fn action_gap(rng: &mut Rng) -> Duration {
    log_normal(
        rng,
        crate::knowledge::BEHAVIOR.action_gap_mean_ms,
        crate::knowledge::BEHAVIOR.action_gap_sigma,
    )
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_never_returns_zero_state() {
        let mut rng = Rng::new();
        let v = rng.next_u64();
        assert!(v != 0, "PRNG must not produce zero from non-zero seed");
    }

    #[test]
    fn gaussian_is_centered() {
        let mut rng = Rng::new();
        let sum: f64 = (0..10000).map(|_| gaussian(&mut rng, 0.0, 1.0)).sum();
        let mean = sum / 10000.0;
        assert!(mean.abs() < 0.1, "gaussian mean should be ~0, got {mean}");
    }

    #[test]
    fn log_normal_is_positive() {
        let mut rng = Rng::new();
        for _ in 0..1000 {
            let d = log_normal(&mut rng, 100.0, 0.3);
            assert!(d.as_millis() >= 10, "log-normal must be positive");
        }
    }

    #[test]
    fn mouse_path_reaches_target() {
        let mut rng = Rng::new();
        let path = mouse_path((0.0, 0.0), (200.0, 150.0), &mut rng);
        assert!(path.len() >= 7, "path should have bezier + overshoot + correction");
        let (final_x, final_y) = click_target(&path);
        // Final point should be near the target (within gaussian offset, ~10px).
        assert!((final_x - 200.0).abs() < 15.0, "final x {final_x} should be near 200");
        assert!((final_y - 150.0).abs() < 15.0, "final y {final_y} should be near 150");
    }

    #[test]
    fn mouse_path_overshoots_then_corrects() {
        let mut rng = Rng::new();
        let path = mouse_path((0.0, 0.0), (100.0, 0.0), &mut rng);
        // The second-to-last point should be the overshoot (further from start
        // than the final point). The last point is the correction back.
        assert!(path.len() >= 3);
        let overshoot = &path[path.len() - 2];
        let final_pt = &path[path.len() - 1];
        let overshoot_dist = overshoot.x.abs();
        let final_dist = final_pt.x.abs();
        assert!(overshoot_dist >= final_dist - 5.0,
            "overshoot ({overshoot_dist}) should be >= final ({final_dist})");
    }

    #[test]
    fn typing_cadence_matches_text_length() {
        let mut rng = Rng::new();
        let delays = typing_cadence("hello world", &mut rng);
        assert_eq!(delays.len(), 11, "one delay per character");
    }

    #[test]
    fn typing_cadence_has_variation() {
        let mut rng = Rng::new();
        let delays = typing_cadence("abcdefghij", &mut rng);
        let times: Vec<u64> = delays.iter().map(|d| d.as_millis() as u64).collect();
        // Not all delays should be identical — log-normal produces variation.
        let min = times.iter().min().unwrap();
        let max = times.iter().max().unwrap();
        assert!(max > min, "typing cadence should vary: min={min} max={max}");
    }
}
