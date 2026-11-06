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
//!   Sections X collapses behind a cursor ("Show replies", "Show more
//!   replies") are not expanded; they keep `complete` false, with a note.
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
            .map(|t| (
                t.op.clone(),
                t.headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
            ))
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

    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;

    use super::dom::dom_items_from_json;
    use super::fetch::{depth_of, thread_walk, timeline_walk};
    use super::gql::parse_gql_url;
    use super::parse::{collect_page, format_x_date, parse_thread_page, parse_timeline_page, T};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/x_tweet_detail.json"))
            .expect("fixture parses")
    }

    const FOCAL: &str = "2103157082937844131";

    // ── Cursor walks against canned pages ─────────────────────────────────
    // A fake CDP endpoint answers each in-page fetch (`Runtime.evaluate`)
    // with the next canned reply, so the real walk loop runs end to end.

    /// One canned answer: `Ok((status, body))`, or a CDP error (the page
    /// went away mid-walk).
    type Reply = std::result::Result<(i64, String), &'static str>;

    /// Serve `replies` in order; the counter says how many were requested.
    async fn fake_cdp(replies: Vec<Reply>) -> (CdpSession, Arc<AtomicUsize>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for reply in replies {
                let Some(Ok(request)) = socket.next().await else {
                    return;
                };
                let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                let answer = match reply {
                    Ok((s, t)) => {
                        json!({"id": request["id"], "result": {"result": {"value": {"s": s, "t": t}}}})
                    }
                    Err(message) => {
                        json!({"id": request["id"], "error": {"code": -32000, "message": message}})
                    }
                };
                counter.fetch_add(1, SeqCst);
                socket
                    .send(Message::Text(answer.to_string().into()))
                    .await
                    .unwrap();
            }
            // Out of replies: hang up, so an extra request fails loudly.
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        (CdpSession::root(client), served)
    }

    fn template(op: &str) -> GqlTemplate {
        GqlTemplate {
            qid: "qid".into(),
            op: op.into(),
            url: String::new(),
            params: Vec::new(),
            variables: json!({}),
            headers: Vec::new(),
        }
    }

    fn tweet(id: &str, author: &str, reply_to: Option<&str>) -> Value {
        json!({"__typename": "Tweet", "rest_id": id,
            "core": {"user_results": {"result": {"core": {"screen_name": author, "name": author}}}},
            "legacy": {"full_text": format!("tweet {id}"),
                "created_at": "Thu Sep 24 16:18:18 +0000 2026",
                "in_reply_to_status_id_str": reply_to,
                "reply_count": 0, "retweet_count": 0, "favorite_count": 0}})
    }

    fn tweet_entry(t: Value) -> Value {
        json!({"entryId": format!("tweet-{}", t["rest_id"].as_str().unwrap_or_default()),
            "content": {"entryType": "TimelineTimelineItem",
                "itemContent": {"itemType": "TimelineTweet", "tweet_results": {"result": t}}}})
    }

    fn cursor_entry(kind: &str, value: &str) -> Value {
        json!({"entryId": format!("cursor-{}-{value}", kind.to_ascii_lowercase()),
            "content": {"entryType": "TimelineTimelineCursor", "cursorType": kind, "value": value}})
    }

    fn add_entries(entries: Vec<Value>) -> Value {
        json!({"type": "TimelineAddEntries", "entries": entries})
    }

    fn terminate_bottom() -> Value {
        json!({"type": "TimelineTerminateTimeline", "direction": "Bottom"})
    }

    /// `n` tweet entries with ids `<prefix>000…`, replying to `reply_to`.
    fn tweets(prefix: &str, n: usize, reply_to: Option<&str>) -> Vec<Value> {
        (0..n)
            .map(|i| tweet_entry(tweet(&format!("{prefix}{i:03}"), "someone", reply_to)))
            .collect()
    }

    fn thread_body(instructions: Vec<Value>) -> String {
        json!({"data": {"threaded_conversation_with_injections_v2": {"instructions": instructions}}})
            .to_string()
    }

    fn search_body(instructions: Vec<Value>) -> String {
        json!({"data": {"search_by_raw_query": {"search_timeline": {"timeline": {
            "instructions": instructions}}}}})
        .to_string()
    }

    #[tokio::test]
    async fn a_terminated_walk_cut_at_the_cap_is_not_complete() {
        // 25 replies and a Bottom terminate on the first page, limit 10: the
        // timeline ended, but the cap cut drops 15 real replies.
        let mut entries = vec![tweet_entry(tweet(FOCAL, "op", None))];
        entries.extend(tweets("7", 25, Some(FOCAL)));
        let body = thread_body(vec![add_entries(entries), terminate_bottom()]);
        let (cdp, served) = fake_cdp(vec![Ok((200, body))]).await;
        let p = thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            10,
            &mut Budget::new(),
        )
        .await
        .expect("thread walk");
        assert_eq!(served.load(SeqCst), 1);
        assert_eq!(p.count, 10);
        assert!(!p.complete, "15 replies were cut — not the whole thread");
        assert!(
            p.note
                .as_deref()
                .is_some_and(|n| n.contains("capped at 10")),
            "the cut is named: {:?}",
            p.note
        );

        // Same for a timeline whose only page also ends it.
        let mut page = tweets("8", 30, None);
        page.push(cursor_entry("Bottom", "c1"));
        let body = search_body(vec![add_entries(page), terminate_bottom()]);
        let (cdp, _) = fake_cdp(vec![Ok((200, body))]).await;
        let p = timeline_walk(
            &cdp,
            &template("SearchTimeline"),
            "search",
            10,
            &mut Budget::new(),
        )
        .await
        .expect("timeline walk");
        assert_eq!(p.count, 10);
        assert!(!p.complete, "20 posts were cut — not the whole timeline");
        assert!(
            p.note
                .as_deref()
                .is_some_and(|n| n.contains("capped at 10")),
            "{:?}",
            p.note
        );
    }

    #[tokio::test]
    async fn a_failed_later_page_keeps_the_pages_already_harvested() {
        let mut first = vec![tweet_entry(tweet(FOCAL, "op", None))];
        first.extend(tweets("7", 5, Some(FOCAL)));
        first.push(cursor_entry("Bottom", "c1"));
        let first = thread_body(vec![add_entries(first)]);
        let failures: [(Reply, &str); 2] = [
            (
                Err("Execution context was destroyed."),
                "page 2 fetch failed",
            ),
            (Ok((200, "<html>upstream error</html>".into())), "non-JSON"),
        ];
        for (failure, named) in failures {
            let (cdp, served) = fake_cdp(vec![Ok((200, first.clone())), failure]).await;
            let p = thread_walk(
                &cdp,
                &template("TweetDetail"),
                FOCAL,
                100,
                &mut Budget::new(),
            )
            .await
            .expect("page 1 landed — the walk must not throw it away");
            assert_eq!(served.load(SeqCst), 2);
            assert_eq!(p.count, 5, "page 1's replies survive");
            assert!(!p.complete);
            assert!(
                p.note.as_deref().is_some_and(|n| n.contains(named)),
                "the failure is named ({named}): {:?}",
                p.note
            );
        }

        // Timelines too.
        let mut first = tweets("8", 4, None);
        first.push(cursor_entry("Bottom", "c1"));
        let first = search_body(vec![add_entries(first)]);
        let (cdp, _) = fake_cdp(vec![Ok((200, first)), Err("Target closed.")]).await;
        let p = timeline_walk(
            &cdp,
            &template("SearchTimeline"),
            "search",
            100,
            &mut Budget::new(),
        )
        .await
        .expect("timeline page 1 survives");
        assert_eq!(p.count, 4);
        assert!(!p.complete);

        // A first-page failure still errors, so the caller can fall back.
        let (cdp, _) = fake_cdp(vec![Ok((200, "not json".into()))]).await;
        assert!(thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            100,
            &mut Budget::new()
        )
        .await
        .is_err());
    }

    fn replace_cursor(kind: &str, value: &str) -> Value {
        let entry = cursor_entry(kind, value);
        json!({"type": "TimelineReplaceEntry", "entry_id_to_replace": entry["entryId"],
            "entry": entry})
    }

    #[tokio::test]
    async fn replaced_cursor_entries_keep_the_timeline_walking() {
        // SearchTimeline's later pages swap their cursors in with
        // TimelineReplaceEntry; the walk must follow them to the empty page
        // that actually ends the timeline.
        let mut first = tweets("1", 10, None);
        first.push(cursor_entry("Top", "t1"));
        first.push(cursor_entry("Bottom", "c1"));
        let later = |prefix: &str, next: &str| {
            search_body(vec![
                add_entries(tweets(prefix, 10, None)),
                replace_cursor("Top", "t"),
                replace_cursor("Bottom", next),
            ])
        };
        let end = search_body(vec![
            add_entries(Vec::new()),
            replace_cursor("Top", "t"),
            replace_cursor("Bottom", "c4"),
        ]);
        let (cdp, served) = fake_cdp(vec![
            Ok((200, search_body(vec![add_entries(first)]))),
            Ok((200, later("2", "c2"))),
            Ok((200, later("3", "c3"))),
            Ok((200, end)),
        ])
        .await;
        let p = timeline_walk(
            &cdp,
            &template("SearchTimeline"),
            "search",
            100,
            &mut Budget::new(),
        )
        .await
        .expect("timeline walk");
        assert_eq!(served.load(SeqCst), 4, "every page is fetched");
        assert_eq!(p.count, 30);
        assert!(p.complete, "the empty page ends the timeline: {:?}", p.note);
        assert!(p.note.is_none());
    }

    #[tokio::test]
    async fn a_walk_that_stops_without_an_end_signal_is_not_complete() {
        let first = || {
            let mut e = tweets("1", 5, None);
            e.push(cursor_entry("Bottom", "c1"));
            search_body(vec![add_entries(e)])
        };
        // An ads-only page: it carries content, just no new post.
        let promoted = json!({"entryId": "promoted-tweet-9000",
            "content": {"entryType": "TimelineTimelineItem", "itemContent": {
                "itemType": "TimelineTweet",
                "tweet_results": {"result": tweet("9000", "brand", None)},
                "promotedMetadata": {"impressionId": "x"}}}});
        let ads = search_body(vec![add_entries(vec![
            promoted,
            cursor_entry("Bottom", "c2"),
        ])]);
        // X hands back the cursor it was just sent.
        let mut stuck = tweets("2", 5, None);
        stuck.push(cursor_entry("Bottom", "c1"));
        let stuck = search_body(vec![add_entries(stuck)]);
        // Posts, but no continuation cursor anywhere on the page.
        let lost = search_body(vec![add_entries(tweets("3", 5, None))]);
        for (second, named) in [
            (ads, "no new posts"),
            (stuck, "cursor stopped advancing"),
            (lost, "no continuation cursor"),
        ] {
            let (cdp, served) = fake_cdp(vec![Ok((200, first())), Ok((200, second))]).await;
            let p = timeline_walk(
                &cdp,
                &template("SearchTimeline"),
                "search",
                100,
                &mut Budget::new(),
            )
            .await
            .expect("timeline walk");
            assert_eq!(served.load(SeqCst), 2);
            assert!(!p.complete, "{named}: X never said the timeline ended");
            assert!(
                p.note.as_deref().is_some_and(|n| n.contains(named)),
                "{named}: {:?}",
                p.note
            );
        }

        // A conversation's last page simply carries no Bottom cursor: that
        // one is the real end.
        let mut last = vec![tweet_entry(tweet(FOCAL, "op", None))];
        last.extend(tweets("4", 3, Some(FOCAL)));
        let (cdp, _) = fake_cdp(vec![Ok((200, thread_body(vec![add_entries(last)])))]).await;
        let p = thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            100,
            &mut Budget::new(),
        )
        .await
        .expect("thread walk");
        assert_eq!(p.count, 3);
        assert!(p.complete, "{:?}", p.note);

        // A status nobody replied to: its only page is the focal post.
        let only = thread_body(vec![add_entries(vec![tweet_entry(tweet(
            FOCAL, "op", None,
        ))])]);
        let (cdp, _) = fake_cdp(vec![Ok((200, only))]).await;
        let p = thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            100,
            &mut Budget::new(),
        )
        .await
        .expect("thread walk");
        assert_eq!(p.count, 0);
        assert!(p.complete, "no replies is the whole thread: {:?}", p.note);
    }

    #[tokio::test]
    async fn a_page_without_timeline_instructions_is_not_the_end() {
        // X answers some replays HTTP 200 with a graphql errors body and no
        // timeline at all: that says nothing about where the timeline ends.
        let errors = || {
            json!({"errors": [{"message": "Timeout: Unspecified", "code": 29}], "data": {}})
                .to_string()
        };
        let mut first = vec![tweet_entry(tweet(FOCAL, "op", None))];
        first.extend(tweets("7", 5, Some(FOCAL)));
        first.push(cursor_entry("Bottom", "c1"));
        let first = thread_body(vec![add_entries(first)]);
        for (second, named) in [
            (errors(), "(graphql errors: Timeout: Unspecified)"),
            // A shape the walker does not know at all.
            (json!({"data": {}}).to_string(), "(unrecognized response)"),
        ] {
            let (cdp, served) = fake_cdp(vec![Ok((200, first.clone())), Ok((200, second))]).await;
            let p = thread_walk(
                &cdp,
                &template("TweetDetail"),
                FOCAL,
                100,
                &mut Budget::new(),
            )
            .await
            .expect("page 1 landed");
            assert_eq!(served.load(SeqCst), 2);
            assert_eq!(p.count, 5, "page 1's replies survive");
            assert!(!p.complete, "X never said the thread ended: {:?}", p.note);
            assert!(
                p.note
                    .as_deref()
                    .is_some_and(|n| n.contains("page 2 carried no timeline") && n.contains(named)),
                "{named}: {:?}",
                p.note
            );
        }

        // Timelines too.
        let mut first = tweets("8", 5, None);
        first.push(cursor_entry("Bottom", "c1"));
        let (cdp, served) = fake_cdp(vec![
            Ok((200, search_body(vec![add_entries(first)]))),
            Ok((200, errors())),
        ])
        .await;
        let p = timeline_walk(
            &cdp,
            &template("SearchTimeline"),
            "search",
            100,
            &mut Budget::new(),
        )
        .await
        .expect("timeline page 1 survives");
        assert_eq!(served.load(SeqCst), 2);
        assert_eq!(p.count, 5);
        assert!(!p.complete, "X never said the timeline ended: {:?}", p.note);
        assert!(
            p.note
                .as_deref()
                .is_some_and(|n| n
                    .contains("page 2 carried no timeline (graphql errors: Timeout: Unspecified)")),
            "{:?}",
            p.note
        );

        // On the first page it stays an error, so the caller falls back to
        // the rendered page instead of returning an empty, "complete" result.
        let (cdp, _) = fake_cdp(vec![Ok((200, errors()))]).await;
        let e = thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            100,
            &mut Budget::new(),
        )
        .await
        .err()
        .expect("thread: first-page errors body is an error")
        .to_string();
        assert!(
            e.contains("Timeout: Unspecified"),
            "the cause is named: {e}"
        );
        let (cdp, _) = fake_cdp(vec![Ok((200, errors()))]).await;
        let e = timeline_walk(
            &cdp,
            &template("SearchTimeline"),
            "search",
            100,
            &mut Budget::new(),
        )
        .await
        .err()
        .expect("timeline: first-page errors body is an error")
        .to_string();
        assert!(
            e.contains("Timeout: Unspecified"),
            "the cause is named: {e}"
        );
    }

    #[tokio::test]
    async fn collapsed_reply_branches_keep_a_thread_incomplete() {
        // The captured page holds three conversation modules that end in a
        // "Show replies" (ShowMore) cursor; the replies behind them are not
        // fetched, so a terminated walk is still not the whole thread.
        let (cdp, served) = fake_cdp(vec![
            Ok((200, fixture().to_string())),
            Ok((200, thread_body(vec![terminate_bottom()]))),
        ])
        .await;
        let p = thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            100,
            &mut Budget::new(),
        )
        .await
        .expect("thread walk");
        assert_eq!(served.load(SeqCst), 2);
        assert_eq!(p.count, 30);
        assert!(!p.complete, "3 reply branches were never expanded");
        let note = p.note.unwrap_or_default();
        assert!(note.contains("3 collapsed reply"), "{note}");
        assert!(
            !note.contains("capped"),
            "nothing was cut at the cap: {note}"
        );
    }

    #[tokio::test]
    async fn hidden_top_level_replies_are_named_apart_from_collapsed_branches() {
        // A "Show replies" cursor closes one reply's branch (a module item);
        // "Show more replies" / "Show probable spam" hide replies to the post
        // itself (top-level cursors, item-wrapped or bare). Only the first
        // kind is readable from a reply's url.
        let cursor_item = |kind: &str, value: &str| json!({"itemType": "TimelineTimelineCursor", "cursorType": kind, "value": value});
        let branch = json!({"entryId": "conversationthread-7000", "content": {
        "entryType": "TimelineTimelineModule", "items": [
            {"entryId": "conversationthread-7000-tweet-7000", "item": {"itemContent": {
                "itemType": "TimelineTweet",
                "tweet_results": {"result": tweet("7000", "someone", Some(FOCAL))}}}},
            {"entryId": "conversationthread-7000-cursor-showmore-sm1",
                "item": {"itemContent": cursor_item("ShowMore", "sm1")}},
        ]}});
        let more = json!({"entryId": "cursor-showmorethreads-smt1", "content": {
            "entryType": "TimelineTimelineItem",
            "itemContent": cursor_item("ShowMoreThreads", "smt1")}});
        let spam = cursor_entry("ShowMoreThreadsPrompt", "smtp1");
        let body = thread_body(vec![
            add_entries(vec![
                tweet_entry(tweet(FOCAL, "op", None)),
                branch,
                more,
                spam,
            ]),
            terminate_bottom(),
        ]);
        let (cdp, _) = fake_cdp(vec![Ok((200, body))]).await;
        let p = thread_walk(
            &cdp,
            &template("TweetDetail"),
            FOCAL,
            100,
            &mut Budget::new(),
        )
        .await
        .expect("thread walk");
        assert_eq!(p.count, 1);
        assert!(!p.complete);
        let note = p.note.unwrap_or_default();
        let parts: Vec<&str> = note.split("; ").collect();
        assert_eq!(parts.len(), 2, "{note}");
        assert!(
            parts[0].starts_with("x: 1 collapsed reply section(s) ('Show replies')")
                && parts[0].contains("open a reply's url"),
            "{note}"
        );
        assert!(
            parts[1].starts_with(
                "x: 2 hidden reply section(s) ('Show more replies' / 'Show probable spam')"
            ) && !parts[1].contains("reply's url"),
            "no reply's url leads to replies hidden under the post: {note}"
        );
    }

    #[test]
    fn pinned_posts_and_whole_retweet_text_are_kept() {
        // Profile timelines deliver the pinned post through its own
        // instruction, ahead of the entries.
        let mut rest = tweets("6", 2, None);
        rest.push(cursor_entry("Bottom", "c1"));
        let j = json!({"data": {"user": {"result": {"timeline_v2": {"timeline": {"instructions": [
            {"type": "TimelineClearCache"},
            {"type": "TimelinePinEntry", "entry": tweet_entry(tweet("5000", "me", None))},
            add_entries(rest),
        ]}}}}}});
        let ids: Vec<String> = parse_timeline_page(&j)
            .tweets
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, ["5000", "6000", "6001"], "the pinned post leads");

        // A retweet's own full_text is X's truncated "RT @user: …" stub; the
        // original rides along whole (sometimes behind the visibility
        // wrapper).
        let whole = format!("{}the end", "long original text ".repeat(12));
        let mut original = tweet("7000", "alice", None);
        original["legacy"]["full_text"] = json!(whole);
        for wrapped in [
            original.clone(),
            json!({"__typename": "TweetWithVisibilityResults", "tweet": original}),
        ] {
            let mut rt = tweet("7001", "bob", None);
            rt["legacy"]["full_text"] =
                json!("RT @alice: long original text long original text long origi…");
            rt["legacy"]["retweeted_status_result"] = json!({"result": wrapped});
            let page = parse_timeline_page(&json!({"instructions": [
                add_entries(vec![tweet_entry(rt)])
            ]}));
            let t = &page.tweets[0];
            assert_eq!(t.author, "bob", "the retweeter stays the author");
            assert_eq!(t.text, format!("RT @alice: {whole}"));
        }
    }

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
        assert_eq!(
            page.tweets.len(),
            30,
            "30 replies (the promoted tweet is not one)"
        );
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
        assert!(
            !collect_page(&j2).terminated,
            "Top terminate is not bottom exhaustion"
        );
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
        assert!(
            rebuilt.starts_with("https://x.com/i/api/graphql/zoF7_t363wZyzylk-BLfZQ/TweetDetail?")
        );
        assert!(
            rebuilt.contains("features=%7B%22a%22%3Atrue%7D"),
            "features preserved"
        );
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
