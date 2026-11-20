//! Binary download and integrity verification.
//!
//! Performance & reliability design:
//! - Reuses the shared pooled HTTP client (one rustls root store + keep-alive).
//! - Fetches the `.sha256` sidecar **concurrently** with the binary download.
//! - Computes SHA-256 in a **single streaming pass** on clean HTTP 200 downloads
//!   (avoiding a second 8 MB disk read pass after download).
//! - Checks disk space natively (`statvfs` / `GetDiskFreeSpaceExW`) as soon as
//!   `Content-Length` arrives — before writing bytes to disk.
//! - Recovers cleanly from HTTP 416 on stale partial files, skips wasted retries
//!   on permanent 4xx errors, and falls back to legacy asset names on 404.

use super::ui;
use super::version;
use crate::error::{BladeError, Result};
use std::path::PathBuf;

/// A downloaded binary, ready to be installed. It owns its temp file:
/// dropping it removes the file, so every exit from an update — a failed
/// verify or swap, an early `?`, a dropped future — cleans up the install
/// dir. After a successful swap the file was renamed into place and there
/// is nothing left to remove.
pub struct DownloadedBinary {
    pub path: PathBuf,
    pub size: u64,
    pub asset_name: String,
    /// Verified lowercase hex SHA-256 digest (None only for legacy <3.3.0 tags
    /// without a sidecar).
    pub sha256: Option<String>,
}

impl Drop for DownloadedBinary {
    fn drop(&mut self) {
        cleanup_tmp(&self.path);
    }
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
const MAX_CHECKSUM_BYTES: usize = 16 * 1024;

/// Is checksum verification mandatory for this target tag? A tag that is
/// not a version cannot be shown to predate checksum uploads, so it is
/// required (fail-closed) — it used to parse as version [] and sort below
/// FIRST_CHECKSUMMED_TAG, i.e. warn-and-skip.
pub fn checksum_required(tag: &str) -> bool {
    !super::version::is_version_tag(tag)
        || super::version::compare_versions(tag, FIRST_CHECKSUMMED_TAG) != std::cmp::Ordering::Less
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

/// Fetch the expected SHA-256 digest for `asset_url`.
///
/// Returns `Ok(Some(hash))` when a valid checksum is retrieved, `Ok(None)`
/// only for legacy (< 3.3.0) tags without a sidecar, and `Err` (fail-closed)
/// for all checksummed releases.
async fn fetch_expected_sha256(asset_url: &str, tag: &str) -> Result<Option<String>> {
    let checksum_url = format!("{asset_url}.sha256");
    let client = version::shared_download_client()?;
    let required = checksum_required(tag);
    let fetch_failed = |why: &str| -> BladeError {
        BladeError::Other(format!(
            "cannot verify update integrity: {why}.\n\
             Release {tag} must ship a .sha256 checksum next to every binary.\n\
             The download was NOT installed. Update via npm instead:\n\
             npm install -g bladebro"
        ))
    };

    // Bounded transport retries (3 attempts, 300/600 ms backoff). Each
    // attempt refetches cleanly — the GET has no side effects.
    const ATTEMPTS: u32 = 3;
    let mut last_transport: Option<String> = None;
    let mut attempt = 0u32;
    let fetched: Option<(reqwest::StatusCode, std::result::Result<String, String>)> = loop {
        attempt += 1;
        let mut resp = match client
            .get(&checksum_url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_transport = Some(format!("{e}"));
                if attempt >= ATTEMPTS {
                    break None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64)).await;
                continue;
            }
        };
        let status = resp.status();
        // Sidecars are ~100 bytes. Enforce the bound while reading, including
        // chunked bodies without Content-Length, before parse/allocation.
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_CHECKSUM_BYTES as u64)
        {
            return Err(fetch_failed("checksum response exceeds 16 KiB"));
        }
        let mut bytes = Vec::new();
        let body = loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if chunk.len() > MAX_CHECKSUM_BYTES.saturating_sub(bytes.len()) {
                        return Err(fetch_failed("checksum response exceeds 16 KiB"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Err(error) => break Err(error),
            }
        };
        match body {
            Ok(t) => break Some((status, Ok(t))),
            Err(e) => {
                last_transport = Some(format!("body read failed: {e}"));
                if attempt >= ATTEMPTS {
                    break Some((status, Err(e.to_string())));
                }
                tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64)).await;
            }
        }
    };

    let (status, body) = match fetched {
        Some(x) => x,
        None if required => {
            return Err(fetch_failed(&format!(
                "checksum file unreachable after {ATTEMPTS} attempts ({})",
                last_transport.as_deref().unwrap_or("transport error")
            )))
        }
        None => return Ok(None),
    };

    if status == reqwest::StatusCode::NOT_FOUND || !status.is_success() {
        if required {
            return Err(fetch_failed(&format!(
                "no checksum file at {} (HTTP {})",
                checksum_url, status
            )));
        }
        return Ok(None);
    }

    let checksum_text = body.map_err(|e| {
        if required {
            fetch_failed(&format!("cannot read checksum: {e}"))
        } else {
            BladeError::Other(format!("cannot read checksum: {e}"))
        }
    })?;

    match parse_checksum(&checksum_text) {
        Some(h) => Ok(Some(h)),
        None if required => Err(fetch_failed("malformed checksum file")),
        None => Ok(None),
    }
}

/// Compute lowercase hex SHA-256 of a file on disk.
fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|e| BladeError::Other(format!("cannot read binary for hash: {e}")))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|e| BladeError::Other(format!("cannot hash binary: {e}")))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Verify `binary_path` against `<asset_url>.sha256` (computing the hash from
/// disk when `precomputed` is `None`).
async fn verify_sha256_with_hash(
    binary_path: &std::path::Path,
    expected: Option<String>,
    precomputed: Option<String>,
) -> Result<Option<String>> {
    let Some(expected_hash) = expected else {
        return Ok(None);
    };
    let actual_hash = match precomputed {
        Some(h) => h,
        None => sha256_file(binary_path)?,
    };
    if actual_hash != expected_hash {
        return Err(BladeError::Other(format!(
            "SHA256 mismatch! Expected {expected_hash}, got {actual_hash}.\n\
             The downloaded binary may be corrupted or tampered with.\n\
             Aborting update for safety."
        )));
    }
    Ok(Some(actual_hash))
}

/// SECURITY: Verify the SHA256 hash of a downloaded binary against a
/// checksum file from the same release (kept as a clean helper for direct
/// verification and tests).
#[cfg(test)]
async fn verify_sha256(binary_path: &std::path::Path, asset_url: &str, tag: &str) -> Result<()> {
    let expected = fetch_expected_sha256(asset_url, tag).await?;
    let _ = verify_sha256_with_hash(binary_path, expected, None).await?;
    Ok(())
}

/// Download outcome classification so permanent 4xx errors don't waste retries.
enum AttemptOutcome {
    Done(u64, Option<String>),
    NotFound(String),
    Permanent(String),
    Transient(String),
}

/// Download the platform binary from a release with concurrent SHA-256 sidecar
/// fetch, single-pass hashing, resume support, legacy asset fallback, and a
/// live animated Mafia Don progress bar on TTYs.
pub async fn download_binary(release: &version::Release) -> Result<DownloadedBinary> {
    let candidates = version::candidate_assets(release);
    if candidates.is_empty() {
        let available = version::asset_names(release);
        let avail_str = if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        };
        return Err(BladeError::Other(format!(
            "no binary for {} in release {}. Available assets: {}\n\n\
             To update via npm:  npm install -g bladebro\n\
             Or build from source:  git clone https://github.com/dondai44423/bladebro.git && cd bladebro && cargo build --release",
            version::platform_label(),
            release.tag_name,
            avail_str,
        )));
    }

    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;
    let dir = current.parent().unwrap_or(std::path::Path::new("."));

    // Unique, unpredictable temp name + O_EXCL create: a predictable
    // fixed name allowed a local attacker to pre-place a symlink at the
    // temp path and have the downloaded binary written through it.
    let mut dl = DownloadedBinary {
        path: create_secure_tmp(dir)?,
        size: 0,
        asset_name: candidates[0].name.clone(),
        sha256: None,
    };

    sweep_leftover_tmps(dir, &dl.path);

    let mut last_err = String::new();
    for (idx, asset) in candidates.iter().enumerate() {
        dl.asset_name = asset.name.clone();
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

        // Launch the `.sha256` sidecar fetch concurrently with the binary download!
        let checksum_fut = fetch_expected_sha256(&asset.browser_download_url, release.tag());
        let bar = ui::ProgressBar::new(&asset.name, asset.size);
        let download_fut = download_asset_with_retries(
            &asset.browser_download_url,
            &dl.path,
            dir,
            asset.size,
            Some(&bar),
        );

        let (dl_outcome, checksum_res) = tokio::join!(download_fut, checksum_fut);

        match dl_outcome {
            AttemptOutcome::Done(size, stream_hash) => {
                bar.finish_ok(size);
                let expected = checksum_res?;
                let verified_hash =
                    verify_sha256_with_hash(&dl.path, expected, stream_hash).await?;
                dl.size = size;
                dl.sha256 = verified_hash;
                return Ok(dl);
            }
            AttemptOutcome::NotFound(msg) => {
                bar.abandon();
                last_err = msg;
                // Truncate temp before trying any fallback legacy asset name.
                let _ = std::fs::File::create(&dl.path);
                if idx + 1 < candidates.len() {
                    continue;
                }
                return Err(BladeError::Other(last_err));
            }
            AttemptOutcome::Permanent(msg) => {
                bar.abandon();
                return Err(BladeError::Other(msg));
            }
            AttemptOutcome::Transient(msg) => {
                bar.abandon();
                last_err = msg;
                return Err(BladeError::Other(format!(
                    "download failed after 3 attempts: {last_err}"
                )));
            }
        }
    }

    Err(BladeError::Other(format!("download failed: {last_err}")))
}

async fn download_asset_with_retries(
    url: &str,
    tmp: &std::path::Path,
    dir: &std::path::Path,
    expected_size: u64,
    progress: Option<&ui::ProgressBar>,
) -> AttemptOutcome {
    let mut last_err = String::new();
    for attempt in 1..=3u32 {
        if attempt > 1 {
            let backoff_ms = if attempt == 2 { 500 } else { 1500 };
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
        match download_once_inner(url, tmp, Some(dir), expected_size, progress).await {
            AttemptOutcome::Done(size, hash) => return AttemptOutcome::Done(size, hash),
            AttemptOutcome::NotFound(e) => return AttemptOutcome::NotFound(e),
            AttemptOutcome::Permanent(e) => return AttemptOutcome::Permanent(e),
            AttemptOutcome::Transient(e) => {
                last_err = e;
            }
        }
    }
    AttemptOutcome::Transient(last_err)
}

/// A download temp untouched this long belongs to no live updater: a live
/// one writes at least every 180 s (the per-attempt download timeout) and
/// verifies within a minute of its last write.
const STALE_TMP_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Remove download temps left in the install dir: this process's earlier
/// attempts, and any temp that has sat untouched for STALE_TMP_AGE — a
/// Ctrl-C'd or crashed run's partial download.
fn sweep_leftover_tmps(dir: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let ours = format!("{TMP_PREFIX}-{}-", std::process::id());
    let any = format!("{TMP_PREFIX}-");
    let now = std::time::SystemTime::now();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(&any) || !name.ends_with(".tmp") || e.path() == keep {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= STALE_TMP_AGE);
        if name.starts_with(&ours) || stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
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

/// Download a URL to a file, with resume support (wrapper for unit tests).
#[cfg(test)]
async fn download_once(url: &str, tmp: &std::path::Path, expected_size: u64) -> Result<u64> {
    match download_once_inner(url, tmp, None, expected_size, None).await {
        AttemptOutcome::Done(size, _) => Ok(size),
        AttemptOutcome::NotFound(e)
        | AttemptOutcome::Permanent(e)
        | AttemptOutcome::Transient(e) => Err(BladeError::Other(e)),
    }
}

/// Download a URL to a file, with resume support, live disk-space verification
/// from `Content-Length`, real-time progress reporting, and single-pass
/// streaming SHA-256 on full (HTTP 200) transfers.
async fn download_once_inner(
    url: &str,
    tmp: &std::path::Path,
    disk_dir: Option<&std::path::Path>,
    expected_size: u64,
    progress: Option<&ui::ProgressBar>,
) -> AttemptOutcome {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let limit = if expected_size == 0 {
        MAX_BINARY_SIZE
    } else {
        expected_size
    };
    if limit > MAX_BINARY_SIZE {
        return AttemptOutcome::Permanent("download size exceeds 500 MB".into());
    }
    let existing = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
    if existing > limit {
        return AttemptOutcome::Permanent("partial download exceeds expected size".into());
    }
    if expected_size > 0 && existing == expected_size {
        return AttemptOutcome::Done(existing, None);
    }

    let client = match version::shared_download_client() {
        Ok(c) => c,
        Err(e) => return AttemptOutcome::Permanent(e.to_string()),
    };
    let mut req = client.get(url).timeout(std::time::Duration::from_secs(300));
    if existing > 0 {
        req = req.header("Range", format!("bytes={existing}-"));
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return AttemptOutcome::Transient(format!("download failed: {e}")),
    };

    let status = resp.status();
    // Stale partial file exceeded server size → truncate and retry from 0.
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && existing > 0 {
        let _ = std::fs::File::create(tmp);
        return AttemptOutcome::Transient("partial range invalid (HTTP 416), restarting".into());
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return AttemptOutcome::NotFound(format!("download failed: HTTP {status}"));
    }
    let partial = status == reqwest::StatusCode::PARTIAL_CONTENT;
    if status != reqwest::StatusCode::OK && !partial {
        let msg = format!("download failed: HTTP {status}");
        if matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::GONE
        ) {
            return AttemptOutcome::Permanent(msg);
        }
        return AttemptOutcome::Transient(msg);
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
                return AttemptOutcome::Permanent(
                    "download returned an invalid content range (Content-Range)".into(),
                )
            }
        }
    }

    if resp
        .content_length()
        .is_some_and(|len| len > response_limit - offset)
    {
        return AttemptOutcome::Permanent("download exceeds expected size".into());
    }

    // Learn total bytes from headers when `synthetic_release` had `size: 0`.
    let total_known = if target_size > 0 {
        target_size
    } else if let Some(cl) = resp.content_length() {
        offset.saturating_add(cl)
    } else {
        0
    };
    if let (Some(dir), true) = (disk_dir, total_known > offset) {
        let remaining = total_known - offset;
        if let Err(e) = check_disk_space(dir, remaining) {
            return AttemptOutcome::Permanent(format!(
                "insufficient disk space for download (~{:.1} MB needed): {e}\n\
                 Free space in {} and try again.",
                remaining as f64 / 1_000_000.0,
                dir.display(),
            ));
        }
    }
    if let Some(bar) = progress {
        if total_known > 0 {
            bar.set_total(total_known);
        }
        if offset > 0 {
            bar.set_position(offset);
        }
    }

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .append(partial)
        .truncate(!partial)
        .open(tmp)
    {
        Ok(f) => f,
        Err(e) => return AttemptOutcome::Permanent(format!("cannot open temp file: {e}")),
    };

    // Single-pass streaming SHA-256 on full HTTP 200 downloads.
    let mut hasher = if !partial { Some(Sha256::new()) } else { None };
    let mut size = offset;
    while let Some(chunk) = match resp.chunk().await {
        Ok(c) => c,
        Err(e) => return AttemptOutcome::Transient(format!("download interrupted: {e}")),
    } {
        if chunk.len() as u64 > response_limit - size {
            return AttemptOutcome::Permanent("download exceeds expected size".into());
        }
        if let Err(e) = file.write_all(&chunk) {
            return AttemptOutcome::Permanent(format!("cannot write temp file: {e}"));
        }
        if let Some(ref mut h) = hasher {
            h.update(&chunk);
        }
        size += chunk.len() as u64;
        if let Some(bar) = progress {
            bar.set_position(size);
        }
    }

    if size == 0 || (target_size > 0 && size != target_size) {
        return AttemptOutcome::Transient(format!(
            "incomplete download: {size} bytes, expected {target_size}"
        ));
    }

    // Flush OS buffers before verification / atomic rename.
    let _ = file.sync_data();
    let hex_hash = hasher.map(|h| h.finalize().iter().map(|b| format!("{b:02x}")).collect());
    AttemptOutcome::Done(size, hex_hash)
}

/// Check if there's enough disk space for a download using native OS APIs
/// (`statvfs` on Unix, `GetDiskFreeSpaceExW` on Windows). Falls through
/// silently if the filesystem cannot be queried.
fn check_disk_space(dir: &std::path::Path, needed: u64) -> Result<()> {
    let Some(avail) = crate::platform::available_disk_bytes(dir) else {
        return Ok(());
    };
    if avail < needed.saturating_add(10_000_000) {
        return Err(BladeError::Other(format!(
            "only {:.1} MB available on disk",
            avail as f64 / 1_000_000.0
        )));
    }
    Ok(())
}

/// Mach-O / fat-archive magic, accepting EVERY standard spelling. Modern
/// macOS builds are little-endian 64-bit Mach-O — file bytes `cf fa ed fe`
/// (MH_CIGAM_64), which is what every darwin asset in this repo starts
/// with.
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
/// shared by `verify_binary` and the rollback's `verify_backup`.
pub(crate) fn binary_magic_ok(data: &[u8]) -> bool {
    if cfg!(any(target_os = "linux", target_os = "android")) {
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
pub fn verify_binary(dl: &DownloadedBinary) -> Result<()> {
    use std::io::Read;
    let mut file = std::fs::File::open(&dl.path)
        .map_err(|e| BladeError::Other(format!("cannot read downloaded file: {e}")))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != dl.size {
        return Err(BladeError::Other(
            "downloaded file size changed after download — refusing verification".into(),
        ));
    }
    if metadata.len() < 4 {
        return Err(BladeError::Other("downloaded file too small".into()));
    }

    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    let magic_ok = binary_magic_ok(&magic);

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

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dl.path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| BladeError::Other(format!("cannot set executable permission: {e}")))?;
    }

    Ok(())
}

/// Try executing any candidate binary at `path` with `--version` to confirm
/// it starts on this host architecture and OS.
pub(crate) async fn verify_exe_runs(path: &std::path::Path) -> Result<()> {
    let mut child = tokio::process::Command::new(path)
        .arg("--version")
        // Startup verification must not wait for GitHub/npm connectivity
        // or spend time animating.
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env("BLADE_PLAIN", "1")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| BladeError::Other(format!("cannot execute binary: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BladeError::Other("missing version stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| BladeError::Other("missing version stderr".into()))?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (stdout, stderr, status) = tokio::try_join!(
            read_version_output(stdout),
            read_version_output(stderr),
            child.wait(),
        )?;
        Ok::<_, std::io::Error>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    let output = match output {
        Ok(Ok(output)) => output,
        error => {
            let _ = child.kill().await;
            return Err(BladeError::Other(match error {
                Ok(Err(error)) => format!("binary version probe failed: {error}"),
                _ => "binary --version did not finish within 5s".into(),
            }));
        }
    };
    if !output.status.success() {
        return Err(BladeError::Other(format!(
            "binary failed to start (exit {:?})\n  stdout: {}\n  stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }
    let combined = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !combined.to_lowercase().contains("bladebro") {
        return Err(BladeError::Other(
            "binary ran but doesn't identify as bladebro".into(),
        ));
    }
    Ok(())
}

/// Try executing the downloaded binary to confirm it starts.
pub async fn verify_binary_runs(dl: &DownloadedBinary) -> Result<()> {
    verify_exe_runs(&dl.path).await
}

async fn read_version_output(
    reader: impl tokio::io::AsyncRead + Unpin,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    reader
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > LIMIT {
        return Err(std::io::Error::other("version output exceeds 64 KiB"));
    }
    Ok(bytes)
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

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_startup_verification_has_a_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let path = create_secure_tmp(&std::env::temp_dir()).unwrap();
        let marker = std::path::PathBuf::from(format!("{}.waiting", path.display()));
        std::fs::write(&marker, b"owned").unwrap();
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile [ -e \"$0.waiting\" ]; do :; done\nprintf 'bladebro fixture\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let dl = DownloadedBinary {
            path: path.clone(),
            size: 0,
            asset_name: "fixture".into(),
            sha256: None,
        };
        let mut task = tokio::spawn(async move { verify_binary_runs(&dl).await });
        let result = tokio::time::timeout(std::time::Duration::from_secs(6), &mut task).await;
        std::fs::remove_file(marker).unwrap();
        let outcome = match result {
            Ok(outcome) => outcome.unwrap(),
            Err(_) => task.await.unwrap(),
        };
        let _ = std::fs::remove_file(path);
        assert!(
            outcome.is_err(),
            "a wedged version probe must fail within its own deadline"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_probe_checks_exit_identity_output_bound_and_offline_mode() {
        use std::os::unix::fs::PermissionsExt;
        for (body, accepted) in [
            (
                "[ \"$BLADE_NO_UPDATE_CHECK\" = 1 ] || exit 7\nprintf 'bladebro fixture\\n'"
                    .to_string(),
                true,
            ),
            ("printf 'bladebro fixture\\n'\nexit 9".to_string(), false),
            ("printf 'different program\\n'".to_string(), false),
            (format!("printf '{}'", "bladebro".repeat(9000)), false),
        ] {
            let path = create_secure_tmp(&std::env::temp_dir()).unwrap();
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let dl = DownloadedBinary {
                path: path.clone(),
                size: 0,
                asset_name: "fixture".into(),
                sha256: None,
            };
            let result = verify_binary_runs(&dl).await;
            std::fs::remove_file(path).unwrap();
            assert_eq!(result.is_ok(), accepted, "{result:?}");
        }
    }

    #[test]
    fn a_rejected_download_does_not_leave_its_temp_file() {
        let path = create_secure_tmp(&std::env::temp_dir()).unwrap();
        std::fs::write(&path, b"not a binary").unwrap();
        let dl = DownloadedBinary {
            path: path.clone(),
            size: 12,
            asset_name: "fixture".into(),
            sha256: None,
        };
        assert!(verify_binary(&dl).is_err());
        drop(dl);
        let left = path.exists();
        let _ = std::fs::remove_file(&path);
        assert!(!left, "the rejected download is still on disk");
    }

    #[test]
    fn leftover_sweep_removes_stale_temps_from_earlier_runs_only() {
        let dir = std::env::temp_dir().join(format!("blade-update-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let age = |p: &std::path::Path| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        };
        let stale = dir.join(format!("{TMP_PREFIX}-1-1700000000000000000-7.tmp"));
        let fresh = dir.join(format!("{TMP_PREFIX}-2-1700000000000000001-8.tmp"));
        let unrelated = dir.join("bladebro");
        for p in [&stale, &fresh, &unrelated] {
            std::fs::write(p, b"x").unwrap();
        }
        age(&stale);
        age(&unrelated);
        let current = create_secure_tmp(&dir).unwrap();
        age(&current);
        sweep_leftover_tmps(&dir, &current);
        let (stale_left, fresh_left, unrelated_left, current_left) = (
            stale.exists(),
            fresh.exists(),
            unrelated.exists(),
            current.exists(),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !stale_left,
            "a stale temp from an earlier run must be swept"
        );
        assert!(fresh_left, "a fresh temp may belong to a running updater");
        assert!(unrelated_left && current_left);
    }

    #[test]
    fn truncated_binary_cannot_pass_a_stale_download_size() {
        let path = create_secure_tmp(&std::env::temp_dir()).unwrap();
        let magic: &[u8] = if cfg!(windows) {
            b"MZxx"
        } else if cfg!(target_os = "macos") {
            &[0xcf, 0xfa, 0xed, 0xfe]
        } else {
            &[0x7f, b'E', b'L', b'F']
        };
        std::fs::write(&path, magic).unwrap();
        let dl = DownloadedBinary {
            path: path.clone(),
            size: 1_000_000,
            asset_name: "fixture".into(),
            sha256: None,
        };
        let result = verify_binary(&dl);
        std::fs::remove_file(path).unwrap();
        assert!(
            result.is_err(),
            "a four-byte file must not pass because its recorded size was 1MB"
        );
    }

    #[tokio::test]
    async fn oversized_checksum_is_refused_with_and_without_content_length() {
        use sha2::{Digest, Sha256};
        let path = create_secure_tmp(&std::env::temp_dir()).unwrap();
        std::fs::write(&path, b"fixture").unwrap();
        let hash: String = Sha256::digest(b"fixture")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let body = format!("{hash}  {}", "x".repeat(32 * 1024));
        for head in [format!("Content-Length: {}\r\n", body.len()), String::new()] {
            let (url, server) = http_fixture(vec![format!(
                "HTTP/1.1 200 OK\r\n{head}Connection: close\r\n\r\n{body}"
            )])
            .await;
            let result = verify_sha256(&path, &url, "9.9.9").await;
            assert_eq!(server.await.unwrap().len(), 1);
            assert!(
                result.is_err(),
                "a checksum response must be bounded before allocation/parse"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

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
    async fn single_pass_streaming_hash_matches_disk_sha256() {
        use sha2::{Digest, Sha256};
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        let payload = "bladebro-single-pass-streaming-digest-payload";
        let expected: String = Sha256::digest(payload.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let (url, server) = http_fixture(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )])
        .await;
        match download_once_inner(&url, &tmp, None, payload.len() as u64, None).await {
            AttemptOutcome::Done(sz, hash) => {
                assert_eq!(sz, payload.len() as u64);
                assert_eq!(hash.as_deref(), Some(expected.as_str()));
            }
            _ => panic!("expected Done with single-pass hash"),
        }
        let _ = server.await.unwrap();
        std::fs::remove_file(tmp).unwrap();
    }

    #[tokio::test]
    async fn http_416_truncates_stale_partial_and_recovers_on_retry() {
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        std::fs::write(&tmp, b"stale-bytes").unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfresh".into(),
        ])
        .await;
        let outcome = download_asset_with_retries(&url, &tmp, &std::env::temp_dir(), 0, None).await;
        match outcome {
            AttemptOutcome::Done(5, Some(_)) => {}
            _ => panic!("expected recovery after 416"),
        }
        assert_eq!(std::fs::read(&tmp).unwrap(), b"fresh");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].to_lowercase().contains("range: bytes=11-"));
        assert!(!requests[1].to_lowercase().contains("range:"));
        std::fs::remove_file(tmp).unwrap();
    }

    #[tokio::test]
    async fn permanent_404_fails_immediately_without_retrying() {
        let tmp = create_secure_tmp(&std::env::temp_dir()).unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ])
        .await;
        let outcome = download_asset_with_retries(&url, &tmp, &std::env::temp_dir(), 0, None).await;
        assert!(matches!(outcome, AttemptOutcome::NotFound(_)));
        assert_eq!(server.await.unwrap().len(), 1);
        std::fs::remove_file(tmp).unwrap();
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
            sha256: None,
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
            sha256: None,
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
            sha256: None,
        };
        assert!(verify_binary(&dl).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_too_large() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-large");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0x7F, b'E', b'L', b'F']).unwrap();
        drop(f);
        let dl = DownloadedBinary {
            path: path.clone(),
            size: 600_000_000,
            asset_name: "test".into(),
            sha256: None,
        };
        assert!(verify_binary(&dl).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checksum_transport_errors_retry_within_bounds_and_succeed() {
        let dir = std::env::temp_dir().join(format!("blade-checksum-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("bladebro");
        std::fs::write(&bin, b"FAKE BINARY BYTES").unwrap();
        use sha2::{Digest, Sha256};
        let hash: String = Sha256::digest(b"FAKE BINARY BYTES")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let body = format!("{hash}  bladebro\n");
        let (url, server) = http_fixture(vec![
            "".into(),
            "".into(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            ),
        ])
        .await;
        let res = verify_sha256(&bin, &url, "9.9.9").await;
        assert!(res.is_ok(), "expected verified after retries: {res:?}");
        let requests = server.await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "two transport failures + one good attempt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checksum_permanent_failures_do_not_retry_and_fail_closed() {
        let dir = std::env::temp_dir().join(format!("blade-checksum-404-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("bladebro");
        std::fs::write(&bin, b"FAKE").unwrap();
        let (url, server) = http_fixture(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ])
        .await;
        let err = verify_sha256(&bin, &url, "9.9.9")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("NOT installed"), "{err}");
        assert!(
            err.contains("404"),
            "single-attempt 404 must be reported as-is: {err}"
        );
        assert!(
            !err.contains("after 3 attempts"),
            "permanent failures must not be retried: {err}"
        );
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod magic_tests {
    use super::*;

    #[test]
    fn macho_magic_accepts_the_real_artifact_headers() {
        assert!(macho_magic_ok(&[
            0xCF, 0xFA, 0xED, 0xFE, 0x0C, 0x00, 0x00, 0x01
        ]));
        assert!(macho_magic_ok(&[
            0xCF, 0xFA, 0xED, 0xFE, 0x07, 0x00, 0x00, 0x01
        ]));
        assert!(macho_magic_ok(&[0xFE, 0xED, 0xFA, 0xCF]));
        assert!(macho_magic_ok(&[0xFE, 0xED, 0xFA, 0xCE]));
        assert!(macho_magic_ok(&[0xCE, 0xFA, 0xED, 0xFE]));
        assert!(macho_magic_ok(&[0xCA, 0xFE, 0xBA, 0xBE]));
        assert!(macho_magic_ok(&[0xBE, 0xBA, 0xFE, 0xCA]));
        assert!(!macho_magic_ok(&[0x7F, b'E', b'L', b'F']));
        assert!(!macho_magic_ok(b"MZ\x90\x00"));
        assert!(!macho_magic_ok(&[]));
        assert!(!macho_magic_ok(&[0xCF, 0xFA]));
    }

    #[test]
    fn binary_magic_matches_the_platform() {
        if cfg!(any(target_os = "linux", target_os = "android")) {
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
