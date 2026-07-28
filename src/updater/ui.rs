//! CLI output formatting. ANSI colors, no external crate.

/// Bold cyan header.
pub fn header(title: &str) {
    println!();
    println!("  {}", bold_cyan(title));
    println!("  {}", dim(&"─".repeat(title.len())));
    println!();
}

/// Numbered step.
pub fn step(n: usize, total: usize, msg: &str) {
    println!("  {} {msg}", dim(&format!("[{n}/{total}]")));
}

/// Info message (cyan).
pub fn info(msg: &str) {
    println!("  {} {msg}", cyan("ℹ"));
}

/// Success message (green).
pub fn success(msg: &str) {
    println!("  {} {msg}", green("✓"));
}

/// Warning message (yellow).
pub fn warn(msg: &str) {
    println!("  {} {msg}", yellow("⚠"));
}

/// Hint (dim, indented).
pub fn hint(msg: &str) {
    println!("    {}", dim(msg));
}

// Color helpers.

pub fn green(s: &str) -> String {
    format!("\x1b[32m{s}\x1b[0m")
}

pub fn red(s: &str) -> String {
    format!("\x1b[31m{s}\x1b[0m")
}

pub fn yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}

pub fn cyan(s: &str) -> String {
    format!("\x1b[36m{s}\x1b[0m")
}

pub fn bold_cyan(s: &str) -> String {
    format!("\x1b[1;36m{s}\x1b[0m")
}

pub fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}
