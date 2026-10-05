//! The `nav` command parser — `<url> [--block <classes>]`.

use super::take_value;
use crate::error::{BladeError, Result};
use serde_json::{json, Value};

/// Parse `nav` args: <url> [--block <classes>]
pub(crate) fn parse_nav_args(args: &[String]) -> Result<Value> {
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
