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
//!
//! Module map: `args` (endpoint extraction + the six per-command parsers in
//! `args/{nav,see,act,state,run,vision}`), `daemon` (socket server +
//! lifecycle), `rb` (the real-browser lane switch), `help` (help text + typo
//! suggestions). This file is the shared core: dispatch, the daemon client,
//! and the daemon/one-shot run paths.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::mcp::server;
use crate::page::Page;

mod args;
mod daemon;
mod help;
mod hermes;
mod rb;

use self::args::{
    extract_endpoint, parse_act_args, parse_nav_args, parse_run_args, parse_see_args,
    parse_state_args, parse_vision_args,
};
use self::daemon::{run_daemon, stop_daemon};
pub use self::help::{command_help_text, help_json, help_text, suggest_command};
use self::rb::run_rb;

/// Unix socket path for the CLI daemon.
#[cfg(unix)]
fn socket_path() -> std::path::PathBuf {
    crate::platform::blade_dir().join("cli.sock")
}

/// Trust boundary for the daemon channel. A connect-success test proves
/// only that *something* is listening at `cli.sock`; the channel carries
/// every tool request and authors every response the CLI prints, yet would
/// otherwise be authenticated solely by the state dir's permissions — a
/// local co-user with write access to a relocated/insecure state dir can
/// bind a listener at the path, lock the real daemon out ("already
/// running"), capture every `{"tool","args"}` request, and author every
/// response. Trust the socket only when its parent directory is a real
/// directory (no symlink), owned by this user, with no group/world write
/// bits.
#[cfg(unix)]
fn socket_dir_trusted() -> bool {
    socket_dir_trusted_at(&socket_path())
}

#[cfg(unix)]
fn socket_dir_trusted_at(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dir = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    match std::fs::symlink_metadata(dir) {
        Ok(md) => md.is_dir() && md.uid() == unsafe { libc::geteuid() } && (md.mode() & 0o022) == 0,
        Err(_) => false,
    }
}

/// True when the connected daemon socket's peer runs as this user
/// (SO_PEERCRED on Linux, getpeereid on macOS). The dir-trust check is the
/// primary gate; this rejects a same-name impostor even if that check was
/// passed on a loosened tree.
#[cfg(unix)]
fn peer_uid_matches_self(fd: std::os::fd::RawFd) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut creds = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut creds as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        rc == 0 && creds.uid == unsafe { libc::geteuid() }
    }
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        rc == 0 && uid == unsafe { libc::geteuid() }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        true
    }
}

/// Result of a tool dispatch — shared between CLI and daemon.
pub struct ToolResult {
    pub text: String,
    pub image: Option<String>,
    pub is_error: bool,
}

/// Dispatch a tool call to the same handlers the MCP server uses.
/// This is the shared core — any handler update auto-propagates to CLI.
pub async fn dispatch(
    tool: &str,
    args: &Value,
    page: &mut Page,
) -> std::result::Result<ToolResult, BladeError> {
    // For see with URL: navigate first, then read. Any non-flag token is a
    // URL; the shared navigate adds the missing scheme.
    if tool == "see" {
        if let Some(url) = args.get("url").and_then(|u| u.as_str()) {
            if !url.is_empty() && !url.starts_with("--") {
                // Manual-control pause: a see with a url navigates first.
                if crate::realbrowser::input_paused() {
                    return Err(crate::realbrowser::paused_error());
                }
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
        return Ok(ToolResult {
            text: text.to_string(),
            image,
            is_error,
        });
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
                text.push_str("\n\u{26a0} dialogs handled:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    let how = if d.via == "expectation" {
                        " (via armed expectation)"
                    } else {
                        ""
                    };
                    text.push_str(&format!(
                        "  {} \"{}\" \u{2014} {}{}\n",
                        d.kind, d.message, action, how
                    ));
                    if let Some(n) = &d.note {
                        text.push_str(&format!("    ({n})\n"));
                    }
                }
            }
            if let Some(note) = page.drain_dialog_expect_note() {
                text.push_str(&format!("\n\u{26a0} {note}\n"));
            }
            let ambient = page.drain_ambient();
            for a in &ambient {
                text.push_str(&format!("\u{26a0} {}\n", a));
            }
            Ok(ToolResult {
                text,
                image: None,
                is_error: false,
            })
        }
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => {
            let mut text = format!("\u{2717} error: {e}");
            let dialogs = page.drain_dialogs();
            if !dialogs.is_empty() {
                text.push_str("\n\n\u{26a0} dialogs handled:\n");
                for d in &dialogs {
                    let action = if d.accepted { "accepted" } else { "cancelled" };
                    let how = if d.via == "expectation" {
                        " (via armed expectation)"
                    } else {
                        ""
                    };
                    text.push_str(&format!(
                        "  {} \"{}\" \u{2014} {}{}\n",
                        d.kind, d.message, action, how
                    ));
                    if let Some(n) = &d.note {
                        text.push_str(&format!("    ({n})\n"));
                    }
                }
            }
            if let Some(note) = page.drain_dialog_expect_note() {
                text.push_str(&format!("\n\u{26a0} {note}\n"));
            }
            Ok(ToolResult {
                text,
                image: None,
                is_error: true,
            })
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
    // Trust boundary: connect-success alone proves only that SOMETHING
    // listens — a co-user listener placed through a loose state dir would
    // otherwise be believed, locking the genuine daemon out and authoring
    // every response the CLI prints.
    if !socket_dir_trusted() {
        return false;
    }
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
    {
        use std::os::fd::AsRawFd;
        if !peer_uid_matches_self(stream.as_raw_fd()) {
            return Err(BladeError::Other(
                "refusing the daemon channel: the socket peer is not this user".into(),
            ));
        }
    }

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
            return Err(BladeError::Other(format!(
                "failed to read daemon response: {e}"
            )));
        }
    }

    let v: Value = serde_json::from_str(&resp)
        .map_err(|e| BladeError::Other(format!("invalid daemon response: {e}")))?;

    let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
    if !ok {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error");
        return Err(BladeError::Other(err.to_string()));
    }

    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let image = v.get("image").and_then(|i| i.as_str()).map(String::from);
    let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    Ok(ToolResult {
        text,
        image,
        is_error,
    })
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
    let (args, external) = match extract_endpoint(args) {
        Ok(v) => v,
        Err(e) => {
            if json_mode {
                println!(
                    "{}",
                    json!({ "ok": false, "is_error": true, "text": e.to_string() })
                );
            }
            return Err(e);
        }
    };
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
        "rb" | "realbrowser" => run_rb(rest, json_mode).await,
        "hermes" => hermes::run(rest, json_mode).await,
        "daemon" => run_daemon().await,
        "stop" => {
            let text = stop_daemon().await?;
            print_result(
                &ToolResult {
                    text,
                    image: None,
                    is_error: false,
                },
                json_mode,
            );
            Ok(())
        }
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

    // One-shot: launch (or attach to) the lane's browser, run, exit.
    // Use a guard to ensure a browser WE OWN is ALWAYS shut down, even on
    // error. The real lane's attach mechanism owns nothing — nothing to shut.
    let (browser, base) = crate::browser::launch_lane().await?;

    // `external = real_lane`: on the real browser we never restore saved
    // logins, warm the profile, or snapshot cookies — it is not ours.
    let result = run_connected(tool, args, &base, crate::realbrowser::real_lane()).await;

    // ALWAYS shut down Chrome, even if the above failed.
    // Without this, any error between launch and shutdown orphans Chrome + Xvfb.
    if let Some(browser) = browser {
        let _ = tokio::task::spawn_blocking(move || browser.shutdown()).await;
    }

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
    let mut page = Page::attach(crate::cdp::CdpSession::root(client), base, None).await?;

    if !external {
        // Re-inject saved logins before anything navigates.
        crate::logins::restore(page.cdp_ref()).await?;

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
        if let Err(e) = crate::logins::snapshot(page.cdp_ref()).await {
            eprintln!("[bladebro] login snapshot failed: {e}");
        }
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
        base64_decode(img)
            .and_then(|data| crate::artifacts::write_artifact_bytes(&data, "png").ok())
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
        println!("{}\nsaved: {}", crate::ui::style_report(&result.text), p);
    } else {
        println!("{}", crate::ui::style_report(&result.text));
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

    let bytes: Vec<u8> = s
        .bytes()
        .filter(|&b| b != b'\n' && b != b'\r' && b != b' ')
        .collect();
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
        match tokio::time::timeout(std::time::Duration::from_secs(4), page.navigate(url)).await {
            Ok(Ok(_)) => {
                ok += 1;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            _ => continue,
        }
    }
    if ok > 0 {
        eprintln!(
            "[bladebro] profile warmed ({ok}/{} sites visited)",
            sites.len()
        );
    }
}
