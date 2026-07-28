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

use bladebro::cdp::CdpClient;
use bladebro::BladeError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

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
        sink.send(Message::Text(ev.to_string().into())).await.unwrap();

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
            sink.send(Message::Text(resp.to_string().into())).await.unwrap();
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
            sink.send(Message::Text(resp.to_string().into())).await.unwrap();
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
