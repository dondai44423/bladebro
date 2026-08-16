//! Artifact offloading (v2, V10) — big structured output goes to
//! files, not the agent's context window.
//!
//! LLM agents read files natively. A 30KB extract dumped inline
//! costs ~8000 tokens of context; a file path + preview costs ~50.
//! Every large result (eval JSON, extract output, console logs)
//! routes through here: write the file, return the path + a small
//! preview + the total size.
//!
//! Files land in `~/.blade/artifacts/` named
//! `blade-<pid>-<seq>.<ext>` — the pid namespace means two
//! concurrent sessions can never overwrite each other's
//! artifacts (they used to share a bare sequence counter).
//! Rotation keeps the newest 300 files; older ones are
//! deleted on write.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Result;

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Max artifact files kept in the directory.
const MAX_ARTIFACTS: usize = 300;

/// Write `data` to an artifact file and return its absolute path.
pub fn write_artifact(data: &str, ext: &str) -> Result<String> {
    let dir = artifact_dir();
    crate::platform::secure_create_dir_all(&dir)
        .map_err(|e| crate::error::BladeError::Other(format!("artifact dir: {e}")))?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = dir.join(format!("blade-{pid}-{seq:04}.{ext}"));
    crate::platform::secure_write_file(&path, data.as_bytes())
        .map_err(|e| crate::error::BladeError::Other(format!("artifact write: {e}")))?;
    rotate_artifacts(&dir);
    Ok(path.display().to_string())
}

/// Write binary `data` to an artifact file and return its absolute path.
/// Used for PDF output (Page.printToPDF bytes) and completed downloads.
pub fn write_artifact_bytes(data: &[u8], ext: &str) -> Result<String> {
    let dir = artifact_dir();
    crate::platform::secure_create_dir_all(&dir)
        .map_err(|e| crate::error::BladeError::Other(format!("artifact dir: {e}")))?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = dir.join(format!("blade-{pid}-{seq:04}.{ext}"));
    crate::platform::secure_write_file(&path, data)
        .map_err(|e| crate::error::BladeError::Other(format!("artifact write: {e}")))?;
    rotate_artifacts(&dir);
    Ok(path.display().to_string())
}

/// Delete the oldest artifacts beyond MAX_ARTIFACTS.
/// Best-effort: any error is ignored (rotation is hygiene,
/// not correctness).
fn rotate_artifacts(dir: &std::path::Path) {
    let mut files: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("blade-")
            })
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((e.path(), modified))
            })
            .collect(),
        Err(_) => return,
    };
    if files.len() <= MAX_ARTIFACTS {
        return;
    }
    // Oldest first.
    files.sort_by_key(|(_, m)| *m);
    for (path, _) in files.iter().take(files.len() - MAX_ARTIFACTS) {
        let _ = std::fs::remove_file(path);
    }
}

/// The artifact directory: `~/.blade/artifacts/`.
pub fn artifact_dir() -> std::path::PathBuf {
    crate::platform::blade_dir().join("artifacts")
}
