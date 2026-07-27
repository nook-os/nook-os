//! MAIN-162 AC-4 (skill half): the two interview skills carry the ask-channel
//! rule, so a detached loop job asks durably instead of blocking on a terminal
//! that is not there. A content grep — no database, no network — guarding
//! against the paragraph being dropped or a bare terminal-only ask creeping back
//! into job mode.

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
        skill("nook-spec").contains("version: 1.1.0"),
        "nook-spec version must be bumped for the ask-channel paragraph"
    );
    assert!(
        skill("nook-epic").contains("version: 2.3.0"),
        "nook-epic version must be bumped for the ask-channel paragraph"
    );
}
