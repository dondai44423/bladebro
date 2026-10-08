//! Browser discovery — per-OS candidate tables and `--version` brand inference.

use std::path::PathBuf;

/// Browser family — drives the CDP-hardening gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brand {
    /// Google Chrome (branded): refuses CDP on the default profile dir (M136+).
    Chrome,
    Chromium,
    Brave,
    Helium,
    Edge,
    Vivaldi,
    Opera,
}

impl Brand {
    pub fn as_str(self) -> &'static str {
        match self {
            Brand::Chrome => "Chrome",
            Brand::Chromium => "Chromium",
            Brand::Brave => "Brave",
            Brand::Helium => "Helium",
            Brand::Edge => "Edge",
            Brand::Vivaldi => "Vivaldi",
            Brand::Opera => "Opera",
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
    } else if l.contains("chromium") {
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

/// Candidate browser installs for this OS. Pure-ish (paths only); the
/// caller filters by existence. Flatpak is deliberately NOT a binary
/// candidate: `/usr/bin/flatpak` is a launcher, not a browser — spawning
/// it with Chrome flags just fails. Flatpak profile roots are listed so a
/// flatpak system can still work via `rb use --binary <wrapper>`.
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home = crate::platform::home_dir();
    let mut out: Vec<(
        &'static str,
        &'static str,
        Brand,
        Vec<PathBuf>,
        Vec<PathBuf>,
    )> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let mut push = |id: &'static str,
                        name: &'static str,
                        brand: Brand,
                        bins: Vec<PathBuf>,
                        roots: Vec<PathBuf>| {
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
                PathBuf::from("/snap/bin/chromium"),
            ],
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
            ],
            vec![
                xdg.join("google-chrome"),
                home.join(".var/app/com.google.Chrome/config/google-chrome"),
            ],
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
            vec![
                xdg.join("BraveSoftware/Brave-Browser"),
                home.join(".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
            ],
        );
        // Official packages use /opt/helium; PATH also covers Nix and
        // user-local tarball wrappers. AppImages with arbitrary names use
        // the existing --binary override (their profile is still discovered).
        let mut helium_bins = vec![
            PathBuf::from("/usr/bin/helium"),
            PathBuf::from("/usr/local/bin/helium"),
            PathBuf::from("/opt/helium/helium-wrapper"),
            PathBuf::from("/opt/helium/helium"),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            helium_bins.extend(std::env::split_paths(&path).map(|p| p.join("helium")));
        }
        let helium_config = std::env::var_os("HELIUM_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| xdg.clone());
        push(
            "helium",
            "Helium",
            Brand::Helium,
            helium_bins,
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
            vec![xdg.join("opera")],
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
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![
                apps.join("Chromium.app/Contents/MacOS/Chromium"),
                user_apps.join("Chromium.app/Contents/MacOS/Chromium"),
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
            "chromium",
            "Chromium",
            Brand::Chromium,
            vec![j(&local, "Chromium/Application/chrome.exe")],
            vec![j(&local, "Chromium/User Data")],
        );
        push(
            "brave",
            "Brave",
            Brand::Brave,
            vec![
                j(&local, "BraveSoftware/Brave-Browser/Application/brave.exe"),
                j(&pf, "BraveSoftware/Brave-Browser/Application/brave.exe"),
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
            ],
            vec![j(&local, "Microsoft/Edge/User Data")],
        );
        push(
            "vivaldi",
            "Vivaldi",
            Brand::Vivaldi,
            vec![j(&local, "Vivaldi/Application/vivaldi.exe")],
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
