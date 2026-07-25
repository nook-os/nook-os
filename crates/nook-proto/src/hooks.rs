//! The canonical NookOS hook set — the single source of truth both the node
//! (which writes it into `~/.claude/settings.json` at `nook setup`) and the
//! control plane (which stores it as managed content, MAIN-78) build from.
//!
//! It lives here, in the crate both sides already share, so a hook the fleet is
//! told about and a hook a node actually installs can never be two different
//! things. The node keeps its own file-merge logic; this end owns only *what*
//! the hooks are and the exact settings fragment they render to.
//!
//! Claude Code fires hooks at points in its lifecycle. Pointing them at
//! `nook notify` / `nook agent-state` closes the loop the rest of NookOS is
//! built around: the agent says what happened, the control plane records it, and
//! every connected UI, phone and channel hears about it at once.

use serde_json::{json, Value};

/// What a hook does when its Claude event fires. Two kinds, deliberately
/// separate: a `Notify` lands in the inbox (toast + phone + channels), a `State`
/// is an ephemeral report that drives the terminal-tab spinner and never touches
/// the inbox. Some events do both — a `Stop` both notifies "finished" and
/// reports `idle` — which is why they are separate entries rather than one.
pub enum Action {
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
pub struct Hook {
    /// The Claude Code hook event name — the key under `hooks` in settings.
    pub event: &'static str,
    pub action: Action,
}

/// The set installed, in the order they appear in the confirmation output.
pub const HOOKS: &[Hook] = &[
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
pub fn marker(h: &Hook) -> String {
    match h.action {
        Action::Notify { kind, .. } => format!("--kind {kind}"),
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
pub fn command(h: &Hook) -> String {
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

/// The `~/.claude/settings.json` `hooks` fragment for the whole set: the object
/// that merges under the top-level `"hooks"` key, one array per event, each
/// entry `{ "matcher": "", "hooks": [{ "type": "command", "command": … }] }`.
///
/// This is the apply-ready representation the control plane stores (MAIN-78 AC-4)
/// and, later, a node will merge in exactly as `wizard::hooks::install` does per
/// hook — so what is stored is what is applied. Deterministic: events are keyed
/// in a `serde_json` map (sorted on serialize) and entries within an event keep
/// `HOOKS` order, so its sha256 is stable across boots.
pub fn claude_settings_fragment() -> Value {
    let mut hooks: serde_json::Map<String, Value> = serde_json::Map::new();
    for h in HOOKS {
        let list = hooks
            .entry(h.event.to_string())
            .or_insert_with(|| json!([]));
        // Every entry is an array element; `HOOKS` only ever yields arrays here.
        if let Some(arr) = list.as_array_mut() {
            arr.push(json!({
                "matcher": "",
                "hooks": [{ "type": "command", "command": command(h) }],
            }));
        }
    }
    Value::Object(hooks)
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
    /// on. A duplicate would make one hook's removal take another's entry.
    #[test]
    fn markers_are_unique() {
        let mut m: Vec<String> = HOOKS.iter().map(marker).collect();
        let n = m.len();
        m.sort();
        m.dedup();
        assert_eq!(m.len(), n, "two hooks share a marker");
    }

    /// The three agent-state hooks that drive the tab indicator are present, and
    /// SubagentStop is deliberately NOT among them (the main agent is still
    /// running when a subagent finishes).
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

    /// A state report is ephemeral — `nook agent-state`, never `nook notify`.
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

    /// No escaped quotes in a notify command — they do not survive the trip
    /// through JSON to a shell, and once ended up in a notification body. And
    /// every notify carries the session deep-link so "Claude needs you" links to
    /// the terminal (state reports do not — the server derives the link there).
    #[test]
    fn notify_commands_are_shaped_right() {
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
                assert!(
                    c.contains("${NOOK_SESSION_ID:+--session $NOOK_SESSION_ID}"),
                    "{}: no session link: {c}",
                    h.event
                );
                assert_eq!(
                    c.matches('"').count() % 2,
                    0,
                    "{}: unbalanced quotes",
                    h.event
                );
            }
        }
    }

    /// The blocked-waiting notification is the point of the inbox set: present,
    /// and a `warning` so it stands out from a routine finish.
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

    /// The rendered fragment is a valid, deterministic settings.json `hooks`
    /// object: one array per event, each entry a `{matcher, hooks:[{type,command}]}`.
    #[test]
    fn fragment_is_a_valid_settings_hooks_object() {
        let frag = claude_settings_fragment();
        let obj = frag.as_object().expect("fragment is an object");
        // Every wired event has an entry, and Stop/Notification carry two hooks.
        assert_eq!(obj["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(obj["Notification"].as_array().unwrap().len(), 2);
        assert_eq!(obj["SubagentStop"].as_array().unwrap().len(), 1);
        assert_eq!(obj["UserPromptSubmit"].as_array().unwrap().len(), 1);
        for (_event, list) in obj {
            for entry in list.as_array().unwrap() {
                assert_eq!(entry["matcher"], "");
                let inner = entry["hooks"].as_array().unwrap();
                assert_eq!(inner[0]["type"], "command");
                assert!(inner[0]["command"].as_str().unwrap().starts_with("nook "));
            }
        }
        // Deterministic: same bytes every call (its sha is stored, MAIN-78).
        assert_eq!(
            serde_json::to_string(&frag).unwrap(),
            serde_json::to_string(&claude_settings_fragment()).unwrap()
        );
    }
}
