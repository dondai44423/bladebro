//! Real-lane clone template integrity, through real CLI subprocesses with an
//! isolated HOME/PATH/BLADE_HOME. Fake user-data dirs only — no browser is
//! launched and no real profile is ever read.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("blade-rb-clone-{}-{nanos}", std::process::id()));
        for dir in [
            "home",
            "config",
            "helium-config/net.imput.helium/Default",
            "bin",
            "blade",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let a = root.join("helium-config/net.imput.helium/Default");
        std::fs::write(a.join("Preferences"), "{}").unwrap();
        std::fs::write(a.join("identity"), "A").unwrap();
        std::fs::write(root.join("bin/helium"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            root.join("bin/helium"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let f = Self(root);
        f.cli(&["use", "helium"]);
        f
    }

    fn rb_root(&self) -> PathBuf {
        self.0.join("blade/realbrowser/helium")
    }

    fn identity(&self) -> String {
        std::fs::read_to_string(self.rb_root().join("template/Default/identity")).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bladebro"))
            .args(["--json", "rb"])
            .args(args)
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("HELIUM_CONFIG_HOME", self.0.join("helium-config"))
            .env("BLADE_HOME", self.0.join("blade"))
            .env("PATH", self.0.join("bin"))
            .env_remove("BLADE_LANE")
            .output()
            .unwrap()
    }

    fn cli(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Run a command that must FAIL; returns its JSON error text.
    fn refused(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            !output.status.success(),
            "{args:?} must be refused: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        v["text"].as_str().unwrap_or_default().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn second_root(f: &Fixture) -> PathBuf {
    let b = f.0.join("other-user-data");
    std::fs::create_dir_all(b.join("Default")).unwrap();
    std::fs::write(b.join("Default/Preferences"), "{}").unwrap();
    std::fs::write(b.join("Default/identity"), "B").unwrap();
    b
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn selecting_a_profile_in_another_user_data_root_reimports_the_clone() {
    // The clone template is keyed by browser id only. Pointing `rb profile`
    // at a different user-data root used to keep the OLD root's clone and
    // launch it with the new profile's directory name: the agent browsed as
    // the wrong identity.
    let f = Fixture::new();
    let b = second_root(&f);
    f.cli(&["on"]);
    assert_eq!(f.identity(), "A");

    // Already on: the self-heal import must notice the root change.
    f.cli(&["profile", s(&b.join("Default"))]);
    f.cli(&["on"]);
    assert_eq!(f.identity(), "B", "clone still holds the old root");
    assert_eq!(f.cli(&["status"])["source"], s(&b));

    // Off → on with the original root selected again.
    f.cli(&["profile", "auto"]);
    f.cli(&["off"]);
    f.cli(&["on"]);
    assert_eq!(f.identity(), "A", "clone still holds the second root");

    // Same root, another profile key: the clone already holds every profile
    // of the root, so nothing is re-imported (agent-made state survives).
    std::fs::write(f.rb_root().join("template/Default/agent-state"), "kept").unwrap();
    f.cli(&["on"]);
    assert!(f.rb_root().join("template/Default/agent-state").is_file());
}

#[test]
fn refresh_and_forget_refuse_while_a_clone_session_is_live() {
    // `rb refresh` / `rb forget` only stopped the CLI daemon. A live MCP
    // server's clone session survived: its exit synced the stale copy back
    // over the refreshed template, and forget deleted the root under its
    // running browser. This test process stands in as that live owner.
    let f = Fixture::new();
    f.cli(&["on"]);
    let root = f.rb_root();
    std::fs::write(root.join("template/Default/agent-state"), "kept").unwrap();
    let me = std::process::id();
    let sess = root.join(format!("profiles/sess-{me}"));
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::write(sess.join(".blade-owner"), me.to_string()).unwrap();

    for sub in ["refresh", "forget"] {
        let text = f.refused(&[sub]);
        assert!(text.contains(&format!("pid {me}")), "{sub}: {text}");
        assert!(
            root.join("template/Default/agent-state").is_file(),
            "{sub} touched the clone under a live session"
        );
        assert!(sess.is_dir(), "{sub} removed a live session dir");
    }

    // Once that session is gone both work again.
    std::fs::remove_dir_all(&sess).unwrap();
    assert_eq!(f.cli(&["refresh"])["ok"], true);
    assert!(!root.join("template/Default/agent-state").exists());
    assert_eq!(f.cli(&["forget"])["removed"], true);
    assert!(!root.exists());
}

#[test]
fn refresh_and_forget_abort_when_the_daemon_does_not_stop() {
    // The daemon restart helper swallowed the stop result: a daemon that
    // failed to stop kept its clone session while the template was replaced
    // or wiped under it.
    use std::io::{BufRead, BufReader, Write};
    let f = Fixture::new();
    f.cli(&["on"]);
    let root = f.rb_root();
    std::fs::write(root.join("template/Default/agent-state"), "kept").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(f.0.join("blade/cli.sock")).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) > 0 {
                let _ = stream.write_all(b"{\"ok\":false}\n");
            }
        }
    });

    for sub in ["refresh", "forget"] {
        let text = f.refused(&[sub]);
        assert!(text.contains("daemon"), "{sub}: {text}");
        assert!(
            root.join("template/Default/agent-state").is_file(),
            "{sub} went ahead although the daemon did not stop"
        );
    }
}
