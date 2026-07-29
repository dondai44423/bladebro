//! The MCP server — stdio JSON-RPC 2.0 loop.
//!
//! The server holds one [`Page`](crate::page::Page) (CDP connection + LPM) for
//! the lifetime of the session. It reads newline-delimited JSON-RPC messages
//! from stdin, dispatches to the appropriate tool, and writes responses to
//! stdout. stderr is for logging only — nothing else goes to stdout.
//!
//! Protocol: dual-dialect. Legacy clients (≤2025-11-25) use the
//! `initialize` handshake; the 2026-07-28 stateless revision (SEP-2575)
//! carries the protocol version per-request in
//! `_meta["io.modelcontextprotocol/protocolVersion"]` and discovers
//! capabilities via `server/discover`. Both are supported; the dialect
//! is negotiated per request. Mismatched versions get
//! `UnsupportedProtocolVersionError` (-32022).

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::action::Action;
use crate::cdp::{self, CdpClient, CdpSession};
use crate::error::{BladeError, Result};
use crate::mcp::tools::tools_to_json;
use crate::page::Page;
use crate::state::StateOp;

/// Default protocol version for legacy clients that don't negotiate.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The 2026-07-28 stateless revision (SEP-2575). Requests carrying this
/// version in `_meta` get new-dialect results: `resultType` (SEP-2322),
/// server identity in `_meta`, and cache hints on list endpoints.
const STATELESS_VERSION: &str = "2026-07-28";

/// All protocol versions this server speaks. Legacy versions behave
/// identically for the methods we implement; the stateless revision
/// changes result shaping only.
const SUPPORTED_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    STATELESS_VERSION,
];

/// Server instructions shared by `initialize` and `server/discover`.
const INSTRUCTIONS: &str = "Stealth browser driver. `see` reads the page (diff-first), `act` interacts (click/type/navigate), `state` manages cookies/tabs/sessions, `run` executes batch JS, `vision` screenshots.";

/// Extract the per-request protocol version (SEP-2575). New-spec clients
/// send `_meta["io.modelcontextprotocol/protocolVersion"]` on every
/// request; legacy clients omit it and get the legacy dialect.
/// Err carries the unsupported version string.
fn request_version(params: &Value) -> std::result::Result<Option<&'static str>, String> {
    let v = params
        .get("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(|v| v.as_str());
    match v {
        None => Ok(None),
        Some(v) => match SUPPORTED_VERSIONS.iter().copied().find(|s| *s == v) {
            Some(s) => Ok(Some(s)),
            None => Err(v.to_string()),
        },
    }
}

/// Add 2026-07-28 dialect fields to a result payload: `resultType`
/// (required by SEP-2322) and server identity in `_meta` (SEP-2575).
fn shape_result(result: &mut Value, version: Option<&str>) {
    if version != Some(STATELESS_VERSION) {
        return;
    }
    if let Some(obj) = result.as_object_mut() {
        obj.entry("resultType").or_insert(json!("complete"));
        obj.entry("_meta").or_insert(json!({
            "io.modelcontextprotocol/serverInfo": {
                "name": "bladebro",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }));
    }
}

/// Run the MCP server over stdio (WS transport). Blocks until stdin closes.
pub async fn run(host: &str, port: u16, browser: Option<crate::browser::Browser>) -> Result<()> {
    let target = cdp::first_page_target(&format!("{host}:{port}")).await?;
    let client = CdpClient::connect(target.ws_url()?).await?;
    let page = Page::attach(CdpSession::root(client), &format!("{host}:{port}"), None).await?;
    eprintln!("[bladebro] MCP server ready on {host}:{port} (ws transport)");
    serve(page, browser, false).await
}

/// Serve MCP over a zero-port CDP pipe connection (S1). `_browser` is held
/// for the server's lifetime; dropping it kills Chrome + Xvfb.
/// Unix-only: Windows uses WS transport.
#[cfg(unix)]
pub async fn run_pipe(client: CdpClient, browser: crate::browser::Browser) -> Result<()> {
    let session = attach_pipe(&client).await?;
    let page = Page::attach(session, "pipe", Some(client)).await?;
    eprintln!("[bladebro] MCP server ready (pipe transport)");
    serve(page, Some(browser), true).await
}

/// Attach to the first page target over the browser-level pipe connection.
/// Creates a fresh tab when none exists (restored-session edge cases).
/// Unix-only: called by run_pipe which is also Unix-only.
#[cfg(unix)]
async fn attach_pipe(client: &CdpClient) -> Result<CdpSession> {
    let targets = client.send("Target.getTargets", None).await?;
    let empty = Vec::new();
    let infos = targets
        .get("targetInfos")
        .and_then(|t| t.as_array())
        .unwrap_or(&empty);
    let page_target = infos
        .iter()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .and_then(|t| t.get("targetId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let target_id = match page_target {
        Some(id) => id,
        None => {
            let res = client
                .send("Target.createTarget", Some(json!({ "url": "about:blank" })))
                .await?;
            res.get("targetId")
                .and_then(|v| v.as_str())
                .ok_or(BladeError::NoTarget)?
                .to_string()
        }
    };

    let res = client
        .send(
            "Target.attachToTarget",
            Some(json!({ "targetId": target_id, "flatten": true })),
        )
        .await?;
    let session_id = res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BladeError::Other("Target.attachToTarget returned no sessionId".into()))?;
    Ok(CdpSession::child(client.clone(), session_id))
}

/// Relaunch Chrome and create a fresh `Page` after the browser connection
/// dies. Drops the old `Browser` (kills Chrome + Xvfb), waits for cleanup,
/// then launches a new one. Returns the new `Page` and `Browser`.
///
/// This is the self-healing core: the agent never sees "browser connection
/// closed" because the server reconnects transparently before the tool
/// call reaches the agent.
async fn reconnect(
    use_pipe: bool,
) -> Result<(Page, Option<crate::browser::Browser>)> {
    eprintln!("[bladebro] browser connection lost, relaunching...");
    // Small delay to let the OS reclaim resources (port, display, etc.)
    // before we launch a new Chrome instance.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    if use_pipe {
        #[cfg(unix)]
        {
            let (browser, client) = crate::browser::Browser::launch_pipe().await?;
            let session = attach_pipe(&client).await?;
            let page = Page::attach(session, "pipe", Some(client)).await?;
            eprintln!("[bladebro] reconnected (pipe transport)");
            return Ok((page, Some(browser)));
        }
        #[cfg(not(unix))]
        {
            return Err(BladeError::Other(
                "pipe transport is Unix-only, cannot reconnect".into(),
            ));
        }
    }

    // WS transport: launch a new browser on a fresh port.
    let browser = crate::browser::Browser::launch(0).await?;
    let base = browser.base();
    let target = cdp::first_page_target(&base).await?;
    let client = CdpClient::connect(target.ws_url()?).await?;
    let page = Page::attach(CdpSession::root(client), &base, None).await?;
    eprintln!("[bladebro] reconnected (ws transport)");
    Ok((page, Some(browser)))
}

/// The stdio JSON-RPC loop, shared by both transports.
///
/// `browser` holds the Chrome child process (dropped on reconnect to kill
/// the old Chrome). `use_pipe` selects the reconnection strategy.
async fn serve(
    mut page: Page,
    mut browser: Option<crate::browser::Browser>,
    use_pipe: bool,
) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[bladebro] stdin read error: {e}");
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[bladebro] invalid JSON: {e}");
                continue;
            }
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // Dispatch. Per-request version negotiation first (SEP-2575):
        // unknown versions fail closed with UnsupportedProtocolVersionError.
        let version = match request_version(&params) {
            Ok(v) => v,
            Err(bad) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32022,
                        "message": format!("unsupported protocol version: {bad}"),
                        "data": { "supportedVersions": SUPPORTED_VERSIONS },
                    }
                });
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(out, "{resp_str}")?;
                out.flush()?;
                continue;
            }
        };

        let response = match method {
            "initialize" => Some(handle_initialize(id, &params)),
            "initialized" | "notifications/initialized" => None, // notification, no response
            "server/discover" => Some(handle_discover(id, version)),
            "tools/list" => Some(handle_tools_list(id, version)),
            "tools/call" => {
                // Pre-call self-heal: if the browser connection is already
                // dead (Chrome crashed between tool calls), reconnect now
                // so the tool call succeeds on the first try.
                if page.is_closed() {
                    // Drop old browser (kills Chrome) before relaunching.
                    drop(browser.take());
                    match reconnect(use_pipe).await {
                        Ok((new_page, new_browser)) => {
                            page = new_page;
                            browser = new_browser;
                        }
                        Err(e) => {
                            eprintln!("[bladebro] pre-call reconnect failed: {e}");
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{ "type": "text", "text": format!("\u{2717} browser connection lost. Bladebro tried to reconnect but failed: {e}. The server is still running, try again in a moment.") }],
                                    "isError": true,
                                }
                            });
                            let resp_str = serde_json::to_string(&resp)?;
                            writeln!(out, "{resp_str}")?;
                            out.flush()?;
                            continue;
                        }
                    }
                }

                // Clone id for retry paths — handle_tools_call consumes it.
                let id_retry = id.clone();

                // Panic isolation: a bug in any tool handler must NOT kill
                // the server. Convert panics into JSON-RPC errors; the page
                // model may be stale afterward but the next capture re-syncs.
                let res = futures_util::FutureExt::catch_unwind(
                    std::panic::AssertUnwindSafe(handle_tools_call(id, &params, &mut page)),
                )
                .await;

                // handle_tools_call returns Result<Value, BladeError>:
                // Ok(Value) = normal response (success or handled error).
                // Err(Closed) = browser died during the call, need to reconnect.
                let resp = match res {
                    Ok(Ok(v)) => v,
                    Ok(Err(BladeError::Closed)) => {
                        // Self-heal: reconnect and retry the tool call once.
                        eprintln!("[bladebro] browser closed during tool call, reconnecting...");
                        // Drop old browser (kills Chrome) before relaunching.
                        drop(browser.take());
                        match reconnect(use_pipe).await {
                            Ok((new_page, new_browser)) => {
                                page = new_page;
                                browser = new_browser;
                                // Retry the tool call with the fresh page.
                                // Clone id_retry for the retry call — handle_tools_call consumes it.
                                let id_retry2 = id_retry.clone();
                                match handle_tools_call(id_retry2, &params, &mut page).await {
                                    Ok(v) => v,
                                    Err(e) => {
                                        eprintln!("[bladebro] retry after reconnect failed: {e}");
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id_retry,
                                            "result": {
                                                "content": [{ "type": "text", "text": format!("\u{2717} browser connection lost. Bladebro reconnected but the retry failed: {e}. Try the tool call again.") }],
                                                "isError": true,
                                            }
                                        })
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("[bladebro] reconnect failed: {e}");
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id_retry,
                                    "result": {
                                        "content": [{ "type": "text", "text": format!("\u{2717} browser connection lost. Bladebro tried to reconnect but failed: {e}. The server is still running, try again in a moment.") }],
                                        "isError": true,
                                    }
                                })
                            }
                        }
                    }
                    // Should never happen (handle_tools_call only returns Err(Closed)),
                    // but handle gracefully to satisfy the type system.
                    Ok(Err(e)) => json!({
                        "jsonrpc": "2.0",
                        "id": id_retry,
                        "error": {
                            "code": -32603,
                            "message": e.to_string(),
                        }
                    }),
                    Err(_) => json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {
                            "code": -32603,
                            "message": "internal panic in tool handler (see stderr) — session survived, retry or re-see",
                        }
                    }),
                };
                let mut resp = resp;
                if let Some(result) = resp.get_mut("result") {
                    shape_result(result, version);
                }
                Some(resp)
            }
            // Removed in 2026-07-28; kept for legacy keepalive clients.
            "ping" => {
                let mut result = json!({});
                shape_result(&mut result, version);
                Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            }
            _ => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {method}"),
                }
            })),
        };

        if let Some(resp) = response {
            // MCP spec: messages MUST NOT contain embedded newlines.
            let resp_str = serde_json::to_string(&resp)?;
            writeln!(out, "{resp_str}")?;
            out.flush()?;
        }
    }

    Ok(())
}

fn handle_initialize(id: Option<Value>, params: &Value) -> Value {
    // Legacy handshake (removed in 2026-07-28 but old clients require
    // it). Negotiate: echo the client's version when we support it,
    // fall back to our default otherwise — the client decides whether
    // to continue with the offered version.
    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    let negotiated = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": negotiated,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": "bladebro",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        }
    })
}

/// `server/discover` — capability advertisement for the 2026-07-28
/// stateless revision (SEP-2575). Servers MUST implement this; new
/// clients may call it instead of the removed `initialize` handshake.
fn handle_discover(id: Option<Value>, version: Option<&str>) -> Value {
    let mut result = json!({
        "supportedVersions": SUPPORTED_VERSIONS,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "instructions": INSTRUCTIONS,
        "ttlMs": 3_600_000,
        "cacheScope": "public",
    });
    shape_result(&mut result, version);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn handle_tools_list(id: Option<Value>, version: Option<&str>) -> Value {
    let mut result = json!({
        "tools": tools_to_json(),
    });
    // CacheableResult (SEP-2549): the tool list is static per binary,
    // so cache aggressively. Tools are returned in a deterministic
    // order for client-side caching and prompt cache hits.
    if version == Some(STATELESS_VERSION) {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("ttlMs".into(), json!(3_600_000));
            obj.insert("cacheScope".into(), json!("public"));
        }
    }
    shape_result(&mut result, version);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

async fn handle_tools_call(
    id: Option<Value>,
    params: &Value,
    page: &mut Page,
) -> std::result::Result<Value, BladeError> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // Vision tool returns an image content block, not text.
    if name == "vision" {
        return handle_vision(id, &args, page).await;
    }

    let result = match name {
        "act" => handle_act(&args, page).await,
        "see" => handle_see(&args, page).await,
        "state" => handle_state(&args, page).await,
        "run" => handle_run(&args, page).await,
        _ => Err(crate::error::BladeError::Other(format!("unknown tool: {name}"))),
    };

    match result {
        Ok(mut text) => {
            // Drain any dialogs that were auto-dismissed during this call.
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\u{26a0} dialogs auto-dismissed:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    text.push_str(&format!("  {} \"{}\" \u{2014} {}\n", d.kind, d.message, action));
                    if let Some(p) = &d.default_prompt {
                        if !p.is_empty() {
                            text.push_str(&format!("    (prompt default: \"{}\")\n", p));
                        }
                    }
                }
            }
            // Drain ambient events (consent, block detection).
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\u{26a0} {}\n", a));
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }]
                }
            }))
        }
        // Propagate Closed so serve() can self-heal (relaunch Chrome
        // and retry the tool call). The agent never sees this error.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => {
            // Also drain dialogs on error.
            let mut text = format!("\u{2717} error: {e}");
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\n\u{26a0} dialogs auto-dismissed:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    text.push_str(&format!("  {} \"{}\" \u{2014} {}\n", d.kind, d.message, action));
                }
            }
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\n\u{26a0} {}\n", a));
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }],
                    "isError": true,
                }
            }))
        }
    }
}

/// M3: Resolve a text query to an element ref via find_by_text. Returns the
/// ref id for a unique match, or an ambiguity/not-found error.
async fn resolve_text_target(
    page: &mut Page,
    query: &str,
    role_filter: Option<&str>,
) -> Result<String> {
    let matches = crate::action::find_by_text(page.cdp_ref(), query, role_filter).await?;
    if matches.is_empty() {
        let view = page.view(2000);
        return Err(BladeError::Other(format!(
            "no element matching \"{}\" found\n\n--- current page ---\n{}",
            query, view
        )));
    }
    if matches.len() == 1 {
        let m = &matches[0];
        return Ok(page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame));
    }
    let mut lines = vec![format!("ambiguous: {} matches for \"{}\":", matches.len(), query)];
    for m in &matches {
        lines.push(format!("  {} \"{}\"", m.role, m.name));
    }
    lines.push("Add role= or use see to get a specific ref.".to_string());
    Err(BladeError::Other(lines.join("\n")))
}

async fn handle_act(args: &Value, page: &mut Page) -> Result<String> {
    let action_str = args.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let ref_id = args.get("ref").and_then(|r| r.as_str()).unwrap_or("");
    let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let key = args.get("key").and_then(|k| k.as_str()).unwrap_or("");
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let dx = args.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
    let dy = args.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);
    let label = args.get("label").and_then(|l| l.as_str()).unwrap_or("");
    let role_str = args.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let expect = args.get("expect").and_then(|e| e.as_str()).unwrap_or("");
    let press = args.get("press").and_then(|p| p.as_str()).unwrap_or("");

    let action = match action_str {
        "click" => {
            let cx = args.get("x").and_then(|v| v.as_f64());
            let cy = args.get("y").and_then(|v| v.as_f64());
            if let (Some(x), Some(y)) = (cx, cy) {
                Action::ClickCoord { x, y }
            } else {
                let resolved = if !ref_id.is_empty() {
                    ref_id.to_string()
                } else if !text.is_empty() {
                    let rf = if !role_str.is_empty() { Some(role_str) } else { None };
                    resolve_text_target(page, text, rf).await?
                } else {
                    return Err(BladeError::Other("click requires 'ref', 'text', or 'x'+'y'".into()));
                };
                Action::Click { ref_id: resolved }
            }
        }
        "type" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() { Some(role_str) } else { Some("textbox") };
                resolve_text_target(page, label, rf).await?
            } else {
                return Err(BladeError::Other("type requires 'ref' or 'label' + 'text'".into()));
            };
            Action::Type { ref_id: resolved, text: text.into() }
        }
        "clear" => Action::Clear { ref_id: ref_id.into() },
        "select" => {
            let opt = args.get("option").and_then(|o| o.as_str())
                .or_else(|| args.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            Action::Select { ref_id: ref_id.into(), option: opt.into() }
        }
        "press" => Action::Press { key: key.into() },
        "scroll" => Action::Scroll { dx, dy },
        "hover" => Action::Hover { ref_id: ref_id.into() },
        "upload" => Action::Upload { ref_id: ref_id.into(), path: text.into() },
        "read" => {
            let text_content = crate::action::read_text(page.cdp_ref(), page.model(), ref_id).await?;
            let el = page.model().element(ref_id);
            let role = el.map(|e| e.raw.role.clone()).unwrap_or_default();
            let name = el.map(|e| e.raw.name.clone()).unwrap_or_default();
            return Ok(format!(
                "Page: {} | phase: {} | {} actionable\n{} {} \"{}\"\n  text: \"{}\"\n",
                page.model().url(),
                page.model().phase(),
                page.model().actionables(),
                ref_id, role, name, text_content
            ));
        }
        "fill" => {
            let fields = args.get("fields").and_then(|f| f.as_array())
                .ok_or_else(|| BladeError::Other("fill requires 'fields' array".into()))?;
            let submit = args.get("submit").and_then(|s| s.as_str()).unwrap_or("");
            let mut last_verdict = String::new();
            let mut count = 0usize;
            for field in fields {
                let f_ref = field.get("ref").and_then(|r| r.as_str()).unwrap_or("");
                let f_label = field.get("label").and_then(|l| l.as_str()).unwrap_or("");
                let f_text = field.get("text").and_then(|t| t.as_str())
                    .or_else(|| field.get("option").and_then(|o| o.as_str()))
                    .unwrap_or("");
                let f_check = field.get("check").and_then(|c| c.as_bool());

                // Resolve the ref — try as-is first, then by label.
                let resolved = if !f_ref.is_empty() {
                    f_ref.to_string()
                } else if !f_label.is_empty() {
                    // Don't restrict to textbox — the field could be a
                    // checkbox or select. Search all actionable elements.
                    resolve_text_target(page, f_label, None).await?
                } else {
                    continue;
                };

                // Dispatch the right action based on element type.
                let role = page.model().element(&resolved)
                    .map(|e| e.raw.role.clone())
                    .unwrap_or_default();
                let action = match role.as_str() {
                    "checkbox" | "radio" => {
                        // For checkboxes/radios: click to toggle.
                        // If 'check' is specified, only click if current
                        // state doesn't match desired state.
                        let should_click = match f_check {
                            Some(want_checked) => {
                                let is_checked = page.model().element(&resolved)
                                    .and_then(|e| e.raw.checked)
                                    .unwrap_or(false);
                                is_checked != want_checked
                            }
                            None => true, // no 'check' param → just click
                        };
                        if should_click {
                            Action::Click { ref_id: resolved }
                        } else {
                            count += 1;
                            continue; // already in desired state
                        }
                    }
                    "combobox" => {
                        Action::Select { ref_id: resolved, option: f_text.into() }
                    }
                    _ => {
                        // Default: type into text-like fields.
                        Action::Type { ref_id: resolved, text: f_text.into() }
                    }
                };
                let (_, verdict) = page.act(action).await?;
                last_verdict = verdict;
                count += 1;
            }
            let last_delta = if !submit.is_empty() {
                let resolved = if submit.starts_with('e') {
                    submit.to_string()
                } else {
                    resolve_text_target(page, submit, None).await?
                };
                let (delta, verdict) = page.act(Action::Click { ref_id: resolved }).await?;
                last_verdict = verdict;
                delta
            } else {
                page.recapture().await?
            };
            return Ok(format!("filled {count} fields\n{last_verdict}\n{}",
                page.delta_view(&last_delta, 8000)));
        }
        "wait" => {
            let condition = args.get("condition").and_then(|c| c.as_str()).unwrap_or("settle");
            let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(10);
            let match_text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Action::Wait {
                condition: condition.into(),
                text: match_text.into(),
                timeout: std::time::Duration::from_secs(timeout_secs),
            }
        }
        "back" => Action::Back,
        "navigate" => {
            let delta = page.navigate(url).await?;
            let verdict = if delta.navigated {
                format!("outcome: navigated \u{2192} {}", page.model().url())
            } else {
                "outcome: already here".to_string()
            };
            return Ok(format!("{verdict}\n{}", page.delta_view(&delta, 8000)));
        }
        _ => {
            return Err(crate::error::BladeError::Other(format!(
                "unknown action: {action_str}"
            )))
        }
    };

    // For scroll, the delta is useless (scrolling doesn't add/remove elements).
    // Return the full view so the agent sees what's on the page after scrolling.
    let is_scroll = matches!(action, Action::Scroll { .. });

    let result = page.act(action).await;

    // If `type` had a `press` param (e.g. press="Enter"), fire the key
    // after the text is in the field. This is the most common agent
    // pattern: type a search query + Enter to submit.
    let result = if action_str == "type" && !press.is_empty() {
        match result {
            Ok((type_delta, type_verdict)) => {
                let press_result = page.act(Action::Press { key: press.into() }).await;
                match press_result {
                    Ok((press_delta, press_verdict)) => {
                        // Merge: the press result is what matters (it triggers
                        // navigation / form submit). Include the type verdict
                        // as context.
                        let merged_verdict = format!(
                            "{type_verdict} then {press_verdict}"
                        );
                        Ok((press_delta, merged_verdict))
                    }
                    // Press failed — return the type result with a note.
                    Err(e) => Ok((type_delta, format!(
                        "{type_verdict} (press {press} failed: {e})"
                    ))),
                }
            }
            other => other,
        }
    } else {
        result
    };

    match result {
        Ok((delta, verdict)) => {
            // M14: Check expect param against observed outcome.
            let expect_note = if !expect.is_empty() {
                let observed = if delta.navigated { "navigation" }
                    else if !delta.is_empty() { "dom-change" }
                    else { "none" };
                if expect != observed && expect != "any" {
                    format!("\n\u{26a0} expected {expect}, got {observed} \u{2014} may have hit wrong target")
                } else { String::new() }
            } else { String::new() };
            if is_scroll {
                Ok(format!("{verdict}{expect_note}\n{}", page.view(8000)))
            } else {
                Ok(format!("{verdict}{expect_note}\n{}", page.delta_view(&delta, 8000)))
            }
        }
        // Error context: recapture and include available elements so the
        // agent doesn't need a separate `see` call to understand the failure.
        Err(e) => {
            let _ = page.recapture().await;
            let view = page.view(3000);
            Err(crate::error::BladeError::Other(format!(
                "{e}\n\n--- current page state ---\n{view}"
            )))
        }
    }
}

async fn handle_see(args: &Value, page: &mut Page) -> Result<String> {
    let budget = args.get("budget").and_then(|b| b.as_u64()).unwrap_or(8000) as usize;
    let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("");
    let want_content = args.get("content").and_then(|c| c.as_bool()).unwrap_or(false);
    let find = args.get("find").and_then(|f| f.as_str()).unwrap_or("");
    let extract = args.get("extract").and_then(|e| e.as_str()).unwrap_or("");
    let scope = args.get("scope").and_then(|s| s.as_str()).unwrap_or("");

    // M11: find — search all actionable elements by text, return matches with refs.
    if !find.is_empty() {
        let matches = crate::action::find_by_text(page.cdp_ref(), find, None).await?;
        if matches.is_empty() {
            // M11 contract is full-page search: when no ACTIONABLE element
            // matches, probe the plain text. Returns a context snippet so
            // agents can verify outcomes ("Order confirmed") without
            // dumping full page content.
            let q_json = serde_json::to_string(&find.to_lowercase()).unwrap_or_else(|_| "''".into());
            let probe = page.cdp_ref().send("Runtime.evaluate", Some(json!({
                "expression": format!("(function(){{var q={q_json};var t=(document.body&&document.body.innerText)||'';var i=t.toLowerCase().indexOf(q);if(i<0)return null;return t.slice(Math.max(0,i-60),i+q.length+60).replace(/\\s+/g,' ').trim();}})()"),
                "returnByValue": true,
            }))).await.ok()
                .and_then(|r| r.get("result").and_then(|x| x.get("value")).cloned());
            if let Some(serde_json::Value::String(snip)) = probe {
                return Ok(format!(
                    "no actionable elements matching \"{find}\", but text present in page:\n  \"…{snip}…\"\n  (see content=true to read full context)"
                ));
            }
            return Ok(format!("no elements matching \"{find}\" found", ));
        }
        let mut out = format!("find \"{}\": {} match{}\n", find, matches.len(), if matches.len() > 1 { "es" } else { "" });
        for m in &matches {
            let ref_id = page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame);
            out.push_str(&format!("{} {} \"{}\" (score: {})\n", ref_id, m.role, m.name, m.score));
        }
        return Ok(out);
    }

    // M12: extract — structured data extraction (links/forms).
    if !extract.is_empty() {
        let expr = match extract {
            "links" => r#"(()=>{const links=[...document.querySelectorAll('a[href]')];return JSON.stringify(links.map(a=>({text:(a.textContent||'').trim().slice(0,80),href:a.href})).filter(l=>l.text||l.href));})()"#,
            "forms" => r#"(()=>{const forms=[...document.querySelectorAll('form')];return JSON.stringify(forms.map(f=>({action:f.action,method:(f.method||'get').toLowerCase(),fields:[...f.elements].filter(e=>e.tagName!=='FIELDSET').map(e=>({tag:e.tagName.toLowerCase(),type:e.type||null,name:e.name||'',label:(e.labels&&e.labels[0]?e.labels[0].textContent:'').trim().slice(0,60)||e.placeholder||''}))})));})()"#,
            _ => return Err(BladeError::Other(format!("unknown extract type: {extract} (use 'links' or 'forms')"))),
        };
        let res = page.cdp_ref().send("Runtime.evaluate", Some(serde_json::json!({
            "expression": expr,
            "returnByValue": true,
        }))).await?;
        let json_str = res.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("[]");
        return Ok(format!("extract {extract}:\n{json_str}"));
    }

    // M12: scope — return the text content of one element's subtree.
    if !scope.is_empty() {
        let text = crate::action::read_text(page.cdp_ref(), page.model(), scope).await?;
        let el = page.model().element(scope);
        let (role, name) = el.map(|e| (e.raw.role.clone(), e.raw.name.clone())).unwrap_or_default();
        return Ok(format!("scope {scope} {role} \"{}\":\n{}", name, text));
    }

    // Recapture to get fresh state.
    let _delta = page.recapture().await?;
    let mut out = if filter.is_empty() {
        page.view(budget)
    } else {
        page.view_filtered(budget, filter)
    };
    // Auto-include content when the page has very few actionable elements —
    // the agent almost certainly needs the text, not just the empty element list.
    let auto_content = !want_content && page.model().actionables() <= 3 && filter.is_empty();
    if want_content || auto_content {
        let content_budget = if auto_content { budget } else { budget.min(4000) };
        let content = page.content(content_budget).await?;
        if !content.is_empty() {
            out.push_str("\n--- page content ---\n");
            out.push_str(&content);
            out.push('\n');
        }
    }
    Ok(out)
}

async fn handle_state(args: &Value, page: &Page) -> Result<String> {
    let op_str = args.get("op").and_then(|o| o.as_str()).unwrap_or("");
    let name = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let target_id = args.get("target_id").and_then(|t| t.as_str()).unwrap_or("");

    let op = match op_str {
        "cookies" => StateOp::GetCookies { urls: vec![] },
        "set-cookie" => StateOp::SetCookie {
            name: name.into(), value: value.into(),
            domain: None, path: None, secure: None, http_only: None, same_site: None,
        },
        "del-cookie" => StateOp::DeleteCookies { name: name.into(), domain: None },
        "ls" => StateOp::GetLocalStorage,
        "ss" => StateOp::GetSessionStorage,
        "set-ls" => StateOp::SetLocalStorage { key: name.into(), value: value.into() },
        "set-ss" => StateOp::SetSessionStorage { key: name.into(), value: value.into() },
        "rm-ls" => StateOp::RemoveLocalStorage { key: name.into() },
        "rm-ss" => StateOp::RemoveSessionStorage { key: name.into() },
        "clear-ls" => StateOp::ClearLocalStorage,
        "clear-ss" => StateOp::ClearSessionStorage,
        "tabs" => StateOp::ListTabs,
        "open-tab" => StateOp::OpenTab { url: url.into() },
        "close-tab" => StateOp::CloseTab { target_id: target_id.into() },
        "save" => StateOp::SaveSession { name: name.into() },
        "load" => StateOp::LoadSession { name: name.into() },
        _ => return Err(crate::error::BladeError::Other(format!("unknown state op: {op_str}"))),
    };

    page.state(op).await
}

async fn handle_run(args: &Value, page: &mut Page) -> Result<String> {
    let steps = args.get("steps").and_then(|s| s.as_array()).ok_or_else(|| {
        crate::error::BladeError::Other("run requires 'steps' array".into())
    })?;

    let mut observations = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        execute_step(page, step, &i.to_string(), &mut observations).await?;
    }
    Ok(observations.join("\n"))
}

/// Build an Action from a step's JSON fields. Used by `execute_step` for
/// regular (non-special) actions.
fn build_action(step: &Value) -> Result<Action> {
    let action_str = step.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let ref_id = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
    let text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let key = step.get("key").and_then(|k| k.as_str()).unwrap_or("");
    let dx = step.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
    let dy = step.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);

    match action_str {
        "click" => Ok(Action::Click { ref_id: ref_id.into() }),
        "type" => Ok(Action::Type { ref_id: ref_id.into(), text: text.into() }),
        "clear" => Ok(Action::Clear { ref_id: ref_id.into() }),
        "select" => {
            let opt = step.get("option").and_then(|o| o.as_str())
                .or_else(|| step.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            Ok(Action::Select { ref_id: ref_id.into(), option: opt.into() })
        }
        "press" => Ok(Action::Press { key: key.into() }),
        "scroll" => Ok(Action::Scroll { dx, dy }),
        "hover" => Ok(Action::Hover { ref_id: ref_id.into() }),
        "upload" => Ok(Action::Upload { ref_id: ref_id.into(), path: text.into() }),
        "wait" => {
            let condition = step.get("condition").and_then(|c| c.as_str()).unwrap_or("settle");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(10);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Ok(Action::Wait {
                condition: condition.into(),
                text: match_text.into(),
                timeout: std::time::Duration::from_secs(timeout_secs),
            })
        }
        "back" => Ok(Action::Back),
        _ => Err(crate::error::BladeError::Other(format!(
            "unknown action: {action_str}"
        ))),
    }
}

/// Execute one step in a `run` sequence. Handles regular actions (via
/// `build_action` + `page.act`), `navigate` (special: stealth + navigate +
/// wait), `read` (special: returns text, not delta), and `if` (conditional
/// branching with `then`/`else` sub-steps). Recursion supports nested `if`.
///
/// `path` is a display label for the step (e.g. "0" for top-level, "0.1" for
/// the first sub-step inside step 0's branch).
async fn execute_step(
    page: &mut Page,
    step: &Value,
    path: &str,
    observations: &mut Vec<String>,
) -> Result<()> {
    let action_str = step.get("action").and_then(|a| a.as_str()).unwrap_or("");

    match action_str {
        "if" => {
            let condition = step.get("condition").and_then(|c| c.as_str()).unwrap_or("settle");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(5);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let then_steps = step.get("then").and_then(|s| s.as_array()).cloned().unwrap_or_default();
            let else_steps = step.get("else").and_then(|s| s.as_array()).cloned().unwrap_or_default();

            // Evaluate the condition (waits up to timeout).
            let met = crate::action::check_condition(
                page.cdp_ref(),
                condition,
                match_text,
                std::time::Duration::from_secs(timeout_secs),
            )
            .await;

            let (branch, label) = if met {
                (&then_steps, "then")
            } else {
                (&else_steps, "else")
            };

            if branch.is_empty() {
                if met {
                    observations.push(format!("step {path}: if({condition} \"{match_text}\") → then (no steps)"));
                } else {
                    observations.push(format!("step {path}: if({condition} \"{match_text}\") → skipped (timeout {timeout_secs}s)"));
                }
            } else {
                observations.push(format!("step {path}: if({condition} \"{match_text}\") → {label}"));
                // If the condition was met, the page may have just changed —
                // wait for settle before recapturing for fresh refs.
                if met {
                    crate::page::wait_for_settle_with_network(
                        page.cdp_ref(),
                        std::time::Duration::from_secs(3),
                        Some(page.in_flight_ref()),
                    )
                    .await?;
                }
                // Always recapture to get fresh refs for branch steps.
                let _ = page.recapture().await?;
                for (j, sub_step) in branch.iter().enumerate() {
                    let sub_path = format!("{path}.{j}");
                    Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                }
            }
        }
        "while" => {
            let condition = step.get("condition").and_then(|c| c.as_str()).unwrap_or("element");
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(5);
            let max = step.get("max").and_then(|m| m.as_u64()).unwrap_or(10) as usize;
            let body = step.get("steps").and_then(|s| s.as_array()).cloned().unwrap_or_default();
            for i in 0..max {
                let met = crate::action::check_condition(
                    page.cdp_ref(), condition, match_text,
                    std::time::Duration::from_secs(timeout_secs),
                ).await;
                if !met {
                    observations.push(format!("step {path}: while({condition} \"{match_text}\") \u{2192} done after {i} iterations"));
                    break;
                }
                observations.push(format!("step {path}: while({condition} \"{match_text}\") iteration {i}"));
                let _ = page.recapture().await?;
                for (j, sub_step) in body.iter().enumerate() {
                    let sub_path = format!("{path}.{i}.{j}");
                    Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                }
                if i + 1 == max {
                    observations.push(format!("step {path}: while \u{2192} reached max ({max}) iterations"));
                }
            }
        }
        "navigate" => {
            let url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let delta = page.navigate(url).await?;
            observations.push(format!("step {path}: {}", page.delta_view(&delta, 4000)));
        }
        "read" => {
            let ref_id = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
            let text_content = crate::action::read_text(page.cdp_ref(), page.model(), ref_id).await?;
            let (role, name) = page
                .model()
                .element(ref_id)
                .map(|e| (e.raw.role.clone(), e.raw.name.clone()))
                .unwrap_or_default();
            let truncated: String = text_content.chars().take(200).collect();
            observations.push(format!(
                "step {path}: read {ref_id} {role} \"{name}\"\n  text: \"{truncated}\""
            ));
        }
        _ => {
            let action = build_action(step)?;
            match page.act(action).await {
                Ok((delta, verdict)) => {
                    observations.push(format!("step {path}: {verdict}\n{}", page.delta_view(&delta, 4000)));
                }
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Capture a screenshot of the current page and return it as an MCP image
/// content block (base64 PNG).
///
/// This is the `vision` tool (decision D5) — a rare fallback for canvas
/// content, exotic layouts, or when the structural model fails.
async fn handle_vision(
    id: Option<Value>,
    _args: &Value,
    page: &mut Page,
) -> std::result::Result<Value, BladeError> {
    let cdp = page.cdp_ref();
    let result = cdp
        .send(
            "Page.captureScreenshot",
            Some(serde_json::json!({
                "format": "png",
            })),
        )
        .await;

    match result {
        Ok(res) => {
            let data = res.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "Screenshot returned no data." }],
                        "isError": true,
                    }
                }));
            }
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{
                        "type": "image",
                        "data": data,
                        "mimeType": "image/png"
                    }]
                }
            }))
        }
        // Propagate Closed so serve() can self-heal.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("\u{2717} error: {e}") }],
                "isError": true,
            }
        })),
    }
}
