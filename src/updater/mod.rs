//! Self-update, rollback, and system diagnostics for Bladebro.
//!
//! Commands:
//! - `bladebro -u` / `bladebro update` — check for updates, download, swap
//! - `bladebro -u --check` — dry run: check for updates, don't install
//! - `bladebro -u --force` — force update even if same version
//! - `bladebro -doc` / `bladebro doctor` — diagnose system, suggest fixes
//! - `bladebro --rollback` — restore previous binary after broken update
//! - `bladebro -v` / `bladebro --version` — show version + update status

pub mod doctor;
pub mod download;
pub mod swap;
pub use crate::ui;
pub mod version;

use std::time::Instant;

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
    let started = Instant::now();
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let dry_run = args.iter().any(|a| a == "--check" || a == "-c");

    let meta = format!(
        "{} · {}",
        version::platform_label(),
        version::install_channel_label()
    );
    ui::masthead(
        if dry_run {
            "Update Check"
        } else {
            "Self-Update"
        },
        &meta,
    );

    // Phase 1: check current vs latest.
    ui::phase(1, 4, "Consulting the syndicate registry");
    let spin = ui::Spinner::start("Checking latest GitHub & npm release...");
    let latest = match version::fetch_latest().await {
        Ok(rel) => rel,
        Err(e) => {
            spin.finish_fail("Could not reach release registry");
            return Err(e);
        }
    };
    let current = CURRENT_VERSION;
    let cmp = version::compare_versions(latest.tag(), current);

    if !force && cmp != std::cmp::Ordering::Greater {
        if cmp == std::cmp::Ordering::Less {
            spin.finish_ok(&format!(
                "Local build v{current} is ahead of latest release {}",
                latest.tag_name
            ));
            ui::verdict_card(
                ui::Tone::Gold,
                &format!("LOCAL BUILD AHEAD · v{current}"),
                &ui::format_elapsed(started.elapsed()),
                &[
                    ("published", latest.tag_name.clone()),
                    ("note", "Running a dev build or pending release".into()),
                    (
                        "downgrade",
                        "Pass --force to reinstall the published release".into(),
                    ),
                ],
            );
        } else {
            spin.finish_ok(&format!("Already on the latest release (v{current})"));
            ui::verdict_card(
                ui::Tone::Success,
                &format!("ON THE LATEST RELEASE · v{current}"),
                &ui::format_elapsed(started.elapsed()),
                &[
                    ("channel", version::install_channel_label().into()),
                    ("reinstall", "Pass --force to reinstall this version".into()),
                ],
            );
        }
        return Ok(());
    }

    match cmp {
        std::cmp::Ordering::Greater => {
            spin.finish_ok(&format!(
                "Release {} available  (v{current} → {})",
                latest.tag_name, latest.tag_name
            ));
        }
        std::cmp::Ordering::Less => {
            spin.finish_warn(&format!(
                "Forcing downgrade: v{current} → {}",
                latest.tag_name
            ));
        }
        std::cmp::Ordering::Equal => {
            spin.finish_warn(&format!(
                "Force reinstall requested for {}",
                latest.tag_name
            ));
        }
    }

    // Dry run: stop here.
    if dry_run {
        ui::verdict_card(
            ui::Tone::Gold,
            &format!("UPDATE AVAILABLE · v{current} → {}", latest.tag_name),
            &ui::format_elapsed(started.elapsed()),
            &[
                ("mode", "dry run (--check) — no files touched".into()),
                ("upgrade", version::update_command_hint().into()),
            ],
        );
        return Ok(());
    }

    // Package-manager detection: redirect to npm/pnpm/bun/yarn/Homebrew unless --force.
    if version::is_package_managed() && !force {
        let channel = version::install_channel_label();
        let cmd = version::update_command_hint();
        ui::verdict_card(
            ui::Tone::Warn,
            &format!("MANAGED BY {} — USE YOUR PACKAGE MANAGER", channel.to_uppercase()),
            &ui::format_elapsed(started.elapsed()),
            &[
                ("reason", format!("Installed via {channel}; overwriting in-place would drift from your package manager")),
                ("upgrade", cmd.into()),
                ("override", "bladebro -u --force (forces direct binary swap)".into()),
            ],
        );
        return Ok(());
    }

    // Android publishes no builds: the released Linux binaries are glibc-linked
    // and cannot run on bionic, so refuse before downloading.
    if cfg!(target_os = "android") {
        return Err(BladeError::Other(
            "self-update is not available on Android: the published Linux binaries are glibc builds and cannot run on Android.\n  \
             Update from wherever this install came from (package manager, npm, or your own build)."
                .to_string(),
        ));
    }

    // Pre-flight writability check BEFORE spending bandwidth on an 8 MB download.
    swap::check_writable_current()?;

    // Phase 2: concurrent binary download + SHA-256 sidecar fetch with live progress bar.
    let asset_name = version::platform_asset_name();
    ui::phase(2, 4, &format!("Acquiring {asset_name} & SHA-256 seal"));
    let binary_path = download::download_binary(&latest).await?;

    // Phase 3: integrity verification (SHA-256 + magic + size + live execution probe).
    ui::phase(3, 4, "Verifying cryptographic seal & binary health");
    if let Some(ref digest) = binary_path.sha256 {
        let short = if digest.len() >= 16 {
            format!("{}…{}", &digest[..8], &digest[digest.len() - 8..])
        } else {
            digest.clone()
        };
        ui::success(&format!("SHA-256 seal verified  (sha256:{short})"));
    } else {
        ui::warn("Legacy pre-3.3.0 release — no .sha256 sidecar published");
    }

    download::verify_binary(&binary_path)?;
    ui::success(&format!(
        "Platform binary verified  ({} · magic + bounds + exec mode)",
        ui::format_bytes(binary_path.size)
    ));

    let probe_spin = ui::Spinner::start("Running live startup smoke test (--version)...");
    match download::verify_binary_runs(&binary_path).await {
        Ok(()) => probe_spin.finish_ok("Live startup verified — new binary executes cleanly"),
        Err(e) => {
            probe_spin.finish_fail("Live startup probe failed");
            return Err(BladeError::Other(format!(
                "downloaded binary failed verification: {e}\n  \
                 The binary may be for the wrong architecture or corrupted.\n  \
                 Try: npm install -g bladebro"
            )));
        }
    }

    // Phase 4: backup + atomic swap + stale daemon stop.
    ui::phase(4, 4, "Installing & sealing backup vault");
    let swap_spin = ui::Spinner::start("Backing up current binary & performing atomic swap...");
    let backup = match swap::swap_binary(&binary_path.path) {
        Ok(b) => {
            swap_spin.finish_ok("Atomic binary swap complete");
            b
        }
        Err(e) => {
            swap_spin.finish_fail("Atomic swap failed");
            return Err(e);
        }
    };

    let daemon_stopped = crate::cli::stop_daemon_if_running().await;
    let mut rows = vec![
        ("release", format!("v{current}  →  {}", latest.tag_name)),
        ("vault", backup.display().to_string()),
        ("rollback", "bladebro --rollback".into()),
    ];
    if daemon_stopped {
        rows.push((
            "daemon",
            format!(
                "stopped old daemon — next command starts on {}",
                latest.tag_name
            ),
        ));
    }
    rows.push((
        "clients",
        "Restart any running MCP client to pick up the new binary".into(),
    ));

    ui::verdict_card(
        ui::Tone::Success,
        &format!("UPGRADE COMPLETE · {}", latest.tag_name),
        &ui::format_elapsed(started.elapsed()),
        &rows,
    );
    Ok(())
}

/// `bladebro -v` / `bladebro --version` — show version identity card + live
/// release status.
///
/// Uses `fetch_latest_tag()` (non-rate-limited GitHub HEAD redirect + npm).
/// Three honest states, never a misleading "up to date":
/// - behind:  update available
/// - equal:   on the latest release
/// - ahead:   local build is newer than the latest release
async fn show_version() -> Result<()> {
    let platform = version::platform_label();
    let channel = version::install_channel_label();
    let check_disabled = std::env::var("BLADE_NO_UPDATE_CHECK")
        .map(|v| v == "1")
        .unwrap_or(false);

    let fetched = if check_disabled {
        None
    } else {
        let spin = ui::Spinner::start("Checking latest release...");
        let res = version::fetch_latest_tag().await;
        spin.finish_clear();
        Some(res)
    };

    match fetched {
        None => {
            ui::version_card(&ui::VersionCard {
                version: CURRENT_VERSION,
                build_id: BUILD_ID,
                platform: &platform,
                install_channel: channel,
                status_tone: ui::Tone::Smoke,
                status_badge: "OFFLINE",
                status_detail: "update check disabled (BLADE_NO_UPDATE_CHECK=1)",
                action_cmd: None,
            });
        }
        Some(Ok(latest_tag)) => match version::compare_versions(&latest_tag, CURRENT_VERSION) {
            std::cmp::Ordering::Greater => {
                let hint = version::update_command_hint();
                let detail = if cfg!(target_os = "android") {
                    format!("v{latest_tag} available (no Android prebuilds — use package manager)")
                } else {
                    format!("update available: v{CURRENT_VERSION} → v{latest_tag}")
                };
                let cmd = if cfg!(target_os = "android") {
                    None
                } else {
                    Some(hint)
                };
                ui::version_card(&ui::VersionCard {
                    version: CURRENT_VERSION,
                    build_id: BUILD_ID,
                    platform: &platform,
                    install_channel: channel,
                    status_tone: ui::Tone::Gold,
                    status_badge: "UPDATE",
                    status_detail: &detail,
                    action_cmd: cmd,
                });
            }
            std::cmp::Ordering::Equal => {
                ui::version_card(&ui::VersionCard {
                    version: CURRENT_VERSION,
                    build_id: BUILD_ID,
                    platform: &platform,
                    install_channel: channel,
                    status_tone: ui::Tone::Success,
                    status_badge: "LATEST",
                    status_detail: "on the latest release",
                    action_cmd: None,
                });
            }
            std::cmp::Ordering::Less => {
                let detail = format!("ahead of latest release (v{latest_tag}) — local dev build");
                ui::version_card(&ui::VersionCard {
                    version: CURRENT_VERSION,
                    build_id: BUILD_ID,
                    platform: &platform,
                    install_channel: channel,
                    status_tone: ui::Tone::Warn,
                    status_badge: "AHEAD",
                    status_detail: &detail,
                    action_cmd: None,
                });
            }
        },
        Some(Err(_)) => {
            ui::version_card(&ui::VersionCard {
                version: CURRENT_VERSION,
                build_id: BUILD_ID,
                platform: &platform,
                install_channel: channel,
                status_tone: ui::Tone::Smoke,
                status_badge: "OFFLINE",
                status_detail: "could not reach release registry",
                action_cmd: None,
            });
        }
    }
    Ok(())
}
