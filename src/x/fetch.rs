//! The fetch loops: TweetDetail thread walk and timeline walk. Both are
//! cursor-paginated and respect the request/time budget.

use super::*;
use super::gql::{build_url, fetch_api, Budget};
use super::parse::{cut_chars, parse_thread_page, parse_timeline_page, T};

fn focal_id_from_url(url: &str) -> Option<String> {
    let id = url
        .split("/status/")
        .nth(1)?
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Fetch the full conversation for a status page (TweetDetail), one cursor
/// page at a time until the timeline terminates, the cap is reached, or the
/// budget runs out.
pub(super) async fn fetch_thread(
    page: &Page,
    templates: &HashMap<String, GqlTemplate>,
    cap: usize,
    budget: &mut Budget,
) -> Result<XPayload> {
    let tpl = templates.get("TweetDetail").ok_or_else(|| {
        BladeError::Other("x: TweetDetail not captured (page API traffic not seen yet)".into())
    })?;
    let focal_id = focal_id_from_url(page.model().url())
        .ok_or_else(|| BladeError::Other("x: URL has no /status/<id>".into()))?;
    let mut base = tpl.variables.clone();
    {
        let obj = base.as_object_mut().ok_or_else(|| {
            BladeError::Other("x: template variables not an object".into())
        })?;
        obj.insert("focalTweetId".into(), json!(focal_id));
        obj.insert("count".into(), json!(PAGE_COUNT));
        obj.remove("cursor");
    }

    let mut focal: Option<T> = None;
    let mut tweets: Vec<T> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut cursor: Option<String> = None;
    let mut complete = false;
    let mut note: Option<String> = None;
    let mut pages = 0usize;

    loop {
        if !budget.take() {
            note = Some("x: request budget reached — partial results".into());
            break;
        }
        let mut vars = base.clone();
        if let Some(c) = &cursor {
            if let Some(o) = vars.as_object_mut() {
                o.insert("cursor".into(), json!(c));
            }
        }
        let url = build_url(tpl, &vars);
        let (status, body) = fetch_api(page.cdp_ref(), &url, &tpl.headers).await?;
        if status == 429 {
            note = Some("x: rate limited (HTTP 429) — partial results, retry in a minute".into());
            break;
        }
        if status != 200 {
            if pages == 0 {
                // First page rejected: this op's replays are not viable on
                // this page state — let the caller pick a fallback.
                return Err(BladeError::Other(format!("x: replay rejected (HTTP {status})")));
            }
            note = Some(format!(
                "x: HTTP {status} after {pages} page(s) — partial results"
            ));
            break;
        }
        let j: Value = serde_json::from_str(&body)
            .map_err(|e| BladeError::Other(format!("x: non-JSON response ({e})")))?;
        let parsed = parse_thread_page(&j, &focal_id);
        let mut new_items = 0usize;
        if focal.is_none() {
            focal = parsed.focal;
        }
        for t in parsed.tweets {
            if seen.insert(t.id.clone(), ()).is_none() {
                tweets.push(t);
                new_items += 1;
            }
        }
        pages += 1;
        if parsed.terminated || new_items == 0 {
            complete = true;
            break;
        }
        match parsed.bottom_cursor {
            Some(c) if cursor.as_deref() != Some(c.as_str()) => cursor = Some(c),
            // No fresh cursor: there is nothing further to fetch.
            _ => {
                complete = true;
                break;
            }
        }
        if tweets.len() >= cap {
            break;
        }
    }

    let focal = focal.ok_or_else(|| {
        BladeError::Other("x: focal tweet missing from the TweetDetail response".into())
    })?;
    let total = focal.replies;
    let op_author = focal.author.clone();
    let parents: HashMap<String, String> = tweets
        .iter()
        .filter_map(|t| t.in_reply_to.clone().map(|p| (t.id.clone(), p)))
        .collect();
    let mut memo: HashMap<String, Option<i64>> = HashMap::new();
    let mut items: Vec<XItem> = Vec::new();
    for t in tweets.into_iter().take(cap) {
        let depth = depth_of(&t.id, &parents, &focal_id, &mut memo);
        let url = t.permalink();
        items.push(XItem {
            op: t.author == op_author,
            depth,
            reply_to: t.reply_to.clone(),
            id: t.id.clone(),
            author: t.author,
            name: t.name,
            date: t.date,
            text: t.text,
            replies: t.replies,
            reposts: t.reposts,
            likes: t.likes,
            media: t.media,
            url,
        });
    }
    if !complete && note.is_none() {
        note = Some(format!(
            "x: partial — capped at {} items, the thread continues",
            items.len()
        ));
    }
    Ok(XPayload {
        container: "x-thread".into(),
        kind: "status".into(),
        post: Some(XPost {
            id: focal.id.clone(),
            author: focal.author.clone(),
            name: focal.name.clone(),
            text: cut_chars(&focal.text, POST_TEXT_CAP),
            date: focal.date.clone(),
            url: focal.permalink(),
            replies: focal.replies,
            reposts: focal.reposts,
            likes: focal.likes,
            views: focal.views,
            media: focal.media.clone(),
        }),
        count: items.len(),
        total,
        complete,
        note,
        items,
    })
}

/// Fetch a timeline (profile / search / home), cursor-paginated to the cap.
pub(super) async fn fetch_timeline(
    page: &Page,
    kind: &str,
    templates: &HashMap<String, GqlTemplate>,
    cap: usize,
    budget: &mut Budget,
) -> Result<XPayload> {
    let ops: &[&str] = match kind {
        // X has renamed the profile timeline op over time; take whichever
        // the page actually used.
        "profile" => &["UserOriginalsTimeline", "UserTweets", "UserTweetsAndReplies"],
        "search" => &["SearchTimeline"],
        "home" => &["HomeTimeline"],
        _ => unreachable!("kind checked by caller"),
    };
    let (op, tpl) = ops
        .iter()
        .find_map(|o| templates.get(*o).map(|t| (*o, t)))
        .ok_or_else(|| {
            BladeError::Other(format!(
                "x: {} not captured (page API traffic not seen yet)",
                ops.join("/")
            ))
        })?;
    let _ = op;
    let mut base = tpl.variables.clone();
    {
        let obj = base.as_object_mut().ok_or_else(|| {
            BladeError::Other("x: template variables not an object".into())
        })?;
        // Page size stays as the app chose it — op schemas validate their
        // variables strictly, and the cursor does the walking anyway.
        obj.remove("cursor");
    }

    let mut tweets: Vec<T> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut cursor: Option<String> = None;
    let mut complete = false;
    let mut note: Option<String> = None;
    let mut pages = 0usize;

    loop {
        if !budget.take() {
            note = Some("x: request budget reached — partial results".into());
            break;
        }
        let mut vars = base.clone();
        if let Some(c) = &cursor {
            if let Some(o) = vars.as_object_mut() {
                o.insert("cursor".into(), json!(c));
            }
        }
        let url = build_url(tpl, &vars);
        let (status, body) = fetch_api(page.cdp_ref(), &url, &tpl.headers).await?;
        if status == 429 {
            note = Some("x: rate limited (HTTP 429) — partial results, retry in a minute".into());
            break;
        }
        if status != 200 {
            if pages == 0 {
                // First page rejected: this op's replays are not viable on
                // this page state — let the caller pick a fallback.
                return Err(BladeError::Other(format!("x: replay rejected (HTTP {status})")));
            }
            note = Some(format!(
                "x: HTTP {status} after {pages} page(s) — partial results"
            ));
            break;
        }
        let j: Value = serde_json::from_str(&body)
            .map_err(|e| BladeError::Other(format!("x: non-JSON response ({e})")))?;
        let parsed = parse_timeline_page(&j);
        let mut new_items = 0usize;
        for t in parsed.0 {
            if seen.insert(t.id.clone(), ()).is_none() {
                tweets.push(t);
                new_items += 1;
            }
        }
        pages += 1;
        if parsed.2 || new_items == 0 {
            complete = true;
            break;
        }
        match parsed.1 {
            Some(c) if cursor.as_deref() != Some(c.as_str()) => cursor = Some(c),
            // No fresh cursor: there is nothing further to fetch.
            _ => {
                complete = true;
                break;
            }
        }
        if tweets.len() >= cap {
            break;
        }
    }

    let items: Vec<XItem> = tweets
        .into_iter()
        .take(cap)
        .map(|t| {
            let url = t.permalink();
            XItem {
                op: false,
                depth: None,
                reply_to: t.reply_to.clone(),
                id: t.id.clone(),
                author: t.author,
                name: t.name,
                date: t.date,
                text: t.text,
                replies: t.replies,
                reposts: t.reposts,
                likes: t.likes,
                media: t.media,
                url,
            }
        })
        .collect();
    if !complete && note.is_none() {
        note = Some(format!(
            "x: partial — capped at {} items, the timeline continues",
            items.len()
        ));
    }
    Ok(XPayload {
        container: "x-timeline".into(),
        kind: kind.to_string(),
        post: None,
        count: items.len(),
        total: None,
        complete,
        note,
        items,
    })
}

/// Thread depth: hops from the focal tweet along in_reply_to chains.
/// `None` when a chain leaves the harvested set (honest, never guessed).
pub(super) fn depth_of(
    id: &str,
    parents: &HashMap<String, String>,
    focal_id: &str,
    memo: &mut HashMap<String, Option<i64>>,
) -> Option<i64> {
    if id == focal_id {
        return Some(0);
    }
    if let Some(v) = memo.get(id) {
        return *v;
    }
    let mut hops = 0i64;
    let mut cur = id.to_string();
    for _ in 0..64 {
        match parents.get(&cur) {
            Some(p) if p == focal_id => {
                let d = hops + 1;
                memo.insert(id.to_string(), Some(d));
                return Some(d);
            }
            Some(p) => {
                cur = p.clone();
                hops += 1;
            }
            None => break,
        }
    }
    memo.insert(id.to_string(), None);
    None
}
