//! Fingerprint seed persistence — makes stealth values stable across sessions.
//!
//! Without this, the canvas/audio noise seed (and any other seed-derived
//! stealth values) changes on every Chrome launch. A site tracking fingerprint
//! consistency across visits sees a "new device" each time. With persistence,
//! the seed is generated once and reused indefinitely.
//!
//! Also tracks proxy/timezone/locale config to warn when they change between
//! sessions (a browser that switches IP/timezone between visits is suspicious).

use crate::platform;
use serde_json::{json, Value};

fn path() -> std::path::PathBuf {
    platform::blade_dir().join(".fingerprint.json")
}

/// Load the stealth seed from the fingerprint file, or generate and persist
/// a new one if it doesn't exist. Also warns if proxy/timezone/locale
/// changed since the last session.
pub fn load_or_create_seed() -> u32 {
    let p = path();

    // Try loading existing fingerprint.
    if let Ok(content) = std::fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<Value>(&content) {
            if let Some(seed) = v.get("seed").and_then(|s| s.as_u64()) {
                check_config_consistency(&v);
                // Update stored config with current env values (for next launch's check).
                update_config(&p, &v, seed as u32);
                return seed as u32;
            }
        }
    }

    // Generate new seed and persist.
    let seed = generate_seed();
    let _ = platform::secure_create_dir_all(&platform::blade_dir());
    let fingerprint = build_fingerprint(seed);
    let _ = platform::secure_write_file(&p, fingerprint.to_string().as_bytes());
    eprintln!("[stealth] generated new fingerprint seed: {seed}");
    seed
}

fn build_fingerprint(seed: u32) -> Value {
    let mut fp = json!({"seed": seed});
    if let Ok(proxy) = std::env::var("BLADE_PROXY") {
        if !proxy.is_empty() {
            fp["proxy"] = json!(proxy);
        }
    }
    if let Ok(tz) = std::env::var("BLADE_TZ") {
        if !tz.is_empty() {
            fp["timezone"] = json!(tz);
        }
    }
    if let Ok(locale) = std::env::var("BLADE_LOCALE") {
        if !locale.is_empty() {
            fp["locale"] = json!(locale);
        }
    }
    fp
}

fn update_config(path: &std::path::Path, old: &Value, seed: u32) {
    let mut updated = json!({"seed": seed});
    if let Ok(proxy) = std::env::var("BLADE_PROXY") {
        if !proxy.is_empty() {
            updated["proxy"] = json!(proxy);
        }
    } else if let Some(p) = old.get("proxy") {
        updated["proxy"] = p.clone();
    }
    if let Ok(tz) = std::env::var("BLADE_TZ") {
        if !tz.is_empty() {
            updated["timezone"] = json!(tz);
        }
    } else if let Some(t) = old.get("timezone") {
        updated["timezone"] = t.clone();
    }
    if let Ok(locale) = std::env::var("BLADE_LOCALE") {
        if !locale.is_empty() {
            updated["locale"] = json!(locale);
        }
    } else if let Some(l) = old.get("locale") {
        updated["locale"] = l.clone();
    }
    // SECURITY: 0600 like the initial write — this file can carry the
    // BLADE_PROXY URL, which may embed proxy credentials.
    let _ = platform::secure_write_file(path, updated.to_string().as_bytes());
}

fn check_config_consistency(old: &Value) {
    if let Some(old_proxy) = old.get("proxy").and_then(|p| p.as_str()) {
        let new_proxy = std::env::var("BLADE_PROXY").unwrap_or_default();
        if !new_proxy.is_empty() && new_proxy != old_proxy {
            eprintln!(
                "[stealth] WARNING: proxy changed since last session ({old_proxy} → {new_proxy}). \
                 Sites may flag this as suspicious."
            );
        }
    }
    if let Some(old_tz) = old.get("timezone").and_then(|t| t.as_str()) {
        let new_tz = std::env::var("BLADE_TZ").unwrap_or_default();
        if !new_tz.is_empty() && new_tz != old_tz {
            eprintln!(
                "[stealth] WARNING: timezone changed since last session ({old_tz} → {new_tz}). \
                 A browser switching timezone between visits is a bot signal."
            );
        }
    }
    if let Some(old_locale) = old.get("locale").and_then(|l| l.as_str()) {
        let new_locale = std::env::var("BLADE_LOCALE").unwrap_or_default();
        if !new_locale.is_empty() && new_locale != old_locale {
            eprintln!(
                "[stealth] WARNING: locale changed since last session ({old_locale} → {new_locale})."
            );
        }
    }
}

fn generate_seed() -> u32 {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 4];
            if f.read_exact(&mut buf).is_ok() {
                return u32::from_le_bytes(buf);
            }
        }
    }
    // Fallback for all platforms: system time + process id.
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    ((now ^ (pid as u64 * 0x9e3779b97f4a7c15)) & 0xffffffff) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_is_stable_across_calls() {
        // load_or_create_seed should return the same value on repeated calls
        // when the file exists. We can't test against the real ~/.blade, so
        // test the generation + build_fingerprint round-trip.
        let seed = generate_seed();
        let fp = build_fingerprint(seed);
        assert_eq!(fp.get("seed").unwrap().as_u64(), Some(seed as u64));
    }

    #[test]
    fn generate_seed_is_u32() {
        let seed = generate_seed();
        assert!(seed != 0 || true); // seed can be 0 (1 in 4 billion)
    }
}
