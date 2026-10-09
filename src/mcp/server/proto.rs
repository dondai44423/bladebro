//! Protocol verbs: the legacy `initialize` handshake, `server/discover`
//! (SEP-2575), `tools/list`, and the `tools/call` dispatch that routes to
//! the tool handlers. Split from the `server` core.

use serde_json::{json, Value};

use crate::error::BladeError;
use crate::mcp::tools::tools_to_json;
use crate::page::Page;

use super::{handle_act, handle_run, handle_see, handle_state, handle_vision};
use super::{shape_result, INSTRUCTIONS, PROTOCOL_VERSION, STATELESS_VERSION, SUPPORTED_VERSIONS};

pub(super) fn handle_initialize(id: Option<Value>, params: &Value) -> Value {
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
pub(super) fn handle_discover(id: Option<Value>, version: Option<&str>) -> Value {
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

pub(super) fn handle_tools_list(id: Option<Value>, version: Option<&str>) -> Value {
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

pub async fn handle_tools_call(
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
        _ => Err(crate::error::BladeError::Other(format!(
            "unknown tool: {name}"
        ))),
    };

    match result {
        Ok(mut text) => {
            // Drain dialogs handled during this call (default handling or
            // an armed expectation), plus a one-shot note for an armed
            // expectation that expired unused.
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\u{26a0} dialogs handled:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    let how = if d.via == "expectation" {
                        " (via armed expectation)"
                    } else {
                        ""
                    };
                    text.push_str(&format!(
                        "  {} \"{}\" \u{2014} {}{}\n",
                        d.kind, d.message, action, how
                    ));
                    if let Some(n) = &d.note {
                        text.push_str(&format!("    ({n})\n"));
                    }
                    if let Some(p) = &d.default_prompt {
                        if !p.is_empty() {
                            text.push_str(&format!("    (prompt default: \"{}\")\n", p));
                        }
                    }
                }
            }
            if let Some(note) = page.drain_dialog_expect_note() {
                text.push_str(&format!("\n\u{26a0} {note}\n"));
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
            // Dead-tab errors propagate as-is so serve()'s tab-recovery
            // branch can run. The old code converted EVERY error to an
            // isError text response, which made that branch unreachable
            // dead code — a closed tab errored forever instead of healing.
            let msg = e.to_string();
            let tab_died = msg.contains("Target closed")
                || msg.contains("No target with given id")
                || msg.contains("Session closed")
                || msg.contains("Target.detachedFromTarget");
            if tab_died {
                return Err(e);
            }
            let mut text = format!("\u{2717} error: {e}");
            // Page-state contract: every error carries enough state for
            // the agent to recover without a separate see call. Handlers
            // that already embedded a state section (handle_act's action
            // path) are not doubled up.
            if !text.contains("--- current page state ---") {
                let view = page.view(1200);
                if !view.trim().is_empty() {
                    text.push_str(&format!("\n--- current page state ---\n{view}"));
                }
                // G09: recovery advice distinguishes "still loading" from
                // "the action did nothing" — one bounded, safe observation.
                if let Some(note) = page.load_state_note().await {
                    text.push_str(&format!("\nnote: {note}"));
                }
            }
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\n\u{26a0} dialogs handled:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    let how = if d.via == "expectation" {
                        " (via armed expectation)"
                    } else {
                        ""
                    };
                    text.push_str(&format!(
                        "  {} \"{}\" \u{2014} {}{}\n",
                        d.kind, d.message, action, how
                    ));
                    if let Some(n) = &d.note {
                        text.push_str(&format!("    ({n})\n"));
                    }
                }
            }
            if let Some(note) = page.drain_dialog_expect_note() {
                text.push_str(&format!("\n\u{26a0} {note}\n"));
            }
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\u{26a0} {}\n", a));
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
