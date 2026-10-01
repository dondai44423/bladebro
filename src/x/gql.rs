//! GraphQL replay machinery: template capture from the page's own XHR
//! traffic, replay-URL rebuilds, the in-page fetch, and the request budget.

use super::*;

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
        let params: Vec<(String, String)> =
            all.into_iter().filter(|(k, _)| k != "variables").collect();
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
pub(super) fn parse_gql_url(url: &str) -> Option<(String, String, String)> {
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
pub(super) struct Budget {
    requests: u32,
    deadline: Instant,
}

impl Budget {
    pub(super) fn new() -> Self {
        Budget {
            requests: 0,
            deadline: Instant::now() + TOTAL_BUDGET,
        }
    }
    pub(super) fn take(&mut self) -> bool {
        self.requests += 1;
        self.requests <= MAX_REQUESTS && Instant::now() < self.deadline
    }
}
