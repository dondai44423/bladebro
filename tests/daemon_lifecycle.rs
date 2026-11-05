//! Daemon lifecycle integration tests (v3.9.10).
//!
//! Two regressions guarded here, both found by the install-surface audit:
//! 1. A second `bladebro daemon` must NOT steal a live daemon's socket —
//!    before, it rebound over the active daemon and orphaned it.
//! 2. A daemon's shutdown must only remove the socket + pidfile while it
//!    still owns them — before, a stale ("ghost") daemon dying unlinked the
//!    ACTIVE daemon's socket, so the next command spawned yet another daemon.
//!
//! The client side of the daemon channel is guarded too: a delivered command
//! is never re-run, a live daemon is never declared dead from its reply text,
//! a command never runs under a daemon whose lane/settings differ from its
//! own, and an oversized request fails with a message instead of a bare
//! "Broken pipe" error.
//!
//! Plus the busy daemon: a SIGTERM that lands while a request is in flight
//! must not be lost.
//!
//! Subprocess-based with an isolated BLADE_HOME — no Chrome involved (the
//! daemon binds lazily; Chrome starts on the first tool call only). Tests
//! that send tool calls pin `BLADE_LANE=agent` and a missing `CHROME_PATH`,
//! so a launch fails fast and the real-browser lane is never entered; the
//! one test that needs a launch in flight uses a stand-in browser script.

#![cfg(unix)]

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_bladebro")
}

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bladebro-daemon-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn `bladebro daemon` and wait (bounded) until it bound its socket
/// AND wrote its pidfile.
///
/// The socket appears before the pidfile write, so tests that inspect or
/// rewrite either file must wait for both. Simulating a takeover inside
/// that startup window lets the daemon's own pidfile write clobber the
/// test's overwrite — a race macOS CI hit once (ghost teardown, 2026-10),
/// never a daemon defect.
fn start_daemon(home: &Path) -> Child {
    start_daemon_with(home, Command::new(bin()))
}

/// [`start_daemon`] with a caller-prepared command (extra env).
fn start_daemon_with(home: &Path, mut cmd: Command) -> Child {
    let mut child = cmd
        .arg("daemon")
        .env("BLADE_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon must spawn");
    let sock = home.join("cli.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sock.exists() {
        if let Ok(Some(st)) = child.try_wait() {
            panic!("daemon exited early: {st:?}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon never bound {}",
            sock.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let pidfile = home.join("cli.pid");
    let own = child.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&pidfile).unwrap_or_default().trim() != own {
        if let Ok(Some(st)) = child.try_wait() {
            panic!("daemon exited early: {st:?}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon never wrote its pidfile at {}",
            pidfile.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    child
}

fn term(child: &Child) {
    // SIGTERM via kill(1) — std has no kill.
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
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

#[test]
fn second_daemon_does_not_steal_a_live_socket() {
    let home = tmp_home("steal");
    let mut d1 = start_daemon(&home);
    let sock = home.join("cli.sock");
    let pidfile = home.join("cli.pid");

    // Second start must refuse quickly (exit 0, "already running") — not take over.
    let mut d2 = Command::new(bin())
        .arg("daemon")
        .env("BLADE_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("second daemon must spawn");
    let st = wait_exit(&mut d2, 10).unwrap_or_else(|| {
        term(&d2);
        panic!("second daemon did not exit — it stole the socket");
    });
    assert!(st.success(), "second daemon exit: {st:?}");
    let mut err = String::new();
    d2.stderr.take().unwrap().read_to_string(&mut err).ok();
    assert!(
        err.contains("already running"),
        "second daemon must say why: {err:?}"
    );

    // The first daemon still owns the socket + pidfile.
    assert!(sock.exists(), "socket must survive the second start");
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, d1.id(), "pidfile must still belong to daemon 1");

    // Normal teardown: the owner removes its files.
    term(&d1);
    let st = wait_exit(&mut d1, 10).expect("daemon 1 must exit on SIGTERM");
    assert!(st.success(), "daemon 1 exit: {st:?}");
    assert!(!sock.exists(), "owner teardown removes the socket");
    assert!(!pidfile.exists(), "owner teardown removes the pidfile");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ghost_teardown_leaves_a_foreign_socket_alone() {
    let home = tmp_home("ghost");
    let mut d1 = start_daemon(&home);
    let sock = home.join("cli.sock");
    let pidfile = home.join("cli.pid");

    // Simulate a takeover: the pidfile now belongs to someone else (a
    // "newer daemon"). This daemon is a ghost — its death must NOT unlink
    // files it does not own, or it would orphan the live daemon.
    std::fs::write(&pidfile, "1").unwrap();

    term(&d1);
    let st = wait_exit(&mut d1, 10).expect("ghost must exit on SIGTERM");
    assert!(st.success(), "ghost exit: {st:?}");
    assert!(
        sock.exists(),
        "ghost must not unlink a socket it does not own"
    );
    assert_eq!(std::fs::read_to_string(&pidfile).unwrap().trim(), "1");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn partial_request_cannot_delay_termination() {
    use std::io::Write;
    let home = tmp_home("partial");
    let mut daemon = start_daemon(&home);
    let mut stream = std::os::unix::net::UnixStream::connect(home.join("cli.sock")).unwrap();
    stream.write_all(b"{\"tool\":").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    term(&daemon);
    let status = wait_exit(&mut daemon, 2).unwrap_or_else(|| {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!("partial request blocked SIGTERM");
    });
    assert!(status.success());
    assert!(!home.join("cli.sock").exists());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn concurrent_cold_starts_have_one_owner() {
    let home = tmp_home("race");
    let mut children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(bin())
                .arg("daemon")
                .env("BLADE_HOME", &home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    let owner: u32 = std::fs::read_to_string(home.join("cli.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let survivors: Vec<_> = children
        .iter_mut()
        .filter_map(|child| child.try_wait().unwrap().is_none().then_some(child.id()))
        .collect();
    // Always clean our children, including on a failed ownership assertion.
    for child in &mut children {
        if child.try_wait().unwrap().is_none() {
            term(child);
        }
        if wait_exit(child, 2).is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    assert_eq!(
        survivors,
        vec![owner],
        "more than one cold-start daemon remained alive"
    );
    assert!(!home.join("cli.sock").exists());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn stop_json_is_one_object_and_lane_restart_emits_no_stray_stdout() {
    let home = tmp_home("json");
    let mut daemon = start_daemon(&home);
    let output = Command::new(bin())
        .args(["--json", "stop"])
        .env("BLADE_HOME", &home)
        .output()
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stop stdout must be one JSON object");
    assert!(output.status.success());
    assert_eq!(value["ok"], true);
    assert_eq!(value["text"], "daemon stopped");
    assert!(wait_exit(&mut daemon, 2).unwrap().success());
    std::fs::write(home.join("realbrowser.json"), br#"{"enabled":true}"#).unwrap();
    let mut daemon = start_daemon(&home);
    let output = Command::new(bin())
        .args(["--json", "rb", "off"])
        .env("BLADE_HOME", &home)
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("lane restart must not print a plaintext stop line");
    assert!(output.status.success());
    assert_eq!(value["ok"], true);
    assert!(wait_exit(&mut daemon, 2).unwrap().success());
    std::fs::remove_dir_all(home).unwrap();
}

// ── Client side of the daemon channel ──────────────────────────────────

/// A `bladebro` process (client or daemon) pinned to the agent lane with a
/// Chrome binary that does not exist: a tool call that reaches a launch
/// fails fast, nothing is ever opened, and the real-browser lane (the
/// user's own browser) is never entered whatever the code under test does.
fn agent_cmd(home: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("BLADE_HOME", home)
        .env("BLADE_LANE", "agent")
        .env("CHROME_PATH", home.join("missing-chrome"))
        .env("BLADE_NO_WARMING", "1")
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null());
    cmd
}

/// Run `bladebro --json <args>` as a client; returns (exit status, the one
/// stdout JSON object, stderr).
fn client_json(
    home: &Path,
    envs: &[(&str, &str)],
    args: &[&str],
) -> (std::process::ExitStatus, serde_json::Value, String) {
    let mut cmd = agent_cmd(home);
    cmd.arg("--json").args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("client must run");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout must be one JSON object ({e}); status {:?}\nstdout: {stdout:?}\nstderr: {stderr}", out.status)
    });
    (out.status, value, stderr)
}

/// A stand-in for a LIVE daemon: holds the daemon lock (as a running
/// daemon does — so an auto-started real daemon bows out "already
/// running") and answers every request with `reply`, recording each
/// request line. The connect-only liveness probes write nothing and are
/// not recorded.
struct FakeDaemon {
    requests: Arc<Mutex<Vec<String>>>,
    _lock: std::fs::File,
}

fn fake_daemon(home: &Path, reply: &'static str) -> FakeDaemon {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("cli.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "the fake daemon must own the daemon lock"
    );
    let listener = std::os::unix::net::UnixListener::bind(home.join("cli.sock")).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut line = String::new();
            let _ = std::io::BufReader::new(&stream).read_line(&mut line);
            if line.trim().is_empty() {
                continue;
            }
            seen.lock().unwrap().push(line);
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
        }
    });
    FakeDaemon {
        requests,
        _lock: lock,
    }
}

/// Stop whatever daemon a test's client auto-started (it is detached into
/// its own session, so it would otherwise outlive the test).
fn stop_daemon(home: &Path) {
    let _ = agent_cmd(home)
        .arg("stop")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[test]
fn daemon_error_text_never_marks_a_live_daemon_unreachable() {
    // A live daemon's tool error can carry an OS error string — here a
    // Chrome spawn ENOENT. The CLI used to grep reply text for "No such
    // file", decide the daemon was dead, unlink the LIVE daemon's socket and
    // auto-start a second daemon (which lost the lock and exited), orphaning
    // the first one and falling back to a one-shot re-run.
    let home = tmp_home("reply-text");
    let fake = fake_daemon(
        &home,
        r#"{"ok":false,"error":"Chrome launch failed: failed to launch Chrome: No such file or directory (os error 2)"}"#,
    );
    let (status, value, stderr) = client_json(&home, &[], &["nav", "http://127.0.0.1:9/"]);
    let sock_alive = home.join("cli.sock").exists();
    let requests = fake.requests.lock().unwrap().len();
    stop_daemon(&home);
    assert!(
        sock_alive,
        "the live daemon's socket was unlinked; stderr: {stderr}"
    );
    assert_eq!(requests, 1, "the command must be delivered exactly once");
    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    assert_eq!(value["ok"], false);
    assert!(
        value["text"]
            .as_str()
            .unwrap()
            .contains("No such file or directory"),
        "the daemon's own error must be surfaced: {value}"
    );
    assert!(
        !stderr.contains("restarting") && !stderr.contains("one-shot"),
        "a delivered command must not trigger a restart or re-run: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn auto_started_daemon_failure_is_not_rerun_one_shot() {
    // The request reached the auto-started daemon and failed there. Any
    // such failure — "outcome UNKNOWN", a client timeout while the daemon
    // still works — used to be followed by a second run of the same command
    // in a one-shot browser: a click could land twice.
    let home = tmp_home("rerun");
    let (status, value, stderr) = client_json(&home, &[], &["nav", "http://127.0.0.1:9/"]);
    stop_daemon(&home);
    assert!(
        !stderr.contains("falling back to one-shot"),
        "a delivered command must never be re-run one-shot: {stderr}"
    );
    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    assert!(
        value["text"]
            .as_str()
            .unwrap()
            .contains("Chrome launch failed"),
        "the daemon's own error must be surfaced: {value}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn daemon_closing_without_a_reply_reports_an_unknown_outcome() {
    // A daemon that dies mid-command (crash, OOM kill) closes the socket
    // without replying. That surfaced as "invalid daemon response: EOF
    // while parsing…" — or, right after an auto-start, as a one-shot re-run
    // of the command. The agent must be told the command may have run.
    let home = tmp_home("eof");
    let fake = fake_daemon(&home, "");
    let (status, value, stderr) = client_json(&home, &[], &["nav", "http://127.0.0.1:9/"]);
    let requests = fake.requests.lock().unwrap().len();
    assert_eq!(requests, 1, "stderr: {stderr}");
    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    let text = value["text"].as_str().unwrap();
    assert!(
        text.contains("may have run") && text.contains("before repeating"),
        "the error must say the outcome is unknown: {text}"
    );
    assert!(!stderr.contains("one-shot"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn cli_request_carries_its_lane_and_explicit_settings() {
    // The daemon's lane and settings came only from ITS environment —
    // inherited from whichever command auto-started it — so
    // `BLADE_LANE=agent bladebro nav …` against a real-lane daemon ran in
    // the user's own browser. The request must say what this command asked
    // for.
    let home = tmp_home("carries");
    let fake = fake_daemon(&home, r#"{"ok":true,"text":"ok","is_error":false}"#);
    let (status, _, stderr) =
        client_json(&home, &[("BLADE_NO_COMPRESS", "1")], &["see", "content"]);
    let requests = fake.requests.lock().unwrap().clone();
    assert!(status.success(), "stderr: {stderr}");
    assert_eq!(requests.len(), 1);
    let req: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    assert_eq!(req["tool"], "see");
    assert_eq!(req["lane"], "agent", "request: {req}");
    assert_eq!(req["env"]["BLADE_NO_COMPRESS"], "1", "request: {req}");
    assert!(
        req["env"].get("BLADE_HOME").is_none(),
        "BLADE_HOME picks the daemon itself; it is not a per-command setting: {req}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn daemon_refuses_a_request_from_another_lane() {
    // Daemon pinned to the agent lane; a request that resolved the real
    // lane must be refused before anything launches (the old daemon ignored
    // the field and launched its own lane). The real lane is never entered:
    // the refusal is what is under test, and the daemon is agent-pinned.
    let home = tmp_home("lane");
    let mut daemon = start_daemon_with(&home, agent_cmd(&home));
    let mut stream = std::os::unix::net::UnixStream::connect(home.join("cli.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(b"{\"tool\":\"see\",\"args\":{\"mode\":\"content\"},\"lane\":\"real\"}\n")
        .unwrap();
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    term(&daemon);
    let _ = wait_exit(&mut daemon, 10);
    let v: serde_json::Value = serde_json::from_str(resp.trim())
        .unwrap_or_else(|e| panic!("daemon reply must be JSON ({e}): {resp:?}"));
    assert_eq!(v["ok"], false, "{v}");
    let err = v["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("lane") && err.contains("real") && err.contains("agent"),
        "the refusal must name both lanes: {err}"
    );
    assert!(
        !err.contains("Chrome launch failed"),
        "a refused request must not launch anything: {err}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn daemon_refuses_a_command_whose_settings_differ() {
    // End to end: the daemon was started without BLADE_NO_COMPRESS; a
    // command that sets it would have been silently served with the
    // daemon's settings. It must be refused, naming the variable and the
    // recovery, without running.
    let home = tmp_home("settings");
    let mut daemon = start_daemon_with(&home, agent_cmd(&home));
    let (status, value, stderr) =
        client_json(&home, &[("BLADE_NO_COMPRESS", "1")], &["see", "content"]);
    term(&daemon);
    let _ = wait_exit(&mut daemon, 10);
    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    let text = value["text"].as_str().unwrap();
    assert!(
        text.contains("BLADE_NO_COMPRESS") && text.contains("bladebro stop"),
        "the refusal must name the setting and the recovery: {text}"
    );
    assert!(
        !text.contains("Chrome launch failed"),
        "a refused command must not run: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn oversized_request_fails_with_an_actionable_message() {
    // The daemon drops a request over 8 MiB mid-read. The CLI then failed
    // on its socket write with a bare "I/O error: Broken pipe (os error
    // 32)" — no size, no limit, no way forward (and with no daemon running
    // it auto-started one first, for nothing). Never by signal: stdout must
    // still carry the one JSON object.
    use std::os::unix::process::ExitStatusExt;
    let home = tmp_home("oversize");
    let mut daemon = start_daemon_with(&home, agent_cmd(&home));
    let payload = home.join("steps.json");
    let pad = "x".repeat(9 * 1024 * 1024);
    std::fs::write(
        &payload,
        format!("[{{\"action\":\"see\",\"pad\":\"{pad}\"}}]"),
    )
    .unwrap();
    let mut cmd = agent_cmd(&home);
    let out = cmd
        .args(["--json", "run"])
        .arg(format!("@{}", payload.display()))
        .output()
        .unwrap();
    let alive = daemon.try_wait().unwrap().is_none();
    term(&daemon);
    let _ = wait_exit(&mut daemon, 10);
    assert_eq!(
        out.status.signal(),
        None,
        "the CLI was killed by a signal: {:?}",
        out.status
    );
    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout must be one JSON object");
    let text = value["text"].as_str().unwrap();
    assert!(
        text.contains("8 MiB") && text.contains("--no-daemon"),
        "the error must say why and how to recover: {text}"
    );
    assert!(alive, "the daemon must survive an oversized request");
    let _ = std::fs::remove_dir_all(&home);
}

/// A stand-in browser: touches `started`, then exits during "startup"
/// without ever opening a debug endpoint. The daemon's launch polls it,
/// retries twice and reports a launch failure — seconds of a request in
/// flight with no real Chrome.
fn fake_chrome(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let started = dir.join("started");
    let script = dir.join("fake-chrome");
    std::fs::write(
        &script,
        format!("#!/bin/sh\n: > '{}'\nsleep 1\nexit 1\n", started.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    (script, started)
}

#[test]
fn sigterm_during_a_request_is_not_lost() {
    use std::io::Write;
    let home = tmp_home("busy-signal");
    let tools = tmp_home("busy-signal-bin");
    let (chrome, started) = fake_chrome(&tools);
    let mut cmd = Command::new(bin());
    cmd.env("CHROME_PATH", &chrome)
        .env("BLADE_LANE", "agent")
        .env("BLADE_INCOGNITO", "0")
        .env("BLADE_NO_WARMING", "1")
        .env("BLADE_NO_UPDATE_CHECK", "1");
    let mut daemon = start_daemon_with(&home, cmd);
    let sock = home.join("cli.sock");
    let client = std::thread::spawn(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(sock).unwrap();
        stream
            .write_all(b"{\"tool\":\"see\",\"args\":{\"mode\":\"content\"}}\n")
            .unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply
    });
    // The request is now in flight: the daemon is launching its browser.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("the request never reached the browser launch");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    term(&daemon);
    let status = wait_exit(&mut daemon, 30);
    if status.is_none() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    let reply = client.join().unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&tools);
    let status =
        status.expect("SIGTERM during a request was lost: daemon still running 30 s later");
    assert!(status.success(), "daemon exit: {status:?}");
    assert!(
        reply.contains("\"ok\":false"),
        "the in-flight request still gets its answer: {reply:?}"
    );
}
