//! CLI surface contracts that only show at the process boundary: where
//! `-h`/`--help` is routed, and what reaches stdout/stderr before the CLI
//! module owns a command.
//!
//! Subprocess-based with an isolated BLADE_HOME. None of these paths may
//! launch Chrome; CHROME_PATH points at a missing binary so a regression
//! fails fast instead of starting a browser.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn home(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bladebro-cli-surface-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn command(home: &PathBuf) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bladebro"));
    cmd.env("BLADE_HOME", home)
        .env("BLADE_LANE", "agent")
        .env("BLADE_NO_UPDATE_CHECK", "1")
        .env("BLADE_NO_WARMING", "1")
        .env("BLADE_IDLE_TIMEOUT", "5")
        .env("BLADE_PLAIN", "1")
        .env("CHROME_PATH", home.join("no-chrome-here"))
        .stdin(Stdio::null());
    cmd
}

fn bladebro(home: &PathBuf, args: &[&str]) -> Output {
    command(home).args(args).output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A `--help` after a command is that command's help; before one it is the
/// global manual. main.rs used to treat it as global ANYWHERE, so
/// `act --help` printed the top-level manual, `act type e3 hello --help`
/// printed it and exited 0 without typing, and `--help --json` was prose.
#[test]
fn help_is_routed_to_the_named_command() {
    let home = home("help");
    let manual = "COMMANDS";
    let act = "bladebro act — interact with the page";
    for args in [
        &["act", "--help"][..],
        &["act", "-h"],
        &["act", "type", "e3", "hello", "--help"],
        &["help", "act"],
        &["--help", "act"],
    ] {
        let out = bladebro(&home, args);
        let text = stdout(&out);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(
            text.contains(act) && !text.contains(manual),
            "{args:?} must print act's help, got: {text}"
        );
    }

    let out = bladebro(&home, &["act", "--help", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).expect("--help --json is one JSON object");
    assert_eq!(v["command"], "act");
    assert_eq!(v["tool_definition"]["name"], "act");

    // Commands main.rs owns answer with their help — the handler must not
    // run (`mcp --help` would start a server, `-u --help` would update).
    for (args, marker) in [
        (&["mcp", "--help"][..], "bladebro mcp — MCP server"),
        (&["-u", "--help"], "bladebro -u / update — self-update"),
        (&["audit", "-h"], "bladebro audit — stealth audit"),
    ] {
        let out = bladebro(&home, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(stdout(&out).contains(marker), "{args:?}: {}", stdout(&out));
    }

    // Before any command, and for `help` itself, it stays the manual.
    for args in [&["--help"][..], &["-h"], &["help", "--help"]] {
        let out = bladebro(&home, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(stdout(&out).contains(manual), "{args:?}: {}", stdout(&out));
    }
    let out = bladebro(&home, &["--help", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).expect("--help --json is one JSON object");
    assert!(v["commands"]["act"].is_object());

    // An unknown command is not a help request for the manual.
    let out = bladebro(&home, &["nva", "--help"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let _ = std::fs::remove_dir_all(&home);
}
