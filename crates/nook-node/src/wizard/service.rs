//! Installing the agent as a service.
//!
//! Defaults to a **user** unit. The agent's whole job is running the person's
//! own tooling in their own checkouts, so it needs nothing root can give it,
//! and asking for sudo to do that teaches a habit worth not teaching. A system
//! unit remains right for a shared box, where the agent should survive the
//! person logging out and belong to a service account.

use anyhow::{bail, Context, Result};

use super::generate::{node_launchd_plist, node_unit};
use super::tty::Tty;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// `systemctl --user`, plus lingering so it survives logout.
    UserSystemd,
    /// `/etc/systemd/system`, running as a named user.
    SystemSystemd,
    /// A launchd agent on macOS: runs as you, starts at login, restarts itself.
    Launchd,
    /// The node in a container.
    Docker,
    /// Nothing installed; print the command.
    None,
}

fn have(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ask how the agent should be kept running.
pub fn choose(t: &mut Tty) -> Result<Service> {
    // macOS has no systemd; launchd is the equivalent and is what a Mac user
    // expects when they ask for "always running". Offering only "run it
    // yourself" there was a gap, not a design decision.
    if have("launchctl") && !have("systemctl") {
        let pick = t.choose(
            "How should the agent stay running?",
            &[
                (
                    "launchd agent",
                    "Starts at login and restarts itself. No sudo — it runs as you.",
                ),
                (
                    "Docker container",
                    "Only sees tooling inside the container — good for CI, poor for a laptop.",
                ),
                ("Don't install a service", "Print the command and stop."),
            ],
            0,
        )?;
        return Ok(match pick {
            0 => Service::Launchd,
            1 => Service::Docker,
            _ => Service::None,
        });
    }

    if !have("systemctl") {
        // Neither init system: a container, or something unusual.
        let pick = t.choose(
            "How should the agent stay running?",
            &[
                (
                    "Don't install a service",
                    "Print the command; run it yourself or use your own supervisor.",
                ),
                (
                    "Docker container",
                    "Only sees tooling inside the container — good for CI, poor for a laptop.",
                ),
            ],
            0,
        )?;
        return Ok(if pick == 0 {
            Service::None
        } else {
            Service::Docker
        });
    }

    let pick = t.choose(
        "How should the agent stay running?",
        &[
            (
                "systemd user service",
                "No sudo. Runs as you, which is what it needs — it runs your tooling.",
            ),
            (
                "systemd system service",
                "Needs sudo. Right for a shared or headless machine.",
            ),
            (
                "Docker container",
                "Only sees tooling inside the container — good for CI, poor for a laptop.",
            ),
            ("Don't install a service", "Print the command and stop."),
        ],
        0,
    )?;
    Ok(match pick {
        0 => Service::UserSystemd,
        1 => Service::SystemSystemd,
        2 => Service::Docker,
        _ => Service::None,
    })
}

impl Service {
    /// The value stored in `node.toml`. `None` means nothing will restart this
    /// agent, which is what makes self-update unsafe.
    pub fn config_value(self) -> Option<&'static str> {
        match self {
            Service::UserSystemd => Some("systemd-user"),
            Service::SystemSystemd => Some("systemd-system"),
            Service::Launchd => Some("launchd"),
            Service::Docker => Some("docker"),
            Service::None => None,
        }
    }
}

/// Write, enable and start the unit.
pub fn install(t: &mut Tty, service: Service, exec: &str) -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let user = whoami();

    match service {
        Service::None => {
            t.say("");
            t.say(&format!("  Start the agent with:  {exec} run"));
            Ok(())
        }

        Service::UserSystemd => {
            let dir = format!("{home}/.config/systemd/user");
            std::fs::create_dir_all(&dir)?;
            let path = format!("{dir}/nook-node.service");
            std::fs::write(&path, node_unit(true, exec, &home, &user))?;
            t.say(&format!("✓ {path}"));

            run(&["systemctl", "--user", "daemon-reload"])?;
            run(&["systemctl", "--user", "enable", "--now", "nook-node"])?;

            // Without lingering the user manager stops at logout and takes the
            // agent with it — which on a headless box means the node is online
            // exactly as long as someone has an ssh session open.
            if run(&["loginctl", "enable-linger", &user]).is_err() {
                t.say("  Note: could not enable lingering. The agent will stop when you log out.");
                t.say(&format!(
                    "        Fix with: sudo loginctl enable-linger {user}"
                ));
            }
            t.say("✓ systemd user service enabled");
            t.say("  Logs:  journalctl --user -u nook-node -f");
            Ok(())
        }

        Service::SystemSystemd => {
            let unit = node_unit(false, exec, &home, &user);
            let tmp = std::env::temp_dir().join("nook-node.service");
            std::fs::write(&tmp, unit)?;
            t.say("Writing /etc/systemd/system/nook-node.service (sudo)");
            run(&[
                "sudo",
                "install",
                "-m644",
                tmp.to_str().unwrap(),
                "/etc/systemd/system/nook-node.service",
            ])?;
            let _ = std::fs::remove_file(&tmp);
            run(&["sudo", "systemctl", "daemon-reload"])?;
            run(&["sudo", "systemctl", "enable", "--now", "nook-node"])?;
            t.say("✓ systemd system service enabled");
            t.say("  Logs:  sudo journalctl -u nook-node -f");
            Ok(())
        }

        Service::Launchd => {
            let label = "dev.nookos.node";
            let dir = format!("{home}/Library/LaunchAgents");
            std::fs::create_dir_all(&dir)?;
            let path = format!("{dir}/{label}.plist");
            std::fs::write(&path, node_launchd_plist(exec, &home, label))?;
            t.say(&format!("✓ {path}"));

            // Replace any previous copy first: `load` on an already-loaded
            // label fails, and a re-run of setup is an ordinary thing to do.
            let _ = run(&["launchctl", "unload", &path]);
            run(&["launchctl", "load", "-w", &path])?;

            t.say("✓ launchd agent loaded");
            t.say(&format!(
                "  Logs:  tail -f {home}/Library/Logs/nook-node.log"
            ));
            t.say(&format!("  Stop:  launchctl unload {path}"));
            Ok(())
        }

        Service::Docker => {
            if !have("docker") {
                bail!("docker is not installed");
            }
            let version = format!("v{}", env!("CARGO_PKG_VERSION"));
            t.say("");
            t.say("  A containerised node can only use tooling inside the container.");
            t.say("  Run it with:");
            t.say("");
            t.say(&format!(
                "    docker run -d --name nook-node --restart unless-stopped \\\n\
                 \x20     -v ~/.config/nook:/root/.config/nook \\\n\
                 \x20     -v ~/workspace:/root/workspace \\\n\
                 \x20     ghcr.io/nook-os/nook-node:{version}"
            ));
            Ok(())
        }
    }
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "nook".into())
        })
}

fn run(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(args[0])
        .args(&args[1..])
        .status()
        .with_context(|| format!("cannot run {}", args[0]))?;
    if !status.success() {
        bail!("{} exited {}", args.join(" "), status);
    }
    Ok(())
}
