//! `bladebro rb` — the real-browser lane command surface: switch, inspect,
//! pick browser/profile, pause/resume, refresh/forget the clone.

use crate::error::Result;

use super::daemon::restart_daemon_for_lane;

/// One aligned `label  value` row for the `rb` blocks: labels dim, column
/// width 9 (the longest label, `mechanism`).
fn rb_row(label: &str, value: &str) -> String {
    crate::ui::kv(label, value, 9)
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
                let v = serde_json::json!({
                    "ok": true,
                    "enabled": cfg.enabled,
                    "mode": cfg.mode.as_str(),
                    "effective_mode": sel.as_ref().ok().map(|(_, p)| rb::effective_mode(&cfg, &p.root).as_str()),
                    "browser": cfg.browser.clone(),
                    "profile": cfg.profile.clone(),
                    "binary": cfg.binary.clone(),
                    "attach_port": sel.as_ref().ok().and_then(|(_, p)| rb::devtools_port(&p.root)),
                    "visible": cfg.visible,
                    "idle_shutdown": cfg.idle_shutdown,
                    "idle_hum": cfg.idle_hum,
                    "paused": paused,
                    "browsers_found": rb::discover().iter().map(|b| b.id.clone()).collect::<Vec<_>>(),
                    "template": sel.as_ref().ok().map(|(s, _)| rb::has_template(&s.id)),
                    "source": sel.as_ref().ok().and_then(|(s, _)| rb::template_source(&s.id)),
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            let state = if cfg.enabled {
                ui::bold(&ui::green("ON"))
            } else {
                ui::dim("off")
            };
            println!("{} {state}", ui::bold("Real-browser lane:"));
            println!();
            match sel {
                Ok((spec, profile)) => {
                    let auto = if cfg.browser.is_none() { " · auto (most recently used)" } else { "" };
                    println!(
                        "{}",
                        rb_row("browser", &format!("{} ({}){auto}", spec.name, spec.brand.as_str()))
                    );
                    let bin = if spec.binary.as_os_str().is_empty() {
                        ui::yellow("none found — install it or set `rb use --binary <path>`")
                    } else {
                        let p = spec.binary.display().to_string();
                        if cfg.binary.is_some() {
                            format!("{p}  {}", ui::dim("(override)"))
                        } else {
                            p
                        }
                    };
                    println!("{}", rb_row("binary", &bin));
                    println!(
                        "{}",
                        rb_row(
                            "profile",
                            &format!("\"{}\" · {}", profile.name, ui::dim(&profile.path.display().to_string()))
                        )
                    );
                    println!(
                        "{}",
                        rb_row(
                            "mode",
                            &format!(
                                "{} → {} (now)",
                                cfg.mode.as_str(),
                                rb::effective_mode(&cfg, &profile.root).as_str()
                            )
                        )
                    );
                    if let Some(port) = rb::devtools_port(&profile.root) {
                        println!(
                            "{}",
                            rb_row("attach", &format!("live debug endpoint on 127.0.0.1:{port} — auto would attach"))
                        );
                    }
                    match rb::template_stats(&spec.id) {
                        Some((files, bytes)) => {
                            println!("{}", rb_row("clone", &format!("{files} files ({})", rb::human_bytes(bytes))));
                            if let Some(src) = rb::template_source(&spec.id) {
                                println!("{}", rb_row("source", &ui::dim(&src)));
                            }
                        }
                        None => println!(
                            "{}",
                            rb_row("clone", &ui::dim("not imported yet (first launch imports)"))
                        ),
                    }
                }
                Err(e) => println!("{}", rb_row("selection", &ui::red(&e.to_string()))),
            }
            if paused {
                println!(
                    "{}",
                    rb_row("paused", &ui::yellow("yes — input, navigation, downloads, collecting and tab ops refuse until `rb resume`"))
                );
            }
            println!();
            println!(
                "{}",
                ui::dim("hint: `bladebro rb on|off` switches the lane · `bladebro help rb` for the full story")
            );
            Ok(())
        }

        "on" => {
            if cfg.enabled {
                // Already on: still self-heal a missing clone (e.g. after
                // `rb forget`) so the next launch never imports mid-run.
                if let Ok((spec, profile)) = rb::resolve_selection(&cfg) {
                    if rb::effective_mode(&cfg, &profile.root) == rb::Mode::Clone
                        && !rb::has_template(&spec.id)
                    {
                        rb::ensure_import(&spec, &profile)?;
                    }
                }
                if json_mode {
                    println!("{}", serde_json::json!({"ok": true, "enabled": true, "note": "already on"}));
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
            // Import now (clone lane) so problems surface HERE, not inside a
            // later agent run.
            if mode == rb::Mode::Clone && !rb::has_template(&spec.id) {
                rb::ensure_import(&spec, &profile)?;
            }
            cfg.enabled = true;
            rb::save_config(&cfg)?;
            restart_daemon_for_lane().await;
            if json_mode {
                let v = serde_json::json!({
                    "ok": true, "enabled": true, "mode": mode.as_str(),
                    "browser": spec.id, "profile": profile.key,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return Ok(());
            }
            println!(
                "{} {} — running surfaces switch at their next action",
                ui::bold("Real-browser lane:"),
                ui::bold(&ui::green("ON"))
            );
            println!();
            println!("{}", rb_row("browser", &format!("{} ({})", spec.name, spec.brand.as_str())));
            println!(
                "{}",
                rb_row("profile", &format!("\"{}\" · {}", profile.name, ui::dim(&profile.path.display().to_string())))
            );
            println!("{}", rb_row("mechanism", mode.as_str()));
            println!("{}", rb_row("lane", "zero page patches — your real environment IS the stealth"));
            println!();
            println!(
                "  {} the agent browses AS YOU — anything it does is attributable to your",
                ui::yellow("⚠")
            );
            println!("    identity (accounts, sessions, reputation).");
            println!();
            println!(
                "  {} while a session runs, browser control is served on a loopback debug",
                ui::yellow("⚠")
            );
            println!("    endpoint any local user of this machine can reach — avoid shared/");
            println!("    multi-user hosts (headless sessions idle-close automatically).");
            println!();
            println!("{}", rb_row("revert", "`bladebro rb off` · wipe the imported copy: `rb forget`"));
            println!("{}", rb_row("control", "`rb pause` / `rb resume` hand the browser to you"));
            println!("{}", rb_row("note", "running surfaces (daemon, MCP) switch at their next action — an owned browser relaunches; an attached browser is left running"));
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
                    ui::dim("[realbrowser]")
                );
            }
            // A live clone session syncs back over the template on exit —
            // that would clobber the fresh import.
            restart_daemon_for_lane().await;
            rb::ensure_import(&spec, &profile)?;
            if json_mode {
                println!("{}", serde_json::json!({"ok": true, "refreshed_from": profile.path.display().to_string()}));
            } else {
                println!("clone refreshed from {}", ui::dim(&profile.path.display().to_string()));
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
            // A live clone session recreates the template on its exit path —
            // take the daemon down before removing the root under it.
            restart_daemon_for_lane().await;
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

        other => Err(crate::error::BladeError::Usage(format!(
            "unknown rb subcommand `{other}` — use on|off|status|mode|use|profile|visible|idle-hum|idle-shutdown|refresh|forget|pause|resume"
        ))),
    }
}
