//! `state` handler: cookies/storage/tabs/sessions + compress/block toggles.
//!
//! Tab lifecycle ops live here (not in the page layer) because they
//! re-attach the session — they need `&mut Page`.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;
use crate::state::StateOp;

pub async fn handle_state(args: &Value, page: &mut Page) -> Result<String> {
    if !args.is_object() {
        return Err(BladeError::Other(
            "state arguments must be an object".into(),
        ));
    }
    for key in [
        "op",
        "name",
        "value",
        "url",
        "domain",
        "path",
        "sameSite",
        "target_id",
        "mode",
        "classes",
    ] {
        if args.get(key).is_some_and(|value| !value.is_string()) {
            return Err(BladeError::Other(format!("state {key} must be a string")));
        }
    }
    for key in ["secure", "httpOnly", "clear"] {
        if args.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(BladeError::Other(format!("state {key} must be a boolean")));
        }
    }
    let op_str = args.get("op").and_then(|o| o.as_str()).unwrap_or("");
    let name = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let target_id = args.get("target_id").and_then(|t| t.as_str()).unwrap_or("");

    // Tab lifecycle ops are handled HERE (not via state.rs) because
    // they re-attach the session — they need &mut Page.
    match op_str {
        // Context pruning toggle.
        "compress" => {
            let mode = args
                .get("mode")
                .and_then(|m| m.as_str())
                .or_else(|| args.get("value").and_then(|v| v.as_str()))
                .unwrap_or("");
            return match mode {
                "on" => {
                    page.set_compress_enabled(true);
                    Ok("context pruning: on (act responses compress after turn 3)".into())
                }
                "off" => {
                    page.set_compress_enabled(false);
                    Ok("context pruning: off (all act responses are full)".into())
                }
                "status" | "" => {
                    let status = if page.compress_enabled() { "on" } else { "off" };
                    let turn = page.act_turn();
                    Ok(format!("context pruning: {status} (current turn: {turn})"))
                }
                _ => Err(crate::error::BladeError::Other(
                    "compress mode must be 'on', 'off', or 'status'".into(),
                )),
            };
        }
        "open-tab" => {
            // Manual-control pause: opening + focusing a tab changes what
            // the person is looking at — the same class as navigation.
            if crate::realbrowser::input_paused() {
                return Err(crate::realbrowser::paused_error());
            }
            // Create + auto-focus. Every agent that opens a tab
            // wants to act in it — a separate switch-tab call
            // would be pure waste. Browser-level create (works in
            // both transports; the page session is not always valid).
            let new_id = page.open_tab_target(url).await?;
            page.switch_tab(&new_id).await?;
            let view = page.view(1500);
            return Ok(format!(
                "\u{2713} opened + switched to tab {new_id}\n{view}"
            ));
        }
        "switch-tab" => {
            // Manual-control pause: switching focuses a different tab —
            // the person's view jumps.
            if crate::realbrowser::input_paused() {
                return Err(crate::realbrowser::paused_error());
            }
            page.switch_tab(target_id).await?;
            let view = page.view(1500);
            return Ok(format!("\u{2713} switched to tab {target_id}\n{view}"));
        }
        "close-tab" => {
            // Manual-control pause: the tab could be one the person is using.
            if crate::realbrowser::input_paused() {
                return Err(crate::realbrowser::paused_error());
            }
            page.cdp_ref()
                .send("Target.closeTarget", Some(json!({ "targetId": target_id })))
                .await?;
            // If the agent closed the tab the session was attached
            // to, the session is now dead — auto-switch to a
            // remaining tab so the next command doesn't error.
            if !page.current_tab_alive().await {
                let tabs = page.tab_targets().await;
                if let Some(first) = tabs.first() {
                    page.switch_tab(&first.id).await?;
                    let view = page.view(1500);
                    return Ok(format!(
                        "\u{2713} closed tab {target_id} (was current; switched to {})\n{view}",
                        first.id
                    ));
                }
                return Ok(format!(
                    "\u{2713} closed tab {target_id} (was the last tab — Chrome exited; the next command relaunches it with a fresh page)"
                ));
            }
            return Ok(format!("\u{2713} closed tab {target_id}"));
        }
        "block" => {
            let classes = args.get("classes").and_then(|c| c.as_str()).unwrap_or("");
            if !classes.is_empty() {
                let mask = page.set_block_classes(classes).await?;
                let here = page.model().url().to_string();
                page.remember_block_choice(&here, classes, mask != 0);
                return Ok(format!(
                    "blocking: {}",
                    crate::page::intercept::InterceptState::describe(mask)
                ));
            }
            if args.get("clear").and_then(|c| c.as_bool()).unwrap_or(false) {
                page.set_block_classes("none").await?;
                let here = page.model().url().to_string();
                page.remember_block_choice(&here, "", false);
                return Ok("blocking: none".to_string());
            }
            return Ok(format!(
                "blocking: {}",
                crate::page::intercept::InterceptState::describe(page.block_rules())
            ));
        }
        _ => {}
    }

    let op = match op_str {
        "cookies" => {
            // If url is provided, filter cookies to that domain.
            // Otherwise, use the current page URL so the agent gets
            // relevant cookies, not a 100+ line dump of all browser cookies.
            let filter_url = if !url.is_empty() {
                Some(url.to_string())
            } else if args.get("domain").and_then(Value::as_str).is_some() {
                None
            } else {
                let current = page.model().url().to_string();
                if current.is_empty() || current == "about:blank" {
                    None
                } else {
                    Some(current)
                }
            };
            StateOp::GetCookies {
                urls: filter_url.map(|u| vec![u]).unwrap_or_default(),
            }
        }
        "set-cookie" => StateOp::SetCookie {
            name: name.into(),
            value: value.into(),
            // CDP Network.setCookie requires either url or domain.
            // Prefer url when the agent provides it; fall back to the
            // current page's url so the call never fails for missing scope.
            url: if !url.is_empty() {
                Some(url.into())
            } else if args.get("domain").and_then(Value::as_str).is_some() {
                None
            } else {
                let current = page.model().url().to_string();
                if current.is_empty() || current == "about:blank" {
                    None
                } else {
                    Some(current)
                }
            },
            domain: args
                .get("domain")
                .and_then(|d| d.as_str())
                .map(String::from),
            path: args.get("path").and_then(|p| p.as_str()).map(String::from),
            secure: args.get("secure").and_then(|s| s.as_bool()),
            http_only: args.get("httpOnly").and_then(|h| h.as_bool()),
            same_site: args
                .get("sameSite")
                .and_then(|s| s.as_str())
                .map(String::from),
        },
        "del-cookie" => StateOp::DeleteCookies {
            name: name.into(),
            domain: args
                .get("domain")
                .and_then(|d| d.as_str())
                .map(String::from),
            // CDP Network.deleteCookies requires either url or domain.
            // Prefer url when the agent provides it; fall back to the
            // current page's url so the call never fails for missing scope.
            url: if !url.is_empty() {
                Some(url.into())
            } else if args.get("domain").and_then(Value::as_str).is_some() {
                None
            } else {
                let current = page.model().url().to_string();
                if current.is_empty() || current == "about:blank" {
                    None
                } else {
                    Some(current)
                }
            },
        },
        "ls" => StateOp::GetLocalStorage,
        "ss" => StateOp::GetSessionStorage,
        "set-ls" => StateOp::SetLocalStorage {
            key: name.into(),
            value: value.into(),
        },
        "set-ss" => StateOp::SetSessionStorage {
            key: name.into(),
            value: value.into(),
        },
        "rm-ls" => StateOp::RemoveLocalStorage { key: name.into() },
        "rm-ss" => StateOp::RemoveSessionStorage { key: name.into() },
        "clear-ls" => StateOp::ClearLocalStorage,
        "clear-ss" => StateOp::ClearSessionStorage,
        "tabs" => StateOp::ListTabs,
        "save" => StateOp::SaveSession { name: name.into() },
        "load" => StateOp::LoadSession { name: name.into() },
        _ => {
            return Err(crate::error::BladeError::Other(format!(
                "unknown state op: {op_str}"
            )))
        }
    };

    page.state(op).await
}
