//! `nook hooks install` — tell the fleet when an agent finishes.
//!
//! Claude Code fires a `Stop` hook when it finishes a turn. Pointing that at
//! `nook notify` closes the loop the rest of NookOS was built around: the agent
//! says it is done, the control plane records it, and every connected UI, every
//! phone and every Slack channel hears about it at once. Nothing polls, and
//! nothing has to watch a terminal for the output to stop changing.
//!
//! Written by a command rather than documented in a README because the
//! alternative is asking somebody to hand-edit JSON that already has their own
//! settings in it — which is exactly the operation people get wrong once and
//! then avoid.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The command the hook runs.
///
/// `|| true` so a control plane that is down, or a machine that is not logged
/// in, can never make an agent's turn look like it failed. A missed
/// notification is a small loss; a hook that breaks the tool it is attached to
/// is a large one.
fn command(label: &str) -> String {
    // `${PWD##*/}` rather than `$(basename "$PWD")`.
    //
    // The command travels through JSON, so every quote in it has to survive
    // two levels of escaping — and it did not: the escaped inner quotes
    // reached the shell literally and `basename` was handed `"/path/to/repo"`
    // WITH the quotes, so every notification read `repo" on host`. Parameter
    // expansion needs no quoting, no subshell, and cannot be mangled by
    // whatever writes the settings file.
    format!(
        "nook notify \"{label}\" --level success --kind agent.finished \
         --body \"${{PWD##*/}} on $(hostname)\" >/dev/null 2>&1 || true"
    )
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME is not set")?,
    ))
}

/// Add (or refresh) the Stop hook in Claude Code's settings.
///
/// Merges rather than overwrites: the file usually holds somebody's own hooks,
/// permissions and model choice, and losing those to gain a notification would
/// be a bad trade. Re-running replaces only the entry NookOS put there, which
/// is how it stays safe to run from an installer.
pub fn install(dry_run: bool) -> Result<()> {
    let path = home()?.join(".claude/settings.json");

    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON — fix it first", path.display()))?,
        _ => json!({}),
    };

    let cmd = command("Claude Code finished");
    let entry = json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": cmd }],
    });

    let hooks = root
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let stop = hooks
        .as_object_mut()
        .context("`hooks` in settings.json is not an object")?
        .entry("Stop")
        .or_insert_with(|| json!([]));
    let list = stop
        .as_array_mut()
        .context("`hooks.Stop` in settings.json is not an array")?;

    // Ours is identified by the command it runs, so re-running updates in place
    // instead of stacking a second copy that fires twice.
    list.retain(|e| {
        !serde_json::to_string(e)
            .unwrap_or_default()
            .contains("--kind agent.finished")
    });
    list.push(entry);

    if dry_run {
        println!("Would write {}:\n", path.display());
        println!("{}", serde_json::to_string_pretty(&root)?);
        return Ok(());
    }

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")
        .with_context(|| format!("cannot write {}", path.display()))?;

    println!("✓ Claude Code will now tell the fleet when it finishes.");
    println!("  {}", path.display());
    println!();
    println!("  Test it without waiting for an agent:");
    println!("    nook notify 'hello' --level success");
    Ok(())
}

/// Remove it again.
pub fn uninstall() -> Result<()> {
    let path = home()?.join(".claude/settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("Nothing to remove — {} does not exist.", path.display());
        return Ok(());
    };
    let mut root: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    let removed = root
        .get_mut("hooks")
        .and_then(|h| h.get_mut("Stop"))
        .and_then(Value::as_array_mut)
        .map(|list| {
            let before = list.len();
            list.retain(|e| {
                !serde_json::to_string(e)
                    .unwrap_or_default()
                    .contains("--kind agent.finished")
            });
            before - list.len()
        })
        .unwrap_or(0);

    if removed == 0 {
        println!("No NookOS finish hook was installed.");
        return Ok(());
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
    println!("✓ removed the NookOS finish hook from {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hook must never be the reason an agent's turn looks broken.
    #[test]
    fn the_command_cannot_fail_the_agent() {
        let c = command("x");
        assert!(c.ends_with("|| true"), "{c}");
        assert!(
            c.contains(">/dev/null 2>&1"),
            "no output into the agent: {c}"
        );
    }

    /// Identified by its own marker, so re-running replaces rather than stacks.
    #[test]
    fn the_command_is_self_identifying() {
        assert!(command("x").contains("--kind agent.finished"));
    }

    /// No escaped quotes, because they do not survive the trip.
    ///
    /// The command is written into JSON and read back by a shell. A `\"` in
    /// the source became a literal `\"` on the command line, so `basename` was
    /// handed a path WITH quotes around it and every notification body read
    /// `myrepo" on host`. It looked right in the source, right in the settings
    /// file, and wrong only in the output nobody diffs.
    #[test]
    fn the_command_contains_no_escaped_quotes() {
        let c = command("Claude Code finished");
        assert!(
            !c.contains("\\\""),
            "an escaped quote reaches the shell literally and ends up in the \
             notification body: {c}"
        );
        assert!(
            !c.contains("basename"),
            "use ${{PWD##*/}} — parameter expansion needs no quoting and no \
             subshell, so it cannot be mangled: {c}"
        );
        assert!(c.contains("${PWD##*/}"), "{c}");
    }

    /// The shape a shell will actually see: balanced double quotes.
    #[test]
    fn the_quotes_are_balanced() {
        let c = command("Claude Code finished");
        assert_eq!(
            c.matches('"').count() % 2,
            0,
            "odd number of quotes means the shell sees an unterminated \
             string: {c}"
        );
    }
}
