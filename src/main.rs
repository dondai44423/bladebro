//! Bladebro binary entry point.
//!
//! Process-level wiring only: logging, the exit-code contract, global flag
//! parsing, and dispatch. The command surfaces live in the library modules —
//! the CLI (daemon + nav/see/act/state/run/vision/rb/help) in
//! [`bladebro::cli`], the stealth audit in [`bladebro::audit`], the update
//! hub in [`bladebro::updater`], the MCP server in [`bladebro::mcp`]. The
//! legacy debug commands kept here (`targets`, `probe`) drive raw CDP.

use bladebro::cdp;
use bladebro::ui;
use bladebro::Result;

fn main() {
    // Initialize structured logging; respect RUST_LOG.
    // In MCP mode, default to warn only — info logs leak to the agent's
    // TUI via stderr. RUST_LOG still overrides if explicitly set.
    let default_filter = if std::env::args().any(|a| a == "mcp") {
        "warn"
    } else {
        "warn,bladebro=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        // Logs go to stderr: stdout is a machine contract (ONE JSON object
        // for --json, pure JSON-RPC for `mcp`).
        .with_writer(std::io::stderr)
        .init();

    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            print_error(&e);
            if matches!(e, bladebro::BladeError::Usage(_)) {
                2
            } else {
                1
            }
        }
    };
    // See exit_immediately: a parked stdin read hangs
    // Runtime::drop after signal-driven shutdown.
    exit_immediately(code);
}

/// Exit immediately without dropping the tokio runtime.
///
/// Why: `tokio::io::stdin()` reads on a blocking-pool thread.
/// When the MCP server exits via a SIGNAL (SIGTERM from the
/// harness), the pending stdin read is still parked — and
/// `Runtime::drop` waits for all blocking tasks to finish.
/// The parked read never finishes (the harness holds the
/// pipe open) → the process hangs forever AFTER all cleanup
/// already ran. Observed live: bladebro survived SIGTERM by
/// minutes with Chrome long dead.
///
/// At this point every teardown step is complete (Chrome
/// killed, profile synced, session dir removed) — the only
/// remaining state is in-memory. Skipping the runtime drop
/// is safe.
fn exit_immediately(code: i32) -> ! {
    std::process::exit(code);
}

/// One error line: `bladebro:` dim, message red — or yellow for usage errors
/// (a typo is not a failure).
fn print_error(e: &bladebro::BladeError) {
    let msg = e.to_string();
    let styled = if matches!(e, bladebro::BladeError::Usage(_)) {
        ui::yellow(&msg)
    } else {
        ui::red(&msg)
    };
    eprintln!("{} {styled}", ui::dim("bladebro:"));
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse global flags (--host/--port) anywhere before or after the command.
    // port=0 means auto-launch Chrome (find binary, pick free port, manage lifecycle).
    let mut host = String::from("127.0.0.1");
    let mut port: u16 = 0;
    // Track whether --host/--port were EXPLICITLY given, so CLI commands can
    // be told to connect to an already-running Chrome (issue #16: the parsed
    // endpoint was dropped before it reached the CLI module).
    let mut host_given = false;
    let mut port_given = false;
    let mut cmd: Option<String> = None;
    let mut help_flag = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                i += 1;
                match args.get(i) {
                    Some(v) if !v.is_empty() => host = v.clone(),
                    _ => {
                        return Err(bladebro::BladeError::Usage(
                            "--host needs a value (e.g. --host 127.0.0.1)".into(),
                        ));
                    }
                }
                host_given = true;
            }
            "--port" => {
                i += 1;
                // Missing/invalid values are loud usage errors (exit 2). They
                // used to fall back to 0 and get silently re-injected as
                // `--port 0` — the user saw an unrelated error, or the run
                // targeted the wrong endpoint.
                let v = args.get(i).ok_or_else(|| {
                    bladebro::BladeError::Usage(
                        "--port needs a value (e.g. --port 9222; 0 = auto-launch)".into(),
                    )
                })?;
                port = v.parse().map_err(|_| {
                    bladebro::BladeError::Usage(format!("--port needs a number, got '{v}'"))
                })?;
                port_given = true;
            }
            "-h" | "--help" => {
                help_flag = true;
            }
            "-u" | "-doc" | "-v" | "--version" | "--rollback" if cmd.is_none() => {
                cmd = Some(args[i].clone());
            }
            s if cmd.is_none() && !s.starts_with('-') => {
                cmd = Some(s.to_string());
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    let cmd = cmd.unwrap_or_else(|| "help".to_string());

    // S18: pick this process's lane (config + BLADE_LANE env override). Every
    // surface — one-shot CLI, daemon, MCP — reads the same switch.
    bladebro::realbrowser::init_lane();

    // Restore the default SIGPIPE for CLI-output commands so an early pipe
    // reader (`bladebro ... | head -1`) exits quietly instead of panicking —
    // Rust std ignores SIGPIPE, which turns EPIPE into a println! panic.
    // NOT for `daemon`/`mcp`: they must survive client disconnects (their
    // teardown still runs and their socket writes handle EPIPE as an error).
    #[cfg(unix)]
    {
        let effective = if help_flag { "help" } else { cmd.as_str() };
        if effective != "daemon" && effective != "mcp" {
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
        }
    }

    if help_flag {
        print!("{}", bladebro::cli::help_text());
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(bladebro::BladeError::other)?;

    // Update hub commands don't need Chrome. Handle them before launch.
    let is_update_cmd = matches!(
        cmd.as_str(),
        "update"
            | "-u"
            | "doctor"
            | "-doc"
            | "rollback"
            | "--rollback"
            | "-v"
            | "--version"
            | "version"
    );

    // CLI commands go through the new CLI module (own Chrome management).
    let is_cli_cmd = matches!(
        cmd.as_str(),
        "nav"
            | "see"
            | "act"
            | "state"
            | "run"
            | "vision"
            | "daemon"
            | "stop"
            | "help"
            | "rb"
            | "realbrowser"
            | "hermes"
    );

    // Legacy debug commands that need a launched browser. They share the
    // same guard discipline as the CLI: the browser is ALWAYS shut down,
    // even on error — `exit_immediately` (below) skips destructors, so a
    // browser left alive on the stack here would leak Chrome + Xvfb on
    // every invocation (observed: `bladebro version` orphaned a full
    // Chrome tree per call).
    let is_debug_cmd = matches!(cmd.as_str(), "probe" | "targets" | "audit");

    // Reject unknown commands BEFORE launching Chrome. Without this,
    // `bladebro bogus` would launch a full Chrome+Xvfb tree just to print
    // an error.
    let is_known = is_update_cmd || is_cli_cmd || is_debug_cmd || cmd == "mcp";
    if !is_known {
        let mut line = format!("{} unknown command `{cmd}`", ui::red("✗"));
        if let Some(s) = bladebro::cli::suggest_command(&cmd) {
            line.push_str(&format!(" — did you mean `{s}`?"));
        }
        eprintln!("{line}");
        eprintln!("  {}", ui::dim("run `bladebro help` for the command list"));
        std::process::exit(2);
    }

    // S1 (default flipped 2026-09-26): the MCP server drives Chrome over
    // WebSocket by DEFAULT. The pipe transport enables Chrome's automation
    // flag, so `navigator.webdriver` reads `true` there and the only way to
    // hide it is a JS mask that lie engines catch (measured: CreepJS
    // `webDriverIsOn` + 33% headless; `Emulation.setAutomationOverride` does
    // NOT clear it — accepted, no effect). WS keeps the native `false` with
    // zero patches — the same lane the CLI daemon uses. `BLADE_TRANSPORT=pipe`
    // opts back into the zero-port transport.
    // CLI one-shot commands always use WS since they depend on HTTP target
    // discovery. Windows uses WS (pipe fds 3/4 don't exist on Windows).
    let use_pipe = port == 0
        && cmd == "mcp"
        && std::env::var("BLADE_TRANSPORT")
            .map(|v| v == "pipe")
            .unwrap_or(false)
        && cfg!(unix);

    // Auto-launch Chrome if port is 0 (default). When --port is explicitly
    // given, connect to the existing Chrome instance on that port.
    // MCP mode uses lazy launch (Chrome starts on first tool call), so
    // we skip the upfront launch for `mcp`. One-shot CLI commands
    // (probe, see, act, etc.) still launch Chrome here as before.
    // Pipe mode launches its own Chrome inside cmd_mcp_pipe.
    // Update hub commands skip Chrome entirely.
    let is_mcp = cmd == "mcp";
    let mut browser = if port == 0 && !use_pipe && !is_update_cmd && !is_mcp && !is_cli_cmd {
        Some(rt.block_on(bladebro::browser::Browser::launch(0))?)
    } else {
        None
    };
    let base = if port == 0 && !use_pipe && !is_update_cmd && !is_mcp && !is_cli_cmd {
        browser.as_ref().unwrap().base()
    } else {
        format!("{host}:{port}")
    };

    let result = match cmd.as_str() {
        "probe" => rt.block_on(with_browser_guard(&mut browser, || cmd_probe(&base))),
        "targets" => rt.block_on(with_browser_guard(&mut browser, || cmd_targets(&base))),
        // `version` prints the TOOL version — no Chrome launch needed.
        // (It used to spawn a full Chrome + Xvfb just to read /json/version,
        // then exit without dropping it: one orphaned browser per call.)
        "version" => {
            println!("bladebro {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        // CLI commands go through the new CLI module.
        "nav" | "see" | "act" | "state" | "run" | "vision" | "daemon" | "stop" | "help" | "rb"
        | "realbrowser" | "hermes" => {
            let mut cli_args: Vec<String> = std::iter::once(cmd.clone())
                .chain(positional.iter().cloned())
                .collect();
            // Forward explicit --host/--port so CLI tool commands (state,
            // see, act, run, nav, vision) connect to that already-running
            // Chrome instead of the daemon / a freshly launched instance.
            // Without this, `state --port 9222` silently ignored the port.
            if host_given {
                cli_args.push("--host".to_string());
                cli_args.push(host.clone());
            }
            if port_given {
                cli_args.push("--port".to_string());
                cli_args.push(port.to_string());
            }
            rt.block_on(bladebro::cli::run_cli(&cli_args))
        }
        // Update hub commands (no Chrome needed).
        "update" | "-u" => rt.block_on(bladebro::updater::run("update", &positional)),
        "doctor" | "-doc" => rt.block_on(bladebro::updater::run("doctor", &positional)),
        "rollback" | "--rollback" => rt.block_on(bladebro::updater::run("rollback", &positional)),
        "-v" | "--version" => rt.block_on(bladebro::updater::run("version", &positional)),
        "mcp" if use_pipe => rt.block_on(cmd_mcp_pipe()),
        "mcp" => rt.block_on(cmd_mcp(&host, port)),
        "audit" => rt.block_on(with_browser_guard(&mut browser, || {
            bladebro::audit::run_audit(&base)
        })),
        _ => {
            // Unknown command: NEVER launch a browser (it used to!), and
            // exit 2 as a usage error (see the exit-code contract in cli.rs).
            Err(bladebro::BladeError::Usage(format!(
                "unknown command: {cmd}"
            )))
        }
    };
    // Exit HERE, before `rt` drops: the MCP server reads
    // stdin on a blocking-pool thread, and after a
    // signal-driven shutdown that read stays parked —
    // Runtime::drop waits for it forever. All teardown is
    // complete at this point; skip the runtime drop.
    match result {
        Ok(()) => exit_immediately(0),
        Err(e) => {
            print_error(&e);
            exit_immediately(if matches!(e, bladebro::BladeError::Usage(_)) {
                2
            } else {
                1
            });
        }
    }
}

/// Run a legacy debug command (probe/targets/audit) and ALWAYS shut the
/// browser down afterward — including on error. `exit_immediately` skips
/// destructors, so without this guard every debug command orphans its
/// Chrome + Xvfb tree.
async fn with_browser_guard<F, Fut>(
    browser: &mut Option<bladebro::browser::Browser>,
    run: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let result = run().await;
    if let Some(b) = browser.take() {
        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
    }
    result
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
    println!(
        "  targets: {} total, {} page(s)",
        targets.len(),
        pages.len()
    );

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

async fn cmd_mcp(host: &str, port: u16) -> Result<()> {
    use bladebro::mcp;
    mcp::run(host, port).await
}

/// The default MCP path (S1): launch Chrome with CDP over pipe fds — no
/// debugging port exists for page JavaScript to probe.
/// Unix-only: Windows uses WS transport.
#[cfg(unix)]
async fn cmd_mcp_pipe() -> Result<()> {
    bladebro::mcp::run_pipe().await
}

/// Windows stub: pipe transport is Unix-only. This is never called
/// (use_pipe is always false on Windows) but the compiler needs the
/// symbol to resolve.
#[cfg(not(unix))]
async fn cmd_mcp_pipe() -> Result<()> {
    Err(bladebro::error::BladeError::Other(
        "pipe transport is Unix-only".into(),
    ))
}
