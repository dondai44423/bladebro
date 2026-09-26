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

/// Read a slice of an artifact file — char-addressed (offset/limit in chars).
/// The read-back surface for pure-MCP clients with no filesystem access:
/// `see artifact="<path>"`. Restricted to the artifacts directory (this must
/// not become an arbitrary file reader) and to text-ish files; binary
/// artifacts (png/pdf) are refused with a pointer to the file.
pub fn read_artifact(path: &str, offset: usize, limit: usize) -> Result<String> {
    let dir = artifact_dir();
    let dir_canon = dir
        .canonicalize()
        .map_err(|e| crate::error::BladeError::Other(format!("artifact dir: {e}")))?;
    let canon = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| crate::error::BladeError::Other(format!("artifact not found: {path} ({e})")))?;
    if !canon.starts_with(&dir_canon) {
        return Err(crate::error::BladeError::Other(format!(
            "artifact read refused: {path} is outside the artifacts directory ({}) — use the CLI or any file tool for arbitrary paths",
            dir_canon.display()
        )));
    }
    let ext = canon
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "json" | "txt" | "log" | "csv" | "md" | "html") {
        let size = std::fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);
        return Err(crate::error::BladeError::Other(format!(
            "artifact is binary ({ext}, {size} bytes) — not text: {}",
            canon.display()
        )));
    }
    let bytes = std::fs::read(&canon)
        .map_err(|e| crate::error::BladeError::Other(format!("artifact read: {e}")))?;
    let text = String::from_utf8_lossy(&bytes);
    let total = text.chars().count();
    let limit = limit.clamp(1, 200_000);
    let offset = offset.min(total);
    let chunk: String = text.chars().skip(offset).take(limit).collect();
    let next = offset + chunk.chars().count();
    let tail = if next >= total {
        "end of artifact".to_string()
    } else {
        format!("read more with offset={next}")
    };
    Ok(format!(
        "artifact {} — chars {offset}..{next} of {total} ({tail})\n{chunk}",
        canon.display()
    ))
}

/// The artifact directory: `~/.blade/artifacts/`.
pub fn artifact_dir() -> std::path::PathBuf {
    crate::platform::blade_dir().join("artifacts")
}
