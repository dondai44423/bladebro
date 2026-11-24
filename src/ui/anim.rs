//! Terminal animations — live Mafia Don spinner, high-resolution gradient
//! download progress bar, and sleek reveal sweeps.
//!
//! All animations are strictly gated to interactive TTY stdout (never in pipes,
//! CI logs, `BLADE_PLAIN=1`, `NO_COLOR`, or `BLADE_NO_ANIM=1`). Cursor
//! visibility (`\x1b[?25l` / `\x1b[?25h`) is RAII-guarded so the user's
//! terminal cursor is ALWAYS restored even on early error returns or panics.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::palette::{
    self, amber, bold_crimson, bold_emerald, bold_gold, cream, crimson, gold, gradient_rgb,
    obsidian, pill_fail, pill_pass, pill_warn, shimmer_cream, smoke, stdout_tier, ColorTier,
};

/// Whether live `\r` terminal animations should run on stdout.
pub fn anim_enabled() -> bool {
    if !super::enabled() {
        return false;
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if std::env::var_os("BLADE_NO_ANIM")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
    {
        return false;
    }
    if std::env::var_os("CI")
        .map(|v| !v.is_empty() && v != "0" && v != "false")
        .unwrap_or(false)
    {
        return false;
    }
    true
}

fn hide_cursor() {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b[?25l");
    let _ = out.flush();
}

fn show_cursor() {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b[?25h");
    let _ = out.flush();
}

fn clear_line() {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();
}

/// Format an elapsed duration concisely (`0.4s`, `12.3s`, `1m 08s`).
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = d.as_secs();
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

/// Format byte sizes cleanly (`842 B`, `148 KB`, `8.2 MB`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

// ── Live Spinner ────────────────────────────────────────────────────────

/// Animated Mafia Don spinner.
///
/// Renders a rotating gold/crimson diamond emblem paired with a subtle
/// champagne-gold shimmer sweep over the active status line and a live
/// elapsed clock. Degrades to zero-thread silence on non-TTY streams until
/// `finish_*` prints one clean plain line.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    msg: Arc<Mutex<String>>,
    started: Instant,
    animated: bool,
    cursor_hidden: bool,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(message: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let msg = Arc::new(Mutex::new(message.to_string()));
        let started = Instant::now();
        let animated = anim_enabled();

        if !animated {
            return Self {
                stop,
                msg,
                started,
                animated: false,
                cursor_hidden: false,
                handle: None,
            };
        }

        hide_cursor();
        let stop_bg = Arc::clone(&stop);
        let msg_bg = Arc::clone(&msg);
        let handle = std::thread::Builder::new()
            .name("blade-ui-spin".into())
            .spawn(move || {
                const GLYPHS: [&str; 8] = ["◆", "◈", "◇", "◈", "◆", "❖", "◈", "◇"];
                const TRACKS: [&str; 8] = [
                    "━───",
                    "─━──",
                    "──━─",
                    "───━",
                    "──━─",
                    "─━──",
                    "━───",
                    "━───",
                ];
                let mut tick: usize = 0;
                while !stop_bg.load(Ordering::Relaxed) {
                    let text = msg_bg.lock().map(|g| g.clone()).unwrap_or_default();
                    let glyph = GLYPHS[tick % GLYPHS.len()];
                    let glyph_styled = if tick % 4 == 0 {
                        bold_crimson(glyph)
                    } else {
                        bold_gold(glyph)
                    };
                    let track = gold(TRACKS[tick % TRACKS.len()]);
                    let len = text.chars().count().max(1);
                    let sweep = ((tick * 2) % (len + 12)) as f64 - 4.0;
                    let label = shimmer_cream(&text, sweep);
                    let elapsed = smoke(&format_elapsed(started.elapsed()));
                    {
                        let mut out = std::io::stdout().lock();
                        let _ = write!(out, "\r\x1b[2K  {glyph_styled} {track} {label}  {elapsed}");
                        let _ = out.flush();
                    }
                    tick = tick.wrapping_add(1);
                    // Sleep in small slices so stop() wakes almost immediately.
                    for _ in 0..4 {
                        if stop_bg.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(18));
                    }
                }
            })
            .ok();

        Self {
            stop,
            msg,
            started,
            animated: true,
            cursor_hidden: true,
            handle,
        }
    }

    /// Update the live spinner label while work is in flight.
    pub fn set_message(&self, message: &str) {
        if let Ok(mut guard) = self.msg.lock() {
            *guard = message.to_string();
        }
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.animated {
            clear_line();
        }
        if self.cursor_hidden {
            show_cursor();
            self.cursor_hidden = false;
        }
    }

    /// Clear the spinner without printing a final line.
    pub fn finish_clear(mut self) {
        self.halt();
    }

    /// Seal the spinner with a velvet emerald OK badge.
    pub fn finish_ok(mut self, message: &str) {
        let elapsed = format_elapsed(self.started.elapsed());
        self.halt();
        if super::enabled() {
            println!(
                "  {}  {}  {}",
                pill_pass("OK"),
                cream(message),
                smoke(&elapsed)
            );
        } else {
            println!("  [OK] {message}");
        }
    }

    /// Seal the spinner with an amber WARN badge.
    pub fn finish_warn(mut self, message: &str) {
        let elapsed = format_elapsed(self.started.elapsed());
        self.halt();
        if super::enabled() {
            println!(
                "  {}  {}  {}",
                pill_warn("WARN"),
                amber(message),
                smoke(&elapsed)
            );
        } else {
            println!("  [WARN] {message}");
        }
    }

    /// Seal the spinner with a crimson FAIL badge.
    pub fn finish_fail(mut self, message: &str) {
        let elapsed = format_elapsed(self.started.elapsed());
        self.halt();
        if super::enabled() {
            println!(
                "  {}  {}  {}",
                pill_fail("FAIL"),
                crimson(message),
                smoke(&elapsed)
            );
        } else {
            println!("  [FAIL] {message}");
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.halt();
    }
}

// ── High-Resolution Gradient Progress Bar ───────────────────────────────

/// Real-time animated download progress bar with 8× sub-block Unicode
/// resolution, gold → crimson TrueColor gradient, smoothed MB/s throughput,
/// and live ETA.
pub struct ProgressBar {
    label: String,
    total: AtomicU64,
    current: AtomicU64,
    started: Instant,
    last_draw: Mutex<Instant>,
    last_bytes: AtomicU64,
    ewma_bps: Mutex<f64>,
    tick: AtomicU64,
    animated: bool,
    cursor_hidden: bool,
    finished: bool,
}

impl ProgressBar {
    pub fn new(label: &str, total: u64) -> Self {
        let animated = anim_enabled();
        if animated {
            hide_cursor();
        }
        let bar = Self {
            label: label.to_string(),
            total: AtomicU64::new(total),
            current: AtomicU64::new(0),
            started: Instant::now(),
            last_draw: Mutex::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap_or_else(Instant::now),
            ),
            last_bytes: AtomicU64::new(0),
            ewma_bps: Mutex::new(0.0),
            tick: AtomicU64::new(0),
            animated,
            cursor_hidden: animated,
            finished: false,
        };
        if animated {
            bar.render(true);
        }
        bar
    }

    /// Set or update the known total byte size (e.g., once HTTP headers arrive).
    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.render(true);
    }

    /// Update current bytes downloaded and redraw if the frame budget elapsed.
    pub fn set_position(&self, bytes: u64) {
        self.current.store(bytes, Ordering::Relaxed);
        self.render(false);
    }

    fn render(&self, force: bool) {
        if !self.animated {
            return;
        }
        let now = Instant::now();
        let mut last = match self.last_draw.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let dt = now.duration_since(*last);
        if !force && dt < Duration::from_millis(45) {
            return;
        }

        let cur = self.current.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let prev_b = self.last_bytes.swap(cur, Ordering::Relaxed);
        let dt_secs = dt.as_secs_f64();
        let bps = if let Ok(mut ewma) = self.ewma_bps.lock() {
            if dt_secs > 0.01 && cur >= prev_b {
                let inst = (cur - prev_b) as f64 / dt_secs;
                if *ewma <= 0.0 {
                    *ewma = inst;
                } else {
                    *ewma = 0.30 * inst + 0.70 * *ewma;
                }
            }
            if *ewma > 0.0 {
                *ewma
            } else {
                let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
                cur as f64 / elapsed
            }
        } else {
            0.0
        };
        *last = now;
        drop(last);

        let frame = self.tick.fetch_add(1, Ordering::Relaxed) as usize;
        let width = super::render::frame_width()
            .saturating_sub(42)
            .clamp(16, 36);
        let bar_str = if total > 0 {
            render_determinate_bar(cur, total, width)
        } else {
            render_pulse_bar(frame, width)
        };

        let speed_str = format_speed(bps);
        let line = if total > 0 {
            let pct = ((cur as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
            let eta = if bps > 1.0 && cur < total {
                let rem = (total - cur) as f64 / bps;
                format!("ETA {}", format_elapsed(Duration::from_secs_f64(rem)))
            } else {
                String::new()
            };
            let pct_s = bold_gold(&format!("{pct:5.1}%"));
            let sizes = cream(&format!("{} / {}", format_bytes(cur), format_bytes(total)));
            let meta = if eta.is_empty() {
                smoke(&speed_str)
            } else {
                smoke(&format!("{speed_str} · {eta}"))
            };
            format!("  {} {bar_str} {pct_s}  {sizes}  {meta}", gold("◆"))
        } else {
            let sizes = cream(&format_bytes(cur));
            let meta = smoke(&format!("{speed_str} · {}", self.label));
            format!("  {} {bar_str}  {sizes}  {meta}", gold("◆"))
        };

        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\r\x1b[2K{line}");
        let _ = out.flush();
    }

    /// Seal the bar into a crisp completion summary line.
    pub fn finish_ok(mut self, bytes: u64) {
        let elapsed = self.started.elapsed();
        let bps = bytes as f64 / elapsed.as_secs_f64().max(0.001);
        self.teardown();
        let summary = format!(
            "{}  {} in {}  ({})",
            self.label,
            format_bytes(bytes),
            format_elapsed(elapsed),
            format_speed(bps)
        );
        if super::enabled() {
            println!("  {}  {}", pill_pass("OK"), cream(&summary));
        } else {
            println!("  [OK] {summary}");
        }
    }

    /// Clear the bar cleanly without printing a summary (e.g., on failure).
    pub fn abandon(mut self) {
        self.teardown();
    }

    fn teardown(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.animated {
            clear_line();
        }
        if self.cursor_hidden {
            show_cursor();
            self.cursor_hidden = false;
        }
    }
}

impl Drop for ProgressBar {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn format_speed(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bps / (1024.0 * 1024.0))
    } else if bps >= 1024.0 {
        format!("{:.0} KB/s", bps / 1024.0)
    } else if bps > 0.0 {
        format!("{bps:.0} B/s")
    } else {
        "—".into()
    }
}

/// Render a smooth 8-substep fractional block progress bar with a gold →
/// crimson TrueColor gradient.
pub(crate) fn render_determinate_bar(cur: u64, total: u64, width: usize) -> String {
    const PARTIALS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let ratio = if total == 0 {
        0.0
    } else {
        (cur as f64 / total as f64).clamp(0.0, 1.0)
    };
    let exact = ratio * width as f64;
    let full = (exact.floor() as usize).min(width);
    let frac = ((exact - full as f64) * 8.0).floor() as usize;
    let tier = stdout_tier();

    let mut bar = String::with_capacity(width * 24);
    bar.push_str(&obsidian("▕"));
    for i in 0..width {
        let t = i as f64 / width.saturating_sub(1).max(1) as f64;
        if i < full {
            bar.push_str(&paint_bar_cell(tier, t, '█'));
        } else if i == full && frac > 0 && full < width {
            bar.push_str(&paint_bar_cell(tier, t, PARTIALS[frac.min(7)]));
        } else {
            bar.push_str(&obsidian("░"));
        }
    }
    bar.push_str(&obsidian("▏"));
    bar
}

/// Indeterminate sweeping crimson-gold pulse bar (used when Content-Length is absent).
fn render_pulse_bar(frame: usize, width: usize) -> String {
    let tier = stdout_tier();
    let span = width.saturating_sub(1).max(1);
    let period = span * 2;
    let pos = frame % period;
    let head = if pos <= span { pos } else { period - pos };

    let mut bar = String::with_capacity(width * 24);
    bar.push_str(&obsidian("▕"));
    for i in 0..width {
        let dist = i.abs_diff(head);
        let ch = match dist {
            0 => '█',
            1 => '▓',
            2 => '▒',
            _ => '░',
        };
        if dist <= 2 {
            let t = i as f64 / span as f64;
            bar.push_str(&paint_bar_cell(tier, t, ch));
        } else {
            bar.push_str(&obsidian("░"));
        }
    }
    bar.push_str(&obsidian("▏"));
    bar
}

fn paint_bar_cell(tier: ColorTier, t: f64, ch: char) -> String {
    match tier {
        ColorTier::Plain => ch.to_string(),
        ColorTier::Ansi16 => {
            if t < 0.6 {
                format!("\x1b[33m{ch}\x1b[0m")
            } else {
                format!("\x1b[31m{ch}\x1b[0m")
            }
        }
        ColorTier::Ansi256 => {
            let c = if t < 0.45 {
                220
            } else if t < 0.75 {
                214
            } else {
                196
            };
            format!("\x1b[38;5;{c}m{ch}\x1b[0m")
        }
        ColorTier::TrueColor => {
            let (r, g, b) = gradient_rgb(t);
            format!("\x1b[38;2;{r};{g};{b}m{ch}\x1b[0m")
        }
    }
}

/// Fast, subtle 4-frame gold shimmer reveal for the masthead title line.
/// Runs ~130ms total ONLY on interactive TrueColor/256 TTYs; instant static
/// render everywhere else.
pub(crate) fn reveal_masthead(
    top: &str,
    mid_fn: &dyn Fn(f64) -> String,
    sep: &str,
    sub: &str,
    bot: &str,
) {
    if !anim_enabled() || matches!(stdout_tier(), ColorTier::Plain | ColorTier::Ansi16) {
        println!();
        println!("{top}");
        println!("{}", mid_fn(999.0));
        println!("{sep}");
        println!("{sub}");
        println!("{bot}");
        println!();
        return;
    }

    hide_cursor();
    let phases: [f64; 5] = [-2.0, 4.0, 10.0, 16.0, 999.0];
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out);
    let _ = writeln!(out, "{top}");
    let _ = writeln!(out, "{}", mid_fn(phases[0]));
    let _ = writeln!(out, "{sep}");
    let _ = writeln!(out, "{sub}");
    let _ = writeln!(out, "{bot}");
    let _ = out.flush();
    drop(out);

    for &phase in &phases[1..] {
        std::thread::sleep(Duration::from_millis(32));
        let mut out = std::io::stdout().lock();
        // Move up 4 lines to the wordmark row, redraw it cleanly, move back down 4 lines.
        let _ = write!(out, "\x1b[4A\r\x1b[2K{}\x1b[4B\r", mid_fn(phase));
        let _ = out.flush();
    }
    show_cursor();
    println!();
}

/// Unused-import guard for palette helpers that may only run on certain targets.
pub(crate) fn _touch_helpers() {
    let _ = (bold_emerald, palette::strip_ansi);
}
