//! The `state` command parser — same op names as MCP, loud on unknown ops/flags.

use super::take_value;
use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Parse `state` args: <op> [args] [flags]. Same op names as MCP (plus
/// rm-ss), and unknown ops/flags are loud errors.
pub(crate) fn parse_state_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Usage(
            "state needs an op — cookies, set-cookie, del-cookie, ls, ss, set-ls, set-ss, \
             rm-ls, rm-ss, clear-ls, clear-ss, tabs, open-tab, close-tab, switch-tab, save, \
             load, compress, block (see 'bladebro help state')"
                .into(),
        ));
    }

    const OPS: &[&str] = &[
        "cookies",
        "set-cookie",
        "del-cookie",
        "ls",
        "ss",
        "set-ls",
        "set-ss",
        "rm-ls",
        "rm-ss",
        "clear-ls",
        "clear-ss",
        "tabs",
        "open-tab",
        "close-tab",
        "switch-tab",
        "save",
        "load",
        "compress",
        "block",
    ];
    // Aliases: MCP-style names.
    let op = match args[0].as_str() {
        "localStorage" => "ls",
        "sessionStorage" => "ss",
        other => other,
    };
    if !OPS.contains(&op) {
        return Err(BladeError::Usage(format!(
            "unknown state op '{op}' — available: {} (see 'bladebro help state')",
            OPS.join(", ")
        )));
    }

    let mut j = json!({ "op": op });
    let mut pos: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "url" | "domain" | "path" | "name" | "value" | "key" | "classes" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "same-site" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["sameSite"] = json!(v);
                }
                "target-id" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["target_id"] = json!(v);
                }
                "mode" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["mode"] = json!(v);
                }
                "secure" => j["secure"] = json!(true),
                "http-only" => j["httpOnly"] = json!(true),
                "clear" => j["clear"] = json!(true),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for state — see 'bladebro help state'"
                    )))
                }
            }
        } else {
            pos.push(a);
        }
        i += 1;
    }

    // Positionals fill name/value per op (flags win when both are given).
    match op {
        "set-cookie" => {
            let mut p = pos.into_iter();
            let name = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| p.next());
            let value = j
                .get("value")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| p.next());
            let name = name.ok_or_else(|| {
                BladeError::Usage(
                    "set-cookie needs a name — `state set-cookie <name> <value> [--url <u> | --domain <d>]`"
                        .into(),
                )
            })?;
            let value = value.ok_or_else(|| {
                BladeError::Usage(
                    "set-cookie needs a value — `state set-cookie <name> <value>`".into(),
                )
            })?;
            j["name"] = json!(name);
            j["value"] = json!(value);
        }
        "del-cookie" => {
            let name = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(
                        "del-cookie needs a name — `state del-cookie <name> [--url <u> | --domain <d>]`"
                            .into(),
                    )
                })?;
            j["name"] = json!(name);
        }
        "set-ls" | "set-ss" => {
            // Storage ops take the key in `name` — the MCP schema field the
            // handler actually reads. The old code emitted `key`, so every
            // set-ls/set-ss stored an EMPTY key (caught live: `ls` showed
            // "=dark"). `--key`/`--name` flags both work.
            let key = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| j.get("key").and_then(|x| x.as_str()).map(String::from))
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a key — `state {op} <key> <value>`"))
                })?;
            let value = j
                .get("value")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.get(1).cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a value — `state {op} <key> <value>`"))
                })?;
            if let Some(obj) = j.as_object_mut() {
                obj.remove("key");
            }
            j["name"] = json!(key);
            j["value"] = json!(value);
        }
        "rm-ls" | "rm-ss" => {
            let key = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| j.get("key").and_then(|x| x.as_str()).map(String::from))
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a key — `state {op} <key>`"))
                })?;
            if let Some(obj) = j.as_object_mut() {
                obj.remove("key");
            }
            j["name"] = json!(key);
        }
        "cookies" => {
            // Optional positional URL filters the list to that domain.
            if j.get("url").is_none() {
                if let Some(u) = pos.first() {
                    j["url"] = json!(u);
                }
            }
        }
        "open-tab" => {
            let url = j
                .get("url")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage("open-tab needs a URL — `state open-tab <url>`".into())
                })?;
            j["url"] = json!(url);
        }
        "close-tab" | "switch-tab" => {
            let id = j
                .get("target_id")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!(
                        "{op} needs a tab id — `state {op} <id>` (ids come from `state tabs`)"
                    ))
                })?;
            j["target_id"] = json!(id);
        }
        "save" | "load" => {
            let name = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a session name — `state {op} <name>`"))
                })?;
            j["name"] = json!(name);
        }
        "compress" => {
            let mode = j
                .get("mode")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .unwrap_or_else(|| "status".to_string());
            if !matches!(mode.as_str(), "on" | "off" | "status") {
                return Err(BladeError::Usage(format!(
                    "compress mode must be on|off|status, got '{mode}'"
                )));
            }
            j["mode"] = json!(mode);
        }
        "block" => {
            if j.get("classes").is_none() {
                if let Some(c) = pos.first() {
                    if c == "clear" {
                        j["clear"] = json!(true);
                    } else {
                        j["classes"] = json!(c);
                    }
                }
            }
        }
        "tabs" | "ls" | "ss" | "clear-ls" | "clear-ss" => {
            if !pos.is_empty() {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}' for {op}",
                    pos[0]
                )));
            }
        }
        _ => unreachable!("op validated above"),
    }

    Ok(j)
}
