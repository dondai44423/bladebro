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
pub(super) fn template_extract_expr(template: &Value, limit: usize) -> Result<String> {
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
            .and_then(Value::as_object)
            .ok_or_else(|| {
                BladeError::Usage(format!("template list '{list_name}' needs a fields object"))
            })?;
        // Field value: a string (rendered text, or css@attr) or an object
        // {"sel": "...", "raw": true} for intentional hidden-text reads.
        let mut field_code = Vec::new();
        for (fname, fsel) in fields {
            let (sel, raw) = match fsel {
                Value::String(s) => (s.clone(), false),
                Value::Object(o) => {
                    if o.get("raw").is_some_and(|r| !r.is_boolean()) {
                        return Err(BladeError::Usage(format!(
                            "template field '{fname}' raw must be a boolean"
                        )));
                    }
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
    page.settle_route(crate::page::ROUTE_BUDGET).await;

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
/// Per-read item ceiling of the generic auto-extract script (`limit` above it
/// is clamped), bounding one Runtime.evaluate payload.
const AUTO_EXTRACT_MAX: usize = 500;

/// V21: Auto-extract — deterministic structural list extraction.
/// Finds the DOM container whose direct children are the most
/// structurally-repeated (the "main list"), extracts per-item fields by
/// content type, returns a JSON array. No template, no LLM.
fn auto_extract_expr(limit: usize, post_marker: bool) -> String {
    let lim = limit.min(AUTO_EXTRACT_MAX);
    include_str!("js/auto_extract.js")
        .replace("__SITE_ADAPTERS__", &crate::adapters::site_adapters_js())
        .replace("__JSONLD_FALLBACK__", crate::adapters::jsonld_fallback_js())
        .replace("__LIMIT__", &lim.to_string())
        .replace(
            "__POST_MARKER__",
            if post_marker { "true" } else { "false" },
        )
}

/// Run auto-extract and return `(parsed JSON payload, live location.href)`.
///
/// Feeds hydrate asynchronously — an extract fired during the stream can
/// find no list yet, or only the first few items. When the result is empty
/// OR tiny (<8 items) and requests are still in flight, settle briefly and
/// re-run (bounded: 2 retries). Handoff markers (`handoff: true`) and
/// single-entity detail adapters (`detail: true`) exit immediately without
/// burning settle latency.
async fn run_auto_extract(
    page: &Page,
    limit: usize,
    post_marker: bool,
) -> Result<(serde_json::Value, String)> {
    // A client-side route change leaves the PREVIOUS route's DOM mounted while
    // the new route is still rendering (the router moves the url first). The
    // detector would answer with stale items that look perfectly healthy, so
    // wait the transition out before reading anything.
    page.settle_route(crate::page::ROUTE_BUDGET).await;
    let expr = auto_extract_expr(limit, post_marker);
    let (mut val, mut live_url) = match auto_extract_eval(page, &expr).await {
        Ok(res) => res,
        Err(e) if crate::cdp::CdpSession::is_stale_context_err(&e) => {
            // A redirect (e.g. reddit short post URL -> canonical slug) or
            // challenge commit landed mid-eval; let the destination load and
            // retry this pure read once.
            let _ =
                crate::page::wait_for_load(page.cdp_ref(), std::time::Duration::from_secs(3)).await;
            page.settle_route(crate::page::ROUTE_BUDGET).await;
            auto_extract_eval(page, &expr).await?
        }
        Err(e) => return Err(e),
    };
    for _ in 0..2 {
        if crate::adapters::is_early_settle(&val) {
            break;
        }
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
        let next = auto_extract_eval(page, &expr).await?;
        val = next.0;
        live_url = next.1;
    }
    Ok((val, live_url))
}

async fn auto_extract_eval(page: &Page, expr: &str) -> Result<(serde_json::Value, String)> {
    let wrapped = format!(
        "(()=>{{const p={expr};return JSON.stringify({{u:String(location.href||''),p}});}})()"
    );
    let res = page
        .cdp_ref()
        .eval_isolated_with_timeout(&wrapped, true, false, std::time::Duration::from_secs(15))
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
    let Some(raw) = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
    else {
        return Err(BladeError::Other(
            "auto-extract: browser returned no data; retry after the page settles".into(),
        ));
    };
    let outer: Value = serde_json::from_str(raw)
        .unwrap_or_else(|_| serde_json::json!({"error": "parse failed", "items": []}));
    if let Some(payload_str) = outer.get("p").and_then(Value::as_str) {
        let live_url = outer
            .get("u")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let val = serde_json::from_str(payload_str)
            .unwrap_or_else(|_| serde_json::json!({"error": "parse failed", "items": []}));
        Ok((val, live_url))
    } else {
        Ok((outer, page.model().url().to_string()))
    }
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
    // The script clamps limit, so compare against the cap it actually used;
    // past the clamp, raising limit cannot return more.
    let cap = limit.min(AUTO_EXTRACT_MAX);
    if count > 0 && count >= cap as u64 {
        if limit > cap {
            notes.push_str(&format!(
                "\nnote: the item cap was reached (limit={limit} is clamped to {cap} for extract=auto) — more items may exist; extract=json with a template is not clamped"
            ));
        } else {
            notes.push_str(&format!(
                "\nnote: the item cap was reached (limit={limit}) — raise limit for more items"
            ));
        }
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
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(s, _)| s.clone())?;
    let hi = prices
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
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
    let (value, _) = run_auto_extract(page, 3, true).await?;
    if let Some(handoff_preview) = crate::adapters::preview_for_handoff(&value) {
        return Ok(handoff_preview.to_string());
    }
    let mut fields: std::collections::BTreeSet<String> = value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .flat_map(|o| o.keys().cloned())
        .collect();
    // Detail adapters with separate metadata objects (e.g. youtube-video when
    // comments haven't loaded yet) surface those entity fields too.
    if fields.is_empty() {
        for key in ["video", "post", "issue", "question"] {
            if let Some(obj) = value.get(key).and_then(Value::as_object) {
                fields.extend(obj.keys().map(|k| format!("{key}.{k}")));
            }
        }
    }
    Ok(json!({"container":value.get("container"),"fields":fields,"sampled":value.get("count"),"note":"schema sampled from up to 3 rendered rows; other rows may have additional fields"}).to_string())
}

pub async fn handle_auto_extract(
    page: &mut Page,
    limit: usize,
    limit_explicit: bool,
    json_mode: bool,
    sort_override: Option<&str>,
) -> Result<String> {
    let (val, live_url) = run_auto_extract(page, limit, true).await?;
    let mut fallback_reason = String::new();
    let (mut val, effective_url) = match crate::adapters::dispatch(
        page,
        &val,
        &live_url,
        limit,
        limit_explicit,
        sort_override,
    )
    .await?
    {
        crate::adapters::AdapterDispatch::Handled(payload) => {
            return auto_extract_output(&payload, json_mode, "");
        }
        crate::adapters::AdapterDispatch::Fallback { reason } => {
            let (mut dom_val, dom_url) = run_auto_extract(page, limit, false).await?;
            dom_val["complete"] = json!(false);
            dom_val["partial"] = json!(true);
            fallback_reason = reason;
            (dom_val, dom_url)
        }
        crate::adapters::AdapterDispatch::PassThrough => (val, live_url),
    };

    // Strip internal engine routing flags before emitting the payload.
    if let Some(obj) = val.as_object_mut() {
        obj.remove("detail");
        obj.remove("handoff");
    }
    let url_for_notes = if effective_url.is_empty() {
        page.model().url()
    } else {
        effective_url.as_str()
    };
    let mut note = fallback_reason;
    note.push_str(&auto_extract_notes(&val, limit));
    if let Some(range_note) = range_filter_note(url_for_notes, &val) {
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
        let (val, _) = run_auto_extract(page, 500, false).await?;
        let items = val
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let new_count = merge_collected(&mut all_items, &mut seen, items, max);

        if all_items.len() >= max {
            stop = format!("max={max} reached — raise max to collect more");
            break;
        }
        if new_count == 0 {
            no_new_streak += 1;
            if no_new_streak >= 2 {
                let pager = detect_pager(page).await;
                stop = collect_stop_reason(all_items.len(), &val, pager.as_ref());
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
/// Merge one extraction pass into the collection; returns how many items
/// were new. Stops at `max`: one pass over a long page used to overshoot it
/// (max=10 returned 50 rows plus "max=10 reached").
fn merge_collected(
    all_items: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<String>,
    items: Vec<Value>,
    max: usize,
) -> usize {
    let mut new_count = 0usize;
    for item in items {
        if all_items.len() >= max {
            break;
        }
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
    new_count
}

const DETECT_PAGER_JS: &str = include_str!("js/detect_pager.js");

/// Check if the current page exposes a visible pagination "next" control.
/// Used by `handle_collect` so scrolling a paginated listing does not falsely
/// assert "feed exhausted".
async fn detect_pager(page: &Page) -> Option<Value> {
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": DETECT_PAGER_JS,
                "returnByValue": true,
            })),
        )
        .await
        .ok()?;
    let val = res.get("result")?.get("value")?;
    if val.is_object() {
        Some(val.clone())
    } else {
        None
    }
}

/// Truthful terminal reason for `handle_collect` after 2 scrolls yield 0 new items.
fn collect_stop_reason(collected: usize, last_val: &Value, pager: Option<&Value>) -> String {
    let paged = last_val.get("paged").and_then(Value::as_bool) == Some(true);
    let page_total = last_val
        .get("page_total")
        .and_then(Value::as_u64)
        .filter(|&t| (t as usize) > collected);
    let next_hint = pager.and_then(|p| {
        if let Some(sel) = p
            .get("selector")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(format!("next={sel}"))
        } else {
            p.get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|t| format!("next=text={t}"))
        }
    });

    if paged || next_hint.is_some() {
        let mut details = Vec::new();
        if let Some(total) = page_total {
            details.push(format!("page states {total} total"));
        }
        if let Some(hint) = next_hint {
            details.push(format!("live next control ({hint})"));
        }
        let detail_str = if details.is_empty() {
            String::new()
        } else {
            format!(" ({})", details.join("; "))
        };
        return format!(
            "scrolling added no new items — pagination detected{detail_str}; use act paginate instead of collect"
        );
    }
    if let Some(total) = page_total {
        return format!(
            "scrolling added no new items after 2 scrolls (page states {total} total — remaining items may require clicking a pager or load-more button)"
        );
    }
    "feed exhausted (no new items after 2 scrolls)".to_string()
}

#[cfg(test)]
mod collect_tests {
    use super::*;

    #[test]
    fn collect_never_returns_more_than_max() {
        // One pass over a 50-row page with max=10 used to keep all 50 rows
        // and then report "max=10 reached".
        let rows: Vec<Value> = (0..50)
            .map(|i| json!({"url":format!("https://e.test/{i}")}))
            .collect();
        let mut all = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let added = merge_collected(&mut all, &mut seen, rows.clone(), 10);
        assert_eq!((all.len(), added), (10, 10));
        // A later pass adds nothing once the cap is met, and repeats of
        // earlier rows are still deduped.
        assert_eq!(merge_collected(&mut all, &mut seen, rows, 10), 0);
        assert_eq!(all.len(), 10);
        let mut all = vec![json!({"url":"https://e.test/0"})];
        let mut seen = std::collections::HashSet::from(["https://e.test/0".to_string()]);
        let pass = vec![
            json!({"url":"https://e.test/0"}),
            json!({"url":"https://e.test/1"}),
        ];
        assert_eq!(merge_collected(&mut all, &mut seen, pass, 100), 1);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn collect_discloses_pagination_instead_of_claiming_feed_exhausted() {
        let paged_val = json!({"page_total": 32, "paged": true, "partial": true});
        let pager = json!({"selector": "li.next > a", "text": "next"});
        let stop = collect_stop_reason(20, &paged_val, Some(&pager));
        assert!(
            stop.contains("pagination detected")
                && stop.contains("32 total")
                && stop.contains("li.next > a")
                && stop.contains("act paginate")
                && !stop.contains("feed exhausted"),
            "unexpected stop: {stop}"
        );

        // Live next link even without a total banner.
        let plain_val = json!({"items": []});
        let stop_pager_only = collect_stop_reason(20, &plain_val, Some(&pager));
        assert!(
            stop_pager_only.contains("pagination detected")
                && !stop_pager_only.contains("feed exhausted"),
            "unexpected stop: {stop_pager_only}"
        );

        // Genuine infinite-scroll feed with no pager and no unreached total.
        let stop_exhausted = collect_stop_reason(20, &plain_val, None);
        assert_eq!(
            stop_exhausted,
            "feed exhausted (no new items after 2 scrolls)"
        );
    }
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

    /// Run the generic auto-extract path in node over a mock list of `rows`
    /// sponsored items whose links are Google redirect wrappers; `dup` makes
    /// row 1 repeat row 0's destination.
    fn auto_extract_in_node(limit: usize, rows: usize, dup: bool) -> serde_json::Value {
        use std::io::Write;
        let mock = format!(
            "const ROWS={rows},DUP={dup};\
             const style={{display:'block',visibility:'visible',opacity:'1'}};\
             const view={{getComputedStyle(){{return style}}}};\
             const doc={{defaultView:view}};\
             const el=(tag,props)=>Object.assign({{nodeType:1,tagName:tag,children:[],childNodes:[],offsetParent:{{}},ownerDocument:doc,\
               querySelector(){{return null}},querySelectorAll(){{return []}},matches(){{return false}},getAttribute(){{return null}}}},props);\
             const rows=[];\
             for(let i=0;i<ROWS;i++){{const n=DUP&&i===1?0:i;\
               const a=el('A',{{href:'https://www.google.com/url?q='+encodeURIComponent('https://shop.example/item/'+n),hostname:'www.google.com',innerText:'Organic looking product number '+i,matches(){{return true}}}});\
               const t={{nodeType:3,textContent:'Sponsored $'+(10+i)+' great value product listing text '+i}};\
               const s=el('SPAN',{{innerText:t.textContent,childNodes:[t]}});\
               rows.push(el('LI',{{children:[a,s],childNodes:[a,s],innerText:a.innerText+' '+s.innerText,querySelectorAll(q){{return q==='a[href]'?[a]:[]}}}}));}}\
             const list=el('UL',{{children:rows,childNodes:rows}});\
             globalThis.location={{hostname:'shop.test',href:'http://shop.test/list',pathname:'/list',origin:'http://shop.test',search:''}};\
             globalThis.document={{body:{{innerText:''}},title:'List',querySelector(){{return null}},querySelectorAll(q){{return q==='*'?[list]:[]}}}};"
        );
        let script = format!(
            "{mock}\nconsole.log({});",
            super::auto_extract_expr(limit, false)
        );
        let mut child = std::process::Command::new("node")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("node is required for injected-script tests");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    #[test]
    fn envelope_counters_describe_the_returned_items() {
        // Ten sponsored, redirect-wrapped rows, row 1 duplicating row 0,
        // limit=5. The counters used to run over all ten rows before the cap
        // ("10 of 4 items are paid placements") and dedupe ran after the cap,
        // so only 4 items came back and the cap note stayed silent.
        let val = auto_extract_in_node(5, 10, true);
        assert_eq!(val["count"], 5, "{val}");
        assert_eq!(val["items"].as_array().map(Vec::len), Some(5), "{val}");
        assert_eq!(val["sponsored"], 5, "{val}");
        assert_eq!(val["redirects_unwrapped"], 5, "{val}");
        assert_eq!(val["duplicates_removed"], 1, "{val}");
        assert_eq!(
            val["items"][0]["url"], "https://shop.example/item/0",
            "{val}"
        );
        let notes = super::auto_extract_notes(&val, 5);
        assert!(
            notes.contains("5 of 5 items are paid placements"),
            "{notes}"
        );
        assert!(notes.contains("item cap was reached"), "{notes}");
        // Under the cap: no cap note, counters cover every item.
        let val = auto_extract_in_node(50, 6, false);
        assert_eq!(
            (val["count"].as_u64(), val["sponsored"].as_u64()),
            (Some(6), Some(6))
        );
        assert!(!super::auto_extract_notes(&val, 50).contains("item cap"));
    }

    #[test]
    fn the_cap_note_uses_the_clamped_limit_and_says_so() {
        // limit is clamped to 500 in the script; comparing 500 items against
        // the requested 1000 hid the truncation.
        let val = serde_json::json!({"count":500,"items":[]});
        let notes = super::auto_extract_notes(&val, 1000);
        assert!(notes.contains("item cap was reached"), "{notes}");
        assert!(notes.contains("500"), "{notes}");
        let val = serde_json::json!({"count":40,"items":[]});
        assert!(!super::auto_extract_notes(&val, 1000).contains("item cap"));
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
