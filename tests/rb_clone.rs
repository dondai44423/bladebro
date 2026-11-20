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

#[test]
fn stale_clone_auto_refreshes_when_source_cookies_update() {
    // Reproduces the exact reported bug: user ran `rb on` earlier, then logged
    // into a site (e.g. x.com) in their real browser, then ran `rb on` again.
    // The stale clone used to be reused verbatim, leaving the user logged out.
    let f = Fixture::new();
    let src_default = f.0.join("helium-config/net.imput.helium/Default");
    std::fs::write(src_default.join("Cookies"), "logged-out-cookies").unwrap();

    f.cli(&["on"]);
    assert_eq!(f.cli(&["status"])["template_fresh"], true);
    assert_eq!(
        std::fs::read_to_string(f.rb_root().join("template/Default/Cookies")).unwrap(),
        "logged-out-cookies"
    );

    // User logs into x.com in their real browser (Cookies grows + mtime bumps).
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        src_default.join("Cookies"),
        "logged-in-x-auth-token-and-ct0-cookies",
    )
    .unwrap();
    assert_eq!(
        f.cli(&["status"])["template_fresh"],
        false,
        "status must report stale clone after source Cookies change"
    );

    // Running `rb on` while already on must automatically refresh the clone.
    let on_again = f.cli(&["on"]);
    assert_eq!(on_again["refreshed"], true, "{on_again}");
    assert_eq!(f.cli(&["status"])["template_fresh"], true);
    assert_eq!(
        std::fs::read_to_string(f.rb_root().join("template/Default/Cookies")).unwrap(),
        "logged-in-x-auth-token-and-ct0-cookies"
    );
}

#[test]
fn clone_sanitizes_crashed_preferences_and_skips_sessions_and_component_caches() {
    let f = Fixture::new();
    let src_root = f.0.join("helium-config/net.imput.helium");
    let src_default = src_root.join("Default");
    let crashed_prefs =
        r#"{"profile":{"exit_type":"Crashed","exited_cleanly":false,"name":"Main"}}"#;
    std::fs::write(src_default.join("Preferences"), crashed_prefs).unwrap();
    std::fs::write(src_default.join("Cookies"), "cookie-bytes").unwrap();
    std::fs::create_dir_all(src_default.join("Local Storage/leveldb")).unwrap();
    std::fs::write(src_default.join("Local Storage/leveldb/001.ldb"), "ls").unwrap();
    std::fs::create_dir_all(src_default.join("Extensions/ext1/1.0")).unwrap();
    std::fs::write(src_default.join("Extensions/ext1/1.0/manifest.json"), "{}").unwrap();
    // Session restore dirs (modern Chromium) and huge component caches must be skipped.
    std::fs::create_dir_all(src_default.join("Sessions")).unwrap();
    std::fs::write(src_default.join("Sessions/Session_1"), "old-tabs").unwrap();
    std::fs::create_dir_all(src_default.join("Sessions_Encrypted")).unwrap();
    std::fs::write(src_default.join("Sessions_Encrypted/S1"), "old-tabs").unwrap();
    std::fs::create_dir_all(src_root.join("component_crx_cache")).unwrap();
    std::fs::write(src_root.join("component_crx_cache/big.crx"), "cache").unwrap();
    std::fs::create_dir_all(src_root.join("WasmTtsEngine")).unwrap();
    std::fs::write(src_root.join("WasmTtsEngine/engine.wasm"), "wasm").unwrap();
    std::fs::create_dir_all(src_root.join("Safe Browsing")).unwrap();
    std::fs::write(src_root.join("Safe Browsing/sb.db"), "sb").unwrap();

    f.cli(&["on"]);

    let tpl = f.rb_root().join("template");
    // Source must remain completely untouched.
    assert_eq!(
        std::fs::read_to_string(src_default.join("Preferences")).unwrap(),
        crashed_prefs
    );
    // Cloned Preferences must be sanitized so no "Restore pages?" bubble pops up.
    let cloned_prefs: Value =
        serde_json::from_str(&std::fs::read_to_string(tpl.join("Default/Preferences")).unwrap())
            .unwrap();
    assert_eq!(cloned_prefs["profile"]["exit_type"], "Normal");
    assert_eq!(cloned_prefs["profile"]["exited_cleanly"], true);
    // User auth/storage/extensions preserved.
    assert_eq!(
        std::fs::read_to_string(tpl.join("Default/Cookies")).unwrap(),
        "cookie-bytes"
    );
    assert!(tpl.join("Default/Local Storage/leveldb/001.ldb").is_file());
    assert!(tpl
        .join("Default/Extensions/ext1/1.0/manifest.json")
        .is_file());
    // Session restore dirs & component caches skipped.
    assert!(!tpl.join("Default/Sessions").exists());
    assert!(!tpl.join("Default/Sessions_Encrypted").exists());
    assert!(!tpl.join("component_crx_cache").exists());
    assert!(!tpl.join("WasmTtsEngine").exists());
    assert!(!tpl.join("Safe Browsing").exists());
}

#[test]
fn audio_cli_defaults_muted_and_persists_opt_in_toggle() {
    let f = Fixture::new();
    let run_audio = |args: &[&str], env_audio: Option<&str>| -> Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bladebro"));
        cmd.arg("--json")
            .args(args)
            .env("HOME", f.0.join("home"))
            .env("BLADE_HOME", f.0.join("blade"))
            .env_remove("BLADE_AUDIO");
        if let Some(val) = env_audio {
            cmd.env("BLADE_AUDIO", val);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let init = run_audio(&["audio"], None);
    assert_eq!(init["audio"], false);
    assert_eq!(init["muted"], true);

    let on = run_audio(&["audio", "on"], None);
    assert_eq!(on["audio"], true);
    assert_eq!(on["muted"], false);
    assert_eq!(run_audio(&["audio", "status"], None)["audio"], true);

    // `rb audio off` alias works identically.
    let off = run_audio(&["rb", "audio", "off"], None);
    assert_eq!(off["audio"], false);
    assert_eq!(off["muted"], true);

    // Per-process env override beats persisted muted state.
    let env_on = run_audio(&["audio", "status"], Some("1"));
    assert_eq!(env_on["audio"], true);
    assert_eq!(env_on["persisted"], false);
}
