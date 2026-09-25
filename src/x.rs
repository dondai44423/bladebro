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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::page::Page;

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

/// A graphql request template captured from the page's own traffic.
#[derive(Debug, Clone)]
pub struct GqlTemplate {
    pub qid: String,
    pub op: String,
    /// Full original URL — replay-exact.
    pub url: String,
    /// Query params minus `variables` (features, fieldToggles, …).
    pub params: Vec<(String, String)>,
    /// The variables blob the app sent (shape reference for rebuilds).
    pub variables: Value,
    /// Request headers captured for replay (auth + twitter metadata subset).
    pub headers: Vec<(String, String)>,
}

/// op → newest template seen in the XHR ring. Iterates newest-first, so the
/// first hit per operation wins.
pub fn capture_templates(page: &Page) -> HashMap<String, GqlTemplate> {
    let mut out = HashMap::new();
    for e in page.xhr_log().iter().rev() {
        let Some((qid, op, query)) = parse_gql_url(&e.url) else {
            continue;
        };
        if out.contains_key(&op) {
            continue;
        }
        let all: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut variables = all
            .iter()
            .find(|(k, _)| k == "variables")
            .and_then(|(_, v)| serde_json::from_str::<Value>(v).ok())
            .unwrap_or(Value::Null);
        // The cursor is ours to manage: our own paginated replays land in
        // the XHR ring too, and a captured page-2 cursor would hijack the
        // next extraction to the tail of the thread.
        if let Some(o) = variables.as_object_mut() {
            o.remove("cursor");
        }
        let params: Vec<(String, String)> = all
            .into_iter()
            .filter(|(k, _)| k != "variables")
            .collect();
        out.insert(
            op.clone(),
            GqlTemplate {
                qid,
                op,
                url: e.url.clone(),
                params,
                variables,
                headers: e.headers.clone(),
            },
        );
    }
    out
}

/// Split `…/i/api/graphql/<qid>/<Op>?<query>` → (qid, op, query).
fn parse_gql_url(url: &str) -> Option<(String, String, String)> {
    let rest = url.split("/i/api/graphql/").nth(1)?;
    let (qid, after) = rest.split_once('/')?;
    let (op, query) = match after.split_once('?') {
        Some((o, q)) => (o, q),
        None => (after, ""),
    };
    if qid.is_empty() || op.is_empty() {
        return None;
    }
    Some((qid.to_string(), op.to_string(), query.to_string()))
}

/// Rebuild the request URL with different variables — everything else
/// (features, fieldToggles) stays exactly as captured.
pub fn build_url(tpl: &GqlTemplate, variables: &Value) -> String {
    let mut q: Vec<(String, String)> = tpl
        .params
        .iter()
        .filter(|(k, _)| k != "variables")
        .cloned()
        .collect();
    q.push((
        "variables".to_string(),
        serde_json::to_string(variables).unwrap_or_default(),
    ));
    let mut out = format!("https://x.com/i/api/graphql/{}/{}?", tpl.qid, tpl.op);
    {
        let mut ser = url::form_urlencoded::Serializer::new(&mut out);
        ser.extend_pairs(q.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    out
}

/// Replay one same-origin API call from page context, exactly as the app
/// does it: cookies ride along, identity headers come from the captured
/// template, and missing ones are synthesized (ct0 from the cookie, the
/// public web bearer, active-user literals).
pub async fn fetch_api(
    cdp: &CdpSession,
    url: &str,
    headers: &[(String, String)],
) -> Result<(i64, String)> {
    let url_js = serde_json::to_string(url)?;
    let hdr_js = serde_json::to_string(headers)?;
    let expr = format!(
        "(async()=>{{try{{\
const hdrs=Object.fromEntries({hdr_js});\
const m=document.cookie.match(/(?:^|;\\s*)ct0=([^;]+)/);\
if(m&&!hdrs['x-csrf-token'])hdrs['x-csrf-token']=m[1];\
if(!hdrs['authorization'])hdrs['authorization']='{WEB_BEARER}';\
if(!hdrs['x-twitter-auth-type'])hdrs['x-twitter-auth-type']='OAuth2Session';\
if(!hdrs['x-twitter-active-user'])hdrs['x-twitter-active-user']='yes';\
const c=new AbortController();const t=setTimeout(()=>c.abort(),{PAGE_FETCH_TIMEOUT_MS});\
const r=await fetch({url_js},{{credentials:'include',headers:hdrs,signal:c.signal}});clearTimeout(t);\
const x=await r.text();return {{s:r.status,t:x}};}}catch(e){{return {{s:0,t:String(e)}}}}}})()"
    );
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
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
        return Err(BladeError::Other(format!(
            "x: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }
    let val = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok((
        val["s"].as_i64().unwrap_or(0),
        val["t"].as_str().unwrap_or_default().to_string(),
    ))
}

/// Request/clock budget for one extract.
struct Budget {
    requests: u32,
    deadline: Instant,
}

impl Budget {
    fn new() -> Self {
        Budget {
            requests: 0,
            deadline: Instant::now() + TOTAL_BUDGET,
        }
    }
    fn take(&mut self) -> bool {
        self.requests += 1;
        self.requests <= MAX_REQUESTS && Instant::now() < self.deadline
    }
}

/// One parsed tweet (focal post or thread item).
#[derive(Debug, Clone)]
struct T {
    id: String,
    author: String,
    name: String,
    text: String,
    date: String,
    in_reply_to: Option<String>,
    reply_to: Option<String>,
    replies: Option<i64>,
    reposts: Option<i64>,
    likes: Option<i64>,
    views: Option<i64>,
    media: Vec<String>,
}

impl T {
    fn permalink(&self) -> String {
        format!("https://x.com/{}/status/{}", self.author, self.id)
    }
}

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
async fn fetch_thread(
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
async fn fetch_timeline(
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

/// DOM fallback: collect the rendered timeline in ONE in-page pass (a
/// bounded scroll loop). Used when rapid replay is rejected — X gates some
/// ops (search) behind per-request `x-client-transaction-id` values that
/// are single-use, so the page's own rendering is the reliable source.
/// Slower than a replay, still one tool call.
async fn dom_collect(page: &Page, kind: &str, cap: usize) -> Result<XPayload> {
    let script = format!(
        r#"(async()=>{{
const cap={cap};
const items=new Map();
function txt(e){{return e?(e.innerText||e.textContent||'').trim():'';}}
function aria(a,sel){{const e=a.querySelector(sel);return (e&&e.getAttribute('aria-label'))||'';}}
function collect(){{
  for(const a of document.querySelectorAll('article[data-testid="tweet"]')){{
    const link=a.querySelector('a[href*="/status/"]');
    if(!link)continue;
    const m=(link.getAttribute('href')||'').match(/\/([^\/]+)\/status\/(\d+)/);
    if(!m)continue;
    const id=m[2];
    if(items.has(id))continue;
    const un=txt(a.querySelector('[data-testid="User-Name"]'));
    const at=(un.match(/@[A-Za-z0-9_]+/)||[''])[0];
    const t=a.querySelector('time');
    items.set(id,{{
      id:id,
      url:'https://x.com/'+m[1]+'/status/'+id,
      author:at.replace('@',''),
      name:(un.split('\n')[0]||''),
      date:t?(t.getAttribute('datetime')||''):'',
      text:txt(a.querySelector('[data-testid="tweetText"]')),
      replies:aria(a,'[data-testid="reply"]'),
      reposts:aria(a,'[data-testid="retweet"]'),
      likes:aria(a,'[data-testid="like"]')
    }});
  }}
}}
collect();
let idle=0,steps=0;
while(items.size<cap&&idle<6&&steps<40){{
  const before=items.size;
  window.scrollBy(0,Math.round(window.innerHeight*0.9));
  await new Promise(r=>setTimeout(r,350));
  collect();
  steps++;
  if(items.size===before)idle++;else idle=0;
}}
return JSON.stringify({{count:items.size,exhausted:idle>=6,items:[...items.values()].slice(0,cap)}});
}})()"#
    );
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": script,
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
            .unwrap_or("dom collect failed");
        return Err(BladeError::Other(format!(
            "x: {}",
            crate::platform::truncate_utf8(msg, 200)
        )));
    }
    let raw = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    let j: Value = serde_json::from_str(raw)
        .map_err(|e| BladeError::Other(format!("x: dom collect parse ({e})")))?;
    let (items, exhausted) = dom_items_from_json(&j);
    let complete = exhausted && items.len() < cap;
    Ok(XPayload {
        container: "x-timeline".into(),
        kind: kind.to_string(),
        post: None,
        count: items.len(),
        total: None,
        complete,
        note: Some(if complete {
            "x: collected from the rendered page (rapid replay unavailable for this page)"
                .into()
        } else {
            format!(
                "x: collected from the rendered page — capped at {} items",
                items.len()
            )
        }),
        items,
    })
}

/// X's transient "Something went wrong. Try reloading." state — a React
/// boot failure, not a block; a reload fixes it. Only fires on pages whose
/// whole text is tiny (an error screen), so real content never matches.
async fn is_flaky_render(page: &Page) -> bool {
    let res = page
        .cdp_ref()
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": "(function(){var t=(document.body&&document.body.innerText)||'';return t.length<600&&/something (went|goes) wrong|try reloading/i.test(t);})()",
                "returnByValue": true,
            })),
        )
        .await;
    matches!(
        res,
        Ok(r) if r.pointer("/result/value").and_then(|v| v.as_bool()) == Some(true)
    )
}

/// Shape DOM-collected rows into payload items (aria-label counts, ISO dates).
fn dom_items_from_json(j: &Value) -> (Vec<XItem>, bool) {
    let exhausted = j.get("exhausted").and_then(|e| e.as_bool()).unwrap_or(false);
    let mut items = Vec::new();
    for it in j
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let count_of = |k: &str| -> Option<i64> {
            let s = it.get(k).and_then(|v| v.as_str()).unwrap_or("");
            let digits: String = s
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| *c != ',')
                .collect();
            digits.parse::<i64>().ok()
        };
        items.push(XItem {
            op: false,
            depth: None,
            reply_to: None,
            id,
            author: it.get("author").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            date: iso_to_utc(it.get("date").and_then(|v| v.as_str()).unwrap_or("")),
            text: cut_chars(
                it.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                ITEM_TEXT_CAP,
            ),
            replies: count_of("replies"),
            reposts: count_of("reposts"),
            likes: count_of("likes"),
            media: Vec::new(),
            url: it.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        });
    }
    (items, exhausted)
}

/// "2026-09-24T16:18:18.000Z" → "2026-09-24 16:18 UTC".
fn iso_to_utc(s: &str) -> String {
    if s.len() >= 16 && s.as_bytes().get(10) == Some(&b'T') {
        return format!("{} {} UTC", &s[..10], &s[11..16]);
    }
    s.to_string()
}

/// Everything the instruction walker yields for one response page.
struct PageBits {
    items: Vec<(String, Value)>,
    bottom_cursor: Option<String>,
    terminated: bool,
}

/// Walk one graphql response page: find the instructions array, collect
/// tweet item contents, the bottom cursor, and whether the timeline is
/// terminated.
fn collect_page(j: &Value) -> PageBits {
    let mut bits = PageBits {
        items: Vec::new(),
        bottom_cursor: None,
        terminated: false,
    };
    let Some(instructions) = find_instructions(j) else {
        return bits;
    };
    for ins in instructions {
        let ty = ins.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "TimelineAddEntries" => {
                if let Some(entries) = ins.get("entries").and_then(|e| e.as_array()) {
                    for e in entries {
                        walk_entry(&mut bits, e.get("content"));
                    }
                }
            }
            "TimelineAddToModule" => {
                if let Some(arr) = ins.get("moduleItems").and_then(|e| e.as_array()) {
                    for it in arr {
                        let eid = it.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
                        push_item(&mut bits, eid, it.pointer("/item/itemContent"));
                    }
                }
            }
            "TimelineTerminateTimeline" => {
                let dir = ins
                    .get("direction")
                    .and_then(|d| d.as_str())
                    .unwrap_or("Bottom");
                if dir == "Bottom" {
                    bits.terminated = true;
                }
            }
            _ => {}
        }
    }
    bits
}

/// Depth-first search for the `instructions` array — every supported
/// response nests it under different keys (threaded_conversation…, user →
/// timeline_v2, search_by_raw_query…), so one walker handles all of them.
fn find_instructions(v: &Value) -> Option<&Vec<Value>> {
    if let Some(arr) = v.get("instructions").and_then(|i| i.as_array()) {
        return Some(arr);
    }
    match v {
        Value::Object(o) => o.values().find_map(find_instructions),
        Value::Array(a) => a.iter().find_map(find_instructions),
        _ => None,
    }
}

fn walk_entry(bits: &mut PageBits, content: Option<&Value>) {
    let Some(c) = content else { return };
    match c.get("entryType").and_then(|t| t.as_str()).unwrap_or("") {
        "TimelineTimelineItem" => {
            let eid = c.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
            push_item(bits, eid, c.get("itemContent"));
        }
        "TimelineTimelineModule" => {
            if let Some(items) = c.get("items").and_then(|i| i.as_array()) {
                for it in items {
                    let eid = it.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
                    push_item(bits, eid, it.pointer("/item/itemContent"));
                }
            }
        }
        "TimelineTimelineCursor" => {
            let ct = c.get("cursorType").and_then(|t| t.as_str()).unwrap_or("");
            if ct == "Bottom" {
                if let Some(v) = c.get("value").and_then(|v| v.as_str()) {
                    bits.bottom_cursor = Some(v.to_string());
                }
            }
        }
        _ => {}
    }
}

fn push_item(bits: &mut PageBits, entry_id: &str, item_content: Option<&Value>) {
    let Some(ic) = item_content else { return };
    // Promoted tweets ride inside conversation modules as if they were
    // replies; they are ads, not part of the conversation.
    if ic.get("tweet_results").is_some() && ic.get("promotedMetadata").is_none() {
        bits.items.push((entry_id.to_string(), ic.clone()));
    }
}

struct ThreadPage {
    focal: Option<T>,
    tweets: Vec<T>,
    bottom_cursor: Option<String>,
    terminated: bool,
}

/// Parse one TweetDetail page: the focal tweet (its own entry) and every
/// tweet item on the page.
fn parse_thread_page(j: &Value, focal_id: &str) -> ThreadPage {
    let bits = collect_page(j);
    let mut focal = None;
    let mut tweets = Vec::new();
    for (_eid, ic) in &bits.items {
        let Some(t) = tweet_from_item(ic) else { continue };
        if t.id == focal_id && focal.is_none() {
            focal = Some(t);
        } else {
            tweets.push(t);
        }
    }
    ThreadPage {
        focal,
        tweets,
        bottom_cursor: bits.bottom_cursor,
        terminated: bits.terminated,
    }
}

/// Parse one timeline page → (tweets, bottom cursor, terminated).
fn parse_timeline_page(j: &Value) -> (Vec<T>, Option<String>, bool) {
    let bits = collect_page(j);
    let tweets = bits
        .items
        .iter()
        .filter_map(|(_, ic)| tweet_from_item(ic))
        .collect();
    (tweets, bits.bottom_cursor, bits.terminated)
}

/// Extract a tweet from a TimelineTweet itemContent. Handles the visibility
/// wrapper, long-form note tweets, media, and view counts.
fn tweet_from_item(ic: &Value) -> Option<T> {
    let raw = ic.get("tweet_results")?.get("result")?;
    let tr = unwrap_visibility(raw);
    if tr.get("__typename").and_then(|t| t.as_str()) == Some("TweetUnavailable") {
        return None;
    }
    let id = tr.get("rest_id").and_then(|r| r.as_str())?.to_string();
    let user = tr.pointer("/core/user_results/result");
    let author = user
        .and_then(|u| u.pointer("/core/screen_name"))
        .and_then(|s| s.as_str())
        .or_else(|| {
            user.and_then(|u| u.pointer("/legacy/screen_name"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("")
        .to_string();
    let name = user
        .and_then(|u| u.pointer("/core/name"))
        .and_then(|s| s.as_str())
        .or_else(|| {
            user.and_then(|u| u.pointer("/legacy/name"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("")
        .to_string();
    let legacy = tr.get("legacy");
    let note = tr
        .pointer("/note_tweet/note_tweet_results/result/text")
        .and_then(|s| s.as_str());
    let text_raw = note
        .or_else(|| {
            legacy
                .and_then(|l| l.get("full_text"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("");
    let date = legacy
        .and_then(|l| l.get("created_at"))
        .and_then(|s| s.as_str())
        .map(format_x_date)
        .unwrap_or_default();
    let replies = legacy.and_then(|l| l.get("reply_count")).and_then(|v| v.as_i64());
    let reposts = legacy.and_then(|l| l.get("retweet_count")).and_then(|v| v.as_i64());
    let likes = legacy
        .and_then(|l| l.get("favorite_count"))
        .and_then(|v| v.as_i64());
    let views = tr
        .pointer("/views/count")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok());
    let in_reply_to = legacy
        .and_then(|l| l.get("in_reply_to_status_id_str"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let reply_to = legacy
        .and_then(|l| l.get("in_reply_to_screen_name"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let media = collect_media(legacy);
    Some(T {
        id,
        author,
        name,
        text: cut_chars(text_raw, ITEM_TEXT_CAP),
        date,
        in_reply_to,
        reply_to,
        replies,
        reposts,
        likes,
        views,
        media,
    })
}

fn unwrap_visibility(raw: &Value) -> &Value {
    if let Some(t) = raw.get("tweet") {
        t
    } else {
        raw
    }
}

fn collect_media(legacy: Option<&Value>) -> Vec<String> {
    let Some(l) = legacy else { return Vec::new() };
    let arr = l
        .pointer("/extended_entities/media")
        .or_else(|| l.pointer("/entities/media"))
        .and_then(|m| m.as_array());
    let Some(arr) = arr else { return Vec::new() };
    arr.iter()
        .filter_map(|m| m.get("media_url_https").and_then(|u| u.as_str()))
        .map(String::from)
        .collect()
}

/// Thread depth: hops from the focal tweet along in_reply_to chains.
/// `None` when a chain leaves the harvested set (honest, never guessed).
fn depth_of(
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

/// "Thu Sep 24 16:18:18 +0000 2026" → "2026-09-24 16:18 UTC".
fn format_x_date(s: &str) -> String {
    let p: Vec<&str> = s.split_whitespace().collect();
    if p.len() >= 6 {
        let mon = match p[1] {
            "Jan" => 1,
            "Feb" => 2,
            "Mar" => 3,
            "Apr" => 4,
            "May" => 5,
            "Jun" => 6,
            "Jul" => 7,
            "Aug" => 8,
            "Sep" => 9,
            "Oct" => 10,
            "Nov" => 11,
            "Dec" => 12,
            _ => 0,
        };
        if mon > 0 && p[3].len() >= 5 && p[4].starts_with('+') {
            if let Ok(day) = p[2].parse::<u32>() {
                return format!("{}-{:02}-{:02} {} UTC", p[5], mon, day, &p[3][..5]);
            }
        }
    }
    s.to_string()
}

fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
