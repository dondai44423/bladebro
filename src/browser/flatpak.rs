//! Flatpak browsers (Linux). On read-only-root distros — SteamOS, Bazzite,
//! Fedora Silverblue — Flatpak is the supported way to install a browser,
//! and `pacman`/`apt` are not an option. An installed app is reachable
//! through its export, `<installation>/exports/bin/<app-id>`: a launcher
//! script around `flatpak run`. Discovery checks the known Chromium-family
//! app ids after every native location.
//!
//! Launching: the browser runs in a bubblewrap sandbox that does not see the
//! host filesystem (Chromium's manifest shares no home or /tmp paths). Run
//! through the export as-is, Chrome silently creates the session profile
//! inside the sandbox and it vanishes on exit — no cookies, localStorage or
//! seasoning ever reach bladebro's profile dir. The agent lane therefore runs
//! an export via `flatpak run --filesystem=<session dir>` itself.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::discover::{find_in_path, is_executable};

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

/// A Flatpak export, recognised by its path: `<installation>/exports/bin/<app-id>`.
#[derive(Debug, PartialEq)]
struct Export {
    app_id: String,
    /// `--user`/`--system` when the export lives in that installation, so
    /// `flatpak run` starts exactly the copy the path names.
    scope: Option<&'static str>,
}

fn parse_export(path: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<Export> {
    let app_id = path.file_name()?.to_str()?;
    let bin = path.parent()?;
    let exports = bin.parent()?;
    if bin.file_name()? != "bin" || exports.file_name()? != "exports" || !is_app_id(app_id) {
        return None;
    }
    let installation = exports.parent()?;
    let same = |root: &Path| {
        root == installation
            || std::fs::canonicalize(root)
                .is_ok_and(|r| std::fs::canonicalize(installation).is_ok_and(|i| r == i))
    };
    let [user, system] = installations(env);
    let scope = if user.as_deref().is_some_and(same) {
        Some("--user")
    } else if system.as_deref().is_some_and(same) {
        Some("--system")
    } else {
        None
    };
    Some(Export {
        app_id: app_id.to_string(),
        scope,
    })
}

/// Reverse-DNS app id with at least three parts. It becomes a `flatpak run`
/// argument, so it must never be mistaken for an option.
fn is_app_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 3
        && id.starts_with(|c: char| c.is_ascii_alphabetic())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Arguments before the browser's own flags. Flatpak ends a `--filesystem`
/// path at the first unescaped `:` (the `:ro`/`:create` suffix), so `\` and
/// `:` in the session dir are escaped.
fn run_args(export: &Export, user_data_dir: &Path) -> Vec<String> {
    let mut shared = String::from("--filesystem=");
    for c in user_data_dir.to_string_lossy().chars() {
        if c == '\\' || c == ':' {
            shared.push('\\');
        }
        shared.push(c);
    }
    let mut args = vec!["run".to_string()];
    args.extend(export.scope.map(String::from));
    args.push(shared);
    args.push(export.app_id.clone());
    args
}

/// `flatpak run` for a Flatpak export with the session profile shared into
/// the sandbox; `None` when `chrome_path` is not an export.
pub(super) fn launch_command(chrome_path: &str, user_data_dir: &Path) -> Option<Command> {
    let export = parse_export(Path::new(chrome_path), &|key| std::env::var(key).ok())?;
    let flatpak = find_in_path("flatpak").or_else(|| {
        ["/usr/bin/flatpak", "/usr/local/bin/flatpak"]
            .into_iter()
            .find(|p| is_executable(Path::new(p)))
            .map(String::from)
    });
    let Some(flatpak) = flatpak else {
        eprintln!(
            "[bladebro] WARNING: {chrome_path} is a Flatpak export but `flatpak` was not found; \
             launching the export directly — the sandbox cannot see the session profile, so \
             cookies and storage will not persist"
        );
        return None;
    };
    let mut cmd = Command::new(flatpak);
    cmd.args(run_args(&export, user_data_dir));
    Some(cmd)
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

    /// Run as-is, a Flatpak export starts Chrome on a sandbox-private copy of
    /// the session dir (measured: the host dir stayed empty and localStorage
    /// was gone on the next launch). The launch must share the session dir
    /// into the sandbox and start the installation the path names.
    #[test]
    fn exports_launch_through_flatpak_run_with_the_profile_shared() {
        let root = std::env::temp_dir().join(format!("blade-flatpak-run-{}", std::process::id()));
        let user = root.join("user");
        let system = root.join("system");
        let vars: HashMap<&str, String> = HashMap::from([
            ("FLATPAK_USER_DIR", user.display().to_string()),
            ("FLATPAK_SYSTEM_DIR", system.display().to_string()),
        ]);
        let env = |key: &str| vars.get(key).cloned();
        let user_export = export(&user, "org.chromium.Chromium", 0o755);
        let system_export = export(&system, "com.brave.Browser", 0o755);
        let elsewhere = export(&root.join("extra"), "com.google.Chrome", 0o755);

        let parsed = parse_export(&user_export, &env).expect("user export");
        assert_eq!(
            run_args(&parsed, Path::new("/state/blade/profiles/sess-42")),
            [
                "run",
                "--user",
                "--filesystem=/state/blade/profiles/sess-42",
                "org.chromium.Chromium"
            ]
        );
        assert_eq!(
            parse_export(&system_export, &env).map(|e| e.scope),
            Some(Some("--system"))
        );
        assert_eq!(
            parse_export(&elsewhere, &env),
            Some(Export {
                app_id: "com.google.Chrome".into(),
                scope: None
            }),
            "an export in another installation still runs, with flatpak's own lookup"
        );
        assert_eq!(
            run_args(&parsed, Path::new("/odd:dir\\x/sess-1"))[2],
            "--filesystem=/odd\\:dir\\\\x/sess-1",
            "a ':' must not end the shared path"
        );
        for native in [
            "/usr/bin/chromium",
            "/opt/exports/bin/chromium",
            "/x/exports/bin/--command=sh.a.b",
            "/x/exports/lib/org.chromium.Chromium",
        ] {
            assert_eq!(parse_export(Path::new(native), &env), None, "{native}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
