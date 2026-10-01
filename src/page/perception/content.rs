//! Read modes: visible text (`capture_content`), markdown
//! (`capture_markdown` + scoped), heading outline. The markdown script
//! lives in `../js/markdown.js`. Split from the `perception` core.

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;

use super::JS_PREAMBLE;

/// Extract visible text content from the page body, excluding scripts,
/// styles, and hidden elements. Returns at most `budget` characters.
///
/// Uses `innerText` which respects CSS visibility (unlike `textContent`).
/// The text is collapsed to single spaces and truncated to the budget.
pub async fn capture_content(cdp: &CdpSession, budget: usize) -> Result<String> {
    let expr = r#"(()=>{const d=document;if(!d||!d.body)return'';const c=d.body.cloneNode(true);c.querySelectorAll("script,style,noscript,svg,template,link,meta,[class*='dfp'],[id*='dfp'],[class*='advert'],[id*='advert'],[class*='sponsored'],[data-sponsored],[data-ad],[data-ad-slot],[data-ad-client],[data-google-query-id],ins.adsbygoogle,[id*='google_ads'],[class*='ad-container'],[class*='ad-wrapper'],[class*='ad-slot'],[class*='ad-banner'],[class*='ad-feedback'],[class*='adBanner'],[class*='adSense'],[class*='adBlock'],[class*='ad-label'],[class*='ads-label'],[class*='ads-container'],[class*='mol-ads'],[class*='promoted'],[aria-label*='advertisement' i]").forEach(e=>e.remove());c.querySelectorAll('select').forEach(s=>{const ts=[...s.options].map(o=>(o.label||o.text||'').trim()).filter(Boolean).slice(0,12);if(ts.length){const extra=Math.max(0,s.options.length-ts.length);s.replaceChildren(document.createTextNode('['+ts.join(' | ')+(extra?' | +'+extra+' more':'')+'] '));}});const t=(c.innerText||c.textContent||'').replace(/\s+/g,' ').trim();return t.slice(0,__BUDGET__);})()"#.replace("__BUDGET__", &budget.to_string());
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;

    let text = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(text.to_string())
}

/// Semantic content extraction: find the main content area and convert it
/// to clean, token-efficient markdown. Strips navigation, footers, ads,
/// scripts, and other noise. Preserves headings, paragraphs, links, lists,
/// code blocks, tables, blockquotes, and images.
///
/// Main content detection: semantic HTML5 (<main>, <article>, [role=main])
/// → common content selectors → text density analysis (highest text-to-link
/// ratio). Layout tables (no <th>) are walked as content; data tables
/// (with <th>) are converted to markdown tables.
///
/// This is the `see mode=content` path: the agent gets clean markdown to
/// READ, not 9KB of ref IDs to parse. Designed for articles, docs, search
/// results — any page where the agent wants the text, not the interactive
/// elements.
pub async fn capture_markdown(cdp: &CdpSession, budget: usize) -> Result<String> {
    let expr = markdown_expr(budget, None)?;
    run_markdown(cdp, expr).await
}

/// Scoped variant: markdown of ONE element's subtree (resolved by its
/// canonical sig), used by `see mode=content scope=eN`. Site-specific
/// branches are bypassed - the caller asked for exactly this element.
pub async fn capture_markdown_scoped(
    cdp: &CdpSession,
    budget: usize,
    sig: &str,
    frame: &[usize],
) -> Result<String> {
    let expr = markdown_expr(budget, Some((sig, frame)))?;
    run_markdown(cdp, expr).await
}

/// Assemble the markdown script. `scoped` = (sig, frame): resolve that one
/// element (shadow-piercing deepAll, same sig scheme as capture) and render
/// only its subtree. `None` = whole-page (findMain) with the site branches.
pub(super) fn markdown_expr(budget: usize, scoped: Option<(&str, &[usize])>) -> Result<String> {
    let scope_flag = if scoped.is_some() { "true" } else { "false" };
    let scope_main = match scoped {
        None => "findMain(document)".to_string(),
        Some((sig, frame)) => {
            let sig_js = serde_json::to_string(sig)?;
            let frame_js = serde_json::to_string(frame)?;
            ("(function(){const frame=__FRAME__;"
                .to_string()
                + &JS_PREAMBLE
                + "const d=document;let doc=d;for(const idx of frame){const ifr=[...doc.querySelectorAll('iframe')][idx];if(!ifr)return null;try{doc=ifr.contentDocument;if(!doc)return null;}catch(e){return null;}}const all=deepAll(doc,sel);const fps=frame.join(',');const counts={};for(const n of all){const r=role(n);if(r==='hidden')continue;const nm=name(n,false);const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;const s=fps+'|'+r+'|'+nm+'|'+counts[key];if(s===__SIG__)return n;}return null;})()")
                .replace("__FRAME__", &frame_js)
                .replace("__SIG__", &sig_js)
        }
    };
    Ok(include_str!("../js/markdown.js")
        .replace("__BUDGET__", &budget.to_string())
        .replace("__SCOPE_FLAG__", scope_flag)
        .replace("__SCOPE_MAIN__", &scope_main))
}

/// Run an assembled markdown expression and return its text.
async fn run_markdown(cdp: &CdpSession, expr: String) -> Result<String> {
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;
    let text = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(text.to_string())
}

/// Outline extraction: return just the page title + heading hierarchy.
/// Ultra-minimal output for "what's on this page" without reading everything.
/// ~50-200 bytes typically. If no headings, suggests mode=content.
pub async fn capture_outline(cdp: &CdpSession) -> Result<String> {
    let expr = r#"(function(){var title=document.title||'';var hs=document.querySelectorAll('h1,h2,h3,h4,h5,h6');var out='';if(title)out+=title+'\n';if(!hs.length)return out+'(no headings — use see mode=content to read)';for(var i=0;i<hs.length;i++){var h=hs[i];var lvl=parseInt(h.tagName.charAt(1));var txt=h.innerText.trim();if(!txt)continue;for(var j=0;j<lvl-1;j++)out+='  ';out+=txt+'\n';}return out.trim();})()"#;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;
    let text = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(text.to_string())
}
