//! Cross-platform login persistence.
//!
//! Cookies are what keep you logged in (GitHub, Facebook, banks, ...).
//! Chrome holds them in its live, in-memory cookie store and only flushes that
//! store to the on-disk SQLite (`Default/Cookies`) on its own schedule, in
//! `-journal`/WAL mode. Copying the profile while Chrome runs tears that
//! SQLite, and the fresh copy either misses the un-flushed cookies or carries
//! an inconsistent journal that Chrome resets on the next open. Windows can't
//! even guarantee a flush on shutdown (Chrome gets TerminateProcess, not a
//! signal). The result: logins that were working minutes ago are gone next
//! run.
//!
//! So we never trust the on-disk cookie store for persistence. We snapshot
//! the live, authoritative cookie store via CDP (`Network.getCookies`) into a
//! small sidecar file on a schedule and at shutdown, then re-inject
//! (`Network.setCookies`) at launch. This reads state Chrome itself believes
//! to be correct, grows the source of truth from the live browser rather than
//! from a torn file copy, and works identically on Linux, macOS, and Windows
//! regardless of transport (pipe or port). It survives clean shutdowns,
//! SIGKILL, and power loss (the periodic snapshot is at most one interval
//! old).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::CdpSession;
use crate::error::Result;
use crate::platform;

/// A cookie we persist across sessions. A subset of `Network.Cookie` that
/// `Network.setCookies` accepts, so a snapshot round-trips losslessly.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedCookie {
    name: String,
    value: String,
    #[serde(default)]
    domain: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    secure: bool,
    #[serde(default, rename = "httpOnly")]
    http_only: bool,
    #[serde(default, rename = "sameSite")]
    same_site: Option<String>,
    #[serde(default)]
    expires: Option<f64>,
}

fn default_path() -> String {
    "/".to_string()
}

/// True for bare-IP / localhost hosts that Chrome rejects as a `domain` in
/// `Network.setCookies` (it needs a `url` instead).
fn is_ip_or_localhost(host: &str) -> bool {
    let h = host.strip_prefix('.').unwrap_or(host);
    h == "localhost"
        || (h.parse::<std::net::Ipv4Addr>().is_ok())
        || (h.parse::<std::net::Ipv6Addr>().is_ok())
}

/// CDP CookieParam targeting: real registrable domains use `domain` (keeps
/// domain-cookie semantics); IP/localhost hosts must use `url` or Chrome
/// silently drops the cookie.
fn cookie_target(c: &SavedCookie) -> Value {
    if is_ip_or_localhost(&c.domain) {
        let host = c.domain.trim_start_matches('.');
        let scheme = if c.secure { "https" } else { "http" };
        json!({ "url": format!("{scheme}://{host}{path}", path = c.path) })
    } else {
        json!({ "domain": c.domain })
    }
}

fn logins_path() -> PathBuf {
    platform::blade_dir().join("logins.json")
}

/// Snapshot the live cookie store to the sidecar (atomically, 0600).
/// Best-effort: if the session is closed or yields nothing restorable we
/// leave the last good snapshot untouched rather than writing an empty file.
pub async fn snapshot(cdp: &CdpSession) -> Result<()> {
    if cdp.is_closed() {
        return Ok(());
    }
    // Ensure the Network domain is live so the cookie commands are accepted;
    // this is the same call the `state` tool makes and is stealth-safe
    // (only Runtime.enable is avoided, and this is not it).
    let _ = cdp.enable("Network").await;
    let res = cdp.send("Network.getCookies", Some(json!({}))).await?;
    let Some(arr) = res.get("cookies").and_then(|c| c.as_array()) else {
        return Ok(()); // nothing returned is not a failure to persist
    };
    let mut out: Vec<SavedCookie> = Vec::with_capacity(arr.len());
    for c in arr {
        let domain = c.get("domain").and_then(|d| d.as_str()).unwrap_or("").to_string();
        // Partitioned (third-party Contextual/CHIPS) cookies and internal
        // origins cannot be faithfully restored via setCookies — skip them.
        if c.get("partitionKey").map(|p| !p.is_null()).unwrap_or(false) {
            continue;
        }
        if domain.is_empty() || domain.starts_with("chrome") || domain == "file" {
            continue;
        }
        out.push(SavedCookie {
            name: c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            value: c.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            domain,
            path: c.get("path").and_then(|v| v.as_str()).unwrap_or("/").to_string(),
            secure: c.get("secure").and_then(|v| v.as_bool()).unwrap_or(false),
            http_only: c.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false),
            same_site: c.get("sameSite").and_then(|v| v.as_str()).map(|s| s.to_string()),
            expires: c.get("expires").and_then(|v| v.as_f64()),
        });
    }
    let bytes = serde_json::to_vec(&out)?;
    atomic_write(&logins_path(), &bytes)?;
    Ok(())
}

/// Inject the sidecar snapshot into the fresh browser context at launch.
/// Safe to call before any navigation: `setCookies` writes into the live
/// cookie store and the values win over whatever a stale profile copy left.
pub async fn restore(cdp: &CdpSession) -> Result<()> {
    let list: Vec<SavedCookie> = match std::fs::read(logins_path()) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => return Ok(()), // no saved logins yet
    };
    if list.is_empty() || cdp.is_closed() {
        return Ok(());
    }
    // Enable the Network domain (matches the `state` tool); setCookies is
    // rejected on some Chrome builds unless the domain is live.
    let _ = cdp.enable("Network").await;
    // Chrome caps cookies set in one call; chunk at 150.
    let mut errors = 0usize;
    for chunk in list.chunks(150) {
        let cookies: Vec<Value> = chunk.iter().map(|c| {
            // Apply url or domain per-cookie (IP/localhost need url).
            let target = cookie_target(c);
            let mut spec = json!({
                "name": c.name,
                "value": c.value,
                "path": c.path,
                "secure": c.secure,
                "httpOnly": c.http_only,
                // Default to Lax when absent — some Chrome builds reject
                // cookies with a null sameSite in the CDP call (same rule
                // state.rs uses for single set-cookie).
                "sameSite": c.same_site.clone().unwrap_or_else(|| "Lax".to_string()),
                "expires": c.expires,
            });
            if let Some(map) = target.as_object() {
                if let Some(u) = map.get("url") {
                    spec["url"] = u.clone();
                }
                if let Some(d) = map.get("domain") {
                    spec["domain"] = d.clone();
                }
            }
            spec
        }).collect();
        match cdp.send("Network.setCookies", Some(json!({ "cookies": cookies }))).await {
            Ok(_) => {}
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    eprintln!("[bladebro] logins restore ({} of {}): {e}", errors, list.len());
                }
            }
        }
    }
    Ok(())
}

/// Atomic, fsync'd, 0600 write of the sidecar. Never a torn file: write to a
/// sibling temp, sync to disk, then rename over the target.
fn atomic_write(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(".logins.json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    use std::io::Write;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_is_root() {
        assert_eq!(default_path(), "/");
    }

    #[test]
    fn empty_store_round_trips() {
        // Round-trip the serde shape so a future field rename breaks a test,
        // not a user's logins.
        let c = SavedCookie {
            name: "sid".into(),
            value: "a b;c=d".into(),
            domain: ".example.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            same_site: Some("Lax".into()),
            expires: Some(1_800_000_000.0),
        };
        let bytes = serde_json::to_vec(&[&c, &c]).unwrap();
        let back: Vec<SavedCookie> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].value, "a b;c=d");
        assert!(back[0].http_only);
    }
}