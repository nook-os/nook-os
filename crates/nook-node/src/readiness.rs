//! Is a joined node capable of doing any work at all (MAIN-647)?
//!
//! A machine can join successfully, report `online`, and claim nothing forever,
//! because four independent gates each refuse it in silence: it declares no
//! loop kinds, it has no job sandbox, its runtime is not signed in, and nothing
//! supervises the agent. Every one of those facts is already in the node's
//! capability report — this assembles them into an answer.
//!
//! **A pure function of [`Capabilities`], and that is the whole design.** The
//! control plane stores that report verbatim, so the same checklist is readable
//! centrally for any node without contacting it (AC-2) — which matters most for
//! exactly the node this ticket is about, the one that has gone dark.

use nook_types::{AuthState, Capabilities, SandboxCapability, SandboxUnavailable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Satisfied.
    Ok,
    /// Not satisfied, but nothing this node would otherwise do is blocked.
    Warn,
    /// Work this node could be doing is refused because of it.
    Fail,
}

impl Verdict {
    pub fn mark(self) -> &'static str {
        match self {
            Verdict::Ok => "\u{2713}",
            Verdict::Warn => "\u{26A0}",
            Verdict::Fail => "\u{2717}",
        }
    }
}

/// One prerequisite, its state, and the command that fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
    /// **Never `None` on a gate that is not `Ok`.** A checklist that says a
    /// machine is broken and not what to do about it is the state this ticket
    /// was filed from; `every_unmet_gate_names_its_remedy` fails the build if a
    /// new gate arrives without one.
    pub remedy: Option<String>,
}

impl Gate {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn unmet(
        name: &'static str,
        verdict: Verdict,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            name,
            verdict,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// The whole checklist, in the order an operator fixes it: supervision first,
/// because an unsupervised node cannot even be repaired by `nook update`.
pub fn assess(caps: &Capabilities) -> Vec<Gate> {
    vec![
        supervision(caps),
        toolchain(caps),
        runtime_auth(caps),
        sandbox(caps),
        loop_kinds(caps),
        port_range(caps),
    ]
}

/// Is anything refusing this node work? A `Warn` is not — that is the whole
/// difference between the two unmet verdicts.
pub fn blocked(gates: &[Gate]) -> bool {
    gates.iter().any(|g| g.verdict == Verdict::Fail)
}

/// The one line under the checklist, and the verdict that colours it.
///
/// Its whole job is to be TRUE, which is why it is a function here rather than
/// a branch at the call site: `blocked` is the only thing that decides whether
/// this node claims work, so a gate that refuses nothing can never make this
/// sentence say it does. A node unsupervised and otherwise perfect used to be
/// told it claimed no loop work while it was running some.
///
/// Three states, not two: "nothing refuses this work" and "everything is
/// answered" are different sentences, and collapsing them would make an
/// unsupervised node read as simply `Ready.` — the opposite error.
pub fn summary(gates: &[Gate]) -> (Verdict, &'static str) {
    if blocked(gates) {
        return (Verdict::Fail, "This node claims no loop work.");
    }
    if gates.iter().any(|g| g.verdict == Verdict::Warn) {
        return (
            Verdict::Warn,
            "Claiming work — with the unmet gates above still worth fixing.",
        );
    }
    (Verdict::Ok, "Ready.")
}

/// Nothing restarts the agent — the risk that is easiest to miss and worst to
/// hit, because the machine is fine until the day it updates itself.
///
/// **`Warn`, not `Fail`, and the distinction is the module's own**: `Fail`
/// means work this node could be doing is refused because of it, and nothing
/// in the control plane gates placement on supervision. An unsupervised node
/// claims and runs loop work right up to the moment it self-updates. Calling
/// that a failure made the summary line say it claimed nothing while it was
/// running something.
///
/// It is not a demotion of the fact — the gate is still first in the
/// checklist, still carries the remedy, and `supervision_warning` is a second,
/// louder channel that fires at join time.
fn supervision(caps: &Capabilities) -> Gate {
    match caps.supervision.as_deref() {
        Some(s) => Gate::ok("supervision", s),
        None => Gate::unmet(
            "supervision",
            Verdict::Warn,
            "nothing will restart this agent — a self-update replaces the binary \
             and exits, and the machine goes dark for good",
            format!("nook setup --service {}", default_service(&caps.platform)),
        ),
    }
}

/// The supervisor to suggest on this platform. macOS has no systemd, and
/// telling a Mac owner to run `systemctl` is how a remedy line stops being one.
fn default_service(platform: &str) -> &'static str {
    if platform == "macos" {
        "launchd"
    } else {
        "systemd-user"
    }
}

/// The executables a session needs before any of the rest matters: tmux to hold
/// one, git to make a checkout, and an agent runtime to actually be the agent.
///
/// One gate rather than three, because they fail together on the machine this
/// was filed from — a bare VM has none of them — and three lines saying "install
/// something" is not three pieces of information.
fn toolchain(caps: &Capabilities) -> Gate {
    let agents: Vec<&str> = crate::runtime_auth::authable_runtimes();
    let installed: Vec<&str> = agents
        .iter()
        .copied()
        .filter(|r| caps.runtimes.iter().any(|have| have == r))
        .collect();

    let mut missing: Vec<String> = Vec::new();
    let mut remedies: Vec<String> = Vec::new();
    if !caps.tmux {
        missing.push("tmux".into());
        remedies.push("install tmux".into());
    }
    if caps.git.is_none() {
        missing.push("git".into());
        remedies.push("install git".into());
    }
    if installed.is_empty() {
        missing.push(format!("an agent runtime ({})", agents.join(" / ")));
        remedies.push(format!(
            "install one of {} on that machine's PATH",
            agents.join(", ")
        ));
    }

    let found = if caps.runtimes.is_empty() {
        "no runtimes detected".to_string()
    } else {
        format!("runtimes: {}", caps.runtimes.join(", "))
    };
    if missing.is_empty() {
        return Gate::ok("toolchain", format!("{found}; tmux, git"));
    }
    Gate::unmet(
        "toolchain",
        Verdict::Fail,
        format!("missing {} — {found}", missing.join(", ")),
        remedies.join("; "),
    )
}

/// Installed is not signed in. The login itself stays a human action (NG-2);
/// this only makes it a NAMED step instead of a surprise.
fn runtime_auth(caps: &Capabilities) -> Gate {
    // Nothing installed to sign in to — either because the node reported no
    // profiles at all, or because every profile it reported is for a runtime
    // that is not there.
    //
    // Warn, not fail: the toolchain gate above has already failed for this same
    // cause, and two failures for one cause reads as two problems. Answering
    // that one makes this line move on its own.
    if caps
        .runtime_auth
        .iter()
        .all(|p| p.state == AuthState::Unavailable)
    {
        return Gate::unmet(
            "runtime auth",
            Verdict::Warn,
            "no runtime to authorize — nothing that needs a login is installed",
            format!(
                "install one of {} first",
                crate::runtime_auth::authable_runtimes().join(", ")
            ),
        );
    }

    let authorized: Vec<String> = caps
        .runtime_auth
        .iter()
        .filter(|p| p.state == AuthState::Authorized)
        .map(|p| match &p.identity {
            Some(who) => format!("{} ({who})", p.label),
            None => p.label.clone(),
        })
        .collect();
    if !authorized.is_empty() {
        return Gate::ok("runtime auth", authorized.join(", "));
    }

    // Only the profiles whose runtime is actually here. A machine with claude
    // installed and signed out does not need to be told that codex and hermes
    // are absent — the toolchain line already covers what is missing, and this
    // line has one job: which installed runtime needs a login.
    let present: Vec<_> = caps
        .runtime_auth
        .iter()
        .filter(|p| p.state != AuthState::Unavailable)
        .collect();
    let detail = present
        .iter()
        .map(|p| format!("{}: {}", p.label, state_word(p.state)))
        .collect::<Vec<_>>()
        .join(", ");
    // Whichever unauthorized profile we can actually name a command for. The
    // device login is the one irreducibly manual step in the whole path, so the
    // checklist says so rather than implying a flag exists that would skip it.
    let remedy = present
        .iter()
        .find_map(|p| {
            crate::runtime_auth::login_args(&p.runtime).map(|args| format!("{} {args}", p.runtime))
        })
        .unwrap_or_else(|| "sign the runtime in on that machine".to_string());
    Gate::unmet(
        "runtime auth",
        Verdict::Fail,
        detail,
        format!("{remedy}  (a device login is a human step — run it on that machine)"),
    )
}

fn state_word(s: AuthState) -> &'static str {
    match s {
        AuthState::Authorized => "authorized",
        AuthState::NotAuthorized => "signed out",
        AuthState::Unavailable => "not installed",
        AuthState::Unknown => "unknown",
    }
}

/// The gate that is easiest to misread as build-only: `sandbox_refusal` applies
/// to EVERY loop kind, so a host node without the image cannot even run a spec
/// pass.
fn sandbox(caps: &Capabilities) -> Gate {
    match &caps.sandbox {
        Some(SandboxCapability::Ready { image }) => Gate::ok("sandbox", image),
        Some(SandboxCapability::Exempt { detail }) => {
            Gate::ok("sandbox", format!("not needed — {detail}"))
        }
        // Minutes from working rather than never (MAIN-643): a warming node is
        // not something to go and fix.
        Some(SandboxCapability::Pulling { image }) => Gate::unmet(
            "sandbox",
            Verdict::Warn,
            format!("pulling {image} — queued jobs start when it settles"),
            "nothing to do; wait for the pull",
        ),
        Some(SandboxCapability::Unavailable { detail, reason }) => Gate::unmet(
            "sandbox",
            Verdict::Fail,
            format!("{} — {detail}", reason.label()),
            sandbox_remedy(reason),
        ),
        // Silence is refusal here, deliberately (MAIN-611): an agent that has
        // not said it confines anything is one that predates confinement.
        None => Gate::unmet(
            "sandbox",
            Verdict::Fail,
            "this agent does not report a sandbox at all, so it is refused every \
             loop kind",
            "nook update  (then reconnect — a capability change travels on Register)",
        ),
    }
}

fn sandbox_remedy(reason: &SandboxUnavailable) -> &'static str {
    match reason {
        SandboxUnavailable::NoDocker => "install Docker and start its daemon",
        // The node pulls the tag matching its own version, so a tag that was
        // never published is an agent ahead of the release, not a missing step.
        SandboxUnavailable::NotPublished => {
            "nook update  (this agent's version has no published sandbox image)"
        }
        SandboxUnavailable::NoCredentials => "docker login ghcr.io",
        SandboxUnavailable::PullRefused => {
            "check that machine's egress to ghcr.io, then restart the agent"
        }
        // An operator naming their own image owns it: nothing will pull it for
        // them, which is the point of NOOK_SANDBOX_IMAGE.
        SandboxUnavailable::NotPresent => {
            "build or pull the image NOOK_SANDBOX_IMAGE names on that machine"
        }
        SandboxUnavailable::Unknown => "nook get nodes --json  (the node's own sandbox detail)",
    }
}

/// The gate the filed machine hit first: an empty list is not "idle", it is
/// "accepts nothing", and every other column reads as a healthy node.
fn loop_kinds(caps: &Capabilities) -> Gate {
    if caps.loop_kinds.is_empty() {
        return Gate::unmet(
            "loop kinds",
            Verdict::Fail,
            "declares none — this node accepts no loop work of any kind",
            "set NOOK_LOOP_KINDS=spec,review,build in the agent's environment, then restart it",
        );
    }
    Gate::ok("loop kinds", caps.loop_kinds.join(", "))
}

/// Not a failure on its own: plenty of sessions run no server. It becomes one
/// only for a workspace that declares a required listener, which is why this
/// warns rather than refuses.
fn port_range(caps: &Capabilities) -> Gate {
    match caps.port_range {
        Some((start, end)) => Gate::ok(
            "port range",
            format!("{start}-{end} ({} ports)", u32::from(end - start) + 1),
        ),
        None => Gate::unmet(
            "port range",
            Verdict::Warn,
            "none — a session or build whose workspace declares a REQUIRED \
             listener will not start here",
            "set NOOK_PORT_RANGE=4200-4299 in the agent's environment, then restart it",
        ),
    }
}

/// The line `nook join` prints when it leaves a machine unsupervised (AC-3).
///
/// `None` when something does supervise it, so the automation path stays quiet
/// on the case that is fine. Joining succeeds either way — this warns, it never
/// fails the join.
pub fn supervision_warning(supervision: Option<&str>, platform: &str) -> Option<String> {
    if supervision.is_some() {
        return None;
    }
    Some(format!(
        "Nothing will restart this agent.\n  \
         `nook update` replaces the binary and exits for a supervisor that is not\n  \
         there, so this machine would go offline permanently the next time the fleet\n  \
         updates. Install one now:\n\n    \
         nook join --service {svc} …    (or, on this machine: nook setup --service {svc})",
        svc = default_service(platform),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nook_types::{AuthProfile, GpuInfo};

    /// A node with every gate failing — the VM this ticket was filed from.
    fn bare() -> Capabilities {
        Capabilities {
            hostname: "vm".into(),
            platform: "linux".into(),
            architecture: "x86_64".into(),
            cpus: 4,
            memory: 8 << 30,
            gpus: Vec::<GpuInfo>::new(),
            docker: false,
            tmux: false,
            git: None,
            agent_version: Some("0.6.13".into()),
            runtimes: vec!["bash".into()],
            chat_runtimes: Vec::new(),
            ssh_public_key: None,
            shared_operator: false,
            isolated_builds: false,
            loop_kinds: Vec::new(),
            max_loop_jobs: Some(2),
            max_loop_jobs_pinned: false,
            port_range: None,
            runtime_auth: Vec::new(),
            sandbox: Some(SandboxCapability::Unavailable {
                detail: "the job image is not on this node".into(),
                reason: SandboxUnavailable::NotPresent,
            }),
            supervision: None,
        }
    }

    /// The same machine after every gate is answered.
    fn ready() -> Capabilities {
        Capabilities {
            docker: true,
            tmux: true,
            git: Some("2.43.0".into()),
            runtimes: vec!["claude".into(), "bash".into()],
            loop_kinds: vec!["spec".into(), "build".into()],
            port_range: Some((4200, 4299)),
            runtime_auth: vec![AuthProfile {
                id: "claude".into(),
                label: "Claude Code".into(),
                runtime: "claude".into(),
                state: AuthState::Authorized,
                identity: Some("fleet@example.com".into()),
            }],
            sandbox: Some(SandboxCapability::Ready {
                image: "ghcr.io/nook-os/nook-job-sandbox:0.6.13".into(),
            }),
            supervision: Some("systemd-user".into()),
            ..bare()
        }
    }

    fn gate<'a>(gates: &'a [Gate], name: &str) -> &'a Gate {
        gates
            .iter()
            .find(|g| g.name == name)
            .unwrap_or_else(|| panic!("no {name} gate in {gates:?}"))
    }

    /// The sentence the checklist ends on, which nothing used to assert — and
    /// which was therefore false for the one node most likely to read it.
    ///
    /// A box joined with a bare `nook join`, with tmux, git, `claude` signed
    /// in, `NOOK_LOOP_KINDS=spec,build` and a ready sandbox, IS claiming loop
    /// work. Telling it that it claims none — and answering `"ready": false` —
    /// is a lie about the only thing the summary exists to say.
    #[test]
    fn an_unsupervised_but_working_node_is_not_told_it_claims_nothing() {
        let caps = Capabilities {
            supervision: None,
            ..ready()
        };
        let gates = assess(&caps);

        assert_eq!(gate(&gates, "supervision").verdict, Verdict::Warn);
        assert!(
            !blocked(&gates),
            "nothing in the control plane gates placement on supervision: {gates:?}"
        );

        let (verdict, sentence) = summary(&gates);
        assert_eq!(verdict, Verdict::Warn);
        assert_eq!(
            sentence,
            "Claiming work — with the unmet gates above still worth fixing."
        );
    }

    /// The other two states of that same sentence, so "claims nothing" is
    /// reserved for a node that really claims nothing and `Ready.` is reserved
    /// for one with nothing left to fix.
    #[test]
    fn the_summary_tells_the_three_states_apart() {
        assert_eq!(
            summary(&assess(&bare())),
            (Verdict::Fail, "This node claims no loop work.")
        );
        assert_eq!(summary(&assess(&ready())), (Verdict::Ok, "Ready."));

        // One genuinely refusing gate is enough, however healthy the rest.
        let no_kinds = Capabilities {
            loop_kinds: Vec::new(),
            ..ready()
        };
        assert_eq!(
            summary(&assess(&no_kinds)),
            (Verdict::Fail, "This node claims no loop work.")
        );
    }

    /// AC-1: run it against a node missing all of them and every gate appears
    /// with the command that fixes it.
    #[test]
    fn every_gate_appears_for_a_node_missing_all_of_them() {
        let gates = assess(&bare());
        for name in [
            "supervision",
            "toolchain",
            "runtime auth",
            "sandbox",
            "loop kinds",
            "port range",
        ] {
            let g = gate(&gates, name);
            assert_ne!(g.verdict, Verdict::Ok, "{name} should not pass: {g:?}");
        }
    }

    /// The invariant the whole card is about: a gate that is not satisfied says
    /// what to run. A checklist without remedies is the state we started in.
    #[test]
    fn every_unmet_gate_names_its_remedy() {
        for caps in [bare(), ready()] {
            for g in assess(&caps) {
                if g.verdict == Verdict::Ok {
                    continue;
                }
                let remedy = g.remedy.as_deref().unwrap_or("");
                assert!(!remedy.trim().is_empty(), "{} has no remedy: {g:?}", g.name);
                assert!(
                    !g.detail.trim().is_empty(),
                    "{} has no detail: {g:?}",
                    g.name
                );
            }
        }
    }

    #[test]
    fn a_fully_provisioned_node_passes_every_gate() {
        for g in assess(&ready()) {
            assert_eq!(g.verdict, Verdict::Ok, "{} should pass: {g:?}", g.name);
        }
    }

    /// Each gate singly: the other five pass, so the checklist points at one
    /// thing rather than at everything.
    #[test]
    fn one_missing_prerequisite_fails_one_gate() {
        let cases: Vec<(&str, Capabilities)> = vec![
            (
                "supervision",
                Capabilities {
                    supervision: None,
                    ..ready()
                },
            ),
            (
                "toolchain",
                Capabilities {
                    runtimes: vec!["bash".into()],
                    ..ready()
                },
            ),
            (
                "runtime auth",
                Capabilities {
                    runtime_auth: vec![AuthProfile {
                        id: "claude".into(),
                        label: "Claude Code".into(),
                        runtime: "claude".into(),
                        state: AuthState::NotAuthorized,
                        identity: None,
                    }],
                    ..ready()
                },
            ),
            (
                "sandbox",
                Capabilities {
                    sandbox: None,
                    ..ready()
                },
            ),
            (
                "loop kinds",
                Capabilities {
                    loop_kinds: Vec::new(),
                    ..ready()
                },
            ),
            (
                "port range",
                Capabilities {
                    port_range: None,
                    ..ready()
                },
            ),
        ];
        for (name, caps) in cases {
            let gates = assess(&caps);
            assert_ne!(gate(&gates, name).verdict, Verdict::Ok, "{name}");
            for g in &gates {
                if g.name != name {
                    assert_eq!(
                        g.verdict,
                        Verdict::Ok,
                        "{} should be unaffected: {g:?}",
                        g.name
                    );
                }
            }
        }
    }

    /// A node with no agent runtime reports a profile per adapter, all
    /// `Unavailable` — which is the toolchain gate's failure, not a second one.
    /// Naming it twice would send an operator to sign in to something that is
    /// not installed.
    #[test]
    fn a_runtime_that_is_not_installed_is_not_a_second_failure() {
        let caps = Capabilities {
            runtimes: vec!["bash".into()],
            runtime_auth: vec![
                AuthProfile {
                    id: "claude".into(),
                    label: "Claude Code".into(),
                    runtime: "claude".into(),
                    state: AuthState::Unavailable,
                    identity: None,
                },
                AuthProfile {
                    id: "codex".into(),
                    label: "Codex CLI".into(),
                    runtime: "codex".into(),
                    state: AuthState::Unavailable,
                    identity: None,
                },
            ],
            ..ready()
        };
        let gates = assess(&caps);
        assert_eq!(gate(&gates, "toolchain").verdict, Verdict::Fail);
        assert_eq!(gate(&gates, "runtime auth").verdict, Verdict::Warn);
    }

    /// The other half: one runtime installed and signed out, the rest absent.
    /// The line names the one that can actually be logged in.
    #[test]
    fn only_the_installed_runtimes_reach_the_auth_line() {
        let caps = Capabilities {
            runtime_auth: vec![
                AuthProfile {
                    id: "claude".into(),
                    label: "Claude Code".into(),
                    runtime: "claude".into(),
                    state: AuthState::NotAuthorized,
                    identity: None,
                },
                AuthProfile {
                    id: "codex".into(),
                    label: "Codex CLI".into(),
                    runtime: "codex".into(),
                    state: AuthState::Unavailable,
                    identity: None,
                },
            ],
            ..ready()
        };
        let gates = assess(&caps);
        let g = gate(&gates, "runtime auth");
        assert_eq!(g.verdict, Verdict::Fail);
        assert_eq!(g.detail, "Claude Code: signed out");
        assert!(
            g.remedy
                .as_deref()
                .is_some_and(|r| r.starts_with("claude ")),
            "the remedy names the installed runtime: {g:?}"
        );
    }

    /// A containerised node has nothing to confine (MAIN-611 NG-5) and must not
    /// be told to go and install an image.
    #[test]
    fn a_containerised_node_passes_the_sandbox_gate() {
        let caps = Capabilities {
            sandbox: Some(SandboxCapability::Exempt {
                detail: "/.dockerenv is present".into(),
            }),
            ..ready()
        };
        assert_eq!(gate(&assess(&caps), "sandbox").verdict, Verdict::Ok);
    }

    /// A pull in flight is temporary, and saying "broken" about it sends
    /// somebody to a machine that is about to work on its own.
    #[test]
    fn a_pulling_node_warns_rather_than_fails() {
        let caps = Capabilities {
            sandbox: Some(SandboxCapability::Pulling {
                image: "ghcr.io/nook-os/nook-job-sandbox:0.6.13".into(),
            }),
            ..ready()
        };
        assert_eq!(gate(&assess(&caps), "sandbox").verdict, Verdict::Warn);
    }

    /// AC-3, both halves: the warning appears when nothing supervises the
    /// agent, and is absent when something does.
    #[test]
    fn the_join_warning_appears_only_when_unsupervised() {
        let warning = supervision_warning(None, "linux").expect("unsupervised warns");
        assert!(
            warning.contains("nook join --service systemd-user"),
            "it names the fix: {warning}"
        );
        assert!(
            warning.contains("nook update"),
            "and why it matters: {warning}"
        );
        assert_eq!(supervision_warning(Some("systemd-user"), "linux"), None);
        assert_eq!(supervision_warning(Some("docker"), "linux"), None);
    }

    /// A Mac has no systemd, and a remedy naming `systemctl` there is not one.
    #[test]
    fn the_suggested_supervisor_matches_the_platform() {
        assert!(supervision_warning(None, "macos")
            .expect("warns")
            .contains("launchd"));
        let caps = Capabilities {
            platform: "macos".into(),
            supervision: None,
            ..ready()
        };
        let gates = assess(&caps);
        let g = gate(&gates, "supervision");
        assert_eq!(g.remedy.as_deref(), Some("nook setup --service launchd"));
    }
}
