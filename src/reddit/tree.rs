//! Reddit DOM/HTML comment-tree builder + lossless DFS thread renderer.
//!
//! Ingests structured snapshots produced by `js/parse_thread.js` (initial DOM,
//! `<faceplate-partial>` HTML fragments, and `div.fold-more` comment permalink
//! HTML pages) into a parent-linked `Tree`, then renders flat thread-ordered
//! items with exact nesting depth and zero dropped or duplicated comments.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::*;

#[derive(Clone, Debug)]
pub(super) struct PartialTask {
    pub url: String,
    pub parent_id: String,
    pub claimed: Option<i64>,
}

#[derive(Clone, Debug)]
pub(super) struct FoldTask {
    pub url: String,
    pub parent_id: String,
    pub comment_id: String,
}

#[derive(Debug)]
pub(super) struct ParsedBatch {
    pub post: Option<PostMeta>,
    pub post_id: String,
    pub post_permalink: String,
    pub post_author_bare: String,
    pub comments: Vec<RawComment>,
    pub partials: Vec<PartialTask>,
    pub folds: Vec<FoldTask>,
    pub clickable_only_partials: usize,
    pub legacy_more_count: usize,
}

#[derive(Debug)]
pub(super) struct RawComment {
    pub id: String,
    pub parent_id: String,
    pub author: String,
    pub score: Option<i64>,
    pub depth: Option<usize>,
    pub date: Option<String>,
    pub permalink: String,
    pub body: String,
}

/// One comment node stored in the thread tree.
pub(super) struct Node {
    pub author: String,
    pub score: Option<i64>,
    pub date: Option<String>,
    pub hint_depth: Option<usize>,
    pub body: String,
    pub permalink: String,
}

#[derive(Default)]
pub(super) struct Tree {
    pub post_id: String,
    pub post_author: String,
    pub post_permalink: String,
    pub nodes: HashMap<String, Node>,
    /// Deterministic insertion order of comment IDs.
    pub order: Vec<String>,
    /// parent bare id → ordered child bare ids.
    pub children: HashMap<String, Vec<String>>,
    /// child bare id → recorded parent bare id.
    pub parents: HashMap<String, String>,
}

/// Decode one `parseRedditThread(...)` JSON payload into typed Rust structs.
pub(super) fn decode_batch(raw: &Value, fallback_post_id: &str) -> ParsedBatch {
    let mut post_id = raw["post_id"].as_str().unwrap_or("").trim().to_string();
    if post_id.is_empty() {
        post_id = fallback_post_id.to_string();
    }

    let mut post_permalink = String::new();
    let mut post_author_bare = String::new();
    let post = raw.get("post").and_then(Value::as_object).map(|p| {
        if let Some(id) = p
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            post_id = id.to_string();
        }
        let author_raw = p
            .get("author")
            .and_then(Value::as_str)
            .unwrap_or("[deleted]");
        post_author_bare = bare_author(author_raw);
        let permalink = p
            .get("permalink")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        post_permalink = permalink.clone();
        let url = p
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| canonical_reddit_url(&permalink));
        let body = p
            .get("body")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| normalize_text(s, POST_BODY_CAP));
        let date = p
            .get("date")
            .and_then(Value::as_str)
            .and_then(normalize_date);
        PostMeta {
            title: p
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            author: u_prefix(&post_author_bare),
            subreddit: p
                .get("subreddit")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            score: p.get("score").and_then(as_int),
            comments: p.get("comments").and_then(as_int),
            date,
            url,
            body,
        }
    });

    let comments = raw
        .get("comments")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let id = strip_prefix(c.get("id")?.as_str()?).trim().to_string();
                    if id.is_empty() {
                        return None;
                    }
                    let parent_id = c
                        .get("parent_id")
                        .and_then(Value::as_str)
                        .map(strip_prefix)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let author = bare_author(
                        c.get("author")
                            .and_then(Value::as_str)
                            .unwrap_or("[deleted]"),
                    );
                    let score = c.get("score").and_then(as_int);
                    let depth = c
                        .get("depth")
                        .and_then(as_int)
                        .and_then(|d| usize::try_from(d).ok());
                    let date = c
                        .get("date")
                        .and_then(Value::as_str)
                        .and_then(normalize_date);
                    let permalink = c
                        .get("permalink")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let body = c
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some(RawComment {
                        id,
                        parent_id,
                        author,
                        score,
                        depth,
                        date,
                        permalink,
                        body,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let partials = raw
        .get("partials")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let url = p.get("url")?.as_str()?.trim().to_string();
                    if url.is_empty() {
                        return None;
                    }
                    let parent_id = p
                        .get("parent_id")
                        .and_then(Value::as_str)
                        .map(strip_prefix)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let claimed = p.get("claimed").and_then(as_int);
                    Some(PartialTask {
                        url,
                        parent_id,
                        claimed,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let folds = raw
        .get("folds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let url = f.get("url")?.as_str()?.trim().to_string();
                    if url.is_empty() {
                        return None;
                    }
                    let parent_id = f
                        .get("parent_id")
                        .and_then(Value::as_str)
                        .map(strip_prefix)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let comment_id = f
                        .get("comment_id")
                        .and_then(Value::as_str)
                        .map(strip_prefix)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    Some(FoldTask {
                        url,
                        parent_id,
                        comment_id,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let clickable_only_partials = raw
        .get("clickable_only_partials")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let legacy_more_count = raw
        .get("legacy_more_count")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    ParsedBatch {
        post,
        post_id,
        post_permalink,
        post_author_bare,
        comments,
        partials,
        folds,
        clickable_only_partials,
        legacy_more_count,
    }
}

/// Merge parsed comments into the tree without duplicating existing nodes or
/// corrupting established parent links. Returns how many new comments arrived.
pub(super) fn ingest_comments(
    tree: &mut Tree,
    comments: Vec<RawComment>,
    fallback_parent: &str,
) -> usize {
    let mut added = 0usize;
    for c in comments {
        if let Some(existing) = tree.nodes.get_mut(&c.id) {
            if existing.body.trim().is_empty() && !c.body.trim().is_empty() {
                existing.body = c.body;
            }
            if existing.date.is_none() && c.date.is_some() {
                existing.date = c.date;
            }
            if existing.score.is_none() && c.score.is_some() {
                existing.score = c.score;
            }
            if existing.permalink.is_empty() && !c.permalink.is_empty() {
                existing.permalink = c.permalink;
            }
            if existing.author == "[deleted]" && c.author != "[deleted]" {
                existing.author = c.author;
            }
            continue;
        }

        let mut parent = if !c.parent_id.is_empty() && c.parent_id != c.id {
            c.parent_id
        } else if !fallback_parent.is_empty() && fallback_parent != c.id {
            fallback_parent.to_string()
        } else {
            tree.post_id.clone()
        };
        if parent.is_empty() {
            parent = tree.post_id.clone();
        }

        tree.nodes.insert(
            c.id.clone(),
            Node {
                author: c.author,
                score: c.score,
                date: c.date,
                hint_depth: c.depth,
                body: c.body,
                permalink: c.permalink,
            },
        );
        tree.order.push(c.id.clone());
        tree.parents.insert(c.id.clone(), parent.clone());
        let kids = tree.children.entry(parent).or_default();
        if !kids.contains(&c.id) {
            kids.push(c.id);
        }
        added += 1;
    }
    added
}

fn push_dfs(
    tree: &Tree,
    start_id: &str,
    start_depth: usize,
    cap: usize,
    visited: &mut HashSet<String>,
    items: &mut Vec<CommentItem>,
    capped: &mut bool,
) {
    let mut stack: Vec<(String, usize)> = vec![(start_id.to_string(), start_depth)];
    while let Some((id, depth)) = stack.pop() {
        if items.len() >= cap {
            *capped = true;
            return;
        }
        if !visited.insert(id.clone()) {
            continue;
        }
        let Some(n) = tree.nodes.get(&id) else {
            continue;
        };
        let url = if n.permalink.is_empty() {
            if tree.post_permalink.is_empty() {
                format!(
                    "https://www.reddit.com/comments/{}/comment/{id}/",
                    tree.post_id
                )
            } else {
                let base = tree.post_permalink.trim_end_matches('/');
                format!("https://www.reddit.com{base}/comment/{id}/")
            }
        } else {
            canonical_reddit_url(&n.permalink)
        };
        items.push(CommentItem {
            id: id.clone(),
            author: u_prefix(&n.author),
            score: n.score,
            date: n.date.clone(),
            depth,
            op: if !tree.post_author.is_empty()
                && n.author == tree.post_author
                && !n.author.starts_with('[')
            {
                Some(true)
            } else {
                None
            },
            text: normalize_text(&n.body, COMMENT_TEXT_CAP),
            url,
        });
        if let Some(kids) = tree.children.get(&id) {
            for k in kids.iter().rev() {
                if !visited.contains(k) {
                    stack.push((k.clone(), depth + 1));
                }
            }
        }
    }
}

/// Lossless DFS thread renderer trimmed at `cap`.
///
/// 1. Walks top-level comments under `tree.post_id` (depth 0 → depth + 1).
/// 2. Walks any detached roots (e.g. a single-comment permalink page or replies
///    whose deleted parent node was omitted by Reddit) in insertion order so no
///    fetched comment is ever silently dropped.
/// 3. Tracks `visited` so cycles or duplicate links can never duplicate output.
pub(super) fn render_items(tree: &Tree, cap: usize) -> (Vec<CommentItem>, bool) {
    let mut items = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut capped = false;

    if let Some(roots) = tree.children.get(&tree.post_id) {
        for root_id in roots {
            if items.len() >= cap {
                capped = true;
                break;
            }
            push_dfs(tree, root_id, 0, cap, &mut visited, &mut items, &mut capped);
        }
    }

    // Detached roots (single-comment permalink pages or replies to omitted
    // deleted parents): parents not in `tree.nodes`.
    if !capped {
        for id in &tree.order {
            if items.len() >= cap {
                capped = true;
                break;
            }
            if visited.contains(id) {
                continue;
            }
            let parent_in_tree = tree
                .parents
                .get(id)
                .is_some_and(|p| tree.nodes.contains_key(p));
            if !parent_in_tree {
                let d0 = tree.nodes.get(id).and_then(|n| n.hint_depth).unwrap_or(0);
                push_dfs(tree, id, d0, cap, &mut visited, &mut items, &mut capped);
            }
        }
    }

    // Final safety pass in case malformed parent references formed a cycle.
    if !capped {
        for id in &tree.order {
            if items.len() >= cap {
                capped = true;
                break;
            }
            if !visited.contains(id) {
                let d0 = tree.nodes.get(id).and_then(|n| n.hint_depth).unwrap_or(0);
                push_dfs(tree, id, d0, cap, &mut visited, &mut items, &mut capped);
            }
        }
    }

    (items, capped)
}

pub(super) fn canonical_reddit_url(permalink: &str) -> String {
    let p = permalink.trim();
    if p.starts_with("http://") || p.starts_with("https://") {
        p.to_string()
    } else if p.starts_with('/') {
        format!("https://www.reddit.com{p}")
    } else if p.is_empty() {
        String::new()
    } else {
        format!("https://www.reddit.com/{p}")
    }
}

pub(super) fn strip_prefix(fullname: &str) -> &str {
    fullname
        .strip_prefix("t1_")
        .or_else(|| fullname.strip_prefix("t3_"))
        .unwrap_or(fullname)
}

pub(super) fn bare_author(raw: &str) -> String {
    let a = raw
        .trim()
        .trim_start_matches('/')
        .strip_prefix("u/")
        .or_else(|| raw.trim().trim_start_matches('/').strip_prefix("U/"))
        .unwrap_or(raw.trim())
        .trim();
    if a.is_empty()
        || a.eq_ignore_ascii_case("[deleted]")
        || a.eq_ignore_ascii_case("deleted")
        || a.eq_ignore_ascii_case("[removed]")
    {
        "[deleted]".to_string()
    } else {
        a.to_string()
    }
}

pub(super) fn u_prefix(author: &str) -> String {
    if author.is_empty() || author.starts_with('[') {
        if author.is_empty() {
            "[deleted]".to_string()
        } else {
            author.to_string()
        }
    } else if author.starts_with("u/") {
        author.to_string()
    } else {
        format!("u/{author}")
    }
}

pub(super) fn as_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64))
}

/// Normalize ISO-8601 timestamps from Shreddit/old.reddit
/// (`2026-11-20T14:22:10.123000+0000`, `2026-11-20T14:22:10Z`) to
/// `YYYY-MM-DD HH:MM UTC`. Non-ISO strings are returned trimmed up to 32 chars.
pub(super) fn normalize_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    if b.len() >= 16
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
    {
        let date_part = &s[..10];
        let time_part = &s[11..16];
        let is_utc = s.ends_with('Z')
            || s.ends_with('z')
            || s.ends_with("+00:00")
            || s.ends_with("+0000")
            || s.ends_with("UTC");
        return Some(if is_utc {
            format!("{date_part} {time_part} UTC")
        } else {
            format!("{date_part} {time_part}")
        });
    }
    Some(cut_chars(s, 32))
}

/// Normalize a comment/post body for reading: CRLF → LF, trailing whitespace
/// stripped per line, 2+ blank lines collapsed to one, then capped.
pub(super) fn normalize_text(s: &str, cap: usize) -> String {
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

pub(super) fn cut_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut o: String = s.chars().take(cap).collect();
    o.push('…');
    o
}
