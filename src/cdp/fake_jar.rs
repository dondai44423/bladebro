//! Test-only CDP double with the cookie-jar semantics the state and login
//! code depends on. Each rule mirrors behavior observed live on Chromium 156:
//!
//! - `Network.setCookie(s)` reports success for an already-expired cookie
//!   but stores nothing (it only evicts an existing cookie of that key);
//! - `SameSite=None` without `Secure` is refused, silently in a batch;
//! - a cookie Chrome cannot build at all (a `__Secure-`/`__Host-` name
//!   without `Secure`) fails the whole `Network.setCookies` call with
//!   -32602 "Invalid cookie fields", and `Network.setCookie` likewise;
//! - `Network.deleteCookies` matches the cookie domain exactly, against
//!   `domain` or the `url` host (`fixture.test` does not delete a
//!   `.fixture.test` cookie);
//! - `Network.getCookies` returns the cookies sent to its `urls`, or to the
//!   current page when none are given; `Storage.getCookies` returns the jar;
//! - `Runtime.evaluate` answers `location.origin` and storage writes.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::cdp::{CdpClient, CdpSession};

pub(crate) struct FakeJar {
    pub(crate) cookies: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeJar {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeJar {
    pub(crate) fn names(&self) -> Vec<String> {
        let jar = self.cookies.lock().unwrap();
        let mut names: Vec<String> = jar
            .iter()
            .map(|c| c["name"].as_str().unwrap_or_default().to_string())
            .collect();
        names.sort();
        names
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Whether a cookie with `domain` is sent to `host`.
fn sent_to(domain: &str, host: &str) -> bool {
    domain == host || (domain.starts_with('.') && (host == &domain[1..] || host.ends_with(domain)))
}

/// A cookie Chrome rejects while parsing the CookieParam.
fn unbuildable(p: &Value) -> bool {
    let name = p["name"].as_str().unwrap_or_default();
    (name.starts_with("__Secure-") || name.starts_with("__Host-"))
        && !p["secure"].as_bool().unwrap_or(false)
}

const INVALID_FIELDS: (i64, &str) = (-32602, "Invalid cookie fields");

/// Store one CookieParam the way Chrome does; false = refused.
fn set_one(jar: &mut Vec<Value>, p: &Value) -> bool {
    let secure = p["secure"].as_bool().unwrap_or(false);
    if p["sameSite"].as_str() == Some("None") && !secure {
        return false;
    }
    let domain = p["domain"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| host_of(p["url"].as_str().unwrap_or_default()));
    let path = p["path"].as_str().unwrap_or("/").to_string();
    let name = p["name"].as_str().unwrap_or_default().to_string();
    jar.retain(|c| !(c["name"] == name && c["domain"] == domain && c["path"] == path));
    let expires = p["expires"].as_f64();
    if expires.is_some_and(|e| e <= now()) {
        return true;
    }
    jar.push(json!({
        "name": name,
        "value": p["value"],
        "domain": domain,
        "path": path,
        "secure": secure,
        "httpOnly": p["httpOnly"].as_bool().unwrap_or(false),
        "sameSite": p["sameSite"],
        "expires": expires.unwrap_or(-1.0),
    }));
    true
}

fn answer(
    jar: &Mutex<Vec<Value>>,
    origin: &str,
    method: &str,
    params: &Value,
) -> Result<Value, (i64, &'static str)> {
    let mut jar = jar.lock().unwrap();
    Ok(match method {
        "Network.setCookie" if unbuildable(params) => return Err(INVALID_FIELDS),
        "Network.setCookie" => json!({ "success": set_one(&mut jar, params) }),
        "Network.setCookies" => {
            let cookies = params["cookies"].as_array().cloned().unwrap_or_default();
            if cookies.iter().any(unbuildable) {
                return Err(INVALID_FIELDS);
            }
            for cookie in &cookies {
                set_one(&mut jar, cookie);
            }
            json!({})
        }
        "Storage.getCookies" => json!({ "cookies": jar.clone() }),
        "Network.getCookies" => {
            let hosts: Vec<String> = match params["urls"].as_array() {
                Some(urls) => urls
                    .iter()
                    .map(|u| host_of(u.as_str().unwrap_or_default()))
                    .collect(),
                None => vec![host_of(origin)],
            };
            let sent: Vec<Value> = jar
                .iter()
                .filter(|c| {
                    let d = c["domain"].as_str().unwrap_or_default();
                    hosts.iter().any(|h| sent_to(d, h))
                })
                .cloned()
                .collect();
            json!({ "cookies": sent })
        }
        "Network.deleteCookies" => {
            let name = params["name"].clone();
            let domain = params["domain"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| host_of(params["url"].as_str().unwrap_or_default()));
            // Partitioned cookies are only deleted with their partitionKey.
            jar.retain(|c| {
                !(c["name"] == name
                    && c["domain"] == domain.as_str()
                    && c.get("partitionKey").is_none_or(Value::is_null))
            });
            json!({})
        }
        "Runtime.evaluate" => {
            let expression = params["expression"].as_str().unwrap_or_default();
            let value = if expression == "location.origin" {
                json!(origin)
            } else if expression.starts_with("Object.keys(") {
                json!([])
            } else {
                json!(true)
            };
            json!({ "result": { "value": value } })
        }
        _ => json!({}),
    })
}

/// Start the double on a loopback WebSocket and return a root session to it.
pub(crate) async fn start(origin: &str, cookies: Vec<Value>) -> (CdpSession, FakeJar) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let jar = Arc::new(Mutex::new(cookies));
    let served = jar.clone();
    let origin = origin.to_string();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let request: Value = serde_json::from_str(&text).unwrap();
            let method = request["method"].as_str().unwrap_or_default();
            let reply = match answer(&served, &origin, method, &request["params"]) {
                Ok(result) => json!({ "id": request["id"], "result": result }),
                Err((code, message)) => {
                    json!({ "id": request["id"], "error": { "code": code, "message": message } })
                }
            };
            if ws
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let client = CdpClient::connect(&format!("ws://{address}"))
        .await
        .unwrap();
    (
        CdpSession::root(client),
        FakeJar {
            cookies: jar,
            server,
        },
    )
}
