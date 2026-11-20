//! Mafia Don / Noir Syndicate layout primitives: masthead crest, dossier
//! sections, aligned status rows, sealed verdict cards, version identity card,
//! and styled help manual.

use super::anim;
use super::palette::{
    amber, bold_cream, bold_crimson, bold_emerald, bold_gold, cream, crimson, emerald, gold,
    gradient_rule, obsidian, pill_fail, pill_gold, pill_pass, pill_smoke, pill_warn, shimmer_gold,
    smoke, truncate_cols, visible_width,
};

/// Visual tone for verdict cards and dossier rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warn,
    Fail,
    Gold,
    Smoke,
}

/// Responsive frame inner width (excluding left 2-space margin and box edges).
pub fn frame_width() -> usize {
    let cols = crate::platform::terminal_width().unwrap_or(80);
    // Reserve 2 spaces left margin + 2 border chars + 2 breathing cols.
    cols.saturating_sub(6).clamp(56, 82)
}

/// Pad a styled string on the right to `width` visible columns.
fn pad_right(s: &str, width: usize) -> String {
    let w = visible_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Wrap plain text cleanly to lines of at most `max_cols` terminal columns,
/// preferring whitespace breaks and falling back to hard column breaks on long
/// paths/URLs.
fn wrap_plain(text: &str, max_cols: usize) -> Vec<String> {
    if max_cols < 8 {
        return vec![truncate_cols(text, max_cols.max(1))];
    }
    if visible_width(text) <= max_cols {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut cur_w = 0usize;

    for word in text.split_whitespace() {
        let ww = visible_width(word);
        if cur_w == 0 {
            if ww <= max_cols {
                current.push_str(word);
                cur_w = ww;
            } else {
                // Hard-break a single long token (e.g. a deep file path).
                let mut chunk = String::new();
                let mut cw = 0usize;
                for ch in word.chars() {
                    let w = visible_width(&ch.to_string());
                    if cw + w > max_cols && !chunk.is_empty() {
                        lines.push(std::mem::take(&mut chunk));
                        cw = 0;
                    }
                    chunk.push(ch);
                    cw += w;
                }
                current = chunk;
                cur_w = cw;
            }
        } else if cur_w + 1 + ww <= max_cols {
            current.push(' ');
            current.push_str(word);
            cur_w += 1 + ww;
        } else {
            lines.push(std::mem::take(&mut current));
            if ww <= max_cols {
                current.push_str(word);
                cur_w = ww;
            } else {
                let mut chunk = String::new();
                let mut cw = 0usize;
                for ch in word.chars() {
                    let w = visible_width(&ch.to_string());
                    if cw + w > max_cols && !chunk.is_empty() {
                        lines.push(std::mem::take(&mut chunk));
                        cw = 0;
                    }
                    chunk.push(ch);
                    cw += w;
                }
                current = chunk;
                cur_w = cw;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Build a single horizontal line inside a box of inner width `inner`, placing
/// `left` at the left and `right` flush-right with at least 2 spaces gap.
fn lr_inside(left: &str, right: &str, inner: usize) -> String {
    let lw = visible_width(left);
    let rw = visible_width(right);
    if lw + rw + 2 <= inner {
        let gap = inner - lw - rw;
        format!("{left}{}{right}", " ".repeat(gap))
    } else if rw == 0 {
        pad_right(left, inner)
    } else {
        pad_right(&format!("{left}  {right}"), inner)
    }
}

/// Iconic double-ruled Mafia Don masthead with a crisp 130ms gold shimmer
/// reveal on interactive TTYs and clean single-line text off-TTY.
pub fn masthead(operation: &str, meta: &str) {
    let version = env!("CARGO_PKG_VERSION");
    let build = env!("BLADE_BUILD_ID");

    if !super::enabled() {
        println!();
        println!("BLADEBRO v{version} ({build}) — {operation} [{meta}]");
        println!("{}", "-".repeat(60));
        println!();
        return;
    }

    let inner = frame_width();
    // Crest cell is 3 visible cols wide (` ◈ `), divider `╦`/`║`/`╩` is 1 col,
    // right pane is `inner - 4` cols → total between outer borders = `inner`.
    let right_pane = inner.saturating_sub(4);
    let top = format!(
        "  {}{}{}{}{}",
        gold("╔"),
        gold(&"═".repeat(3)),
        gold("╦"),
        gradient_rule(&"═".repeat(right_pane)),
        crimson("╗"),
    );
    let sep = format!(
        "  {}{}{}{}{}",
        gold("╠"),
        gold(&"═".repeat(3)),
        gold("╩"),
        obsidian(&"═".repeat(right_pane)),
        obsidian("╣"),
    );
    let bot = format!(
        "  {}{}{}",
        obsidian("╚"),
        obsidian(&"═".repeat(inner)),
        obsidian("╝"),
    );

    let ver_tag = format!("{} {} ", bold_gold(&format!("v{version}")), smoke(build));
    let mid_fn = |phase: f64| -> String {
        let wordmark = format!("  {}", shimmer_gold("B L A D E B R O", phase, true));
        let right_content = lr_inside(&wordmark, &ver_tag, right_pane);
        format!(
            "  {} {} {}{}{}",
            gold("║"),
            bold_crimson("◈"),
            gold("║"),
            right_content,
            crimson("║"),
        )
    };

    let op_left = format!(
        "  {}  {} {}",
        gold("◆"),
        smoke("OPERATION ·"),
        bold_cream(&operation.to_uppercase())
    );
    let op_right = format!("{} ", smoke(meta));
    let sub_content = lr_inside(&op_left, &op_right, inner);
    let sub = format!("  {}{}{}", obsidian("║"), sub_content, obsidian("║"));

    anim::reveal_masthead(&top, &mid_fn, &sep, &sub, &bot);
}

/// Dossier section divider: `━━ ◆ SECTION TITLE ━━━━━━━━━━━━━━━━━━`.
pub fn section(title: &str) {
    println!();
    if !super::enabled() {
        println!("  -- {title} --");
        return;
    }
    let width = frame_width();
    let prefix_text = format!("━━ ◆ {} ", title.to_uppercase());
    let used = visible_width(&prefix_text);
    let tail_len = width.saturating_sub(used).max(4);
    println!(
        "  {} {} {} {}",
        gold("━━"),
        bold_crimson("◆"),
        bold_gold(&title.to_uppercase()),
        obsidian(&"━".repeat(tail_len)),
    );
}

/// Numbered phase header for multi-step operations (`[01/04] Title`).
pub fn phase(n: usize, total: usize, title: &str) {
    println!();
    if !super::enabled() {
        println!("  [{n}/{total}] {title}");
        return;
    }
    let tag = pill_gold(&format!("{n:02}/{total:02}"));
    let width = frame_width();
    let left_plain_len = format!("[{n:02}/{total:02}]  {title} ").chars().count();
    let rule_len = width.saturating_sub(left_plain_len + 2).max(4);
    println!(
        "  {tag}  {}  {}",
        bold_cream(title),
        obsidian(&"─".repeat(rule_len))
    );
}

/// One diagnostic / audit dossier row.
///
/// In plain mode, emits `"  ✓ {label:<w} {detail}"` and `"    fix: {fix}"` on
/// the immediately following line so machine parsers and tests
/// (`tests/doctor_flags.rs`) keep exact compatibility.
pub fn dossier_row(tone: Tone, label: &str, detail: &str, fix: Option<&str>, label_width: usize) {
    if !super::enabled() {
        let icon = match tone {
            Tone::Success => "✓",
            Tone::Warn => "⚠",
            Tone::Fail => "✗",
            Tone::Gold | Tone::Smoke => "•",
        };
        println!("  {icon} {label:<label_width$} {detail}");
        if let Some(f) = fix {
            println!("    fix: {f}");
        }
        return;
    }

    let badge = match tone {
        Tone::Success => pill_pass("PASS"),
        Tone::Warn => pill_warn("WARN"),
        Tone::Fail => pill_fail("FAIL"),
        Tone::Gold => pill_gold("INFO"),
        Tone::Smoke => pill_smoke("NOTE"),
    };
    let padded_label = format!("{label:<label_width$}");
    let styled_label = match tone {
        Tone::Fail => bold_crimson(&padded_label),
        Tone::Warn => amber(&padded_label),
        _ => cream(&padded_label),
    };
    let styled_detail = match tone {
        Tone::Fail => crimson(detail),
        Tone::Warn => amber(detail),
        Tone::Success => smoke(detail),
        Tone::Gold => gold(detail),
        Tone::Smoke => smoke(detail),
    };
    println!("  {badge}  {styled_label}  {styled_detail}");
    if let Some(f) = fix {
        let fix_prefix = match tone {
            Tone::Fail => crimson("↳ fix:"),
            _ => gold("↳ fix:"),
        };
        println!("          {fix_prefix} {}", cream(f));
    }
}

/// Framed verdict / summary card with rounded obsidian/gold/crimson borders.
///
/// `right_meta` sits flush-right on the header row (e.g. elapsed time `0.28s`).
/// `body` lines render cleanly inside the box with gold bullet accents.
pub fn verdict_card(tone: Tone, title: &str, right_meta: &str, body: &[(&str, String)]) {
    println!();
    if !super::enabled() {
        let icon = match tone {
            Tone::Success => "✓",
            Tone::Warn => "⚠",
            Tone::Fail => "✗",
            Tone::Gold | Tone::Smoke => "ℹ",
        };
        if right_meta.is_empty() {
            println!("  {icon} {title}");
        } else {
            println!("  {icon} {title} ({right_meta})");
        }
        for (k, v) in body {
            if k.is_empty() {
                println!("    {v}");
            } else {
                println!("    {k}: {v}");
            }
        }
        return;
    }

    let inner = frame_width();
    let border = |s: &str| match tone {
        Tone::Success => emerald(s),
        Tone::Warn => amber(s),
        Tone::Fail => crimson(s),
        Tone::Gold => gold(s),
        Tone::Smoke => obsidian(s),
    };
    let emblem = match tone {
        Tone::Success => bold_emerald("◆"),
        Tone::Warn => amber("▲"),
        Tone::Fail => bold_crimson("✖"),
        Tone::Gold => bold_gold("◈"),
        Tone::Smoke => smoke("◇"),
    };
    let styled_title = match tone {
        Tone::Success => bold_emerald(title),
        Tone::Warn => amber(title),
        Tone::Fail => bold_crimson(title),
        Tone::Gold => bold_gold(title),
        Tone::Smoke => cream(title),
    };

    println!(
        "  {}{}{}",
        border("╭"),
        border(&"─".repeat(inner)),
        border("╮")
    );
    let head_left = format!("  {emblem}  {styled_title}");
    let head_right = if right_meta.is_empty() {
        String::new()
    } else {
        format!("{} ", smoke(right_meta))
    };
    let head_row = lr_inside(&head_left, &head_right, inner);
    println!("  {}{}{}", border("│"), head_row, border("│"));

    if !body.is_empty() {
        println!(
            "  {}{}{}",
            border("├"),
            obsidian(&"─".repeat(inner)),
            border("┤")
        );
        let key_w = body
            .iter()
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        for (k, v) in body {
            let max_val = inner.saturating_sub(if k.is_empty() { 7 } else { key_w + 7 });
            let wrapped = wrap_plain(v, max_val);
            for (idx, line) in wrapped.iter().enumerate() {
                let content = if k.is_empty() {
                    if idx == 0 {
                        format!("  {}  {}", gold("›"), cream(line))
                    } else {
                        format!("     {}", cream(line))
                    }
                } else if idx == 0 {
                    let kp = format!("{k:<key_w$}");
                    format!("  {}  {}", smoke(&kp), cream(line))
                } else {
                    let blank = " ".repeat(key_w);
                    format!("  {blank}  {}", cream(line))
                };
                let padded = pad_right(&content, inner);
                println!("  {}{}{}", border("│"), padded, border("│"));
            }
        }
    }

    println!(
        "  {}{}{}",
        border("╰"),
        border(&"─".repeat(inner)),
        border("╯")
    );
    println!();
}

/// Fields for the Mafia Don version identity card (`bladebro -v`).
pub struct VersionCard<'a> {
    pub version: &'a str,
    pub build_id: &'a str,
    pub platform: &'a str,
    pub install_channel: &'a str,
    pub status_tone: Tone,
    pub status_badge: &'a str,
    pub status_detail: &'a str,
    pub action_cmd: Option<&'a str>,
}

/// Sleek Mafia Don version identity card for `bladebro -v` / `--version`.
pub fn version_card(card: &VersionCard<'_>) {
    let VersionCard {
        version,
        build_id,
        platform,
        install_channel,
        status_tone,
        status_badge,
        status_detail,
        action_cmd,
    } = *card;
    if !super::enabled() {
        println!("bladebro v{version} ({build_id})");
        println!("  platform: {platform}");
        println!("  install: {install_channel}");
        println!("  {status_detail}");
        if let Some(cmd) = action_cmd {
            println!("  run: {cmd}");
        }
        return;
    }

    let inner = frame_width().min(72);
    let right_pane = inner.saturating_sub(4);
    let top = format!(
        "  {}{}{}{}{}",
        gold("╔"),
        gold(&"═".repeat(3)),
        gold("╦"),
        gradient_rule(&"═".repeat(right_pane)),
        crimson("╗"),
    );
    let sep = format!(
        "  {}{}{}{}{}",
        gold("╠"),
        gold(&"═".repeat(3)),
        gold("╩"),
        obsidian(&"═".repeat(right_pane)),
        obsidian("╣"),
    );
    let bot = format!(
        "  {}{}{}",
        obsidian("╚"),
        obsidian(&"═".repeat(inner)),
        obsidian("╝"),
    );

    let pill = match status_tone {
        Tone::Success => pill_pass(status_badge),
        Tone::Warn => pill_warn(status_badge),
        Tone::Fail => pill_fail(status_badge),
        Tone::Gold => pill_gold(status_badge),
        Tone::Smoke => pill_smoke(status_badge),
    };
    let head_right = format!("{pill} ");
    let mid_left = format!(
        "  {}  {}",
        bold_gold("B L A D E B R O"),
        bold_cream(&format!("v{version}"))
    );
    let mid = format!(
        "  {} {} {}{}{}",
        gold("║"),
        bold_crimson("◈"),
        gold("║"),
        lr_inside(&mid_left, &head_right, right_pane),
        crimson("║"),
    );

    println!();
    println!("{top}");
    println!("{mid}");
    println!("{sep}");

    let rows: Vec<(&str, String)> = vec![
        ("build", format!("{build_id}  ·  {platform}")),
        ("channel", install_channel.to_string()),
        ("release", status_detail.to_string()),
    ];
    for (k, v) in rows {
        let max_v = inner.saturating_sub(14);
        let clipped = truncate_cols(&v, max_v);
        let val = if k == "release" {
            match status_tone {
                Tone::Success => emerald(&clipped),
                Tone::Gold => bold_gold(&clipped),
                Tone::Warn => amber(&clipped),
                Tone::Fail => crimson(&clipped),
                Tone::Smoke => smoke(&clipped),
            }
        } else {
            cream(&clipped)
        };
        let content = format!("  {}  {val}", smoke(&format!("{k:<8}")));
        println!(
            "  {}{}{}",
            obsidian("║"),
            pad_right(&content, inner),
            obsidian("║")
        );
    }

    if let Some(cmd) = action_cmd {
        let sep2 = format!(
            "  {}{}{}",
            obsidian("╟"),
            obsidian(&"─".repeat(inner)),
            obsidian("╢")
        );
        println!("{sep2}");
        let content = format!("  {}  {}  {}", gold("›"), smoke("upgrade:"), bold_gold(cmd));
        println!(
            "  {}{}{}",
            obsidian("║"),
            pad_right(&content, inner),
            obsidian("║")
        );
    }

    println!("{bot}");
    println!();
}

/// Framed Mafia Don key-value dossier box (great for `rb`, `hermes`, etc.).
pub fn dossier_card(
    title: &str,
    badge: (&str, Tone),
    rows: &[(&str, String)],
    footer: Option<&str>,
) {
    if !super::enabled() {
        println!("{title}: {}", badge.0);
        println!();
        let w = rows
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(8);
        for (k, v) in rows {
            println!("  {k:<w$}  {v}");
        }
        if let Some(f) = footer {
            println!();
            println!("  {f}");
        }
        return;
    }

    let inner = frame_width();
    let pill = match badge.1 {
        Tone::Success => pill_pass(badge.0),
        Tone::Warn => pill_warn(badge.0),
        Tone::Fail => pill_fail(badge.0),
        Tone::Gold => pill_gold(badge.0),
        Tone::Smoke => pill_smoke(badge.0),
    };

    println!();
    println!(
        "  {}{}{}",
        gold("╭"),
        gradient_rule(&"─".repeat(inner)),
        crimson("╮")
    );
    let head = lr_inside(
        &format!(
            "  {}  {}",
            bold_crimson("◈"),
            bold_gold(&title.to_uppercase())
        ),
        &format!("{pill} "),
        inner,
    );
    println!("  {}{}{}", gold("│"), head, crimson("│"));
    println!(
        "  {}{}{}",
        obsidian("├"),
        obsidian(&"─".repeat(inner)),
        obsidian("┤")
    );

    let key_w = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8);
    for (k, v) in rows {
        let max_v = inner.saturating_sub(key_w + 6);
        if v.contains('\x1b') && visible_width(v) <= max_v {
            let kp = format!("{k:<key_w$}");
            let row = format!("  {}  {v}", smoke(&kp));
            println!(
                "  {}{}{}",
                obsidian("│"),
                pad_right(&row, inner),
                obsidian("│")
            );
        } else {
            let plain = super::palette::strip_ansi(v);
            let is_warn = *k == "caution" || *k == "paused";
            let wrapped = wrap_plain(&plain, max_v);
            for (idx, line) in wrapped.iter().enumerate() {
                let styled_line = if is_warn { amber(line) } else { cream(line) };
                let row = if idx == 0 {
                    let kp = format!("{k:<key_w$}");
                    format!("  {}  {styled_line}", smoke(&kp))
                } else {
                    let blank = " ".repeat(key_w);
                    format!("  {blank}  {styled_line}")
                };
                println!(
                    "  {}{}{}",
                    obsidian("│"),
                    pad_right(&row, inner),
                    obsidian("│")
                );
            }
        }
    }

    if let Some(f) = footer {
        println!(
            "  {}{}{}",
            obsidian("├"),
            obsidian(&"─".repeat(inner)),
            obsidian("┤")
        );
        let max_f = inner.saturating_sub(7);
        for (idx, line) in wrap_plain(f, max_f).iter().enumerate() {
            let foot = if idx == 0 {
                format!("  {}  {}", gold("›"), smoke(line))
            } else {
                format!("     {}", smoke(line))
            };
            println!(
                "  {}{}{}",
                obsidian("│"),
                pad_right(&foot, inner),
                obsidian("│")
            );
        }
    }

    println!(
        "  {}{}{}",
        obsidian("╰"),
        obsidian(&"─".repeat(inner)),
        obsidian("╯")
    );
    println!();
}

/// Rich Mafia Don styling for the interactive CLI help manual.
///
/// Only runs when `enabled()` is true (pipes and `BLADE_PLAIN=1` get the
/// exact unmodified plain manual). Preserves every line and command intact.
pub fn style_help_noir(text: &str) -> String {
    if !super::enabled() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 512);
    let mut lines = text.split_inclusive('\n').peekable();

    // Style the opening title line as a sleek Noir masthead bar.
    if let Some(first_seg) = lines.next() {
        let (first, nl) = match first_seg.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (first_seg, ""),
        };
        let w = frame_width();
        out.push('\n');
        out.push_str(&format!(
            "  {}{}{}\n",
            gold("╔"),
            gradient_rule(&"═".repeat(w)),
            crimson("╗")
        ));
        let title_row = format!("  {}  {}", bold_crimson("◈"), bold_gold(first));
        out.push_str(&format!(
            "  {}{}{}\n",
            gold("║"),
            pad_right(&title_row, w),
            crimson("║")
        ));
        out.push_str(&format!(
            "  {}{}{}",
            obsidian("╚"),
            obsidian(&"═".repeat(w)),
            obsidian("╝")
        ));
        out.push_str(nl);
    }

    let mut current_section = "";
    for seg in lines {
        let (line, nl) = match seg.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (seg, ""),
        };
        if super::is_section_header(line) {
            current_section = line.trim();
            let w = frame_width();
            let prefix_len = line.chars().count() + 6;
            let tail = w.saturating_sub(prefix_len).max(4);
            out.push_str(&format!(
                "{} {} {} {}",
                gold("━━"),
                bold_crimson("◆"),
                bold_gold(line),
                obsidian(&"━".repeat(tail)),
            ));
        } else if let Some(trimmed) = line.strip_prefix("  ") {
            match current_section {
                "COMMANDS" | "FLAGS" | "MODES" | "EXTRACTION" | "ENVIRONMENT" | "COOKIES"
                | "STORAGE" | "TABS" | "SESSIONS / CONTROL" => {
                    // Split column 1 (command/flag) from column 2 (description) on 2+ spaces.
                    if let Some(gap) = find_double_space(trimmed) {
                        let left = &trimmed[..gap];
                        let right = &trimmed[gap..];
                        let left_styled = if left.trim().is_empty() {
                            left.to_string()
                        } else if current_section == "ENVIRONMENT" {
                            gold(left)
                        } else {
                            bold_cream(left)
                        };
                        out.push_str(&format!("  {left_styled}{}", smoke(right)));
                    } else {
                        out.push_str(&smoke(line));
                    }
                }
                "QUICK START" | "EXAMPLES" => {
                    if let Some(gap) = find_double_space(trimmed) {
                        let left = &trimmed[..gap];
                        let right = &trimmed[gap..];
                        out.push_str(&format!("  {} {}{}", gold("›"), cream(left), smoke(right)));
                    } else {
                        out.push_str(&format!("  {} {}", gold("›"), cream(trimmed)));
                    }
                }
                "EXIT CODES" => {
                    if let Some(rest) = trimmed.strip_prefix("0  ") {
                        out.push_str(&format!("  {}  {}", pill_pass("0"), cream(rest)));
                    } else if let Some(rest) = trimmed.strip_prefix("1  ") {
                        out.push_str(&format!("  {}  {}", pill_fail("1"), cream(rest)));
                    } else if let Some(rest) = trimmed.strip_prefix("2  ") {
                        out.push_str(&format!("  {}  {}", pill_warn("2"), cream(rest)));
                    } else {
                        out.push_str(line);
                    }
                }
                _ => {
                    out.push_str(&cream(line));
                }
            }
        } else {
            out.push_str(&smoke(line));
        }
        out.push_str(nl);
    }
    out
}

fn find_double_space(s: &str) -> Option<usize> {
    // Only split after at least one non-space char.
    let start = s.find(|c: char| !c.is_whitespace())?;
    s[start..].find("  ").map(|i| start + i)
}
