//! Pause (manual control handover) + idle policy — whether the driver may
//! touch the browser at all right now, and whether idle may close it.

use super::*;
use crate::error::{BladeError, Result};
use crate::platform;
use std::path::PathBuf;

/// The single pause-refusal error, so every gate speaks identically.
pub fn paused_error() -> BladeError {
    BladeError::Other(
        "paused — manual control is claimed (`bladebro rb resume` to hand it back)".into(),
    )
}

/// `<data-dir>/realbrowser-pause` — while this file exists, input-
/// dispatching actions refuse to run and the idle hum stays silent, so the
/// user can use the browser without the agent fighting them.
pub fn pause_path() -> PathBuf {
    platform::blade_dir().join("realbrowser-pause")
}

/// True while manual control is claimed. Honored on every lane (harmless
/// on the agent lane; the point is the real one).
pub fn input_paused() -> bool {
    pause_path().exists()
}

pub fn set_paused(paused: bool) -> Result<()> {
    let path = pause_path();
    if paused {
        if let Some(parent) = path.parent() {
            platform::secure_create_dir_all(parent)
                .map_err(|e| BladeError::Other(format!("cannot create data dir: {e}")))?;
        }
        platform::secure_write_file(&path, b"paused")
            .map_err(|e| BladeError::Other(format!("cannot write pause marker: {e}")))?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// Whether the daemon/MCP idle timeout may close the lane's browser. On the
/// real lane the browser is the user's — default no, opt-in via config.
pub fn should_idle_shutdown() -> bool {
    if !real_lane() {
        return true;
    }
    config().idle_shutdown
}

/// Whether the idle-hum behavior layer runs on this lane. The pause marker
/// silences it on EVERY lane (the pause contract covers the agent lane too);
/// otherwise the agent lane always hums and the real lane follows its config.
pub fn hum_enabled() -> bool {
    if input_paused() {
        return false;
    }
    if !real_lane() {
        return true;
    }
    config().idle_hum
}
