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
//! sits in the same uniquely-fingerprinted slot under a shifted frame index,
//! rebind it (the frames reordered, not replaced). Only truly novel
//! role+name pairs get fresh refs; truly gone ones invalidate — a ref never
//! follows the fingerprint to an element with a different name (bar a form
//! control named by its own content, see `same_element`).
//!
//! The exact match is by rank among same-named elements, so when one ahead
//! of an element comes or goes, the ref stays with its rank, i.e. with
//! whichever element holds that rank now; the fingerprint pass never sees
//! that case.

use std::collections::HashMap;

use crate::page::perception::{RawElement, SelectOptions};

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
    /// because the structural fingerprint matched and role+name did not
    /// change. This is the re-render immunity signal: the move is never
    /// visible to the agent as a loss.
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
                if let Some(change) =
                    diff_state_opt(prev.get(ref_id).and_then(|p| p.probe.as_ref()), el)
                {
                    result.changed.push((ref_id.to_string(), change));
                }
                continue;
            }
        }
        fresh.insert(i);
        fresh_vec.push(i);
    }

    // Pass 2: fingerprint rebind (D48). The element's sig moved because its
    // frame index shifted (an iframe added or removed ahead of it) but its
    // structural fingerprint is identical — the same element in the same
    // slot. A rank shift among same-named elements never gets here: pass 1
    // has already given that rank's sig to whichever element holds it now.
    // The fingerprint is a SLOT identity, not an element identity: it
    // carries no text, so on its own it cannot tell a re-render from new
    // content swapped into the same markup (a result list refreshed in
    // place). Two guards keep this pass from ever moving a ref to a
    // different element:
    // - the fingerprint must be unique on both sides. Identical component
    //   markup hashes alike (a shadow root's chain stops at the root; deep
    //   div soup past 10 levels), and a hash shared by several elements
    //   identifies none of them — picking one was a silent misbind, and in
    //   HashMap order a random one;
    // - role and name must still match (see `same_element`).
    let mut next_fp_count: HashMap<u64, usize> = HashMap::new();
    for el in next {
        *next_fp_count.entry(el.fingerprint).or_default() += 1;
    }
    for &i in &fresh_vec {
        let el = &next[i];
        if el.fingerprint == 0 {
            continue; // Pre-D48 captures or 0-fp elements can't fingerprint-match
        }
        let Some(&[ref_id]) = prev_by_fp.get(&el.fingerprint).map(Vec::as_slice) else {
            continue;
        };
        if next_fp_count.get(&el.fingerprint) != Some(&1) || used_prev.contains(ref_id) {
            continue;
        }
        let Some(ps) = prev.get(ref_id) else {
            continue;
        };
        if !same_element(ps, el) {
            continue;
        }
        // This new element IS the old ref, moved.
        result.live.insert(el.sig.clone(), ref_id.to_string());
        used_prev.insert(ref_id.to_string());
        if let Some(change) = diff_state_opt(ps.probe.as_ref(), el) {
            result.changed.push((ref_id.to_string(), change));
        }
        result.rebound.push((ref_id.to_string(), el.sig.clone()));
        fresh.remove(&i);
    }

    // Pass 3: mint fresh refs for remaining unclaimed elements.
    // v3.9: iterate in DOCUMENT ORDER (fresh_vec, filtered by what pass 2
    // claimed) — iterating the HashSet directly assigned ref numbers in
    // hash order, so a first capture could number elements e5, e2, e9, e1…
    // instead of e1, e2, e3 down the page. Deterministic and readable.
    for &i in fresh_vec.iter().filter(|i| fresh.contains(i)) {
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

/// Whether a fingerprint-matched element is still the element the old ref
/// named. A ref is a promise to the agent ("e5 is 'Result A'"), so a name
/// change in the same slot is a different element: the old ref dies (and
/// self-heals by its old identity, or errors) and the new one gets a fresh
/// ref — `- e5` / `+ e9`, never a silent `↺ e5` onto other content.
///
/// The exception is a form control named by its own content — an
/// unlabeled contenteditable (its accessible name IS the typed text) or an
/// unlabeled `<select>` (named by an option, the selected one when the first
/// is blank) — where typing or picking renames it without making it a
/// different field.
fn same_element(prev: &PrevState, el: &RawElement) -> bool {
    if prev.role != el.role {
        return false;
    }
    if prev.name == el.name {
        return true;
    }
    if let Some(opts) = &el.options {
        return named_by_option(&prev.name, opts) && named_by_option(&el.name, opts);
    }
    let prev_value = prev.probe.as_ref().and_then(|p| p.value.as_deref());
    named_by_value(&prev.role, &prev.name, prev_value)
        && named_by_value(&el.role, &el.name, el.value.as_deref())
}

/// A `<select>` whose name is blank or one of its own options' labels (the
/// name fallback for an unlabeled select). Labels are captured clipped to 60
/// chars with `|` escaped as `¦`; the name is not, so compare it the same
/// way. Checked against the NEW option list on both sides: a select whose
/// options were swapped for an unrelated set is not the same choice.
fn named_by_option(name: &str, opts: &SelectOptions) -> bool {
    if name.is_empty() {
        return true;
    }
    let clipped: String = name.replace('|', "¦").chars().take(60).collect();
    opts.items.iter().any(|(label, _)| *label == clipped)
}

/// An editable control whose name is (a prefix of) its value. Compared with
/// all whitespace removed: the capture takes the name from text content and
/// the value from `innerText`, which differ only in the whitespace block
/// boundaries and `<br>` produce; either side may be truncated.
fn named_by_value(role: &str, name: &str, value: Option<&str>) -> bool {
    if !matches!(role, "textbox" | "combobox") {
        return false;
    }
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let (name, value) = (squash(name), squash(value.unwrap_or("")));
    if name.is_empty() || value.is_empty() {
        // An empty editor has neither; a labeled one has a name but no value.
        return name.is_empty() && value.is_empty();
    }
    value.starts_with(&name) || name.starts_with(&value)
}

/// Structural identity fingerprint (D48): hash over ancestor chain +
/// tag name + first children + identity attributes. Survives DOM
/// re-renders that preserve structure even when text or classes change
/// (React/Vue/Angular re-renders, live region updates, counter changes).
/// Same fingerprint = same slot, not necessarily the same element: see
/// `same_element`.
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
        value: if value_changed {
            el.value.clone()
        } else {
            None
        },
        disabled: if disabled_changed {
            Some(el.disabled)
        } else {
            None
        },
        checked: if checked_changed { el.checked } else { None },
    })
}
