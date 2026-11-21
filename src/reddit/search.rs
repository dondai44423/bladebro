//! Reddit search results via live DOM + sequential HTML pagination — zero
//! `.json` API endpoints.
//!
//! Reads rendered Shreddit SDUI units (`[data-testid="search-post-unit"]`,
//! `[data-testid="sdui-post-unit"]`, `<shreddit-post>`) and `old.reddit.com`
//! `.search-result` rows directly from the page, then follows Shreddit's own
//! `<faceplate-partial src="/svc/shreddit/search/...">` (or `old.reddit` next
//! links) sequentially when `limit` asks for more items than the first page.

use std::collections::HashSet;

use serde_json::Value;

use super::net::{
    dom_parse_expr, eval_dom, fetch_html_parsed, is_rate_limit, is_security_block, polite_pause,
    retry_after_from_err,
};
use super::tree::{
    as_int, bare_author, canonical_reddit_url, cut_chars, normalize_date, normalize_text, u_prefix,
};
use super::*;

pub(super) const PARSE_SEARCH_JS: &str = include_str!("js/parse_search.js");

/// URL params that shape a search listing. Challenge handshake and tracking
/// params (`solution`, `js_challenge`, `jsc_*`, …) are dropped.
const KEPT_PARAMS: [&str; 6] = ["q", "type", "sort", "t", "restrict_sr", "include_over_18"];

/// Maximum search items returned in one extract call.
pub const MAX_SEARCH_PAGE: usize = 100;
/// Default result cap when the caller passed no explicit limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 25;
/// Hard ceiling on extra pagination HTML requests per search extract.
const MAX_SEARCH_REQUESTS: u32 = 5;
/// Wall-clock budget for paginated search reads.
const SEARCH_BUDGET: Duration = Duration::from_secs(15);

/// One search result.
#[derive(Debug, Serialize)]
pub struct SearchItem {
    pub title: String,
    pub url: String,
    pub subreddit: String,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Self-post body excerpt (absent for link posts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The `extract auto` payload for a Reddit search page.
#[derive(Debug, Serialize)]
pub struct SearchPayload {
    pub container: &'static str,
    /// The query this listing answers.
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    pub count: usize,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<SearchItem>,
}

/// `(path, kept query)` for a posts-shaped Reddit search page, or `None` when
/// this page is not a posts search (communities/users keep generic DOM read).
pub fn search_target(pathname: &str, raw_params: &str) -> Option<(String, String)> {
    let path = pathname.trim_end_matches('/');
    if path != "/search" && !path.ends_with("/search") {
        return None;
    }
    let qs = raw_params.strip_prefix('?').unwrap_or(raw_params);
    let mut kept: Vec<(&str, &str)> = Vec::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if KEPT_PARAMS.contains(&k) {
            kept.push((k, v));
        }
    }
    let q = kept
        .iter()
        .find(|(k, _)| *k == "q")
        .map(|(_, v)| decode(v))?;
    if q.trim().is_empty() {
        return None;
    }
    let ty = kept
        .iter()
        .find(|(k, _)| *k == "type")
        .map(|(_, v)| *v)
        .unwrap_or("");
    if !matches!(ty, "" | "posts" | "links" | "all") {
        return None;
    }
    let mut query = kept
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let in_subreddit = path
        .strip_prefix("/r/")
        .and_then(|rest| rest.strip_suffix("/search"))
        .is_some_and(|sub| !sub.is_empty() && !sub.contains('/'));
    if in_subreddit && !kept.iter().any(|(k, _)| *k == "restrict_sr") {
        query.push_str("&restrict_sr=1");
    }
    Some((path.to_string(), query))
}

/// Extract Reddit search results from the live DOM, following Shreddit HTML
/// partials (`/svc/shreddit/search/...`) or `old.reddit` next links
/// sequentially when `limit` needs additional pages. Never calls `.json`.
pub async fn fetch_search(
    cdp: &CdpSession,
    path: &str,
    query: &str,
    limit: usize,
) -> Result<SearchPayload> {
    if !query.split('&').any(|p| p.starts_with("q=")) {
        return Err(BladeError::Other("reddit: search without a query".into()));
    }
    let limit = limit.clamp(1, MAX_SEARCH_PAGE);
    let deadline = Instant::now() + SEARCH_BUDGET;
    let param = |k: &str| {
        query.split('&').find_map(|p| {
            p.split_once('=')
                .filter(|(pk, _)| *pk == k)
                .map(|(_, v)| decode(v))
        })
    };
    let q_str = param("q").unwrap_or_default();
    let sort = param("sort");
    let time = param("t");

    let dom_expr = dom_parse_expr(PARSE_SEARCH_JS, "parseRedditSearch(document)");
    let mut first = eval_dom(cdp, &dom_expr).await?;
    let first_empty = first
        .get("items")
        .and_then(Value::as_array)
        .is_none_or(|a| a.is_empty());
    if first_empty {
        // SPA search routes can still be painting right after URL change;
        // pause briefly and re-read the DOM once before falling back to one
        // same-origin HTML GET of the search page itself (never `.json`).
        tokio::time::sleep(Duration::from_millis(600)).await;
        first = eval_dom(cdp, &dom_expr).await?;
        let still_empty = first
            .get("items")
            .and_then(Value::as_array)
            .is_none_or(|a| a.is_empty());
        if still_empty {
            let html_path = format!("{path}/?{query}");
            if let Ok(fetched) =
                fetch_html_parsed(cdp, &html_path, PARSE_SEARCH_JS, "parseRedditSearch(doc)").await
            {
                first = fetched;
            }
        }
    }

    let mut items: Vec<SearchItem> = Vec::new();
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut skipped_ads = 0usize;
    let mut next_url =
        ingest_search_batch(&first, &mut items, &mut seen_urls, &mut skipped_ads, limit);
    let mut seen_pages: HashSet<String> = HashSet::new();
    let mut requests = 0u32;
    let mut rate_limited = false;
    let mut retry_after: Option<String> = None;
    let mut security_blocked = false;
    let mut budget_hit = false;

    while items.len() < limit {
        let Some(url) = next_url.take() else {
            break;
        };
        if !seen_pages.insert(url.clone()) {
            break;
        }
        if requests >= MAX_SEARCH_REQUESTS || Instant::now() >= deadline {
            next_url = Some(url);
            budget_hit = true;
            break;
        }
        polite_pause().await;
        requests += 1;
        match fetch_html_parsed(cdp, &url, PARSE_SEARCH_JS, "parseRedditSearch(doc)").await {
            Ok(batch) => {
                let before = items.len();
                next_url = ingest_search_batch(
                    &batch,
                    &mut items,
                    &mut seen_urls,
                    &mut skipped_ads,
                    limit,
                );
                if items.len() == before {
                    break;
                }
            }
            Err(e) => {
                if is_rate_limit(&e) {
                    rate_limited = true;
                    retry_after = retry_after_from_err(&e);
                } else if is_security_block(&e) {
                    security_blocked = true;
                }
                next_url = Some(url);
                break;
            }
        }
    }

    Ok(assemble_search(
        q_str,
        sort,
        time,
        items,
        limit,
        next_url.is_some(),
        skipped_ads,
        budget_hit,
        rate_limited,
        retry_after,
        security_blocked,
    ))
}

fn ingest_search_batch(
    raw: &Value,
    items: &mut Vec<SearchItem>,
    seen_urls: &mut HashSet<String>,
    skipped_ads: &mut usize,
    limit: usize,
) -> Option<String> {
    *skipped_ads += raw.get("skipped_ads").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut overflowed = false;
    if let Some(arr) = raw.get("items").and_then(Value::as_array) {
        for it in arr {
            let title = it.get("title").and_then(Value::as_str).unwrap_or("").trim();
            let raw_url = it.get("url").and_then(Value::as_str).unwrap_or("").trim();
            if title.is_empty() || raw_url.is_empty() {
                continue;
            }
            let url = canonical_reddit_url(raw_url);
            let key = url
                .split('#')
                .next()
                .unwrap_or(&url)
                .trim_end_matches('/')
                .to_string();
            if !seen_urls.insert(key) {
                continue;
            }
            if items.len() >= limit {
                overflowed = true;
                break;
            }
            let subreddit = it
                .get("subreddit")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let author = u_prefix(&bare_author(
                it.get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("[deleted]"),
            ));
            let text = it
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| normalize_text(s, 240));
            items.push(SearchItem {
                title: cut_chars(title, 200),
                url,
                subreddit,
                author,
                score: it.get("score").and_then(as_int),
                comments: it.get("comments").and_then(as_int),
                date: it
                    .get("date")
                    .and_then(Value::as_str)
                    .and_then(normalize_date),
                domain: it
                    .get("domain")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                text,
            });
        }
    }
    let next = raw
        .get("next_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    if overflowed && next.is_none() {
        Some("#more-in-batch".to_string())
    } else {
        next
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble_search(
    query: String,
    sort: Option<String>,
    time: Option<String>,
    items: Vec<SearchItem>,
    limit: usize,
    has_more: bool,
    skipped_ads: usize,
    budget_hit: bool,
    rate_limited: bool,
    retry_after: Option<String>,
    security_blocked: bool,
) -> SearchPayload {
    let count = items.len();
    let mut notes: Vec<String> = Vec::new();
    if rate_limited {
        let ra = retry_after
            .map(|r| format!(", retry-after: {r}"))
            .unwrap_or_default();
        notes.push(format!(
            "reddit rate limit reached (HTTP 429{ra}): showing {count} results — wait before retrying"
        ));
    } else if security_blocked {
        notes.push(format!(
            "reddit's network-security wall interrupted pagination: showing {count} results"
        ));
    } else if budget_hit {
        notes.push(format!(
            "pagination budget reached: showing the first {count} results"
        ));
    } else if has_more {
        if limit >= MAX_SEARCH_PAGE {
            notes.push(format!(
                "showing the first {count} results — at the {MAX_SEARCH_PAGE}-item search extract ceiling"
            ));
        } else {
            notes.push(format!(
                "showing the first {count} results — more pages exist (raise limit to fetch more)"
            ));
        }
    }
    if skipped_ads > 0 {
        notes.push(format!("{skipped_ads} promoted post(s) skipped"));
    }
    let complete = !has_more && !rate_limited && !security_blocked && !budget_hit;
    let note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    SearchPayload {
        container: "reddit-search",
        query,
        sort,
        time,
        count,
        complete,
        note,
        items,
    }
}

/// Percent-decode one param value for the query echo.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn search_target_sanitizes_and_scopes_subreddit_queries() {
        assert_eq!(
            search_target("/search/", "?q=best+monitor&sort=top&t=year&js_challenge=1"),
            Some(("/search".into(), "q=best+monitor&sort=top&t=year".into()))
        );
        assert_eq!(
            search_target("/r/rust/search/", "?q=async&type=posts"),
            Some((
                "/r/rust/search".into(),
                "q=async&type=posts&restrict_sr=1".into()
            ))
        );
        assert_eq!(
            search_target("/r/rust/search/", "?q=async&restrict_sr=0"),
            Some(("/r/rust/search".into(), "q=async&restrict_sr=0".into()))
        );
        assert!(search_target("/search/", "?q=rust&type=communities").is_none());
        assert!(search_target("/search/", "?q=rust&type=people").is_none());
        assert!(search_target("/search/", "?type=posts").is_none());
        assert!(search_target("/r/rust/", "?q=rust").is_none());
    }

    #[test]
    fn search_batch_dedupes_skips_ads_and_tracks_completion() {
        let raw = json!({
            "items": [
                {
                    "title": "Async Rust in 2026",
                    "url": "https://www.reddit.com/r/rust/comments/abc1/async_rust/",
                    "subreddit": "r/rust",
                    "author": "u/alice",
                    "score": 142,
                    "comments": 37,
                    "date": "2026-11-20T12:30:00Z",
                    "text": "Overview of async runtimes."
                },
                {
                    "title": "Duplicate",
                    "url": "https://www.reddit.com/r/rust/comments/abc1/async_rust",
                    "subreddit": "r/rust",
                    "author": "alice"
                }
            ],
            "skipped_ads": 1,
            "next_url": "/svc/shreddit/search/?q=async&cursor=c2"
        });
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let mut ads = 0;
        let next = ingest_search_batch(&raw, &mut items, &mut seen, &mut ads, 25);
        assert_eq!(items.len(), 1);
        assert_eq!(ads, 1);
        assert_eq!(
            next.as_deref(),
            Some("/svc/shreddit/search/?q=async&cursor=c2")
        );
        let payload = assemble_search(
            "async".into(),
            Some("relevance".into()),
            None,
            items,
            25,
            true,
            ads,
            false,
            false,
            None,
            false,
        );
        assert_eq!(payload.container, "reddit-search");
        assert!(!payload.complete);
        let note = payload.note.as_deref().unwrap_or("");
        assert!(note.contains("1 promoted post(s) skipped"), "{note}");
        assert!(note.contains("more pages exist"), "{note}");
    }

    #[tokio::test]
    async fn fetch_search_paginates_via_html_partials_with_zero_json_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut exprs = Vec::new();
            for step in 0..2 {
                let req = socket.next().await.unwrap().unwrap();
                let req: Value = serde_json::from_str(req.to_text().unwrap()).unwrap();
                let expr = req["params"]["expression"].as_str().unwrap().to_string();
                assert!(
                    !expr.contains(".json"),
                    "search must never hit .json endpoints: {expr:.200}"
                );
                exprs.push(expr);
                let value = if step == 0 {
                    json!({
                        "items": [{
                            "title": "First post",
                            "url": "https://www.reddit.com/r/rust/comments/1/first/",
                            "subreddit": "r/rust",
                            "author": "alice",
                            "score": 50,
                            "comments": 10
                        }],
                        "skipped_ads": 0,
                        "next_url": "/svc/shreddit/search/?q=rust&cursor=p2"
                    })
                } else {
                    json!({
                        "s": 200,
                        "ra": "",
                        "head": "",
                        "len": 200,
                        "data": {
                            "items": [{
                                "title": "Second post",
                                "url": "https://www.reddit.com/r/rust/comments/2/second/",
                                "subreddit": "r/rust",
                                "author": "bob",
                                "score": 25,
                                "comments": 4
                            }],
                            "skipped_ads": 1,
                            "next_url": null
                        }
                    })
                };
                socket
                    .send(Message::Text(
                        json!({"id": req["id"], "result": {"result": {"value": value}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            exprs
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let p = fetch_search(&cdp, "/r/rust/search", "q=rust&restrict_sr=1", 10)
            .await
            .unwrap();
        let exprs = task.await.unwrap();
        assert_eq!(exprs.len(), 2);
        assert!(exprs[1].contains("/svc/shreddit/search/?q=rust&cursor=p2"));
        assert_eq!(p.count, 2);
        assert!(p.complete);
        assert_eq!(p.items[0].title, "First post");
        assert_eq!(p.items[1].title, "Second post");
        assert_eq!(p.note.as_deref(), Some("1 promoted post(s) skipped"));
    }

    #[test]
    fn query_decoding_handles_plus_and_percent_escapes() {
        assert_eq!(decode("best+monitor"), "best monitor");
        assert_eq!(decode("%E2%82%AC%20100"), "€ 100");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("a%2"), "a%2");
    }
}
