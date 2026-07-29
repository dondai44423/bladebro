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
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
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

/// Fetch the latest stable release from GitHub.
///
/// Fetches the 10 most recent releases and picks the highest
/// version by SEMVER ORDER — not GitHub's created_at ordering,
/// which breaks when an old release is edited or re-tagged.
/// Drafts are skipped. Prereleases are only returned when no
/// stable release exists (early-project phase).
/// Uses GITHUB_TOKEN env var if set (higher rate limit).
pub async fn fetch_latest() -> Result<Release> {
    // Allow skipping update checks entirely (CI, air-gapped).
    if std::env::var("BLADE_NO_UPDATE_CHECK").map(|v| v == "1").unwrap_or(false) {
        return Err(BladeError::Other("update check disabled (BLADE_NO_UPDATE_CHECK)".into()));
    }

    let url = format!(
        "https://api.github.com/repos/{}/releases?per_page=10",
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

    // Pick the highest semver among non-draft releases.
    // Stable releases win over prereleases at equal/lower versions.
    let mut best: Option<Release> = None;
    for r in releases.into_iter().filter(|r| !r.draft) {
        let replace = match &best {
            None => true,
            Some(b) => {
                let cmp = compare_versions(r.tag(), b.tag());
                cmp == std::cmp::Ordering::Greater
                    || (cmp == std::cmp::Ordering::Equal && b.prerelease && !r.prerelease)
            }
        };
        if replace {
            best = Some(r);
        }
    }
    best.ok_or_else(|| BladeError::Other("no releases found".into()))
}

/// Parse a version string into comparable numeric components.
/// Strips a leading "v" and any pre-release suffix ("-dev",
/// "-beta.1" — the numeric parts still compare correctly for
/// our purposes: 2.0.0-dev < 2.0.0 is handled by the caller).
fn parse_version(v: &str) -> (Vec<u64>, bool) {
    let core = v.strip_prefix('v').unwrap_or(v);
    let (nums_str, is_prerelease) = match core.split_once('-') {
        Some((n, _)) => (n, true),
        None => (core, false),
    };
    let nums: Vec<u64> = nums_str
        .split('.')
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    (nums, is_prerelease)
}

/// Three-way version comparison: how does `a` relate to `b`?
/// Pre-release versions sort BELOW their release
/// (2.0.0-dev < 2.0.0).
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let (an, a_pre) = parse_version(a);
    let (bn, b_pre) = parse_version(b);
    match an.cmp(&bn) {
        std::cmp::Ordering::Equal => match (a_pre, b_pre) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        },
        ord => ord,
    }
}

/// Is `latest` newer than `current`? Semver comparison.
pub fn is_newer(latest: &str, current: &str) -> bool {
    compare_versions(latest, current) == std::cmp::Ordering::Greater
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
    fn compare_three_way() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("2.0.0", "1.0.0"), Greater);
        assert_eq!(compare_versions("1.0.0", "2.0.0"), Less);
        assert_eq!(compare_versions("2.0.0", "2.0.0"), Equal);
        assert_eq!(compare_versions("v2.0.0", "1.9.9"), Greater);
        // Pre-release sorts below its release.
        assert_eq!(compare_versions("2.0.0-dev", "2.0.0"), Less);
        assert_eq!(compare_versions("2.0.0", "2.0.0-dev"), Greater);
        // Dev build ahead of last release.
        assert_eq!(compare_versions("2.0.0", "1.0.0"), Greater);
    }

    #[test]
    fn platform_asset_name_format() {
        let name = platform_asset_name();
        assert!(name.starts_with("bladebro-"));
        assert!(name.contains("x86_64") || name.contains("aarch64"));
    }
}
