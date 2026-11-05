//! Humanized input dispatch: mouse (Bezier + gaussian offset via biometrics)
//! and keyboard (chords, per-char cadence, native key events).

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::stealth::biometrics::gaussian;

/// Dispatch a human-like mouse move from a random start point to `target`.
/// Returns the final (click) position after bezier path + overshoot correction.
/// Shared by click (adds press/release after) and hover (no click).
pub(super) async fn dispatch_mouse_move(
    cdp: &CdpSession,
    target: (f64, f64),
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<(f64, f64)> {
    dispatch_mouse_move_bounded(cdp, target, last_mouse, None).await
}

async fn dispatch_mouse_move_bounded(
    cdp: &CdpSession,
    target: (f64, f64),
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
    bounds: Option<[f64; 4]>,
) -> Result<(f64, f64)> {
    let mut rng = crate::stealth::Rng::new();
    // Random start point — offset from the target by 100-300px.
    // If we have a last position, start from there instead (continuity).
    let start = {
        let lm = last_mouse.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((lx, ly)) = *lm {
            (lx, ly)
        } else {
            let angle = rng.uniform() * 2.0 * std::f64::consts::PI;
            let start_dist = 100.0 + rng.uniform() * 200.0;
            (
                target.0 + angle.cos() * start_dist,
                target.1 + angle.sin() * start_dist,
            )
        }
    };

    // Generate the bezier path with overshoot + gaussian click offset.
    let mut path = crate::stealth::mouse_path(start, target, &mut rng);
    if let Some(box_) = bounds {
        // Overshoot is natural; the correction/hesitation must land on the
        // addressed control. Unbounded Gaussian tails miss small checkboxes.
        for point in path.iter_mut().rev().take(2) {
            (point.x, point.y) = bounded_click_point((point.x, point.y), box_);
        }
    }

    // Move along the path, dispatching mouseMoved events with natural delays.
    // Include movementX/movementY deltas — PerimeterX/HUMAN tracks these
    // coordinate deltas as behavioral biometrics; missing or zero values
    // are an instant bot flag.
    // 5s timeout: if Chrome is stuck (e.g. new tab loading), don't hang 30s.
    let to = Duration::from_secs(5);
    for pt in &path {
        let (mx, my) = {
            let lm = last_mouse.lock().unwrap_or_else(|e| e.into_inner());
            lm.map(|(lx, ly)| ((pt.x - lx).round() as i64, (pt.y - ly).round() as i64))
                .unwrap_or((0, 0))
        };
        cdp.send_with_timeout(
            "Input.dispatchMouseEvent",
            Some(json!({
                "type": "mouseMoved",
                "x": pt.x, "y": pt.y,
                "movementX": mx, "movementY": my,
            })),
            to,
        )
        .await?;
        *last_mouse.lock().unwrap_or_else(|e| e.into_inner()) = Some((pt.x, pt.y));
        tokio::time::sleep(pt.delay).await;
    }

    // Return the final click point (after overshoot correction).
    Ok(crate::stealth::click_target(&path))
}

/// Dispatch a real mouse click at (x, y) via `Input.dispatchMouseEvent`,
/// with a human-like bezier mouse path from a random start point.
pub(crate) async fn dispatch_mouse_click(
    cdp: &CdpSession,
    x: f64,
    y: f64,
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<()> {
    dispatch_mouse_click_bounded(cdp, x, y, last_mouse, None).await
}

pub(super) async fn dispatch_mouse_click_in_box(
    cdp: &CdpSession,
    box_: [f64; 4],
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
) -> Result<()> {
    if box_.iter().any(|v| !v.is_finite()) || box_[2] <= 0.0 || box_[3] <= 0.0 {
        return Err(BladeError::NotInteractable(
            "click target has no positive area; refresh the target before clicking".into(),
        ));
    }
    dispatch_mouse_click_bounded(
        cdp,
        box_[0] + box_[2] / 2.0,
        box_[1] + box_[3] / 2.0,
        last_mouse,
        Some(box_),
    )
    .await
}

// A continuous bounded distribution preserves variation without clipping
// many samples onto one edge. Relative precision adapts to the control size.
fn bounded_click_point(point: (f64, f64), box_: [f64; 4]) -> (f64, f64) {
    let axis = |p: f64, origin: f64, size: f64| {
        let center = origin + size / 2.0;
        let radius = size / 4.0;
        center + radius * ((p - center) / radius).tanh()
    };
    (
        axis(point.0, box_[0], box_[2]),
        axis(point.1, box_[1], box_[3]),
    )
}

async fn dispatch_mouse_click_bounded(
    cdp: &CdpSession,
    x: f64,
    y: f64,
    last_mouse: &Arc<std::sync::Mutex<Option<(f64, f64)>>>,
    bounds: Option<[f64; 4]>,
) -> Result<()> {
    let (cx, cy) = dispatch_mouse_move_bounded(cdp, (x, y), last_mouse, bounds).await?;

    let mut rng = crate::stealth::Rng::new();
    let to = Duration::from_secs(5);
    // Micro-tremors: 1-3 tiny jitter points (1-3px) before clicking.
    // Real humans have involuntary hand tremors even when "holding still".
    // A perfectly stationary cursor before a click is a bot signal.
    let tremor_count = rng.range(1, 3) as usize;
    for _ in 0..tremor_count {
        let jx = gaussian(&mut rng, 0.0, 1.5).clamp(-3.0, 3.0);
        let jy = gaussian(&mut rng, 0.0, 1.5).clamp(-3.0, 3.0);
        let (tx, ty) = bounds
            .map(|box_| bounded_click_point((cx + jx, cy + jy), box_))
            .unwrap_or((cx + jx, cy + jy));
        let (mx, my) = {
            let lm = last_mouse.lock().unwrap_or_else(|e| e.into_inner());
            lm.map(|(lx, ly)| ((tx - lx).round() as i64, (ty - ly).round() as i64))
                .unwrap_or((0, 0))
        };
        cdp.send_with_timeout(
            "Input.dispatchMouseEvent",
            Some(json!({
                "type": "mouseMoved",
                "x": tx, "y": ty,
                "movementX": mx, "movementY": my,
            })),
            to,
        )
        .await?;
        *last_mouse.lock().unwrap_or_else(|e| e.into_inner()) = Some((tx, ty));
        tokio::time::sleep(Duration::from_millis(rng.range(8, 24) as u64)).await;
    }
    // Press + release at the final position.
    cdp.send_with_timeout(
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mousePressed", "x": cx, "y": cy, "button": "left", "clickCount": 1,
            "movementX": 0, "movementY": 0,
        })),
        to,
    )
    .await?;
    // Small delay between press and release (28-70ms).
    tokio::time::sleep(Duration::from_millis(28 + rng.range(0, 42) as u64)).await;
    cdp.send_with_timeout(
        "Input.dispatchMouseEvent",
        Some(json!({
            "type": "mouseReleased", "x": cx, "y": cy, "button": "left", "clickCount": 1,
            "movementX": 0, "movementY": 0,
        })),
        to,
    )
    .await?;
    *last_mouse.lock().unwrap_or_else(|e| e.into_inner()) = Some((cx, cy));
    Ok(())
}

#[cfg(test)]
mod click_bounds_tests {
    use super::*;

    #[test]
    fn extreme_offsets_stay_inside_tiny_fractional_and_regular_targets() {
        for box_ in [
            [100.0, 100.0, 2.0, 2.0],
            [200.75, 100.75, 0.5, 0.5],
            [0.0, 0.0, 13.0, 13.0],
            [50.0, 80.0, 200.0, 40.0],
        ] {
            let center = (box_[0] + box_[2] / 2.0, box_[1] + box_[3] / 2.0);
            assert_eq!(bounded_click_point(center, box_), center);
            for offset in [-1e6, -20.0, -5.0, 0.5, 5.0, 20.0, 1e6] {
                let (x, y) = bounded_click_point((center.0 + offset, center.1 - offset), box_);
                assert!(
                    x > box_[0] && x < box_[0] + box_[2] && y > box_[1] && y < box_[1] + box_[3],
                    "{box_:?}: {x},{y}"
                );
            }
            assert_ne!(
                bounded_click_point((center.0 + 0.5, center.1 + 0.5), box_),
                center
            );
        }
    }
}

/// A parsed key chord ("Control+a", "Meta+Enter", "Shift+Tab").
#[derive(Debug, Clone, PartialEq)]
pub(super) struct KeyCombo {
    pub(super) ctrl: bool,
    pub(super) alt: bool,
    pub(super) shift: bool,
    pub(super) meta: bool,
    pub(super) key: String,
}

/// Parse chord syntax. `Ok(None)` when the input is not a chord (no '+' at
/// all); `Err` for malformed chords so the caller can name the problem.
pub(super) fn parse_key_combo(input: &str) -> std::result::Result<Option<KeyCombo>, String> {
    // A single character (including '+' itself) is never chord syntax.
    if input.chars().count() <= 1 || !input.contains('+') {
        return Ok(None);
    }
    let parts: Vec<&str> = input.split('+').collect();
    let mut combo = KeyCombo {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
        key: String::new(),
    };
    for (i, raw) in parts.iter().enumerate() {
        let p = raw.trim();
        if p.is_empty() {
            return Err(format!("invalid key combo '{input}'"));
        }
        if i == parts.len() - 1 {
            combo.key = p.to_string();
        } else {
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => combo.ctrl = true,
                "alt" => combo.alt = true,
                "shift" => combo.shift = true,
                "meta" | "cmd" | "command" | "super" | "win" => combo.meta = true,
                other => return Err(format!("unsupported modifier '{other}' in '{input}'")),
            }
        }
    }
    Ok(Some(combo))
}

/// Named keys (case-insensitive) -> (key name, code, windowsVirtualKeyCode).
pub(super) fn named_key(key: &str) -> Option<(&'static str, &'static str, u32)> {
    match key.to_ascii_lowercase().as_str() {
        "enter" => Some(("Enter", "Enter", 13)),
        "tab" => Some(("Tab", "Tab", 9)),
        "escape" | "esc" => Some(("Escape", "Escape", 27)),
        "backspace" => Some(("Backspace", "Backspace", 8)),
        "delete" => Some(("Delete", "Delete", 46)),
        "arrowdown" | "down" => Some(("ArrowDown", "ArrowDown", 40)),
        "arrowup" | "up" => Some(("ArrowUp", "ArrowUp", 38)),
        "arrowleft" | "left" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "arrowright" | "right" => Some(("ArrowRight", "ArrowRight", 39)),
        "home" => Some(("Home", "Home", 36)),
        "end" => Some(("End", "End", 35)),
        "pagedown" => Some(("PageDown", "PageDown", 34)),
        "pageup" => Some(("PageUp", "PageUp", 33)),
        "space" | " " => Some((" ", "Space", 32)),
        _ => None,
    }
}

/// Build one CDP key event.
pub(super) fn key_event(
    kind: &str,
    key: &str,
    code: &str,
    vk: u32,
    modifiers: u8,
    text: Option<&str>,
) -> serde_json::Value {
    let mut ev = json!({
        "type": kind,
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": vk,
        "modifiers": modifiers,
    });
    if let Some(t) = text {
        ev["text"] = json!(t);
    }
    ev
}

/// Resolve a chord's main key to (key, code, vk, text).
pub(super) fn resolve_combo_key(
    combo: &KeyCombo,
) -> std::result::Result<(String, String, u32, Option<String>), String> {
    let key = combo.key.as_str();
    if key.chars().count() == 1 {
        let mut ch = key.chars().next().unwrap();
        // Shift over a letter reports the shifted glyph, as real keyboards do.
        if combo.shift && ch.is_ascii_alphabetic() {
            ch = ch.to_ascii_uppercase();
        }
        let (code, vk, _) = char_to_key_code(ch);
        if code == "Unidentified" {
            return Err(format!("unsupported key '{key}' in combo"));
        }
        Ok((ch.to_string(), code, vk, Some(ch.to_string())))
    } else {
        let (kname, code, vk) =
            named_key(key).ok_or_else(|| format!("unsupported key '{key}' in combo"))?;
        let text = match kname {
            "Enter" => Some("\r".to_string()),
            "Tab" => Some("\t".to_string()),
            " " => Some(" ".to_string()),
            _ => None,
        };
        Ok((kname.to_string(), code.to_string(), vk, text))
    }
}

/// The CDP event sequence for a chord: modifier downs, main down, main up,
/// modifier ups (each release drops its own bit, in reverse order). Returns
/// the events and the index of the main keydown so the dispatcher can hold
/// the key like the plain path does.
pub(super) fn combo_events(
    combo: &KeyCombo,
) -> std::result::Result<(Vec<serde_json::Value>, usize), String> {
    let (kname, kcode, kvk, ktext) = resolve_combo_key(combo)?;
    let mods = [
        ("Control", "ControlLeft", 17u32, 2u8, combo.ctrl),
        ("Alt", "AltLeft", 18, 1, combo.alt),
        ("Shift", "ShiftLeft", 16, 8, combo.shift),
        ("Meta", "MetaLeft", 91, 4, combo.meta),
    ];
    let mut bits: u8 = 0;
    let mut evs: Vec<serde_json::Value> = Vec::new();
    for (name, code, vk, bit, on) in mods {
        if on {
            bits |= bit;
            evs.push(key_event("keyDown", name, code, vk, bits, None));
        }
    }
    // Ctrl/Alt/Meta chords are shortcuts, never text; Shift keeps the glyph.
    let text = if combo.ctrl || combo.alt || combo.meta {
        None
    } else {
        ktext
    };
    let kind = if text.is_some() {
        "keyDown"
    } else {
        "rawKeyDown"
    };
    let main_idx = evs.len();
    evs.push(key_event(kind, &kname, &kcode, kvk, bits, text.as_deref()));
    evs.push(key_event("keyUp", &kname, &kcode, kvk, bits, None));
    for (name, code, vk, bit, on) in mods.into_iter().rev() {
        if on {
            bits &= !bit;
            evs.push(key_event("keyUp", name, code, vk, bits, None));
        }
    }
    Ok((evs, main_idx))
}

// CDP cannot synthesize macOS's native editing selector from Meta+A alone.
// Carry Chrome's editing command on that keydown; canceled key events still
// suppress it, and unrelated chords keep their ordinary event semantics.
fn shortcut_commands(combo: &KeyCombo, macos: bool) -> Option<Vec<String>> {
    (macos
        && combo.meta
        && !combo.ctrl
        && !combo.alt
        && !combo.shift
        && combo.key.eq_ignore_ascii_case("a"))
    .then(|| vec!["selectAll".to_string()])
}

/// Dispatch a key press (keyDown + keyUp) via `Input.dispatchKeyEvent`.
/// Accepts single characters, named keys, and chords ("Control+a").
pub(super) async fn dispatch_key(cdp: &CdpSession, key: &str) -> Result<()> {
    if let Some(combo) = parse_key_combo(key).map_err(BladeError::Other)? {
        let commands = shortcut_commands(&combo, cfg!(target_os = "macos"));
        return dispatch_combo(cdp, &combo, commands).await;
    }
    // Printable single character: full char event (code + VK + text).
    // `act press key="a"` used to dispatch key:"a" code:"Unidentified"
    // vk:0 - a synthetic-looking no-op no page would act on.
    if key.chars().count() == 1 {
        let ch = key.chars().next().unwrap();
        let (code, vk, shift) = char_to_key_code(ch);
        let modifiers: u8 = if shift { 8 } else { 0 };
        if shift {
            cdp.send(
                "Input.dispatchKeyEvent",
                Some(json!({
                    "type": "keyDown", "key": "Shift", "code": "ShiftLeft",
                    "windowsVirtualKeyCode": 16, "modifiers": 8,
                })),
            )
            .await?;
        }
        cdp.send(
            "Input.dispatchKeyEvent",
            Some(json!({
                "type": "keyDown", "key": key, "code": code,
                "windowsVirtualKeyCode": vk, "text": key, "modifiers": modifiers,
                "keyChar": key,
            })),
        )
        .await?;
        // Key press duration: 25-70ms - real humans hold before releasing.
        let mut rng = crate::stealth::Rng::new();
        tokio::time::sleep(Duration::from_millis(25 + rng.range(0, 45) as u64)).await;
        cdp.send(
            "Input.dispatchKeyEvent",
            Some(json!({
                "type": "keyUp", "key": key, "code": code,
                "windowsVirtualKeyCode": vk, "modifiers": modifiers,
            })),
        )
        .await?;
        if shift {
            cdp.send(
                "Input.dispatchKeyEvent",
                Some(json!({
                    "type": "keyUp", "key": "Shift", "code": "ShiftLeft",
                    "windowsVirtualKeyCode": 16, "modifiers": 0,
                })),
            )
            .await?;
        }
        return Ok(());
    }

    let (kname, code, vk) = named_key(key).ok_or_else(|| BladeError::Other(format!(
        "unsupported key: '{key}'. Supported: single characters, named keys (Enter, Tab, Escape, Backspace, Delete, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, Space), and chords (Control+a, Meta+Enter, Shift+Tab)"
    )))?;
    // The `text` field is critical for keys that produce text - without it,
    // the browser fires keydown but doesn't process the default action
    // (e.g. Enter won't submit forms, Space won't scroll/click).
    let text = match kname {
        "Enter" => Some("\r"),
        "Tab" => Some("\t"),
        " " => Some(" "),
        _ => None,
    };
    let mut key_down = json!({
        "type": "keyDown",
        "key": kname,
        "code": code,
        "windowsVirtualKeyCode": vk,
    });
    if let Some(t) = text {
        key_down["text"] = json!(t);
    }
    let key_up = json!({
        "type": "keyUp",
        "key": kname,
        "code": code,
        "windowsVirtualKeyCode": vk,
    });
    cdp.send("Input.dispatchKeyEvent", Some(key_down)).await?;
    // Key press duration: 40-110ms - real humans hold before releasing.
    let mut rng = crate::stealth::Rng::new();
    tokio::time::sleep(Duration::from_millis(40 + rng.range(0, 70) as u64)).await;
    cdp.send("Input.dispatchKeyEvent", Some(key_up)).await?;
    Ok(())
}

/// Dispatch a parsed chord, optionally attaching Chrome editing commands
/// (e.g. ["selectAll"]) to the main keydown - the deterministic fallback
/// when a framework swallows the plain shortcut.
pub(super) async fn dispatch_combo(
    cdp: &CdpSession,
    combo: &KeyCombo,
    commands: Option<Vec<String>>,
) -> Result<()> {
    let (mut evs, main_idx) = combo_events(combo).map_err(BladeError::Other)?;
    if let Some(cmds) = commands {
        if let Some(obj) = evs.get_mut(main_idx).and_then(|v| v.as_object_mut()) {
            obj.insert("commands".to_string(), json!(cmds));
        }
    }
    for (i, ev) in evs.into_iter().enumerate() {
        cdp.send("Input.dispatchKeyEvent", Some(ev)).await?;
        if i == main_idx {
            // Hold the key like real fingers do (matches the plain path).
            let mut rng = crate::stealth::Rng::new();
            tokio::time::sleep(Duration::from_millis(25 + rng.range(0, 45) as u64)).await;
        }
    }
    Ok(())
}

/// Type text with per-character key events and human cadence.
/// Returns false when a dispatch fails (caller falls back to insertText).
///
/// v3.9 realism (M6): the inter-key cadence sleep happens BETWEEN keyDown
/// and keyUp — the key is HELD for the interval, as real fingers do (the
/// old code slept after keyUp: hold time was a constant ~1-5ms). Uppercase
/// and shifted symbols get a real Shift keyDown/up with modifiers:8.
/// Non-ASCII chars dispatch with VK 229 (IME), matching real IME input.
pub(super) async fn type_per_char(cdp: &CdpSession, text: &str) -> bool {
    let mut rng = crate::stealth::Rng::new();
    let cadence = crate::stealth::typing_cadence(text, &mut rng);
    for (i, ch) in text.chars().enumerate() {
        let ch_str = ch.to_string();
        let (code, vk, shift) = char_to_key_code(ch);
        let modifiers: u8 = if shift { 8 } else { 0 };
        if shift
            && cdp
                .send(
                    "Input.dispatchKeyEvent",
                    Some(json!({
                        "type": "keyDown", "key": "Shift", "code": "ShiftLeft",
                        "windowsVirtualKeyCode": 16, "modifiers": 8,
                    })),
                )
                .await
                .is_err()
        {
            return false;
        }
        let mut down = json!({
            "type": "keyDown",
            "key": ch_str,
            "code": code,
            "windowsVirtualKeyCode": vk,
            "modifiers": modifiers,
        });
        // `text` produces the char + keypress; non-ASCII uses IME VK 229
        // without text (the input event comes from the IME commit path).
        if ch.is_ascii() {
            down["text"] = json!(ch_str);
        }
        if cdp
            .send("Input.dispatchKeyEvent", Some(down))
            .await
            .is_err()
        {
            return false;
        }
        // HOLD the key for the inter-key interval.
        if i < cadence.len() {
            tokio::time::sleep(cadence[i]).await;
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if cdp
            .send(
                "Input.dispatchKeyEvent",
                Some(json!({
                    "type": "keyUp",
                    "key": ch_str,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "modifiers": modifiers,
                })),
            )
            .await
            .is_err()
        {
            return false;
        }
        if shift
            && cdp
                .send(
                    "Input.dispatchKeyEvent",
                    Some(json!({
                        "type": "keyUp", "key": "Shift", "code": "ShiftLeft",
                        "windowsVirtualKeyCode": 16, "modifiers": 0,
                    })),
                )
                .await
                .is_err()
        {
            return false;
        }
    }
    true
}

/// Map a printable character to (code, windowsVirtualKeyCode, needsShift)
/// for CDP key events. Shifted symbols resolve to their BASE key + Shift
/// ('!' → Digit1 + Shift), exactly as a real keyboard reports them; the
/// old map gave symbols code:"Unidentified" and vk:<unicode codepoint> —
/// a synthetic signature no real keyboard produces. Non-ASCII returns
/// VK 229 (IME composition), matching real IME input.
pub(super) fn char_to_key_code(ch: char) -> (String, u32, bool) {
    if !ch.is_ascii() {
        return ("Unidentified".to_string(), 229, false);
    }
    let shift = ch.is_uppercase() || "!@#$%^&*()_+{}|:\"<>?~".contains(ch);
    let base = ch.to_ascii_lowercase();
    let (code, vk): (String, u32) = match base {
        'a'..='z' => {
            let up = base.to_ascii_uppercase();
            (format!("Key{up}"), up as u32)
        }
        '0'..='9' => (format!("Digit{base}"), base as u32),
        ' ' => ("Space".to_string(), 32),
        '.' | '>' => ("Period".to_string(), 190),
        ',' | '<' => ("Comma".to_string(), 188),
        '-' | '_' => ("Minus".to_string(), 189),
        '=' | '+' => ("Equal".to_string(), 187),
        '/' | '?' => ("Slash".to_string(), 191),
        '\\' | '|' => ("Backslash".to_string(), 220),
        ';' | ':' => ("Semicolon".to_string(), 186),
        '\'' | '"' => ("Quote".to_string(), 222),
        '`' | '~' => ("Backquote".to_string(), 192),
        '[' | '{' => ("BracketLeft".to_string(), 219),
        ']' | '}' => ("BracketRight".to_string(), 221),
        '!' => ("Digit1".to_string(), 49),
        '@' => ("Digit2".to_string(), 50),
        '#' => ("Digit3".to_string(), 51),
        '$' => ("Digit4".to_string(), 52),
        '%' => ("Digit5".to_string(), 53),
        '^' => ("Digit6".to_string(), 54),
        '&' => ("Digit7".to_string(), 55),
        '*' => ("Digit8".to_string(), 56),
        '(' => ("Digit9".to_string(), 57),
        ')' => ("Digit0".to_string(), 48),
        _ => ("Unidentified".to_string(), 0),
    };
    (code, vk, shift)
}

#[cfg(test)]
mod mac_shortcut_tests {
    use super::*;
    #[test]
    fn macos_meta_a_requests_native_select_all_without_other_chords() {
        let select = parse_key_combo("Meta+a").unwrap().unwrap();
        assert_eq!(
            shortcut_commands(&select, true),
            Some(vec!["selectAll".into()])
        );
        assert_eq!(shortcut_commands(&select, false), None);
        for key in [
            "Control+a",
            "Meta+Shift+a",
            "Meta+Alt+a",
            "Meta+Enter",
            "Meta+z",
        ] {
            assert_eq!(
                shortcut_commands(&parse_key_combo(key).unwrap().unwrap(), true),
                None,
                "{key}"
            );
        }
    }
}
