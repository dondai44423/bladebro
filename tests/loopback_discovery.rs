//! The DevTools HTTP endpoint is always on the local machine (or reached
//! directly, like the WebSocket that follows discovery): an ambient
//! `HTTP_PROXY`/`ALL_PROXY` must never capture `/json/version` or `/json`.
//! The 4.6.7 clients honored those variables with no loopback bypass, so a
//! shell with a proxy configured and an empty `NO_PROXY` failed every launch
//! with "Chrome debug endpoint not responding after 20s" while Chrome was up.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Answer each connection with `respond(request_line)`, counting hits.
fn serve(respond: fn(&str) -> String) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let line = head.lines().next().unwrap_or("").to_string();
            let _ = stream.write_all(respond(&line).as_bytes());
        }
    });
    (port, hits)
}

fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn devtools_discovery_bypasses_environment_proxies() {
    let (devtools, devtools_hits) = serve(|line| {
        if line.starts_with("GET /json/version ") {
            http(
                "200 OK",
                r#"{"Browser":"FakeChrome/1.0","Protocol-Version":"1.3"}"#,
            )
        } else if line.starts_with("GET /json ") {
            http("200 OK", "[]")
        } else {
            http("404 Not Found", "{}")
        }
    });
    // A proxy that is up but refuses everything: a captured request fails
    // fast (502) instead of hanging the test on a timeout.
    let (proxy, proxy_hits) = serve(|_| http("502 Bad Gateway", ""));
    let proxy_url = format!("http://127.0.0.1:{proxy}");
    let home = std::env::temp_dir().join(format!("blade-loopback-proxy-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bladebro"))
        .args(["probe", "--port", &devtools.to_string()])
        .env("BLADE_HOME", &home)
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "probe failed behind an env proxy:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("FakeChrome/1.0"), "{stdout}");
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "a DevTools discovery request went through the env proxy"
    );
    assert!(devtools_hits.load(Ordering::SeqCst) >= 2, "{stdout}");
}
