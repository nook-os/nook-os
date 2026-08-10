//! Bringing a finished build's docker compose stack down (MAIN-507).
//!
//! A build run that boots the dev stack in its worktree used to leave it
//! running forever: three finished builds on one machine held 28 containers and
//! 5.8GB doing nothing. Nothing on this node can decide whether a stack is
//! still wanted — that is a card fact — so this module only ever reports what
//! is running and reaps exactly the projects it is told to, having checked each
//! name is a build worktree's.

use std::process::Command;

use nook_proto::compose::is_build_stack_project;

/// What a reap did, in the shape [`nook_proto::NodeToControl::OpResult`] carries.
pub struct Reaped {
    /// The projects that actually came down. Empty is the ordinary case for a
    /// repo whose builds boot nothing.
    pub projects: Vec<String>,
    pub ok: bool,
    pub message: String,
}

/// Every build worktree compose project this machine currently holds.
///
/// Empty when docker is absent or unreachable — a node without docker holds no
/// stacks, which is the same answer either way.
pub fn build_stacks_held() -> Vec<String> {
    projects_present()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| is_build_stack_project(p))
        .collect()
}

/// Bring a worktree's stack down, THEN let `remove` take the directory (AC-3).
pub fn reap_then_remove_worktree(
    worktree_path: &str,
    remove: impl FnOnce(&str) -> crate::gitops::OpOutcome,
) -> crate::gitops::OpOutcome {
    reap_then_remove(
        worktree_path,
        |path| reap_projects(&nook_proto::compose::build_stack_projects(path)),
        remove,
    )
}

/// The order is the whole point, and is why these two are one function rather
/// than two statements at a call site: the compose project name is derived from
/// the directory's name, so a removal that goes first orphans the containers
/// beyond the reach of anything but a human.
///
/// `reap` is a parameter so the ordering can be pinned by a test that supplies
/// its own. The alternative — a test calling the real one with a build-shaped
/// path — reaches `projects_present` and then a live `down --volumes`, which is
/// the module's whole contract inverted: a name should reach docker only
/// because the control plane decided a card is over, never because a test
/// constant looked the part.
fn reap_then_remove(
    worktree_path: &str,
    reap: impl FnOnce(&str) -> Reaped,
    remove: impl FnOnce(&str) -> crate::gitops::OpOutcome,
) -> crate::gitops::OpOutcome {
    let stack = reap(worktree_path);
    if !stack.projects.is_empty() || !stack.ok {
        tracing::info!(
            path = %worktree_path, stack = %stack.message,
            "the build worktree's compose stack, before removing the directory"
        );
    }
    let mut outcome = remove(worktree_path);
    if !stack.projects.is_empty() {
        outcome.message = format!("{}; {}", outcome.message, stack.message);
    }
    outcome
}

/// Bring each named project down with its volumes.
///
/// Tolerant by construction (AC-4): a stack already down, a project that never
/// existed, and a docker daemon that is absent or unreachable are all reported,
/// never raised — the caller is a card move that must not fail because of them.
pub fn reap_projects(projects: &[String]) -> Reaped {
    let wanted: Vec<&String> = projects
        .iter()
        .filter(|p| {
            let ours = is_build_stack_project(p);
            if !ours {
                tracing::warn!(project = %p, "refusing to reap a project that is not a build worktree's");
            }
            ours
        })
        .collect();
    if wanted.is_empty() {
        return Reaped {
            projects: Vec::new(),
            ok: true,
            message: "no build stack to reap".into(),
        };
    }

    let present = match projects_present() {
        Ok(present) => present,
        Err(e) => {
            return Reaped {
                projects: Vec::new(),
                ok: false,
                message: format!("docker unavailable: {e}"),
            }
        }
    };

    let mut reaped = Vec::new();
    let mut failed = Vec::new();
    for project in wanted {
        if !present.contains(project) {
            continue;
        }
        // The whole project, not a named subset (AC-2): `down` collects every
        // container carrying the project label, so a stack that gained a
        // service since this was written is still fully taken, and
        // `--remove-orphans` reaches the ones no compose file mentions any more.
        match compose(&["-p", project, "down", "--volumes", "--remove-orphans"]) {
            Ok(_) => reaped.push(project.clone()),
            Err(e) => failed.push(format!("{project}: {e}")),
        }
    }

    let message = if !failed.is_empty() {
        format!("could not bring down {}", failed.join("; "))
    } else if reaped.is_empty() {
        "no build stack was running".into()
    } else {
        format!("brought down {} with its volumes", reaped.join(", "))
    };
    Reaped {
        ok: failed.is_empty(),
        projects: reaped,
        message,
    }
}

fn projects_present() -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct Project {
        #[serde(rename = "Name")]
        name: String,
    }
    let out = compose(&["ls", "--all", "--format", "json"])?;
    let rows: Vec<Project> = serde_json::from_str(&out)
        .map_err(|e| format!("unreadable `docker compose ls` output: {e}"))?;
    Ok(rows.into_iter().map(|p| p.name).collect())
}

/// Run compose from a directory that holds no compose file.
///
/// `-p` addresses the project by label and needs none, and starting anywhere
/// else risks picking up whatever `docker-compose.yml` the node's working
/// directory happens to contain.
fn compose(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .arg("compose")
        .args(args)
        .current_dir(std::env::temp_dir())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build-shaped path naming nothing that can exist. The real workspace
    /// UUID would make this a live project name on the machine the branch is
    /// built on, and a test that reaches `projects_present` then reaps it for
    /// real — containers, volume and all.
    const BUILD_WT: &str = "/tmp/nook/worktrees/build-00000000-0000-0000-0000-000000000000-TEST-0";

    fn removed(path: &str) -> crate::gitops::OpOutcome {
        crate::gitops::OpOutcome {
            ok: true,
            path: Some(path.to_string()),
            message: "removed".into(),
        }
    }

    /// NG-3, at the last gate before docker: a name the control plane should
    /// never have sent is dropped here rather than run.
    #[test]
    fn a_project_that_is_not_a_build_worktrees_is_never_run() {
        let r = reap_projects(&["nook-nook-os".into(), "services".into()]);
        assert!(r.ok);
        assert!(r.projects.is_empty());
        assert_eq!(r.message, "no build stack to reap");
    }

    /// AC-3: the stack comes down while the directory is still there, and only
    /// then is git allowed to take it away.
    ///
    /// Both halves record into one list, so the assertion is the ORDER itself
    /// rather than a flag either step could set — and neither half runs docker.
    #[test]
    fn the_stack_comes_down_before_the_directory() {
        let order = std::cell::RefCell::new(Vec::new());
        let outcome = reap_then_remove(
            BUILD_WT,
            |path| {
                order.borrow_mut().push(format!("reap {path}"));
                Reaped {
                    projects: vec!["nook-build-x".into()],
                    ok: true,
                    message: "brought down nook-build-x with its volumes".into(),
                }
            },
            |path| {
                order.borrow_mut().push(format!("remove {path}"));
                removed(path)
            },
        );
        assert_eq!(
            order.into_inner(),
            vec![format!("reap {BUILD_WT}"), format!("remove {BUILD_WT}")]
        );
        assert_eq!(
            outcome.message, "removed; brought down nook-build-x with its volumes",
            "the removal reports what came down with it"
        );
    }

    /// A path that is not a build worktree's names no project, so the prune of
    /// a human's own checkout cannot reach docker at all.
    #[test]
    fn a_worktree_that_is_not_a_builds_reaps_nothing() {
        assert!(nook_proto::compose::build_stack_projects("/home/ryan/nook-os").is_empty());
        let outcome = reap_then_remove_worktree("/home/ryan/nook-os", removed);
        assert_eq!(outcome.message, "removed", "no stack should be reported");
    }
}
