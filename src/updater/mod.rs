//! Self-update, rollback, and system diagnostics for Bladebro.
//!
//! Commands:
//! - `bladebro -u` / `bladebro update` — check for updates, download, swap
//! - `bladebro -doc` / `bladebro doctor` — diagnose system, suggest fixes
//! - `bladebro --rollback` — restore previous binary after broken update
//! - `bladebro -v` / `bladebro --version` — show version + update status

pub mod download;
pub mod doctor;
pub mod swap;
pub mod ui;
pub mod version;

use crate::error::{BladeError, Result};

/// The current version, baked in at compile time.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub repo for release checks.
const GITHUB_REPO: &str = "dondai44423/bladebro";

/// Dispatch an update-hub command.
pub async fn run(cmd: &str, args: &[String]) -> Result<()> {
    match cmd {
        "update" | "-u" => update(args).await,
        "doctor" | "-doc" => doctor::run().await,
        "rollback" | "--rollback" => swap::rollback().await,
        "version" | "-v" | "--version" => show_version().await,
        other => Err(BladeError::Other(format!(
            "unknown command: {other}\n  \
             Usage: bladebro <mcp|audit|see|act|state|run|nav|probe|targets|version>\n  \
             Update hub: bladebro <update|-u|doctor|-doc|rollback|--rollback|-v|--version>"
        ))),
    }
}

/// `bladebro update` / `bladebro -u` — check for updates, download, swap.
async fn update(args: &[String]) -> Result<()> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");

    ui::header("Bladebro Update");

    // Step 1: check current vs latest.
    ui::step(1, 4, "Checking for updates...");
    let latest = version::fetch_latest().await?;
    let current = CURRENT_VERSION;

    if !force && !version::is_newer(latest.tag(), current) {
        ui::info(&format!("Already up to date (v{current})"));
        return Ok(());
    }

    if version::is_newer(latest.tag(), current) {
        ui::info(&format!("Update available: v{current} -> {}", latest.tag()));
    } else {
        ui::warn("Force update requested (same or older version)");
    }

    // Step 2: download the binary.
    ui::step(2, 4, "Downloading...");
    let binary_path = download::download_binary(&latest).await?;
    ui::info(&format!("Downloaded {} bytes", binary_path.size));

    // Step 3: verify the download.
    ui::step(3, 4, "Verifying...");
    download::verify_binary(&binary_path)?;
    ui::info("Binary verified");

    // Step 4: swap.
    ui::step(4, 4, "Installing...");
    let backup = swap::swap_binary(&binary_path.path)?;
    ui::info(&format!("Backed up previous version to {}", backup.display()));

    ui::success(&format!("Updated to {}", latest.tag()));
    ui::hint("Restart your MCP client to use the new version.");
    ui::hint("If anything breaks: bladebro --rollback");
    Ok(())
}

/// `bladebro -v` / `bladebro --version` — show version + update status.
async fn show_version() -> Result<()> {
    println!("bladebro v{CURRENT_VERSION}");
    match version::fetch_latest().await {
        Ok(latest) => {
            if version::is_newer(latest.tag(), CURRENT_VERSION) {
                println!("  update available: {}", latest.tag());
                println!("  run: bladebro -u");
            } else {
                println!("  up to date");
            }
        }
        Err(_) => {
            println!("  (could not check for updates)");
        }
    }
    Ok(())
}
