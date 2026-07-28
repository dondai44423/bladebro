//! Atomic binary swap and rollback.

use crate::error::{BladeError, Result};
use std::path::PathBuf;

/// Swap the current binary with a downloaded one.
///
/// 1. Back up the current binary to ~/.blade/backups/
/// 2. Move the downloaded binary into place
/// 3. Set executable permissions (Unix)
///
/// On Windows: renames the running exe first (running exes are locked
/// for overwrite but not for rename on Windows).
pub fn swap_binary(downloaded: &std::path::Path) -> Result<PathBuf> {
    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;

    // Backup the current binary.
    let backup_dir = crate::platform::blade_dir().join("backups");
    std::fs::create_dir_all(&backup_dir)
        .map_err(|e| BladeError::Other(format!("cannot create backup dir: {e}")))?;

    let backup = backup_dir.join(format!(
        "bladebro-v{}",
        super::CURRENT_VERSION
    ));

    // Remove old backup if it exists (same version).
    if backup.exists() {
        std::fs::remove_file(&backup)
            .map_err(|e| BladeError::Other(format!("cannot remove old backup: {e}")))?;
    }

    #[cfg(windows)]
    {
        // On Windows, we can't overwrite a running exe. But we CAN
        // rename it. So: rename current to .old, copy new in place.
        let old_path = current.with_extension("exe.old");
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
        std::fs::rename(&current, &old_path)
            .map_err(|e| BladeError::Other(format!(
                "cannot rename current exe (is bladebro running?). Close it first. Error: {e}"
            )))?;
        // Copy downloaded to current path.
        std::fs::copy(downloaded, &current)
            .map_err(|e| BladeError::Other(format!("cannot install new binary: {e}")))?;
        // Copy backup.
        std::fs::copy(&old_path, &backup)
            .map_err(|e| BladeError::Other(format!("cannot save backup: {e}")))?;
        // Clean up.
        let _ = std::fs::remove_file(downloaded);
        let _ = std::fs::remove_file(&old_path);
    }

    #[cfg(not(windows))]
    {
        // On Unix, we can atomically replace a running binary.
        // The kernel keeps the old inode alive until the process exits.
        std::fs::copy(&current, &backup)
            .map_err(|e| BladeError::Other(format!("cannot save backup: {e}")))?;
        std::fs::rename(downloaded, &current)
            .map_err(|e| BladeError::Other(format!("cannot install new binary: {e}")))?;
        // Set executable permission.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&current)
                .map_err(|e| BladeError::Other(format!("cannot read new binary: {e}")))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&current, perms)
                .map_err(|e| BladeError::Other(format!("cannot set permissions: {e}")))?;
        }
    }

    Ok(backup)
}

/// Rollback to the previous binary.
///
/// Finds the most recent backup in ~/.blade/backups/ and swaps it back.
pub async fn rollback() -> Result<()> {
    super::ui::header("Bladebro Rollback");

    let backup_dir = crate::platform::blade_dir().join("backups");
    if !backup_dir.exists() {
        return Err(BladeError::Other(
            "no backups found. Nothing to roll back to.".into(),
        ));
    }

    // Find the most recent backup.
    let mut backups: Vec<_> = std::fs::read_dir(&backup_dir)
        .map_err(|e| BladeError::Other(format!("cannot read backup dir: {e}")))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("bladebro-v")
        })
        .collect();

    if backups.is_empty() {
        return Err(BladeError::Other(
            "no backups found. Nothing to roll back to.".into(),
        ));
    }

    // Sort by modification time, most recent first.
    backups.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    backups.reverse();

    let backup = &backups[0];
    let backup_path = backup.path();
    let backup_name = backup.file_name().to_string_lossy().to_string();

    super::ui::info(&format!("Rolling back to {backup_name}"));

    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;

    #[cfg(windows)]
    {
        let old_path = current.with_extension("exe.old");
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
        std::fs::rename(&current, &old_path)
            .map_err(|e| BladeError::Other(format!(
                "cannot rename current exe (is bladebro running?). Close it first. Error: {e}"
            )))?;
        std::fs::copy(&backup_path, &current)
            .map_err(|e| BladeError::Other(format!("cannot restore backup: {e}")))?;
        let _ = std::fs::remove_file(&old_path);
    }

    #[cfg(not(windows))]
    {
        // On Linux/macOS, rename works on a running binary (the kernel
        // keeps the old inode alive until the process exits). But
        // fs::copy fails with ETXTBSY. So: rename current out of the
        // way, copy backup to current path, delete the old binary.
        let old_path = current.with_extension("old");
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
        std::fs::rename(&current, &old_path)
            .map_err(|e| BladeError::Other(format!("cannot move current binary: {e}")))?;
        std::fs::copy(&backup_path, &current)
            .map_err(|e| BladeError::Other(format!("cannot restore backup: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&current)
                .map_err(|e| BladeError::Other(format!("cannot read restored binary: {e}")))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&current, perms)
                .map_err(|e| BladeError::Other(format!("cannot set permissions: {e}")))?;
        }
        let _ = std::fs::remove_file(&old_path);
    }

    super::ui::success(&format!("Rolled back to {backup_name}"));
    super::ui::hint("Restart your MCP client to use the restored version.");
    Ok(())
}
