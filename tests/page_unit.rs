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
