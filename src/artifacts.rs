//! Artifact offloading (v2, V10) — big structured output goes to
//! files, not the agent's context window.
//!
//! LLM agents read files natively. A 30KB extract dumped inline
//! costs ~8000 tokens of context; a file path + preview costs ~50.
//! Every large result (eval JSON, extract output, console logs)
//! routes through here: write the file, return the path + a small
//! preview + the total size.
//!
//! Files land in `~/.blade/artifacts/` with a monotonically
//! increasing sequence number so agents can glob/sort.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Result;

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Write `data` to an artifact file and return its absolute path.
pub fn write_artifact(data: &str, ext: &str) -> Result<String> {
    let dir = artifact_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::error::BladeError::Other(format!("artifact dir: {e}")))?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("blade-{seq:04}.{ext}"));
    std::fs::write(&path, data)
        .map_err(|e| crate::error::BladeError::Other(format!("artifact write: {e}")))?;
    Ok(path.display().to_string())
}

/// The artifact directory: `~/.blade/artifacts/`.
pub fn artifact_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".blade").join("artifacts")
}
