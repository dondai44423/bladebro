//! Binary download and verification.

use super::version;
use crate::error::{BladeError, Result};
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
const MAX_BINARY_SIZE: u64 = 500_000_000;

/// Is checksum verification mandatory for this target tag?
pub fn checksum_required(tag: &str) -> bool {
    super::version::compare_versions(tag, FIRST_CHECKSUMMED_TAG) != std::cmp::Ordering::Less
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
        Err(e) if required => {
            return Err(fetch_failed(&format!("checksum file unreachable ({e})")))
        }
        Err(_) => {
            eprintln!(
                "  warn: no checksum file found, skipping SHA256 verification (legacy release)"
            );
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

    let checksum_text = resp.text().await.map_err(|e| {
        if required {
            fetch_failed(&format!("cannot read checksum: {e}"))
        } else {
            BladeError::Other(format!("cannot read checksum: {e}"))
        }
    })?;

    let expected_hash = match parse_checksum(&checksum_text) {
        Some(h) => h,
        None if required => return Err(fetch_failed("malformed checksum file")),
        None => {
            eprintln!(
                "  warn: invalid checksum file, skipping SHA256 verification (legacy release)"
            );
            return Ok(());
        }
    };

    // Compute SHA256 of the downloaded binary.
    use sha2::{Digest, Sha256};
    let data = std::fs::read(binary_path)
        .map_err(|e| BladeError::Other(format!("cannot read binary for hash: {e}")))?;
    let actual_hash: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

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
                if let Err(e) =
                    verify_sha256(&tmp, &asset.browser_download_url, release.tag()).await
                {
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
                // KEEP the partial file: the next attempt resumes from it
                // with a Range request. Deleting it here (as this used to)
                // made the advertised resume support dead code — every
                // retry was a full re-download. SHA256 verification still
                // gates whatever a resumed download produces.
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
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
        {
            Ok(_) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
                }
                return Ok(tmp);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(BladeError::Other(format!("cannot create temp file: {e}")));
            }
        }
    }
    Err(BladeError::Other(
        "cannot create temp file: name collisions".into(),
    ))
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
    (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ t.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ n.wrapping_mul(0x1656_67B1_9E37_79F9)
}

/// Download a URL to a file, with resume support.
///
/// Resume logic: if a partial file exists, send a Range header.
/// If the server responds 206 (Partial Content), append.
/// If the server responds 200 (full content) despite the Range header,
/// truncate and start fresh (server doesn't support range requests).
async fn download_once(url: &str, tmp: &std::path::Path, expected_size: u64) -> Result<u64> {
    use std::io::Write;
    // The post-download verifier has the same ceiling. Enforce it while
    // reading, before a bad response can fill memory or the install disk.
    let limit = if expected_size == 0 {
        MAX_BINARY_SIZE
    } else {
        expected_size
    };
    if limit > MAX_BINARY_SIZE {
        return Err(BladeError::Other("download size exceeds 500 MB".into()));
    }
    let existing = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
    if existing > limit {
        return Err(BladeError::Other(
            "partial download exceeds expected size".into(),
        ));
    }
    // A connection can fail after all bytes reached disk. SHA256 still gates
    // installation; requesting an empty suffix would only produce HTTP 416.
    if expected_size > 0 && existing == expected_size {
        return Ok(existing);
    }
    let client = reqwest::Client::builder()
        .user_agent("bladebro-updater")
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| BladeError::Other(format!("http client: {e}")))?;
    let mut req = client.get(url);
    if existing > 0 {
        req = req.header("Range", format!("bytes={existing}-"));
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| BladeError::Other(format!("download failed: {e}")))?;
    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if resp.status() != reqwest::StatusCode::OK && !partial {
        return Err(BladeError::Other(format!(
            "download failed: HTTP {}",
            resp.status()
        )));
    }
    let offset = if partial { existing } else { 0 };
    let mut target_size = expected_size;
    let mut response_limit = limit;
    if partial {
        let range = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("bytes "))
            .and_then(|h| h.split_once('/'))
            .and_then(|(range, total)| {
                let (start, end) = range.split_once('-')?;
                Some((
                    start.parse::<u64>().ok()?,
                    end.parse::<u64>().ok()?,
                    total.parse::<u64>().ok()?,
                ))
            });
        match range {
            Some((start, end, total))
                if start == existing
                    && start <= end
                    && end < total
                    && total <= limit
                    && (expected_size == 0 || total == expected_size)
                    && resp
                        .content_length()
                        .is_none_or(|len| len == end - start + 1) =>
            {
                target_size = total;
                response_limit = end + 1;
            }
            _ => {
                return Err(BladeError::Other(
                    "download returned an invalid content range (Content-Range)".into(),
                ))
            }
        }
    }
    if resp
        .content_length()
        .is_some_and(|len| len > response_limit - offset)
    {
        return Err(BladeError::Other("download exceeds expected size".into()));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .append(partial)
        .truncate(!partial)
        .open(tmp)
        .map_err(|e| BladeError::Other(format!("cannot open temp file: {e}")))?;
    let mut size = offset;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| BladeError::Other(format!("download interrupted: {e}")))?
    {
        if chunk.len() as u64 > response_limit - size {
            return Err(BladeError::Other("download exceeds expected size".into()));
        }
        file.write_all(&chunk)
            .map_err(|e| BladeError::Other(format!("cannot write temp file: {e}")))?;
        size += chunk.len() as u64;
    }
    if size == 0 || (target_size > 0 && size != target_size) {
        return Err(BladeError::Other(format!(
            "incomplete download: {size} bytes, expected {target_size}"
        )));
    }
    Ok(size)
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

/// Mach-O / fat-archive magic, accepting EVERY standard spelling. Modern
/// macOS builds are little-endian 64-bit Mach-O — file bytes `cf fa ed fe`
/// (MH_CIGAM_64), which is what every darwin asset in this repo starts
/// with. The pre-fix check compared a big-endian u32 against only the
/// big-endian spellings, so it rejected every real darwin binary: macOS
/// `bladebro -u` and `--rollback` failed with "not a valid binary for this
/// platform". Split from `binary_magic_ok` so the bytes stay unit-testable
/// on every OS.
pub(crate) fn macho_magic_ok(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let m = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    m == 0xFEEDFACE || m == 0xCEFAEDFE // 32-bit, big/little endian
        || m == 0xFEEDFACF || m == 0xCFFAEDFE // 64-bit, big/little endian
        || m == 0xCAFEBABE || m == 0xBEBAFECA // fat archives
}

/// Platform magic-byte check for a candidate binary. Single definition,
/// shared by `verify_binary` and the rollback's `verify_backup` — the two
/// used to carry duplicated (and equally wrong) copies.
pub(crate) fn binary_magic_ok(data: &[u8]) -> bool {
    if cfg!(target_os = "linux") {
        data.len() >= 4 && data[..4] == [0x7F, b'E', b'L', b'F']
    } else if cfg!(target_os = "macos") {
        macho_magic_ok(data)
    } else if cfg!(windows) {
        data.len() >= 2 && data[..2] == *b"MZ"
    } else {
        true
    }
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

    let magic_ok = binary_magic_ok(&data);

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
    if dl.size > MAX_BINARY_SIZE {
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
                         Run: chmod +x the binary path"
                            .into(),
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

    async fn http_fixture(
        responses: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/binary", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    request.push_str(&line);
                }
                requests.push(request);
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
            requests
        });
        (url, task)
    }

    #[tokio::test]
    async fn interrupted_download_keeps_bytes_and_resumes_exactly() {
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc".into(),
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 3-9/10\r\nConnection: close\r\n\r\ndefghij".into(),
        ]).await;
        assert!(download_once(&url, &tmp, 10).await.is_err());
        assert_eq!(
            std::fs::read(&tmp).unwrap(),
            b"abc",
            "interrupted bytes must reach disk before retry"
        );
        assert_eq!(download_once(&url, &tmp, 10).await.unwrap(), 10);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"abcdefghij");
        let requests = server.await.unwrap();
        assert!(!requests[0].to_lowercase().contains("range:"));
        assert!(requests[1].to_lowercase().contains("range: bytes=3-"));
        std::fs::remove_file(tmp).unwrap();
    }

    #[tokio::test]
    async fn download_refuses_wrong_ranges_and_oversized_bodies() {
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        std::fs::write(&tmp, b"abc").unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 2-8/10\r\nConnection: close\r\n\r\ndefghij".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nabcdefghijk".into(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nb\r\nabcdefghijk\r\n0\r\n\r\n".into(),
        ]).await;
        assert!(download_once(&url, &tmp, 10)
            .await
            .unwrap_err()
            .to_string()
            .contains("range"));
        assert_eq!(std::fs::read(&tmp).unwrap(), b"abc");
        assert!(download_once(&url, &tmp, 10)
            .await
            .unwrap_err()
            .to_string()
            .contains("size"));
        assert_eq!(std::fs::read(&tmp).unwrap(), b"abc");
        assert!(download_once(&url, &tmp, 10)
            .await
            .unwrap_err()
            .to_string()
            .contains("size"));
        assert!(std::fs::metadata(&tmp).unwrap().len() <= 10);
        assert_eq!(server.await.unwrap().len(), 3);
        std::fs::remove_file(tmp).unwrap();
    }

    #[tokio::test]
    async fn chunked_resume_cannot_exceed_its_declared_range() {
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        std::fs::write(&tmp, b"abc").unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-4/10\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7\r\ndefghij\r\n0\r\n\r\n".into(),
        ]).await;
        assert!(download_once(&url, &tmp, 10).await.is_err());
        assert!(std::fs::metadata(&tmp).unwrap().len() <= 5);
        assert_eq!(server.await.unwrap().len(), 1);
        std::fs::remove_file(tmp).unwrap();
    }

    #[tokio::test]
    async fn server_ignoring_range_replaces_partial_file() {
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        std::fs::write(&tmp, b"old").unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnew!".into(),
        ])
        .await;
        assert_eq!(download_once(&url, &tmp, 4).await.unwrap(), 4);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"new!");
        assert!(server.await.unwrap()[0]
            .to_lowercase()
            .contains("range: bytes=3-"));
        std::fs::remove_file(tmp).unwrap();
    }

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

#[cfg(test)]
mod magic_tests {
    use super::*;

    /// First 4 bytes of the shipped v4.0.1 darwin assets (od -t x1): both
    /// darwin-arm64 and darwin-x64 start `cf fa ed fe` — little-endian
    /// 64-bit Mach-O (MH_CIGAM_64). The pre-fix check accepted only the
    /// big-endian spellings and rejected these, so every macOS self-update
    /// and rollback failed with "not a valid binary for this platform".
    #[test]
    fn macho_magic_accepts_the_real_artifact_headers() {
        assert!(macho_magic_ok(&[
            0xCF, 0xFA, 0xED, 0xFE, 0x0C, 0x00, 0x00, 0x01
        ]));
        assert!(macho_magic_ok(&[
            0xCF, 0xFA, 0xED, 0xFE, 0x07, 0x00, 0x00, 0x01
        ]));
        // The other standard spellings stay accepted.
        assert!(macho_magic_ok(&[0xFE, 0xED, 0xFA, 0xCF]));
        assert!(macho_magic_ok(&[0xFE, 0xED, 0xFA, 0xCE]));
        assert!(macho_magic_ok(&[0xCE, 0xFA, 0xED, 0xFE]));
        assert!(macho_magic_ok(&[0xCA, 0xFE, 0xBA, 0xBE]));
        assert!(macho_magic_ok(&[0xBE, 0xBA, 0xFE, 0xCA]));
        // Foreign formats and short inputs are refused.
        assert!(!macho_magic_ok(&[0x7F, b'E', b'L', b'F']));
        assert!(!macho_magic_ok(b"MZ\x90\x00"));
        assert!(!macho_magic_ok(&[]));
        assert!(!macho_magic_ok(&[0xCF, 0xFA]));
    }

    /// The platform dispatch picks the right magic family per OS; the
    /// target-specific arm must accept the same bytes the release assets use.
    #[test]
    fn binary_magic_matches_the_platform() {
        if cfg!(target_os = "linux") {
            assert!(binary_magic_ok(&[
                0x7F, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00
            ]));
            assert!(!binary_magic_ok(&[0xCF, 0xFA, 0xED, 0xFE]));
        } else if cfg!(target_os = "macos") {
            assert!(binary_magic_ok(&[
                0xCF, 0xFA, 0xED, 0xFE, 0x0C, 0x00, 0x00, 0x01
            ]));
            assert!(!binary_magic_ok(&[0x7F, b'E', b'L', b'F']));
        } else if cfg!(windows) {
            assert!(binary_magic_ok(b"MZ\x90\x00"));
            assert!(!binary_magic_ok(&[0xCF, 0xFA, 0xED, 0xFE]));
        }
    }
}
