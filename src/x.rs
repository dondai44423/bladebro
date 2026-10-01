//! X.com (Twitter) adapter — the `extract auto` fast path on x.com pages.
//!
//! Pure optimization: no new tools, no new params. X renders a virtualized
//! SPA — only the cells near the viewport exist in the DOM, so DOM scraping
//! is both incomplete and slow (scroll, re-read, repeat). Instead the
//! adapter reads the page's OWN API traffic — the graphql calls the app
//! already makes carry the query id, feature flags and auth headers — and
//! replays them from page context (same origin, same cookies, same
//! headers). One tool call then returns the full structured result
//! regardless of what the DOM happens to have mounted.
//!
//! - status pages → the whole conversation (TweetDetail): the focal tweet
//!   plus every reply and nested reply the ranking returns, cursor-paginated
//!   while the timeline is not terminated, thread-ordered with depth.
//! - profile / search / home → the timeline (UserTweets / SearchTimeline /
//!   HomeTimeline), cursor-paginated to the requested cap.
//!
//! Query ids rotate with every app deploy; they are never hard-coded —
//! templates are captured from live traffic (`Page::xhr_log`), so the
//! adapter self-heals across deploys.

//!
//! Module map: this file is the public surface (payload types + [`extract`])
//! and the tests; `gql` is the replay machinery, `parse` the response parsing,
//! `fetch` the thread/timeline fetch loops, `dom` the rendered-page fallback.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::page::Page;

mod dom;
mod fetch;
mod gql;
mod parse;

pub use self::gql::{build_url, capture_templates, fetch_api, GqlTemplate};

use self::dom::{dom_collect, is_flaky_render};
use self::fetch::{fetch_thread, fetch_timeline};
use self::gql::Budget;

/// In-page fetch budget for one API call.
const PAGE_FETCH_TIMEOUT_MS: u64 = 20_000;
/// Graphql requests allowed per extract (cursor pages are serial).
const MAX_REQUESTS: u32 = 30;
/// Wall-clock budget for one extract.
const TOTAL_BUDGET: Duration = Duration::from_secs(25);
/// Items requested per graphql page.
const PAGE_COUNT: usize = 40;
/// Default item cap when the caller did not pass an explicit limit.
pub const DEFAULT_ITEM_CAP: usize = 100;
/// Hard cap for explicit limits.
pub const MAX_ITEM_CAP: usize = 500;
/// Per-comment text cap (long-form posts can reach 25k — agents don't need that).
const ITEM_TEXT_CAP: usize = 4000;
/// Focal post text cap.
const POST_TEXT_CAP: usize = 8000;

/// The public web-client bearer embedded in every X bundle. Not a secret:
/// it identifies the web app, not a user. If it ever rotates, replays fail
/// with 401 and the adapter falls back to the DOM path with a note.
const WEB_BEARER: &str =
    "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

#[derive(Serialize)]
pub struct XPost {
    pub id: String,
    pub author: String,
    pub name: String,
    pub text: String,
    pub date: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replies: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reposts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
}

#[derive(Serialize)]
pub struct XItem {
    pub id: String,
    pub author: String,
    pub name: String,
    pub date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<i64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub op: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replies: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reposts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
    pub url: String,
}

#[derive(Serialize)]
pub struct XPayload {
    pub container: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<XPost>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<XItem>,
}

/// Extract structured data for an x.com page. Dispatches on page kind:
/// status → the conversation thread; profile/search/home → the timeline.
pub async fn extract(page: &Page, kind: &str, cap: usize) -> Result<XPayload> {
    // Transient React boot failures ("Something went wrong. Try reloading.")
    // are common on missed first paints; one bounded reload clears them.
    if is_flaky_render(page).await {
        tracing::debug!("x: error-state render detected — reloading once");
        let _ = page.cdp_ref().send("Page.reload", Some(json!({}))).await;
        let _ = crate::page::wait_for_load(page.cdp_ref(), Duration::from_secs(10)).await;
        let _ = crate::page::wait_for_settle_with_network(
            page.cdp_ref(),
            Duration::from_millis(2500),
            Some(page.in_flight_ref()),
        )
        .await;
    }
    let templates = capture_templates(page);
    tracing::debug!(
        "x: {} templates for {kind}: {:?}",
        templates.len(),
        templates
            .values()
            .map(|t| (t.op.clone(), t.headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    );
    let mut budget = Budget::new();
    let payload = match kind {
        "status" => fetch_thread(page, &templates, cap, &mut budget).await?,
        "profile" | "search" | "home" => {
            match fetch_timeline(page, kind, &templates, cap, &mut budget).await {
                Ok(p) => p,
                Err(e) => {
                    // Rapid replay rejected (transaction-gated op) or the op
                    // was never captured — collect the rendered page instead.
                    tracing::debug!("x: {kind} replay failed ({e}); DOM fallback");
                    dom_collect(page, kind, cap).await?
                }
            }
        }
        other => {
            return Err(BladeError::Other(format!(
                "x: unsupported page kind {other}"
            )))
        }
    };
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::dom::dom_items_from_json;
    use super::fetch::depth_of;
    use super::gql::parse_gql_url;
    use super::parse::{collect_page, format_x_date, parse_thread_page, T};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/x_tweet_detail.json"))
            .expect("fixture parses")
    }

    const FOCAL: &str = "2103157082937844131";

    #[test]
    fn fixture_parses_the_complete_page() {
        let j = fixture();
        let page = parse_thread_page(&j, FOCAL);
        assert!(
            !page.terminated,
            "only a Top terminate is present — the bottom is not marked exhausted"
        );
        assert!(page.bottom_cursor.is_some(), "cursor present");
        let focal = page.focal.expect("focal found");
        assert_eq!(focal.id, FOCAL);
        assert_eq!(focal.author, "swarajb");
        assert_eq!(focal.replies, Some(23));
        assert_eq!(focal.views, Some(12100));
        assert!(!focal.media.is_empty(), "focal has media");
        assert_eq!(page.tweets.len(), 30, "30 replies (the promoted tweet is not one)");
        assert!(
            page.tweets.iter().all(|t| t.id != "2096286950328304004"),
            "promoted tweets are filtered out of the conversation"
        );
    }

    #[test]
    fn bottom_terminate_marks_the_page_complete() {
        let j = json!({
            "data": {"threaded_conversation_with_injections_v2": {"instructions": [
                {"type": "TimelineTerminateTimeline", "direction": "Bottom"}
            ]}}
        });
        assert!(collect_page(&j).terminated, "Bottom terminate is honoured");
        let j2 = json!({
            "data": {"threaded_conversation_with_injections_v2": {"instructions": [
                {"type": "TimelineTerminateTimeline", "direction": "Top"}
            ]}}
        });
        assert!(!collect_page(&j2).terminated, "Top terminate is not bottom exhaustion");
    }

    #[test]
    fn depths_resolve_through_the_chain() {
        let j = fixture();
        let page = parse_thread_page(&j, FOCAL);
        let mut all: Vec<&T> = page.tweets.iter().collect();
        let focal = page.focal.as_ref().expect("focal");
        all.push(focal);
        let parents: HashMap<String, String> = all
            .iter()
            .filter_map(|t| t.in_reply_to.clone().map(|p| (t.id.clone(), p)))
            .collect();
        let mut memo = HashMap::new();
        for t in &page.tweets {
            let d = depth_of(&t.id, &parents, FOCAL, &mut memo)
                .unwrap_or_else(|| panic!("depth resolves for {}", t.id));
            assert!(d >= 1, "reply depth >= 1");
            assert!(d <= 6, "depth sane");
        }
        // The nested reply inside the first thread module sits two hops down.
        let nested = all
            .iter()
            .find(|t| t.id == "2103202870967583069")
            .expect("nested reply present");
        assert_eq!(nested.in_reply_to.as_deref(), Some("2103157086070898918"));
        assert_eq!(
            depth_of(&nested.id, &parents, FOCAL, &mut memo),
            Some(2),
            "nested reply depth 2"
        );
    }

    #[test]
    fn op_flag_marks_the_author_thread() {
        let j = fixture();
        let page = parse_thread_page(&j, FOCAL);
        let op_author = page.focal.as_ref().expect("focal").author.clone();
        // The author continues his own thread — those replies carry op:true.
        let own = page.tweets.iter().filter(|t| t.author == op_author).count();
        assert!(own >= 5, "author continuation replies present");
    }

    #[test]
    fn long_text_uses_note_tweet_and_caps() {
        let j = fixture();
        let page = parse_thread_page(&j, FOCAL);
        let long = page
            .tweets
            .iter()
            .chain(page.focal.iter())
            .max_by_key(|t| t.text.chars().count())
            .expect("some tweet");
        assert!(
            long.text.chars().count() > 280,
            "note_tweet text survives (len {})",
            long.text.chars().count()
        );
        assert!(long.text.chars().count() <= ITEM_TEXT_CAP + 1);
    }

    #[test]
    fn dom_rows_shape_into_items() {
        let j = json!({
            "exhausted": true,
            "items": [
                {"id": "1", "author": "a", "name": "A", "date": "2026-09-24T16:18:18.000Z",
                 "text": "hello", "replies": "1,234 Replies. Reply", "reposts": "29 reposts. Repost",
                 "likes": "176 Likes. Like", "url": "https://x.com/a/status/1"},
                {"id": "", "text": "skip me"},
                {"id": "2", "date": "no-date", "replies": ""}
            ]
        });
        let (items, exhausted) = dom_items_from_json(&j);
        assert!(exhausted, "exhausted flag carried");
        assert_eq!(items.len(), 2, "empty ids dropped");
        assert_eq!(items[0].replies, Some(1234), "comma counts parse");
        assert_eq!(items[0].reposts, Some(29));
        assert_eq!(items[0].likes, Some(176));
        assert_eq!(items[0].date, "2026-09-24 16:18 UTC");
        assert_eq!(items[1].date, "no-date", "non-ISO dates pass through");
        assert_eq!(items[1].replies, None, "empty count → None");
    }

    #[test]
    fn date_formatting() {
        assert_eq!(
            format_x_date("Thu Sep 24 16:18:18 +0000 2026"),
            "2026-09-24 16:18 UTC"
        );
        assert_eq!(
            format_x_date("Mon Jan 1 00:00:05 +0000 2024"),
            "2024-01-01 00:00 UTC"
        );
        assert_eq!(format_x_date("garbage"), "garbage");
    }

    #[test]
    fn gql_url_parsing_and_rebuild() {
        let url = "https://x.com/i/api/graphql/zoF7_t363wZyzylk-BLfZQ/TweetDetail?variables=%7B%22focalTweetId%22%3A%22123%22%7D&features=%7B%22a%22%3Atrue%7D";
        let (qid, op, query) = parse_gql_url(url).expect("parses");
        assert_eq!(qid, "zoF7_t363wZyzylk-BLfZQ");
        assert_eq!(op, "TweetDetail");
        assert!(query.contains("variables="));
        let params: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .filter(|(k, _)| k != "variables")
            .collect();
        let tpl = GqlTemplate {
            qid,
            op,
            url: url.to_string(),
            params,
            variables: json!({"focalTweetId": "123"}),
            headers: Vec::new(),
        };
        let rebuilt = build_url(&tpl, &json!({"focalTweetId": "999", "count": 40}));
        assert!(rebuilt.starts_with("https://x.com/i/api/graphql/zoF7_t363wZyzylk-BLfZQ/TweetDetail?"));
        assert!(rebuilt.contains("features=%7B%22a%22%3Atrue%7D"), "features preserved");
        let vars: Value = {
            let q = rebuilt.split('?').nth(1).expect("query");
            let pairs: HashMap<String, String> = url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            serde_json::from_str(pairs.get("variables").expect("variables present"))
                .expect("variables json")
        };
        assert_eq!(vars["focalTweetId"], "999");
        assert_eq!(vars["count"], 40);
    }

    #[test]
    fn cap_produces_partial_note_and_depth_kept() {
        // Build a payload like fetch_thread does, but capped small.
        let j = fixture();
        let page = parse_thread_page(&j, FOCAL);
        let focal = page.focal.expect("focal");
        let parents: HashMap<String, String> = page
            .tweets
            .iter()
            .filter_map(|t| t.in_reply_to.clone().map(|p| (t.id.clone(), p)))
            .collect();
        let mut memo = HashMap::new();
        let items: Vec<XItem> = page
            .tweets
            .iter()
            .take(10)
            .map(|t| XItem {
                op: t.author == focal.author,
                depth: depth_of(&t.id, &parents, FOCAL, &mut memo),
                reply_to: t.reply_to.clone(),
                id: t.id.clone(),
                author: t.author.clone(),
                name: t.name.clone(),
                date: t.date.clone(),
                text: t.text.clone(),
                replies: t.replies,
                reposts: t.reposts,
                likes: t.likes,
                media: t.media.clone(),
                url: t.permalink(),
            })
            .collect();
        assert_eq!(items.len(), 10);
        let payload = XPayload {
            container: "x-thread".into(),
            kind: "status".into(),
            post: None,
            count: items.len(),
            total: focal.replies,
            complete: false,
            note: Some("capped".into()),
            items,
        };
        let s = serde_json::to_string(&payload).expect("serializes");
        assert!(s.contains("\"complete\":false"));
        assert!(s.contains("\"depth\":"));
        assert!(!s.contains("\"op\":false"), "false op omitted");
    }
}
