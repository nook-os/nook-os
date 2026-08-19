//! What a person's OWN session never has to approve (MAIN-620).
//!
//! A loop job runs `--dangerously-skip-permissions` because nobody is watching.
//! A human session had the opposite posture and nothing in between: every tool
//! call raised a prompt, so drafting a spec on a phone was a page of "allow"
//! taps and the session read as deprivileged. Both extremes are wrong here —
//! the answer is a NARROWED gate, not a removed one (NG-2).
//!
//! The narrowing is expressed as Claude Code's own permission rules, handed to
//! the runtime through `--settings`, and it says exactly what MAIN-620's AC-1
//! says: the `nook` CLI, reading files, and edits **inside this session's own
//! checkout**. Everything else — a destructive shell command, network egress,
//! a write outside the checkout — matches no rule and still prompts (AC-2).
//!
//! Two properties are worth stating because they are what make this safe:
//!
//! - **The rules are a pure function of the checkout**, so the whole policy is
//!   unit-testable without a process, in the shape `sandbox::run_args` set.
//! - **One policy, both surfaces.** The structured chat and the interactive
//!   tmux session load the SAME document (AC-4). A person must never meet a
//!   prompt inside a TUI they cannot reach on a phone, and the only way to
//!   guarantee that is for neither surface to own a list of its own.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Tools that read and never change anything, allowed unqualified.
///
/// AC-1 names "reading files" without a boundary, and AC-2's list of what must
/// still gate is writes, shell and egress — so a read is safe wherever it
/// points. `WebFetch`/`WebSearch` are deliberately absent: they are the egress
/// AC-2 keeps behind the gate, whatever they superficially resemble.
const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "NotebookRead", "TodoWrite"];

/// Writing a file, allowed only WITHIN the session's checkout.
///
/// `Edit` and ONLY `Edit`: the runtime matches a file-tool check against `Edit`
/// rules alone, and an `Edit` rule covers `Write` and `NotebookEdit` too.
/// Naming those two as well is not belt-and-braces — the runtime REJECTS each
/// unmatched rule out loud on stderr, at every launch of both surfaces, which
/// is four warnings in the pane a person is looking at on a card about a
/// session that reads as deprivileged.
const WRITE_RULE_TOOL: &str = "Edit";

/// The one shell allowance: the `nook` CLI.
///
/// A session's agent drives the board, reads cards and files tickets with it,
/// which is most of what a spec conversation does — and it is the one command
/// whose blast radius is the control plane's own authorization rather than the
/// machine. Every other program stays behind the gate, which is what keeps
/// "destructive shell" in AC-2's list meaningful.
const NOOK_CLI_RULE: &str = "Bash(nook:*)";

/// The permission rules a human-driven session starts with.
///
/// `//<abs>/**` is Claude Code's absolute-path form — the rule's own `/`
/// followed by the path's, which is why the leading slashes are normalised
/// here rather than concatenated hopefully. Absolute rather than relative
/// because a relative rule resolves against the settings file's own directory,
/// not the session's, and this file does not live in the checkout.
pub fn allow_rules(cwd: &Path) -> Vec<String> {
    let root = canonical(cwd);
    let root = root.to_string_lossy();
    let root = root.trim_matches('/');
    let mut rules: Vec<String> = vec![NOOK_CLI_RULE.to_string()];
    rules.extend(READ_ONLY_TOOLS.iter().map(|t| (*t).to_string()));
    rules.push(format!("{WRITE_RULE_TOOL}(//{root}/**)"));
    rules
}

/// The settings document `--settings` loads.
///
/// `defaultMode` is pinned to `default` — the asking mode — rather than left
/// out. What this file exists to do is make the posture of a fleet-driven
/// session a fact rather than an inheritance: a node whose own
/// `~/.claude/settings.json` happens to say `bypassPermissions` would otherwise
/// hand a person's session the very blanket skip NG-2 forbids.
pub fn settings_document(cwd: &Path) -> serde_json::Value {
    serde_json::json!({
        "permissions": {
            "defaultMode": "default",
            "allow": allow_rules(cwd),
        }
    })
}

/// Is this runtime the one these rules are written for?
///
/// Matched on the file stem so an absolute path to the binary counts, which is
/// how tests and a non-PATH install both spell it.
pub fn is_claude(runtime: &str) -> bool {
    Path::new(runtime)
        .file_stem()
        .map(|s| s == "claude")
        .unwrap_or(false)
}

/// Where a checkout's document lives.
///
/// Keyed by the checkout rather than by the session, because the content is a
/// function of the checkout and nothing else: two sessions in one tree share a
/// file, and the directory therefore stops growing at "one per checkout on this
/// machine" instead of "one per session ever started". There is no cleanup path
/// to forget.
pub fn settings_path(cwd: &Path) -> Option<PathBuf> {
    let dir = if let Ok(d) = std::env::var("NOOK_CONFIG_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config/nook")
    };
    let digest = Sha256::digest(canonical(cwd).to_string_lossy().as_bytes());
    Some(
        dir.join("session-permissions")
            .join(format!("{:x}.json", digest)),
    )
}

/// Write the document for `cwd` and return the path to hand `--settings`.
///
/// `None` for any runtime but `claude`, and `None` when the write fails —
/// **never an error**. A settings file that could not be written must not stop
/// a person's session from starting; the cost of failing open here is the
/// per-action prompting this card removes, which is exactly the behaviour every
/// session had before it.
///
/// Written to a sibling and RENAMED, because two sessions starting in one
/// checkout write this same path at the same time: truncate-then-write leaves a
/// window in which the loser's runtime reads a half-written document, and the
/// content being identical either way does not make a partial read valid JSON.
pub fn settings_for(runtime: &str, cwd: &Path) -> Option<PathBuf> {
    if !is_claude(runtime) {
        return None;
    }
    let path = settings_path(cwd)?;
    let body = serde_json::to_string_pretty(&settings_document(cwd)).ok()?;
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, "could not create the managed permissions directory");
            return None;
        }
    }
    let staged = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = std::fs::write(&staged, body).and_then(|()| std::fs::rename(&staged, &path));
    match written {
        Ok(()) => Some(path),
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            tracing::warn!(error = %e, path = %path.display(),
                "could not write the managed permissions — this session will prompt per action");
            None
        }
    }
}

/// The checkout's real path where one exists.
///
/// A worktree reached through a symlinked home is the ordinary case on macOS
/// (`/tmp` → `/private/tmp`), and a rule naming the unresolved spelling matches
/// nothing the runtime ever sees. Best-effort: a path that does not exist yet
/// is left as given rather than dropped, since the caller has already decided
/// it is the session's directory.
fn canonical(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-1's three names, spelled as rules. Asserted by MEANING rather than by
    /// comparing the whole vector, so adding a read-only tool does not fail a
    /// test that is about the contract.
    #[test]
    fn the_safe_set_is_the_nook_cli_reads_and_in_checkout_writes() {
        let rules = allow_rules(Path::new("/srv/checkout"));
        assert!(rules.contains(&NOOK_CLI_RULE.to_string()), "{rules:?}");
        assert!(rules.contains(&"Read".to_string()), "{rules:?}");
        assert!(
            rules.contains(&"Edit(//srv/checkout/**)".to_string()),
            "an in-checkout write of any kind is allowed: {rules:?}"
        );
    }

    /// A rule the runtime does not match is not harmless — it is rejected on
    /// stderr at every launch, which lands in the pane a person is reading.
    ///
    /// `Write` and `NotebookEdit` are the two that read as belt-and-braces and
    /// are not: a file-tool check consults `Edit` rules alone, and an `Edit`
    /// rule already covers both. So does a bare `Edit(//<root>)` beside the
    /// `/**` one — the tree rule matches a file directly in the root by itself.
    #[test]
    fn only_rule_forms_the_runtime_matches_are_emitted() {
        let rules = allow_rules(Path::new("/srv/checkout"));
        for inert in [
            "Write(//srv/checkout/**)",
            "NotebookEdit(//srv/checkout/**)",
            "Edit(//srv/checkout)",
            "Write(//srv/checkout)",
            "NotebookEdit(//srv/checkout)",
        ] {
            assert!(
                !rules.iter().any(|r| r == inert),
                "{inert} is a rule the runtime warns about rather than honours: {rules:?}"
            );
        }
        assert_eq!(
            rules.iter().filter(|r| r.starts_with("Edit(")).count(),
            1,
            "exactly one write rule: {rules:?}"
        );
    }

    /// AC-2, stated as the absence of a rule: nothing in the document can match
    /// a shell command other than `nook`, an egress tool, or a path outside the
    /// checkout. This is the assertion that fails if a future edit widens the
    /// set into a blanket skip.
    #[test]
    fn nothing_outside_the_safe_set_is_pre_approved() {
        let rules = allow_rules(Path::new("/srv/checkout"));
        for forbidden in ["Bash", "Bash(*)", "Bash(rm:*)", "WebFetch", "WebSearch"] {
            assert!(
                !rules.iter().any(|r| r == forbidden),
                "{forbidden} must still prompt: {rules:?}"
            );
        }
        // Every write rule names the checkout — so a write anywhere else has
        // no rule to match.
        for rule in rules.iter().filter(|r| r.contains('(')) {
            assert!(
                rule == NOOK_CLI_RULE || rule.contains("/srv/checkout"),
                "a qualified rule must be scoped to the checkout: {rule}"
            );
        }
    }

    /// The gate is narrowed, not removed: the document must never carry a mode
    /// that skips the check for everything (NG-2).
    #[test]
    fn the_document_pins_the_asking_mode() {
        let doc = settings_document(Path::new("/srv/checkout"));
        assert_eq!(doc["permissions"]["defaultMode"], "default");
        let text = doc.to_string();
        assert!(!text.contains("bypassPermissions"), "{text}");
        assert!(!text.contains("acceptEdits"), "{text}");
    }

    /// Two sessions in one checkout share a document; two checkouts do not.
    #[test]
    fn the_document_is_keyed_by_the_checkout() {
        let a = settings_path(Path::new("/srv/one")).expect("a path");
        let b = settings_path(Path::new("/srv/one")).expect("a path");
        let c = settings_path(Path::new("/srv/two")).expect("a path");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn only_claude_gets_a_managed_document() {
        assert!(is_claude("claude"));
        assert!(is_claude("/home/someone/.local/bin/claude"));
        assert!(!is_claude("bash"));
        assert!(!is_claude("codex"));
        assert!(settings_for("bash", Path::new("/srv/checkout")).is_none());
    }
}
