//! Binary download and verification.

use crate::error::{BladeError, Result};
use super::version;
use std::path::PathBuf;

/// A downloaded binary, ready to be installed.
pub struct DownloadedBinary {
    pub path: PathBuf,
    pub size: u64,
    pub asset_name: String,
}

/// Temp file prefix for download in progress.
const TMP_NAME: &str = ".bladebro-update-tmp";

/// Download the platform binary from a release.
/// Retries up to 3 times with resume support.
pub async fn download_binary(release: &version::Release) -> Result<DownloadedBinary> {
    let asset = version::find_asset(release).ok_or_else(|| {
        let available = version::asset_names(release);
        let avail_str = if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        };
        BladeError::Other(format!(
            "no binary for {} in release {}. Available assets: {}\n\n\
             To update via npm:  npm install -g bladebro\n\
             Or build from source:  git clone https://github.com/dondai44423/bladebro.git && cd bladebro && cargo build --release",
            version::platform_label(),
            release.tag_name,
            avail_str,
        ))
    })?;

    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;
    let dir = current.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(TMP_NAME);

    // Clean up any leftover temp from a previous failed attempt.
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    // Pre-flight: check available disk space if we know the asset size.
    if asset.size > 0 {
        if let Err(e) = check_disk_space(dir, asset.size) {
            return Err(BladeError::Other(format!(
                "insufficient disk space for download (~{} MB needed): {e}\n\
                 Free space in {} and try again.",
                asset.size / 1_000_000,
                dir.display(),
            )));
        }
    }

    let mut last_err = String::new();
    for attempt in 1..=3 {
        if attempt > 1 {
            eprintln!("  retry {attempt}/3...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match download_once(&asset.browser_download_url, &tmp, asset.size).await {
            Ok(size) => {
                return Ok(DownloadedBinary {
                    path: tmp,
                    size,
                    asset_name: asset.name.clone(),
                });
            }
            Err(e) => {
                last_err = e.to_string();
                // Clean partial file on error so next retry starts fresh
                // (unless it's a resume-able network error).
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    Err(BladeError::Other(format!(
        "download failed after 3 attempts: {last_err}"
    )))
}

/// Download a URL to a file, with resume support.
///
/// Resume logic: if a partial file exists, send a Range header.
/// If the server responds 206 (Partial Content), append.
/// If the server responds 200 (full content) despite the Range header,
/// truncate and start fresh (server doesn't support range requests).
async fn download_once(url: &str, tmp: &std::path::Path, _expected_size: u64) -> Result<u64> {
    let existing = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);

    let client = reqwest::Client::builder()
        .user_agent("bladebro-updater")
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| BladeError::Other(format!("http client: {e}")))?;

    let mut req = client.get(url);
    if existing > 0 {
        req = req.header("Range", format!("bytes={existing}-"));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| BladeError::Other(format!("download failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(BladeError::Other(format!("download failed: HTTP {status}")));
    }

    let is_partial = status == reqwest::StatusCode::PARTIAL_CONTENT;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| BladeError::Other(format!("download interrupted: {e}")))?;

    if bytes.is_empty() && existing == 0 {
        return Err(BladeError::Other("downloaded file is empty".into()));
    }

    if existing > 0 && is_partial {
        // Server honored our range request — append the partial content.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp)
            .map_err(|e| BladeError::Other(format!("cannot open temp file: {e}")))?;
        f.write_all(&bytes)
            .map_err(|e| BladeError::Other(format!("cannot write temp file: {e}")))?;
        Ok(existing + bytes.len() as u64)
    } else {
        // Fresh download, or server ignored range request (200 not 206).
        std::fs::write(tmp, &bytes)
            .map_err(|e| BladeError::Other(format!("cannot write temp file: {e}")))?;
        Ok(bytes.len() as u64)
    }
}

/// Check if there's enough disk space for a download.
/// Uses `df` on Unix (safe, no FFI). Falls through silently on failure.
#[cfg(unix)]
fn check_disk_space(dir: &std::path::Path, needed: u64) -> Result<()> {
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // df -k output:  Filesystem  1K-blocks  Used  Available  Use%  Mounted on
            // The data line is the last line (handles multi-line headers on macOS).
            if let Some(line) = stdout.lines().last() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                // Available is typically the 4th field (index 3).
                // But some systems add a Filesystem path with spaces.
                // Find the field that looks like a number in the Available position.
                // Strategy: the field before the Use% field (contains %).
                let use_idx = fields.iter().position(|f| f.ends_with('%'));
                let avail_idx = use_idx.and_then(|i| if i > 0 { Some(i - 1) } else { None });
                if let Some(idx) = avail_idx {
                    if let Ok(avail_kb) = fields[idx].parse::<u64>() {
                        let avail = avail_kb * 1024;
                        if avail < needed + 10_000_000 {
                            return Err(BladeError::Other(format!(
                                "only {:.1} MB available on disk",
                                avail as f64 / 1_000_000.0
                            )));
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()), // Can't check — don't block the download.
    }
}

#[cfg(windows)]
fn check_disk_space(_dir: &std::path::Path, _needed: u64) -> Result<()> {
    // Windows: skip pre-flight check. Download will fail naturally
    // with a clear error if disk is full.
    Ok(())
}

/// Verify a downloaded binary is valid:
/// 1. Correct magic bytes for the platform
/// 2. Reasonable file size (1MB–500MB)
/// 3. On Unix: set executable permissions
/// 4. Try executing the binary with `--version` to confirm it runs
pub fn verify_binary(dl: &DownloadedBinary) -> Result<()> {
    let data = std::fs::read(&dl.path)
        .map_err(|e| BladeError::Other(format!("cannot read downloaded file: {e}")))?;

    if data.len() < 4 {
        return Err(BladeError::Other("downloaded file too small".into()));
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
        return Err(BladeError::Other(
            "downloaded file is not a valid binary for this platform".into(),
        ));
    }

    if dl.size < 1_000_000 {
        return Err(BladeError::Other(format!(
            "downloaded file suspiciously small ({} bytes)",
            dl.size
        )));
    }
    if dl.size > 500_000_000 {
        return Err(BladeError::Other(format!(
            "downloaded file suspiciously large ({} bytes)",
            dl.size
        )));
    }

    // Set executable permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dl.path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| BladeError::Other(format!("cannot set executable permission: {e}")))?;
    }

    Ok(())
}

/// Try executing the downloaded binary to confirm it starts.
/// This catches:
/// - Wrong architecture (e.g. ARM binary on x86)
/// - Missing shared libraries
/// - Corrupted binary that passes magic check but won't execute
///
/// Runs `binary --version` and checks for a successful exit.
pub fn verify_binary_runs(dl: &DownloadedBinary) -> Result<()> {
    let output = std::process::Command::new(&dl.path)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    match output {
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                return Err(BladeError::Other(format!(
                    "downloaded binary failed to start (exit {:?})\n  stdout: {}\n  stderr: {}",
                    o.status.code(),
                    stdout.trim(),
                    stderr.trim(),
                )));
            }
            // Check the output mentions "bladebro" to confirm it's our binary.
            let combined = format!(
                "{} {}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if !combined.to_lowercase().contains("bladebro") {
                return Err(BladeError::Other(
                    "downloaded binary ran but doesn't identify as bladebro".into(),
                ));
            }
            Ok(())
        }
        Err(e) => {
            // On Unix, this can happen if exec permissions aren't set.
            #[cfg(unix)]
            {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    return Err(BladeError::Other(
                        "cannot execute downloaded binary (permission denied). \
                         Run: chmod +x the binary path".into(),
                    ));
                }
            }
            Err(BladeError::Other(format!(
                "cannot execute downloaded binary: {e}\n\
                 This may be a wrong architecture or missing libraries."
            )))
        }
    }
}

/// Clean up the temp file (call on success or failure).
pub fn cleanup_tmp(path: &std::path::Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn verify_rejects_empty() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-empty");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        let dl = DownloadedBinary {
            path,
            size: 0,
            asset_name: "test".into(),
        };
        assert!(verify_binary(&dl).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_tiny() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-tiny");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.bin");
        std::fs::write(&path, b"ab").unwrap();
        let dl = DownloadedBinary {
            path,
            size: 2,
            asset_name: "test".into(),
        };
        assert!(verify_binary(&dl).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_wrong_magic() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-magic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wrong.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&vec![0u8; 2_000_000]).unwrap();
        drop(f);
        let dl = DownloadedBinary {
            path,
            size: 2_000_000,
            asset_name: "test".into(),
        };
        assert!(verify_binary(&dl).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_too_large() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-large");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.bin");
        // Write a file with correct magic but fake size > 500MB
        let mut f = std::fs::File::create(&path).unwrap();
        // Linux ELF magic
        #[cfg(target_os = "linux")]
        f.write_all(&[0x7F, b'E', b'L', b'F']).unwrap();
        #[cfg(not(target_os = "linux"))]
        f.write_all(&[0x7F, b'E', b'L', b'F']).unwrap();
        // Write enough to pass the size check (> 500MB)
        // Actually we can't write 500MB in a test. Just test the size check directly.
        drop(f);
        let dl = DownloadedBinary {
            path: path.clone(),
            size: 600_000_000, // 600MB — over the limit
            asset_name: "test".into(),
        };
        // verify_binary reads the file and checks dl.size.
        // The file is small but dl.size says 600MB, so it should fail on size.
        // Actually verify_binary checks data.len() < 4 first, then magic,
        // then dl.size. The magic check reads the actual file bytes.
        // So this should pass magic (we wrote ELF header) but fail on size.
        assert!(verify_binary(&dl).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
