//! Stable semantic refs — the #1 reliability win (decision D4).
//!
//! Other tools hand the agent a positional index (`ref=e5`) that silently
//! points at the *wrong* element after a re-render. Bladebro's refs are
//! **semantic anchors**: the stabilizer matches the new capture against the
//! previous Live Page Model by signature (`role|name|ordinal`) and persists the
//! ref number for matched elements. When an element is truly gone, we report
//! the invalidation explicitly and offer the nearest equivalent by signature
//! similarity. The agent never clicks a stale ref into the void.
//!
//! Matching strategy (v1): exact signature match first; if a new element's
//! signature has no exact match but the old element with the same role+name
//! exists with a shifted ordinal, rebind it (the set reordered, not replaced).
//! Only truly novel role+name pairs get fresh refs; truly gone ones invalidate.

use std::collections::HashMap;

use crate::page::perception::RawElement;

/// A ref assigned to a captured element, persisted across captures when the
/// element survives.
#[derive(Debug, Clone)]
pub struct RefEntry {
    /// Stable id like `"e12"` — the handle the agent refers to.
    pub id: String,
    pub sig: String,
    pub role: String,
    pub name: String,
}

/// The outcome of stabilizing a new capture against the previous model.
#[derive(Debug, Clone, Default)]
pub struct StabilizeResult {
    /// New `sig → ref id` map for the current capture (the live ref table).
    pub live: HashMap<String, String>,
    /// Refs that were present last capture and are gone now.
    pub removed: Vec<RefEntry>,
    /// Refs newly assigned this capture (with their element index).
    pub added: Vec<(usize, String)>,
    /// Refs whose element survived but whose live *state* changed (value,
    /// disabled, checked). Stored as `(ref id, what changed)` so the diff can
    /// surface it to the agent.
    pub changed: Vec<(String, StateChange)>,
    /// Refs rebound via structural fingerprint after a re-render (D48).
    /// `(ref id, new sig)` — the ref survived even though its sig changed
    /// because the structural fingerprint matched. This is the re-render
    /// immunity signal: the agent was never visible to the agent as a loss.
    pub rebound: Vec<(String, String)>,
}

/// A detected change in an element's live state between captures.
#[derive(Debug, Clone)]
pub struct StateChange {
    pub value: Option<String>,
    pub disabled: Option<bool>,
    pub checked: Option<bool>,
}

/// Previous-state entry: everything needed to match a ref to its next
/// incarnation across captures.
#[derive(Debug, Clone)]
pub struct PrevState {
    pub sig: String,
    pub role: String,
    pub name: String,
    pub probe: Option<StateProbe>,
    /// Structural identity fingerprint (D48). 0 means "no fingerprint"
    /// (pre-D48 refs).
    pub fingerprint: u64,
}

/// Stabilize a new capture against the previous ref table.
///
/// `prev` maps `ref id -> PrevState` from the prior LPM.
/// `next` is the freshly captured elements (index = position in capture).
///
/// Returns the new live ref table plus the added/removed/changed deltas.
pub fn stabilize(
    prev: &HashMap<String, PrevState>,
    next: &[RawElement],
    next_ref: &mut u64,
) -> StabilizeResult {
    // D48: index previous elements by fingerprint for re-render immunity.
    // Index by both exact sig (primary) and structural fingerprint (secondary).
    let mut prev_by_sig: HashMap<&str, &str> = HashMap::new(); // sig -> ref id
    let mut prev_by_fp: HashMap<u64, Vec<&str>> = HashMap::new(); // fp -> [ref ids]
    for (ref_id, ps) in prev {
        prev_by_sig.insert(ps.sig.as_str(), ref_id.as_str());
        if ps.fingerprint != 0 {
            prev_by_fp
                .entry(ps.fingerprint)
                .or_default()
                .push(ref_id.as_str());
        }
    }


    let mut result = StabilizeResult::default();
    let mut used_prev: std::collections::HashSet<String> = std::collections::HashSet::new();
    // `fresh` tracks element indexes needing NEW refs (not rebound).
    // Using a HashSet so we can `.remove(i)` when fingerprint-rebind claims one.
    let mut fresh: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut fresh_vec: Vec<usize> = Vec::new();

    // Pass 1: exact signature match — the element survived unchanged.
    for (i, el) in next.iter().enumerate() {
        if let Some(&ref_id) = prev_by_sig.get(el.sig.as_str()) {
            if used_prev.insert(ref_id.to_string()) {
                result.live.insert(el.sig.clone(), ref_id.to_string());
                if let Some(change) = diff_state_opt(prev.get(ref_id).and_then(|p| p.probe.as_ref()), el) {
                    result.changed.push((ref_id.to_string(), change));
                }
                continue;
            }
        }
        fresh.insert(i);
        fresh_vec.push(i);
    }

    // Pass 2: fingerprint rebind (D48). The element's sig changed (text,
    // class, state) but its structural fingerprint is identical — meaning
    // the same component re-rendered into a new DOM node. This is the core
    // innovation: React/Vue/Angular re-renders kill refs in every other
    // agent browser. Bladebro's fingerprint survives them.
    for &i in &fresh_vec {
        let el = &next[i];
if el.fingerprint == 0 {
            continue; // Pre-D48 captures or 0-fp elements can't fingerprint-match
        }
        if let Some(candidates) = prev_by_fp.get(&el.fingerprint) {
for &ref_id in candidates {
                if used_prev.contains(ref_id) {
                    continue;
                }
                // Found the match: this new element IS the old ref, re-rendered.
result.live.insert(el.sig.clone(), ref_id.to_string());
                used_prev.insert(ref_id.to_string());
                if let Some(change) = diff_state_opt(prev.get(ref_id).and_then(|p| p.probe.as_ref()), el) {
                    result.changed.push((ref_id.to_string(), change));
                }
                result.rebound.push((ref_id.to_string(), el.sig.clone()));
                fresh.remove(&i);
                break;
            }
        }
    }

    // Pass 3: mint fresh refs for remaining unclaimed elements.
    for i in fresh {
        let el = &next[i];
        let mut id = format!("e{}", *next_ref);
        while used_prev.contains(&id) {
            *next_ref += 1;
            id = format!("e{}", *next_ref);
        }
        *next_ref += 1;
        used_prev.insert(id.clone());
        result.live.insert(el.sig.clone(), id.clone());
        result.added.push((i, id.clone()));
    }

    // Pass 4: removed = prev refs not used by any survivor.
    for (ref_id, ps) in prev {
        if !used_prev.contains(ref_id) {
            result.removed.push(RefEntry {
                id: ref_id.clone(),
                sig: ps.sig.clone(),
                role: ps.role.clone(),
                name: ps.name.clone(),
            });
        }
    }

    result
}

/// Structural identity fingerprint (D48): hash over ancestor chain +
/// tag name + first children + identity attributes. Survives DOM
/// re-renders that preserve structure even when text or classes change
/// (React/Vue/Angular re-renders, live region updates, counter changes).
/// Different sigs but same structural fingerprint = same element.
/// 
/// The live state of an element at capture time, stored on the LPM so the next
/// capture can detect changes without a separate query.
#[derive(Debug, Clone)]
pub struct StateProbe {
    pub value: Option<String>,
    pub disabled: bool,
    pub checked: Option<bool>,
}

impl StateProbe {
    pub fn from(el: &RawElement) -> Self {
        Self {
            value: el.value.clone(),
            disabled: el.disabled,
            checked: el.checked,
        }
    }
}

/// Compare previous state to a new element; return a change record if anything
/// the agent cares about actually differs.
fn diff_state_opt(prev: Option<&StateProbe>, el: &RawElement) -> Option<StateChange> {
    let prev = prev?;
    diff_state(prev, el)
}

fn diff_state(prev: &StateProbe, el: &RawElement) -> Option<StateChange> {
    let value_changed = prev.value != el.value && el.value.is_some();
    let disabled_changed = prev.disabled != el.disabled;
    let checked_changed = prev.checked != el.checked;
    if !value_changed && !disabled_changed && !checked_changed {
        return None;
    }
    Some(StateChange {
        value: if value_changed { el.value.clone() } else { None },
        disabled: if disabled_changed { Some(el.disabled) } else { None },
        checked: if checked_changed { el.checked } else { None },
    })
}
