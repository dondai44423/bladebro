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
                    c.value,
                    c.domain,
                    if c.secure { " secure" } else { "" },
                    if c.http_only { " httpOnly" } else { "" },
                ));
            }
            bounded_state_read(out, &res["cookies"])
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
            let stored = cdp.cookie_store().await?;
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
            let mut host = String::new();
            if let Some(u) = url {
                params["url"] = json!(u);
                host = url::Url::parse(u)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
            } else if let Some(d) = domain {
                params["domain"] = json!(d);
                host = d.trim_start_matches('.').to_string();
            }
            // Chrome deletes only cookies whose domain equals `domain` (or
            // the url host) exactly, so `--domain example.com` leaves a
            // `.example.com` cookie in place. Read the store back on both
            // sides of the call rather than claiming a deletion.
            let before = cookie_domains_sent_to(cdp, name, &host).await?;
            cdp.send("Network.deleteCookies", Some(params)).await?;
            let after = cookie_domains_sent_to(cdp, name, &host).await?;
            if let Some(first) = after.first() {
                return Err(BladeError::Other(format!(
                    "cookie {name:?} is still set for {} — Chrome deletes only an exact domain match; retry del-cookie with domain {first}",
                    after.join(", ")
                )));
            }
            if before.is_empty() {
                return Ok(format!("no cookie {name:?} for {host}; nothing deleted"));
            }
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
            // SECURITY: the real-browser lane skips the login snapshot
            // ("not our cookie store") — the same boundary must hold here.
            // Without this gate, one agent-callable state save on the rb
            // lane writes the user's REAL browser cookie jar in cleartext
            // under the blade data dir, where the upload read lane could
            // forward it to any driven page.
            if crate::realbrowser::real_lane() {
                return Err(BladeError::Other(
                    "state save is disabled on the real-browser lane — it would persist your real browser's cookies into bladebro's data dir".into(),
                ));
            }
            validate_session_name(name)?;

            let captured = capture_session(cdp).await?;
            let dir = crate::platform::blade_dir().join("sessions");
            crate::platform::secure_create_dir_all(&dir)
                .map_err(|e| BladeError::Other(format!("cannot create sessions dir: {e}")))?;
            let path = dir.join(format!("{name}.json"));
            let bytes = serde_json::to_vec_pretty(&captured.session)?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(BladeError::Other(
                    "session exceeds 8 MiB; existing snapshot preserved".into(),
                ));
            }
            crate::platform::secure_write_file(&path, &bytes)
                .map_err(|e| BladeError::Other(format!("cannot write session: {e}")))?;
            let skipped = if captured.partitioned > 0 {
                format!(" ({} partitioned, skipped)", captured.partitioned)
            } else {
                String::new()
            };
            Ok(format!(
                "✓ saved session '{}': {} cookies{skipped}, {} localStorage entries\n  → {}",
                name,
                captured.cookies,
                captured.storage,
                path.display()
            ))
        }

        StateOp::LoadSession { name } => {
            validate_session_name(name)?;
            let path = crate::platform::blade_dir()
                .join("sessions")
                .join(format!("{name}.json"));
            if std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                // A symlinked session file is attack state — never read
                // through it into the restore path.
                return Err(BladeError::Other(format!(
                    "session '{name}' is a symlink — refusing to read it"
                )));
            }
            use std::io::Read;
            let file = crate::platform::open_nofollow(&path)
                .map_err(|e| BladeError::Other(format!("cannot read session '{name}': {e}")))?;
            let mut content = Vec::new();
            file.take(8 * 1024 * 1024 + 1).read_to_end(&mut content)?;
            if content.len() > 8 * 1024 * 1024 {
                return Err(BladeError::Other("saved session exceeds 8 MiB".into()));
            }
            let session: SavedSession = serde_json::from_slice(&content)?;
            restore_session(cdp, name, &session).await
        }
    }
}

/// Domains of the unpartitioned cookies named `name` that Chrome would
/// send to `host` (deleteCookies never touches partitioned ones).
async fn cookie_domains_sent_to(cdp: &CdpSession, name: &str, host: &str) -> Result<Vec<String>> {
    let store = cdp.cookie_store().await?;
    let all = store
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Other("cookie readback missing cookie store".into()))?;
    let mut domains: Vec<String> = all
        .iter()
        .filter(|c| c["name"].as_str() == Some(name) && !cookie_partitioned(c))
        .filter_map(|c| c["domain"].as_str())
        .filter(|d| {
            !host.is_empty()
                && (d.trim_start_matches('.') == host || (d.starts_with('.') && host.ends_with(d)))
        })
        .map(str::to_owned)
        .collect();
    domains.sort();
    domains.dedup();
    Ok(domains)
}

/// What `state save` writes, plus the counts its verdict reports.
struct CapturedSession {
    session: Value,
    cookies: usize,
    partitioned: usize,
    storage: usize,
}

/// Read the current page's cookies + localStorage into a session document.
async fn capture_session(cdp: &CdpSession) -> Result<CapturedSession> {
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
    let all = res
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Other("cookie snapshot missing cookie array".into()))?;
    // getCookies includes partitioned (CHIPS) cookies, which `state load`
    // cannot set as ordinary cookies: saving one made the whole session
    // unloadable. Leave them out, as the login snapshot does, and count them.
    let cookies: Vec<&Value> = all.iter().filter(|c| !cookie_partitioned(c)).collect();
    let partitioned = all.len() - cookies.len();
    // Save structured entries, never the display representation:
    // values may contain '=' or newlines.
    let ls_result = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": "Object.keys(localStorage).map(k=>({key:k,value:localStorage.getItem(k)}))",
                "returnByValue": true,
            })),
        )
        .await?;
    let ls_entries: Vec<StorageEntry> = serde_json::from_value(extract_eval_value(&ls_result)?)?;
    Ok(CapturedSession {
        storage: ls_entries.len(),
        cookies: cookies.len(),
        session: json!({ "cookies": cookies, "localStorage": ls_entries, "origin": origin }),
        partitioned,
    })
}

/// A named session file (`state save`), as read back by `state load`.
#[derive(Deserialize)]
struct SavedSession {
    origin: String,
    cookies: Vec<Value>,
    #[serde(rename = "localStorage")]
    storage: Vec<StorageEntry>,
}

/// Apply a saved session to the current page: cookies, readback, then
/// localStorage. Split from the file read so the restore contract can be
/// exercised against a CDP double.
async fn restore_session(cdp: &CdpSession, name: &str, session: &SavedSession) -> Result<String> {
    // A saved cookie that has since expired (`_gat` lives one minute) is
    // accepted by setCookie but never stored, so requiring it at readback
    // failed every later load after some cookies were already set. Skip it
    // in both places, as the login sidecar restore does, and say so.
    // Partitioned cookies in sessions saved before `state save` dropped
    // them cannot be set as ordinary cookies; they are skipped the same way.
    let now = unix_now()?;
    let partitioned = session
        .cookies
        .iter()
        .filter(|c| cookie_partitioned(c))
        .count();
    let unpartitioned = || session.cookies.iter().filter(|c| !cookie_partitioned(c));
    let live: Vec<&Value> = unpartitioned()
        .filter(|c| !cookie_expired(c, now))
        .collect();
    let expired = unpartitioned().count() - live.len();
    // Validate every cookie and the origin before changing browser state.
    let cookies = live
        .iter()
        .map(|c| crate::logins::cookie_params(c))
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
    let restored = cdp.cookie_store().await?;
    let all = restored
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Other("cookie readback missing after session restore".into()))?;
    // Re-read the clock: a cookie may expire between set and readback.
    let now = unix_now()?;
    for saved in live.iter().filter(|c| !cookie_expired(c, now)) {
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
    let skipped: Vec<String> = [(expired, "expired"), (partitioned, "partitioned")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, kind)| format!("{n} {kind}"))
        .collect();
    let skipped = if skipped.is_empty() {
        String::new()
    } else {
        format!(" ({}, skipped)", skipped.join(", "))
    };
    Ok(format!(
        "✓ loaded session '{}': {} cookies{skipped}, {} localStorage entries — reload to apply",
        name,
        cookies.len(),
        session.storage.len()
    ))
}

/// A partitioned (CHIPS) cookie: its jar is keyed by a top-level site.
fn cookie_partitioned(cookie: &Value) -> bool {
    cookie.get("partitionKey").is_some_and(|k| !k.is_null())
}

/// A persistent cookie whose expiry has passed. Session cookies report
/// `expires: -1` and never count as expired.
fn cookie_expired(cookie: &Value, now: f64) -> bool {
    cookie["expires"]
        .as_f64()
        .is_some_and(|e| e >= 0.0 && e <= now)
}

fn unix_now() -> Result<f64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(BladeError::other)?
        .as_secs_f64())
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
        out.push_str(&format!("{}={}\n", e.key, e.value));
    }
    bounded_state_read(out, &entries)
}

// Values are evidence, not previews. Bound the context cost by offloading the
// complete structured read rather than silently clipping individual tokens.
fn bounded_state_read(text: String, data: &impl Serialize) -> Result<String> {
    if text.len() <= 12000 {
        return Ok(text);
    }
    let payload = serde_json::to_string(data)?;
    let path = crate::artifacts::write_artifact(&payload, "json")?;
    Ok(format!("state read offloaded ({} bytes); full payload: {path} — read with see artifact=\"{path}\" format=json items=true", payload.len()))
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

#[cfg(test)]
mod cookie_jar_tests {
    use super::*;
    use crate::cdp::fake_jar;

    const ORIGIN: &str = "http://127.0.0.1:8080";

    fn cookie(name: &str, domain: &str, expires: f64) -> Value {
        json!({"name": name, "value": "v", "domain": domain, "path": "/",
               "secure": false, "httpOnly": false, "sameSite": "Lax", "expires": expires})
    }

    fn session(cookies: Vec<Value>) -> SavedSession {
        SavedSession {
            origin: ORIGIN.into(),
            cookies,
            storage: vec![StorageEntry {
                key: "k".into(),
                value: "v".into(),
            }],
        }
    }

    /// A session saved with a short-lived cookie (`_gat`, max-age 60) must
    /// still load once that cookie has expired: Chrome accepts the expired
    /// cookie and stores nothing, so requiring it at readback failed every
    /// later load halfway through.
    #[tokio::test]
    async fn expired_saved_cookie_does_not_abort_session_load() {
        let (cdp, jar) = fake_jar::start(ORIGIN, vec![]).await;
        let saved = session(vec![
            cookie("_gat", "127.0.0.1", 1_000_000_000.0),
            cookie("sid", "127.0.0.1", 4_000_000_000.0),
        ]);
        let out = restore_session(&cdp, "s1", &saved)
            .await
            .expect("an expired cookie must not fail the load");
        assert!(out.contains("1 cookies"), "{out}");
        assert!(out.contains("1 expired"), "{out}");
        assert_eq!(jar.names(), ["sid"]);
    }

    fn partitioned(name: &str) -> Value {
        let mut c = cookie(name, "127.0.0.1", -1.0);
        c["secure"] = json!(true);
        c["partitionKey"] =
            json!({"topLevelSite": "http://127.0.0.1", "hasCrossSiteAncestor": false});
        c
    }

    /// `Network.getCookies` returns partitioned (CHIPS) cookies, which a
    /// restore cannot set as ordinary cookies: a session that captured one
    /// saved fine and then could never load. Save leaves them out and says
    /// so; a session file written before that still loads without them.
    #[tokio::test]
    async fn partitioned_cookies_neither_poison_a_save_nor_fail_a_load() {
        let (cdp, _jar) = fake_jar::start(
            ORIGIN,
            vec![partitioned("chip"), cookie("plain", "127.0.0.1", -1.0)],
        )
        .await;
        let captured = capture_session(&cdp).await.unwrap();
        let names: Vec<&str> = captured.session["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(names, ["plain"], "partitioned cookie must not be saved");
        assert_eq!(captured.cookies, 1);
        assert_eq!(captured.partitioned, 1);

        let (cdp, jar) = fake_jar::start(ORIGIN, vec![]).await;
        let legacy = session(vec![
            partitioned("chip"),
            cookie("plain", "127.0.0.1", -1.0),
        ]);
        let out = restore_session(&cdp, "c1", &legacy)
            .await
            .expect("a partitioned cookie must not fail the load");
        assert!(out.contains("1 partitioned"), "{out}");
        assert_eq!(jar.names(), ["plain"]);
    }

    fn delete(name: &str, domain: Option<&str>, url: Option<&str>) -> StateOp {
        StateOp::DeleteCookies {
            name: name.into(),
            domain: domain.map(str::to_owned),
            url: url.map(str::to_owned),
        }
    }

    /// `Network.deleteCookies` matches the cookie domain exactly, so
    /// `del-cookie sid --domain fixture.test` against a `.fixture.test`
    /// cookie deleted nothing and still reported "✓ cookie deleted".
    #[tokio::test]
    async fn del_cookie_reports_a_cookie_it_did_not_delete() {
        let (cdp, jar) = fake_jar::start(ORIGIN, vec![cookie("sid", ".fixture.test", -1.0)]).await;
        let err = perform(&cdp, &delete("sid", Some("fixture.test"), None))
            .await
            .expect_err("the cookie is still set; success would be a lie");
        let err = err.to_string();
        assert!(err.contains("still set for .fixture.test"), "{err}");
        assert!(
            err.contains("domain .fixture.test"),
            "names the retry: {err}"
        );
        assert_eq!(jar.names(), ["sid"]);

        // The same holds for a url scope: the url host is matched exactly too.
        let err = perform(&cdp, &delete("sid", None, Some("http://fixture.test/")))
            .await
            .expect_err("a url scope does not delete a domain cookie");
        assert!(err.to_string().contains(".fixture.test"), "{err}");

        let out = perform(&cdp, &delete("sid", Some(".fixture.test"), None))
            .await
            .unwrap();
        assert_eq!(out, "✓ cookie deleted: sid");
        assert!(jar.names().is_empty());

        let out = perform(&cdp, &delete("sid", Some(".fixture.test"), None))
            .await
            .unwrap();
        assert!(!out.starts_with('✓'), "nothing left to delete: {out}");
        assert!(out.contains("nothing deleted"), "{out}");
    }
}
