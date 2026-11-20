//! Atomic binary swap and verified rollback (cross-platform: Linux, macOS, Windows).

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::download;
use super::ui;
use super::version;
use crate::error::{BladeError, Result};

/// Maximum number of backups to keep. Old backups are pruned.
const MAX_BACKUPS: usize = 5;

/// Pre-flight writability check for the current binary's directory.
/// Called before downloading so permission errors fail instantaneously.
pub(crate) fn check_writable_current() -> Result<()> {
    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;
    check_writable(&current)
}

/// Remove a leftover Windows `*.exe.old` from an earlier in-place update
/// once the old process has exited and released its lock.
#[cfg(windows)]
pub(crate) fn sweep_windows_old(current: &Path) {
    let old_path = current.with_extension("exe.old");
    if old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }
}

#[cfg(not(windows))]
pub(crate) fn sweep_windows_old(_current: &Path) {}

/// Swap the current binary with a downloaded one.
///
/// 1. Pre-flight writability check.
/// 2. Back up the current binary FIRST (before touching `current`) to
///    `<data_dir>/backups/bladebro-v<CURRENT>` using O_EXCL on Unix.
/// 3. Atomically rename the downloaded binary into place (on Windows:
///    rename running exe aside to `.exe.old` first, then rename new binary
///    with brief retry for AV scanners, restoring on error).
/// 4. Set executable permissions (Unix) and prune old backups.
pub fn swap_binary(downloaded: &Path) -> Result<PathBuf> {
    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;

    check_writable(&current)?;
    sweep_windows_old(&current);

    let backup = write_backup(&current)?;

    #[cfg(windows)]
    {
        install_windows_exe(downloaded, &current)?;
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(downloaded, &current)
            .map_err(|e| BladeError::Other(format!("cannot install new binary: {e}")))?;
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

    let backup_dir = crate::platform::blade_dir().join("backups");
    prune_backups(&backup_dir);

    Ok(backup)
}

/// Copy `current` to `<data_dir>/backups/bladebro-v<CURRENT>` safely before
/// modifying the live executable.
fn write_backup(current: &Path) -> Result<PathBuf> {
    let backup_dir = crate::platform::blade_dir().join("backups");
    crate::platform::secure_create_dir_all(&backup_dir)
        .map_err(|e| BladeError::Other(format!("cannot create backup dir: {e}")))?;

    let backup = backup_dir.join(format!("bladebro-v{}", super::CURRENT_VERSION));

    // Remove old backup if it exists (same version). symlink_metadata, not
    // Path::exists() — exists() follows symlinks, so a pre-planted DANGLING
    // symlink at this predictable name skipped the remove and the write
    // below then went THROUGH the link to an attacker-chosen target.
    if let Ok(meta) = std::fs::symlink_metadata(&backup) {
        if !meta.is_dir() {
            std::fs::remove_file(&backup)
                .map_err(|e| BladeError::Other(format!("cannot remove old backup: {e}")))?;
        }
    }

    // O_EXCL copy: creation at an existing object (a raced-in symlink)
    // fails instead of writing through it.
    {
        let mut src = std::fs::File::open(current)
            .map_err(|e| BladeError::Other(format!("cannot open current binary: {e}")))?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
            .map_err(|e| BladeError::Other(format!("cannot save backup: {e}")))?;
        std::io::copy(&mut src, &mut dst)
            .map_err(|e| BladeError::Other(format!("cannot save backup: {e}")))?;
        use std::io::Write;
        dst.flush()
            .map_err(|e| BladeError::Other(format!("cannot save backup: {e}")))?;
        let _ = dst.sync_data();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(current) {
            let _ = std::fs::set_permissions(
                &backup,
                std::fs::Permissions::from_mode(m.permissions().mode() & 0o777),
            );
        }
    }
    Ok(backup)
}

/// On Windows, a running `.exe` cannot be overwritten directly, but it CAN be
/// renamed. Move `current` aside to `.exe.old`, then atomically rename
/// `staged` into place (with brief backoff for antivirus scanners), falling
/// back to copy and restoring `current` on failure.
#[cfg(windows)]
fn install_windows_exe(staged: &Path, current: &Path) -> Result<()> {
    let old_path = current.with_extension("exe.old");
    if old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }
    std::fs::rename(current, &old_path).map_err(|e| {
        BladeError::Other(format!(
            "cannot rename current exe (is another bladebro running?). Close it first. Error: {e}"
        ))
    })?;

    // Same directory → rename is instant & atomic. Retry briefly in case
    // Windows Defender briefly holds a scan handle on the newly written file.
    let mut installed = false;
    let mut last_err: Option<std::io::Error> = None;
    for delay_ms in [0u64, 100, 250, 500] {
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        match std::fs::rename(staged, current) {
            Ok(()) => {
                installed = true;
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }

    if !installed {
        if let Err(copy_err) = std::fs::copy(staged, current) {
            let _ = std::fs::rename(&old_path, current);
            let reason = last_err
                .map(|r| format!("rename: {r}; copy: {copy_err}"))
                .unwrap_or_else(|| copy_err.to_string());
            return Err(BladeError::Other(format!(
                "cannot install new binary: {reason}"
            )));
        }
        let _ = std::fs::remove_file(staged);
    }

    // Best-effort cleanup; Windows locks the running old exe until exit, so
    // `sweep_windows_old` removes it on the next update/rollback if busy now.
    let _ = std::fs::remove_file(&old_path);
    Ok(())
}

/// Check if the current binary's location is writable.
/// Returns a clear error with a fix if not.
fn check_writable(current: &Path) -> Result<()> {
    let parent = current.parent().unwrap_or(std::path::Path::new("."));

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let test_file = parent.join(format!(
        ".bladebro-write-test-{}-{nanos}",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&test_file)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&test_file);
            Ok(())
        }
        Err(e) => {
            let update_cmd = version::update_command_hint();
            let hint = match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    if version::is_package_managed() {
                        format!(
                            "This binary is managed by {}. Use: {update_cmd}\n  \
                             Or if you need self-update: sudo bladebro -u --force",
                            version::install_channel_label()
                        )
                    } else if cfg!(windows) {
                        "Permission denied. Run your terminal as Administrator, \
                         or update via npm: npm install -g bladebro"
                            .to_string()
                    } else {
                        "Permission denied. Try: sudo bladebro -u\n  \
                         Or update via npm: npm install -g bladebro"
                            .to_string()
                    }
                }
                std::io::ErrorKind::ReadOnlyFilesystem => "Filesystem is read-only. \
                     Cannot update in place. \
                     Use: npm install -g bladebro"
                    .to_string(),
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

    backups.retain(|e| e.file_name().to_string_lossy().starts_with("bladebro-v"));

    if backups.len() <= MAX_BACKUPS {
        return;
    }

    backups.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    backups.reverse();

    for entry in backups.into_iter().skip(MAX_BACKUPS) {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// List available backups (newest first).
fn list_backups(backup_dir: &Path) -> Vec<(PathBuf, String, u64)> {
    let entries = match std::fs::read_dir(backup_dir) {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    let mut backups: Vec<(PathBuf, String, u64)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("bladebro-v"))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            (e.path(), name, size)
        })
        .collect();

    backups.sort_by_key(|(path, _, _)| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    backups.reverse();
    backups
}

/// Rollback to the previous binary.
///
/// Finds the most recent verified backup in `<data_dir>/backups/`, stages it
/// securely in the install directory, confirms it starts (`--version`), and
/// atomically swaps it into place.
pub async fn rollback() -> Result<()> {
    let started = Instant::now();
    let meta = format!(
        "{} · {}",
        version::platform_label(),
        version::install_channel_label()
    );
    ui::masthead("Rollback", &meta);

    let backup_dir = crate::platform::blade_dir().join("backups");
    if !backup_dir.exists() {
        return Err(BladeError::Other(
            "no backups found in vault. Nothing to roll back to.".into(),
        ));
    }

    let backups = list_backups(&backup_dir);
    if backups.is_empty() {
        return Err(BladeError::Other(
            "no backups found in vault. Nothing to roll back to.".into(),
        ));
    }

    // Fail fast on permissions before staging anything.
    check_writable_current()?;

    // Display the Backup Vault.
    ui::section("Backup Vault");
    for (i, (_, name, size)) in backups.iter().take(MAX_BACKUPS).enumerate() {
        let size_str = ui::format_bytes(*size);
        let detail = if i == 0 {
            format!("{size_str}  ◀ active target")
        } else {
            size_str
        };
        let tone = if i == 0 {
            ui::Tone::Gold
        } else {
            ui::Tone::Smoke
        };
        ui::dossier_row(tone, name, &detail, None, 20);
    }

    ui::section("Restore");
    for (idx, (path, name, _)) in backups.iter().enumerate() {
        let spin = ui::Spinner::start(&format!("Verifying backup {name}..."));
        if let Err(e) = verify_backup(path) {
            spin.finish_warn(&format!("{name} failed static check ({e})"));
            if idx + 1 < backups.len() {
                continue;
            }
            return Err(BladeError::Other(format!(
                "no valid backups could be restored (last error: {e})\n  \
                 Reinstall: npm install -g bladebro"
            )));
        }
        spin.finish_ok(&format!(
            "Backup {name} passed ownership, magic & size checks"
        ));

        match do_rollback(path, name, started).await {
            Ok(()) => return Ok(()),
            Err(e) if idx + 1 < backups.len() => {
                ui::warn(&format!(
                    "{name} could not be restored ({e}); trying older backup..."
                ));
            }
            Err(e) => return Err(e),
        }
    }

    Err(BladeError::Other(
        "all backups are corrupted. Nothing to roll back to.\n  \
         Reinstall: npm install -g bladebro"
            .into(),
    ))
}

/// Stage a backup into a unique file inside `parent`, verify it executes,
/// and atomically swap it into `current`.
async fn do_rollback(backup_path: &Path, backup_name: &str, started: Instant) -> Result<()> {
    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;
    let parent = current.parent().unwrap_or(Path::new("."));
    sweep_windows_old(&current);

    let spin = ui::Spinner::start(&format!("Staging & smoke-testing {backup_name}..."));

    // Stage into a unique temp file in the SAME directory as `current` so the
    // final rename is atomic on Unix (zero window where `current` is absent)
    // and avoids predictable `.old` paths. On Windows, append `.exe` so
    // `verify_exe_runs` can spawn it before swap.
    let staged_base = download::create_secure_tmp(parent)?;
    let staged = if cfg!(windows) {
        let exe_staged = staged_base.with_extension("exe");
        let _ = std::fs::remove_file(&staged_base);
        exe_staged
    } else {
        staged_base
    };

    // Guard so any early error removes the staged file cleanly.
    struct StageGuard(PathBuf);
    impl Drop for StageGuard {
        fn drop(&mut self) {
            if self.0.exists() {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }
    let _guard = StageGuard(staged.clone());

    {
        let mut src = std::fs::File::open(backup_path)
            .map_err(|e| BladeError::Other(format!("cannot open backup: {e}")))?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&staged)
            .map_err(|e| BladeError::Other(format!("cannot create staging file: {e}")))?;
        std::io::copy(&mut src, &mut dst)
            .map_err(|e| BladeError::Other(format!("cannot stage backup: {e}")))?;
        use std::io::Write;
        dst.flush()
            .map_err(|e| BladeError::Other(format!("cannot flush staging file: {e}")))?;
        let _ = dst.sync_data();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| BladeError::Other(format!("cannot set executable permissions: {e}")))?;
    }

    // Confirm the staged backup actually starts on this host before replacing `current`.
    if let Err(e) = download::verify_exe_runs(&staged).await {
        spin.finish_fail(&format!("{backup_name} failed live startup check"));
        return Err(BladeError::Other(format!(
            "backup {backup_name} failed startup verification: {e}"
        )));
    }
    spin.finish_ok(&format!(
        "Backup {backup_name} starts cleanly (--version verified)"
    ));

    let swap_spin = ui::Spinner::start("Performing atomic binary swap...");
    #[cfg(windows)]
    {
        if let Err(e) = install_windows_exe(&staged, &current) {
            swap_spin.finish_fail("Atomic swap failed");
            return Err(e);
        }
    }

    #[cfg(not(windows))]
    {
        if let Err(e) = std::fs::rename(&staged, &current) {
            swap_spin.finish_fail("Atomic swap failed");
            return Err(BladeError::Other(format!("cannot restore backup: {e}")));
        }
    }
    swap_spin.finish_ok(&format!("Swapped {} atomically", current.display()));

    let daemon_stopped = crate::cli::stop_daemon_if_running().await;
    let mut rows = vec![
        ("restored", backup_name.to_string()),
        ("binary", current.display().to_string()),
    ];
    if daemon_stopped {
        rows.push((
            "daemon",
            "stopped stale daemon — next command boots on restored version".into(),
        ));
    }
    rows.push((
        "next",
        "Restart any running MCP client to pick up the restored binary".into(),
    ));

    ui::verdict_card(
        ui::Tone::Success,
        &format!("ROLLBACK COMPLETE · {backup_name}"),
        &ui::format_elapsed(started.elapsed()),
        &rows,
    );
    Ok(())
}

/// Verify a backup file is a valid binary (magic bytes + size) AND a
/// regular file owned by the current user.
fn verify_backup(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| BladeError::Other(format!("cannot stat backup: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(BladeError::Other(format!(
                "backup {} is a symlink — refusing it (possible local hijack)",
                path.display()
            )));
        }
        if !meta.is_file() {
            return Err(BladeError::Other(format!(
                "backup {} is not a regular file — refusing it",
                path.display()
            )));
        }
        if meta.uid() != unsafe { libc::getuid() } {
            return Err(BladeError::Other(format!(
                "backup {} is owned by uid {} (not this user) — refusing it (possible local hijack)",
                path.display(),
                meta.uid()
            )));
        }
    }
    let data =
        std::fs::read(path).map_err(|e| BladeError::Other(format!("cannot read backup: {e}")))?;

    if data.len() < 4 {
        return Err(BladeError::Other("backup file too small".into()));
    }

    let magic_ok = crate::updater::download::binary_magic_ok(&data);

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

#[cfg(test)]
mod backup_verify_tests {
    use super::*;

    fn platform_magic(data: &mut [u8]) {
        if cfg!(any(target_os = "linux", target_os = "android")) {
            data[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        } else if cfg!(target_os = "macos") {
            data[..4].copy_from_slice(&[0xCF, 0xFA, 0xED, 0xFE]);
        } else if cfg!(windows) {
            data[..2].copy_from_slice(b"MZ");
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_backup_refuses_symlinks_and_non_files() {
        let dir = std::env::temp_dir().join(format!("blade-swap-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        let mut data = vec![0u8; 1_000_100];
        platform_magic(&mut data);
        std::fs::write(&victim, &data).unwrap();
        let link = dir.join("bladebro-v9.9.9");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(
            verify_backup(&link).is_err(),
            "a symlinked backup must be refused"
        );
        assert!(verify_backup(&dir).is_err(), "a directory is not a backup");
        let real = dir.join("bladebro-v9.9.8");
        std::fs::write(&real, &data).unwrap();
        assert!(
            verify_backup(&real).is_ok(),
            "a plain real-magic file passes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_backup_accepts_platform_real_magic() {
        let dir = std::env::temp_dir().join(format!("blade-swap-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("backup");
        let mut data = vec![0u8; 1_000_100];
        platform_magic(&mut data);
        std::fs::write(&path, &data).unwrap();
        assert!(
            verify_backup(&path).is_ok(),
            "a real-artifact-magic backup must verify"
        );
        let mut bad = data.clone();
        bad[0] ^= 0xFF;
        std::fs::write(&path, &bad).unwrap();
        assert!(verify_backup(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
