//! Version checking against GitHub releases + npm registry, with shared HTTP
//! connection pooling and accurate cross-platform install-channel detection.

use std::sync::OnceLock;
use std::time::Duration;

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
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

/// Shared pooled HTTP client for downloads & checksum fetches: builds the
/// rustls root store once per process, reuses keep-alive connections, and
/// enforces an idle read timeout (45s) so stalled connections recover quickly
/// via Range resume instead of hanging for minutes.
pub(crate) fn shared_download_client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(concat!("bladebro-updater/", env!("CARGO_PKG_VERSION")))
                .tcp_nodelay(true)
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(45))
                .pool_idle_timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| format!("http client: {e}"))
        })
        .as_ref()
        .map_err(|e| BladeError::Other(e.clone()))
}

/// Shared no-redirect client for fast `releases/latest` `HEAD` probes.
fn shared_probe_client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(concat!("bladebro-updater/", env!("CARGO_PKG_VERSION")))
                .tcp_nodelay(true)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| format!("http client: {e}"))
        })
        .as_ref()
        .map_err(|e| BladeError::Other(e.clone()))
}

/// Fetch the latest version tag from non-rate-limited sources.
///
/// 1. `HEAD github.com/.../releases/latest` — 302 redirect, zero body bytes,
///    parse tag from `Location` (falls back to `GET` only if a proxy blocks
///    `HEAD` with 405/501).
/// 2. npm registry fallback — JSON with the latest published version.
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

    let client = shared_probe_client()?;
    let url = format!("https://github.com/{}/releases/latest", super::GITHUB_REPO);

    // 1. Fast HEAD redirect check (avoids downloading any HTML body).
    let mut try_get = false;
    if let Ok(resp) = client.head(&url).send().await {
        let status = resp.status();
        if let Some(tag) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|loc| loc.to_str().ok())
            .and_then(|loc| tag_from_latest_redirect(status, loc))
        {
            return Ok(tag);
        }
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            || status == reqwest::StatusCode::NOT_IMPLEMENTED
        {
            try_get = true;
        }
    }

    if try_get {
        if let Ok(resp) = client.get(&url).send().await {
            let status = resp.status();
            if let Some(tag) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|loc| loc.to_str().ok())
                .and_then(|loc| tag_from_latest_redirect(status, loc))
            {
                return Ok(tag);
            }
        }
    }

    // 2. npm registry fallback (works even if GitHub is firewalled or a proxy strips 302).
    let npm_url = "https://registry.npmjs.org/bladebro/latest";
    if let Ok(resp) = client.get(npm_url).send().await {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(ver) = json
                    .get("version")
                    .and_then(|v| v.as_str())
                    .filter(|v| is_version_tag(v))
                {
                    return Ok(ver.to_string());
                }
            }
        }
    }

    Err(BladeError::Other(
        "cannot reach GitHub or npm registry for version check".into(),
    ))
}

/// Extract the release tag from the `releases/latest` redirect. Only a 3xx
/// pointing at `…/releases/tag/<version>` names a release. GitHub answers a
/// repo with no published release with a redirect to `…/releases`; taking
/// the last path segment of any Location turned that into the tag
/// "releases", which parsed as no version at all, compared as older than
/// every build ("ahead of the latest release") and made the checksum
/// optional. Anything else falls through to the npm registry.
pub(crate) fn tag_from_latest_redirect(
    status: reqwest::StatusCode,
    location: &str,
) -> Option<String> {
    if !status.is_redirection() {
        return None;
    }
    // URL format: https://github.com/dondai44423/bladebro/releases/tag/v3.0.12
    let path = location.split(['?', '#']).next()?;
    let (_, tag) = path.rsplit_once("/releases/tag/")?;
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    is_version_tag(tag).then(|| tag.to_string())
}

/// Is `v` a release version — `[v]N(.N)*` with an optional `-pre` suffix?
/// URL segments like "releases" or "latest" and empty strings are not.
pub(crate) fn is_version_tag(v: &str) -> bool {
    let core = v.strip_prefix('v').unwrap_or(v);
    let core = core.split_once('-').map_or(core, |(n, _)| n);
    !core.is_empty()
        && core.split('.').all(|n| {
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && n.parse::<u64>().is_ok()
        })
}

/// Construct a Release from a tag.
/// Download URLs are predictable:
///   https://github.com/{repo}/releases/download/v{tag}/bladebro-{os}-{arch}
/// Both new and legacy asset names are included so `find_asset` (and 404
/// fallback in `download_binary`) can match.
pub fn synthetic_release(tag: &str) -> Release {
    let clean = tag.strip_prefix('v').unwrap_or(tag);
    let tag_name = format!("v{clean}");
    let base = format!(
        "https://github.com/{}/releases/download/{}",
        super::GITHUB_REPO,
        tag_name
    );

    let mut assets = Vec::new();

    // New naming: bladebro-{os}-{arch}
    let primary = platform_asset_name();
    assets.push(Asset {
        browser_download_url: format!("{base}/{primary}"),
        name: primary,
        size: 0, // learned from Content-Length on response headers
    });

    // Legacy naming: bladebro-{os}-{x86_64|aarch64}
    let legacy = legacy_platform_asset_name();
    if legacy != platform_asset_name() {
        assets.push(Asset {
            browser_download_url: format!("{base}/{legacy}"),
            name: legacy,
            size: 0,
        });
    }

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
/// Uses `fetch_latest_tag()` (github.com HEAD redirect + npm, no rate limits)
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
    if cfg!(any(target_os = "linux", target_os = "android")) {
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
    let os = if cfg!(any(target_os = "linux", target_os = "android")) {
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

/// Human-readable platform label for display and error messages.
pub fn platform_label() -> String {
    let os = if cfg!(target_os = "android") {
        "Android"
    } else if cfg!(target_os = "linux") {
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

/// Find the primary asset for this platform in a release.
/// Strategy: exact match → legacy name → fuzzy keyword match.
pub fn find_asset(release: &Release) -> Option<&Asset> {
    candidate_assets(release).into_iter().next()
}

/// Ordered candidate assets for this platform: exact current name first,
/// followed by legacy name (so a 404 on an older tag falls back cleanly),
/// followed by fuzzy keyword matches.
pub(crate) fn candidate_assets(release: &Release) -> Vec<&Asset> {
    let mut out: Vec<&Asset> = Vec::new();
    let name = platform_asset_name();
    if let Some(a) = release.assets.iter().find(|a| a.name == name) {
        out.push(a);
    }

    let legacy = legacy_platform_asset_name();
    if legacy != name {
        if let Some(a) = release.assets.iter().find(|a| a.name == legacy) {
            if !out.iter().any(|x| x.name == a.name) {
                out.push(a);
            }
        }
    }

    if out.is_empty() {
        let (os_kws, arch_kws) = platform_keywords();
        if !os_kws.is_empty() && !arch_kws.is_empty() {
            for a in &release.assets {
                let lower = a.name.to_lowercase();
                let os_match = os_kws.iter().any(|kw| lower.contains(kw));
                let arch_match = arch_kws.iter().any(|kw| lower.contains(kw));
                if os_match
                    && arch_match
                    && !lower.ends_with(".sha256")
                    && !lower.ends_with(".txt")
                    && !lower.ends_with(".md")
                    && !lower.ends_with(".tar.gz")
                    && !lower.ends_with(".zip")
                {
                    out.push(a);
                }
            }
        }
    }

    out
}

/// List asset names in a release (for error messages).
pub fn asset_names(release: &Release) -> Vec<String> {
    release.assets.iter().map(|a| a.name.clone()).collect()
}

// ── install method detection ─────────────────────────────────────

/// Pure cross-platform path classifier for install method detection.
/// Normalizes `\` → `/` and matches case-insensitively so Windows paths work
/// identically to Unix paths.
pub(crate) fn detect_install_method(raw_path: &str) -> &'static str {
    let norm = raw_path.replace('\\', "/").to_ascii_lowercase();
    if norm.contains("/node_modules/") || norm.ends_with("/node_modules") {
        if norm.contains("/pnpm/") || norm.contains("/.pnpm/") {
            return "pnpm";
        }
        if norm.contains("/.bun/") || norm.contains("/bun/install/") {
            return "bun";
        }
        if norm.contains("/yarn/") || norm.contains("/.yarn/") {
            return "yarn";
        }
        return "npm";
    }
    if norm.contains("/cellar/bladebro/") || norm.contains("/homebrew/cellar/") {
        return "homebrew";
    }
    if norm.contains("/.cargo/bin/") {
        return "cargo";
    }
    if norm.contains("/target/release/")
        || norm.contains("/target/debug/")
        || norm.contains("/target/gate/")
    {
        return "source";
    }
    "binary"
}

/// Detect if this binary was installed via a JS package manager (npm/pnpm/bun/yarn).
pub fn is_npm_install() -> bool {
    matches!(install_method(), "npm" | "pnpm" | "bun" | "yarn")
}

/// Detect if this binary is managed by an external package manager whose files
/// should not be overwritten in-place without `--force`.
pub fn is_package_managed() -> bool {
    matches!(
        install_method(),
        "npm" | "pnpm" | "bun" | "yarn" | "homebrew"
    )
}

/// Detect if this binary was built from source.
pub fn is_source_build() -> bool {
    install_method() == "source"
}

/// How was this binary installed?
pub fn install_method() -> &'static str {
    std::env::current_exe()
        .map(|p| detect_install_method(&p.to_string_lossy()))
        .unwrap_or("binary")
}

/// Human-friendly channel name for display cards and doctor rows.
pub fn install_channel_label() -> &'static str {
    match install_method() {
        "npm" => "npm",
        "pnpm" => "pnpm",
        "bun" => "bun",
        "yarn" => "yarn",
        "homebrew" => "Homebrew",
        "cargo" => "Cargo",
        "source" => "source build",
        _ => "standalone binary",
    }
}

/// Recommended upgrade command for the current install channel.
pub fn update_command_hint() -> &'static str {
    if cfg!(target_os = "android") {
        return "update via your package manager or rebuild from source";
    }
    match install_method() {
        "npm" => "npm update -g bladebro",
        "pnpm" => "pnpm update -g bladebro",
        "bun" => "bun update -g bladebro",
        "yarn" => "yarn global upgrade bladebro",
        "homebrew" => "brew upgrade bladebro",
        "source" => "git pull && cargo build --release",
        _ => "bladebro -u",
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
    fn latest_redirect_yields_only_real_version_tags() {
        use reqwest::StatusCode;
        let repo = "https://github.com/dondai44423/bladebro";
        let found = StatusCode::FOUND;
        assert_eq!(
            tag_from_latest_redirect(found, &format!("{repo}/releases/tag/v4.6.7")).as_deref(),
            Some("4.6.7")
        );
        assert_eq!(
            tag_from_latest_redirect(
                StatusCode::MOVED_PERMANENTLY,
                &format!("{repo}/releases/tag/v4.7.0-rc.1")
            )
            .as_deref(),
            Some("4.7.0-rc.1")
        );
        assert_eq!(
            tag_from_latest_redirect(found, &format!("{repo}/releases")),
            None
        );
        assert_eq!(
            tag_from_latest_redirect(
                StatusCode::MOVED_PERMANENTLY,
                "https://github.com/other/bladebro/releases/latest"
            ),
            None
        );
        assert_eq!(
            tag_from_latest_redirect(
                found,
                "https://github.com/login?return_to=%2Freleases%2Ftag%2Fv9.9.9"
            ),
            None
        );
        for status in [
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::NOT_FOUND,
        ] {
            assert_eq!(
                tag_from_latest_redirect(status, &format!("{repo}/releases/tag/v4.6.7")),
                None,
                "{status}"
            );
        }
    }

    #[test]
    fn version_tags_are_numeric() {
        for ok in ["4.6.7", "v4.6.7", "4.7.0-rc.1", "10"] {
            assert!(is_version_tag(ok), "{ok:?}");
        }
        for bad in [
            "releases", "latest", "", "v", "4..7", "4.6.x", "+4.6.7", "4.6.7 ",
        ] {
            assert!(!is_version_tag(bad), "{bad:?}");
        }
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
    fn install_method_detection_handles_windows_and_all_package_managers() {
        assert_eq!(
            detect_install_method(
                r"C:\Users\don\AppData\Roaming\npm\node_modules\bladebro-windows-x64\bladebro.exe"
            ),
            "npm"
        );
        assert_eq!(
            detect_install_method(
                "/home/don/.local/share/pnpm/global/5/node_modules/bladebro-linux-x64/bladebro"
            ),
            "pnpm"
        );
        assert_eq!(
            detect_install_method(
                "/home/don/.bun/install/global/node_modules/bladebro-linux-x64/bladebro"
            ),
            "bun"
        );
        assert_eq!(
            detect_install_method("/opt/homebrew/Cellar/bladebro/4.8.0/bin/bladebro"),
            "homebrew"
        );
        assert_eq!(
            detect_install_method(r"C:\src\blade\target\release\bladebro.exe"),
            "source"
        );
        assert_eq!(
            detect_install_method("/home/don/blade/target/gate/bladebro"),
            "source"
        );
        assert_eq!(
            detect_install_method("/home/don/.cargo/bin/bladebro"),
            "cargo"
        );
        assert_eq!(detect_install_method("/usr/local/bin/bladebro"), "binary");
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
