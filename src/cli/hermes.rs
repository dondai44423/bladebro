//! Reversible Hermes setup through its public CLI. The browser remains the
//! ordinary MCP server: no copied schemas, Hermes imports, plugin or model code.

use crate::error::{BladeError, Result};
use crate::platform;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;

const LIMIT: u64 = 256 * 1024;
const SERVER: &str = "mcp_servers.bladebro";
const DISABLED: &str = "agent.disabled_toolsets";

struct Hermes {
    command: String,
    profile: Option<String>,
}

impl Hermes {
    async fn call(&self, args: &[&str]) -> Result<(bool, String)> {
        let mut cmd = tokio::process::Command::new(&self.command);
        if let Some(profile) = &self.profile {
            cmd.args(["-p", profile]);
        }
        let mut child = cmd
            .args(args)
            .env("NO_COLOR", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                BladeError::Other(format!(
                    "cannot start Hermes: {e}; install its CLI or pass --hermes <path>"
                ))
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BladeError::other("Hermes stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BladeError::other("Hermes stderr unavailable"))?;
        let read = async move { tokio::try_join!(bounded(stdout), bounded(stderr)) };
        let result = tokio::time::timeout(std::time::Duration::from_secs(180), async {
            let ((out, err), status) = tokio::try_join!(read, async { child.wait().await.map_err(BladeError::from) })?;
            // Config reads may return defaults after a malformed YAML warning.
            // Never turn that fallback into a successful installation.
            if !status.success() && args.get(..2) == Some(&["config", "get"]) {
                let missing = format!("Config key not set: {}", args.get(2).unwrap_or(&""));
                if out.is_empty() && String::from_utf8_lossy(&err).trim() == missing {
                    return Ok((false, missing));
                }
            }
            if !err.is_empty() && args.get(..2) == Some(&["config", "get"]) {
                return Err(BladeError::other("Hermes reported a diagnostic; run `hermes config check` and resolve it before switching browsers"));
            }
            Ok((status.success(), String::from_utf8(out).map_err(BladeError::other)?))
        }).await.map_err(|_| BladeError::other("Hermes did not finish within 180s; setup state was retained — retry the command"))?;
        result
    }

    async fn get(&self, key: &str) -> Result<Option<Value>> {
        let (ok, out) = self
            .call(&["config", "get", key, "--json", "--raw"])
            .await?;
        if !ok {
            if out.trim() == format!("Config key not set: {key}") {
                return Ok(None);
            }
            return Err(BladeError::Other(format!(
                "Hermes could not read {key}; requires `hermes config get --json --raw`"
            )));
        }
        Ok(Some(serde_json::from_str(&out)?))
    }

    async fn write(&self, key: &str, value: Option<&Value>) -> Result<()> {
        let encoded;
        let args = if let Some(value) = value {
            encoded = serde_json::to_string(value)?;
            vec!["config", "set", key, &encoded]
        } else {
            vec!["config", "unset", key]
        };
        let (ok, _) = self.call(&args).await?;
        if !ok || self.get(key).await? != value.cloned() {
            return Err(BladeError::Other(format!("Hermes did not persist {key}; recovery state retained — retry `bladebro hermes off`")));
        }
        Ok(())
    }
}

async fn bounded(stream: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    stream.take(LIMIT + 1).read_to_end(&mut out).await?;
    if out.len() as u64 > LIMIT {
        return Err(BladeError::other("Hermes output exceeds 256 KiB"));
    }
    Ok(out)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    config: PathBuf,
    before: Option<Value>,
    installed: Value,
    browser_was_disabled: bool,
}

fn disabled(value: Option<Value>) -> Result<Vec<String>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => Ok(items
            .into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()),
        // Hermes accepts a bare name as one item. Ambiguous serialized lists
        // are refused rather than silently losing an existing suppression.
        Some(Value::String(s)) if !s.trim_start().starts_with('[') => Ok(vec![s]),
        _ => Err(BladeError::other("agent.disabled_toolsets must be a list of names; normalize it with `hermes config set agent.disabled_toolsets '[\"browser\"]'` (retain your other disabled names)")),
    }
}

fn has_browser(names: &[String]) -> bool {
    names
        .iter()
        .any(|n| matches!(n.trim(), "browser" | "browser_tools"))
}

fn check_file(path: &Path) -> Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !md.is_file() || md.file_type().is_symlink() {
        return Err(BladeError::Other(format!(
            "refusing unsafe setup file {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if md.uid() != unsafe { libc::geteuid() } || md.mode() & 0o077 != 0 {
            return Err(BladeError::Other(format!(
                "setup file must be owned by you and private (0600): {}",
                path.display()
            )));
        }
    }
    Ok(())
}

struct SetupGuard {
    _file: std::fs::File,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        // A concurrent fork can retain our open file description until exec.
        // Unlock explicitly so it cannot prolong the parent's critical section.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

fn lock(root: &Path) -> Result<SetupGuard> {
    platform::validate_dir_ancestors(&root.join("state.json")).map_err(BladeError::other)?;
    if let Ok(md) = std::fs::symlink_metadata(root) {
        if !md.is_dir() || md.file_type().is_symlink() {
            return Err(BladeError::other("refusing unsafe Hermes setup directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if md.uid() != unsafe { libc::geteuid() } || md.mode() & 0o077 != 0 {
                return Err(BladeError::other(
                    "Hermes setup directory must be owned by you and private (0700)",
                ));
            }
        }
    }
    platform::secure_create_dir_all(root)?;
    let path = root.join("guard");
    check_file(&path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    let file = opts
        .open(path)
        .map_err(|e| BladeError::Other(format!("Hermes setup is busy or inaccessible: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(BladeError::other(
                "Hermes setup is busy; retry when the other switch finishes",
            ));
        }
    }
    Ok(SetupGuard { _file: file })
}

pub(super) async fn run(args: &[String], json_mode: bool) -> Result<()> {
    let mut hermes = Hermes {
        command: "hermes".into(),
        profile: None,
    };
    let mut action = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" | "--hermes" => {
                let value = iter
                    .next()
                    .filter(|s| !s.trim().is_empty() && !s.starts_with('-'))
                    .ok_or_else(|| BladeError::Usage(format!("{arg} needs a value")))?;
                if arg == "--profile" {
                    hermes.profile = Some(value.clone());
                } else {
                    hermes.command = value.clone();
                }
            }
            "on" | "off" | "status" if action.is_none() => action = Some(arg.as_str()),
            _ => {
                return Err(BladeError::Usage(
                    "bladebro hermes on|off|status [--profile <name>] [--hermes <path>] [--json]"
                        .into(),
                ))
            }
        }
    }
    let action = action.unwrap_or("status");
    let (ok, config) = hermes.call(&["config", "path"]).await?;
    let config = PathBuf::from(config.trim());
    if !ok || !config.is_absolute() {
        return Err(BladeError::other(
            "Hermes did not return an absolute config path",
        ));
    }
    let root = config
        .parent()
        .ok_or_else(|| BladeError::other("Hermes config has no parent"))?
        .join(".bladebro-browser");
    let _lock = lock(&root)?;
    let state = root.join("state.json");
    check_file(&state)?;
    let saved: Option<Saved> = match platform::open_nofollow(&state) {
        Ok(file) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(LIMIT + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > LIMIT {
                return Err(BladeError::other("Hermes setup state is too large"));
            }
            let s: Saved = serde_json::from_slice(&bytes)?;
            if s.version != 1 || s.config != config {
                return Err(BladeError::other(
                    "Hermes setup state belongs to another config or version; no settings changed",
                ));
            }
            Some(s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let agent = hermes
        .get("agent")
        .await?
        .ok_or_else(|| BladeError::other("Hermes agent config is unavailable"))?;
    if !agent.is_object() {
        return Err(BladeError::other("Hermes agent config must be a mapping"));
    }
    let mut names = disabled(agent.get("disabled_toolsets").cloned())?;
    let servers = hermes
        .get("mcp_servers")
        .await?
        .unwrap_or_else(|| json!({}));
    if !servers.is_object() {
        return Err(BladeError::other("Hermes mcp_servers must be a mapping"));
    }
    let current = servers.get("bladebro").cloned();
    let text = match action {
        "status" => {
            let active = saved
                .as_ref()
                .is_some_and(|s| current.as_ref() == Some(&s.installed))
                && has_browser(&names);
            format!("Hermes browser: {}. Config: {}. {}", if active { "Bladebro configured" } else if saved.is_some() { "setup pending or settings changed" } else { "unmanaged" }, config.display(), "Start a new Hermes chat after switching; restart running gateways/desktop clients.")
        }
        "on" => {
            let s = if let Some(s) = saved {
                if current != s.before && current.as_ref() != Some(&s.installed) {
                    return Err(BladeError::other("mcp_servers.bladebro changed after setup; no settings changed. Restore the saved entry before retrying, or review .bladebro-browser/state.json"));
                }
                s
            } else {
                let exe = std::env::current_exe()?;
                let exe = exe
                    .to_str()
                    .ok_or_else(|| BladeError::other("Bladebro install path must be UTF-8"))?;
                // Hermes filters the parent environment. Keep existing server
                // options and explicit data/browser paths instead of silently
                // launching against a different profile or Chromium install.
                let mut env = match current.as_ref().and_then(|s| s.get("env")) {
                    None => serde_json::Map::new(),
                    Some(Value::Object(env)) => env.clone(),
                    _ => {
                        return Err(BladeError::other(
                            "mcp_servers.bladebro.env must be a mapping; no settings changed",
                        ))
                    }
                };
                for key in ["BLADE_HOME", "CHROME_PATH"] {
                    if !env.contains_key(key) {
                        if let Some(value) = std::env::var_os(key) {
                            let value = value
                                .to_str()
                                .ok_or_else(|| BladeError::Other(format!("{key} must be UTF-8")))?;
                            let path = PathBuf::from(value);
                            let path = if value.is_empty() || path.is_absolute() {
                                path
                            } else {
                                std::env::current_dir()?.join(path)
                            };
                            env.insert(key.into(), serde_json::to_value(path)?);
                        }
                    }
                }
                let mut installed = json!({"command":exe,"args":["mcp"],"enabled":true});
                if !env.is_empty() {
                    installed["env"] = json!(env);
                }
                let s = Saved {
                    version: 1,
                    config: config.clone(),
                    before: current.clone(),
                    installed,
                    browser_was_disabled: has_browser(&names),
                };
                // Recovery precedes both writes; an interrupted setup can always
                // resume or restore only the settings it owns.
                let encoded = serde_json::to_vec(&s)?;
                if encoded.len() as u64 > LIMIT {
                    return Err(BladeError::other("Hermes recovery state exceeds 256 KiB; no settings changed — reduce the existing Bladebro server environment before switching"));
                }
                platform::secure_write_file(&state, &encoded)?;
                s
            };
            if current.as_ref() != Some(&s.installed) {
                hermes.write(SERVER, Some(&s.installed)).await?;
            }
            let (connected, _) = hermes.call(&["mcp", "test", "bladebro"]).await?;
            if !connected {
                return Err(BladeError::other("Hermes could not connect to Bladebro; built-in browser suppression was not changed. Run `hermes mcp test bladebro`, then retry `bladebro hermes on` or restore with `bladebro hermes off`"));
            }
            if !has_browser(&names) {
                names.push("browser".into());
                hermes.write(DISABLED, Some(&json!(names))).await?;
            }
            "Bladebro configured through Hermes MCP; built-in browser tools disabled, web tools preserved. Start a new chat; restart running gateways/desktop clients. Revert: bladebro hermes off (same profile). Chrome/Chromium must be installed; `bladebro doctor` checks browser prerequisites.".into()
        }
        "off" => {
            if let Some(s) = saved {
                if current != s.before && current.as_ref() != Some(&s.installed) {
                    return Err(BladeError::other("mcp_servers.bladebro changed after setup; refusing to overwrite it. Review .bladebro-browser/state.json and restore the saved entry before retrying"));
                }
                // Restore the original browser policy before removing the MCP
                // entry. Repeated off after a crash is safe at either boundary.
                if !s.browser_was_disabled && has_browser(&names) {
                    names.retain(|n| !matches!(n.trim(), "browser" | "browser_tools"));
                    hermes.write(DISABLED, Some(&json!(names))).await?;
                }
                if current != s.before {
                    hermes.write(SERVER, s.before.as_ref()).await?;
                }
                std::fs::remove_file(&state)?;
                "Previous Hermes browser settings restored; unrelated settings preserved. Start a new chat; restart running gateways/desktop clients.".into()
            } else {
                "No managed Hermes setup to revert; settings unchanged.".into()
            }
        }
        _ => unreachable!(),
    };
    if json_mode {
        println!(
            "{}",
            json!({"ok":true,"is_error":false,"text":text,"config":config})
        );
    } else {
        println!("{text}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suppression_preserves_names_and_refuses_ambiguous_types() {
        let names = disabled(Some(json!(["terminal", " browser "]))).unwrap();
        assert_eq!(names, vec!["terminal", " browser "]);
        assert!(has_browser(&names));
        assert!(has_browser(
            &disabled(Some(json!("browser_tools"))).unwrap()
        ));
        for v in [
            json!(42),
            json!({}),
            json!(["browser", 1]),
            json!("['browser']"),
        ] {
            assert!(disabled(Some(v)).is_err());
        }
        assert!(disabled(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn stream_limit_is_reached_and_never_truncates_silently() {
        assert_eq!(bounded(&b"Hermes"[..]).await.unwrap(), b"Hermes");
        let exact = vec![b'x'; LIMIT as usize];
        assert_eq!(bounded(&exact[..]).await.unwrap().len() as u64, LIMIT);
        let overflow = vec![b'x'; LIMIT as usize + 1];
        assert!(bounded(&overflow[..])
            .await
            .unwrap_err()
            .to_string()
            .contains("256 KiB"));
    }

    fn scratch() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("blade-hermes-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn setup_guard_serializes_and_releases_without_a_stale_pid_marker() {
        let root = scratch();
        let first = lock(&root).unwrap();
        assert!(lock(&root).is_err());
        #[cfg(unix)]
        let child = unsafe { libc::fork() };
        #[cfg(unix)]
        {
            assert!(child >= 0);
            if child == 0 {
                // Only async-signal-safe syscalls after fork in this threaded
                // test process; keep the inherited descriptor alive until kill.
                unsafe {
                    libc::pause();
                    libc::_exit(0);
                }
            }
        }
        drop(first);
        let second = lock(&root);
        #[cfg(unix)]
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, std::ptr::null_mut(), 0);
        }
        let second = second.unwrap();
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlinks_and_public_files_without_touching_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = scratch();
        platform::secure_create_dir_all(&root).unwrap();
        let target = root.join("target");
        std::fs::write(&target, "sentinel").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_file(&target).is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_file(&target).is_ok());
        let link = root.join("state.json");
        symlink(&target, &link).unwrap();
        assert!(check_file(&link).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "sentinel");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    async fn fake(script: &str) -> Result<Option<Value>> {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        platform::secure_create_dir_all(&root).unwrap();
        let command = root.join("hermes");
        std::fs::write(&command, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let h = Hermes {
            command: command.to_str().unwrap().into(),
            profile: None,
        };
        let result = h.get("mcp_servers").await;
        std::fs::remove_dir_all(root).unwrap();
        result
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn public_cli_missing_key_is_distinct_from_failure_and_fallback_defaults() {
        assert_eq!(
            fake("printf 'Config key not set: mcp_servers\\n' >&2; exit 1")
                .await
                .unwrap(),
            None
        );
        assert_eq!(fake("printf '{}\\n'").await.unwrap(), Some(json!({})));
        assert!(
            fake("printf '{}\\n'; printf 'invalid YAML; using defaults\\n' >&2")
                .await
                .is_err()
        );
        assert!(fake("printf 'permission denied\\n' >&2; exit 1")
            .await
            .is_err());
        assert!(fake("printf 'not JSON\\n'").await.is_err());
    }
}
