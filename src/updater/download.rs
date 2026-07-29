//! Binary download and verification.

use crate::error::{BladeError, Result};
use super::version;
use std::path::PathBuf;

/// A downloaded binary, ready to be installed.
pub struct DownloadedBinary {
    pub path: PathBuf,
    pub size: u64,
}

/// Download the platform binary from a release.
/// Retries up to 3 times with resume — slow/flaky
/// connections are the norm, not the exception.
pub async fn download_binary(release: &version::Release) -> Result<DownloadedBinary> {
    let asset = version::find_asset(release).ok_or_else(|| {
        BladeError::Other(format!(
            "no binary for this platform ({}). Available assets: {}",
            version::platform_asset_name(),
            release
                .assets
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;

    let current = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find current exe: {e}")))?;
    let dir = current.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(".bladebro-update-tmp");

    let mut last_err = String::new();
    for attempt in 1..=3 {
        if attempt > 1 {
            eprintln!("  retry {attempt}/3...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match download_once(&asset.browser_download_url, &tmp).await {
            Ok(size) => {
                return Ok(DownloadedBinary { path: tmp, size });
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
    }
    Err(BladeError::Other(format!(
        "download failed after 3 attempts: {last_err}"
    )))
}

async fn download_once(url: &str, tmp: &std::path::Path) -> Result<u64> {
    // Resume from wherever the last attempt left off.
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

    if !resp.status().is_success() {
        return Err(BladeError::Other(format!(
            "download failed: HTTP {}",
            resp.status()
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| BladeError::Other(format!("download interrupted: {e}")))?;

    if bytes.is_empty() && existing == 0 {
        return Err(BladeError::Other("downloaded file is empty".into()));
    }

    // Append for resumes, truncate for fresh downloads.
    if existing > 0 {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp)
            .map_err(|e| BladeError::Other(format!("cannot open temp file: {e}")))?;
        f.write_all(&bytes)
            .map_err(|e| BladeError::Other(format!("cannot write temp file: {e}")))?;
        Ok(existing + bytes.len() as u64)
    } else {
        std::fs::write(tmp, &bytes)
            .map_err(|e| BladeError::Other(format!("cannot write temp file: {e}")))?;
        Ok(bytes.len() as u64)
    }
}

/// Verify a downloaded binary is valid (correct magic bytes, reasonable size).
pub fn verify_binary(dl: &DownloadedBinary) -> Result<()> {
    let data = std::fs::read(&dl.path)
        .map_err(|e| BladeError::Other(format!("cannot read downloaded file: {e}")))?;

    if data.len() < 4 {
        return Err(BladeError::Other("downloaded file too small".into()));
    }

    let magic_ok = if cfg!(target_os = "linux") {
        // ELF magic: 0x7F 'E' 'L' 'F'
        data[..4] == [0x7F, b'E', b'L', b'F']
    } else if cfg!(target_os = "macos") {
        // Mach-O: 0xFEEDFACE (32-bit), 0xFEEDFACF (64-bit), or 0xCAFEBABE (fat)
        let m = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        m == 0xFEEDFACE || m == 0xFEEDFACF || m == 0xCAFEBABE || m == 0xBEBAFECA
    } else if cfg!(windows) {
        // PE: 'M' 'Z'
        data[..2] == *b"MZ"
    } else {
        true // unknown platform, skip magic check
    };

    if !magic_ok {
        return Err(BladeError::Other(
            "downloaded file is not a valid binary for this platform".into(),
        ));
    }

    // Reasonable size: at least 1MB (bladebro is ~5MB), at most 500MB.
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

    Ok(())
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
        let dl = DownloadedBinary { path, size: 0 };
        assert!(verify_binary(&dl).is_err());
    }

    #[test]
    fn verify_rejects_tiny() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-tiny");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.bin");
        std::fs::write(&path, b"ab").unwrap();
        let dl = DownloadedBinary { path, size: 2 };
        assert!(verify_binary(&dl).is_err());
    }

    #[test]
    fn verify_rejects_wrong_magic() {
        let dir = std::env::temp_dir().join("bladebro-test-verify-magic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wrong.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        // Write 2MB of zeros (wrong magic)
        f.write_all(&vec![0u8; 2_000_000]).unwrap();
        drop(f);
        let dl = DownloadedBinary { path, size: 2_000_000 };
        assert!(verify_binary(&dl).is_err());
    }
}
