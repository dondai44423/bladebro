//! The `vision` command parser — `[--marks]`.

use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Parse `vision` args: [--marks].
pub(crate) fn parse_vision_args(args: &[String]) -> Result<Value> {
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
