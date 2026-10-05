//! Reddit search results via the page's own listing JSON.
//!
//! The search page is fully client-rendered: result units are SDUI components
//! (`[data-testid=search-post-unit]`, …, verified live) and no `shreddit-post`
//! ever appears — so the feed fast path cannot see this page, and the
//! structural fallback used to read it blind. Worse, the page's router moves
//! the search query in the URL BEFORE the new feed paints: a DOM read that
//! lands in that window answers the previous query.
//!
//! Reading reddit's own `/search.json` listing avoids both: exact scores
//! (the DOM only shows reddit's fuzzed display values), the query the listing
//! actually answered echoed back in the payload, and freshness that does not
//! depend on what the router has painted yet.

use super::*;

use super::tree::{cut_chars, format_epoch, normalize_text};
/// URL params that shape a search listing. Everything else on the page's own
/// url — reddit's `solution`/`js_challenge`/`jsc_*` challenge handshake and
/// tracking params — is dropped; replaying it into the API path would be
/// meaningless.
const KEPT_PARAMS: [&str; 6] = ["q", "type", "sort", "t", "restrict_sr", "include_over_18"];

/// Reddit serves at most this many items per listing page.
pub const MAX_SEARCH_PAGE: usize = 100;
/// Default result cap when the caller passed no explicit limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 25;

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
    /// The query this listing answers (echoed so a caller can prove the
    /// results match what was asked — the DOM cannot say this).
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<SearchItem>,
}

/// `(path, kept query)` for the listing fetch, or `None` when this page is not
/// a posts-shaped search (communities/users searches keep the DOM path).
///
/// `pathname`/`raw_params` come from the page itself (`location`), so the
/// fetch happens against the url the router already moved to — not against
/// whatever feed happens to be mounted.
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
    let query = kept
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    Some((path.to_string(), query))
}

/// Fetch one page of search results. `path` is `/search` or `/r/<sub>/search`,
/// `query` the kept params (already url-encoded — reddit sees its own bytes).
pub async fn fetch_search(
    cdp: &CdpSession,
    path: &str,
    query: &str,
    limit: usize,
) -> Result<SearchPayload> {
    if !query.split('&').any(|p| p.starts_with("q=")) {
        return Err(BladeError::Other("reddit: search without a query".into()));
    }
    // Same loid gate as the comment sweep: without the token these API paths
    // answer with the network-security wall instead of JSON.
    if !ensure_loid(cdp).await {
        return Err(BladeError::Other(
            "reddit: no loid session token yet (the page-load challenge has not resolved) — load the page once, then retry".into(),
        ));
    }
    let limit = limit.clamp(1, MAX_SEARCH_PAGE);
    let raw = fetch_json(
        cdp,
        &format!("{path}.json?{query}&limit={limit}&raw_json=1"),
    )
    .await?;
    let param = |k: &str| {
        query.split('&').find_map(|p| {
            p.split_once('=')
                .filter(|(pk, _)| *pk == k)
                .map(|(_, v)| decode(v))
        })
    };
    parse_search(
        &raw,
        &param("q").unwrap_or_default(),
        param("sort"),
        param("t"),
        limit,
    )
}

/// Reddit serializes some numerics as floats (`"created_utc": 1789604179.0`,
/// captured live) — accept both shapes rather than dropping the field.
fn as_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// Parse one search listing into the payload. Pure: the unit tests run it on
/// a real captured listing.
fn parse_search(
    raw: &Value,
    query: &str,
    sort: Option<String>,
    time: Option<String>,
    page: usize,
) -> Result<SearchPayload> {
    let children = raw["data"]["children"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::new();
    let mut skipped_ads = 0usize;
    for c in &children {
        if items.len() >= page {
            break;
        }
        if c["kind"].as_str() != Some("t3") {
            continue;
        }
        let d = &c["data"];
        // Promoted/promoted-UI posts are ads, not results — serving them as
        // ordinary items without a word is the confident-but-wrong class.
        if d["is_created_from_ads_ui"].as_bool() == Some(true)
            || d["promoted"].as_bool() == Some(true)
        {
            skipped_ads += 1;
            continue;
        }
        let title = d["title"].as_str().unwrap_or("").trim();
        if title.is_empty() {
            continue;
        }
        let permalink = d["permalink"].as_str().unwrap_or("");
        let url = if permalink.is_empty() {
            d["url"].as_str().unwrap_or("").to_string()
        } else if permalink.starts_with('/') {
            format!("https://www.reddit.com{permalink}")
        } else {
            format!("https://www.reddit.com/{permalink}")
        };
        let subreddit = d["subreddit_name_prefixed"]
            .as_str()
            .map(str::to_string)
            .or_else(|| d["subreddit"].as_str().map(|s| format!("r/{s}")))
            .unwrap_or_default();
        let author = match d["author"].as_str().unwrap_or("") {
            "" | "[deleted]" => "[deleted]".to_string(),
            a => format!("u/{a}"),
        };
        let selftext = d["selftext"].as_str().unwrap_or("").trim();
        items.push(SearchItem {
            title: cut_chars(title, 200),
            url,
            subreddit,
            author,
            score: as_int(&d["score"]),
            comments: as_int(&d["num_comments"]),
            date: as_int(&d["created_utc"]).map(format_epoch),
            domain: d["domain"].as_str().map(str::to_string),
            text: (!selftext.is_empty()).then(|| normalize_text(selftext, 240)),
        });
    }
    let count = items.len();
    let mut note = raw["data"]["after"]
        .as_str()
        .filter(|a| !a.is_empty())
        .map(|_| {
            format!(
                "showing the first {count} results — reddit pages search listings at {MAX_SEARCH_PAGE} per request (raise limit to fetch more)"
            )
        });
    if skipped_ads > 0 {
        let ads = format!("{skipped_ads} promoted post(s) skipped");
        note = Some(match note {
            Some(n) => format!("{n}; {ads}"),
            None => ads,
        });
    }
    Ok(SearchPayload {
        container: "reddit-search",
        query: query.to_string(),
        sort,
        time,
        count,
        note,
        items,
    })
}

/// Percent-decode one param value for the query echo (display only — the
/// request path replays the page's own encoded bytes).
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

    fn fixture() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/reddit_search.json");
        let raw = std::fs::read_to_string(&path).expect("fixture readable");
        serde_json::from_str(&raw).expect("fixture is JSON")
    }

    #[test]
    fn target_keeps_result_params_and_drops_the_challenge_handshake() {
        // The live shape: the page-load JS challenge leaves solution/
        // js_challenge/jsc_* params on the url (captured 2026-10-01).
        let params = "?q=best+monitor+for+programming&type=posts&solution=0bb183e5\
                      &js_challenge=1&jsc_token=2824be&jsc_orig_r=&sort=top&t=year";
        let (path, query) = search_target("/search/", params).expect("posts search accepted");
        assert_eq!(path, "/search");
        assert_eq!(
            query,
            "q=best+monitor+for+programming&type=posts&sort=top&t=year"
        );

        // Subreddit-scoped search: same endpoint family.
        let (path, query) =
            search_target("/r/rust/search/", "?q=lifetimes&restrict_sr=1&type=posts")
                .expect("scoped search accepted");
        assert_eq!(path, "/r/rust/search");
        assert_eq!(query, "q=lifetimes&restrict_sr=1&type=posts");
    }

    #[test]
    fn target_rejects_non_post_searches_and_missing_queries() {
        // Community/user searches are not listings of posts.
        assert!(search_target("/search/", "?q=rust&type=communities").is_none());
        assert!(search_target("/search/", "?q=rust&type=users").is_none());
        // The "All" tab is the no-type search (posts listing) — accepted.
        assert!(search_target("/search/", "?q=rust&type=all").is_some());
        // No query at all, or a blank one.
        assert!(search_target("/search/", "?type=posts").is_none());
        assert!(search_target("/search/", "?q=&type=posts").is_none());
        // Not a search page.
        assert!(search_target("/r/rust/", "?q=rust").is_none());
        assert!(search_target("/search-ish/", "?q=rust").is_none());
    }

    #[test]
    fn a_real_listing_parses_with_exact_fields_and_the_query_echo() {
        let p = parse_search(
            &fixture(),
            "best monitor for programming",
            Some("relevance".into()),
            None,
            100,
        )
        .expect("parse");
        assert_eq!(p.container, "reddit-search");
        assert_eq!(p.query, "best monitor for programming");
        assert_eq!(p.sort.as_deref(), Some("relevance"));
        assert_eq!(p.count, 3);
        assert_eq!(p.items.len(), 3);

        let first = &p.items[0];
        assert_eq!(
            first.title,
            "Which is the best monitor for programming worth buying?"
        );
        assert_eq!(first.subreddit, "r/Monitors");
        assert_eq!(first.author, "u/Reasonable_You5995");
        assert_eq!(first.score, Some(3), "exact score, not a fuzzed one");
        assert_eq!(first.comments, Some(21));
        assert_eq!(first.date.as_deref(), Some("2026-09-17 00:16 UTC"));
        assert_eq!(first.url, "https://www.reddit.com/r/Monitors/comments/1wiegex/which_is_the_best_monitor_for_programming_worth/");
        assert!(
            first.text.as_deref().is_some_and(|t| !t.is_empty()),
            "self-post excerpt present"
        );
        // Reddit still has pages of results behind this one.
        assert!(
            p.note
                .as_deref()
                .is_some_and(|n| n.contains("showing the first 3 results")),
            "paging note: {:?}",
            p.note
        );
        // Field order keeps `container` first (payload contract).
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.starts_with("{\"container\":\"reddit-search\""), "{s:.80}");
    }

    #[test]
    fn non_post_children_are_filtered_and_empty_listings_stay_honest() {
        let raw = serde_json::json!({"kind":"Listing","data":{"after":null,"children":[
            {"kind":"t5","data":{"title":"subreddit"}},
            {"kind":"t3","data":{"title":"","permalink":"/r/x/comments/1/a/"}},
            {"kind":"t3","data":{"title":"Real result","permalink":"/r/x/comments/2/b/",
             "subreddit":"x","subreddit_name_prefixed":"r/x","author":"[deleted]",
             "score":1,"num_comments":0,"created_utc":0,"domain":"self.x","selftext":""}}
        ]}});
        let p = parse_search(&raw, "q", None, None, 100).expect("parse");
        assert_eq!(p.count, 1, "t5 and blank-title children dropped");
        assert_eq!(p.items[0].author, "[deleted]");
        assert_eq!(p.items[0].text, None, "empty selftext stays absent");
        assert!(p.note.is_none(), "no `after` = complete listing");

        let empty = serde_json::json!({"kind":"Listing","data":{"after":null,"children":[]}});
        let p = parse_search(&empty, "q", None, None, 100).expect("parse");
        assert_eq!(p.count, 0);
        assert!(p.items.is_empty());
    }

    #[test]
    fn promoted_posts_are_skipped_with_a_note() {
        let raw = serde_json::json!({"kind":"Listing","data":{"after":null,"children":[
            {"kind":"t3","data":{"title":"Promoted thing","is_created_from_ads_ui":true,
             "permalink":"/r/x/comments/1/a/","score":1,"num_comments":0}},
            {"kind":"t3","data":{"title":"Real result","is_created_from_ads_ui":false,
             "permalink":"/r/x/comments/2/b/","score":1,"num_comments":0}}
        ]}});
        let p = parse_search(&raw, "q", None, None, 100).expect("parse");
        assert_eq!(p.count, 1);
        assert_eq!(p.items[0].title, "Real result");
        assert!(
            p.note
                .as_deref()
                .is_some_and(|n| n.contains("1 promoted post(s) skipped")),
            "ad skip is disclosed: {:?}",
            p.note
        );
    }

    #[test]
    fn the_page_cap_trims_the_listing() {
        let raw = fixture();
        let p = parse_search(&raw, "q", None, None, 2).expect("parse");
        assert_eq!(p.count, 2, "caller's cap is honored");
    }

    #[test]
    fn query_decoding_handles_plus_and_percent_escapes() {
        assert_eq!(decode("best+monitor"), "best monitor");
        assert_eq!(decode("%E2%82%AC%20100"), "€ 100");
        assert_eq!(decode("100%"), "100%", "a stray % is kept");
        assert_eq!(decode("a%2"), "a%2");
    }
}
