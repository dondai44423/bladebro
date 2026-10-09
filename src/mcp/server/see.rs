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

pub async fn handle_see(args: &Value, page: &mut Page) -> Result<String> {
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
        return crate::artifacts::read_artifact(artifact, offset, limit);
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

    // V8: logs — console (injection hook) or network (tracker ring).
    if !logs.is_empty() {
        return handle_logs(page, logs).await;
    }

    // V9: template extraction — structured data in ONE call.
    if extract == "json" {
        let tpl = template.ok_or_else(|| BladeError::Other(
            "extract=json requires 'template': {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css|css@attr\"}}} (text reads are rendered - {\"sel\":\"css\",\"raw\":true} reads hidden text). For template-free structured extraction use extract=auto.".into()
        ))?;
        return handle_template_extract(page, &tpl, limit).await;
    }

    // V21: auto-extract — deterministic structural analysis. Finds the DOM
    // container with the most repeated structurally-similar children (the
    // "main list": products, articles, results), extracts per-item fields,
    // and INFERS field names by content type (title/link/image/price/date).
    // No template, no LLM.
    if extract == "auto" {
        return handle_auto_extract(page, limit, limit_explicit).await;
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
            let diag_note = miss_diag_note(
                crate::action::find_miss_diag(page.cdp_ref(), find)
                    .await
                    .ok(),
            );
            // M11 contract is full-page search: when no ACTIONABLE element
            // matches, locate the text itself - the deepest containing
            // element with its container chain plus the actionables inside
            // it. "Leftover body text" then explains WHERE it lives (the
            // header pill, a draft editor, a closed confirm row) instead of
            // costing the agent an ancestor-walk to find out.
            if let Some(loc) = crate::action::locate_text(page.cdp_ref(), find).await {
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
        let expr = match extract {
            "links" => {
                r#"(()=>{const links=[...document.querySelectorAll('a[href]')];return JSON.stringify(links.map(a=>({text:(a.textContent||'').trim().slice(0,80),href:a.href})).filter(l=>l.text||l.href));})()"#
            }
            "forms" => {
                r#"(()=>{
function extractForms(doc){
const forms=[...doc.querySelectorAll('form')];
return forms.map(f=>({action:f.action,method:(f.method||'get').toLowerCase(),fields:[...f.elements].filter(e=>e.tagName!=='FIELDSET'&&e.tagName!=='BUTTON').map(e=>{
// Label resolution priority: <label for=id> > aria-label > aria-labelledby > placeholder > wrapping <label> > preceding text
var label='';
if(e.id){var lbl=doc.querySelector('label[for="'+e.id+'"]');if(lbl)label=lbl.textContent.trim().slice(0,60);}
if(!label&&e.getAttribute('aria-label'))label=e.getAttribute('aria-label').trim().slice(0,60);
if(!label&&e.getAttribute('aria-labelledby')){var lb=doc.getElementById(e.getAttribute('aria-labelledby'));if(lb)label=lb.textContent.trim().slice(0,60);}
if(!label&&e.placeholder)label=e.placeholder.trim().slice(0,60);
if(!label&&e.closest('label'))label=e.closest('label').textContent.trim().slice(0,60);
return{tag:e.tagName.toLowerCase(),type:e.type||null,name:e.name||'',label:label};
})}));
}
var allForms=extractForms(document);
// Search iframes too (W3Schools TryIt etc render forms in iframes).
try{for(const ifr of document.querySelectorAll('iframe')){try{if(ifr.contentDocument){allForms=allForms.concat(extractForms(ifr.contentDocument));}}catch(e){}}
}catch(e){}
return JSON.stringify(allForms);
})()"#
            }
            _ => {
                return Err(BladeError::Other(format!(
                    "unknown extract type: {extract} (use 'links' or 'forms')"
                )))
            }
        };
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
        let json_str = res
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("[]");
        // V10: offload large extracts to a file.
        if json_str.len() > 6000 {
            let path = crate::artifacts::write_artifact(json_str, "json")?;
            let count = json_str.matches("href").count();
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

/// V8: `see logs=console|network` — introspection for agent
/// self-diagnosis. Errors/warnings first. Artifact-offloaded
/// when the log is long.
pub async fn handle_logs(page: &mut Page, kind: &str) -> Result<String> {
    match kind {
        "console" => {
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
        _ => Err(BladeError::Other(format!(
            "unknown logs kind: {kind} (use 'console' or 'network')"
        ))),
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
