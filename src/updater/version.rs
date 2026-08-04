//! Version checking against GitHub releases.

use crate::error::{BladeError, Result};

/// A GitHub release.
#[derive(Debug)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
}

impl Release {
    /// The tag with the leading "v" stripped (e.g. "0.9.0" from "v0.9.0").
    pub fn tag(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }
}

/// A release asset (downloadable binary).
#[derive(Debug)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

/// Fetch the latest version tag from non-rate-limited sources.
///
/// 1. `github.com/.../releases/latest` — 301 redirect, parse tag from URL.
/// 2. npm registry — JSON with the latest published version.
///
/// No API key needed. No rate limits. Works for all users out of the box.
pub async fn fetch_latest_tag() -> Result<String> {
    if std::env::var("BLADE_NO_UPDATE_CHECK")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return Err(BladeError::Other(
            "update check disabled (BLADE_NO_UPDATE_CHECK)".into(),
        ));
    }

    let client = reqwest::Client::builder()
        .user_agent("bladebro-updater")
        .redirect(reqwest::redirect::Policy::none()) // don't follow, read Location
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| BladeError::Other(format!("http client: {e}")))?;

    // 1. GitHub releases/latest redirect.
    let url = format!("https://github.com/{}/releases/latest", super::GITHUB_REPO);
    if let Ok(resp) = client.get(&url).send().await {
        if let Some(loc) = resp.headers().get(reqwest::header::LOCATION) {
            if let Ok(loc_str) = loc.to_str() {
                // URL format: https://github.com/dondai44423/bladebro/releases/tag/v3.0.12
                if let Some(tag) = loc_str.rsplit('/').next() {
                    let tag = tag.strip_prefix('v').unwrap_or(tag);
                    return Ok(tag.to_string());
                }
            }
        }
        // Some CDNs/proxies eat the 301. Fall through to npm.
    }

    // 2. npm registry fallback.
    let npm_url = "https://registry.npmjs.org/bladebro/latest";
    if let Ok(resp) = client.get(npm_url).send().await {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(ver) = json.get("version").and_then(|v| v.as_str()) {
                    return Ok(ver.to_string());
                }
            }
        }
    }

    Err(BladeError::Other(
        "cannot reach GitHub or npm registry for version check".into(),
    ))
}

/// Construct a Release from a tag.
/// Download URLs are predictable:
///   https://github.com/{repo}/releases/download/v{tag}/bladebro-{os}-{arch}
/// Both new and legacy asset names are included so `find_asset` can match.
pub fn synthetic_release(tag: &str) -> Release {
    let tag_name = format!("v{tag}");
    let base = format!(
        "https://github.com/{}/releases/download/{}",
        super::GITHUB_REPO,
        tag_name
    );

    let mut assets = Vec::new();
    let ext = if cfg!(windows) { ".exe" } else { "" };

    // New naming: bladebro-{os}-{arch}
    assets.push(Asset {
        name: platform_asset_name(),
        browser_download_url: format!("{}/{}", base, platform_asset_name()),
        size: 0, // unknown, skip pre-flight disk check
    });

    // Legacy naming: bladebro-{os}-{x86_64|aarch64}
    let legacy = legacy_platform_asset_name();
    if legacy != platform_asset_name() {
        assets.push(Asset {
            name: legacy.clone(),
            browser_download_url: format!("{}/{}", base, legacy),
            size: 0,
        });
    }

    let _ = ext; // ext already baked into platform_asset_name / legacy name

    Release {
        tag_name,
        assets,
        body: None,
        draft: false,
        prerelease: false,
    }
}

/// Fetch the latest release.
///
/// Uses `fetch_latest_tag()` (github.com redirect + npm, no rate limits)
/// and constructs download URLs via `synthetic_release()`. No API key needed.
pub async fn fetch_latest() -> Result<Release> {
    let tag = fetch_latest_tag().await?;
    Ok(synthetic_release(&tag))
}

// ── version comparison ───────────────────────────────────────────

/// Parse a version string into comparable numeric components.
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

/// Three-way version comparison. Pre-release sorts below its release.
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

/// Is `latest` newer than `current`?
pub fn is_newer(latest: &str, current: &str) -> bool {
    compare_versions(latest, current) == std::cmp::Ordering::Greater
}

// ── platform asset naming ────────────────────────────────────────

/// The expected asset name for this platform.
/// Uses npm-consistent naming: `bladebro-{os}-{arch}{ext}`
///   Linux x86_64  → bladebro-linux-x64
///   Windows x64   → bladebro-windows-x64.exe
///   macOS Intel   → bladebro-darwin-x64
///   macOS ARM     → bladebro-darwin-arm64
pub fn platform_asset_name() -> String {
    let os = platform_os_name();
    let arch = platform_arch_name();
    let ext = if cfg!(windows) { ".exe" } else { "" };
    format!("bladebro-{os}-{arch}{ext}")
}

/// Legacy asset name (x86_64, macos) — for old releases pre-v3.0.3.
fn legacy_platform_asset_name() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        platform_os_name()
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

fn platform_os_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

fn platform_arch_name() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    }
}

/// Keyword sets for fuzzy asset matching.
/// Handles naming variations: x64/x86_64, arm64/aarch64, darwin/macos.
fn platform_keywords() -> (Vec<&'static str>, Vec<&'static str>) {
    let os = if cfg!(target_os = "linux") {
        vec!["linux"]
    } else if cfg!(target_os = "macos") {
        vec!["darwin", "macos"]
    } else if cfg!(target_os = "windows") {
        vec!["windows", "win32"]
    } else {
        vec![]
    };
    let arch = if cfg!(target_arch = "x86_64") {
        vec!["x64", "x86_64"]
    } else if cfg!(target_arch = "aarch64") {
        vec!["arm64", "aarch64"]
    } else {
        vec![]
    };
    (os, arch)
}

/// Human-readable platform label for error messages.
pub fn platform_label() -> String {
    let os = if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "ARM64"
    } else {
        "unknown"
    };
    format!("{os} ({arch})")
}

/// Find the asset for this platform in a release.
/// Strategy: exact match → legacy name → fuzzy keyword match.
pub fn find_asset(release: &Release) -> Option<&Asset> {
    // 1. Exact match with current naming.
    let name = platform_asset_name();
    if let Some(a) = release.assets.iter().find(|a| a.name == name) {
        return Some(a);
    }

    // 2. Exact match with legacy naming (pre-v3.0.3).
    let legacy = legacy_platform_asset_name();
    if legacy != name {
        if let Some(a) = release.assets.iter().find(|a| a.name == legacy) {
            return Some(a);
        }
    }

    // 3. Fuzzy: asset name contains both an OS and arch keyword.
    let (os_kws, arch_kws) = platform_keywords();
    if os_kws.is_empty() || arch_kws.is_empty() {
        return None;
    }
    release.assets.iter().find(|a| {
        let lower = a.name.to_lowercase();
        let os_match = os_kws.iter().any(|kw| lower.contains(kw));
        let arch_match = arch_kws.iter().any(|kw| lower.contains(kw));
        // Exclude checksums, text files, source archives.
        os_match
            && arch_match
            && !lower.ends_with(".sha256")
            && !lower.ends_with(".txt")
            && !lower.ends_with(".md")
            && !lower.ends_with(".tar.gz")
            && !lower.ends_with(".zip")
    })
}

/// List asset names in a release (for error messages).
pub fn asset_names(release: &Release) -> Vec<String> {
    release
        .assets
        .iter()
        .map(|a| a.name.clone())
        .collect()
}

// ── install method detection ─────────────────────────────────────

/// Detect if this binary was installed via npm.
/// Heuristic: the exe path contains "node_modules".
pub fn is_npm_install() -> bool {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains("node_modules"))
        .unwrap_or(false)
}

/// Detect if this binary was built from source.
/// Heuristic: the exe path contains "target/release" or "target/debug".
pub fn is_source_build() -> bool {
    std::env::current_exe()
        .map(|p| {
            let path = p.to_string_lossy();
            path.contains("target/release") || path.contains("target/debug")
        })
        .unwrap_or(false)
}

/// How was this binary installed?
pub fn install_method() -> &'static str {
    if is_npm_install() {
        "npm"
    } else if is_source_build() {
        "source"
    } else {
        "binary"
    }
}

// ── tests ─────────────────────────────────────────────────────────

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
        assert_eq!(compare_versions("2.0.0-dev", "2.0.0"), Less);
        assert_eq!(compare_versions("2.0.0", "2.0.0-dev"), Greater);
    }

    #[test]
    fn platform_asset_name_format() {
        let name = platform_asset_name();
        assert!(name.starts_with("bladebro-"));
        assert!(
            name.contains("x64") || name.contains("arm64"),
            "expected arch in name: {name}"
        );
    }

    #[test]
    fn find_asset_exact_match() {
        let release = Release {
            tag_name: "v3.0.3".into(),
            assets: vec![Asset {
                name: platform_asset_name(),
                browser_download_url: "https://example.com".into(),
                size: 5_600_000,
            }],
            body: None,
            draft: false,
            prerelease: false,
        };
        assert!(find_asset(&release).is_some());
    }

    #[test]
    fn find_asset_legacy_name() {
        let release = Release {
            tag_name: "v3.0.1".into(),
            assets: vec![Asset {
                name: legacy_platform_asset_name(),
                browser_download_url: "https://example.com".into(),
                size: 5_600_000,
            }],
            body: None,
            draft: false,
            prerelease: false,
        };
        assert!(find_asset(&release).is_some());
    }

    #[test]
    fn find_asset_fuzzy_match() {
        // Simulate a release with a differently-named asset.
        let release = Release {
            tag_name: "v3.0.3".into(),
            assets: vec![Asset {
                name: "bladebro_linux_amd64_v3".into(), // non-standard naming
                browser_download_url: "https://example.com".into(),
                size: 5_600_000,
            }],
            body: None,
            draft: false,
            prerelease: false,
        };
        // On Linux x86_64, "linux" matches but "amd64" doesn't match "x64" or "x86_64".
        // This should NOT match (amd64 is not in our keyword list).
        // Fix the test: use a name that contains our keywords.
        let release2 = Release {
            tag_name: "v3.0.3".into(),
            assets: vec![Asset {
                name: "myapp-linux-x64-binary".into(),
                browser_download_url: "https://example.com".into(),
                size: 5_600_000,
            }],
            body: None,
            draft: false,
            prerelease: false,
        };
        if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            assert!(find_asset(&release2).is_some());
        }
    }

    #[test]
    fn find_asset_empty() {
        let release = Release {
            tag_name: "v3.0.3".into(),
            assets: vec![],
            body: None,
            draft: false,
            prerelease: false,
        };
        assert!(find_asset(&release).is_none());
    }

    #[test]
    fn find_asset_excludes_checksums() {
        let release = Release {
            tag_name: "v3.0.3".into(),
            assets: vec![Asset {
                name: format!("{}.sha256", platform_asset_name()),
                browser_download_url: "https://example.com".into(),
                size: 100,
            }],
            body: None,
            draft: false,
            prerelease: false,
        };
        assert!(find_asset(&release).is_none());
    }
}
