//! The `see` command parser — `[mode] [url] [extract <type>] [flags]`.

use super::{looks_like_url, resolve_payload, take_num, take_value};
use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Parse `see` args: [mode] [url] [extract <type>] [flags].
pub(crate) fn parse_see_args(args: &[String]) -> Result<Value> {
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
