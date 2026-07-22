//! Asking questions when stdin is not a keyboard.
//!
//! Under `curl … | sh` the script *is* stdin, so anything read from it consumes
//! the installer rather than the user's answer — the classic failure where a
//! piped installer appears to accept every default instantly. Read the terminal
//! directly instead, which is what rustup and Homebrew do.
//!
//! When there is no terminal at all — CI, a Dockerfile, `sh < /dev/null` — the
//! honest move is to stop. A wizard that silently picks defaults writes real
//! configuration and secrets to disk on a machine nobody was watching.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};

use anyhow::{bail, Context, Result};

/// A handle on the controlling terminal, opened once.
pub struct Tty {
    reader: BufReader<File>,
    writer: File,
}

impl Tty {
    /// `None` when this process has no terminal.
    pub fn open() -> Option<Self> {
        let reader = BufReader::new(File::open("/dev/tty").ok()?);
        let writer = File::options().write(true).open("/dev/tty").ok()?;
        Some(Self { reader, writer })
    }

    fn ask(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        match default {
            Some(d) => write!(self.writer, "{question} [{d}]: ")?,
            None => write!(self.writer, "{question}: ")?,
        }
        self.writer.flush()?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("input closed — setup aborted");
        }
        let line = line.trim().to_string();
        Ok(if line.is_empty() {
            default.unwrap_or_default().to_string()
        } else {
            line
        })
    }

    /// A free-text answer, with an optional default.
    pub fn text(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        loop {
            let v = self.ask(question, default)?;
            if !v.is_empty() {
                return Ok(v);
            }
            self.say("  An answer is required.");
        }
    }

    /// A free-text answer that may be left blank.
    pub fn optional(&mut self, question: &str) -> Result<Option<String>> {
        let v = self.ask(question, Some(""))?;
        Ok(if v.is_empty() { None } else { Some(v) })
    }

    pub fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        let d = if default { "Y/n" } else { "y/N" };
        loop {
            let v = self.ask(&format!("{question} [{d}]"), Some(""))?;
            match v.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("  Please answer y or n."),
            }
        }
    }

    /// A numbered menu. Returns the index of the chosen option.
    pub fn choose(
        &mut self,
        question: &str,
        options: &[(&str, &str)],
        default: usize,
    ) -> Result<usize> {
        self.say("");
        self.say(question);
        for (i, (label, detail)) in options.iter().enumerate() {
            let marker = if i == default { " (recommended)" } else { "" };
            self.say(&format!(
                "  [{}] {}{}",
                i + 1,
                crate::style::bold(label),
                crate::style::dim(marker)
            ));
            if !detail.is_empty() {
                self.say(&format!("      {detail}"));
            }
        }
        loop {
            let v = self.ask("Choice", Some(&(default + 1).to_string()))?;
            match v.parse::<usize>() {
                Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
                _ => self.say(&format!("  Enter a number from 1 to {}.", options.len())),
            }
        }
    }

    pub fn say(&mut self, line: &str) {
        // Colour the markers the wizard emits, so a ✓ reads as done and a ✗ as
        // not. `style` decides whether colour is wanted at all, and writing to
        // /dev/tty means it always is when a person is watching.
        let coloured = match line.trim_start().chars().next() {
            Some(m @ ('\u{2713}' | '\u{2717}' | '\u{25B8}' | '\u{26A0}')) => {
                line.replacen(m, &crate::style::forced(m), 1)
            }
            _ => line.to_string(),
        };
        let _ = writeln!(self.writer, "{coloured}");
    }
}

/// Open the terminal, or explain why we are refusing to continue.
///
/// The message names the flags to re-run with rather than just failing: someone
/// hitting this is usually inside a Dockerfile or CI job, where the fix is to
/// answer up front, not to find a terminal.
pub fn require(non_interactive_hint: &str) -> Result<Tty> {
    Tty::open().with_context(|| {
        format!(
            "no terminal to ask questions on.\n\n\
             This happens in CI, a Dockerfile, or when stdin is closed. Rather than\n\
             pick defaults and write secrets to a machine nobody is watching, it stops.\n\n\
             Answer up front instead:\n  {non_interactive_hint}"
        )
    })
}
