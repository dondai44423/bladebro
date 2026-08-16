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

/// Temp file prefix for download in progress. The full name carries the
/// pid + nanos so it is unpredictable: a fixed name in a shared install
/// dir (e.g. group-writable /usr/local/bin) let a local attacker pre-place
/// a symlink and have the "downloaded binary" written over an arbitrary
/// victim file — which the updater then EXECUTES during verification.
const TMP_PREFIX: &str = ".bladebro-update";

/// The first release tag whose assets MUST ship .sha256 checksums.
/// Targets at or above this version fail the update when no checksum can
/// be verified (fail-closed). Older tags predate checksum uploads, so they
/// keep the warn-and-skip behavior — otherwise `--force` downgrades to
/// them would become impossible.
const FIRST_CHECKSUMMED_TAG: &str = "3.3.0";

/// Is checksum verification mandatory for this target tag?
pub fn checksum_required(tag: &str) -> bool {
    super::version::compare_versions(tag, FIRST_CHECKSUMMED_TAG)
        != std::cmp::Ordering::Less
}

/// Parse a checksum file body: `<hash>  <filename>` or just `<hash>`.
/// Returns None when the body doesn't look like a valid SHA256 hex digest.
pub fn parse_checksum(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?.to_lowercase();
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash)
    } else {
        None
    }
}

/// SECURITY: Verify the SHA256 hash of a downloaded binary against a
/// checksum file from the same release.
///
/// Fail-closed policy: for release tags >= FIRST_CHECKSUMMED_TAG a missing,
/// unreachable, or malformed checksum file ABORTS the update. The old
/// warn-and-skip behavior turned the advertised "SHA256 verification"
/// into a no-op whenever an attacker (or an ordinary release without
/// checksum assets — none were uploaded before this fix) simply omitted
/// the .sha256 file, and the downloaded binary was then executed by
/// `verify_binary_runs` regardless.
async fn verify_sha256(binary_path: &std::path::Path, asset_url: &str, tag: &str) -> Result<()> {
    let checksum_url = format!("{asset_url}.sha256");
    let client = reqwest::Client::builder()
        .user_agent("bladebro-updater")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| BladeError::Other(format!("http client: {e}")))?;

    let required = checksum_required(tag);
    let fetch_failed = |why: &str| -> BladeError {
        BladeError::Other(format!(
            "cannot verify update integrity: {why}.\n\
             Release {tag} must ship a .sha256 checksum next to every binary.\n\
             The download was NOT installed. Update via npm instead:\n\
             npm install -g bladebro"
        ))
    };

    let resp = match client.get(&checksum_url).send().await {
        Ok(r) => r,
        Err(e) if required => return Err(fetch_failed(&format!(
            "checksum file unreachable ({e})"
        ))),
        Err(_) => {
            eprintln!("  warn: no checksum file found, skipping SHA256 verification (legacy release)");
            return Ok(());
        }
    };

    if resp.status() == reqwest::StatusCode::NOT_FOUND || !resp.status().is_success() {
        if required {
            return Err(fetch_failed(&format!(
                "no checksum file at {} (HTTP {})",
                checksum_url,
                resp.status()
            )));
        }
        eprintln!("  warn: no checksum file found, skipping SHA256 verification (legacy release)");
        return Ok(());
    }

    let checksum_text = resp
        .text()
        .await
        .map_err(|e| if required {
            fetch_failed(&format!("cannot read checksum: {e}"))
        } else {
            BladeError::Other(format!("cannot read checksum: {e}"))
        })?;

    let expected_hash = match parse_checksum(&checksum_text) {
        Some(h) => h,
        None if required => return Err(fetch_failed("malformed checksum file")),
        None => {
            eprintln!("  warn: invalid checksum file, skipping SHA256 verification (legacy release)");
            return Ok(());
        }
    };

    // Compute SHA256 of the downloaded binary.
    use sha2::{Sha256, Digest};
    let data = std::fs::read(binary_path)
        .map_err(|e| BladeError::Other(format!("cannot read binary for hash: {e}")))?;
    let actual_hash: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();

    if actual_hash != expected_hash {
        return Err(BladeError::Other(format!(
            "SHA256 mismatch! Expected {expected_hash}, got {actual_hash}.\n\
             The downloaded binary may be corrupted or tampered with.\n\
             Aborting update for safety."
        )));
    }

    eprintln!("  ok: SHA256 verified ({actual_hash})");
    Ok(())
}

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
    // Unique, unpredictable temp name + O_EXCL create: a predictable
    // fixed name allowed a local attacker to pre-place a symlink at the
    // temp path and have the downloaded binary written through it.
    let tmp = create_secure_tmp(dir)?;

    // Clean up any leftover temps from OUR previous failed attempts
    // (same pid prefix only — never touch other processes' files).
    if let Ok(entries) = std::fs::read_dir(dir) {
        let prefix = format!("{TMP_PREFIX}-{}-", std::process::id());
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".tmp") && e.path() != tmp {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    // Pre-flight: check available disk space if we know the asset size.
    if asset.size > 0 {
        if let Err(e) = check_disk_space(dir, asset.size) {
            let _ = std::fs::remove_file(&tmp);
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
                // SECURITY: Verify SHA256 before the binary is ever
                // executed or installed. Fail-closed for releases >=
                // FIRST_CHECKSUMMED_TAG; abort + clean up on failure.
                if let Err(e) = verify_sha256(&tmp, &asset.browser_download_url, release.tag()).await {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
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
    let _ = std::fs::remove_file(&tmp);
    Err(BladeError::Other(format!(
        "download failed after 3 attempts: {last_err}"
    )))
}

/// Create the download temp file: unique unpredictable name + O_EXCL, so a
/// pre-placed symlink or file at a guessable path makes creation FAIL
/// instead of being written through. Mode 0600 until verification promotes
/// the binary to 0755.
pub fn create_secure_tmp(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Retry on the (astronomically unlikely) name collision.
    for _ in 0..5 {
        let tmp = dir.join(format!(
            "{TMP_PREFIX}-{}-{nanos}-{}.tmp",
            std::process::id(),
            rand_suffix()
        ));
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&tmp) {
            Ok(_) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &tmp,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
                return Ok(tmp);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(BladeError::Other(format!("cannot create temp file: {e}")));
            }
        }
    }
    Err(BladeError::Other("cannot create temp file: name collisions".into()))
}

/// Cheap per-process randomness for temp-name uniqueness (no extra deps):
/// address entropy + a monotonic counter, XOR-folded.
fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    (std::process::id() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ t.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ n.wrapping_mul(0x1656_67B1_9E37_79F9)
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
