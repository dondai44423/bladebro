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
            "url",
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
            "extract",
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
    if args.get("template").is_some_and(|v| !v.is_object()) {
        return Err(BladeError::Usage(format!(
            "{tool} template must be an object"
        )));
    }
    if args.get("timeout").is_some() {
        super::run::resolve_wait_timeout(args, 10)?;
    }
    Ok(())
}

pub(super) fn act(args: &Value) -> Result<()> {
    act_fields(args)?;
    if args.get("action").and_then(Value::as_str) == Some("batch") {
        sequence(args, "steps", true, 0, &mut 0)?;
    }
    Ok(())
}

fn act_fields(args: &Value) -> Result<()> {
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
    for key in ["key", "press"] {
        if let Some(key) = args
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            crate::action::validate_key(key)?;
        }
    }
    match action {
        "wait" => {
            let (condition, _) = super::run::condition_from_args(args, "settle")?;
            if condition == "settle"
                && args
                    .get("else")
                    .and_then(Value::as_array)
                    .is_some_and(|s| !s.is_empty())
            {
                return Err(BladeError::Usage(
                    "wait else: 'else' needs a real condition; 'settle' always succeeds".into(),
                ));
            }
        }
        "eval" => require_nonempty(args, "js", "eval")?,
        "navigate" => require_nonempty(args, "url", "navigate")?,
        "upload" => require_nonempty(args, "text", "upload file path")?,
        "select" if args.get("option").is_none() && args.get("text").is_none() => {
            return Err(BladeError::Usage(
                "select requires option (text or value; empty string is allowed)".into(),
            ));
        }
        "extract" | "paginate" => {
            let mut read = args.clone();
            if read.get("extract").is_none() {
                read["extract"] = serde_json::json!("auto");
            }
            see(&read)?;
        }
        "state" | "open-tab" | "close-tab" | "switch-tab" | "save" | "load" | "cookies"
        | "set-cookie" => {
            let mut op = args.clone();
            if action != "state" && op.get("op").is_none() {
                op["op"] = serde_json::json!(action);
            }
            state(&op)?;
        }
        _ => {}
    }
    if action == "upload" {
        // The handler resolves only ref/selector to the file input; a label
        // used to pass here and fail after url= pre-navigation/earlier steps.
        if !["ref", "selector"].iter().any(|k| nonempty(args, k)) {
            return Err(BladeError::Usage(
                "upload requires ref or selector (the file input; label is not resolved for uploads)"
                    .into(),
            ));
        }
    } else if matches!(action, "type" | "clear" | "select" | "read") {
        if !["ref", "label", "selector"]
            .iter()
            .any(|k| nonempty(args, k))
        {
            return Err(BladeError::Usage(format!(
                "{action} requires ref, label or selector"
            )));
        }
    } else if matches!(action, "click" | "hover")
        && !["ref", "text", "label", "selector"]
            .iter()
            .any(|k| nonempty(args, k))
        && !(action == "click" && args.get("x").is_some())
    {
        return Err(BladeError::Usage(format!(
            "{action} requires ref, text, label or selector{}",
            if action == "click" {
                ", or x and y"
            } else {
                ""
            }
        )));
    }
    if action == "dialog"
        && args.get("expect").and_then(Value::as_str).is_some_and(|s| {
            !matches!(
                s,
                "" | "alert" | "confirm" | "prompt" | "beforeunload" | "any" | "clear"
            )
        })
    {
        return Err(BladeError::Usage(
            "dialog expect must be alert|confirm|prompt|beforeunload|any|clear".into(),
        ));
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
        if ["text", "option", "check"]
            .iter()
            .filter(|k| field.get(**k).is_some())
            .count()
            > 1
        {
            return Err(BladeError::Usage(format!(
                "{prefix}: supply only one of text, option or check"
            )));
        }
    }
    Ok(())
}

pub(super) fn see(args: &Value) -> Result<()> {
    fields(args, "see")?;
    if args.get("preview").and_then(Value::as_bool) == Some(true)
        && args.get("extract").and_then(Value::as_str) != Some("auto")
    {
        return Err(BladeError::Usage(
            "preview=true requires extract=auto".into(),
        ));
    }
    if args.get("items").and_then(Value::as_bool) == Some(true) && !nonempty(args, "artifact") {
        return Err(BladeError::Usage(
            "items=true requires artifact=path".into(),
        ));
    }
    if args.get("changes").and_then(Value::as_bool) == Some(true)
        && ["extract", "logs", "artifact", "scope", "find"]
            .iter()
            .any(|k| nonempty(args, k))
    {
        return Err(BladeError::Usage(
            "changes=true is a model delta read; omit extract/logs/artifact/scope/find".into(),
        ));
    }
    // handle_see answers mode/logs BEFORE extract and extract before
    // find/scope, so these pairs silently dropped one side (format=json then
    // came back as markdown/log text). One read per call; say which.
    if nonempty(args, "extract") {
        let mode = args.get("mode").and_then(Value::as_str).unwrap_or("");
        if matches!(mode, "model" | "content" | "outline") {
            return Err(BladeError::Usage(format!(
                "extract= and mode={mode} are separate reads; drop mode for the extract, or split into two see calls"
            )));
        }
        if let Some(key) = ["logs", "find", "scope"]
            .into_iter()
            .find(|k| nonempty(args, k))
        {
            return Err(BladeError::Usage(format!(
                "extract= and {key}= are separate reads and one would be ignored; split into two see calls"
            )));
        }
    }
    if args.get("format").and_then(Value::as_str) == Some("json")
        && !nonempty(args, "extract")
        && !nonempty(args, "artifact")
        && args.get("changes").and_then(Value::as_bool) != Some(true)
    {
        return Err(BladeError::Usage(
            "format=json applies to extract=, artifact= or changes=true; this mode returns text"
                .into(),
        ));
    }
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
    if args.get("extract").and_then(Value::as_str) == Some("json") {
        let template = args
            .get("template")
            .ok_or_else(|| BladeError::Usage("extract=json requires template".into()))?;
        super::extract::template_extract_expr(template, 1)?;
    }
    Ok(())
}

fn nonempty(args: &Value, key: &str) -> bool {
    args.get(key)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn require_nonempty(args: &Value, key: &str, context: &str) -> Result<()> {
    if nonempty(args, key) {
        Ok(())
    } else {
        Err(BladeError::Usage(format!("{context} requires {key}")))
    }
}

pub(super) fn state(args: &Value) -> Result<()> {
    fields(args, "state")?;
    for key in [
        "op",
        "name",
        "value",
        "domain",
        "path",
        "sameSite",
        "target_id",
        "mode",
        "classes",
    ] {
        if args.get(key).is_some_and(|v| !v.is_string()) {
            return Err(BladeError::Usage(format!("state {key} must be a string")));
        }
    }
    for key in ["secure", "httpOnly"] {
        if args.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(BladeError::Usage(format!("state {key} must be a boolean")));
        }
    }
    let op = args.get("op").and_then(Value::as_str).unwrap_or("");
    if !crate::mcp::tools::STATE_OPS.contains(&op) {
        return Err(BladeError::Usage(format!(
            "unknown state op: {op}; use {}",
            crate::mcp::tools::STATE_OPS.join("|")
        )));
    }
    if matches!(op, "set-cookie" | "del-cookie" | "save" | "load") {
        require_nonempty(args, "name", op)?;
    }
    if matches!(op, "set-ls" | "set-ss" | "rm-ls" | "rm-ss") && args.get("name").is_none() {
        return Err(BladeError::Usage(format!(
            "{op} requires name (empty key is allowed)"
        )));
    }
    if matches!(op, "set-cookie" | "set-ls" | "set-ss") && args.get("value").is_none() {
        return Err(BladeError::Usage(format!(
            "{op} requires value (empty string is allowed)"
        )));
    }
    if matches!(op, "close-tab" | "switch-tab") {
        require_nonempty(args, "target_id", op)?;
    }
    if args
        .get("sameSite")
        .and_then(Value::as_str)
        .is_some_and(|s| !matches!(s, "Strict" | "Lax" | "None"))
    {
        return Err(BladeError::Usage("sameSite must be Strict|Lax|None".into()));
    }
    if op == "compress"
        && args
            .get("mode")
            .or_else(|| args.get("value"))
            .and_then(Value::as_str)
            .is_some_and(|s| !matches!(s, "" | "on" | "off" | "status"))
    {
        return Err(BladeError::Usage(
            "compress mode must be on|off|status".into(),
        ));
    }
    Ok(())
}

pub(super) fn vision(args: &Value) -> Result<()> {
    fields(args, "vision")?;
    if args.get("marks").is_some_and(|v| !v.is_boolean()) {
        return Err(BladeError::Usage("vision marks must be a boolean".into()));
    }
    Ok(())
}

pub(super) fn run(args: &Value) -> Result<()> {
    fields(args, "run")?;
    if args
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|s| !matches!(s, "" | "text" | "json"))
    {
        return Err(BladeError::Usage("run format must be text or json".into()));
    }
    sequence(args, "steps", false, 0, &mut 0)
}

// Validate the complete static plan before its first effect, including branches
// that may not run. Bounds protect the async recursive executor's stack/work.
fn sequence(args: &Value, key: &str, batch: bool, depth: usize, count: &mut usize) -> Result<()> {
    if depth > 32 {
        return Err(BladeError::Usage("step nesting exceeds 32 levels".into()));
    }
    let steps = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| BladeError::Usage(format!("requires {key} array")))?;
    if batch && steps.is_empty() {
        return Err(BladeError::Usage("batch requires at least one step".into()));
    }
    for (i, step) in steps.iter().enumerate() {
        *count += 1;
        if *count > 1000 {
            return Err(BladeError::Usage("plan exceeds 1000 steps".into()));
        }
        let check = || -> Result<()> {
            fields(step, "step")?;
            let action = step.get("action").and_then(Value::as_str).unwrap_or("");
            match action {
                "if" | "while" if !batch => {
                    if action == "while" {
                        super::run::while_condition(step)?;
                    } else {
                        super::run::condition_from_args(step, "settle")?;
                    }
                    if action == "while"
                        && step
                            .get("max")
                            .and_then(Value::as_u64)
                            .is_some_and(|n| n > 100)
                    {
                        return Err(BladeError::Usage("while max must be 0-100".into()));
                    }
                    if action == "while" && step.get("steps").is_none() {
                        return Err(BladeError::Usage("while requires steps array".into()));
                    }
                    Ok(())
                }
                "see" => see(step),
                "js" if !batch => {
                    let mut eval = step.clone();
                    eval["action"] = serde_json::json!("eval");
                    if eval.get("js").is_none() {
                        eval["js"] = step.get("text").cloned().unwrap_or(Value::Null);
                    }
                    act_fields(&eval)
                }
                "batch" if batch => Err(BladeError::Usage(
                    "batch steps cannot nest batch; use run".into(),
                )),
                _ => act_fields(step),
            }
        };
        check().map_err(|e| BladeError::Usage(format!("step {}: {e}", i + 1)))?;
        for child in ["then", "else", "steps"] {
            if step.get(child).is_some() {
                sequence(
                    step,
                    child,
                    batch || step["action"] == "batch",
                    depth + 1,
                    count,
                )
                .map_err(|e| BladeError::Usage(format!("step {} {child}: {e}", i + 1)))?;
            }
        }
    }
    Ok(())
}

pub(super) fn tool_call(params: &Value) -> Result<()> {
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match params.get("name").and_then(Value::as_str).unwrap_or("") {
        "act" => act(&args),
        "see" => see(&args),
        "state" => state(&args),
        "run" => run(&args),
        "vision" => vision(&args),
        name => Err(BladeError::Usage(format!(
            "unknown tool: {name}; use act|see|state|run|vision"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_plans_validate_before_their_first_effect() {
        let effect = json!({"action":"eval","js":"window.count++"});
        for bad in [
            json!(null),
            json!({"action":"type","ref":"e1","text":7}),
            json!({"action":"wait","condition":"js"}),
            json!({"action":"if","condition":"js","js":"true","then":[{"action":"clik"}]}),
            json!({"action":"while","condition":"js","js":"true","max":101,"steps":[]}),
            json!({"action":"state","op":"set-ls","name":"key"}),
            json!({"action":"extract","extract":"json","template":{"items":{"container":"li","fields":[]}}}),
            json!({"action":"extract","extract":"json","template":{"items":{"container":"li","fields":{"name":{"sel":"","raw":"false"}}}}}),
        ] {
            assert!(run(&json!({"steps":[effect,bad]})).is_err(), "{bad}");
        }
        assert!(act(
            &json!({"action":"batch","steps":[effect,{"action":"batch","steps":[effect]}]})
        )
        .is_err());
        assert!(run(&json!({"steps":[{"action":"batch","steps":[effect]}]})).is_ok());
    }

    #[test]
    fn recursive_plans_and_iteration_counts_are_bounded() {
        let mut step = json!({"action":"eval","js":"1"});
        for _ in 0..34 {
            step = json!({"action":"if","js":"true","then":[step]});
        }
        assert!(run(&json!({"steps":[step]}))
            .unwrap_err()
            .to_string()
            .contains("nesting"));
        let steps = vec![json!({"action":"eval","js":"1"}); 1001];
        assert!(run(&json!({"steps":steps}))
            .unwrap_err()
            .to_string()
            .contains("1000"));
        assert!(
            run(&json!({"steps":[{"action":"while","js":"false","max":100,"steps":[]}]})).is_ok()
        );
    }

    #[test]
    fn supplied_timeouts_and_mutation_values_never_become_defaults() {
        for action in ["download", "collect", "dialog", "wait"] {
            for timeout in [
                json!(-1),
                json!(1.5),
                json!("10"),
                json!(3601),
                json!(u64::MAX),
            ] {
                assert!(act(&json!({"action":action,"timeout":timeout})).is_err());
            }
        }
        assert!(state(&json!({"op":"set-ls","name":"","value":""})).is_ok());
        assert!(
            state(&json!({"op":"set-cookie","name":"session","value":"","sameSite":"Lax"})).is_ok()
        );
        assert!(state(&json!({"op":"set-cookie","name":"session","value":false})).is_err());
        assert!(vision(&json!({"marks":"false"})).is_err());
        assert!(tool_call(&json!({"name":"unknown"})).is_err());
        assert!(tool_call(&json!({"name":"see","arguments":null})).is_err());
    }

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
            json!({"action":"type","ref":"e1","text":"query","press":"Control+"}),
            json!({"action":"clik","url":"https://example.com"}),
            json!({"action":"fill","fields":[{"label":"Name","text":"good"},{"label":"Later","text":false}]}),
            json!({"action":"fill","fields":[]}),
            json!({"action":"fill","submit":true,"fields":[{"label":"Name","text":"good"}]}),
        ] {
            assert!(act(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn while_without_an_exit_test_is_rejected_before_effects() {
        // No condition/text/js/selector used to default to an empty element
        // needle, which every visible element matches: the body ran `max`
        // times with no exit test after the earlier steps had already run.
        let effect = json!({"action":"eval","js":"window.count++"});
        let loop_ = json!({"action":"while","steps":[{"action":"click","text":"Load more"}]});
        let err = run(&json!({"steps":[effect, loop_]}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("step 2") && err.contains("while"), "{err}");
        let nested = json!({"action":"if","js":"true","then":[loop_]});
        assert!(run(&json!({"steps":[effect, nested]})).is_err());
        for ok in [
            json!({"action":"while","text":"Load more","steps":[]}),
            json!({"action":"while","condition":"element","text":"Load more","steps":[]}),
            json!({"action":"while","selector":".more","steps":[]}),
            json!({"action":"while","js":"window.more","steps":[]}),
            json!({"action":"while","condition":"settle","max":3,"steps":[]}),
        ] {
            assert!(run(&json!({"steps":[ok]})).is_ok(), "{ok}");
        }
    }

    #[test]
    fn upload_requires_the_addressing_its_handler_resolves() {
        // The upload handler resolves only ref/selector. label used to pass
        // validation and fail after url= pre-navigation and earlier steps.
        let upload = json!({"action":"upload","label":"Resume","text":"cv.pdf"});
        assert!(act(&upload).is_err());
        let err = run(&json!({"steps":[{"action":"eval","js":"window.count++"}, upload]}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ref or selector"), "{err}");
        assert!(
            act(&json!({"action":"upload","selector":"input[type=file]","text":"cv.pdf"})).is_ok()
        );
        assert!(act(&json!({"action":"upload","ref":"e3","text":"cv.pdf"})).is_ok());
    }

    #[test]
    fn extract_cannot_combine_with_a_read_that_replaces_or_drops_it() {
        // handle_see answers mode/logs before extract, and extract before
        // find/scope: format=json returned markdown, and find/scope vanished.
        for args in [
            json!({"mode":"content","extract":"auto","format":"json"}),
            json!({"mode":"outline","extract":"links","format":"json"}),
            json!({"mode":"model","extract":"auto"}),
            json!({"logs":"console","extract":"auto","format":"json"}),
            json!({"find":"Price","extract":"auto"}),
            json!({"scope":"e5","extract":"links"}),
        ] {
            assert!(see(&args).is_err(), "{args}");
        }
        let err = act(&json!({"action":"extract","find":"Price"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("find"), "{err}");
        for ok in [
            json!({"extract":"auto","format":"json"}),
            json!({"mode":"extract","extract":"auto"}),
            json!({"mode":"content","scope":"e5"}),
            json!({"logs":"network"}),
            json!({"find":"Price"}),
        ] {
            assert!(see(&ok).is_ok(), "{ok}");
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
