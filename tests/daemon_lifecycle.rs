//! Daemon lifecycle integration tests (v3.9.10).
//!
//! Two regressions guarded here, both found by the install-surface audit:
//! 1. A second `bladebro daemon` must NOT steal a live daemon's socket —
//!    before, it rebound over the active daemon and orphaned it.
//! 2. A daemon's shutdown must only remove the socket + pidfile while it
//!    still owns them — before, a stale ("ghost") daemon dying unlinked the
//!    ACTIVE daemon's socket, so the next command spawned yet another daemon.
//!
//! Subprocess-based with an isolated BLADE_HOME — no Chrome involved (the
//! daemon binds lazily; Chrome starts on the first tool call only).

#![cfg(unix)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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

/// Spawn `bladebro daemon` and wait (bounded) until it bound its socket.
fn start_daemon(home: &PathBuf) -> Child {
    let mut child = Command::new(bin())
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
        assert!(Instant::now() < deadline, "daemon never bound {}", sock.display());
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
