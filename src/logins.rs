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
    let res = cdp.cookie_store().await?;
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
    let sidecar = logins_path();
    if std::fs::symlink_metadata(&sidecar)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        // SECURITY: a symlinked sidecar is attack state, not a login store —
        // refusing it prevents re-injecting attacker-authored cookies into
        // every future browser context.
        return Err(BladeError::Other(
            "logins sidecar is a symlink — refusing to restore it".into(),
        ));
    }
    let list: Vec<SavedCookie> = match crate::platform::read_file_nofollow(&sidecar) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if list.is_empty() || cdp.is_closed() {
        return Ok(());
    }
    let missing = restore_cookies(cdp, &list).await?;
    if missing > 0 {
        // A cookie Chrome refuses at this launch is refused at every launch.
        // Failing the launch over it shut Chrome down with the sidecar kept,
        // so every later launch failed the same way. Go on without those
        // cookies (the next snapshot records the store as it really is) and
        // say how many were lost.
        eprintln!(
            "[bladebro] login restore: {missing}/{} saved cookies were not restored (rejected or did not read back); continuing without them",
            list.len()
        );
    }
    Ok(())
}

/// Set `list` in the live store and read it back. Returns how many saved
/// cookies are missing from the store afterwards; only a failed readback
/// is an error.
async fn restore_cookies(cdp: &CdpSession, list: &[SavedCookie]) -> Result<usize> {
    // Enable the Network domain (matches the `state` tool); setCookies is
    // rejected on some Chrome builds unless the domain is live.
    cdp.enable("Network").await?;
    // Chrome caps cookies set in one call; chunk at 150. Whether each cookie
    // landed is judged by the readback below rather than the call result:
    // builds differ in what setCookies reports, and some drop cookies whose
    // origin has no loaded page yet.
    for chunk in list.chunks(150) {
        // An entry that cannot be encoded is not sent; the readback counts it.
        let cookies: Vec<Value> = chunk
            .iter()
            .filter_map(|c| saved_cookie_params(c).ok())
            .collect();
        if cookies.is_empty() {
            continue;
        }
        match cdp
            .send("Network.setCookies", Some(json!({ "cookies": cookies })))
            .await
        {
            Ok(_) => {}
            // One cookie Chrome cannot parse fails the whole call ("Invalid
            // cookie fields"). Now that a restore goes on without missing
            // cookies, the next snapshot would then drop every login in
            // the chunk, so set them one by one and lose only that cookie.
            Err(BladeError::Cdp { .. }) => {
                for cookie in cookies {
                    match cdp.send("Network.setCookie", Some(cookie)).await {
                        Ok(_) | Err(BladeError::Cdp { .. }) => {}
                        // A dead or wedged transport: stop rather than wait
                        // out a timeout per cookie; the readback decides.
                        Err(_) => break,
                    }
                }
            }
            Err(e) => eprintln!(
                "[bladebro] logins restore call failed ({} cookies): {e}",
                cookies.len()
            ),
        }
    }
    let store = cdp.cookie_store().await?;
    let restored = store
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Other("missing cookie store after login restore".into()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(BladeError::other)?
        .as_secs_f64();
    let missing = list
        .iter()
        // Expired persistent cookies should remain deleted.
        .filter(|saved| !saved.expires.is_some_and(|e| e >= 0.0 && e <= now))
        .filter(|saved| {
            !restored.iter().any(|cookie| {
                cookie["name"] == saved.name
                    && cookie["value"] == saved.value
                    && cookie["domain"] == saved.domain
                    && cookie["path"] == saved.path
                    && cookie["httpOnly"] == saved.http_only
                    && cookie["secure"] == saved.secure
            })
        })
        .count();
    Ok(missing)
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

    fn saved(name: &str, same_site: &str) -> SavedCookie {
        SavedCookie {
            name: name.into(),
            value: "v".into(),
            domain: ".fixture.test".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: Some(same_site.into()),
            expires: Some(4_000_000_000.0),
        }
    }

    /// One cookie Chrome refuses (SameSite=None without Secure) used to fail
    /// the whole restore, and with it every browser launch: the launcher
    /// shut Chrome down, the sidecar was kept, and the next launch failed
    /// the same way. The good cookies must be restored and the launch go on.
    /// A cookie Chrome cannot parse at all (`__Secure-` without Secure)
    /// fails the whole setCookies batch; it must not take the good cookies
    /// of its batch with it.
    #[tokio::test]
    async fn a_refused_cookie_does_not_fail_the_login_restore() {
        let (cdp, jar) = crate::cdp::fake_jar::start("http://127.0.0.1:8080", vec![]).await;
        let list = [
            saved("good", "Lax"),
            saved("bad", "None"),
            saved("__Secure-bad", "Lax"),
            saved("other", "Lax"),
        ];
        let missing = restore_cookies(&cdp, &list)
            .await
            .expect("a refused cookie must not fail the restore");
        assert_eq!(missing, 2, "the refused cookies are counted, not fatal");
        assert_eq!(jar.names(), ["good", "other"]);
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
