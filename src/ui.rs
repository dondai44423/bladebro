//! Terminal presentation — the one place colors, animations, and Mafia Don
//! Noir Syndicate styling live.
//!
//! Module map:
//! - `palette`: TrueColor → 256-color → 16-color → Plain color engine, pills,
//!   gold→crimson gradients, shimmer sweeps, and visible-width helpers.
//! - `anim`: TTY-gated animated spinner, 8× sub-block gradient progress bar,
//!   and fast masthead shimmer reveal (with RAII cursor restoration).
//! - `render`: Responsive framed masthead, dossier sections, aligned status
//!   rows, sealed verdict cards, version identity card, and Noir help styling.
//!
//! Every helper degrades to byte-clean plain text when styling is off, so one
//! code path serves both an interactive TTY (styled, animated) and a pipe /
//! file / MCP stream (byte-exact plain). Styling is ON only when it cannot
//! corrupt a machine consumer — the order matters:
//!
//! 1. `BLADE_PLAIN` set (any non-empty value) → OFF. Our hard switch.
//! 2. `CLICOLOR_FORCE` set (non-empty, not `0`) → ON, even off-TTY (demos,
//!    recordings, docs).
//! 3. `NO_COLOR` set (the de-facto convention) → OFF.
//! 4. `TERM=dumb` → OFF.
//! 5. otherwise ON exactly when the stream being written is a terminal.
//!
//! stdout and stderr decide separately: text bound for stderr uses the
//! [`err`] painters. `bladebro nav x 2>err.log` on a terminal must not write
//! ANSI codes into the log, and `bladebro nav x | jq` still colors the error
//! lines that reach the terminal.

use std::io::IsTerminal;
use std::sync::OnceLock;

pub mod anim;
pub mod palette;
pub mod render;

pub use anim::{anim_enabled, format_bytes, format_elapsed, ProgressBar, Spinner};
pub use palette::{
    amber, bold_cream, bold_crimson, bold_emerald, bold_gold, cream, crimson, emerald, gold,
    gradient_rule, obsidian, pill_fail, pill_gold, pill_pass, pill_smoke, pill_warn, smoke,
    strip_ansi, visible_width, ColorTier,
};
pub use render::{
    dossier_card, dossier_row, frame_width, masthead, phase, section, style_help_noir,
    verdict_card, version_card, Tone, VersionCard,
};

fn env_set(name: &str) -> bool {
    std::env::var_os(name)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

fn env_on(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Pure decision — the environment facts in, the styling verdict out.
fn decide(tty: bool, plain: bool, forced: bool, no_color: bool, term_dumb: bool) -> bool {
    if plain {
        return false;
    }
    if forced {
        return true;
    }
    if no_color || term_dumb {
        return false;
    }
    tty
}

/// The verdict for a stream whose terminal state is `tty`.
fn decide_for(tty: bool) -> bool {
    decide(
        tty,
        env_set("BLADE_PLAIN"),
        env_on("CLICOLOR_FORCE"),
        env_set("NO_COLOR"),
        std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false),
    )
}

/// Is styled output enabled on stdout? Computed once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = decide_for(std::io::stdout().is_terminal());
        if on {
            crate::platform::enable_virtual_terminal();
        }
        on
    })
}

/// Is styled output enabled on stderr? Computed once, from stderr's OWN
/// terminal state — it used to follow stdout's.
pub fn err_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = decide_for(std::io::stderr().is_terminal());
        if on {
            crate::platform::enable_virtual_terminal();
        }
        on
    })
}

fn paint_c(sgr: &str, s: &str, color: bool) -> String {
    if color {
        format!("\x1b[{sgr}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn paint(sgr: &str, s: &str) -> String {
    paint_c(sgr, s, enabled())
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn green(s: &str) -> String {
    paint("32", s)
}

pub fn red(s: &str) -> String {
    paint("31", s)
}

pub fn yellow(s: &str) -> String {
    paint("33", s)
}

pub fn cyan(s: &str) -> String {
    paint("36", s)
}

pub fn bold_cyan(s: &str) -> String {
    paint("1;36", s)
}

/// Painters for text written to stderr (error lines, diagnostics): styled
/// only when stderr itself is a terminal.
pub mod err {
    use super::palette::{paint_tier, stderr_tier};

    pub fn dim(s: &str) -> String {
        super::paint_c("2", s, super::err_enabled())
    }

    pub fn red(s: &str) -> String {
        super::paint_c("31", s, super::err_enabled())
    }

    pub fn yellow(s: &str) -> String {
        super::paint_c("33", s, super::err_enabled())
    }

    pub fn gold(s: &str) -> String {
        paint_tier(stderr_tier(), (212, 175, 55), false, 178, "33", s)
    }

    pub fn bold_crimson(s: &str) -> String {
        paint_tier(stderr_tier(), (234, 60, 72), true, 196, "1;31", s)
    }

    pub fn cream(s: &str) -> String {
        paint_tier(stderr_tier(), (242, 232, 213), false, 230, "37", s)
    }
}

/// One aligned `label  value` row: the label is dim and padded on the PLAIN
/// text, so ANSI never breaks the column.
pub fn kv(label: &str, value: &str, width: usize) -> String {
    format!("  {}  {}", smoke(&format!("{label:<width$}")), value)
}

/// Gold-accented heading with a gradient rule under it (blocks of output).
pub fn header(title: &str) {
    println!();
    if enabled() {
        let rule = "━".repeat(title.chars().count().max(24));
        println!("  {}  {}", bold_crimson("◈"), bold_gold(title));
        println!("  {}", gradient_rule(&rule));
    } else {
        println!("  {title}");
        println!("  {}", "─".repeat(title.chars().count()));
    }
    println!();
}

/// Numbered step (`[n/total]`).
pub fn step(n: usize, total: usize, msg: &str) {
    phase(n, total, msg);
}

/// Info message (gold diamond `◆`).
pub fn info(msg: &str) {
    if enabled() {
        println!("  {}  {}", gold("◆"), cream(msg));
    } else {
        println!("  ℹ {msg}");
    }
}

/// Success message (emerald velvet `✓`).
pub fn success(msg: &str) {
    if enabled() {
        println!("  {}  {}", pill_pass("OK"), bold_emerald(msg));
    } else {
        println!("  ✓ {msg}");
    }
}

/// Warning message (amber cigar `⚠`).
pub fn warn(msg: &str) {
    if enabled() {
        println!("  {}  {}", pill_warn("WARN"), amber(msg));
    } else {
        println!("  ⚠ {msg}");
    }
}

/// Hint (smoke, indented with gold chevron).
pub fn hint(msg: &str) {
    if enabled() {
        println!("    {} {}", gold("›"), smoke(msg));
    } else {
        println!("    {msg}");
    }
}

/// Human report text: `✗ error:` lines red, `--- section ---` dividers dim.
/// Passthrough (byte-identical) when styling is off.
pub fn style_report(text: &str) -> String {
    style_report_impl(text, enabled())
}

fn style_report_impl(text: &str, color: bool) -> String {
    if !color {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 64);
    for seg in text.split_inclusive('\n') {
        let (line, nl) = match seg.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (seg, ""),
        };
        if line.starts_with("✗ ") {
            out.push_str(&paint_c("31", line, true));
        } else if line.starts_with("--- ") && line.ends_with(" ---") {
            out.push_str(&paint_c("2", line, true));
        } else {
            out.push_str(line);
        }
        out.push_str(nl);
    }
    out
}

/// Human help manual: Noir Syndicate themed when styling is on, byte-identical
/// passthrough when styling is off.
pub fn style_help(text: &str) -> String {
    style_help_noir(text)
}

#[cfg(test)]
fn style_help_impl(text: &str, color: bool) -> String {
    if !color {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 128);
    for seg in text.split_inclusive('\n') {
        let (line, nl) = match seg.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (seg, ""),
        };
        if is_section_header(line) {
            out.push_str(&paint_c("1;36", line, true));
        } else {
            out.push_str(line);
        }
        out.push_str(nl);
    }
    out
}

/// A manual header: column 0, at least three chars, only uppercase letters
/// and separators. Body lines are indented; prose has lowercase letters.
pub(crate) fn is_section_header(line: &str) -> bool {
    line.chars().count() >= 3
        && !line.starts_with(' ')
        && line.chars().any(|c| c.is_ascii_uppercase())
        && line
            .chars()
            .all(|c| c.is_ascii_uppercase() || matches!(c, ' ' | '-' | '/' | '&' | '(' | ')' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_respects_the_switches_in_order() {
        assert!(
            !decide(true, true, true, false, false),
            "BLADE_PLAIN is absolute"
        );
        assert!(
            decide(false, false, true, false, false),
            "CLICOLOR_FORCE wins off-TTY"
        );
        assert!(
            decide(false, false, true, true, false),
            "force beats NO_COLOR"
        );
        assert!(
            !decide(true, false, false, true, false),
            "NO_COLOR disables"
        );
        assert!(
            !decide(true, false, false, false, true),
            "TERM=dumb disables"
        );
        assert!(decide(true, false, false, false, false), "plain TTY is on");
        assert!(!decide(false, false, false, false, false), "a pipe is off");
    }

    #[test]
    fn report_styling_only_touches_errors_and_dividers() {
        let text = "outcome: navigated → x\n✗ error: boom\n--- content ---\nhello\n";
        assert_eq!(
            style_report_impl(text, false),
            text,
            "plain mode is byte-identical"
        );
        let styled = style_report_impl(text, true);
        assert!(
            styled.contains("\x1b[31m✗ error: boom\x1b[0m"),
            "error line red"
        );
        assert!(
            styled.contains("\x1b[2m--- content ---\x1b[0m"),
            "divider dim"
        );
        assert!(
            styled.contains("outcome: navigated → x\n"),
            "other lines untouched"
        );
        assert!(
            styled.ends_with("hello\n"),
            "trailing text + newline preserved"
        );
        assert_eq!(styled.matches('\n').count(), text.matches('\n').count());
    }

    #[test]
    fn help_styling_marks_only_headers() {
        let text = "bladebro v1 — manual\n\nUSAGE\n  bladebro x\n\nEXIT CODES\n  0  ok\n";
        let styled = style_help_impl(text, true);
        assert!(styled.contains("\x1b[1;36mUSAGE\x1b[0m"));
        assert!(styled.contains("\x1b[1;36mEXIT CODES\x1b[0m"));
        assert!(
            !styled.contains("\x1b[1;36mbladebro"),
            "title line stays plain"
        );
        assert!(
            !styled.contains("\x1b[1;36m  0  ok"),
            "body lines stay plain"
        );
        assert_eq!(
            style_help_impl(text, false),
            text,
            "plain mode is byte-identical"
        );
    }

    #[test]
    fn kv_rows_keep_their_shape() {
        let row = kv("mode", "auto", 9);
        assert!(row.starts_with("  "), "indented");
        assert!(row.contains("mode") && row.contains("auto"));
        assert!(visible_width(row.trim_start()) >= "mode".len() + 2 + "auto".len());
    }

    #[test]
    fn ansi_stripping_and_visible_width_stay_exact() {
        let s = "\x1b[1;38;2;212;175;55mBLADEBRO\x1b[0m \x1b[31m◈\x1b[0m";
        assert_eq!(strip_ansi(s), "BLADEBRO ◈");
        assert_eq!(visible_width(s), 10);
        assert_eq!(palette::truncate_cols("abcdefghij", 6), "abcde…");
        assert_eq!(palette::truncate_cols("abc", 6), "abc");
    }

    #[test]
    fn progress_bar_renders_exact_width_across_ratios() {
        for cur in [0u64, 1, 50, 99, 100] {
            let bar = anim::render_determinate_bar(cur, 100, 20);
            let plain = strip_ansi(&bar);
            assert_eq!(
                visible_width(&plain),
                22,
                "bar width mismatch for {cur}/100: {plain:?}"
            );
        }
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(8 * 1024 * 1024), "8.0 MB");
    }

    #[test]
    fn palette_tier_plain_emits_zero_escape_codes() {
        let s = palette::paint_tier(ColorTier::Plain, (212, 175, 55), true, 220, "1;33", "DON");
        assert_eq!(s, "DON");
        let p = palette::paint_pill(
            ColorTier::Plain,
            (255, 255, 255),
            (0, 0, 0),
            230,
            236,
            "32",
            "PASS",
        );
        assert_eq!(p, "[PASS]");
    }
}
