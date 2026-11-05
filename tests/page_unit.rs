//! Unit tests for the Live Page Model: ref stabilization + diff, with no
//! browser. Proves the reliability core (refs survive re-renders, stale refs
//! invalidate, state changes surface) against synthetic captures.

use bladebro::page::model::LivePageModel;
use bladebro::page::perception::RawElement;

fn el(role: &str, name: &str, sig: &str, value: Option<&str>, disabled: bool) -> RawElement {
    RawElement {
        tag: "input".into(),
        role: role.into(),
        name: name.into(),
        element_type: Some("text".into()),
        value: value.map(String::from),
        options: None,
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
        pending: false,
        ctx: String::new(),
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
    let d = lpm.ingest(cap(
        "https://x.test/",
        vec![
            el("button", "Sign in", "button|Sign in|1", None, false),
            el("textbox", "Email", "textbox|Email|1", None, false),
        ],
    ));

    assert!(!d.navigated);
    assert_eq!(d.added.len(), 2);
    assert!(d.removed.is_empty());
    assert_eq!(lpm.actionables(), 2);
    assert_eq!(lpm.url(), "https://x.test/");
}

#[test]
fn identical_recapture_is_no_change() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://x.test/",
        vec![el("button", "Sign in", "button|Sign in|1", None, false)],
    ));
    let d = lpm.ingest(cap(
        "https://x.test/",
        vec![el("button", "Sign in", "button|Sign in|1", None, false)],
    ));
    assert!(
        d.is_empty(),
        "identical recapture must produce no delta: {d:?}"
    );
}

#[test]
fn refs_persist_across_recapture() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://x.test/",
        vec![el("button", "Sign in", "button|Sign in|1", None, false)],
    ));
    let first_ref = lpm.elements()[0].ref_id.clone();
    lpm.ingest(cap(
        "https://x.test/",
        vec![
            el("button", "Sign in", "button|Sign in|1", None, false),
            el("button", "Cancel", "button|Cancel|1", None, false),
        ],
    ));
    assert_eq!(
        lpm.elements()[0].ref_id,
        first_ref,
        "surviving element must keep its ref"
    );
}

#[test]
fn removed_elements_invalidate_refs() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://x.test/",
        vec![
            el("button", "Sign in", "button|Sign in|1", None, false),
            el("button", "Cancel", "button|Cancel|1", None, false),
        ],
    ));
    let d = lpm.ingest(cap(
        "https://x.test/",
        vec![el("button", "Sign in", "button|Sign in|1", None, false)],
    ));
    assert_eq!(d.removed.len(), 1, "Cancel should be reported removed");
    assert!(d.added.is_empty());
    assert_eq!(lpm.actionables(), 1);
}

#[test]
fn state_change_surfaces_in_changed() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://x.test/",
        vec![el("textbox", "Email", "textbox|Email|1", None, false)],
    ));
    let d = lpm.ingest(cap(
        "https://x.test/",
        vec![el(
            "textbox",
            "Email",
            "textbox|Email|1",
            Some("a@b.com"),
            false,
        )],
    ));
    assert_eq!(d.changed.len(), 1, "typed value should surface as a change");
    assert_eq!(d.changed[0].1.value.as_deref(), Some("a@b.com"));
}

#[test]
fn navigation_detected_on_url_change() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://x.test/login",
        vec![el("button", "Sign in", "button|Sign in|1", None, false)],
    ));
    let d = lpm.ingest(cap(
        "https://x.test/home",
        vec![el("button", "Sign in", "button|Sign in|1", None, false)],
    ));
    assert!(d.navigated);
    assert_eq!(d.url, "https://x.test/home");
}

#[test]
fn fuzzy_rebind_when_ordinal_shifts() {
    // Two same-role-name buttons; if their ordinals swap, the set reordered,
    // not replaced — rebind to existing refs instead of minting new ones.
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://x.test/",
        vec![
            el("button", "Go", "button|Go|1", None, false),
            el("button", "Go", "button|Go|2", None, false),
        ],
    ));
    let refs: Vec<String> = lpm.elements().iter().map(|e| e.ref_id.clone()).collect();
    // Swap which one is "first" in document order (sig ordinal swapped).
    lpm.ingest(cap(
        "https://x.test/",
        vec![
            el("button", "Go", "button|Go|2", None, false),
            el("button", "Go", "button|Go|1", None, false),
        ],
    ));
    let now: Vec<String> = lpm.elements().iter().map(|e| e.ref_id.clone()).collect();
    assert_eq!(now.len(), 2);
    // No NEW refs minted (no additions) — the elements were rebound.
    assert!(
        lpm.elements().iter().all(|e| refs.contains(&e.ref_id)),
        "rebind must reuse existing refs, minted: {:?}",
        now
    );
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
        Action::Click {
            ref_id: "e1".into(),
        },
        Action::Type {
            ref_id: "e1".into(),
            text: "x".into(),
        },
        Action::Clear {
            ref_id: "e1".into(),
        },
        Action::Select {
            ref_id: "e1".into(),
            option: "o".into(),
        },
        Action::Read {
            ref_id: "e1".into(),
        },
        Action::Hover {
            ref_id: "e1".into(),
        },
        Action::Upload {
            ref_id: "e1".into(),
            path: "/tmp/f".into(),
        },
    ];
    for a in &ref_actions {
        assert_eq!(
            a.ref_id(),
            Some("e1"),
            "{a:?} must report its ref — perform_with_network unwraps it"
        );
    }
    let no_ref = vec![
        Action::ClickCoord { x: 10.0, y: 20.0 },
        Action::Press {
            key: "Enter".into(),
        },
        Action::Scroll { dx: 0, dy: 100 },
        Action::Wait {
            condition: "settle".into(),
            text: String::new(),
            timeout: std::time::Duration::from_secs(1),
        },
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
    lpm.ingest(cap(
        "https://x.test/1",
        vec![
            el("button", "Go", "button|Go|1", None, false),
            el("textbox", "Name", "textbox|Name|1", None, false),
        ],
    ));
    // Page 2 (navigated): the textbox persists (same sig → rebind),
    // plus a fresh button and link minted from the reset counter.
    lpm.ingest(cap(
        "https://x.test/2",
        vec![
            el("button", "Alert", "button|Alert|1", None, false),
            el("link", "New Link", "link|New Link|1", None, false),
            el("textbox", "Name", "textbox|Name|1", None, false),
        ],
    ));
    let refs: Vec<&str> = lpm.elements().iter().map(|e| e.ref_id.as_str()).collect();
    let uniq: std::collections::HashSet<&&str> = refs.iter().collect();
    assert_eq!(
        refs.len(),
        uniq.len(),
        "ref collision after navigation mint: {refs:?}"
    );
    assert_eq!(lpm.elements().len(), 3);
}

// ===== D48: Structural fingerprint re-render immunity =====

#[test]
fn re_render_immunity_refs_survive_when_sig_moves_but_structure_is_identical() {
    let mut lpm = LivePageModel::new();

    // First capture: a checkout widget in frame 0 (button + card field) and
    // a top-level link. Distinct structural fingerprints (different DOM
    // locations).
    let fp_submit: u64 = 0xdeadbeef;
    let fp_card: u64 = 0xfeedface;
    let fp_link: u64 = 0xcafebabe;

    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Submit", "0|button|Submit|1", fp_submit),
            el_fp("textbox", "Card number", "0|textbox|Card number|1", fp_card),
            el_fp("link", "Help", "|link|Help|1", fp_link),
        ],
    ));
    assert_eq!(d.added.len(), 3);
    let submit_ref = d.added[0].ref_id.clone(); // e1
    let card_ref = d.added[1].ref_id.clone(); // e2
    let help_ref = d.added[2].ref_id.clone(); // e3
    assert_eq!(d.rebound.len(), 0, "no rebound expected on first capture");

    // Re-render: an iframe mounted ahead of the widget's frame, so the
    // widget's sigs move 0 -> 1. Structure and names are IDENTICAL -> same
    // fingerprints. The link is unchanged -> exact sig match.
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Submit", "1|button|Submit|1", fp_submit),
            el_fp("textbox", "Card number", "1|textbox|Card number|1", fp_card),
            el_fp("link", "Help", "|link|Help|1", fp_link),
        ],
    ));

    // THE CORE ASSERTION: refs SURVIVED the re-render.
    // Without fingerprint matching, the widget's two refs would have been
    // removed+re-added; with it they rebound, and the link matched by sig.
    assert_eq!(
        d.rebound.len(),
        2,
        "both widget refs rebind: {:?}",
        d.rebound
    );
    assert!(
        d.removed.is_empty() && d.added.is_empty(),
        "no refs should die or be minted during re-render: {d:?}"
    );

    // Verify the refs are still the SAME identity (e1, e2, e3 preserved).
    let live_refs: Vec<&str> = lpm.elements().iter().map(|e| e.ref_id.as_str()).collect();
    for r in [&submit_ref, &card_ref, &help_ref] {
        assert!(
            live_refs.contains(&r.as_str()),
            "{r} should survive re-render"
        );
    }

    // A label change is not a re-render: the slot now holds a different
    // element as far as the agent's ref is concerned.
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Submitted", "1|button|Submitted|1", fp_submit),
            el_fp("textbox", "Card number", "1|textbox|Card number|1", fp_card),
            el_fp("link", "Help", "|link|Help|1", fp_link),
        ],
    ));
    assert!(d.rebound.is_empty(), "{:?}", d.rebound);
    assert_eq!(d.removed, vec![submit_ref]);
    assert_eq!(d.added.len(), 1);
    assert_eq!(d.added[0].raw.name, "Submitted");
}

#[test]
fn re_render_immunity_does_not_cross_match_different_elements() {
    // Two identical "Delete" buttons at DIFFERENT structural positions
    // (different fingerprints) must not cross-match during a re-render.
    // Each ref must track back to its OWN DOM position, not the other's.
    let mut lpm = LivePageModel::new();
    let fp_a: u64 = 0x1111;
    let fp_b: u64 = 0x2222;

    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Delete", "0|button|Delete|1", fp_a),
            el_fp("button", "Delete", "0|button|Delete|2", fp_b),
        ],
    ));
    assert_eq!(d.added.len(), 2);
    // Record ref_id -> fingerprint mapping for later identity checking.
    let first_world: Vec<(String, u64)> = lpm
        .elements()
        .iter()
        .map(|e| (e.ref_id.clone(), e.raw.fingerprint))
        .collect();

    // Re-render: the frame index moved, structure IDENTICAL (fingerprint
    // preserved), names unchanged.
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Delete", "1|button|Delete|1", fp_a),
            el_fp("button", "Delete", "1|button|Delete|2", fp_b),
        ],
    ));

    // Re-render immunity: no refs die, both rebind via fingerprint.
    assert!(d.removed.is_empty(), "no refs should die: {:?}", d.removed);
    assert_eq!(d.rebound.len(), 2, "both should rebind: {:?}", d.rebound);

    // CRITICAL: no cross-matching. The fingerprint is the identity anchor.
    // Verify each rebound ref tracked back to an element with the SAME
    // fingerprint it had originally (this is what prevents silent confusion).
    for (ref_id, _new_sig) in &d.rebound {
        let original_fp = first_world
            .iter()
            .find(|(rid, _)| rid == ref_id)
            .map(|(_, fp)| *fp)
            .unwrap_or(0);
        let current = lpm
            .elements()
            .iter()
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
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![el_fp("button", "Save", "button|Save|1", 0)],
    ));
    let ref_save = d.added[0].ref_id.clone();

    // Same sig: exact match -> ref survives (existing behavior).
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![el_fp("button", "Save", "button|Save|1", 0)],
    ));
    assert!(d.rebound.is_empty(), "zero-fp: no rebound expected");
    assert!(d.removed.is_empty(), "exact sig match should keep ref");

    // Re-render with ZERO fingerprint but same sig: exact match still works.
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![el_fp("button", "Save", "button|Save|1", 0)],
    ));
    assert!(d.rebound.is_empty(), "zero-fp: no rebound expected");
    assert!(d.removed.is_empty(), "exact sig match should keep ref");
    assert!(
        lpm.elements().iter().any(|e| e.ref_id == ref_save),
        "ref should still exist via exact sig match"
    );

    // Re-render with ZERO fingerprint but DIFFERENT sig (text changed):
    // NO fingerprint to match on -> ref is lost (pre-D48 behavior, correct).
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![el_fp("button", "Save (2)", "button|Save (2)|1", 0)],
    ));
    assert_eq!(d.rebound.len(), 0, "zero-fp elements never rebound");
    assert_eq!(d.removed.len(), 1, "old ref should be removed");
    assert_eq!(d.removed[0], ref_save, "correct ref removed");
}

/// Regression: the structural fingerprint carries no text, so an XHR that
/// swaps a result list's items in place (new query, same URL, same markup)
/// left every slot with an identical fingerprint — and the old result's ref
/// was rebound to the NEW item in its slot. An agent holding `e1` for
/// "Result A" then clicked "Result X". A changed name is a different
/// element: the old ref must die (and stay healable by identity) and the
/// new item must get its own ref.
#[test]
fn fingerprint_rebind_never_moves_a_ref_to_a_differently_named_element() {
    let mut lpm = LivePageModel::new();
    // 13 stable links make this a "large" page (> 15 elements), where the
    // delta renders as a diff and a rebind shows as just `↺ eN`.
    let chrome = |els: &mut Vec<RawElement>| {
        for i in 1..=13u64 {
            els.push(el_fp(
                "link",
                &format!("Nav {i}"),
                &format!("link|Nav {i}|1"),
                0x900 + i,
            ));
        }
    };
    let mut first = vec![
        el_fp("button", "Result A", "button|Result A|1", 0xa1),
        el_fp("button", "Result B", "button|Result B|1", 0xa2),
        el_fp("button", "Result C", "button|Result C|1", 0xa3),
    ];
    chrome(&mut first);
    lpm.ingest(cap("https://search.test/", first));
    let ref_a = lpm.elements()[0].ref_id.clone();
    assert_eq!(lpm.elements()[0].raw.name, "Result A");

    // New query: the same <li><button> slots now hold different results.
    let mut second = vec![
        el_fp("button", "Result X", "button|Result X|1", 0xa1),
        el_fp("button", "Result Y", "button|Result Y|1", 0xa2),
        el_fp("button", "Result Z", "button|Result Z|1", 0xa3),
    ];
    chrome(&mut second);
    let d = lpm.ingest(cap("https://search.test/", second));

    if let Some(e) = lpm.element(&ref_a) {
        panic!(
            "{ref_a} named \"Result A\" now points at {} \"{}\" (silent rebind)",
            e.raw.role, e.raw.name
        );
    }
    assert!(
        d.rebound.is_empty(),
        "no rebind across names: {:?}",
        d.rebound
    );
    assert_eq!(d.removed.len(), 3, "old results are gone: {:?}", d.removed);
    assert_eq!(d.added.len(), 3, "new results get fresh refs");
    assert!(d.added.iter().all(|e| e.ref_id != ref_a));
    // The agent-facing delta says so with the existing line formats.
    let text = lpm.compress_delta(&d, 4000, 0);
    assert!(text.contains(&format!("- {ref_a} (gone)")), "{text}");
    assert!(text.contains("button \"Result X\""), "{text}");
    assert!(!text.contains('↺'), "{text}");
    // The dead ref still knows what it was, so a heal finds "Result A" or
    // reports it gone — it never resolves to "Result X".
    let (_, _, name) = lpm.graveyard_lookup(&ref_a).expect("buried");
    assert_eq!(name, "Result A");
}

/// Regression: a fingerprint shared by several elements (identical
/// component markup — every shadow-DOM card's button hashes the same, the
/// ancestor chain stops at the shadow root) identifies none of them, yet the
/// first unclaimed candidate in HashMap order won the rebind. Scrolling a
/// list of such cards rebound an off-screen card's ref to whichever card
/// scrolled in.
#[test]
fn shared_fingerprint_never_rebinds() {
    let mut lpm = LivePageModel::new();
    let fp = 0xc0ffee;
    lpm.ingest(cap(
        "https://shop.test/",
        vec![
            el_fp("button", "Add to cart", "button|Add to cart|1", fp),
            el_fp("button", "Add to cart", "button|Add to cart|2", fp),
        ],
    ));
    let old: Vec<String> = lpm.elements().iter().map(|e| e.ref_id.clone()).collect();
    // Scrolled: cards 1-2 left the capture window, card 5 entered it.
    let d = lpm.ingest(cap(
        "https://shop.test/",
        vec![el_fp("button", "Add to cart", "button|Add to cart|5", fp)],
    ));
    assert!(d.rebound.is_empty(), "ambiguous rebind: {:?}", d.rebound);
    assert_eq!(d.added.len(), 1, "card 5 is a new element");
    assert!(!old.contains(&d.added[0].ref_id));
    let mut removed = d.removed.clone();
    removed.sort();
    assert_eq!(removed, old, "cards 1-2 left the window");
}

/// What the fingerprint rebind is FOR: the element's sig moved (its frame
/// index shifted, or a same-named element ahead of it came or went) while
/// its structure and name stayed put. The ref follows it.
#[test]
fn fingerprint_rebind_keeps_ref_when_only_position_shifts() {
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Pay", "0|button|Pay|1", 0x51),
            el_fp("link", "Help", "|link|Help|1", 0x52),
        ],
    ));
    let pay = lpm.elements()[0].ref_id.clone();
    // An iframe was inserted ahead of the checkout frame: frame 0 → 1.
    let d = lpm.ingest(cap(
        "https://app.test/",
        vec![
            el_fp("button", "Pay", "1|button|Pay|1", 0x51),
            el_fp("link", "Help", "|link|Help|1", 0x52),
        ],
    ));
    assert!(d.removed.is_empty() && d.added.is_empty(), "{d:?}");
    assert_eq!(d.rebound, vec![(pay.clone(), "1|button|Pay|1".to_string())]);
    assert_eq!(
        lpm.element(&pay).map(|e| e.raw.sig.as_str()),
        Some("1|button|Pay|1")
    );
}

/// The one name change that IS the same element: an editable named by its
/// own text (an unlabeled contenteditable's accessible name is what was
/// typed). A labeled field whose label changed is a different field.
#[test]
fn fingerprint_rebind_follows_an_editor_named_by_its_own_text() {
    let mut lpm = LivePageModel::new();
    let mut editor = el_fp("textbox", "", "|textbox||1", 0xed);
    editor.tag = "div".into();
    editor.element_type = None;
    let mut first_name = el_fp("textbox", "First name", "|textbox|First name|1", 0xf1);
    first_name.value = Some("Ada".into());
    lpm.ingest(cap("https://form.test/", vec![editor, first_name]));
    let ed_ref = lpm.elements()[0].ref_id.clone();
    let fn_ref = lpm.elements()[1].ref_id.clone();

    // Typed two lines into the editor. Its name is the text content
    // (`<p>Hello<br>world</p>` → "Helloworld") and its value the innerText
    // ("Hello world"): same text, different whitespace. Meanwhile
    // the wizard moved on and the same input slot is now "Last name".
    let mut typed = el_fp("textbox", "Helloworld", "|textbox|Helloworld|1", 0xed);
    typed.tag = "div".into();
    typed.element_type = None;
    typed.value = Some("Hello world".into());
    let last_name = el_fp("textbox", "Last name", "|textbox|Last name|1", 0xf1);
    let d = lpm.ingest(cap("https://form.test/", vec![typed, last_name]));

    assert_eq!(
        lpm.element(&ed_ref).map(|e| e.raw.name.as_str()),
        Some("Helloworld"),
        "typing into an editor keeps its ref: {d:?}"
    );
    assert!(
        lpm.element(&fn_ref).is_none(),
        "{fn_ref} \"First name\" must not become \"Last name\""
    );
    assert_eq!(d.removed, vec![fn_ref]);
}

/// The other control named by its own content: an unlabeled `<select>`
/// whose first option is blank is named by its selected option, so picking
/// one renames it. Same control, same ref. A select whose options were
/// replaced by an unrelated set is a different choice and gets a fresh ref.
#[test]
fn fingerprint_rebind_follows_a_select_named_by_its_selection() {
    use bladebro::page::perception::SelectOptions;
    let select = |name: &str, sel: i64, items: &[(&str, &str)], fp: u64| {
        let mut e = el_fp("combobox", name, &format!("|combobox|{name}|1"), fp);
        e.tag = "select".into();
        e.element_type = Some("select-one".into());
        e.options = Some(SelectOptions {
            sel,
            total: items.len(),
            items: items
                .iter()
                .map(|(t, v)| (t.to_string(), v.to_string()))
                .collect(),
        });
        e
    };
    // The capture drops options blank on both sides (label and value).
    let countries = [("United States", "us"), ("Canada", "ca")];
    let sizes = [("Pick a size", ""), ("Small", "s"), ("Large", "l")];
    let mut lpm = LivePageModel::new();
    lpm.ingest(cap(
        "https://form.test/",
        vec![
            select("", 0, &countries, 0x5e1),
            select("Pick a size", 0, &sizes, 0x5e2),
        ],
    ));
    let country = lpm.elements()[0].ref_id.clone();
    let size = lpm.elements()[1].ref_id.clone();

    // Picked a country; the wizard swapped the second select's options.
    let colours = [("Pick a colour", ""), ("Red", "r")];
    let d = lpm.ingest(cap(
        "https://form.test/",
        vec![
            select("Canada", 2, &countries, 0x5e1),
            select("Pick a colour", 0, &colours, 0x5e2),
        ],
    ));
    assert_eq!(
        lpm.element(&country).map(|e| e.raw.name.as_str()),
        Some("Canada"),
        "selecting keeps the select's ref: {d:?}"
    );
    assert_eq!(
        d.removed,
        vec![size],
        "a different option set is a new select"
    );
    assert_eq!(d.added.len(), 1);
}

/// Regression: a heal could leave one sig owned by two live refs. An
/// element scrolled out of the capture window (its ref died), scrolled back
/// (fresh ref), then the agent acted on the old ref: the heal matched the
/// exact sig and `adopt_as` re-adopted the old ref next to the fresh one.
/// The next ingest kept one of them in HashMap order and reported the other
/// `- eN (gone)` while the element was still on the page (live: the click's
/// verdict read `dom-changed (+0 −1)`).
#[test]
fn heal_never_leaves_two_refs_on_one_element() {
    let mut lpm = LivePageModel::new();
    let top = || el("button", "Top", "|button|Top|1", None, false);
    let docs = || el("button", "Docs", "|button|Docs|1", None, false);
    lpm.ingest(cap("https://x.test/", vec![top(), docs()]));
    let old = lpm.elements()[1].ref_id.clone();
    // Scrolled away: Docs left the capture window, its ref died.
    let d = lpm.ingest(cap("https://x.test/", vec![top()]));
    assert_eq!(d.removed, vec![old.clone()]);
    // Scrolled back: the same element is minted a fresh ref.
    let d = lpm.ingest(cap("https://x.test/", vec![top(), docs()]));
    let fresh = d.added[0].ref_id.clone();
    assert_ne!(fresh, old);
    // The agent acts on its old ref: the heal finds the exact sig.
    let (sig, role, name) = lpm.graveyard_lookup(&old).expect("buried");
    lpm.adopt_as(&old, &sig, &role, &name, &[]);
    let owners: Vec<String> = lpm
        .elements()
        .iter()
        .filter(|e| e.raw.sig == sig)
        .map(|e| e.ref_id.clone())
        .collect();

    let d = lpm.ingest(cap("https://x.test/", vec![top(), docs()]));
    assert!(
        d.removed.is_empty(),
        "element is on the page but reported gone: {:?} (sig owners after heal: {owners:?})",
        d.removed
    );
    assert_eq!(owners, vec![old.clone()], "one sig, one ref");
    assert_eq!(
        lpm.element(&old).map(|e| e.raw.name.as_str()),
        Some("Docs"),
        "the ref the agent acted through stays live"
    );
    // The retired ref still heals to the same element by its exact sig.
    let (retired_sig, _, _) = lpm.graveyard_lookup(&fresh).expect("retired, healable");
    assert_eq!(retired_sig, sig);
}

/// Regression: a healed ref kept its PRE-heal identity in the graveyard
/// (ingest skips refs already recorded there), so when it died again its
/// next heal looked for the element it named before the heal, not the one
/// the heal note told the agent about.
#[test]
fn a_healed_ref_heals_to_the_element_it_named_last() {
    let mut lpm = LivePageModel::new();
    let first = el("link", "Docs", "|link|Docs|1", None, false);
    lpm.ingest(cap("https://x.test/", vec![first]));
    let r = lpm.elements()[0].ref_id.clone();
    lpm.ingest(cap("https://x.test/", vec![]));
    // Healed by name onto the other "Docs" link (a different sig).
    lpm.adopt_as(&r, "|link|Docs|2", "link", "Docs", &[]);
    let second = el("link", "Docs", "|link|Docs|2", None, false);
    lpm.ingest(cap("https://x.test/", vec![second]));
    assert!(lpm.element(&r).is_some());
    // It dies again: its identity is the element it named last.
    lpm.ingest(cap("https://x.test/", vec![]));
    let (sig, _, _) = lpm.graveyard_lookup(&r).expect("buried");
    assert_eq!(sig, "|link|Docs|2");
}

#[test]
fn reddit_markdown_is_observational_and_preserves_live_dom() {
    let mock = r#"
let removed=0;
global.location={hostname:'www.reddit.com',href:'https://www.reddit.com/r/rust/comments/test/title/',pathname:'/r/rust/comments/test/title/'};
const text={nodeType:3,textContent:'Visible article content'};
const ad={remove(){removed++}};
const main={nodeType:1,tagName:'MAIN',innerHTML:'content',innerText:'Visible article content '.repeat(20),childNodes:[text],className:'',id:'',hidden:false,hasAttribute(){return false},getAttribute(){return null},querySelectorAll(s){return s.includes('data-testid')?[ad]:[]}};
global.document={body:{innerText:'Visible article content '.repeat(20)},title:'Title',querySelector(s){if(s==='h1')return {innerText:'Discussion title'};if(s==='main,[role="main"]')return main;return null},querySelectorAll(){return []},defaultView:{getComputedStyle(){return {display:'block',visibility:'visible',opacity:'1'}}}};
main.ownerDocument=document;
"#;
    let script = include_str!("../src/page/js/markdown.js")
        .replace("__SCOPE_FLAG__", "false")
        .replace("__BUDGET__", "8000")
        .replace("__SCOPE_MAIN__", "findMain(document)");
    let output=std::process::Command::new("node").arg("-e").arg(format!("{mock}\nconst value={script};if(removed!==0||!value.includes('Visible article content'))throw Error('markdown changed the live DOM: '+removed);"))
        .output().expect("node is required for injected-script tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn redirect_resolution_preserves_destination_escaping_and_wrapper_origin() {
    let script = include_str!("../src/mcp/server/js/auto_extract.js");
    let start = script.find("const REDIRECT_PATH=").unwrap();
    let end = script.find("// How many results").unwrap();
    let probe = r#"
global.location={href:'https://page.example/',origin:'https://page.example'};
const assert=require('node:assert/strict');
assert.equal(canonicalURL('https://www.google.com/url?q=https%3A%2F%2Fexample.com%2Factual%3Fq%3Da%2526b'),'https://example.com/actual?q=a%26b');
assert.equal(canonicalURL('https://wrapper.example/url?q=%2Frelative'),'https://wrapper.example/relative');
assert.equal(canonicalURL('https://www.google.com/goto?url=OPAQUE'),'https://www.google.com/goto?url=OPAQUE');
assert.equal(wrappedURL('https://en.wikipedia.org/wiki/URL'),false);
assert.equal(wrappedURL('https://www.google.com/goto?url=OPAQUE'),true);
"#;
    let result = std::process::Command::new("node")
        .arg("-e")
        .arg(format!("{}\n{probe}", &script[start..end]))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
