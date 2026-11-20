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
                let (done, reason) = classify_next_error(pages.len(), &e.to_string());
                complete = done;
                stop = reason;
                break;
            }
        }
    }
    let payload = json!({"pages":pages,"count":pages.len(),"complete":complete,"stop":stop});
    let output = serde_json::to_string(&payload)?;
    if output.len() > 12000 {
        let path = crate::artifacts::write_artifact(&output, "json")?;
        return Ok(json!({"artifact":path,"bytes":output.len(),"next_offset":0,"pages":pages.len(),"complete":complete,"stop":stop}).to_string());
    }
    Ok(output)
}

/// Classify why advancing to the next page failed.
///
/// Only treat a missing/disabled next control as `complete = true` when
/// pagination actually succeeded at least once (`pages_collected > 1`). If
/// `next` never matched even on page 1, a broken selector is indistinguishable
/// from a single-page result, so `complete` stays `false` and the stop reason
/// states both possibilities instead of claiming the dataset is complete.
fn classify_next_error(pages_collected: usize, message: &str) -> (bool, String) {
    let summary = message
        .split("\n\n--- current page state ---")
        .next()
        .unwrap_or(message)
        .trim();
    let is_ambiguous_or_invalid = summary.contains("ambiguous")
        || (summary.contains("matches ") && summary.contains("visible elements"))
        || summary.contains("SyntaxError")
        || summary.contains("not a valid selector");
    if is_ambiguous_or_invalid {
        return (false, format!("next unavailable: {summary}"));
    }
    let next_absent = summary.contains("no element matching")
        || summary.contains("no actionable element matches selector")
        || summary.contains("but none is visible")
        || summary.contains("is disabled")
        || summary.contains("gone from the current page");
    if next_absent {
        if pages_collected > 1 {
            return (
                true,
                format!(
                    "reached last page (page {pages_collected}): next control is no longer available"
                ),
            );
        }
        return (
            false,
            format!(
                "next control never matched on page 1 — single-page result or wrong next target ({summary})"
            ),
        );
    }
    (false, format!("next unavailable: {summary}"))
}

#[cfg(test)]
mod tests {
    use super::classify_next_error;

    #[test]
    fn real_pagination_end_with_suffix_fallback_diag_is_complete() {
        let diag = "no actionable element matches selector \"li.next a\" (searched light DOM + open shadow roots) - closest live matches for \"a\" (75 found): link \"Books\" - adjust the selector (a tag or class in the path may have changed)";
        let (complete, stop) = classify_next_error(2, diag);
        assert!(
            complete,
            "page 2 end must be complete=true, got stop={stop}"
        );
        assert!(
            stop.contains("reached last page (page 2)") && !stop.contains("adjust the selector"),
            "stop should cleanly report last page without blaming the selector: {stop}"
        );
    }

    #[test]
    fn broken_selector_on_page_one_is_never_complete() {
        let diag = "no actionable element matches selector \".definitely-not-real\" (searched light DOM + open shadow roots) - 0 raw matches, 0 actionable: the selector matched nothing - check its spelling against the live page";
        let (complete, stop) = classify_next_error(1, diag);
        assert!(
            !complete,
            "page 1 never-matched selector must be complete=false"
        );
        assert!(
            stop.contains("never matched on page 1"),
            "stop should explain page-1 miss: {stop}"
        );
    }

    #[test]
    fn text_next_end_after_multiple_pages_is_complete() {
        let diag =
            "no element matching \"Next\" found\n\n--- current page state ---\nPage: /pages/3";
        let (complete, stop) = classify_next_error(3, diag);
        assert!(complete);
        assert!(stop.contains("page 3") && !stop.contains("current page state"));
    }

    #[test]
    fn ambiguous_or_invalid_selector_is_never_complete() {
        let amb = "selector \"a.next\" matches 2 visible elements - add nth=N";
        assert!(!classify_next_error(2, amb).0);
        let bad = "SyntaxError: '[bad' is not a valid selector";
        assert!(!classify_next_error(2, bad).0);
    }
}
