//! CLI argument parsing — the `bladebro <cmd> ...` surface.
//!
//! One parser per command (`nav` / `see` / `act` / `state` / `run` / `vision`)
//! plus the shared `--host`/`--port` endpoint extraction. All of them are loud
//! on unknown flags and missing values (exit 2 — the agent-native CLI
//! contract) and all return the exact JSON args object the shared MCP
//! handlers expect.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};

// ── Arg Parsers ────────────────────────────────────────────────────────

/// Extract `--host`/`--port` into an external endpoint (`host:port`),
/// removing them from the arg list. Returns `None` when no `--port` is given
/// (fall back to the daemon / a freshly launched Chrome).
///
/// This is issue #16's fix: these flags were parsed by main.rs but never
/// forwarded to the CLI, so `state --port 9222` silently ignored the port.
/// Position-independent — works regardless of whether the flags precede or
/// follow the command.
pub(super) fn extract_endpoint(args: &[String]) -> Result<(Vec<String>, Option<String>)> {
    let mut host = String::from("127.0.0.1");
    let mut port: Option<u16> = None;
    let mut cleaned: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    BladeError::Usage("--host needs a value (e.g. --host 127.0.0.1)".into())
                })?;
                host = v.clone();
                i += 2;
                continue;
            }
            "--port" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    BladeError::Usage("--port needs a value (e.g. --port 9222)".into())
                })?;
                let p: u16 = v
                    .parse()
                    .map_err(|_| BladeError::Usage(format!("--port needs a number, got '{v}'")))?;
                if p == 0 {
                    return Err(BladeError::Usage(
                        "--port 0 is not a usable debug port".into(),
                    ));
                }
                port = Some(p);
                i += 2;
                continue;
            }
            _ => {}
        }
        cleaned.push(args[i].clone());
        i += 1;
    }
    let external = port.map(|p| format!("{host}:{p}"));
    Ok((cleaned, external))
}

// ── Arg Parsing ────────────────────────────────────────────────────────
//
// Rules (v3.9.7, agent-native CLI):
// - Known flags are accepted anywhere; `--flag value` always takes the next
//   token (a missing value is a loud usage error, never a silent default).
// - Unknown flags are hard errors pointing at `help` — the old parsers
//   dropped them silently, so `see --budjet 5` read the wrong thing.
// - Positionals fill the fields the action needs, MCP-style: target first,
//   then value. Unquoted multi-word values join ("click Sign in" works).
// - `@file` reads the value from a file, `-` reads stdin — no shell-quoting
//   games for big JSON payloads.

/// The next token as a flag value; loud error when missing.
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| BladeError::Usage(format!("--{flag} needs a value (e.g. --{flag} <value>)")))
}

/// The next token parsed as a number; loud error on both missing and bad.
fn take_num<T: std::str::FromStr>(args: &[String], i: &mut usize, flag: &str) -> Result<T> {
    let raw = take_value(args, i, flag)?;
    raw.parse::<T>()
        .map_err(|_| BladeError::Usage(format!("--{flag} needs a number, got '{raw}'")))
}

/// Ref ids are `e` followed by digits (e1, e5, e12) — 'Edit'/'Enter' are text.
fn is_ref(s: &str) -> bool {
    s.len() > 1 && s.as_bytes()[0] == b'e' && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Resolve a payload argument: inline value, `@file`, or `-` (stdin).
fn resolve_payload(arg: &str) -> Result<String> {
    if arg == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| BladeError::Usage(format!("cannot read stdin: {e}")))?;
        if buf.trim().is_empty() {
            return Err(BladeError::Usage(
                "stdin was empty — pipe the payload in, e.g. `cat steps.json | bladebro run -`"
                    .into(),
            ));
        }
        return Ok(buf);
    }
    if let Some(path) = arg.strip_prefix('@') {
        return std::fs::read_to_string(path)
            .map_err(|e| BladeError::Usage(format!("cannot read {arg}: {e}")));
    }
    Ok(arg.to_string())
}

/// `fill` accepts several shapes — normalize to the array the MCP handler
/// requires:
/// - [{"ref":"e3","text":"John"}]   array form, as-is
/// - {"e3":"John","e5":"Doe"}        flat ref map
/// - {"label":"Email","text":"x"}    ONE field spec (any reserved key)
///   The third shape used to be misread as a ref-map entry named "label"
///   and failed with "stale ref: label" (caught live).
fn normalize_fields(parsed: Value) -> Result<Value> {
    match parsed {
        Value::Array(_) => Ok(parsed),
        Value::Object(map) => {
            const RESERVED: &[&str] = &["ref", "label", "text", "option", "check"];
            if map.keys().any(|k| RESERVED.contains(&k.as_str())) {
                return Ok(Value::Array(vec![Value::Object(map)]));
            }
            Ok(Value::Array(
                map.into_iter()
                    .map(|(k, v)| json!({ "ref": k, "text": v }))
                    .collect(),
            ))
        }
        _ => Err(BladeError::Usage(
            "fields must be a JSON object {\"e3\":\"John\"} (ref map), one spec {\"label\":\"Email\",\"text\":\"x\"}, or an array [{\"ref\":\"e3\",\"text\":\"John\"}]".into(),
        )),
    }
}

/// Token that looks like a URL/host: has a scheme, a port, or a dot.
fn looks_like_url(s: &str) -> bool {
    s.contains("://")
        || s.starts_with("data:")
        || s.starts_with("file:")
        || s.starts_with("about:")
        || s.contains("localhost")
        || s.contains('.')
}

/// Parse `nav` args: <url> [--block <classes>]
pub(super) fn parse_nav_args(args: &[String]) -> Result<Value> {
    let mut url: Option<String> = None;
    let mut block: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "url" => url = Some(take_value(args, &mut i, "url")?),
                "block" => block = Some(take_value(args, &mut i, "block")?),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for nav — see 'bladebro help nav'"
                    )))
                }
            }
        } else if url.is_none() {
            url = Some(a);
        } else {
            return Err(BladeError::Usage(format!(
                "unexpected extra argument '{a}' — nav takes exactly one URL"
            )));
        }
        i += 1;
    }
    let url =
        url.ok_or_else(|| BladeError::Usage("nav needs a URL — bladebro nav <url>".into()))?;
    let mut j = json!({ "action": "navigate", "url": url });
    if let Some(b) = block {
        j["block"] = json!(b);
    }
    Ok(j)
}

/// Parse `see` args: [mode] [url] [extract <type>] [flags].
pub(super) fn parse_see_args(args: &[String]) -> Result<Value> {
    let mut j = json!({});
    let mut url: Option<String> = None;
    let mut mode: Option<String> = None;
    let mut extract_pending = false;
    let mut extract_type: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        // Short aliases first: -f/-e/-b/-l/-t.
        let long = match a.as_str() {
            "-f" => "--filter",
            "-e" => "--extract",
            "-b" => "--budget",
            "-l" => "--limit",
            "-t" => "--template",
            other => other,
        };
        if let Some(flag) = long.strip_prefix("--") {
            match flag {
                "filter" | "find" | "logs" | "scope" | "artifact" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "budget" | "limit" | "offset" => {
                    let v: u64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "extract" => {
                    let v = take_value(args, &mut i, flag)?;
                    if !matches!(v.as_str(), "auto" | "links" | "forms" | "json") {
                        return Err(BladeError::Usage(format!(
                            "--extract must be auto|links|forms|json, got '{v}'"
                        )));
                    }
                    extract_type = Some(v);
                }
                "template" => {
                    let raw = resolve_payload(&take_value(args, &mut i, flag)?)?;
                    let tpl: Value = serde_json::from_str(&raw).map_err(|e| {
                        BladeError::Usage(format!("--template must be valid JSON: {e}"))
                    })?;
                    j["template"] = tpl;
                }
                "url" => url = Some(take_value(args, &mut i, flag)?),
                "content" => j["content"] = json!(true),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for see — see 'bladebro help see'"
                    )))
                }
            }
        } else if a == "extract" {
            extract_pending = true;
        } else if matches!(a.as_str(), "model" | "content" | "outline") && mode.is_none() {
            mode = Some(a);
        } else if extract_pending
            && extract_type.is_none()
            && matches!(a.as_str(), "auto" | "links" | "forms" | "json")
        {
            extract_type = Some(a);
        } else if looks_like_url(&a) {
            if url.is_some() {
                return Err(BladeError::Usage(format!(
                    "multiple URLs given ('{a}') — see takes at most one"
                )));
            }
            url = Some(a);
        } else {
            return Err(BladeError::Usage(format!(
                "unrecognized argument '{a}' — expected a mode (model|content|outline), \
                 'extract <auto|links|forms|json>', or a URL. See 'bladebro help see'"
            )));
        }
        i += 1;
    }

    if extract_pending && extract_type.is_none() {
        return Err(BladeError::Usage(
            "see extract needs a type — auto, links, forms, or json (json also needs --template)"
                .into(),
        ));
    }
    if let Some(t) = extract_type {
        j["extract"] = json!(t);
        if j.get("mode").is_none() {
            j["mode"] = json!("extract");
        }
    }
    if let Some(m) = mode {
        j["mode"] = json!(m);
    }
    if let Some(u) = url {
        j["url"] = json!(u);
    }
    Ok(j)
}

/// Parse `act` args. Universal `--field` flags mirror the MCP schema exactly
/// (--ref, --label, --text, --role, --nth, --key, --url, --option, …), so an
/// agent that knows the MCP tool can drive the CLI 1:1. Positionals are the
/// terse form: target first, value second.
pub(super) fn parse_act_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Usage(
            "act needs an action — click, type, fill, select, clear, press, scroll, hover, \
             navigate, upload, download, wait, eval, collect, read, batch, pdf, back, forward, \
             reload, save, load, open-tab, close-tab, switch-tab (see 'bladebro help act')"
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
                | "fields" | "selector" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "target-id" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["target_id"] = json!(v);
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
            if j.get("ref").is_none() && j.get("selector").is_none() && !pos.is_empty() {
                j["ref"] = json!(pos[0].clone());
            }
            if j.get("ref").is_none() && j.get("selector").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a ref or --selector — `act {action} e5` (refs come from see model / nav)"
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
            if cond != "settle" && j.get("text").is_none() {
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

/// Parse `state` args: <op> [args] [flags]. Same op names as MCP (plus
/// rm-ss), and unknown ops/flags are loud errors.
pub(super) fn parse_state_args(args: &[String]) -> Result<Value> {
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

/// Parse `run` args: <json-steps> | @file | - (stdin) | --steps <json>.
pub(super) fn parse_run_args(args: &[String]) -> Result<Value> {
    let mut steps_raw: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "steps" => steps_raw = Some(take_value(args, &mut i, flag)?),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for run — see 'bladebro help run'"
                    )))
                }
            }
        } else if steps_raw.is_none() {
            steps_raw = Some(a);
        } else {
            return Err(BladeError::Usage(
                "unexpected extra argument — pass steps once, or use @file / - for big payloads"
                    .into(),
            ));
        }
        i += 1;
    }

    let raw = steps_raw.ok_or_else(|| {
        BladeError::Usage(
            "run needs a JSON steps array, e.g. bladebro run '[{\"action\":\"click\",\"ref\":\"e5\"}]' \
             (or @steps.json / - for stdin)"
                .into(),
        )
    })?;
    let raw = resolve_payload(&raw)?;
    let steps: Value = serde_json::from_str(&raw)
        .map_err(|e| BladeError::Usage(format!("invalid steps JSON: {e}")))?;
    let arr = steps
        .as_array()
        .ok_or_else(|| BladeError::Usage("steps must be a JSON array".into()))?;
    if arr.is_empty() {
        return Err(BladeError::Usage("steps must not be empty".into()));
    }
    Ok(json!({ "steps": steps }))
}

/// Parse `vision` args: [--marks].
pub(super) fn parse_vision_args(args: &[String]) -> Result<Value> {
    let mut marks = false;
    for a in args {
        match a.as_str() {
            "--marks" => marks = true,
            s if s.starts_with("--") => {
                return Err(BladeError::Usage(format!(
                    "unknown flag {s} for vision — see 'bladebro help vision'"
                )))
            }
            s => {
                return Err(BladeError::Usage(format!(
                    "unexpected argument '{s}' for vision — bladebro vision [--marks]"
                )))
            }
        }
    }
    Ok(json!({ "marks": marks }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn tmp_file(tag: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("bt-cli-{}-{}.tmp", tag, std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    // ── endpoint extraction (issue #16) ────────────────────────────────

    #[test]
    fn port_maps_to_default_host_endpoint() {
        let (cleaned, external) = extract_endpoint(&[
            "state".into(),
            "tabs".into(),
            "--port".into(),
            "9223".into(),
        ])
        .expect("valid endpoint");
        assert_eq!(cleaned, vec!["state".to_string(), "tabs".to_string()]);
        assert_eq!(external.as_deref(), Some("127.0.0.1:9223"));
    }

    #[test]
    fn explicit_host_combines_with_port() {
        let (cleaned, external) = extract_endpoint(&[
            "--host".into(),
            "192.168.1.50".into(),
            "see".into(),
            "content".into(),
            "--port".into(),
            "9222".into(),
        ])
        .expect("valid endpoint");
        assert_eq!(cleaned, vec!["see".to_string(), "content".to_string()]);
        assert_eq!(external.as_deref(), Some("192.168.1.50:9222"));
    }

    #[test]
    fn no_port_means_no_external_endpoint() {
        let (cleaned, external) = extract_endpoint(&[
            "state".into(),
            "cookies".into(),
            "--host".into(),
            "127.0.0.1".into(),
        ])
        .expect("host without port is still fine");
        assert_eq!(cleaned, vec!["state".to_string(), "cookies".to_string()]);
        assert!(external.is_none(), "host alone must not pin an endpoint");
    }

    #[test]
    fn flags_before_command_still_parse() {
        let (cleaned, external) = extract_endpoint(&[
            "--port".into(),
            "9333".into(),
            "vision".into(),
            "--marks".into(),
        ])
        .expect("valid endpoint");
        assert_eq!(cleaned, vec!["vision".to_string(), "--marks".to_string()]);
        assert_eq!(external.as_deref(), Some("127.0.0.1:9333"));
    }

    // ── act parsing ────────────────────────────────────────────────────

    #[test]
    fn act_click_by_ref_and_by_label() {
        let v = parse_act_args(&a(&["click", "e5"])).unwrap();
        assert_eq!(v["action"], "click");
        assert_eq!(v["ref"], "e5");

        let v = parse_act_args(&a(&["click", "Sign", "in"])).unwrap();
        assert_eq!(v["label"], "Sign in");
        assert!(v.get("ref").is_none());
    }

    #[test]
    fn act_accepts_selector_flag() {
        // --selector is an addressing mode for click/hover/type/select/
        // clear/read/upload/eval: it must survive parsing and suppress the
        // positional label fallback.
        let v = parse_act_args(&a(&["click", "--selector", "#menu li"])).unwrap();
        assert_eq!(v["selector"], "#menu li");
        let v = parse_act_args(&a(&["hover", "--selector", "button[aria-label=\"x\"]"])).unwrap();
        assert_eq!(v["selector"], "button[aria-label=\"x\"]");
        let v = parse_act_args(&a(&[
            "type",
            "--selector",
            "[role=textbox]",
            "--text",
            "hi",
        ]))
        .unwrap();
        assert_eq!(v["selector"], "[role=textbox]");
        assert_eq!(v["text"], "hi");
        let v = parse_act_args(&a(&["select", "--selector", "#pet", "cat"])).unwrap();
        assert_eq!(v["selector"], "#pet");
        assert_eq!(v["option"], "cat");
        let v = parse_act_args(&a(&["read", "--selector", "#note"])).unwrap();
        assert_eq!(v["selector"], "#note");
        let v = parse_act_args(&a(&["clear", "--selector", "#note"])).unwrap();
        assert_eq!(v["selector"], "#note");
        let v = parse_act_args(&a(&[
            "upload",
            "--selector",
            "input[type=file]",
            "--path",
            "/tmp/f.pdf",
        ]))
        .unwrap();
        assert_eq!(v["selector"], "input[type=file]");
        assert_eq!(v["text"], "/tmp/f.pdf");
    }

    #[test]
    fn act_click_flags_win() {
        let v = parse_act_args(&a(&["click", "--ref", "e7", "--nth", "2"])).unwrap();
        assert_eq!(v["ref"], "e7");
        assert_eq!(v["nth"], 2);
    }

    #[test]
    fn act_type_joins_unquoted_text() {
        let v = parse_act_args(&a(&["type", "e12", "hello", "world"])).unwrap();
        assert_eq!(v["ref"], "e12");
        assert_eq!(v["text"], "hello world");

        let v = parse_act_args(&a(&["type", "--ref", "e5", "hi", "there"])).unwrap();
        assert_eq!(v["ref"], "e5");
        assert_eq!(v["text"], "hi there");
    }

    #[test]
    fn act_type_requires_text_and_target() {
        assert!(parse_act_args(&a(&["type", "e5"])).is_err());
        assert!(parse_act_args(&a(&["type"])).is_err());
    }

    #[test]
    fn act_fill_normalizes_object_fields() {
        let v = parse_act_args(&a(&["fill", "{\"e3\":\"John\",\"e5\":\"Doe\"}"])).unwrap();
        let fields = v["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields
            .iter()
            .any(|f| f["ref"] == "e3" && f["text"] == "John"));
    }

    #[test]
    fn act_fill_accepts_array_and_rejects_garbage() {
        let arr = "[{\"ref\":\"e1\",\"text\":\"x\"}]";
        let v = parse_act_args(&a(&["fill", arr])).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 1);
        assert!(parse_act_args(&a(&["fill", "not-json"])).is_err());
    }

    #[test]
    fn act_fill_object_with_reserved_keys_is_one_field_spec() {
        // {"label":"Email","text":"x"} means ONE field addressed by
        // label — it used to be misread as a ref-map entry named "label"
        // and failed live with "stale ref: label".
        let v = parse_act_args(&a(&["fill", "{\"label\":\"Email\",\"text\":\"x\"}"])).unwrap();
        let fields = v["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["label"], "Email");
        assert_eq!(fields[0]["text"], "x");

        let v = parse_act_args(&a(&["fill", "{\"ref\":\"e3\",\"text\":\"y\"}"])).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 1);
        assert_eq!(v["fields"][0]["ref"], "e3");
    }

    #[test]
    fn act_fill_from_file() {
        let path = tmp_file("fill", "{\"e9\":\"from-file\"}");
        let arg = format!("@{}", path.display());
        let v = parse_act_args(&a(&["fill", &arg])).unwrap();
        assert_eq!(v["fields"][0]["text"], "from-file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn act_upload_sends_path_in_text() {
        // Regression: the MCP handler reads the file path from `text`; the
        // old CLI put it in `path`, so every CLI upload arrived empty.
        let v = parse_act_args(&a(&["upload", "e5", "/tmp/f.pdf"])).unwrap();
        assert_eq!(v["text"], "/tmp/f.pdf");
        assert!(v.get("path").is_none());

        let v = parse_act_args(&a(&["upload", "--ref", "e5", "--path", "/tmp/g.png"])).unwrap();
        assert_eq!(v["text"], "/tmp/g.png");
    }

    #[test]
    fn act_navigate_requires_exactly_one_url() {
        let v = parse_act_args(&a(&["navigate", "example.com", "--block", "images"])).unwrap();
        assert_eq!(v["url"], "example.com");
        assert_eq!(v["block"], "images");
        assert!(parse_act_args(&a(&["navigate"])).is_err());
        assert!(parse_act_args(&a(&["navigate", "a.com", "b.com"])).is_err());
    }

    #[test]
    fn act_wait_validates_condition_and_text() {
        let v = parse_act_args(&a(&["wait", "js", "document.title"])).unwrap();
        assert_eq!(v["condition"], "js");
        assert_eq!(v["text"], "document.title");

        let v = parse_act_args(&a(&["wait", "settle"])).unwrap();
        assert_eq!(v["condition"], "settle");

        assert!(parse_act_args(&a(&["wait", "text"])).is_err());
        assert!(parse_act_args(&a(&["wait", "bogus", "x"])).is_err());
    }

    #[test]
    fn act_scroll_numbers_and_negatives() {
        let v = parse_act_args(&a(&["scroll", "0", "-500"])).unwrap();
        assert_eq!(v["dx"], 0);
        assert_eq!(v["dy"], -500);
        assert!(parse_act_args(&a(&["scroll", "down"])).is_err());
        assert!(parse_act_args(&a(&["scroll"])).is_err());
    }

    #[test]
    fn act_batch_parses_steps_and_rejects_non_array() {
        let v = parse_act_args(&a(&["batch", "[{\"action\":\"reload\"}]"])).unwrap();
        assert_eq!(v["steps"][0]["action"], "reload");
        assert!(parse_act_args(&a(&["batch", "{}"])).is_err());
        assert!(parse_act_args(&a(&["batch", "[]"])).is_err());
        assert!(parse_act_args(&a(&["batch"])).is_err());
    }

    #[test]
    fn act_tabs_save_load_and_read() {
        let v = parse_act_args(&a(&["open-tab", "https://x.com"])).unwrap();
        assert_eq!(v["url"], "https://x.com");
        let v = parse_act_args(&a(&["switch-tab", "ABC123"])).unwrap();
        assert_eq!(v["target_id"], "ABC123");
        let v = parse_act_args(&a(&["read", "e5"])).unwrap();
        assert_eq!(v["ref"], "e5");
        let v = parse_act_args(&a(&["save", "me"])).unwrap();
        assert_eq!(v["name"], "me");
        assert!(parse_act_args(&a(&["switch-tab"])).is_err());
    }

    #[test]
    fn act_rejects_unknown_flags_and_actions_loudly() {
        let e = parse_act_args(&a(&["click", "e5", "--bogus", "x"])).unwrap_err();
        assert!(e.to_string().contains("--bogus"));
        let e = parse_act_args(&a(&["clik", "e5"])).unwrap_err();
        assert!(e.to_string().contains("clik"));
    }

    #[test]
    fn act_flag_value_missing_is_loud() {
        assert!(parse_act_args(&a(&["click", "--ref"])).is_err());
        assert!(parse_act_args(&a(&["click", "e5", "--nth", "abc"])).is_err());
    }

    #[test]
    fn act_eval_joins_and_reads_files() {
        let v = parse_act_args(&a(&["eval", "1", "+", "2"])).unwrap();
        assert_eq!(v["js"], "1 + 2");

        let path = tmp_file("eval", "6*7");
        let arg = format!("@{}", path.display());
        let v = parse_act_args(&a(&["eval", &arg])).unwrap();
        assert_eq!(v["js"], "6*7");
        let _ = std::fs::remove_file(&path);
    }

    // ── see parsing ────────────────────────────────────────────────────

    #[test]
    fn see_bare_domain_is_a_url_not_a_mode() {
        let v = parse_see_args(&a(&["example.com"])).unwrap();
        assert_eq!(v["url"], "example.com");
        assert!(v.get("mode").is_none());
    }

    #[test]
    fn see_mode_and_url_in_any_order() {
        let v = parse_see_args(&a(&["content", "example.com"])).unwrap();
        assert_eq!(v["mode"], "content");
        assert_eq!(v["url"], "example.com");
        let v = parse_see_args(&a(&["https://x.com", "outline"])).unwrap();
        assert_eq!(v["mode"], "outline");
        assert_eq!(v["url"], "https://x.com");
    }

    #[test]
    fn see_extract_type_attaches() {
        let v = parse_see_args(&a(&["extract", "auto"])).unwrap();
        assert_eq!(v["extract"], "auto");
        let v = parse_see_args(&a(&["--extract", "links", "--limit", "5"])).unwrap();
        assert_eq!(v["extract"], "links");
        assert_eq!(v["limit"], 5);
    }

    #[test]
    fn see_artifact_readback_flags_parse() {
        let v = parse_see_args(&a(&[
            "--artifact",
            "/x/y.json",
            "--offset",
            "100",
            "--limit",
            "500",
        ]))
        .unwrap();
        assert_eq!(v["artifact"], "/x/y.json");
        assert_eq!(v["offset"], 100);
        assert_eq!(v["limit"], 500);
    }

    #[test]
    fn see_extract_without_type_errors() {
        assert!(parse_see_args(&a(&["extract"])).is_err());
        assert!(parse_see_args(&a(&["--extract", "bogus"])).is_err());
    }

    #[test]
    fn see_rejects_unknown_tokens_and_bad_numbers() {
        assert!(parse_see_args(&a(&["bogus"])).is_err());
        assert!(parse_see_args(&a(&["--budget", "abc"])).is_err());
        assert!(parse_see_args(&a(&["--budjet", "5"])).is_err());
        assert!(parse_see_args(&a(&["--budget"])).is_err());
    }

    #[test]
    fn see_scope_content_and_template() {
        let v = parse_see_args(&a(&["--scope", "e9", "--content"])).unwrap();
        assert_eq!(v["scope"], "e9");
        assert_eq!(v["content"], true);
        let v = parse_see_args(&a(&["--template", "{\"items\":{}}"])).unwrap();
        assert!(v["template"].is_object());
        assert!(parse_see_args(&a(&["--template", "not-json"])).is_err());
    }

    // ── state parsing ──────────────────────────────────────────────────

    #[test]
    fn state_storage_ops_use_the_name_field() {
        // Regression: the handler reads `name` (its MCP schema field).
        // The old code emitted `key`, so every set-ls stored an EMPTY key —
        // live `state ls` showed "=dark".
        let v = parse_state_args(&a(&["rm-ss", "key1"])).unwrap();
        assert_eq!(v["op"], "rm-ss");
        assert_eq!(v["name"], "key1");

        let v = parse_state_args(&a(&["set-ls", "theme", "dark"])).unwrap();
        assert_eq!(v["name"], "theme");
        assert_eq!(v["value"], "dark");
        assert!(v.get("key").is_none());

        let v = parse_state_args(&a(&["set-ss", "--key", "k", "--value", "v"])).unwrap();
        assert_eq!(v["name"], "k");
        assert_eq!(v["value"], "v");
    }

    #[test]
    fn state_set_cookie_full_form() {
        let v = parse_state_args(&a(&[
            "set-cookie",
            "tok",
            "abc",
            "--domain",
            "example.com",
            "--secure",
            "--http-only",
            "--same-site",
            "Strict",
        ]))
        .unwrap();
        assert_eq!(v["name"], "tok");
        assert_eq!(v["value"], "abc");
        assert_eq!(v["domain"], "example.com");
        assert_eq!(v["secure"], true);
        assert_eq!(v["httpOnly"], true);
        assert_eq!(v["sameSite"], "Strict");
    }

    #[test]
    fn state_missing_values_are_loud() {
        assert!(parse_state_args(&a(&["set-cookie", "tok"])).is_err());
        assert!(parse_state_args(&a(&["set-ls", "k"])).is_err());
        assert!(parse_state_args(&a(&["rm-ls"])).is_err());
        assert!(parse_state_args(&a(&["close-tab"])).is_err());
    }

    #[test]
    fn state_cookies_optional_url_and_block_clear() {
        let v = parse_state_args(&a(&["cookies", "example.com"])).unwrap();
        assert_eq!(v["url"], "example.com");
        let v = parse_state_args(&a(&["block", "clear"])).unwrap();
        assert_eq!(v["clear"], true);
        let v = parse_state_args(&a(&["block", "images,fonts"])).unwrap();
        assert_eq!(v["classes"], "images,fonts");
    }

    #[test]
    fn state_compress_and_unknown_op() {
        let v = parse_state_args(&a(&["compress", "off"])).unwrap();
        assert_eq!(v["mode"], "off");
        assert!(parse_state_args(&a(&["compress", "sometimes"])).is_err());
        assert!(parse_state_args(&a(&["frobnicate"])).is_err());
    }

    // ── run parsing ────────────────────────────────────────────────────

    #[test]
    fn run_accepts_inline_file_and_validates() {
        let v = parse_run_args(&a(&["[{\"action\":\"reload\"}]"])).unwrap();
        assert_eq!(v["steps"][0]["action"], "reload");

        let path = tmp_file("run", "[{\"action\":\"reload\"}]");
        let arg = format!("@{}", path.display());
        let v = parse_run_args(&a(&[&arg])).unwrap();
        assert!(v["steps"].is_array());
        let _ = std::fs::remove_file(&path);

        assert!(parse_run_args(&a(&["{}"])).is_err());
        assert!(parse_run_args(&a(&["[]"])).is_err());
        assert!(parse_run_args(&a(&[])).is_err());
    }

    // ── nav / vision ───────────────────────────────────────────────────

    #[test]
    fn nav_requires_url_and_takes_block() {
        let v = parse_nav_args(&a(&["example.com", "--block", "images"])).unwrap();
        assert_eq!(v["action"], "navigate");
        assert_eq!(v["url"], "example.com");
        assert_eq!(v["block"], "images");
        assert!(parse_nav_args(&a(&[])).is_err());
        assert!(parse_nav_args(&a(&["a.com", "b.com"])).is_err());
    }

    #[test]
    fn vision_only_takes_marks() {
        assert_eq!(parse_vision_args(&a(&["--marks"])).unwrap()["marks"], true);
        assert_eq!(parse_vision_args(&a(&[])).unwrap()["marks"], false);
        assert!(parse_vision_args(&a(&["--bogus"])).is_err());
        assert!(parse_vision_args(&a(&["wat"])).is_err());
    }

    // ── payloads / help ────────────────────────────────────────────────

    #[test]
    fn resolve_payload_variants() {
        assert_eq!(resolve_payload("inline").unwrap(), "inline");
        let path = tmp_file("payload", "from file");
        let arg = format!("@{}", path.display());
        assert_eq!(resolve_payload(&arg).unwrap(), "from file");
        let _ = std::fs::remove_file(&path);
        assert!(resolve_payload("@/nonexistent/definitely-missing").is_err());
    }
}

#[cfg(test)]
mod endpoint_args_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn endpoint_flags_parse_and_strip() {
        let (cleaned, ext) =
            extract_endpoint(&args(&["see", "--port", "9222"])).expect("valid port");
        assert_eq!(cleaned, vec!["see"]);
        assert_eq!(ext.as_deref(), Some("127.0.0.1:9222"));
        let (cleaned, ext) =
            extract_endpoint(&args(&["--host", "10.0.0.5", "state", "--port", "7"]))
                .expect("valid");
        assert_eq!(cleaned, vec!["state"]);
        assert_eq!(ext.as_deref(), Some("10.0.0.5:7"));
        // No --port: no external endpoint, daemon flow untouched.
        let (cleaned, ext) = extract_endpoint(&args(&["nav", "example.com"])).expect("no endpoint");
        assert_eq!(cleaned, vec!["nav", "example.com"]);
        assert!(ext.is_none());
    }

    #[test]
    fn endpoint_missing_or_bad_values_are_usage_errors() {
        // Both used to be silently dropped and the call fell back to the
        // daemon — against the loud-flag-error contract.
        assert!(matches!(
            extract_endpoint(&args(&["see", "--port"])),
            Err(BladeError::Usage(_))
        ));
        assert!(matches!(
            extract_endpoint(&args(&["see", "--port", "abc"])),
            Err(BladeError::Usage(_))
        ));
        assert!(matches!(
            extract_endpoint(&args(&["see", "--port", "0"])),
            Err(BladeError::Usage(_))
        ));
        assert!(matches!(
            extract_endpoint(&args(&["see", "--host"])),
            Err(BladeError::Usage(_))
        ));
    }
}
