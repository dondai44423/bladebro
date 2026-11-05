//! Behaviour tests for the injected action scripts. They run the REAL
//! assembled expressions (find-by-sig, scroll probe) under node against
//! `tests/fixtures/mini_dom.js`, a test-only DOM stand-in, so a targeting
//! regression fails here without a browser. Node is required, like the
//! injected-script tests under `tests/`.

use super::find::find_sig_expr;
use super::verdict::scroll_probe_expr;

const MINI_DOM: &str = include_str!("../../tests/fixtures/mini_dom.js");

/// Run `body` after the harness and fail the test on any failed assertion.
/// Placeholders `__NAME__` in `body` are replaced from `exprs`.
fn run(name: &str, body: &str, exprs: &[(&str, String)]) {
    let mut script = body.to_string();
    for (key, js) in exprs {
        script = script.replace(key, js);
    }
    let path = std::env::temp_dir().join(format!("bladebro-dom-{name}-{}.js", std::process::id()));
    std::fs::write(&path, format!("{MINI_DOM}\n{script}\ndone({name:?});\n"))
        .expect("write dom case");
    let out = std::process::Command::new("node")
        .arg(&path)
        .output()
        .expect("node is required for injected-script tests");
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "{name}:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// find-by-sig, wrapped as a JS thunk so the case decides when it runs.
fn sig_in(sig: &str, mode: &str, text: Option<&str>, frame: &[usize]) -> String {
    format!(
        "(() => {})",
        find_sig_expr(sig, mode, text, frame).expect("find-sig expr builds")
    )
}

fn sig(sig: &str, mode: &str, text: Option<&str>) -> String {
    sig_in(sig, mode, text, &[])
}

// ---- L2: input actions adopt only the addressed field's own editor ----
//
// The input paths (prepare/check) used to accept ANY focused editable when
// the addressed sig matched nothing, and any nearby visible input for a
// hidden target. A password typed by ref landed in the focused email field
// and the verdict called it "the live editor".
const LOGIN: &str = r#"
const d = use(page({}, h('form', {rect: [0, 0, 400, 200]},
  h('p', {rect: [0, 0, 400, 40]}, h('input', {id: 'email', type: 'email', 'aria-label': 'Email', rect: [10, 10, 300, 30]})),
  h('p', {rect: [0, 40, 400, 40]}, h('input', {id: 'pw', type: 'password', 'aria-label': 'Your password', rect: [10, 50, 300, 30]})),
  h('p', {rect: [0, 80, 400, 40]}, h('input', {id: 'pw2', type: 'password', 'aria-label': 'Second password', rect: [10, 90, 300, 30], style: {display: 'none'}})),
  h('p', {rect: [0, 120, 400, 40]}, h('input', {id: 'pw3', type: 'password', 'aria-label': 'Locked password', rect: [10, 130, 300, 30], disabled: ''}))),
  h('div', {id: 'notes', contenteditable: 'true', rect: [0, 300, 400, 60]}, '')));
const email = d.getElementById('email');
email.value = 'me@example.com';
email.focus();
const untouched = (label) => {
  ok(d.activeElement === email, label + ': focus stays on the email field');
  eq(email.value, 'me@example.com', label + ': email untouched');
};
"#;

#[test]
fn drifted_sig_never_adopts_an_unrelated_focused_field() {
    // Captured as "Password"; the page relabelled it "Your password".
    let body = LOGIN.to_string()
        + r#"
for (const [mode, run] of [['check', __CHECK__], ['prepare', __PREPARE__]]) {
  const r = run();
  ok(r && r.ok === false, mode + ': a sig that matches nothing must be not-found, not the focused email', r);
  untouched(mode);
}
// An unlabelled rich editor holding focus is not the missing password
// either: prepare resolves the addressed element or nothing.
d.getElementById('notes').focus();
const p = __PREPARE__();
ok(p && p.ok === false, 'prepare never adopts a focused editor for a missing target', p);
"#;
    run(
        "l2_drift",
        &body,
        &[
            ("__CHECK__", sig("|textbox|Password|1", "check", None)),
            ("__PREPARE__", sig("|textbox|Password|1", "prepare", None)),
        ],
    );
}

#[test]
fn hidden_target_never_adopts_a_sibling_form_field() {
    let body = LOGIN.to_string()
        + r#"
for (const [mode, run] of [['check', __CHECK__], ['prepare', __PREPARE__], ['focus', __FOCUS__]]) {
  const r = run();
  ok(r && r.ok === false && r.reason === 'element hidden', mode + ': a hidden password has no editor of its own here', r);
  untouched(mode);
}
"#;
    run(
        "l2_hidden",
        &body,
        &[
            (
                "__CHECK__",
                sig("|textbox|Second password|1", "check", None),
            ),
            (
                "__PREPARE__",
                sig("|textbox|Second password|1", "prepare", None),
            ),
            (
                "__FOCUS__",
                sig("|textbox|Second password|1", "focus", None),
            ),
        ],
    );
}

#[test]
fn unfocusable_target_reports_the_field_holding_focus() {
    // A disabled field cannot take focus: keys would go to the email field.
    // prepare must keep the target as the host and name where focus is.
    let body = LOGIN.to_string()
        + r#"
const r = __PREPARE__();
ok(r && r.ok && r.hostIsTgt === true && r.disabled === true, 'the disabled target stays the host', r);
ok(r && typeof r.away === 'string' && r.away.indexOf('email') >= 0, 'prepare names the focused field keys would reach', r);
ok(r && String(r.away).indexOf('me@example.com') < 0, 'the focused field is named, never its value', r);
const c = __CHECK__();
ok(c && c.ok && c.hostIsTgt === true && c.text === '', 'check reads the target, not the focused email', c);
untouched('disabled');
"#;
    run(
        "l2_disabled",
        &body,
        &[
            (
                "__PREPARE__",
                sig("|textbox|Locked password|1", "prepare", None),
            ),
            (
                "__CHECK__",
                sig("|textbox|Locked password|1", "check", None),
            ),
        ],
    );
}

// The legitimate adoptions the fallback exists for (qol_probe locks them
// live): a hidden facade wrapper hands off to the rich editor mounted in its
// own wrapper, and an unlabelled contenteditable whose name IS its content
// drifts away from its captured sig while being edited.
#[test]
fn own_editor_adoption_still_works() {
    let body = r#"
const d = use(page({},
  h('section', {id: 'plain', rect: [0, 0, 600, 60]}, h('input', {id: 'other', 'aria-label': 'Other field', rect: [10, 10, 300, 30]})),
  h('section', {id: 'composer', rect: [0, 80, 600, 200]},
    h('div', {id: 'cwrap', rect: [0, 80, 600, 120]},
      h('textarea', {id: 'facade', placeholder: 'Join the conversation', rect: [0, 80, 500, 46], style: {display: 'none'}}),
      h('div', {id: 'eslot', rect: [0, 130, 600, 60]},
        h('div', {id: 'editor', contenteditable: 'true', 'aria-label': 'Comment editor', rect: [0, 130, 500, 50]}, 'draft words')))),
  h('div', {id: 'free', contenteditable: 'true', rect: [0, 300, 500, 50]}, 'new words')));
const editor = d.getElementById('editor');
const other = d.getElementById('other');
editor.focus();
let r = __FACADE_CHECK__();
ok(r && r.ok && r.hostIsTgt === false && r.hostKind === 'ce' && r.text === 'draft words', 'focused editor in the facade wrapper is adopted', r);
other.focus();
r = __FACADE_CHECK__();
ok(r && r.ok && r.text === 'draft words', 'the wrapper editor is adopted even when an unrelated field holds focus', r);
ok(d.activeElement === other, 'check never moves focus');
r = __FACADE_PREPARE__();
ok(r && r.ok && r.text === 'draft words', 'prepare adopts the wrapper editor', r);
ok(d.activeElement === editor, 'prepare focuses the adopted editor');
d.getElementById('free').focus();
r = __DRIFT_CHECK__();
ok(r && r.ok && r.hostKind === 'ce' && r.text === 'new words' && r.tgtMissing === true, 'an unlabelled editor whose content (its name) changed is still read back', r);
"#;
    run(
        "l2_own_editor",
        body,
        &[
            (
                "__FACADE_CHECK__",
                sig("|textbox|Join the conversation|1", "check", None),
            ),
            (
                "__FACADE_PREPARE__",
                sig("|textbox|Join the conversation|1", "prepare", None),
            ),
            (
                "__DRIFT_CHECK__",
                sig("|textbox|old words|1", "check", None),
            ),
        ],
    );
}

#[test]
fn visible_facade_hands_focus_to_its_own_editor() {
    // The facade stays rendered but its focus handler moves focus into the
    // rich editor mounted beside it: that hand-off is the target's own.
    let body = r#"
const d = use(page({},
  h('input', {id: 'other', 'aria-label': 'Other field', rect: [10, 10, 300, 30]}),
  h('div', {id: 'cwrap', rect: [0, 80, 600, 120]},
    h('textarea', {id: 'facade', placeholder: 'Write a reply', rect: [0, 80, 500, 30]}),
    h('div', {id: 'editor', contenteditable: 'true', 'aria-label': 'Reply editor', rect: [0, 120, 500, 50]}, 'kept'))));
const editor = d.getElementById('editor');
d.getElementById('facade').addEventListener('focus', () => editor.focus());
d.getElementById('other').focus();
const r = __PREPARE__();
ok(r && r.ok && r.hostIsTgt === false && r.hostKind === 'ce' && r.text === 'kept' && !r.away, 'prepare follows the hand-off into the wrapper editor', r);
ok(d.activeElement === editor, 'the editor keeps focus');
const c = __CHECK__();
ok(c && c.ok && c.hostIsTgt === false && c.text === 'kept', 'check reads the editor that holds the keys', c);
"#;
    run(
        "l2_visible_facade",
        body,
        &[
            (
                "__PREPARE__",
                sig("|textbox|Write a reply|1", "prepare", None),
            ),
            ("__CHECK__", sig("|textbox|Write a reply|1", "check", None)),
        ],
    );
}

#[test]
fn foreign_focus_is_named_not_adopted() {
    // Focusing the target moved focus to an editor OUTSIDE its wrapper, and
    // a hidden body-level field with one page editor: neither is provably
    // the target's own, so prepare names the focus holder / refuses.
    let body = r#"
const d = use(page({},
  h('div', {id: 'cwrap', rect: [0, 0, 600, 60]}, h('textarea', {id: 'ta', placeholder: 'Message', rect: [0, 0, 500, 30]})),
  h('div', {id: 'portal', rect: [0, 300, 600, 80]}, h('div', {id: 'modal', contenteditable: 'true', 'aria-label': 'Modal editor', rect: [0, 300, 500, 50]}, 'secret draft')),
  h('textarea', {id: 'bare', placeholder: 'Bare backing field', rect: [0, 400, 500, 30], style: {display: 'none'}})));
d.getElementById('ta').addEventListener('focus', () => d.getElementById('modal').focus());
const r = __PREPARE__();
ok(r && r.ok && r.hostIsTgt === true, 'an editor outside the wrapper is not adopted', r);
ok(r && typeof r.away === 'string' && r.away.indexOf('Modal editor') >= 0 && r.away.indexOf('secret') < 0, 'prepare names the editor holding focus (label only)', r);
const b = __BARE__();
ok(b && b.ok === false && b.reason === 'element hidden', 'a body-level hidden field does not own every page editor', b);
"#;
    run(
        "l2_foreign_focus",
        body,
        &[
            ("__PREPARE__", sig("|textbox|Message|1", "prepare", None)),
            (
                "__BARE__",
                sig("|textbox|Bare backing field|1", "check", None),
            ),
        ],
    );
}

// ---- L3: leaf-forwarding never trades an actionable target for another ----
#[test]
fn actionable_container_is_clicked_itself() {
    // A clickable row (role=button, focusable, own handler) holding a Delete
    // button: clicking the row's center opens it. Leaf-forwarding used to
    // move the click onto Delete.
    let body = r#"
const d = use(page({},
  h('div', {id: 'row', role: 'button', tabindex: '0', onclick: 'open()', 'aria-label': 'Open thread', rect: [0, 0, 400, 60]},
    'Thread title',
    h('button', {id: 'del', 'aria-label': 'Delete', rect: [330, 18, 60, 24]}, 'x'))));
const r = __ROW__();
eq(r && r.box, [0, 0, 400, 60], 'the click box is the row itself');
ok(r && !r.leaf && String(r.hit_tgt).indexOf('Open thread') >= 0, 'no redirect is reported for the row', r);
"#;
    run(
        "l3_row",
        body,
        &[("__ROW__", sig("|button|Open thread|1", "box", None))],
    );
}

#[test]
fn dead_container_still_leaf_forwards_and_says_so() {
    // The #15 class keeps working: a role=button wrapper (not focusable
    // itself) around its native button forwards to the control - and the
    // box result flags the redirect so the verdict can name it.
    let body = r#"
const d = use(page({},
  h('div', {id: 'wrap', role: 'button', 'aria-label': 'Account menu', rect: [0, 100, 300, 60]},
    h('button', {id: 'acct', 'aria-label': 'Account menu', rect: [250, 110, 40, 40]}))));
let r = __WRAP__();
eq(r && r.box, [250, 110, 40, 40], 'a same-named inner button is the wrapper\'s control');
ok(r && r.leaf === true, 'the redirect is flagged', r);
"#;
    run(
        "l3_wrapper",
        body,
        &[("__WRAP__", sig("|button|Account menu|1", "box", None))],
    );
}

// ---- L5: offscreen targets are scrolled into view before lanes are chosen ----
#[test]
fn offscreen_target_is_scrolled_into_view_before_lane_choice() {
    let body = r#"
const d = use(page({vw: 800, vh: 600, h: 3000},
  h('button', {id: 'top', rect: [10, 10, 100, 30]}, 'Top button'),
  h('button', {id: 'below', rect: [10, 2400, 100, 30]}, 'Below fold')));
let r = __TOP__();
ok(r && r.ok && r.isTopmost === true && d.scrolls === 0, 'a visible target never scrolls the page', r);
r = __BELOW__();
ok(r && r.ok && r.isTopmost === true, 'below the fold: measured after scrolling into view, so the trusted mouse lane is chosen', r);
ok(r && r.box && r.box[1] >= 0 && r.box[1] + r.box[3] <= 600, 'the box is inside the viewport', r);
"#;
    run(
        "l5_offscreen",
        body,
        &[
            ("__TOP__", sig("|button|Top button|1", "box", None)),
            ("__BELOW__", sig("|button|Below fold|1", "box", None)),
        ],
    );
}

// ---- L6: select is honest about what it changed ----
#[test]
fn native_select_refuses_disabled_and_reads_back() {
    let body = r#"
const d = use(page({},
  h('select', {id: 'locked', 'aria-label': 'Locked choice', disabled: '', rect: [0, 0, 200, 30]}, h('option', {value: 'alpha'}, 'Alpha'), h('option', {value: 'beta'}, 'Beta')),
  h('select', {id: 'plan', 'aria-label': 'Plan choice', rect: [0, 40, 200, 30]}, h('option', {value: 'free'}, 'Free'), h('option', {value: 'pro', disabled: ''}, 'Pro'), h('option', {value: 'team'}, 'Team')),
  h('select', {id: 'sticky', 'aria-label': 'Sticky choice', rect: [0, 80, 200, 30]}, h('option', {value: 'one'}, 'One'), h('option', {value: 'two'}, 'Two')),
  h('select', {id: 'dup', 'aria-label': 'Shared values', rect: [0, 120, 200, 30]}, h('option', {value: ''}, 'Choose'), h('option', {value: ''}, 'None'))));
let r = __LOCKED__();
ok(r && r.ok === false && /disabled/.test(r.reason), 'a disabled select is refused', r);
eq(d.getElementById('locked').value, 'alpha', 'the disabled select is unchanged');
r = __PRO__();
ok(r && r.ok === false && /disabled/.test(r.reason), 'a disabled option is refused', r);
eq(d.getElementById('plan').value, 'free', 'the plan is unchanged');
r = __TEAM__();
ok(r && r.ok === true && r.text === 'Team', 'an enabled option is selected and read back', r);
eq(d.getElementById('plan').value, 'team', 'the plan changed');
// A page that vetoes the change (resets it in its change handler).
const st = d.getElementById('sticky');
st.addEventListener('change', () => { st.value = 'one'; });
r = __STICKY__();
ok(r && r.ok === false && r.text === 'One', 'a vetoed selection is reported with what the select shows', r);
// Options sharing a value: the requested one is selected, not the first.
r = __DUP__();
ok(r && r.ok === true && r.text === 'None' && d.getElementById('dup').selectedIndex === 1, 'the requested option of a shared value is selected', r);
"#;
    run(
        "l6_native",
        body,
        &[
            (
                "__LOCKED__",
                sig("|combobox|Locked choice|1", "select", Some("Beta")),
            ),
            (
                "__PRO__",
                sig("|combobox|Plan choice|1", "select", Some("Pro")),
            ),
            (
                "__TEAM__",
                sig("|combobox|Plan choice|1", "select", Some("Team")),
            ),
            (
                "__STICKY__",
                sig("|combobox|Sticky choice|1", "select", Some("Two")),
            ),
            (
                "__DUP__",
                sig("|combobox|Shared values|1", "select", Some("None")),
            ),
        ],
    );
}

#[test]
fn select_on_a_custom_combobox_writes_nothing() {
    let body = r#"
const d = use(page({},
  h('div', {id: 'fake', role: 'combobox', 'aria-label': 'Fruit picker', tabindex: '0', rect: [0, 0, 200, 30]}, 'choose'),
  h('input', {id: 'auto', role: 'combobox', 'aria-label': 'City search', rect: [0, 40, 200, 30]})));
let r = __FAKE__();
ok(r && r.ok === false && r.reason === 'not a native select', 'a div combobox is not claimed as selected', r);
ok(!('value' in d.getElementById('fake')), 'no expando value was written');
r = __AUTO__();
ok(r && r.ok === false && r.reason === 'text combobox', 'a text combobox is routed to typing, not assigned', r);
eq(d.getElementById('auto').value, '', 'no untrusted value was assigned');
"#;
    run(
        "l6_custom",
        body,
        &[
            (
                "__FAKE__",
                sig("|combobox|Fruit picker|1", "select", Some("banana")),
            ),
            (
                "__AUTO__",
                sig("|combobox|City search|1", "select", Some("Paris")),
            ),
        ],
    );
}

// ---- A-iframe: frame offsets start at the iframe's content box ----
#[test]
fn frame_offsets_include_the_iframe_border_and_padding() {
    let body = r#"
const inner = page({vw: 300, vh: 200},
  h('button', {id: 'tiny', 'aria-label': 'Tiny corner', rect: [0, 0, 8, 8]}));
const d = use(page({},
  h('iframe', {id: 'f', rect: [40, 40, 328, 228], style: {paddingLeft: '12px', paddingTop: '12px'}, props: {contentDocument: inner, clientLeft: 2, clientTop: 2}})));
const r = __TINY__();
eq(r && r.box, [54, 54, 8, 8], 'box = iframe border box + border + padding + element offset');
"#;
    run(
        "iframe_offset",
        body,
        &[(
            "__TINY__",
            sig_in("0|button|Tiny corner|1", "box", None, &[0]),
        )],
    );
}

// ---- A-hscroll: the scroll probe measures horizontal movement too ----
#[test]
fn scroll_probe_measures_the_horizontal_axis() {
    let body = r#"
const d = use(page({vw: 800, vh: 600, w: 4000},
  h('div', {id: 'car', rect: [0, 240, 800, 120], style: {overflowX: 'auto', overflowY: 'hidden'}, props: {scrollWidth: 6200, clientWidth: 800, scrollLeft: 300, scrollHeight: 120, clientHeight: 120}},
    h('div', {id: 'card', rect: [0, 245, 300, 100]}, 'card'))));
const p = __PROBE__;
eq([p.x, p.maxX], [0, 3200], 'window horizontal position and range');
eq([p.elx, p.elLeft], ['div#car', 300], 'the horizontal scroller under the wheel point');
"#;
    run(
        "hscroll_probe",
        body,
        &[("__PROBE__", scroll_probe_expr(400.0, 300.0))],
    );
}

// ---- A-ws: value fields read back exactly ----
#[test]
fn value_field_readback_keeps_whitespace() {
    let body = r#"
const d = use(page({}, h('input', {id: 'f', 'aria-label': 'Name field', rect: [0, 0, 200, 30]})));
const f = d.getElementById('f');
f.value = '  x';
f.focus();
const r = __CHECK__();
eq(r && r.text, '  x', 'a value field is read back verbatim, not whitespace-normalized');
"#;
    run(
        "ws_readback",
        body,
        &[("__CHECK__", sig("|textbox|Name field|1", "check", None))],
    );
}
