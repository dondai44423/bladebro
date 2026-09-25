//! First-class CLI with the same power as the MCP server.
//!
//! Architecture: the CLI calls the exact same handler functions as the
//! MCP server (`handle_act`, `handle_see`, `handle_state`, `handle_run`,
//! `handle_vision`). Any change to a handler auto-propagates to both
//! surfaces — zero maintenance overhead.
//!
//! Two modes:
//! - **Daemon**: `bladebro daemon` starts a persistent Chrome session.
//!   Subsequent `bladebro` commands connect to it via Unix socket.
//!   Same lifecycle as MCP (self-healing, idle timeout, reaper).
//! - **One-shot**: if no daemon is running, each command launches Chrome,
//!   runs, and exits.
//!
//! Agent contract (v3.9.7):
//! - `bladebro help [--json]` is the single self-teaching surface: the JSON
//!   form carries the same tool schemas as MCP `tools/list` plus CLI-only
//!   details (usage, examples, exit codes, stdin/@file payloads).
//! - `--json` on any command prints ONE machine-readable object on stdout:
//!   `{"ok", "is_error", "text"}` (+ `"image_path"` for vision).
//! - Exit codes: 0 ok, 1 command ran but failed, 2 usage error.
//! - Flags mirror the MCP schema fields 1:1 (`--ref`, `--label`, `--text`,
//!   …) and are accepted anywhere; unknown flags are loud errors, never
//!   silently dropped.


use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;
use crate::mcp::server;

/// Unix socket path for the CLI daemon.
#[cfg(unix)]
fn socket_path() -> std::path::PathBuf {
    crate::platform::blade_dir().join("cli.sock")
}

/// Result of a tool dispatch — shared between CLI and daemon.
pub struct ToolResult {
    pub text: String,
    pub image: Option<String>,
    pub is_error: bool,
}

/// Dispatch a tool call to the same handlers the MCP server uses.
/// This is the shared core — any handler update auto-propagates to CLI.
pub async fn dispatch(tool: &str, args: &Value, page: &mut Page) -> std::result::Result<ToolResult, BladeError> {
    // For see with URL: navigate first, then read. Any non-flag token is a
    // URL; the shared navigate adds the missing scheme.
    if tool == "see" {
        if let Some(url) = args.get("url").and_then(|u| u.as_str()) {
            if !url.is_empty() && !url.starts_with("--") {
                page.navigate(url).await?;
            }
        }
    }

    // Vision is special — returns a JSON-RPC response with image data.
    if tool == "vision" {
        let result = server::handle_vision(None, args, page).await?;
        let content = result
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array());
        let text = content
            .and_then(|c| c.first())
            .and_then(|t| t.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let is_error = result
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        let image = content
            .and_then(|c| c.get(1))
            .and_then(|i| i.get("data"))
            .and_then(|d| d.as_str())
            .map(String::from);
        return Ok(ToolResult { text: text.to_string(), image, is_error });
    }

    let result = match tool {
        "act" => server::handle_act(args, page).await,
        "see" => server::handle_see(args, page).await,
        "state" => server::handle_state(args, page).await,
        "run" => server::handle_run(args, page).await,
        _ => return Err(BladeError::Other(format!("unknown tool: {tool}"))),
    };

    match result {
        Ok(mut text) => {
            // Drain dialogs and ambient events (same as MCP handle_tools_call).
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\u{26a0} dialogs auto-dismissed:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    text.push_str(&format!("  {} \"{}\" \u{2014} {}\n", d.kind, d.message, action));
                }
            }
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\u{26a0} {}\n", a));
            }
            Ok(ToolResult { text, image: None, is_error: false })
        }
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => {
            let mut text = format!("\u{2717} error: {e}");
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\n\u{26a0} dialogs auto-dismissed:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    text.push_str(&format!("  {} \"{}\" \u{2014} {}\n", d.kind, d.message, action));
                }
            }
            Ok(ToolResult { text, image: None, is_error: true })
        }
    }
}

/// Auto-start the daemon as a detached background process.
/// The first CLI command triggers this — the agent never needs to
/// run `bladebro daemon` explicitly.
///
/// SECURITY: spawns the binary DIRECTLY (no `sh -c`). The old shell path
/// interpolated the exe path into single quotes — an install path
/// containing a quote broke out of the quoting.
#[cfg(unix)]
fn auto_start_daemon() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find bladebro binary: {e}")))?;
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        // Detach into a new session so the daemon survives the parent
        // CLI exiting (what nohup used to do).
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| BladeError::Other(format!("failed to start daemon: {e}")))?;
    Ok(())
}

#[cfg(windows)]
#[allow(dead_code)]
fn auto_start_daemon() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| BladeError::Other(format!("cannot find bladebro binary: {e}")))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: survive parent exit.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    cmd.spawn()
        .map_err(|e| BladeError::Other(format!("failed to start daemon: {e}")))?;
    Ok(())
}

/// Ignore SIGHUP — the daemon must survive the parent CLI exiting.
/// Called at daemon startup.
#[cfg(unix)]
fn ignore_sighup() {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

/// Check if the daemon is running by trying to connect to the socket.
#[cfg(unix)]
fn daemon_running() -> bool {
    let path = socket_path();
    if !path.exists() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        UnixStream::connect(&path).is_ok()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Send a tool call to the daemon over Unix socket.
///
/// Hardening: the read has a timeout (`BLADE_CMD_TIMEOUT` seconds, default
/// 300). Before this, a wedged daemon left the client blocking forever —
/// an agent's shell call hung until its harness killed it, with no output
/// and no way to tell "still working" from "dead".
#[cfg(unix)]
fn send_to_daemon(tool: &str, args: &Value) -> Result<ToolResult> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let timeout_secs: u64 = std::env::var("BLADE_CMD_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(300);

    let mut stream = UnixStream::connect(socket_path())
        .map_err(|e| BladeError::Other(format!("daemon not running: {e}")))?;

    let req = serde_json::to_string(&json!({ "tool": tool, "args": args }))?;
    writeln!(stream, "{req}")?;
    stream.flush()?;

    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(timeout_secs)));

    let mut resp = String::new();
    match stream.read_to_string(&mut resp) {
        Ok(_) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            return Err(BladeError::Other(format!(
                "no daemon response after {timeout_secs}s — the command may still be running.\n\
                 Check with 'bladebro state tabs', cancel with 'bladebro stop', or raise the\n\
                 client limit with BLADE_CMD_TIMEOUT=<seconds>."
            )));
        }
        Err(e) => {
            return Err(BladeError::Other(format!("failed to read daemon response: {e}")));
        }
    }

    let v: Value = serde_json::from_str(&resp)
        .map_err(|e| BladeError::Other(format!("invalid daemon response: {e}")))?;

    let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
    if !ok {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error");
        return Err(BladeError::Other(err.to_string()));
    }

    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let image = v.get("image").and_then(|i| i.as_str()).map(String::from);
    let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    Ok(ToolResult { text, image, is_error })
}

/// Main CLI entry point. Called from main.rs.
///
/// Exit-code contract (documented in `help`):
/// - 0: the command ran successfully
/// - 1: the command ran but failed (tool error, daemon failure) — details
///   are in the output text
/// - 2: usage error (unknown command/flag/argument) — fix the command
pub async fn run_cli(args: &[String]) -> Result<()> {
    let json_mode = args.iter().any(|a| a == "--json");
    let no_daemon = args.iter().any(|a| a == "--no-daemon");
    // --host / --port override the browser endpoint: drive an already-running
    // Chrome instead of the local daemon / a freshly launched one. main.rs
    // parses these globally and (since issue #16's fix) re-injects them here.
    let (args, external) = extract_endpoint(args);
    let args: Vec<String> = args
        .iter()
        .filter(|a| a != &"--json" && a != &"--no-daemon")
        .cloned()
        .collect();

    match run_cli_inner(&args, json_mode, no_daemon, external).await {
        Ok(()) => Ok(()),
        // Machine-readable failures also land on stdout: an agent parsing
        // --json output must never get an empty read on a failed command.
        Err(e) => {
            if json_mode {
                println!(
                    "{}",
                    json!({ "ok": false, "is_error": true, "text": e.to_string() })
                );
            }
            Err(e)
        }
    }
}

async fn run_cli_inner(
    args: &[String],
    json_mode: bool,
    no_daemon: bool,
    external: Option<String>,
) -> Result<()> {
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    let rest = &args[1.min(args.len())..];

    // `--help`/`-h` anywhere in a command's args → that command's help.
    if cmd != "help" && rest.iter().any(|a| a == "--help" || a == "-h") {
        if let Some(text) = command_help_text(cmd) {
            print!("{text}");
            return Ok(());
        }
    }

    match cmd {
        "daemon" => run_daemon().await,
        "stop" => stop_daemon().await,
        "nav" => {
            let parsed = parse_nav_args(rest)?;
            run_tool("act", &parsed, json_mode, no_daemon, external).await
        }
        "see" => {
            let parsed = parse_see_args(rest)?;
            run_tool("see", &parsed, json_mode, no_daemon, external).await
        }
        "act" => {
            let parsed = parse_act_args(rest)?;
            run_tool("act", &parsed, json_mode, no_daemon, external).await
        }
        "state" => {
            let parsed = parse_state_args(rest)?;
            run_tool("state", &parsed, json_mode, no_daemon, external).await
        }
        "run" => {
            let parsed = parse_run_args(rest)?;
            run_tool("run", &parsed, json_mode, no_daemon, external).await
        }
        "vision" => {
            let parsed = parse_vision_args(rest)?;
            run_tool("vision", &parsed, json_mode, no_daemon, external).await
        }
        "help" => {
            if json_mode {
                print!("{}", help_json(rest.first().map(|s| s.as_str()))?);
            } else if rest.is_empty() {
                print!("{}", help_text());
            } else {
                match command_help_text(&rest[0]) {
                    Some(t) => print!("{t}"),
                    None => {
                        return Err(BladeError::Usage(format!(
                            "unknown command '{}' — run 'bladebro help' for the command list",
                            rest[0]
                        )))
                    }
                }
            }
            Ok(())
        }
        _ => Err(BladeError::Usage(format!(
            "unknown command: {cmd} — run 'bladebro help' for the command list"
        ))),
    }
}

/// Run a tool: auto-start the daemon if not running, connect to it.
/// One-shot mode only with --no-daemon.
///
/// Exit hardening: a tool result with `is_error` prints normally and then
/// exits 1 — the old CLI exited 0 on tool errors, so scripts and agents
/// could not tell a failed click from a successful one.
///
/// Failure handling is deliberately two-tiered:
/// - the socket connect fails → the daemon is dead; restart it and retry;
/// - the socket connect succeeds but the call fails (e.g. client timeout) →
///   the daemon is alive and may still be running the command; never kill
///   or bypass it, surface the error instead.
async fn run_tool(
    tool: &str,
    args: &Value,
    json_mode: bool,
    no_daemon: bool,
    external: Option<String>,
) -> Result<()> {
    // Explicit --host/--port: drive that already-running browser directly.
    // Skip the local daemon entirely (never launch or own Chrome here).
    if let Some(base) = external {
        let result = run_connected(tool, args, &base, true).await?;
        let code = print_result(&result, json_mode);
        if code != 0 {
            exit_with(code);
        }
        return Ok(());
    }

    if !no_daemon {
        #[cfg(unix)]
        {
            // Try the running daemon first.
            if daemon_running() {
                match send_to_daemon(tool, args) {
                    Ok(result) => {
                        let code = print_result(&result, json_mode);
                        if code != 0 {
                            exit_with(code);
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let unreachable = msg.contains("Connection refused")
                            || msg.contains("No such file")
                            || msg.contains("daemon not running");
                        if unreachable {
                            eprintln!("[bladebro] daemon unreachable ({msg}) — restarting it");
                            // A dead daemon can leave a stale socket file behind.
                            let _ = std::fs::remove_file(socket_path());
                        } else {
                            // Alive but the call failed: do not kill or bypass.
                            return Err(e);
                        }
                    }
                }
            }

            // Auto-start daemon in the background.
            auto_start_daemon()?;

            // Wait for socket to appear (up to 10s).
            let mut waited = 0;
            while waited < 100 {
                if daemon_running() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }

            if daemon_running() {
                match send_to_daemon(tool, args) {
                    Ok(result) => {
                        let code = print_result(&result, json_mode);
                        if code != 0 {
                            exit_with(code);
                        }
                        return Ok(());
                    }
                    Err(e) => eprintln!(
                        "[bladebro] daemon failed after auto-start ({e}), falling back to one-shot"
                    ),
                }
            } else {
                eprintln!(
                    "[bladebro] daemon didn't come up within 10s — falling back to one-shot\n\
                     (run 'bladebro daemon' in a terminal to see startup errors)"
                );
            }
        }
    }

    // One-shot: launch Chrome, run, exit.
    // Use a guard to ensure browser is ALWAYS shut down, even on error.
    let browser = crate::browser::Browser::launch(0).await?;
    let base = browser.base();

    let result = run_connected(tool, args, &base, false).await;

    // ALWAYS shut down Chrome, even if the above failed.
    // Without this, any error between launch and shutdown orphans Chrome + Xvfb.
    let _ = tokio::task::spawn_blocking(move || browser.shutdown()).await;

    let result = result?;
    let code = print_result(&result, json_mode);
    if code != 0 {
        exit_with(code);
    }
    Ok(())
}

/// Attach to a page at `base`, run one tool call, return the result.
///
/// `external=true` drives a browser Bladebro does not own (an already-running
/// Chrome reached via `--host`/`--port`): it must NOT re-inject saved logins,
/// warm the profile (no surprise navigation away from the user's tab), or shut
/// Chrome down. `external=false` owns the browser, so it restores logins,
/// warms on first run, and the caller owns shutdown.
async fn run_connected(tool: &str, args: &Value, base: &str, external: bool) -> Result<ToolResult> {
    let target = crate::cdp::first_page_target(base).await?;
    let client = crate::cdp::CdpClient::connect(target.ws_url()?).await?;
    let mut page = Page::attach(
        crate::cdp::CdpSession::root(client),
        base,
        None,
    ).await?;

    if !external {
        // Re-inject saved logins before anything navigates.
        let _ = crate::logins::restore(page.cdp_ref()).await;

        // Warm profile on first run.
        if crate::session_profile::SessionProfile::claim_warming() {
            warm_profile(&mut page).await;
        }
    }

    let result = dispatch(tool, args, &mut page).await?;

    // Persist live logins before tearing down, including in the one-shot
    // --no-daemon path (this used to be daemon/MCP-only, so `state set-cookie`
    // in a one-shot run never survived into the next one). Never for an
    // external browser we don't own.
    if !external {
        let _ = crate::logins::snapshot(page.cdp_ref()).await;
    }

    Ok(result)
}

/// Print a tool result and return the process exit code (0 ok, 1 error).
fn print_result(result: &ToolResult, json_mode: bool) -> i32 {
    let code = if result.is_error { 1 } else { 0 };

    // Vision: always persist the PNG and hand back a PATH. The old --json
    // inlined the full base64 (1-2MB for a screenshot) — one vision call
    // could blow an agent's context. The file is the CLI-native artifact;
    // MCP keeps the inline image because the protocol has image content.
    let image_path = result.image.as_deref().and_then(|img| {
        base64_decode(img).and_then(|data| crate::artifacts::write_artifact_bytes(&data, "png").ok())
    });

    if json_mode {
        let mut v = json!({
            "ok": !result.is_error,
            "is_error": result.is_error,
            "text": result.text,
        });
        if let Some(p) = &image_path {
            v["image_path"] = json!(p);
        }
        println!("{v}");
    } else if let Some(p) = &image_path {
        println!("{}\nsaved: {}", result.text, p);
    } else {
        println!("{}", result.text);
    }
    code
}

/// Exit the process with `code` after flushing the std streams.
///
/// Safe here: cli.rs is the process's top surface for client commands and
/// every cleanup step (browser shutdown, logins snapshot) has already run.
/// Rust's stdout is line-buffered, but flush anyway — a half-written JSON
/// object on exit would poison an agent's parse.
fn exit_with(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

/// Decode base64 to bytes. Public: shared with the MCP vision handler
/// (large-screenshot offloading).
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    
    // Minimal base64 decoder — avoids adding a dependency.
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    lookup[b'=' as usize] = 0;

    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'\n' && b != b'\r' && b != b' ').collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let vals: Vec<u8> = chunk.iter().map(|&b| lookup[b as usize]).collect();
        let n = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | (vals[3] as u32);
        out.push((n >> 16) as u8);
        if chunk[2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if chunk[3] != b'=' {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Warm the profile on first run (same as MCP server).
async fn warm_profile(page: &mut Page) {
    let sites = [
        "https://www.google.com",
        "https://github.com",
        "https://www.wikipedia.org",
    ];
    let mut ok = 0;
    for url in &sites {
        match tokio::time::timeout(
            std::time::Duration::from_secs(4),
            page.navigate(url),
        ).await {
            Ok(Ok(_)) => {
                ok += 1;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            _ => continue,
        }
    }
    if ok > 0 {
        eprintln!("[bladebro] profile warmed ({ok}/{} sites visited)", sites.len());
    }
}

// ── Arg Parsers ────────────────────────────────────────────────────────

/// Extract `--host`/`--port` into an external endpoint (`host:port`),
/// removing them from the arg list. Returns `None` when no `--port` is given
/// (fall back to the daemon / a freshly launched Chrome).
///
/// This is issue #16's fix: these flags were parsed by main.rs but never
/// forwarded to the CLI, so `state --port 9222` silently ignored the port.
/// Position-independent — works regardless of whether the flags precede or
/// follow the command.
fn extract_endpoint(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut host = String::from("127.0.0.1");
    let mut port: Option<u16> = None;
    let mut cleaned: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                if let Some(v) = args.get(i + 1) {
                    host = v.clone();
                }
                i += 2;
                continue;
            }
            "--port" => {
                if let Some(v) = args.get(i + 1) {
                    port = v.parse().ok();
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        cleaned.push(args[i].clone());
        i += 1;
    }
    let external = port.map(|p| format!("{host}:{p}"));
    (cleaned, external)
}

// ── Arg Parsing ────────────────────────────────────────────────────────
//
// Rules (v3.9.7, agent-native CLI):
// - Known flags are accepted anywhere; `--flag value` always takes the next
//   token (a missing value is a loud usage error, never a silent default).
// - Unknown flags are hard errors pointing at `help` — the old parsers
//   dropped them silently, so `see --budjet 5` read the wrong thing.
// - Positionals fill the fields the action needs, MCP-style: target first,
//   then value. Unquoted multi-word values join ("click Sign in" works).
// - `@file` reads the value from a file, `-` reads stdin — no shell-quoting
//   games for big JSON payloads.

/// The next token as a flag value; loud error when missing.
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| BladeError::Usage(format!("--{flag} needs a value (e.g. --{flag} <value>)")))
}

/// The next token parsed as a number; loud error on both missing and bad.
fn take_num<T: std::str::FromStr>(args: &[String], i: &mut usize, flag: &str) -> Result<T> {
    let raw = take_value(args, i, flag)?;
    raw.parse::<T>()
        .map_err(|_| BladeError::Usage(format!("--{flag} needs a number, got '{raw}'")))
}

/// Ref ids are `e` followed by digits (e1, e5, e12) — 'Edit'/'Enter' are text.
fn is_ref(s: &str) -> bool {
    s.len() > 1 && s.as_bytes()[0] == b'e' && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Resolve a payload argument: inline value, `@file`, or `-` (stdin).
fn resolve_payload(arg: &str) -> Result<String> {
    if arg == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| BladeError::Usage(format!("cannot read stdin: {e}")))?;
        if buf.trim().is_empty() {
            return Err(BladeError::Usage(
                "stdin was empty — pipe the payload in, e.g. `cat steps.json | bladebro run -`".into(),
            ));
        }
        return Ok(buf);
    }
    if let Some(path) = arg.strip_prefix('@') {
        return std::fs::read_to_string(path)
            .map_err(|e| BladeError::Usage(format!("cannot read {arg}: {e}")));
    }
    Ok(arg.to_string())
}

/// `fill` accepts several shapes — normalize to the array the MCP handler
/// requires:
/// - [{"ref":"e3","text":"John"}]   array form, as-is
/// - {"e3":"John","e5":"Doe"}        flat ref map
/// - {"label":"Email","text":"x"}    ONE field spec (any reserved key)
///   The third shape used to be misread as a ref-map entry named "label"
///   and failed with "stale ref: label" (caught live).
fn normalize_fields(parsed: Value) -> Result<Value> {
    match parsed {
        Value::Array(_) => Ok(parsed),
        Value::Object(map) => {
            const RESERVED: &[&str] = &["ref", "label", "text", "option", "check"];
            if map.keys().any(|k| RESERVED.contains(&k.as_str())) {
                return Ok(Value::Array(vec![Value::Object(map)]));
            }
            Ok(Value::Array(
                map.into_iter()
                    .map(|(k, v)| json!({ "ref": k, "text": v }))
                    .collect(),
            ))
        }
        _ => Err(BladeError::Usage(
            "fields must be a JSON object {\"e3\":\"John\"} (ref map), one spec {\"label\":\"Email\",\"text\":\"x\"}, or an array [{\"ref\":\"e3\",\"text\":\"John\"}]".into(),
        )),
    }
}

/// Token that looks like a URL/host: has a scheme, a port, or a dot.
fn looks_like_url(s: &str) -> bool {
    s.contains("://")
        || s.starts_with("data:")
        || s.starts_with("file:")
        || s.starts_with("about:")
        || s.contains("localhost")
        || s.contains('.')
}

/// Parse `nav` args: <url> [--block <classes>]
fn parse_nav_args(args: &[String]) -> Result<Value> {
    let mut url: Option<String> = None;
    let mut block: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "url" => url = Some(take_value(args, &mut i, "url")?),
                "block" => block = Some(take_value(args, &mut i, "block")?),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for nav — see 'bladebro help nav'"
                    )))
                }
            }
        } else if url.is_none() {
            url = Some(a);
        } else {
            return Err(BladeError::Usage(format!(
                "unexpected extra argument '{a}' — nav takes exactly one URL"
            )));
        }
        i += 1;
    }
    let url = url.ok_or_else(|| BladeError::Usage("nav needs a URL — bladebro nav <url>".into()))?;
    let mut j = json!({ "action": "navigate", "url": url });
    if let Some(b) = block {
        j["block"] = json!(b);
    }
    Ok(j)
}

/// Parse `see` args: [mode] [url] [extract <type>] [flags].
fn parse_see_args(args: &[String]) -> Result<Value> {
    let mut j = json!({});
    let mut url: Option<String> = None;
    let mut mode: Option<String> = None;
    let mut extract_pending = false;
    let mut extract_type: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        // Short aliases first: -f/-e/-b/-l/-t.
        let long = match a.as_str() {
            "-f" => "--filter",
            "-e" => "--extract",
            "-b" => "--budget",
            "-l" => "--limit",
            "-t" => "--template",
            other => other,
        };
        if let Some(flag) = long.strip_prefix("--") {
            match flag {
                "filter" | "find" | "logs" | "scope" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "budget" | "limit" => {
                    let v: u64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "extract" => {
                    let v = take_value(args, &mut i, flag)?;
                    if !matches!(v.as_str(), "auto" | "links" | "forms" | "json") {
                        return Err(BladeError::Usage(format!(
                            "--extract must be auto|links|forms|json, got '{v}'"
                        )));
                    }
                    extract_type = Some(v);
                }
                "template" => {
                    let raw = resolve_payload(&take_value(args, &mut i, flag)?)?;
                    let tpl: Value = serde_json::from_str(&raw).map_err(|e| {
                        BladeError::Usage(format!("--template must be valid JSON: {e}"))
                    })?;
                    j["template"] = tpl;
                }
                "url" => url = Some(take_value(args, &mut i, flag)?),
                "content" => j["content"] = json!(true),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for see — see 'bladebro help see'"
                    )))
                }
            }
        } else if a == "extract" {
            extract_pending = true;
        } else if matches!(a.as_str(), "model" | "content" | "outline") && mode.is_none() {
            mode = Some(a);
        } else if extract_pending
            && extract_type.is_none()
            && matches!(a.as_str(), "auto" | "links" | "forms" | "json")
        {
            extract_type = Some(a);
        } else if looks_like_url(&a) {
            if url.is_some() {
                return Err(BladeError::Usage(format!(
                    "multiple URLs given ('{a}') — see takes at most one"
                )));
            }
            url = Some(a);
        } else {
            return Err(BladeError::Usage(format!(
                "unrecognized argument '{a}' — expected a mode (model|content|outline), \
                 'extract <auto|links|forms|json>', or a URL. See 'bladebro help see'"
            )));
        }
        i += 1;
    }

    if extract_pending && extract_type.is_none() {
        return Err(BladeError::Usage(
            "see extract needs a type — auto, links, forms, or json (json also needs --template)"
                .into(),
        ));
    }
    if let Some(t) = extract_type {
        j["extract"] = json!(t);
        if j.get("mode").is_none() {
            j["mode"] = json!("extract");
        }
    }
    if let Some(m) = mode {
        j["mode"] = json!(m);
    }
    if let Some(u) = url {
        j["url"] = json!(u);
    }
    Ok(j)
}

/// Parse `act` args. Universal `--field` flags mirror the MCP schema exactly
/// (--ref, --label, --text, --role, --nth, --key, --url, --option, …), so an
/// agent that knows the MCP tool can drive the CLI 1:1. Positionals are the
/// terse form: target first, value second.
fn parse_act_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Usage(
            "act needs an action — click, type, fill, select, clear, press, scroll, hover, \
             navigate, upload, download, wait, eval, collect, read, batch, pdf, back, forward, \
             reload, save, load, open-tab, close-tab, switch-tab (see 'bladebro help act')"
                .into(),
        ));
    }

    const ACTIONS: &[&str] = &[
        "click", "type", "fill", "select", "clear", "press", "scroll", "hover", "navigate",
        "upload", "download", "wait", "eval", "collect", "read", "batch", "pdf", "back",
        "forward", "reload", "save", "load", "open-tab", "close-tab", "switch-tab",
    ];
    let action = args[0].as_str();
    if !ACTIONS.contains(&action) {
        return Err(BladeError::Usage(format!(
            "unknown act action '{action}' — available: {} (see 'bladebro help act')",
            ACTIONS.join(", ")
        )));
    }

    let mut j = json!({ "action": action });
    let mut pos: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "ref" | "label" | "text" | "role" | "key" | "url" | "option" | "js" | "path"
                | "condition" | "press" | "submit" | "block" | "name" | "expect" | "steps"
                | "fields" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "target-id" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["target_id"] = json!(v);
                }
                "nth" | "timeout" | "max" => {
                    let v: u64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "dx" | "dy" => {
                    let v: i64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "x" | "y" | "scale" => {
                    let v: f64 = take_num(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "slim" => j["slim"] = json!(true),
                "landscape" => j["landscape"] = json!(true),
                "print-background" => j["printBackground"] = json!(true),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for act — see 'bladebro help act'"
                    )))
                }
            }
        } else {
            pos.push(a);
        }
        i += 1;
    }

    match action {
        "click" | "hover" => {
            if j.get("ref").is_none() && j.get("label").is_none() && !pos.is_empty() {
                if is_ref(&pos[0]) {
                    j["ref"] = json!(pos[0].clone());
                    if pos.len() > 1 {
                        return Err(BladeError::Usage(format!(
                            "unexpected extra argument '{}' after a ref — use --label/--nth, or quote the label",
                            pos[1]
                        )));
                    }
                } else {
                    j["label"] = json!(pos.join(" "));
                }
            }
        }
        "type" => {
            let mut pi = 0usize;
            if j.get("ref").is_none() && j.get("label").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(t);
                    }
                    pi = 1;
                }
            }
            if j.get("text").is_none() && pos.len() > pi {
                j["text"] = json!(pos[pi..].join(" "));
            }
            if j.get("ref").is_none() && j.get("label").is_none() {
                return Err(BladeError::Usage(
                    "type needs a target — `act type <ref|label> <text>` or --ref/--label/--text"
                        .into(),
                ));
            }
            if j.get("text").is_none() {
                return Err(BladeError::Usage(
                    "type needs text — `act type <target> <text>` or --text <value>".into(),
                ));
            }
        }
        "fill" => {
            if let Some(v) = j.get("fields").and_then(|f| f.as_str()).map(String::from) {
                let raw = resolve_payload(&v)?;
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| BladeError::Usage(format!("invalid fields JSON: {e}")))?;
                j["fields"] = normalize_fields(parsed)?;
            } else if j.get("fields").is_none() {
                let raw_arg = pos.first().ok_or_else(|| {
                    BladeError::Usage(
                        "fill needs fields — bladebro act fill '{\"e3\":\"John\",\"e5\":\"Doe\"}' \
                         [--submit <ref|text>] (or @file / - for big payloads)"
                            .into(),
                    )
                })?;
                let raw = resolve_payload(raw_arg)?;
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| BladeError::Usage(format!("invalid fields JSON: {e}")))?;
                j["fields"] = normalize_fields(parsed)?;
            }
            if pos.len() > 1 {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}'",
                    pos[1]
                )));
            }
        }
        "select" => {
            let mut pi = 0usize;
            if j.get("ref").is_none() && j.get("label").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(t);
                    }
                    pi = 1;
                }
            }
            if j.get("option").is_none() && pos.len() > pi {
                j["option"] = json!(pos[pi..].join(" "));
            }
            if j.get("ref").is_none() && j.get("label").is_none() {
                return Err(BladeError::Usage(
                    "select needs a target — `act select <ref|label> <option>`".into(),
                ));
            }
            if j.get("option").is_none() {
                return Err(BladeError::Usage(
                    "select needs an option — `act select <target> <option text|value>`".into(),
                ));
            }
        }
        "clear" | "read" => {
            if j.get("ref").is_none() && !pos.is_empty() {
                j["ref"] = json!(pos[0].clone());
            }
            if j.get("ref").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a ref — `act {action} e5` (refs come from see model / nav)"
                )));
            }
        }
        "press" => {
            if j.get("key").is_none() && !pos.is_empty() {
                j["key"] = json!(pos[0].clone());
            }
            if j.get("key").is_none() {
                return Err(BladeError::Usage(
                    "press needs a key — `act press Enter` (Enter, Tab, Escape, ArrowDown, …)".into(),
                ));
            }
        }
        "scroll" => {
            if j.get("dx").is_none() && !pos.is_empty() {
                let v: i64 = pos[0].parse().map_err(|_| {
                    BladeError::Usage(format!("scroll dx must be a number, got '{}'", pos[0]))
                })?;
                j["dx"] = json!(v);
            }
            if j.get("dy").is_none() && pos.len() > 1 {
                let v: i64 = pos[1].parse().map_err(|_| {
                    BladeError::Usage(format!("scroll dy must be a number, got '{}'", pos[1]))
                })?;
                j["dy"] = json!(v);
            }
            if j.get("dx").is_none() && j.get("dy").is_none() {
                return Err(BladeError::Usage(
                    "scroll needs a distance — `act scroll 0 500` or --dy <px> (negative scrolls up)"
                        .into(),
                ));
            }
        }
        "navigate" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if j.get("url").is_none() {
                return Err(BladeError::Usage(
                    "navigate needs a URL — `act navigate example.com`".into(),
                ));
            }
            if pos.len() > 1 {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}'",
                    pos[1]
                )));
            }
        }
        "upload" => {
            let mut pi = 0usize;
            if j.get("ref").is_none() && j.get("label").is_none() {
                if let Some(t) = pos.first() {
                    if is_ref(t) {
                        j["ref"] = json!(t);
                    } else {
                        j["label"] = json!(t);
                    }
                    pi = 1;
                }
            }
            // The MCP handler takes the file path in `text` (NOT `path`) —
            // the old CLI sent `path`, so every CLI upload arrived empty.
            let path = j
                .get("path")
                .and_then(|p| p.as_str())
                .map(String::from)
                .or_else(|| j.get("text").and_then(|t| t.as_str()).map(String::from))
                .or_else(|| if pos.len() > pi { Some(pos[pi..].join(" ")) } else { None })
                .ok_or_else(|| {
                    BladeError::Usage("upload needs a file path — `act upload e5 /path/file.pdf`".into())
                })?;
            if let Some(obj) = j.as_object_mut() {
                obj.remove("path");
            }
            j["text"] = json!(path);
            if j.get("ref").is_none() && j.get("label").is_none() {
                return Err(BladeError::Usage(
                    "upload needs a target — `act upload <ref|label> <path>`".into(),
                ));
            }
        }
        "download" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if j.get("url").is_none() {
                return Err(BladeError::Usage(
                    "download needs a URL — `act download https://x/file.pdf [--path <dir>]`".into(),
                ));
            }
        }
        "wait" => {
            if j.get("condition").is_none() && !pos.is_empty() {
                j["condition"] = json!(pos[0].clone());
            }
            if j.get("condition").is_none() {
                return Err(BladeError::Usage(
                    "wait needs a condition — element, title, url, text, settle, or js \
                     (e.g. `act wait settle`, `act wait js \"document.title\"`)"
                        .into(),
                ));
            }
            let cond = j["condition"].as_str().unwrap_or("").to_string();
            if !matches!(cond.as_str(), "element" | "title" | "url" | "text" | "settle" | "js") {
                return Err(BladeError::Usage(format!(
                    "unknown wait condition '{cond}' — element, title, url, text, settle, js"
                )));
            }
            if j.get("text").is_none() && pos.len() > 1 {
                j["text"] = json!(pos[1..].join(" "));
            }
            if cond != "settle" && j.get("text").is_none() {
                return Err(BladeError::Usage(format!(
                    "wait {cond} needs a match value — `act wait {cond} --text \"…\"`"
                )));
            }
        }
        "eval" => {
            if j.get("js").is_none() && !pos.is_empty() {
                let joined = pos.join(" ");
                j["js"] = json!(resolve_payload(&joined)?);
            } else if let Some(v) = j.get("js").and_then(|x| x.as_str()).map(String::from) {
                j["js"] = json!(resolve_payload(&v)?);
            }
            if j.get("js").is_none() {
                return Err(BladeError::Usage(
                    "eval needs JS — `act eval \"document.title\"` (or @script.js / - for stdin)".into(),
                ));
            }
        }
        "collect" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if j.get("url").is_none() {
                return Err(BladeError::Usage(
                    "collect needs a URL — `act collect <url> [--max N]`".into(),
                ));
            }
        }
        "pdf" => {
            if !pos.is_empty() {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}' for pdf — use --path/--landscape",
                    pos[0]
                )));
            }
        }
        "back" | "forward" | "reload" => {
            if !pos.is_empty() {
                return Err(BladeError::Usage(format!(
                    "{action} takes no arguments, got '{}'",
                    pos[0]
                )));
            }
        }
        "save" | "load" => {
            if j.get("name").is_none() && !pos.is_empty() {
                j["name"] = json!(pos[0].clone());
            }
            if j.get("name").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a name — `act {action} my-session`"
                )));
            }
        }
        "batch" => {
            let raw = if let Some(s) = j.get("steps").and_then(|x| x.as_str()).map(String::from) {
                s
            } else if let Some(first) = pos.first() {
                first.clone()
            } else {
                return Err(BladeError::Usage(
                    "batch needs steps — bladebro act batch '[{\"action\":\"click\",\"ref\":\"e5\"}]' \
                     (or @steps.json / - for stdin)"
                        .into(),
                ));
            };
            let raw = resolve_payload(&raw)?;
            let steps: Value = serde_json::from_str(&raw)
                .map_err(|e| BladeError::Usage(format!("invalid steps JSON: {e}")))?;
            let arr = steps
                .as_array()
                .ok_or_else(|| BladeError::Usage("steps must be a JSON array".into()))?;
            if arr.is_empty() {
                return Err(BladeError::Usage("batch needs at least one step".into()));
            }
            j["steps"] = steps;
        }
        "open-tab" => {
            if j.get("url").is_none() && !pos.is_empty() {
                j["url"] = json!(pos[0].clone());
            }
            if pos.len() > 1 {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}'",
                    pos[1]
                )));
            }
        }
        "close-tab" | "switch-tab" => {
            if j.get("target_id").is_none() && !pos.is_empty() {
                j["target_id"] = json!(pos[0].clone());
            }
            if j.get("target_id").is_none() {
                return Err(BladeError::Usage(format!(
                    "{action} needs a tab id — `act {action} <id>` (ids come from `bladebro state tabs`)"
                )));
            }
        }
        _ => unreachable!("action validated above"),
    }

    Ok(j)
}

/// Parse `state` args: <op> [args] [flags]. Same op names as MCP (plus
/// rm-ss), and unknown ops/flags are loud errors.
fn parse_state_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Usage(
            "state needs an op — cookies, set-cookie, del-cookie, ls, ss, set-ls, set-ss, \
             rm-ls, rm-ss, clear-ls, clear-ss, tabs, open-tab, close-tab, switch-tab, save, \
             load, compress, block (see 'bladebro help state')"
                .into(),
        ));
    }

    const OPS: &[&str] = &[
        "cookies", "set-cookie", "del-cookie", "ls", "ss", "set-ls", "set-ss", "rm-ls",
        "rm-ss", "clear-ls", "clear-ss", "tabs", "open-tab", "close-tab", "switch-tab",
        "save", "load", "compress", "block",
    ];
    // Aliases: MCP-style names.
    let op = match args[0].as_str() {
        "localStorage" => "ls",
        "sessionStorage" => "ss",
        other => other,
    };
    if !OPS.contains(&op) {
        return Err(BladeError::Usage(format!(
            "unknown state op '{op}' — available: {} (see 'bladebro help state')",
            OPS.join(", ")
        )));
    }

    let mut j = json!({ "op": op });
    let mut pos: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "url" | "domain" | "path" | "name" | "value" | "key" | "classes" => {
                    let v = take_value(args, &mut i, flag)?;
                    j[flag] = json!(v);
                }
                "same-site" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["sameSite"] = json!(v);
                }
                "target-id" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["target_id"] = json!(v);
                }
                "mode" => {
                    let v = take_value(args, &mut i, flag)?;
                    j["mode"] = json!(v);
                }
                "secure" => j["secure"] = json!(true),
                "http-only" => j["httpOnly"] = json!(true),
                "clear" => j["clear"] = json!(true),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for state — see 'bladebro help state'"
                    )))
                }
            }
        } else {
            pos.push(a);
        }
        i += 1;
    }

    // Positionals fill name/value per op (flags win when both are given).
    match op {
        "set-cookie" => {
            let mut p = pos.into_iter();
            let name = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| p.next());
            let value = j
                .get("value")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| p.next());
            let name = name.ok_or_else(|| {
                BladeError::Usage(
                    "set-cookie needs a name — `state set-cookie <name> <value> [--url <u> | --domain <d>]`"
                        .into(),
                )
            })?;
            let value = value.ok_or_else(|| {
                BladeError::Usage("set-cookie needs a value — `state set-cookie <name> <value>`".into())
            })?;
            j["name"] = json!(name);
            j["value"] = json!(value);
        }
        "del-cookie" => {
            let name = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(
                        "del-cookie needs a name — `state del-cookie <name> [--url <u> | --domain <d>]`"
                            .into(),
                    )
                })?;
            j["name"] = json!(name);
        }
        "set-ls" | "set-ss" => {
            // Storage ops take the key in `name` — the MCP schema field the
            // handler actually reads. The old code emitted `key`, so every
            // set-ls/set-ss stored an EMPTY key (caught live: `ls` showed
            // "=dark"). `--key`/`--name` flags both work.
            let key = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| j.get("key").and_then(|x| x.as_str()).map(String::from))
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a key — `state {op} <key> <value>`"))
                })?;
            let value = j
                .get("value")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.get(1).cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a value — `state {op} <key> <value>`"))
                })?;
            if let Some(obj) = j.as_object_mut() {
                obj.remove("key");
            }
            j["name"] = json!(key);
            j["value"] = json!(value);
        }
        "rm-ls" | "rm-ss" => {
            let key = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| j.get("key").and_then(|x| x.as_str()).map(String::from))
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| BladeError::Usage(format!("{op} needs a key — `state {op} <key>`")))?;
            if let Some(obj) = j.as_object_mut() {
                obj.remove("key");
            }
            j["name"] = json!(key);
        }
        "cookies" => {
            // Optional positional URL filters the list to that domain.
            if j.get("url").is_none() {
                if let Some(u) = pos.first() {
                    j["url"] = json!(u);
                }
            }
        }
        "open-tab" => {
            let url = j
                .get("url")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage("open-tab needs a URL — `state open-tab <url>`".into())
                })?;
            j["url"] = json!(url);
        }
        "close-tab" | "switch-tab" => {
            let id = j
                .get("target_id")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!(
                        "{op} needs a tab id — `state {op} <id>` (ids come from `state tabs`)"
                    ))
                })?;
            j["target_id"] = json!(id);
        }
        "save" | "load" => {
            let name = j
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .ok_or_else(|| {
                    BladeError::Usage(format!("{op} needs a session name — `state {op} <name>`"))
                })?;
            j["name"] = json!(name);
        }
        "compress" => {
            let mode = j
                .get("mode")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| pos.first().cloned())
                .unwrap_or_else(|| "status".to_string());
            if !matches!(mode.as_str(), "on" | "off" | "status") {
                return Err(BladeError::Usage(format!(
                    "compress mode must be on|off|status, got '{mode}'"
                )));
            }
            j["mode"] = json!(mode);
        }
        "block" => {
            if j.get("classes").is_none() {
                if let Some(c) = pos.first() {
                    if c == "clear" {
                        j["clear"] = json!(true);
                    } else {
                        j["classes"] = json!(c);
                    }
                }
            }
        }
        "tabs" | "ls" | "ss" | "clear-ls" | "clear-ss" => {
            if !pos.is_empty() {
                return Err(BladeError::Usage(format!(
                    "unexpected extra argument '{}' for {op}",
                    pos[0]
                )));
            }
        }
        _ => unreachable!("op validated above"),
    }

    Ok(j)
}

/// Parse `run` args: <json-steps> | @file | - (stdin) | --steps <json>.
fn parse_run_args(args: &[String]) -> Result<Value> {
    let mut steps_raw: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        if let Some(flag) = a.strip_prefix("--") {
            match flag {
                "steps" => steps_raw = Some(take_value(args, &mut i, flag)?),
                _ => {
                    return Err(BladeError::Usage(format!(
                        "unknown flag --{flag} for run — see 'bladebro help run'"
                    )))
                }
            }
        } else if steps_raw.is_none() {
            steps_raw = Some(a);
        } else {
            return Err(BladeError::Usage(
                "unexpected extra argument — pass steps once, or use @file / - for big payloads"
                    .into(),
            ));
        }
        i += 1;
    }

    let raw = steps_raw.ok_or_else(|| {
        BladeError::Usage(
            "run needs a JSON steps array, e.g. bladebro run '[{\"action\":\"click\",\"ref\":\"e5\"}]' \
             (or @steps.json / - for stdin)"
                .into(),
        )
    })?;
    let raw = resolve_payload(&raw)?;
    let steps: Value = serde_json::from_str(&raw)
        .map_err(|e| BladeError::Usage(format!("invalid steps JSON: {e}")))?;
    let arr = steps
        .as_array()
        .ok_or_else(|| BladeError::Usage("steps must be a JSON array".into()))?;
    if arr.is_empty() {
        return Err(BladeError::Usage("steps must not be empty".into()));
    }
    Ok(json!({ "steps": steps }))
}

/// Parse `vision` args: [--marks].
fn parse_vision_args(args: &[String]) -> Result<Value> {
    let mut marks = false;
    for a in args {
        match a.as_str() {
            "--marks" => marks = true,
            s if s.starts_with("--") => {
                return Err(BladeError::Usage(format!(
                    "unknown flag {s} for vision — see 'bladebro help vision'"
                )))
            }
            s => {
                return Err(BladeError::Usage(format!(
                    "unexpected argument '{s}' for vision — bladebro vision [--marks]"
                )))
            }
        }
    }
    Ok(json!({ "marks": marks }))
}

/// Wait for a termination signal (SIGTERM/SIGINT/SIGHUP on Unix, Ctrl+C on Windows).
/// Same as the MCP server's signal handler.
#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Only catch SIGTERM and SIGINT. NOT SIGHUP — SIGHUP is sent
        // when the parent process exits, which would kill the daemon
        // immediately after the first CLI command. Daemons ignore SIGHUP.
        let mut term = signal(SignalKind::terminate()).ok();
        let mut int = signal(SignalKind::interrupt()).ok();
        tokio::select! {
            _ = async { if let Some(s) = &mut term { s.recv().await } else { std::future::pending().await } } => {}
            _ = async { if let Some(s) = &mut int { s.recv().await } else { std::future::pending().await } } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ── Daemon ──────────────────────────────────────────────────────────────

/// Start the CLI daemon: persistent Chrome + Unix socket server.
/// Same lifecycle as MCP (lazy launch, self-healing, idle timeout, reaper).
#[cfg(unix)]
pub async fn run_daemon() -> Result<()> {
    use tokio::net::UnixListener;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Ignore SIGHUP — the daemon must survive the parent CLI exiting.
    #[cfg(unix)]
    ignore_sighup();

    let path = socket_path();
    // Never steal a LIVE daemon's socket: connect to check first. Without
    // this, a second `bladebro daemon` rebinds over the active one and
    // orphans it — a ghost that keeps its Chrome but that `stop` can no
    // longer reach (and whose death later unlinks the new daemon's socket).
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        eprintln!("[bladebro] daemon already running on {} — exiting", path.display());
        return Ok(());
    }
    // Remove stale socket (connect failed — nobody is listening).
    let _ = std::fs::remove_file(&path);
    // Create parent dir with secure permissions.
    if let Some(parent) = path.parent() {
        let _ = crate::platform::secure_create_dir_all(parent);
    }

    let listener = UnixListener::bind(&path)
        .map_err(|e| BladeError::Other(format!("failed to bind socket {}: {e}", path.display())))?;

    // SECURITY: Restrict socket to owner-only. Without this, the socket
    // inherits umask permissions (often 755), which on misconfigured systems
    // (umask 000) lets any local user connect and control the browser.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    eprintln!("[bladebro] daemon listening on {}", path.display());
    let _ = std::fs::write(pid_path(), std::process::id().to_string());

    let mut browser: Option<crate::browser::Browser> = None;
    let mut page: Option<Page> = None;
    let mut last_activity = std::time::Instant::now();
    let idle_secs: u64 = std::env::var("BLADE_IDLE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let mut idle_check = tokio::time::interval(std::time::Duration::from_secs(15));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let knowledge = crate::knowledge::load_shared();
    // Periodic profile sync: every 60s while Chrome is alive,
    // sync the session profile to the template. SIGKILL resilience.
    let mut last_sync = std::time::Instant::now();
    let sync_interval = std::time::Duration::from_secs(60);

    loop {
        tokio::select! {
            // Graceful shutdown on SIGTERM/SIGINT/SIGHUP.
            _ = wait_for_shutdown_signal() => {
                eprintln!("[bladebro] termination signal \u{2014} shutting down Chrome gracefully");
                break;
            }
            // Accept new connections.
            accept = listener.accept() => {
                let (stream, _) = match accept {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[bladebro] accept error: {e}");
                        continue;
                    }
                };

                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    continue;
                }
                let line = line.trim();
                if line.is_empty() { continue; }

                let req: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let tool = req.get("tool").and_then(|t| t.as_str()).unwrap_or("");
                let args = req.get("args").cloned().unwrap_or(json!({}));

                // Stop command.
                if tool == "stop" {
                    // Flush the knowledge base BEFORE acknowledging: once
                    // `bladebro stop` returns, consent/visits/timing/block
                    // knowledge must already be durable on disk.
                    if let Ok(mut kb) = knowledge.lock() {
                        kb.prune();
                        kb.sync();
                    }
                    let resp = json!({ "ok": true, "text": "daemon stopped" });
                    let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                    let _ = reader.get_mut().write_all(resp_str.as_bytes()).await;
                    let _ = reader.get_mut().write_all(b"\n").await;
                    eprintln!("[bladebro] daemon stopping (stop command)");
                    break;
                }

                // Lazy launch + self-heal (same as MCP server).
                let need_launch = page.is_none()
                    || page.as_ref().map(|p| p.is_closed()).unwrap_or(true);
                if need_launch {
                    if browser.is_some() {
                        eprintln!("[bladebro] browser connection lost, relaunching...");
                        if let Some(b) = browser.take() {
                            let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    match launch_browser().await {
                        Ok((new_page, new_browser)) => {
                            browser = new_browser;
                            page = Some(new_page);
                            if let Some(ref mut p) = page {
                                p.set_knowledge(knowledge.clone());
                            }
                            if crate::session_profile::SessionProfile::claim_warming() {
                                if let Some(ref mut p) = page {
                                    warm_profile(p).await;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[bladebro] Chrome launch failed: {e}");
                            let resp = json!({ "ok": false, "error": format!("Chrome launch failed: {e}") });
                            let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                            let _ = reader.get_mut().write_all(resp_str.as_bytes()).await;
                            let _ = reader.get_mut().write_all(b"\n").await;
                            continue;
                        }
                    }
                }

                // Dispatch.
                let result = {
                    let p = page.as_mut().unwrap();
                    dispatch(tool, &args, p).await
                };

                let resp = match result {
                    Ok(r) => json!({
                        "ok": true,
                        "text": r.text,
                        "image": r.image,
                        "is_error": r.is_error,
                    }),
                    Err(BladeError::Closed) => {
                        // Self-heal: relaunch and retry.
                        eprintln!("[bladebro] browser closed during tool call, reconnecting...");
                        if let Some(b) = browser.take() {
                            let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                        }
                        page = None;
                        match launch_browser().await {
                            Ok((new_page, new_browser)) => {
                                browser = new_browser;
                                page = Some(new_page);
                                if let Some(ref mut p) = page {
                                    p.set_knowledge(knowledge.clone());
                                }
                                // Retry.
                                let p = page.as_mut().unwrap();
                                match dispatch(tool, &args, p).await {
                                    Ok(r) => json!({
                                        "ok": true,
                                        "text": r.text,
                                        "image": r.image,
                                        "is_error": r.is_error,
                                    }),
                                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                                }
                            }
                            Err(e) => json!({ "ok": false, "error": format!("reconnect failed: {e}") }),
                        }
                    }
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                };

                let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                let _ = reader.get_mut().write_all(resp_str.as_bytes()).await;
                let _ = reader.get_mut().write_all(b"\n").await;

                last_activity = std::time::Instant::now();
            }
            _ = idle_check.tick() => {
                if idle_secs > 0 && browser.is_some() && last_activity.elapsed().as_secs() > idle_secs {
                    eprintln!("[bladebro] idle timeout ({idle_secs}s), shutting down Chrome");
                    if let Some(b) = browser.take() {
                        if let Some(ref p) = page {
                            let _ = crate::logins::snapshot(p.cdp_ref()).await;
                        }
                        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                    }
                    page = None;
                }
                // Periodic login snapshot (same as MCP server). Persist the
                // authoritative live cookie store, never a hot profile copy.
                if browser.is_some() && last_sync.elapsed() > sync_interval {
                    last_sync = std::time::Instant::now();
                    if let Some(ref p) = page {
                        if !p.cdp_ref().is_closed() {
                            let _ = crate::logins::snapshot(p.cdp_ref()).await;
                        }
                    }
                    // Sync the knowledge base to disk (prune + write) —
                    // consent/visits/timing/block-risk compound across sessions.
                    let kb = knowledge.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(mut kb) = kb.lock() {
                            kb.prune();
                            kb.sync();
                        }
                    }).await;
                }
            }
        }
    }

    // Cleanup: persist live logins, then kill Chrome gracefully.
    if let Some(ref p) = page {
        let _ = crate::logins::snapshot(p.cdp_ref()).await;
    }
    if let Some(b) = browser {
        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
    }
    // Flush the knowledge base before exit (consent, visits, timing, risk).
    if let Ok(mut kb) = knowledge.lock() {
        kb.prune();
        kb.sync();
    }
    // Clean up only while WE still own the socket + pidfile. If a newer
    // daemon has taken over (this one is a "ghost"), unlinking would
    // delete the LIVE daemon's files and orphan it in turn — the
    // self-perpetuating ghost cycle. Ownership test: the pidfile says us.
    if read_pid_file() == Some(std::process::id() as i32) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(pid_path());
    }
    eprintln!("[bladebro] daemon stopped");
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_daemon() -> Result<()> {
    Err(BladeError::Other("daemon mode is Unix-only (requires Unix sockets)".into()))
}

/// Launch Chrome and create a Page for the daemon.
/// Cleans up the browser if any step after launch fails.
#[cfg(unix)]
async fn launch_browser() -> Result<(Page, Option<crate::browser::Browser>)> {
    let browser = crate::browser::Browser::launch(0).await?;
    let base = browser.base();
    let result = async {
        let target = crate::cdp::first_page_target(&base).await?;
        let client = crate::cdp::CdpClient::connect(target.ws_url()?).await?;
        let page = Page::attach(
            crate::cdp::CdpSession::root(client),
            &base,
            None,
        ).await?;
        Ok(page)
    }.await;

    match result {
        Ok(page) => {
            // Re-inject saved logins before anything navigates.
            let _ = crate::logins::restore(page.cdp_ref()).await;
            Ok((page, Some(browser)))
        }
        Err(e) => {
            // Clean up the browser we just launched.
            let _ = tokio::task::spawn_blocking(move || browser.shutdown()).await;
            Err(e)
        }
    }
}

// ── Daemon lifecycle helpers ───────────────────────────────────────────

/// Path of the daemon pid file. Stale files (SIGKILL) are detected by
/// checking whether the pid is alive, and only then killed.
#[cfg(unix)]
fn pid_path() -> std::path::PathBuf {
    crate::platform::blade_dir().join("cli.pid")
}

#[cfg(unix)]
fn read_pid_file() -> Option<i32> {
    std::fs::read_to_string(pid_path()).ok()?.trim().parse().ok()
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

/// Verify the pid is actually a bladebro process before killing it — pids
/// get recycled and a blind kill could hit an innocent process. Without
/// /proc (macOS), assume yes: the file was just written by a daemon whose
/// socket went dead, so the risk window is tiny.
#[cfg(unix)]
fn looks_like_bladebro(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm.trim().starts_with("bladebro"),
        Err(_) => true,
    }
}

/// Stop the daemon: graceful over the socket; idempotent when nothing runs.
///
/// Reliability: if the socket is gone but the pid file says a daemon is
/// alive (wedged, or SIGKILLed mid-cleanup), terminate it — otherwise
/// `stop` would lie "not running" while an orphan Chrome kept running.
#[cfg(unix)]
pub async fn stop_daemon() -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let path = socket_path();

    // Graceful path: a live daemon answers the stop command itself
    // (it flushes logins + knowledge before acknowledging).
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        writeln!(stream, "{{\"tool\":\"stop\"}}")?;
        stream.flush()?;
        let mut resp = String::new();
        let _ = stream.read_to_string(&mut resp);
        // Wait for the daemon to finish teardown — it removes the socket
        // file last. Without this, an immediate second `stop` could connect
        // to the dying daemon's still-bound socket and report "stopped"
        // again instead of "not running".
        for _ in 0..50 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        println!("daemon stopped");
        return Ok(());
    }

    // No socket. A daemon process may still linger — check the pid file.
    if let Some(pid) = read_pid_file() {
        if process_alive(pid) && looks_like_bladebro(pid) {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            for _ in 0..50 {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if process_alive(pid) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            println!("daemon (pid {pid}) was unresponsive — terminated");
        } else {
            println!("daemon not running");
        }
        let _ = std::fs::remove_file(pid_path());
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }

    let _ = std::fs::remove_file(&path);
    println!("daemon not running");
    Ok(())
}

#[cfg(not(unix))]
pub async fn stop_daemon() -> Result<()> {
    println!("daemon mode is Unix-only — commands run one-shot on this platform");
    Ok(())
}

// ── Help ───────────────────────────────────────────────────────────────

/// The CLI's agent manual — served by `help --json`. This is the single
/// discovery surface: tool schemas (identical to MCP `tools/list`), the
/// command map, the output contract, and the workflow that gets the most
/// out of both.
const INSTRUCTIONS: &str = "\
Bladebro CLI — drive a real, stealthy browser from shell commands.
SETUP: none. The first command auto-starts a background daemon (one Chrome for all later commands). --no-daemon forces isolated one-shot runs; 'bladebro stop' cleans up.
DISCOVERY: 'bladebro help' (human) — 'bladebro help --json' (this document) — 'bladebro help <command>' for one command in depth.
OUTPUT CONTRACT: --json on any command gives ONE JSON object on stdout: {\"ok\":bool,\"is_error\":bool,\"text\":string} (+ \"image_path\" for vision — the PNG is saved to a file, never inlined base64). Exit codes: 0 = success; 1 = the command ran but failed (read 'text' — it carries current page state so you can recover without an extra call); 2 = usage error (bad command/flag/argument — fix the command).
WORKFLOW: 1) nav <url> gives refs + a content preview (often enough to act). 2) Address by text when you can: 'act click \"Sign in\"' needs no see; refs self-heal; labels work for form fields. 3) On list/search/product/profile pages, 'see extract auto' FIRST: one call returns structured items (site adapters add fields on Reddit/GitHub/product pages; on Reddit POST pages it returns the full comment tree — all replies, thread order, complete flag). 4) Collapse round-trips: 'act fill' for whole forms; 'act batch' / 'run' for sequences (if/while branching; {\"action\":\"see\"} steps read inline). 5) Big JSON payloads: @file or - (stdin) — no shell-quoting games. 6) Keep budgets default; big outputs are written to files with an inline preview.
RELIABILITY: real Chromium with the full stealth stack and human-like input; per-domain knowledge (settle timing, block config, bot risk) compounds across runs. Errors are loud, never silent: unknown flags and missing values fail with exit 2; a failed action returns the page state for recovery. Vision is the last resort — refs and deltas are cheaper.
";

/// The human manual — printed by `bladebro help` / `-h` / bare `bladebro`.
pub fn help_text() -> String {
    HELP_TEXT.replace("__VERSION__", env!("CARGO_PKG_VERSION"))
}

const HELP_TEXT: &str = r#"bladebro __VERSION__ — agentic browser driver (CLI)

USAGE
  bladebro <command> [args] [--json] [--no-daemon]
  bladebro help [command]      this manual (start here)

The first command auto-starts a background daemon: ONE Chrome instance for
all later commands (no startup delay after the first launch). `bladebro stop`
shuts it down. `--no-daemon` forces isolated one-shot runs.

COMMANDS
  nav <url> [--block <classes>]   navigate (refs + content preview; usually enough to act)
  see [mode] [url] [flags]        read without acting: model|content|outline,
                                  extract <auto|links|forms|json>, --find, --scope, --logs
  act <action> [args]             interact: click, type, fill, select, clear, press, scroll,
                                  hover, navigate, upload, download, wait, eval, collect,
                                  read, batch, pdf, back, forward, reload, save, load,
                                  open-tab, close-tab, switch-tab
  state <op> [args]               cookies, set-cookie, del-cookie, ls, ss, set-ls, set-ss,
                                  rm-ls, rm-ss, clear-ls, clear-ss, tabs, open-tab,
                                  close-tab, switch-tab, save, load, compress, block
  run '<json-steps>'              batch with if/while branching + inline see reads
  vision [--marks]                screenshot (saved to a file; path printed)
  daemon | stop                   manage the persistent Chrome session
  mcp                             MCP server on stdio — add to your agent's client config
  audit                           stealth audit — 36-vector suite + boot self-check
  update | -u [--check] [--force]  self-update; --rollback restores the previous binary
  doctor | -doc                   system diagnostics (13 checks)
  -v | --version                  version + install method + update status
  help [command]                  this manual; 'help --json' for the machine version

QUICK START
  bladebro nav example.com                      navigate
  bladebro see model                            interactive elements with refs
  bladebro see content                          page as clean markdown
  bladebro see extract auto                     structured data (lists, posts, repos, products)
  bladebro act click e5                         click by ref
  bladebro act click "Sign in"                  click by text (no see needed)
  bladebro act type e12 "hello world"           type into one field
  bladebro act fill '{"e12":"John"}' --submit e20      fill a form in ONE call
  bladebro act batch '[{"action":"navigate","url":"x.com"},{"action":"see","mode":"content"}]'
  bladebro run @steps.json                      big payload from a file (- = stdin)
  bladebro act eval "document.title"            evaluate JS

FLAGS
  --json         one JSON object on stdout: {"ok", "is_error", "text"} (+ "image_path" for vision)
  --no-daemon    one-shot mode: launch Chrome per command
  --host/--port  drive an already-running Chrome on this debug port (skips the daemon)
  --marks        vision: overlay numbered ref badges

EXIT CODES
  0  success
  1  the command ran but failed — read the output text (it carries page state for recovery)
  2  usage error — bad command/flag/argument; the message says how to fix it

ENVIRONMENT
  BLADE_HOME           isolate everything (daemon, Chrome, sessions, knowledge) in a directory
  BLADE_CMD_TIMEOUT    client wait for a daemon response, seconds (default 300)
  BLADE_IDLE_TIMEOUT   daemon idle before Chrome shuts down, seconds (default 600)
  BLADE_NO_COMPRESS=1  disable response compression
  CHROME_PATH          override the Chrome/Chromium binary

Agents: `bladebro help --json` returns the machine version of this manual —
tool schemas (same as MCP tools/list), per-command usage, exit codes, payload
conventions, and examples. Fetch it once, then drive the CLI with --json.
"#;

/// Canonical command name: resolve aliases to what the user actually types
/// on the CLI, so every help surface accepts the same spellings.
fn normalize_cmd(cmd: &str) -> &str {
    match cmd {
        "navigate" => "nav",
        "-u" => "update",
        "-doc" => "doctor",
        "--rollback" => "rollback",
        "-v" | "--version" => "version",
        _ => cmd,
    }
}

/// Per-command machine help (`help <cmd> --json` and the `commands` map in
/// the full JSON manual).
fn command_help_json(cmd: &str) -> Option<Value> {
    let cmd = normalize_cmd(cmd);
    let v = match cmd {
        "nav" => json!({
            "usage": "bladebro nav <url> [--block <classes>]",
            "tool": "act",
            "args": { "url": "target URL; bare domains get https://" },
            "flags": { "--block": "images,fonts,media,trackers (remembered per domain)" },
            "examples": ["bladebro nav example.com", "bladebro nav https://x.com --block images,fonts --json"],
            "notes": ["returns refs + a content preview — often enough to act without a separate see"]
        }),
        "see" => json!({
            "usage": "bladebro see [mode] [url] [extract <type>] [flags]",
            "tool": "see",
            "args": {
                "mode": "model (default) | content | outline",
                "url": "optional — navigates first",
                "extract": "auto | links | forms | json (json needs --template)"
            },
            "flags": {
                "--filter": "role filter (model mode)",
                "--find": "search by text → refs",
                "--scope": "ref id — read one element's subtree",
                "--content": "include page text in model mode",
                "--budget": "max chars (default 8000)",
                "--limit": "max extract items (default 50; Reddit post comments: all up to 1000 unless set)",
                "--logs": "console | network",
                "--template": "JSON or @file — for extract=json"
            },
            "examples": [
                "bladebro see model",
                "bladebro see content example.com",
                "bladebro see extract auto --limit 20",
                "bladebro see --find \"Submit\"",
                "bladebro see --logs network"
            ],
            "notes": ["extract=auto is the first move on list/search/product/profile pages — one call returns structured items; on Reddit post pages it returns the full comment tree (every reply, complete flag)"]
        }),
        "act" => json!({
            "usage": "bladebro act <action> [target] [value] [--flags]",
            "tool": "act",
            "actions": ["click","type","fill","select","clear","press","scroll","hover","navigate","upload","download","wait","eval","collect","read","batch","pdf","back","forward","reload","save","load","open-tab","close-tab","switch-tab"],
            "universal_flags": {
                "--ref": "element ref (self-heals)",
                "--label": "field label",
                "--text": "value — text to type, file path (upload), wait match value",
                "--role": "role filter for text/label resolution",
                "--nth": "1-based pick among matches",
                "--key": "press key",
                "--url": "navigate first (any action), or the URL for navigate/download/collect",
                "--option": "select option",
                "--condition": "wait condition",
                "--timeout": "seconds",
                "--dx/--dy": "scroll distance",
                "--js": "eval expression (@file or - for scripts)",
                "--submit": "fill: submit button ref or text",
                "--slim": "skip the delta"
            },
            "examples": [
                "bladebro act click e5",
                "bladebro act click \"Sign in\"",
                "bladebro act type e12 \"hello\"",
                "bladebro act fill '{\"e3\":\"John\"}' --submit e8",
                "bladebro act click Submit --url example.com/login",
                "bladebro act upload e5 /tmp/file.pdf",
                "bladebro act batch @steps.json",
                "bladebro act wait settle",
                "bladebro act eval \"document.title\""
            ],
            "notes": [
                "unquoted multi-word values join: act click Sign in == act click \"Sign in\"",
                "big JSON payloads: @file or - (stdin)",
                "batch steps may include {\"action\":\"see\",...} to read inline"
            ]
        }),
        "state" => json!({
            "usage": "bladebro state <op> [args] [flags]",
            "tool": "state",
            "ops": ["cookies","set-cookie","del-cookie","ls","ss","set-ls","set-ss","rm-ls","rm-ss","clear-ls","clear-ss","tabs","open-tab","close-tab","switch-tab","save","load","compress","block"],
            "flags": {
                "--url": "cookie scope / cookies filter / open-tab url",
                "--domain": "cookie domain",
                "--path": "cookie path",
                "--secure": "secure cookie",
                "--http-only": "httpOnly cookie",
                "--same-site": "Strict | Lax | None",
                "--target-id": "close-tab / switch-tab id",
                "--clear": "block: stop blocking",
                "--mode": "compress: on | off | status"
            },
            "examples": [
                "bladebro state cookies",
                "bladebro state cookies example.com",
                "bladebro state set-cookie token abc --domain example.com",
                "bladebro state tabs",
                "bladebro state save my-session",
                "bladebro state block images,fonts",
                "bladebro state compress off"
            ],
            "notes": ["save/load persists cookies+storage; load then navigate to the site"]
        }),
        "run" => json!({
            "usage": "bladebro run '<json-steps>' | @file | -",
            "tool": "run",
            "steps": "array; each {action:...} uses the same fields as act; plus {action:'if'|'while'|'see'}",
            "examples": [
                "bladebro run '[{\"action\":\"navigate\",\"url\":\"example.com\"},{\"action\":\"see\",\"mode\":\"content\"}]'",
                "bladebro run @steps.json",
                "cat steps.json | bladebro run -"
            ],
            "notes": ["stops on first error with the step number + page state", "while+see reads across pages in ONE call"]
        }),
        "vision" => json!({
            "usage": "bladebro vision [--marks]",
            "tool": "vision",
            "flags": { "--marks": "numbered ref badges (Set-of-Marks)" },
            "examples": ["bladebro vision", "bladebro vision --marks --json | jq -r .image_path"],
            "notes": ["screenshot is saved to a file; path printed (json: image_path)", "last resort — see/act are cheaper and return actionable refs"]
        }),
        "daemon" => json!({
            "usage": "bladebro daemon",
            "tool": null,
            "examples": [],
            "notes": ["auto-starts on the first command; run manually only to watch startup errors"]
        }),
        "stop" => json!({
            "usage": "bladebro stop",
            "tool": null,
            "examples": [],
            "notes": ["graceful (flushes logins + knowledge); idempotent — exit 0 when nothing is running"]
        }),
        "help" => json!({
            "usage": "bladebro help [command] [--json]",
            "tool": null,
            "examples": ["bladebro help --json", "bladebro help act"],
            "notes": ["the single self-teaching surface — fetch 'help --json' once"]
        }),
        "mcp" => json!({
            "usage": "bladebro mcp",
            "tool": null,
            "examples": ["bladebro mcp"],
            "notes": [
                "MCP server on stdio — the integration surface for AI agents (Claude, Cursor, opencode, pi)",
                "client config: {\"command\":\"bladebro\",\"args\":[\"mcp\"]}",
                "Unix defaults to the zero-port pipe transport; BLADE_TRANSPORT=ws forces WebSocket"
            ]
        }),
        "audit" => json!({
            "usage": "bladebro audit",
            "tool": null,
            "examples": ["bladebro audit"],
            "notes": ["stealth audit — 36-vector local suite + boot self-check scorecard; 36/36 is the bar"]
        }),
        "update" => json!({
            "usage": "bladebro update | -u [--check] [--force]",
            "tool": null,
            "flags": {
                "--check / -c": "dry run — check only",
                "--force / -f": "reinstall even at the same version"
            },
            "examples": ["bladebro -u --check", "bladebro -u"],
            "notes": [
                "download → verify → back up → swap, from GitHub releases",
                "npm installs are detected and redirected to `npm update -g bladebro` (override: --force)"
            ]
        }),
        "doctor" => json!({
            "usage": "bladebro doctor | -doc",
            "tool": null,
            "examples": ["bladebro doctor"],
            "notes": ["13 system checks: data root, Chrome, Xvfb, profile, logins, hygiene, locks, network, binary, disk, version"]
        }),
        "rollback" => json!({
            "usage": "bladebro --rollback",
            "tool": null,
            "examples": ["bladebro --rollback"],
            "notes": ["restores the previous binary from <data dir>/backups (written by every self-update)"]
        }),
        "version" => json!({
            "usage": "bladebro -v | --version",
            "tool": null,
            "examples": ["bladebro -v"],
            "notes": ["version + install method + update status (behind / on latest / ahead — never a false 'up to date')"]
        }),
        _ => return None,
    };
    Some(v)
}

/// The MCP tool a command maps to (for schema lookup in help --json).
fn command_tool(cmd: &str) -> Option<&'static str> {
    match cmd {
        "nav" => Some("act"),
        "see" => Some("see"),
        "act" => Some("act"),
        "state" => Some("state"),
        "run" => Some("run"),
        "vision" => Some("vision"),
        _ => None,
    }
}

/// The machine manual — `bladebro help --json` (optionally per command).
/// One call teaches an agent everything the CLI can do: the same depth as
/// MCP's tools + instructions, plus CLI-only details (exit codes, stdin/@file
/// payloads, universal flags, examples).
pub fn help_json(cmd: Option<&str>) -> Result<String> {
    let tools = crate::mcp::tools::tools_to_json();

    if let Some(name) = cmd {
        let norm = if name == "navigate" { "nav" } else { name };
        let detail = command_help_json(norm).ok_or_else(|| {
            BladeError::Usage(format!(
                "unknown command '{name}' — run 'bladebro help' for the command list"
            ))
        })?;
        let mut out = json!({ "command": norm, "detail": detail });
        if let Some(t) = command_tool(norm) {
            if let Some(def) = tools
                .iter()
                .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(t))
            {
                out["tool_definition"] = def.clone();
            }
        }
        return Ok(format!("{out}"));
    }

    let mut commands = serde_json::Map::new();
    for c in [
        "nav", "see", "act", "state", "run", "vision", "daemon", "stop", "help",
        "mcp", "audit", "update", "doctor", "rollback", "version",
    ] {
        if let Some(d) = command_help_json(c) {
            commands.insert(c.to_string(), d);
        }
    }

    let out = json!({
        "bladebro": { "version": env!("CARGO_PKG_VERSION"), "surface": "cli" },
        "instructions": INSTRUCTIONS,
        "quick_start": [
            "bladebro nav example.com",
            "bladebro see extract auto",
            "bladebro act click \"Sign in\"",
            "bladebro act fill '{\"e12\":\"John\"}' --submit e20",
            "bladebro run '[{\"action\":\"navigate\",\"url\":\"x.com\"},{\"action\":\"see\",\"mode\":\"content\"}]'",
            "bladebro stop"
        ],
        "commands": commands,
        "tools": tools,
        "output": {
            "human": "result text on stdout; diagnostics on stderr",
            "--json": "ONE JSON object on stdout: {\"ok\":bool,\"is_error\":bool,\"text\":string} (+ \"image_path\" for vision)",
            "payloads": {
                "@file": "read the argument from a file (steps, fields, templates, js)",
                "-": "read from stdin",
                "example": "bladebro run @steps.json  |  cat steps.json | bladebro run -"
            }
        },
        "exit_codes": {
            "0": "success",
            "1": "command ran but failed — details in text (includes page state)",
            "2": "usage error — bad command/flag/argument; the message says how to fix it"
        },
        "env": {
            "BLADE_HOME": "isolate daemon/Chrome/sessions/knowledge in a directory",
            "BLADE_CMD_TIMEOUT": "client wait for a daemon response, seconds (default 300)",
            "BLADE_IDLE_TIMEOUT": "daemon idle before Chrome shuts down, seconds (default 600)",
            "BLADE_NO_COMPRESS": "set 1 to disable response compression",
            "BLADE_TRANSPORT": "mcp: 'ws' forces the WebSocket transport",
            "CHROME_PATH": "override the Chrome/Chromium binary",
            "RUST_LOG": "log filter (default warn,bladebro=info)"
        }
    });
    Ok(format!("{out}"))
}

/// Detailed per-command human help (`help <cmd>`, `<cmd> --help`).
pub fn command_help_text(cmd: &str) -> Option<String> {
    let cmd = normalize_cmd(cmd);
    let text = match cmd {
        "nav" => r#"bladebro nav — navigate

USAGE
  bladebro nav <url> [--block <classes>] [--json]

Returns the page title/URL, interactive element refs, and a content preview —
usually enough to act without a separate see call. Bare domains work:
`bladebro nav example.com` → https://example.com. The first command
auto-starts the daemon (one Chrome shared by all later commands).

--block <classes>   block inert resources for this load AND remember the
                    choice for the domain: images,fonts,media,trackers

EXAMPLES
  bladebro nav example.com
  bladebro nav https://x.com --block images,fonts
  bladebro nav example.com --json | jq .text
"#,
        "see" => r#"bladebro see — read the page without acting

USAGE
  bladebro see [mode] [url] [extract <type>] [flags]

MODES
  model (default)   interactive elements with refs (e1, e2, …)
  content           page text as clean markdown (reading articles/docs)
  outline           heading hierarchy only (cheapest read)

EXTRACTION
  extract auto      structured items in ONE call — lists, search results,
                    products, posts. Site-aware: Reddit POST pages → the
                    full comment tree (every reply incl. collapsed, thread
                    order, complete flag); feeds/GitHub/products get fields.
  extract links     all links
  extract forms     all forms with fields
  extract json      custom template (--template '{"items":{…}}' or @file)

FLAGS
  --filter <role>     only elements of a role (button, link, textbox, …)
  --find <text>       search by text → refs
  --scope <ref>       read one element's subtree
  --content           include text in model mode
  --budget <N>        max response chars (default 8000)
  --limit <N>         max extract items (default 50; Reddit comments: all ≤1000)
  --logs console|network
  --url <url>         navigate first (or pass the URL positionally)

EXAMPLES
  bladebro see                            interactive elements
  bladebro see content                    page as markdown
  bladebro see extract auto --limit 20    structured items
  bladebro see example.com content        navigate + read in one call
  bladebro see --find "Submit"            refs by text
  bladebro see --logs network             recent requests
"#,
        "act" => r#"bladebro act — interact with the page

USAGE
  bladebro act <action> [target] [value] [--flags]

ACTIONS
  click <ref|label>        type <target> <text>      fill <fields> [--submit]
  select <target> <option> clear <ref>               press <key>
  scroll <dx> <dy>         hover <target>            navigate <url> [--block]
  upload <target> <path>   download <url> [--path]  wait <condition> [value]
  eval <js> [--ref]        collect <url> [--max N]   read <ref>
  batch <steps|@file|->    pdf [--path] [--landscape]
  back / forward / reload  save <name> / load <name>
  open-tab [url] / switch-tab <id> / close-tab <id>

UNIVERSAL FLAGS (mirror the MCP schema — accepted on every action)
  --ref --label --text --role --nth --key --url --option --condition --timeout
  --dx --dy --js --submit --block --slim   (per-action: --path, --max, --x/--y…)

Multi-word values don't need quotes: 'act click Sign in' works. Big JSON
payloads: @file or - (stdin). Unknown flags fail loudly (exit 2).

EXAMPLES
  bladebro act click e5                          click a ref
  bladebro act click "Sign in"                   click by text
  bladebro act type e12 "hello world"            type
  bladebro act fill '{"e3":"John","e5":"Doe"}' --submit e8
  bladebro act click Submit --url example.com/login   navigate + click in ONE call
  bladebro act upload e5 /tmp/file.pdf           file upload
  bladebro act wait settle                       wait for DOM quiet
  bladebro act wait element --text "Results"     wait for an element
  bladebro act eval "document.title"             evaluate JS
  bladebro act batch '[{"action":"reload"}]'     sequential steps in ONE call
  bladebro act collect https://x.com/list --max 100   infinite-scroll collect
"#,
        "state" => r#"bladebro state — cookies, storage, tabs, sessions, blocking

USAGE
  bladebro state <op> [args] [flags]

COOKIES
  cookies [url]                        list (filtered to url / current page)
  set-cookie <name> <value> [flags]    flags: --url --domain --path --secure
                                       --http-only --same-site Strict|Lax|None
  del-cookie <name> [--url|--domain]

STORAGE
  ls / ss                              list localStorage / sessionStorage
  set-ls <key> <value> · set-ss <key> <value>
  rm-ls <key> · rm-ss <key> · clear-ls · clear-ss

TABS
  tabs                                 list (* = current)
  open-tab <url>                       open + auto-switch
  switch-tab <id> · close-tab <id>     ids come from 'tabs'

SESSIONS / CONTROL
  save <name> · load <name>            persist/restore cookies+storage
  block [classes] · block clear        image/font/media/tracker blocking
  compress on|off|status               context pruning

EXAMPLES
  bladebro state cookies
  bladebro state set-cookie token abc --domain example.com --secure
  bladebro state tabs
  bladebro state save my-session
"#,
        "run" => r#"bladebro run — batch actions with branching and loops

USAGE
  bladebro run '<json-steps>'      (or @file / - for stdin)

Steps are action objects (same fields as act) plus:
  {"action":"if","condition":…,"then":[…],"else":[…]}   branch
  {"action":"while","condition":…,"steps":[…],"max":N}  loop
  {"action":"see",…}        read inline — while+see reads across pages in ONE call
  state ops (open-tab, save, load, …) work as steps too

Stops on the first error and returns the step number + page state.
Conditions: element, title, url, text, settle, js.

EXAMPLES
  bladebro run '[{"action":"navigate","url":"example.com"},{"action":"see","mode":"content"}]'
  bladebro run @steps.json
  cat steps.json | bladebro run -
"#,
        "vision" => r#"bladebro vision — screenshot

USAGE
  bladebro vision [--marks]

Always saves a PNG to a file and prints the path (--json: "image_path").
--marks overlays numbered ref badges (Set-of-Marks) so you can click by ref
after looking at the image. Vision is the LAST RESORT: the structural model
(see/act with refs) is cheaper and gives actionable refs.

EXAMPLES
  bladebro vision
  bladebro vision --marks --json | jq -r .image_path
"#,
        "daemon" => r#"bladebro daemon — persistent Chrome session

USAGE
  bladebro daemon

Auto-starts on the first command — run it manually only to watch startup
errors. One Chrome instance serves every later command (Unix socket under the
data dir; socket + pid file are 0600). Idle timeout: BLADE_IDLE_TIMEOUT
seconds (default 600), then Chrome shuts down; the next command relaunches it.
"#,
        "stop" => r#"bladebro stop — shut the daemon down

USAGE
  bladebro stop

Graceful: flushes logins + domain knowledge, kills Chrome, removes the
socket. Idempotent — exit 0 even when nothing is running. If the daemon is
wedged, falls back to terminating the pid from the pid file.
"#,
        "help" => r#"bladebro help — the single self-teaching surface

USAGE
  bladebro help              this manual
  bladebro help <command>    one command in depth
  bladebro help --json       the machine manual: tool schemas (same as MCP
                             tools/list), per-command usage, exit codes,
                             payload conventions, examples — call ONCE and
                             you know the whole CLI
  bladebro help <cmd> --json per-command machine help
"#,
        "mcp" => r#"bladebro mcp — MCP server (stdio JSON-RPC)

USAGE
  bladebro mcp

The integration surface for AI agents. Client config:

  {"mcpServers": {"bladebro": {"command": "bladebro", "args": ["mcp"]}}}

Speaks MCP 2024-11-05 through 2026-07-28 (legacy initialize handshake +
the 2026-07-28 stateless dialect). Unix defaults to the zero-port pipe
transport (no scannable debugging port); BLADE_TRANSPORT=ws forces
WebSocket. Chrome launches lazily on the first tool call.
"#,
        "audit" => r#"bladebro audit — stealth audit

USAGE
  bladebro audit

Runs the 36-vector local suite (tests/vectors.html) plus a boot
self-check (webdriver, cdc_, plugins, toString integrity) and prints a
scorecard. Run it after any stealth-affecting change; 36/36 is the bar.
"#,
        "update" => r#"bladebro -u / update — self-update

USAGE
  bladebro -u [--check | -c] [--force | -f]

Download → verify (magic + size + executes) → back up → swap, from
GitHub releases. --check is a dry run; --force reinstalls the current
version. npm installs are detected (path contains node_modules) and
redirected to `npm update -g bladebro`; --force overrides. After an
update: restart your MCP client. Regret it: bladebro --rollback.
"#,
        "doctor" => r#"bladebro doctor / -doc — system diagnostics

USAGE
  bladebro doctor

13 checks: data directory, Chrome (+version), Xvfb, profile dir, login
persistence, profile hygiene, stale locks, GitHub reachability, binary
integrity, disk space, version-vs-latest. Failures print a fix.
"#,
        "rollback" => r#"bladebro --rollback — restore the previous binary

USAGE
  bladebro --rollback

Restores the most recent backup from <data dir>/backups (written by every
self-update). The backup is verified as a valid binary first — a corrupted
newest backup falls through to the next one.
"#,
        "version" => r#"bladebro -v / --version — version + update status

USAGE
  bladebro -v

Version + build id, install method (npm / source / binary) and update
state: behind (with the hint), on the latest release, or ahead of it
(dev builds). Never a misleading "up to date" when the check failed.
"#,
        _ => return None,
    };
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn tmp_file(tag: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("bt-cli-{}-{}.tmp", tag, std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    // ── endpoint extraction (issue #16) ────────────────────────────────

    #[test]
    fn port_maps_to_default_host_endpoint() {
        let (cleaned, external) = extract_endpoint(&[
            "state".into(), "tabs".into(), "--port".into(), "9223".into(),
        ]);
        assert_eq!(cleaned, vec!["state".to_string(), "tabs".to_string()]);
        assert_eq!(external.as_deref(), Some("127.0.0.1:9223"));
    }

    #[test]
    fn explicit_host_combines_with_port() {
        let (cleaned, external) = extract_endpoint(&[
            "--host".into(), "192.168.1.50".into(),
            "see".into(), "content".into(), "--port".into(), "9222".into(),
        ]);
        assert_eq!(cleaned, vec!["see".to_string(), "content".to_string()]);
        assert_eq!(external.as_deref(), Some("192.168.1.50:9222"));
    }

    #[test]
    fn no_port_means_no_external_endpoint() {
        let (cleaned, external) = extract_endpoint(&[
            "state".into(), "cookies".into(), "--host".into(), "127.0.0.1".into(),
        ]);
        assert_eq!(cleaned, vec!["state".to_string(), "cookies".to_string()]);
        assert!(external.is_none(), "host alone must not pin an endpoint");
    }

    #[test]
    fn flags_before_command_still_parse() {
        let (cleaned, external) = extract_endpoint(&[
            "--port".into(), "9333".into(), "vision".into(), "--marks".into(),
        ]);
        assert_eq!(cleaned, vec!["vision".to_string(), "--marks".to_string()]);
        assert_eq!(external.as_deref(), Some("127.0.0.1:9333"));
    }

    // ── act parsing ────────────────────────────────────────────────────

    #[test]
    fn act_click_by_ref_and_by_label() {
        let v = parse_act_args(&a(&["click", "e5"])).unwrap();
        assert_eq!(v["action"], "click");
        assert_eq!(v["ref"], "e5");

        let v = parse_act_args(&a(&["click", "Sign", "in"])).unwrap();
        assert_eq!(v["label"], "Sign in");
        assert!(v.get("ref").is_none());
    }

    #[test]
    fn act_click_flags_win() {
        let v = parse_act_args(&a(&["click", "--ref", "e7", "--nth", "2"])).unwrap();
        assert_eq!(v["ref"], "e7");
        assert_eq!(v["nth"], 2);
    }

    #[test]
    fn act_type_joins_unquoted_text() {
        let v = parse_act_args(&a(&["type", "e12", "hello", "world"])).unwrap();
        assert_eq!(v["ref"], "e12");
        assert_eq!(v["text"], "hello world");

        let v = parse_act_args(&a(&["type", "--ref", "e5", "hi", "there"])).unwrap();
        assert_eq!(v["ref"], "e5");
        assert_eq!(v["text"], "hi there");
    }

    #[test]
    fn act_type_requires_text_and_target() {
        assert!(parse_act_args(&a(&["type", "e5"])).is_err());
        assert!(parse_act_args(&a(&["type"])).is_err());
    }

    #[test]
    fn act_fill_normalizes_object_fields() {
        let v = parse_act_args(&a(&["fill", "{\"e3\":\"John\",\"e5\":\"Doe\"}"])).unwrap();
        let fields = v["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields.iter().any(|f| f["ref"] == "e3" && f["text"] == "John"));
    }

    #[test]
    fn act_fill_accepts_array_and_rejects_garbage() {
        let arr = "[{\"ref\":\"e1\",\"text\":\"x\"}]";
        let v = parse_act_args(&a(&["fill", arr])).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 1);
        assert!(parse_act_args(&a(&["fill", "not-json"])).is_err());
    }

    #[test]
    fn act_fill_object_with_reserved_keys_is_one_field_spec() {
        // {"label":"Email","text":"x"} means ONE field addressed by
        // label — it used to be misread as a ref-map entry named "label"
        // and failed live with "stale ref: label".
        let v = parse_act_args(&a(&["fill", "{\"label\":\"Email\",\"text\":\"x\"}"])).unwrap();
        let fields = v["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["label"], "Email");
        assert_eq!(fields[0]["text"], "x");

        let v = parse_act_args(&a(&["fill", "{\"ref\":\"e3\",\"text\":\"y\"}"])).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 1);
        assert_eq!(v["fields"][0]["ref"], "e3");
    }

    #[test]
    fn act_fill_from_file() {
        let path = tmp_file("fill", "{\"e9\":\"from-file\"}");
        let arg = format!("@{}", path.display());
        let v = parse_act_args(&a(&["fill", &arg])).unwrap();
        assert_eq!(v["fields"][0]["text"], "from-file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn act_upload_sends_path_in_text() {
        // Regression: the MCP handler reads the file path from `text`; the
        // old CLI put it in `path`, so every CLI upload arrived empty.
        let v = parse_act_args(&a(&["upload", "e5", "/tmp/f.pdf"])).unwrap();
        assert_eq!(v["text"], "/tmp/f.pdf");
        assert!(v.get("path").is_none());

        let v = parse_act_args(&a(&["upload", "--ref", "e5", "--path", "/tmp/g.png"])).unwrap();
        assert_eq!(v["text"], "/tmp/g.png");
    }

    #[test]
    fn act_navigate_requires_exactly_one_url() {
        let v = parse_act_args(&a(&["navigate", "example.com", "--block", "images"])).unwrap();
        assert_eq!(v["url"], "example.com");
        assert_eq!(v["block"], "images");
        assert!(parse_act_args(&a(&["navigate"])).is_err());
        assert!(parse_act_args(&a(&["navigate", "a.com", "b.com"])).is_err());
    }

    #[test]
    fn act_wait_validates_condition_and_text() {
        let v = parse_act_args(&a(&["wait", "js", "document.title"])).unwrap();
        assert_eq!(v["condition"], "js");
        assert_eq!(v["text"], "document.title");

        let v = parse_act_args(&a(&["wait", "settle"])).unwrap();
        assert_eq!(v["condition"], "settle");

        assert!(parse_act_args(&a(&["wait", "text"])).is_err());
        assert!(parse_act_args(&a(&["wait", "bogus", "x"])).is_err());
    }

    #[test]
    fn act_scroll_numbers_and_negatives() {
        let v = parse_act_args(&a(&["scroll", "0", "-500"])).unwrap();
        assert_eq!(v["dx"], 0);
        assert_eq!(v["dy"], -500);
        assert!(parse_act_args(&a(&["scroll", "down"])).is_err());
        assert!(parse_act_args(&a(&["scroll"])).is_err());
    }

    #[test]
    fn act_batch_parses_steps_and_rejects_non_array() {
        let v = parse_act_args(&a(&["batch", "[{\"action\":\"reload\"}]"])).unwrap();
        assert_eq!(v["steps"][0]["action"], "reload");
        assert!(parse_act_args(&a(&["batch", "{}"])).is_err());
        assert!(parse_act_args(&a(&["batch", "[]"])).is_err());
        assert!(parse_act_args(&a(&["batch"])).is_err());
    }

    #[test]
    fn act_tabs_save_load_and_read() {
        let v = parse_act_args(&a(&["open-tab", "https://x.com"])).unwrap();
        assert_eq!(v["url"], "https://x.com");
        let v = parse_act_args(&a(&["switch-tab", "ABC123"])).unwrap();
        assert_eq!(v["target_id"], "ABC123");
        let v = parse_act_args(&a(&["read", "e5"])).unwrap();
        assert_eq!(v["ref"], "e5");
        let v = parse_act_args(&a(&["save", "me"])).unwrap();
        assert_eq!(v["name"], "me");
        assert!(parse_act_args(&a(&["switch-tab"])).is_err());
    }

    #[test]
    fn act_rejects_unknown_flags_and_actions_loudly() {
        let e = parse_act_args(&a(&["click", "e5", "--bogus", "x"])).unwrap_err();
        assert!(e.to_string().contains("--bogus"));
        let e = parse_act_args(&a(&["clik", "e5"])).unwrap_err();
        assert!(e.to_string().contains("clik"));
    }

    #[test]
    fn act_flag_value_missing_is_loud() {
        assert!(parse_act_args(&a(&["click", "--ref"])).is_err());
        assert!(parse_act_args(&a(&["click", "e5", "--nth", "abc"])).is_err());
    }

    #[test]
    fn act_eval_joins_and_reads_files() {
        let v = parse_act_args(&a(&["eval", "1", "+", "2"])).unwrap();
        assert_eq!(v["js"], "1 + 2");

        let path = tmp_file("eval", "6*7");
        let arg = format!("@{}", path.display());
        let v = parse_act_args(&a(&["eval", &arg])).unwrap();
        assert_eq!(v["js"], "6*7");
        let _ = std::fs::remove_file(&path);
    }

    // ── see parsing ────────────────────────────────────────────────────

    #[test]
    fn see_bare_domain_is_a_url_not_a_mode() {
        let v = parse_see_args(&a(&["example.com"])).unwrap();
        assert_eq!(v["url"], "example.com");
        assert!(v.get("mode").is_none());
    }

    #[test]
    fn see_mode_and_url_in_any_order() {
        let v = parse_see_args(&a(&["content", "example.com"])).unwrap();
        assert_eq!(v["mode"], "content");
        assert_eq!(v["url"], "example.com");
        let v = parse_see_args(&a(&["https://x.com", "outline"])).unwrap();
        assert_eq!(v["mode"], "outline");
        assert_eq!(v["url"], "https://x.com");
    }

    #[test]
    fn see_extract_type_attaches() {
        let v = parse_see_args(&a(&["extract", "auto"])).unwrap();
        assert_eq!(v["extract"], "auto");
        let v = parse_see_args(&a(&["--extract", "links", "--limit", "5"])).unwrap();
        assert_eq!(v["extract"], "links");
        assert_eq!(v["limit"], 5);
    }

    #[test]
    fn see_extract_without_type_errors() {
        assert!(parse_see_args(&a(&["extract"])).is_err());
        assert!(parse_see_args(&a(&["--extract", "bogus"])).is_err());
    }

    #[test]
    fn see_rejects_unknown_tokens_and_bad_numbers() {
        assert!(parse_see_args(&a(&["bogus"])).is_err());
        assert!(parse_see_args(&a(&["--budget", "abc"])).is_err());
        assert!(parse_see_args(&a(&["--budjet", "5"])).is_err());
        assert!(parse_see_args(&a(&["--budget"])).is_err());
    }

    #[test]
    fn see_scope_content_and_template() {
        let v = parse_see_args(&a(&["--scope", "e9", "--content"])).unwrap();
        assert_eq!(v["scope"], "e9");
        assert_eq!(v["content"], true);
        let v = parse_see_args(&a(&["--template", "{\"items\":{}}"])).unwrap();
        assert!(v["template"].is_object());
        assert!(parse_see_args(&a(&["--template", "not-json"])).is_err());
    }

    // ── state parsing ──────────────────────────────────────────────────

    #[test]
    fn state_storage_ops_use_the_name_field() {
        // Regression: the handler reads `name` (its MCP schema field).
        // The old code emitted `key`, so every set-ls stored an EMPTY key —
        // live `state ls` showed "=dark".
        let v = parse_state_args(&a(&["rm-ss", "key1"])).unwrap();
        assert_eq!(v["op"], "rm-ss");
        assert_eq!(v["name"], "key1");

        let v = parse_state_args(&a(&["set-ls", "theme", "dark"])).unwrap();
        assert_eq!(v["name"], "theme");
        assert_eq!(v["value"], "dark");
        assert!(v.get("key").is_none());

        let v = parse_state_args(&a(&["set-ss", "--key", "k", "--value", "v"])).unwrap();
        assert_eq!(v["name"], "k");
        assert_eq!(v["value"], "v");
    }

    #[test]
    fn state_set_cookie_full_form() {
        let v = parse_state_args(&a(&[
            "set-cookie", "tok", "abc", "--domain", "example.com", "--secure",
            "--http-only", "--same-site", "Strict",
        ]))
        .unwrap();
        assert_eq!(v["name"], "tok");
        assert_eq!(v["value"], "abc");
        assert_eq!(v["domain"], "example.com");
        assert_eq!(v["secure"], true);
        assert_eq!(v["httpOnly"], true);
        assert_eq!(v["sameSite"], "Strict");
    }

    #[test]
    fn state_missing_values_are_loud() {
        assert!(parse_state_args(&a(&["set-cookie", "tok"])).is_err());
        assert!(parse_state_args(&a(&["set-ls", "k"])).is_err());
        assert!(parse_state_args(&a(&["rm-ls"])).is_err());
        assert!(parse_state_args(&a(&["close-tab"])).is_err());
    }

    #[test]
    fn state_cookies_optional_url_and_block_clear() {
        let v = parse_state_args(&a(&["cookies", "example.com"])).unwrap();
        assert_eq!(v["url"], "example.com");
        let v = parse_state_args(&a(&["block", "clear"])).unwrap();
        assert_eq!(v["clear"], true);
        let v = parse_state_args(&a(&["block", "images,fonts"])).unwrap();
        assert_eq!(v["classes"], "images,fonts");
    }

    #[test]
    fn state_compress_and_unknown_op() {
        let v = parse_state_args(&a(&["compress", "off"])).unwrap();
        assert_eq!(v["mode"], "off");
        assert!(parse_state_args(&a(&["compress", "sometimes"])).is_err());
        assert!(parse_state_args(&a(&["frobnicate"])).is_err());
    }

    // ── run parsing ────────────────────────────────────────────────────

    #[test]
    fn run_accepts_inline_file_and_validates() {
        let v = parse_run_args(&a(&["[{\"action\":\"reload\"}]"])).unwrap();
        assert_eq!(v["steps"][0]["action"], "reload");

        let path = tmp_file("run", "[{\"action\":\"reload\"}]");
        let arg = format!("@{}", path.display());
        let v = parse_run_args(&a(&[&arg])).unwrap();
        assert!(v["steps"].is_array());
        let _ = std::fs::remove_file(&path);

        assert!(parse_run_args(&a(&["{}"])).is_err());
        assert!(parse_run_args(&a(&["[]"])).is_err());
        assert!(parse_run_args(&a(&[])).is_err());
    }

    // ── nav / vision ───────────────────────────────────────────────────

    #[test]
    fn nav_requires_url_and_takes_block() {
        let v = parse_nav_args(&a(&["example.com", "--block", "images"])).unwrap();
        assert_eq!(v["action"], "navigate");
        assert_eq!(v["url"], "example.com");
        assert_eq!(v["block"], "images");
        assert!(parse_nav_args(&a(&[])).is_err());
        assert!(parse_nav_args(&a(&["a.com", "b.com"])).is_err());
    }

    #[test]
    fn vision_only_takes_marks() {
        assert_eq!(parse_vision_args(&a(&["--marks"])).unwrap()["marks"], true);
        assert_eq!(parse_vision_args(&a(&[])).unwrap()["marks"], false);
        assert!(parse_vision_args(&a(&["--bogus"])).is_err());
        assert!(parse_vision_args(&a(&["wat"])).is_err());
    }

    // ── payloads / help ────────────────────────────────────────────────

    #[test]
    fn resolve_payload_variants() {
        assert_eq!(resolve_payload("inline").unwrap(), "inline");
        let path = tmp_file("payload", "from file");
        let arg = format!("@{}", path.display());
        assert_eq!(resolve_payload(&arg).unwrap(), "from file");
        let _ = std::fs::remove_file(&path);
        assert!(resolve_payload("@/nonexistent/definitely-missing").is_err());
    }

    #[test]
    fn help_json_is_complete_and_parses() {
        let raw = help_json(None).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v["instructions"].as_str().unwrap().len() > 200);
        assert_eq!(v["tools"].as_array().unwrap().len(), 5);
        assert!(v["commands"]["act"]["usage"].as_str().is_some());
        assert!(v["commands"]["nav"].is_object());
        assert!(v["commands"]["mcp"].is_object(), "help --json must cover mcp");
        assert!(v["commands"]["doctor"].is_object());
        assert!(v["exit_codes"]["2"].as_str().unwrap().contains("usage"));
        assert!(v["output"]["payloads"]["@file"].as_str().is_some());

        let raw = help_json(Some("act")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["command"], "act");
        assert_eq!(v["tool_definition"]["name"], "act");
        assert!(help_json(Some("nope")).is_err());
    }

    #[test]
    fn every_command_has_help_text_and_json() {
        for c in [
            "nav", "see", "act", "state", "run", "vision", "daemon", "stop", "help",
            "mcp", "audit", "update", "doctor", "rollback", "version",
        ] {
            let text = command_help_text(c).unwrap_or_else(|| panic!("{c} human help"));
            assert!(text.contains("USAGE"), "{c} help lacks USAGE");
            let j = command_help_json(c).unwrap_or_else(|| panic!("{c} json help"));
            assert!(j["usage"].as_str().is_some(), "{c} json lacks usage");
        }
        // Aliases resolve to the canonical command (v3.9.10: `help mcp` etc. work).
        assert!(command_help_text("-u").is_some() && command_help_text("-doc").is_some());
        assert!(command_help_text("--version").is_some() && command_help_text("--rollback").is_some());
        assert!(command_help_json("-u").is_some() && command_help_json("-v").is_some());
        assert!(command_help_text("bogus").is_none());
        assert!(command_help_json("bogus").is_none());
    }

    #[test]
    fn help_text_mentions_version_and_contract() {
        let t = help_text();
        assert!(t.contains(env!("CARGO_PKG_VERSION")));
        assert!(t.contains("EXIT CODES"));
        assert!(t.contains("help --json"));
    }
}
