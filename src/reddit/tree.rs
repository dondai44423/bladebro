//! Reddit listing parsing + thread rendering: listing JSON → tree → flat
//! thread-ordered items, plus the shared value/format helpers.

use super::*;

/// Parse `[postListing, commentListing]` into post meta + a fresh tree.
pub(super) fn parse_discussion(raw: &Value) -> Result<(PostMeta, Tree)> {
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
        return Err(BladeError::Other(
            "reddit: post id missing from payload".into(),
        ));
    }
    let post_author = post_data["author"]
        .as_str()
        .unwrap_or("[deleted]")
        .to_string();
    let permalink = post_data["permalink"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let subreddit = post_data["subreddit"].as_str().unwrap_or_default();
    let body = post_data["selftext"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| normalize_text(s, POST_BODY_CAP));

    let post = PostMeta {
        title: post_data["title"].as_str().unwrap_or_default().to_string(),
        author: u_prefix(&post_author),
        subreddit: if subreddit.is_empty() {
            String::new()
        } else {
            format!("r/{subreddit}")
        },
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
                tree.children
                    .entry(parent.to_string())
                    .or_default()
                    .push(id.clone());
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
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
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
pub(super) fn parse_subtree(raw: &Value) -> Result<(String, Tree)> {
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
        return Err(BladeError::Other(
            "reddit: comment subtree has no root id".into(),
        ));
    }
    let mut sub = Tree::default();
    walk(&mut sub, "", std::slice::from_ref(first));
    sub.children.remove("");
    Ok((root, sub))
}

/// Merge a fetched subtree into the tree. The fetched children of the root
/// are authoritative and complete; anything previously known that the fetch
/// omitted is kept after them.
pub(super) fn merge_subtree(tree: &mut Tree, root: &str, sub: Tree) {
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
pub(super) fn attach_info_things(
    tree: &mut Tree,
    things: &[Value],
    fallback_parent: &str,
) -> usize {
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
        let parent = d["parent_id"]
            .as_str()
            .map(strip_prefix)
            .unwrap_or(fallback_parent);
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
pub(super) fn render_items(tree: &Tree, cap: usize) -> (Vec<CommentItem>, bool) {
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
        let Some(n) = tree.nodes.get(&id) else {
            continue;
        };
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

/// Does this stub hide comments worth a dedicated parent-subtree fetch?
/// Two shapes: an EMPTY stub (count 0, no ids — reddit's collapsed-region
/// marker; the parent's own permalink listing still serves the subtree) and
/// a partial region (count exceeds the id list) with a real shortfall.
pub(super) fn stub_hides_comments(s: &Stub) -> bool {
    let empty = s.count == 0 && s.ids.is_empty();
    !s.top_level && (empty || s.count - s.ids.len() as i64 >= RECOVERY_MIN_SHORTFALL)
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
pub(super) fn format_epoch(secs: i64) -> String {
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
