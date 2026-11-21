//! Site adapter engine & registry for `see extract=auto`.
//!
//! Centralizes site-specific extraction so every adapter shares one contract:
//! - Modular page-side JS adapters in `src/adapters/js/*.js` assembled into
//!   `auto_extract.js` at call time (syntax-checked via `node --check`).
//! - Standardized handoff markers (`handoff: true`, live `url`) and detail
//!   markers (`detail: true`) so the engine exits its hydration settle loop
//!   immediately instead of waiting for 8 items.
//! - Unified Rust handoff dispatch (Reddit posts/search, X GraphQL) with
//!   consistent DOM fallback, note rendering, and accurate `preview=true`
//!   schema responses without firing unnecessary API requests.
//!
//! Module map: this file is the adapter registry, JS assembly, Rust handoff
//! dispatcher, shared compact-count parser, and unit test suite; page-side
//! site extractors live in `src/adapters/js/`.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;

/// Ordered site adapter JS fragments injected into `auto_extract.js`.
/// Each fragment runs inside the extractor IIFE and returns `JSON.stringify(...)`
/// immediately when it claims the page.
const SITE_ADAPTERS: &[(&str, &str)] = &[
    ("reddit", include_str!("adapters/js/reddit.js")),
    ("x", include_str!("adapters/js/x.js")),
    ("hn", include_str!("adapters/js/hn.js")),
    ("github", include_str!("adapters/js/github.js")),
    ("youtube", include_str!("adapters/js/youtube.js")),
    ("search", include_str!("adapters/js/search.js")),
    (
        "stackexchange",
        include_str!("adapters/js/stackexchange.js"),
    ),
    ("wikipedia", include_str!("adapters/js/wikipedia.js")),
    ("amazon", include_str!("adapters/js/amazon.js")),
    ("packages", include_str!("adapters/js/packages.js")),
];

/// Fallback JSON-LD (`schema.org`) extractor invoked only when no site adapter
/// and no repeated DOM list matched.
const JSONLD_FALLBACK: &str = include_str!("adapters/js/jsonld.js");

/// Concatenate all registered site adapter JS fragments for `auto_extract.js`.
pub fn site_adapters_js() -> String {
    let mut out = String::new();
    for (_, src) in SITE_ADAPTERS {
        out.push_str(src);
        out.push('\n');
    }
    out
}

/// Return the schema.org JSON-LD fallback JS fragment.
pub fn jsonld_fallback_js() -> &'static str {
    JSONLD_FALLBACK
}

/// True when the extractor result should exit the hydration retry loop
/// immediately: either a Rust API handoff (`handoff: true`) or a single-entity
/// detail page (`detail: true`, e.g. product, repo, video, article, paper,
/// issue, question) where waiting for 8 list rows would only burn latency.
pub fn is_early_settle(val: &Value) -> bool {
    val.get("handoff").and_then(Value::as_bool) == Some(true)
        || val.get("detail").and_then(Value::as_bool) == Some(true)
}

/// Outcome of dispatching a JS handoff marker through the Rust adapter layer.
pub enum AdapterDispatch {
    /// Rust adapter succeeded and produced a serialized JSON payload.
    Handled(Value),
    /// Rust adapter failed cleanly; caller should fall back to a DOM-only pass
    /// and attach `reason` alongside standard DOM notes/trimming.
    Fallback { reason: String },
    /// Marker is not a Rust-backed handoff; keep the DOM payload as-is.
    PassThrough,
}

/// Return the known schema preview for Rust-backed handoff markers so
/// `see extract=auto preview=true` answers accurately and instantly without
/// making live API calls or falling back to stale DOM schemas.
pub fn preview_for_handoff(marker: &Value) -> Option<Value> {
    let container = marker.get("container").and_then(Value::as_str)?;
    match container {
        "reddit-post-page" => Some(json!({
            "container": "reddit-comments",
            "fields": ["author", "date", "depth", "id", "op", "score", "text", "url"],
            "sampled": 0,
            "note": "schema for Reddit full-thread DOM/HTML adapter (post metadata + expanded comment tree; no network call made for preview)",
        })),
        "reddit-search-page" => Some(json!({
            "container": "reddit-search",
            "fields": ["author", "comments", "date", "domain", "score", "subreddit", "text", "title", "url"],
            "sampled": 0,
            "note": "schema for Reddit search DOM/HTML adapter (no network call made for preview)",
        })),
        "x-page" => {
            let kind = marker
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("other");
            let target = if kind == "status" {
                "x-thread"
            } else {
                "x-timeline"
            };
            Some(json!({
                "container": target,
                "fields": ["author", "date", "depth", "id", "likes", "media", "name", "op", "quoted", "replies", "reply_to", "reposts", "text", "url"],
                "sampled": 0,
                "note": "schema for X GraphQL/DOM adapter (no API call made for preview)",
            }))
        }
        _ => None,
    }
}

/// Dispatch Rust-backed site adapters when the page-side extractor returned a
/// handoff marker. Validates `sort_override` against the matched adapter.
pub async fn dispatch(
    page: &mut Page,
    marker: &Value,
    live_url: &str,
    limit: usize,
    limit_explicit: bool,
    sort_override: Option<&str>,
) -> Result<AdapterDispatch> {
    let container = marker
        .get("container")
        .and_then(Value::as_str)
        .unwrap_or("");
    if sort_override.is_some() && container != "reddit-post-page" {
        return Err(BladeError::Usage(
            "sort applies to extract=auto on Reddit post pages only".into(),
        ));
    }

    match container {
        "reddit-post-page" => {
            let permalink = marker
                .get("permalink")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if permalink.is_empty() {
                return Ok(AdapterDispatch::PassThrough);
            }
            let page_path = marker
                .get("pathname")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or(&permalink)
                .to_string();
            let page_sort = marker
                .get("sort")
                .and_then(Value::as_str)
                .unwrap_or("confidence");
            let (sort, sort_explicit) = match sort_override {
                Some(s) => (s.to_string(), !s.eq_ignore_ascii_case(page_sort)),
                None => (page_sort.to_string(), false),
            };
            let cap = if limit_explicit {
                limit.clamp(1, crate::reddit::MAX_COMMENT_CAP)
            } else {
                crate::reddit::DEFAULT_COMMENT_CAP
            };
            match crate::reddit::fetch_comments(
                page.cdp_ref(),
                &permalink,
                &page_path,
                &sort,
                sort_explicit,
                cap,
            )
            .await
            {
                Ok(payload) => {
                    let _ = page.take_route_unsettled();
                    Ok(AdapterDispatch::Handled(serde_json::to_value(payload)?))
                }
                Err(e) => {
                    let hint = if crate::reddit::is_security_block(&e) {
                        " (Reddit blocked the expansion request; repeated extracts may remain blocked. DOM coverage is partial; verify complete before treating this as the full thread)"
                    } else {
                        ""
                    };
                    Ok(AdapterDispatch::Fallback {
                        reason: format!(
                            "\nnote: full-thread extraction failed ({e}); items above are a DOM fallback and may miss collapsed replies{hint}"
                        ),
                    })
                }
            }
        }
        "reddit-search-page" => {
            let pathname = marker.get("path").and_then(Value::as_str).unwrap_or("");
            let params = marker.get("params").and_then(Value::as_str).unwrap_or("");
            let Some((path, query)) = crate::reddit::search_target(pathname, params) else {
                return Ok(AdapterDispatch::PassThrough);
            };
            let cap = if limit_explicit {
                limit.clamp(1, crate::reddit::MAX_SEARCH_PAGE)
            } else {
                crate::reddit::DEFAULT_SEARCH_LIMIT
            };
            match crate::reddit::fetch_search(page.cdp_ref(), &path, &query, cap).await {
                Ok(payload) => {
                    let _ = page.take_route_unsettled();
                    Ok(AdapterDispatch::Handled(serde_json::to_value(payload)?))
                }
                Err(e) => {
                    let hint = if crate::reddit::is_security_block(&e) {
                        " (Reddit blocked the request; repeated extracts may remain blocked. DOM coverage is partial)"
                    } else {
                        ""
                    };
                    Ok(AdapterDispatch::Fallback {
                        reason: format!(
                            "\nnote: reddit search extraction failed ({e}); items above are a DOM fallback{hint}"
                        ),
                    })
                }
            }
        }
        "x-page" => {
            let kind = marker
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("other");
            let marker_url = marker
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
                .unwrap_or(live_url);
            let cap = if limit_explicit {
                limit.clamp(1, crate::x::MAX_ITEM_CAP)
            } else {
                crate::x::DEFAULT_ITEM_CAP
            };
            match crate::x::extract(page, kind, marker_url, cap).await {
                Ok(payload) => {
                    let _ = page.take_route_unsettled();
                    Ok(AdapterDispatch::Handled(serde_json::to_value(payload)?))
                }
                Err(e) => Ok(AdapterDispatch::Fallback {
                    reason: format!(
                        "\nnote: x.com fast path failed ({e}); items above are a DOM fallback"
                    ),
                }),
            }
        }
        _ => Ok(AdapterDispatch::PassThrough),
    }
}

/// Parse a leading human-formatted count from aria labels or UI counters:
/// `"1,234 Replies. Reply"` → `Some(1234)`, `"1.2K Likes"` → `Some(1200)`,
/// `"3.5M views"` → `Some(3_500_000)`, `""` → `None`.
pub fn parse_compact_count(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    let start = trimmed.find(|c: char| c.is_ascii_digit())?;
    let rest = &trimmed[start..];
    let num_end = rest
        .find(|c: char| !c.is_ascii_digit() && c != ',' && c != '.')
        .unwrap_or(rest.len());
    let num_str = &rest[..num_end];
    if num_str.is_empty() {
        return None;
    }
    let after = rest[num_end..].trim_start();
    let mult = match after.as_bytes().first().map(|b| b.to_ascii_lowercase()) {
        Some(b'k') => 1_000.0,
        Some(b'm') => 1_000_000.0,
        Some(b'b') => 1_000_000_000.0,
        _ => 1.0,
    };
    let val: f64 = num_str.replace(',', "").parse().ok()?;
    if !val.is_finite() {
        return None;
    }
    Some((val * mult).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn compact_count_handles_commas_and_suffixes() {
        assert_eq!(parse_compact_count("1,234 Replies. Reply"), Some(1234));
        assert_eq!(parse_compact_count("1.2K Likes. Like"), Some(1200));
        assert_eq!(parse_compact_count("3.5M views"), Some(3_500_000));
        assert_eq!(parse_compact_count("2B"), Some(2_000_000_000));
        assert_eq!(parse_compact_count("29 reposts. Repost"), Some(29));
        assert_eq!(parse_compact_count("Reply. 5 replies"), Some(5));
        assert_eq!(parse_compact_count(""), None);
        assert_eq!(parse_compact_count("Like"), None);
    }

    #[test]
    fn early_settle_recognizes_handoffs_and_details() {
        assert!(is_early_settle(
            &json!({"container":"x-page","handoff":true})
        ));
        assert!(is_early_settle(
            &json!({"container":"wikipedia-article","detail":true,"count":1})
        ));
        assert!(!is_early_settle(
            &json!({"container":"ul","count":3,"items":[]})
        ));
    }

    #[test]
    fn preview_schema_covers_all_rust_handoffs() {
        for (marker, expected) in [
            (
                json!({"container":"reddit-post-page","handoff":true}),
                "reddit-comments",
            ),
            (
                json!({"container":"reddit-search-page","handoff":true}),
                "reddit-search",
            ),
            (
                json!({"container":"x-page","handoff":true,"kind":"status"}),
                "x-thread",
            ),
            (
                json!({"container":"x-page","handoff":true,"kind":"profile"}),
                "x-timeline",
            ),
        ] {
            let p = preview_for_handoff(&marker).expect("handoff preview");
            assert_eq!(p["container"], expected);
            assert!(!p["fields"].as_array().unwrap().is_empty());
        }
        assert!(preview_for_handoff(&json!({"container":"hn-items"})).is_none());
    }

    fn run_assembled_in_node(mock_js: &str, limit: usize, post_marker: bool) -> Value {
        let script = include_str!("mcp/server/js/auto_extract.js")
            .replace("__SITE_ADAPTERS__", &site_adapters_js())
            .replace("__JSONLD_FALLBACK__", jsonld_fallback_js())
            .replace("__LIMIT__", &limit.to_string())
            .replace(
                "__POST_MARKER__",
                if post_marker { "true" } else { "false" },
            );
        let full = format!("{mock_js}\nconsole.log({script});");
        let mut child = std::process::Command::new("node")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("node is required for adapter tests");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(full.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "node error:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("valid JSON output")
    }

    const DOM_HARNESS: &str = r#"
const style={display:'block',visibility:'visible',opacity:'1'};
const view={getComputedStyle(){return style;}};
function mkEl(tag, opts={}){
  const attrs=Object.assign({}, opts.attrs||{});
  const cls=new Set((opts.className||'').split(/\s+/).filter(Boolean));
  const node={
    nodeType:1,
    tagName:tag.toUpperCase(),
    className:opts.className||'',
    classList:{contains(c){return cls.has(c);}},
    id:opts.id||'',
    href:opts.href!==undefined?opts.href:(attrs.href||''),
    src:opts.src||attrs.src||'',
    alt:opts.alt||attrs.alt||'',
    content:opts.content||attrs.content||'',
    value:opts.value||'',
    width:opts.width,
    innerText:opts.innerText!==undefined?opts.innerText:(opts.textContent||''),
    textContent:opts.textContent!==undefined?opts.textContent:(opts.innerText||''),
    offsetParent:opts.hidden?null:{},
    children:[],
    childNodes:[],
    parentElement:null,
    nextElementSibling:null,
    ownerDocument:null,
    getAttribute(n){if(n==='id'&&this.id)return this.id;if(n==='class'&&this.className)return this.className;if(n==='href'&&this.href)return this.href;if(n==='src'&&this.src)return this.src;return attrs[n]!==undefined?attrs[n]:null;},
    hasAttribute(n){return this.getAttribute(n)!==null;},
    matches(sel){return matchAny(this, sel);},
    closest(sel){let cur=this;while(cur){if(matchAny(cur,sel))return cur;cur=cur.parentElement;}return null;},
    querySelector(sel){return this.querySelectorAll(sel)[0]||null;},
    querySelectorAll(sel){
      const out=[];
      const walk=n=>{for(const ch of n.children){if(matchAny(ch,sel))out.push(ch);walk(ch);}};
      walk(this);
      return out;
    },
  };
  for(const ch of (opts.children||[])){
    if(ch.nodeType===1){ch.parentElement=node;node.children.push(ch);}
    node.childNodes.push(ch);
  }
  for(let i=0;i<node.children.length-1;i++){node.children[i].nextElementSibling=node.children[i+1];}
  if(!opts.innerText && opts.children && opts.children.length){
    node.innerText=node.children.map(c=>c.innerText||'').filter(Boolean).join('\n');
    if(!opts.textContent)node.textContent=node.innerText;
  }
  return node;
}
function matchSimple(el, part){
  part=part.trim();
  if(!part||part==='*')return true;
  if(part.includes(':not(')){
    const m=part.match(/^(.*):not\(([^)]+)\)(.*)$/);
    if(m){return matchSimple(el,m[1]+m[3])&&!matchSimple(el,m[2]);}
  }
  // Split tag / #id / .class / [attr...]
  const tokens=part.match(/(?:^[a-zA-Z0-9_-]+)|(?:#[a-zA-Z0-9_-]+)|(?:\.[a-zA-Z0-9_-]+)|(?:\[[^\]]+\])/g)||[];
  if(!tokens.length)return false;
  for(const tk of tokens){
    if(tk.startsWith('#')){if(el.id!==tk.slice(1))return false;}
    else if(tk.startsWith('.')){if(!el.classList.contains(tk.slice(1)))return false;}
    else if(tk.startsWith('[')){
      const m=tk.match(/^\[([^\]=*^$~|]+)(?:([*^$]?=)["']?([^"'\]]*)["']?)?\]$/);
      if(!m)return false;
      const v=el.getAttribute(m[1]);
      if(v===null)return false;
      if(m[2]==='='){if(v!==m[3])return false;}
      else if(m[2]==='*='){if(!v.includes(m[3]))return false;}
      else if(m[2]==='^='){if(!v.startsWith(m[3]))return false;}
      else if(m[2]==='$='){if(!v.endsWith(m[3]))return false;}
    }else{
      if(el.tagName!==tk.toUpperCase())return false;
    }
  }
  return true;
}
function splitOutsideBrackets(sel, sepRe){
  const out=[];let cur='',depth=0;
  for(let i=0;i<sel.length;i++){
    const ch=sel[i];
    if(ch==='[')depth++;
    else if(ch===']'&&depth>0)depth--;
    if(depth===0&&sepRe.test(ch)){if(cur.trim())out.push(cur.trim());cur='';}
    else cur+=ch;
  }
  if(cur.trim())out.push(cur.trim());
  return out;
}
function matchSelector(el, sel){
  const parts=splitOutsideBrackets(sel.trim(), />/);
  if(parts.length>1){
    let cur=el;
    for(let i=parts.length-1;i>=0;i--){
      if(!cur||!matchSelector(cur,parts[i]))return false;
      cur=cur.parentElement;
    }
    return true;
  }
  const desc=splitOutsideBrackets(sel.trim(), /\s/);
  if(desc.length>1){
    if(!matchSimple(el,desc[desc.length-1]))return false;
    let cur=el.parentElement;
    for(let i=desc.length-2;i>=0;i--){
      while(cur&&!matchSimple(cur,desc[i]))cur=cur.parentElement;
      if(!cur)return false;
      cur=cur.parentElement;
    }
    return true;
  }
  return matchSimple(el, sel);
}
function matchAny(el, sel){
  return sel.split(',').some(s=>matchSelector(el,s));
}
function setupDoc(url, body, title='Test'){
  const u=new URL(url);
  globalThis.location={hostname:u.hostname,href:u.href,pathname:u.pathname,origin:u.origin,search:u.search};
  const doc={
    body,title,defaultView:view,
    querySelector(s){return body.querySelector(s);},
    querySelectorAll(s){return body.querySelectorAll(s);},
  };
  const bind=n=>{n.ownerDocument=doc;for(const c of n.children)bind(c);};
  bind(body);
  globalThis.document=doc;
}
"#;

    #[test]
    fn hn_item_page_extracts_comments_instead_of_dropping_them() {
        let mock = format!(
            r#"{DOM_HARNESS}
const sub = mkEl('tr', {{id:'50569226', className:'athing submission', children:[
  mkEl('td', {{children:[mkEl('span', {{className:'titleline', children:[mkEl('a', {{attrs:{{href:'https://example.org/cve'}}, innerText:'OpenSSL CVE'}}) ]}})]}})
]}});
const subMeta = mkEl('tr', {{children:[
  mkEl('td', {{className:'subtext', children:[
    mkEl('span', {{className:'score', innerText:'221 points'}}),
    mkEl('a', {{className:'hnuser', innerText:'alice'}}),
    mkEl('span', {{className:'age', children:[mkEl('a', {{innerText:'7 hours ago'}})]}}),
    mkEl('a', {{innerText:'2 comments'}}),
  ]}})
]}});
const fat = mkEl('table', {{className:'fatitem', children:[sub, subMeta]}});
const c1 = mkEl('tr', {{id:'100', className:'athing comtr', children:[
  mkEl('td', {{className:'ind', attrs:{{indent:'0'}}}}),
  mkEl('a', {{className:'hnuser', innerText:'bob'}}),
  mkEl('span', {{className:'age', children:[mkEl('a', {{innerText:'6 hours ago'}})]}}),
  mkEl('div', {{className:'commtext', innerText:'Root comment body'}}),
]}});
const c2 = mkEl('tr', {{id:'101', className:'athing comtr', children:[
  mkEl('td', {{className:'ind', attrs:{{indent:'1'}}}}),
  mkEl('a', {{className:'hnuser', innerText:'carol'}}),
  mkEl('span', {{className:'age', children:[mkEl('a', {{innerText:'2 hours ago'}})]}}),
  mkEl('div', {{className:'commtext', innerText:'Nested reply body'}}),
]}});
setupDoc('https://news.ycombinator.com/item?id=50569226', mkEl('body', {{children:[fat, mkEl('table', {{children:[c1, c2]}})]}}));
"#
        );
        let v = run_assembled_in_node(&mock, 50, true);
        assert_eq!(v["container"], "hn-comments", "{v}");
        assert_eq!(v["detail"], true, "{v}");
        assert_eq!(v["post"]["title"], "OpenSSL CVE");
        assert_eq!(v["post"]["points"], 221);
        assert_eq!(v["count"], 2);
        assert_eq!(v["complete"], true);
        assert_eq!(v["items"][0]["depth"], 0);
        assert_eq!(v["items"][0]["author"], "bob");
        assert_eq!(v["items"][1]["depth"], 1);
        assert_eq!(v["items"][1]["text"], "Nested reply body");
    }

    #[test]
    fn bing_redirect_unwrapping_decodes_base64url_u_param() {
        let mock = format!(
            r#"{DOM_HARNESS}
const li1 = mkEl('li', {{className:'b_algo', children:[
  mkEl('h2', {{children:[mkEl('a', {{href:'https://www.bing.com/ck/a?!&&u=a1aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw&ntb=1', innerText:'Rust Programming Language'}})]}}),
  mkEl('div', {{className:'b_caption', children:[mkEl('p', {{innerText:'Fast and memory-efficient language.'}})]}}),
]}});
setupDoc('https://www.bing.com/search?q=rust+programming', mkEl('body', {{children:[mkEl('ol', {{children:[li1]}})]}}));
"#
        );
        let v = run_assembled_in_node(&mock, 20, true);
        assert_eq!(v["container"], "bing-search", "{v}");
        assert_eq!(v["query"], "rust programming");
        assert_eq!(v["items"][0]["url"], "https://rust-lang.org/");
        assert_eq!(v["items"][0]["domain"], "rust-lang.org");
        assert_eq!(v["redirects_unwrapped"], 1);
    }

    #[test]
    fn github_pulls_and_issue_detail_extract_cleanly() {
        let mock_pulls = format!(
            r#"{DOM_HARNESS}
const row = mkEl('li', {{children:[
  mkEl('a', {{attrs:{{'data-testid':'listitem-title-link', href:'/rust-lang/rust/pull/166242'}}, innerText:'Rollup of 6 pull requests'}}),
  mkEl('a', {{attrs:{{'data-hovercard-type':'user'}}, innerText:'bors'}}),
  mkEl('svg', {{className:'octicon-git-pull-request'}}),
]}});
setupDoc('https://github.com/rust-lang/rust/pulls', mkEl('body', {{children:[mkEl('ul', {{children:[row]}})]}}));
"#
        );
        let v = run_assembled_in_node(&mock_pulls, 20, true);
        assert_eq!(v["container"], "github-pulls", "{v}");
        assert_eq!(v["items"][0]["number"], 166242);
        assert_eq!(v["items"][0]["author"], "bors");
        assert_eq!(v["items"][0]["status"], "open");

        let mock_issue = format!(
            r#"{DOM_HARNESS}
const body = mkEl('body', {{children:[
  mkEl('h1', {{attrs:{{'data-testid':'issue-title'}}, innerText:'Fix parser crash'}}),
  mkEl('span', {{attrs:{{'data-testid':'header-state'}}, innerText:'Open'}}),
  mkEl('div', {{attrs:{{'data-testid':'issue-body'}}, children:[
    mkEl('a', {{attrs:{{'data-testid':'issue-body-header-author'}}, innerText:'alice'}}),
    mkEl('div', {{attrs:{{'data-testid':'issue-body-viewer'}}, children:[
      mkEl('div', {{attrs:{{'data-testid':'markdown-body'}}, innerText:'Steps to reproduce the crash.'}}),
    ]}}),
  ]}}),
  mkEl('div', {{attrs:{{'data-testid':'comment-viewer-outer-box-1'}}, children:[
    mkEl('a', {{attrs:{{'data-testid':'comment-subject-author'}}, innerText:'bob'}}),
    mkEl('div', {{attrs:{{'data-testid':'markdown-body'}}, innerText:'Reproduced on main.'}}),
  ]}}),
]}});
setupDoc('https://github.com/org/repo/issues/42', body);
"#
        );
        let v = run_assembled_in_node(&mock_issue, 20, true);
        assert_eq!(v["container"], "github-issue", "{v}");
        assert_eq!(v["detail"], true);
        assert_eq!(v["issue"]["number"], 42);
        assert_eq!(v["issue"]["state"], "open");
        assert_eq!(v["issue"]["author"], "alice");
        assert_eq!(v["count"], 1);
        assert_eq!(v["items"][0]["author"], "bob");
    }

    #[test]
    fn stackoverflow_skips_hidden_modal_h1s_and_marks_accepted_answer() {
        let mock = format!(
            r#"{DOM_HARNESS}
const modalH1 = mkEl('h1', {{className:'s-modal--header', hidden:true, innerText:''}});
const hdr = mkEl('div', {{id:'question-header', children:[
  mkEl('h1', {{children:[mkEl('a', {{innerText:'Why is sorted array faster?'}})]}}),
]}});
const q = mkEl('div', {{id:'question', attrs:{{'data-score':'27549'}}, children:[
  mkEl('a', {{className:'post-tag', innerText:'c++'}}),
  mkEl('a', {{className:'post-tag', innerText:'c++'}}),
  mkEl('div', {{className:'js-post-body', innerText:'Sorting the array speeds up the loop.'}}),
]}});
const a1 = mkEl('div', {{className:'answer accepted-answer', attrs:{{'data-answerid':'11227902', 'data-score':'35296'}}, children:[
  mkEl('div', {{className:'js-post-body', innerText:'You are a victim of branch prediction fail.'}}),
]}});
const answers = mkEl('div', {{id:'answers', children:[a1]}});
setupDoc('https://stackoverflow.com/questions/11227809/sorted-array', mkEl('body', {{children:[modalH1, hdr, q, answers]}}));
"#
        );
        let v = run_assembled_in_node(&mock, 20, true);
        assert_eq!(v["container"], "stackexchange-question", "{v}");
        assert_eq!(v["question"]["title"], "Why is sorted array faster?");
        assert_eq!(v["question"]["score"], 27549);
        assert_eq!(v["question"]["tags"], json!(["c++"]));
        assert_eq!(v["items"][0]["accepted"], true);
        assert_eq!(v["items"][0]["score"], 35296);
    }

    #[test]
    fn amazon_search_reads_offscreen_price_textcontent() {
        let mock = format!(
            r#"{DOM_HARNESS}
const card = mkEl('div', {{attrs:{{'data-component-type':'s-search-result', 'data-asin':'B07ZGDPT4M'}}, children:[
  mkEl('h2', {{innerText:'SteelSeries Apex 3 Gaming Keyboard'}}),
  mkEl('span', {{className:'a-price', children:[
    mkEl('span', {{className:'a-offscreen', innerText:'', textContent:'$49.99'}}),
  ]}}),
  mkEl('span', {{attrs:{{'aria-label':'4.6 out of 5 stars'}}}}),
  mkEl('a', {{attrs:{{href:'#customerReviews'}}, children:[mkEl('span', {{textContent:'8.1K'}})]}}),
]}});
setupDoc('https://www.amazon.com/s?k=keyboard', mkEl('body', {{children:[card]}}));
"#
        );
        let v = run_assembled_in_node(&mock, 20, true);
        assert_eq!(v["container"], "amazon-search", "{v}");
        assert_eq!(v["query"], "keyboard");
        assert_eq!(v["items"][0]["asin"], "B07ZGDPT4M");
        assert_eq!(v["items"][0]["url"], "https://www.amazon.com/dp/B07ZGDPT4M");
        assert_eq!(v["items"][0]["price"], "$49.99");
        assert_eq!(v["items"][0]["rating"], 4.6);
        assert_eq!(v["items"][0]["reviews"], 8100);
    }

    #[test]
    fn jsonld_fallback_extracts_job_posting_when_no_list_exists() {
        let mock = format!(
            r#"{DOM_HARNESS}
const ld = mkEl('script', {{attrs:{{type:'application/ld+json'}}, textContent:JSON.stringify({{
  '@context':'https://schema.org',
  '@type':'JobPosting',
  title:'Senior Rust Engineer',
  hiringOrganization:{{name:'Acme Systems'}},
  jobLocation:{{address:{{addressLocality:'Remote', addressCountry:'US'}}}},
  employmentType:'FULL_TIME',
  description:'Build fast browser engines.'
}})}});
setupDoc('https://jobs.example/rust-eng', mkEl('body', {{children:[ld, mkEl('h1', {{innerText:'Senior Rust Engineer'}})]}}));
"#
        );
        let v = run_assembled_in_node(&mock, 20, true);
        assert_eq!(v["container"], "schema-job", "{v}");
        assert_eq!(v["detail"], true);
        assert_eq!(v["items"][0]["title"], "Senior Rust Engineer");
        assert_eq!(v["items"][0]["company"], "Acme Systems");
        assert_eq!(v["items"][0]["location"], "Remote, US");
    }

    #[test]
    fn generic_extractor_excludes_hidden_siblings_and_groups_heterogeneous_ul_items() {
        let mock = format!(
            r#"{DOM_HARNESS}
const li1 = mkEl('li', {{children:[
  mkEl('a', {{href:'https://shop.test/1', innerText:'First great widget with badge'}}),
  mkEl('span', {{className:'badge', innerText:'Best Seller'}}),
]}});
const li2 = mkEl('li', {{children:[
  mkEl('a', {{href:'https://shop.test/2', innerText:'Second great widget plain'}}),
]}});
const li3 = mkEl('li', {{children:[
  mkEl('a', {{href:'https://shop.test/3', innerText:'Third great widget with price'}}),
  mkEl('div', {{innerText:'$19.99'}}),
]}});
const hiddenLi = mkEl('li', {{hidden:true, children:[
  mkEl('a', {{href:'https://shop.test/hidden', innerText:'Hidden template card must not leak'}}),
]}});
setupDoc('https://shop.test/catalog', mkEl('body', {{children:[mkEl('ul', {{children:[li1, li2, li3, hiddenLi]}})]}}));
"#
        );
        let v = run_assembled_in_node(&mock, 20, false);
        assert_eq!(v["container"], "ul", "{v}");
        assert_eq!(
            v["count"], 3,
            "heterogeneous visible items kept, hidden excluded: {v}"
        );
        let dump = v.to_string();
        assert!(!dump.contains("Hidden template card"), "{v}");
    }

    #[test]
    fn x_route_classifier_handles_profile_tabs_and_excludes_system_routes() {
        let mock_tab = format!(
            r#"{DOM_HARNESS}
setupDoc('https://x.com/rustlang/with_replies', mkEl('body'));
"#
        );
        let v = run_assembled_in_node(&mock_tab, 20, true);
        assert_eq!(v["container"], "x-page");
        assert_eq!(v["handoff"], true);
        assert_eq!(v["kind"], "profile");
        assert_eq!(v["url"], "https://x.com/rustlang/with_replies");

        let mock_explore = format!(
            r#"{DOM_HARNESS}
setupDoc('https://x.com/explore', mkEl('body'));
"#
        );
        let v2 = run_assembled_in_node(&mock_explore, 20, true);
        assert_eq!(
            v2["kind"], "other",
            "/explore must not be classified as a user profile"
        );
    }

    #[test]
    fn host_matching_does_not_false_match_unrelated_domains() {
        let mock = format!(
            r#"{DOM_HARNESS}
setupDoc('https://notreddit.com/r/rust/comments/abc123/', mkEl('body'));
"#
        );
        let v = run_assembled_in_node(&mock, 20, true);
        assert_ne!(
            v["container"], "reddit-post-page",
            "notreddit.com must not trigger reddit adapter"
        );
    }

    #[test]
    fn youtube_search_and_watch_extract_structured_fields() {
        let mock_search = format!(
            r#"{DOM_HARNESS}
const card = mkEl('ytd-video-renderer', {{children:[
  mkEl('a', {{id:'video-title', attrs:{{href:'/watch?v=flS3cmjcows&pp=tracking', title:'Learn Rust in 2027'}}, innerText:'Learn Rust in 2027'}}),
  mkEl('ytd-channel-name', {{children:[mkEl('a', {{href:'/@letsgetrusty', innerText:'Lets Get Rusty'}})]}}),
  mkEl('div', {{id:'metadata-line', children:[
    mkEl('span', {{innerText:'29K views'}}),
    mkEl('span', {{innerText:'2 days ago'}}),
  ]}}),
  mkEl('badge-shape', {{innerText:'5:51'}}),
]}});
setupDoc('https://www.youtube.com/results?search_query=rust', mkEl('body', {{children:[card]}}));
"#
        );
        let v = run_assembled_in_node(&mock_search, 20, true);
        assert_eq!(v["container"], "youtube-search", "{v}");
        assert_eq!(v["query"], "rust");
        assert_eq!(v["items"][0]["video_id"], "flS3cmjcows");
        assert_eq!(
            v["items"][0]["url"],
            "https://www.youtube.com/watch?v=flS3cmjcows"
        );
        assert_eq!(v["items"][0]["channel"], "Lets Get Rusty");
        assert_eq!(v["items"][0]["duration"], "5:51");

        // YouTube A/B variant: yt-lockup-view-model on /results, skipping CTA/ad lockups.
        let mock_lockup = format!(
            r#"{DOM_HARNESS}
const cta = mkEl('yt-lockup-view-model', {{children:[
  mkEl('a', {{attrs:{{href:'/watch?v=9ywFGczGEv8'}}, innerText:'Watch'}}),
]}});
const card = mkEl('yt-lockup-view-model', {{children:[
  mkEl('a', {{attrs:{{href:'/watch?v=BpPEoZW5IiY&pp=abc'}}, innerText:'Learn Rust Programming - Complete Course'}}),
  mkEl('a', {{attrs:{{href:'/@freecodecamp'}}, innerText:'freeCodeCamp.org'}}),
  mkEl('yt-content-metadata-view-model', {{children:[
    mkEl('span', {{attrs:{{role:'text'}}, innerText:'1.3M views'}}),
    mkEl('span', {{attrs:{{role:'text'}}, innerText:'3 years ago'}}),
  ]}}),
  mkEl('badge-shape', {{innerText:'13:59:10'}}),
]}});
setupDoc('https://www.youtube.com/results?search_query=rust', mkEl('body', {{children:[cta, card]}}));
"#
        );
        let vl = run_assembled_in_node(&mock_lockup, 20, true);
        assert_eq!(vl["container"], "youtube-search", "{vl}");
        assert_eq!(vl["count"], 1, "CTA-only lockup must be skipped: {vl}");
        assert_eq!(vl["items"][0]["video_id"], "BpPEoZW5IiY");
        assert_eq!(vl["items"][0]["channel"], "freeCodeCamp.org");
        assert_eq!(vl["items"][0]["duration"], "13:59:10");

        let mock_watch = format!(
            r#"{DOM_HARNESS}
const body = mkEl('body', {{children:[
  mkEl('meta', {{attrs:{{itemprop:'datePublished', content:'2009-10-24T23:57:33-07:00'}}}}),
  mkEl('meta', {{attrs:{{itemprop:'duration', content:'PT3M34S'}}}}),
  mkEl('meta', {{attrs:{{itemprop:'interactionCount', content:'1500000000'}}}}),
  mkEl('ytd-watch-metadata', {{children:[
    mkEl('h1', {{innerText:'Never Gonna Give You Up'}}),
    mkEl('ytd-channel-name', {{children:[mkEl('a', {{href:'/@RickAstleyYT', innerText:'Rick Astley'}})]}}),
    mkEl('span', {{id:'owner-sub-count', innerText:'4.56M subscribers'}}),
    mkEl('like-button-view-model', {{children:[
      mkEl('button', {{attrs:{{'aria-label':'like this video along with 19,511,999 other people'}}}}),
    ]}}),
  ]}}),
  mkEl('ytd-comment-thread-renderer', {{children:[
    mkEl('a', {{id:'author-text', innerText:'@fan'}}),
    mkEl('div', {{id:'content-text', innerText:'Classic song!'}}),
    mkEl('span', {{id:'vote-count-middle', innerText:'1.5K'}}),
  ]}}),
]}});
setupDoc('https://www.youtube.com/watch?v=dQw4w9WgXcQ', body);
"#
        );
        let v2 = run_assembled_in_node(&mock_watch, 20, true);
        assert_eq!(v2["container"], "youtube-video", "{v2}");
        assert_eq!(v2["detail"], true);
        assert_eq!(v2["video"]["title"], "Never Gonna Give You Up");
        assert_eq!(v2["video"]["duration"], "3:34");
        assert_eq!(v2["video"]["likes"], 19_511_999);
        assert_eq!(v2["video"]["views"], 1_500_000_000);
        assert_eq!(v2["count"], 1);
        assert_eq!(v2["items"][0]["likes"], 1500);
    }

    #[test]
    fn wikipedia_and_arxiv_and_pypi_extract_cleanly() {
        let mock_wiki = format!(
            r#"{DOM_HARNESS}
const body = mkEl('body', {{children:[
  mkEl('h1', {{id:'firstHeading', innerText:'Rust (programming language)'}}),
  mkEl('div', {{id:'mw-content-text', children:[
    mkEl('div', {{className:'mw-parser-output', children:[
      mkEl('table', {{className:'infobox', children:[
        mkEl('tr', {{children:[mkEl('th', {{innerText:'Stable release'}}), mkEl('td', {{innerText:'1.100.0[2]'}})]}}),
      ]}}),
      mkEl('section', {{children:[
        mkEl('p', {{innerText:'Rust is a general-purpose programming language focused on safety and speed.[1]'}}),
      ]}}),
      mkEl('div', {{className:'mw-heading', children:[mkEl('h2', {{innerText:'History'}})]}}),
    ]}}),
  ]}}),
]}});
setupDoc('https://en.wikipedia.org/wiki/Rust_(programming_language)', body);
"#
        );
        let vw = run_assembled_in_node(&mock_wiki, 20, true);
        assert_eq!(vw["container"], "wikipedia-article", "{vw}");
        assert_eq!(vw["detail"], true);
        assert_eq!(vw["items"][0]["infobox"]["Stable release"], "1.100.0");
        assert!(vw["items"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("general-purpose"));
        assert_eq!(vw["items"][0]["sections"], json!(["History"]));

        let mock_arxiv = format!(
            r#"{DOM_HARNESS}
const body = mkEl('body', {{children:[
  mkEl('h1', {{className:'title', innerText:'Title: Attention Is All You Need'}}),
  mkEl('div', {{className:'authors', children:[mkEl('a', {{innerText:'Ashish Vaswani'}})]}}),
  mkEl('blockquote', {{className:'abstract', innerText:'Abstract: The dominant sequence transduction models...'}}),
  mkEl('a', {{className:'download-pdf', href:'https://arxiv.org/pdf/1706.03762'}}),
]}});
setupDoc('https://arxiv.org/abs/1706.03762', body);
"#
        );
        let va = run_assembled_in_node(&mock_arxiv, 20, true);
        assert_eq!(va["container"], "arxiv-paper", "{va}");
        assert_eq!(va["items"][0]["id"], "1706.03762");
        assert_eq!(va["items"][0]["title"], "Attention Is All You Need");
        assert_eq!(va["items"][0]["authors"], json!(["Ashish Vaswani"]));

        let mock_pypi = format!(
            r#"{DOM_HARNESS}
const body = mkEl('body', {{children:[
  mkEl('h1', {{className:'package-header__name', innerText:'requests 2.32.5'}}),
  mkEl('p', {{className:'package-description__summary', innerText:'Python HTTP for Humans.'}}),
  mkEl('span', {{id:'pip-command', innerText:'pip install requests'}}),
]}});
setupDoc('https://pypi.org/project/requests/', body);
"#
        );
        let vp = run_assembled_in_node(&mock_pypi, 20, true);
        assert_eq!(vp["container"], "pypi-package", "{vp}");
        assert_eq!(vp["items"][0]["name"], "requests");
        assert_eq!(vp["items"][0]["version"], "2.32.5");
        assert_eq!(vp["items"][0]["install"], "pip install requests");
    }

    #[test]
    fn google_and_ddg_search_extract_organic_results() {
        let mock_ddg = format!(
            r#"{DOM_HARNESS}
const a1 = mkEl('article', {{attrs:{{'data-testid':'result'}}, children:[
  mkEl('a', {{attrs:{{'data-testid':'result-title-a', href:'https://duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F'}}, innerText:'Rust Programming Language'}}),
  mkEl('div', {{attrs:{{'data-result':'snippet'}}, innerText:'Reliable and efficient software.'}}),
]}});
setupDoc('https://duckduckgo.com/?q=rust', mkEl('body', {{children:[a1]}}));
"#
        );
        let vd = run_assembled_in_node(&mock_ddg, 20, true);
        assert_eq!(vd["container"], "ddg-search", "{vd}");
        assert_eq!(vd["query"], "rust");
        assert_eq!(vd["items"][0]["url"], "https://rust-lang.org/");
        assert_eq!(vd["redirects_unwrapped"], 1);

        let mock_google = format!(
            r#"{DOM_HARNESS}
const card = mkEl('div', {{children:[
  mkEl('a', {{href:'https://www.google.com/url?q=https%3A%2F%2Frust-lang.org%2F', children:[
    mkEl('h3', {{innerText:'Rust Programming Language'}}),
  ]}}),
  mkEl('cite', {{innerText:'https://rust-lang.org'}}),
  mkEl('div', {{innerText:'Rust is blazingly fast and memory-efficient with no runtime or garbage collector.'}}),
]}});
const rso = mkEl('div', {{id:'rso', children:[card]}});
setupDoc('https://www.google.com/search?q=rust', mkEl('body', {{children:[rso]}}));
"#
        );
        let vg = run_assembled_in_node(&mock_google, 20, true);
        assert_eq!(vg["container"], "google-search", "{vg}");
        assert_eq!(vg["query"], "rust");
        assert_eq!(vg["items"][0]["url"], "https://rust-lang.org/");
        assert_eq!(vg["items"][0]["display_url"], "https://rust-lang.org");
        assert_eq!(vg["redirects_unwrapped"], 1);
    }

    #[test]
    fn reddit_dom_fallback_scopes_comment_bodies_and_handles_old_reddit_and_search() {
        let mock_shreddit = format!(
            r#"{DOM_HARNESS}
const child = mkEl('shreddit-comment', {{
  attrs: {{ thingid:'t1_c2', author:'bob', score:'7', depth:'1', permalink:'/r/rust/comments/1xqudci/comment/c2/' }},
  children: [
    mkEl('faceplate-timeago', {{ attrs: {{ ts:'2026-11-20T15:00:00Z' }} }}),
    mkEl('div', {{ attrs: {{ slot:'comment' }}, innerText:'Nested reply body' }}),
  ]
}});
const deletedParent = mkEl('shreddit-comment', {{
  attrs: {{ thingid:'t1_c1', 'is-comment-deleted':'', 'is-author-deleted':'', depth:'0' }},
  children: [child]
}});
setupDoc('https://www.reddit.com/r/rust/comments/1xqudci/everything_is_becoming_rust_now/', mkEl('body', {{children:[deletedParent]}}));
"#
        );
        let v = run_assembled_in_node(&mock_shreddit, 20, false);
        assert_eq!(v["container"], "reddit-comments", "{v}");
        assert_eq!(v["count"], 2);
        assert_eq!(v["items"][0]["author"], "[deleted]");
        assert_eq!(
            v["items"][0]["text"], "[deleted]",
            "deleted parent must not steal child's [slot=comment] text: {v}"
        );
        assert_eq!(v["items"][1]["author"], "u/bob");
        assert_eq!(v["items"][1]["text"], "Nested reply body");
        assert_eq!(v["items"][1]["date"], "2026-11-20 15:00");

        let mock_search = format!(
            r#"{DOM_HARNESS}
const ad = mkEl('div', {{ attrs: {{ 'data-testid':'search-post-unit', 'is-promoted':'' }}, children: [
  mkEl('a', {{ attrs: {{ 'data-testid':'post-title', href:'/r/ads/comments/a1/ad/' }}, innerText:'Sponsored ad' }}),
]}});
const hit = mkEl('div', {{ attrs: {{ 'data-testid':'search-post-unit' }}, children: [
  mkEl('a', {{ attrs: {{ 'data-testid':'post-title', href:'/r/rust/comments/1xqudci/rust_now/' }}, innerText:'Everything is becoming Rust now' }}),
  mkEl('a', {{ attrs: {{ href:'/r/rust/' }}, innerText:'r/rust' }}),
]}});
setupDoc('https://www.reddit.com/search/?q=rust', mkEl('body', {{children:[ad, hit]}}));
"#
        );
        let vs = run_assembled_in_node(&mock_search, 20, false);
        assert_eq!(vs["container"], "reddit-search", "{vs}");
        assert_eq!(vs["count"], 1, "promoted unit skipped: {vs}");
        assert_eq!(vs["items"][0]["subreddit"], "r/rust");
    }
}
