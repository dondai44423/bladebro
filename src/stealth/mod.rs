//! The stealth module — three layers that make Bladebro hard to detect.
//!
//! 1. **Protocol-level**: don't send `Runtime.enable` (defuses DataDome console
//!    serialization detection). Already handled in `page::Page::attach`.
//! 2. **Injection**: `stealth::inject` — minimal self-cleaning script that
//!    removes CDP artifacts (`cdc_*` properties, `navigator.webdriver`).
//! 3. **Biometrics**: `stealth::biometrics` — human-like mouse trajectories
//!    (bezier + overshoot), typing cadence (log-normal), action timing.
//!
//! The unique angle: most stealth makes the browser *look* like a non-bot.
//! Bladebro also makes it *act* like a human. Behavioral signals (mouse
//! linearity, click precision, inter-action timing) are scored by anti-bot
//! systems and ignored by every other open-source tool.

pub mod biometrics;
pub mod hum;
pub mod inject;

pub use biometrics::{mouse_path, typing_cadence, action_gap, click_target, Rng};
pub use hum::spawn_hum;
pub use inject::STEALTH_SCRIPT_TEMPLATE;
pub use inject::{ScriptId, apply as apply_stealth, worker_gl_spoof, full_script, has_full_script};
