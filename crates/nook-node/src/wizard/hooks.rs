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

/// The nook marker an entry carries, if any — re-keying removal to the pushed
/// content rather than the compile-time set. A nook-managed hook command embeds
/// exactly one marker: a notify by its `--kind <kind>`, a state report by its
/// `agent-state <value>` (see `nook_proto::hooks::marker`). A user's own hook
/// carries neither, so it is not "ours" and survives the merge.
fn marker_of(entry: &Value) -> Option<String> {
    let cmd = entry
        .get("hooks")?
        .as_array()?
        .first()?
        .get("command")?
        .as_str()?;
    if let Some(rest) = cmd.split("agent-state ").nth(1) {
        let value = rest.split_whitespace().next()?;
        return Some(format!("agent-state {value}"));
    }
    if let Some(rest) = cmd.split("--kind ").nth(1) {
        let kind = rest.split_whitespace().next()?;
        return Some(format!("--kind {kind}"));
    }
    None
}

/// What applying a pushed hooks fragment did.
#[derive(Debug)]
pub struct HooksApply {
    /// The settings file targeted.
    pub path: PathBuf,
    /// False when the file already carried this exact managed set, so nothing
    /// was written (AC-4 — connect-replay is a no-op).
    pub wrote: bool,
}

/// Merge a control-plane-pushed managed hooks fragment into the user's
/// `settings.json` (MAIN-105 AC-2).
///
/// Same merge semantics as [`install`], but re-keyed to the PUSHED content
/// rather than the compile-time `HOOKS`: for every event the fragment carries,
/// the nook-managed entries already in the file (identified by their marker) are
/// dropped and the pushed entries put in their place, while the user's own hooks
/// and every other setting are left untouched. Applying the same fragment twice
/// is byte-identical, and when the file already carries this exact set nothing is
/// written at all — the sha-skip AC-4 asks for, expressed as the natural
/// checksum of a merge: the resulting bytes are unchanged.
///
/// A missing file is created; an existing file that is not valid JSON is a
/// reported error (the caller keeps the node running — AC-2), never a silent
/// overwrite of something a person may have hand-edited.
pub fn apply_pushed(fragment_json: &str) -> Result<HooksApply> {
    let path = home()?.join(".claude/settings.json");
    let original = std::fs::read_to_string(&path).ok();

    match merged_settings(original.as_deref(), fragment_json)? {
        // Already carries this exact set — write nothing (AC-4).
        None => Ok(HooksApply { path, wrote: false }),
        Some(rendered) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, &rendered)
                .with_context(|| format!("cannot write {}", path.display()))?;
            Ok(HooksApply { path, wrote: true })
        }
    }
}

/// The pure merge, split out so it is testable without touching `$HOME`: given
/// the current `settings.json` text (if the file exists) and the pushed fragment
/// JSON, return the text to write — or `None` when the file already carries this
/// exact managed set, so the caller writes nothing. Invalid current JSON, or a
/// malformed fragment, is an error.
fn merged_settings(current: Option<&str>, fragment_json: &str) -> Result<Option<String>> {
    let fragment: Value =
        serde_json::from_str(fragment_json).context("pushed hooks fragment is not valid JSON")?;
    let frag_hooks = fragment
        .as_object()
        .context("pushed hooks fragment is not an object")?;

    let mut root: Value = match current {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).context("settings.json is not valid JSON — fix it first")?
        }
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

    for (event, pushed_list) in frag_hooks {
        let pushed_entries = pushed_list
            .as_array()
            .with_context(|| format!("pushed hooks.{event} is not an array"))?;
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("`hooks.{event}` in settings.json is not an array"))?;
        // Drop nook-managed entries (keep the user's), then put the pushed set
        // in their place — replace, never duplicate (AC-2).
        list.retain(|e| marker_of(e).is_none());
        for e in pushed_entries {
            list.push(e.clone());
        }
    }

    let rendered = serde_json::to_string_pretty(&root)? + "\n";
    if current == Some(rendered.as_str()) {
        return Ok(None);
    }
    Ok(Some(rendered))
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

    // ── MAIN-105: applying a PUSHED fragment (control-plane delivery) ─────────

    use nook_proto::hooks::claude_settings_fragment;

    fn fragment_json() -> String {
        serde_json::to_string_pretty(&claude_settings_fragment()).unwrap()
    }

    /// A fresh machine (no settings file) gains exactly the managed set.
    #[test]
    fn apply_to_a_fresh_file_writes_the_managed_set() {
        let out = merged_settings(None, &fragment_json())
            .unwrap()
            .expect("a fresh file must be written");
        let root: Value = serde_json::from_str(&out).unwrap();
        let hooks = root["hooks"].as_object().unwrap();
        for h in HOOKS {
            let list = hooks[h.event].as_array().unwrap();
            assert!(
                list.iter().any(|e| is_ours(e, &marker(h))),
                "{} missing after apply",
                h.event
            );
        }
    }

    /// The user's own hooks and settings survive the merge; only nook entries
    /// are (re)placed.
    #[test]
    fn apply_preserves_user_entries_and_settings() {
        let user = json!({
            "model": "opus",
            "hooks": {
                // A user's own Stop hook — no nook marker, must survive.
                "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "echo mine" }] }],
                "PreToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": "echo pre" }] }]
            }
        });
        let text = serde_json::to_string_pretty(&user).unwrap() + "\n";
        let out = merged_settings(Some(&text), &fragment_json())
            .unwrap()
            .expect("a change is expected");
        let root: Value = serde_json::from_str(&out).unwrap();
        // Untouched settings and a whole untouched event.
        assert_eq!(root["model"], "opus");
        assert_eq!(root["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        // The user's Stop hook is still there, alongside the two managed ones.
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert!(
            stop.iter().any(|e| e["hooks"][0]["command"] == "echo mine"),
            "user's own Stop hook was lost"
        );
        assert_eq!(
            stop.iter().filter(|e| marker_of(e).is_some()).count(),
            2,
            "Stop must carry exactly the two managed entries (finished + idle)"
        );
    }

    /// Applying the same fragment twice yields a byte-identical file, and the
    /// second apply is a no-op (nothing to write) — AC-2 + AC-4.
    #[test]
    fn double_apply_is_byte_identical_and_the_second_is_a_noop() {
        let once = merged_settings(None, &fragment_json()).unwrap().unwrap();
        // Re-applying to the just-written file changes nothing.
        let twice = merged_settings(Some(&once), &fragment_json()).unwrap();
        assert!(
            twice.is_none(),
            "re-applying the same fragment must not rewrite the file"
        );
    }

    /// A new managed set REPLACES the old nook entries rather than stacking them.
    #[test]
    fn apply_replaces_stale_managed_entries() {
        // Start from a file carrying a DIFFERENT (old) managed Stop entry.
        let old = json!({
            "hooks": {
                "Stop": [{ "matcher": "", "hooks": [{ "type": "command",
                    "command": "nook notify \"old\" --kind agent.finished || true" }] }]
            }
        });
        let text = serde_json::to_string_pretty(&old).unwrap() + "\n";
        let out = merged_settings(Some(&text), &fragment_json())
            .unwrap()
            .unwrap();
        let root: Value = serde_json::from_str(&out).unwrap();
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        // The old finished-entry is gone; exactly one finished entry remains.
        assert_eq!(
            stop.iter()
                .filter(|e| is_ours(e, "--kind agent.finished"))
                .count(),
            1,
            "the stale managed entry was not replaced"
        );
        assert!(
            !out.contains("\"old\""),
            "the old managed command must not survive"
        );
    }

    /// An existing file that is not valid JSON is an error — never a silent
    /// overwrite of something a person may have hand-edited (AC-2).
    #[test]
    fn apply_reports_invalid_existing_json() {
        let err = merged_settings(Some("{ not json"), &fragment_json())
            .expect_err("invalid settings.json must be an error");
        assert!(
            format!("{err:#}").contains("not valid JSON"),
            "unexpected error: {err:#}"
        );
    }

    /// `marker_of` recognises nook-managed entries and leaves the user's alone —
    /// the re-keying that keeps a merge from eating someone's own hooks.
    #[test]
    fn marker_of_distinguishes_managed_from_user_entries() {
        let managed = json!({ "hooks": [{ "type": "command",
            "command": "nook notify \"x\" --kind agent.finished || true" }] });
        let state = json!({ "hooks": [{ "type": "command",
            "command": "nook agent-state idle >/dev/null 2>&1 || true" }] });
        let mine = json!({ "hooks": [{ "type": "command", "command": "echo hi" }] });
        assert_eq!(
            marker_of(&managed).as_deref(),
            Some("--kind agent.finished")
        );
        assert_eq!(marker_of(&state).as_deref(), Some("agent-state idle"));
        assert_eq!(marker_of(&mine), None);
    }
}
