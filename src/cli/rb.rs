//! `bladebro rb` — the real-browser lane command surface: switch, inspect,
//! pick browser/profile, pause/resume, refresh/forget the clone.

use crate::error::Result;

use super::daemon::{restart_daemon_for_lane, stop_daemon_for_clone_change};

/// One aligned `label  value` row for the `rb` blocks: labels dim, column
/// width 9 (the longest label, `mechanism`).
fn rb_row(label: &str, value: &str) -> String {
    crate::ui::kv(label, value, 9)
}

/// `rb refresh` / `rb forget` must not run under a live clone session (an
/// MCP server, a second daemon): only the CLI daemon can be stopped from
/// here, and any other session's exit copies its stale clone back over a
/// refresh — or its browser keeps running inside a wiped root.
fn refuse_under_live_clone_sessions(id: &str, sub: &str) -> Result<()> {
    let pids = crate::realbrowser::live_clone_sessions(id);
    if pids.is_empty() {
        return Ok(());
    }
    let owners: Vec<String> = pids.iter().map(|p| format!("pid {p}")).collect();
    Err(crate::error::BladeError::Other(format!(
        "the `{id}` clone is in use by a running bladebro session ({}) — {} Close that \
         session (its MCP client, or `bladebro stop`), then retry `bladebro rb {sub}`.",
        owners.join(", "),
        if sub == "forget" {
            "wiping it would delete the profile under its open browser."
        } else {
            "its exit would copy the old clone back over the refresh."
        }
    )))
}

/// `bladebro rb ...` — switch between the default agent browser and the
/// user's real browser (S18). Local config work only: nothing here talks to
/// a running browser except the daemon restart that makes the switch take
/// effect on the next launch.
pub(super) async fn run_rb(args: &[String], json_mode: bool) -> Result<()> {
    use crate::realbrowser as rb;
    use crate::ui;

    let raw_sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    let sub = match raw_sub {
        "true" | "1" | "enable" => "on",
        "false" | "0" | "disable" => "off",
        other => other,
    };
    let rest = &args[1.min(args.len())..];
    let mut cfg = rb::config();

    match sub {
        "status" => {
            let paused = rb::input_paused();
            let sel = rb::resolve_selection(&cfg);
            if json_mode {
                let attach_port = sel.as_ref().ok().and_then(|(_, p)| rb::devtools_port(&p.root));
                let attach_live = sel
                    .as_ref()
                    .ok()
                    .and_then(|(_, p)| rb::devtools_port_live(&p.root))
                    .is_some();
                let template_fresh = sel
                    .as_ref()
                    .ok()
                    .map(|(s, p)| rb::template_is_fresh(&s.id, &p.root, &p.key));
                let source_in_use = sel
                    .as_ref()
                    .ok()
                    .and_then(|(_, p)| rb::profile_in_use(&p.root));
                let v = serde_json::json!({
                    "ok": true,
                    "enabled": cfg.enabled,
                    "mode": cfg.mode.as_str(),
                    "effective_mode": sel.as_ref().ok().map(|(_, p)| rb::effective_mode(&cfg, &p.root).as_str()),
                    "browser": cfg.browser.clone(),
                    "profile": cfg.profile.clone(),
                    "binary": cfg.binary.clone(),
                    "attach_port": attach_port,
                    "attach_live": attach_live,
                    "visible": cfg.visible,
                    "idle_shutdown": cfg.idle_shutdown,
                    "idle_hum": cfg.idle_hum,
                    "paused": paused,
                    "browsers_found": rb::discover().iter().map(|b| b.id.clone()).collect::<Vec<_>>(),
                    "template": sel.as_ref().ok().map(|(s, _)| rb::has_template(&s.id)),
                    "template_fresh": template_fresh,
                    "source": sel.as_ref().ok().and_then(|(s, _)| rb::template_source(&s.id)),
                    "source_in_use": source_in_use,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            let badge = if cfg.enabled {
                ("ACTIVE", ui::Tone::Success)
            } else {
                ("STANDBY", ui::Tone::Smoke)
            };
            let mut rows: Vec<(&str, String)> = Vec::new();
            match sel {
                Ok((spec, profile)) => {
                    let auto = if cfg.browser.is_none() {
                        " · auto (most recently used)"
                    } else {
                        ""
                    };
                    rows.push((
                        "browser",
                        format!("{} ({}){auto}", spec.name, spec.brand.as_str()),
                    ));
                    let bin = if spec.binary.as_os_str().is_empty() {
                        ui::amber("none found — install it or set `rb use --binary <path>`")
                    } else {
                        let p = spec.binary.display().to_string();
                        if cfg.binary.is_some() {
                            format!("{p}  {}", ui::smoke("(override)"))
                        } else {
                            p
                        }
                    };
                    rows.push(("binary", bin));
                    rows.push((
                        "profile",
                        format!(
                            "\"{}\" · {}",
                            profile.name,
                            ui::smoke(&profile.path.display().to_string())
                        ),
                    ));
                    rows.push((
                        "mode",
                        format!(
                            "{} → {} (now)",
                            cfg.mode.as_str(),
                            rb::effective_mode(&cfg, &profile.root).as_str()
                        ),
                    ));
                    if let Some(port) = rb::devtools_port_live(&profile.root) {
                        rows.push((
                            "attach",
                            format!("live debug endpoint on 127.0.0.1:{port} — auto will attach"),
                        ));
                    } else if let Some(port) = rb::devtools_port(&profile.root) {
                        rows.push((
                            "attach",
                            ui::smoke(&format!(
                                "stale DevToolsActivePort ({port}, not listening) — ignored"
                            )),
                        ));
                    }
                    match rb::template_stats(&spec.id) {
                        Some((files, bytes)) => {
                            let freshness = if rb::template_is_fresh(&spec.id, &profile.root, &profile.key) {
                                ui::green("up to date")
                            } else {
                                ui::amber("stale — source profile updated (auto-refreshes on `rb on` or next launch)")
                            };
                            rows.push((
                                "clone",
                                format!("{files} files ({}) · {freshness}", rb::human_bytes(bytes)),
                            ));
                            if let Some(src) = rb::template_source(&spec.id) {
                                rows.push(("source", ui::smoke(&src)));
                            }
                            let failed = rb::template_failed_critical(&spec.id);
                            if !failed.is_empty() {
                                rows.push((
                                    "warning",
                                    ui::amber(&format!(
                                        "last import could not copy: {} — close {} and run `rb refresh`",
                                        failed.join(", "),
                                        spec.name
                                    )),
                                ));
                            }
                        }
                        None => rows.push((
                            "clone",
                            ui::smoke("not imported yet (first launch imports)"),
                        )),
                    }
                }
                Err(e) => rows.push(("selection", ui::crimson(&e.to_string()))),
            }
            if paused {
                rows.push((
                    "paused",
                    ui::amber("yes — input, navigation, downloads, collecting and tab ops refuse until `rb resume`"),
                ));
            }
            ui::dossier_card(
                "Real-Browser Lane",
                badge,
                &rows,
                Some("`bladebro rb on|off` switches the lane · `bladebro help rb` for the full story"),
            );
            Ok(())
        }

        "on" => {
            if cfg.enabled {
                // Already on: still self-heal a missing, mismatched, or stale
                // clone (e.g. after logging into a site in the real browser,
                // `rb forget`, or `rb profile` into another user-data root).
                // The fresh import bumps `.blade-import`, which makes any
                // running MCP/daemon session relaunch onto the new clone on its
                // next call.
                let mut refreshed = false;
                if let Ok((spec, profile)) = rb::resolve_selection(&cfg) {
                    if rb::effective_mode(&cfg, &profile.root) == rb::Mode::Clone
                        && !rb::template_is_fresh(&spec.id, &profile.root, &profile.key)
                    {
                        restart_daemon_for_lane().await;
                        refreshed = rb::ensure_current_import(&spec, &profile)?.is_some();
                    }
                }
                if json_mode {
                    println!("{}", serde_json::json!({
                        "ok": true,
                        "enabled": true,
                        "refreshed": refreshed,
                        "note": if refreshed { "already on (clone refreshed from newer profile)" } else { "already on" }
                    }));
                } else if refreshed {
                    println!(
                        "{} {} — refreshed clone with newer logins/state (`bladebro rb status` for details)",
                        ui::bold("Real-browser lane:"),
                        ui::bold(&ui::green("already ON"))
                    );
                } else {
                    println!("{} {}", ui::bold("Real-browser lane:"), ui::bold(&ui::green("already ON")) + " — `bladebro rb status` for details");
                }
                return Ok(());
            }
            let (spec, profile) = rb::resolve_selection(&cfg)?;
            let mode = rb::effective_mode(&cfg, &profile.root);
            if mode != rb::Mode::Attach && spec.binary.as_os_str().is_empty() {
                return Err(crate::error::BladeError::Other(format!(
                    "{} has a profile dir but no launchable binary — install it, or pick \
                     another browser with `bladebro rb use`.",
                    spec.name
                )));
            }
            // Stop any running CLI daemon BEFORE replacing the clone template
            // so its teardown never races with the import.
            restart_daemon_for_lane().await;
            // Import now (clone lane) so problems surface HERE, not inside a
            // later agent run.
            let imported = if mode == rb::Mode::Clone {
                rb::ensure_current_import(&spec, &profile)?
            } else {
                None
            };
            cfg.enabled = true;
            rb::save_config(&cfg)?;
            if json_mode {
                let v = serde_json::json!({
                    "ok": true, "enabled": true, "mode": mode.as_str(),
                    "browser": spec.id, "profile": profile.key,
                    "refreshed": imported.is_some(),
                    "failed_critical": imported.as_ref().map(|s| &s.failed_critical),
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            let mut rows = vec![
                ("browser", format!("{} ({})", spec.name, spec.brand.as_str())),
                (
                    "profile",
                    format!(
                        "\"{}\" · {}",
                        profile.name,
                        ui::smoke(&profile.path.display().to_string())
                    ),
                ),
                ("mechanism", mode.as_str().to_string()),
                (
                    "stealth",
                    "zero page patches — your real environment IS the stealth".into(),
                ),
                (
                    "caution",
                    ui::amber("the agent browses AS YOU (accounts, sessions, reputation) over a loopback debug port"),
                ),
                (
                    "revert",
                    "`bladebro rb off` · wipe imported copy: `rb forget`".into(),
                ),
                (
                    "control",
                    "`rb pause` / `rb resume` hand the browser back to you".into(),
                ),
            ];
            if let Some(stats) = &imported {
                if !stats.failed_critical.is_empty() {
                    rows.push((
                        "warning",
                        ui::amber(&format!(
                            "locked profile file(s) skipped: {} — close {} and run `rb refresh`",
                            stats.failed_critical.join(", "),
                            spec.name
                        )),
                    ));
                }
            }
            ui::dossier_card(
                "Real-Browser Lane",
                ("ON", ui::Tone::Success),
                &rows,
                Some("Running surfaces (daemon, MCP) switch automatically at their next action"),
            );
            Ok(())
        }

        "off" => {
            if !cfg.enabled {
                if json_mode {
                    println!("{}", serde_json::json!({"ok": true, "enabled": false, "note": "already off"}));
                } else {
                    println!("{} {}", ui::bold("Real-browser lane:"), ui::dim("already off"));
                }
                return Ok(());
            }
            cfg.enabled = false;
            rb::save_config(&cfg)?;
            let _ = rb::set_paused(false);
            restart_daemon_for_lane().await;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "enabled": false}));
            } else {
                println!(
                    "{} {} — the isolated agent browser is the default again",
                    ui::bold("Real-browser lane:"),
                    ui::dim("off")
                );
                println!(
                    "{}",
                    rb_row("note", "a running browser is switched at its next use — relaunched if owned, left running if attached; no restart needed")
                );
            }
            Ok(())
        }

        "mode" => {
            let m = rest.first().ok_or_else(|| {
                crate::error::BladeError::Usage(
                    "bladebro rb mode <auto|clone|profile|attach>".into(),
                )
            })?;
            let mode = match m.as_str() {
                "auto" => rb::Mode::Auto,
                "clone" => rb::Mode::Clone,
                "profile" => rb::Mode::Profile,
                "attach" => rb::Mode::Attach,
                other => {
                    return Err(crate::error::BladeError::Usage(format!(
                        "unknown mode `{other}` — use auto|clone|profile|attach"
                    )))
                }
            };
            cfg.mode = mode;
            rb::save_config(&cfg)?;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "mode": mode.as_str()}));
            } else {
                println!("mechanism set to {}", ui::bold(mode.as_str()));
                if cfg.enabled {
                    println!("  {}", ui::dim("applies at the running session's next action — owned browsers relaunch; attach sessions detach"));
                }
            }
            Ok(())
        }

        "visible" | "idle-hum" | "idle-shutdown" => {
            let val = rest.first().map(|s| s.as_str()).unwrap_or("");
            let on = match val {
                "on" | "true" | "1" | "yes" => true,
                "off" | "false" | "0" | "no" => false,
                _ => {
                    return Err(crate::error::BladeError::Usage(format!(
                        "bladebro rb {sub} <on|off>"
                    )))
                }
            };
            match sub {
                "visible" => cfg.visible = on,
                "idle-hum" => cfg.idle_hum = on,
                _ => cfg.idle_shutdown = on,
            }
            rb::save_config(&cfg)?;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "setting": sub, "value": on}));
            } else {
                match sub {
                    "visible" => {
                        let (head, tail) = if on {
                            (ui::green("visible"), "the browser opens as a real window (the point of the lane)")
                        } else {
                            (ui::dim("invisible"), "--headless=new; honest, but a degraded environment (servers/CI)")
                        };
                        println!("{head} — {tail}");
                    }
                    "idle-hum" => {
                        let (head, tail) = if on {
                            (ui::green("idle hum on"), " (silent while paused)")
                        } else {
                            (ui::dim("idle hum off"), "")
                        };
                        println!("{head}{tail}");
                    }
                    _ => {
                        let (head, tail) = if on {
                            (ui::yellow("idle shutdown on"), " — the idle timeout may close a real-lane browser")
                        } else {
                            (ui::dim("idle shutdown off"), " — a real-lane browser is never closed by the idle timeout (default)")
                        };
                        println!("{head}{tail}");
                    }
                }
            }
            Ok(())
        }

        "use" => {
            // `rb use --binary <path|auto>` — a custom binary (nix wrapper,
            // flatpak launcher script, dev build) or a reset to discovery.
            let bin_arg = rest.first().and_then(|a| {
                if a == "--binary" {
                    Some(rest.get(1).cloned().unwrap_or_default())
                } else {
                    a.strip_prefix("--binary=").map(String::from)
                }
            });
            if let Some(val) = bin_arg {
                if val.is_empty() {
                    return Err(crate::error::BladeError::Usage(
                        "bladebro rb use --binary <path|auto>".into(),
                    ));
                }
                if val == "auto" {
                    cfg.binary = None;
                    rb::save_config(&cfg)?;
                    if !json_mode {
                        println!("binary override cleared — auto-discovery again");
                    }
                } else {
                    let p = rb::validate_binary_override(&val)?;
                    cfg.binary = Some(p.display().to_string());
                    rb::save_config(&cfg)?;
                    if !json_mode {
                        println!("binary override: {}", p.display());
                    }
                }
                if json_mode {
                    println!("{}", serde_json::json!({"ok": true, "binary": cfg.binary}));
                }
                return Ok(());
            }
            match rest.first() {
                None => {
                    let browsers = rb::discover();
                    if json_mode {
                        println!("{}", serde_json::json!({
                            "ok": true, "browser": cfg.browser, "binary": cfg.binary,
                            "browsers": browsers.iter().map(|b| serde_json::json!({
                                "id": b.id, "name": b.name, "binary": b.binary,
                                "profile_root": b.profile_root,
                            })).collect::<Vec<_>>()
                        }));
                        return Ok(());
                    }
                    if browsers.is_empty() {
                        println!("{}", ui::yellow("no Chromium-family browsers found"));
                    }
                    let selected = rb::resolve_selection(&cfg).ok().map(|(s, _)| s.id);
                    let id_w = browsers.iter().map(|b| b.id.chars().count()).max().unwrap_or(2);
                    let name_w = browsers.iter().map(|b| b.name.chars().count()).max().unwrap_or(4);
                    for b in browsers {
                        let sel = selected.as_deref() == Some(b.id.as_str());
                        let star = if sel { ui::green("*") } else { " ".to_string() };
                        let idp = format!("{:<id_w$}", b.id);
                        let id = if sel { ui::bold(&idp) } else { idp };
                        let name = format!("{:<name_w$}", b.name);
                        let missing = if b.binary.as_os_str().is_empty() {
                            format!("  {}", ui::yellow("[binary missing]"))
                        } else {
                            String::new()
                        };
                        println!(
                            "  {star} {id}  {name}  {}{missing}",
                            ui::dim(&b.profile_root.display().to_string())
                        );
                    }
                    if let Some(ov) = cfg.binary.as_deref() {
                        println!("{}", rb_row("override", ov));
                    }
                    println!();
                    println!(
                        "{}",
                        ui::dim(
                            "hint: `bladebro rb use <id>` selects one · custom / nix / flatpak install: `rb use --binary <path>`"
                        )
                    );
                }
                Some(id) => {
                    let spec = rb::find_browser(id).ok_or_else(|| {
                        let avail: Vec<String> = rb::discover().iter().map(|b| b.id.clone()).collect();
                        crate::error::BladeError::Other(format!(
                            "browser `{id}` not found. Available: {}",
                            avail.join(", ")
                        ))
                    })?;
                    let cleared = cfg.binary.is_some();
                    cfg.browser = Some(id.clone());
                    cfg.binary = None; // an explicit browser switch resets a custom binary
                    rb::save_config(&cfg)?;
                    if !json_mode {
                        println!("browser set to {} ({})", ui::bold(&spec.name), ui::dim(&spec.binary.display().to_string()));
                        if cleared {
                            println!(
                                "  {}",
                                ui::dim("binary override cleared (re-apply with `rb use --binary` if wanted)")
                            );
                        }
                        if !rb::has_template(&spec.id) && cfg.enabled {
                            println!("  {}", ui::dim(&format!("note: no clone for `{id}` yet — the first launch imports it")));
                        }
                    }
                }
            }
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "browser": cfg.browser}));
            }
            Ok(())
        }

        "profile" => {
            match rest.first().map(|s| s.as_str()) {
                None => {
                    let (spec, _) = rb::resolve_selection(&rb::Config {
                        profile: None,
                        ..cfg.clone()
                    })?;
                    let profiles = rb::list_profiles(&spec.profile_root);
                    if json_mode {
                        println!("{}", serde_json::json!({
                            "ok": true, "profile": cfg.profile,
                            "profiles": profiles.iter().map(|p| serde_json::json!({
                                "key": p.key, "name": p.name, "path": p.path,
                            })).collect::<Vec<_>>()
                        }));
                        return Ok(());
                    }
                    let key_w = profiles.iter().map(|p| p.key.chars().count()).max().unwrap_or(4);
                    for p in &profiles {
                        let sel = cfg.profile.as_deref() == Some(p.key.as_str());
                        let star = if sel { ui::green("*") } else { " ".to_string() };
                        let kp = format!("{:<key_w$}", p.key);
                        let key = if sel { ui::bold(&kp) } else { kp };
                        println!("  {star} {key}  \"{}\"  {}", p.name, ui::dim(&p.path.display().to_string()));
                    }
                    println!();
                    println!(
                        "{}",
                        ui::dim(
                            "hint: `bladebro rb profile <key>` selects one · `rb profile auto` resets · an absolute path to a profile dir also works"
                        )
                    );
                }
                Some("auto") => {
                    cfg.profile = None;
                    rb::save_config(&cfg)?;
                    if !json_mode {
                        println!("profile set to auto (most recently used)");
                    }
                }
                Some(key) => {
                    if std::path::Path::new(key).is_absolute() {
                        // An absolute path is resolved lazily (root or
                        // profile-subdir); it only has to exist now.
                        if !std::path::Path::new(key).is_dir() {
                            return Err(crate::error::BladeError::Other(format!(
                                "profile path not found: {key}"
                            )));
                        }
                    } else {
                        // Validate against the selected browser so a typo
                        // fails here, not inside a later launch.
                        let (spec, _) = rb::resolve_selection(&rb::Config {
                            profile: None,
                            ..cfg.clone()
                        })?;
                        let profiles = rb::list_profiles(&spec.profile_root);
                        if !profiles.iter().any(|p| p.key == *key) {
                            let avail: Vec<&str> =
                                profiles.iter().map(|p| p.key.as_str()).collect();
                            return Err(crate::error::BladeError::Other(format!(
                                "profile `{key}` not found in {}. Available: {}",
                                spec.profile_root.display(),
                                if avail.is_empty() { "(none)".to_string() } else { avail.join(", ") }
                            )));
                        }
                    }
                    cfg.profile = Some(key.to_string());
                    rb::save_config(&cfg)?;
                    if !json_mode {
                        println!("profile set to `{key}`");
                    }
                }
            }
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "profile": cfg.profile}));
            }
            Ok(())
        }

        "refresh" => {
            let (spec, profile) = rb::resolve_selection(&cfg)?;
            if let Some(owner) = rb::profile_in_use(&profile.root) {
                eprintln!(
                    "{} note: your browser ({owner}) is open — the copy may miss the last writes \
                     (the source is never touched).",
                    ui::err::dim("[realbrowser]")
                );
            }
            // A live clone session syncs back over the template on exit —
            // that would clobber the fresh import. Stop the daemon (failing
            // if it does not stop) and refuse while any other session runs.
            stop_daemon_for_clone_change().await?;
            refuse_under_live_clone_sessions(&spec.id, "refresh")?;
            let stats = rb::ensure_import(&spec, &profile)?;
            if json_mode {
                println!("{}", serde_json::json!({
                    "ok": true,
                    "refreshed_from": profile.path.display().to_string(),
                    "files": stats.files,
                    "bytes": stats.bytes,
                    "ms": stats.ms,
                    "copied_cookies": stats.copied_cookies,
                    "failed_critical": stats.failed_critical,
                }));
            } else {
                println!(
                    "clone refreshed from {} ({} files, {} in {}ms)",
                    ui::dim(&profile.path.display().to_string()),
                    stats.files,
                    rb::human_bytes(stats.bytes),
                    stats.ms
                );
                if !stats.failed_critical.is_empty() {
                    println!(
                        "{} locked profile file(s) skipped: {} — close {} and run `bladebro rb refresh` again",
                        ui::amber("warning:"),
                        stats.failed_critical.join(", "),
                        spec.name
                    );
                }
            }
            Ok(())
        }

        "forget" => {
            let id = match cfg.browser.clone() {
                Some(id) => id,
                None => match rb::resolve_selection(&cfg) {
                    Ok((s, _)) => s.id,
                    Err(e) => match rb::sole_root_id() {
                        Some(only) => {
                            eprintln!(
                                "[realbrowser] {e} — wiping `{only}` (the only imported copy found)"
                            );
                            only
                        }
                        None => return Err(e),
                    },
                },
            };
            // A live clone session's browser runs inside this root — take the
            // daemon down (failing if it does not stop) and refuse while any
            // other session still runs one.
            stop_daemon_for_clone_change().await?;
            refuse_under_live_clone_sessions(&id, "forget")?;
            let removed = rb::forget(&id)?;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "removed": removed}));
            } else if removed {
                println!("clone for `{id}` wiped (template + session dirs)");
            } else {
                println!("nothing to wipe for `{id}`");
            }
            Ok(())
        }

        "pause" => {
            rb::set_paused(true)?;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "paused": true}));
            } else {
                println!(
                    "{} — input, navigation, history, downloads, collecting and tab \
                     operations refuse; the browser is yours (reads, waits and eval stay \
                     available; `rb resume` hands it back)",
                    ui::yellow("paused")
                );
            }
            Ok(())
        }

        "resume" => {
            rb::set_paused(false)?;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "paused": false}));
            } else {
                println!("{} — the agent has the wheel again", ui::green("resumed"));
            }
            Ok(())
        }

        "audio" => run_audio(rest, json_mode).await,

        other => Err(crate::error::BladeError::Usage(format!(
            "unknown rb subcommand `{other}` — use on|off|status|mode|use|profile|visible|idle-hum|idle-shutdown|audio|refresh|forget|pause|resume"
        ))),
    }
}

/// `bladebro audio [on|off|status]` — control browser audio output across both
/// the default agent browser and owned real-browser launches. Muted by default
/// so media players (YouTube, video ads, audio streams) never play unexpected
/// sound back to the user unless explicitly opted in.
pub(super) async fn run_audio(args: &[String], json_mode: bool) -> Result<()> {
    use crate::ui;

    let raw = args.first().map(|s| s.as_str()).unwrap_or("status");
    let sub = match raw {
        "on" | "true" | "1" | "yes" | "enable" | "unmute" => "on",
        "off" | "false" | "0" | "no" | "disable" | "mute" => "off",
        "status" => "status",
        other => {
            return Err(crate::error::BladeError::Usage(format!(
                "unknown audio option `{other}` — use `bladebro audio [on|off|status]`"
            )))
        }
    };

    let env_raw = std::env::var("BLADE_AUDIO").ok();
    match sub {
        "status" => {
            let persisted = crate::browser::audio_persisted();
            let effective = crate::browser::audio_enabled();
            if json_mode {
                let v = serde_json::json!({
                    "ok": true,
                    "audio": effective,
                    "muted": !effective,
                    "persisted": persisted,
                    "env_override": env_raw,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            let badge = if effective {
                ("AUDIBLE", ui::Tone::Warn)
            } else {
                ("MUTED", ui::Tone::Success)
            };
            let mut rows = vec![
                (
                    "output",
                    if effective {
                        "ON — browser audio plays through your speakers/headphones".into()
                    } else {
                        "OFF (default) — browser audio is muted (`--mute-audio`)".into()
                    },
                ),
                (
                    "scope",
                    "applies to the agent browser and owned real-browser launches (never an externally attached browser)".into(),
                ),
            ];
            if let Some(ev) = env_raw.as_deref().filter(|s| !s.is_empty()) {
                rows.push(("env", format!("BLADE_AUDIO={ev} (overrides saved setting)")));
            }
            ui::dossier_card(
                "Browser Audio",
                badge,
                &rows,
                Some("`bladebro audio on` enables sound · `bladebro audio off` mutes again"),
            );
            Ok(())
        }
        "on" | "off" => {
            let on = sub == "on";
            crate::browser::set_audio_enabled(on)?;
            restart_daemon_for_lane().await;
            let effective = crate::browser::audio_enabled();
            if json_mode {
                let v = serde_json::json!({
                    "ok": true,
                    "audio": effective,
                    "muted": !effective,
                    "persisted": on,
                    "env_override": env_raw,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            if on {
                println!(
                    "{} {} — browser sound is now audible (running MCP sessions pick this up on the next call; `bladebro audio off` mutes again)",
                    ui::bold("Browser audio:"),
                    ui::amber("ON")
                );
            } else {
                println!(
                    "{} {} — browser sound is muted on both agent and owned real-browser launches",
                    ui::bold("Browser audio:"),
                    ui::green("MUTED")
                );
            }
            if let Some(ev) = env_raw.as_deref().filter(|s| !s.is_empty()) {
                println!(
                    "  {}",
                    ui::dim(&format!(
                        "note: BLADE_AUDIO={ev} is set in this shell and overrides the saved setting"
                    ))
                );
            }
            Ok(())
        }
        _ => unreachable!(),
    }
}
