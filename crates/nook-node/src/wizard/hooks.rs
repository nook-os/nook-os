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

/// What a hook does when its Claude event fires. Two kinds, deliberately
/// separate: a `Notify` lands in the inbox (toast + phone + channels), a `State`
/// is an ephemeral report that drives the terminal-tab spinner and never
/// touches the inbox. Some events do both — a `Stop` both notifies "finished"
/// and reports `idle` — which is why they are separate entries rather than one.
enum Action {
    Notify {
        title: &'static str,
        level: &'static str,
        kind: &'static str,
    },
    State {
        value: &'static str,
    },
}

/// One Claude Code event, mapped to a single action. An event can appear more
/// than once (a notify AND a state).
struct Hook {
    /// The Claude Code hook event name — the key under `hooks` in settings.
    event: &'static str,
    action: Action,
}

/// The set installed, in the order they appear in the confirmation output.
const HOOKS: &[Hook] = &[
    // ── Inbox notifications ───────────────────────────────────────────────
    Hook {
        event: "Stop",
        action: Action::Notify {
            title: "Claude Code finished",
            level: "success",
            kind: "agent.finished",
        },
    },
    Hook {
        // Claude fires this when it wants input or a permission — stopped, and
        // waiting on a human. `warning` so it stands out from routine finishes.
        event: "Notification",
        action: Action::Notify {
            title: "Claude needs you",
            level: "warning",
            kind: "agent.waiting",
        },
    },
    Hook {
        event: "SubagentStop",
        action: Action::Notify {
            title: "A subagent finished",
            level: "info",
            kind: "agent.subagent_finished",
        },
    },
    // ── Ephemeral tab state ───────────────────────────────────────────────
    // Running the moment a prompt is submitted, waiting when Claude blocks,
    // idle when the turn ends. `SubagentStop` is intentionally NOT here — the
    // main agent is still running while a subagent finishes (NG-3).
    Hook {
        event: "UserPromptSubmit",
        action: Action::State { value: "running" },
    },
    Hook {
        event: "Notification",
        action: Action::State { value: "waiting" },
    },
    Hook {
        event: "Stop",
        action: Action::State { value: "idle" },
    },
];

/// The unique substring that identifies THIS hook's command, for idempotent
/// re-install and uninstall — a notify by its `--kind`, a state report by its
/// `agent-state <value>`.
fn marker(h: &Hook) -> String {
    match h.action {
        Action::Notify { kind, .. } => format!("--kind {kind}"),
        Action::State { value } => format!("agent-state {value}"),
    }
}

/// A short human label for the confirmation output.
fn label(h: &Hook) -> String {
    match h.action {
        Action::Notify { kind, .. } => format!("notify {kind}"),
        Action::State { value } => format!("agent-state {value}"),
    }
}

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
    match h.action {
        // `${NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}` expands to the two
        // words `--session <uuid>` only when the var is set — so "Claude needs
        // you" deep-links to the terminal in a nook session, and an agent in a
        // plain terminal still notifies, just without a link.
        Action::Notify { title, level, kind } => format!(
            "nook notify \"{title}\" --level {level} --kind {kind} \
             --body \"${{PWD##*/}} on $(hostname)\" \
             ${{NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}} >/dev/null 2>&1 || true",
        ),
        // Ephemeral: `nook agent-state` is a no-op outside a nook session, so
        // this is harmless in a plain terminal and never touches the inbox.
        Action::State { value } => {
            format!("nook agent-state {value} >/dev/null 2>&1 || true")
        }
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

    /// Every hook's marker is unique — the string re-install and uninstall key
    /// on. A duplicate would make one hook's removal take another's entry. This
    /// matters more now that one EVENT (Notification, Stop) carries two hooks.
    #[test]
    fn markers_are_unique() {
        let mut m: Vec<String> = HOOKS.iter().map(marker).collect();
        let n = m.len();
        m.sort();
        m.dedup();
        assert_eq!(m.len(), n, "two hooks share a marker");
    }

    /// The agent-state hooks that drive the tab indicator are all present, and
    /// SubagentStop is deliberately NOT among them (NG-3 — the main agent is
    /// still running when a subagent finishes).
    #[test]
    fn the_three_agent_states_are_wired() {
        let states: Vec<(&str, &str)> = HOOKS
            .iter()
            .filter_map(|h| match h.action {
                Action::State { value } => Some((h.event, value)),
                _ => None,
            })
            .collect();
        assert!(
            states.contains(&("UserPromptSubmit", "running")),
            "{states:?}"
        );
        assert!(states.contains(&("Notification", "waiting")), "{states:?}");
        assert!(states.contains(&("Stop", "idle")), "{states:?}");
        assert!(
            !states.iter().any(|(e, _)| *e == "SubagentStop"),
            "SubagentStop must not change the session state: {states:?}"
        );
    }

    /// A state report is ephemeral — it calls `nook agent-state`, NOT
    /// `nook notify`, so a per-turn running/idle stream never touches the inbox.
    #[test]
    fn state_hooks_do_not_notify() {
        for h in HOOKS {
            if let Action::State { value } = h.action {
                let c = command(h);
                assert!(c.contains(&format!("agent-state {value}")), "{c}");
                assert!(
                    !c.contains("nook notify"),
                    "state hook must not notify: {c}"
                );
            }
        }
    }

    /// The blocked-waiting notification is the point of the inbox set, so it had
    /// better be present and stand out from routine completion.
    #[test]
    fn the_waiting_notification_is_a_warning() {
        let waiting = HOOKS
            .iter()
            .find(|h| {
                matches!(
                    h.action,
                    Action::Notify {
                        kind: "agent.waiting",
                        ..
                    }
                )
            })
            .expect("a notification for when Claude is blocked");
        assert_eq!(waiting.event, "Notification");
        assert!(matches!(
            waiting.action,
            Action::Notify {
                level: "warning",
                ..
            }
        ));
    }

    /// Notify commands carry the session deep-link; state commands do not need
    /// it (the server derives the link on the `agent-state` path from the id).
    #[test]
    fn notify_commands_link_to_their_session() {
        for h in HOOKS {
            if matches!(h.action, Action::Notify { .. }) {
                let c = command(h);
                assert!(
                    c.contains("${NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}"),
                    "{}: no session link: {c}",
                    h.event
                );
            }
        }
    }

    /// No escaped quotes in a notify command — they do not survive the trip
    /// through JSON to a shell, and once ended up in a notification body as a
    /// literal character. It looked right everywhere except the output.
    #[test]
    fn notify_commands_have_no_escaped_quotes() {
        for h in HOOKS {
            if matches!(h.action, Action::Notify { .. }) {
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
    }

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
