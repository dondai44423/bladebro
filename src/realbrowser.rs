//! Real-browser lane (`bladebro rb`).
//!
//! Bladebro's default lane owns an isolated Chromium + seasoned profile on a
//! virtual display, and manufactures coherence with a page-injection layer.
//! The real-browser lane is the opposite trade, and for protected sites the
//! stronger one: the agent drives the user's OWN Chromium-family browser —
//! their binary, their real profile data, the real display — and the
//! injection layer is switched OFF entirely. Truth has no lies to catch:
//! every mask this crate maintains is a measurable risk (the S10 `toString`
//! episode is the receipt), so on this lane the correct amount of page
//! patching is zero.
//!
//! What still runs on the real lane: everything driver-side — perception,
//! the Live Page Model, refs, adapters, token-efficiency compression,
//! interception, the biometrics/hum behavior layer. None of it is
//! page-visible.
//!
//! Three mechanisms, chosen by [`Mode`]:
//! - **Clone** (default): the user's profile is imported once into a
//!   blade-owned template and per-process session dirs (the exact machinery
//!   the agent lane already uses for seasoning), then launched with the
//!   user's real browser binary. Works while their browser is running,
//!   works for Google-Chrome-branded builds (whose 136+ CDP hardening
//!   refuses remote debugging on the *default* profile dir — a non-default
//!   clone dir sidesteps it), and never touches their live profile.
//! - **Profile**: launch their binary directly on their real profile dir.
//!   Full fidelity, writes persist into their profile — requires their
//!   browser to be closed, and branded Chrome requires a non-default dir.
//! - **Attach**: drive an already-running browser (classic pre-armed
//!   `--remote-debugging-port`, or Chrome >=144's official
//!   `chrome://inspect#remote-debugging` approval flow). No ownership: no
//!   launch, no shutdown.
//!
//! Ground truth this design obeys (verified on the dev machine, Chrome 151):
//! the `default_user_data_dir` CDP refusal is compiled in only for
//! `GOOGLE_CHROME_BRANDING` (chromium source), so plain Chromium accepts a
//! debug port on any dir; a WS-attached browser reports `webdriver=false`
//! natively; `--remote-debugging-pipe` reports `true` (so the real lane uses
//! WS — masking would be a lie, and a lie is exactly what this lane exists
//! to delete).
//!
//! Module map: `config` (realbrowser.json + [`Mode`]), `browsers` (per-OS
//! discovery), `profiles` (enumeration + live probes), `template` (clone
//! import/stats/forget), `select` (which browser/profile the lane uses +
//! drift), `controls` (pause + idle policy). The lane switch and its
//! fingerprint live in this file.

use std::sync::atomic::{AtomicBool, Ordering};

mod browsers;
mod config;
mod controls;
mod profiles;
mod select;
mod template;

pub use self::browsers::{
    brand_from_version, discover, find_browser, requires_non_default_dir, Brand, BrowserSpec,
};
pub use self::config::{
    config, config_from, config_path, save_config, save_config_to, Config, Mode,
};
pub use self::controls::{
    hum_enabled, input_paused, pause_path, paused_error, set_paused, should_idle_shutdown,
};
pub use self::profiles::{
    devtools_port, list_profiles, parse_devtools_active_port, parse_singleton_owner,
    profile_in_use, profile_owner_pid, same_dir, ProfileInfo,
};
pub use self::select::{
    effective_mode, ensure_import, resolve_selection, session_drifted, sole_root_id,
    validate_binary_override,
};
pub use self::template::{
    forget, has_template, human_bytes, import_template, root_for, source_meta_path, template_dir,
    template_source, template_stats, ImportStats,
};

// ── Lane switch ─────────────────────────────────────────────────────────

static REAL_LANE: AtomicBool = AtomicBool::new(false);

/// Force the lane for this process (tests, harnesses).
pub fn set_real_lane(on: bool) {
    REAL_LANE.store(on, Ordering::Relaxed);
}

/// True when this process must drive the user's real browser instead of
/// launching the isolated agent browser.
pub fn real_lane() -> bool {
    REAL_LANE.load(Ordering::Relaxed)
}

/// Initialise the lane for this process: `BLADE_LANE=real|agent` overrides,
/// otherwise the persisted config decides. Called once at process start.
pub fn init_lane() {
    set_real_lane(lane_from(
        std::env::var("BLADE_LANE").ok().as_deref(),
        config().enabled,
    ));
}

/// Effective lane: the `BLADE_LANE=real|agent` env override wins, else the
/// persisted config. Pure — the decision is unit-testable, and every reader
/// of the lane (init, refresh, fingerprint) goes through it.
pub fn lane_from(env_override: Option<&str>, cfg_enabled: bool) -> bool {
    match env_override {
        Some("real") => true,
        Some("agent") => false,
        _ => cfg_enabled,
    }
}

/// Re-read the effective lane and switch this process when it moved.
/// Returns true when the lane changed — the browser that is running belongs
/// to the OLD lane, so the caller must relaunch it. Without that, `rb on`
/// / `rb off` is silently ignored by a long-lived MCP or daemon session
/// until its browser happens to die (the observed "rb on does nothing").
pub fn refresh_lane() -> bool {
    let want = lane_from(
        std::env::var("BLADE_LANE").ok().as_deref(),
        config().enabled,
    );
    let had = real_lane();
    if want != had {
        set_real_lane(want);
        true
    } else {
        false
    }
}

/// Fingerprint of everything that decides how the next launch behaves.
/// Long-lived surfaces snapshot it after each launch and compare it on every
/// call: a mismatch means the running browser no longer matches the config
/// (`rb on|off`, `rb mode|use|profile|visible`) and must be relaunched.
/// On the agent lane the real-browser fields cannot affect the browser, so
/// the fingerprint is constant there — an agent session never pays a
/// relaunch for config that does not concern it.
pub fn launch_fingerprint() -> u64 {
    launch_fingerprint_from(std::env::var("BLADE_LANE").ok().as_deref(), &config())
}

/// Testable core of [`launch_fingerprint`] (no env/disk reads).
pub fn launch_fingerprint_from(env_override: Option<&str>, cfg: &Config) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let lane = lane_from(env_override, cfg.enabled);
    lane.hash(&mut h);
    if lane {
        cfg.mode.as_str().hash(&mut h);
        cfg.browser.hash(&mut h);
        cfg.profile.hash(&mut h);
        cfg.binary.hash(&mut h);
        // `visible` shapes launches we OWN. An attach session never launches
        // (the browser belongs to the user), so toggling it must not reset an
        // attached page; every other mode still gets the relaunch.
        if cfg.mode != Mode::Attach {
            cfg.visible.hash(&mut h);
        }
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::select::pick_recency_index;
    use super::*;

    #[test]
    fn brand_from_version_parses_every_family() {
        assert_eq!(
            brand_from_version("Google Chrome 151.0.7922.108"),
            Some(Brand::Chrome)
        );
        assert_eq!(
            brand_from_version("Chromium 151.0.7922.108"),
            Some(Brand::Chromium)
        );
        assert_eq!(
            brand_from_version("Brave Browser 1.79.126"),
            Some(Brand::Brave)
        );
        assert_eq!(
            brand_from_version("Microsoft Edge 151.0.0.0"),
            Some(Brand::Edge)
        );
        assert_eq!(
            brand_from_version("Vivaldi 7.5.3735.58"),
            Some(Brand::Vivaldi)
        );
        assert_eq!(brand_from_version("Opera 118.0.0.0"), Some(Brand::Opera));
        assert_eq!(brand_from_version("Mozilla Firefox 141"), None);
    }

    #[test]
    fn only_branded_chrome_requires_a_non_default_dir() {
        assert!(requires_non_default_dir(Brand::Chrome));
        for b in [
            Brand::Chromium,
            Brand::Brave,
            Brand::Edge,
            Brand::Vivaldi,
            Brand::Opera,
        ] {
            assert!(!requires_non_default_dir(b), "{b:?} must not be gated");
        }
    }

    #[test]
    fn devtools_active_port_parses_first_line_only() {
        assert_eq!(
            parse_devtools_active_port("9222\n/devtools/browser/abc\n"),
            Some(9222)
        );
        assert_eq!(parse_devtools_active_port("0\n"), None);
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("not-a-port\n"), None);
    }

    #[test]
    fn singleton_owner_parses_hostname_pid() {
        assert_eq!(parse_singleton_owner("myhost-4242"), Some(4242));
        assert_eq!(parse_singleton_owner("host-with-dashes-7"), Some(7));
        assert_eq!(parse_singleton_owner("nopid"), None);
    }

    #[cfg(unix)]
    #[test]
    fn profile_in_use_reads_live_and_stale_singleton_locks() {
        let root = std::env::temp_dir().join(format!("blade-rb-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(profile_in_use(&root), None, "no lock → not in use");

        let live = format!("host-{}", std::process::id());
        std::os::unix::fs::symlink(&live, root.join("SingletonLock")).unwrap();
        assert_eq!(
            profile_in_use(&root),
            Some(format!("pid {}", std::process::id())),
            "a lock held by a live pid reads as in use"
        );

        std::fs::remove_file(root.join("SingletonLock")).unwrap();
        // Beyond any pid_max: kill(pid, 0) errors → not alive → stale.
        std::os::unix::fs::symlink("host-99999999", root.join("SingletonLock")).unwrap();
        assert_eq!(profile_in_use(&root), None, "a dead pid is a stale lock");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lane_decision_prefers_env_over_config() {
        assert!(
            lane_from(Some("real"), false),
            "env real beats a disabled config"
        );
        assert!(
            !lane_from(Some("agent"), true),
            "env agent beats an enabled config"
        );
        assert!(lane_from(None, true));
        assert!(!lane_from(None, false));
        // Anything that is not exactly real|agent falls through to the
        // config — a typo must never silently flip the lane.
        assert!(
            !lane_from(Some("Real"), false),
            "a typo does not force the real lane"
        );
        assert!(
            lane_from(Some("agentx"), true),
            "a typo does not force the agent lane either"
        );
    }

    #[test]
    fn fingerprint_tracks_launch_inputs_only_on_the_real_lane() {
        let base = Config {
            enabled: true,
            ..Default::default()
        };
        let fp = launch_fingerprint_from(None, &base);
        assert_eq!(
            fp,
            launch_fingerprint_from(None, &base),
            "stable across reads"
        );

        let mut other = base.clone();
        other.visible = false;
        assert_ne!(
            fp,
            launch_fingerprint_from(None, &other),
            "visible is a launch input"
        );
        let mut other = base.clone();
        other.mode = Mode::Profile;
        assert_ne!(
            fp,
            launch_fingerprint_from(None, &other),
            "mode is a launch input"
        );
        let mut other = base.clone();
        other.profile = Some("Work".into());
        assert_ne!(
            fp,
            launch_fingerprint_from(None, &other),
            "profile is a launch input"
        );

        // The lane itself is part of the fingerprint: on→off must drift.
        let off = Config {
            enabled: false,
            ..base.clone()
        };
        assert_ne!(fp, launch_fingerprint_from(None, &off));

        // Agent lane (env override): the real-lane fields cannot affect an
        // agent browser, so the fingerprint stays put — no pointless relaunch.
        let a1 = Config {
            enabled: true,
            mode: Mode::Clone,
            visible: true,
            profile: Some("Work".into()),
            ..Default::default()
        };
        let a2 = Config {
            enabled: true,
            mode: Mode::Profile,
            visible: false,
            profile: None,
            ..Default::default()
        };
        assert_eq!(
            launch_fingerprint_from(Some("agent"), &a1),
            launch_fingerprint_from(Some("agent"), &a2)
        );
        // ...and the agent lane never equals the real lane.
        assert_ne!(
            launch_fingerprint_from(Some("agent"), &a1),
            launch_fingerprint_from(None, &a1)
        );

        // `visible` shapes launches we OWN only: under Attach (never a
        // launch) toggling it must NOT drift — an attached page must not be
        // reset for a setting that cannot affect it...
        let at1 = Config {
            enabled: true,
            mode: Mode::Attach,
            visible: true,
            ..Default::default()
        };
        let at2 = Config {
            enabled: true,
            mode: Mode::Attach,
            visible: false,
            ..Default::default()
        };
        assert_eq!(
            launch_fingerprint_from(None, &at1),
            launch_fingerprint_from(None, &at2),
            "visible cannot affect an attach session"
        );
        // ...while every owned mode still gets the relaunch on a toggle.
        let cl1 = Config {
            enabled: true,
            mode: Mode::Clone,
            visible: true,
            ..Default::default()
        };
        let cl2 = Config {
            enabled: true,
            mode: Mode::Clone,
            visible: false,
            ..Default::default()
        };
        assert_ne!(
            launch_fingerprint_from(None, &cl1),
            launch_fingerprint_from(None, &cl2)
        );
    }

    #[test]
    fn session_drift_requires_live_state_and_a_change() {
        // A live session (owned browser or attached page) drifts on a lane
        // switch or a launch-input change...
        assert!(session_drifted(true, true, 7, 7));
        assert!(session_drifted(true, false, 7, 8));
        // ...a settled live session must not (no pointless relaunch)...
        assert!(!session_drifted(true, false, 7, 7));
        // ...and a session with nothing live never drifts.
        assert!(!session_drifted(false, true, 7, 8));
    }

    #[test]
    fn recency_tie_keeps_first_browser() {
        // All-equal recency (a fresh machine) keeps the FIRST table entry
        // (chromium) — `max_by_key` returned the LAST (opera).
        assert_eq!(pick_recency_index(&[0, 0, 0, 0, 0]), Some(0));
        assert_eq!(pick_recency_index(&[5, 9, 9, 2]), Some(1));
        assert_eq!(pick_recency_index(&[1, 2, 3]), Some(2));
        assert_eq!(pick_recency_index(&[]), None);
    }

    #[test]
    fn binary_override_must_exist_and_be_executable() {
        let dir = std::env::temp_dir().join(format!("blade-rb-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("chrome");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(validate_binary_override(&exe.display().to_string()).is_ok());
        assert!(validate_binary_override("/nonexistent/bladebro-browser").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let noexec = dir.join("noexec");
            std::fs::write(&noexec, b"x").unwrap();
            std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                validate_binary_override(&noexec.display().to_string()).is_err(),
                "a non-executable file must be refused"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_override_stores_absolute_paths() {
        // A relative override must be stored resolved: a later daemon/MCP
        // process runs with a different cwd and the relative path would
        // silently break there. (Unit-test cwd is the package root.)
        let rel = std::path::PathBuf::from("target/rb-bin-rel-test");
        let _ = std::fs::remove_dir_all(&rel);
        std::fs::create_dir_all(&rel).unwrap();
        let exe = rel.join("chrome");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let got =
            validate_binary_override("target/rb-bin-rel-test/chrome").expect("relative override");
        assert!(
            got.is_absolute(),
            "stored path must be absolute, got {}",
            got.display()
        );
        assert!(got.exists());
        let _ = std::fs::remove_dir_all(&rel);
    }

    #[test]
    fn config_round_trips_and_survives_corruption() {
        let dir = std::env::temp_dir().join(format!("blade-rb-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("realbrowser.json");

        let mut cfg = Config::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.mode, Mode::Auto);
        assert!(cfg.visible, "visible must default on");
        assert!(!cfg.idle_shutdown, "idle shutdown must default off");
        assert!(cfg.binary.is_none(), "no binary override by default");
        cfg.enabled = true;
        cfg.mode = Mode::Clone;
        cfg.browser = Some("brave".into());
        save_config_to(&path, &cfg).expect("save");
        let back = config_from(&path);
        assert!(
            back.enabled && back.mode == Mode::Clone && back.browser.as_deref() == Some("brave")
        );

        std::fs::write(&path, b"{ not json").unwrap();
        let corrupt = config_from(&path);
        assert!(
            !corrupt.enabled,
            "corruption reads as default-off, never a crash"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_profiles_reads_local_state_and_scans_dirs() {
        let root = std::env::temp_dir().join(format!("blade-rb-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Default")).unwrap();
        std::fs::create_dir_all(root.join("Profile 1")).unwrap();
        // Chrome's internal dirs carry a Preferences file but are not user
        // profiles — listing them would let the default pick import an
        // empty, login-less profile.
        std::fs::create_dir_all(root.join("System Profile")).unwrap();
        std::fs::create_dir_all(root.join("Guest Profile")).unwrap();
        std::fs::write(root.join("Default/Preferences"), "{}").unwrap();
        std::fs::write(root.join("Profile 1/Preferences"), "{}").unwrap();
        std::fs::write(root.join("System Profile/Preferences"), "{}").unwrap();
        std::fs::write(root.join("Guest Profile/Preferences"), "{}").unwrap();
        std::fs::write(
            root.join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"Main"},"Profile 1":{"name":"Work"},"System Profile":{"name":"System Profile"},"Guest Profile":{"name":"Guest Profile"}}}}"#,
        )
        .unwrap();

        let got = list_profiles(&root);
        let keys: Vec<(&str, &str)> = got
            .iter()
            .map(|p| (p.key.as_str(), p.name.as_str()))
            .collect();
        assert!(keys.contains(&("Default", "Main")), "got {keys:?}");
        assert!(keys.contains(&("Profile 1", "Work")), "got {keys:?}");
        assert!(
            keys.iter()
                .all(|(k, _)| *k != "System Profile" && *k != "Guest Profile"),
            "internal profiles must never be listed: got {keys:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pause_marker_round_trips_via_env_home() {
        // blade_dir() is env-driven; point it at a scratch home so the test
        // never touches a real install. (set_var is process-global — same
        // pattern the platform tests use, and this test owns the name.)
        let home = std::env::temp_dir().join(format!("blade-rb-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let key = "BLADE_HOME";
        let prev = std::env::var(key).ok();
        std::env::set_var(key, &home);
        assert!(!input_paused());
        // The agent lane hums by default...
        assert!(hum_enabled(), "agent lane hums when not paused");
        set_paused(true).expect("pause");
        assert!(input_paused());
        // ...and the pause marker silences the hum on EVERY lane, not just
        // the real one (the pre-fix order let agent sessions keep humming).
        assert!(
            !hum_enabled(),
            "pause must silence the hum on the agent lane"
        );
        set_paused(false).expect("resume");
        assert!(!input_paused());
        assert!(hum_enabled(), "resume restores the hum");
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
