//! The `state` tool (decision D5) — cookies, storage, and tabs.
//!
//! The agent rarely touches this, but when it does, it needs full control:
//! read/write cookies (auth flows), inspect localStorage/sessionStorage (SPA
//! state), and manage tabs (multi-page workflows). One tool, three domains.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};

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
    DeleteCookies {
        name: String,
        domain: Option<String>,
        url: Option<String>,
    },
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

            let res = cdp.send("Network.setCookie", Some(params)).await?;
            if res.get("success").and_then(Value::as_bool) == Some(false) {
                return Err(BladeError::Other(format!("Chrome rejected cookie {name:?}; check its URL, domain and security attributes")));
            }
            let stored = cdp.send("Storage.getCookies", None).await?;
            let expected_domain = if let Some(u) = url {
                url::Url::parse(u)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
            } else {
                domain.clone()
            };
            let verified = stored
                .get("cookies")
                .and_then(Value::as_array)
                .is_some_and(|cookies| {
                    cookies.iter().any(|c| {
                        c["name"].as_str() == Some(name)
                            && c["value"].as_str() == Some(value)
                            && expected_domain.as_deref().is_some_and(|d| {
                                c["domain"].as_str().is_some_and(|actual| {
                                    actual.trim_start_matches('.') == d.trim_start_matches('.')
                                })
                            })
                            && path
                                .as_deref()
                                .is_none_or(|p| c["path"].as_str() == Some(p))
                            && http_only.is_none_or(|h| c["httpOnly"].as_bool() == Some(h))
                            && secure.is_none_or(|s| c["secure"].as_bool() == Some(s))
                    })
                });
            if !verified {
                return Err(BladeError::Other(format!(
                    "cookie {name:?} did not match readback; check its scope and attributes"
                )));
            }
            Ok(format!("✓ cookie set: {name}={}", truncate(value, 40)))
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
        StateOp::RemoveLocalStorage { key } => remove_storage(cdp, "localStorage", key).await,
        StateOp::RemoveSessionStorage { key } => remove_storage(cdp, "sessionStorage", key).await,
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
                .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
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
            // Bare hosts must work here like everywhere else — a scheme-less
            // URL ("localhost:3000") leaves a stuck tab (Chrome reads
            // "localhost:" as the scheme).
            let url = crate::page::with_scheme(url);
            let res = cdp
                .send("Target.createTarget", Some(json!({ "url": url.as_str() })))
                .await?;
            let target_id = res
                .get("targetId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| BladeError::Other("no targetId in response".into()))?;
            Ok(format!("✓ opened tab {target_id}: {}", truncate(&url, 60)))
        }

        StateOp::CloseTab { target_id } => {
            let result = cdp
                .send("Target.closeTarget", Some(json!({ "targetId": target_id })))
                .await?;
            if result.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(BladeError::Other(format!(
                    "could not close tab {target_id}"
                )));
            }
            Ok(format!("✓ closed tab {target_id}"))
        }

        // ---- sessions (M10) ----
        StateOp::SaveSession { name } => {
            validate_session_name(name)?;

            cdp.enable("Network").await?;
            let origin_res = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": "location.origin",
                        "returnByValue": true,
                    })),
                )
                .await?;
            let origin = origin_res
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Error pages (chrome-error://) and untouched about:blank report
            // a literal "null" origin.
            if origin.is_empty() || origin == "null" {
                return Err(BladeError::Other(
                    "cannot save a session without an open page: navigate to a site first so cookies and storage have an origin".into(),
                ));
            }
            let res = cdp.send("Network.getCookies", Some(json!({}))).await?;
            let cookies = res
                .get("cookies")
                .filter(|v| v.is_array())
                .cloned()
                .ok_or_else(|| BladeError::Other("cookie snapshot missing cookie array".into()))?;
            // Full-fidelity dump: NOT via get_storage (that truncates values
            // to 60 chars for display — saving from it corrupts tokens) and
            // not via text lines (values may contain '=' or newlines).
            let ls_result = cdp.send("Runtime.evaluate", Some(json!({
                "expression": "Object.keys(localStorage).map(k=>({key:k,value:localStorage.getItem(k)}))",
                "returnByValue": true,
            }))).await?;
            let ls_entries: Vec<StorageEntry> =
                serde_json::from_value(extract_eval_value(&ls_result)?)?;
            let session =
                json!({ "cookies": cookies, "localStorage": ls_entries, "origin": origin });
            let dir = crate::platform::blade_dir().join("sessions");
            crate::platform::secure_create_dir_all(&dir)
                .map_err(|e| BladeError::Other(format!("cannot create sessions dir: {e}")))?;
            let path = dir.join(format!("{name}.json"));
            let bytes = serde_json::to_vec_pretty(&session)?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(BladeError::Other(
                    "session exceeds 8 MiB; existing snapshot preserved".into(),
                ));
            }
            crate::platform::secure_write_file(&path, &bytes)
                .map_err(|e| BladeError::Other(format!("cannot write session: {e}")))?;
            let cookie_count = cookies.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(format!(
                "✓ saved session '{}': {} cookies, {} localStorage entries\n  → {}",
                name,
                cookie_count,
                ls_entries.len(),
                path.display()
            ))
        }

        StateOp::LoadSession { name } => {
            validate_session_name(name)?;
            let path = crate::platform::blade_dir()
                .join("sessions")
                .join(format!("{name}.json"));
            use std::io::Read;
            let file = std::fs::File::open(&path)
                .map_err(|e| BladeError::Other(format!("cannot read session '{name}': {e}")))?;
            let mut content = Vec::new();
            file.take(8 * 1024 * 1024 + 1).read_to_end(&mut content)?;
            if content.len() > 8 * 1024 * 1024 {
                return Err(BladeError::Other("saved session exceeds 8 MiB".into()));
            }
            #[derive(Deserialize)]
            struct SavedSession {
                origin: String,
                cookies: Vec<Value>,
                #[serde(rename = "localStorage")]
                storage: Vec<StorageEntry>,
            }
            let session: SavedSession = serde_json::from_slice(&content)?;
            // Validate every cookie and the origin before changing browser state.
            let cookies = session
                .cookies
                .iter()
                .map(crate::logins::cookie_params)
                .collect::<Result<Vec<_>>>()?;
            let origin_res = cdp
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": "location.origin", "returnByValue": true,
                    })),
                )
                .await?;
            let origin = extract_eval_value(&origin_res)?;
            if session.origin == "null" || origin.as_str() != Some(session.origin.as_str()) {
                return Err(BladeError::Other(format!(
                    "session '{name}' belongs to {}; navigate there before loading it",
                    session.origin
                )));
            }
            cdp.enable("Network").await?;
            for cookie in &cookies {
                let result = cdp.send("Network.setCookie", Some(cookie.clone())).await?;
                if result.get("success").and_then(Value::as_bool) == Some(false) {
                    return Err(BladeError::Other(format!(
                        "Chrome rejected saved cookie {}; session only partially restored",
                        cookie["name"]
                    )));
                }
            }
            let restored = cdp.send("Storage.getCookies", None).await?;
            let all = restored
                .get("cookies")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    BladeError::Other("cookie readback missing after session restore".into())
                })?;
            for saved in &session.cookies {
                if !all.iter().any(|c| {
                    c["name"] == saved["name"]
                        && c["value"] == saved["value"]
                        && c["domain"] == saved["domain"]
                        && c["path"] == saved["path"]
                }) {
                    return Err(BladeError::Other(format!(
                        "saved cookie {} did not match readback; session only partially restored",
                        saved["name"]
                    )));
                }
            }
            for entry in &session.storage {
                set_storage(cdp, "localStorage", &entry.key, &entry.value).await?;
            }
            Ok(format!(
                "✓ loaded session '{}': {} cookies, {} localStorage entries — reload to apply",
                name,
                cookies.len(),
                session.storage.len()
            ))
        }
    }
}

async fn get_storage(cdp: &CdpSession, storage: &str) -> Result<String> {
    let expr = format!(r#"Object.keys({storage}).map(k=>({{key:k,value:{storage}.getItem(k)}}))"#);
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
    let result = cdp.send(
        "Runtime.evaluate",
        Some(json!({
            "expression": format!("(()=>{{{storage}.setItem({key_js},{val_js});return {storage}.getItem({key_js})==={val_js};}})()"),
            "returnByValue": true,
        })),
    )
    .await?;
    if extract_eval_value(&result)? != json!(true) {
        return Err(BladeError::Other(format!(
            "{storage} mutation did not match readback"
        )));
    }
    Ok(format!("✓ {storage} set: {key}={}", truncate(value, 40)))
}

async fn remove_storage(cdp: &CdpSession, storage: &str, key: &str) -> Result<String> {
    let key_js = serde_json::to_string(key)?;
    let result = cdp.send(
        "Runtime.evaluate",
        Some(json!({
            "expression": format!("(()=>{{{storage}.removeItem({key_js});return {storage}.getItem({key_js})===null;}})()"),
            "returnByValue": true,
        })),
    )
    .await?;
    if extract_eval_value(&result)? != json!(true) {
        return Err(BladeError::Other(format!(
            "{storage} mutation did not match readback"
        )));
    }
    Ok(format!("✓ {storage} removed: {key}"))
}

async fn clear_storage(cdp: &CdpSession, storage: &str) -> Result<String> {
    let result = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": format!("(()=>{{{storage}.clear();return {storage}.length===0;}})()"),
                "returnByValue": true,
            })),
        )
        .await?;
    if extract_eval_value(&result)? != json!(true) {
        return Err(BladeError::Other(format!(
            "{storage} mutation did not match readback"
        )));
    }
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

fn validate_session_name(name: &str) -> Result<()> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || device.is_some_and(|s| {
            matches!(
                s,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if name.is_empty()
        || name.len() > 128
        || name.contains("..")
        || name.ends_with(['.', ' '])
        || reserved
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return Err(BladeError::Other(format!("invalid session name: {name:?}")));
    }
    Ok(())
}

#[cfg(test)]
mod reliability_tests {
    use super::*;
    #[test]
    fn session_names_are_portable_and_cannot_escape_storage() {
        for name in [
            "",
            "../escape",
            "a/b",
            "a\\b",
            "a:b",
            "CON",
            "con.txt",
            "LPT1",
            "COM9",
            "a.",
            "a ",
            "bad\0",
            "x?",
            "<x>",
        ] {
            assert!(validate_session_name(name).is_err(), "accepted {name:?}");
        }
        assert!(validate_session_name(&"a".repeat(129)).is_err());
        for name in ["account", "π 🦀", "work.account", "COM10"] {
            validate_session_name(name).unwrap();
        }
    }
    #[test]
    fn page_exceptions_never_count_as_storage_success() {
        let value = json!({"result":{"value":true},"exceptionDetails":{"text":"SecurityError"}});
        assert!(extract_eval_value(&value)
            .unwrap_err()
            .to_string()
            .contains("SecurityError"));
        assert!(extract_eval_value(&json!({"result":{}})).is_err());
    }
}
