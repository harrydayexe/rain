//! Terminal output helpers.
//!
//! rain runs unattended for hours at a time, so every step it takes has to be
//! visible as it happens. Output is deliberately line-oriented and timestamped
//! so a log captured by `tee` or a systemd unit reads the same as a live run.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static COLOR: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn init(verbose: bool) {
    let enabled = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    COLOR.store(enabled, Ordering::Relaxed);
    VERBOSE.store(verbose, Ordering::Relaxed);
}

pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

fn paint(code: &str, text: &str) -> String {
    if COLOR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(text: &str) -> String {
    paint("1", text)
}
pub fn dim(text: &str) -> String {
    paint("2", text)
}
pub fn green(text: &str) -> String {
    paint("32", text)
}
pub fn yellow(text: &str) -> String {
    paint("33", text)
}
pub fn red(text: &str) -> String {
    paint("31", text)
}
pub fn cyan(text: &str) -> String {
    paint("36", text)
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn emit(line: String) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{line}");
    let _ = err.flush();
}

/// A major phase boundary — one per issue, per stage.
pub fn step(msg: &str) {
    emit(format!("{} {} {}", dim(&stamp()), cyan("::"), bold(msg)));
}

/// Routine progress.
pub fn info(msg: &str) {
    emit(format!("{} {}  {}", dim(&stamp()), dim("  "), msg));
}

pub fn ok(msg: &str) {
    emit(format!("{} {}  {}", dim(&stamp()), green("ok"), msg));
}

pub fn warn(msg: &str) {
    emit(format!("{} {}  {}", dim(&stamp()), yellow("!!"), msg));
}

pub fn error(msg: &str) {
    emit(format!("{} {}  {}", dim(&stamp()), red("xx"), msg));
}

/// Agent chatter and command echoes — only shown with `--verbose`.
pub fn trace(msg: &str) {
    if is_verbose() {
        emit(format!("{} {}  {}", dim(&stamp()), dim(".."), dim(msg)));
    }
}

/// Live output from a Claude Code session, indented so it is distinguishable
/// from rain's own reporting.
pub fn agent(msg: &str) {
    emit(format!("{}      {} {}", dim(&stamp()), dim("│"), msg));
}

pub fn blank() {
    emit(String::new());
}

pub fn rule(title: &str) {
    let width = 72usize.saturating_sub(title.chars().count() + 3);
    emit(format!(
        "\n{} {}",
        bold(title),
        dim(&"─".repeat(width.max(3)))
    ));
}

/// Collapse whitespace and clip to `max` characters for single-line summaries.
pub fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}
