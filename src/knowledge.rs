//! Domain knowledge base — cross-session learning that compounds with use.
//!
//! Two subsystems, both persisted in `~/.blade/knowledge/`, both surviving
//! machine restarts, both compounding with use:
//!
//! ## 1. Domain knowledge
//!
//! Per-site consent selectors, block configs, page timing, bot risk level.
//! Learned from successful agent interactions, applied automatically on
//! subsequent visits. Confidence-scored: entries below 0.3 are evicted;
//! entries above 0.7 are trusted and auto-applied.
//!
//! The compounding effect: visit 1 is cold start (agent dismisses consent
//! manually, configures blocking, waits for settle). Visit 10 is near-zero
//! overhead (consent auto-dismissed, blocking pre-configured, settle timeout
//! calibrated). Over hundreds of sessions, this saves thousands of agent
//! turns.
//!
//! ## 2. Behavioral fingerprint
//!
//! Persistent biometric parameters that make the browser's behavioral
//! identity stable across sessions. Same "person" types at the same speed,
//! moves the mouse with the same style, has consistent reaction time.
//! Generated once per installation with small random variations, reused
//! forever. A bot detector tracking behavioral consistency across visits
//! sees the same identity every time.
//!
//! ## Safety
//!
//! - Learn only from success. Never auto-learn from failures.
//! - Confidence scoring: asymmetric (success +0.05, failure -0.15).
//! - Expiry: entries below 0.3 confidence AND older than 30 days evicted.
//! - Fresh observation priority: new data overrides stale stored data.
//! - Graceful degradation: any knowledge failure falls back to cold-start.
//! - Atomic writes: write to `.tmp`, then `rename()`. Never half-written.
//! - Corruption recovery: corrupted file → delete, start fresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::platform;

// ── Confidence thresholds ─────────────────────────────────────────────

/// Above this, auto-apply stored knowledge.
const TRUST_THRESHOLD: f64 = 0.7;
/// Below this, the entry is a candidate for eviction.
const EVICT_THRESHOLD: f64 = 0.3;
/// Each successful application adds this (cap 1.0).
const SUCCESS_INCREMENT: f64 = 0.05;
/// Each failure subtracts this (asymmetric: failures cost more than successes earn).
const FAIL_DECREMENT: f64 = 0.15;
/// Entries below EVICT_THRESHOLD AND older than this are deleted on prune.
const EVICT_AGE_DAYS: u64 = 30;
/// New entries start here (below trust threshold — must prove itself).
const INITIAL_CONFIDENCE: f64 = 0.6;
/// Cap on number of domain files to prevent unbounded growth.
const MAX_DOMAINS: usize = 2000;
/// EWMA alpha for timing: newer samples weigh more.
const TIMING_ALPHA: f64 = 0.3;

// ── Domain knowledge ──────────────────────────────────────────────────

/// Per-site knowledge accumulated across sessions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DomainKnowledge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<ConsentKnowledge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<TimingKnowledge>,
    #[serde(default)]
    pub bot_risk: BotRiskLevel,
    #[serde(default)]
    pub visit_count: u32,
    #[serde(default)]
    pub last_visit: u64,
}

/// A learned consent dialog selector with confidence tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsentKnowledge {
    /// CSS selector that successfully dismissed the banner.
    /// Empty = generic match (not stored, re-discovered each visit).
    pub selector: String,
    /// Framework name: "onetrust", "cookiebot", "didomi", "quantcast", "truste", "generic".
    pub framework: String,
    /// 0.0-1.0. Above TRUST_THRESHOLD → auto-applied. Below EVICT_THRESHOLD → evicted.
    pub confidence: f64,
    /// Epoch seconds of last successful validation.
    pub last_validated: u64,
    #[serde(default)]
    pub success_count: u32,
    #[serde(default)]
    pub fail_count: u32,
}

/// EWMA of page settle time for a domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingKnowledge {
    /// Exponentially-weighted moving average of settle duration (ms).
    pub settle_ms: u64,
    #[serde(default)]
    pub sample_count: u32,
}

/// Observed bot detection risk for a domain.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum BotRiskLevel {
    #[default]
    Unknown,
    Low,
    Medium,
    Heavy,
}

// ── Global stats ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalStats {
    #[serde(default)]
    pub total_navigations: u32,
    #[serde(default)]
    pub total_consent_dismissed: u32,
    #[serde(default)]
    pub total_blocks_detected: u32,
}

// ── Behavioral profile ────────────────────────────────────────────────

/// Persistent biometric parameters — the browser's behavioral "personality".
/// Generated once with small random variations around the defaults, stored,
/// reused forever. Makes behavioral identity stable across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehavioralProfile {
    #[serde(default = "default_click_precision")]
    pub click_precision: f64,
    #[serde(default = "default_curve_factor")]
    pub curve_factor: f64,
    #[serde(default = "default_typing_mean")]
    pub typing_mean_ms: f64,
    #[serde(default = "default_typing_sigma")]
    pub typing_sigma: f64,
    #[serde(default = "default_gap_mean")]
    pub action_gap_mean_ms: f64,
    #[serde(default = "default_gap_sigma")]
    pub action_gap_sigma: f64,
    #[serde(default = "default_overshoot_min")]
    pub overshoot_min: i64,
    #[serde(default = "default_overshoot_max")]
    pub overshoot_max: i64,
    #[serde(default = "default_hum_mean")]
    pub hum_interval_ms: f64,
    /// Timing-profile version. Bumped when the generated speed baseline
    /// changes; older behavior.json files are rescaled on load (see `migrate`).
    #[serde(default)]
    pub version: u32,
}

fn default_click_precision() -> f64 { 2.5 }
fn default_curve_factor() -> f64 { 0.15 }
fn default_typing_mean() -> f64 { 50.0 }
fn default_typing_sigma() -> f64 { 0.3 }
fn default_gap_mean() -> f64 { 400.0 }
fn default_gap_sigma() -> f64 { 0.35 }
fn default_overshoot_min() -> i64 { 5 }
fn default_overshoot_max() -> i64 { 15 }
fn default_hum_mean() -> f64 { 2000.0 }

/// Current timing baseline version. Bump when generated speed parameters
/// change; older behavior.json files are rescaled on load (see `migrate`).
const BEHAVIOR_VERSION: u32 = 2;

/// One-time load. Thread-safe. Read-only after init.
pub static BEHAVIOR: LazyLock<BehavioralProfile> = LazyLock::new(BehavioralProfile::load_or_create);

impl BehavioralProfile {
    fn path() -> PathBuf {
        knowledge_dir().join("behavior.json")
    }

    /// Load from disk, or generate+persist a new one with small random variations.
    /// Validates and clamps all fields to sane ranges.
    fn load_or_create() -> BehavioralProfile {
        let p = Self::path();
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Ok(bp) = serde_json::from_str::<BehavioralProfile>(&content) {
                let was = bp.version;
                let bp = bp.migrate().clamped();
                if bp.version != was {
                    // Persist the one-time rescale so it doesn't re-run.
                    let _ = crate::platform::secure_write_file(
                        &p,
                        serde_json::to_string_pretty(&bp).unwrap_or_default().as_bytes(),
                    );
                }
                return bp;
            }
            eprintln!("[knowledge] corrupted behavior.json — regenerating");
            let _ = std::fs::remove_file(&p);
        }
        let bp = Self::generate();
        let _ = crate::platform::secure_create_dir_all(&knowledge_dir());
        let _ = crate::platform::secure_write_file(&p, serde_json::to_string_pretty(&bp).unwrap_or_default().as_bytes());
        eprintln!("[knowledge] generated behavioral profile");
        bp
    }

    /// Generate a new profile with small random variations around defaults.
    /// Each installation gets a slightly different "personality".
    fn generate() -> BehavioralProfile {
        let t = nanos();
        BehavioralProfile {
            click_precision: vary_f64(t, 2.5, 0.5),         // 2.0-3.0
            curve_factor: vary_f64(t.wrapping_mul(3), 0.15, 0.03), // 0.12-0.18
            typing_mean_ms: vary_f64(t.wrapping_mul(5), 50.0, 8.0), // 42-58 (fast typist)
            typing_sigma: vary_f64(t.wrapping_mul(7), 0.3, 0.05),  // 0.25-0.35
            action_gap_mean_ms: vary_f64(t.wrapping_mul(11), 400.0, 60.0), // 340-460
            action_gap_sigma: vary_f64(t.wrapping_mul(13), 0.35, 0.05), // 0.30-0.40
            overshoot_min: 5,
            overshoot_max: vary_i64(t.wrapping_mul(17), 15, 3),  // 12-18
            hum_interval_ms: vary_f64(t.wrapping_mul(19), 2000.0, 300.0), // 1700-2300
            version: BEHAVIOR_VERSION,
        }
    }

    /// Rescale timing fields of pre-v2 profiles onto the fast-human band.
    /// Maps the old generation distribution (75-105ms/char) linearly onto
    /// the new one (42-58) so every install keeps its relative personality
    /// while shedding the old self-imposed slowness.
    fn migrate(mut self) -> BehavioralProfile {
        if self.version < 2 {
            self.typing_mean_ms = 50.0 + (self.typing_mean_ms - 90.0) * (8.0 / 15.0);
            self.version = 2;
        }
        self
    }

    /// Clamp all fields to sane human-like ranges. Protects against corrupted files.
    fn clamped(self) -> BehavioralProfile {
        BehavioralProfile {
            click_precision: self.click_precision.clamp(1.0, 5.0),
            curve_factor: self.curve_factor.clamp(0.05, 0.3),
            typing_mean_ms: self.typing_mean_ms.clamp(38.0, 80.0),
            typing_sigma: self.typing_sigma.clamp(0.1, 0.6),
            action_gap_mean_ms: self.action_gap_mean_ms.clamp(200.0, 800.0),
            action_gap_sigma: self.action_gap_sigma.clamp(0.1, 0.6),
            overshoot_min: self.overshoot_min.clamp(2, 10),
            overshoot_max: self.overshoot_max.clamp(8, 25),
            hum_interval_ms: self.hum_interval_ms.clamp(1000.0, 4000.0),
            version: self.version,
        }
    }
}

fn nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
}

fn vary_f64(seed: u64, base: f64, range: f64) -> f64 {
    let t = ((seed % 10000) as f64) / 10000.0;
    base + (t - 0.5) * 2.0 * range
}

fn vary_i64(seed: u64, base: i64, range: i64) -> i64 {
    let t = (seed % 10000) as i64;
    base + ((t - 5000) * 2 * range) / 10000
}

// ── KnowledgeBase ──────────────────────────────────────────────────────

/// In-memory knowledge base. Loaded at startup, synced on shutdown + periodically.
/// Wrapped in `Arc<Mutex<>>` by the caller for shared access.
#[derive(Debug, Clone, Default)]
pub struct KnowledgeBase {
    pub domains: HashMap<String, DomainKnowledge>,
    pub stats: GlobalStats,
    dirty: bool,
}

impl KnowledgeBase {
    /// Load all domain knowledge + stats from disk. Corruption-tolerant:
    /// corrupted files are deleted and skipped. Missing dir → empty KB.
    pub fn load() -> KnowledgeBase {
        let mut kb = KnowledgeBase::default();
        let dir = knowledge_dir().join("domains");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return kb,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<DomainKnowledge>(&content) {
                    Ok(dk) => {
                        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                            kb.domains.insert(name.to_string(), dk);
                        }
                    }
                    Err(_) => {
                        eprintln!("[knowledge] corrupted domain file {} — deleting", path.display());
                        let _ = std::fs::remove_file(&path);
                    }
                },
                Err(_) => continue,
            }
        }
        // Load stats.
        let stats_path = knowledge_dir().join("stats.json");
        if let Ok(content) = std::fs::read_to_string(&stats_path) {
            if let Ok(stats) = serde_json::from_str::<GlobalStats>(&content) {
                kb.stats = stats;
            }
        }
        kb.pre_seed();
        kb
    }

    /// Pre-seed known domain knowledge for major sites. Only adds entries
    /// that don't already exist — never overwrites user-learned data.
    /// Pre-seeded entries start at TRUST_THRESHOLD so they're auto-applied
    /// immediately. If the selector is wrong or stale, the confidence system
    /// handles it (downgrade, eviction, fallback to full detection).
    fn pre_seed(&mut self) {
        let now = now_secs();
        // Amazon EU cookie consent (same selector across all Amazon EU TLDs).
        for domain in &["amazon.com", "amazon.co.uk", "amazon.de", "amazon.fr", "amazon.it", "amazon.es", "amazon.nl"] {
            let need = match self.domains.get(*domain) {
                None => true,
                Some(dk) => dk.consent.is_none(),
            };
            if need {
                let dk = self.domains.entry(domain.to_string()).or_default();
                dk.consent = Some(ConsentKnowledge {
                    selector: "#sp-cc-rejectall-link".to_string(),
                    framework: "amazon".to_string(),
                    confidence: TRUST_THRESHOLD,
                    last_validated: now,
                    success_count: 0,
                    fail_count: 0,
                });
                dk.last_visit = now;
            }
        }
        self.dirty = true;
    }

    /// Atomically write all dirty domain knowledge + stats to disk.
    /// Called on shutdown and periodically (every 60s). Best-effort.
    pub fn sync(&mut self) {
        if !self.dirty {
            return;
        }
        let dir = knowledge_dir().join("domains");
        let _ = crate::platform::secure_create_dir_all(&dir);

        // Write dirty domains.
        for (domain, dk) in &self.domains {
            let path = dir.join(format!("{domain}.json"));
            let tmp = dir.join(format!("{domain}.json.tmp"));
            if let Ok(json) = serde_json::to_string_pretty(dk) {
                if crate::platform::secure_write_file(&tmp, json.as_bytes()).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }

        // Write stats.
        let stats_path = knowledge_dir().join("stats.json");
        let stats_tmp = knowledge_dir().join("stats.json.tmp");
        if let Ok(json) = serde_json::to_string_pretty(&self.stats) {
            if crate::platform::secure_write_file(&stats_tmp, json.as_bytes()).is_ok() {
                let _ = std::fs::rename(&stats_tmp, &stats_path);
            }
        }
        self.dirty = false;
    }

    /// Evict stale/low-confidence entries. Called during sync.
    pub fn prune(&mut self) {
        let now = now_secs();
        let evict_age = EVICT_AGE_DAYS * 86400;
        self.domains.retain(|_, dk| {
            let consent_ok = dk.consent.as_ref()
                .map(|c| c.confidence >= EVICT_THRESHOLD || (now - c.last_validated) < evict_age)
                .unwrap_or(true);
            let domain_fresh = dk.visit_count > 0 && (now - dk.last_visit) < evict_age;
            consent_ok || domain_fresh || dk.block_config.is_some() || dk.timing.is_some()
        });
        // Hard cap: if still too many, evict lowest-value.
        if self.domains.len() > MAX_DOMAINS {
            let mut entries: Vec<_> = self.domains.iter()
                .map(|(k, v)| (k.clone(), v.visit_count, v.last_visit))
                .collect();
            entries.sort_by_key(|&(_, visits, last)| (visits, last));
            let to_evict = self.domains.len() - MAX_DOMAINS;
            for (k, _, _) in entries.into_iter().take(to_evict) {
                self.domains.remove(&k);
            }
        }
        self.dirty = true;
    }

    // ── Consent knowledge ─────────────────────────────────────────────

    /// Get trusted consent knowledge for a domain. Returns None if no
    /// consent knowledge or confidence below TRUST_THRESHOLD.
    pub fn get_consent(&self, domain: &str) -> Option<&ConsentKnowledge> {
        self.domains.get(domain)
            .and_then(|d| d.consent.as_ref())
            .filter(|c| c.confidence >= TRUST_THRESHOLD && !c.selector.is_empty())
    }

    /// Learn a consent selector from a successful dismissal.
    /// If we already have knowledge for this domain, update it (new selector wins).
    pub fn learn_consent(&mut self, domain: &str, selector: &str, framework: &str) {
        if selector.is_empty() {
            return; // generic match — not stable enough to persist
        }
        let now = now_secs();
        let dk = self.domains.entry(domain.to_string()).or_default();
        match &dk.consent {
            Some(existing) if existing.selector == selector => {
                // Same selector confirmed — bump confidence.
                let mut c = existing.clone();
                c.confidence = (c.confidence + SUCCESS_INCREMENT).min(1.0);
                c.success_count += 1;
                c.last_validated = now;
                dk.consent = Some(c);
            }
            _ => {
                // New or changed selector — start fresh.
                dk.consent = Some(ConsentKnowledge {
                    selector: selector.to_string(),
                    framework: framework.to_string(),
                    confidence: INITIAL_CONFIDENCE,
                    last_validated: now,
                    success_count: 1,
                    fail_count: 0,
                });
            }
        }
        dk.last_visit = now;
        self.dirty = true;
    }

    /// Downgrade consent confidence after a failed application.
    pub fn downgrade_consent(&mut self, domain: &str) {
        let now = now_secs();
        if let Some(dk) = self.domains.get_mut(domain) {
            if let Some(c) = &mut dk.consent {
                c.confidence = (c.confidence - FAIL_DECREMENT).max(0.0);
                c.fail_count += 1;
                c.last_validated = now;
                if c.confidence < EVICT_THRESHOLD {
                    dk.consent = None;
                }
            }
        }
        self.dirty = true;
    }

    // ── Block config knowledge ────────────────────────────────────────

    /// Get stored block config for a domain.
    pub fn get_block_config(&self, domain: &str) -> Option<&str> {
        self.domains.get(domain)
            .and_then(|d| d.block_config.as_deref())
            .filter(|s| !s.trim().is_empty())
    }

    /// Learn block config from agent's explicit setting.
    pub fn learn_block_config(&mut self, domain: &str, classes: &str) {
        let now = now_secs();
        let dk = self.domains.entry(domain.to_string()).or_default();
        dk.block_config = Some(classes.to_string());
        dk.last_visit = now;
        self.dirty = true;
    }

    // ── Timing knowledge ──────────────────────────────────────────────

    /// Get stored settle time for a domain.
    pub fn get_settle_ms(&self, domain: &str) -> Option<u64> {
        self.domains.get(domain)
            .and_then(|d| d.timing.as_ref())
            .map(|t| t.settle_ms)
    }

    /// Update timing EWMA with a new settle duration sample.
    pub fn update_timing(&mut self, domain: &str, settle_ms: u64) {
        let now = now_secs();
        let dk = self.domains.entry(domain.to_string()).or_default();
        dk.timing = Some(match &dk.timing {
            Some(t) => {
                let ewma = (TIMING_ALPHA * settle_ms as f64
                    + (1.0 - TIMING_ALPHA) * t.settle_ms as f64) as u64;
                TimingKnowledge {
                    settle_ms: ewma,
                    sample_count: t.sample_count + 1,
                }
            }
            None => TimingKnowledge { settle_ms, sample_count: 1 },
        });
        dk.last_visit = now;
        self.dirty = true;
    }

    // ── Bot risk ──────────────────────────────────────────────────────

    pub fn set_bot_risk(&mut self, domain: &str, risk: BotRiskLevel) {
        let now = now_secs();
        let dk = self.domains.entry(domain.to_string()).or_default();
        dk.bot_risk = risk;
        dk.last_visit = now;
        self.dirty = true;
    }

    /// Last observed bot-detection risk for a domain.
    pub fn get_bot_risk(&self, domain: &str) -> BotRiskLevel {
        self.domains.get(domain).map(|d| d.bot_risk).unwrap_or_default()
    }

    /// Raise (never lower) the recorded bot-detection risk for a domain.
    pub fn raise_bot_risk(&mut self, domain: &str, risk: BotRiskLevel) {
        if risk > self.get_bot_risk(domain) {
            self.set_bot_risk(domain, risk);
        }
    }

    // ── Visit tracking ────────────────────────────────────────────────

    pub fn record_visit(&mut self, domain: &str) {
        let now = now_secs();
        let dk = self.domains.entry(domain.to_string()).or_default();
        dk.visit_count += 1;
        dk.last_visit = now;
        self.dirty = true;
    }

    pub fn record_consent_dismissed(&mut self) {
        self.stats.total_consent_dismissed += 1;
        self.dirty = true;
    }

    pub fn record_block_detected(&mut self) {
        self.stats.total_blocks_detected += 1;
        self.dirty = true;
    }

    pub fn record_navigation(&mut self) {
        self.stats.total_navigations += 1;
        self.dirty = true;
    }
}

/// Vendor → severity for observed block pages. Heavy vendors ring the
/// bell that this domain actively fights automation; the level scales
/// challenge patience and is persisted per domain.
pub fn vendor_risk(vendor: &str) -> BotRiskLevel {
    match vendor {
        "datadome" | "akamai" | "perimeterx" => BotRiskLevel::Heavy,
        "cloudflare" | "recaptcha" | "reddit" | "reddit-humanity" => BotRiskLevel::Medium,
        "js-challenge" | "rate-limit" => BotRiskLevel::Low,
        _ => BotRiskLevel::Medium,
    }
}

/// Navigation settle cap from learned per-domain timing. Unknown domains
/// keep the 2.5s default; known domains get headroom proportional to
/// their observed settle (bounded 2.5-6s) so slow SPAs finish quieting.
pub fn nav_settle_cap_ms(learned: Option<u64>) -> u64 {
    learned.map(|ms| ms.saturating_mul(2).clamp(2500, 6000)).unwrap_or(2500)
}

/// Infer the consent framework from a CSS selector.
/// Used by `learn_consent_result` — the caller doesn't need to know the framework.
fn infer_consent_framework(selector: &str) -> &'static str {
    let s = selector.to_ascii_lowercase();
    if s.contains("onetrust") { "onetrust" }
    else if s.contains("cookiebot") { "cookiebot" }
    else if s.contains("didomi") { "didomi" }
    else if s.contains("qc-cmp") { "quantcast" }
    else if s.contains("truste") { "truste" }
    else { "generic" }
}

impl KnowledgeBase {
    /// Learn a consent selector from a successful dismissal.
    /// Infers the framework from the selector. Ignores "generic" results.
    pub fn learn_consent_result(&mut self, domain: &str, selector: &str) {
        if selector.is_empty() || selector == "generic" {
            return;
        }
        let framework = infer_consent_framework(selector);
        self.learn_consent(domain, selector, framework);
    }
}

/// Extract the registrable domain from a full URL.
/// "https://www.example.co.uk/path" → "example.co.uk"
pub fn domain_from_url(url: &str) -> String {
    // Only HTTP(S) URLs have a registrable domain.
    // Non-HTTP schemes (about, data, chrome, file) return empty.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return String::new();
    }
    let host = crate::page::intercept::url_host(url);
    if host.is_empty() {
        return String::new();
    }
    crate::page::intercept::registrable_domain(&host)
}

fn knowledge_dir() -> PathBuf {
    platform::blade_dir().join("knowledge")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Shared knowledge handle ────────────────────────────────────────────

/// Convenience type for the shared knowledge base used throughout the driver.
pub type SharedKnowledge = Arc<Mutex<KnowledgeBase>>;

/// Load a shared knowledge base from disk. Use at MCP session start.
pub fn load_shared() -> SharedKnowledge {
    Arc::new(Mutex::new(KnowledgeBase::load()))
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_confidence_increases_on_success() {
        let mut kb = KnowledgeBase::default();
        kb.learn_consent("example.com", "#onetrust-reject-all-handler", "onetrust");
        let c = kb.get_consent("example.com");
        assert!(c.is_none(), "initial confidence 0.6 is below trust threshold 0.7");
        // Simulate 3 more successes to reach trust threshold.
        for _ in 0..3 {
            kb.learn_consent("example.com", "#onetrust-reject-all-handler", "onetrust");
        }
        let c = kb.get_consent("example.com").expect("should be trusted after 4 successes");
        assert!((c.confidence - 0.75).abs() < 0.01, "confidence should be 0.75, got {}", c.confidence);
        assert_eq!(c.success_count, 4);
    }

    #[test]
    fn consent_confidence_decreases_on_failure() {
        let mut kb = KnowledgeBase::default();
        kb.learn_consent("example.com", "#btn", "generic");
        let initial = kb.domains.get("example.com").unwrap().consent.as_ref().unwrap().confidence;
        kb.downgrade_consent("example.com");
        let after = kb.domains.get("example.com").unwrap().consent.as_ref().unwrap().confidence;
        assert!(after < initial, "confidence should decrease: {initial} → {after}");
        assert_eq!(after, initial - FAIL_DECREMENT);
    }

    #[test]
    fn consent_evicted_on_low_confidence() {
        let mut kb = KnowledgeBase::default();
        kb.learn_consent("example.com", "#btn", "test");
        // Fail enough times to drop below EVICT_THRESHOLD.
        for _ in 0..5 {
            kb.downgrade_consent("example.com");
        }
        assert!(kb.domains.get("example.com").unwrap().consent.is_none(),
            "consent should be evicted after repeated failures");
    }

    #[test]
    fn consent_changed_selector_replaces() {
        let mut kb = KnowledgeBase::default();
        kb.learn_consent("example.com", "#old-btn", "onetrust");
        kb.learn_consent("example.com", "#new-btn", "onetrust");
        let c = kb.domains.get("example.com").unwrap().consent.as_ref().unwrap();
        assert_eq!(c.selector, "#new-btn");
        assert_eq!(c.success_count, 1, "new selector should reset success count");
        assert!((c.confidence - INITIAL_CONFIDENCE).abs() < 0.01, "new selector starts at initial confidence");
    }

    #[test]
    fn empty_selector_not_learned() {
        let mut kb = KnowledgeBase::default();
        kb.learn_consent("example.com", "", "generic");
        assert!(kb.domains.get("example.com").is_none() || kb.domains["example.com"].consent.is_none(),
            "empty selector should not be stored");
    }

    #[test]
    fn timing_ewma_converges() {
        let mut kb = KnowledgeBase::default();
        // Feed 10 samples of 1000ms; EWMA should converge toward 1000.
        for _ in 0..10 {
            kb.update_timing("example.com", 1000);
        }
        let t = kb.get_settle_ms("example.com").unwrap();
        assert!((t as i64 - 1000).abs() < 50, "EWMA should converge to ~1000ms, got {t}");
    }

    #[test]
    fn timing_ewma_weights_recent() {
        let mut kb = KnowledgeBase::default();
        kb.update_timing("example.com", 2000);
        kb.update_timing("example.com", 1000);
        let t = kb.get_settle_ms("example.com").unwrap();
        // Alpha=0.3: 0.3*1000 + 0.7*2000 = 1700
        assert_eq!(t, 1700, "EWMA should weight recent sample: expected 1700, got {t}");
    }

    #[test]
    fn block_config_learned_and_retrieved() {
        let mut kb = KnowledgeBase::default();
        kb.learn_block_config("example.com", "images,fonts,trackers");
        assert_eq!(kb.get_block_config("example.com"), Some("images,fonts,trackers"));
    }

    #[test]
    fn visit_count_increments() {
        let mut kb = KnowledgeBase::default();
        kb.record_visit("example.com");
        kb.record_visit("example.com");
        kb.record_visit("example.com");
        assert_eq!(kb.domains.get("example.com").unwrap().visit_count, 3);
    }

    #[test]
    fn domain_from_url_extracts_registrable_domain() {
        assert_eq!(domain_from_url("https://www.example.com/path"), "example.com");
        assert_eq!(domain_from_url("https://example.co.uk/page?q=1"), "example.co.uk");
        assert_eq!(domain_from_url("https://sub.example.com/"), "example.com");
        assert_eq!(domain_from_url("http://localhost:3000"), "localhost");
        assert_eq!(domain_from_url("about:blank"), "");
        assert_eq!(domain_from_url("data:text/html,<h1>hi</h1>"), "");
    }

    #[test]
    fn behavioral_profile_generates_in_range() {
        let bp = BehavioralProfile::generate();
        assert!(bp.click_precision >= 2.0 && bp.click_precision <= 3.0);
        assert!(bp.curve_factor >= 0.12 && bp.curve_factor <= 0.18);
        assert!(bp.typing_mean_ms >= 42.0 && bp.typing_mean_ms <= 58.0);
        assert_eq!(bp.version, 2);
        assert!(bp.overshoot_max >= 12 && bp.overshoot_max <= 18);
        assert!(bp.hum_interval_ms >= 1700.0 && bp.hum_interval_ms <= 2300.0);
    }

    #[test]
    fn behavioral_profile_clamps_extremes() {
        let bp = BehavioralProfile {
            click_precision: 100.0,
            curve_factor: -1.0,
            typing_mean_ms: 999.0,
            version: 0,
            typing_sigma: 99.0,
            action_gap_mean_ms: 9999.0,
            action_gap_sigma: 99.0,
            overshoot_min: -5,
            overshoot_max: 999,
            hum_interval_ms: 99999.0,
        };
        let c = bp.clamped();
        assert_eq!(c.click_precision, 5.0);
        assert_eq!(c.curve_factor, 0.05);
        assert_eq!(c.typing_mean_ms, 80.0);
        assert_eq!(c.overshoot_max, 25);
        assert_eq!(c.hum_interval_ms, 4000.0);
    }

    #[test]
    fn behavioral_profile_migrates_legacy_timing() {
        // Old range maps linearly: 75 -> 42, 90 -> 50, 105 -> 58.
        let fast = BehavioralProfile { typing_mean_ms: 75.0, version: 0, ..BehavioralProfile::generate() };
        let m = fast.migrate();
        assert_eq!(m.version, 2);
        assert!((m.typing_mean_ms - 42.0).abs() < 0.001, "got {}", m.typing_mean_ms);
        let slow = BehavioralProfile { typing_mean_ms: 105.0, version: 0, ..BehavioralProfile::generate() };
        assert!((slow.migrate().typing_mean_ms - 58.0).abs() < 0.001);
        // Already-current profiles are untouched.
        let v2 = BehavioralProfile { typing_mean_ms: 47.3, version: 2, ..BehavioralProfile::generate() };
        assert!((v2.migrate().typing_mean_ms - 47.3).abs() < 0.001);
        // Migration runs BEFORE clamping: a corrupted 999 comes out clamped, not scaled.
        let corrupt = BehavioralProfile { typing_mean_ms: 999.0, version: 0, ..BehavioralProfile::generate() };
        assert_eq!(corrupt.migrate().clamped().typing_mean_ms, 80.0);
    }

    #[test]
    fn prune_evicts_old_low_confidence() {
        let mut kb = KnowledgeBase::default();
        // Domain with low-confidence consent and old last_visit.
        kb.domains.insert("old.com".to_string(), DomainKnowledge {
            consent: Some(ConsentKnowledge {
                selector: "#btn".into(),
                framework: "test".into(),
                confidence: 0.1, // below EVICT_THRESHOLD
                last_validated: now_secs() - 60 * 86400, // 60 days ago
                success_count: 1,
                fail_count: 5,
            }),
            visit_count: 0,
            last_visit: now_secs() - 60 * 86400,
            ..Default::default()
        });
        // Domain with high-confidence consent — should survive.
        kb.domains.insert("good.com".to_string(), DomainKnowledge {
            consent: Some(ConsentKnowledge {
                selector: "#btn".into(),
                framework: "test".into(),
                confidence: 0.9,
                last_validated: now_secs(),
                success_count: 10,
                fail_count: 0,
            }),
            visit_count: 5,
            last_visit: now_secs(),
            ..Default::default()
        });
        kb.prune();
        assert!(!kb.domains.contains_key("old.com"), "old low-confidence should be evicted");
        assert!(kb.domains.contains_key("good.com"), "good high-confidence should survive");
    }

    #[test]
    fn knowledge_base_roundtrip() {
        // NOTE: this test intentionally does NOT touch BLADE_HOME. Env vars
        // are process-global and mutations race with parallel tests reading
        // platform::blade_dir(); the BLADE_HOME resolution is covered by
        // platform::tests (pure resolver) instead.
        let mut kb = KnowledgeBase::default();
        kb.learn_consent("example.com", "#onetrust-reject-all-handler", "onetrust");
        kb.learn_block_config("example.com", "images,fonts");
        kb.update_timing("example.com", 800);
        kb.record_visit("example.com");

        // Verify the knowledge is stored in memory.
        let dk = kb.domains.get("example.com").unwrap();
        assert!(dk.consent.is_some());
        assert_eq!(dk.block_config.as_deref(), Some("images,fonts"));
        assert_eq!(dk.timing.as_ref().unwrap().settle_ms, 800);
        assert_eq!(dk.visit_count, 1);
    }

    #[test]
    fn pre_seed_amazon_consent() {
        let mut kb = KnowledgeBase::default();
        kb.pre_seed();
        // Amazon.com should have consent at trust threshold.
        let c = kb.get_consent("amazon.com").expect("amazon.com should be pre-seeded");
        assert_eq!(c.selector, "#sp-cc-rejectall-link");
        assert_eq!(c.framework, "amazon");
        assert!((c.confidence - 0.7).abs() < 0.01, "should start at trust threshold");
        // Other Amazon TLDs should also be seeded.
        assert!(kb.get_consent("amazon.co.uk").is_some());
        assert!(kb.get_consent("amazon.de").is_some());
        assert!(kb.get_consent("amazon.fr").is_some());
    }

    #[test]
    fn pre_seed_does_not_overwrite_existing() {
        let mut kb = KnowledgeBase::default();
        // Simulate user-learned consent for amazon.com.
        kb.learn_consent("amazon.com", "#user-learned-btn", "test");
        kb.pre_seed();
        // Pre-seed should NOT overwrite the user-learned selector.
        let dk = kb.domains.get("amazon.com").unwrap();
        let c = dk.consent.as_ref().unwrap();
        assert_eq!(c.selector, "#user-learned-btn", "pre-seed should not overwrite existing");
    }

    #[test]
    fn bot_risk_raises_but_never_lowers() {
        let mut kb = KnowledgeBase::default();
        kb.raise_bot_risk("example.com", BotRiskLevel::Heavy);
        kb.raise_bot_risk("example.com", BotRiskLevel::Low);
        assert_eq!(kb.get_bot_risk("example.com"), BotRiskLevel::Heavy);
        assert_eq!(kb.get_bot_risk("other.com"), BotRiskLevel::Unknown);
    }

    #[test]
    fn vendor_risk_mapping() {
        assert_eq!(vendor_risk("datadome"), BotRiskLevel::Heavy);
        assert_eq!(vendor_risk("akamai"), BotRiskLevel::Heavy);
        assert_eq!(vendor_risk("perimeterx"), BotRiskLevel::Heavy);
        assert_eq!(vendor_risk("cloudflare"), BotRiskLevel::Medium);
        assert_eq!(vendor_risk("recaptcha"), BotRiskLevel::Medium);
        assert_eq!(vendor_risk("reddit"), BotRiskLevel::Medium);
        assert_eq!(vendor_risk("reddit-humanity"), BotRiskLevel::Medium);
        assert_eq!(vendor_risk("js-challenge"), BotRiskLevel::Low);
        assert_eq!(vendor_risk("rate-limit"), BotRiskLevel::Low);
    }

    #[test]
    fn settle_cap_adapts_and_clamps() {
        assert_eq!(nav_settle_cap_ms(None), 2500);
        assert_eq!(nav_settle_cap_ms(Some(200)), 2500);
        assert_eq!(nav_settle_cap_ms(Some(900)), 2500);
        assert_eq!(nav_settle_cap_ms(Some(1400)), 2800);
        assert_eq!(nav_settle_cap_ms(Some(9000)), 6000);
    }

    #[test]
    fn cleared_block_config_is_not_returned() {
        let mut kb = KnowledgeBase::default();
        kb.learn_block_config("example.com", "images,fonts");
        assert_eq!(kb.get_block_config("example.com"), Some("images,fonts"));
        kb.learn_block_config("example.com", "");
        assert_eq!(kb.get_block_config("example.com"), None, "explicit clear must not read as a config");
    }
}
