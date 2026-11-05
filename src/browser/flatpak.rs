//! Flatpak browsers (Linux). On read-only-root distros — SteamOS, Bazzite,
//! Fedora Silverblue — Flatpak is the supported way to install a browser,
//! and `pacman`/`apt` are not an option. An installed app is reachable
//! through its export, `<installation>/exports/bin/<app-id>`: a launcher
//! script around `flatpak run`. Discovery checks the known Chromium-family
//! app ids after every native location.

use std::path::PathBuf;

use super::discover::is_executable;

/// Chromium-family Flatpak apps, in preference order (Chromium first, as in
/// native discovery).
const FLATPAK_BROWSERS: &[&str] = &[
    "org.chromium.Chromium",
    "com.google.Chrome",
    "com.brave.Browser",
    "com.microsoft.Edge",
    "com.vivaldi.Vivaldi",
    "io.github.ungoogled_software.ungoogled_chromium",
];

/// Installation roots as `flatpak` itself resolves them: the per-user one
/// (`FLATPAK_USER_DIR`, else `$XDG_DATA_HOME/flatpak`, else
/// `~/.local/share/flatpak`) before the system one (`FLATPAK_SYSTEM_DIR`,
/// else `/var/lib/flatpak`) — `flatpak run` also prefers a user install.
fn installations(env: &dyn Fn(&str) -> Option<String>) -> [Option<PathBuf>; 2] {
    let var = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    let user = var("FLATPAK_USER_DIR").or_else(|| {
        var("XDG_DATA_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| var("HOME").map(|home| home.join(".local/share")))
            .map(|data| data.join("flatpak"))
    });
    let system = var("FLATPAK_SYSTEM_DIR").unwrap_or_else(|| PathBuf::from("/var/lib/flatpak"));
    [user, Some(system)]
}

/// The export of the first installed Chromium-family Flatpak app.
pub(super) fn find_flatpak_browser(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let roots = installations(env);
    FLATPAK_BROWSERS.iter().find_map(|app| {
        roots
            .iter()
            .flatten()
            .map(|root| root.join("exports").join("bin").join(app))
            .find(|export| is_executable(export))
            .map(|export| export.to_string_lossy().into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn export(root: &Path, app: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = root.join("exports/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join(app);
        std::fs::write(&path, format!("#!/bin/sh\nexec flatpak run {app} \"$@\"\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// SteamOS ships no native Chromium and a read-only root: a Flatpak
    /// browser is the only one there, and discovery must find it.
    #[test]
    fn flatpak_browsers_are_discovered_in_preference_order() {
        let root = std::env::temp_dir().join(format!("blade-flatpak-{}", std::process::id()));
        let user = root.join("user");
        let system = root.join("system");
        let vars: HashMap<&str, String> = HashMap::from([
            ("FLATPAK_USER_DIR", user.display().to_string()),
            ("FLATPAK_SYSTEM_DIR", system.display().to_string()),
            ("HOME", root.join("home").display().to_string()),
        ]);
        let env = |key: &str| vars.get(key).cloned();

        assert_eq!(find_flatpak_browser(&env), None, "nothing installed");
        let brave = export(&user, "com.brave.Browser", 0o755);
        export(&system, "com.google.Chrome", 0o644); // not executable: skipped
        assert_eq!(
            find_flatpak_browser(&env),
            Some(brave.display().to_string())
        );
        let chromium = export(&system, "org.chromium.Chromium", 0o755);
        assert_eq!(
            find_flatpak_browser(&env),
            Some(chromium.display().to_string()),
            "app preference wins over installation order"
        );
        let user_chromium = export(&user, "org.chromium.Chromium", 0o755);
        assert_eq!(
            find_flatpak_browser(&env),
            Some(user_chromium.display().to_string()),
            "a per-user install shadows the system one, as for `flatpak run`"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn user_installation_follows_flatpak_resolution() {
        let env_of = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        let user = |env: &dyn Fn(&str) -> Option<String>| installations(env)[0].clone();
        assert_eq!(
            user(&env_of(&[("FLATPAK_USER_DIR", "/fp"), ("HOME", "/h")])),
            Some(PathBuf::from("/fp"))
        );
        assert_eq!(
            user(&env_of(&[("XDG_DATA_HOME", "/data"), ("HOME", "/h")])),
            Some(PathBuf::from("/data/flatpak"))
        );
        assert_eq!(
            user(&env_of(&[("XDG_DATA_HOME", "relative"), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.local/share/flatpak")),
            "a relative XDG_DATA_HOME is ignored"
        );
        assert_eq!(
            installations(&env_of(&[]))[1],
            Some(PathBuf::from("/var/lib/flatpak"))
        );
    }
}
