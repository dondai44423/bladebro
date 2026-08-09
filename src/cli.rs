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
//! `--json` flag: structured output for AI agents. Human-readable by default.


use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::Page;
use crate::mcp::server;

/// Unix socket path for the CLI daemon.
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
    // For see with URL: navigate first, then read.
    if tool == "see" {
        if let Some(url) = args.get("url").and_then(|u| u.as_str()) {
            if !url.is_empty() && (url.starts_with("http") || url.starts_with("data:") || url.starts_with("file:")) {
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

/// Check if the daemon is running by trying to connect to the socket.
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
#[cfg(unix)]
fn send_to_daemon(tool: &str, args: &Value) -> Result<ToolResult> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path())
        .map_err(|e| BladeError::Other(format!("daemon not running: {e}")))?;

    let req = serde_json::to_string(&json!({ "tool": tool, "args": args }))?;
    writeln!(stream, "{req}")?;
    stream.flush()?;

    let mut resp = String::new();
    stream.read_to_string(&mut resp)
        .map_err(|e| BladeError::Other(format!("failed to read daemon response: {e}")))?;

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
pub async fn run_cli(args: &[String]) -> Result<()> {
    let json_mode = args.iter().any(|a| a == "--json");
    let no_daemon = args.iter().any(|a| a == "--no-daemon");
    let args: Vec<String> = args.iter()
        .filter(|a| a != &"--json" && a != &"--no-daemon")
        .cloned()
        .collect();

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    let rest = &args[1.min(args.len())..];

    match cmd {
        "daemon" => run_daemon().await,
        "stop" => stop_daemon().await,
        "nav" => {
            let url = rest.first().cloned().unwrap_or_default();
            let args = json!({ "action": "navigate", "url": url });
            run_tool("act", &args, json_mode, no_daemon).await
        }
        "see" => {
            let parsed = parse_see_args(rest);
            run_tool("see", &parsed, json_mode, no_daemon).await
        }
        "act" => {
            let parsed = parse_act_args(rest)?;
            run_tool("act", &parsed, json_mode, no_daemon).await
        }
        "state" => {
            let parsed = parse_state_args(rest)?;
            run_tool("state", &parsed, json_mode, no_daemon).await
        }
        "run" => {
            let parsed = parse_run_args(rest)?;
            run_tool("run", &parsed, json_mode, no_daemon).await
        }
        "vision" => {
            let marks = rest.iter().any(|a| a == "--marks");
            let args = json!({ "marks": marks });
            run_tool("vision", &args, json_mode, no_daemon).await
        }
        "help" => {
            if json_mode {
                print_help_json();
            } else {
                print_cli_help();
            }
            Ok(())
        }
        _ => {
            print_cli_help();
            Ok(())
        }
    }
}

/// Run a tool: connect to daemon if running, else launch Chrome one-shot.
async fn run_tool(tool: &str, args: &Value, json_mode: bool, no_daemon: bool) -> Result<()> {
    // Try daemon first (unless --no-daemon).
    if !no_daemon && daemon_running() {
        #[cfg(unix)]
        {
            let result = send_to_daemon(tool, args)?;
            print_result(&result, json_mode);
            return Ok(());
        }
    }

    // One-shot: launch Chrome, run, exit.
    let browser = crate::browser::Browser::launch(0).await?;
    let base = browser.base();
    let target = crate::cdp::first_page_target(&base).await?;
    let client = crate::cdp::CdpClient::connect(target.ws_url()?).await?;
    let mut page = Page::attach(
        crate::cdp::CdpSession::root(client),
        &base,
        None,
    ).await?;

    // Warm profile on first run.
    if crate::session_profile::SessionProfile::claim_warming() {
        warm_profile(&mut page).await;
    }

    let result = dispatch(tool, args, &mut page).await?;
    print_result(&result, json_mode);

    // Graceful shutdown.
    let _ = tokio::task::spawn_blocking(move || browser.shutdown()).await;
    Ok(())
}

/// Print result in human or JSON format.
fn print_result(result: &ToolResult, json_mode: bool) {
    if json_mode {
        let v = json!({
            "ok": !result.is_error,
            "text": result.text,
            "image": result.image,
            "is_error": result.is_error,
        });
        println!("{v}");
    } else if let Some(ref img) = result.image {
        // Vision: save screenshot to temp file, print path.
        let path = std::env::temp_dir().join(format!("bladebro-screenshot-{}.png", std::process::id()));
        if let Ok(data) = base64_decode(img) {
            if std::fs::write(&path, data).is_ok() {
                println!("{}\nsaved: {}", result.text, path.display());
            } else {
                println!("{}", result.text);
            }
        } else {
            println!("{}", result.text);
        }
    } else {
        println!("{}", result.text);
    }
}

/// Decode base64 to bytes.
fn base64_decode(s: &str) -> std::result::Result<Vec<u8>, ()> {
    
    // Minimal base64 decoder — avoids adding a dependency.
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    lookup[b'=' as usize] = 0;

    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'\n' && b != b'\r' && b != b' ').collect();
    if bytes.len() % 4 != 0 {
        return Err(());
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
    Ok(out)
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

/// Parse `see` args: [mode] [url] [--filter <role>] [--extract <type>] [--find <text>] [--json]
fn parse_see_args(args: &[String]) -> Value {
    let mut mode = String::new();
    let mut url = String::new();
    let mut filter = String::new();
    let mut extract = String::new();
    let mut find = String::new();
    let mut logs = String::new();
    let mut budget: Option<u64> = None;
    let mut limit: Option<u64> = None;
    let mut template = String::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--filter" | "-f" => {
                i += 1;
                if let Some(v) = args.get(i) { filter = v.clone(); }
            }
            "--extract" | "-e" => {
                i += 1;
                if let Some(v) = args.get(i) { extract = v.clone(); }
            }
            "--find" => {
                i += 1;
                if let Some(v) = args.get(i) { find = v.clone(); }
            }
            "--logs" => {
                i += 1;
                if let Some(v) = args.get(i) { logs = v.clone(); }
            }
            "--budget" | "-b" => {
                i += 1;
                if let Some(v) = args.get(i) { budget = v.parse().ok(); }
            }
            "--limit" | "-l" => {
                i += 1;
                if let Some(v) = args.get(i) { limit = v.parse().ok(); }
            }
            "--template" | "-t" => {
                i += 1;
                if let Some(v) = args.get(i) { template = v.clone(); }
            }
            s if s.starts_with("http") || s.starts_with("data:") || s.starts_with("file:") => {
                if url.is_empty() { url = s.to_string(); }
            }
            s if s.starts_with("--") => {}
            s if mode.is_empty() && url.is_empty() => {
                mode = s.to_string();
            }
            s if url.is_empty() => {
                url = s.to_string();
            }
            _ => {}
        }
        i += 1;
    }

    let mut j = json!({});
    if !mode.is_empty() {
        j["mode"] = json!(mode);
    }
    if !url.is_empty() {
        j["url"] = json!(url);
    }
    if !filter.is_empty() {
        j["filter"] = json!(filter);
    }
    if !extract.is_empty() {
        j["extract"] = json!(extract);
    }
    if !find.is_empty() {
        j["find"] = json!(find);
    }
    if !logs.is_empty() {
        j["logs"] = json!(logs);
    }
    if let Some(b) = budget {
        j["budget"] = json!(b);
    }
    if let Some(l) = limit {
        j["limit"] = json!(l);
    }
    if !template.is_empty() {
        j["template"] = serde_json::from_str(&template).unwrap_or(json!(template));
    }
    j
}

/// Parse `act` args: <action> [ref|label] [text|url|key|...] [options]
fn parse_act_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Other("act needs an action (click, type, fill, navigate, scroll, press, hover, select, clear, upload, download, wait, eval, back, forward, reload)".into()));
    }

    let action = args[0].as_str();
    let mut j = json!({ "action": action });

    match action {
        "click" => {
            // click <ref|label> [--role <role>] [--nth <n>]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--role" => { i += 1; if let Some(v) = args.get(i) { j["role"] = json!(v); } }
                    "--nth" => { i += 1; if let Some(v) = args.get(i) { j["nth"] = serde_json::from_str(v).unwrap_or(json!(1)); } }
                    s if s.starts_with("--") => {}
                    s if j.get("ref").is_none() && j.get("label").is_none() => {
                        if s.starts_with("e") && s[1..].chars().all(|c| c.is_ascii_digit()) {
                            j["ref"] = json!(s);
                        } else {
                            j["label"] = json!(s);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "type" => {
            // type <ref|label> <text>
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    s if s.starts_with("--") => {}
                    s if j.get("ref").is_none() && j.get("label").is_none() => {
                        if s.starts_with("e") && s[1..].chars().all(|c| c.is_ascii_digit()) {
                            j["ref"] = json!(s);
                        } else {
                            j["label"] = json!(s);
                        }
                    }
                    s if j.get("text").is_none() => {
                        j["text"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "clear" => {
            // clear <ref|label>
            if let Some(v) = args.get(1) {
                if v.starts_with("e") && v[1..].chars().all(|c| c.is_ascii_digit()) {
                    j["ref"] = json!(v);
                } else {
                    j["label"] = json!(v);
                }
            }
        }
        "select" => {
            // select <ref|label> <option>
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    s if s.starts_with("--") => {}
                    s if j.get("ref").is_none() && j.get("label").is_none() => {
                        if s.starts_with("e") && s[1..].chars().all(|c| c.is_ascii_digit()) {
                            j["ref"] = json!(s);
                        } else {
                            j["label"] = json!(s);
                        }
                    }
                    s if j.get("option").is_none() => {
                        j["option"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "press" => {
            // press <key>
            if let Some(v) = args.get(1) {
                j["key"] = json!(v);
            }
        }
        "scroll" => {
            // scroll <dx> <dy>
            if let Some(v) = args.get(1) { j["dx"] = serde_json::from_str(v).unwrap_or(json!(0)); }
            if let Some(v) = args.get(2) { j["dy"] = serde_json::from_str(v).unwrap_or(json!(0)); }
        }
        "hover" => {
            // hover <ref|label>
            if let Some(v) = args.get(1) {
                if v.starts_with("e") && v[1..].chars().all(|c| c.is_ascii_digit()) {
                    j["ref"] = json!(v);
                } else {
                    j["label"] = json!(v);
                }
            }
        }
        "navigate" => {
            // navigate <url> [--block <classes>]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--block" => { i += 1; if let Some(v) = args.get(i) { j["block"] = json!(v); } }
                    s if s.starts_with("http") || s.starts_with("data:") || s.starts_with("file:") => {
                        j["url"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "fill" => {
            // fill <json-fields> [--submit <ref>]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--submit" => { i += 1; if let Some(v) = args.get(i) { j["submit"] = json!(v); } }
                    s if !s.starts_with("--") && j.get("fields").is_none() => {
                        j["fields"] = serde_json::from_str(s)
                            .map_err(|e| BladeError::Other(format!("invalid fields JSON: {e}")))?;
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "upload" => {
            // upload <ref|label> <path>
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    s if s.starts_with("--") => {}
                    s if j.get("ref").is_none() && j.get("label").is_none() => {
                        if s.starts_with("e") && s[1..].chars().all(|c| c.is_ascii_digit()) {
                            j["ref"] = json!(s);
                        } else {
                            j["label"] = json!(s);
                        }
                    }
                    s if j.get("path").is_none() => {
                        j["path"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "download" => {
            // download <url> [--path <path>]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--path" => { i += 1; if let Some(v) = args.get(i) { j["path"] = json!(v); } }
                    s if s.starts_with("http") => {
                        j["url"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "wait" => {
            // wait <condition> [--text <text>] [--timeout <secs>]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--text" => { i += 1; if let Some(v) = args.get(i) { j["text"] = json!(v); } }
                    "--timeout" => { i += 1; if let Some(v) = args.get(i) { j["timeout"] = serde_json::from_str(v).unwrap_or(json!(30)); } }
                    s if !s.starts_with("--") && j.get("condition").is_none() => {
                        j["condition"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "eval" => {
            // eval <js-expression>
            if let Some(v) = args.get(1) {
                j["js"] = json!(v);
            }
        }
        "collect" => {
            // collect <url> [--max <n>]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--max" => { i += 1; if let Some(v) = args.get(i) { j["max"] = serde_json::from_str(v).unwrap_or(json!(100)); } }
                    s if s.starts_with("http") => {
                        j["url"] = json!(s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        "back" | "forward" | "reload" => {
            // no args needed
        }
        "pdf" => {
            // pdf [--path <path>] [--landscape]
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--path" => { i += 1; if let Some(v) = args.get(i) { j["path"] = json!(v); } }
                    "--landscape" => { j["landscape"] = json!(true); }
                    _ => {}
                }
                i += 1;
            }
        }
        "save" | "load" => {
            // save|load <name>
            if let Some(v) = args.get(1) {
                j["name"] = json!(v);
            }
        }
        _ => {
            return Err(BladeError::Other(format!(
                "unknown action: {action}\navailable: click, type, clear, select, press, scroll, hover, navigate, fill, upload, download, wait, eval, collect, pdf, back, forward, reload, save, load"
            )));
        }
    }

    // If a URL is in the remaining args and action isn't navigate, set it for pre-navigation.
    for a in args.iter().skip(1) {
        if a.starts_with("http") && !a.starts_with("--") && j.get("url").is_none() && action != "navigate" {
            j["url"] = json!(a);
            break;
        }
    }

    Ok(j)
}

/// Parse `state` args: <op> [args]
fn parse_state_args(args: &[String]) -> Result<Value> {
    if args.is_empty() {
        return Err(BladeError::Other(
            "state needs an op (cookies, set-cookie, del-cookie, ls, ss, set-ls, set-ss, rm-ls, clear-ls, clear-ss, tabs, open-tab, close-tab, switch-tab, save, load)".into()
        ));
    }

    let op = args[0].as_str();
    let mut j = json!({});

    match op {
        "cookies" => {
            j["op"] = json!("cookies");
        }
        "set-cookie" => {
            // set-cookie <name> <value> [--url <url>] [--domain <domain>]
            j["op"] = json!("set-cookie");
            if let Some(v) = args.get(1) { j["name"] = json!(v); }
            if let Some(v) = args.get(2) { j["value"] = json!(v); }
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--url" => { i += 1; if let Some(v) = args.get(i) { j["url"] = json!(v); } }
                    "--domain" => { i += 1; if let Some(v) = args.get(i) { j["domain"] = json!(v); } }
                    _ => {}
                }
                i += 1;
            }
        }
        "del-cookie" => {
            j["op"] = json!("del-cookie");
            if let Some(v) = args.get(1) { j["name"] = json!(v); }
        }
        "ls" | "localStorage" => {
            j["op"] = json!("ls");
        }
        "ss" | "sessionStorage" => {
            j["op"] = json!("ss");
        }
        "set-ls" => {
            j["op"] = json!("set-ls");
            if let Some(v) = args.get(1) { j["key"] = json!(v); }
            if let Some(v) = args.get(2) { j["value"] = json!(v); }
        }
        "set-ss" => {
            j["op"] = json!("set-ss");
            if let Some(v) = args.get(1) { j["key"] = json!(v); }
            if let Some(v) = args.get(2) { j["value"] = json!(v); }
        }
        "rm-ls" => {
            j["op"] = json!("rm-ls");
            if let Some(v) = args.get(1) { j["key"] = json!(v); }
        }
        "clear-ls" => { j["op"] = json!("clear-ls"); }
        "clear-ss" => { j["op"] = json!("clear-ss"); }
        "tabs" => { j["op"] = json!("tabs"); }
        "open-tab" => {
            j["op"] = json!("open-tab");
            if let Some(v) = args.get(1) { j["url"] = json!(v); }
        }
        "close-tab" => {
            j["op"] = json!("close-tab");
            if let Some(v) = args.get(1) { j["target_id"] = json!(v); }
        }
        "switch-tab" => {
            j["op"] = json!("switch-tab");
            if let Some(v) = args.get(1) { j["target_id"] = json!(v); }
        }
        "save" => {
            j["op"] = json!("save");
            if let Some(v) = args.get(1) { j["name"] = json!(v); }
        }
        "load" => {
            j["op"] = json!("load");
            if let Some(v) = args.get(1) { j["name"] = json!(v); }
        }
        _ => {
            return Err(BladeError::Other(format!(
                "unknown state op: {op}\navailable: cookies, set-cookie, del-cookie, ls, ss, set-ls, set-ss, rm-ls, clear-ls, clear-ss, tabs, open-tab, close-tab, switch-tab, save, load"
            )));
        }
    }

    Ok(j)
}

/// Parse `run` args: <json-steps>
fn parse_run_args(args: &[String]) -> Result<Value> {
    let steps_json = args.first().ok_or_else(|| {
        BladeError::Other("run needs a JSON steps array, e.g.: bladebro run '[{\"action\":\"click\",\"ref\":\"e5\"}]'".into())
    })?;

    let steps: Value = serde_json::from_str(steps_json)
        .map_err(|e| BladeError::Other(format!("invalid steps JSON: {e}")))?;

    Ok(json!({ "steps": steps }))
}

// ── Daemon ──────────────────────────────────────────────────────────────

/// Start the CLI daemon: persistent Chrome + Unix socket server.
/// Same lifecycle as MCP (lazy launch, self-healing, idle timeout, reaper).
#[cfg(unix)]
pub async fn run_daemon() -> Result<()> {
    use tokio::net::UnixListener;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let path = socket_path();
    // Remove stale socket.
    let _ = std::fs::remove_file(&path);
    // Create parent dir.
    let _ = std::fs::create_dir_all(path.parent().unwrap());

    let listener = UnixListener::bind(&path)
        .map_err(|e| BladeError::Other(format!("failed to bind socket {}: {e}", path.display())))?;

    eprintln!("[bladebro] daemon listening on {}", path.display());

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

    loop {
        tokio::select! {
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
                        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                    }
                    page = None;
                }
            }
        }
    }

    // Cleanup.
    if let Some(b) = browser {
        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
    }
    let _ = std::fs::remove_file(&path);
    eprintln!("[bladebro] daemon stopped");
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_daemon() -> Result<()> {
    Err(BladeError::Other("daemon mode is Unix-only (requires Unix sockets)".into()))
}

/// Launch Chrome and create a Page for the daemon.
#[cfg(unix)]
async fn launch_browser() -> Result<(Page, Option<crate::browser::Browser>)> {
    let browser = crate::browser::Browser::launch(0).await?;
    let base = browser.base();
    let target = crate::cdp::first_page_target(&base).await?;
    let client = crate::cdp::CdpClient::connect(target.ws_url()?).await?;
    let page = Page::attach(
        crate::cdp::CdpSession::root(client),
        &base,
        None,
    ).await?;
    Ok((page, Some(browser)))
}

/// Stop the daemon by sending a stop command.
pub async fn stop_daemon() -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        use std::io::{Read, Write};

        let path = socket_path();
        let mut stream = UnixStream::connect(&path)
            .map_err(|e| BladeError::Other(format!("daemon not running: {e}")))?;

        writeln!(stream, "{{\"tool\":\"stop\"}}")?;
        stream.flush()?;

        let mut resp = String::new();
        stream.read_to_string(&mut resp)?;
        println!("daemon stopped");
    }
    Ok(())
}

// ── Help ───────────────────────────────────────────────────────────────

/// Output structured tool definitions + CLI command mapping as JSON.
/// This is the CLI equivalent of MCP `tools/list`. An AI agent calls
/// `bladebro help --json` once to discover the full interface, then uses
/// `--json` on every command for structured output.
fn print_help_json() {
    let tools = crate::mcp::tools::tools_to_json();
    let cli_commands = json!({
        "nav": {
            "tool": "act",
            "args": {"action": "navigate", "url": "<url>"},
            "description": "Navigate to a URL. Returns refs + content preview."
        },
        "see": {
            "tool": "see",
            "args": {"mode": "model|content|outline", "url": "<optional>", "extract": "auto|links|forms", "filter": "<role>", "find": "<text>", "budget": 8000},
            "description": "Read the page without acting. model=interactive elements with refs, content=clean markdown, outline=headings only."
        },
        "act": {
            "tool": "act",
            "args": {"action": "click|type|fill|navigate|scroll|press|hover|select|clear|upload|download|wait|eval|back|forward|reload|collect|pdf", "ref": "e5", "label": "text", "text": "value", "url": "<url>", "key": "Enter", "dx": 0, "dy": 0},
            "description": "Interact with the page. Returns verdict + delta. Use ref from see model, or label text."
        },
        "state": {
            "tool": "state",
            "args": {"op": "cookies|set-cookie|del-cookie|ls|ss|set-ls|set-ss|rm-ls|clear-ls|clear-ss|tabs|open-tab|close-tab|switch-tab|save|load", "name": "<key>", "value": "<val>", "url": "<url>"},
            "description": "Manage cookies, storage, tabs, sessions."
        },
        "run": {
            "tool": "run",
            "args": {"steps": [{"action": "click", "ref": "e5"}]},
            "description": "Batch actions with branching and loops."
        },
        "vision": {
            "tool": "vision",
            "args": {"marks": false},
            "description": "Screenshot as PNG. marks=true overlays numbered ref badges."
        },
        "daemon": {
            "tool": null,
            "args": null,
            "description": "Start persistent Chrome session. Subsequent commands connect via Unix socket."
        },
        "stop": {
            "tool": null,
            "args": null,
            "description": "Stop the daemon."
        },
        "help": {
            "tool": null,
            "args": {"json": true},
            "description": "Show this help. Use --json for structured tool definitions (same as MCP tools/list)."
        }
    });
    let flags = json!({
        "--json": "Structured JSON output {ok, text, image, is_error} for scripts and agents.",
        "--no-daemon": "Force one-shot mode (launch Chrome per command).",
        "--marks": "Overlay numbered ref badges on screenshot (vision only)."
    });
    let output = json!({
        "tools": tools,
        "cli_commands": cli_commands,
        "flags": flags,
    });
    println!("{output}");
}

fn print_cli_help() {
    eprintln!(
        "bladebro — agentic browser driver\n\n\
         USAGE:\n    bladebro <COMMAND> [OPTIONS] [--json] [--no-daemon]\n\n\
         COMMANDS:\n    nav <url>              navigate to a URL\n    see [mode] [url]      read the page (model|content|outline|extract|links|forms)\n    act <action> [args]   interact (click|type|fill|navigate|scroll|press|hover|select|clear|...)\n    state <op> [args]      manage cookies, storage, tabs\n    run <json-steps>       batch actions\n    vision [--marks]       screenshot\n    daemon                 start persistent Chrome session\n    stop                   stop daemon\n\n\
         MODES (see):\n    model                  interactive elements with refs (default)\n    content                clean markdown for reading\n    outline                heading hierarchy only\n    extract auto           auto-detect and extract structured data\n    links                  all links\n    forms                  all forms\n\n\
         ACTIONS (act):\n    click <ref|label>      click an element\n    type <ref|label> <text>  type text into an element\n    fill <json-fields> [--submit <ref>]  fill a form\n    navigate <url>         navigate to a URL\n    scroll <dx> <dy>       scroll the page\n    press <key>            press a key (Enter, Tab, Escape, ...)\n    hover <ref|label>      hover an element\n    select <ref|label> <option>  select an option\n    clear <ref|label>      clear an input\n    upload <ref|label> <path>  upload a file\n    download <url> [--path <path>]  download a file\n    wait <condition> [--text <text>] [--timeout <secs>]  wait\n    eval <js>              evaluate JavaScript\n    back / forward / reload  navigation\n\n\
         STATE OPS:\n    cookies                list cookies\n    set-cookie <name> <value>  set a cookie\n    del-cookie <name>     delete a cookie\n    ls / ss                list local/session storage\n    set-ls/set-ss <key> <val>  set storage\n    rm-ls <key>            remove from localStorage\n    clear-ls / clear-ss    clear storage\n    tabs                   list tabs\n    open-tab <url>         open a new tab\n    close-tab <id>         close a tab\n    switch-tab <id>        switch to a tab\n\n\
         FLAGS:\n    --json                 structured JSON output (for AI agents)\n    --no-daemon            force one-shot mode (launch Chrome per command)\n    --marks (vision)       overlay numbered ref badges on screenshot\n\n\
         EXAMPLES:\n    bladebro daemon                     # start persistent session\n    bladebro nav https://example.com      # navigate\n    bladebro see content                 # read page as markdown\n    bladebro see model                   # interactive elements\n    bladebro act click e5                # click element e5\n    bladebro act type e12 \"hello world\"  # type text\n    bladebro see --json                  # JSON output for agents\n    bladebro see extract auto            # auto-extract structured data"
    );
}
