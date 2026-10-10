//! Author: TechnoL0g
//!
//! Console highlights: one coloured line on stdout for the events an operator watches for (own block forged, relayed,
//! slot skipped, block rejected). The tracing INFO/WARN lines stay as they are for log files; highlights are printed
//! only when colour is on (`--color auto` = stdout is a terminal, `--color always` for pm2 which keeps ANSI).

use std::sync::atomic::{AtomicBool, Ordering};

static COLOR: AtomicBool = AtomicBool::new(false);

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const AMBER: &str = "\x1b[1;33m";
pub const GREEN: &str = "\x1b[32m";
pub const CYAN: &str = "\x1b[1;36m";
pub const BLUE: &str = "\x1b[1;34m";
pub const RED: &str = "\x1b[1;31m";

/// `--color auto|always|never` → resolved once at start.
pub fn set_color(mode: &str) {
    let on = match mode {
        "always" => true,
        "never" => false,
        _ => std::env::var_os("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    COLOR.store(on, Ordering::Relaxed);
}

pub fn color() -> bool {
    COLOR.load(Ordering::Relaxed)
}

pub fn paint(code: &str, s: &str) -> String {
    if color() { format!("{code}{s}{RESET}") } else { s.to_string() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Our delegate produced a block.
    Forged,
    /// The block was accepted by the peers it was pushed to.
    Relayed,
    /// Our slot passed without a block from us.
    Skipped,
    /// Nobody accepted our block / it will be rolled back.
    Rejected,
}

impl Kind {
    fn style(self) -> (&'static str, &'static str) {
        match self {
            Kind::Forged => (BLUE, "FORGED  "),
            Kind::Relayed => (GREEN, "RELAYED "),
            Kind::Skipped => (AMBER, "SKIPPED "),
            Kind::Rejected => (RED, "REJECTED"),
        }
    }
}

fn clock() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// The highlight line: `▌ 18:01:21  FORGED   #11,916,646  paracelsus  ·  0 tx · fees 0 STH · id 61eb58cfa0…`.
pub fn line(kind: Kind, subject: &str, detail: &str, color: bool) -> String {
    let (code, label) = kind.style();
    if color {
        format!("{code}▌{RESET} {DIM}{}{RESET}  {code}{label}{RESET}  {BOLD}{subject}{RESET}  {DIM}·{RESET}  {detail}", clock())
    } else {
        format!("▌ {}  {label}  {subject}  ·  {detail}", clock())
    }
}

/// Print a highlight (no-op without colour: the tracing line already carries the information).
pub fn event(kind: Kind, subject: &str, detail: &str) {
    if color() {
        println!("{}", line(kind, subject, detail, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_line_is_labelled_and_coloured_only_on_request() {
        let plain = line(Kind::Forged, "#11,916,646  paracelsus", "0 tx · fees 0 STH", false);
        assert!(plain.starts_with("▌ ") && plain.contains("  FORGED    #11,916,646  paracelsus  ·  0 tx · fees 0 STH"), "{plain}");
        assert!(!plain.contains("\x1b["));
        let colored = line(Kind::Rejected, "#1", "none accepted", true);
        assert!(colored.contains(RED) && colored.contains("REJECTED") && colored.contains(RESET));
        assert_eq!(Kind::Skipped.style().1, "SKIPPED ");
        assert_eq!(Kind::Relayed.style().0, GREEN);
    }

    #[test]
    fn color_mode_resolves() {
        set_color("always");
        assert!(color());
        assert_eq!(paint(BOLD, "x"), format!("{BOLD}x{RESET}"));
        set_color("never");
        assert!(!color());
        assert_eq!(paint(BOLD, "x"), "x");
    }
}
