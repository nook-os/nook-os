//! Making sure tmux is actually there.
//!
//! tmux is not optional: every session is a tmux session, so a node without it
//! joins successfully and then fails the moment anyone opens a terminal. That
//! is the worst shape of failure — the setup said yes, and the thing you set it
//! up for does not work.
//!
//! So setup checks, and offers to install rather than telling the person to go
//! away and come back.

use anyhow::Result;

use super::tty::Tty;

/// A package manager we know how to drive, and the command to install tmux.
struct Installer {
    name: &'static str,
    /// Run as-is. `sudo` is included where the manager needs it, so the
    /// prompt can say honestly whether a password will be asked for.
    argv: Vec<&'static str>,
    needs_sudo: bool,
}

fn have(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The first package manager present on this machine.
fn installer() -> Option<Installer> {
    // Homebrew first on macOS: it is what a developer's tooling already lives
    // in, and it needs no sudo.
    if have("brew") {
        return Some(Installer {
            name: "Homebrew",
            argv: vec!["brew", "install", "tmux"],
            needs_sudo: false,
        });
    }
    if have("apt-get") {
        return Some(Installer {
            name: "apt",
            argv: vec!["sudo", "apt-get", "install", "-y", "tmux"],
            needs_sudo: true,
        });
    }
    if have("dnf") {
        return Some(Installer {
            name: "dnf",
            argv: vec!["sudo", "dnf", "install", "-y", "tmux"],
            needs_sudo: true,
        });
    }
    if have("pacman") {
        return Some(Installer {
            name: "pacman",
            argv: vec!["sudo", "pacman", "-S", "--noconfirm", "tmux"],
            needs_sudo: true,
        });
    }
    if have("apk") {
        return Some(Installer {
            name: "apk",
            argv: vec!["sudo", "apk", "add", "tmux"],
            needs_sudo: true,
        });
    }
    if have("zypper") {
        return Some(Installer {
            name: "zypper",
            argv: vec!["sudo", "zypper", "install", "-y", "tmux"],
            needs_sudo: true,
        });
    }
    None
}

/// Check for tmux and, if it is missing, offer to install it.
///
/// Returns whether tmux is present afterwards. A `false` is not fatal — a node
/// can still register and report itself — but the caller should say plainly
/// that sessions will not work until it is fixed.
pub fn ensure(t: &mut Tty) -> Result<bool> {
    if crate::capabilities::detect_tmux().is_some() {
        return Ok(true);
    }

    t.say("");
    t.say("tmux is not installed, and every session on this machine is a tmux");
    t.say("session — without it this node can join but cannot open a terminal.");

    let Some(inst) = installer() else {
        t.say("");
        t.say("  No package manager I recognise. Install tmux and re-run `nook setup`:");
        t.say("    macOS         brew install tmux");
        t.say("    Debian/Ubuntu sudo apt-get install tmux");
        t.say("    Fedora/RHEL   sudo dnf install tmux");
        return Ok(false);
    };

    let prompt = if inst.needs_sudo {
        format!(
            "Install it now with {} (asks for your password)?",
            inst.name
        )
    } else {
        format!("Install it now with {}?", inst.name)
    };
    if !t.confirm(&prompt, true)? {
        t.say("  Skipped. Sessions will fail until tmux is installed.");
        return Ok(false);
    }

    t.say(&format!("▸ {}", inst.argv.join(" ")));
    let status = std::process::Command::new(inst.argv[0])
        .args(&inst.argv[1..])
        .status();

    match status {
        Ok(s) if s.success() && crate::capabilities::detect_tmux().is_some() => {
            t.say("✓ tmux installed");
            Ok(true)
        }
        // Exit zero but still missing: a package manager can "succeed" having
        // installed nothing useful, and claiming victory then would reproduce
        // the exact confusion this function exists to remove.
        Ok(_) => {
            t.say("  That did not leave a working tmux. Install it by hand and re-run setup.");
            Ok(false)
        }
        Err(e) => {
            t.say(&format!("  Could not run {}: {e}", inst.argv[0]));
            Ok(false)
        }
    }
}
