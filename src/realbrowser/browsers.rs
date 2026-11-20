//! Browser discovery — per-OS candidate tables and `--version` brand inference.

use std::path::PathBuf;

/// Browser family — drives the CDP-hardening gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brand {
    /// Google Chrome (branded: Stable, Beta, Canary): refuses CDP on the
    /// default profile dir (M136+).
    Chrome,
    Chromium,
    UngoogledChromium,
    Brave,
    Helium,
    Edge,
    Vivaldi,
    Opera,
    Arc,
    Thorium,
}

impl Brand {
    pub fn as_str(self) -> &'static str {
        match self {
            Brand::Chrome => "Chrome",
            Brand::Chromium => "Chromium",
            Brand::UngoogledChromium => "Ungoogled Chromium",
            Brand::Brave => "Brave",
            Brand::Helium => "Helium",
            Brand::Edge => "Edge",
            Brand::Vivaldi => "Vivaldi",
            Brand::Opera => "Opera",
            Brand::Arc => "Arc",
            Brand::Thorium => "Thorium",
        }
    }
}

/// True when this brand is known to refuse `--remote-debugging-*` on the
/// default user-data dir — i.e. profile-mode on the default dir cannot work
/// and clone/attach are the only routes. Compiled in for
/// `GOOGLE_CHROME_BRANDING` only (chromium `remote_debugging_server.cc`),
/// so everything else must NOT be gated by default.
pub fn requires_non_default_dir(brand: Brand) -> bool {
    matches!(brand, Brand::Chrome)
}

/// Infer the brand from a `--version` output line
/// (e.g. "Google Chrome 151.0.…", "Chromium 151.0.…", "Brave Browser 1.7…").
pub fn brand_from_version(out: &str) -> Option<Brand> {
    let l = out.to_lowercase();
    if l.contains("google chrome") {
        Some(Brand::Chrome)
    } else if l.contains("brave") {
        Some(Brand::Brave)
    } else if l.contains("microsoft edge") || l.contains("msedge") {
        Some(Brand::Edge)
    } else if l.contains("vivaldi") {
        Some(Brand::Vivaldi)
    } else if l.contains("opera") {
        Some(Brand::Opera)
    } else if l.contains("helium") {
        Some(Brand::Helium)
    } else if l.contains("thorium") {
        Some(Brand::Thorium)
    } else if l.contains("ungoogled") {
        Some(Brand::UngoogledChromium)
    } else if l.contains("arc") && l.contains("chrom") {
        Some(Brand::Arc)
    } else if l.contains("chromium") {
        Some(Brand::Chromium)
    } else {
        None
    }
}

/// Best-effort brand inference from an executable path (used when
/// `rb use --binary` overrides discovery so Chrome's default-dir gate still
/// matches the overridden binary without spawning it).
pub fn brand_from_binary_path(path: &std::path::Path) -> Option<Brand> {
    let raw = path.to_string_lossy().to_lowercase();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("google-chrome")
        || raw.contains("google chrome")
        || raw.contains("google/chrome")
        || raw.contains("google\\chrome")
    {
        Some(Brand::Chrome)
    } else if name.contains("brave") || raw.contains("com.brave.browser") {
        Some(Brand::Brave)
    } else if name.contains("msedge")
        || name.contains("microsoft-edge")
        || raw.contains("microsoft edge")
        || raw.contains("com.microsoft.edge")
    {
        Some(Brand::Edge)
    } else if name.contains("vivaldi") || raw.contains("com.vivaldi.vivaldi") {
        Some(Brand::Vivaldi)
    } else if name.contains("opera") {
        Some(Brand::Opera)
    } else if name.contains("helium") {
        Some(Brand::Helium)
    } else if name.contains("thorium") {
        Some(Brand::Thorium)
    } else if name.contains("ungoogled") {
        Some(Brand::UngoogledChromium)
    } else if name == "arc" || name == "arc.exe" || raw.contains("arc.app") {
        Some(Brand::Arc)
    } else if name.contains("chromium") || name == "chrome" || name == "chrome.exe" {
        Some(Brand::Chromium)
    } else {
        None
    }
}

/// One installed browser.
#[derive(Clone, Debug)]
pub struct BrowserSpec {
    pub id: String,
    pub name: String,
    pub brand: Brand,
    /// Resolved binary (first existing candidate).
    pub binary: PathBuf,
    /// Profile root (first existing candidate; if none exists yet, the
    /// canonical first candidate for display).
    pub profile_root: PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn path_bins(names: &[&str]) -> Vec<PathBuf> {
    let mut native = Vec::new();
    let mut snap = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for name in names {
            for dir in std::env::split_paths(&path) {
                let full = dir.join(name);
                let s = full.to_string_lossy();
                if s.starts_with("/snap/") || s.starts_with("/var/lib/snapd/") {
                    snap.push(full);
                } else {
                    native.push(full);
                }
            }
        }
    }
    native.extend(snap);
    native
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn flatpak_export_candidates(home: &std::path::Path, app_id: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let user_root = std::env::var_os("FLATPAK_USER_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"))
                .join("flatpak")
        });
    let sys_root = std::env::var_os("FLATPAK_SYSTEM_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/flatpak"));
    out.push(user_root.join("exports/bin").join(app_id));
    out.push(sys_root.join("exports/bin").join(app_id));
    out
}

/// Candidate browser installs for this OS. Pure-ish (paths only); the
/// caller filters by existence. Native binaries come first, then PATH
/// entries (non-snap before snap), then Flatpak exports (launched via
/// `flatpak run` with `--filesystem=<user_data_dir>` in `Browser::launch_real`).
#[allow(clippy::type_complexity)]
fn candidates() -> Vec<(
    &'static str,
    &'static str,
    Brand,
    Vec<PathBuf>,
    Vec<PathBuf>,
)> {
    // Used by the Linux + macOS candidate tables; Windows builds its paths
    // from LOCALAPPDATA/APPDATA/PROGRAMFILES instead.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
    let home = crate::platform::home_dir();
    let mut out: Vec<(
        &'static str,
        &'static str,
        Brand,
        Vec<PathBuf>,
        Vec<PathBuf>,
    )> = Vec::new();

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let mut push = |id: &'static str,
                        name: &'static str,
                        brand: Brand,
                        mut bins: Vec<PathBuf>,
                        path_names: &[&str],
                        flatpak_app: Option<&str>,
                        roots: Vec<PathBuf>| {
            bins.extend(path_bins(path_names));
            if let Some(app) = flatpak_app {
                bins.extend(flatpak_export_candidates(&home, app));
            }
            out.push((id, name, brand, bins, roots));
        };
        push(
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![
                PathBuf::from("/usr/sbin/chromium"),
                PathBuf::from("/usr/bin/chromium"),
                PathBuf::from("/usr/bin/chromium-browser"),
                PathBuf::from("/run/current-system/sw/bin/chromium"),
            ],
            &["chromium", "chromium-browser"],
            Some("org.chromium.Chromium"),
            vec![
                xdg.join("chromium"),
                home.join("snap/chromium/common/chromium"),
                home.join(".var/app/org.chromium.Chromium/config/chromium"),
            ],
        );
        push(
            "chrome",
            "Google Chrome",
            Brand::Chrome,
            vec![
                PathBuf::from("/usr/bin/google-chrome"),
                PathBuf::from("/usr/bin/google-chrome-stable"),
                PathBuf::from("/opt/google/chrome/chrome"),
                PathBuf::from("/run/current-system/sw/bin/google-chrome"),
            ],
            &["google-chrome", "google-chrome-stable"],
            Some("com.google.Chrome"),
            vec![
                xdg.join("google-chrome"),
                home.join(".var/app/com.google.Chrome/config/google-chrome"),
            ],
        );
        push(
            "chrome-beta",
            "Google Chrome Beta",
            Brand::Chrome,
            vec![
                PathBuf::from("/usr/bin/google-chrome-beta"),
                PathBuf::from("/opt/google/chrome-beta/chrome"),
            ],
            &["google-chrome-beta"],
            None,
            vec![xdg.join("google-chrome-beta")],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                PathBuf::from("/usr/bin/brave"),
                PathBuf::from("/usr/bin/brave-browser"),
                PathBuf::from("/opt/brave.com/brave/brave"),
            ],
            &["brave", "brave-browser"],
            Some("com.brave.Browser"),
            vec![
                xdg.join("BraveSoftware/Brave-Browser"),
                home.join(".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
            ],
        );
        // Official packages use /opt/helium; PATH also covers Nix and
        // user-local tarball wrappers. AppImages with arbitrary names use
        // the existing --binary override (their profile is still discovered).
        let helium_config = std::env::var_os("HELIUM_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| xdg.clone());
        push(
            "helium",
            "Helium",
            Brand::Helium,
            vec![
                PathBuf::from("/usr/bin/helium"),
                PathBuf::from("/usr/local/bin/helium"),
                PathBuf::from("/opt/helium/helium-wrapper"),
                PathBuf::from("/opt/helium/helium"),
            ],
            &["helium"],
            None,
            vec![helium_config.join("net.imput.helium")],
        );
        push(
            "edge",
            "Microsoft Edge",
            Brand::Edge,
            vec![
                PathBuf::from("/usr/bin/microsoft-edge"),
                PathBuf::from("/usr/bin/microsoft-edge-stable"),
                PathBuf::from("/opt/microsoft/msedge/msedge"),
            ],
            &["microsoft-edge", "microsoft-edge-stable"],
            Some("com.microsoft.Edge"),
            vec![
                xdg.join("microsoft-edge"),
                home.join(".var/app/com.microsoft.Edge/config/microsoft-edge"),
            ],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![
                PathBuf::from("/usr/bin/vivaldi"),
                PathBuf::from("/usr/bin/vivaldi-stable"),
            ],
            &["vivaldi", "vivaldi-stable"],
            Some("com.vivaldi.Vivaldi"),
            vec![
                xdg.join("vivaldi"),
                home.join(".var/app/com.vivaldi.Vivaldi/config/vivaldi"),
            ],
        );
        push(
            "opera",
            "Opera",
            Brand::Opera,
            vec![PathBuf::from("/usr/bin/opera")],
            &["opera"],
            None,
            vec![xdg.join("opera")],
        );
        push(
            "ungoogled-chromium",
            "Ungoogled Chromium",
            Brand::UngoogledChromium,
            vec![PathBuf::from("/usr/bin/ungoogled-chromium")],
            &["ungoogled-chromium"],
            Some("io.github.ungoogled_software.ungoogled_chromium"),
            vec![
                home.join(
                    ".var/app/io.github.ungoogled_software.ungoogled_chromium/config/chromium",
                ),
                xdg.join("ungoogled-chromium"),
            ],
        );
        push(
            "thorium",
            "Thorium",
            Brand::Thorium,
            vec![
                PathBuf::from("/usr/bin/thorium-browser"),
                PathBuf::from("/opt/chromium.org/thorium/thorium-browser"),
            ],
            &["thorium-browser", "thorium"],
            None,
            vec![xdg.join("thorium")],
        );
    }

    #[cfg(target_os = "macos")]
    {
        let apps = PathBuf::from("/Applications");
        let user_apps = home.join("Applications");
        let sup = home.join("Library/Application Support");
        let mut push = |id: &'static str,
                        name: &'static str,
                        brand: Brand,
                        bins: Vec<PathBuf>,
                        roots: Vec<PathBuf>| {
            out.push((id, name, brand, bins, roots));
        };
        push(
            "chrome",
            "Google Chrome",
            Brand::Chrome,
            vec![
                apps.join("Google Chrome.app/Contents/MacOS/Google Chrome"),
                user_apps.join("Google Chrome.app/Contents/MacOS/Google Chrome"),
            ],
            vec![sup.join("Google/Chrome")],
        );
        push(
            "chrome-beta",
            "Google Chrome Beta",
            Brand::Chrome,
            vec![
                apps.join("Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta"),
                user_apps.join("Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta"),
            ],
            vec![sup.join("Google/Chrome Beta")],
        );
        push(
            "chrome-canary",
            "Google Chrome Canary",
            Brand::Chrome,
            vec![
                apps.join("Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"),
                user_apps.join("Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"),
            ],
            vec![sup.join("Google/Chrome Canary")],
        );
        push(
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![
                apps.join("Chromium.app/Contents/MacOS/Chromium"),
                user_apps.join("Chromium.app/Contents/MacOS/Chromium"),
                PathBuf::from("/opt/homebrew/bin/chromium"),
                PathBuf::from("/usr/local/bin/chromium"),
            ],
            vec![sup.join("Chromium")],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                apps.join("Brave Browser.app/Contents/MacOS/Brave Browser"),
                user_apps.join("Brave Browser.app/Contents/MacOS/Brave Browser"),
            ],
            vec![sup.join("BraveSoftware/Brave-Browser")],
        );
        push(
            "helium",
            "Helium",
            Brand::Helium,
            vec![
                apps.join("Helium.app/Contents/MacOS/Helium"),
                user_apps.join("Helium.app/Contents/MacOS/Helium"),
            ],
            vec![sup.join("net.imput.helium")],
        );
        push(
            "arc",
            "Arc",
            Brand::Arc,
            vec![
                apps.join("Arc.app/Contents/MacOS/Arc"),
                user_apps.join("Arc.app/Contents/MacOS/Arc"),
            ],
            vec![sup.join("Arc/User Data")],
        );
        push(
            "edge",
            "Microsoft Edge",
            Brand::Edge,
            vec![
                apps.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
                user_apps.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            ],
            vec![sup.join("Microsoft Edge")],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![
                apps.join("Vivaldi.app/Contents/MacOS/Vivaldi"),
                user_apps.join("Vivaldi.app/Contents/MacOS/Vivaldi"),
            ],
            vec![sup.join("Vivaldi")],
        );
        push(
            "opera",
            "Opera",
            Brand::Opera,
            vec![
                apps.join("Opera.app/Contents/MacOS/Opera"),
                user_apps.join("Opera.app/Contents/MacOS/Opera"),
            ],
            vec![sup.join("com.operasoftware.Opera")],
        );
        push(
            "opera-gx",
            "Opera GX",
            Brand::Opera,
            vec![
                apps.join("Opera GX.app/Contents/MacOS/Opera"),
                user_apps.join("Opera GX.app/Contents/MacOS/Opera"),
            ],
            vec![sup.join("com.operasoftware.OperaGX")],
        );
        push(
            "thorium",
            "Thorium",
            Brand::Thorium,
            vec![
                apps.join("Thorium.app/Contents/MacOS/Thorium"),
                user_apps.join("Thorium.app/Contents/MacOS/Thorium"),
            ],
            vec![sup.join("Thorium")],
        );
    }

    #[cfg(target_os = "windows")]
    {
        // Env-var roots: a missing variable must never produce a RELATIVE
        // candidate (`PathBuf::from("").join(x)` is cwd-relative and could
        // accidentally exist). Absent base → no candidate.
        let ev = |var: &str| {
            std::env::var(var)
                .ok()
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
        };
        let local = ev("LOCALAPPDATA");
        let roaming = ev("APPDATA");
        let pf = ev("PROGRAMFILES");
        let pf86 = ev("PROGRAMFILES(X86)");
        let j = |b: &Option<PathBuf>, rel: &str| b.as_ref().map(|b| b.join(rel));
        let mut push = |id: &'static str,
                        name: &'static str,
                        brand: Brand,
                        bins: Vec<Option<PathBuf>>,
                        roots: Vec<Option<PathBuf>>| {
            out.push((
                id,
                name,
                brand,
                bins.into_iter().flatten().collect(),
                roots.into_iter().flatten().collect(),
            ));
        };
        push(
            "chrome",
            "Google Chrome",
            Brand::Chrome,
            vec![
                j(&local, "Google/Chrome/Application/chrome.exe"),
                j(&pf, "Google/Chrome/Application/chrome.exe"),
                j(&pf86, "Google/Chrome/Application/chrome.exe"),
            ],
            vec![j(&local, "Google/Chrome/User Data")],
        );
        push(
            "chrome-beta",
            "Google Chrome Beta",
            Brand::Chrome,
            vec![
                j(&local, "Google/Chrome Beta/Application/chrome.exe"),
                j(&pf, "Google/Chrome Beta/Application/chrome.exe"),
                j(&pf86, "Google/Chrome Beta/Application/chrome.exe"),
            ],
            vec![j(&local, "Google/Chrome Beta/User Data")],
        );
        push(
            "chrome-canary",
            "Google Chrome Canary",
            Brand::Chrome,
            vec![j(&local, "Google/Chrome SxS/Application/chrome.exe")],
            vec![j(&local, "Google/Chrome SxS/User Data")],
        );
        push(
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![
                j(&local, "Chromium/Application/chrome.exe"),
                j(&pf, "Chromium/Application/chrome.exe"),
                j(&pf86, "Chromium/Application/chrome.exe"),
            ],
            vec![j(&local, "Chromium/User Data")],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                j(&local, "BraveSoftware/Brave-Browser/Application/brave.exe"),
                j(&pf, "BraveSoftware/Brave-Browser/Application/brave.exe"),
                j(&pf86, "BraveSoftware/Brave-Browser/Application/brave.exe"),
            ],
            vec![j(&local, "BraveSoftware/Brave-Browser/User Data")],
        );
        push(
            "helium",
            "Helium",
            Brand::Helium,
            vec![
                j(&local, "imput/Helium/Application/chrome.exe"),
                j(&pf, "imput/Helium/Application/chrome.exe"),
                j(&pf86, "imput/Helium/Application/chrome.exe"),
            ],
            vec![j(&local, "imput/Helium/User Data")],
        );
        push(
            "edge",
            "Microsoft Edge",
            Brand::Edge,
            vec![
                j(&pf86, "Microsoft/Edge/Application/msedge.exe"),
                j(&pf, "Microsoft/Edge/Application/msedge.exe"),
                j(&local, "Microsoft/Edge/Application/msedge.exe"),
            ],
            vec![j(&local, "Microsoft/Edge/User Data")],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![
                j(&local, "Vivaldi/Application/vivaldi.exe"),
                j(&pf, "Vivaldi/Application/vivaldi.exe"),
            ],
            vec![j(&local, "Vivaldi/User Data")],
        );
        push(
            "opera",
            "Opera",
            Brand::Opera,
            vec![
                j(&local, "Programs/Opera/opera.exe"),
                j(&pf, "Programs/Opera/opera.exe"),
            ],
            // Opera's profile lives in Roaming (APPDATA) on Windows — its
            // LOCALAPPDATA entry is the install location, not the profile.
            vec![
                j(&roaming, "Opera Software/Opera Stable"),
                j(&local, "Opera Software/Opera Stable"),
            ],
        );
        push(
            "opera-gx",
            "Opera GX",
            Brand::Opera,
            vec![
                j(&local, "Programs/Opera GX/opera.exe"),
                j(&pf, "Programs/Opera GX/opera.exe"),
            ],
            vec![
                j(&roaming, "Opera Software/Opera GX Stable"),
                j(&local, "Opera Software/Opera GX Stable"),
            ],
        );
        push(
            "thorium",
            "Thorium",
            Brand::Thorium,
            vec![
                j(&local, "Thorium/Application/thorium.exe"),
                j(&pf, "Thorium/Application/thorium.exe"),
            ],
            vec![j(&local, "Thorium/User Data")],
        );
    }

    out
}

/// Installed browsers (binary OR profile root present), in table order.
pub fn discover() -> Vec<BrowserSpec> {
    let mut found = Vec::new();
    for (id, name, brand, bins, roots) in candidates() {
        let binary = bins
            .iter()
            .find_map(|p| super::validate_binary_override(&p.to_string_lossy()).ok());
        let root = roots.iter().find(|p| p.is_dir()).cloned();
        let root = match (root, binary.as_ref()) {
            (Some(r), _) => r,
            // Binary without a profile root: show the canonical path. Never
            // index blindly — a platform table may legitimately leave the
            // root list empty (e.g. a missing %%APPDATA%% on Windows).
            (None, Some(_)) => roots.first().cloned().unwrap_or_default(),
            (None, None) => continue,
        };
        found.push(BrowserSpec {
            id: id.to_string(),
            name: name.to_string(),
            brand,
            binary: binary.unwrap_or_default(),
            profile_root: root,
        });
    }
    found
}

/// Find one browser by id.
pub fn find_browser(id: &str) -> Option<BrowserSpec> {
    discover().into_iter().find(|b| b.id == id)
}
