//! Reddit comment-tree extraction — the `extract auto` fast path on
//! reddit.com post pages.
//!
//! Comments come from Reddit's own JSON endpoints, fetched from inside the
//! page (same origin, same cookies, same session — exactly the traffic the
//! web app itself makes), never from scraping the rendered DOM:
//!
//! - `GET <post>.json` → the inline comment tree (structured things) plus
//!   "more" stubs for whatever the server did not inline (listings cap
//!   around a few hundred comments per level).
//! - A nested stub is completed with one
//!   `GET /comments/<post>/comment/<parent>.json` — that listing returns the
//!   stub parent's full subtree, still structured.
//! - A top-level stub is completed with batched `GET /api/info.json?id=…`
//!   calls, which return full comment things by id.
//!
//! The result is a flat, thread-ordered item list with full bodies and an
//! honest `complete` flag: no evals, no vision, no collapsed replies missed.
//!
//! Both API paths are gated on Reddit's `loid` client token (set by the
//! page-load JS challenge): without it the endpoints answer with the
//! network-security wall (HTTP 403) instead of JSON — never the challenge.
//! `ensure_loid` guards the sweep against sending those requests tokenless,
//! and wall/challenge bodies that do slip through are classified as
//! stop-signals (partial results + honest note), never hammered.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};

/// Budget for the whole comment sweep (initial fetch + stub resolution).
const TOTAL_BUDGET: Duration = Duration::from_secs(25);
/// Hard cap on HTTP requests the sweep may make.
const MAX_REQUESTS: u32 = 40;
/// Per-request in-page fetch timeout.
const PAGE_FETCH_TIMEOUT_MS: u64 = 12_000;
/// Ids per /api/info batch.
const INFO_BATCH: usize = 100;
/// /api/info batches fired concurrently per CDP round-trip.
const WAVE_BATCHES: usize = 5;
/// A more-region worth a dedicated parent-subtree fetch (shortfall ≥ this).
const RECOVERY_MIN_SHORTFALL: i64 = 3;
/// Ceiling on dedicated parent-subtree recovery fetches per sweep.
const MAX_RECOVERY_FETCHES: usize = 12;
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
    /// The comment count reddit reports for the post.
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
    /// Present (true) only when the comment author is the post author.
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
    /// Comments reddit reports for the post.
    pub total: Option<i64>,
    /// True only when the returned list is the complete discussion.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<CommentItem>,
}

/// One comment as the API reports it.
struct Node {
    author: String,
    score: Option<i64>,
    created: Option<i64>,
    body: String,
    permalink: String,
}

/// A "more" region: comments the server reported but did not inline.
#[derive(Clone)]
struct Stub {
    /// Bare id of the parent comment, or the post id for a top-level stub.
    parent: String,
    /// True when the stub sits directly under the post.
    top_level: bool,
    /// How many comments the region claims.
    count: i64,
    /// The ids the server exposed (bare). Can be fewer than `count` on very
    /// large threads.
    ids: Vec<String>,
}

#[derive(Default)]
struct Tree {
    post_id: String,
    post_author: String,
    post_permalink: String,
    nodes: HashMap<String, Node>,
    /// parent bare id → ordered child bare ids.
    children: HashMap<String, Vec<String>>,
    stubs: Vec<Stub>,
}

struct Budget {
    requests: u32,
    deadline: Instant,
}

impl Budget {
    fn new() -> Self {
        Self { requests: 0, deadline: Instant::now() + TOTAL_BUDGET }
    }

    /// Consume `n` request slots; false when the sweep must stop.
    fn take(&mut self, n: u32) -> bool {
        self.requests += n;
        self.ok()
    }

    /// Check the remaining allowance without consuming it.
    fn ok(&self) -> bool {
        self.requests < MAX_REQUESTS && Instant::now() < self.deadline
    }
}

/// Fetch the complete comment tree for a reddit post page.
///
/// `post_base` is the page path up to the post id
/// (`/r/rust/comments/1wn7jni`), `sort` the page's active comment sort. The
/// sweep stops at `cap` comments (or an internal request/time budget) and
/// reports exactly what it got via `count`/`total`/`complete`/`note`.
pub async fn fetch_comments(
    cdp: &CdpSession,
    post_base: &str,
    sort: &str,
    cap: usize,
) -> Result<CommentsPayload> {
    if !post_base.starts_with('/') || !post_base.contains("/comments/") {
        return Err(BladeError::Other(format!("reddit: bad post path {post_base:?}")));
    }
    let sort = if !sort.is_empty() && sort.len() <= 16 && sort.chars().all(|c| c.is_ascii_alphanumeric()) {
        sort
    } else {
        "confidence"
    };
    let cap = cap.clamp(1, MAX_COMMENT_CAP);
    let mut budget = Budget::new();

    tracing::debug!(post = post_base, sort, cap, "reddit sweep start");

    // Never send API paths without the loid token: without it reddit answers
    // with the network-security wall (guaranteed 403s) instead of JSON.
    if !ensure_loid(cdp).await {
        return Err(BladeError::Other(
            "reddit: no loid session token yet (the page-load challenge has not resolved) — load the page once, then retry".into(),
        ));
    }

    if !budget.take(1) {
        return Err(BladeError::Other("reddit: fetch budget exhausted".into()));
    }
    let raw = fetch_json(cdp, &format!("{post_base}.json?limit=500&raw_json=1&sort={sort}")).await?;
    let (post, mut tree) = parse_discussion(&raw)?;
    tracing::debug!(
        nodes = tree.nodes.len(),
        stubs = tree.stubs.len(),
        requests = budget.requests,
        "reddit sweep initial fetch done"
    );

    let mut capped = false;
    let mut budget_hit = false;
    let mut rate_limited = false;
    let mut security_blocked = false;
    let mut gaps: Vec<String> = Vec::new();

    // Phase A — one batched id sweep across every "more" region. Both nested
    // and top-level stubs expose comment ids, and /api/info returns full
    // things for up to 100 ids per call — so ALL ids go through one queue
    // (deduplicated, listing order) instead of one request per region. Large
    // threads carry hundreds of tiny nested regions; per-region requests burn
    // the whole budget before the big top-level region is ever reached.
    let mut pending: Vec<String> = Vec::new();
    {
        let mut seen = HashSet::new();
        for s in &tree.stubs {
            for id in &s.ids {
                if seen.insert(id.clone()) {
                    pending.push(id.clone());
                }
            }
        }
    }
    let post_key = tree.post_id.clone();
    let mut cursor = 0usize;
    while cursor < pending.len() {
        if rate_limited || security_blocked {
            break;
        }
        let need = cap.saturating_sub(tree.nodes.len());
        if need == 0 {
            capped = true;
            break;
        }
        // Round up to whole batches so the cap is never overshot by more
        // than one batch, and cap the wave at WAVE_BATCHES.
        let take = need
            .div_ceil(INFO_BATCH)
            .saturating_mul(INFO_BATCH)
            .min(WAVE_BATCHES * INFO_BATCH)
            .min(pending.len() - cursor);
        let wave = &pending[cursor..cursor + take];
        let sub_batches: Vec<&[String]> = wave.chunks(INFO_BATCH).collect();
        if !budget.take(sub_batches.len() as u32) {
            budget_hit = true;
            break;
        }
        let urls: Vec<String> = sub_batches
            .iter()
            .map(|chunk| {
                let ids_param = chunk
                    .iter()
                    .map(|i| format!("t1_{i}"))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("/api/info.json?id={ids_param}&raw_json=1")
            })
            .collect();
        match fetch_json_many(cdp, &urls).await {
            Ok(results) => {
                let mut failed = false;
                let mut attached = 0usize;
                for (chunk, res) in sub_batches.iter().zip(results) {
                    match res {
                        Ok(raw) => {
                            let things = raw["data"]["children"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            let got = attach_info_things(&mut tree, &things, &post_key);
                            attached += got;
                            if got < chunk.len() {
                                gaps.push(format!(
                                    "{} comments no longer retrievable",
                                    chunk.len() - got
                                ));
                            }
                        }
                        Err(e) => {
                            if is_rate_limit(&e) {
                                rate_limited = true;
                            } else if is_security_block(&e) {
                                security_blocked = true;
                            } else {
                                gaps.push(format!("comment batch fetch failed ({e})"));
                            }
                            failed = true;
                        }
                    }
                }
                tracing::debug!(
                    batches = sub_batches.len(),
                    attached,
                    nodes = tree.nodes.len(),
                    requests = budget.requests,
                    "reddit info wave done"
                );
                if failed {
                    break;
                }
            }
            Err(e) => {
                if is_rate_limit(&e) {
                    rate_limited = true;
                } else if is_security_block(&e) {
                    security_blocked = true;
                } else {
                    gaps.push(format!("comment batch wave failed ({e})"));
                }
                break;
            }
        }
        cursor += take;

        // Human-ish gap between waves — the burst pattern is the one thing
        // reddit's rate-scoring can object to, and only multi-wave sweeps
        // pay it (a few hundred ms), never small threads.
        if cursor < pending.len() && !rate_limited && !security_blocked {
            let pause = 180
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_millis() as u64)
                    .unwrap_or(120)
                    % 320;
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
    }

    // Phase B — parent-subtree recovery for regions whose id lists are only
    // partial (a region can claim N comments but expose fewer ids). One
    // comment-permalink listing returns the parent's entire subtree, so
    // regions with a real shortfall get a dedicated fetch.
    let mut residue_regions = 0usize;
    let mut residue_comments: i64 = 0;
    if !capped && !budget_hit && !rate_limited && !security_blocked {
        let mut recovered = 0usize;
        // Snapshot: the loop merges into `tree` while reading the stub list.
        let stubs_snapshot = tree.stubs.clone();
        for s in &stubs_snapshot {
            if s.count <= s.ids.len() as i64 {
                continue;
            }
            let shortfall = s.count - s.ids.len() as i64;
            if !s.top_level && shortfall >= RECOVERY_MIN_SHORTFALL && recovered < MAX_RECOVERY_FETCHES {
                if tree.nodes.len() >= cap {
                    capped = true;
                    break;
                }
                if !budget.take(1) {
                    budget_hit = true;
                    break;
                }
                recovered += 1;
                let url = format!(
                    "/comments/{}/comment/{}.json?limit=500&raw_json=1&sort={sort}",
                    tree.post_id, s.parent
                );
                match fetch_json(cdp, &url).await {
                    Ok(raw) => match parse_subtree(&raw) {
                        Ok((root, sub)) if root == s.parent => {
                            merge_subtree(&mut tree, &root, sub);
                            continue;
                        }
                        Ok((root, _)) => {
                            gaps.push(format!(
                                "subtree fetch returned {root}, expected {}",
                                s.parent
                            ));
                        }
                        Err(e) => gaps.push(e.to_string()),
                    },
                    Err(e) => {
                        if is_rate_limit(&e) {
                            rate_limited = true;
                            break;
                        }
                        if is_security_block(&e) {
                            security_blocked = true;
                            break;
                        }
                        gaps.push(format!("subtree fetch failed ({e})"));
                    }
                }
            }
            residue_regions += 1;
            residue_comments += shortfall;
        }
    }
    if residue_regions > 0 && !capped && !budget_hit && !rate_limited && !security_blocked {
        gaps.push(format!(
            "{residue_regions} more-regions incomplete ({residue_comments} comments not retrievable)"
        ));
    }

    let (items, dfs_capped) = render_items(&tree, cap);
    tracing::debug!(
        nodes = tree.nodes.len(),
        items = items.len(),
        requests = budget.requests,
        capped,
        budget_hit,
        rate_limited,
        security_blocked,
        "reddit sweep done"
    );
    Ok(assemble(
        post, items, capped || dfs_capped, budget_hit, rate_limited, security_blocked, gaps,
    ))
}

/// Assemble the payload: honest counts, deduplicated notes, completion flag.
fn assemble(
    post: PostMeta,
    items: Vec<CommentItem>,
    capped: bool,
    budget_hit: bool,
    rate_limited: bool,
    security_blocked: bool,
    mut gaps: Vec<String>,
) -> CommentsPayload {
    let count = items.len();
    let total = post.comments;
    let mut notes: Vec<String> = Vec::new();
    let total_s = total.map(|t| t.to_string()).unwrap_or_else(|| "?".into());
    let mut status: Option<String> = None;
    if capped {
        status = Some(format!("capped: {count} of {total_s} comments shown (raise limit to fetch more)"));
    } else if security_blocked {
        status = Some(format!(
            "reddit's network-security wall interrupted the sweep: {count} of {total_s} comments loaded (it's transient — retry in a few seconds for the rest)"
        ));
    } else if rate_limited {
        status = Some(format!(
            "reddit rate limit reached: {count} of {total_s} comments loaded (retry in ~a minute for more)"
        ));
    } else if budget_hit {
        status = Some(format!("fetch budget exhausted: {count} of {total_s} comments loaded"));
    } else if total.is_some_and(|t| (count as i64) < t) {
        status = Some(format!("loaded {count} of {total_s} comments (some are deleted or restricted)"));
    }
    if let Some(s) = status {
        notes.push(s);
    }
    notes.append(&mut gaps);
    let mut seen = HashSet::new();
    notes.retain(|n| seen.insert(n.clone()));
    let note = if notes.is_empty() { None } else { Some(cut_chars(&notes.join("; "), 600)) };
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

/// Parse `[postListing, commentListing]` into post meta + a fresh tree.
fn parse_discussion(raw: &Value) -> Result<(PostMeta, Tree)> {
    let arr = raw
        .as_array()
        .ok_or_else(|| BladeError::Other("reddit: discussion payload is not a listing".into()))?;
    let post_data = arr
        .first()
        .and_then(|l| l.get("data"))
        .and_then(|d| d.get("children"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("data"))
        .ok_or_else(|| BladeError::Other("reddit: post object missing from payload".into()))?;

    let post_id = post_data["id"].as_str().unwrap_or_default().to_string();
    if post_id.is_empty() {
        return Err(BladeError::Other("reddit: post id missing from payload".into()));
    }
    let post_author = post_data["author"].as_str().unwrap_or("[deleted]").to_string();
    let permalink = post_data["permalink"].as_str().unwrap_or_default().to_string();
    let subreddit = post_data["subreddit"].as_str().unwrap_or_default();
    let body = post_data["selftext"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| normalize_text(s, POST_BODY_CAP));

    let post = PostMeta {
        title: post_data["title"].as_str().unwrap_or_default().to_string(),
        author: u_prefix(&post_author),
        subreddit: if subreddit.is_empty() { String::new() } else { format!("r/{subreddit}") },
        score: as_int(&post_data["score"]),
        comments: as_int(&post_data["num_comments"]),
        date: as_epoch(&post_data["created_utc"]).map(format_epoch),
        url: format!("https://www.reddit.com{permalink}"),
        body,
    };

    let mut tree = Tree {
        post_id: post_id.clone(),
        post_author,
        post_permalink: permalink,
        ..Default::default()
    };
    if let Some(kids) = arr
        .get(1)
        .and_then(|l| l.get("data"))
        .and_then(|d| d.get("children"))
        .and_then(|c| c.as_array())
    {
        walk(&mut tree, &post_id, kids);
    }
    Ok((post, tree))
}

/// Walk a comment listing (recursively through `replies`) into the tree.
fn walk(tree: &mut Tree, parent: &str, children: &[Value]) {
    for c in children {
        match c["kind"].as_str().unwrap_or_default() {
            "t1" => {
                let d = &c["data"];
                let Some(id) = d["id"].as_str() else { continue };
                let id = id.to_string();
                tree.nodes.entry(id.clone()).or_insert_with(|| Node {
                    author: d["author"].as_str().unwrap_or("[deleted]").to_string(),
                    score: as_int(&d["score"]),
                    created: as_epoch(&d["created_utc"]),
                    body: d["body"].as_str().unwrap_or_default().to_string(),
                    permalink: d["permalink"].as_str().unwrap_or_default().to_string(),
                });
                tree.children.entry(parent.to_string()).or_default().push(id.clone());
                if let Some(kids) = d
                    .get("replies")
                    .and_then(|r| r.get("data"))
                    .and_then(|dd| dd.get("children"))
                    .and_then(|k| k.as_array())
                {
                    walk(tree, &id, kids);
                }
            }
            "more" => {
                let d = &c["data"];
                let parent_full = d["parent_id"].as_str().unwrap_or_default();
                let ids: Vec<String> = d["children"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                tree.stubs.push(Stub {
                    parent: strip_prefix(parent_full).to_string(),
                    top_level: parent_full.starts_with("t3_"),
                    count: as_int(&d["count"]).unwrap_or(0),
                    ids,
                });
            }
            _ => {}
        }
    }
}

/// Parse a comment-permalink payload
/// (`[postListing, listingOfTheComment]`) into the comment's subtree.
fn parse_subtree(raw: &Value) -> Result<(String, Tree)> {
    let arr = raw
        .as_array()
        .ok_or_else(|| BladeError::Other("reddit: subtree payload is not a listing".into()))?;
    let first = arr
        .get(1)
        .and_then(|l| l.get("data"))
        .and_then(|d| d.get("children"))
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| BladeError::Other("reddit: comment subtree is empty".into()))?;
    if first["kind"].as_str() != Some("t1") {
        return Err(BladeError::Other(
            "reddit: comment subtree unavailable (deleted or removed)".into(),
        ));
    }
    let root = first["data"]["id"].as_str().unwrap_or_default().to_string();
    if root.is_empty() {
        return Err(BladeError::Other("reddit: comment subtree has no root id".into()));
    }
    let mut sub = Tree::default();
    walk(&mut sub, "", std::slice::from_ref(first));
    sub.children.remove("");
    Ok((root, sub))
}

/// Merge a fetched subtree into the tree. The fetched children of the root
/// are authoritative and complete; anything previously known that the fetch
/// omitted is kept after them.
fn merge_subtree(tree: &mut Tree, root: &str, sub: Tree) {
    for (id, node) in sub.nodes {
        tree.nodes.insert(id, node);
    }
    for (pid, kids) in sub.children {
        if pid == root {
            let mut merged = kids;
            if let Some(old) = tree.children.get(&pid) {
                for k in old {
                    if !merged.contains(k) {
                        merged.push(k.clone());
                    }
                }
            }
            tree.children.insert(pid, merged);
        } else {
            tree.children.insert(pid, kids);
        }
    }
    tree.children.entry(root.to_string()).or_default();
    // Stubs discovered inside the subtree resolve after the current ones.
    tree.stubs.extend(sub.stubs);
}

/// Attach `/api/info` things under their real parent; returns how many were
/// t1 comments.
fn attach_info_things(tree: &mut Tree, things: &[Value], fallback_parent: &str) -> usize {
    let mut got = 0usize;
    for t in things {
        if t["kind"].as_str() != Some("t1") {
            continue;
        }
        let d = &t["data"];
        let Some(id) = d["id"].as_str() else { continue };
        let id = id.to_string();
        tree.nodes.insert(
            id.clone(),
            Node {
                author: d["author"].as_str().unwrap_or("[deleted]").to_string(),
                score: as_int(&d["score"]),
                created: as_epoch(&d["created_utc"]),
                body: d["body"].as_str().unwrap_or_default().to_string(),
                permalink: d["permalink"].as_str().unwrap_or_default().to_string(),
            },
        );
        let parent = d["parent_id"].as_str().map(strip_prefix).unwrap_or(fallback_parent);
        let list = tree.children.entry(parent.to_string()).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
        got += 1;
    }
    got
}

/// Thread-ordered (DFS) render, trimmed at `cap`; returns whether trimming
/// happened.
fn render_items(tree: &Tree, cap: usize) -> (Vec<CommentItem>, bool) {
    let mut items = Vec::new();
    let mut capped = false;
    let Some(roots) = tree.children.get(&tree.post_id) else {
        return (items, false);
    };
    let mut stack: Vec<(String, usize)> = roots.iter().rev().map(|id| (id.clone(), 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if items.len() >= cap {
            capped = true;
            break;
        }
        let Some(n) = tree.nodes.get(&id) else { continue };
        let url = if n.permalink.is_empty() {
            format!("https://www.reddit.com{}comment/{id}/", tree.post_permalink)
        } else {
            format!("https://www.reddit.com{}", n.permalink)
        };
        items.push(CommentItem {
            id: id.clone(),
            author: u_prefix(&n.author),
            score: n.score,
            date: n.created.map(format_epoch),
            depth,
            op: if n.author == tree.post_author && !n.author.starts_with('[') {
                Some(true)
            } else {
                None
            },
            text: normalize_text(&n.body, COMMENT_TEXT_CAP),
            url,
        });
        if let Some(kids) = tree.children.get(&id) {
            for k in kids.iter().rev() {
                stack.push((k.clone(), depth + 1));
            }
        }
    }
    (items, capped)
}

/// Reddit gates its JSON paths (`.json`, `/api/info`, subtree listings) on
/// the `loid` client token: without it they answer with the network-security
/// wall (HTTP 403) rather than the JS challenge that HTML navigations get.
/// The token is set by the page-load challenge and persists in the profile,
/// so it is missing only on a cold profile or mid-challenge. Wait briefly
/// (the challenge auto-submits in ~1s), then re-serve the page once; give up
/// honestly rather than burn the sweep on guaranteed 403s.
async fn ensure_loid(cdp: &CdpSession) -> bool {
    async fn has_loid(cdp: &CdpSession) -> bool {
        cdp.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "document.cookie.includes('loid=')",
                "returnByValue": true,
            })),
        )
        .await
        .ok()
        .and_then(|r| {
            r.get("result")
                .and_then(|x| x.get("value"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false)
    }

    if has_loid(cdp).await {
        return true;
    }
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if has_loid(cdp).await {
            return true;
        }
    }
    // Still tokenless — re-serve the page; the challenge resolves in ~1s
    // and its solved response sets `loid`.
    let _ = cdp
        .send("Page.reload", Some(serde_json::json!({ "ignoreCache": false })))
        .await;
    let _ = crate::page::wait_for_load(cdp, Duration::from_secs(10)).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if has_loid(cdp).await {
            return true;
        }
    }
    false
}

/// Marker classification for HTML bodies served to reddit API paths: the
/// network-security wall and the unsolved JS challenge both mean "stop the
/// sweep — this is a gate, not a gap". Anything else keeps the generic
/// HTTP / parse error.
fn classify_gate_body(path: &str, text: &str) -> Option<BladeError> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("blocked by network security") || lower.contains("whoa there, pardner") {
        return Some(BladeError::Other(format!(
            "reddit: network-security block (soft, transient — retry shortly) on {path}"
        )));
    }
    if lower.contains("js_challenge") || lower.contains("requestsubmit") {
        return Some(BladeError::Other(format!(
            "reddit: JS challenge served to an API path on {path}"
        )));
    }
    None
}

/// Fetch one same-origin reddit JSON path from inside the page.
async fn fetch_json(cdp: &CdpSession, path: &str) -> Result<Value> {
    let url_js = serde_json::to_string(path)?;
    let expr = format!(
        "(async()=>{{try{{const c=new AbortController();const t=setTimeout(()=>c.abort(),{PAGE_FETCH_TIMEOUT_MS});\
const r=await fetch({url_js},{{credentials:'include',signal:c.signal}});clearTimeout(t);const x=await r.text();\
return {{s:r.status,t:x}};}}catch(e){{return {{s:0,t:String(e)}}}}}})()"
    );
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expr,
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
            .unwrap_or("fetch failed");
        return Err(BladeError::Other(format!("reddit: {}", crate::platform::truncate_utf8(msg, 200))));
    }
    let val = res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
    let status = val["s"].as_i64().unwrap_or(0);
    let text = val["t"].as_str().unwrap_or_default();
    if status == 429 {
        return Err(BladeError::Other("reddit: rate limited (HTTP 429)".into()));
    }
    if status != 200 {
        return Err(classify_gate_body(path, text)
            .unwrap_or_else(|| BladeError::Other(format!("reddit: GET {path} → HTTP {status}"))));
    }
    if text.trim().is_empty() {
        return Err(BladeError::Other("reddit: empty response (throttled?)".into()));
    }
    serde_json::from_str(text).map_err(|e| {
        classify_gate_body(path, text)
            .unwrap_or_else(|| BladeError::Other(format!("reddit: non-JSON response from {path} ({e})")))
    })
}

/// Fetch several reddit JSON paths concurrently in one CDP round-trip.
/// Each URL resolves to `Ok(value)` / `Err(reason)` independently.
async fn fetch_json_many(cdp: &CdpSession, urls: &[String]) -> Result<Vec<Result<Value>>> {
    let urls_js = serde_json::to_string(urls)?;
    let expr = format!(
        "(async()=>{{const us={urls_js};return await Promise.all(us.map(u=>{{const c=new AbortController();\
const tm=setTimeout(()=>c.abort(),{PAGE_FETCH_TIMEOUT_MS});\
return fetch(u,{{credentials:'include',signal:c.signal}}).then(r=>r.text().then(t=>({{s:r.status,t}})))\
.catch(e=>({{s:0,t:String(e)}})).finally(()=>clearTimeout(tm));}}));}})()"
    );
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expr,
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
            .unwrap_or("batch fetch failed");
        return Err(BladeError::Other(format!("reddit: {}", crate::platform::truncate_utf8(msg, 200))));
    }
    let value = res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for (item, url) in arr.iter().zip(urls) {
        let status = item["s"].as_i64().unwrap_or(0);
        let text = item["t"].as_str().unwrap_or_default();
        if status == 429 {
            out.push(Err(BladeError::Other("rate limited (HTTP 429)".into())));
        } else if status != 200 {
            out.push(Err(classify_gate_body(url, text)
                .unwrap_or_else(|| BladeError::Other(format!("HTTP {status}")))));
        } else if text.trim().is_empty() {
            out.push(Err(BladeError::Other("empty response (throttled?)".into())));
        } else {
            out.push(serde_json::from_str(text).map_err(|e| {
                classify_gate_body(url, text)
                    .unwrap_or_else(|| BladeError::Other(format!("non-JSON response ({e})")))
            }));
        }
    }
    Ok(out)
}

/// Rate-limit signal: Reddit throttles with 429s (and empty bodies).
fn is_rate_limit(e: &BladeError) -> bool {
    let s = e.to_string();
    s.contains("429") || s.contains("throttled")
}

/// Security-wall signal: the sweep hit reddit's network-security block page
/// and must stop — it is transient, and hammering extends it.
pub fn is_security_block(e: &BladeError) -> bool {
    e.to_string().contains("network-security block")
}

fn strip_prefix(fullname: &str) -> &str {
    fullname
        .strip_prefix("t1_")
        .or_else(|| fullname.strip_prefix("t3_"))
        .unwrap_or(fullname)
}

fn u_prefix(author: &str) -> String {
    if author.starts_with('[') {
        author.to_string()
    } else {
        format!("u/{author}")
    }
}

fn as_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64))
}

fn as_epoch(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// Epoch seconds → `YYYY-MM-DD HH:MM UTC` (civil-from-days; no chrono).
fn format_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm) = (rem / 3_600, (rem % 3_600) / 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// Normalize a comment body for reading: CRLF → LF, trailing whitespace
/// stripped per line, 2+ blank lines collapsed to one, then capped.
fn normalize_text(s: &str, cap: usize) -> String {
    let t = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(t.len().min(cap + 8));
    let mut blank = 0usize;
    for line in t.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    cut_chars(out.trim(), cap)
}

fn cut_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut o: String = s.chars().take(cap).collect();
    o.push('…');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/reddit_post_comments.json");
        let raw = std::fs::read_to_string(&path).expect("fixture readable");
        serde_json::from_str(&raw).expect("fixture is JSON")
    }

    fn post_listing(id: &str, author: &str) -> Value {
        json!({"kind":"Listing","data":{"children":[{"kind":"t3","data":{
            "id":id,"author":author,"subreddit":"test","title":"T",
            "score":10,"num_comments":4,"created_utc":1_750_000_000,
            "permalink":format!("/r/test/comments/{id}/p/"),"selftext":""
        }}]}})
    }

    fn t1(id: &str, author: &str, parent: &str, body: &str, replies: Value) -> Value {
        json!({"kind":"t1","data":{
            "id":id,"author":author,"parent_id":parent,"score":5,
            "created_utc":1_750_000_000,
            "permalink":format!("/r/test/comments/abc/comment/{id}/"),
            "body":body,"replies":replies
        }})
    }

    fn more(parent: &str, count: i64, ids: &[&str]) -> Value {
        json!({"kind":"more","data":{"parent_id":parent,"count":count,"children":ids}})
    }

    #[test]
    fn fixture_parses_the_complete_thread() {
        let (post, tree) = parse_discussion(&fixture()).expect("parse");
        assert_eq!(post.title, "Fearless SIMD v1.0 is here");
        assert_eq!(post.author, "u/Shnatsel");
        assert_eq!(post.subreddit, "r/rust");
        assert_eq!(post.comments, Some(26));
        assert_eq!(post.score, Some(486));
        assert_eq!(tree.nodes.len(), 26);
        assert!(tree.stubs.is_empty(), "26-comment thread carries no stubs");

        let roots = tree.children.get(&tree.post_id).expect("roots");
        assert_eq!(roots.len(), 12, "12 top-level comments");

        let (items, capped) = render_items(&tree, DEFAULT_COMMENT_CAP);
        assert!(!capped);
        assert_eq!(items.len(), 26);

        // DFS order: first root, then its reply.
        assert_eq!(items[0].id, "pbcven4");
        assert_eq!(items[1].id, "pbd2e82");
        assert_eq!(items[2].id, "pbd57io");
        assert_eq!(items[2].depth, 1);

        // The previously DOM-collapsed depth-3 reply is present.
        let deep = items.iter().find(|i| i.id == "pbehi7l").expect("depth-3 comment");
        assert_eq!(deep.depth, 3);

        // Full bodies, no mid-word 500-char cut.
        let straight = items.iter().find(|i| i.id == "pbd2e82").expect("comment");
        assert!(straight.text.chars().count() > 500, "long body kept whole");
        let reply = items.iter().find(|i| i.id == "pbd57io").expect("reply");
        assert!(reply.text.chars().count() > 500, "1194-char reply kept whole");

        // Dates formatted from created_utc.
        assert_eq!(straight.date.as_deref(), Some("2026-09-22 13:30 UTC"));

        // OP flag on Shnatsel's comments, not on others.
        assert_eq!(reply.op, Some(true));
        assert_eq!(straight.op, None);

        // Complete: rendered count equals the reported total.
        let payload = assemble(post, items, false, false, false, false, vec![]);
        assert!(payload.complete);
        assert!(payload.note.is_none());
        assert_eq!(payload.count, 26);
    }

    #[test]
    fn epoch_formatting() {
        assert_eq!(format_epoch(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_epoch(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_epoch(1_709_164_800), "2024-02-29 00:00 UTC");
        assert_eq!(format_epoch(1_790_083_846), "2026-09-22 13:30 UTC");
    }

    #[test]
    fn nested_stub_resolves_via_subtree_with_correct_depths() {
        // Initial listing: root comment `a` with a stub for 2 more replies.
        let initial = json!([
            post_listing("abc", "opuser"),
            {"kind":"Listing","data":{"children":[
                t1("a","alice","t3_abc","A", json!("")),
                more("t1_a", 2, &["b","c"])
            ]}}
        ]);
        let (post, mut tree) = parse_discussion(&initial).unwrap();
        assert_eq!(tree.stubs.len(), 1);
        assert!(!tree.stubs[0].top_level);

        // Permalink listing for `a`: full subtree — b (with reply d) and c.
        let subtree = json!([
            post_listing("abc", "opuser"),
            {"kind":"Listing","data":{"children":[
                t1("a","alice","t3_abc","A",
                    json!({"kind":"Listing","data":{"children":[
                        t1("b","bob","t1_a","B",
                            json!({"kind":"Listing","data":{"children":[
                                t1("d","dan","t1_b","D", json!(""))
                            ]}})),
                        t1("c","carol","t1_a","C", json!(""))
                    ]}}))
            ]}}
        ]);
        let (root, sub) = parse_subtree(&subtree).unwrap();
        assert_eq!(root, "a");
        merge_subtree(&mut tree, &root, sub);

        let (items, _) = render_items(&tree, 100);
        let order: Vec<(String, usize)> = items.iter().map(|i| (i.id.clone(), i.depth)).collect();
        assert_eq!(
            order,
            vec![
                ("a".into(), 0),
                ("b".into(), 1),
                ("d".into(), 2),
                ("c".into(), 1),
            ]
        );
        let payload = assemble(post, items, false, false, false, false, vec![]);
        assert!(payload.complete);
    }

    #[test]
    fn top_level_stub_resolves_via_info_batches() {
        let initial = json!([
            post_listing("abc", "opuser"),
            {"kind":"Listing","data":{"children":[
                t1("a","alice","t3_abc","A", json!("")),
                more("t3_abc", 3, &["b","c","d"])
            ]}}
        ]);
        let (post, mut tree) = parse_discussion(&initial).unwrap();
        assert!(tree.stubs[0].top_level);
        assert_eq!(tree.stubs[0].parent, "abc");

        let things = vec![
            json!({"kind":"t1","data":{"id":"b","author":"bob","parent_id":"t3_abc","score":2,
                "created_utc":1_750_000_001,"permalink":"/r/test/comments/abc/comment/b/","body":"B"}}),
            json!({"kind":"t1","data":{"id":"c","author":"carol","parent_id":"t3_abc","score":1,
                "created_utc":1_750_000_002,"permalink":"/r/test/comments/abc/comment/c/","body":"C"}}),
            json!({"kind":"t1","data":{"id":"d","author":"dan","parent_id":"t3_abc","score":1,
                "created_utc":1_750_000_003,"permalink":"/r/test/comments/abc/comment/d/","body":"D"}}),
        ];
        let got = attach_info_things(&mut tree, &things, "abc");
        assert_eq!(got, 3);

        let (items, _) = render_items(&tree, 100);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"], "resolved top-level comments appended in order");
        let payload = assemble(post, items, false, false, false, false, vec![]);
        assert!(payload.complete);
    }

    #[test]
    fn cap_and_gaps_produce_honest_notes() {
        let (post, tree) = parse_discussion(&fixture()).unwrap();
        let (items, capped) = render_items(&tree, 3);
        assert!(capped);
        assert_eq!(items.len(), 3);

        let payload = assemble(post, items, capped, false, false, false, vec![]);
        assert!(!payload.complete);
        let note = payload.note.unwrap();
        assert!(note.contains("capped: 3 of 26"), "note names the cap: {note}");

        // Clean full render → complete, no note.
        let (post2, tree2) = parse_discussion(&fixture()).unwrap();
        let (items2, capped2) = render_items(&tree2, 1_000);
        let payload2 = assemble(post2, items2, capped2, false, false, false, vec![]);
        assert!(payload2.complete);

        // Budget shortfall reads as a budget issue, not as deletions.
        let (post3, tree3) = parse_discussion(&fixture()).unwrap();
        let (items3, _) = render_items(&tree3, 3);
        let payload3 = assemble(post3, items3, false, true, false, false, vec![]);
        assert!(!payload3.complete);
        let note3 = payload3.note.unwrap();
        assert!(note3.contains("fetch budget exhausted: 3 of 26"), "{note3}");
        assert!(!note3.contains("deleted"), "budget shortfall must not read as deletions: {note3}");

        // Rate limiting reads as its own status, with a retry hint.
        let (post4, tree4) = parse_discussion(&fixture()).unwrap();
        let (items4, _) = render_items(&tree4, 3);
        let payload4 = assemble(post4, items4, false, false, true, false, vec![]);
        assert!(!payload4.complete);
        let note4 = payload4.note.unwrap();
        assert!(note4.contains("rate limit reached: 3 of 26"), "{note4}");
        assert!(note4.contains("retry in ~a minute"), "{note4}");

        // The network-security wall reads as its own transient status.
        let (post5, tree5) = parse_discussion(&fixture()).unwrap();
        let (items5, _) = render_items(&tree5, 3);
        let payload5 = assemble(post5, items5, false, false, false, true, vec![]);
        assert!(!payload5.complete);
        let note5 = payload5.note.unwrap();
        assert!(note5.contains("network-security wall interrupted"), "{note5}");
        assert!(note5.contains("transient"), "{note5}");
    }

    #[test]
    fn gate_bodies_classify_by_markers() {
        // Real block-page text (captured live from a tokenless `.json` hit).
        let wall = "<div>You've been blocked by network security.</div>\
                    <div>If you think you've been blocked by mistake, file a ticket below and we'll look into it.</div>";
        let e = classify_gate_body("/r/x/comments/y/.json", wall).expect("wall classified");
        assert!(is_security_block(&e));
        assert!(!is_rate_limit(&e));
        assert!(e.to_string().contains("transient"), "{e}");

        // The classic variant some reddit edges still serve.
        let whoa = "<h1>Whoa there, pardner!</h1>Your request has been blocked due to a network policy.";
        assert!(is_security_block(
            &classify_gate_body("/api/info.json", whoa).expect("whoa variant classified")
        ));

        // The challenge page (hidden auto-submit form) served to an API path.
        let challenge = "<form hidden method=\"GET\" action=\"/r/x/\">\
            <input type=\"hidden\" name=\"solution\" />\
            <input type=\"hidden\" name=\"js_challenge\" value=\"1\"/>\
            <input type=\"hidden\" name=\"jsc_token\" value=\"abc\"/></form>\
            <script>document.forms[0].requestSubmit()</script>";
        let e = classify_gate_body("/api/info.json", challenge).expect("challenge classified");
        assert!(!is_security_block(&e));
        assert!(e.to_string().contains("JS challenge"), "{e}");

        // Ordinary failures keep the generic path — no false classification.
        assert!(classify_gate_body("/x.json", "{\"kind\":\"Listing\"}").is_none());
        assert!(classify_gate_body("/x.json", "<html><body>Service Unavailable</body></html>").is_none());
    }

    #[test]
    fn gate_classification_is_case_insensitive() {
        assert!(classify_gate_body("/x.json", "YOU'VE BEEN BLOCKED BY Network Security.").is_some());
    }

    #[test]
    fn body_normalization_and_caps() {
        let n = normalize_text("line one   \r\n\r\n\r\nline two\r\n", 100);
        assert_eq!(n, "line one\n\nline two");
        let long = "x".repeat(80);
        let cut = normalize_text(&long, 10);
        assert_eq!(cut.chars().count(), 11, "10 chars + ellipsis");
        assert!(cut.ends_with('…'));
        let utf = "héllo wörld";
        assert_eq!(cut_chars(utf, 100), utf);
    }

    #[test]
    fn payload_serialization_keeps_field_order_and_skips_empty() {
        let (post, tree) = parse_discussion(&fixture()).unwrap();
        let (items, _) = render_items(&tree, 100);
        let payload = assemble(post, items, false, false, false, false, vec![]);
        let s = serde_json::to_string(&payload).unwrap();
        assert!(s.starts_with("{\"container\":\"reddit-comments\","), "container first: {s:.80}");
        assert!(s.contains("\"complete\":true"));
        assert!(!s.contains("\"note\""), "clean payload carries no note");
        // Non-op comments do not serialize the op key.
        let first_item = s.find("\"items\":[").unwrap();
        let head: String = s.get(first_item..).unwrap_or("").chars().take(400).collect();
        assert!(!head.contains("\"op\":false"), "op is omitted unless true");
    }
}
