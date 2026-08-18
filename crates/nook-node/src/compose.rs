//! Bringing a finished build's docker compose stack down (MAIN-507).
//!
//! A build run that boots the dev stack in its worktree used to leave it
//! running forever: three finished builds on one machine held 28 containers and
//! 5.8GB doing nothing. Nothing on this node can decide whether a stack is
//! still wanted — that is a card fact — so this module only ever reports what
//! is running and reaps exactly the projects it is told to, having checked each
//! name is a build worktree's.
//!
//! **One exception, and it is the whole of [`reconcile_pre_sandbox_stacks`]
//! (MAIN-630): a build stack on the HOST daemon.** Since MAIN-611 a build's
//! stack runs in its job container's NESTED daemon and dies with it, so a
//! build-worktree project the host daemon is still running was started before
//! that shipped and nothing will ever return to it. That is not a card fact —
//! it is a fact about this machine's daemon, which only this side can see, and
//! the control plane's own sweep protects exactly these stacks because their
//! cards are still in flight.

use std::process::Command;

use nook_proto::compose::{is_build_stack_project, is_build_worktree_path};

/// Compose's own labels, on every container it starts.
pub const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
const COMPOSE_WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";

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

/// A compose project the HOST daemon holds, and the directory it was brought
/// up from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProject {
    pub name: String,
    pub working_dir: String,
}

/// Every compose project the host daemon holds, read off its containers' own
/// labels.
///
/// The labels rather than `docker compose ls`, because the WORKING DIRECTORY is
/// what identifies a stack as ours (MAIN-630 AC-1) and `ls` reports only the
/// config files — which `-f` and `--project-directory` can put anywhere. A
/// container records the directory compose was actually run from.
///
/// One row per project: every container of a stack carries the same pair.
pub fn host_projects(ps: &str) -> Vec<HostProject> {
    let mut out: Vec<HostProject> = Vec::new();
    for labels in ps.lines() {
        let (Some(name), Some(working_dir)) = (
            crate::sandbox::label_value(labels, COMPOSE_PROJECT_LABEL),
            crate::sandbox::label_value(labels, COMPOSE_WORKING_DIR_LABEL),
        ) else {
            continue;
        };
        let project = HostProject { name, working_dir };
        if !out.contains(&project) {
            out.push(project);
        }
    }
    out
}

/// The build stacks stranded on the host daemon (MAIN-630 AC-1).
///
/// **A build stack on the HOST daemon is dead weight, whatever card it belongs
/// to.** Since the per-job sandbox shipped (MAIN-611) a build's `dev-up.sh`
/// runs in the job container's NESTED daemon, which the host daemon cannot see
/// and which dies with the container — so a build-worktree project the host is
/// still running is one that was started before the upgrade, and no repair pass
/// will ever return to it. It holds the host ports its card still leases, which
/// is what makes the card's own next run fail to publish them.
///
/// That is why this asks nothing of the control plane. MAIN-507's sweep already
/// reaps stacks no UNFINISHED card wants and deliberately spares the rest
/// (MAIN-480) — and the stranded stack's card is precisely one that is still in
/// flight, so that sweep protects the very thing blocking it.
///
/// The working directory is the whole eligibility test, and it is what keeps
/// this away from a person's own stack (AC-2, NG-3): only a directory the node
/// itself created is named `build-<workspace uuid>-<card key>`.
pub fn stranded_build_stacks(projects: &[HostProject]) -> Vec<&HostProject> {
    projects
        .iter()
        .filter(|p| is_build_worktree_path(&p.working_dir))
        .collect()
}

/// Bring a stranded stack down WITHOUT its volumes (MAIN-630 AC-3).
///
/// The one difference from [`reap_projects`], and it is deliberate: that reaps
/// a stack whose card is over, where the dev database is genuinely finished
/// with. This reaps a stack whose card is still in flight — the port is what is
/// wanted back, and a volume costs disk rather than blocking anything.
pub fn stack_down_args(project: &str) -> Vec<String> {
    vec![
        "-p".into(),
        project.into(),
        "down".into(),
        "--remove-orphans".into(),
    ]
}

/// Bring down every build stack still running on this node's host daemon.
///
/// Best effort and never fatal: a failure is one line and the next project is
/// still attempted, exactly as the sandbox sweep does.
pub fn reconcile_pre_sandbox_stacks() {
    if let Some(detail) = crate::sandbox::containerised() {
        // No socket, so no host daemon to reconcile and no builds run here.
        tracing::debug!(%detail, "no host build stacks to reconcile: this node runs none");
        return;
    }
    let ps = match docker(&["ps", "-a", "--format", "{{.Labels}}"]) {
        Ok(ps) => ps,
        Err(ComposeError::Absent(_)) => return,
        Err(e) => {
            tracing::warn!(error = %e, "could not list containers to reconcile host build stacks");
            return;
        }
    };
    reconcile_with(&host_projects(&ps), |project| {
        let args = stack_down_args(project);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        compose(&argv).map(|_| ()).map_err(|e| e.to_string())
    });
}

/// The decision, against whichever runner it is handed — so AC-5's "nothing
/// found means nothing run" is a test rather than a claim.
fn reconcile_with(
    projects: &[HostProject],
    mut down: impl FnMut(&str) -> Result<(), String>,
) -> usize {
    let stranded = stranded_build_stacks(projects);
    if stranded.is_empty() {
        return 0;
    }
    let mut count = 0;
    for project in stranded {
        tracing::info!(
            project = %project.name, working_dir = %project.working_dir,
            "bringing down a build stack stranded on the host daemon: its \
             worktree's builds now run in a sandbox of their own, so nothing \
             will ever return to this one and it is holding its card's ports"
        );
        match down(&project.name) {
            Ok(()) => count += 1,
            Err(e) => {
                tracing::warn!(project = %project.name, error = %e, "could not bring it down")
            }
        }
    }
    count
}

/// Bring a worktree's stack down, THEN let `remove` take the directory (AC-3).
///
/// `remove` is the caller's choice of removal: the plain one, or the build
/// prune that also frees the card's branch (MAIN-537 AC-5).
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
/// **A down that FAILED stops the removal (MAIN-537 AC-3).** Same reasoning one
/// step further: the containers are still up and only this directory can name
/// them, so removing it converts a stack we can still reap into one nothing
/// can. Leaving a worktree behind wastes disk and is recoverable; orphaning a
/// stack is not.
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
    if !stack.ok {
        return crate::gitops::OpOutcome {
            ok: false,
            path: None,
            message: format!(
                "the worktree was kept: its compose stack is still up and only this \
                 directory names it — {}",
                stack.message
            ),
        };
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
        Err(e) => return no_inventory(e),
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

fn projects_present() -> Result<Vec<String>, ComposeError> {
    #[derive(serde::Deserialize)]
    struct Project {
        #[serde(rename = "Name")]
        name: String,
    }
    let out = compose(&["ls", "--all", "--format", "json"])?;
    let rows: Vec<Project> = serde_json::from_str(&out).map_err(|e| {
        ComposeError::Refused(format!("unreadable `docker compose ls` output: {e}"))
    })?;
    Ok(rows.into_iter().map(|p| p.name).collect())
}

/// What a docker that would not list its projects means for a directory the
/// caller was about to remove (MAIN-537 AC-3).
///
/// "There is no docker on this machine" and "docker is here and would not
/// answer" are different statements, and only the second is a reason to keep
/// the directory: a node without the binary has never created a compose
/// project, so there is nothing a removal could orphan. Reporting that as a
/// failure would permanently block every prune on such a node — the reaper's,
/// the REST route's and the MCP tool's alike.
fn no_inventory(e: ComposeError) -> Reaped {
    match e {
        ComposeError::Absent(e) => Reaped {
            projects: Vec::new(),
            ok: true,
            message: format!("no docker on this node ({e}) — no build stack can exist here"),
        },
        ComposeError::Refused(e) => Reaped {
            projects: Vec::new(),
            ok: false,
            message: format!("docker unavailable: {e}"),
        },
    }
}

/// Why a docker command produced nothing, split by what it means for a
/// directory the caller is about to remove.
enum ComposeError {
    /// The binary is not on this machine — nothing here has ever run a stack.
    Absent(String),
    /// Docker is here and said no. A stack may well exist.
    Refused(String),
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComposeError::Absent(e) | ComposeError::Refused(e) => f.write_str(e),
        }
    }
}

/// Run compose from a directory that holds no compose file.
///
/// `-p` addresses the project by label and needs none, and starting anywhere
/// else risks picking up whatever `docker-compose.yml` the node's working
/// directory happens to contain.
fn compose(args: &[&str]) -> Result<String, ComposeError> {
    let mut argv = vec!["compose"];
    argv.extend_from_slice(args);
    docker(&argv)
}

/// One docker command, from the neutral working directory above.
fn docker(args: &[&str]) -> Result<String, ComposeError> {
    let out = Command::new("docker")
        .args(args)
        .current_dir(std::env::temp_dir())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ComposeError::Absent(e.to_string()),
            _ => ComposeError::Refused(e.to_string()),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(ComposeError::Refused(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
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

    /// MAIN-537 AC-3, the same reasoning one step further: a down that FAILED
    /// leaves containers only this directory can name, so git is not allowed to
    /// take the directory. The card is told which of the two problems it has.
    #[test]
    fn a_stack_that_would_not_come_down_keeps_its_directory() {
        let removals = std::cell::RefCell::new(0);
        let outcome = reap_then_remove(
            BUILD_WT,
            |_| Reaped {
                projects: Vec::new(),
                ok: false,
                message: "docker unavailable: Cannot connect to the Docker daemon".into(),
            },
            |path| {
                *removals.borrow_mut() += 1;
                removed(path)
            },
        );
        assert_eq!(removals.into_inner(), 0, "the directory must survive");
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("docker unavailable"),
            "{}",
            outcome.message
        );
    }

    /// MAIN-537, the other side of AC-3: a machine with no docker is not a
    /// reason to keep a directory, and treating it as one would block every
    /// prune on such a node — the reaper's, the REST route's and the MCP
    /// tool's — for good. A daemon that IS there and refuses still stops it.
    #[test]
    fn a_missing_docker_permits_the_prune_and_a_refusing_one_does_not() {
        let absent = no_inventory(ComposeError::Absent(
            "No such file or directory (os error 2)".into(),
        ));
        assert!(absent.ok, "{}", absent.message);
        assert!(absent.projects.is_empty());
        assert!(absent.message.contains("no docker on this node"));

        let refused = no_inventory(ComposeError::Refused(
            "Cannot connect to the Docker daemon".into(),
        ));
        assert!(!refused.ok, "a daemon that will not answer keeps the tree");
        assert!(refused.message.contains("docker unavailable"));
    }

    /// The split itself: `Command::output` reports a binary that is not on PATH
    /// as `NotFound`, and that is the only shape read as "no docker here".
    #[test]
    fn a_binary_that_is_not_there_is_the_absent_case() {
        let e = std::process::Command::new("nook-no-such-binary-537")
            .output()
            .expect_err("a binary that does not exist cannot spawn");
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    }

    /// A path that is not a build worktree's names no project, so the prune of
    /// a human's own checkout cannot reach docker at all.
    #[test]
    fn a_worktree_that_is_not_a_builds_reaps_nothing() {
        assert!(nook_proto::compose::build_stack_projects("/home/ryan/nook-os").is_empty());
        let outcome = reap_then_remove_worktree("/home/ryan/nook-os", removed);
        assert_eq!(outcome.message, "removed", "no stack should be reported");
    }

    /// One container's `{{.Labels}}` column, as docker renders it.
    fn labelled(project: &str, working_dir: &str) -> String {
        format!(
            "com.docker.compose.config-hash=abc,{COMPOSE_PROJECT_LABEL}={project},\
             com.docker.compose.service=web,{COMPOSE_WORKING_DIR_LABEL}={working_dir}"
        )
    }

    const STRANDED_DIR: &str =
        "/home/ryan/.nook/clone-cache/host/worktrees/build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-MAIN-611";
    const STRANDED: &str = "nook-build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-main-611";

    /// A stack of every service of one project is one project, and its
    /// containers agree about where it came from.
    #[test]
    fn every_container_of_one_stack_reports_one_project() {
        let ps = format!(
            "{}\n{}\n{}",
            labelled(STRANDED, STRANDED_DIR),
            labelled(STRANDED, STRANDED_DIR),
            labelled("nook-os", "/home/ryan/nook-os"),
        );
        assert_eq!(
            host_projects(&ps),
            vec![
                HostProject {
                    name: STRANDED.into(),
                    working_dir: STRANDED_DIR.into(),
                },
                HostProject {
                    name: "nook-os".into(),
                    working_dir: "/home/ryan/nook-os".into(),
                },
            ]
        );
    }

    /// A container that is not compose's — the job sandbox's own, a database
    /// somebody started by hand — belongs to no project and is invisible here.
    #[test]
    fn a_container_outside_compose_names_no_project() {
        let ps = "nook.job=019f840f-2d80-7163-b4b1-8b1e12d7e0d3\n\n\
                  org.opencontainers.image.title=Postgres";
        assert!(host_projects(ps).is_empty());
    }

    /// AC-1: the identification predicate, both ways round. A host-daemon
    /// project whose working directory is a build worktree goes; anything else
    /// stays (AC-2, NG-3) — including a project in a normal checkout, one that
    /// merely LOOKS like a build, and one whose name is ours but whose
    /// directory is not.
    #[test]
    fn only_a_build_worktrees_stack_is_stranded() {
        let projects = vec![
            HostProject {
                name: STRANDED.into(),
                working_dir: STRANDED_DIR.into(),
            },
            HostProject {
                name: "nook-os".into(),
                working_dir: "/home/ryan/nook-os".into(),
            },
            HostProject {
                name: "build-tools".into(),
                working_dir: "/srv/build-tools".into(),
            },
            HostProject {
                name: "postgres".into(),
                working_dir: "/home/ryan/experiments/db".into(),
            },
            // The name is a build stack's and the directory is a person's; the
            // directory is what decides.
            HostProject {
                name: STRANDED.into(),
                working_dir: "/home/ryan/nook-os".into(),
            },
        ];
        assert_eq!(
            stranded_build_stacks(&projects)
                .into_iter()
                .map(|p| p.working_dir.as_str())
                .collect::<Vec<_>>(),
            vec![STRANDED_DIR]
        );
    }

    /// AC-3: containers and networks, and the volumes stay. The card this
    /// reaps for is still in flight, so its dev database is not finished with.
    #[test]
    fn the_teardown_keeps_the_volumes() {
        let args = stack_down_args(STRANDED);
        assert_eq!(args, vec!["-p", STRANDED, "down", "--remove-orphans"]);
        assert!(
            !args.iter().any(|a| a == "-v" || a == "--volumes"),
            "{args:?}"
        );
    }

    /// AC-5: a node with nothing to reap runs nothing at all — which is every
    /// node from the day after this ships, on every reconnect.
    #[test]
    fn a_node_with_nothing_stranded_runs_nothing() {
        let mut ran: Vec<String> = Vec::new();
        let nothing_of_ours = vec![HostProject {
            name: "nook-os".into(),
            working_dir: "/home/ryan/nook-os".into(),
        }];
        for projects in [Vec::new(), nothing_of_ours] {
            assert_eq!(
                reconcile_with(&projects, |p| {
                    ran.push(p.to_string());
                    Ok(())
                }),
                0
            );
        }
        assert!(ran.is_empty(), "{ran:?}");
    }

    /// And when there is something: only the stranded project, and a failure
    /// does not stop the next one.
    #[test]
    fn a_failure_does_not_stop_the_rest() {
        let second = "nook-build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-main-617";
        let projects = vec![
            HostProject {
                name: STRANDED.into(),
                working_dir: STRANDED_DIR.into(),
            },
            HostProject {
                name: "nook-os".into(),
                working_dir: "/home/ryan/nook-os".into(),
            },
            HostProject {
                name: second.into(),
                working_dir: STRANDED_DIR.replace("MAIN-611", "MAIN-617"),
            },
        ];
        let mut ran: Vec<String> = Vec::new();
        let brought_down = reconcile_with(&projects, |p| {
            ran.push(p.to_string());
            (p != STRANDED)
                .then_some(())
                .ok_or_else(|| "daemon said no".to_string())
        });
        assert_eq!(ran, vec![STRANDED.to_string(), second.to_string()]);
        assert_eq!(brought_down, 1);
    }
}
