//! Reddit extraction — the `extract auto` fast path for Reddit post pages and
//! search pages, built purely on the live DOM and Shreddit's own HTML partials
//! (zero `.json` API endpoints).
//!
//! Why zero `.json`: Reddit's network-security layer routinely denies `.json`
//! requests (HTTP 403) even when the browser session loaded the HTML page
//! cleanly, and Shreddit itself never calls `.json`. Instead, the web app
//! renders `<shreddit-post>` and `<shreddit-comment>` nodes directly in HTML
//! and expands collapsed branches via:
//!
//! - `<faceplate-partial src="/svc/shreddit/more-comments/...">` (both
//!   top-level "View more comments" and nested "N more replies" buttons),
//! - `div.fold-more a[href*="/comment/"]` (deep-thread continuation links,
//!   e.g. `?force-legacy-sct=1`), and
//! - `old.reddit.com` `.thing.comment` trees + `.deepthread a` links.
//!
//! This module reads the live DOM observationally, then expands discovered
//! partials and deep continuation links **strictly sequentially** (one GET at a
//! time with short polite jitter — never parallel bursts), parses each HTML
//! fragment in-browser with `DOMParser`, and renders a lossless DFS comment
//! tree with honest `complete`, `total`, and `note` fields (propagating exact
//! HTTP 429 + `Retry-After` or security walls immediately without hammering).
//!
//! Module map: this file is the comment-tree sweep orchestrator
//! ([`fetch_comments`]) and search entry points ([`fetch_search`] /
//! [`search_target`]); `net` is the isolated DOM-read + sequential HTML
//! partial transport; `tree` is the parent-linked tree builder + lossless DFS
//! renderer; `search` is the zero-`.json` search extractor; `js/` holds the
//! syntax-checked in-browser HTML/DOM parsers.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};

mod net;
mod search;
mod tree;

pub use self::net::is_security_block;
use self::net::{
    dom_parse_expr, eval_dom, fetch_html_parsed, is_rate_limit, opts_json, polite_pause,
    retry_after_from_err,
};
pub use self::search::{
    fetch_search, search_target, SearchItem, SearchPayload, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_PAGE,
};
use self::tree::{
    canonical_reddit_url, cut_chars, decode_batch, ingest_comments, render_items, FoldTask,
    PartialTask, Tree,
};

pub(super) const PARSE_THREAD_JS: &str = include_str!("reddit/js/parse_thread.js");

/// Wall-clock budget for the full comment expansion sweep.
const TOTAL_BUDGET: Duration = Duration::from_secs(30);
/// Hard cap on sequential HTML expansion requests per sweep.
const MAX_REQUESTS: u32 = 45;
/// Per-request in-page fetch timeout.
const PAGE_FETCH_TIMEOUT_MS: u64 = 12_000;
/// Safety cap for a single comment body (kept whole below this).
const COMMENT_TEXT_CAP: usize = 5_000;
/// Safety cap for the post selftext.
const POST_BODY_CAP: usize = 8_000;

/// Default number of comments fetched when the caller passed no limit.
pub const DEFAULT_COMMENT_CAP: usize = 1_000;
/// Absolute ceiling for an explicit limit.
pub const MAX_COMMENT_CAP: usize = 5_000;

/// Post metadata carried alongside the comment list.
#[derive(Debug, Serialize)]
pub struct PostMeta {
    pub title: String,
    /// `u/name` (or `[deleted]`).
    pub author: String,
    /// `r/name`.
    pub subreddit: String,
    pub score: Option<i64>,
    /// The comment count Reddit reports for the post.
    pub comments: Option<i64>,
    pub date: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// One comment, thread-ordered (DFS) with its nesting depth.
#[derive(Debug, Serialize)]
pub struct CommentItem {
    pub id: String,
    pub author: String,
    pub score: Option<i64>,
    pub date: Option<String>,
    pub depth: usize,
    /// Present (`true`) only when the comment author is the post author.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<bool>,
    pub text: String,
    pub url: String,
}

/// The `extract auto` payload for a Reddit post page.
#[derive(Debug, Serialize)]
pub struct CommentsPayload {
    pub container: &'static str,
    pub post: PostMeta,
    /// Comments returned.
    pub count: usize,
    /// Comments Reddit reports for the post.
    pub total: Option<i64>,
    /// True only when the returned list is the complete discussion.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<CommentItem>,
}

struct Budget {
    requests: u32,
    deadline: Instant,
}

impl Budget {
    fn new() -> Self {
        Self {
            requests: 0,
            deadline: Instant::now() + TOTAL_BUDGET,
        }
    }

    fn take(&mut self, n: u32) -> bool {
        if !self.ok() {
            return false;
        }
        self.requests += n;
        true
    }

    fn ok(&self) -> bool {
        self.requests < MAX_REQUESTS && Instant::now() < self.deadline
    }
}

fn post_id_from_base(post_base: &str) -> String {
    post_base
        .split("/comments/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn subreddit_from_base(post_base: &str) -> String {
    post_base
        .strip_prefix("/r/")
        .and_then(|rest| rest.split('/').next())
        .filter(|s| !s.is_empty())
        .map(|s| format!("r/{s}"))
        .unwrap_or_default()
}

/// Extract the comment tree for a Reddit post (or single-comment permalink)
/// purely from the live DOM plus sequential HTML partial/continuation fetches.
///
/// Never calls `.json` endpoints and never fires parallel request bursts.
/// `post_base` is `/r/<sub>/comments/<id>`; `page_path` is the page's current
/// `location.pathname`; `sort_explicit` is true only when the caller explicitly
/// asked for a sort different from the page's currently rendered sort.
pub async fn fetch_comments(
    cdp: &CdpSession,
    post_base: &str,
    page_path: &str,
    sort: &str,
    sort_explicit: bool,
    cap: usize,
) -> Result<CommentsPayload> {
    if !post_base.starts_with('/') || !post_base.contains("/comments/") {
        return Err(BladeError::Other(format!(
            "reddit: bad post path {post_base:?}"
        )));
    }
    let sort = if !sort.is_empty()
        && sort.len() <= 16
        && sort.chars().all(|c| c.is_ascii_alphanumeric())
    {
        sort
    } else {
        "confidence"
    };
    let cap = cap.clamp(1, MAX_COMMENT_CAP);
    let mut budget = Budget::new();
    let fallback_post_id = post_id_from_base(post_base);

    tracing::debug!(
        post = post_base,
        page_path,
        sort,
        sort_explicit,
        cap,
        "reddit html/dom sweep start"
    );

    // Step 1: initial read. When the caller did not ask for a different sort,
    // read the live DOM observationally (0 HTTP requests). When an explicit
    // sort override differs from the rendered page, fetch the HTML page with
    // `?sort=<sort>` (still zero `.json`).
    let initial_raw = if sort_explicit {
        let base_path = if !page_path.is_empty() && page_path.starts_with('/') {
            page_path
        } else {
            post_base
        };
        let clean_path = base_path.split('?').next().unwrap_or(base_path);
        let sort_url = format!("{clean_path}?sort={sort}");
        budget.take(1);
        let opts = opts_json(false, &fallback_post_id);
        fetch_html_parsed(
            cdp,
            &sort_url,
            PARSE_THREAD_JS,
            &format!("parseRedditThread(doc, {opts})"),
        )
        .await?
    } else {
        let opts = opts_json(false, &fallback_post_id);
        let expr = dom_parse_expr(
            PARSE_THREAD_JS,
            &format!("parseRedditThread(document, {opts})"),
        );
        eval_dom(cdp, &expr).await?
    };

    let initial = decode_batch(&initial_raw, &fallback_post_id);
    if initial.post.is_none()
        && initial.comments.is_empty()
        && initial.partials.is_empty()
        && initial.folds.is_empty()
    {
        return Err(BladeError::Other(
            "reddit: post and comment elements not found in DOM".into(),
        ));
    }

    let post_id = if !initial.post_id.is_empty() {
        initial.post_id
    } else {
        fallback_post_id
    };
    let post_permalink = if !initial.post_permalink.is_empty() {
        initial.post_permalink
    } else if !page_path.is_empty() && page_path.starts_with('/') {
        page_path.to_string()
    } else {
        post_base.to_string()
    };
    let post = initial.post.unwrap_or_else(|| PostMeta {
        title: String::new(),
        author: "[deleted]".into(),
        subreddit: subreddit_from_base(post_base),
        score: None,
        comments: None,
        date: None,
        url: canonical_reddit_url(&post_permalink),
        body: None,
    });

    let mut tree = Tree {
        post_id: post_id.clone(),
        post_author: initial.post_author_bare,
        post_permalink,
        ..Default::default()
    };
    ingest_comments(&mut tree, initial.comments, &post_id);

    let mut partial_queue: VecDeque<PartialTask> = VecDeque::new();
    let mut fold_queue: VecDeque<FoldTask> = VecDeque::new();
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut seen_fold_ids: HashSet<String> = HashSet::new();
    let mut clickable_only_partials = initial.clickable_only_partials;
    let mut legacy_more_count = initial.legacy_more_count;

    enqueue_controls(
        initial.partials,
        initial.folds,
        &mut partial_queue,
        &mut fold_queue,
        &mut seen_urls,
        &mut seen_fold_ids,
    );

    tracing::debug!(
        nodes = tree.nodes.len(),
        partials = partial_queue.len(),
        folds = fold_queue.len(),
        "reddit initial dom parse done"
    );

    // Step 2: strictly sequential BFS expansion over `<faceplate-partial>`
    // ("View more comments" / "N more replies") and `div.fold-more` deep
    // continuation links. One request at a time with polite jitter.
    let mut capped = false;
    let mut budget_hit = false;
    let mut rate_limited = false;
    let mut retry_after: Option<String> = None;
    let mut security_blocked = false;
    let mut gaps: Vec<String> = Vec::new();
    let mut failed_regions = 0usize;

    while !partial_queue.is_empty() || !fold_queue.is_empty() {
        if tree.nodes.len() >= cap {
            capped = true;
            break;
        }
        if !budget.ok() {
            budget_hit = true;
            break;
        }

        enum Step {
            Partial(PartialTask),
            Fold(FoldTask),
        }
        let step = if let Some(p) = partial_queue.pop_front() {
            Step::Partial(p)
        } else if let Some(f) = fold_queue.pop_front() {
            Step::Fold(f)
        } else {
            break;
        };

        if budget.requests > 0 {
            polite_pause().await;
        }
        if !budget.take(1) {
            // Put back so remaining count stays exact.
            match step {
                Step::Partial(p) => partial_queue.push_front(p),
                Step::Fold(f) => fold_queue.push_front(f),
            }
            budget_hit = true;
            break;
        }

        let (url, fallback_parent) = match &step {
            Step::Partial(p) => (
                p.url.as_str(),
                if p.parent_id.is_empty() {
                    post_id.as_str()
                } else {
                    p.parent_id.as_str()
                },
            ),
            Step::Fold(f) => (
                f.url.as_str(),
                if !f.parent_id.is_empty() {
                    f.parent_id.as_str()
                } else if !f.comment_id.is_empty() {
                    f.comment_id.as_str()
                } else {
                    post_id.as_str()
                },
            ),
        };

        let opts = opts_json(true, fallback_parent);
        match fetch_html_parsed(
            cdp,
            url,
            PARSE_THREAD_JS,
            &format!("parseRedditThread(doc, {opts})"),
        )
        .await
        {
            Ok(raw) => {
                let batch = decode_batch(&raw, &post_id);
                let added = ingest_comments(&mut tree, batch.comments, fallback_parent);
                clickable_only_partials += batch.clickable_only_partials;
                legacy_more_count += batch.legacy_more_count;
                enqueue_controls(
                    batch.partials,
                    batch.folds,
                    &mut partial_queue,
                    &mut fold_queue,
                    &mut seen_urls,
                    &mut seen_fold_ids,
                );
                tracing::debug!(
                    requests = budget.requests,
                    added,
                    nodes = tree.nodes.len(),
                    partials_left = partial_queue.len(),
                    folds_left = fold_queue.len(),
                    "reddit html expansion step done"
                );
            }
            Err(e) => {
                if is_rate_limit(&e) {
                    rate_limited = true;
                    retry_after = retry_after_from_err(&e);
                    failed_regions += 1;
                    break;
                }
                if is_security_block(&e) {
                    security_blocked = true;
                    failed_regions += 1;
                    break;
                }
                failed_regions += 1;
                gaps.push(format!("reply branch fetch failed ({e})"));
            }
        }
    }

    let remaining_regions = partial_queue.len()
        + fold_queue.len()
        + clickable_only_partials
        + legacy_more_count
        + failed_regions;
    let remaining_claimed: i64 = partial_queue.iter().filter_map(|p| p.claimed).sum();

    let (items, dfs_capped) = render_items(&tree, cap);
    tracing::debug!(
        nodes = tree.nodes.len(),
        items = items.len(),
        requests = budget.requests,
        capped = capped || dfs_capped,
        budget_hit,
        rate_limited,
        security_blocked,
        remaining_regions,
        "reddit html/dom sweep done"
    );

    Ok(assemble(
        post,
        items,
        capped || dfs_capped,
        budget_hit,
        rate_limited,
        retry_after,
        security_blocked,
        remaining_regions,
        remaining_claimed,
        clickable_only_partials,
        legacy_more_count,
        gaps,
    ))
}

fn enqueue_controls(
    partials: Vec<PartialTask>,
    folds: Vec<FoldTask>,
    partial_queue: &mut VecDeque<PartialTask>,
    fold_queue: &mut VecDeque<FoldTask>,
    seen_urls: &mut HashSet<String>,
    seen_fold_ids: &mut HashSet<String>,
) {
    for p in partials {
        if seen_urls.insert(p.url.clone()) {
            partial_queue.push_back(p);
        }
    }
    for f in folds {
        let id_ok = f.comment_id.is_empty() || seen_fold_ids.insert(f.comment_id.clone());
        if id_ok && seen_urls.insert(f.url.clone()) {
            fold_queue.push_back(f);
        }
    }
}

/// Assemble the payload with honest counts, completion flag, and actionable
/// notes describing any cap, budget, HTTP 429 (`Retry-After`), security wall,
/// or unexpanded reply branches.
#[allow(clippy::too_many_arguments)]
fn assemble(
    post: PostMeta,
    items: Vec<CommentItem>,
    capped: bool,
    budget_hit: bool,
    rate_limited: bool,
    retry_after: Option<String>,
    security_blocked: bool,
    remaining_regions: usize,
    remaining_claimed: i64,
    clickable_only_partials: usize,
    legacy_more_count: usize,
    mut gaps: Vec<String>,
) -> CommentsPayload {
    let count = items.len();
    let total = post.comments;
    let total_s = total.map(|t| t.to_string()).unwrap_or_else(|| "?".into());
    let mut notes: Vec<String> = Vec::new();

    let remain_tail = if remaining_regions > 0 {
        if remaining_claimed > 0 {
            format!(" ({remaining_regions} unexpanded reply region(s), ~{remaining_claimed}+ replies remain)")
        } else {
            format!(" ({remaining_regions} unexpanded reply region(s) remain)")
        }
    } else {
        String::new()
    };

    if capped {
        notes.push(format!(
            "capped: {count} of {total_s} comments shown{remain_tail} (raise limit to fetch more)"
        ));
    } else if rate_limited {
        let ra = retry_after
            .map(|r| format!(", retry-after: {r}"))
            .unwrap_or_default();
        notes.push(format!(
            "reddit rate limit reached (HTTP 429{ra}): {count} of {total_s} comments loaded{remain_tail} — wait before retrying"
        ));
    } else if security_blocked {
        notes.push(format!(
            "reddit's network-security wall interrupted expansion: {count} of {total_s} comments loaded{remain_tail} (remaining replies were not fetched)"
        ));
    } else if budget_hit {
        notes.push(format!(
            "expansion budget reached ({MAX_REQUESTS} requests / {}s): {count} of {total_s} comments loaded{remain_tail}",
            TOTAL_BUDGET.as_secs()
        ));
    } else if remaining_regions > 0 {
        if clickable_only_partials > 0 {
            gaps.push(format!(
                "{clickable_only_partials} collapsed reply control(s) had no partial URL and were not expanded"
            ));
        }
        if legacy_more_count > 0 {
            gaps.push(format!(
                "{legacy_more_count} old.reddit 'load more comments' branch(es) remain collapsed"
            ));
        }
        notes.push(format!("loaded {count} of {total_s} comments{remain_tail}"));
    } else if total.is_some_and(|t| (count as i64) < t) {
        if gaps.is_empty() {
            notes.push(format!(
                "loaded {count} of {total_s} comments — every visible reply branch Reddit exposed was expanded; the remainder are deleted/removed or unserved comments"
            ));
        } else {
            notes.push(format!("loaded {count} of {total_s} comments"));
        }
    }

    notes.append(&mut gaps);
    let mut seen = HashSet::new();
    notes.retain(|n| seen.insert(n.clone()));
    let note = if notes.is_empty() {
        None
    } else {
        Some(cut_chars(&notes.join("; "), 600))
    };

    CommentsPayload {
        container: "reddit-comments",
        post,
        count,
        total,
        complete: note.is_none(),
        note,
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::io::Write;
    use tokio_tungstenite::tungstenite::Message;

    use super::net::{classify_gate_body, is_rate_limit, rate_limit_error};
    use super::tree::{normalize_date, normalize_text};

    const NODE_DOM_SHIM: &str = r#"
function mkEl(tag, opts = {}) {
  const attrs = Object.assign({}, opts.attrs || {});
  const cls = new Set((opts.className || '').split(/\s+/).filter(Boolean));
  const node = {
    nodeType: 1,
    tagName: tag.toUpperCase(),
    className: opts.className || '',
    classList: { contains(c) { return cls.has(c); } },
    id: opts.id || attrs.id || '',
    children: [],
    childNodes: [],
    parentElement: null,
    getAttribute(n) {
      if (n === 'id' && this.id) return this.id;
      if (n === 'class' && this.className) return this.className;
      return attrs[n] !== undefined ? String(attrs[n]) : null;
    },
    hasAttribute(n) { return this.getAttribute(n) !== null; },
    matches(sel) { return matchAny(this, sel); },
    closest(sel) {
      let cur = this;
      while (cur) {
        if (matchAny(cur, sel)) return cur;
        cur = cur.parentElement;
      }
      return null;
    },
    querySelector(sel) { return this.querySelectorAll(sel)[0] || null; },
    querySelectorAll(sel) {
      const out = [];
      const walk = n => {
        for (const ch of n.children) {
          if (matchAny(ch, sel)) out.push(ch);
          walk(ch);
        }
      };
      walk(this);
      return out;
    },
  };
  if (opts.text !== undefined) {
    node.childNodes.push({ nodeType: 3, textContent: String(opts.text) });
  }
  for (const ch of (opts.children || [])) {
    if (typeof ch === 'string') {
      node.childNodes.push({ nodeType: 3, textContent: ch });
    } else if (ch && ch.nodeType === 1) {
      ch.parentElement = node;
      node.children.push(ch);
      node.childNodes.push(ch);
    }
  }
  return node;
}
function matchSimple(el, part) {
  part = part.trim();
  if (!part || part === '*') return true;
  if (part.includes(':not(')) {
    const m = part.match(/^(.*):not\(([^)]+)\)(.*)$/);
    if (m) return matchSimple(el, m[1] + m[3]) && !matchSimple(el, m[2]);
  }
  const tokens = part.match(/(?:^[a-zA-Z0-9_-]+)|(?:#[a-zA-Z0-9_-]+)|(?:\.[a-zA-Z0-9_-]+)|(?:\[[^\]]+\])/g) || [];
  if (!tokens.length) return false;
  for (const tk of tokens) {
    if (tk.startsWith('#')) { if (el.id !== tk.slice(1)) return false; }
    else if (tk.startsWith('.')) { if (!el.classList.contains(tk.slice(1))) return false; }
    else if (tk.startsWith('[')) {
      const m = tk.match(/^\[([^\]=*^$~|]+)(?:([*^$~]?=)["']?([^"'\]]*)["']?)?\]$/);
      if (!m) return false;
      const v = el.getAttribute(m[1]);
      if (v === null) return false;
      if (m[2] === '=') { if (v !== m[3]) return false; }
      else if (m[2] === '*=') { if (!v.includes(m[3])) return false; }
      else if (m[2] === '^=') { if (!v.startsWith(m[3])) return false; }
      else if (m[2] === '$=') { if (!v.endsWith(m[3])) return false; }
      else if (m[2] === '~=') { if (!v.split(/\s+/).includes(m[3])) return false; }
    } else {
      if (el.tagName !== tk.toUpperCase()) return false;
    }
  }
  return true;
}
function splitOutside(sel, sepRe) {
  const out = []; let cur = '', d = 0;
  for (let i = 0; i < sel.length; i++) {
    const ch = sel[i];
    if (ch === '[') d++; else if (ch === ']' && d > 0) d--;
    if (d === 0 && sepRe.test(ch)) { if (cur.trim()) out.push(cur.trim()); cur = ''; }
    else cur += ch;
  }
  if (cur.trim()) out.push(cur.trim());
  return out;
}
function matchSel(el, sel) {
  const desc = splitOutside(sel.trim(), /\s/);
  if (desc.length > 1) {
    if (!matchSimple(el, desc[desc.length - 1])) return false;
    let cur = el.parentElement;
    for (let i = desc.length - 2; i >= 0; i--) {
      while (cur && !matchSimple(cur, desc[i])) cur = cur.parentElement;
      if (!cur) return false;
      cur = cur.parentElement;
    }
    return true;
  }
  return matchSimple(el, sel);
}
function matchAny(el, sel) {
  return sel.split(',').some(s => matchSel(el, s));
}
"#;

    fn run_node(js: &str) -> Value {
        let mut child = std::process::Command::new("node")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("node required for reddit parser tests");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(js.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "node error:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("valid JSON from node")
    }

    #[test]
    fn reddit_parser_scripts_pass_node_syntax_check() {
        let has_node = std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            return;
        }
        for (name, src) in [
            ("parse_thread", PARSE_THREAD_JS),
            ("parse_search", search::PARSE_SEARCH_JS),
        ] {
            let path = std::env::temp_dir().join(format!("bladebro-reddit-{name}.js"));
            std::fs::write(&path, src).unwrap();
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .unwrap();
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "{name}.js syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn shreddit_parser_extracts_comments_partials_folds_and_never_steals_child_body() {
        let js = format!(
            r#"{NODE_DOM_SHIM}
{PARSE_THREAD_JS}
const post = mkEl('shreddit-post', {{
  attrs: {{
    id: 't3_1xqudci',
    'post-title': 'Everything is becoming Rust now',
    author: 'ferris',
    'subreddit-prefixed-name': 'r/rust',
    score: '412',
    'comment-count': '453',
    permalink: '/r/rust/comments/1xqudci/everything_is_becoming_rust_now/',
    'created-timestamp': '2026-11-20T14:22:10.000000+0000',
  }},
  children: [
    mkEl('div', {{ attrs: {{ slot: 'text-body' }}, children: [mkEl('p', {{ text: 'Post body line 1.' }})] }}),
  ],
}});
const nestedChild = mkEl('shreddit-comment', {{
  attrs: {{ thingid: 't1_c2', parentid: 't1_c1', author: 'bob', score: '19', depth: '1', permalink: '/r/rust/comments/1xqudci/comment/c2/' }},
  children: [
    mkEl('faceplate-timeago', {{ attrs: {{ ts: '2026-11-20T15:00:00Z' }} }}),
    mkEl('div', {{ attrs: {{ slot: 'comment' }}, children: [mkEl('p', {{ text: 'Child reply text must stay on c2 only' }})] }}),
    mkEl('div', {{ className: 'fold-more', children: [
      mkEl('a', {{ attrs: {{ href: '/r/rust/comments/1xqudci/comment/c2/?force-legacy-sct=1' }}, text: '1 more reply' }}),
    ]}}),
  ],
}});
// Deleted parent with NO own [slot=comment] must NOT steal c2's body.
const deletedParent = mkEl('shreddit-comment', {{
  attrs: {{ thingid: 't1_c1', parentid: 't3_1xqudci', 'is-comment-deleted': '', 'is-author-deleted': '', depth: '0' }},
  children: [
    mkEl('a', {{ className: 'more-comments-link', attrs: {{ href: '/r/rust/comments/1xqudci/comment/c1/' }}, text: 'hidden seo link' }}),
    mkEl('faceplate-partial', {{
      attrs: {{ name: 'more-comments_c1', src: '/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=abc', loading: 'action' }},
      children: [mkEl('button', {{ text: '29 more replies' }})],
    }}),
    nestedChild,
  ],
}});
const sidebarPartial = mkEl('aside', {{ children: [
  mkEl('faceplate-partial', {{ attrs: {{ name: 'right-sidebar', src: '/svc/shreddit/pdp-right-rail/r/rust' }} }}),
]}});
const postMethodPartial = mkEl('faceplate-partial', {{
  attrs: {{ method: 'post', src: '/svc/shreddit/more-comments/r/rust/t3_1xqudci?mut=1' }},
}});
const tree = mkEl('shreddit-comment-tree', {{ children: [
  deletedParent,
  postMethodPartial,
  mkEl('faceplate-partial', {{
    attrs: {{ name: 'more-comments_top', src: '/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=top2' }},
    children: [mkEl('button', {{ text: 'View more comments' }})],
  }}),
]}});
const doc = mkEl('body', {{ children: [post, mkEl('main', {{ children: [tree] }}), sidebarPartial] }});
console.log(JSON.stringify(parseRedditThread(doc, {{
  isFragment: false,
  origin: 'https://www.reddit.com',
  url: 'https://www.reddit.com/r/rust/comments/1xqudci/everything_is_becoming_rust_now/',
  pathname: '/r/rust/comments/1xqudci/everything_is_becoming_rust_now/',
}})));
"#
        );
        let v = run_node(&js);
        assert_eq!(v["post"]["title"], "Everything is becoming Rust now");
        assert_eq!(v["post"]["comments"], 453);
        assert_eq!(v["post"]["body"], "Post body line 1.");

        let cmts = v["comments"].as_array().unwrap();
        assert_eq!(cmts.len(), 2);
        assert_eq!(cmts[0]["id"], "c1");
        assert_eq!(cmts[0]["author"], "[deleted]");
        assert_eq!(
            cmts[0]["body"], "[deleted]",
            "deleted parent must never steal child comment text"
        );
        assert_eq!(cmts[1]["id"], "c2");
        assert_eq!(cmts[1]["body"], "Child reply text must stay on c2 only");
        assert_eq!(cmts[1]["date"], "2026-11-20T15:00:00Z");

        let partials = v["partials"].as_array().unwrap();
        assert_eq!(
            partials.len(),
            2,
            "sidebar and method=post partials excluded: {partials:?}"
        );
        assert_eq!(partials[0]["parent_id"], "c1");
        assert_eq!(partials[0]["claimed"], 29);
        assert_eq!(partials[1]["parent_id"], "1xqudci");

        let folds = v["folds"].as_array().unwrap();
        assert_eq!(
            folds.len(),
            1,
            "only div.fold-more captured, hidden a.more-comments-link ignored: {folds:?}"
        );
        assert_eq!(folds[0]["comment_id"], "c2");
        assert_eq!(
            folds[0]["url"],
            "/r/rust/comments/1xqudci/comment/c2/?force-legacy-sct=1"
        );
    }

    #[test]
    fn search_parser_extracts_shreddit_sdui_and_skips_promoted_ads() {
        let js = format!(
            r#"{NODE_DOM_SHIM}
{}
const ad = mkEl('div', {{ attrs: {{ 'data-testid': 'search-post-unit', 'is-promoted': '' }}, children: [
  mkEl('a', {{ attrs: {{ 'data-testid': 'post-title', href: '/r/ads/comments/ad1/buy/' }}, text: 'Buy this' }}),
]}});
const unit = mkEl('div', {{ attrs: {{ 'data-testid': 'search-post-unit' }}, children: [
  mkEl('a', {{ attrs: {{ 'data-testid': 'post-title', href: '/r/rust/comments/1xqudci/everything_is_becoming_rust_now/' }}, text: 'Everything is becoming Rust now' }}),
  mkEl('a', {{ attrs: {{ href: '/r/rust/' }}, text: 'r/rust' }}),
  mkEl('a', {{ attrs: {{ href: '/user/ferris/' }}, text: 'u/ferris' }}),
  mkEl('div', {{ attrs: {{ 'data-testid': 'search-counter-row' }}, text: '1.2K upvotes · 453 comments' }}),
  mkEl('faceplate-timeago', {{ attrs: {{ ts: '2026-11-20T14:22:10Z' }} }}),
]}});
const nextP = mkEl('faceplate-partial', {{ attrs: {{ src: '/svc/shreddit/search/?q=rust&cursor=next2' }} }});
const doc = mkEl('body', {{ children: [ad, unit, nextP] }});
console.log(JSON.stringify(parseRedditSearch(doc, {{
  origin: 'https://www.reddit.com',
  url: 'https://www.reddit.com/search/?q=rust',
}})));
"#,
            search::PARSE_SEARCH_JS
        );
        let v = run_node(&js);
        assert_eq!(v["skipped_ads"], 1);
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["items"][0]["title"], "Everything is becoming Rust now");
        assert_eq!(v["items"][0]["subreddit"], "r/rust");
        assert_eq!(v["items"][0]["author"], "u/ferris");
        assert_eq!(v["items"][0]["score"], 1200);
        assert_eq!(v["items"][0]["comments"], 453);
        assert_eq!(v["next_url"], "/svc/shreddit/search/?q=rust&cursor=next2");
    }

    #[tokio::test]
    async fn sweep_expands_partials_and_fold_more_sequentially_with_zero_json() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut seen_exprs = Vec::new();

            let responses = vec![
                // Step 0: live DOM read (1 root comment + 1 nested partial + 1 deep fold-more)
                json!({
                    "post_id": "1xqudci",
                    "post": {
                        "id": "1xqudci",
                        "title": "Everything is becoming Rust now",
                        "author": "ferris",
                        "subreddit": "r/rust",
                        "score": 400,
                        "comments": 4,
                        "date": "2026-11-20T14:22:00Z",
                        "permalink": "/r/rust/comments/1xqudci/everything_is_becoming_rust_now/",
                        "url": "https://www.reddit.com/r/rust/comments/1xqudci/everything_is_becoming_rust_now/"
                    },
                    "comments": [{
                        "id": "c1",
                        "parent_id": "1xqudci",
                        "author": "alice",
                        "score": 50,
                        "depth": 0,
                        "date": "2026-11-20T14:30:00Z",
                        "permalink": "/r/rust/comments/1xqudci/comment/c1/",
                        "body": "Root comment"
                    }],
                    "partials": [{
                        "url": "/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=p1",
                        "parent_id": "c1",
                        "claimed": 2
                    }],
                    "folds": []
                }),
                // Step 1: partial wave 1 adds c2 + nested partial p2 + fold-more f1
                json!({
                    "s": 200, "ra": "", "head": "", "len": 400,
                    "data": {
                        "post_id": "1xqudci",
                        "comments": [{
                            "id": "c2",
                            "parent_id": "c1",
                            "author": "ferris",
                            "score": 20,
                            "depth": 1,
                            "permalink": "/r/rust/comments/1xqudci/comment/c2/",
                            "body": "OP nested reply"
                        }],
                        "partials": [{
                            "url": "/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=p2",
                            "parent_id": "c2",
                            "claimed": 1
                        }],
                        "folds": [{
                            "url": "/r/rust/comments/1xqudci/comment/c3/?force-legacy-sct=1",
                            "parent_id": "c3",
                            "comment_id": "c3"
                        }]
                    }
                }),
                // Step 2: partial wave 2 adds c3 under c2
                json!({
                    "s": 200, "ra": "", "head": "", "len": 300,
                    "data": {
                        "post_id": "1xqudci",
                        "comments": [{
                            "id": "c3",
                            "parent_id": "c2",
                            "author": "carol",
                            "score": 10,
                            "depth": 2,
                            "permalink": "/r/rust/comments/1xqudci/comment/c3/",
                            "body": "Depth 2 reply"
                        }],
                        "partials": [],
                        "folds": []
                    }
                }),
                // Step 3: div.fold-more permalink returns c3 (already known!) + deep child c4
                json!({
                    "s": 200, "ra": "", "head": "", "len": 350,
                    "data": {
                        "post_id": "1xqudci",
                        "comments": [
                            {
                                "id": "c3",
                                "parent_id": "c2",
                                "author": "carol",
                                "score": 10,
                                "depth": 0,
                                "permalink": "/r/rust/comments/1xqudci/comment/c3/",
                                "body": "Depth 2 reply"
                            },
                            {
                                "id": "c4",
                                "parent_id": "c3",
                                "author": "dave",
                                "score": 5,
                                "depth": 1,
                                "permalink": "/r/rust/comments/1xqudci/comment/c4/",
                                "body": "Deep fold-more reply"
                            }
                        ],
                        "partials": [],
                        "folds": []
                    }
                }),
            ];

            for val in responses {
                let req = socket.next().await.unwrap().unwrap();
                let req: Value = serde_json::from_str(req.to_text().unwrap()).unwrap();
                let expr = req["params"]["expression"].as_str().unwrap().to_string();
                assert!(
                    !expr.contains(".json"),
                    "Reddit sweep must NEVER touch .json endpoints: {expr:.200}"
                );
                assert!(
                    !expr.contains("Promise.all"),
                    "Reddit sweep must stay strictly sequential (no parallel bursts)"
                );
                seen_exprs.push(expr);
                socket
                    .send(Message::Text(
                        json!({"id": req["id"], "result": {"result": {"value": val}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            seen_exprs
        });

        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let p = fetch_comments(
            &cdp,
            "/r/rust/comments/1xqudci",
            "/r/rust/comments/1xqudci/everything_is_becoming_rust_now/",
            "confidence",
            false,
            100,
        )
        .await
        .unwrap();
        let exprs = task.await.unwrap();
        assert_eq!(exprs.len(), 4);
        assert!(exprs[1].contains("/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=p1"));
        assert!(exprs[2].contains("/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=p2"));
        assert!(exprs[3].contains("/r/rust/comments/1xqudci/comment/c3/?force-legacy-sct=1"));

        assert_eq!(p.container, "reddit-comments");
        assert!(p.complete, "all 4 comments resolved cleanly: {:?}", p.note);
        assert_eq!(p.count, 4);
        let order: Vec<(&str, usize, Option<bool>)> = p
            .items
            .iter()
            .map(|c| (c.id.as_str(), c.depth, c.op))
            .collect();
        assert_eq!(
            order,
            vec![
                ("c1", 0, None),
                ("c2", 1, Some(true)),
                ("c3", 2, None),
                ("c4", 3, None),
            ],
            "DFS depth re-rooted cleanly even though fold-more permalink reported depth 0/1"
        );
    }

    #[tokio::test]
    async fn sweep_stops_immediately_on_429_and_propagates_retry_after() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let responses = vec![
                json!({
                    "post_id": "1xqudci",
                    "post": {
                        "id": "1xqudci",
                        "title": "Thread",
                        "author": "op",
                        "subreddit": "r/rust",
                        "comments": 50,
                        "permalink": "/r/rust/comments/1xqudci/thread/"
                    },
                    "comments": [{
                        "id": "c1", "parent_id": "1xqudci", "author": "alice", "body": "Initial"
                    }],
                    "partials": [
                        {"url": "/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=1", "parent_id": "c1", "claimed": 20},
                        {"url": "/svc/shreddit/more-comments/r/rust/t3_1xqudci?cursor=2", "parent_id": "1xqudci", "claimed": 29}
                    ],
                    "folds": []
                }),
                json!({
                    "s": 429, "ra": "60", "head": "Too Many Requests", "len": 17
                }),
            ];
            let mut count = 0;
            for val in responses {
                let req = socket.next().await.unwrap().unwrap();
                let req: Value = serde_json::from_str(req.to_text().unwrap()).unwrap();
                socket
                    .send(Message::Text(
                        json!({"id": req["id"], "result": {"result": {"value": val}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                count += 1;
            }
            count
        });

        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let p = fetch_comments(
            &cdp,
            "/r/rust/comments/1xqudci",
            "/r/rust/comments/1xqudci/thread/",
            "confidence",
            false,
            100,
        )
        .await
        .unwrap();
        assert_eq!(task.await.unwrap(), 2, "must stop immediately on first 429");
        assert!(!p.complete);
        assert_eq!(p.count, 1, "initial DOM comments preserved");
        let note = p.note.as_deref().unwrap_or("");
        assert!(note.contains("HTTP 429"), "{note}");
        assert!(note.contains("retry-after: 60s"), "{note}");
        assert!(note.contains("2 unexpanded reply region"), "{note}");
    }

    #[tokio::test]
    async fn single_comment_permalink_renders_detached_root_losslessly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let req = socket.next().await.unwrap().unwrap();
            let req: Value = serde_json::from_str(req.to_text().unwrap()).unwrap();
            let val = json!({
                "post_id": "1xqudci",
                "post": {
                    "id": "1xqudci",
                    "title": "Thread",
                    "author": "op",
                    "subreddit": "r/rust",
                    "comments": 2,
                    "permalink": "/r/rust/comments/1xqudci/thread/"
                },
                "comments": [
                    {"id": "deep1", "parent_id": "unloaded_ancestor", "author": "alice", "depth": 0, "body": "Focused comment"},
                    {"id": "deep2", "parent_id": "deep1", "author": "bob", "depth": 1, "body": "Its nested reply"}
                ],
                "partials": [],
                "folds": []
            });
            socket
                .send(Message::Text(
                    json!({"id": req["id"], "result": {"result": {"value": val}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let p = fetch_comments(
            &cdp,
            "/r/rust/comments/1xqudci",
            "/r/rust/comments/1xqudci/comment/deep1/",
            "confidence",
            false,
            100,
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(p.count, 2);
        assert_eq!(p.items[0].id, "deep1");
        assert_eq!(p.items[0].depth, 0);
        assert_eq!(p.items[1].id, "deep2");
        assert_eq!(p.items[1].depth, 1);
    }

    #[tokio::test]
    async fn sort_override_fetches_html_page_never_json() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let req = socket.next().await.unwrap().unwrap();
            let req: Value = serde_json::from_str(req.to_text().unwrap()).unwrap();
            let expr = req["params"]["expression"].as_str().unwrap().to_string();
            assert!(!expr.contains(".json"), "sort override must not use .json");
            assert!(
                expr.contains("/r/rust/comments/1xqudci/everything_is_becoming_rust_now/?sort=new"),
                "expected HTML sort URL, got: {expr:.240}"
            );
            let val = json!({
                "s": 200, "ra": "", "head": "", "len": 500,
                "data": {
                    "post_id": "1xqudci",
                    "post": {
                        "id": "1xqudci",
                        "title": "Everything is becoming Rust now",
                        "author": "ferris",
                        "subreddit": "r/rust",
                        "comments": 1,
                        "permalink": "/r/rust/comments/1xqudci/everything_is_becoming_rust_now/"
                    },
                    "comments": [{
                        "id": "newest1", "parent_id": "1xqudci", "author": "zoe", "body": "Just posted!"
                    }],
                    "partials": [],
                    "folds": []
                }
            });
            socket
                .send(Message::Text(
                    json!({"id": req["id"], "result": {"result": {"value": val}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let p = fetch_comments(
            &cdp,
            "/r/rust/comments/1xqudci",
            "/r/rust/comments/1xqudci/everything_is_becoming_rust_now/",
            "new",
            true,
            100,
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert!(p.complete);
        assert_eq!(p.count, 1);
        assert_eq!(p.items[0].id, "newest1");
    }

    #[test]
    fn old_reddit_parser_extracts_nested_comments_and_deepthread() {
        let js = format!(
            r#"{NODE_DOM_SHIM}
{PARSE_THREAD_JS}
const post = mkEl('div', {{
  className: 'thing link',
  attrs: {{ 'data-fullname':'t3_abc123', 'data-author':'opuser', 'data-subreddit':'rust', 'data-score':'99', 'data-comments-count':'5', 'data-permalink':'/r/rust/comments/abc123/old_post/' }},
  children: [
    mkEl('a', {{ className:'title', text:'Old Reddit Post' }}),
    mkEl('time', {{ attrs: {{ datetime:'2026-11-20T10:00:00Z' }} }}),
  ]
}});
const c2 = mkEl('div', {{
  className: 'thing comment',
  attrs: {{ 'data-fullname':'t1_r2', 'data-author':'bob', 'data-permalink':'/r/rust/comments/abc123/old_post/r2/' }},
  children: [
    mkEl('div', {{ className:'entry', children: [
      mkEl('div', {{ className:'usertext-body', children: [mkEl('div', {{ className:'md', children: [mkEl('p', {{ text:'Old nested reply' }})] }})] }}),
    ]}}),
    mkEl('span', {{ className:'deepthread', children: [
      mkEl('a', {{ attrs: {{ href:'/r/rust/comments/abc123/old_post/r2/' }}, text:'Continue this thread' }}),
    ]}}),
  ]
}});
const c1 = mkEl('div', {{
  className: 'thing comment',
  attrs: {{ 'data-fullname':'t1_r1', 'data-author':'alice', 'data-permalink':'/r/rust/comments/abc123/old_post/r1/' }},
  children: [
    mkEl('div', {{ className:'entry', children: [
      mkEl('span', {{ className:'score unvoted', attrs: {{ title:'42' }}, text:'42 points' }}),
      mkEl('div', {{ className:'usertext-body', children: [mkEl('div', {{ className:'md', children: [mkEl('p', {{ text:'Old root comment' }})] }})] }}),
    ]}}),
    mkEl('div', {{ className:'child', children: [c2] }}),
  ]
}});
const doc = mkEl('body', {{ children: [mkEl('div', {{ id:'siteTable', children:[post] }}), c1] }});
console.log(JSON.stringify(parseRedditThread(doc, {{
  isFragment: false,
  origin: 'https://old.reddit.com',
  url: 'https://old.reddit.com/r/rust/comments/abc123/old_post/',
  pathname: '/r/rust/comments/abc123/old_post/',
}})));
"#
        );
        let v = run_node(&js);
        assert_eq!(v["post"]["title"], "Old Reddit Post");
        assert_eq!(v["comments"].as_array().unwrap().len(), 2);
        assert_eq!(v["comments"][0]["id"], "r1");
        assert_eq!(v["comments"][0]["score"], 42);
        assert_eq!(v["comments"][1]["id"], "r2");
        assert_eq!(v["comments"][1]["parent_id"], "r1");
        assert_eq!(v["folds"].as_array().unwrap().len(), 1);
        assert_eq!(v["folds"][0]["comment_id"], "r2");
    }

    #[test]
    fn date_and_text_normalization_and_gate_classification() {
        assert_eq!(
            normalize_date("2026-11-20T14:22:10.123000+0000").as_deref(),
            Some("2026-11-20 14:22 UTC")
        );
        assert_eq!(
            normalize_date("2026-11-20T14:22:10Z").as_deref(),
            Some("2026-11-20 14:22 UTC")
        );
        assert_eq!(normalize_text("a  \r\n\r\n\r\nb\r\n", 100), "a\n\nb");
        assert!(classify_gate_body("/p", "You've been blocked by network security").is_some());
        assert!(classify_gate_body("/p", "Whoa there, pardner!").is_some());
        assert!(classify_gate_body("/p", "Prove your humanity").is_some());
        assert!(is_rate_limit(&rate_limit_error("30")));
    }
}
