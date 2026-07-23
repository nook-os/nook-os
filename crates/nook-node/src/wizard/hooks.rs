//! `nook hooks install` — tell the fleet what an agent is doing.
//!
//! Claude Code fires hooks at points in its lifecycle. Pointing them at
//! `nook notify` closes the loop the rest of NookOS was built around: the agent
//! says what happened, the control plane records it, and every connected UI,
//! every phone and every Slack channel hears about it at once. Nothing polls,
//! and nothing has to watch a terminal for the output to stop changing.
//!
//! Three events are wired, chosen for signal rather than completeness:
//!
//! - **Stop** → `agent.finished`. The turn is done.
//! - **Notification** → `agent.waiting`. Claude is BLOCKED — waiting for input
//!   or a permission. This is the one worth a buzz in your pocket: "finished"
//!   can wait, "stuck until you come back" cannot.
//! - **SubagentStop** → `agent.subagent_finished`. A delegated task returned.
//!
//! Deliberately NOT wired: `SessionStart` and `PreCompact` fire on every resume
//! and every compaction, so they are heartbeats disguised as events — a lot of
//! noise for a fact the UI already shows. `PreToolUse`/`PostToolUse` fire per
//! tool call, which is a firehose. If you want those, they are one entry each
//! in the same file; this installs the set that earns its place.
//!
//! Written by a command rather than documented in a README because the
//! alternative is asking somebody to hand-edit JSON that already has their own
//! settings in it — which is exactly the operation people get wrong once and
//! then avoid.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// One Claude Code event, mapped to the notification it raises.
struct Hook {
    /// The Claude Code hook event name — the key under `hooks` in settings.
    event: &'static str,
    /// The `--kind` the notification carries, and the marker that identifies
    /// THIS hook for idempotent re-install: unique per event.
    kind: &'static str,
    level: &'static str,
    title: &'static str,
}

/// The set installed, in the order they appear in the confirmation output.
const HOOKS: &[Hook] = &[
    Hook {
        event: "Stop",
        kind: "agent.finished",
        level: "success",
        title: "Claude Code finished",
    },
    Hook {
        // The high-value one. Claude fires this when it wants input or a
        // permission — i.e. it has stopped and is waiting on a human. `warning`
        // so it stands out from the routine "finished".
        event: "Notification",
        kind: "agent.waiting",
        level: "warning",
        title: "Claude needs you",
    },
    Hook {
        event: "SubagentStop",
        kind: "agent.subagent_finished",
        level: "info",
        title: "A subagent finished",
    },
];

/// The shell command a hook runs.
///
/// `|| true` so a control plane that is down, or a machine that is not logged
/// in, can never make an agent's turn look like it failed. A missed
/// notification is a small loss; a hook that breaks the tool it is attached to
/// is a large one.
///
/// `${PWD##*/}` rather than `$(basename "$PWD")`: the command travels through
/// JSON, so every quote in it has to survive two levels of escaping — and it
/// did not, once. The escaped inner quotes reached the shell literally and
/// `basename` was handed `"/path/to/repo"` WITH the quotes, so every
/// notification read `repo" on host`. Parameter expansion needs no quoting, no
/// subshell, and cannot be mangled by whatever writes the settings file.
fn command(h: &Hook) -> String {
    // `${NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}` expands to the two words
    // `--session <uuid>` only when the var is set — i.e. when the agent runs
    // inside a nook session — and to nothing otherwise. A session id has no
    // spaces, so it needs no quoting; and `:+` means an agent running in a
    // plain terminal (no NOOK_SESSION_ID) still notifies, just without a link.
    // This is what makes "Claude needs you" open the actual terminal.
    format!(
        "nook notify \"{title}\" --level {level} --kind {kind} \
         --body \"${{PWD##*/}} on $(hostname)\" \
         ${{NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}} >/dev/null 2>&1 || true",
        title = h.title,
        level = h.level,
        kind = h.kind,
    )
}

/// Does this settings entry belong to the given hook?
///
/// Matched by the `--kind`, which is unique per hook, so re-installing updates
/// in place instead of stacking a second copy that fires twice.
fn is_ours(entry: &Value, kind: &str) -> bool {
    serde_json::to_string(entry)
        .unwrap_or_default()
        .contains(&format!("--kind {kind}"))
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME is not set")?,
    ))
}

/// Add (or refresh) the NookOS hooks in Claude Code's settings.
///
/// Merges rather than overwrites: the file usually holds somebody's own hooks,
/// permissions and model choice, and losing those to gain a notification would
/// be a bad trade. Re-running replaces only the entries NookOS put there, which
/// is how it stays safe to run from an installer.
pub fn install(dry_run: bool) -> Result<()> {
    let path = home()?.join(".claude/settings.json");

    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON — fix it first", path.display()))?,
        _ => json!({}),
    };

    let hooks = root
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("`hooks` in settings.json is not an object")?;

    for h in HOOKS {
        let list = hooks
            .entry(h.event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("`hooks.{}` in settings.json is not an array", h.event))?;
        list.retain(|e| !is_ours(e, h.kind));
        list.push(json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": command(h) }],
        }));
    }

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

    println!("✓ Claude Code will now tell the fleet what it is doing:");
    for h in HOOKS {
        println!("    {:16} → {}", h.event, h.kind);
    }
    println!("  {}", path.display());
    println!();
    println!("  Test it without waiting for an agent:");
    println!("    nook notify 'hello' --level success");
    Ok(())
}

/// Remove them again.
pub fn uninstall() -> Result<()> {
    let path = home()?.join(".claude/settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("Nothing to remove — {} does not exist.", path.display());
        return Ok(());
    };
    let mut root: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    let mut removed = 0usize;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for h in HOOKS {
            if let Some(list) = hooks.get_mut(h.event).and_then(Value::as_array_mut) {
                let before = list.len();
                list.retain(|e| !is_ours(e, h.kind));
                removed += before - list.len();
            }
        }
    }

    if removed == 0 {
        println!("No NookOS hooks were installed.");
        return Ok(());
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
    println!("✓ removed {removed} NookOS hook(s) from {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook must never be the reason an agent's turn looks broken.
    #[test]
    fn no_hook_can_fail_the_agent() {
        for h in HOOKS {
            let c = command(h);
            assert!(c.ends_with("|| true"), "{}: {c}", h.event);
            assert!(
                c.contains(">/dev/null 2>&1"),
                "{}: no output into the agent: {c}",
                h.event
            );
        }
    }

    /// Each hook carries a unique kind — the marker re-install and uninstall
    /// key on. A duplicate would make one hook's removal take another's entry.
    #[test]
    fn kinds_are_unique() {
        let mut kinds: Vec<&str> = HOOKS.iter().map(|h| h.kind).collect();
        let n = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), n, "two hooks share a --kind");
    }

    /// Every hook passes the session through, so a notification can deep-link
    /// to the terminal. Guarded with `:+` so it only appears when set — an
    /// agent outside a nook session still notifies, just without a link.
    #[test]
    fn every_command_links_to_its_session() {
        for h in HOOKS {
            let c = command(h);
            assert!(
                c.contains("${NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}"),
                "{}: no session link: {c}",
                h.event
            );
        }
    }

    /// The blocked-waiting hook is the point of adding more than one, so it had
    /// better be present and stand out from routine completion.
    #[test]
    fn the_waiting_hook_exists_and_is_a_warning() {
        let waiting = HOOKS
            .iter()
            .find(|h| h.kind == "agent.waiting")
            .expect("a hook for when Claude is blocked");
        assert_eq!(waiting.event, "Notification");
        assert_eq!(waiting.level, "warning");
    }

    /// No escaped quotes, because they do not survive the trip through JSON to
    /// a shell — an escaped quote once ended up in the notification body as a
    /// literal character. It looked right everywhere except the output.
    #[test]
    fn no_command_has_escaped_quotes() {
        for h in HOOKS {
            let c = command(h);
            assert!(!c.contains("\\\""), "{}: {c}", h.event);
            assert!(
                !c.contains("basename"),
                "{}: use ${{PWD##*/}}: {c}",
                h.event
            );
            assert!(c.contains("${PWD##*/}"), "{}: {c}", h.event);
            assert_eq!(
                c.matches('"').count() % 2,
                0,
                "{}: unbalanced quotes",
                h.event
            );
        }
    }

    /// Installing is idempotent: two runs leave one entry per event, not two.
    /// The bug this guards against is a hook that fires twice per turn because
    /// re-running stacked a second copy.
    #[test]
    fn reinstall_is_idempotent() {
        let mut hooks = serde_json::Map::new();
        for _ in 0..2 {
            for h in HOOKS {
                let list = hooks
                    .entry(h.event.to_string())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap();
                list.retain(|e| !is_ours(e, h.kind));
                list.push(json!({
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": command(h) }],
                }));
            }
        }
        for h in HOOKS {
            let n = hooks[h.event].as_array().unwrap().len();
            assert_eq!(n, 1, "{} has {n} entries after two installs", h.event);
        }
    }
}
