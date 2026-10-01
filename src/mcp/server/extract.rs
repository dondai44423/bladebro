//! Extraction + collection handlers: template, `auto`, and `collect`.
//!
//! `auto` is the deterministic structural extractor; its script carries the
//! site fast paths (Reddit/HN/GitHub/X/product pages) and a syntax error
//! disables it everywhere — hence the node --check test at the bottom. The
//! script text itself lives in `js/auto_extract.js` (`include_str!`, with
//! `__LIMIT__`/`__POST_MARKER__` substituted at call time).

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;

use super::artifact_hint;

/// V9: template extraction. The agent provides a declarative
/// template; the driver runs ONE query and returns structured
/// JSON. Zero LLM in the loop — the fastest extraction of any
/// agent browser.
///
/// Template shape:
/// ```json
/// {"items": {"container": "css", "fields": {"name": "css|css@attr"}}}
/// ```
/// Multiple top-level keys are allowed (multiple lists in one
/// call). A field value of "" reads the container element
/// itself. `@attr` reads an attribute; default is textContent.
pub async fn handle_template_extract(
    page: &mut Page,
    template: &Value,
    limit: usize,
) -> Result<String> {
    let obj = template
        .as_object()
        .ok_or_else(|| BladeError::Other("template must be a JSON object".into()))?;

    // Build ONE JS expression covering all lists.
    let mut list_builders = Vec::new();
    for (list_name, spec) in obj {
        let container = spec.get("container").and_then(|c| c.as_str()).unwrap_or("");
        if container.is_empty() {
            return Err(BladeError::Other(format!(
                "template list '{list_name}' needs a 'container' selector"
            )));
        }
        let fields = spec
            .get("fields")
            .and_then(|f| f.as_object())
            .cloned()
            .unwrap_or_default();
        let mut field_parts = Vec::new();
        for (fname, fsel) in &fields {
            let sel = fsel.as_str().unwrap_or("");
            field_parts.push(format!(
                "{}:read(c,{})",
                serde_json::to_string(fname)?,
                serde_json::to_string(sel)?
            ));
        }
        list_builders.push(format!(
            "{}:(()=>{{const cs=[...document.querySelectorAll({})].slice(0,{});return cs.map(c=>({{{}}}));}})()",
            serde_json::to_string(list_name)?,
            serde_json::to_string(container)?,
            limit,
            field_parts.join(","),
        ));
    }

    let expr = format!(
        "(()=>{{const read=(c,sel)=>{{let s=sel,attr=null;const ai=sel.lastIndexOf('@');if(ai>0){{attr=sel.slice(ai+1);s=sel.slice(0,ai);}}const el=s?c.querySelector(s):c;if(!el)return null;if(attr)return el.getAttribute(attr);return(el.innerText||el.textContent||'').trim();}};return {{{}}};}})()",
        list_builders.join(",")
    );

    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("template extraction failed");
        return Err(BladeError::Other(format!(
            "extract failed: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(json!({}));
    let json_str = serde_json::to_string_pretty(&value)?;

    // Count total items across lists.
    let total: usize = value
        .as_object()
        .map(|o| {
            o.values()
                .filter_map(|v| v.as_array().map(|a| a.len()))
                .sum()
        })
        .unwrap_or(0);

    if json_str.len() > 6000 {
        let path = crate::artifacts::write_artifact(&json_str, "json")?;
        let preview: String = json_str.chars().take(600).collect();
        return Ok(format!(
            "extract json ({total} items, {} bytes)\npreview: {preview}…\n{}",
            json_str.len(),
            artifact_hint(&path)
        ));
    }
    Ok(format!("extract json ({total} items):\n{json_str}"))
}
/// V21: Auto-extract — deterministic structural list extraction.
/// Finds the DOM container whose direct children are the most
/// structurally-repeated (the "main list"), extracts per-item fields by
/// content type, returns a JSON array. No template, no LLM.
fn auto_extract_expr(limit: usize, post_marker: bool) -> String {
    let lim = limit.min(500);
    include_str!("js/auto_extract.js")
        .replace("__LIMIT__", &lim.to_string())
        .replace(
            "__POST_MARKER__",
            if post_marker { "true" } else { "false" },
        )
}

/// Run auto-extract and return the parsed JSON value.
///
/// Feeds hydrate asynchronously — an extract fired during the stream can
/// find no list yet, or only the first few items. When the result is empty
/// OR tiny (<8 items) and requests are still in flight, settle briefly and
/// re-run (bounded: 2 retries). Quiet pages pay zero extra latency.
async fn run_auto_extract(
    page: &Page,
    limit: usize,
    post_marker: bool,
) -> Result<serde_json::Value> {
    // A client-side route change leaves the PREVIOUS route's DOM mounted while
    // the new route is still rendering (the router moves the url first). The
    // detector would answer with stale items that look perfectly healthy, so
    // wait the transition out before reading anything.
    page.settle_route(crate::page::ROUTE_BUDGET).await;
    let expr = auto_extract_expr(limit, post_marker);
    let mut val = auto_extract_eval(page, &expr).await?;
    for _ in 0..2 {
        let items_len = val
            .get("items")
            .and_then(|i| i.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if items_len >= 8 || page.in_flight() == 0 {
            break;
        }
        crate::page::wait_for_settle_with_network(
            page.cdp_ref(),
            std::time::Duration::from_millis(800),
            Some(page.in_flight_ref()),
        )
        .await
        .ok();
        val = auto_extract_eval(page, &expr).await?;
    }
    Ok(val)
}

async fn auto_extract_eval(page: &Page, expr: &str) -> Result<serde_json::Value> {
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("auto-extract eval failed");
        return Err(BladeError::Other(format!(
            "auto-extract: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }
    let json_str = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    Ok(serde_json::from_str(json_str)
        .unwrap_or_else(|_| serde_json::json!({"error": "parse failed", "items": []})))
}
/// Offload big extract payloads to an artifact; render inline otherwise.
fn auto_extract_output(json_str: &str) -> Result<String> {
    if json_str.len() > 12000 {
        let path = crate::artifacts::write_artifact(json_str, "json")?;
        let preview: String = json_str.chars().take(1000).collect();
        return Ok(format!(
            "extract auto ({} bytes)\npreview: {preview}…\n{}",
            json_str.len(),
            artifact_hint(&path)
        ));
    }
    Ok(format!("extract auto:\n{json_str}"))
}
/// Honest note when the read raced a client-side route transition — the page
/// was still rendering the new route while the DOM was read.
fn route_note(page: &Page) -> &'static str {
    if page.take_route_unsettled() {
        "\nnote: the page was still rendering a client-side navigation when this was read — items may predate it; re-run to refresh"
    } else {
        ""
    }
}

pub async fn handle_auto_extract(
    page: &mut Page,
    limit: usize,
    limit_explicit: bool,
) -> Result<String> {
    let val = run_auto_extract(page, limit, true).await?;

    // Reddit post pages: the marker hands off to the comment-tree sweep — one
    // in-page API pass returns every comment (collapsed replies included),
    // thread-ordered and structured, instead of scraping the rendered DOM.
    if val.get("container").and_then(|c| c.as_str()) == Some("reddit-post-page") {
        let permalink = val
            .get("permalink")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        let sort = val
            .get("sort")
            .and_then(|s| s.as_str())
            .unwrap_or("confidence")
            .to_string();
        if !permalink.is_empty() {
            let cap = if limit_explicit {
                limit.clamp(1, crate::reddit::MAX_COMMENT_CAP)
            } else {
                crate::reddit::DEFAULT_COMMENT_CAP
            };
            match crate::reddit::fetch_comments(page.cdp_ref(), &permalink, &sort, cap).await {
                Ok(payload) => {
                    let json_str = serde_json::to_string(&payload)?;
                    let _ = page.take_route_unsettled();
                    return auto_extract_output(&json_str);
                }
                Err(e) => {
                    // API route failed (exotic host, blocked page): fall back to
                    // the DOM listing — and say so, because the DOM misses
                    // collapsed replies. A transient network-security wall gets
                    // a retry hint: it clears in seconds, and the DOM fallback
                    // is genuinely partial.
                    let val = run_auto_extract(page, limit, false).await?;
                    let json_str = serde_json::to_string(&val)?;
                    let mut out = auto_extract_output(&json_str)?;
                    let hint = if crate::reddit::is_security_block(&e) {
                        " (reddit's network-security wall is transient — retrying the extract in a few seconds usually returns the full thread)"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "\nnote: full-thread fetch failed ({e}); items above are a DOM fallback and may miss collapsed replies{hint}"
                    ));
                    out.push_str(route_note(page));
                    return Ok(out);
                }
            }
        }
    }

    // Reddit SEARCH pages: results are client-rendered SDUI units (no
    // `shreddit-post` exists, so the feed fast path cannot see them) and the
    // page's router swaps the feed AFTER the url moves — a DOM read here can
    // answer the previous query. Hand off to reddit's own listing JSON: exact
    // scores, the query it answered, and freshness independent of what the
    // router has painted yet.
    if val.get("container").and_then(|c| c.as_str()) == Some("reddit-search-page") {
        let pathname = val.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let params = val.get("params").and_then(|p| p.as_str()).unwrap_or("");
        if let Some((path, query)) = crate::reddit::search_target(pathname, params) {
            let cap = if limit_explicit {
                limit.clamp(1, crate::reddit::MAX_SEARCH_PAGE)
            } else {
                crate::reddit::DEFAULT_SEARCH_LIMIT
            };
            match crate::reddit::fetch_search(page.cdp_ref(), &path, &query, cap).await {
                Ok(payload) => {
                    let json_str = serde_json::to_string(&payload)?;
                    let _ = page.take_route_unsettled();
                    return auto_extract_output(&json_str);
                }
                Err(e) => {
                    // Wall / cold profile: fall back to the mounted DOM, and
                    // say so — the DOM cannot prove which query it shows.
                    let val = run_auto_extract(page, limit, false).await?;
                    let json_str = serde_json::to_string(&val)?;
                    let mut out = auto_extract_output(&json_str)?;
                    let hint = if crate::reddit::is_security_block(&e) {
                        " (reddit's network-security wall is transient — retrying the extract in a few seconds usually returns the full listing)"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "\nnote: reddit search listing fetch failed ({e}); items above are a DOM fallback{hint}"
                    ));
                    out.push_str(route_note(page));
                    return Ok(out);
                }
            }
        }
    }

    // X.COM pages: the marker hands off to the graphql fast path — the
    // page's own API traffic is replayed from page context, so the result
    // is complete regardless of what the virtualized DOM has mounted.
    if val.get("container").and_then(|c| c.as_str()) == Some("x-page") {
        let kind = val.get("kind").and_then(|k| k.as_str()).unwrap_or("other");
        let cap = if limit_explicit {
            limit.clamp(1, crate::x::MAX_ITEM_CAP)
        } else {
            crate::x::DEFAULT_ITEM_CAP
        };
        match crate::x::extract(page, kind, cap).await {
            Ok(payload) => {
                let json_str = serde_json::to_string(&payload)?;
                let _ = page.take_route_unsettled();
                return auto_extract_output(&json_str);
            }
            Err(e) => {
                // Capture/fetch failed (no API traffic seen, exotic page):
                // fall back to the DOM listing and say so.
                let val = run_auto_extract(page, limit, false).await?;
                let json_str = serde_json::to_string(&val)?;
                let mut out = auto_extract_output(&json_str)?;
                out.push_str(&format!(
                    "\nnote: x.com fast path failed ({e}); items above are a DOM fallback"
                ));
                out.push_str(route_note(page));
                return Ok(out);
            }
        }
    }

    let json_str = serde_json::to_string(&val)?;
    let mut out = auto_extract_output(&json_str)?;
    out.push_str(route_note(page));
    Ok(out)
}
/// V22: collect — auto-extract + scroll + dedupe loop. ONE call collects
/// an entire infinite-scroll feed into a single artifact. The result names
/// WHY collection stopped (feed exhausted vs. max vs. timeout) so the agent
/// knows whether re-running can get more.
pub async fn handle_collect(page: &mut Page, args: &Value) -> Result<String> {
    // Manual-control pause: collect navigates and auto-scrolls the page —
    // it must not run while the person is using the browser.
    if crate::realbrowser::input_paused() {
        return Err(crate::realbrowser::paused_error());
    }
    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(30);
    let max = args.get("max").and_then(|m| m.as_u64()).unwrap_or(100) as usize;
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");

    // Navigate first if url is provided. Without this, collect
    // extracts from whatever page happens to be current (Bug: asked
    // for Reddit, got eBay items).
    if !url.is_empty() {
        page.navigate(url).await?;
    }

    let mut all_items: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut no_new_streak = 0u32;
    // Set on every break path below; declared uninitialized so the compiler
    // proves it (an initial value would be dead).
    let stop: String;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        let val = run_auto_extract(page, 500, false).await?;
        let items = val
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let mut new_count = 0usize;
        for item in items {
            // Keyless items dedupe by their JSON — the old empty-key branch
            // re-pushed them every scroll iteration.
            let key = item
                .get("url")
                .or_else(|| item.get("title"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| item.to_string());
            if seen.insert(key) {
                all_items.push(item);
                new_count += 1;
            }
        }

        if all_items.len() >= max {
            stop = format!("max={max} reached — raise max to collect more");
            break;
        }
        if new_count == 0 {
            no_new_streak += 1;
            if no_new_streak >= 2 {
                stop = "feed exhausted (no new items after 2 scrolls)".to_string();
                break;
            }
        } else {
            no_new_streak = 0;
        }
        if std::time::Instant::now() > deadline {
            stop = format!(
                "timeout {timeout_secs}s — the feed may have more; raise timeout or re-run"
            );
            break;
        }

        let _ = page
            .cdp_ref()
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": "window.scrollBy(0, Math.floor(window.innerHeight*0.9))",
                    "returnByValue": true,
                })),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    let status = format!(
        "collected {} items — {stop}{}",
        all_items.len(),
        route_note(page)
    );
    let json = serde_json::to_string_pretty(&all_items)?;
    if json.len() <= 12000 {
        return Ok(format!("{status}:\n{json}"));
    }
    let path = crate::artifacts::write_artifact(&json, "json")?;
    let preview: String = json.chars().take(1000).collect();
    Ok(format!(
        "{status}\n{}\npreview: {preview}…",
        artifact_hint(&path)
    ))
}
#[cfg(test)]
mod extract_script_tests {
    //! The auto-extract script is a large injected string — a syntax error
    //! disables `extract=auto` everywhere. `node --check` it at test time
    //! (skipped when node is not installed; the driver itself never needs it).

    #[test]
    fn extract_script_is_valid_js() {
        for post_marker in [false, true] {
            let js = super::auto_extract_expr(50, post_marker);
            assert!(
                js.contains("Quality gate"),
                "quality gate must survive placeholder substitution"
            );
            assert!(
                !js.contains("__LIMIT__"),
                "limit placeholder must be substituted everywhere"
            );
            assert!(
                !js.contains("__POST_MARKER__"),
                "marker placeholder must be substituted everywhere"
            );
            if post_marker {
                assert!(
                    js.contains("reddit-post-page"),
                    "post marker present when enabled"
                );
                assert!(
                    js.contains("reddit-search-page"),
                    "search handoff marker present when enabled"
                );
            }
            let has_node = std::process::Command::new("node")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !has_node {
                eprintln!("node not available — skipping extract script syntax check");
                return;
            }
            let path =
                std::env::temp_dir().join(format!("bladebro-js-check-extract-{post_marker}.js"));
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "node --check failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
