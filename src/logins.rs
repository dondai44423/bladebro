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
//! the live, authoritative cookie store via CDP (`Storage.getCookies`) into a
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
use crate::error::{BladeError, Result};
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
/// domain-cookie semantics, and becomes active once that origin is loaded);
/// Host-only cookies, IP/localhost hosts and `__Host-` cookies use
/// `url`, or Chrome silently drops the cookie.
fn cookie_target(c: &SavedCookie) -> Value {
    // Only __Host- forbids a Domain attribute. __Secure- domain cookies
    // retain their scope; host-only cookies always derive it from a URL.
    if is_ip_or_localhost(&c.domain) || !c.domain.starts_with('.') || c.name.starts_with("__Host-")
    {
        let raw_host = c.domain.trim_start_matches('.');
        let host = if raw_host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{raw_host}]")
        } else {
            raw_host.to_string()
        };
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
/// A closed transport preserves the previous snapshot; a live empty store
/// records logout rather than resurrecting deleted cookies.
pub async fn snapshot(cdp: &CdpSession) -> Result<()> {
    if cdp.is_closed() {
        return Ok(());
    }
    // Page-scoped Network.getCookies omits other sites; Storage reads the
    // browser-wide store, even before the first origin loads.
    let res = cdp.send("Storage.getCookies", None).await?;
    let arr = res
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Other("cookie snapshot missing cookie store".into()))?;
    let mut out: Vec<SavedCookie> = Vec::with_capacity(arr.len());
    for c in arr {
        let domain = c
            .get("domain")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        // Partitioned (third-party Contextual/CHIPS) cookies and internal
        // origins cannot be faithfully restored via setCookies — skip them.
        if c.get("partitionKey").map(|p| !p.is_null()).unwrap_or(false) {
            continue;
        }
        if domain.is_empty() || domain.starts_with("chrome") || domain == "file" {
            continue;
        }
        out.push(SavedCookie {
            name: c
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            value: c
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            domain,
            path: c
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("/")
                .to_string(),
            secure: c.get("secure").and_then(|v| v.as_bool()).unwrap_or(false),
            http_only: c.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false),
            same_site: c
                .get("sameSite")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            expires: c.get("expires").and_then(|v| v.as_f64()),
        });
    }
    let bytes = serde_json::to_vec(&out)?;
    // The browser-wide store is authoritative, including logout/deletion.
    let path = logins_path();
    platform::secure_create_dir_all(&platform::blade_dir())?;
    platform::secure_write_file(&path, &bytes)?;
    Ok(())
}

/// Inject the sidecar snapshot into the fresh browser context at launch.
/// Safe to call before any navigation: `setCookies` writes into the live
/// cookie store and the values win over whatever a stale profile copy left.
pub async fn restore(cdp: &CdpSession) -> Result<()> {
    let list: Vec<SavedCookie> = match std::fs::read(logins_path()) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if list.is_empty() || cdp.is_closed() {
        return Ok(());
    }
    // Enable the Network domain (matches the `state` tool); setCookies is
    // rejected on some Chrome builds unless the domain is live.
    cdp.enable("Network").await?;
    // Chrome caps cookies set in one call; chunk at 150.
    // Track per-cookie rejections (setCookies reports each in `data`) so a
    // silently partial restore is VISIBLE instead of the user just being
    // quietly logged out with no signal.
    let mut rejected = 0usize;
    let total = list.len();
    for chunk in list.chunks(150) {
        let cookies = chunk
            .iter()
            .map(saved_cookie_params)
            .collect::<Result<Vec<_>>>()?;
        match cdp
            .send("Network.setCookies", Some(json!({ "cookies": cookies })))
            .await
        {
            Ok(res) => {
                // Chrome 150+ returns success in `data` per cookie; count the
                // rejected ones so a partial restore is visible. (Some builds
                // drop cookies whose origin has no loaded page yet — warning
                // here, rather than silently losing the login.)
                if let Some(data) = res.get("data").and_then(|d| d.as_array()) {
                    for item in data {
                        if !item
                            .get("success")
                            .and_then(|s| s.as_bool())
                            .unwrap_or(true)
                        {
                            rejected += 1;
                        }
                    }
                }
            }
            Err(e) => {
                rejected += chunk.len();
                eprintln!(
                    "[bladebro] logins restore call failed ({} cookies): {e}",
                    chunk.len()
                );
            }
        }
    }
    if rejected > 0 {
        return Err(BladeError::Other(format!(
            "{rejected}/{total} saved cookies were rejected; login sidecar preserved"
        )));
    }
    let store = cdp.send("Storage.getCookies", None).await?;
    let restored = store
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Other("missing cookie store after login restore".into()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(BladeError::other)?
        .as_secs_f64();
    for saved in &list {
        // Expired persistent cookies should remain deleted.
        if saved.expires.is_some_and(|e| e >= 0.0 && e <= now) {
            continue;
        }
        if !restored.iter().any(|cookie| {
            cookie["name"] == saved.name
                && cookie["value"] == saved.value
                && cookie["domain"] == saved.domain
                && cookie["path"] == saved.path
                && cookie["httpOnly"] == saved.http_only
                && cookie["secure"] == saved.secure
        }) {
            return Err(BladeError::Other(format!(
                "saved cookie {:?} did not match readback; login sidecar preserved",
                saved.name
            )));
        }
    }
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

    #[test]
    fn prefix_cookies_use_url_targeting() {
        // __Host- forbids Domain; __Secure- can remain a domain cookie.
        let host = SavedCookie {
            name: "__Host-sid".into(),
            value: "x".into(),
            domain: "example.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            same_site: None,
            expires: None,
        };
        let t = cookie_target(&host);
        assert_eq!(
            t["url"].as_str(),
            Some("https://example.com/"),
            "__Host- needs url"
        );
        assert!(t.get("domain").is_none(), "__Host- must not carry a domain");

        // __Secure- preserves domain scope.
        let sec = SavedCookie {
            name: "__Secure-tok".into(),
            value: "x".into(),
            domain: ".example.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: None,
            expires: None,
        };
        let t2 = cookie_target(&sec);
        assert_eq!(
            t2["url"].as_str(),
            None,
            "__Secure- preserves a domain cookie"
        );

        // Plain cookies keep domain semantics (host-only vs domain preserved).
        let norm = SavedCookie {
            name: "sid".into(),
            value: "x".into(),
            domain: ".example.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: None,
            expires: None,
        };
        let t3 = cookie_target(&norm);
        assert_eq!(t3["domain"].as_str(), Some(".example.com"));
        assert!(t3.get("url").is_none());

        // IP hosts keep url targeting too.
        let ip = SavedCookie {
            name: "ipc".into(),
            value: "x".into(),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: None,
            expires: None,
        };
        assert_eq!(
            cookie_target(&ip)["url"].as_str(),
            Some("http://127.0.0.1/")
        );
    }

    #[test]
    fn restored_cookie_preserves_session_host_and_domain_scope() {
        let mut c = SavedCookie {
            name: "sid".into(),
            value: "x".into(),
            domain: "example.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            same_site: None,
            expires: Some(-1.0),
        };
        let params = saved_cookie_params(&c).unwrap();
        assert_eq!(params["url"], "https://example.com/");
        assert!(params.get("domain").is_none());
        assert!(
            params.get("expires").is_none(),
            "session cookie must not become expired"
        );
        c.name = "__Secure-sid".into();
        c.domain = ".example.com".into();
        assert_eq!(saved_cookie_params(&c).unwrap()["domain"], ".example.com");
        c.domain = "::1".into();
        assert_eq!(saved_cookie_params(&c).unwrap()["url"], "https://[::1]/");
        c.path = "relative".into();
        assert!(saved_cookie_params(&c).is_err());
    }
}

/// Shared restore encoding for named sessions and the login sidecar.
pub(crate) fn cookie_params(value: &Value) -> Result<Value> {
    if value.get("partitionKey").is_some_and(|key| !key.is_null()) {
        return Err(BladeError::Other(
            "partitioned cookies cannot be restored as ordinary cookies".into(),
        ));
    }
    saved_cookie_params(&serde_json::from_value(value.clone())?)
}

fn saved_cookie_params(c: &SavedCookie) -> Result<Value> {
    if c.domain.is_empty()
        || !c.path.starts_with('/')
        || c.same_site
            .as_deref()
            .is_some_and(|s| !matches!(s, "Strict" | "Lax" | "None"))
    {
        return Err(BladeError::Other(format!(
            "invalid saved cookie {:?}: domain, path or sameSite",
            c.name
        )));
    }
    let mut params = cookie_target(c);
    params["name"] = json!(c.name);
    params["value"] = json!(c.value);
    params["path"] = json!(c.path);
    params["secure"] = json!(c.secure);
    params["httpOnly"] = json!(c.http_only);
    if let Some(same_site) = &c.same_site {
        params["sameSite"] = json!(same_site);
    }
    if let Some(expires) = c.expires.filter(|e| *e >= 0.0) {
        params["expires"] = json!(expires);
    }
    Ok(params)
}
