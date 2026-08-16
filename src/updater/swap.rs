//! Atomic binary swap and rollback.

use crate::error::{BladeError, Result};
use std::path::{Path, PathBuf};

/// Maximum number of backups to keep. Old backups are pruned.
const MAX_BACKUPS: usize = 5;

/// Swap the current binary with a downloaded one.
///
/// 1. Back up the current binary to ~/.blade/backups/
/// 2. Move the downloaded binary into place
/// 3. Set executable permissions (Unix)
/// 4. Prune old backups (keep last MAX_BACKUPS)
///
/// On Windows: renames the running exe first (running exes are locked
/// for overwrite but not for rename on Windows).
pub fn swap_binary(downloaded: &Path) -> Result<PathBuf> {
    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;

    // Pre-flight: check if we can write to the current binary's location.
    check_writable(&current)?;

    // Backup the current binary.
    let backup_dir = crate::platform::blade_dir().join("backups");
    crate::platform::secure_create_dir_all(&backup_dir)
        .map_err(|e| BladeError::Other(format!("cannot create backup dir: {e}")))?;

    let backup = backup_dir.join(format!("bladebro-v{}", super::CURRENT_VERSION));

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
        std::fs::rename(&current, &old_path).map_err(|e| {
            BladeError::Other(format!(
                "cannot rename current exe (is bladebro running?). Close it first. Error: {e}"
            ))
        })?;
        std::fs::copy(downloaded, &current)
            .map_err(|e| BladeError::Other(format!("cannot install new binary: {e}")))?;
        std::fs::copy(&old_path, &backup)
            .map_err(|e| BladeError::Other(format!("cannot save backup: {e}")))?;
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

    // Prune old backups.
    prune_backups(&backup_dir);

    Ok(backup)
}

/// Check if the current binary's location is writable.
/// Returns a clear error with a fix if not.
fn check_writable(current: &Path) -> Result<()> {
    let parent = current
        .parent()
        .unwrap_or(std::path::Path::new("."));

    // Try creating a temp file in the same directory. Random suffix +
    // O_EXCL: a fixed name let a local attacker pre-place a symlink and
    // turn this probe into an arbitrary-file overwrite.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let test_file = parent.join(format!(".bladebro-write-test-{}-{nanos}", std::process::id()));
    match std::fs::OpenOptions::new().create_new(true).write(true).open(&test_file) {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Should not happen with a unique name; treat as writable-dir
            // evidence and move on.
            let _ = std::fs::remove_file(&test_file);
            Ok(())
        }
        Err(e) => {
            let method = super::version::install_method();
            let hint = match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    if method == "npm" {
                        "This binary was installed via npm. \
                         Use: npm update -g bladebro\n\
                         Or if you need self-update: \
                         sudo bladebro -u --force"
                            .to_string()
                    } else {
                        "Permission denied. Try: sudo bladebro -u\n\
                             Or update via npm: npm install -g bladebro".to_string()
                    }
                }
                std::io::ErrorKind::ReadOnlyFilesystem => {
                    "Filesystem is read-only. \
                     Cannot update in place. \
                     Use: npm install -g bladebro"
                        .to_string()
                }
                _ => format!("cannot write to {}: {e}", parent.display()),
            };
            Err(BladeError::Other(format!(
                "cannot update binary at {}\n  {hint}",
                current.display()
            )))
        }
    }
}

/// Prune old backups, keeping only the most recent MAX_BACKUPS.
fn prune_backups(backup_dir: &Path) {
    let mut backups: Vec<_> = match std::fs::read_dir(backup_dir) {
        Ok(dir) => dir.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };

    // Filter to bladebro-v* files.
    backups.retain(|e| {
        e.file_name()
            .to_string_lossy()
            .starts_with("bladebro-v")
    });

    if backups.len() <= MAX_BACKUPS {
        return;
    }

    // Sort by modification time, newest first.
    backups.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    backups.reverse();

    // Remove old backups beyond MAX_BACKUPS.
    for entry in backups.into_iter().skip(MAX_BACKUPS) {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// List available backups (for rollback and display).
fn list_backups(backup_dir: &Path) -> Vec<(PathBuf, String)> {
    let entries = match std::fs::read_dir(backup_dir) {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    let mut backups: Vec<(PathBuf, String)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("bladebro-v")
        })
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            (e.path(), name)
        })
        .collect();

    // Sort by modification time, newest first.
    backups.sort_by_key(|(path, _)| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    backups.reverse();
    backups
}

/// Rollback to the previous binary.
///
/// Finds the most recent backup in ~/.blade/backups/ and swaps it back.
/// Verifies the backup is a valid binary before restoring.
pub async fn rollback() -> Result<()> {
    super::ui::header("Bladebro Rollback");

    let backup_dir = crate::platform::blade_dir().join("backups");
    if !backup_dir.exists() {
        return Err(BladeError::Other(
            "no backups found. Nothing to roll back to.".into(),
        ));
    }

    let backups = list_backups(&backup_dir);
    if backups.is_empty() {
        return Err(BladeError::Other(
            "no backups found. Nothing to roll back to.".into(),
        ));
    }

    // Show available backups if more than one.
    if backups.len() > 1 {
        super::ui::info(&format!(
            "{} backup{} available:",
            backups.len(),
            if backups.len() == 1 { "" } else { "s" }
        ));
        for (i, (_, name)) in backups.iter().take(5).enumerate() {
            let marker = if i == 0 { " ← most recent" } else { "" };
            super::ui::hint(&format!("{name}{marker}"));
        }
    }

    let (backup_path, backup_name) = &backups[0];
    super::ui::info(&format!("Rolling back to {backup_name}"));

    // Verify the backup is a valid binary before restoring.
    if let Err(e) = verify_backup(backup_path) {
        // Try the next backup if the most recent one is corrupted.
        if backups.len() > 1 {
            super::ui::warn(&format!(
                "most recent backup is corrupted ({e}), trying next one..."
            ));
            for (path, name) in backups.iter().skip(1) {
                super::ui::info(&format!("Trying {name}..."));
                if verify_backup(path).is_ok() {
                    return do_rollback(path, name).await;
                }
            }
            return Err(BladeError::Other(
                "all backups are corrupted. Nothing to roll back to.\n\
                 Reinstall: npm install -g bladebro".into(),
            ));
        }
        return Err(BladeError::Other(format!(
            "backup is corrupted and cannot be restored: {e}\n\
             Reinstall: npm install -g bladebro"
        )));
    }

    do_rollback(backup_path, backup_name).await
}

async fn do_rollback(backup_path: &Path, backup_name: &str) -> Result<()> {
    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;

    #[cfg(windows)]
    {
        let old_path = current.with_extension("exe.old");
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
        std::fs::rename(&current, &old_path).map_err(|e| {
            BladeError::Other(format!(
                "cannot rename current exe (is bladebro running?). Close it first. Error: {e}"
            ))
        })?;
        std::fs::copy(backup_path, &current)
            .map_err(|e| BladeError::Other(format!("cannot restore backup: {e}")))?;
        let _ = std::fs::remove_file(&old_path);
    }

    #[cfg(not(windows))]
    {
        // On Unix, rename works on a running binary (the kernel
        // keeps the old inode alive until the process exits). But
        // fs::copy fails with ETXTBSY. So: rename current out of the
        // way, copy backup to current path, delete the old binary.
        let old_path = current.with_extension("old");
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
        std::fs::rename(&current, &old_path)
            .map_err(|e| BladeError::Other(format!("cannot move current binary: {e}")))?;
        std::fs::copy(backup_path, &current)
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

/// Verify a backup file is a valid binary (magic bytes + size).
fn verify_backup(path: &Path) -> Result<()> {
    let data = std::fs::read(path)
        .map_err(|e| BladeError::Other(format!("cannot read backup: {e}")))?;

    if data.len() < 4 {
        return Err(BladeError::Other("backup file too small".into()));
    }

    let magic_ok = if cfg!(target_os = "linux") {
        data[..4] == [0x7F, b'E', b'L', b'F']
    } else if cfg!(target_os = "macos") {
        let m = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        m == 0xFEEDFACE || m == 0xFEEDFACF || m == 0xCAFEBABE || m == 0xBEBAFECA
    } else if cfg!(windows) {
        data[..2] == *b"MZ"
    } else {
        true
    };

    if !magic_ok {
        return Err(BladeError::Other("invalid magic bytes".into()));
    }

    if data.len() < 1_000_000 {
        return Err(BladeError::Other(format!(
            "suspiciously small ({} bytes)",
            data.len()
        )));
    }

    Ok(())
}
