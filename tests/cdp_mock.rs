//! Integration test: a mock CDP server proves the [`CdpClient`] multiplexer
//! without needing a real browser.
//!
//! The mock server speaks just enough CDP over a WebSocket to exercise:
//! - command round-trip (request id → matched response),
//! - unsolicited event fan-out,
//! - per-command timeout,
//! - clean failure when the server closes mid-flight.
//!
//! This is a real-protocol test (real WebSocket, real JSON framing), not a mock
//! of the client — so it catches transport-level regressions a unit test of
//! pure logic would miss.

use std::time::Duration;

use bladebro::cdp::{CdpClient, CdpSession};
use bladebro::BladeError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn event_between_wait_creation_and_command_response_is_not_lost() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let request = ws.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
        for message in [
            json!({"method":"Page.frameNavigated","params":{"frame":{"url":"http://fixture/"}}}),
            json!({"id":request["id"],"result":{"loaderId":"loaded"}}),
        ] {
            ws.send(Message::Text(message.to_string().into()))
                .await
                .unwrap();
        }
        while ws.next().await.is_some() {}
    });
    let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
    let session = CdpSession::child(client.clone(), "page-session");
    let root = CdpSession::root(client.clone());
    let navigation = client.wait_for("Page.frameNavigated", Duration::from_millis(100));
    let root_navigation = root.wait_for("Page.frameNavigated", Duration::from_millis(100));
    let unrelated = session.wait_for("Page.frameNavigated", Duration::from_millis(100));
    client.send("Page.navigate", None).await.unwrap();
    let event = navigation
        .await
        .expect("an event preceding the command response must survive");
    assert_eq!(event.params["frame"]["url"], "http://fixture/");
    assert!(root_navigation.await.is_ok());
    assert!(
        matches!(unrelated.await, Err(BladeError::Timeout(_))),
        "session filters must still reject root events"
    );
    drop(client);
    server.abort();
}

#[tokio::test]
async fn roundtrip_command_and_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/devtools/page/test");

    // Mock server: accept, echo a response per request id, push one event.
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = ws.split();

        // Push an unsolicited event immediately.
        let ev = json!({
            "method": "Page.frameNavigated",
            "params": { "frame": { "url": "https://example.test/" } }
        });
        sink.send(Message::Text(ev.to_string().into()))
            .await
            .unwrap();

        // Respond to each request by echoing its id with a canned result.
        while let Some(Ok(msg)) = stream.next().await {
            let text = match msg {
                Message::Text(t) => String::from_utf8_lossy(t.as_bytes()).into_owned(),
                Message::Close(_) => break,
                _ => continue,
            };
            let req: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let resp = match method {
                "Page.getFrameTree" => json!({
                    "id": id,
                    "result": {
                        "frameTree": { "frame": { "url": "https://example.test/" } }
                    }
                }),
                _ => json!({ "id": id, "result": {} }),
            };
            sink.send(Message::Text(resp.to_string().into()))
                .await
                .unwrap();
        }
    });

    let client = CdpClient::connect(&url).await.unwrap();

    // Wait for the unsolicited event.
    let ev = client
        .wait_for("Page.frameNavigated", Duration::from_secs(2))
        .await
        .expect("should receive frameNavigated event");
    assert_eq!(
        ev.params["frame"]["url"].as_str(),
        Some("https://example.test/")
    );

    // Round-trip a command; the mock returns a canned frame tree.
    let tree = client
        .send("Page.getFrameTree", None)
        .await
        .expect("command should succeed");
    assert_eq!(
        tree["frameTree"]["frame"]["url"].as_str(),
        Some("https://example.test/")
    );
}

#[tokio::test]
async fn command_timeout_fires() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/devtools/page/test");

    // Server that accepts but never replies.
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let (_sink, mut stream) = ws.split();
        // Drain and ignore; never respond.
        while let Some(Ok(_)) = stream.next().await {}
    });

    let client = CdpClient::connect(&url).await.unwrap();
    let err = client
        .send_with_timeout("Page.getFrameTree", None, Duration::from_millis(200))
        .await
        .expect_err("should time out");
    assert!(matches!(err, BladeError::Timeout(_)), "got {err:?}");
}

#[tokio::test]
async fn server_close_fails_pending() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/devtools/page/test");

    // Server that accepts, then immediately closes.
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        // Read one frame then close.
        let _ = stream.next().await;
        let _ = sink.send(Message::Close(None)).await;
    });

    let client = CdpClient::connect(&url).await.unwrap();
    // Send a command; the server closes before replying.
    let err = client
        .send_with_timeout("Page.getFrameTree", None, Duration::from_secs(3))
        .await
        .expect_err("should fail with Closed");
    assert!(matches!(err, BladeError::Closed), "got {err:?}");
}

#[tokio::test]
async fn protocol_error_is_typed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/devtools/page/test");

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        while let Some(Ok(msg)) = stream.next().await {
            let text = match msg {
                Message::Text(t) => String::from_utf8_lossy(t.as_bytes()).into_owned(),
                Message::Close(_) => break,
                _ => continue,
            };
            let req: Value = serde_json::from_str(&text).unwrap();
            let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let resp = json!({
                "id": id,
                "error": { "code": -32000, "message": "Cannot find context" }
            });
            sink.send(Message::Text(resp.to_string().into()))
                .await
                .unwrap();
        }
    });

    let client = CdpClient::connect(&url).await.unwrap();
    let err = client
        .send("Runtime.evaluate", Some(json!({})))
        .await
        .expect_err("should surface CDP error");
    match err {
        BladeError::Cdp { code, message } => {
            assert_eq!(code, -32000);
            assert_eq!(message, "Cannot find context");
        }
        other => panic!("expected Cdp, got {other:?}"),
    }
}

#[tokio::test]
async fn rejected_scroll_stops_at_first_dispatch_without_claiming_success() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dispatched = Arc::new(AtomicUsize::new(0));
    let observed = dispatched.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        while let Some(Ok(message)) = ws.next().await {
            let Ok(text) = message.to_text() else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let response = if request["method"] == "Input.dispatchMouseEvent" {
                observed.fetch_add(1, Ordering::SeqCst);
                json!({"id":request["id"],"error":{"code":-32000,"message":"owned scroll refusal"}})
            } else {
                json!({"id":request["id"],"result":{"result":{"value":{}}}})
            };
            if ws
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let client = CdpClient::connect(&format!("ws://{address}"))
        .await
        .unwrap();
    let cdp = CdpSession::root(client);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        bladebro::action::perform(
            &cdp,
            &mut bladebro::page::LivePageModel::new(),
            &bladebro::action::Action::Scroll { dx: 0, dy: 500 },
            &Arc::new(Mutex::new(None)),
        ),
    )
    .await
    .unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("owned scroll refusal"));
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    server.abort();
}

/// A tab/browser that dies under a capture must surface as `Closed` - the
/// error the MCP/daemon loops self-heal on. It used to come back as an
/// empty "loading" page (url "", 0 actionable), which hid the death from
/// self-heal and read to the agent like a blank page.
#[tokio::test]
async fn capture_surfaces_a_dead_connection_instead_of_an_empty_page() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        // Read the capture request, then die without answering.
        let _ = stream.next().await;
        let _ = sink.send(Message::Close(None)).await;
    });
    let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
    let cdp = CdpSession::root(client);
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        bladebro::page::perception::capture(&cdp),
    )
    .await
    .expect("capture must not hang on a closed connection");
    match res {
        Err(BladeError::Closed) => {}
        Err(e) => panic!("expected Closed, got {e:?}"),
        Ok(cap) => panic!(
            "a dead connection was reported as a page: url={:?} phase={} elements={}",
            cap.url,
            cap.phase(),
            cap.elements.len()
        ),
    }
}

// ---- scripted page: drives `action::perform` end to end without a browser ----

type Responder = Box<dyn Fn(&str, &str) -> Value + Send + 'static>;

const FIND_MODES: [&str; 11] = [
    "box", "prepare", "check", "focus", "clear", "selhost", "type", "select", "click", "state",
    "hover",
];

/// Serve one connection, answering every request with `respond(method,
/// expression)` (a `{"result":..}` or `{"error":..}` body; the id is added
/// here) and logging each method name.
async fn scripted_page(
    respond: Responder,
) -> (
    CdpSession,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = log.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        while let Some(Ok(message)) = ws.next().await {
            let Ok(text) = message.to_text() else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let method = request["method"].as_str().unwrap_or("").to_string();
            let expression = request["params"]["expression"].as_str().unwrap_or("");
            let mut response = respond(&method, expression);
            response["id"] = request["id"].clone();
            // find-by-sig calls are logged with their mode ("find:select").
            let entry = match FIND_MODES
                .iter()
                .find(|m| expression.contains(&format!(",\"{m}\",")))
            {
                Some(m) if expression.starts_with("((sig,mode,text,frame)=>") => {
                    format!("find:{m}")
                }
                _ => method,
            };
            seen.lock().unwrap().push(entry);
            if ws
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let client = CdpClient::connect(&format!("ws://{address}"))
        .await
        .unwrap();
    (CdpSession::root(client), log, server)
}

/// A model holding one element, as a capture would mint it.
fn one_element_model(tag: &str, role: &str, name: &str) -> (bladebro::page::LivePageModel, String) {
    let mut lpm = bladebro::page::LivePageModel::new();
    lpm.ingest(bladebro::page::PageCapture {
        url: "http://fixture.test/".into(),
        title: String::new(),
        ready_state: "complete".into(),
        muts: 0,
        elements: vec![bladebro::page::perception::RawElement {
            tag: tag.into(),
            role: role.into(),
            name: name.into(),
            element_type: None,
            value: None,
            options: None,
            disabled: false,
            checked: None,
            href: None,
            placeholder: None,
            required: false,
            has_popup: false,
            landmark: None,
            box_: [10.0, 10.0, 80.0, 30.0],
            sig: format!("|{role}|{name}|1"),
            frame: Vec::new(),
            shadow: false,
            fingerprint: 0,
            pending: false,
            ctx: String::new(),
        }],
    });
    let ref_id = lpm.elements()[0].ref_id.clone();
    (lpm, ref_id)
}

/// Page-side answers shared by the scripted cases: an empty settled page.
fn page_basics(expression: &str) -> Value {
    if expression.contains("cap(d,[],0,0)") {
        json!({"result":{"result":{"type":"object","value":{"url":"http://fixture.test/","readyState":"complete","elements":[]}}}})
    } else {
        json!({"result":{"result":{"type":"object","value":null}}})
    }
}

#[tokio::test]
async fn type_never_presses_keys_when_the_target_cannot_take_them() {
    // L2: keys go wherever focus is. A target that is disabled, that left
    // focus on another field, or that is gone (while some other editor is
    // focused) must be refused BEFORE any key - the old flow typed a
    // password into the focused email field and called it "the live editor".
    let cases: [(&str, Value, Value); 3] = [
        (
            "disabled",
            json!({"ok":true,"box":[10,10,80,30],"disabled":true,"text":"","hostKind":"input","hostIsTgt":true,"sel":0,"tgtMissing":false}),
            json!({"ok":true,"text":"","hostKind":"input","hostIsTgt":true,"sel":0,"tgtMissing":false}),
        ),
        (
            "input#email \"Email\"",
            json!({"ok":true,"box":[10,10,80,30],"disabled":false,"text":"","hostKind":"input","hostIsTgt":true,"sel":0,"tgtMissing":false,"away":"input#email \"Email\""}),
            json!({"ok":true,"text":"","hostKind":"input","hostIsTgt":true,"sel":0,"tgtMissing":false}),
        ),
        (
            "element not found",
            json!({"ok":false,"reason":"not found"}),
            json!({"ok":true,"text":"","hostKind":"ce","hostIsTgt":false,"sel":0,"tgtMissing":true}),
        ),
    ];
    for (want, prepare, check) in cases {
        let respond: Responder = Box::new(move |method, expression| {
            if method == "Runtime.evaluate" && expression.contains(",\"prepare\",null,") {
                json!({"result":{"result":{"type":"object","value":prepare.clone()}}})
            } else if method == "Runtime.evaluate"
                && expression.starts_with("((sig,mode,text,frame)=>")
            {
                json!({"result":{"result":{"type":"object","value":check.clone()}}})
            } else if method == "Runtime.evaluate" {
                page_basics(expression)
            } else {
                json!({"result":{}})
            }
        });
        let (cdp, log, server) = scripted_page(respond).await;
        let (mut lpm, ref_id) = one_element_model("input", "textbox", "Locked password");
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            bladebro::action::perform(
                &cdp,
                &mut lpm,
                &bladebro::action::Action::Type {
                    ref_id,
                    text: "hunter2".into(),
                },
                &std::sync::Arc::new(std::sync::Mutex::new(None)),
            ),
        )
        .await
        .unwrap();
        let keys = log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "Input.dispatchKeyEvent" || m.as_str() == "Input.insertText")
            .count();
        assert_eq!(keys, 0, "{want}: keys were dispatched; result {result:?}");
        let err = result.expect_err(want).to_string();
        assert!(err.contains(want), "{want}: {err}");
        server.abort();
    }
}

#[tokio::test]
async fn clear_of_a_vanished_target_touches_no_other_field() {
    // L2: the readback may re-adopt a focused unlabelled editor (its sig
    // drifts with its own text). A clear whose target is gone must stop at
    // not-found instead of emptying whatever editor holds focus.
    let respond: Responder = Box::new(|method, expression| {
        if method == "Runtime.evaluate" && expression.contains(",\"state\",null,") {
            json!({"result":{"result":{"type":"object","value":{"ok":false,"reason":"not found"}}}})
        } else if method == "Runtime.evaluate" && expression.starts_with("((sig,mode,text,frame)=>")
        {
            json!({"result":{"result":{"type":"object","value":{"ok":true,"text":"someone else's draft","hostKind":"ce","hostIsTgt":false,"sel":0,"tgtMissing":true}}}})
        } else if method == "Runtime.evaluate" {
            page_basics(expression)
        } else {
            json!({"result":{}})
        }
    });
    let (cdp, log, server) = scripted_page(respond).await;
    let (mut lpm, ref_id) = one_element_model("div", "textbox", "Notes");
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        bladebro::action::perform(
            &cdp,
            &mut lpm,
            &bladebro::action::Action::Clear { ref_id },
            &std::sync::Arc::new(std::sync::Mutex::new(None)),
        ),
    )
    .await
    .unwrap();
    let keys = log
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.as_str() == "Input.dispatchKeyEvent")
        .count();
    assert_eq!(keys, 0, "keys were dispatched; result {result:?}");
    assert!(
        matches!(result, Err(BladeError::ElementNotFound(_))),
        "{result:?}"
    );
    server.abort();
}

#[tokio::test]
async fn select_on_a_text_combobox_types_instead_of_assigning() {
    // L6: `select`/`fill` on an <input role=combobox> used to assign .value
    // from script (untrusted input events, no readback) and report
    // "selected". It now goes through the trusted typing path.
    let respond: Responder = Box::new(|method, expression| {
        if method == "Runtime.evaluate" && expression.contains(",\"prepare\",null,") {
            json!({"result":{"result":{"type":"object","value":{"ok":true,"box":[10,10,80,30],"disabled":false,"text":"","hostKind":"input","hostIsTgt":true,"sel":0,"tgtMissing":false}}}})
        } else if method == "Runtime.evaluate" && expression.starts_with("((sig,mode,text,frame)=>")
        {
            json!({"result":{"result":{"type":"object","value":{"ok":true,"text":"Paris","hostKind":"input","hostIsTgt":true,"sel":0,"tgtMissing":false}}}})
        } else if method == "Runtime.evaluate" {
            page_basics(expression)
        } else {
            json!({"result":{}})
        }
    });
    let (cdp, log, server) = scripted_page(respond).await;
    let (mut lpm, ref_id) = one_element_model("input", "combobox", "City search");
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        bladebro::action::perform(
            &cdp,
            &mut lpm,
            &bladebro::action::Action::Select {
                ref_id,
                option: "Paris".into(),
            },
            &std::sync::Arc::new(std::sync::Mutex::new(None)),
        ),
    )
    .await
    .unwrap();
    let log = log.lock().unwrap().clone();
    assert!(
        !log.iter().any(|m| m == "find:select"),
        "assigned through the select path: {log:?}"
    );
    assert!(
        log.iter().any(|m| m == "Input.dispatchKeyEvent"),
        "no trusted keys: {log:?}"
    );
    let verdict = result.expect("typed").1;
    assert!(verdict.starts_with("outcome: typed \"Paris\""), "{verdict}");
    server.abort();
}
