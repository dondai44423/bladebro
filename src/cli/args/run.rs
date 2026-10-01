//! The `run` command parser — `<json-steps> | @file | - (stdin) | `--steps <json>`.

use super::{resolve_payload, take_value};
use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Parse `run` args: <json-steps> | @file | - (stdin) | --steps <json>.
pub(crate) fn parse_run_args(args: &[String]) -> Result<Value> {
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
