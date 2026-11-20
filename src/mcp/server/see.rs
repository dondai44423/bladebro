//! `see` + `logs` handlers — the reading surface.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;

use super::artifact_hint;
use super::extract::{handle_auto_extract, handle_template_extract};
use super::resolve::miss_diag_note;

/// G08: detect that the current page is a PDF document (Chrome's viewer).
/// Three observations, so the guidance stays honest about what was seen:
/// content type, the viewer's embed element, or a .pdf URL.
const PDF_STATE_EXPR: &str = "(()=>{try{const ct=(document.contentType||'').toLowerCase();if(ct==='application/pdf')return 'pdf';const emb=document.querySelector('embed[type=\"application/pdf\"],embed[type*=\"pdf\" i]');if(emb)return 'pdf-embed';const u=(location.href||'').split(/[?#]/)[0].toLowerCase();if(u.endsWith('.pdf'))return 'pdf-url';return '';}catch(e){return '';}})()";

/// `see changes=true format=json`. An unanswered read carries the last
/// answered url and empty lists, which reads exactly like "nothing changed";
/// `"unanswered":true` (present only then) says nothing was observed.
fn changes_json(delta: &crate::page::PageDelta) -> Value {
    let mut value = json!({"url":delta.url,"title":delta.title,"phase":delta.phase,"navigated":delta.navigated,
        "added":delta.added.iter().map(|e| json!({"ref":e.ref_id,"role":e.raw.role,"name":e.raw.name})).collect::<Vec<_>>(),
        "removed":delta.removed,"changed":delta.changed.iter().map(|(r,c)| json!({"ref":r,"value":c.value,"disabled":c.disabled,"checked":c.checked})).collect::<Vec<_>>(),"rebound":delta.rebound});
    if delta.unanswered {
        value["unanswered"] = json!(true);
    }
    value
}

/// Evaluate [`PDF_STATE_EXPR`]; best-effort (None on any hiccup).
async fn pdf_document_state(page: &Page) -> Option<String> {
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(json!({ "expression": PDF_STATE_EXPR, "returnByValue": true })),
        )
        .await
        .ok()?;
    let s = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// G08: the honest read contract for a PDF page. The text is not in the DOM;
/// the supported paths are the download (an artifact the host reads with its
/// own tools) and vision. The driver does not claim text extraction - a PDF
/// parser is a packaging/maintenance cost that buys the host nothing over
/// handing it the bytes.
fn pdf_read_message(url: &str) -> String {
    format!(
        "this page is a PDF document: {url} is rendered by Chrome's PDF viewer and its text is NOT in the page DOM, so text reads return nothing. Supported paths: act download url=\"{url}\" saves the PDF file and returns its path for your own reader, or vision for a screenshot of the viewer. Scanned PDFs have no text layer anywhere - that is the document, not the read; a viewer load error does not affect the download path."
    )
}

/// `see` as a tool call (MCP and CLI/daemon): `url=` navigates first, then
/// reads - the `bladebro see <url>` contract, and what `retry_safe` already
/// assumes. The MCP dispatch used to skip this and silently read whatever page
/// was current. Steps and act aliases call [`handle_see`] directly because
/// they navigate on url= themselves (batch/run see steps, act pre-navigation
/// for extract/paginate).
pub async fn handle_see_tool(args: &Value, page: &mut Page) -> Result<String> {
    super::validate::see(args)?;
    if let Some(url) = args
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty() && !u.starts_with("--"))
    {
        // Manual-control pause: a see with a url navigates first.
        if crate::realbrowser::input_paused() {
            return Err(crate::realbrowser::paused_error());
        }
        if let Some(block) = args
            .get("block")
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty())
        {
            let mask = page.set_block_classes(block).await?;
            page.remember_block_choice(url, block, mask != 0);
        }
        page.navigate(url).await?;
    }
    handle_see(args, page).await
}

pub async fn handle_see(args: &Value, page: &mut Page) -> Result<String> {
    super::validate::see(args)?;
    // NOTE (4.4.0): `see` deliberately does NOT reset the act-loop counter.
    // It used to, which kept the dominant act→see→act→see loop permanently
    // fresh: every post-read act got the full budget and re-sent a delta the
    // read had just delivered. A full `see` leaves the agent holding the
    // page state, so the next act's budget-capped delta is the correct,
    // cheap shape. Resets remain where prior context is truly invalidated:
    // navigation and act-error recapture (both in act.rs). `state
    // op=compress mode=off` restores always-full responses.
    let budget = args.get("budget").and_then(|b| b.as_u64()).unwrap_or(8000) as usize;
    let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("");
    let want_content = args
        .get("content")
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    let find = args.get("find").and_then(|f| f.as_str()).unwrap_or("");
    let extract = args.get("extract").and_then(|e| e.as_str()).unwrap_or("");
    // W2: format=json — pure parseable output for extract= and artifact=
    // reads (the JSON.parse pain point). Every other mode stays text.
    let format = args.get("format").and_then(|f| f.as_str()).unwrap_or("");
    if !matches!(format, "" | "text" | "json") {
        return Err(BladeError::Other(format!(
            "unknown format: {format} (use 'text' or 'json')"
        )));
    }
    let json_out = format == "json";
    let scope = args.get("scope").and_then(|s| s.as_str()).unwrap_or("");
    let logs = args.get("logs").and_then(|l| l.as_str()).unwrap_or("");
    let template = args.get("template").cloned();
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
    // Distinguishes "fetch the whole discussion" defaults (Reddit comments)
    // from a caller-chosen cap.
    let limit_explicit = args.get("limit").is_some();
    let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("");

    // Artifact read-back: paged access to an offloaded payload for clients
    // without filesystem access. Paths are restricted to the artifacts dir;
    // binary artifacts (png/pdf) are refused with a pointer instead.
    let artifact = args.get("artifact").and_then(|a| a.as_str()).unwrap_or("");
    if !artifact.is_empty() {
        let offset = args.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(20000) as usize;
        if args.get("items").and_then(Value::as_bool).unwrap_or(false) {
            return crate::artifacts::read_artifact_items(
                artifact,
                offset,
                args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize,
            );
        }
        return if json_out {
            crate::artifacts::read_artifact_json(artifact, offset, limit)
        } else {
            crate::artifacts::read_artifact(artifact, offset, limit)
        };
    }
    if args
        .get("changes")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let delta = page.recapture().await?;
        return if json_out {
            let value = changes_json(&delta).to_string();
            if value.len() > 12000 {
                let path = crate::artifacts::write_artifact(&value, "json")?;
                Ok(json!({"artifact":path,"bytes":value.len(),"next_offset":0}).to_string())
            } else {
                Ok(value)
            }
        } else {
            Ok(page.delta_view(&delta, budget))
        };
    }
    // format=json is only meaningful for structured reads; anything else is
    // refused instead of silently returning text.
    if json_out && extract.is_empty() && logs.is_empty() && find.is_empty() {
        return Err(BladeError::Other(
            "format=json applies to extract= (auto/json/links/forms), artifact=, changes=true, find= and logs= (console/network/xhr) reads - this mode returns text; drop format or use a structured read".into(),
        ));
    }

    // mode=content: clean markdown extraction for reading. No refs, no
    // actionability markers — just the page text as structured markdown.
    // Headings, links, lists, code blocks, tables preserved.
    if mode == "content" {
        // scope=eN: ONE element's subtree. scope used to be silently
        // ignored here (a caller asking for scope="main" got the whole
        // page); it is honored now and errors loudly when the scope is
        // not a ref.
        if !scope.is_empty() {
            let _ = page.ensure_ref(scope).await;
            match page.model().element(scope).cloned() {
                Some(e) => {
                    let md = page
                        .markdown_scoped(budget, &e.raw.sig, &e.raw.frame)
                        .await?;
                    if md.is_empty() {
                        return Err(BladeError::Other(format!(
                            "scope {scope}: element found but has no readable text (or it went stale mid-read - recapture with see)"
                        )));
                    }
                    return Ok(md);
                }
                None => {
                    return Err(BladeError::Other(format!(
                        "scope \"{scope}\" is not a known ref - pass a ref id from see (e.g. e5), or drop scope and use find=\"<text>\" for a cheap existence check"
                    )));
                }
            }
        }
        let md = page.markdown(budget).await?;
        if md.is_empty() {
            // G08: a PDF page reads as empty because its text lives in the
            // viewer plugin, not the DOM. Say exactly that and name the
            // supported paths instead of a generic SPA suggestion.
            if pdf_document_state(page).await.is_some() {
                let url = page.model().url();
                return Ok(pdf_read_message(url));
            }
            return Ok("page has no text content (may be a SPA that hasn't rendered — try waiting, or use mode=model for interactive elements)".into());
        }
        return Ok(md);
    }

    // mode=outline: just headings. Ultra-minimal for "what's on this page".
    if mode == "outline" {
        let out = page.outline().await?;
        return Ok(out);
    }

    // V8: logs — console (injection hook), network (completed ring), or xhr (deduplicated API ring).
    if !logs.is_empty() {
        let out = if json_out {
            handle_logs_json(page, logs).await?
        } else {
            handle_logs(page, logs).await?
        };
        if args.get("clear").and_then(Value::as_bool).unwrap_or(false) {
            match logs {
                "network" => page.clear_network_log(),
                "xhr" => page.clear_xhr_log(),
                _ => page.clear_console_log().await?,
            }
        }
        return Ok(out);
    }

    // V9: template extraction — structured data in ONE call.
    if extract == "json" {
        let tpl = template.ok_or_else(|| BladeError::Other(
            "extract=json requires 'template': {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css|css@attr\"}}} (text reads are rendered - {\"sel\":\"css\",\"raw\":true} reads hidden text). For template-free structured extraction use extract=auto.".into()
        ))?;
        return handle_template_extract(page, &tpl, limit, json_out).await;
    }

    // V21: auto-extract — deterministic structural analysis. Finds the DOM
    // container with the most repeated structurally-similar children (the
    // "main list": products, articles, results), extracts per-item fields,
    // and INFERS field names by content type (title/link/image/price/date).
    // No template, no LLM.
    if extract == "auto" {
        if args
            .get("preview")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return super::extract::auto_preview(page).await;
        }
        return handle_auto_extract(
            page,
            limit,
            limit_explicit,
            json_out,
            args.get("sort").and_then(Value::as_str),
        )
        .await;
    }

    // M11: find — search all actionable elements by text, return matches with refs.
    if !find.is_empty() {
        let matches = crate::action::find_by_text(page.cdp_ref(), find, None, false).await?;
        if matches.is_empty() {
            // Explain the miss: matches that exist but were not returned
            // (hidden / unreachable) are the difference between "the
            // control is not there" and "it is there but not visible from
            // here" - one flat "not found" sent an agent hunting through
            // raw eval for a desktop-hidden trigger.
            let diag = crate::action::find_miss_diag(page.cdp_ref(), find)
                .await
                .ok();
            let diag_note = miss_diag_note(diag);
            // M11 contract is full-page search: when no ACTIONABLE element
            // matches, locate the text itself - the deepest containing
            // element with its container chain plus the actionables inside
            // it. "Leftover body text" then explains WHERE it lives (the
            // header pill, a draft editor, a closed confirm row) instead of
            // costing the agent an ancestor-walk to find out.
            let loc = crate::action::locate_text(page.cdp_ref(), find).await;
            if json_out {
                let mut payload = json!({
                    "query": find,
                    "count": 0,
                    "matches": [],
                });
                if !diag_note.is_empty() {
                    payload["diag"] = json!(diag_note.trim());
                }
                if let Some(loc) = loc {
                    payload["text_match"] = json!({
                        "snippet": loc.snippet,
                        "container": loc.desc,
                        "nearby_actionables": loc.near,
                    });
                }
                return super::bounded_json(&payload);
            }
            if let Some(loc) = loc {
                let near = if loc.near.is_empty() {
                    String::new()
                } else {
                    format!(" (nearby actionables: {})", loc.near.join(", "))
                };
                let where_ = if loc.desc.is_empty() {
                    String::new()
                } else {
                    format!(" - text found in {}{}", loc.desc, near)
                };
                return Ok(format!(
                    "no actionable elements matching \"{find}\"{diag_note}, but text present in page{where_}:\n  \"…{}…\"\n  (see content=true to read full context)",
                    loc.snippet
                ));
            }
            return Ok(format!("no elements matching \"{find}\" found{diag_note}"));
        }
        if json_out {
            let rows: Vec<Value> = matches
                .iter()
                .map(|m| {
                    let ref_id = page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame);
                    json!({
                        "ref": ref_id,
                        "role": m.role,
                        "name": m.name,
                        "ctx": m.ctx,
                        "score": m.score,
                    })
                })
                .collect();
            return super::bounded_json(&json!({
                "query": find,
                "count": rows.len(),
                "matches": rows,
            }));
        }
        let mut out = format!(
            "find \"{}\": {} match{}\n",
            find,
            matches.len(),
            if matches.len() > 1 { "es" } else { "" }
        );
        for m in &matches {
            let ref_id = page.model_mut().adopt(&m.sig, &m.role, &m.name, &m.frame);
            let ctx = if m.ctx.is_empty() {
                String::new()
            } else {
                format!(" (in {})", m.ctx)
            };
            out.push_str(&format!(
                "{} {} \"{}\"{} (score: {})\n",
                ref_id, m.role, m.name, ctx, m.score
            ));
        }
        return Ok(out);
    }

    // M12: extract — structured data extraction (links/forms).
    if !extract.is_empty() {
        let expr = format!(
            "{}({}, {})",
            include_str!("../../page/js/extract_basic.js"),
            serde_json::to_string(extract)?,
            limit,
        );
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
            let message = exc
                .pointer("/exception/description")
                .and_then(Value::as_str)
                .or_else(|| exc.get("text").and_then(Value::as_str))
                .unwrap_or("JavaScript failed");
            return Err(BladeError::Other(format!(
                "extract {extract} failed: {}",
                crate::platform::truncate_utf8(message, 240)
            )));
        }
        let json_str = res
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| BladeError::Other(format!("extract {extract}: browser returned no data; retry the read after the page settles")))?;
        // V10: offload large extracts to a file. W2: format=json returns a
        // parseable payload (bare when it fits, an artifact envelope when
        // not) instead of the truncating preview.
        if json_out {
            if json_str.len() > 6000 {
                let path = crate::artifacts::write_artifact(json_str, "json")?;
                return Ok(serde_json::json!({
                    "artifact": path,
                    "bytes": json_str.len(),
                    "next_offset": 0,
                })
                .to_string());
            }
            return Ok(json_str.to_string());
        }
        if json_str.len() > 6000 {
            let path = crate::artifacts::write_artifact(json_str, "json")?;
            // Item count from the array itself (the old href-count claimed
            // ~0 items for form lists).
            let count = serde_json::from_str::<Value>(json_str)
                .ok()
                .and_then(|v| {
                    v.as_array()
                        .or_else(|| v.get("data").and_then(Value::as_array))
                        .map(|a| a.len())
                })
                .unwrap_or(0);
            return Ok(format!(
                "extract {extract} (~{count} items, {} bytes)\npreview: {}…\n{}",
                json_str.len(),
                json_str.chars().take(600).collect::<String>(),
                artifact_hint(&path)
            ));
        }
        return Ok(format!("extract {extract}:\n{json_str}"));
    }

    // M12: scope — return the text content of one element's subtree.
    if !scope.is_empty() {
        let text = crate::action::read_text(page.cdp_ref(), page.model(), scope).await?;
        let el = page.model().element(scope);
        let (role, name) = el
            .map(|e| (e.raw.role.clone(), e.raw.name.clone()))
            .unwrap_or_default();
        return Ok(format!("scope {scope} {role} \"{}\":\n{}", name, text));
    }

    // Recapture to get fresh state.
    let _delta = page.recapture().await?;
    let mut out = if filter.is_empty() {
        page.view(budget)
    } else {
        page.view_filtered(budget, filter)
    };
    // Auto-include content when the page has very few actionable elements —
    // the agent almost certainly needs the text, not just the empty element list.
    let auto_content = !want_content && page.model().actionables() <= 3 && filter.is_empty();
    if want_content || auto_content {
        let content_budget = if auto_content {
            budget
        } else {
            budget.min(4000)
        };
        let content = page.content(content_budget).await?;
        if !content.is_empty() {
            out.push_str("\n--- page content ---\n");
            out.push_str(&content);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Structured JSON form of `see logs=console|network|xhr format=json`.
/// Offloads payloads past 12 KB into a JSON artifact envelope so responses stay
/// bounded and strictly parseable.
async fn handle_logs_json(page: &mut Page, kind: &str) -> Result<String> {
    let value = match kind {
        "xhr" => {
            let entries = page.xhr_log();
            serde_json::to_string(&xhr_entries_json(&entries))?
        }
        "network" => {
            let entries = page.network_log();
            let mut bad = Vec::new();
            let mut good = Vec::new();
            for e in &entries {
                let row = json!({
                    "method": e.method,
                    "url": e.url,
                    "status": e.status,
                    "error": e.error,
                });
                if e.error.is_some() || e.status >= 400 {
                    bad.push(row);
                } else {
                    good.push(row);
                }
            }
            bad.extend(good);
            serde_json::to_string(&bad)?
        }
        "console" => {
            if crate::realbrowser::real_lane() {
                json!({
                    "data": [],
                    "note": "console: unavailable on the real-browser lane (page injection is off)"
                })
                .to_string()
            } else {
                let entries = page.console_log().await?;
                let arr = entries.as_array().cloned().unwrap_or_default();
                let mut errors = Vec::new();
                let mut warnings = Vec::new();
                let mut rest = Vec::new();
                for e in arr {
                    let level = e.get("l").and_then(Value::as_str).unwrap_or("");
                    let row = json!({
                        "level": level,
                        "message": e.get("m").and_then(Value::as_str).unwrap_or(""),
                        "timestamp": e.get("t").cloned().unwrap_or(Value::Null),
                    });
                    match level {
                        "error" | "exception" | "unhandledrejection" => errors.push(row),
                        "warn" => warnings.push(row),
                        _ => rest.push(row),
                    }
                }
                errors.extend(warnings);
                errors.extend(rest);
                serde_json::to_string(&errors)?
            }
        }
        _ => {
            return Err(BladeError::Other(format!(
                "unknown logs kind: {kind} (use 'console', 'network', or 'xhr')"
            )))
        }
    };
    if value.len() > 12000 {
        let path = crate::artifacts::write_artifact(&value, "json")?;
        Ok(json!({"artifact":path,"bytes":value.len(),"next_offset":0}).to_string())
    } else {
        Ok(value)
    }
}

/// V8: `see logs=console|network` — introspection for agent
/// self-diagnosis. Errors/warnings first. Artifact-offloaded
/// when the log is long.
pub async fn handle_logs(page: &mut Page, kind: &str) -> Result<String> {
    match kind {
        "console" => {
            if crate::realbrowser::real_lane() {
                return Ok(
                    "console: unavailable on the real-browser lane (page injection is off)"
                        .to_string(),
                );
            }
            let entries = page.console_log().await?;
            let arr = entries.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                return Ok("console: (empty)".to_string());
            }
            // Errors first, then warnings, then the rest.
            let mut errors = Vec::new();
            let mut warnings = Vec::new();
            let mut rest = Vec::new();
            for e in &arr {
                let level = e.get("l").and_then(|l| l.as_str()).unwrap_or("");
                let msg = e.get("m").and_then(|m| m.as_str()).unwrap_or("");
                let line = format!("{level}: {msg}");
                match level {
                    "error" | "exception" | "unhandledrejection" => errors.push(line),
                    "warn" => warnings.push(line),
                    _ => rest.push(line),
                }
            }
            let mut out = format!("console ({} entries):\n", arr.len());
            for l in errors
                .iter()
                .chain(warnings.iter())
                .chain(rest.iter())
                .take(30)
            {
                out.push_str(l);
                out.push('\n');
            }
            if arr.len() > 30 {
                let json_str = serde_json::to_string_pretty(&arr)?;
                let path = crate::artifacts::write_artifact(&json_str, "json")?;
                out.push_str(&format!("…and {} more → {path}\n", arr.len() - 30));
            }
            Ok(out)
        }
        "network" => {
            let entries = page.network_log();
            if entries.is_empty() {
                return Ok("network: (no completed requests)".to_string());
            }
            // Failures and 4xx/5xx first — that's what the agent
            // is debugging.
            let mut bad = Vec::new();
            let mut good = Vec::new();
            for e in &entries {
                let status_str = if e.status > 0 {
                    e.status.to_string()
                } else {
                    e.error.clone().unwrap_or_else(|| "ERR".into())
                };
                let short_url = if e.url.len() > 90 {
                    format!("{}\u{2026}", crate::platform::truncate_utf8(&e.url, 87))
                } else {
                    e.url.clone()
                };
                let line = format!("{} {} {}", e.method, status_str, short_url);
                if e.error.is_some() || e.status >= 400 {
                    bad.push(line);
                } else {
                    good.push(line);
                }
            }
            let mut out = format!("network ({} requests):\n", entries.len());
            for l in bad.iter().chain(good.iter()).take(30) {
                out.push_str(l);
                out.push('\n');
            }
            if entries.len() > 30 {
                out.push_str(&format!("…and {} more\n", entries.len() - 30));
            }
            Ok(out)
        }
        "xhr" => {
            let entries = page.xhr_log();
            format_xhr_log(&entries)
        }
        _ => Err(BladeError::Other(format!(
            "unknown logs kind: {kind} (use 'console', 'network', or 'xhr')"
        ))),
    }
}

fn redacted_header_names(headers: &[(String, String)]) -> Vec<String> {
    let mut names: Vec<String> = headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn xhr_entries_json(entries: &[crate::page::XhrEntry]) -> Vec<Value> {
    entries
        .iter()
        .map(|e| {
            json!({
                "method": e.method,
                "url": e.url,
                "status": e.status,
                "done": e.done,
                "error": e.error,
                "header_names": redacted_header_names(&e.headers),
            })
        })
        .collect()
}

fn format_xhr_log(entries: &[crate::page::XhrEntry]) -> Result<String> {
    if entries.is_empty() {
        return Ok("xhr: (no XHR/fetch requests observed)".to_string());
    }
    let mut bad = Vec::new();
    let mut good = Vec::new();
    let mut pending = Vec::new();
    for e in entries {
        let status_str = if !e.done {
            "PENDING".to_string()
        } else if e.status > 0 {
            e.status.to_string()
        } else {
            e.error.clone().unwrap_or_else(|| "ERR".into())
        };
        let names = redacted_header_names(&e.headers);
        let hdr_note = if names.is_empty() {
            String::new()
        } else {
            format!(" [headers: {}]", names.join(", "))
        };
        // Keep full URLs untruncated: GraphQL operations, cursors, and query
        // filters are the exact state an agent inspects logs=xhr to read.
        let line = format!("{} {} {}{}", e.method, status_str, e.url, hdr_note);
        if !e.done {
            pending.push(line);
        } else if e.error.is_some() || e.status >= 400 {
            bad.push(line);
        } else {
            good.push(line);
        }
    }
    let mut out = format!("xhr ({} endpoints):\n", entries.len());
    for l in bad.iter().chain(good.iter()).chain(pending.iter()).take(30) {
        out.push_str(l);
        out.push('\n');
    }
    if entries.len() > 30 {
        let json_str = serde_json::to_string_pretty(&xhr_entries_json(entries))?;
        let path = crate::artifacts::write_artifact(&json_str, "json")?;
        out.push_str(&format!("…and {} more → {path}\n", entries.len() - 30));
    }
    Ok(out)
}

#[cfg(test)]
mod xhr_log_tests {
    use super::*;
    use crate::page::XhrEntry;

    #[test]
    fn xhr_log_orders_failures_preserves_full_query_and_redacts_secret_values() {
        let long_url = "https://api.example.com/graphql?operationName=ListItems&variables=%7B%22cursor%22%3A%22abc1234567890def%22%2C%22limit%22%3A50%7D";
        let entries = vec![
            XhrEntry {
                id: "1".into(),
                method: "GET".into(),
                url: long_url.into(),
                status: 200,
                done: true,
                error: None,
                headers: vec![
                    (
                        "Authorization".into(),
                        "Bearer SECRET_TOKEN_DO_NOT_LEAK".into(),
                    ),
                    ("X-CSRF-Token".into(), "csrf_secret_999".into()),
                ],
            },
            XhrEntry {
                id: "2".into(),
                method: "POST".into(),
                url: "https://api.example.com/v1/submit".into(),
                status: 429,
                done: true,
                error: None,
                headers: vec![],
            },
            XhrEntry {
                id: "3".into(),
                method: "GET".into(),
                url: "https://api.example.com/v1/poll".into(),
                status: 0,
                done: false,
                error: None,
                headers: vec![],
            },
        ];
        let text = format_xhr_log(&entries).unwrap();
        assert!(
            text.contains(long_url),
            "full query string must not be truncated: {text}"
        );
        assert!(text.contains("[headers: authorization, x-csrf-token]"));
        assert!(
            !text.contains("SECRET_TOKEN_DO_NOT_LEAK"),
            "header values must never leak"
        );
        assert!(
            !text.contains("csrf_secret_999"),
            "csrf values must never leak"
        );
        let pos_429 = text.find("POST 429").unwrap();
        let pos_200 = text.find("GET 200").unwrap();
        let pos_pending = text.find("GET PENDING").unwrap();
        assert!(pos_429 < pos_200 && pos_200 < pos_pending);

        let json_rows = serde_json::to_string(&xhr_entries_json(&entries)).unwrap();
        assert!(!json_rows.contains("SECRET_TOKEN_DO_NOT_LEAK"));
        assert!(json_rows.contains("\"header_names\":[\"authorization\",\"x-csrf-token\"]"));
    }

    #[test]
    fn validate_accepts_structured_json_reads_and_rejects_text_only_modes() {
        for ok in [
            json!({"logs": "xhr"}),
            json!({"logs": "xhr", "format": "json"}),
            json!({"logs": "network", "format": "json"}),
            json!({"logs": "console", "format": "json"}),
            json!({"find": "Submit", "format": "json"}),
            json!({"url": "https://example.com", "block": "images,fonts", "mode": "content"}),
        ] {
            crate::mcp::server::validate::see(&ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            json!({"mode": "content", "format": "json"}),
            json!({"mode": "outline", "format": "json"}),
            json!({"mode": "model", "format": "json"}),
        ] {
            assert!(
                crate::mcp::server::validate::see(&bad).is_err(),
                "expected error for {bad}"
            );
        }
    }
}

#[cfg(test)]
mod pdf_read_tests {
    use super::*;

    #[test]
    fn pdf_state_expr_detects_all_three_shapes() {
        assert!(PDF_STATE_EXPR.contains("application/pdf"));
        assert!(PDF_STATE_EXPR.contains(".pdf"));
        assert!(PDF_STATE_EXPR.contains("contentType"));
        // node --check (skipped when node is unavailable).
        let dir = std::env::temp_dir().join("bladebro-pdf-tests");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("pdf-state.js");
        if std::fs::write(&path, PDF_STATE_EXPR).is_err() {
            return;
        }
        let out = match std::process::Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
        {
            Ok(o) => o,
            Err(_) => return,
        };
        assert!(
            out.status.success(),
            "PDF_STATE_EXPR must parse as JS:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn pdf_read_message_names_paths_and_provenance() {
        let m = pdf_read_message("https://shop.example/manual.pdf");
        assert!(m.contains("https://shop.example/manual.pdf"));
        assert!(m.contains("act download"));
        assert!(m.contains("vision"));
        assert!(m.contains("NOT in the page DOM"));
        assert!(!m.contains('\u{2014}'), "no em-dash in agent-facing text");
        // A viewer error must not invalidate the download path claim.
        assert!(m.contains("does not affect the download path"));
    }
}

#[cfg(test)]
mod changes_json_tests {
    use super::changes_json;
    use crate::page::{LivePageModel, PageCapture};

    /// An unanswered read showed the old url, phase "loading" and empty
    /// lists with no other signal: indistinguishable from "no change".
    #[test]
    fn unanswered_read_is_flagged_and_answered_reads_are_unchanged() {
        let page = |url: &str| PageCapture {
            url: url.into(),
            title: "Fixture".into(),
            ready_state: "complete".into(),
            muts: 0,
            elements: Vec::new(),
        };
        let mut lpm = LivePageModel::new();
        lpm.ingest(page("https://a.test/"));
        let quiet = changes_json(&lpm.ingest(page("https://a.test/")));
        assert!(quiet.get("unanswered").is_none(), "{quiet}");
        let busy = changes_json(&lpm.ingest(PageCapture::unanswered()));
        assert_eq!(busy["unanswered"], true, "{busy}");
        assert_eq!(busy["url"], "https://a.test/");
    }
}
