//! Bounded pagination composed from the ordinary extraction and click paths.
use crate::{
    error::{BladeError, Result},
    page::Page,
};
use serde_json::{json, Value};

pub(super) async fn handle_paginate(args: &Value, page: &mut Page) -> Result<String> {
    let next = args
        .get("next")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            BladeError::Usage(
                "paginate requires next=selector or next=text=Next (also label=/ref=)".into(),
            )
        })?;
    let max = args.get("max").and_then(Value::as_u64).unwrap_or(5);
    if !(1..=100).contains(&max) {
        return Err(BladeError::Usage("paginate max must be 1-100 pages".into()));
    }
    let (field, target) = next
        .split_once('=')
        .filter(|(f, _)| matches!(*f, "text" | "label" | "selector" | "ref"))
        .unwrap_or(("selector", next));
    let mut pages = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut complete = false;
    let mut stop = "max pages reached; more pages may exist".to_string();
    for i in 0..max {
        let mut read = args.clone();
        read["extract"] = args.get("extract").cloned().unwrap_or(json!("auto"));
        read["format"] = json!("json");
        let result = super::see::handle_see(&read, page).await;
        let text = match result {
            Ok(text) => text,
            Err(BladeError::Closed) => return Err(BladeError::Closed),
            Err(e) if pages.is_empty() => return Err(e),
            Err(e) => {
                stop = format!("extract failed after advance: {e}; prior effects remain applied");
                break;
            }
        };
        let mut payload: Value = serde_json::from_str(&text)?;
        if let Some(path) = payload.get("artifact").and_then(Value::as_str) {
            payload = crate::artifacts::read_artifact_value(path)?;
        }
        let identity = format!("{}:{payload}", page.model().url());
        if !seen.insert(identity) {
            stop = "repeated page; stopped to prevent a cycle".into();
            break;
        }
        pages.push(json!({"url":page.model().url(),"data":payload}));
        if i + 1 == max {
            break;
        }
        let mut click = json!({"action":"click","slim":true});
        click[field] = json!(target);
        match Box::pin(super::act::handle_act(&click, page)).await {
            Ok(_) => {}
            Err(BladeError::Closed) => return Err(BladeError::Closed),
            Err(e) => {
                let message = e.to_string();
                complete = message.contains("no element matching")
                    || message.contains("0 raw matches, 0 actionable");
                stop = format!("next unavailable: {message}");
                break;
            }
        }
    }
    let payload = json!({"pages":pages,"count":pages.len(),"complete":complete,"stop":stop});
    let output = serde_json::to_string(&payload)?;
    if output.len() > 12000 {
        let path = crate::artifacts::write_artifact(&output, "json")?;
        return Ok(json!({"artifact":path,"bytes":output.len(),"next_offset":0,"pages":pages.len(),"complete":complete}).to_string());
    }
    Ok(output)
}
