//! The action layer (decision D5/D6): the `act` tool.
//!
//! `act` resolves a stable ref to its signature, re-locates the element in the
//! live DOM by that signature, dispatches the action via CDP, waits for the
//! page to settle (navigation or DOM mutation), then recaptures and returns the
//! delta as the observation. This closes the loop: act → observe → act.
//!
//! Design choices:
//! - **Click** uses `Input.dispatchMouseEvent` at the element's center — real
//!   mouse events, not `el.click()`. Better for stealth and for sites that
//!   listen for mousedown/mouseup, not just synthetic click.
//! - **Type** uses `Input.insertText` into a focused element — fires proper
//!   input events, works for text inputs, textareas, and contenteditable.
//! - **Select** sets the `<select>` value in-page and dispatches change —
//!   mouse-based option selection is fragile across select implementations.
//! - **Press** dispatches keyDown + keyUp via `Input.dispatchKeyEvent`.
//! - **Scroll** uses `window.scrollBy` via evaluate.
//!
//! Module map: this file is the shared core — the `Action` enum and its
//! predicates; children: `perform` (the dispatch engine: perform /
//! perform_with_network plus dispatch-time watchers), `find` (element
//! location and miss diagnostics), `input` (mouse/keyboard dispatch and
//! combos), `edit` (type/clear ladders and edit verdicts), `verdict`
//! (conditions and scroll reports).
use std::time::Duration;

mod edit;
mod find;
mod input;
mod perform;
mod verdict;

pub use self::find::{
    find_by_selector, find_by_text, find_miss_diag, locate_text, read_text, MissDiag, MissExample,
    SelectorDiag, SelectorLookup, TextLocation, TextMatch,
};
pub use self::perform::{perform, perform_with_network, MUT_WATCH};
pub use self::verdict::check_condition;
/// What the agent can do. Few verbs, full control.
#[derive(Debug, Clone)]
pub enum Action {
    /// Click an element by its ref id.
    Click { ref_id: String },
    /// S10: Click at exact viewport coordinates — works on cross-origin
    /// iframes, canvas, shadow DOM (anything Input-domain can reach).
    ClickCoord { x: f64, y: f64 },
    /// Type text into a textbox, replacing existing content.
    Type { ref_id: String, text: String },
    /// Clear a textbox.
    Clear { ref_id: String },
    /// Select an option in a `<select>` by value or visible text.
    Select { ref_id: String, option: String },
    /// Press a key (e.g. "Enter", "Tab", "Escape", "ArrowDown").
    Press { key: String },
    /// Scroll by (dx, dy) CSS pixels.
    Scroll { dx: i64, dy: i64 },
    /// Read the text content of a specific element by ref id.
    Read { ref_id: String },
    /// Wait for a condition: "element" (element with matching role/name appears),
    /// "title" (title contains text), "settle" (DOM stabilizes).
    Wait {
        condition: String,
        text: String,
        timeout: Duration,
    },
    /// Go back in browser history.
    Back,
    /// Go forward in browser history.
    Forward,
    /// Reload the current page.
    Reload,
    /// Hover over an element (move mouse to its center, no click).
    /// Triggers CSS :hover, hover-dropdowns, tooltips, hover-cards.
    Hover { ref_id: String },
    /// Upload a file to an `<input type="file">` element.
    Upload { ref_id: String, path: String },
}

impl Action {
    /// Returns the ref id this action targets, if any.
    pub fn ref_id(&self) -> Option<&str> {
        match self {
            Action::ClickCoord { .. } => None,
            Action::Click { ref_id }
            | Action::Type { ref_id, .. }
            | Action::Clear { ref_id }
            | Action::Select { ref_id, .. }
            | Action::Read { ref_id }
            | Action::Hover { ref_id }
            | Action::Upload { ref_id, .. } => Some(ref_id),
            Action::Press { .. }
            | Action::Scroll { .. }
            | Action::Wait { .. }
            | Action::Back
            | Action::Forward
            | Action::Reload => None,
        }
    }

    /// True for actions that dispatch synthetic input events (or file
    /// uploads) — refused while manual control is claimed (`rb pause`), so
    /// the agent never fights the person using the browser.
    pub fn injects_input(&self) -> bool {
        matches!(
            self,
            Action::Click { .. }
                | Action::ClickCoord { .. }
                | Action::Type { .. }
                | Action::Clear { .. }
                | Action::Select { .. }
                | Action::Press { .. }
                | Action::Scroll { .. }
                | Action::Hover { .. }
                | Action::Upload { .. }
        )
    }

    /// True for actions the pause must refuse: input dispatch plus the
    /// page-level moves that would yank the user's context out from under
    /// them (history, reload). Reads, waits and eval stay available.
    pub fn disrupts_page(&self) -> bool {
        self.injects_input() || matches!(self, Action::Back | Action::Forward | Action::Reload)
    }
}

#[cfg(test)]
mod action_tests {
    // Regression tests for the CL3 / #15 click fix: leaf-targeting of
    // container-matched controls and the no-effect diagnostic that names the
    // resolved target. These lock the message format (which consumers parse)
    // and the em-dash-free guarantee (public-facing strings use hyphens).

    #[test]
    fn no_effect_verdict_names_target_without_em_dashes() {
        let msg = super::verdict::no_effect_verdict(
            &["mouse", "js", "enter"],
            "button [Account menu] (topmost=true,disabled=false)",
        );
        assert_eq!(
            msg,
            "outcome: no-effect (click dispatched via mouse, js, enter on button [Account menu] (topmost=true,disabled=false) - no navigation, no observable DOM or state change)"
        );
        assert!(!msg.contains('\u{2014}'), "no em-dash in public verdict");
    }

    #[test]
    fn dom_effect_summary_distinguishes_state_only_changes() {
        use super::verdict::dom_effect_summary;
        use crate::page::refs::StateChange;
        use crate::page::PageDelta;
        // Nothing changed → None (the no-effect path).
        assert!(dom_effect_summary(&PageDelta::default()).is_none());
        // Node change → real counts.
        let mut d = PageDelta::default();
        d.removed.push("e9".into());
        assert_eq!(dom_effect_summary(&d).unwrap(), "+0 \u{2212}1");
        // State-only change (value/checked/disabled flip) → explicitly weak:
        // this is the class that used to print a self-contradictory "(+0 -0)".
        let mut d = PageDelta::default();
        d.changed.push((
            "e2".into(),
            StateChange {
                value: None,
                disabled: None,
                checked: Some(true),
            },
        ));
        let s = dom_effect_summary(&d).unwrap();
        assert!(s.starts_with("state-only:"), "{s}");
        assert!(
            s.contains("e2") && s.contains("no nodes added/removed"),
            "{s}"
        );
    }

    #[test]
    fn no_effect_verdict_falls_back_when_target_unknown() {
        let msg = super::verdict::no_effect_verdict(&["mouse"], "");
        assert!(
            msg.ends_with("- no navigation, no observable DOM or state change; the element may be disabled, hidden, or hover-gated)"),
            "unexpected fallback: {msg}"
        );
        assert!(!msg.contains('\u{2014}'), "no em-dash in fallback verdict");
    }

    // Guards the leaf-targeting JS fragment: the box-mode resolver must stay
    // wired into the injected script, or container-matched clicks silently
    // regress to the no-effect they were built to fix.
    #[test]
    fn leaf_target_fragment_is_present_and_em_dash_free() {
        let js = super::verdict::LEAF_TARGET_JS;
        assert!(
            js.contains("elementFromPoint"),
            "leaf-target uses hit-testing"
        );
        assert!(
            js.contains("querySelectorAll('button,a[href]"),
            "leaf-target finds native controls"
        );
        assert!(
            js.contains("_lbest"),
            "leaf-target selects nearest candidate"
        );
        assert!(js.contains("_lClick"), "leaf-target rewrites the click box");
        assert!(js.contains("_lhr"), "leaf-target rewrites the target label");
        assert!(!js.contains('\u{2014}'), "no em-dash in injected JS");
    }

    // The if/while absence fast path (v3.10): only a QUIET page counts as
    // confirmed absence, and only when a probe is supplied (wait passes None
    // and keeps its full time budget).
    #[test]
    fn absence_confirms_only_when_quiet_and_past_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut since = None;
        assert!(!super::verdict::absence_confirmed(&mut since, None));
        assert!(since.is_none(), "no probe must not start the absence clock");
        let counter = AtomicUsize::new(1);
        since = Some(std::time::Instant::now() - std::time::Duration::from_millis(900));
        assert!(
            !super::verdict::absence_confirmed(&mut since, Some(&counter)),
            "busy page never confirms"
        );
        counter.store(0, Ordering::Relaxed);
        assert!(
            super::verdict::absence_confirmed(&mut since, Some(&counter)),
            "quiet + past window confirms"
        );
    }

    // The find-by-sig script is built at runtime; a syntax error in it
    // disables every ref-targeted action at once. `node --check` guards it,
    // following the perception.rs script-check pattern (skips without node).
    #[test]
    fn find_sig_script_is_valid_js() {
        let has_node = std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping find-sig syntax check");
            return;
        }
        for (mode, text) in [
            ("box", None),
            ("prepare", None),
            ("check", None),
            ("type", Some("hello 'quoted' text")),
            ("clear", None),
            ("selhost", None),
            ("select", Some("opt")),
            ("read", None),
            ("hover", None),
            ("click", None),
        ] {
            let js = super::find::find_sig_expr("0|textbox|Post text|1", mode, text, &[0])
                .expect("find-sig expr builds");
            let path = std::env::temp_dir().join(format!("bladebro-findsig-{mode}.js"));
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "find-sig ({mode}) has a JS syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // The find-by-text script powers text addressing AND ref healing;
        // guard it the same way.
        for (q, rf, ih) in [
            ("Post text", None, false),
            ("Join the conversation", Some("textbox"), true),
        ] {
            let js = super::find::find_text_expr(q, rf, ih).expect("find-text expr builds");
            let path = std::env::temp_dir().join("bladebro-findtext.js");
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "find-text has a JS syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    // The selector matcher, the find-miss diagnostic and the coordinate
    // hit probe are injected into live pages the same way; a syntax error in
    // any of them would silently break addressing or misreport misses.
    // Guarded with node --check (skips without node).
    #[test]
    fn selector_diag_and_probe_scripts_are_valid_js() {
        let has_node = std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping selector/diag/probe syntax checks");
            return;
        }
        let cases: Vec<(&str, String)> = vec![
            (
                "bd-select",
                super::find::find_selector_expr("#overflow-trigger").expect("selector expr"),
            ),
            (
                "bd-select2",
                super::find::find_selector_expr("[role=menuitem]").expect("selector expr"),
            ),
            (
                "bd-missdiag",
                super::find::find_miss_expr("Open user actions \"quoted\"").expect("miss expr"),
            ),
            ("bd-hitprobe", super::verdict::hit_probe_expr(216.0, 616.5)),
            (
                "bd-textloc",
                super::find::text_locate_expr("are you sure? yes").expect("text-locate expr"),
            ),
            (
                "bd-scrollprobe",
                super::verdict::scroll_probe_expr(480.0, 270.0),
            ),
        ];
        for (name, js) in cases {
            let path = std::env::temp_dir().join(format!("bladebro-{name}.js"));
            std::fs::write(&path, &js).expect("write js fixture");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "{name} has a JS syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn key_combos_parse_and_reject_junk() {
        use super::input::{parse_key_combo, KeyCombo};
        let c = parse_key_combo("Control+a").unwrap().expect("combo");
        assert_eq!(
            c,
            KeyCombo {
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
                key: "a".into()
            }
        );
        let c = parse_key_combo("Meta+Enter").unwrap().expect("combo");
        assert!(c.meta && !c.ctrl && c.key == "Enter");
        let c = parse_key_combo("ctrl+shift+z").unwrap().expect("combo");
        assert!(c.ctrl && c.shift && c.key == "z");
        // Single characters (including '+' itself) are never chords.
        assert!(parse_key_combo("a").unwrap().is_none());
        assert!(parse_key_combo("+").unwrap().is_none());
        // Malformed chords are loud, not silently mis-dispatched.
        assert!(parse_key_combo("Control+").is_err());
        assert!(parse_key_combo("Foo+a").is_err());
    }

    #[test]
    fn combo_events_shape_native_chords() {
        use super::input::{combo_events, parse_key_combo};
        // Ctrl+A: control down, 'a' rawKeyDown (shortcuts carry no text),
        // 'a' up, control up - modifiers as a real keyboard reports them.
        let c = parse_key_combo("Control+a").unwrap().expect("combo");
        let (evs, main) = combo_events(&c).unwrap();
        assert_eq!(evs.len(), 4);
        assert_eq!(evs[0]["key"].as_str(), Some("Control"));
        assert_eq!(evs[0]["modifiers"].as_u64(), Some(2));
        assert_eq!(evs[main]["type"].as_str(), Some("rawKeyDown"));
        assert_eq!(evs[main]["key"].as_str(), Some("a"));
        assert_eq!(evs[main]["code"].as_str(), Some("KeyA"));
        assert_eq!(evs[main]["windowsVirtualKeyCode"].as_u64(), Some(65));
        assert_eq!(evs[main]["modifiers"].as_u64(), Some(2));
        assert!(evs[main].get("text").is_none());
        assert_eq!(evs[3]["key"].as_str(), Some("Control"));
        assert_eq!(evs[3]["modifiers"].as_u64(), Some(0));
        // Shift+A types the shifted glyph.
        let c = parse_key_combo("Shift+a").unwrap().expect("combo");
        let (evs, main) = combo_events(&c).unwrap();
        assert_eq!(evs[main]["type"].as_str(), Some("keyDown"));
        assert_eq!(evs[main]["key"].as_str(), Some("A"));
        assert_eq!(evs[main]["text"].as_str(), Some("A"));
        assert_eq!(evs[main]["modifiers"].as_u64(), Some(8));
        // Meta+Enter is a shortcut: no text, meta bit set.
        let c = parse_key_combo("Meta+Enter").unwrap().expect("combo");
        let (evs, main) = combo_events(&c).unwrap();
        assert_eq!(evs[main]["key"].as_str(), Some("Enter"));
        assert_eq!(evs[main]["type"].as_str(), Some("rawKeyDown"));
        assert!(evs[main].get("text").is_none());
        assert_eq!(evs[main]["modifiers"].as_u64(), Some(4));
    }

    #[test]
    fn verdicts_never_outclaim_the_readback() {
        use super::edit::{clear_verdict_text, type_verdict_text, EditKind, EditReport};
        let lpm = crate::page::LivePageModel::new();
        let base = EditReport {
            kind: EditKind::Type,
            text: "hi".into(),
            final_text: "hi".into(),
            branch_text: "hi".into(),
            host_kind: "ce".into(),
            host_is_tgt: false,
            tgt_missing: false,
            pre_text_len: 4,
            pre_cleared: true,
            verified: true,
            set_via_js: false,
            corrected: false,
        };
        // Clean replace on a framework editor: honest value + where it landed.
        let v = type_verdict_text("e4", "hi", &base, &lpm);
        assert!(v.contains("value=\"hi\""), "{v}");
        assert!(v.contains("replaced 4 chars"), "{v}");
        assert!(v.contains("landed in the focused editor"), "{v}");
        // Unverified: says so, never claims a value.
        let rep = EditReport {
            final_text: String::new(),
            verified: false,
            ..base.clone()
        };
        let v = type_verdict_text("e4", "hi", &rep, &lpm);
        assert!(v.contains("unverified"), "{v}");
        assert!(!v.contains("value=\"hi\""), "{v}");
        // Late restore caught by the final readback.
        let rep = EditReport {
            final_text: "DRAFT hi".into(),
            verified: false,
            ..base.clone()
        };
        let v = type_verdict_text("e4", "hi", &rep, &lpm);
        assert!(v.contains("DRAFT hi"), "{v}");
        assert!(v.contains("late restore"), "{v}");
        // A clear that failed never reads "cleared".
        let rep = EditReport {
            kind: EditKind::Clear,
            text: String::new(),
            final_text: "abc".into(),
            branch_text: "abc".into(),
            host_is_tgt: true,
            pre_text_len: 0,
            pre_cleared: false,
            verified: false,
            ..base.clone()
        };
        let v = clear_verdict_text("e3", &rep);
        assert!(v.contains("clear failed"), "{v}");
        assert!(!v.contains("cleared e3"), "{v}");
        // Verified clear.
        let rep = EditReport {
            final_text: String::new(),
            branch_text: String::new(),
            verified: true,
            ..base.clone()
        };
        let v = clear_verdict_text("e3", &rep);
        assert!(v.contains("cleared e3 (verified empty)"), "{v}");
    }
}

#[cfg(test)]
mod pause_gate_tests {
    use super::*;

    #[test]
    fn pause_refuses_input_and_page_moves_but_not_reads() {
        // Input dispatch.
        assert!(Action::Click {
            ref_id: "e1".into()
        }
        .disrupts_page());
        assert!(Action::ClickCoord { x: 1.0, y: 2.0 }.disrupts_page());
        assert!(Action::Type {
            ref_id: "e1".into(),
            text: "x".into()
        }
        .disrupts_page());
        assert!(Action::Upload {
            ref_id: "e1".into(),
            path: "/tmp/x".into()
        }
        .disrupts_page());
        // Page-level moves (history/reload) — they replace what the person
        // using the browser is looking at.
        assert!(Action::Back.disrupts_page());
        assert!(Action::Forward.disrupts_page());
        assert!(Action::Reload.disrupts_page());
        // Reads and waits stay available while paused.
        assert!(!Action::Read {
            ref_id: "e1".into()
        }
        .disrupts_page());
        assert!(!Action::Wait {
            condition: "settle".into(),
            text: String::new(),
            timeout: std::time::Duration::from_secs(1),
        }
        .disrupts_page());
        // `injects_input` stays exactly the input set (no navigation).
        assert!(!Action::Back.injects_input());
        assert!(!Action::Reload.injects_input());
    }
}
pub(crate) use self::input::dispatch_mouse_click;
