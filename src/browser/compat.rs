//! Narrow host compatibility policy; ordinary profiles remain the default.

use super::*;
use crate::cdp::CdpClient;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

static INCOGNITO: AtomicBool = AtomicBool::new(false);

pub(crate) fn launched_incognito() -> bool {
    INCOGNITO.load(Ordering::Relaxed)
}

pub(super) fn clear_incognito() {
    INCOGNITO.store(false, Ordering::Relaxed);
}

fn release_value<'a>(release: &'a str, key: &str) -> Option<&'a str> {
    release.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches(['\'', '"']))
    })
}

fn affected_pi(os: &str, arch: &str, release: &str, model: &str) -> bool {
    os == "linux"
        && arch == "aarch64"
        && matches!(release_value(release, "ID"), Some("debian" | "raspbian"))
        && release_value(release, "VERSION_CODENAME") == Some("trixie")
        && model.trim_end_matches('\0').starts_with("Raspberry Pi ")
}

/// `0` disables automatic compatibility, `1` explicitly requests it. Raw
/// BLADE_CHROME_FLAGS remain authoritative, including an explicit incognito.
fn decide(setting: &str, affected: bool, custom_profile: bool, extra: &[String]) -> Result<bool> {
    let automatic = match setting.trim() {
        "" | "auto" => affected && !custom_profile,
        "0" => false,
        "1" => true,
        _ => {
            return Err(BladeError::Other(
                "BLADE_INCOGNITO must be auto, 0 or 1".into(),
            ))
        }
    };
    Ok(automatic || extra.iter().any(|flag| flag == "--incognito"))
}

pub(crate) fn incognito_policy() -> Result<bool> {
    let extra = extra_flags();
    let affected = if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let model = std::fs::read_to_string("/proc/device-tree/model")
            .or_else(|_| std::fs::read_to_string("/sys/firmware/devicetree/base/model"))
            .unwrap_or_default();
        affected_pi(
            std::env::consts::OS,
            std::env::consts::ARCH,
            &release,
            &model,
        )
    } else {
        false
    };
    decide(
        &std::env::var("BLADE_INCOGNITO").unwrap_or_default(),
        affected,
        std::env::var_os("BLADE_PROFILE_DIR").is_some_and(|p| !p.is_empty()),
        &extra,
    )
}

fn extra_flags() -> Vec<String> {
    std::env::var("BLADE_CHROME_FLAGS")
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

pub(super) fn agent_extra_flags() -> Result<Vec<String>> {
    let mut extra = extra_flags();
    let incognito = incognito_policy()?;
    INCOGNITO.store(incognito, Ordering::Relaxed);
    if incognito {
        if !extra.iter().any(|flag| flag == "--incognito") {
            extra.push("--incognito".into());
        }
        eprintln!("[bladebro] incognito compatibility: cookies and named sessions remain supported; history/cache and automatic localStorage persistence are unavailable. BLADE_INCOGNITO=0 disables automatic Raspberry Pi OS trixie compatibility (restart the client/daemon).");
    }
    Ok(extra)
}

/// The primary --incognito window is not an addressable CDP browser context:
/// new tabs silently land in the regular profile, and Storage.getCookies
/// reads its empty jar. Use ONE managed off-the-record context for all owned
/// tabs, so the workaround survives new tabs and login readback remains true.
pub(super) async fn prepare_incognito(client: &CdpClient) -> Result<()> {
    if !launched_incognito() {
        return Ok(());
    }
    let old = client.send("Target.getTargets", None).await?;
    let context = client
        .send(
            "Target.createBrowserContext",
            Some(json!({"disposeOnDetach":false})),
        )
        .await?;
    let id = context["browserContextId"].as_str().ok_or_else(|| {
        BladeError::Other("incognito compatibility: Chrome returned no browserContextId".into())
    })?;
    client
        .send(
            "Target.createTarget",
            Some(json!({"url":"about:blank","browserContextId":id})),
        )
        .await?;
    if let Some(targets) = old["targetInfos"].as_array() {
        for target in targets.iter().filter(|t| t["type"] == "page") {
            if let Some(id) = target["targetId"].as_str() {
                client
                    .send("Target.closeTarget", Some(json!({"targetId":id})))
                    .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const DEBIAN: &str = "ID=debian\nVERSION_CODENAME=trixie\n";

    #[test]
    fn compatibility_is_confined_to_the_reported_host_family() {
        assert!(affected_pi(
            "linux",
            "aarch64",
            DEBIAN,
            "Raspberry Pi 5 Model B Rev 1.0\0"
        ));
        assert!(affected_pi(
            "linux",
            "aarch64",
            "ID='raspbian'\nVERSION_CODENAME=\"trixie\"",
            "Raspberry Pi 4 Model B"
        ));
        for (os, arch, release, model) in [
            ("linux", "x86_64", DEBIAN, "Raspberry Pi 5"),
            ("android", "aarch64", DEBIAN, "Raspberry Pi 5"),
            ("linux", "aarch64", DEBIAN, "Generic ARM server"),
            (
                "linux",
                "aarch64",
                "ID=ubuntu\nVERSION_CODENAME=trixie",
                "Raspberry Pi 5",
            ),
            (
                "linux",
                "aarch64",
                "ID=debian\nVERSION_CODENAME=bookworm",
                "Raspberry Pi 5",
            ),
            ("linux", "aarch64", "", "Raspberry Pi 5"),
        ] {
            assert!(!affected_pi(os, arch, release, model));
        }
    }

    #[test]
    fn explicit_policy_and_borrowed_profiles_are_preserved() {
        assert!(decide("auto", true, false, &[]).unwrap());
        assert!(!decide("auto", true, true, &[]).unwrap());
        assert!(!decide("auto", false, false, &[]).unwrap());
        assert!(!decide("0", true, false, &[]).unwrap());
        assert!(decide("1", false, true, &[]).unwrap());
        assert!(decide("0", true, false, &["--incognito".into()]).unwrap());
        assert!(decide("typo", false, false, &[]).is_err());
    }
}
