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
}

/// A detected change in an element's live state between captures.
#[derive(Debug, Clone)]
pub struct StateChange {
    pub value: Option<String>,
    pub disabled: Option<bool>,
    pub checked: Option<bool>,
}

/// Stabilize a new capture against the previous ref table.
///
/// `prev` maps `ref id -> (sig, role, name, last state)` from the prior LPM.
/// `next` is the freshly captured elements (index = position in capture).
///
/// Returns the new live ref table plus the added/removed/changed deltas.
pub fn stabilize(
    prev: &HashMap<String, (String, String, String, Option<StateProbe>)>,
    next: &[RawElement],
    next_ref: &mut u64,
) -> StabilizeResult {
    // Index previous elements by signature for exact matching. With the V25c
    // global per-frame rank sig scheme, an exact sig match now survives BOTH
    // scrolls (document order never changes) and insertions of
    // differently-named elements (a per-name rank is unaffected). No fuzzy
    // role+name rebind is done: it cannot distinguish "same element
    // re-rendered" from "a different same-named element scrolled into view"
    // under viewport culling, and guessing wrong is a silent misclick (D25).
    // The rare same-named reorder orphans the ref to the graveyard, and a
    // later click sig-precise-heals to the exact original element.
    let mut prev_by_sig: HashMap<&str, &str> = HashMap::new(); // sig -> ref id
    for (ref_id, (sig, _, _, _)) in prev {
        prev_by_sig.insert(sig.as_str(), ref_id.as_str());
    }

    let mut result = StabilizeResult::default();
    let mut used_prev: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut fresh: Vec<usize> = Vec::new();

    // Pass 1: rebind survivors. This MUST run before any minting —
    // after a navigation the mint counter resets to 1 while old refs
    // (e1, e2…) are still rebindable, so interleaved minting hands an
    // old ref to a NEW element and the later rebind collides with it
    // (two elements sharing one ref — the ref-keyed model clobbers
    // one, and an agent holding the old ref acts on the wrong node).
    for (i, el) in next.iter().enumerate() {
        // 1. Exact signature match — the element survived unchanged.
        if let Some(&ref_id) = prev_by_sig.get(el.sig.as_str()) {
            if used_prev.insert(ref_id.to_string()) {
                result.live.insert(el.sig.clone(), ref_id.to_string());
                // Detect state change vs previous probe.
                if let Some((_, _, _, Some(prev_probe))) = prev.get(ref_id) {
                    if let Some(change) = diff_state(prev_probe, el) {
                        result.changed.push((ref_id.to_string(), change));
                    }
                }
                continue;
            }
            // The ref this sig maps to was already consumed by an earlier
            // element this round (cannot happen with rank-unique sigs, but
            // keep the guard) — treat as fresh.
        }
        // No exact sig match: this is a genuinely new element (or a survivor
        // whose rank shifted on a same-named reorder). Mint a fresh ref; the
        // old ref orphans to the graveyard and sig-precise-heals on use.
        fresh.push(i);
    }

    // Pass 2: mint fresh refs for new elements, skipping any id a
    // rebind already took.
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

    // 4. Removed: prev refs not used by any survivor.
    for (ref_id, (sig, role, name, _)) in prev {
        if !used_prev.contains(ref_id) {
            result.removed.push(RefEntry {
                id: ref_id.clone(),
                sig: sig.clone(),
                role: role.clone(),
                name: name.clone(),
            });
        }
    }

    result
}

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
