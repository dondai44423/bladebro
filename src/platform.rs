//! Cross-platform process management and paths.
//!
//! Every OS-specific operation lives here. The rest of the codebase
//! calls these helpers — no `#[cfg]` outside this module (except for
//! transport-level differences that can't be abstracted).

use std::path::PathBuf;
use std::process::Child;
#[cfg(unix)]
use std::time::Duration;

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(h) = std::env::var("HOME") {
            if !h.trim().is_empty() {
                return PathBuf::from(h);
            }
        }
        // HOME unset or empty (cron, services, sanitized envs): ask the
        // password database for the account's real home. Falling straight
        // to /tmp put the cookie-bearing data dir in a shared directory.
        // getpwuid_r (not getpwuid): home_dir may run on any tokio worker.
        unsafe {
            let mut buf = [0 as libc::c_char; 1024];
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            if libc::getpwuid_r(
                libc::getuid(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            ) == 0
                && !result.is_null()
                && !pwd.pw_dir.is_null()
            {
                let dir = std::ffi::CStr::from_ptr(pwd.pw_dir)
                    .to_string_lossy()
                    .to_string();
                if !dir.trim().is_empty() {
                    return PathBuf::from(dir);
                }
            }
        }
        PathBuf::from("/tmp")
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\Users\\Default"))
    }
}

/// The Bladebro data directory. Resolution order:
/// 1. `BLADE_HOME` (explicit override, highest priority).
/// 2. Unix: `$XDG_STATE_HOME/blade` when `XDG_STATE_HOME` is set.
/// 3. Unix: `$HOME/.local/state/blade` when that parent exists or can be
///    created (a `.local` dir already present is enough).
/// 4. Unix: `$HOME/.blade` (legacy default; kept for minimal setups).
///    Windows: `%USERPROFILE%\.blade`.
///
/// Migration-free upgrade: when a DEFAULT resolution (cases 2-4, not an
/// explicit `BLADE_HOME`) lands somewhere other than the legacy dir while
/// `~/.blade` already holds Bladebro state, the legacy dir wins. An
/// upgraded install must never silently split its state in two.
pub fn blade_dir() -> PathBuf {
    let home = home_dir();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let legacy = home.join(".blade");
    resolve_blade_dir(
        &home,
        &env,
        dir_has_state(&legacy),
        local_state_creatable(&home),
    )
}

/// Can `$HOME/.local/state/blade` be created? True when the parent exists
/// or `.local` does (so `state`/`blade` only need a mkdir under it).
fn local_state_creatable(home: &std::path::Path) -> bool {
    home.join(".local").join("state").exists() || home.join(".local").exists()
}

/// Pure resolution core (tested directly; no env mutation or FS checks in
/// tests — the two FS facts are injected as flags).
fn resolve_blade_dir(
    home: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
    legacy_has_state: bool,
    local_state_creatable: bool,
) -> PathBuf {
    // 1. Explicit override wins unconditionally.
    if let Some(v) = env("BLADE_HOME").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(v);
    }
    let legacy = home.join(".blade");
    #[cfg(windows)]
    {
        // Windows has no XDG fallbacks; the flags exist for the Unix path.
        let _ = (legacy_has_state, local_state_creatable);
        legacy
    }
    #[cfg(not(windows))]
    {
        // 2. XDG_STATE_HOME, then 3. HOME/.local/state (existing or creatable).
        let mut resolved = None;
        if let Some(xdg) = env("XDG_STATE_HOME").filter(|v| !v.trim().is_empty()) {
            resolved = Some(PathBuf::from(xdg).join("blade"));
        } else if local_state_creatable {
            resolved = Some(home.join(".local").join("state").join("blade"));
        }
        let resolved = resolved.unwrap_or_else(|| legacy.clone());
        // Migration-free fallback: an existing install keeps its legacy dir.
        if resolved != legacy && legacy_has_state {
            return legacy;
        }
        resolved
    }
}

/// Does `~/.blade` hold Bladebro state? (Any known artifact, not just a
/// directory that happens to exist.)
fn dir_has_state(legacy: &std::path::Path) -> bool {
    if !legacy.is_dir() {
        return false;
    }
    const MARKS: &[&str] = &[
        ".fingerprint.json",
        "logins.json",
        "knowledge",
        ".warmed",
        "profile",
        "profiles",
        "sessions",
        "artifacts",
        "downloads",
        "cli.sock",
    ];
    MARKS.iter().any(|m| legacy.join(m).exists())
}

/// Create a directory and set restrictive permissions (0700 on Unix).
/// SECURITY: Session files, fingerprints, and backups contain sensitive
/// data (cookies, auth tokens). Without explicit permissions, they get
/// the process umask (often 755/644), making them world-readable.
/// Every component this call CREATES is chmodded 0700 — ancestors that
/// already exist (e.g. $HOME) are never touched.
pub fn secure_create_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    // Deepest existing ancestor — creation (and chmodding) starts below it.
    let mut first_missing = path.to_path_buf();
    while !first_missing.exists() {
        match first_missing.parent() {
            Some(p) if p != first_missing => first_missing = p.to_path_buf(),
            _ => break,
        }
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(tail) = path.strip_prefix(first_missing.as_path()) {
            let mut cur = first_missing;
            for comp in tail.components() {
                cur.push(comp.as_os_str());
                let _ = std::fs::set_permissions(&cur, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write a file with restrictive permissions (0600 on Unix).
/// SECURITY: Session files contain cookies and localStorage — world-readable
/// by default (644). This ensures only the owner can read them.
pub fn secure_write_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let tmp = parent.join(format!(
        ".blade-write-{}-{timestamp}-{}",
        std::process::id(),
        NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // A create collision is not our file to remove on the failure path.
    let mut file = options.open(&tmp)?;
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Spell a path the way the write-path checks compare it: strip Windows
/// verbatim prefixes (`\\?\C:\…`, `\\?\UNC\server\share`) and normalize
/// separators to `/`. Windows `canonicalize` returns verbatim paths, so a
/// lexical prefix check (`c:/windows`) would otherwise silently never match.
fn plain_spelling(path: &std::path::Path) -> String {
    const VERBATIM: &str = r"\\?\";
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    let s = path.to_string_lossy().into_owned();
    let s = if let Some(rest) = s.strip_prefix(VERBATIM_UNC) {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(VERBATIM) {
        rest.to_string()
    } else {
        s
    };
    s.replace('\\', "/")
}

/// Validate a file write path to prevent writing to system directories.
/// SECURITY: Blocks path traversal attacks that could overwrite critical
/// system files (e.g., /etc/cron.d, /usr/bin, /boot) via prompt injection,
/// plus credential stores and shell-startup files in the user's home
/// (~/.ssh, ~/.gnupg, ~/.bashrc, ...) — the classic persistence/exfiltration
/// sinks a prompt-injected page would target.
/// Returns Ok(()) if safe, Err(message) if blocked.
pub fn validate_write_path(path: &std::path::Path) -> Result<(), String> {
    let canonical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    // Normalize the path (resolve . and .. without requiring the file to exist).
    let mut normalized = std::path::PathBuf::new();
    for component in canonical.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    // Symlink resolution: canonicalize the deepest EXISTING ancestor and
    // re-append the untouched tail. A path spelled through a symlink
    // (~/drop → /etc) must not pass on its harmless-looking spelling — the
    // pre-fix check never resolved symlinks. An existing FINAL symlink
    // resolves too, so the write target itself is what gets checked.
    let mut probe = normalized.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !probe.exists() {
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) if parent != probe => {
                tail.push(name.to_os_string());
                probe = parent.to_path_buf();
            }
            _ => break,
        }
    }
    // The caller's spelling, kept for checks that must match both what was
    // written and what it resolves to (see the blocked-prefix loop below).
    let lexical = plain_spelling(&normalized);
    let normalized = match std::fs::canonicalize(&probe) {
        Ok(base) => {
            let mut p = base;
            for c in tail.iter().rev() {
                p.push(c);
            }
            p
        }
        Err(_) => normalized,
    };
    let path_str = plain_spelling(&normalized);
    let lower = path_str.to_lowercase();

    #[cfg(unix)]
    {
        let blocked_prefixes: &[&str] = &[
            "/etc",
            "/usr",
            "/bin",
            "/sbin",
            "/boot",
            "/dev",
            "/proc",
            "/sys",
            "/var/log",
            "/var/spool",
            "/root",
            "/lib",
            "/lib64",
            "/run",
            "/snap",
        ];
        for prefix in blocked_prefixes {
            // Three spellings must all match: what the caller wrote
            // (`lexical`), what it resolves to (`path_str`), and the
            // platform's own real spelling of the blocked directory — on
            // macOS /etc is a symlink to /private/etc, so a canonicalized
            // path would otherwise slip past a lexical-only compare.
            let resolved = std::fs::canonicalize(prefix)
                .ok()
                .map(|p| plain_spelling(&p));
            let hit = path_str.starts_with(prefix)
                || lexical.starts_with(prefix)
                || resolved.as_deref().is_some_and(|r| path_str.starts_with(r));
            if hit {
                return Err(format!(
                    "blocked: writing to system directory ({prefix}) is not allowed"
                ));
            }
        }
    }

    #[cfg(windows)]
    {
        let blocked_win: &[&str] = &[
            "c:/windows",
            "c:/program files",
            "c:/program files (x86)",
            "c:/programdata/microsoft/windows/start menu",
        ];
        for prefix in blocked_win {
            if lower.starts_with(prefix) {
                return Err(format!(
                    "blocked: writing to system directory ({prefix}) is not allowed"
                ));
            }
        }
        // Autostart persistence: the per-user Startup folder.
        if lower.contains("/microsoft/windows/start menu/programs/startup") {
            return Err("blocked: writing to the Windows Startup folder is not allowed".into());
        }
    }

    // Credential/config sinks that exist anywhere in the path (any home).
    // A prompt-injected page convincing the agent to write here gains
    // persistence (rc files) or steals credentials (.ssh/.aws/.gnupg).
    const BLOCKED_COMPONENTS: &[&str] = &[
        ".ssh", ".gnupg", ".aws", ".kube", ".docker", ".config", ".gnome", ".git", ".cargo", ".m2",
        ".vscode", ".atom",
    ];
    for comp in normalized
        .components()
        .filter_map(|c| c.as_os_str().to_str())
    {
        let cl = comp.to_lowercase();
        if BLOCKED_COMPONENTS.contains(&cl.as_str())
            || cl == "authorized_keys"
            || cl == "known_hosts"
            || cl == "id_rsa"
            || cl.starts_with("id_rsa.")
            || cl.starts_with("id_ed25519")
            || cl.starts_with("id_ecdsa")
            // npm config files: user-level ~/.npmrc, per-project ./.npmrc
            // (which outranks the user file — a `registry=` line there
            // redirects installs and runs attacker-served tarballs'
            // install scripts), and the Windows per-user global
            // %APPDATA%\npm\etc\npmrc.
            || cl == ".npmrc"
            || cl == "npmrc"
            // Project env files (.env.local/.env.production/...) are read
            // by Next.js/Vite/dotenv-style tooling; `.env` alone used to be
            // rc-blocked inside $HOME only.
            || cl == ".env"
            || cl.starts_with(".env.")
            // Conda / yarn / pip user configs — the same poison-the-
            // toolchain class as .npmrc.
            || cl == ".condarc"
            || cl == ".yarnrc"
            || cl == ".yarnrc.yml"
            || cl == ".pypirc"
        {
            return Err(format!(
                "blocked: writing to credential/config location ({comp}) is not allowed"
            ));
        }
    }
    // systemd user-unit persistence spans multiple components.
    if lower.contains("/.local/share/systemd/") {
        return Err("blocked: writing to systemd user units is not allowed".into());
    }

    // Shell startup files directly in the home directory (~/.bashrc etc.).
    let file_name = normalized
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    const RC_FILES: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".bash_logout",
        ".bash_aliases",
        ".profile",
        ".zshrc",
        ".zprofile",
        ".zshenv",
        ".zlogin",
        ".kshrc",
        ".cshrc",
        ".gitconfig",
        ".tmux.conf",
        ".xinitrc",
        ".xsession",
        ".xprofile",
        ".crontab",
        ".vimrc",
        ".exrc",
        ".curlrc",
        ".wgetrc",
        ".netrc",
        ".env",
    ];
    // Check both the caller's and the resolved spelling: canonicalizing a
    // path under a symlinked HOME root (e.g. /home → /var/home) rewrites it
    // past the user's own $HOME spelling, which must not unlock rc files.
    let under = |h: String| {
        let h = h.replace('\\', "/");
        let prefix = format!("{h}/");
        [&path_str, &lexical]
            .iter()
            .any(|p| p.as_str() == h.as_str() || p.as_str().starts_with(prefix.as_str()))
    };
    let in_home = std::env::var("HOME").map(under).unwrap_or(false)
        || std::env::var("USERPROFILE").map(under).unwrap_or(false);
    if in_home && RC_FILES.contains(&file_name.as_str()) {
        return Err(format!(
            "blocked: writing to shell/config startup file ({file_name}) is not allowed"
        ));
    }

    Ok(())
}

/// Validate a local file READ whose bytes are forwarded into the driven
/// page (`act upload` → `DOM.setFileInputFiles`). SECURITY: everything set
/// here is delivered to the page — an untrusted origin — so this is a
/// disclosure sink, not a neutral file picker: a prompt-injected page
/// steers the agent toward exactly the credential locations the write path
/// blocks, plus bladebro's own cleartext credential stores under the data
/// dir (logins.json, sessions/, realbrowser templates). The artifacts dir
/// stays uploadable — it is the sanctioned file-exchange zone.
///
/// Absolute paths only: Chrome cannot resolve relative or `~` spellings
/// (they yield an unreadable File and crash the browser process), so
/// refusing them here turns a crash into a clean error.
pub fn validate_upload_path(path: &std::path::Path) -> Result<(), String> {
    validate_upload_path_at(path, &blade_dir())
}

/// [`validate_upload_path`] with the data dir injected (tests pass a
/// scratch root instead of mutating global env).
pub fn validate_upload_path_at(
    path: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "blocked: upload requires an absolute file path (got {:?}) — the browser cannot read relative or ~ spellings",
            path.display()
        ));
    }
    // Resolve symlinks fully: the check must see the real target, and a
    // non-existent or unreadable source is a clean error here instead of
    // a browser crash later.
    let resolved = std::fs::canonicalize(path)
        .map_err(|e| format!("upload source cannot be read: {} ({e})", path.display()))?;
    if !resolved.is_file() {
        return Err(format!(
            "upload source is not a regular file: {}",
            path.display()
        ));
    }
    let path_str = plain_spelling(&resolved);

    #[cfg(unix)]
    {
        let blocked_prefixes: &[&str] = &[
            "/etc",
            "/usr",
            "/bin",
            "/sbin",
            "/boot",
            "/dev",
            "/proc",
            "/sys",
            "/var/log",
            "/var/spool",
            "/root",
            "/lib",
            "/lib64",
            "/run",
            "/snap",
        ];
        for prefix in blocked_prefixes {
            let resolved_prefix = std::fs::canonicalize(prefix)
                .ok()
                .map(|p| plain_spelling(&p));
            if path_str.starts_with(prefix)
                || resolved_prefix
                    .as_deref()
                    .is_some_and(|r| path_str.starts_with(r))
            {
                return Err(format!(
                    "blocked: uploading from a system location ({prefix}) is not allowed"
                ));
            }
        }
    }

    #[cfg(windows)]
    {
        let lower = path_str.to_lowercase();
        for prefix in ["c:/windows", "c:/program files", "c:/program files (x86)"] {
            if lower.starts_with(prefix) {
                return Err(format!(
                    "blocked: uploading from a system location ({prefix}) is not allowed"
                ));
            }
        }
    }

    // Credential/config locations, same families the write side blocks.
    const BLOCKED_COMPONENTS: &[&str] = &[
        ".ssh", ".gnupg", ".aws", ".kube", ".docker", ".config", ".gnome",
    ];
    for comp in resolved.components().filter_map(|c| c.as_os_str().to_str()) {
        let cl = comp.to_lowercase();
        if BLOCKED_COMPONENTS.contains(&cl.as_str())
            || cl == "authorized_keys"
            || cl == "known_hosts"
            || cl == "id_rsa"
            || cl.starts_with("id_rsa.")
            || cl.starts_with("id_ed25519")
            || cl.starts_with("id_ecdsa")
            || cl == ".netrc"
            || cl == "netrc"
            || cl == ".npmrc"
            || cl == "npmrc"
            || cl == ".git-credentials"
            || cl == ".pypirc"
            || cl == ".condarc"
        {
            return Err(format!(
                "blocked: uploading from a credential/config location ({comp}) is not allowed"
            ));
        }
    }

    // Bladebro's own data stores — the complete session jar, saved
    // sessions, profile templates, the rb clone of the user's real profile.
    // The artifacts dir is the sanctioned exchange zone and stays
    // uploadable.
    let resolve_dir = |dir: &std::path::Path| -> std::path::PathBuf {
        match std::fs::canonicalize(dir) {
            Ok(p) => p,
            Err(_) => match dir.parent().and_then(|p| std::fs::canonicalize(p).ok()) {
                Some(parent) => parent.join(dir.file_name().unwrap_or_default()),
                None => dir.to_path_buf(),
            },
        }
    };
    let data_c = resolve_dir(data_dir);
    let artifacts_c = resolve_dir(&data_c.join("artifacts"));
    let in_data = resolved.starts_with(&data_c);
    let in_artifacts = resolved.starts_with(&artifacts_c);
    // The legacy ~/.blade spelling can hold state even when the active dir
    // resolves elsewhere (migration keeps one dir; a stale second copy is
    // exactly what an attacker would aim at).
    let legacy_c = resolve_dir(&home_dir().join(".blade"));
    let in_legacy =
        resolved.starts_with(&legacy_c) && !resolved.starts_with(legacy_c.join("artifacts"));
    if (in_data && !in_artifacts) || in_legacy {
        return Err(format!(
            "blocked: uploading from bladebro's data directory is not allowed ({} — use the artifacts dir)",
            resolved.display()
        ));
    }

    Ok(())
}

/// Re-tighten the sensitive parts of an EXISTING state tree on every
/// launch. SECURITY: the 0600/0700 discipline is creation-time-only; trees
/// created by pre-3.2.0 binaries — or restored through a mode-stripping
/// transfer (cloud sync, permission-less filesystems) — keep
/// world-readable cookie-bearing files (`sessions/*.json`, `logins.json`,
/// ...) indefinitely. Only components owned by the current uid are
/// touched — foreign-owned components are left as-is (never silently
/// legitimize a pre-planted tree). Exec bits are never stripped (downloaded
/// binaries, backups).
#[cfg(unix)]
pub fn reharden_state_tree(root: &std::path::Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = unsafe { libc::getuid() };
    let owned = |p: &std::path::Path| std::fs::metadata(p).ok().filter(|m| m.uid() == uid);
    let dir700 = |p: &std::path::Path| {
        if owned(p).is_some() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
        }
    };
    // 0600, but never strip an exec bit (downloaded binaries, backups).
    let file600 = |p: &std::path::Path| {
        if let Some(m) = owned(p) {
            if m.permissions().mode() & 0o111 == 0 {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
            }
        }
    };
    dir700(root);
    for name in ["logins.json", ".fingerprint.json", "realbrowser.json"] {
        file600(&root.join(name));
    }
    // Saved sessions hold complete cookie sets + localStorage; knowledge
    // and artifacts hold captured page state. Bounded two-level walk.
    // Profile template trees are excluded: copy_profile recreates their
    // dirs 0700 at every sync-back; backups keep their modes for rollback.
    for name in ["sessions", "knowledge", "artifacts", "downloads"] {
        let dir = root.join(name);
        dir700(&dir);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dir700(&p);
                    if let Ok(rd2) = std::fs::read_dir(&p) {
                        for e2 in rd2.flatten() {
                            file600(&e2.path());
                        }
                    }
                } else {
                    file600(&p);
                }
            }
        }
    }
}

/// Open a file for reading without following a final-component symlink.
/// SECURITY: the sidecar files (logins.json, sessions/*.json) are
/// trust-bearing — a pre-planted symlink must be a refusal, not a read
/// through to attacker-authored bytes that get re-injected into the
/// browser. On Unix this is atomic (O_NOFOLLOW); elsewhere a lexical
/// pre-check stands in (Windows symlink creation needs elevation).
pub fn open_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(std::io::Error::other("refusing to read through a symlink"));
        }
        std::fs::File::open(path)
    }
}

/// Read a whole file without following a final-component symlink.
pub fn read_file_nofollow(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = open_nofollow(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Walk a directory's ancestors (root → parent): every existing component
/// must be a real directory — or a root-owned symlink, which only the OS
/// itself can place (macOS /var → private/var, /tmp → private/tmp) — owned
/// by this uid or root, and not group/other-writable unless sticky (the
/// /tmp case — /tmp is root-owned 1777, and stickiness is what makes
/// creation inside it safe; a non-sticky world-writable ancestor lets any
/// local user replace this process's freshly created directory with a
/// symlink between creation and use). Used for the operator-override
/// profile lanes, where the destination can be far outside the 0700 state
/// tree.
pub fn validate_dir_ancestors(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = unsafe { libc::getuid() };
        let mut cur = std::path::PathBuf::new();
        let components: Vec<_> = path.components().collect();
        let Some((_last, ancestors)) = components.split_last() else {
            return Ok(());
        };
        for comp in ancestors {
            cur.push(comp);
            let md = match std::fs::symlink_metadata(&cur) {
                Ok(m) => m,
                Err(_) => continue, // not there yet — creation handles it
            };
            if md.file_type().is_symlink() {
                // Root-owned symlinks are the OS's own layout — macOS
                // /var → private/var, /tmp → private/tmp; merged-/usr
                // /bin → usr/bin. A local co-user cannot create a
                // root-owned entry, so these are trusted; any other
                // symlink is an attacker-plantable redirect and refuses.
                if md.uid() != 0 {
                    return Err(format!(
                        "{} resolves through a symlink — refusing it",
                        cur.display()
                    ));
                }
                continue;
            }
            if !md.is_dir() {
                return Err(format!("{} is not a directory", cur.display()));
            }
            if md.uid() != uid && md.uid() != 0 {
                return Err(format!(
                    "{} is owned by another user — refusing it",
                    cur.display()
                ));
            }
            let mode = md.permissions().mode();
            if mode & 0o022 != 0 && mode & 0o1000 == 0 {
                return Err(format!(
                    "{} is group/other-writable and not sticky — refusing it",
                    cur.display()
                ));
            }
        }
    }
    // Non-unix hosts have no comparable ownership/mode model; the caller's
    // portable checks (symlink, is-dir) still run.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Char-safe string truncation. Byte slicing (`&s[..n]`) panics when `n`
/// lands inside a multi-byte UTF-8 char — and these strings are often
/// page-controlled (URLs, exception messages), so a malicious page could
/// crash the daemon. Returns the longest prefix of at most `n` bytes that
/// ends on a char boundary.
pub fn truncate_utf8(s: &str, n: usize) -> &str {
    if n >= s.len() {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Is this process alive?
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    if pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(windows)]
    {
        // tasklist /FI "PID eq 1234" /NH — if the process exists, output
        // contains the PID; if not, output says "No tasks".
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout).lines().any(|line| {
                    line.split(',')
                        .nth(1)
                        .and_then(|field| field.trim_matches('"').parse::<u32>().ok())
                        == Some(pid)
                })
            })
            .unwrap_or(false)
    }
}

/// Is this process a Chrome/Chromium process?
pub fn process_is_chrome(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .and_then(|bytes| {
                bytes
                    .split(|b| *b == 0)
                    .next()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
            })
            .is_some_and(|exe| {
                std::path::Path::new(&exe)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| {
                        matches!(
                            name,
                            "chrome"
                                | "chromium"
                                | "chromium-browser"
                                | "google-chrome"
                                | "google-chrome-stable"
                                | "chrome_crashpad_handler"
                        )
                    })
            })
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .to_lowercase()
                    .contains("chrom")
            })
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .to_lowercase()
                    .contains("chrome")
            })
            .unwrap_or(false)
    }
}

/// Send SIGTERM to a process by PID (Unix) or graceful-terminate (Windows).
pub fn kill_process_graceful(pid: u32) {
    if pid <= 1 {
        return;
    }
    #[cfg(unix)]
    if pid > i32::MAX as u32 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        // Windows has no SIGTERM. /T kills the process tree, without /F
        // it's a graceful request (WM_CLOSE to console apps).
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .output();
    }
}

/// Send SIGKILL to a process by PID (Unix) or force-terminate (Windows).
pub fn kill_process_force(pid: u32) {
    if pid <= 1 {
        return;
    }
    #[cfg(unix)]
    if pid > i32::MAX as u32 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Shut down a child process gracefully: SIGTERM first (Unix), then
/// SIGKILL after a grace period. On Windows, TerminateProcess directly.
pub fn shutdown_child(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        for _ in 0..30 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(windows)]
    {
        // Windows: no graceful signal. TerminateProcess is the only option.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// True when the binary backing this process was replaced on disk while the
/// process kept running (self-update, or the rename-swap used when a live
/// process holds the file). The process is serving old code; only a restart
/// picks up the new build.
///
/// Two Linux tells: `/proc/self/exe` is ` (deleted)` (the old image was
/// unlinked), or it no longer resolves to the same file the host invoked
/// (`argv[0]` — a rename-swap that kept the old image under another name).
/// Both sides must resolve for the second check; otherwise stay quiet rather
/// than guess. Elsewhere there is no portable signal and this answers false.
pub fn stale_binary() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::path::{Path, PathBuf};
        let exe = match std::fs::read_link("/proc/self/exe") {
            Ok(p) => p,
            Err(_) => return false,
        };
        if exe.to_string_lossy().ends_with(" (deleted)") {
            return true;
        }
        let resolve = |p: &Path| -> Option<PathBuf> {
            if p.components().count() > 1 {
                std::fs::canonicalize(p).ok()
            } else {
                // Bare name: resolve via PATH the way the host's shell would.
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|d| d.join(p))
                        .find(|c| c.exists())
                        .and_then(|c| std::fs::canonicalize(c).ok())
                })
            }
        };
        let argv0 = std::env::args_os().next().map(PathBuf::from);
        match (
            argv0.as_deref().and_then(resolve),
            std::fs::canonicalize(&exe).ok(),
        ) {
            (Some(invoked), Some(running)) => invoked != running,
            _ => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn stale_binary_is_false_for_a_normally_launched_process() {
        // The test process runs from the path that invoked it — the check must
        // not fire (a false positive would nag every user after any unrelated
        // file shuffle). The true cases are live-verified: a rename-swap or
        // unlink under a running MCP produces the advisory exactly once.
        assert!(
            !stale_binary(),
            "false positive in the ordinary launch case"
        );
    }

    fn env_of<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| map.get(k).map(|v| v.to_string())
    }

    #[test]
    fn blade_home_wins_unconditionally() {
        let home = std::path::Path::new("/home/u");
        let mut env = HashMap::new();
        env.insert("BLADE_HOME", "/custom/blade");
        env.insert("XDG_STATE_HOME", "/xdg");
        // Even with legacy state present, an explicit override must win.
        assert_eq!(
            resolve_blade_dir(home, &env_of(&env), true, true),
            std::path::PathBuf::from("/custom/blade")
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_state_home_is_second() {
        let home = std::path::Path::new("/home/u");
        let mut env = HashMap::new();
        env.insert("XDG_STATE_HOME", "/xdg/state");
        assert_eq!(
            resolve_blade_dir(home, &env_of(&env), false, false),
            std::path::PathBuf::from("/xdg/state/blade")
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_state_default_for_fresh_installs() {
        let home = std::path::Path::new("/home/u");
        let empty: HashMap<&str, &str> = HashMap::new();
        // A modern home (.local/state creatable) gets the XDG-style default.
        assert_eq!(
            resolve_blade_dir(home, &env_of(&empty), false, true),
            std::path::PathBuf::from("/home/u/.local/state/blade")
        );
    }

    #[test]
    fn legacy_dir_with_state_wins_over_new_default() {
        let home = std::path::Path::new("/home/u");
        let empty: HashMap<&str, &str> = HashMap::new();
        // An existing ~/.blade install must not be silently split: the
        // migration-free fallback keeps the legacy dir.
        assert_eq!(
            resolve_blade_dir(home, &env_of(&empty), true, true),
            std::path::PathBuf::from("/home/u/.blade")
        );
    }

    #[test]
    fn minimal_home_falls_back_to_legacy_and_ignores_empty_env() {
        let home = std::path::Path::new("/home/u");
        let mut env = HashMap::new();
        // Empty overrides count as unset.
        env.insert("BLADE_HOME", "");
        env.insert("XDG_STATE_HOME", "");
        // No .local anywhere: fall all the way back to the legacy default.
        assert_eq!(
            resolve_blade_dir(home, &env_of(&env), false, false),
            std::path::PathBuf::from("/home/u/.blade")
        );
        // And with legacy state present it stays put either way.
        assert_eq!(
            resolve_blade_dir(home, &env_of(&env), true, true),
            std::path::PathBuf::from("/home/u/.blade")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_keeps_legacy_dir_unless_blade_home() {
        let home = std::path::Path::new("C:\\Users\\u");
        let env = HashMap::new();
        // XDG flags must not move the data dir on Windows.
        assert_eq!(
            resolve_blade_dir(home, &env_of(&env), false, true),
            std::path::PathBuf::from("C:\\Users\\u\\.blade")
        );
        let mut over = HashMap::new();
        over.insert("BLADE_HOME", "D:\\blade");
        assert_eq!(
            resolve_blade_dir(home, &env_of(&over), false, true),
            std::path::PathBuf::from("D:\\blade")
        );
    }
}

#[cfg(test)]
mod write_path_tests {
    use super::*;

    /// Windows `canonicalize` returns `\\?\`-prefixed verbatim paths; the
    /// spelling normalizer must strip the prefix (and re-form UNC paths) or
    /// every string check downstream would miss. Pure string logic — runs on
    /// every platform.
    #[test]
    fn plain_spelling_strips_windows_verbatim_prefixes() {
        assert_eq!(
            plain_spelling(std::path::Path::new(r"\\?\C:\Windows\System32")),
            "C:/Windows/System32"
        );
        assert_eq!(
            plain_spelling(std::path::Path::new(r"\\?\UNC\srv\share\x")),
            "//srv/share/x"
        );
        assert_eq!(
            plain_spelling(std::path::Path::new("/etc/passwd")),
            "/etc/passwd"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_path_resolves_symlinked_ancestors() {
        let dir = std::env::temp_dir().join(format!("blade-vwp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("drop");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        // Spelled through the symlink, this resolves into /etc — blocked.
        // The pre-fix check only saw the harmless-looking spelling.
        assert!(validate_write_path(&link.join("bladebro-test")).is_err());
        // A plain path under the same dir still passes.
        assert!(validate_write_path(&dir.join("ok.txt")).is_ok());
        // The platform's REAL spelling of a blocked directory is blocked
        // too: on macOS /etc is a symlink to /private/etc, so a check that
        // only knew the literal spelling let this through (CI red,
        // 2026-09-29).
        let real_etc = std::fs::canonicalize("/etc").expect("canonicalize /etc");
        assert!(validate_write_path(&real_etc.join("bladebro-test")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix))]
mod security_hardening_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_path_blocks_toolchain_config_sinks() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tester".into());
        for p in [
            format!("{home}/.npmrc"),
            "/tmp/proj/.npmrc".to_string(),
            "/tmp/proj/.git/config".to_string(),
            format!("{home}/.cargo/config.toml"),
            format!("{home}/.m2/settings.xml"),
            "/tmp/proj/.env.local".to_string(),
            "/tmp/proj/.env".to_string(),
            format!("{home}/.condarc"),
        ] {
            assert!(
                validate_write_path(std::path::Path::new(&p)).is_err(),
                "{p} must be blocked"
            );
        }
        for p in ["/tmp/report.pdf", "/tmp/sub/dir/notes.txt"] {
            assert!(
                validate_write_path(std::path::Path::new(p)).is_ok(),
                "{p} should pass"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn upload_path_blocks_credentials_and_data_dir_but_allows_artifacts() {
        let base = std::env::temp_dir().join(format!("blade-upload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("blade");
        let artifacts = data.join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let ok_file = artifacts.join("shot.png");
        std::fs::write(&ok_file, b"x").unwrap();
        let secret = data.join("logins.json");
        std::fs::write(&secret, b"[]").unwrap();
        let sessions = data.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let session = sessions.join("bank.json");
        std::fs::write(&session, b"{}").unwrap();
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let doc = docs.join("report.pdf");
        std::fs::write(&doc, b"%PDF").unwrap();
        let sshdir = base.join(".ssh");
        std::fs::create_dir_all(&sshdir).unwrap();
        let key = sshdir.join("id_rsa");
        std::fs::write(&key, b"-----BEGIN").unwrap();

        // Sanctioned zone + ordinary documents pass.
        assert!(validate_upload_path_at(&ok_file, &data).is_ok());
        assert!(validate_upload_path_at(&doc, &data).is_ok());
        // Data-dir stores are refused; artifacts are the only exception.
        assert!(validate_upload_path_at(&secret, &data).is_err());
        assert!(validate_upload_path_at(&session, &data).is_err());
        // Credential locations are refused.
        assert!(validate_upload_path_at(&key, &data).is_err());
        // System dirs are refused when they exist.
        if std::path::Path::new("/etc/hostname").exists() {
            assert!(validate_upload_path_at(std::path::Path::new("/etc/hostname"), &data).is_err());
        }
        // A symlink resolves to its real target — a link to a credential
        // file is refused on the target's identity, not the spelling.
        let link = docs.join("innocent.txt");
        std::os::unix::fs::symlink(&key, &link).unwrap();
        assert!(validate_upload_path_at(&link, &data).is_err());
        // Relative + nonexistent are clean errors, not browser crashes.
        assert!(validate_upload_path_at(std::path::Path::new("relative.txt"), &data).is_err());
        assert!(validate_upload_path_at(&base.join("missing.txt"), &data).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn reharden_state_tree_tightens_owned_loose_state() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("blade-reharden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::create_dir_all(root.join("knowledge/domains")).unwrap();
        let sess = root.join("sessions/site.json");
        std::fs::write(&sess, b"{}").unwrap();
        let side = root.join("logins.json");
        std::fs::write(&side, b"[]").unwrap();
        let dom = root.join("knowledge/domains/x.com.json");
        std::fs::write(&dom, b"{}").unwrap();
        // Simulate a mode-stripped restore: everything 0755/0644.
        for (p, m) in [
            (root.clone(), 0o755),
            (root.join("sessions"), 0o755),
            (root.join("knowledge"), 0o755),
            (root.join("knowledge/domains"), 0o755),
            (sess.clone(), 0o644),
            (side.clone(), 0o644),
            (dom.clone(), 0o644),
        ] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m)).unwrap();
        }
        reharden_state_tree(&root);
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("sessions")), 0o700);
        assert_eq!(mode(&sess), 0o600);
        assert_eq!(mode(&side), 0o600);
        assert_eq!(mode(&dom), 0o600);
        // An executable is never de-execed (downloads can carry binaries).
        let dl = root.join("downloads");
        std::fs::create_dir_all(&dl).unwrap();
        let exe = dl.join("tool");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        reharden_state_tree(&root);
        assert_eq!(mode(&exe), 0o755, "exec bit must survive rehardening");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn nofollow_reads_refuse_symlinked_sidecars() {
        let dir = std::env::temp_dir().join(format!("blade-nofollow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("attacker.json");
        std::fs::write(&victim, b"[\"injected\"]").unwrap();
        let link = dir.join("logins.json");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(
            read_file_nofollow(&link).is_err(),
            "a symlinked sidecar must be refused, not read through"
        );
        let real = dir.join("sessions.json");
        std::fs::write(&real, b"ok").unwrap();
        assert_eq!(read_file_nofollow(&real).unwrap(), b"ok");
        assert_eq!(
            read_file_nofollow(&dir.join("missing")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn dir_ancestor_walk_rejects_writable_or_symlinked_paths() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("blade-anc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let safe = base.join("safe/target");
        std::fs::create_dir_all(&safe).unwrap();
        // Sticky /tmp + our own 0755 dirs pass.
        assert!(validate_dir_ancestors(&safe).is_ok());
        // A non-sticky world-writable ancestor refuses.
        let loose = base.join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_dir_ancestors(&loose.join("target")).is_err());
        // A user-planted (non-root) symlinked ancestor refuses.
        let link = base.join("link");
        std::os::unix::fs::symlink(&safe, &link).unwrap();
        assert!(validate_dir_ancestors(&link.join("deeper")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn root_owned_system_symlinks_are_traversed() {
        use std::os::unix::fs::MetadataExt;
        // The OS's own redirects must not trip the walk: macOS routes
        // $TMPDIR through /var → private/var and /tmp → private/tmp;
        // merged-/usr Linux ships /bin → usr/bin. A local co-user cannot
        // create a root-owned symlink, so these are trusted, while the
        // user-owned plant case above still refuses.
        for probe in ["/var", "/tmp", "/bin"] {
            let path = std::path::Path::new(probe);
            let is_root_symlink = path
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink() && m.uid() == 0)
                .unwrap_or(false);
            if is_root_symlink {
                assert!(
                    validate_dir_ancestors(&path.join("blade-ancestor-probe")).is_ok(),
                    "{probe} is a root-owned system symlink and must be traversed"
                );
            }
        }
        // The real temp chain — what BLADE_FRESH creation walks — must
        // pass on every OS (this walk is what failed on macOS CI).
        assert!(validate_dir_ancestors(&std::env::temp_dir().join("blade-ancestor-probe")).is_ok());
    }
}

#[cfg(test)]
mod persistence_reliability_tests {
    use super::*;
    #[test]
    fn invalid_pids_are_never_process_groups() {
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
        assert!(process_alive(std::process::id()));
    }
    #[test]
    fn private_atomic_write_replaces_complete_contents() {
        let dir = std::env::temp_dir().join(format!("blade-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.json");
        secure_write_file(&path, b"old").unwrap();
        secure_write_file(&path, b"new-complete").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new-complete");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary files left"
        );
        // Rename failure must preserve the existing destination and remove its temp.
        assert!(secure_write_file(&dir, b"cannot replace directory").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new-complete");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
