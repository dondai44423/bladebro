//! Read modes: visible text (`capture_content`), markdown
//! (`capture_markdown` + scoped), heading outline. The markdown script
//! lives in `../js/markdown.js`. Split from the `perception` core.

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;

use super::{JS_PREAMBLE, JS_READ_TREE};

/// Extract visible text content from the page body, excluding scripts,
/// styles, ads, and hidden elements. Returns at most `budget` characters.
///
/// Walks the live tree with computed-style visibility, descending open
/// shadow roots and same-origin iframes. (The old clone-based version lost
/// every shadow and iframe text — cloneNode does not clone shadow trees —
/// and, because `innerText` on a detached clone degrades to `textContent`,
/// leaked display:none text against this contract.) Text collapses to
/// single spaces and truncates to the budget.
pub async fn capture_content(cdp: &CdpSession, budget: usize) -> Result<String> {
    let expr = content_expr(budget);
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

/// Build the visible-text probe used by [`capture_content`]. The script
/// lives in `js/content_text.js`.
pub(super) fn content_expr(budget: usize) -> String {
    include_str!("../js/content_text.js").replace("__BUDGET__", &budget.to_string())
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
    let expr = outline_expr();
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

/// Heading outlines traverse the visible shadow/iframe tree; hidden frames
/// and script text must never become headings in the agent's page summary.
pub(super) fn outline_expr() -> String {
    "(()=>{const d=document;"
        .to_string()
        + &JS_PREAMBLE
        + JS_READ_TREE
        + "var title=document.title||'';var out='';if(title)out+=title+'\\n';const hs=[];walkReadTree(d.body,n=>{if(n.nodeType===1&&/^H[1-6]$/.test(n.tagName))hs.push(n);});if(!hs.length)return out+'(no headings — use see mode=content to read)';for(let i=0;i<hs.length;i++){const h=hs[i];const lvl=parseInt(h.tagName.charAt(1));const txt=(h.innerText||'').trim();if(!txt)continue;for(let j=0;j<lvl-1;j++)out+='  ';out+=txt+'\\n';}return out.trim();})()"
}
