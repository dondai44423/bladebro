//! Bladebro binary entry point.
//!
//! For now this is a minimal hand-rolled CLI used to exercise the CDP layer
//! against a live browser. The real product surface is the MCP server (added
//! later); the CLI stays as a direct-use / debugging path.

use std::process::ExitCode;

use bladebro::cdp;
use bladebro::Result;

fn main() -> ExitCode {
    // Initialize structured logging; respect RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,bladebro=info")),
        )
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bladebro: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse global flags (--host/--port) anywhere before or after the command.
    // port=0 means auto-launch Chrome (find binary, pick free port, manage lifecycle).
    let mut host = String::from("127.0.0.1");
    let mut port: u16 = 0;
    let mut cmd: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    host = v.clone();
                }
            }
            "--port" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    port = v.parse().unwrap_or(0);
                }
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            s if cmd.is_none() && !s.starts_with('-') => {
                cmd = Some(s.to_string());
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    let cmd = cmd.unwrap_or_else(|| "help".to_string());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(bladebro::BladeError::other)?;

    // S1: the mcp daemon defaults to the zero-port pipe transport (Unix).
    // BLADE_TRANSPORT=ws forces the WebSocket transport; CLI one-shot
    // commands always use WS since they depend on HTTP target discovery.
    // Windows uses WS (pipe fds 3/4 don't exist on Windows).
    let use_pipe = port == 0
        && cmd == "mcp"
        && std::env::var("BLADE_TRANSPORT").map(|v| v != "ws").unwrap_or(true)
        && cfg!(unix);

    // Auto-launch Chrome if port is 0 (default). When --port is explicitly
    // given, connect to the existing Chrome instance on that port.
    // Pipe mode launches its own Chrome inside cmd_mcp_pipe.
    let browser = if port == 0 && !use_pipe {
        Some(rt.block_on(bladebro::browser::Browser::launch(0))?)
    } else {
        None
    };
    let base = if port == 0 && !use_pipe {
        browser.as_ref().unwrap().base()
    } else {
        format!("{host}:{port}")
    };

    match cmd.as_str() {
        "probe" => rt.block_on(cmd_probe(&base)),
        "targets" => rt.block_on(cmd_targets(&base)),
        "version" => rt.block_on(cmd_version(&base)),
        "nav" => {
            let url = positional
                .first()
                .cloned()
                .unwrap_or_else(|| "data:text/html,<h1>bladebro</h1>".to_string());
            rt.block_on(cmd_nav(&base, &url))
        }
        "see" => {
            // URL: first positional starting with http/data/file. Filter: first non-URL positional.
            let url = positional.iter()
                .find(|p| p.starts_with("http") || p.starts_with("data:") || p.starts_with("file:"))
                .map(String::as_str);
            let filter = positional.iter()
                .find(|p| !p.starts_with("http") && !p.starts_with("data:") && !p.starts_with("file:"))
                .map(String::as_str);
            rt.block_on(cmd_see(&base, url, filter))
        }
        "act" => {
            let sub = positional.first().cloned().unwrap_or_default();
            let args = &positional[1..];
            rt.block_on(cmd_act(&base, &sub, args))
        }
        "state" => {
            let sub = positional.first().cloned().unwrap_or_default();
            let args = &positional[1..];
            rt.block_on(cmd_state(&base, &sub, args))
        }
        "mcp" if use_pipe => rt.block_on(cmd_mcp_pipe()),
        "mcp" => rt.block_on(cmd_mcp(&base, browser)),
        "audit" => rt.block_on(cmd_audit(&base)),
        _ => {
            print_usage();
            Ok(())
        }
    }
}

async fn cmd_version(base: &str) -> Result<()> {
    let v = cdp::version(base).await?;
    println!("Browser:     {}", v.browser);
    println!("Protocol:    {}", v.protocol_version);
    if let Some(ua) = v.user_agent {
        println!("User-Agent:  {ua}");
    }
    if let Some(ws) = v.web_socket_debugger_url {
        println!("Browser WS:  {ws}");
    }
    Ok(())
}

async fn cmd_targets(base: &str) -> Result<()> {
    let targets = cdp::list_targets(base).await?;
    if targets.is_empty() {
        println!("no targets");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<6} URL", "ID", "TYPE", "ATT");
    for t in targets {
        println!(
            "{:<24} {:<8} {:<6} {}",
            t.id,
            t.kind,
            if t.attached { "yes" } else { "no" },
            t.url,
        );
    }
    Ok(())
}

async fn cmd_probe(base: &str) -> Result<()> {
    println!("→ probing {base}");
    let v = cdp::version(base).await?;
    println!("  browser: {} (protocol {})", v.browser, v.protocol_version);

    let targets = cdp::list_targets(base).await?;
    let pages: Vec<_> = targets.iter().filter(|t| t.is_page()).collect();
    println!("  targets: {} total, {} page(s)", targets.len(), pages.len());

    let Some(page) = pages.iter().find(|t| t.web_socket_debugger_url.is_some()) else {
        println!("  no page target with a WebSocket URL — nothing to drive");
        return Ok(());
    };

    println!("  → connecting to page {}", page.id);
    let client = cdp::CdpClient::connect(page.ws_url()?).await?;

    // Enable the domains we'll need for the Live Page Model, proving the
    // full request/response loop works against a real browser.
    client.enable("Page").await?;
    client.enable("Runtime").await?;
    client.enable("DOM").await?;
    println!("  ✓ enabled Page / Runtime / DOM");

    // A trivial command round-trip: ask for the current frame tree root URL.
    let tree = client.send("Page.getFrameTree", None).await?;
    let url = tree
        .get("frameTree")
        .and_then(|ft| ft.get("frame"))
        .and_then(|f| f.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("(unknown)");
    println!("  ✓ current frame: {url}");

    println!("→ probe OK");
    Ok(())
}

async fn cmd_nav(base: &str, url: &str) -> Result<()> {
    println!("→ navigating to {url}");
    let page = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(page.ws_url()?).await?;
    client.enable("Page").await?;

    // Subscribe BEFORE navigating so we don't miss the frameNavigated event.
    let wait = client.wait_for("Page.frameNavigated", std::time::Duration::from_secs(15));
    client
        .send("Page.navigate", Some(serde_json::json!({ "url": url })))
        .await?;

    let ev = wait.await?;
    let landed = ev
        .params
        .get("frame")
        .and_then(|f| f.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("(unknown)");
    println!("  ✓ frameNavigated → {landed}");
    Ok(())
}

async fn cmd_see(base: &str, url: Option<&str>, filter: Option<&str>) -> Result<()> {
    use bladebro::page::Page;
    let target = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(target.ws_url()?).await?;
    // Navigate first if a URL was given.
    if let Some(u) = url {
        if u.starts_with("http") || u.starts_with("data:") || u.starts_with("file:") {
            client.enable("Page").await?;
            let wait =
                client.wait_for("Page.frameNavigated", std::time::Duration::from_secs(15));
            client
                .send("Page.navigate", Some(serde_json::json!({ "url": u })))
                .await?;
            let _ = wait.await;
        }
    }
    let mut page = Page::attach(bladebro::cdp::CdpSession::root(client), base, None).await?;
    let _delta = page.recapture().await?;
    match filter {
        Some(f) if !f.is_empty() => {
            println!("{}", page.view_filtered(8000, f));
        }
        _ => {
            println!("{}", page.view(8000));
        }
    }
    Ok(())
}

async fn cmd_act(base: &str, sub: &str, args: &[String]) -> Result<()> {
    use bladebro::action::Action;
    use bladebro::page::Page;

    let target = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(target.ws_url()?).await?;

    // If the last arg looks like a URL, navigate first.
    let url = args.iter().find(|a| a.starts_with("http") || a.starts_with("data:"));
    if let Some(u) = url {
        client.enable("Page").await?;
        let wait =
            client.wait_for("Page.frameNavigated", std::time::Duration::from_secs(15));
        client
            .send("Page.navigate", Some(serde_json::json!({ "url": u })))
            .await?;
        let _ = wait.await;
    }

    let mut page = Page::attach(bladebro::cdp::CdpSession::root(client), base, None).await?;

    // Show the current page before acting.
    println!("{}", page.view(8000));

    let action = match sub {
        "click" => {
            let ref_id = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("click needs a ref id".into())
            })?;
            Action::Click { ref_id: ref_id.clone() }
        }
        "type" => {
            let ref_id = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("type needs a ref id".into())
            })?;
            let text = args.get(1).ok_or_else(|| {
                bladebro::error::BladeError::Other("type needs text".into())
            })?;
            Action::Type { ref_id: ref_id.clone(), text: text.clone() }
        }
        "clear" => {
            let ref_id = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("clear needs a ref id".into())
            })?;
            Action::Clear { ref_id: ref_id.clone() }
        }
        "select" => {
            let ref_id = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("select needs a ref id".into())
            })?;
            let option = args.get(1).ok_or_else(|| {
                bladebro::error::BladeError::Other("select needs an option value".into())
            })?;
            Action::Select { ref_id: ref_id.clone(), option: option.clone() }
        }
        "press" => {
            let key = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("press needs a key name".into())
            })?;
            Action::Press { key: key.clone() }
        }
        "scroll" => {
            let dx = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let dy = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            Action::Scroll { dx, dy }
        }
        "hover" => {
            let ref_id = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("hover needs a ref id".into())
            })?;
            Action::Hover { ref_id: ref_id.clone() }
        }
        "upload" => {
            let ref_id = args.first().ok_or_else(|| {
                bladebro::error::BladeError::Other("upload needs a ref id".into())
            })?;
            let path = args.get(1).ok_or_else(|| {
                bladebro::error::BladeError::Other("upload needs a file path".into())
            })?;
            Action::Upload { ref_id: ref_id.clone(), path: path.clone() }
        }
        _ => {
            eprintln!("act subcommands: click <ref>, type <ref> <text>, clear <ref>, select <ref> <value>, press <key>, scroll <dx> <dy>, hover <ref>, upload <ref> <path>");
            return Ok(());
        }
    };

    println!("\n→ {action:?}");
    match page.act(action).await {
        Ok((delta, verdict)) => {
            println!("{verdict}");
            println!("{}", page.delta_view(&delta, 8000));
        }
        Err(e) => {
            eprintln!("bladebro: {e}");
        }
    }
    Ok(())
}

async fn cmd_state(base: &str, sub: &str, args: &[String]) -> Result<()> {
    use bladebro::state::StateOp;
    use bladebro::page::Page;
    use bladebro::error::BladeError;

    let target = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(target.ws_url()?).await?;
    let page = Page::attach(bladebro::cdp::CdpSession::root(client), base, None).await?;

    let op = match sub {
        "cookies" => StateOp::GetCookies { urls: vec![] },
        "set-cookie" => {
            let name = args.first().ok_or_else(|| BladeError::Other("set-cookie needs name".into()))?;
            let value = args.get(1).ok_or_else(|| BladeError::Other("set-cookie needs value".into()))?;
            StateOp::SetCookie {
                name: name.clone(), value: value.clone(),
                domain: None, path: None, secure: None,
                http_only: None, same_site: None,
            }
        }
        "del-cookie" => {
            let name = args.first().ok_or_else(|| BladeError::Other("del-cookie needs name".into()))?;
            StateOp::DeleteCookies { name: name.clone(), domain: None }
        }
        "ls" | "localStorage" => StateOp::GetLocalStorage,
        "ss" | "sessionStorage" => StateOp::GetSessionStorage,
        "set-ls" => {
            let key = args.first().ok_or_else(|| BladeError::Other("set-ls needs key".into()))?;
            let value = args.get(1).ok_or_else(|| BladeError::Other("set-ls needs value".into()))?;
            StateOp::SetLocalStorage { key: key.clone(), value: value.clone() }
        }
        "set-ss" => {
            let key = args.first().ok_or_else(|| BladeError::Other("set-ss needs key".into()))?;
            let value = args.get(1).ok_or_else(|| BladeError::Other("set-ss needs value".into()))?;
            StateOp::SetSessionStorage { key: key.clone(), value: value.clone() }
        }
        "rm-ls" => {
            let key = args.first().ok_or_else(|| BladeError::Other("rm-ls needs key".into()))?;
            StateOp::RemoveLocalStorage { key: key.clone() }
        }
        "clear-ls" => StateOp::ClearLocalStorage,
        "clear-ss" => StateOp::ClearSessionStorage,
        "tabs" => StateOp::ListTabs,
        "open-tab" => {
            let url = args.first().ok_or_else(|| BladeError::Other("open-tab needs url".into()))?;
            StateOp::OpenTab { url: url.clone() }
        }
        "close-tab" => {
            let tid = args.first().ok_or_else(|| BladeError::Other("close-tab needs target id".into()))?;
            StateOp::CloseTab { target_id: tid.clone() }
        }
        _ => {
            eprintln!("state subcommands: cookies, set-cookie <name> <value>, del-cookie <name>, ls, ss, set-ls <key> <val>, set-ss <key> <val>, rm-ls <key>, clear-ls, clear-ss, tabs, open-tab <url>, close-tab <id>");
            return Ok(());
        }
    };

    match page.state(op).await {
        Ok(result) => print!("{result}"),
        Err(e) => eprintln!("bladebro: {e}"),
    }
    Ok(())
}

async fn cmd_mcp(base: &str, _browser: Option<bladebro::browser::Browser>) -> Result<()> {
    // _browser keeps Chrome alive for the MCP server's lifetime.
    use bladebro::mcp;
    let (host, port) = parse_host_port(base);
    mcp::run(host, port).await
}

/// The default MCP path (S1): launch Chrome with CDP over pipe fds — no
/// debugging port exists for page JavaScript to probe.
/// Unix-only: Windows uses WS transport.
#[cfg(unix)]
async fn cmd_mcp_pipe() -> Result<()> {
    use bladebro::mcp;
    let (browser, client) = bladebro::browser::Browser::launch_pipe().await?;
    mcp::run_pipe(client, browser).await
}

/// S13: `bladebro audit` — run the stealth vectors + boot self-check (S2)
/// and print a scorecard. One-shot CLI command (WS transport).
async fn cmd_audit(base: &str) -> Result<()> {
    use bladebro::action::Action;
    use bladebro::page::Page;
    use bladebro::BladeError;
    let target = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(target.ws_url()?).await?;
    let mut page = Page::attach(
        bladebro::cdp::CdpSession::root(client),
        base,
        None,
    ).await?;

    // Find vectors.html — project root or CARGO_MANIFEST_DIR.
    let vectors_url = {
        let candidates = [
            std::path::PathBuf::from("tests/vectors.html"),
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors.html"),
        ];
        let path = candidates
            .iter()
            .find(|p| p.exists())
            .ok_or_else(|| BladeError::Other("vectors.html not found. Run from project root.".into()))?;
        format!("file://{}", path.canonicalize().unwrap_or(path.clone()).display())
    };

    println!("[audit] running stealth vectors...");
    page.navigate(&vectors_url).await?;
    let _ = page
        .act(Action::Wait {
            condition: "title".into(),
            text: "DONE".into(),
            timeout: std::time::Duration::from_secs(15),
        })
        .await;

    let content = page.content(2000).await?;

    // S2: boot self-check — verify key stealth properties.
    let session = page.cdp_ref();
    let selfcheck = session
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "JSON.stringify({wd:navigator.webdriver,cdc:typeof window.cdc_,plugins:navigator.plugins.length,native:Function.prototype.toString.call(navigator.permissions.query).includes('native code')})",
                "returnByValue": true,
            })),
        )
        .await
        .ok()
        .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from));

    let bar = "=".repeat(52);
    println!("\n{bar}");
    println!("  BLADEBRO STEALTH AUDIT");
    println!("{bar}");

    for line in content.lines() {
        if line.contains("SCORE:") || line.starts_with("FAIL ") {
            println!("  {line}");
        }
    }

    if let Some(ref result) = selfcheck {
        let v: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
        let wd = v.get("wd").and_then(|x| x.as_bool()).unwrap_or(true);
        let cdc = v.get("cdc").and_then(|x| x.as_str()).unwrap_or("undefined");
        let plugins = v.get("plugins").and_then(|x| x.as_i64()).unwrap_or(0);
        let native = v.get("native").and_then(|x| x.as_bool()).unwrap_or(false);
        println!("\n  Self-check:");
        println!("    navigator.webdriver: {}", if wd { "FAIL (true)" } else { "OK (false)" });
        println!("    window.cdc_:         {} ({})", if cdc == "undefined" { "OK" } else { "FAIL" }, cdc);
        println!("    navigator.plugins:   {} ({} plugins)", if plugins > 0 { "OK" } else { "WARN" }, plugins);
        println!("    toString integrity:  {}", if native { "OK (native)" } else { "FAIL" });
    }
    println!("{bar}");
    Ok(())
}

fn parse_host_port(base: &str) -> (&str, u16) {
    if let Some((h, p)) = base.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h, port);
        }
    }
    ("127.0.0.1", 9222)
}

fn print_usage() {
    eprintln!(
        "bladebro — agentic browser driver for AI\n\n\
         USAGE:\n    bladebro <COMMAND> [OPTIONS]\n\n\
         COMMANDS:\n    probe      connect to a browser, enable core domains, round-trip a command\n    targets    list CDP targets\n    version    print /json/version\n    nav <url>  navigate the first page tab and wait for frameNavigated\n    see [url]  capture the page and print the agent view + a recapture delta\n    act <sub> <args...> [url]  perform an action and show the delta\n    state <sub> <args...>  inspect/modify cookies, storage, and tabs\n    mcp        run the MCP server (stdio JSON-RPC)
    audit      run stealth vectors + boot self-check, print scorecard\n    help       show this message\n\n\
         OPTIONS:\n    --host <h>   browser debug host (default 127.0.0.1)\n    --port <p>   browser debug port (default: auto-launch Chrome)\n\n\
         With no --port, Bladebro finds Chrome, launches it with stealth flags,\n         and manages its lifecycle. Set CHROME_PATH to override the binary location.\n         To connect to an already-running Chrome, pass --port 9222."
    );
}
