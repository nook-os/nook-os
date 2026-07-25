//! `nook hooks install` — tell the fleet what an agent is doing.
//!
//! Claude Code fires hooks at points in its lifecycle. Pointing them at
//! `nook notify` closes the loop the rest of NookOS was built around: the agent
//! says what happened, the control plane records it, and every connected UI,
//! every phone and every Slack channel hears about it at once. Nothing polls,
//! and nothing has to watch a terminal for the output to stop changing.
//!
//! Two families of hook are wired. **Notifications** land in the inbox (toast +
//! phone + channels): Stop → `agent.finished`, Notification → `agent.waiting`
//! (BLOCKED — the one worth a buzz in your pocket), SubagentStop →
//! `agent.subagent_finished`. **State reports** are ephemeral and drive the
//! terminal-tab spinner without touching the inbox: UserPromptSubmit → running,
//! Notification → waiting, Stop → idle, via `nook agent-state`.
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

// The hook set itself — what the hooks are and the command each runs — is the
// shared source of truth in `nook_proto::hooks`, so the fleet the control plane
// tells about a hook and the file this end writes can never drift apart
// (MAIN-78). This module keeps the local half: merging them into the user's
// `~/.claude/settings.json` without disturbing their own settings.
use nook_proto::hooks::{command, marker, Action, Hook, HOOKS};

/// A short human label for the confirmation output.
fn label(h: &Hook) -> String {
    match h.action {
        Action::Notify { kind, .. } => format!("notify {kind}"),
        Action::State { value } => format!("agent-state {value}"),
    }
}

/// Does this settings entry belong to the given hook? Matched by its unique
/// marker, so re-installing updates in place instead of stacking a duplicate.
fn is_ours(entry: &Value, marker: &str) -> bool {
    serde_json::to_string(entry)
        .unwrap_or_default()
        .contains(marker)
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
        list.retain(|e| !is_ours(e, &marker(h)));
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
        println!("    {:16} → {}", h.event, label(h));
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
                list.retain(|e| !is_ours(e, &marker(h)));
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

    // The hook set's own shape (commands, markers, wired states) is tested in
    // `nook_proto::hooks`, where the set now lives. What is node-specific — and
    // tested here — is the merge into the user's settings file.

    /// Installing is idempotent: two runs leave one entry PER HOOK, not two —
    /// even for an event that carries two hooks. The bug this guards against is
    /// a hook that fires twice per turn because re-running stacked a copy.
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
                list.retain(|e| !is_ours(e, &marker(h)));
                list.push(json!({
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": command(h) }],
                }));
            }
        }
        // Each hook appears exactly once, found by its own marker.
        for h in HOOKS {
            let list = hooks[h.event].as_array().unwrap();
            let n = list.iter().filter(|e| is_ours(e, &marker(h))).count();
            assert_eq!(n, 1, "{} / {} has {n} entries", h.event, marker(h));
        }
    }
}
