//! Unit tests for the Live Page Model: ref stabilization + diff, with no
//! browser. Proves the reliability core (refs survive re-renders, stale refs
//! invalidate, state changes surface) against synthetic captures.

use bladebro::page::perception::RawElement;
use bladebro::page::model::LivePageModel;

fn el(role: &str, name: &str, sig: &str, value: Option<&str>, disabled: bool) -> RawElement {
    RawElement {
        tag: "input".into(),
        role: role.into(),
        name: name.into(),
        element_type: Some("text".into()),
        value: value.map(String::from),
        disabled,
        checked: None,
        href: None,
        placeholder: None,
        required: false,
        has_popup: false,
        landmark: None,
        box_: [0.0; 4],
        sig: sig.into(),
        frame: Vec::new(),
        shadow: false,
        fingerprint: 0,
    }
}

fn cap(url: &str, els: Vec<RawElement>) -> bladebro::page::PageCapture {
    bladebro::page::PageCapture {
        url: url.into(),
        title: String::new(),
        ready_state: "complete".into(),
        muts: 0,
        elements: els,
    }
}

/// Helper: construct an element with a specific structural fingerprint.
/// Simulates a re-rendered element: same fingerprint, different sig/text.
fn el_fp(role: &str, name: &str, sig: &str, fingerprint: u64) -> RawElement {
    let mut e = el(role, name, sig, None, false);
    e.fingerprint = fingerprint;
    e
}

#[test]
fn first_capture_mints_refs_and_sees_all_added() {
    let mut lpm = LivePageModel::new();
    let d = lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
        el("textbox", "Email", "textbox|Email|1", None, false),
    ]));

    assert!(!d.navigated);
    assert_eq!(d.added.len(), 2);
    assert!(d.removed.is_empty());
    assert_eq!(lpm.actionables(), 2);
    assert_eq!(lpm.url(), "https://x.test/");
}

#[test]
fn identical_recapture_is_no_change() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
    ]));
    let d = lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
    ]));
    assert!(d.is_empty(), "identical recapture must produce no delta: {d:?}");
}

#[test]
fn refs_persist_across_recapture() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
    ]));
    let first_ref = lpm.elements()[0].ref_id.clone();
    lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
        el("button", "Cancel", "button|Cancel|1", None, false),
    ]));
    assert_eq!(
        lpm.elements()[0].ref_id, first_ref,
        "surviving element must keep its ref"
    );
}

#[test]
fn removed_elements_invalidate_refs() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
        el("button", "Cancel", "button|Cancel|1", None, false),
    ]));
    let d = lpm.ingest(cap("https://x.test/", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
    ]));
    assert_eq!(d.removed.len(), 1, "Cancel should be reported removed");
    assert!(d.added.is_empty());
    assert_eq!(lpm.actionables(), 1);
}

#[test]
fn state_change_surfaces_in_changed() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap("https://x.test/", vec![
        el("textbox", "Email", "textbox|Email|1", None, false),
    ]));
    let d = lpm.ingest(cap("https://x.test/", vec![
        el("textbox", "Email", "textbox|Email|1", Some("a@b.com"), false),
    ]));
    assert_eq!(d.changed.len(), 1, "typed value should surface as a change");
    assert_eq!(d.changed[0].1.value.as_deref(), Some("a@b.com"));
}

#[test]
fn navigation_detected_on_url_change() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap("https://x.test/login", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
    ]));
    let d = lpm.ingest(cap("https://x.test/home", vec![
        el("button", "Sign in", "button|Sign in|1", None, false),
    ]));
    assert!(d.navigated);
    assert_eq!(d.url, "https://x.test/home");
}

#[test]
fn fuzzy_rebind_when_ordinal_shifts() {
    // Two same-role-name buttons; if their ordinals swap, the set reordered,
    // not replaced — rebind to existing refs instead of minting new ones.
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap("https://x.test/", vec![
        el("button", "Go", "button|Go|1", None, false),
        el("button", "Go", "button|Go|2", None, false),
    ]));
    let refs: Vec<String> = lpm.elements().iter().map(|e| e.ref_id.clone()).collect();
    // Swap which one is "first" in document order (sig ordinal swapped).
    lpm.ingest(cap("https://x.test/", vec![
        el("button", "Go", "button|Go|2", None, false),
        el("button", "Go", "button|Go|1", None, false),
    ]));
    let now: Vec<String> = lpm.elements().iter().map(|e| e.ref_id.clone()).collect();
    assert_eq!(now.len(), 2);
    // No NEW refs minted (no additions) — the elements were rebound.
    assert!(lpm.elements().iter().all(|e| refs.contains(&e.ref_id)),
        "rebind must reuse existing refs, minted: {:?}", now);
}

/// Regression: WS5 grouped `Action::Click` with `ClickCoord` in `ref_id()`,
/// returning None — so `perform_with_network` never resolved the sig and the
/// Click arm's `sig_frame.unwrap()` panicked, killing the MCP server on ANY
/// ref/text click. This test pins the contract: every ref-targeted action
/// must report its ref via `ref_id()`.
#[test]
fn ref_targeted_actions_report_ref_id() {
    use bladebro::action::Action;
    let ref_actions = vec![
        Action::Click { ref_id: "e1".into() },
        Action::Type { ref_id: "e1".into(), text: "x".into() },
        Action::Clear { ref_id: "e1".into() },
        Action::Select { ref_id: "e1".into(), option: "o".into() },
        Action::Read { ref_id: "e1".into() },
        Action::Hover { ref_id: "e1".into() },
        Action::Upload { ref_id: "e1".into(), path: "/tmp/f".into() },
    ];
    for a in &ref_actions {
        assert_eq!(a.ref_id(), Some("e1"),
            "{a:?} must report its ref — perform_with_network unwraps it");
    }
    let no_ref = vec![
        Action::ClickCoord { x: 10.0, y: 20.0 },
        Action::Press { key: "Enter".into() },
        Action::Scroll { dx: 0, dy: 100 },
        Action::Wait { condition: "settle".into(), text: String::new(), timeout: std::time::Duration::from_secs(1) },
        Action::Back,
    ];
    for a in &no_ref {
        assert_eq!(a.ref_id(), None, "{a:?} has no ref");
    }
}

/// Regression: on navigation the ref counter resets to 1 while old refs
/// can be REUSED by signature match (SPA route where an element
/// persists). Naive minting then assigned the same ref to two different
/// elements — the ref-keyed model clobbered one, and an agent holding
/// the old ref would act on the wrong element.
#[test]
fn navigation_mint_never_collides_with_rebound_refs() {
    let mut lpm = LivePageModel::new();
    // Page 1: button + textbox → e1, e2.
    lpm.ingest(cap("https://x.test/1", vec![
        el("button", "Go", "button|Go|1", None, false),
        el("textbox", "Name", "textbox|Name|1", None, false),
    ]));
    // Page 2 (navigated): the textbox persists (same sig → rebind),
    // plus a fresh button and link minted from the reset counter.
    lpm.ingest(cap("https://x.test/2", vec![
        el("button", "Alert", "button|Alert|1", None, false),
        el("link", "New Link", "link|New Link|1", None, false),
        el("textbox", "Name", "textbox|Name|1", None, false),
    ]));
    let refs: Vec<&str> = lpm.elements().iter().map(|e| e.ref_id.as_str()).collect();
    let uniq: std::collections::HashSet<&&str> = refs.iter().collect();
    assert_eq!(refs.len(), uniq.len(),
        "ref collision after navigation mint: {refs:?}");
    assert_eq!(lpm.elements().len(), 3);
}

// ===== D48: Structural fingerprint re-render immunity =====

#[test]
fn re_render_immunity_refs_survive_when_text_changes_but_structure_is_identical() {
    let mut lpm = LivePageModel::new();

    // First capture: a SPA button "Submit" and a counter "0 items".
    // Both have distinct structural fingerprints (they sit in different DOM
    // locations).
    let fp_submit: u64 = 0xdeadbeef;
    let fp_counter: u64 = 0xfeedface;
    let fp_link: u64 = 0xcafebabe;

    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Submit", "button|Submit|1", fp_submit),
        el_fp("text", "0 items", "text|0 items|1", fp_counter),
        el_fp("link", "Help", "link|Help|1", fp_link),
    ]));
    assert_eq!(d.added.len(), 3);
    let submit_ref = d.added[0].ref_id.clone(); // e1
    let counter_ref = d.added[1].ref_id.clone(); // e2
    let help_ref = d.added[2].ref_id.clone();    // e3
    assert_eq!(d.rebound.len(), 0, "no rebound expected on first capture");

    // Re-render: React/Vue rebuilds the component tree.
    // Text changes: "Submit" -> "Submit (2)", "0 items" -> "1 item".
    // Structure is IDENTICAL -> same fingerprints.
    // The link's text/structure is unchanged -> exact sig match.
    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Submit (2)", "button|Submit (2)|1", fp_submit), // text changed, fp same
        el_fp("text", "1 item", "text|1 item|1", fp_counter),           // text changed, fp same
        el_fp("link", "Help", "link|Help|1", fp_link),                 // unchanged
    ]));

    // THE CORE ASSERTION: refs SURVIVED the re-render.
    // Without fingerprint matching, all three would have been removed+re-added.
    // With fingerprint matching, the first two rebound (refs kept) and the third
    // is unchanged (exact sig match).
    assert!(d.rebound.len() >= 1,
        "expected at least one fingerprint rebind, got: {:?}", d.rebound);
    assert!(d.removed.len() == 0,
        "no refs should have been removed during re-render: {:?}", d.removed);

    // Verify the refs are still the SAME identity (e1, e2, e3 preserved).
    let live_refs: Vec<&str> = lpm.elements().iter().map(|e| e.ref_id.as_str()).collect();
    assert!(live_refs.contains(&submit_ref.as_str()),
        "submit ref {submit_ref} should survive re-render");
    assert!(live_refs.contains(&counter_ref.as_str()),
        "counter ref {counter_ref} should survive re-render");
    assert!(live_refs.contains(&help_ref.as_str()),
        "help ref {help_ref} should survive");
}

#[test]
fn re_render_immunity_does_not_cross_match_different_elements() {
    // Two identical "Delete" buttons at DIFFERENT structural positions
    // (different fingerprints) must not cross-match during a re-render.
    // Each ref must track back to its OWN DOM position, not the other's.
    let mut lpm = LivePageModel::new();
    let fp_a: u64 = 0x1111;
    let fp_b: u64 = 0x2222;

    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Delete", "button|Delete|1", fp_a),
        el_fp("button", "Delete", "button|Delete|2", fp_b),
    ]));
    assert_eq!(d.added.len(), 2);
    // Record ref_id -> fingerprint mapping for later identity checking.
    let first_world: Vec<(String, u64)> = lpm.elements().iter()
        .map(|e| (e.ref_id.clone(), e.raw.fingerprint))
        .collect();

    // Re-render: text changes, structure IDENTICAL (fingerprint preserved).
    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Delete (renamed)", "button|Delete (renamed)|1", fp_a),
        el_fp("button", "Delete (renamed)", "button|Delete (renamed)|2", fp_b),
    ]));

    // Re-render immunity: no refs die, both rebind via fingerprint.
    assert!(d.removed.is_empty(), "no refs should die: {:?}", d.removed);
    assert_eq!(d.rebound.len(), 2, "both should rebind: {:?}", d.rebound);

    // CRITICAL: no cross-matching. The fingerprint is the identity anchor.
    // Verify each rebound ref tracked back to an element with the SAME
    // fingerprint it had originally (this is what prevents silent confusion).
    for (ref_id, _new_sig) in &d.rebound {
        let original_fp = first_world.iter()
            .find(|(rid, _)| rid == ref_id)
            .map(|(_, fp)| *fp)
            .unwrap_or(0);
        let current = lpm.elements().iter()
            .find(|e| e.ref_id == *ref_id)
            .map(|e| e.raw.fingerprint)
            .unwrap_or(0);
        assert_eq!(original_fp, current,
            "ref {ref_id}: original fingerprint {original_fp:#x} != current {current:#x} (cross-match!)");
    }
}

#[test]
fn re_render_no_fingerprint_falls_back_to_normal_flow() {
    let mut lpm = LivePageModel::new();

    // Pre-D48 elements: fingerprint = 0 (no structural identity).
    // Behavior should be identical to pre-D48: sig exact match only.
    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Save", "button|Save|1", 0),
    ]));
    let ref_save = d.added[0].ref_id.clone();

    // Same sig: exact match -> ref survives (existing behavior).
    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Save", "button|Save|1", 0),
    ]));
    assert!(d.rebound.is_empty(), "zero-fp: no rebound expected");
    assert!(d.removed.is_empty(), "exact sig match should keep ref");

    // Re-render with ZERO fingerprint but same sig: exact match still works.
    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Save", "button|Save|1", 0),
    ]));
    assert!(d.rebound.is_empty(), "zero-fp: no rebound expected");
    assert!(d.removed.is_empty(), "exact sig match should keep ref");
    assert!(lpm.elements().iter().any(|e| e.ref_id == ref_save),
        "ref should still exist via exact sig match");

    // Re-render with ZERO fingerprint but DIFFERENT sig (text changed):
    // NO fingerprint to match on -> ref is lost (pre-D48 behavior, correct).
    let d = lpm.ingest(cap("https://app.test/", vec![
        el_fp("button", "Save (2)", "button|Save (2)|1", 0),
    ]));
    assert_eq!(d.rebound.len(), 0, "zero-fp elements never rebound");
    assert_eq!(d.removed.len(), 1, "old ref should be removed");
    assert_eq!(d.removed[0], ref_save, "correct ref removed");
}
