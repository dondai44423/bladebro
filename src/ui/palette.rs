//! Noir Syndicate ("Mafia Don") terminal color engine.
//!
//! Palette mirrors Bladebro's README identity: obsidian darks, signature
//! crimson (`#D52932`), antique gold (`#D4AF37`), champagne letterpress cream
//! (`#F2E8D5`), emerald velvet (`#2ECC71`), cigar amber (`#E5A93C`), and smoke
//! (`#8E929F`). Automatically negotiates TrueColor → 256-color → 16-color →
//! Plain per stream so every terminal renders cleanly and every pipe stays
//! byte-clean.

use std::sync::OnceLock;

/// Color capability tier negotiated for a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorTier {
    Plain,
    Ansi16,
    Ansi256,
    TrueColor,
}

fn env_lower(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Detect the terminal's color depth when styling is enabled.
///
/// Pure-ish check over standard terminal environment hints. Note: a plain
/// `TERM=xterm` with no `COLORTERM` or modern terminal markers stays `Ansi16`
/// so legacy terminals (and tests asserting standard `\x1b[31m`) get classic
/// SGR sequences.
pub(crate) fn detect_tier(enabled: bool) -> ColorTier {
    if !enabled {
        return ColorTier::Plain;
    }
    let colorterm = env_lower("COLORTERM");
    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorTier::TrueColor;
    }
    // Known 24-bit capable terminal emulators.
    if env_nonempty("WT_SESSION")
        || env_nonempty("KITTY_WINDOW_ID")
        || env_nonempty("WEZTERM_EXECUTABLE")
        || env_nonempty("ALACRITTY_SOCKET")
        || env_nonempty("GHOSTTY_RESOURCES_DIR")
    {
        return ColorTier::TrueColor;
    }
    let term_program = env_lower("TERM_PROGRAM");
    if matches!(
        term_program.as_str(),
        "iterm.app" | "wezterm" | "vscode" | "hyper" | "warpterminal" | "rio" | "tabby" | "ghostty"
    ) {
        return ColorTier::TrueColor;
    }
    let term = env_lower("TERM");
    if term.contains("direct") || term.contains("truecolor") || term.contains("24bit") {
        return ColorTier::TrueColor;
    }
    if term.contains("256color") || term_program == "apple_terminal" {
        return ColorTier::Ansi256;
    }
    ColorTier::Ansi16
}

/// Negotiated color tier for stdout.
pub fn stdout_tier() -> ColorTier {
    static TIER: OnceLock<ColorTier> = OnceLock::new();
    *TIER.get_or_init(|| {
        let on = super::enabled();
        if on {
            crate::platform::enable_virtual_terminal();
        }
        detect_tier(on)
    })
}

/// Negotiated color tier for stderr.
pub fn stderr_tier() -> ColorTier {
    static TIER: OnceLock<ColorTier> = OnceLock::new();
    *TIER.get_or_init(|| {
        let on = super::err_enabled();
        if on {
            crate::platform::enable_virtual_terminal();
        }
        detect_tier(on)
    })
}

/// Paint `s` using exact codes for each tier.
pub(crate) fn paint_tier(
    tier: ColorTier,
    rgb: (u8, u8, u8),
    bold: bool,
    c256: u8,
    c16: &str,
    s: &str,
) -> String {
    match tier {
        ColorTier::Plain => s.to_string(),
        ColorTier::Ansi16 => {
            if bold && !c16.starts_with("1;") && c16 != "1" {
                format!("\x1b[1;{c16}m{s}\x1b[0m")
            } else {
                format!("\x1b[{c16}m{s}\x1b[0m")
            }
        }
        ColorTier::Ansi256 => {
            if bold {
                format!("\x1b[1;38;5;{c256}m{s}\x1b[0m")
            } else {
                format!("\x1b[38;5;{c256}m{s}\x1b[0m")
            }
        }
        ColorTier::TrueColor => {
            let (r, g, b) = rgb;
            if bold {
                format!("\x1b[1;38;2;{r};{g};{b}m{s}\x1b[0m")
            } else {
                format!("\x1b[38;2;{r};{g};{b}m{s}\x1b[0m")
            }
        }
    }
}

/// Paint a pill badge with subtle dark background on TrueColor/256 terminals.
pub(crate) fn paint_pill(
    tier: ColorTier,
    fg_rgb: (u8, u8, u8),
    bg_rgb: (u8, u8, u8),
    fg256: u8,
    bg256: u8,
    c16: &str,
    label: &str,
) -> String {
    match tier {
        ColorTier::Plain => format!("[{label}]"),
        ColorTier::Ansi16 => format!("\x1b[1;{c16}m[{label}]\x1b[0m"),
        ColorTier::Ansi256 => {
            format!("\x1b[1;38;5;{fg256};48;5;{bg256}m {label} \x1b[0m")
        }
        ColorTier::TrueColor => {
            let (fr, fg, fb) = fg_rgb;
            let (br, bg, bb) = bg_rgb;
            format!("\x1b[1;38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m {label} \x1b[0m")
        }
    }
}

// ── Noir Syndicate Semantic Painters (stdout) ───────────────────────────

/// Antique Don Gold (`#D4AF37`).
pub fn gold(s: &str) -> String {
    paint_tier(stdout_tier(), (212, 175, 55), false, 178, "33", s)
}

/// Polished Bold Gold (`#ECC95B`).
pub fn bold_gold(s: &str) -> String {
    paint_tier(stdout_tier(), (236, 201, 91), true, 220, "1;33", s)
}

/// Bladebro Signature Crimson (`#D52932`).
pub fn crimson(s: &str) -> String {
    paint_tier(stdout_tier(), (213, 41, 50), false, 160, "31", s)
}

/// Bright Wax-Seal Crimson (`#EA3C48`).
pub fn bold_crimson(s: &str) -> String {
    paint_tier(stdout_tier(), (234, 60, 72), true, 196, "1;31", s)
}

/// Letterpress Champagne Cream (`#F2E8D5`).
pub fn cream(s: &str) -> String {
    paint_tier(stdout_tier(), (242, 232, 213), false, 230, "37", s)
}

/// Bold Champagne Cream (`#FBF4E4`).
pub fn bold_cream(s: &str) -> String {
    paint_tier(stdout_tier(), (251, 244, 228), true, 230, "1;37", s)
}

/// Velvet Emerald (`#2ECC71`).
pub fn emerald(s: &str) -> String {
    paint_tier(stdout_tier(), (46, 204, 113), false, 78, "32", s)
}

/// Bold Velvet Emerald (`#45E087`).
pub fn bold_emerald(s: &str) -> String {
    paint_tier(stdout_tier(), (69, 224, 135), true, 78, "1;32", s)
}

/// Cigar Amber (`#E5A93C`).
pub fn amber(s: &str) -> String {
    paint_tier(stdout_tier(), (229, 169, 60), false, 214, "33", s)
}

/// Cigar Smoke (`#8E929F`) — metadata, secondary hints, subtle paths.
pub fn smoke(s: &str) -> String {
    paint_tier(stdout_tier(), (142, 146, 159), false, 245, "2", s)
}

/// Deep Obsidian Slate (`#505466`) — box borders and unfilled bar tracks.
pub fn obsidian(s: &str) -> String {
    paint_tier(stdout_tier(), (80, 84, 102), false, 240, "2", s)
}

// ── Noir Badges / Pills ─────────────────────────────────────────────────

pub fn pill_pass(label: &str) -> String {
    paint_pill(
        stdout_tier(),
        (74, 222, 128),
        (16, 44, 28),
        78,
        22,
        "32",
        label,
    )
}

pub fn pill_warn(label: &str) -> String {
    paint_pill(
        stdout_tier(),
        (246, 193, 73),
        (48, 35, 11),
        214,
        58,
        "33",
        label,
    )
}

pub fn pill_fail(label: &str) -> String {
    paint_pill(
        stdout_tier(),
        (248, 92, 100),
        (54, 14, 18),
        203,
        52,
        "31",
        label,
    )
}

pub fn pill_gold(label: &str) -> String {
    paint_pill(
        stdout_tier(),
        (245, 212, 105),
        (45, 36, 14),
        220,
        58,
        "33",
        label,
    )
}

pub fn pill_smoke(label: &str) -> String {
    paint_pill(
        stdout_tier(),
        (180, 184, 198),
        (34, 36, 44),
        250,
        236,
        "2",
        label,
    )
}

// ── Gradient & Shimmer Effects ──────────────────────────────────────────

/// Interpolate a smooth gold → burnished-amber → signature-crimson gradient
/// along `t` in `[0.0, 1.0]`.
pub(crate) fn gradient_rgb(t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    // Gold (#E5C158) → Warm Amber (#DF7A2D) → Signature Crimson (#D52932)
    let (r, g, b) = if t < 0.55 {
        let u = t / 0.55;
        (
            229.0 + (223.0 - 229.0) * u,
            193.0 + (122.0 - 193.0) * u,
            88.0 + (45.0 - 88.0) * u,
        )
    } else {
        let u = (t - 0.55) / 0.45;
        (
            223.0 + (213.0 - 223.0) * u,
            122.0 + (41.0 - 122.0) * u,
            45.0 + (50.0 - 45.0) * u,
        )
    };
    (r.round() as u8, g.round() as u8, b.round() as u8)
}

/// Paint a horizontal string with a gold → crimson gradient across its characters.
pub fn gradient_rule(s: &str) -> String {
    let tier = stdout_tier();
    match tier {
        ColorTier::Plain => s.to_string(),
        ColorTier::Ansi16 => format!("\x1b[33m{s}\x1b[0m"),
        ColorTier::Ansi256 => {
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len().max(1);
            let mut out = String::with_capacity(s.len() * 14);
            for (i, ch) in chars.iter().enumerate() {
                let code = if i * 3 < n {
                    178
                } else if i * 3 < n * 2 {
                    172
                } else {
                    160
                };
                out.push_str(&format!("\x1b[38;5;{code}m{ch}"));
            }
            out.push_str("\x1b[0m");
            out
        }
        ColorTier::TrueColor => {
            let chars: Vec<char> = s.chars().collect();
            let denom = (chars.len().saturating_sub(1)).max(1) as f64;
            let mut out = String::with_capacity(s.len() * 20);
            for (i, ch) in chars.iter().enumerate() {
                let (r, g, b) = gradient_rgb(i as f64 / denom);
                out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{ch}"));
            }
            out.push_str("\x1b[0m");
            out
        }
    }
}

/// Paint text with a sweeping champagne-gold specular highlight centered at
/// `center` (in character index coordinates). Used for live spinner labels
/// and the masthead reveal sweep.
pub(crate) fn shimmer_gold(text: &str, center: f64, bold: bool) -> String {
    let tier = stdout_tier();
    match tier {
        ColorTier::Plain => text.to_string(),
        ColorTier::Ansi16 => paint_tier(tier, (212, 175, 55), bold, 178, "33", text),
        ColorTier::Ansi256 => {
            let mut out = String::with_capacity(text.len() * 16);
            for (i, ch) in text.chars().enumerate() {
                let d = (i as f64 - center).abs();
                let c = if d < 1.6 {
                    230
                } else if d < 3.6 {
                    221
                } else {
                    178
                };
                if bold {
                    out.push_str(&format!("\x1b[1;38;5;{c}m{ch}"));
                } else {
                    out.push_str(&format!("\x1b[38;5;{c}m{ch}"));
                }
            }
            out.push_str("\x1b[0m");
            out
        }
        ColorTier::TrueColor => {
            let mut out = String::with_capacity(text.len() * 24);
            for (i, ch) in text.chars().enumerate() {
                let d = (i as f64 - center).abs();
                let glow = (1.0 - (d / 4.2)).clamp(0.0, 1.0);
                // Base antique gold (205, 166, 56) → luminous champagne (255, 248, 224)
                let r = (205.0 + 50.0 * glow).round() as u8;
                let g = (166.0 + 82.0 * glow).round() as u8;
                let b = (56.0 + 168.0 * glow).round() as u8;
                if bold {
                    out.push_str(&format!("\x1b[1;38;2;{r};{g};{b}m{ch}"));
                } else {
                    out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{ch}"));
                }
            }
            out.push_str("\x1b[0m");
            out
        }
    }
}

/// Paint text with a sweeping cream/silver highlight over warm cream text
/// for animated spinner status lines.
pub(crate) fn shimmer_cream(text: &str, center: f64) -> String {
    let tier = stdout_tier();
    match tier {
        ColorTier::Plain => text.to_string(),
        ColorTier::Ansi16 | ColorTier::Ansi256 => cream(text),
        ColorTier::TrueColor => {
            let mut out = String::with_capacity(text.len() * 24);
            for (i, ch) in text.chars().enumerate() {
                let d = (i as f64 - center).abs();
                let glow = (1.0 - (d / 3.8)).clamp(0.0, 1.0);
                // Warm cream (215, 206, 188) → bright gold-white (252, 232, 162)
                let r = (215.0 + 37.0 * glow).round() as u8;
                let g = (206.0 + 26.0 * glow).round() as u8;
                let b = (188.0 - 26.0 * glow).round() as u8;
                out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{ch}"));
            }
            out.push_str("\x1b[0m");
            out
        }
    }
}

// ── Display Width / ANSI Stripping ──────────────────────────────────────

/// Strip ANSI CSI SGR escape sequences (`\x1b[...m`).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                if ('@'..='~').contains(&nc) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Approximate terminal column width of a single Unicode scalar (ignoring
/// combining marks and handling East Asian wide ranges).
fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7F..0xA0).contains(&u) {
        return 0;
    }
    // Combining diacritical marks
    if (0x0300..=0x036F).contains(&u)
        || (0x1AB0..=0x1AFF).contains(&u)
        || (0x1DC0..=0x1DFF).contains(&u)
        || (0x20D0..=0x20FF).contains(&u)
        || (0xFE20..=0xFE2F).contains(&u)
    {
        return 0;
    }
    // Common wide CJK / fullwidth / emoji blocks
    if (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE10..=0xFE6F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u)
    {
        return 2;
    }
    1
}

/// Visible terminal column width of a possibly ANSI-styled string.
pub fn visible_width(s: &str) -> usize {
    strip_ansi(s).chars().map(char_width).sum()
}

/// Truncate plain text to at most `max_cols` terminal columns, appending `…`
/// when clipped.
pub fn truncate_cols(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if visible_width(s) <= max_cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    let budget = max_cols.saturating_sub(1);
    for c in s.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}
