//! Terminal presentation — the one place colors and decorative glyphs live.
//!
//! Every helper degrades to plain text when styling is off, so one code path
//! serves both a TTY (structured, colored) and a pipe / file / MCP stream
//! (byte-exact plain). Styling is ON only when it cannot corrupt a machine
//! consumer — the order matters:
//!
//! 1. `BLADE_PLAIN` set (any non-empty value) → OFF. Our hard switch.
//! 2. `CLICOLOR_FORCE` set (non-empty, not `0`) → ON, even off-TTY (demos,
//!    recordings, docs).
//! 3. `NO_COLOR` set (the de-facto convention) → OFF.
//! 4. `TERM=dumb` → OFF.
//! 5. otherwise ON exactly when stdout is a terminal.
//!
//! Nothing here is platform-specific: on Windows, Rust's std enables VT
//! processing on the console when it is supported (Windows Terminal, modern
//! conhost). A legacy console that would garble escape codes is one
//! `BLADE_PLAIN=1` away from plain output.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn env_set(name: &str) -> bool {
    std::env::var_os(name).map(|v| !v.is_empty()).unwrap_or(false)
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

/// Is styled output enabled for this process? Computed once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        decide(
            std::io::stdout().is_terminal(),
            env_set("BLADE_PLAIN"),
            env_on("CLICOLOR_FORCE"),
            env_set("NO_COLOR"),
            std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false),
        )
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

/// One aligned `label  value` row: the label is dim and padded on the PLAIN
/// text, so ANSI never breaks the column.
pub fn kv(label: &str, value: &str, width: usize) -> String {
    format!("  {}  {}", dim(&format!("{label:<width$}")), value)
}

/// Bold-cyan heading with a dim rule under it (blocks of output).
pub fn header(title: &str) {
    println!();
    println!("  {}", bold_cyan(title));
    println!("  {}", dim(&"─".repeat(title.chars().count())));
    println!();
}

/// Numbered step (`[n/total]` dim).
pub fn step(n: usize, total: usize, msg: &str) {
    println!("  {} {msg}", dim(&format!("[{n}/{total}]")));
}

/// Info message (cyan `ℹ`).
pub fn info(msg: &str) {
    println!("  {} {msg}", cyan("ℹ"));
}

/// Success message (green `✓`).
pub fn success(msg: &str) {
    println!("  {} {msg}", green("✓"));
}

/// Warning message (yellow `⚠`).
pub fn warn(msg: &str) {
    println!("  {} {msg}", yellow("⚠"));
}

/// Hint (dim, indented).
pub fn hint(msg: &str) {
    println!("    {}", dim(msg));
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

/// Human help manual: ALL-CAPS section headers become bold-cyan. Passthrough
/// (byte-identical) when styling is off.
pub fn style_help(text: &str) -> String {
    style_help_impl(text, enabled())
}

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
fn is_section_header(line: &str) -> bool {
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
        assert!(!decide(true, true, true, false, false), "BLADE_PLAIN is absolute");
        assert!(decide(false, false, true, false, false), "CLICOLOR_FORCE wins off-TTY");
        assert!(decide(false, false, true, true, false), "force beats NO_COLOR");
        assert!(!decide(true, false, false, true, false), "NO_COLOR disables");
        assert!(!decide(true, false, false, false, true), "TERM=dumb disables");
        assert!(decide(true, false, false, false, false), "plain TTY is on");
        assert!(!decide(false, false, false, false, false), "a pipe is off");
    }

    #[test]
    fn report_styling_only_touches_errors_and_dividers() {
        let text = "outcome: navigated → x\n✗ error: boom\n--- content ---\nhello\n";
        assert_eq!(style_report_impl(text, false), text, "plain mode is byte-identical");
        let styled = style_report_impl(text, true);
        assert!(styled.contains("\x1b[31m✗ error: boom\x1b[0m"), "error line red");
        assert!(styled.contains("\x1b[2m--- content ---\x1b[0m"), "divider dim");
        assert!(styled.contains("outcome: navigated → x\n"), "other lines untouched");
        assert!(styled.ends_with("hello\n"), "trailing text + newline preserved");
        assert_eq!(styled.matches('\n').count(), text.matches('\n').count());
    }

    #[test]
    fn help_styling_marks_only_headers() {
        let text = "bladebro v1 — manual\n\nUSAGE\n  bladebro x\n\nEXIT CODES\n  0  ok\n";
        let styled = style_help_impl(text, true);
        assert!(styled.contains("\x1b[1;36mUSAGE\x1b[0m"));
        assert!(styled.contains("\x1b[1;36mEXIT CODES\x1b[0m"));
        assert!(!styled.contains("\x1b[1;36mbladebro"), "title line stays plain");
        assert!(!styled.contains("\x1b[1;36m  0  ok"), "body lines stay plain");
        assert_eq!(style_help_impl(text, false), text, "plain mode is byte-identical");
    }

    #[test]
    fn kv_rows_keep_their_shape() {
        let row = kv("mode", "auto", 9);
        assert!(row.starts_with("  "), "indented");
        assert!(row.contains("mode") && row.contains("auto"));
        assert!(row.trim_start().len() >= "mode".len() + 2 + "auto".len());
    }
}
