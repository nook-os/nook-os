//! Colour for the CLI.
//!
//! Hand-rolled rather than a crate: this is a handful of SGR codes, and the
//! palette has to stay in step with the amber-CRT theme the web UI and
//! nookos.dev already use. Keeping it in one small file makes "do these agree?"
//! a question you can answer by reading twenty lines.
//!
//! Colour is suppressed when stdout is not a terminal, or when `NO_COLOR` is
//! set. Both matter: `nook get sessions | grep` and `nook exec … > log` are
//! ordinary things to do, and escape codes in a pipe are corruption, not
//! decoration.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// 256-colour approximations of the theme:
///   accent  #f5b301 → 214
///   ok      #2dd4a7 → 43
///   info    #58a6ff → 75
///   err     #ff5c5c → 203
const ACCENT: &str = "\x1b[38;5;214m";
const OK: &str = "\x1b[38;5;43m";
const INFO: &str = "\x1b[38;5;75m";
const ERR: &str = "\x1b[38;5;203m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        // https://no-color.org — set to anything at all means "no colour".
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
            return false;
        }
        std::io::stdout().is_terminal()
    })
}

fn wrap(code: &str, s: &str) -> String {
    if enabled() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

pub fn accent(s: &str) -> String {
    wrap(ACCENT, s)
}
pub fn ok_c(s: &str) -> String {
    wrap(OK, s)
}
pub fn info(s: &str) -> String {
    wrap(INFO, s)
}
pub fn err(s: &str) -> String {
    wrap(ERR, s)
}
pub fn dim(s: &str) -> String {
    wrap(DIM, s)
}
pub fn bold(s: &str) -> String {
    wrap(BOLD, s)
}

/// `✓ <line>` — something finished.
pub fn success(line: &str) -> String {
    format!("{} {line}", ok_c("✓"))
}

/// `▸ <line>` — something is happening.
pub fn step(line: &str) -> String {
    format!("{} {line}", accent("▸"))
}

/// `✗ <line>` — it did not work.
pub fn failure(line: &str) -> String {
    format!("{} {line}", err("✗"))
}

/// A follow-up command worth running next. Indented and dim, so it reads as a
/// suggestion rather than as more output.
pub fn hint(line: &str) -> String {
    format!("  {}", dim(line))
}

/// What the user typed, echoed back before the reply.
pub fn prompt_echo(line: &str) -> String {
    format!("{} {}", accent("❯"), line)
}

/// A runtime's answer.
pub fn reply(line: &str) -> String {
    format!("{} {line}", ok_c("●"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under a pipe — which is how these tests run — nothing may be coloured.
    /// A CLI that emits escape codes into `grep` has corrupted its own output.
    #[test]
    fn no_escapes_when_not_a_terminal() {
        assert_eq!(accent("x"), "x");
        assert_eq!(success("done"), "✓ done");
        assert!(!reply("hi").contains('\x1b'));
    }

    /// The glyphs are part of the contract: the web demo and the docs show
    /// these exact marks, so changing one silently desynchronises them.
    #[test]
    fn markers_are_stable() {
        assert!(success("a").starts_with('✓'));
        assert!(step("a").starts_with('▸'));
        assert!(failure("a").starts_with('✗'));
        assert!(prompt_echo("a").starts_with('❯'));
        assert!(reply("a").starts_with('●'));
    }
}
