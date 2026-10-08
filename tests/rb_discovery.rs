//! Real CLI subprocesses isolate HOME/PATH from parallel tests. Browser files
//! here exercise discovery only; tools/rb_live/helium.py drives official Helium.
#![cfg(target_os = "linux")]

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("blade-rb-discovery-{}-{nanos}", std::process::id()));
        for dir in [
            "home",
            "config",
            "helium-config/net.imput.helium/Default",
            "bin",
            "shadow/helium",
            "blade",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("helium-config/net.imput.helium/Default/Preferences"),
            "{}",
        )
        .unwrap();
        std::fs::write(root.join("bin/helium"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            root.join("bin/helium"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        Self(root)
    }

    fn cli(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_bladebro"))
            .args(["--json", "rb"])
            .args(args)
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("HELIUM_CONFIG_HOME", self.0.join("helium-config"))
            .env("BLADE_HOME", self.0.join("blade"))
            .env(
                "PATH",
                std::env::join_paths([self.0.join("shadow"), self.0.join("bin")]).unwrap(),
            )
            .env_remove("BLADE_LANE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: invalid JSON ({e}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn helium_discovery_skips_directories_and_honors_its_config_home() {
    let f = Fixture::new();
    let listing = f.cli(&["use"]);
    let helium = listing["browsers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "helium")
        .expect("Helium listed");
    assert_eq!(helium["binary"], f.0.join("bin/helium").to_str().unwrap());
    assert_eq!(
        helium["profile_root"],
        f.0.join("helium-config/net.imput.helium").to_str().unwrap()
    );
    assert_eq!(f.cli(&["use", "helium"])["browser"], "helium");
    let status = f.cli(&["status"]);
    assert_eq!(status["browser"], "helium");
    assert_eq!(status["effective_mode"], "clone");
    let profiles = f.cli(&["profile"]);
    assert_eq!(profiles["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(profiles["profiles"][0]["key"], "Default");
}

#[test]
fn profile_metadata_cannot_select_deleted_profiles_or_escape_the_root() {
    let f = Fixture::new();
    let root = f.0.join("helium-config/net.imput.helium");
    std::fs::create_dir_all(root.join("../outside")).unwrap();
    std::fs::write(root.join("../outside/Preferences"), "{}").unwrap();
    std::os::unix::fs::symlink(root.join("../outside"), root.join("Linked")).unwrap();
    // Local State can describe an active first-run profile before Chrome
    // flushes Preferences; a real directory is enough, a symlink is not.
    std::fs::create_dir(root.join("First run")).unwrap();
    std::fs::write(
        root.join("Local State"),
        json!({"profile":{"info_cache":{
            "Default":{"name":"Main"}, "Deleted":{"name":"Ghost"},
            "Linked":{"name":"Linked escape"}, "First run":{"name":"First run"},
            "../outside":{"name":"Escape"}, "C:\\outside":{"name":"Windows escape"}
        }}})
        .to_string(),
    )
    .unwrap();
    f.cli(&["use", "helium"]);
    let profiles = f.cli(&["profile"]);
    assert_eq!(
        profiles["profiles"].as_array().unwrap().len(),
        2,
        "only live child profiles may be selected: {profiles}"
    );
    assert_eq!(profiles["profiles"][0]["name"], "Main");
    // Persisted selection can outlive a removed profile. Listing and picking
    // a replacement must not require the old key to resolve successfully.
    std::fs::write(
        f.0.join("blade/realbrowser.json"),
        json!({"browser":"helium","profile":"Deleted"}).to_string(),
    )
    .unwrap();
    assert_eq!(f.cli(&["profile"])["profiles"][0]["key"], "Default");
    assert_eq!(f.cli(&["profile", "Default"])["profile"], "Default");
    assert_eq!(f.cli(&["status"])["profile"], "Default");
}

#[test]
fn relative_and_empty_config_homes_do_not_select_cwd_profiles() {
    let f = Fixture::new();
    let root = f.0.join("home/.config/net.imput.helium");
    std::fs::create_dir_all(root.join("Default")).unwrap();
    std::fs::write(root.join("Default/Preferences"), "{}").unwrap();
    for invalid in ["", "relative"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bladebro"))
            .args(["--json", "rb", "use"])
            .current_dir(&f.0)
            .env("HOME", f.0.join("home"))
            .env("BLADE_HOME", f.0.join("blade"))
            .env("XDG_CONFIG_HOME", invalid)
            .env("HELIUM_CONFIG_HOME", invalid)
            .env("PATH", f.0.join("bin"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
        let helium = listing["browsers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "helium")
            .expect("Helium listed");
        assert_eq!(
            helium["profile_root"],
            root.to_str().unwrap(),
            "invalid config home {invalid:?} must use HOME/.config"
        );
    }
}

#[test]
fn forget_refuses_path_ids_without_deleting_outside_its_namespace() {
    let f = Fixture::new();
    let blade = f.0.join("blade");
    std::fs::create_dir(blade.join("realbrowser")).unwrap();
    let victim = blade.join("keep");
    std::fs::create_dir(&victim).unwrap();
    std::fs::write(victim.join("sentinel"), "must remain").unwrap();
    for id in ["../keep", "", "..", "C:\\keep", victim.to_str().unwrap()] {
        std::fs::write(
            blade.join("realbrowser.json"),
            json!({"browser": id}).to_string(),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bladebro"))
            .args(["--json", "rb", "forget"])
            .env("BLADE_HOME", &blade)
            .output()
            .unwrap();
        assert!(!output.status.success(), "unsafe id {id:?} was accepted");
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            error["text"]
                .as_str()
                .unwrap()
                .contains("invalid browser id"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(victim.join("sentinel")).unwrap(),
            "must remain"
        );
    }
    // The guard must not break actual clone removal.
    std::fs::create_dir(blade.join("realbrowser/helium")).unwrap();
    std::fs::write(blade.join("realbrowser.json"), r#"{"browser":"helium"}"#).unwrap();
    assert_eq!(f.cli(&["forget"])["removed"], true);
    assert!(!blade.join("realbrowser/helium").exists());
    assert!(victim.join("sentinel").is_file());
}
