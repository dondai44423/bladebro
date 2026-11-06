//! Flatpak browsers (Linux). On read-only-root distros — SteamOS, Bazzite,
//! Fedora Silverblue — Flatpak is the supported way to install a browser,
//! and `pacman`/`apt` are not an option. An installed app is reachable
//! through its export, `<installation>/exports/bin/<app-id>`: a launcher
//! script around `flatpak run`. Discovery checks the known Chromium-family
//! app ids after every native location.
//!
//! Launching: the browser runs in a bubblewrap sandbox that sees little of
//! the host filesystem (Chromium's manifest shares the home dir at most —
//! never /tmp, nor a data dir outside home). Run through the export as-is,
//! Chrome silently creates the session profile inside the sandbox and it
//! vanishes on exit — no cookies, localStorage or seasoning ever reach
//! bladebro's profile dir. Downloads likewise complete into the sandbox's own
//! copy of the downloads dir, and an upload hands the page a file the
//! sandbox cannot open. The agent lane therefore runs an export via
//! `flatpak run` itself, sharing the session dir, the downloads dir and a
//! per-process upload staging dir that uploads are copied into first.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

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

/// A read-write share of `dir` into the sandbox. Flatpak ends a
/// `--filesystem` path at the first unescaped `:` (the `:ro`/`:create`
/// suffix), so `\` and `:` in the path are escaped.
fn filesystem_arg(dir: &Path) -> String {
    let mut shared = String::from("--filesystem=");
    for c in dir.to_string_lossy().chars() {
        if c == '\\' || c == ':' {
            shared.push('\\');
        }
        shared.push(c);
    }
    shared
}

/// Arguments before the browser's own flags: the session profile, the
/// downloads dir `Page.setDownloadBehavior` names and the upload staging
/// dir, all shared into the sandbox.
fn run_args(
    export: &Export,
    user_data_dir: &Path,
    downloads_dir: &Path,
    staging_dir: &Path,
) -> Vec<String> {
    let mut args = vec!["run".to_string()];
    args.extend(export.scope.map(String::from));
    args.push(filesystem_arg(user_data_dir));
    args.push(filesystem_arg(downloads_dir));
    args.push(filesystem_arg(staging_dir));
    args.push(export.app_id.clone());
    args
}

/// `flatpak run` for a Flatpak export with the session profile, the
/// downloads dir and the upload staging dir shared into the sandbox; `None`
/// when `chrome_path` is not an export.
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
    // Unshared, a download "completes" into the sandbox's private copy of
    // this path and never reaches the host. Flatpak skips a path that does
    // not exist, so create it now (0700, as the attach would) rather than
    // let the share silently drop.
    let downloads = crate::platform::downloads_dir();
    let _ = crate::platform::secure_create_dir_all(&downloads);
    // From here on this process stages every upload (see `upload_path`).
    let staging = UPLOAD_STAGING.get_or_init(|| {
        prepare_staging(
            &crate::platform::blade_dir().join("uploads"),
            std::process::id(),
        )
    });
    let _ = crate::platform::secure_create_dir_all(staging);
    let mut cmd = Command::new(flatpak);
    cmd.args(run_args(&export, user_data_dir, &downloads, staging));
    Some(cmd)
}

/// Upload staging. `DOM.setFileInputFiles` hands Chrome a path, and a
/// sandboxed Chrome cannot open a host path outside its shares: the page got
/// an empty `File` whose read failed with NotFoundError while the verdict
/// said "uploaded" (measured with Flathub Chromium 156, source under a
/// directory the sandbox does not see). Once this process has launched a
/// Flatpak browser, every upload is copied into
/// `<data-dir>/uploads/<pid>/<n>/<file name>` and Chrome gets the copy. The
/// dir is per process so a sandbox never sees another session's files.
static UPLOAD_STAGING: OnceLock<PathBuf> = OnceLock::new();
static UPLOAD_SEQ: AtomicU64 = AtomicU64::new(0);

/// This process's staging dir under `root`, created (0700) before the
/// launch shares it — Flatpak drops a share whose path does not exist. Dirs
/// of bladebro processes that are gone (a crash skips teardown) are removed.
fn prepare_staging(root: &Path, pid: u32) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let dead = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .is_some_and(|other| other != pid && !crate::platform::process_alive(other));
            if dead {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let own = root.join(pid.to_string());
    let _ = crate::platform::secure_create_dir_all(&own);
    own
}

/// The path to hand the browser for an upload of `source`: a copy staged in
/// the shared dir once this process launched a Flatpak browser, else
/// `source` itself (a native browser reads it in place).
pub(crate) fn upload_path(source: &Path) -> std::io::Result<PathBuf> {
    upload_path_in(UPLOAD_STAGING.get().map(PathBuf::as_path), source)
}

fn upload_path_in(staging: Option<&Path>, source: &Path) -> std::io::Result<PathBuf> {
    match staging {
        Some(staging) => stage_copy(staging, source),
        None => Ok(source.to_path_buf()),
    }
}

/// Copy `source` into a fresh `<staging>/<n>/` under its own file name (the
/// page reads it as `File.name`), owner-only, keeping its modification time
/// (`File.lastModified`). A copy, not a link: the sandbox never gets a
/// writable handle on the original.
fn stage_copy(staging: &Path, source: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let name = source.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    crate::platform::secure_create_dir_all(staging)?;
    let dir = loop {
        let dir = staging.join(UPLOAD_SEQ.fetch_add(1, Ordering::Relaxed).to_string());
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let staged = dir.join(name);
    match copy_owner_only(source, &staged) {
        Ok(()) => Ok(staged),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

fn copy_owner_only(source: &Path, staged: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = std::fs::File::open(source)?;
    let modified = from.metadata()?.modified();
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staged)?;
    std::io::copy(&mut from, &mut to)?;
    if let Ok(modified) = modified {
        to.set_modified(modified)?;
    }
    Ok(())
}

/// Remove this process's staged copies once its browser is gone — the page
/// reads an uploaded `File` lazily (at submit time, say), so not before. The
/// dir itself stays: it is the path the next launch of this process shares.
pub(crate) fn discard_staged_uploads() {
    if let Some(staging) = UPLOAD_STAGING.get() {
        clear_dir(staging);
    }
}

fn clear_dir(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let _ = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
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
    /// was gone on the next launch), and a download "completes" into a
    /// sandbox-private downloads dir (measured: the host dir stayed empty).
    /// The launch must share both dirs into the sandbox and start the
    /// installation the path names.
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
            run_args(
                &parsed,
                Path::new("/state/blade/profiles/sess-42"),
                Path::new("/state/blade/downloads"),
                Path::new("/state/blade/uploads/42")
            ),
            [
                "run",
                "--user",
                "--filesystem=/state/blade/profiles/sess-42",
                "--filesystem=/state/blade/downloads",
                "--filesystem=/state/blade/uploads/42",
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
            run_args(
                &parsed,
                Path::new("/odd:dir\\x/sess-1"),
                Path::new("/odd:dir\\x/downloads"),
                Path::new("/odd:dir\\x/uploads/1")
            )[2..5],
            [
                "--filesystem=/odd\\:dir\\\\x/sess-1",
                "--filesystem=/odd\\:dir\\\\x/downloads",
                "--filesystem=/odd\\:dir\\\\x/uploads/1"
            ],
            "a ':' must not end a shared path"
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

    /// Measured with Flathub Chromium: `act upload` of a file outside the
    /// sandbox's shares said "uploaded" while the page's FileReader got
    /// NotFoundError on an empty `File`. The path handed to Chrome must lie
    /// inside a dir the launch shares, and still carry the file's name,
    /// bytes and modification time.
    #[test]
    fn uploads_are_staged_into_a_dir_the_sandbox_shares() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "blade-flatpak-upload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("outside").join("report.pdf");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"%PDF-1.7 staged upload").unwrap();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let data = root.join("blade");
        let uploads = data.join("uploads");
        // Left by a bladebro that is gone (no pid above i32::MAX is alive),
        // by a live process (pid 1), and not ours at all.
        for leftover in ["4294967295/0", "1/0", "notes"] {
            std::fs::create_dir_all(uploads.join(leftover)).unwrap();
        }

        let staging = prepare_staging(&uploads, 4242);
        assert_eq!(staging, uploads.join("4242"));
        assert_eq!(
            std::fs::metadata(&staging).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            !uploads.join("4294967295").exists(),
            "a dead process's dir is swept"
        );
        assert!(uploads.join("1/0").exists() && uploads.join("notes").exists());

        let export = Export {
            app_id: "org.chromium.Chromium".into(),
            scope: Some("--user"),
        };
        let args = run_args(
            &export,
            &data.join("profiles/sess-4242"),
            &data.join("downloads"),
            &staging,
        );
        let shared: Vec<PathBuf> = args
            .iter()
            .filter_map(|a| a.strip_prefix("--filesystem="))
            .map(PathBuf::from)
            .collect();
        let readable = |p: &Path| shared.iter().any(|dir| p.starts_with(dir));
        assert!(
            !readable(&source),
            "fixture: the source is outside every share"
        );

        let handed = upload_path_in(Some(&staging), &source).unwrap();
        assert!(
            readable(&handed),
            "{} is not in a shared dir",
            handed.display()
        );
        assert_eq!(
            handed.file_name(),
            source.file_name(),
            "the page sees File.name"
        );
        assert_eq!(std::fs::read(&handed).unwrap(), b"%PDF-1.7 staged upload");
        let meta = std::fs::metadata(&handed).unwrap();
        assert_eq!(meta.modified().unwrap(), mtime, "File.lastModified");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let again = upload_path_in(Some(&staging), &source).unwrap();
        assert_ne!(again, handed, "each upload gets its own copy");
        assert!(upload_path_in(Some(&staging), &root.join("missing.txt")).is_err());
        assert_eq!(
            std::fs::read_dir(&staging).unwrap().count(),
            2,
            "a failed copy leaves nothing behind"
        );
        assert_eq!(
            upload_path_in(None, &source).unwrap(),
            source,
            "a native browser reads the file in place"
        );

        clear_dir(&staging);
        assert!(staging.is_dir(), "the shared dir itself stays");
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        assert!(source.exists(), "the original is never touched");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
