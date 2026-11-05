//! The fetch loops: TweetDetail thread walk and timeline walk. Both run the
//! same cursor [`walk`] — paginated, within the request/time budget — over a
//! CDP session, so the tests can drive it with canned response pages.

use std::collections::HashSet;

use super::gql::{build_url, fetch_api, Budget};
use super::parse::{cut_chars, no_timeline_reason, parse_thread_page, parse_timeline_page, T};
use super::*;

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
    thread_walk(page.cdp_ref(), tpl, &focal_id, cap, budget).await
}

/// The TweetDetail walk for one focal tweet, shaped into the thread payload.
pub(super) async fn thread_walk(
    cdp: &CdpSession,
    tpl: &GqlTemplate,
    focal_id: &str,
    cap: usize,
    budget: &mut Budget,
) -> Result<XPayload> {
    let mut base = tpl.variables.clone();
    {
        let obj = base
            .as_object_mut()
            .ok_or_else(|| BladeError::Other("x: template variables not an object".into()))?;
        obj.insert("focalTweetId".into(), json!(focal_id));
        obj.insert("count".into(), json!(PAGE_COUNT));
        obj.remove("cursor");
    }

    let w = walk(cdp, tpl, &base, Some(focal_id), cap, budget).await?;
    let (complete, note) = w.verdict(cap, "thread");
    let focal = w.focal.ok_or_else(|| {
        BladeError::Other("x: focal tweet missing from the TweetDetail response".into())
    })?;
    let total = focal.replies;
    let op_author = focal.author.clone();
    let parents: HashMap<String, String> = w
        .tweets
        .iter()
        .filter_map(|t| t.in_reply_to.clone().map(|p| (t.id.clone(), p)))
        .collect();
    let mut memo: HashMap<String, Option<i64>> = HashMap::new();
    let mut items: Vec<XItem> = Vec::new();
    for t in w.tweets.into_iter().take(cap) {
        let depth = depth_of(&t.id, &parents, focal_id, &mut memo);
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
        "profile" => &[
            "UserOriginalsTimeline",
            "UserTweets",
            "UserTweetsAndReplies",
        ],
        "search" => &["SearchTimeline"],
        "home" => &["HomeTimeline"],
        _ => unreachable!("kind checked by caller"),
    };
    let tpl = ops.iter().find_map(|o| templates.get(*o)).ok_or_else(|| {
        BladeError::Other(format!(
            "x: {} not captured (page API traffic not seen yet)",
            ops.join("/")
        ))
    })?;
    timeline_walk(page.cdp_ref(), tpl, kind, cap, budget).await
}

/// The timeline walk for one captured template, shaped into the payload.
pub(super) async fn timeline_walk(
    cdp: &CdpSession,
    tpl: &GqlTemplate,
    kind: &str,
    cap: usize,
    budget: &mut Budget,
) -> Result<XPayload> {
    let mut base = tpl.variables.clone();
    {
        let obj = base
            .as_object_mut()
            .ok_or_else(|| BladeError::Other("x: template variables not an object".into()))?;
        // Page size stays as the app chose it — op schemas validate their
        // variables strictly, and the cursor does the walking anyway.
        obj.remove("cursor");
    }

    let w = walk(cdp, tpl, &base, None, cap, budget).await?;
    let (complete, note) = w.verdict(cap, "timeline");
    let items: Vec<XItem> = w
        .tweets
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

/// What one cursor walk harvested, and how it ended.
struct Walk {
    focal: Option<T>,
    tweets: Vec<T>,
    /// The timeline itself ran out (the walk was not cut short).
    complete: bool,
    /// Why the walk stopped early, when it did.
    note: Option<String>,
    /// Thread only: reply sections X left collapsed behind a cursor. They
    /// are not replayed, so the replies behind them are missing from the
    /// harvest. `collapsed`: the rest of one reply's branch ("Show
    /// replies"), readable from that reply's own status page.
    collapsed: usize,
    /// `hidden`: replies to the post itself ("Show more replies", "Show
    /// probable spam"), under no harvested reply.
    hidden: usize,
}

impl Walk {
    /// The payload's `complete` flag and note; `what` names the container
    /// ("thread" / "timeline").
    fn verdict(&self, cap: usize, what: &str) -> (bool, Option<String>) {
        // The cap is checked only after a whole page lands, so the walk can
        // harvest past it — and the payload keeps just the first `cap`. A
        // timeline that ended still is not complete once that cut drops
        // real items.
        let cut = self.tweets.len() > cap;
        let mut notes: Vec<String> = self.note.iter().cloned().collect();
        // No end and no stop reason: the loop broke on the cap.
        if cut || (!self.complete && self.note.is_none()) {
            notes.push(format!(
                "x: partial — capped at {} items, the {what} continues",
                self.tweets.len().min(cap)
            ));
        }
        if self.collapsed > 0 {
            notes.push(format!(
                "x: {} collapsed reply section(s) ('Show replies') not fetched — the replies behind them are missing (open a reply's url to read its branch)",
                self.collapsed
            ));
        }
        if self.hidden > 0 {
            notes.push(format!(
                "x: {} hidden reply section(s) ('Show more replies' / 'Show probable spam') not fetched — the replies behind them are missing",
                self.hidden
            ));
        }
        let complete = self.complete && !cut && self.collapsed == 0 && self.hidden == 0;
        (complete, (!notes.is_empty()).then(|| notes.join("; ")))
    }
}

/// Walk one graphql op's cursor pages until the timeline ends, the cap is
/// reached, or the budget runs out. `focal_id` is set for TweetDetail: the
/// focal tweet is lifted out of the reply list.
async fn walk(
    cdp: &CdpSession,
    tpl: &GqlTemplate,
    base: &Value,
    focal_id: Option<&str>,
    cap: usize,
    budget: &mut Budget,
) -> Result<Walk> {
    let mut w = Walk {
        focal: None,
        tweets: Vec::new(),
        complete: false,
        note: None,
        collapsed: 0,
        hidden: 0,
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;

    loop {
        if !budget.take() {
            w.note = Some("x: request budget reached — partial results".into());
            break;
        }
        let mut vars = base.clone();
        if let Some(c) = &cursor {
            if let Some(o) = vars.as_object_mut() {
                o.insert("cursor".into(), json!(c));
            }
        }
        let url = build_url(tpl, &vars);
        // Past the first page a failed read (the page navigated away, a
        // proxy error page) must not throw away the pages already
        // harvested: return them as partial and say why. A first-page
        // failure still errors so the caller can pick a fallback.
        let (status, body) = match fetch_api(cdp, &url, &tpl.headers).await {
            Ok(reply) => reply,
            Err(e) if pages > 0 => {
                w.note = Some(format!(
                    "x: page {} fetch failed ({e}) — partial results",
                    pages + 1
                ));
                break;
            }
            Err(e) => return Err(e),
        };
        if status == 429 {
            w.note = Some("x: rate limited (HTTP 429) — partial results, retry in a minute".into());
            break;
        }
        if status != 200 {
            if pages == 0 {
                // First page rejected: this op's replays are not viable on
                // this page state — let the caller pick a fallback.
                return Err(BladeError::Other(format!(
                    "x: replay rejected (HTTP {status})"
                )));
            }
            w.note = Some(format!(
                "x: HTTP {status} after {pages} page(s) — partial results"
            ));
            break;
        }
        let j: Value = match serde_json::from_str(&body) {
            Ok(j) => j,
            Err(e) if pages > 0 => {
                w.note = Some(format!(
                    "x: non-JSON response after {pages} page(s) ({e}) — partial results"
                ));
                break;
            }
            Err(e) => return Err(BladeError::Other(format!("x: non-JSON response ({e})"))),
        };
        let parsed = match focal_id {
            Some(f) => parse_thread_page(&j, f),
            None => parse_timeline_page(&j),
        };
        if !parsed.timeline {
            // A 200 without timeline instructions (X's graphql errors body,
            // an unknown shape) parses to zero entries, which must not read
            // as the cursors-only page that ends a timeline.
            let why = no_timeline_reason(&j);
            if pages == 0 {
                // Nothing harvested yet: an error lets the caller fall back,
                // where an empty payload would claim an empty timeline.
                return Err(BladeError::Other(format!(
                    "x: response carried no timeline ({why})"
                )));
            }
            w.note = Some(format!(
                "x: page {} carried no timeline ({why}) — partial results",
                pages + 1
            ));
            break;
        }
        let mut new_items = 0usize;
        if w.focal.is_none() && parsed.focal.is_some() {
            w.focal = parsed.focal;
            // The focal post is new content too: a status with no replies
            // has nothing else on its only page.
            new_items += 1;
        }
        for t in parsed.tweets {
            if seen.insert(t.id.clone()) {
                w.tweets.push(t);
                new_items += 1;
            }
        }
        // Collapsed sections matter to the conversation only: a timeline's
        // own walk is whole without the threads it previews.
        if focal_id.is_some() {
            w.collapsed += parsed.collapsed;
            w.hidden += parsed.hidden;
        }
        pages += 1;
        // `complete` is claimed only on X's own end signals: a Bottom
        // terminate, or a page carrying nothing but cursors (how search and
        // profile timelines run out).
        if parsed.terminated || parsed.entries == 0 {
            w.complete = true;
            break;
        }
        if new_items == 0 {
            // Content (ads, modules, repeats) but not one new post: the
            // walk stops, and must not read as the end of the timeline.
            w.note = Some(format!(
                "x: page {pages} brought no new posts (ads/repeats only) — stopped; partial results"
            ));
            break;
        }
        match parsed.bottom_cursor {
            Some(c) if cursor.as_deref() != Some(c.as_str()) => cursor = Some(c),
            Some(_) => {
                w.note = Some(format!(
                    "x: the cursor stopped advancing after {pages} page(s) — partial results"
                ));
                break;
            }
            // A conversation's last TweetDetail page simply carries no
            // Bottom cursor.
            None if focal_id.is_some() => {
                w.complete = true;
                break;
            }
            // Timelines carry one on every page (it is how X pages them):
            // posts without one mean the page shape was not understood.
            None => {
                w.note = Some(format!(
                    "x: no continuation cursor on page {pages} — partial results"
                ));
                break;
            }
        }
        if w.tweets.len() >= cap {
            break;
        }
    }
    Ok(w)
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
