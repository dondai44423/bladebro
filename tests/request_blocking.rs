//! Integration test: the request-interception task against a mock CDP page.
//!
//! The mock plays `Fetch.requestPaused` events one at a time and records
//! which verdict (`Fetch.failRequest` / `Fetch.continueRequest`) the real
//! `run_interception` task answers each with — the same event path a live
//! page takes, without a browser. A second mock fails `Fetch.enable` and
//! `Fetch.disable` once each, to pin what the stored block rules say when
//! the interception toggle fails.

use std::sync::{Arc, Mutex};

use bladebro::cdp::{CdpClient, CdpSession};
use bladebro::page::intercept::{run_interception, InterceptState, BLOCK_IMAGES, BLOCK_TRACKERS};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const MAIN: &str = "MAIN-FRAME";
const CHILD: &str = "CHILD-FRAME";
const FAIL: &str = "Fetch.failRequest";
const CONTINUE: &str = "Fetch.continueRequest";

/// Plays each `(resourceType, url, frameId)` as a paused request and returns
/// the verdict method the interception task answered it with.
async fn verdicts(state: &InterceptState, paused: &[(&str, &str, &str)]) -> Vec<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let events: Vec<Value> = paused
        .iter()
        .enumerate()
        .map(|(i, (ty, url, frame))| {
            json!({
                "method": "Fetch.requestPaused",
                "params": {
                    "requestId": format!("interception-{i}"),
                    "request": { "url": url, "method": "GET", "headers": {} },
                    "frameId": frame,
                    "resourceType": ty,
                }
            })
        })
        .collect();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        // The client announces it has subscribed before any event is sent,
        // so no event can race past the task's subscription.
        let ready: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        ws.send(reply(&ready)).await.unwrap();
        let mut seen = Vec::new();
        for (i, ev) in events.into_iter().enumerate() {
            ws.send(Message::Text(ev.to_string().into())).await.unwrap();
            let verdict: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(
                verdict["params"]["requestId"],
                format!("interception-{i}"),
                "verdict answers the paused request"
            );
            seen.push(verdict["method"].as_str().unwrap_or("").to_string());
            ws.send(reply(&verdict)).await.unwrap();
        }
        seen
    });
    let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
    let cdp = CdpSession::root(client);
    let task = tokio::spawn(run_interception(
        cdp.clone(),
        state.clone(),
        cdp.subscribe(),
        MAIN.to_string(),
    ));
    cdp.send("Test.ready", None).await.unwrap();
    let seen = server.await.unwrap();
    task.abort();
    seen
}

fn reply(req: &Value) -> Message {
    Message::Text(json!({ "id": req["id"], "result": {} }).to_string().into())
}

fn page_domain(state: &InterceptState) -> String {
    state.page_domain.read().unwrap().clone()
}

#[tokio::test]
async fn tracker_blocking_never_fails_the_main_document() {
    // On example.com with tracker blocking on, the agent navigates to a site
    // whose own domain is on the tracker list. The old baseline was the
    // PREVIOUS page (updated only by the post-navigation recapture), so the
    // new page's document was "third-party" and failed with BlockedByClient.
    let state = InterceptState::default();
    state.set_rules(BLOCK_TRACKERS);
    state.set_page_url("https://example.com/");
    let got = verdicts(
        &state,
        &[
            ("Document", "https://sentry.io/welcome/", MAIN),
            // The new page's own script is first-party to the new page.
            ("Script", "https://sentry.io/_static/app.js", MAIN),
            // Tracker iframes and third-party trackers stay blocked.
            ("Document", "https://ad.doubleclick.net/frame.html", CHILD),
            (
                "Script",
                "https://www.google-analytics.com/analytics.js",
                MAIN,
            ),
        ],
    )
    .await;
    assert_eq!(got, [CONTINUE, CONTINUE, FAIL, FAIL]);
    assert_eq!(page_domain(&state), "sentry.io");
}

/// A mock page whose FIRST `Fetch.enable` and FIRST `Fetch.disable` fail;
/// every other command succeeds. Returns the session and the commands seen.
async fn flaky_fetch_page() -> (CdpSession, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_srv = seen.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let mut failed: Vec<String> = Vec::new();
        while let Some(Ok(msg)) = ws.next().await {
            let Ok(req) = serde_json::from_str::<Value>(msg.to_text().unwrap_or("")) else {
                continue;
            };
            let method = req["method"].as_str().unwrap_or("").to_string();
            seen_srv.lock().unwrap().push(method.clone());
            let answer = if method.starts_with("Fetch.") && !failed.contains(&method) {
                failed.push(method);
                Message::Text(
                    json!({
                        "id": req["id"],
                        "error": { "code": -32000, "message": "Fetch unavailable" }
                    })
                    .to_string()
                    .into(),
                )
            } else {
                reply(&req)
            };
            if ws.send(answer).await.is_err() {
                break;
            }
        }
    });
    let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
    (CdpSession::root(client), seen)
}

#[tokio::test]
async fn failed_fetch_toggle_leaves_the_reported_rules_true() {
    let (cdp, seen) = flaky_fetch_page().await;
    let state = InterceptState::default();

    // Fetch.enable fails: nothing is being blocked, so `state block get`
    // must not report trackers — and the next request must retry enable.
    assert!(state.apply_rules(&cdp, BLOCK_TRACKERS).await.is_err());
    assert_eq!(state.rules(), 0, "failed enable still reported as active");

    // A retry turns interception on for real (it used to see the stale
    // mask, skip Fetch.enable and "block" with interception off for good).
    assert_eq!(
        state.apply_rules(&cdp, BLOCK_IMAGES).await.unwrap(),
        BLOCK_IMAGES
    );
    assert_eq!(state.rules(), BLOCK_IMAGES);

    // Fetch.disable fails: interception is still on and still blocking, so
    // the rules stay — and a retried clear really sends Fetch.disable.
    assert!(state.apply_rules(&cdp, 0).await.is_err());
    assert_eq!(
        state.rules(),
        BLOCK_IMAGES,
        "failed disable reported as cleared"
    );
    assert_eq!(state.apply_rules(&cdp, 0).await.unwrap(), 0);
    assert_eq!(state.rules(), 0);

    assert_eq!(
        *seen.lock().unwrap(),
        [
            "Fetch.enable",
            "Fetch.enable",
            "Fetch.disable",
            "Fetch.disable"
        ]
    );
}
