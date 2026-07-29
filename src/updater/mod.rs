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

/// Git commit + dirty flag, baked in by build.rs.
/// Lets `-v` identify exactly what code a binary runs.
pub const BUILD_ID: &str = env!("BLADE_BUILD_ID");

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
        if version::compare_versions(latest.tag(), current) == std::cmp::Ordering::Less {
            ui::info(&format!(
                "Local build (v{current}) is ahead of the latest release ({})",
                latest.tag()
            ));
        } else {
            ui::info(&format!("Already on the latest release (v{current})"));
        }
        return Ok(());
    }

    if version::is_newer(latest.tag(), current) {
        ui::info(&format!("Update available: v{current} -> {}", latest.tag()));
    } else if version::compare_versions(latest.tag(), current) == std::cmp::Ordering::Less {
        ui::warn(&format!(
            "Local build (v{current}) is AHEAD of the latest release ({}).",
            latest.tag()
        ));
        ui::warn("Downgrading is only possible with --force.");
        if !force {
            return Ok(());
        }
    } else {
        ui::warn("Force update requested (same version)");
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
///
/// Three honest states, never a misleading "up to date":
/// - behind:  update available → tells you to run -u
/// - equal:   on the latest release
/// - ahead:   local build is NEWER than the latest release
///   (dev build or pending release) — says so.
async fn show_version() -> Result<()> {
    println!("bladebro v{CURRENT_VERSION} ({BUILD_ID})");
    match version::fetch_latest().await {
        Ok(latest) => {
            match version::compare_versions(latest.tag(), CURRENT_VERSION) {
                std::cmp::Ordering::Greater => {
                    println!("  update available: {}", latest.tag());
                    println!("  run: bladebro -u");
                }
                std::cmp::Ordering::Equal => {
                    println!("  on the latest release");
                }
                std::cmp::Ordering::Less => {
                    println!("  ahead of the latest release ({})", latest.tag());
                    println!("  (local build is newer — dev build or pending release)");
                }
            }
        }
        Err(_) => {
            println!("  (could not check for updates)");
        }
    }
    Ok(())
}
