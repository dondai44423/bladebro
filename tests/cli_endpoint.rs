//! Integration test for issue #16: `state --port` (and every one-shot CLI
//! command) must drive the Chrome endpoint the user explicitly requested,
//! never a hard-coded default.
//!
//! Two DISTINCT mock CDP debug endpoints, each serving a different tab, prove
//! that the discovery layer used by `state`/`see`/`act` (via `run_connected`)
//! targets the exact `host:port` that `--port` selects.

use bladebro::cdp;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a minimal Chrome-style HTTP debug endpoint. It serves `/json` with a
/// single page target whose id/url/ws-url all carry `marker`, so the two
/// endpoints are unambiguously distinguishable. Returns `host:port`.
async fn spawn_mock_endpoint(marker: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = addr.to_string();
    let marker = marker.to_string();
    let ws_url = format!("ws://{addr}/devtools/page/{marker}");

    tokio::spawn(async move {
        // Discovery makes exactly one GET per request; serve it, then close.
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await;
        let req = String::from_utf8_lossy(&buf);
        let path = req.split_whitespace().nth(1).unwrap_or("/");

        let body = if path.starts_with("/json/version") {
            json!({
                "Browser": format!("Chrome/{marker}/version"),
                "Protocol-Version": "1.3",
                "webSocketDebuggerUrl": ws_url,
            })
            .to_string()
        } else {
            json!([{
                "id": format!("page-{marker}"),
                "type": "page",
                "url": format!("https://{marker}.example/"),
                "title": format!("Tab {marker}"),
                "webSocketDebuggerUrl": ws_url,
            }])
            .to_string()
        };

        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.flush().await;
    });

    base
}

#[tokio::test]
async fn discovery_selects_the_requested_endpoint() {
    // Two distinct endpoints, each reporting a different tab.
    let base_a = spawn_mock_endpoint("alpha").await;
    let base_b = spawn_mock_endpoint("beta").await;

    // The exact call chain run_connected uses for state/see/act when --port is
    // given: first_page_target(<base>) must resolve against THAT base.
    let a = cdp::first_page_target(&base_a).await.expect("endpoint A reachable");
    let b = cdp::first_page_target(&base_b).await.expect("endpoint B reachable");

    // Each resolves to its own distinct target.
    assert_eq!(a.id, "page-alpha");
    assert_eq!(a.url, "https://alpha.example/");
    assert_eq!(b.id, "page-beta");
    assert_eq!(b.url, "https://beta.example/");

    // And each WebSocket URL points at its own host:port, not a shared default.
    let wa = a.ws_url().unwrap();
    let wb = b.ws_url().unwrap();
    assert_ne!(wa, wb, "two endpoints must not share a target");
    assert!(wa.contains(&format!("{base_a}/devtools/page/alpha")), "A ws on {wa}");
    assert!(wb.contains(&format!("{base_b}/devtools/page/beta")), "B ws on {wb}");
}