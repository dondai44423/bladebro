//! DOM fallback: bounded in-page scroll collection when rapid replay is
//! rejected by the page's per-request transaction gate.

use super::parse::cut_chars;
use super::*;

/// DOM fallback: collect the rendered timeline in ONE in-page pass (a
/// bounded scroll loop). Used when rapid replay is rejected — X gates some
/// ops (search) behind per-request `x-client-transaction-id` values that
/// are single-use, so the page's own rendering is the reliable source.
/// Slower than a replay, still one tool call.
pub(super) async fn dom_collect(page: &Page, kind: &str, cap: usize) -> Result<XPayload> {
    let script = dom_collect_expr(cap);
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": script,
                "returnByValue": true,
                "awaitPromise": true,
            })),
        )
        .await?;
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("dom collect failed");
        return Err(BladeError::Other(format!(
            "x: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }
    let raw = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    let j: Value = serde_json::from_str(raw)
        .map_err(|e| BladeError::Other(format!("x: dom collect parse ({e})")))?;
    let hit_step_cap = j
        .get("hit_step_cap")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let (items, exhausted) = dom_items_from_json(&j);
    let complete = exhausted && !hit_step_cap && !items.is_empty() && items.len() < cap;
    let note = if items.is_empty() {
        "x: no tweets found in rendered DOM (login wall or empty page)".into()
    } else if complete {
        "x: collected from the rendered page (rapid replay unavailable for this page)".into()
    } else if hit_step_cap {
        format!(
            "x: collected from the rendered page — scroll step limit reached after {} items",
            items.len()
        )
    } else {
        format!(
            "x: collected from the rendered page — capped at {} items",
            items.len()
        )
    };
    Ok(XPayload {
        container: if kind == "status" {
            "x-thread".into()
        } else {
            "x-timeline".into()
        },
        kind: kind.to_string(),
        post: None,
        count: items.len(),
        total: None,
        complete,
        note: Some(note),
        items,
    })
}

pub(super) fn dom_collect_expr(cap: usize) -> String {
    include_str!("js/dom_collect.js").replace("__CAP__", &cap.to_string())
}

/// X's transient "Something went wrong. Try reloading." state — a React
/// boot failure, not a block; a reload fixes it. Only fires on pages whose
/// whole text is tiny (an error screen), so real content never matches.
pub(super) async fn is_flaky_render(page: &Page) -> bool {
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": "(function(){var t=(document.body&&document.body.innerText)||'';return t.length<600&&/something (went|goes) wrong|try reloading/i.test(t);})()",
                "returnByValue": true,
            })),
        )
        .await;
    matches!(
        res,
        Ok(r) if r.pointer("/result/value").and_then(|v| v.as_bool()) == Some(true)
    )
}

/// Shape DOM-collected rows into payload items (aria-label counts, ISO dates).
pub(super) fn dom_items_from_json(j: &Value) -> (Vec<XItem>, bool) {
    let exhausted = j
        .get("exhausted")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    let mut items = Vec::new();
    for it in j
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let id = it
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let count_of = |k: &str| -> Option<i64> {
            let s = it.get(k).and_then(|v| v.as_str()).unwrap_or("");
            crate::adapters::parse_compact_count(s)
        };
        let media = it
            .get("media")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        items.push(XItem {
            op: false,
            depth: None,
            reply_to: None,
            id,
            author: it
                .get("author")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: it
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            date: iso_to_utc(it.get("date").and_then(|v| v.as_str()).unwrap_or("")),
            text: cut_chars(
                it.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                ITEM_TEXT_CAP,
            ),
            quoted: None,
            replies: count_of("replies"),
            reposts: count_of("reposts"),
            likes: count_of("likes"),
            media,
            url: it
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    (items, exhausted)
}

/// "2026-09-24T16:18:18.000Z" → "2026-09-24 16:18 UTC".
fn iso_to_utc(s: &str) -> String {
    if s.len() >= 16 && s.as_bytes().get(10) == Some(&b'T') {
        return format!("{} {} UTC", &s[..10], &s[11..16]);
    }
    s.to_string()
}
