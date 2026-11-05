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

/// `--json` means ONE object on stdout, also for errors raised before the
/// CLI module owns the command. These used to print only the human line on
/// stderr, leaving an agent's JSON parse an empty read.
#[test]
fn json_errors_before_dispatch_are_one_object_on_stdout() {
    let home = home("json");
    for (args, needle) in [
        (&["nva", "--json"][..], "did you mean `nav`"),
        (
            &["nav", "x", "--port", "abc", "--json"],
            "--port needs a number",
        ),
        (&["--json", "nav", "x", "--host"], "--host needs a value"),
    ] {
        let out = bladebro(&home, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let text = stdout(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{args:?}: exactly one object, got {text:?}");
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["is_error"], true);
        assert!(
            v["text"].as_str().unwrap().contains(needle),
            "{args:?}: {v}"
        );
    }
    // Without --json, stdout stays empty: the human error is on stderr.
    let out = bladebro(&home, &["nva"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));
    let _ = std::fs::remove_dir_all(&home);
}

/// A non-UTF-8 argument (an upload path with a stray byte) panicked in
/// std::env::args() — exit 101, nothing on stdout. It is a usage error.
#[cfg(unix)]
#[test]
fn non_utf8_argument_is_a_usage_error() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let home = home("utf8");
    let bad = OsStr::from_bytes(b"/tmp/bad\xffname.pdf");
    let out = command(&home)
        .args(["act", "upload", "e5"])
        .arg(bad)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(v["ok"], false);
    assert!(
        v["text"].as_str().unwrap().contains("not valid UTF-8"),
        "{v}"
    );
    let out = command(&home)
        .args(["act", "upload", "e5"])
        .arg(bad)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("argument 4 is not valid UTF-8"));
    let _ = std::fs::remove_dir_all(&home);
}

/// A pseudo-terminal pair: (master, slave). None where the host has no pty.
#[cfg(target_os = "linux")]
fn open_pty() -> Option<(std::fs::File, std::fs::File)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return None;
        }
        let master = std::fs::File::from_raw_fd(master);
        let fd = master.as_raw_fd();
        let mut name = [0 as libc::c_char; 128];
        if libc::grantpt(fd) != 0
            || libc::unlockpt(fd) != 0
            || libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) != 0
        {
            return None;
        }
        let path = std::ffi::CStr::from_ptr(name.as_ptr())
            .to_str()
            .ok()?
            .to_owned();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .ok()?;
        Some((master, slave))
    }
}

/// Styling is decided per stream. stderr used to follow STDOUT's terminal
/// state: `bladebro nav x 2>err.log` on a terminal wrote ANSI codes into
/// the log, while a terminal stderr behind `| jq` stayed plain.
#[cfg(target_os = "linux")]
#[test]
fn stderr_styling_follows_stderr_not_stdout() {
    let home = home("color");
    let colorable = |cmd: &mut Command| {
        cmd.env_remove("BLADE_PLAIN")
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR_FORCE")
            .env("TERM", "xterm");
    };

    // stdout on a terminal, stderr into a file: the file stays plain.
    let Some((master, slave)) = open_pty() else {
        eprintln!("no pty on this host; skipping");
        return;
    };
    let mut cmd = command(&home);
    colorable(&mut cmd);
    let out = cmd
        .arg("nva")
        .stdout(slave)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    drop(cmd);
    drop(master);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown command `nva`"), "{err}");
    assert!(
        !err.contains('\x1b'),
        "ANSI written to a non-terminal stderr: {err:?}"
    );

    // stdout into a pipe, stderr on a terminal: the terminal gets styling.
    let (mut master, slave) = open_pty().expect("pty available above");
    let mut cmd = command(&home);
    colorable(&mut cmd);
    let status = cmd
        .arg("nva")
        .stdout(Stdio::piped())
        .stderr(slave)
        .status()
        .unwrap();
    // Close our copy of the slave (the Command owns it): with the child
    // gone too, the read below ends with EIO once the output is drained.
    drop(cmd);
    assert_eq!(status.code(), Some(2));
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut master, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    let err = String::from_utf8_lossy(&seen);
    assert!(err.contains("unknown command `nva`"), "{err:?}");
    assert!(
        err.contains("\x1b[31m"),
        "terminal stderr lost its styling: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
