//! Validate supplied fields before navigation or input can change the page.
//! Clients may skip schema validation; wrong types must never become defaults.

use serde_json::Value;

use crate::error::{BladeError, Result};

pub(super) fn fields(args: &Value, tool: &str) -> Result<()> {
    if !args.is_object() {
        return Err(BladeError::Usage(format!(
            "{tool} arguments must be an object"
        )));
    }
    let strings: &[&str] = match tool {
        "see" => &[
            "mode", "filter", "find", "extract", "logs", "scope", "artifact", "format", "sort",
        ],
        _ => &[
            "action",
            "ref",
            "text",
            "label",
            "selector",
            "role",
            "key",
            "press",
            "url",
            "js",
            "option",
            "expect",
            "message",
            "prompt_text",
            "submit",
            "condition",
            "format",
            "path",
            "block",
            "next",
            "sort",
        ],
    };
    for key in strings {
        if args.get(key).is_some_and(|v| !v.is_string()) {
            return Err(BladeError::Usage(format!("{tool} {key} must be a string")));
        }
    }
    for key in [
        "optional",
        "slim",
        "accept",
        "content",
        "landscape",
        "printBackground",
        "clear",
        "preview",
        "items",
        "changes",
        "body",
    ] {
        if args.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(BladeError::Usage(format!("{tool} {key} must be a boolean")));
        }
    }
    for key in ["nth", "budget", "limit", "offset", "max"] {
        if let Some(value) = args.get(key) {
            let valid = value.as_u64().is_some_and(|n| key != "nth" || n > 0);
            if !valid {
                return Err(BladeError::Usage(format!(
                    "{tool} {key} must be {} integer",
                    if key == "nth" {
                        "a positive (1-based)"
                    } else {
                        "a non-negative"
                    }
                )));
            }
        }
    }
    for key in ["dx", "dy"] {
        if args.get(key).is_some_and(|v| v.as_i64().is_none()) {
            return Err(BladeError::Usage(format!(
                "{tool} {key} must be an integer"
            )));
        }
    }
    for key in ["x", "y", "scale"] {
        if args
            .get(key)
            .is_some_and(|v| v.as_f64().is_none_or(|n| !n.is_finite()))
        {
            return Err(BladeError::Usage(format!(
                "{tool} {key} must be a finite number"
            )));
        }
    }
    if args.get("x").is_some() != args.get("y").is_some() {
        return Err(BladeError::Usage(
            "coordinate addressing requires both x and y".into(),
        ));
    }
    for key in ["steps", "then", "else"] {
        if args.get(key).is_some_and(|v| !v.is_array()) {
            return Err(BladeError::Usage(format!(
                "{tool} {key} must be an array of steps"
            )));
        }
    }
    Ok(())
}

pub(super) fn act(args: &Value) -> Result<()> {
    fields(args, "act")?;
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if !crate::action::ACT_ACTIONS.contains(&action)
        && !matches!(action, "state" | "cookies" | "set-cookie")
    {
        return Err(BladeError::Usage(crate::action::unknown_action_message(
            action,
        )));
    }
    if action == "paginate" {
        if args
            .get("next")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(BladeError::Usage(
                "paginate requires next=CSS or text=Next".into(),
            ));
        }
        if args
            .get("max")
            .and_then(Value::as_u64)
            .is_some_and(|n| !(1..=100).contains(&n))
        {
            return Err(BladeError::Usage("paginate max must be 1-100".into()));
        }
        let kind = args
            .get("extract")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        if !matches!(kind, "auto" | "json" | "links" | "forms") {
            return Err(BladeError::Usage(
                "paginate extract must be auto|json|links|forms".into(),
            ));
        }
        if kind == "json" && args.get("template").is_none_or(|v| !v.is_object()) {
            return Err(BladeError::Usage(
                "paginate extract=json requires template".into(),
            ));
        }
    }
    if action == "type" && args.get("text").is_none() {
        return Err(BladeError::Usage(
            "type requires text (use clear to empty a field, or text=\"\" explicitly)".into(),
        ));
    }
    if action == "press"
        && args
            .get("key")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err(BladeError::Usage(
            "press requires key (e.g. Enter, Tab, Control+a)".into(),
        ));
    }
    if action == "fill" {
        fill(args)?;
    }
    if args
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|s| !matches!(s, "" | "text" | "json"))
    {
        return Err(BladeError::Usage("format must be text or json".into()));
    }
    Ok(())
}

pub(super) fn fill(args: &Value) -> Result<()> {
    fields(args, "fill")?;
    let fields = args
        .get("fields")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| BladeError::Usage("fill requires a non-empty fields array".into()))?;
    for (i, field) in fields.iter().enumerate() {
        let prefix = format!("fill field {}", i + 1);
        self::fields(field, &prefix)?;
        if !["ref", "label", "selector"].iter().any(|k| {
            field
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        }) {
            return Err(BladeError::Usage(format!(
                "{prefix} requires ref, label or selector"
            )));
        }
        if field.get("check").is_some_and(|v| !v.is_boolean()) {
            return Err(BladeError::Usage(format!(
                "{prefix}: check must be a boolean"
            )));
        }
    }
    Ok(())
}

pub(super) fn see(args: &Value) -> Result<()> {
    fields(args, "see")?;
    if let Some(sort) = args.get("sort").and_then(Value::as_str) {
        if !matches!(
            sort,
            "confidence" | "top" | "new" | "controversial" | "old" | "qa"
        ) {
            return Err(BladeError::Usage(
                "sort must be confidence|top|new|controversial|old|qa".into(),
            ));
        }
        if args.get("extract").and_then(Value::as_str) != Some("auto") {
            return Err(BladeError::Usage(
                "sort requires extract=auto on a Reddit post".into(),
            ));
        }
    }
    for (key, choices) in [
        ("mode", "model|content|outline"),
        ("extract", "auto|json|links|forms"),
        ("logs", "console|network"),
        ("format", "text|json"),
    ] {
        if let Some(value) = args
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            // Older CLI parsers use this internal sentinel alongside the
            // real extract field. Preserve that path without accepting typos.
            if key == "mode"
                && value == "extract"
                && args
                    .get("extract")
                    .and_then(Value::as_str)
                    .is_some_and(|s| matches!(s, "auto" | "json" | "links" | "forms"))
            {
                continue;
            }
            if !choices.split('|').any(|c| c == value) {
                return Err(BladeError::Usage(format!(
                    "unknown {key}: {value} (use {choices})"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invalid_input_cannot_silently_become_a_default() {
        for args in [
            json!({"action":"type","label":"Name","text":["oops"]}),
            json!({"action":"type","label":"Name"}),
            json!({"action":"click","selector":true}),
            json!({"action":"click","nth":0}),
            json!({"action":"click","x":1}),
            json!({"action":"click","x":"1","y":2}),
            json!({"action":"press","key":""}),
            json!({"action":"clik","url":"https://example.com"}),
            json!({"action":"fill","fields":[{"label":"Name","text":"good"},{"label":"Later","text":false}]}),
            json!({"action":"fill","fields":[]}),
            json!({"action":"fill","submit":true,"fields":[{"label":"Name","text":"good"}]}),
        ] {
            assert!(act(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn explicit_empty_text_and_coordinate_floats_remain_supported() {
        assert!(act(&json!({"action":"type","ref":"e1","text":""})).is_ok());
        assert!(act(&json!({"action":"click","x":1.5,"y":2})).is_ok());
        assert!(act(&json!({"action":"fill","fields":[{"label":"Agree","check":false}]})).is_ok());
        assert!(see(&json!({"mode":"content","budget":10})).is_ok());
        assert!(see(&json!({"mode":"extract","extract":"auto"})).is_ok());
        assert!(see(&json!({"mode":"extract"})).is_err());
        for args in [
            json!({"mode":"typo"}),
            json!({"mode":true}),
            json!({"budget":-1}),
            json!({"extract":"typo"}),
        ] {
            assert!(see(&args).is_err(), "{args}");
        }
    }
}
