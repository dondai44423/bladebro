//! Self-update, rollback, and system diagnostics for Bladebro.
//!
//! Commands:
//! - `bladebro -u` / `bladebro update` — check for updates, download, swap
//! - `bladebro -u --check` — dry run: check for updates, don't install
//! - `bladebro -u --force` — force update even if same version
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
///
/// Flags:
///   --check / -c  — dry run: check but don't install
///   --force / -f  — force update even if same version (reinstall)
async fn update(args: &[String]) -> Result<()> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let dry_run = args.iter().any(|a| a == "--check" || a == "-c");

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

    // Dry run: stop here.
    if dry_run {
        ui::info("Dry run (--check): skipping download and install");
        ui::hint("To update: bladebro -u");
        return Ok(());
    }

    // npm detection: warn if installed via npm.
    if version::is_npm_install() && !force {
        ui::warn("This binary was installed via npm (detected node_modules in path)");
        ui::warn("Self-updating would break the npm installation.");
        println!();
        ui::info("To update via npm:");
        ui::hint("npm update -g bladebro");
        println!();
        ui::hint("Or force self-update: bladebro -u --force");
        return Ok(());
    }

    // Step 2: download the binary.
    let asset_name = version::platform_asset_name();
    let asset_size = version::find_asset(&latest).map(|a| a.size).unwrap_or(0);
    let download_label = if asset_size > 0 {
        format!("{asset_name} (~{:.1} MB)", asset_size as f64 / 1_000_000.0)
    } else {
        asset_name
    };
    ui::step(2, 4, &format!("Downloading {download_label}..."));

    let binary_path = download::download_binary(&latest).await?;
    ui::info(&format!(
        "Downloaded {:.1} MB",
        binary_path.size as f64 / 1_000_000.0
    ));

    // Step 3: verify the download.
    ui::step(3, 4, "Verifying...");
    download::verify_binary(&binary_path)?;
    ui::info("Binary verified (magic + size)");

    // Try executing the binary to confirm it runs.
    match download::verify_binary_runs(&binary_path) {
        Ok(()) => ui::info("Binary starts: confirmed"),
        Err(e) => {
            download::cleanup_tmp(&binary_path.path);
            return Err(BladeError::Other(format!(
                "downloaded binary failed verification: {e}\n\
                 The binary may be for the wrong architecture or corrupted.\n\
                 Try: npm install -g bladebro"
            )));
        }
    }

    // Step 4: swap.
    ui::step(4, 4, "Installing...");
    let backup = swap::swap_binary(&binary_path.path)?;
    ui::info(&format!("Backed up previous version to {}", backup.display()));

    // Clean up temp file.
    download::cleanup_tmp(&binary_path.path);

    ui::success(&format!("Updated to {}", latest.tag()));
    ui::hint("Restart your MCP client to use the new version.");
    ui::hint("If anything breaks: bladebro --rollback");
    Ok(())
}

/// `bladebro -v` / `bladebro --version` — show version + update status.
///
/// Uses `fetch_latest_tag()` (non-rate-limited github.com redirect + npm)
/// instead of `fetch_latest()` (GitHub API, 60 req/hr unauthenticated).
/// Three honest states, never a misleading "up to date":
/// - behind:  update available
/// - equal:   on the latest release
/// - ahead:   local build is newer than the latest release
async fn show_version() -> Result<()> {
    println!("bladebro v{CURRENT_VERSION} ({BUILD_ID})");

    // Show install method.
    let method = version::install_method();
    if method == "npm" {
        println!("  install: npm");
    } else if method == "source" {
        println!("  install: source build");
    }

    match version::fetch_latest_tag().await {
        Ok(latest_tag) => {
            match version::compare_versions(&latest_tag, CURRENT_VERSION) {
                std::cmp::Ordering::Greater => {
                    println!("  update available: {latest_tag}");
                    println!("  run: bladebro -u");
                }
                std::cmp::Ordering::Equal => {
                    println!("  on the latest release");
                }
                std::cmp::Ordering::Less => {
                    println!("  ahead of the latest release ({latest_tag})");
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
