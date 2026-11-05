//! MCP server lifecycle while a request is in flight.
//!
//! The server must end through its graceful teardown (Browser.close flush,
//! login snapshot, knowledge sync) however the session ends — including
//! when the end arrives while a tools/call is still running:
//! 1. A SIGTERM that lands mid-call must not be lost (it used to be: the
//!    signal listeners only existed while the loop sat idle, so the host's
//!    SIGTERM did nothing and the host had to SIGKILL).
//!
//! No Chrome involved: `--port` points the server at a TCP endpoint that
//! accepts and never answers, so the first tools/call sits in target
//! discovery (a 5 s HTTP timeout) and then fails — a request in flight with
//! nothing to launch.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bladebro-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A debug endpoint that accepts connections and never answers. The
/// receiver yields once per accepted connection: proof that the server is
/// inside the tools/call.
fn silent_endpoint() -> (u16, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
            let _ = tx.send(());
        }
    });
    (port, rx)
}

fn spawn_mcp(home: &Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bladebro"))
        .args(["mcp", "--port", &port.to_string()])
        .env("BLADE_HOME", home)
        .env("BLADE_LANE", "agent")
        .env("BLADE_NO_WARMING", "1")
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env_remove("BLADE_TRANSPORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mcp must spawn")
}

/// Send one tools/call and wait (bounded) until the server is inside it.
fn start_call(child: &mut Child, accepted: &mpsc::Receiver<()>) {
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"see","arguments":{"mode":"content"}}}"#;
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{call}").unwrap();
    stdin.flush().unwrap();
    if accepted.recv_timeout(Duration::from_secs(10)).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the tools/call never reached the debug endpoint");
    }
}

fn wait_exit(child: &mut Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn stderr_of(child: &mut Child) -> String {
    let mut err = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut err);
    }
    err
}

#[test]
fn sigterm_during_a_tool_call_is_not_lost() {
    let home = tmp_home("sigterm");
    let (port, accepted) = silent_endpoint();
    let mut child = spawn_mcp(&home, port);
    start_call(&mut child, &accepted);
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    // stdin stays open: only the signal may end this session.
    let status = wait_exit(&mut child, 20);
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let err = stderr_of(&mut child);
    let _ = std::fs::remove_dir_all(&home);
    let status = status.unwrap_or_else(|| {
        panic!("SIGTERM during a tool call was lost: still running 20 s later\n{err}")
    });
    assert!(
        status.success(),
        "signal teardown must exit 0: {status:?}\n{err}"
    );
    assert!(err.contains("termination signal"), "{err}");
}
