//! Version checking against GitHub releases.

use crate::error::{BladeError, Result};
use serde::Deserialize;

/// A GitHub release.
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub body: Option<String>,
}

impl Release {
    /// The tag with the leading "v" stripped (e.g. "0.9.0" from "v0.9.0").
    pub fn tag(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }
}

/// A release asset (downloadable binary).
#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// Fetch the latest release from GitHub.
/// Uses GITHUB_TOKEN env var if set (higher rate limit).
pub async fn fetch_latest() -> Result<Release> {
    // Allow skipping update checks entirely (CI, air-gapped).
    if std::env::var("BLADE_NO_UPDATE_CHECK").map(|v| v == "1").unwrap_or(false) {
        return Err(BladeError::Other("update check disabled (BLADE_NO_UPDATE_CHECK)".into()));
    }

    let url = format!(
        "https://api.github.com/repos/{}/releases?per_page=1",
        super::GITHUB_REPO
    );
    let mut client_builder = reqwest::Client::builder()
        .user_agent("bladebro-updater")
        .timeout(std::time::Duration::from_secs(10));

    // Use GITHUB_TOKEN if available (5000 req/hr vs 60 unauthenticated).
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            client_builder = client_builder
                .default_headers({
                    let mut h = reqwest::header::HeaderMap::new();
                    if let Ok(v) = reqwest::header::HeaderValue::from_str(
                        &format!("Bearer {token}"),
                    ) {
                        h.insert(reqwest::header::AUTHORIZATION, v);
                    }
                    h
                });
        }
    }

    let client = client_builder
        .build()
        .map_err(|e| BladeError::Other(format!("http client: {e}")))?;

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| BladeError::Other(format!("cannot reach GitHub: {e}")))?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(BladeError::Other(
            "GitHub API rate limited. Set GITHUB_TOKEN for higher limits.".into(),
        ));
    }
    if !resp.status().is_success() {
        return Err(BladeError::Other(format!(
            "GitHub API returned {}",
            resp.status()
        )));
    }

    let releases: Vec<Release> = resp
        .json::<Vec<Release>>()
        .await
        .map_err(|e| BladeError::Other(format!("cannot parse release: {e}")))?;

    releases
        .into_iter()
        .next()
        .ok_or_else(|| BladeError::Other("no releases found".into()))
}

/// Is `latest` newer than `current`? Simple semver comparison.
/// Strips a leading "v" prefix if present.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.strip_prefix('v')
            .unwrap_or(v)
            .split('.')
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    };
    let l = parse(latest);
    let c = parse(current);
    l > c
}

/// The expected asset name for this platform.
pub fn platform_asset_name() -> String {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    format!("bladebro-{os}-{arch}{ext}")
}

/// Find the asset for this platform in a release.
pub fn find_asset(release: &Release) -> Option<&Asset> {
    let name = platform_asset_name();
    release.assets.iter().find(|a| a.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_basic() {
        assert!(is_newer("1.0.0", "0.9.0"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("0.9.1", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.9.0"));
        assert!(!is_newer("0.8.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
    }

    #[test]
    fn is_newer_strips_v_prefix() {
        assert!(is_newer("v1.0.0", "0.9.0"));
    }

    #[test]
    fn platform_asset_name_format() {
        let name = platform_asset_name();
        assert!(name.starts_with("bladebro-"));
        assert!(name.contains("x86_64") || name.contains("aarch64"));
    }
}
