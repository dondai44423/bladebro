//! The `act` command parser — universal `--field` flags + terse positionals, 1:1 with the MCP schema.

use super::{is_ref, normalize_fields, resolve_payload, take_num, take_value};
use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Parse `act` args. Universal `--field` flags mirror the MCP schema exactly
/// (--ref, --label, --text, --role, --nth, --key, --url, --option, …), so an
/// agent that knows the MCP tool can drive the CLI 1:1. Positionals are the
/// terse form: target first, value second.
pub(crate) fn parse_act_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Usage(
            "act needs an action — click, type, fill, select, clear, press, scroll, hover, \
             navigate, upload, download, wait, eval, collect, extract, read, batch, pdf, back, \
             forward, reload, save, load, open-tab, close-tab, switch-tab (see 'bladebro help act')"
                .into(),
        ));
    }

    const ACTIONS: &[&str] = &[
        "click",
        "type",
        "fill",
        "select",
        "clear",
        "press",
        "scroll",
        "hover",
        "navigate",
        "upload",
        "download",
        "wait",
        "eval",
        "collect",
        "extract",
        "read",
        "batch",
        "pdf",
        "back",
        "forward",
        "reload",
        "save",
        "load",
        "open-tab",
        "close-tab",
        "switch-tab",
        "dialog",
    ];
    let action = args[0].as_str();
    if !ACTIONS.contains(&action) {
        return Err(BladeError::Usage(format!(
            "unknown act action '{action}' — available: {} (see 'bladebro help act')",
            ACTIONS.join(", ")
        )));
    }

    let mut j = json!({ "action": action });
    let mut pos: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "ref" | "label" | "text" | "role" | "key" | "url" | "option" | "js" | "path"
                | "condition" | "press" | "submit" | "block" | "name" | "expect" | "steps"
                | "fields" | "selector" | "message" | "format" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "template" => {
                    let raw = resolve_payload(&take_value(args, &mut i, flag)?)?;
                    let tpl: Value = serde_json::from_str(&raw).map_err(|e| {
                        BladeError::Usage(format!("--template must be valid JSON: {e}"))
                    })?;
                    j["template"] = tpl;
                }
                "target-id" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["target_id"] = json!(v);
                }
                "prompt-text" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["prompt_text"] = json!(v);
                }
                "accept" => {
                    let v = take_value(args, &mut i, flag)?;
                    match v.as_str() {
                        "true" | "1" | "yes" => j["accept"] = json!(true),
                        "false" | "0" | "no" => j["accept"] = json!(false),
                        _ => {
                            return Err(BladeError::Usage(format!(
                                "--accept takes true or false, got '{v}'"
                            )))
                        }
                    }
                }
                "nth" | "timeout" | "max" => {
                    let v: u64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "dx" | "dy" => {
                    let v: i64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "x" | "y" | "scale" => {
                    let v: f64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "slim" => j["slim"] = json!(true),
                "landscape" => j["landscape"] = json!(true),
                "print-background" => j["printBackground"] = json!(true),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for act — see 'bladebro help act'"
                    )))
                }
            }
        } else {
            pos.push(a);
        }
        i += 1;
    }

    match action {
        "click" | "hover" => {
            if j.get("ref").is_none()
                && j.get("label").is_none()
                && j.get("selector").is_none()
                && !pos.is_empty()
            {
                if is_ref(&pos[0]) {
                    j["ref"] = json!(pos[0].clone());
                    if pos.len() > 1 {
                        return Err(BladeError::Usage(format!(
                            "unexpected extra argument '{}' after a ref — use --label/--nth, or quote the label",
                            pos[1]
                        )));
                    }
                } else {
                    j["label"] = json!(pos.join(" "));
                }
            }
        }
        "type" => {
            let mut pi = 0usize;
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(t);
                    }
                    pi = 1;
                }
            }
            if j.get("text").is_none() && pos.len() > pi {
                j["text"] = json!(pos[pi..].join(" "));
            }
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                return Err(BladeError::Usage(
                    "type needs a target — `act type <ref|label> <text>` or --ref/--label/--selector/--text"
                        .into(),
                ));
            }
            if j.get("text").is_none() {
                return Err(BladeError::Usage(
                    "type needs text — `act type <target> <text>` or --text <value>".into(),
                ));
            }
        }
        "fill" => {
            if let Some(v) = j.get("fields").and_then(|f| f.as_str()).map(String::from) {
                let raw = resolve_payload(&v)?;
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| BladeError::Usage(format!("invalid fields JSON: {e}")))?;
                j["fields"] = normalize_fields(parsed)?;
            } else if j.get("fields").is_none() {
                let raw_arg = pos.first().ok_or_else(|| {
                    BladeError::Usage(
                        "fill needs fields — bladebro act fill '{\"e3\":\"John\",\"e5\":\"Doe\"}' \
                         [--submit <ref|text>] (or @file / - for big payloads)"
                            .into(),
                    )
                })?;
                let raw = resolve_payload(raw_arg)?;
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| BladeError::Usage(format!("invalid fields JSON: {e}")))?;
                j["fields"] = normalize_fields(parsed)?;
            }
            if pos.len() > 1 {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}'",
                    pos[1]
                )));
            }
        }
        "select" => {
            let mut pi = 0usize;
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(t);
                    }
                    pi = 1;
                }
            }
            if j.get("option").is_none() && pos.len() > pi {
                j["option"] = json!(pos[pi..].join(" "));
            }
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                return Err(BladeError::Usage(
                    "select needs a target — `act select <ref|label|--selector> <option>`".into(),
                ));
            }
            if j.get("option").is_none() {
                return Err(BladeError::Usage(
                    "select needs an option — `act select <target> <option text|value>`".into(),
                ));
            }
        }
        "clear" | "read" => {
            // R3: non-ref positionals are labels, exactly like click/type
            // (the old arm forced every positional into ref= and died
            // "stale ref: Search box").
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(pos.join(" "));
                    }
                }
            }
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a ref, label or --selector — `act {action} e5` or `act {action} \"Search\"` (refs come from see model / nav)"
                )));
            }
        }
        "press" => {
            if j.get("key").is_none() && !pos.is_empty() {
                j["key"] = json!(pos[0].clone());
            }
            if j.get("key").is_none() {
                return Err(BladeError::Usage(
                    "press needs a key — `act press Enter` (Enter, Tab, Escape, ArrowDown, …)"
                        .into(),
                ));
            }
        }
        "scroll" => {
            if j.get("dx").is_none() && !pos.is_empty() {
                let v: i64 = pos[0].parse().map_err(|_| {
                    BladeError::Usage(format!("scroll dx must be a number, got '{}'", pos[0]))
                })?;
                j["dx"] = json!(v);
            }
            if j.get("dy").is_none() && pos.len() > 1 {
                let v: i64 = pos[1].parse().map_err(|_| {
                    BladeError::Usage(format!("scroll dy must be a number, got '{}'", pos[1]))
                })?;
                j["dy"] = json!(v);
            }
            if j.get("dx").is_none() && j.get("dy").is_none() {
                return Err(BladeError::Usage(
                    "scroll needs a distance — `act scroll 0 500` or --dy <px> (negative scrolls up)"
                        .into(),
                ));
            }
        }
        "navigate" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if j.get("url").is_none() {
                return Err(BladeError::Usage(
                    "navigate needs a URL — `act navigate example.com`".into(),
                ));
            }
            if pos.len() > 1 {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}'",
                    pos[1]
                )));
            }
        }
        "upload" => {
            let mut pi = 0usize;
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(t);
                    }
                    pi = 1;
                }
            }
            // The MCP handler takes the file path in `text` (NOT `path`) —
            // the old CLI sent `path`, so every CLI upload arrived empty.
            let path = j
                .get("path")
                .and_then(|p| p.as_str())
                .map(String::from)
                .or_else(|| j.get("text").and_then(|t| t.as_str()).map(String::from))
                .or_else(|| {
                    if pos.len() > pi {
                        Some(pos[pi..].join(" "))
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    BladeError::Usage(
                        "upload needs a file path — `act upload e5 /path/file.pdf`".into(),
                    )
                })?;
            if let Some(obj) = j.as_object_mut() {
                obj.remove("path");
            }
            j["text"] = json!(path);
            if j.get("ref").is_none() && j.get("label").is_none() && j.get("selector").is_none() {
                return Err(BladeError::Usage(
                    "upload needs a target — `act upload <ref|label|--selector> <path>`".into(),
                ));
            }
        }
        "download" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if j.get("url").is_none() {
                return Err(BladeError::Usage(
                    "download needs a URL — `act download https://x/file.pdf [--path <dir>]`"
                        .into(),
                ));
            }
        }
        "wait" => {
            if j.get("condition").is_none() && !pos.is_empty() {
                j["condition"] = json!(pos[0].clone());
            }
            if j.get("condition").is_none() {
                return Err(BladeError::Usage(
                    "wait needs a condition — element, title, url, text, settle, or js \
                     (e.g. `act wait settle`, `act wait js \"document.title\"`)"
                        .into(),
                ));
            }
            let cond = j["condition"].as_str().unwrap_or("").to_string();
            if !matches!(
                cond.as_str(),
                "element" | "title" | "url" | "text" | "settle" | "js"
            ) {
                return Err(BladeError::Usage(format!(
                    "unknown wait condition '{cond}' — element, title, url, text, settle, js"
                )));
            }
            if j.get("text").is_none() && pos.len() > 1 {
                j["text"] = json!(pos[1..].join(" "));
            }
            // G02: condition=js carries its expression in js= (preferred) or
            // the legacy text= alias (the `act wait js "expr"` positional
            // form fills text=). The shared resolver on the dispatch side
            // validates conflicts and empty expressions; here we only reject
            // the clearly-missing case early.
            if cond == "js" {
                if j.get("js").is_none() && j.get("text").is_none() {
                    return Err(BladeError::Usage(
                        "wait js needs an expression — `act wait js \"window.ready\"` (or --js \"…\")".into(),
                    ));
                }
            } else if cond != "settle" && j.get("text").is_none() {
                return Err(BladeError::Usage(format!(
                    "wait {cond} needs a match value — `act wait {cond} --text \"…\"`"
                )));
            }
        }
        "eval" => {
            if j.get("js").is_none() && !pos.is_empty() {
                let joined = pos.join(" ");
                j["js"] = json!(resolve_payload(&joined)?);
            } else if let Some(v) = j.get("js").and_then(|x| x.as_str()).map(String::from) {
                j["js"] = json!(resolve_payload(&v)?);
            }
            if j.get("js").is_none() {
                return Err(BladeError::Usage(
                    "eval needs JS — `act eval \"document.title\"` (or @script.js / - for stdin)"
                        .into(),
                ));
            }
        }
        "collect" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if j.get("url").is_none() {
                return Err(BladeError::Usage(
                    "collect needs a URL — `act collect <url> [--max N]`".into(),
                ));
            }
        }
        "extract" => {
            // W1: mirrors `see extract <type>` — the type is the first
            // positional (default auto); json needs --template. Output shape:
            // --format json is accepted like see's.
            if j.get("extract").is_none() && !pos.is_empty() {
                let t = pos[0].clone();
                if !matches!(t.as_str(), "auto" | "links" | "forms" | "json") {
                    return Err(BladeError::Usage(format!(
                        "extract type must be auto|links|forms|json, got '{t}'"
                    )));
                }
                j["extract"] = json!(t);
            }
            if j.get("extract").is_none() {
                j["extract"] = json!("auto");
            }
            if j["extract"].as_str() == Some("json") && j.get("template").is_none() {
                return Err(BladeError::Usage(
                    "extract json needs --template '<json>' (or use `extract auto`)".into(),
                ));
            }
        }
        "pdf" => {
            if !pos.is_empty() {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}' for pdf — use --path/--landscape",
                    pos[0]
                )));
            }
        }
        "back" | "forward" | "reload" => {
            if !pos.is_empty() {
                return Err(BladeError::Usage(format!(
                    "{action} takes no arguments, got '{}'",
                    pos[0]
                )));
            }
        }
        "save" | "load" => {
            if j.get("name").is_none() && !pos.is_empty() {
                j["name"] = json!(pos[0].clone());
            }
            if j.get("name").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a name — `act {action} my-session`"
                )));
            }
        }
        "batch" => {
            let raw = if let Some(s) = j.get("steps").and_then(|x| x.as_str()).map(String::from) {
                s
            } else if let Some(first) = pos.first() {
                first.clone()
            } else {
                return Err(BladeError::Usage(
                    "batch needs steps — bladebro act batch '[{\"action\":\"click\",\"ref\":\"e5\"}]' \
                     (or @steps.json / - for stdin)"
                        .into(),
                ));
            };
            let raw = resolve_payload(&raw)?;
            let steps: Value = serde_json::from_str(&raw)
                .map_err(|e| BladeError::Usage(format!("invalid steps JSON: {e}")))?;
            let arr = steps
                .as_array()
                .ok_or_else(|| BladeError::Usage("steps must be a JSON array".into()))?;
            if arr.is_empty() {
                return Err(BladeError::Usage("batch needs at least one step".into()));
            }
            j["steps"] = steps;
        }
        "open-tab" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if pos.len() > 1 {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}'",
                    pos[1]
                )));
            }
        }
        "close-tab" | "switch-tab" => {
            if j.get("target_id").is_none() && !pos.is_empty() {
                j["target_id"] = json!(pos[0].clone());
            }
            if j.get("target_id").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a tab id — `act {action} <id>` (ids come from `bladebro state tabs`)"
                )));
            }
        }
        _ => unreachable!("action validated above"),
    }

    Ok(j)
}
