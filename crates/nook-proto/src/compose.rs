//! Which docker compose project a build worktree's stack runs under (MAIN-507).
//!
//! The project name is derived from the worktree's DIRECTORY, which is what
//! stops two checkouts of one repo fighting over container names and volumes
//! (CLAUDE.md) — and also why nothing reaped a finished build's stack: after
//! the directory goes, the name is unrecoverable and the containers are beyond
//! the reach of anything but a human. So the derivation lives here, shared by
//! the control plane (which decides WHICH stacks may go) and the node (which
//! actually runs docker), rather than being guessed on either side.

/// The compose projects a build worktree's stack can be running under.
///
/// Two, because the repo boots two ways. `scripts/compose-project.sh` — sourced
/// by `run.sh` and `dev-up.sh` — exports `nook-<slug>`; a bare
/// `docker compose up -d` in the worktree gets compose's own default, the bare
/// `<slug>`. Both were observed on the same machine, so both are named.
///
/// Empty for any path that is not a build worktree's, which is the guard that
/// keeps this away from a human's own stack: the caller reaps what this
/// returns and nothing else.
pub fn build_stack_projects(worktree_path: &str) -> Vec<String> {
    let Some(slug) = build_slug(worktree_path) else {
        return Vec::new();
    };
    vec![format!("nook-{slug}"), slug]
}

/// Is this directory a build worktree's — whatever the stack in it is CALLED?
///
/// The same judgement [`build_stack_projects`] makes, read from the other end.
/// A compose project reports the directory it was brought up from, and that is
/// a stronger claim of ownership than its name: a name can be given with `-p`,
/// while the directory is one the node itself created (MAIN-630).
pub fn is_build_worktree_path(path: &str) -> bool {
    build_slug(path).is_some()
}

fn build_slug(worktree_path: &str) -> Option<String> {
    let dir = worktree_path
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())?;
    let slug = slugify(dir);
    is_build_stack_project(&slug).then_some(slug)
}

/// Is this compose project a build worktree's, and therefore reapable?
///
/// The shape is `[nook-]build-<workspace uuid>-<card key>`, built by the node's
/// `build_dirname`. Requiring the UUID rather than the bare `build-` prefix is
/// deliberate: `build-` alone would also match somebody's `build-tools` stack,
/// and the cost of a false positive here is a human's containers.
pub fn is_build_stack_project(project: &str) -> bool {
    let rest = project.strip_prefix("nook-").unwrap_or(project);
    let Some(rest) = rest.strip_prefix("build-") else {
        return false;
    };
    // uuid, a separator, then a non-empty card key.
    starts_with_uuid(rest) && rest.as_bytes().get(36) == Some(&b'-') && rest.len() > 37
}

/// Compose's project-name normalization, as `scripts/compose-project.sh`
/// spells it: lowercase, and anything outside `[a-z0-9_-]` becomes `-`.
///
/// Compose's own default DROPS those characters instead of replacing them. The
/// two agree here because a build worktree's directory name is already
/// `[A-Za-z0-9_-]` (the node sanitizes it), so neither branch of the difference
/// can be reached — asserted by `both_derivations_agree`.
fn slugify(dir: &str) -> String {
    dir.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn starts_with_uuid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = s.as_bytes();
    if bytes.len() < 36 {
        return false;
    }
    let mut i = 0;
    for (n, len) in GROUPS.iter().enumerate() {
        if n > 0 {
            if bytes[i] != b'-' {
                return false;
            }
            i += 1;
        }
        for _ in 0..*len {
            if !bytes[i].is_ascii_hexdigit() {
                return false;
            }
            i += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const WT: &str =
        "/home/x/.nook/clone-cache/host/worktrees/build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-MAIN-507";

    #[test]
    fn names_both_spellings_a_build_stack_can_have() {
        assert_eq!(
            build_stack_projects(WT),
            vec![
                "nook-build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-main-507".to_string(),
                "build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-main-507".to_string(),
            ]
        );
    }

    #[test]
    fn a_trailing_slash_does_not_lose_the_directory() {
        assert_eq!(
            build_stack_projects(&format!("{WT}/")),
            build_stack_projects(WT)
        );
    }

    /// A directory names a project exactly when it is a build worktree's, so
    /// the two readings of the same judgement cannot drift apart.
    #[test]
    fn a_path_is_a_build_worktrees_exactly_when_it_names_projects() {
        for path in [
            WT,
            &format!("{WT}/"),
            "/home/ryan/nook-os",
            "/srv/build-tools",
            "/",
            "",
        ] {
            assert_eq!(
                is_build_worktree_path(path),
                !build_stack_projects(path).is_empty(),
                "{path}"
            );
        }
        assert!(is_build_worktree_path(WT));
        assert!(!is_build_worktree_path("/home/ryan/nook-os"));
    }

    /// NG-3: nothing outside a build worktree is nameable, so nothing outside
    /// one can be reaped.
    #[test]
    fn a_humans_own_checkout_names_nothing() {
        assert!(build_stack_projects("/home/ryan/nook-os").is_empty());
        assert!(build_stack_projects("/srv/build-tools").is_empty());
        assert!(build_stack_projects("/").is_empty());
        assert!(build_stack_projects("").is_empty());
        assert!(!is_build_stack_project("nook-nook-os"));
        assert!(!is_build_stack_project("build-tools"));
        // A uuid and nothing after it is a directory name this never produces.
        assert!(!is_build_stack_project(
            "build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3"
        ));
    }

    #[test]
    fn every_name_it_produces_it_also_accepts() {
        for project in build_stack_projects(WT) {
            assert!(is_build_stack_project(&project), "{project}");
        }
    }

    /// `scripts/compose-project.sh` replaces invalid characters, compose's own
    /// default drops them; a build worktree's directory name contains none, so
    /// the slug is the same either way and both stacks are addressable.
    #[test]
    fn both_derivations_agree() {
        let dir = "build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-MAIN-507";
        let dropped: String = dir
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
            .collect();
        assert_eq!(slugify(dir), dropped);
    }
}
