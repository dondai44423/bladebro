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

/// V9: template extraction. The agent provides a declarative template; the
/// driver runs ONE query and returns structured JSON. Zero LLM in the loop —
/// the fastest extraction of any agent browser.
///
/// Template shape:
/// ```json
/// {"items": {"container": "css", "fields": {"name": "css", "link": "css@attr"}}}
/// ```
/// Multiple top-level keys are allowed (multiple lists in one call). A field
/// value of "" reads the container element itself.
///
/// Extraction contract (G06):
/// - Plain text fields are RENDERED reads: an element that is not rendered
///   (display:none / visibility:hidden anywhere on its composed chain) is
///   OMITTED — the field key is absent and the item carries
///   `_omitted: ["field", ...]` instead of leaking hidden text into
///   structured evidence.
/// - `css@attr` reads an attribute (intentionally raw by nature).
/// - `{"sel": "css", "raw": true}` reads RAW text (`innerText || textContent`),
///   hidden content included — the legacy behavior, now opt-in.
/// - Containers and fields are looked up across the light DOM, open shadow
///   roots and same-origin frames, all BOUNDED (20k-node budget, 48-frame
///   cap); skipped cross-origin frames and budget exhaustion are reported in
///   the output, never silent. Field states stay distinct: omitted (absent +
///   `_omitted`), selector missed (null), genuinely empty ("").
///
/// The traversal/read script lives in `js/template_extract.js` (house rule:
/// embedded JS stays in src/**/js); `__LISTS__` is substituted with the
/// per-list code generated from the template.
fn template_extract_expr(template: &Value, limit: usize) -> Result<String> {
    let obj = template
        .as_object()
        .ok_or_else(|| BladeError::Other("template must be a JSON object".into()))?;
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
        // Field value: a string (rendered text, or css@attr) or an object
        // {"sel": "...", "raw": true} for intentional hidden-text reads.
        let mut field_code = Vec::new();
        for (fname, fsel) in &fields {
            let (sel, raw) = match fsel {
                Value::String(s) => (s.clone(), false),
                Value::Object(o) => {
                    let sel = o
                        .get("sel")
                        .and_then(|s| s.as_str())
                        .ok_or_else(|| {
                            BladeError::Other(format!(
                                "template field '{fname}' object needs a 'sel' selector string"
                            ))
                        })?
                        .to_string();
                    (sel, o.get("raw").and_then(|r| r.as_bool()).unwrap_or(false))
                }
                _ => {
                    return Err(BladeError::Other(format!(
                        "template field '{fname}' must be a selector string (\".css\" or \".css@attr\") or {{\"sel\": \"...\", \"raw\": true}}"
                    )));
                }
            };
            let idx = field_code.len();
            let fname_js = serde_json::to_string(fname)?;
            let sel_js = serde_json::to_string(&sel)?;
            field_code.push(format!(
                "{{const r{idx}=read(c,{sel_js},{raw});if(r{idx}.om){{om.push({fname_js});}}else{{o[{fname_js}]=r{idx}.v;}}}}",
                raw = if raw { "true" } else { "false" },
            ));
        }
        list_builders.push(format!(
            "{}:(()=>{{const cs=collect(document,{},st).slice(0,{limit});return cs.map(c=>{{const o={{}};const om=[];{}if(om.length){{o._omitted=om;st.om+=om.length;}}return o;}});}})()",
            serde_json::to_string(list_name)?,
            serde_json::to_string(container)?,
            field_code.join("")
        ));
    }
    list_builders.push("__blade_meta:{cross:st.cross,budget:st.hit,omitted:st.om}".to_string());
    Ok(TEMPLATE_EXTRACT_EXPR.replace("__LISTS__", &list_builders.join(",")))
}

/// The bounded traversal + rendered/raw read script (contract documented on
/// [`template_extract_expr`]).
const TEMPLATE_EXTRACT_EXPR: &str = include_str!("js/template_extract.js");

pub async fn handle_template_extract(
    page: &mut Page,
    template: &Value,
    limit: usize,
    json_mode: bool,
) -> Result<String> {
    let expr = template_extract_expr(template, limit)?;

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
            crate::platform::truncate_utf8(msg.lines().next().unwrap_or(msg), 200)
        )));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(json!({}));
    let json_str = serde_json::to_string_pretty(&value)?;

    // W2: JSON mode returns a parseable payload only — caveats live in
    // __blade_meta, and an oversized payload is an artifact envelope, not
    // a truncated page.
    if json_mode {
        if json_str.len() > 6000 {
            let path = crate::artifacts::write_artifact(&json_str, "json")?;
            return Ok(json!({
                "artifact": path,
                "bytes": json_str.len(),
                "next_offset": 0,
            })
            .to_string());
        }
        return Ok(json_str);
    }

    // Count total items across lists (the meta key is an object; skipped).
    let total: usize = value
        .as_object()
        .map(|o| {
            o.values()
                .filter_map(|v| v.as_array().map(|a| a.len()))
                .sum()
        })
        .unwrap_or(0);

    // Explicit omissions: rendered-skips, unsearched frames and any budget
    // exhaustion are reported instead of looking like clean data.
    let meta = value.get("__blade_meta");
    let omitted = meta
        .and_then(|m| m.get("omitted"))
        .and_then(|o| o.as_u64())
        .unwrap_or(0);
    let cross = meta
        .and_then(|m| m.get("cross"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let budget = meta
        .and_then(|m| m.get("budget"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let mut notes = String::new();
    if omitted > 0 {
        notes.push_str(&format!(
            "\nnote: {omitted} field value(s) omitted as not rendered - read one with {{\"sel\":\"...\",\"raw\":true}}, or css@attr for attributes"
        ));
    }
    if cross > 0 {
        notes.push_str(&format!(
            "\nnote: {cross} frame(s) not searched (cross-origin or unloaded) - results may be incomplete"
        ));
    }
    if budget {
        notes.push_str(
            "\nnote: traversal node budget reached - results may be partial; narrow the container selector",
        );
    }

    if json_str.len() > 6000 {
        let path = crate::artifacts::write_artifact(&json_str, "json")?;
        let preview: String = json_str.chars().take(600).collect();
        return Ok(format!(
            "extract json ({total} items, {} bytes){notes}\npreview: {preview}…\n{}",
            json_str.len(),
            artifact_hint(&path)
        ));
    }
    Ok(format!("extract json ({total} items){notes}:\n{json_str}"))
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
            crate::platform::truncate_utf8(msg.lines().next().unwrap_or(msg), 200)
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
/// Render an auto-extract payload. Text mode: header + inline JSON, big
/// payloads offloaded with a preview. JSON mode (W2): a bare parseable
/// payload; a caveat note wraps it as {"data":...,"note":...}; oversized
/// payloads hand back an {"artifact","bytes","next_offset"} envelope
/// instead of a truncated preview — every response is valid JSON.
fn auto_extract_output<T: serde::Serialize>(
    val: &T,
    json_mode: bool,
    note: &str,
) -> Result<String> {
    let json_str = serde_json::to_string(val)?;
    if json_mode {
        let payload = if note.trim().is_empty() {
            json_str
        } else {
            json!({ "data": val, "note": note.trim() }).to_string()
        };
        if payload.len() > 12000 {
            let path = crate::artifacts::write_artifact(&payload, "json")?;
            return Ok(json!({
                "artifact": path,
                "bytes": payload.len(),
                "next_offset": 0,
            })
            .to_string());
        }
        return Ok(payload);
    }
    let mut out = if json_str.len() > 12000 {
        let path = crate::artifacts::write_artifact(&json_str, "json")?;
        let preview: String = json_str.chars().take(1000).collect();
        format!(
            "extract auto ({} bytes)\npreview: {preview}…\n{}",
            json_str.len(),
            artifact_hint(&path)
        )
    } else {
        format!("extract auto:\n{json_str}")
    };
    if !note.is_empty() {
        out.push_str(note);
    }
    Ok(out)
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

/// Plain-language notes for the generic auto-extract envelope (P1). Each one
/// answers a question the payload raises but does not answer: why fewer rows
/// came back than the page claims, why some rows are marked sponsored, where
/// duplicates went, and what happened to wrapper links. Advisory wording only
/// — never a cause claim the driver cannot prove.
fn auto_extract_notes(val: &serde_json::Value, limit: usize) -> String {
    let num = |k: &str| val.get(k).and_then(|v| v.as_u64());
    let count = num("count").unwrap_or(0);
    let mut notes = String::new();
    if val.get("partial").and_then(|p| p.as_bool()) == Some(true) {
        if let Some(total) = num("page_total") {
            if val.get("paged").and_then(|p| p.as_bool()) == Some(true) {
                notes.push_str(&format!(
                    "\nnote: the page states {total} results and this is ONE page of them — only the {count} on this page were in the DOM; follow the pagination links, or act collect url=<next page> for the rest"
                ));
            } else {
                notes.push_str(&format!(
                    "\nnote: the page states {total} results but only {count} were in the DOM — the rest are not loaded yet (lazy load or pagination); act collect scrolls + dedupes the whole feed, or follow the next-page link"
                ));
            }
        }
    }
    if count > 0 && count >= limit as u64 {
        notes.push_str(&format!(
            "\nnote: the item cap was reached (limit={limit}) — raise limit for more items"
        ));
    }
    if let Some(sp) = num("sponsored") {
        notes.push_str(&format!(
            "\nnote: {sp} of {count} items are paid placements (sponsored:true, left in page order) — filter them out for organic results only"
        ));
    }
    if let Some(d) = num("duplicates_removed") {
        notes.push_str(&format!(
            "\nnote: {d} duplicate item(s) removed (same canonical url or title)"
        ));
    }
    if let Some(u) = num("redirects_unwrapped") {
        notes.push_str(&format!(
            "\nnote: {u} redirect/tracking wrapper link(s) unwrapped to the real destination (item urls are canonical)"
        ));
    }
    notes
}

/// Trim optional per-item fields when the payload would blow the inline
/// budget. `description` goes first (longest, least load-bearing on a
/// listing), then `image_alt`, then `image` — items and their core fields are
/// never dropped, and the FULL untrimmed payload is written to an artifact
/// first so nothing is lost. Returns the artifact path + what was dropped.
fn trim_for_inline(val: &mut serde_json::Value) -> Option<(String, Vec<&'static str>)> {
    const CAP: usize = 12_000;
    let size = serde_json::to_string(val).map(|s| s.len()).unwrap_or(0);
    if size <= CAP {
        return None;
    }
    let full = serde_json::to_string(val).ok()?;
    let path = crate::artifacts::write_artifact(&full, "json").ok()?;
    let mut dropped: Vec<&'static str> = Vec::new();
    for field in ["description", "image_alt", "image"] {
        let mut changed = false;
        if let Some(items) = val.get_mut("items").and_then(|i| i.as_array_mut()) {
            for it in items.iter_mut() {
                if let Some(obj) = it.as_object_mut() {
                    if obj.remove(field).is_some() {
                        changed = true;
                    }
                }
            }
        }
        if changed {
            dropped.push(field);
            if serde_json::to_string(val).map(|s| s.len()).unwrap_or(0) <= CAP {
                break;
            }
        }
    }
    Some((path, dropped))
}

/// A `min-max` range filter carried in the URL (Amazon's `rh=p_36:35000-45000`,
/// `price=100-200`, …) versus the prices the page ITSELF shows. When every
/// readable price sits outside the requested range, the results are not what
/// was asked for — the site ignored its own filter. Both sides are stated as
/// numbers the caller can check; nothing is inferred about intent.
fn range_filter_note(url: &str, val: &serde_json::Value) -> Option<String> {
    let (min, max, raw) = url_range_filter(url)?;
    let items = val.get("items")?.as_array()?;
    let mut prices: Vec<(String, f64)> = Vec::new();
    for it in items {
        if let Some(p) = it.get("price").and_then(|p| p.as_str()) {
            if let Some(v) = parse_price(p) {
                prices.push((p.to_string(), v));
            }
        }
    }
    if prices.len() < 3 {
        return None;
    }
    if prices.iter().any(|(_, v)| *v >= min && *v <= max) {
        return None; // the page honors the filter — no news
    }
    let lo = prices
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(s, _)| s.clone())?;
    let hi = prices
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(s, _)| s.clone())?;
    Some(format!(
        "\nnote: the URL carries a range filter ({raw}), but every readable item price on this page is outside it ({lo}–{hi}, {n} of {n} items) — the site may not have applied that filter; verify with the site's own filter UI before trusting these results",
        raw = raw,
        lo = lo,
        hi = hi,
        n = prices.len()
    ))
}

/// Pull a `min-max` range out of the URL's query parameters. Only shapes that
/// are unambiguously a numeric range count, and only when min < max.
fn url_range_filter(url: &str) -> Option<(f64, f64, String)> {
    let params = crate::page::navigate::query_params(url)?;
    for (k, v) in &params {
        let decoded = v.replace("%3A", ":").replace("%3a", ":");
        let candidate = if let Some((_, tail)) = decoded.split_once(':') {
            // `p_36:35000-45000` — the range follows the colon.
            let key = k.to_ascii_lowercase();
            if key.contains("p_36")
                || key.contains("price")
                || key.contains("filter")
                || key == "rh"
            {
                tail.to_string()
            } else {
                continue;
            }
        } else {
            let key = k.to_ascii_lowercase();
            if key.contains("price") || key.contains("range") || key.contains("filter") {
                decoded.clone()
            } else {
                continue;
            }
        };
        let mut parts = candidate.split('-');
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(min), Ok(max)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) else {
            continue;
        };
        if min > 0.0 && max > min {
            return Some((min, max, format!("{k}={v}")));
        }
    }
    None
}

/// The first number-shaped token in a displayed price, thousands separators
/// removed. `₹1,299` → 1299, `$49.99` → 49.99. Deliberately dumb: a heuristic
/// that feeds a note, never a value anything else depends on.
fn parse_price(shown: &str) -> Option<f64> {
    let start = shown.find(|c: char| c.is_ascii_digit())?;
    let rest = &shown[start..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    let cleaned = token.replace(',', "");
    cleaned.parse::<f64>().ok()
}

pub(super) async fn auto_preview(page: &Page) -> Result<String> {
    let value = run_auto_extract(page, 3, false).await?;
    let fields: std::collections::BTreeSet<String> = value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .flat_map(|o| o.keys().cloned())
        .collect();
    Ok(json!({"container":value.get("container"),"fields":fields,"sampled":value.get("count"),"note":"schema sampled from up to 3 rendered rows; other rows may have additional fields"}).to_string())
}

pub async fn handle_auto_extract(
    page: &mut Page,
    limit: usize,
    limit_explicit: bool,
    json_mode: bool,
    sort_override: Option<&str>,
) -> Result<String> {
    let val = run_auto_extract(page, limit, true).await?;

    if sort_override.is_some()
        && val.get("container").and_then(Value::as_str) != Some("reddit-post-page")
    {
        return Err(BladeError::Usage(
            "sort applies to extract=auto on Reddit post pages only".into(),
        ));
    }
    // Reddit post pages: the marker hands off to the comment-tree sweep — one
    // in-page API pass returns every comment (collapsed replies included),
    // thread-ordered and structured, instead of scraping the rendered DOM.
    if val.get("container").and_then(|c| c.as_str()) == Some("reddit-post-page") {
        let permalink = val
            .get("permalink")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        let sort = sort_override.map(str::to_string).unwrap_or_else(|| {
            val.get("sort")
                .and_then(|s| s.as_str())
                .unwrap_or("confidence")
                .to_string()
        });
        if !permalink.is_empty() {
            let cap = if limit_explicit {
                limit.clamp(1, crate::reddit::MAX_COMMENT_CAP)
            } else {
                crate::reddit::DEFAULT_COMMENT_CAP
            };
            match crate::reddit::fetch_comments(page.cdp_ref(), &permalink, &sort, cap).await {
                Ok(payload) => {
                    let _ = page.take_route_unsettled();
                    return auto_extract_output(&payload, json_mode, "");
                }
                Err(e) => {
                    // API route failed (exotic host, blocked page): fall back to
                    // the DOM listing — and say so, because the DOM misses
                    // collapsed replies. Policy denials can persist; the
                    // fallback never claims the full tree or promises a retry.
                    let mut val = run_auto_extract(page, limit, false).await?;
                    val["complete"] = json!(false);
                    val["partial"] = json!(true);
                    let hint = if crate::reddit::is_security_block(&e) {
                        " (Reddit denied the API request; repeated extracts may remain blocked. DOM coverage is partial; verify complete before treating this as the full thread)"
                    } else {
                        ""
                    };
                    let note = format!(
                        "\nnote: full-thread fetch failed ({e}); items above are a DOM fallback and may miss collapsed replies{hint}{}",
                        route_note(page)
                    );
                    return auto_extract_output(&val, json_mode, &note);
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
                    let _ = page.take_route_unsettled();
                    return auto_extract_output(&payload, json_mode, "");
                }
                Err(e) => {
                    // Wall / cold profile: fall back to the mounted DOM, and
                    // say so — the DOM cannot prove which query it shows.
                    let mut val = run_auto_extract(page, limit, false).await?;
                    val["complete"] = json!(false);
                    val["partial"] = json!(true);
                    let hint = if crate::reddit::is_security_block(&e) {
                        " (Reddit denied the API request; repeated extracts may remain blocked. DOM coverage is partial)"
                    } else {
                        ""
                    };
                    let note = format!(
                        "\nnote: reddit search listing fetch failed ({e}); items above are a DOM fallback{hint}{}",
                        route_note(page)
                    );
                    return auto_extract_output(&val, json_mode, &note);
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
                let _ = page.take_route_unsettled();
                return auto_extract_output(&payload, json_mode, "");
            }
            Err(e) => {
                // Capture/fetch failed (no API traffic seen, exotic page):
                // fall back to the DOM listing and say so.
                let mut val = run_auto_extract(page, limit, false).await?;
                val["complete"] = json!(false);
                val["partial"] = json!(true);
                let note = format!(
                    "\nnote: x.com fast path failed ({e}); items above are a DOM fallback{}",
                    route_note(page)
                );
                return auto_extract_output(&val, json_mode, &note);
            }
        }
    }

    let mut val = val;
    let mut note = auto_extract_notes(&val, limit);
    if let Some(range_note) = range_filter_note(page.model().url(), &val) {
        note.push_str(&range_note);
    }
    if let Some((path, dropped)) = trim_for_inline(&mut val) {
        note.push_str(&format!(
            "\nnote: optional fields trimmed to fit (dropped: {}) — items and core fields are complete; full untrimmed payload: {path}",
            dropped.join(", ")
        ));
    }
    note.push_str(route_note(page));
    auto_extract_output(&val, json_mode, &note)
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

    /// G06: the template script (assembled with a real template) must parse,
    /// substitute every placeholder, and carry the bounded traversal + meta.
    #[test]
    fn template_extract_script_is_valid_js() {
        let tpl = serde_json::json!({
            "items": {"container": ".row", "fields": {
                "a": ".name",
                "b": ".link@href",
                "c": {"sel": ".hidden", "raw": true}
            }}
        });
        let js = super::template_extract_expr(&tpl, 25).expect("template expr");
        assert!(!js.contains("__LISTS__"), "placeholder substituted");
        assert!(js.contains("__blade_meta"), "meta block present");
        assert!(js.contains("NODE_BUDGET"), "bounded walker present");
        assert!(js.contains("read(c,\".hidden\",true)"), "raw flag threaded");
        assert!(js.contains("slice(0,25)"), "item limit threaded");
        let has_node = std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping template script syntax check");
            return;
        }
        let path = std::env::temp_dir().join("bladebro-js-check-template.js");
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

    #[test]
    fn template_field_forms_validate() {
        // Object field without sel: loud, names the field.
        let tpl = serde_json::json!({"l": {"container": ".x", "fields": {"f": {"raw": true}}}});
        let err = super::template_extract_expr(&tpl, 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'f'") && err.contains("'sel'"), "{err}");
        // Wrong type: loud, names the field and the accepted forms.
        let tpl = serde_json::json!({"l": {"container": ".x", "fields": {"f": 7}}});
        let err = super::template_extract_expr(&tpl, 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'f'") && err.contains("raw"), "{err}");
        // Missing container stays loud.
        let tpl = serde_json::json!({"l": {"fields": {}}});
        let err = super::template_extract_expr(&tpl, 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("container"), "{err}");
    }
}

#[cfg(test)]
mod range_filter_tests {
    //! P1: the "the site ignored its own filter" note. The rule is deliberately
    //! conservative — it fires only when EVERY readable price is outside the
    //! requested range, and it is never a claim about intent.

    use super::*;

    fn items_with_prices(prices: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "items": prices.iter().map(|p| serde_json::json!({"price": p})).collect::<Vec<_>>()
        })
    }

    #[test]
    fn range_is_read_from_the_url_shapes_that_mean_one() {
        // Amazon's refinement: rh=p_36%3A35000-45000
        let (min, max, raw) =
            url_range_filter("https://www.amazon.in/s?k=ssd&rh=p_36%3A35000-45000")
                .expect("amazon range");
        assert_eq!((min, max), (35000.0, 45000.0));
        assert!(raw.contains("35000-45000"), "{raw}");
        // A bare price range parameter.
        assert!(url_range_filter("https://e.com/s?price=100-200").is_some());
        // Not a range: inverted, non-numeric, or an unrelated param.
        assert!(url_range_filter("https://e.com/s?sr=8-1").is_none());
        assert!(url_range_filter("https://e.com/s?q=ssd").is_none());
        assert!(url_range_filter("https://e.com/s?page=2").is_none());
    }

    #[test]
    fn the_note_fires_only_when_every_price_is_outside() {
        let url = "https://www.amazon.in/s?k=ssd&rh=p_36%3A35000-45000";
        // The live defect: a ₹35k–₹45k filter answering with ₹399 enclosures.
        let note = range_filter_note(url, &items_with_prices(&["₹399", "₹426", "₹350", "₹1,299"]))
            .expect("all prices outside the range");
        assert!(note.contains("35000-45000"), "{note}");
        assert!(note.contains("₹350"), "cheapest price named: {note}");
        assert!(note.contains("₹1,299"), "dearest price named: {note}");
        // One price inside → the filter is plausibly working, stay quiet.
        assert!(range_filter_note(url, &items_with_prices(&["₹399", "₹426", "₹40,000"])).is_none());
        // Too few prices to say anything.
        assert!(range_filter_note(url, &items_with_prices(&["₹399", "₹426"])).is_none());
        // No range in the URL → nothing to compare.
        assert!(
            range_filter_note("https://e.com/s?q=ssd", &items_with_prices(&["₹399"; 4])).is_none()
        );
    }

    #[test]
    fn prices_parse_through_separators_and_currency() {
        assert_eq!(parse_price("₹1,299"), Some(1299.0));
        assert_eq!(parse_price("$49.99"), Some(49.99));
        assert_eq!(parse_price("399"), Some(399.0));
        assert_eq!(parse_price("no price here"), None);
    }
}
