//! The `state` tool (decision D5) — cookies, storage, and tabs.
//!
//! The agent rarely touches this, but when it does, it needs full control:
//! read/write cookies (auth flows), inspect localStorage/sessionStorage (SPA
//! state), and manage tabs (multi-page workflows). One tool, three domains.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};

/// Percent-encode a cookie value for the `document.cookie` JS fallback.
/// Raw values containing `;`, `=`, `,`, or whitespace are silently
/// truncated or reshaped by the cookie parser. RFC 3986 unreserved set.
fn js_percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What the agent wants to do with page/browser state.
#[derive(Debug, Clone)]
pub enum StateOp {
    /// Read cookies for the current page (or all browser cookies if `urls` empty).
    GetCookies { urls: Vec<String> },
    /// Set a cookie.
    SetCookie {
        name: String,
        value: String,
        /// Page URL — CDP uses this to derive domain. Preferred over `domain`.
        url: Option<String>,
        domain: Option<String>,
        path: Option<String>,
        secure: Option<bool>,
        http_only: Option<bool>,
        same_site: Option<String>,
    },
    /// Delete cookies by name (optionally filtered by domain or url).
    DeleteCookies { name: String, domain: Option<String>, url: Option<String> },
    /// Read all localStorage keys/values.
    GetLocalStorage,
    /// Read all sessionStorage keys/values.
    GetSessionStorage,
    /// Set a localStorage key.
    SetLocalStorage { key: String, value: String },
    /// Set a sessionStorage key.
    SetSessionStorage { key: String, value: String },
    /// Remove a localStorage key.
    RemoveLocalStorage { key: String },
    /// Remove a sessionStorage key.
    RemoveSessionStorage { key: String },
    /// Clear all localStorage.
    ClearLocalStorage,
    /// Clear all sessionStorage.
    ClearSessionStorage,
    /// List all open page targets.
    ListTabs,
    /// Open a new tab with the given URL.
    OpenTab { url: String },
    /// Close a tab by target id.
    CloseTab { target_id: String },
    /// Save cookies + localStorage to ~/.blade/sessions/<name>.json.
    SaveSession { name: String },
    /// Load cookies + localStorage from a saved session.
    LoadSession { name: String },
}

/// A cookie as returned by `Network.getCookies`.
#[derive(Debug, Clone, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default, rename = "httpOnly")]
    pub http_only: bool,
    #[serde(default, rename = "sameSite")]
    pub same_site: Option<String>,
    #[serde(default)]
    pub expires: Option<f64>,
}

/// A storage entry (key + value) from localStorage/sessionStorage.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageEntry {
    pub key: String,
    pub value: String,
}

/// A tab (page target) from `Target.getTargets`.
#[derive(Debug, Clone, Deserialize)]
pub struct Tab {
    #[serde(rename = "targetId")]
    pub target_id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// True if a CDP session is attached (i.e. this is the tab
    /// bladebro is currently driving).
    #[serde(default)]
    pub attached: bool,
}

/// Perform a state operation and return a compact, agent-facing result string.
pub async fn perform(cdp: &CdpSession, op: &StateOp) -> Result<String> {
    match op {
        // ---- cookies ----

        StateOp::GetCookies { urls } => {
            cdp.enable("Network").await?;
            let params = if urls.is_empty() {
                json!({})
            } else {
                json!({ "urls": urls })
            };
            let res = cdp.send("Network.getCookies", Some(params)).await?;
            let cookies: Vec<Cookie> = serde_json::from_value(
                res.get("cookies")
                    .cloned()
                    .ok_or_else(|| BladeError::Other("no cookies field".into()))?,
            )?;
            if cookies.is_empty() {
                return Ok("(no cookies)".into());
            }
            let mut out = String::new();
            for c in &cookies {
                out.push_str(&format!(
                    "{}={} domain={}{}{}\n",
                    c.name,
                    truncate(&c.value, 60),
                    c.domain,
                    if c.secure { " secure" } else { "" },
                    if c.http_only { " httpOnly" } else { "" },
                ));
            }
            Ok(out)
        }

        StateOp::SetCookie {
            name,
            value,
            url,
            domain,
            path,
            secure,
            http_only,
            same_site,
        } => {
            cdp.enable("Network").await?;
            let mut params = json!({
                "name": name,
                "value": value,
            });
            // CDP Network.setCookie requires either `url` or `domain`.
            // Prefer `url` when given — Chrome derives the domain from it.
            if let Some(u) = url {
                params["url"] = json!(u);
            } else if let Some(d) = domain {
                params["domain"] = json!(d);
            }
            if let Some(p) = path {
                params["path"] = json!(p);
            }
            if let Some(s) = secure {
                params["secure"] = json!(s);
            }
            if let Some(h) = http_only {
                params["httpOnly"] = json!(h);
            }
            // Default sameSite to "Lax" — Chrome 80+ defaults to Lax,
            // but some Chrome versions fail cookie sanitization when
            // sameSite is omitted from the CDP call entirely.
            let ss = same_site.as_deref().unwrap_or("Lax");
            params["sameSite"] = json!(ss);

            #[allow(clippy::needless_late_init)]
            let cdp_err;
            match cdp.send("Network.setCookie", Some(params)).await {
                Ok(res) => {
                    let ok = res.get("success").and_then(|v| v.as_bool()).unwrap_or(true);
                    if ok {
                        return Ok(format!("✓ cookie set: {name}={}", truncate(value, 40)));
                    }
                    // CDP returned success=false — fall through to JS fallback.
                    cdp_err = "CDP success=false".into();
                }
                Err(e) => {
                    // CDP error — fall through to JS fallback (reason kept
                    // for the failure message; the old code discarded it).
                    cdp_err = e.to_string();
                }
            }

            // Fallback: set cookie via document.cookie in JavaScript.
            // Works even when CDP Network.setCookie fails (Chrome version
            // quirks, partitioned cookies, sanitizer edge cases).
            // The value is percent-encoded: a raw value containing ';', '=',
            // ',', or whitespace used to be silently truncated or reshaped
            // by the cookie parser.
            let mut cookie_parts = vec![format!("{name}={}", js_percent_encode(value))];
            if let Some(d) = domain {
                cookie_parts.push(format!("domain={d}"));
            }
            cookie_parts.push(format!("path={}", path.as_deref().unwrap_or("/")));
            if matches!(secure, Some(true)) {
                cookie_parts.push("secure".to_string());
            }
            // httpOnly can't be set via document.cookie — skip it.
            cookie_parts.push(format!("samesite={}", same_site.as_deref().unwrap_or("Lax")));
            let cookie_str = cookie_parts.join("; ");
            // JSON-escape into a JS string literal. Rust's `{:?}` Debug
            // escaping is NOT JS escaping — edge-case characters produced
            // invalid JS (or changed semantics).
            let cookie_js = serde_json::to_string(&cookie_str)
                .map_err(|e| BladeError::Other(format!("cookie encode: {e}")))?;
            let js = format!("document.cookie={cookie_js}");
            match cdp.send("Runtime.evaluate", Some(json!({
                "expression": js,
                "returnByValue": true,
            }))).await {
                Ok(_) => Ok(format!("✓ cookie set (via JS): {name}={}", truncate(value, 40))),
                Err(e) => Ok(format!("✗ cookie set failed: {name} (CDP: {cdp_err}, JS: {e})")),
            }
        }

        StateOp::DeleteCookies { name, domain, url } => {
            cdp.enable("Network").await?;
            let mut params = json!({ "name": name });
            // CDP Network.deleteCookies requires either url or domain.
            // Prefer url when given (Chrome derives domain), fall back to domain.
            if let Some(u) = url {
                params["url"] = json!(u);
            } else if let Some(d) = domain {
                params["domain"] = json!(d);
            }
            cdp.send("Network.deleteCookies", Some(params)).await?;
            Ok(format!("✓ cookie deleted: {name}"))
        }

        // ---- storage ----

        StateOp::GetLocalStorage => get_storage(cdp, "localStorage").await,
        StateOp::GetSessionStorage => get_storage(cdp, "sessionStorage").await,
        StateOp::SetLocalStorage { key, value } => {
            set_storage(cdp, "localStorage", key, value).await
        }
        StateOp::SetSessionStorage { key, value } => {
            set_storage(cdp, "sessionStorage", key, value).await
        }
        StateOp::RemoveLocalStorage { key } => {
            remove_storage(cdp, "localStorage", key).await
        }
        StateOp::RemoveSessionStorage { key } => {
            remove_storage(cdp, "sessionStorage", key).await
        }
        StateOp::ClearLocalStorage => clear_storage(cdp, "localStorage").await,
        StateOp::ClearSessionStorage => clear_storage(cdp, "sessionStorage").await,

        // ---- tabs ----

        StateOp::ListTabs => {
            let res = cdp.send("Target.getTargets", None).await?;
            let targets = res
                .get("targetInfos")
                .cloned()
                .ok_or_else(|| BladeError::Other("no targetInfos field".into()))?;
            let all: Vec<Value> = serde_json::from_value(targets)?;
            let tabs: Vec<Tab> = all
                .into_iter()
                .filter(|t| {
                    t.get("type").and_then(|v| v.as_str()) == Some("page")
                })
                .filter_map(|t| serde_json::from_value(t).ok())
                .collect();
            if tabs.is_empty() {
                return Ok("(no tabs)".into());
            }
            let mut out = String::new();
            for tab in &tabs {
                let marker = if tab.attached { "*" } else { " " };
                out.push_str(&format!(
                    "{} {}  {}\n",
                    marker,
                    tab.target_id,
                    if tab.title.is_empty() {
                        truncate(&tab.url, 60)
                    } else {
                        format!("{} — {}", truncate(&tab.title, 40), truncate(&tab.url, 40))
                    }
                ));
            }
            out.push_str("(* = current tab; switch-tab <target_id> to change)");
            Ok(out)
        }

        StateOp::OpenTab { url } => {
            let res = cdp
                .send(
                    "Target.createTarget",
                    Some(json!({ "url": url })),
                )
                .await?;
            let target_id = res
                .get("targetId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| BladeError::Other("no targetId in response".into()))?;
            Ok(format!("✓ opened tab {target_id}: {}", truncate(url, 60)))
        }

        StateOp::CloseTab { target_id } => {
            cdp.send(
                "Target.closeTarget",
                Some(json!({ "targetId": target_id })),
            )
            .await?;
            Ok(format!("✓ closed tab {target_id}"))
        }

        // ---- sessions (M10) ----

        StateOp::SaveSession { name } => {
                if name.is_empty() || name.contains('/') || name.contains("..") || name.contains('\\') {
                    return Err(BladeError::Other(format!("invalid session name: {name:?}")));
                }
            
            cdp.enable("Network").await?;
            let res = cdp.send("Network.getCookies", Some(json!({}))).await?;
            let cookies = res.get("cookies").cloned().unwrap_or(json!([]));
            let origin_res = cdp.send("Runtime.evaluate", Some(json!({
                "expression": "location.origin",
                "returnByValue": true,
            }))).await?;
            let origin = origin_res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("");
            // Full-fidelity dump: NOT via get_storage (that truncates values
            // to 60 chars for display — saving from it corrupts tokens) and
            // not via text lines (values may contain '=' or newlines).
            let ls_entries: Vec<StorageEntry> = cdp.send("Runtime.evaluate", Some(json!({
                "expression": "Object.keys(localStorage).map(k=>({key:k,value:localStorage.getItem(k)}))",
                "returnByValue": true,
            }))).await.ok()
                .and_then(|r| extract_eval_value(&r).ok())
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let session = json!({ "cookies": cookies, "localStorage": ls_entries, "origin": origin });
            let dir = crate::platform::blade_dir().join("sessions");
            crate::platform::secure_create_dir_all(&dir).map_err(|e| BladeError::Other(format!("cannot create sessions dir: {e}")))?;
            let path = dir.join(format!("{name}.json"));
            crate::platform::secure_write_file(&path, serde_json::to_string_pretty(&session)?.as_bytes())
                .map_err(|e| BladeError::Other(format!("cannot write session: {e}")))?;
            let cookie_count = cookies.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(format!("✓ saved session '{}': {} cookies, {} localStorage entries\n  → {}", name, cookie_count, ls_entries.len(), path.display()))
        }

        StateOp::LoadSession { name } => {
                if name.is_empty() || name.contains('/') || name.contains("..") || name.contains('\\') {
                    return Err(BladeError::Other(format!("invalid session name: {name:?}")));
                }
            
            cdp.enable("Network").await?;
            let path = crate::platform::blade_dir().join("sessions").join(format!("{name}.json"));
            let content = std::fs::read_to_string(&path)
                .map_err(|e| BladeError::Other(format!("cannot read session '{name}': {e}")))?;
            let session: Value = serde_json::from_str(&content)?;
            let cookies = session.get("cookies").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            let mut cookie_count = 0usize;
            for cookie in &cookies {
                let mut params = json!({
                    "name": cookie.get("name").unwrap_or(&json!("")),
                    "value": cookie.get("value").unwrap_or(&json!("")),
                });
                if let Some(d) = cookie.get("domain") { params["domain"] = d.clone(); }
                if let Some(p) = cookie.get("path") { params["path"] = p.clone(); }
                if let Some(s) = cookie.get("secure") { params["secure"] = s.clone(); }
                if let Some(h) = cookie.get("httpOnly") { params["httpOnly"] = h.clone(); }
                if let Some(e) = cookie.get("expires") { params["expires"] = e.clone(); }
                if cdp.send("Network.setCookie", Some(params)).await.is_ok() {
                    cookie_count += 1;
                }
            }
            let ls_entries = session.get("localStorage").and_then(|l| l.as_array()).cloned().unwrap_or_default();
            for entry in &ls_entries {
                let key = entry.get("key").and_then(|k| k.as_str()).unwrap_or("");
                let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if !key.is_empty() {
                    let _ = set_storage(cdp, "localStorage", key, value).await;
                }
            }
            Ok(format!("✓ loaded session '{}': {} cookies, {} localStorage entries — navigate to apply", name, cookie_count, ls_entries.len()))
        }
    }
}

async fn get_storage(cdp: &CdpSession, storage: &str) -> Result<String> {
    let expr = format!(
        r#"Object.keys({storage}).map(k=>({{key:k,value:{storage}.getItem(k)}}))"#
    );
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;
    let value = extract_eval_value(&res)?;
    let entries: Vec<StorageEntry> = serde_json::from_value(value)?;
    if entries.is_empty() {
        return Ok(format!("({storage} empty)"));
    }
    let mut out = String::new();
    for e in &entries {
        out.push_str(&format!("{}={}\n", e.key, truncate(&e.value, 60)));
    }
    Ok(out)
}

async fn set_storage(cdp: &CdpSession, storage: &str, key: &str, value: &str) -> Result<String> {
    let key_js = serde_json::to_string(key)?;
    let val_js = serde_json::to_string(value)?;
    cdp.send(
        "Runtime.evaluate",
        Some(json!({
            "expression": format!("{storage}.setItem({key_js},{val_js})"),
            "returnByValue": true,
        })),
    )
    .await?;
    Ok(format!("✓ {storage} set: {key}={}", truncate(value, 40)))
}

async fn remove_storage(cdp: &CdpSession, storage: &str, key: &str) -> Result<String> {
    let key_js = serde_json::to_string(key)?;
    cdp.send(
        "Runtime.evaluate",
        Some(json!({
            "expression": format!("{storage}.removeItem({key_js})"),
            "returnByValue": true,
        })),
    )
    .await?;
    Ok(format!("✓ {storage} removed: {key}"))
}

async fn clear_storage(cdp: &CdpSession, storage: &str) -> Result<String> {
    cdp.send(
        "Runtime.evaluate",
        Some(json!({
            "expression": format!("{storage}.clear()"),
            "returnByValue": true,
        })),
    )
    .await?;
    Ok(format!("✓ {storage} cleared"))
}

fn extract_eval_value(res: &Value) -> Result<Value> {
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown page exception");
        return Err(BladeError::Other(format!("storage eval failed: {msg}")));
    }
    res.get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .ok_or_else(|| BladeError::Other("evaluate returned no value".into()))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}
