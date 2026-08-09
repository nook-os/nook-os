//! MAIN-162 AC-4 (skill half): the two interview skills carry the ask-channel
//! rule, so a detached loop job asks durably instead of blocking on a terminal
//! that is not there. A content grep — no database, no network — guarding
//! against the paragraph being dropped or a bare terminal-only ask creeping back
//! into job mode.
//!
//! MAIN-232 extends the same guard to the INPUT half: job mode must read the
//! seed as the opening idea and fold steering messages into the interview, must
//! still gate filing on a shown draft plus a go-ahead, and must leave the
//! terminal paths exactly as they were.

use std::path::PathBuf;

/// Read a repo skill's `SKILL.md`. `CARGO_MANIFEST_DIR` is `crates/nook-control`,
/// so the skills live two levels up.
fn skill(name: &str) -> String {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "skills",
        name,
        "SKILL.md",
    ]
    .iter()
    .collect();
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The skill with every run of whitespace collapsed to one space, so a phrase
/// assertion matches regardless of where the markdown happens to wrap. What is
/// being guarded is the WORDING of an instruction, not its reflow.
fn flat(name: &str) -> String {
    skill(name).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every interview skill that can run as a loop job must name the durable
/// ask primitive and the marker that selects it — the ask-channel paragraph.
#[test]
fn interview_skills_route_job_mode_through_durable_interactions() {
    for name in ["nook-spec", "nook-epic"] {
        let body = skill(name);
        assert!(
            body.contains("nook interactions ask --wait"),
            "{name}: the ask-channel paragraph must name the durable ask primitive"
        );
        assert!(
            body.contains("NOOK_JOB_ID"),
            "{name}: job mode is selected by the NOOK_JOB_ID marker"
        );
        // The primitive and the marker sit in one paragraph: the durable-ask
        // instruction is scoped to job mode, not a bare terminal ask.
        assert!(
            body.contains("Detached loop job"),
            "{name}: the detached-job channel must be called out explicitly"
        );
    }
}

/// The versions were bumped alongside the paragraph (so `nook teach` re-publishes
/// them) — a guard that the content change and the version change ship together.
#[test]
fn interview_skill_versions_were_bumped() {
    assert!(
        skill("nook-spec").contains("version: 1.2.0"),
        "nook-spec version must be bumped for the job-mode input channel"
    );
    assert!(
        skill("nook-epic").contains("version: 2.4.0"),
        "nook-epic version must be bumped for the job-mode input channel"
    );
}

/// MAIN-232 AC-1: job mode reads the human's *unsolicited* input — the seed as
/// the opening idea, and steering messages folded into the interview. Without
/// these the skills would still only know the ticket body and the answers to
/// their own questions.
#[test]
fn interview_skills_read_the_seed_and_steering_messages_in_job_mode() {
    for name in ["nook-spec", "nook-epic"] {
        let body = flat(name);
        assert!(
            body.contains("NOOK_JOB_SEED"),
            "{name}: the seed must be named by the variable that carries it"
        );
        assert!(
            body.contains("Steering messages"),
            "{name}: steering messages must be an explicit input channel"
        );
        // The trap this closes: treating a message that lands mid-wait as the
        // answer to the outstanding ask, and continuing on an answer nobody gave.
        assert!(
            body.contains("not an answer to an outstanding ask"),
            "{name}: a steering message must not be mistaken for an ask's answer"
        );
    }
}

/// MAIN-232 AC-4: a job shows its draft and files on a go-ahead — the attended
/// gate, not a silent auto-file. Both skills must say the draft is printed BEFORE
/// blocking (a draft stacked behind a blocking ask is one nobody has read) and
/// that silence is not consent.
#[test]
fn job_mode_keeps_the_draft_then_file_gate() {
    for name in ["nook-spec", "nook-epic"] {
        let body = flat(name);
        assert!(
            body.contains("Print the complete draft first"),
            "{name}: job mode must print the whole draft before gating on it"
        );
        assert!(
            body.contains("stacked behind a blocking ask"),
            "{name}: the ordering hazard must stay written down"
        );
        assert!(
            body.contains("never file on silence"),
            "{name}: a job that ends without a go-ahead must file nothing"
        );
    }
}

/// MAIN-232 AC-2: terminal mode is untouched. These are the exact sentences a
/// human at the terminal is driven by — the ask primitive, the interview cap
/// rule, and the terminal draft gate. If a job-mode edit ever rewrites one of
/// them, this fails rather than shipping a changed experience for someone who
/// asked for none.
#[test]
fn terminal_mode_instructions_are_unchanged() {
    let spec = flat("nook-spec");
    for line in [
        // The terminal channel, verbatim.
        "**Human at a terminal** (`NOOK_JOB_ID` is NOT in the environment): ask interactively, exactly as today",
        // The unattended-no-job refusal.
        "there is no one to ask and nothing to pause on",
        // The interview contract.
        "There is NO cap on rounds",
        // The terminal draft gate, still the first thing §4 says.
        "Show the full draft in chat and get the user's go-ahead. Then file it.",
    ] {
        assert!(
            spec.contains(line),
            "nook-spec: terminal instruction changed or removed: {line:?}"
        );
    }

    let epic = flat("nook-epic");
    for line in [
        // Attended is still the default and still asks inline.
        "**Attended** (the DEFAULT — a human just ran `/nook-epic`, so they are in the room)",
        "**ask them 1–4 questions inline**",
        // The attended draft gate.
        "**Never auto-file in attended mode.**",
        "Then **end the turn and wait.**",
        // Unattended still escalates by comment.
        "### 5b. Unattended — comment and escalate",
    ] {
        assert!(
            epic.contains(line),
            "nook-epic: terminal/attended instruction changed or removed: {line:?}"
        );
    }
}

/// MAIN-459: nook-build 2.0 is directed-only and judgment-only. The directive
/// and the structured ending must be taught; the pick/claim/board mechanics
/// left with MAIN-458's control-plane machinery and are BANNED from
/// reappearing — a skill that picks its own card is inventing work, and one
/// that moves its own card can half-update the board.
#[test]
fn build_skill_is_directed_and_judgment_only() {
    let body = flat("nook-build");
    for taught in [
        // The directive, and the no-directive rule.
        "NOOK_BUILD_TASK",
        "end the pass and say so",
        // The structured ending, all three shapes.
        "nook builds outcome pr --url",
        "nook builds outcome blocked --question -",
        "nook builds outcome nothing",
        // Blocked keeps BOTH shapes: the live pause and the async handback.
        "nook interactions ask --wait",
        // The repair flow's JUDGMENT survived the rewrite: a card with a
        // recorded PR is repaired against the verdict, on the same PR. The
        // trigger is the `pr:` line the human rendering prints, and BOTH
        // comment markers are named — a conflict-only repair has no verdict
        // comment to find.
        "when the card already records a PR",
        "shows a **`pr:` line**",
        "Loop review of COMMIT_SHA",
        "Loop conflict check of <head>",
        "Fix **only** its \"Must fix before merge\" items.",
        "nook builds outcome pr --url <the existing PR's URL>",
        // NG-2: the hard rules survive the rewrite verbatim.
        "Never merge and never enable auto-merge.",
        "Never apply `agent-ready` to anything.",
    ] {
        assert!(body.contains(taught), "nook-build must teach: {taught:?}");
    }
    // The mechanics that left the skill. Each string is distinctive of the
    // 1.x flow: the pick query, the claim verb, and the hand-driven column
    // move payload.
    for banned in [
        "nook tasks --label agent-ready",
        "nook claim",
        "--column-type started",
        "\"column\":",
    ] {
        assert!(
            !body.contains(banned),
            "nook-build 2.0 must not carry the retired mechanic: {banned:?}"
        );
    }
    assert!(
        skill("nook-build").contains("version: 2.0.0"),
        "nook-build's version must be bumped with the directed-only rewrite"
    );
}
