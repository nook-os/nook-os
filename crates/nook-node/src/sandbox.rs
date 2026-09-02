//! Per-job container confinement (MAIN-611).
//!
//! A loop-job agent's instructions are UNTRUSTED INPUT — a card body, a PR
//! comment, a dependency's README. Before this module the agent ran as the
//! node's OS user with that user's whole world: every file in `$HOME`, every
//! sibling checkout, the node's own credentials, the host's Docker, and the
//! LAN. A successful injection reached the owner's personal machine.
//!
//! So every job agent on a HOST node runs inside its own container: only the
//! job's checkout mounted, a private `/tmp`, its own nested Docker daemon, and
//! an egress policy that drops the private address space. The host's Docker
//! socket is never mounted — a container holding it undoes all of the above in
//! one `docker run -v /:/host`.
//!
//! Everything a reviewer has to check is a PURE FUNCTION here — [`run_args`],
//! [`egress_script`], [`isolation_args`] — because "which flags does a job
//! container actually get" is the whole security argument, and an argument you
//! can only observe by starting Docker is one nobody re-checks.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use nook_types::{SandboxCapability, SandboxUnavailable};

/// Where the published job sandbox image lives. Built by
/// `deploy/docker/job-sandbox.Dockerfile` and pushed by the release workflow
/// beside every other image (MAIN-643 AC-1).
pub const IMAGE_REPO: &str = "ghcr.io/nook-os/nook-job-sandbox";

/// The stand-in an allow entry uses for "the machine this container runs on",
/// resolved inside the container because only it knows its own gateway. The
/// dev stack's control plane is on the host's loopback and is reachable no
/// other way.
pub const HOST_GATEWAY: &str = "HOST_GATEWAY";

/// The NAME a container reaches the host by, aliased to the gateway with
/// `--add-host`.
///
/// It is not `localhost`, and it cannot be: Docker writes `127.0.0.1 localhost`
/// into every container's `/etc/hosts` before any `--add-host` of ours, the
/// resolver takes the first match, and the alias is silently shadowed. Aliasing
/// a name with no built-in entry is the only form that survives.
pub const HOST_ALIAS: &str = "host.docker.internal";

/// The address space a job container may not reach (AC-5): RFC1918 plus
/// link-local. No SSH to a NAS, no scanning the LAN, no cloud metadata service.
///
/// A DENY list rather than an allow list on purpose (NG-7): the public internet
/// has to keep working — a policy that breaks `npm install` is one an operator
/// turns off — and naming which external hosts are permitted is a later card.
pub const PRIVATE_RANGES: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
];

/// The agent's `HOME` inside the container. NOT the host's home path: the whole
/// point is that the host's `$HOME` is absent, and pointing `HOME` at a path
/// Docker scaffolded for a mount would put dotfiles inside the bind.
pub const AGENT_HOME: &str = "/home/agent";

/// Where the node's Claude session (and its skills) is mounted. A fixed path
/// rather than the host's, for [`AGENT_HOME`]'s reason — and the same path the
/// containerised nodes already use, so one convention covers both.
pub const CLAUDE_DIR: &str = "/nook-claude";

/// What one loop KIND needs of its sandbox (AC-12).
///
/// A RECORD, not a branch. Confinement itself is universal by construction —
/// `job_adapter::adapter_for` selects by runtime and never by kind, so every
/// kind already funnels through the two wrapped adapters — and the mounts of
/// AC-2 and the egress policy of AC-5 are identical for all of them. The one
/// thing that genuinely differs is whether the kind runs containers, and that
/// is one field. A new kind picks a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub kind: &'static str,
    /// Provision the nested Docker daemon (AC-4).
    ///
    /// `build` needs it: `./test.sh`, `dev-up.sh` and the card's own compose
    /// stack are the work. Nothing else does — a spec agent files a ticket, a
    /// review agent reads a PR, an investigate agent reads code and a sealed
    /// email — and giving them one would cost every such run a daemon cold
    /// start, plus the `seccomp`/`apparmor` relaxation nesting requires. So
    /// they get a STRICTER box, not merely a cheaper one.
    pub nested_docker: bool,
}

/// The profile table. Every kind in `capabilities::KNOWN_LOOP_KINDS` has a row
/// here, and a test fails the build if a new kind arrives without one.
pub const PROFILES: &[Profile] = &[
    Profile {
        kind: "build",
        nested_docker: true,
    },
    Profile {
        kind: "review",
        nested_docker: false,
    },
    Profile {
        kind: "spec",
        nested_docker: false,
    },
    Profile {
        kind: "decompose",
        nested_docker: false,
    },
    Profile {
        kind: "epic-run",
        nested_docker: false,
    },
    // The one kind whose brief is written by a STRANGER (MAIN-331). Every other
    // kind's input originates inside the tenant — a card a member wrote, a PR
    // somebody opened — while an investigate run is driven by a support email
    // that arrived unauthenticated from outside. So of the five, this is the
    // last one to hand a nesting relaxation to: the run already gets no forge
    // credential and a throwaway worktree, and a privileged daemon would be the
    // one powerful thing left in a box built to be read-only.
    //
    // The cost is real and named rather than hidden: `nook-investigate/SKILL.md`
    // §2 tells the agent to reproduce the fault, and on a repo whose
    // reproduction is `./run.sh` it cannot. That section says so now instead of
    // promising a box it does not get.
    Profile {
        kind: "investigate",
        nested_docker: false,
    },
];

/// What a kind with no row gets. No Docker: an unrecognised kind is the one
/// case where guessing generously is guessing wrong.
pub const DEFAULT_PROFILE: Profile = Profile {
    kind: "",
    nested_docker: false,
};

/// The profile a kind runs under (AC-12).
pub fn profile_for(kind: &str) -> Profile {
    PROFILES
        .iter()
        .find(|p| p.kind == kind)
        .copied()
        .unwrap_or(DEFAULT_PROFILE)
}

/// How the nested Docker daemon is isolated (AC-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Docker-in-Docker WITHOUT `--privileged`: `CAP_SYS_ADMIN` for the mounts
    /// a daemon makes, `CAP_NET_ADMIN` for the egress policy, and Docker's
    /// seccomp and AppArmor profiles off because a nested daemon's syscalls are
    /// what they exist to refuse.
    ///
    /// **The device cgroup is untouched, and that is the whole difference.**
    /// This container's `/dev` holds the ordinary dozen entries and no block
    /// device: measured on 2026-08-15, `mknod` of a disk node succeeds (it is
    /// in Docker's default capability set) and the `mount` that follows is
    /// refused by the cgroup. Under `--privileged` the same container's `/dev`
    /// carries the host's `sda`…`sdf`, and that path is open.
    ///
    /// **ROOTLESS DinD was the first choice and does not work here.** Measured
    /// the same day, on this kernel, in this image: `rootlesskit` dies with
    /// *"failed to setup UID/GID map: newuidmap … write to uid_map failed:
    /// Operation not permitted"* in any container that is not `--privileged` —
    /// and rootless-inside-privileged is the weaker box above with extra steps,
    /// since the escape is the outer container's `/dev` either way. Docker's
    /// own `dind-rootless` documentation says the same: it still wants
    /// `--privileged`. So this is the safer mode that actually runs, rather
    /// than the safer mode on paper.
    Unprivileged,
    /// `--privileged`, for a kernel where the above will not start.
    ///
    /// **It is a weaker box, and knowingly so**: a privileged container has the
    /// host's `/dev`, so a job that gets to a block device reads the machine's
    /// disk and AC-2 is undone without ever touching a Docker socket. Opt in
    /// with `NOOK_SANDBOX_ISOLATION=privileged`, and expect `nook get nodes` to
    /// say which one a machine got.
    Privileged,
}

impl Isolation {
    fn as_str(self) -> &'static str {
        match self {
            Isolation::Unprivileged => "unprivileged",
            Isolation::Privileged => "privileged",
        }
    }

    /// From `NOOK_SANDBOX_ISOLATION`. Anything unrecognised is the unprivileged
    /// mode: the safer box is what an unreadable setting should get.
    pub fn parse(raw: &str) -> Isolation {
        match raw.trim().to_ascii_lowercase().as_str() {
            "privileged" => Isolation::Privileged,
            _ => Isolation::Unprivileged,
        }
    }
}

/// A read-write bind mount, host path to container path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub container: PathBuf,
}

/// Everything a job container is built from. Assembled by the caller so that
/// [`run_args`] stays pure and the argument vector is testable without Docker.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub job_id: String,
    pub image: String,
    /// What this job's KIND needs (AC-12), looked up rather than branched on.
    pub profile: Profile,
    pub isolation: Isolation,
    /// The job's checkout, mounted at the SAME path it has on the host.
    ///
    /// Same path, deliberately: the agent's warm Claude session is keyed on its
    /// working directory, the control plane records this path on the card, and
    /// `prune-worktree` addresses it. A container-local `/workspace` would make
    /// every one of those disagree with what the agent sees.
    pub worktree: PathBuf,
    /// The mirror the checkout is a linked worktree OF.
    ///
    /// Part of "the job's checkout" (AC-2) even though it is a second path: a
    /// linked worktree's `.git` is a *file* pointing here, so without it the
    /// container has a directory of source and no repository at all. It is the
    /// one repo this job is working on — never the clone-cache root, which
    /// holds every sibling checkout on the machine.
    pub gitdir: Option<PathBuf>,
    /// The node's Claude session directory — the credential AC-7 says the agent
    /// legitimately needs, plus the loop skills it is told to run.
    pub claude_dir: Option<PathBuf>,
    /// Extra read-write mounts the WORKSPACE declares, from its `.nook.toml`
    /// `[sandbox] caches`. Nothing is mounted because the node happens to have
    /// it; a repo asks for its own package cache by name.
    pub caches: Vec<Mount>,
    /// Checkouts of the workspaces the run's CARD names with `@slug`
    /// (MAIN-632), mounted READ-ONLY at their own host paths.
    ///
    /// The card is the whole authority: only a workspace it names is here, so
    /// an unreferenced sibling checkout stays exactly as unreachable as it was
    /// (AC-6). Read-only because the card produces one PR in its own workspace
    /// and nothing else (NG-1) — a writable mount would make "no writes to a
    /// referenced repo" a thing the agent is asked to observe rather than a
    /// thing the box enforces.
    pub references: Vec<PathBuf>,
    /// Host ports this run leased (MAIN-552), published from the job container
    /// so a human can still open the build's dev stack in a browser. The lease
    /// is what makes the bind free.
    pub ports: Vec<u16>,
    /// Resolved addresses the egress policy lets through — the control plane,
    /// and nothing else private (AC-5).
    pub allow: Vec<String>,
    /// `--add-host` entries. One case only: a control plane on the host's
    /// loopback, whose NAME has to resolve to the gateway inside or the agent's
    /// `nook` cannot reach the board it was issued a token for.
    pub add_hosts: Vec<String>,
    /// The control-plane URL **as spelled inside the container**
    /// ([`server_for_container`]). Empty means "say nothing", which is what the
    /// escape suite uses — it attacks the box, it does not talk to a board.
    pub server: String,
    /// The uid/gid the AGENT runs as inside. The container itself is root (the
    /// nested daemon and the firewall need that); the agent is not, so what it
    /// writes into the bind-mounted checkout is owned by the node's user and
    /// the prune that follows can delete it (MAIN-537).
    pub agent_uid: u32,
    pub agent_gid: u32,
}

/// The label every object this module creates carries, and the ONLY thing
/// [`sweep_orphans`] matches on (MAIN-617 AC-5).
///
/// A NAME would very nearly do — [`container_name`] and [`network_name`] are
/// both derived from the job id — but "very nearly" is the wrong standard for a
/// function whose job is deleting things on a machine that also runs the
/// owner's own work. The label is a claim NookOS made about an object it
/// created; a name is a coincidence anyone can reproduce.
pub const JOB_LABEL: &str = "nook.job";

/// The job's own Docker network, named for it so a leftover is identifiable.
pub fn network_name(job_id: &str) -> String {
    format!("{}-net", container_name(job_id))
}

/// The network a job's container joins, LABELLED so the sweep can find it again
/// after the node that created it has died (MAIN-617 AC-2).
pub fn network_create_args(job_id: &str) -> Vec<String> {
    vec![
        "network".into(),
        "create".into(),
        "--label".into(),
        format!("{JOB_LABEL}={job_id}"),
        network_name(job_id),
    ]
}

/// A container's name, derived from the job id so a leftover is identifiable.
pub fn container_name(job_id: &str) -> String {
    let safe: String = job_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("nook-job-{safe}")
}

/// The isolation flags, alone, so the security-relevant half of [`run_args`]
/// can be read and asserted without the mounts around it.
///
/// A profile that runs no containers gets NONE of the nesting relaxations —
/// it keeps Docker's default seccomp and AppArmor profiles and is handed no
/// device. That is the ordinary case (spec, review, decompose, epic-run), so
/// the strictest box is also the common one.
pub fn isolation_args(profile: Profile, mode: Isolation) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if !profile.nested_docker {
        a.push("--cap-add".into());
        a.push("NET_ADMIN".into());
        return a;
    }
    match mode {
        Isolation::Unprivileged => {
            // A nested daemon mounts overlays and makes namespaces; Docker's
            // default seccomp and AppArmor profiles refuse exactly that (the
            // same wall MAIN-609 measured for bubblewrap). Turning those two
            // off, and adding one capability, is what makes nesting work
            // WITHOUT handing over the host's devices — which `--privileged`
            // does and this deliberately does not.
            a.push("--security-opt".into());
            a.push("seccomp=unconfined".into());
            a.push("--security-opt".into());
            a.push("apparmor=unconfined".into());
            a.push("--cap-add".into());
            a.push("SYS_ADMIN".into());
        }
        Isolation::Privileged => a.push("--privileged".into()),
    }
    // The firewall is applied inside the container's OWN network namespace, so
    // this capability reaches nothing else. It is added in both modes because
    // AC-5 is not optional.
    a.push("--cap-add".into());
    a.push("NET_ADMIN".into());
    a
}

/// The exact `docker` argv a job container is started with.
///
/// Pure, and that is the point: AC-3 is the claim that `/var/run/docker.sock`
/// is not in this list, and AC-2 the claim that nothing outside the checkout
/// is. Both are assertions about a vector, so both are unit tests.
pub fn run_args(spec: &SandboxSpec) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        container_name(&spec.job_id),
        // A leftover container must not outlive the daemon's view of it; the
        // node removes it explicitly too (see `Sandbox::stop`).
        "--rm".into(),
        "--label".into(),
        format!("{JOB_LABEL}={}", spec.job_id),
        // A network of the job's OWN, and not the default bridge, because that
        // is what gets Docker's embedded resolver on 127.0.0.11. Measured: on
        // the default bridge the container inherits the host's nameservers,
        // which on any ordinary machine are RFC1918 — so AC-5's policy dropped
        // them and every lookup failed, taking AC-6 with it. The embedded
        // resolver is reached over loopback and forwarded from the HOST's
        // namespace, so the drop never applies to it.
        "--network".into(),
        network_name(&spec.job_id),
        // Its own cgroup namespace, so the nested daemon's cgroups are created
        // under a root of its own and the host's tree is neither visible nor
        // writable. The alternative every DinD recipe reaches for is
        // `-v /sys/fs/cgroup:/sys/fs/cgroup:rw`, which hands a job the HOST's
        // cgroup tree — the thing AC-2 is about.
        "--cgroupns".into(),
        "private".into(),
        // A PRIVATE /tmp (AC-2). `exec` because build toolchains write and run
        // scripts there; `nosuid`/`nodev` because nothing legitimate needs
        // either and a writable world-shared /tmp is where escalation starts.
        "--tmpfs".into(),
        "/tmp:rw,exec,nosuid,nodev".into(),
    ];
    a.extend(isolation_args(spec.profile, spec.isolation));
    if spec.profile.nested_docker {
        // The nested daemon's own storage, on an ANONYMOUS per-job volume.
        //
        // Not a host path and not a hole in AC-2: Docker creates it empty, it
        // holds only images this job pulled, and `--rm` destroys it with the
        // container. It exists because overlayfs will not stack on overlayfs —
        // measured, the nested daemon's first `docker run` dies with `mount …
        // fstype: overlay … invalid argument` — which is the same reason the
        // official `docker:dind` image declares this exact volume.
        a.push("-v".into());
        a.push("/var/lib/docker".into());
    }
    // THE CHECKOUT, at the same path, read-write. First mount and the only one
    // the job's own work touches.
    a.push("-v".into());
    a.push(bind(&spec.worktree, &spec.worktree));
    if let Some(gitdir) = &spec.gitdir {
        a.push("-v".into());
        a.push(bind(gitdir, gitdir));
    }
    if let Some(claude) = &spec.claude_dir {
        a.push("-v".into());
        a.push(bind(claude, Path::new(CLAUDE_DIR)));
    }
    for m in &spec.caches {
        a.push("-v".into());
        a.push(bind(&m.host, &m.container));
    }
    // The card's `@slug` references (MAIN-632), at their own host paths and
    // READ-ONLY. Same path for the same reason the checkout is: the agent is
    // told a path and the path it is told has to be the one on the card.
    for path in &spec.references {
        a.push("-v".into());
        a.push(format!("{}:ro", bind(path, path)));
    }
    for port in &spec.ports {
        a.push("-p".into());
        a.push(format!("{port}:{port}"));
    }
    for entry in &spec.add_hosts {
        a.push("--add-host".into());
        a.push(entry.clone());
    }
    a.push("-e".into());
    a.push(format!(
        "NOOK_SANDBOX_DOCKER={}",
        u8::from(spec.profile.nested_docker)
    ));
    a.push("-e".into());
    a.push(format!("NOOK_SANDBOX_UID={}", spec.agent_uid));
    a.push("-e".into());
    a.push(format!("NOOK_SANDBOX_GID={}", spec.agent_gid));

    a.push("-w".into());
    a.push(spec.worktree.to_string_lossy().to_string());
    a.push(spec.image.clone());
    a
}

/// Rewrite `<name>:host-gateway` entries onto a concrete address.
///
/// Pure so the substitution is assertable without Docker; see the caller in
/// [`Sandbox::start`] for why the network's own gateway is the right address.
pub fn pin_host_alias(add_hosts: &mut [String], gateway: &str) {
    for entry in add_hosts {
        if let Some(name) = entry.strip_suffix(":host-gateway") {
            *entry = format!("{name}:{gateway}");
        }
    }
}

fn bind(host: &Path, container: &Path) -> String {
    format!("{}:{}", host.display(), container.display())
}

/// The host-side chain every job's rules live in, jumped to from Docker's own
/// `DOCKER-USER` (which netfilter consults for FORWARDed traffic, before
/// Docker's per-network rules).
pub const HOST_CHAIN: &str = "NOOK-SANDBOX";

/// The host firewall rules for one job, in the order they must be applied
/// (AC-5). Pure, so the policy is a vector a test can assert rather than a
/// side effect only a live LAN could reveal.
///
/// **This is where the policy is actually ENFORCED**, and the in-container
/// rules below are defence in depth. Two things a `build` job can do defeat an
/// in-container OUTPUT policy, and neither can touch this one:
///
/// 1. A container started by the NESTED daemon is *forwarded*, not locally
///    generated, so its packets traverse PREROUTING → FORWARD → POSTROUTING and
///    never meet the job container's OUTPUT chain at all. From the host they
///    arrive SNAT'ed to the job container's address, which is why matching on
///    the job network's SUBNET catches them.
/// 2. The agent is in the nested daemon's group by design, so
///    `docker run --net=host --cap-add=NET_ADMIN -u 0 … iptables -F OUTPUT`
///    flushes the container's own chain — "host" for the nested daemon being
///    the job container's netns. The host's tables are in another namespace the
///    job has no route into.
///
/// `RETURN` rather than `ACCEPT` for the exception: it hands the packet back to
/// `DOCKER-USER` so Docker's own FORWARD filtering still decides, instead of
/// short-circuiting it.
///
/// **IPv4 only.** These are `iptables` rules, and a job network with an IPv6
/// subnet would have unpoliced v6 egress. Not reachable today — the
/// `docker network create` in `Sandbox::start` takes the daemon's default,
/// which is v4-only unless an operator has turned on IPv6 — which is why this
/// is a note rather than an `ip6tables` twin nothing would exercise.
pub fn host_rules(
    subnet: &str,
    allow: &[String],
    port: &str,
    gateway: Option<&str>,
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for addr in allow {
        // A control plane on the host's loopback is reached through the
        // container's gateway; the caller resolves that address because only
        // the host can see it.
        let target = match (addr.as_str(), gateway) {
            (HOST_GATEWAY, Some(gw)) => gw.to_string(),
            (HOST_GATEWAY, None) => continue,
            (a, _) => a.to_string(),
        };
        // The exception is scoped to the control plane's ADDRESS AND PORT, not
        // its address alone. On a single-box or dev install the control plane
        // shares the docker gateway with every other published service — the
        // database, the object store, the web app — and an address-only RETURN
        // let a job reach all of them: measured, `curl gw:4446` connected
        // straight to the dev Postgres. Only the one port the token is for is
        // opened.
        //
        // Two forms per exception, because a published control plane is DNAT'ed
        // before the filter table runs — by then `-d` is its container address
        // inside `172.16/12`, dropped by the very next rules, so without the
        // conntrack form the agent cannot reach the board that issued its
        // token. `--ctorigdstport` matches the ORIGINAL (pre-DNAT) port; the
        // `-d`/`--dport` form covers a control plane that is not DNAT'ed at all,
        // the ordinary remote case.
        out.push(vec![
            "-s".into(),
            subnet.into(),
            "-p".into(),
            "tcp".into(),
            "-m".into(),
            "conntrack".into(),
            "--ctorigdst".into(),
            target.clone(),
            "--ctorigdstport".into(),
            port.into(),
            "-j".into(),
            "RETURN".into(),
        ]);
        out.push(vec![
            "-s".into(),
            subnet.into(),
            "-d".into(),
            target,
            "-p".into(),
            "tcp".into(),
            "--dport".into(),
            port.into(),
            "-j".into(),
            "RETURN".into(),
        ]);
    }
    for range in PRIVATE_RANGES {
        out.push(vec![
            "-s".into(),
            subnet.into(),
            "-d".into(),
            (*range).into(),
            "-j".into(),
            "REJECT".into(),
            "--reject-with".into(),
            "icmp-admin-prohibited".into(),
        ]);
    }
    out
}

/// The egress policy, as the shell that installs it (AC-5).
///
/// Ordering IS the policy, so it reads top to bottom: loopback first (Docker's
/// embedded DNS resolver lives on 127.0.0.11, and AC-6 fails without it); then
/// anything that is not leaving by the container's external interface, which is
/// how the NESTED daemon's own bridges keep working even though they too are
/// RFC1918; then the control plane BY ADDRESS — the single documented exception,
/// allowed because of what it is and not because private ranges are trusted;
/// then the drops.
///
/// `REJECT` rather than `DROP`: a job that cannot reach the NAS should fail in
/// a millisecond with "permission denied", not hang for two minutes and read as
/// a broken network.
///
/// **Defence in depth, not the enforcement point.** This chain covers only
/// traffic the job container generates itself, and a `build` job can evade it
/// two ways (see [`host_rules`], which is where the policy actually holds). It
/// stays because it costs nothing, it fails a packet earlier, and it is the
/// only policy a profile with no nested daemon can be evaded through at all.
pub fn egress_script(allow: &[String], port: &str) -> String {
    let mut s = String::from("set -e\n");
    s.push_str("EXT=$(ip route show default | awk '{print $5; exit}')\n");
    s.push_str("GW=$(ip route show default | awk '{print $3; exit}')\n");
    s.push_str("[ -n \"$EXT\" ] || { echo 'no default route to police' >&2; exit 1; }\n");
    s.push_str("iptables -F OUTPUT\n");
    s.push_str("iptables -A OUTPUT -o lo -j ACCEPT\n");
    s.push_str("iptables -A OUTPUT ! -o \"$EXT\" -j ACCEPT\n");
    for addr in allow {
        // The dev stack's control plane is on the host's loopback, which the
        // container reaches through its gateway and nowhere else. Resolved in
        // the container because only the container knows its own gateway.
        let target = if addr == HOST_GATEWAY {
            "\"$GW\"".to_string()
        } else {
            addr.clone()
        };
        // Port-scoped for the same reason the host chain is: the gateway hosts
        // every published dev-stack service, and an address-only ACCEPT would
        // reopen the ones the drops below are meant to close.
        s.push_str(&format!(
            "iptables -A OUTPUT -o \"$EXT\" -d {target} -p tcp --dport {port} -j ACCEPT\n"
        ));
    }
    for range in PRIVATE_RANGES {
        s.push_str(&format!(
            "iptables -A OUTPUT -o \"$EXT\" -d {range} -j REJECT --reject-with icmp-admin-prohibited\n"
        ));
    }
    s
}

/// Whoever already holds a host port a job container tried to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHolder {
    pub container: String,
    /// The compose project the container belongs to, when it belongs to one.
    /// That is the name a person brings the squatter down BY, so it is worth
    /// more than the container's own.
    pub project: Option<String>,
}

/// The host port Docker's bind failure names, if a bind failure is what this is.
///
/// Docker spells the collision two ways depending on where it is caught —
/// `Bind for 0.0.0.0:4389 failed: port is already allocated` from the daemon's
/// port allocator, `listen tcp4 0.0.0.0:4389: bind: address already in use`
/// from the proxy — and both are the same event to the person reading it.
pub fn bind_conflict_port(err: &str) -> Option<String> {
    if !err.contains("port is already allocated") && !err.contains("address already in use") {
        return None;
    }
    err.split_whitespace().find_map(|token| {
        let (host, port) = token.trim_matches([',', ';', '.', ':']).rsplit_once(':')?;
        (!host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
            .then(|| port.to_string())
    })
}

/// Every running container with its published ports and its labels.
pub fn published_ports_args() -> Vec<String> {
    vec![
        "ps".into(),
        "--format".into(),
        "{{.Names}}\t{{.Ports}}\t{{.Labels}}".into(),
    ]
}

/// The container publishing `port`, out of [`published_ports_args`]' output.
pub fn holder_of_port(ps: &str, port: &str) -> Option<PortHolder> {
    ps.lines().find_map(|line| {
        let mut columns = line.split('\t');
        let name = columns.next()?.trim();
        let ports = columns.next().unwrap_or("");
        let labels = columns.next().unwrap_or("");
        (!name.is_empty() && publishes(ports, port)).then(|| PortHolder {
            container: name.to_string(),
            project: label_value(labels, crate::compose::COMPOSE_PROJECT_LABEL),
        })
    })
}

/// Does this `docker ps` PORTS column bind `port` on the host?
///
/// The column is `0.0.0.0:4389->4389/tcp, [::]:4389->4389/tcp`, and only the
/// half before the arrow is the host's — a container exposing 4389 internally
/// binds nothing and is not the holder.
fn publishes(ports: &str, port: &str) -> bool {
    ports
        .split(',')
        .filter_map(|mapping| mapping.split("->").next())
        .any(|host| {
            host.trim()
                .rsplit_once(':')
                .is_some_and(|(_, bound)| bound == port)
        })
}

/// Docker's bind failure, rewritten to name who is squatting (MAIN-630 AC-4).
///
/// *"Bind for 0.0.0.0:4389 failed: port is already allocated"* names no cause a
/// person can act on: the port is one the control plane LEASED this run, so the
/// holder is almost always a stack of ours that outlived the run that started
/// it, and finding it meant `docker ps` on the machine by hand. Pure over both
/// texts — the failure and one `docker ps` — so the mapping is a unit test
/// rather than a collision somebody has to reproduce.
///
/// `None` for any error that is not a bind conflict: an unrecognised failure is
/// reported exactly as Docker gave it, never decorated with a guess.
pub fn describe_bind_conflict(err: &str, ps: &str) -> Option<String> {
    let port = bind_conflict_port(err)?;
    Some(match holder_of_port(ps, &port) {
        Some(PortHolder {
            container,
            project: Some(project),
        }) => format!(
            "host port {port} is held by container `{container}` of compose project \
             `{project}`; `docker compose -p {project} down` frees it"
        ),
        Some(PortHolder {
            container,
            project: None,
        }) => format!("host port {port} is held by container `{container}`"),
        // Worth saying, and the more urgent of the two: the leased port is
        // bound by something Docker did not start, so no `compose down` will
        // free it and the node's own range may overlap something else's.
        None => format!(
            "host port {port} is bound by something outside this Docker daemon — \
             no container publishes it"
        ),
    })
}

/// [`describe_bind_conflict`] against the live daemon, as a clause to append.
///
/// Empty for anything that is not a bind conflict, and empty too when the
/// daemon that just refused a `run` will not answer a `ps` — a second failure
/// is not a better report of the first.
fn bind_conflict_hint(err: &str) -> String {
    if bind_conflict_port(err).is_none() {
        return String::new();
    }
    let ps = docker_args(&published_ports_args()).unwrap_or_default();
    describe_bind_conflict(err, &ps)
        .map(|who| format!(" — {who}"))
        .unwrap_or_default()
}

/// A live job container. Dropping it removes the container, so every early
/// return in `loop_job::run` tears the sandbox down without a cleanup branch of
/// its own.
pub struct Sandbox {
    name: String,
    network: String,
    /// The job network's subnet — what every host rule is keyed on, and the
    /// reason a nested container's SNAT'ed traffic is caught too.
    subnet: String,
    /// The image the host-policy helper container runs from; it carries the
    /// `iptables` the node itself is not root enough to use.
    image: String,
    /// The addresses this job's host rules let through, kept so teardown can
    /// delete each rule by the exact spec it was added with.
    allow: Vec<String>,
    /// The one control-plane port those addresses are opened on. The exception
    /// is address+port, never address alone — the gateway hosts every other
    /// published service too (MAIN-611 review: an address-only rule reached the
    /// dev Postgres).
    allow_port: String,
    /// The control-plane URL the agent inside must use. Held here so ONE place
    /// decides it: both drivers launch through this type, and a second copy of
    /// the loopback rewrite is a second chance to get it wrong.
    server: String,
    uid: u32,
    gid: u32,
}

impl Sandbox {
    /// Start the container and install the egress policy, or fail.
    ///
    /// There is no partial success: a container whose firewall did not apply is
    /// removed rather than used, because the alternative is an agent running
    /// with LAN access and nobody able to tell from the outside.
    pub fn start(spec: &SandboxSpec) -> Result<Sandbox, String> {
        // A leftover from a killed run holds the name and the ports.
        let name = container_name(&spec.job_id);
        let net = network_name(&spec.job_id);
        let _ = docker(&["rm", "-f", &name]);
        let _ = docker(&["network", "rm", &net]);
        docker_args(&network_create_args(&spec.job_id))
            .map_err(|e| format!("could not create the job sandbox network: {e}"))?;
        // THE ENFORCEMENT POINT (AC-5), on the host, before the job container
        // exists — so there is no instant at which a job has a route to the LAN
        // and no chain it can reach to remove one. Failing here fails the whole
        // start: a sandbox whose egress policy did not apply is not a sandbox.
        let subnet = network_subnet(&net)?;
        let sb = Sandbox {
            name: name.clone(),
            network: net,
            subnet: subnet.clone(),
            image: spec.image.clone(),
            allow: spec.allow.clone(),
            allow_port: host_and_port(&spec.server)
                .map(|(_, p)| p)
                .unwrap_or_else(|| "8080".into()),
            server: spec.server.clone(),
            uid: spec.agent_uid,
            gid: spec.agent_gid,
        };
        if let Err(e) = sb.apply_host_policy() {
            sb.stop();
            return Err(format!("could not apply the sandbox egress policy: {e}"));
        }
        // `--add-host <name>:host-gateway` names DOCKER's idea of the host, and
        // on Docker Desktop that is a proxy address in 192.168/16 — a range the
        // policy above drops, so the alias resolved to an address the agent was
        // then forbidden to reach and every `nook` call inside failed. This
        // network's gateway serves the same published ports and IS what
        // `HOST_GATEWAY` resolves to in `host_rules`, so pinning the alias to it
        // makes the name and the policy one decision instead of two.
        let mut spec = spec.clone();
        if let Some(gw) = sb.gateway() {
            pin_host_alias(&mut spec.add_hosts, &gw);
        }
        let args = run_args(&spec);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = docker(&argv) {
            // Asked before the teardown, so the observation is of the machine
            // as it was at the failure (AC-4).
            let holder = bind_conflict_hint(&e);
            sb.stop();
            return Err(format!("could not start the job sandbox: {e}{holder}"));
        }
        if let Err(e) = sb.wait_started() {
            sb.stop();
            return Err(e);
        }
        // The in-container half, defence in depth (see `egress_script`). Also
        // before the daemon is waited for, so there is no window in which
        // anything inside has a route out that the host policy has not already
        // closed anyway.
        let script = egress_script(&spec.allow, &sb.allow_port);
        if let Err(e) = docker(&["exec", &name, "sh", "-c", &script]) {
            sb.stop();
            return Err(format!("could not apply the sandbox egress policy: {e}"));
        }
        // Only a profile that asked for a daemon waits for one (AC-12) — a
        // spec run must not pay a cold start to file a ticket.
        if spec.profile.nested_docker {
            if let Err(e) = sb.wait_ready() {
                sb.stop();
                return Err(e);
            }
        }
        Ok(sb)
    }

    /// Install this job's rules in the host's netfilter tables.
    ///
    /// Run through a throwaway `--net=host` container rather than by shelling
    /// out to `iptables` directly, because the NODE is an ordinary user: it has
    /// Docker (it must, to run a job at all) and it does not have root. The job
    /// container cannot do the same thing — its `docker` speaks to the NESTED
    /// daemon, for which `--net=host` means its own namespace, and AC-3 keeps
    /// the host's socket out of it.
    fn apply_host_policy(&self) -> Result<(), String> {
        self.host_iptables(&["-N".into(), HOST_CHAIN.into()]).ok();
        // Jumped unconditionally and only once: the rules inside are
        // source-scoped, so everything that is not a job's traffic falls
        // straight through.
        if self
            .host_iptables(&[
                "-C".into(),
                "DOCKER-USER".into(),
                "-j".into(),
                HOST_CHAIN.into(),
            ])
            .is_err()
        {
            self.host_iptables(&[
                "-I".into(),
                "DOCKER-USER".into(),
                "1".into(),
                "-j".into(),
                HOST_CHAIN.into(),
            ])
            .map_err(|e| format!("could not hook {HOST_CHAIN} into DOCKER-USER: {e}"))?;
        }
        let gateway = self.gateway();
        for rule in host_rules(
            &self.subnet,
            &self.allow,
            &self.allow_port,
            gateway.as_deref(),
        ) {
            let mut args = vec!["-A".to_string(), HOST_CHAIN.to_string()];
            args.extend(rule);
            self.host_iptables(&args)
                .map_err(|e| format!("could not install a host egress rule: {e}"))?;
        }
        Ok(())
    }

    /// Take this job's rules back out.
    ///
    /// By the EXACT spec each was added with — `iptables -D` matches a whole
    /// rule, not a prefix of one, so deleting by source alone would silently
    /// match nothing and leak the policy onto whichever job Docker next hands
    /// this subnet to. That is why the allow list is kept on the struct.
    ///
    /// Best effort otherwise: a rule already gone is the state we wanted.
    fn remove_host_policy(&self) {
        let gateway = self.gateway();
        for rule in host_rules(
            &self.subnet,
            &self.allow,
            &self.allow_port,
            gateway.as_deref(),
        ) {
            let mut args = vec!["-D".to_string(), HOST_CHAIN.to_string()];
            args.extend(rule);
            let _ = self.host_iptables(&args);
        }
    }

    /// The job network's gateway on the host — the address a container reaches
    /// the machine itself on, and the one a loopback control plane needs.
    fn gateway(&self) -> Option<String> {
        docker(&[
            "network",
            "inspect",
            &self.network,
            "--format",
            "{{range .IPAM.Config}}{{.Gateway}} {{end}}",
        ])
        .ok()
        .as_deref()
        .and_then(first_ipv4)
    }

    /// One `iptables` invocation against the HOST's tables.
    fn host_iptables(&self, args: &[String]) -> Result<String, String> {
        host_iptables(&self.image, args)
    }

    /// Wait for the container itself to answer — all a profile with no nested
    /// daemon has to wait for.
    fn wait_started(&self) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut last = String::new();
        while std::time::Instant::now() < deadline {
            match docker(&["exec", &self.name, "true"]) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(format!("the job sandbox never became runnable: {last}"))
    }

    /// Wait for the nested daemon to answer. A build's first act is often
    /// `docker compose up`, and racing the daemon reads as a broken image.
    fn wait_ready(&self) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let mut last = String::from("the sandbox never became ready");
        while std::time::Instant::now() < deadline {
            match docker(&["exec", &self.name, "docker", "info", "--format", "{{.ID}}"]) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        Err(format!("the nested Docker daemon never came up: {last}"))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `docker exec` that runs one program inside, as the agent's uid.
    ///
    /// This is the whole of AC-7's "nothing else does": `exec` inherits NOTHING
    /// from the node process, so the node's own environment — its join token,
    /// its server credential, every other workspace's secrets — is absent
    /// unless it appears in `env` here. `Command::new(runtime)` inherited all
    /// of it by default, which is the bug.
    pub fn exec_command(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(&str, &str)],
    ) -> Command {
        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-i");
        cmd.arg("-u").arg(format!("{}:{}", self.uid, self.gid));
        cmd.arg("-w").arg(cwd);
        for (k, v) in self.base_env().iter().chain(env.iter()) {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        cmd.arg(&self.name);
        cmd.arg(program).args(args);
        cmd
    }

    /// The same launch as a shell line, for the tmux adapter — which types a
    /// command rather than spawning one (AC-1's second wrap point).
    ///
    /// `forward` names variables to carry in from the LAUNCHING SHELL rather
    /// than pairs to write out, and that is deliberate twice over. It keeps
    /// `tmux::spawn` the one place that decides what a session's environment
    /// holds — the invariant that file's own guard test pins — and it keeps a
    /// `GH_TOKEN` out of a command line that `tmux capture-pane` would happily
    /// hand back. `docker exec -e NAME` with no `=` reads the client's value,
    /// and silently skips a name the shell does not have.
    pub fn exec_shell_line(&self, program: &str, cwd: &Path, forward: &[&str]) -> String {
        let mut parts: Vec<String> = vec![
            "docker".into(),
            "exec".into(),
            "-it".into(),
            "-u".into(),
            format!("{}:{}", self.uid, self.gid),
            "-w".into(),
            shell_quote(&cwd.to_string_lossy()),
        ];
        for (k, v) in self.base_env() {
            parts.push("-e".into());
            parts.push(shell_quote(&format!("{k}={v}")));
        }
        for name in forward {
            parts.push("-e".into());
            parts.push(shell_quote(name));
        }
        parts.push(self.name.clone());
        parts.push(program.into());
        parts.join(" ")
    }

    /// What every launch inside gets, sandbox or not.
    fn base_env(&self) -> Vec<(&str, &str)> {
        let mut env = vec![
            ("HOME", AGENT_HOME),
            // The node's own session, mounted (AC-7). Same variable the
            // containerised nodes already set, so there is one convention.
            ("CLAUDE_CONFIG_DIR", CLAUDE_DIR),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("NOOK_SANDBOX", "1"),
        ];
        // Written by VALUE rather than forwarded by name, because the name is
        // the one thing the launching shell cannot be trusted to hold: the
        // node reads its server from `node.toml`, so `docker exec -e
        // NOOK_SERVER` would carry nothing at all — and where the variable IS
        // set it holds the host's spelling, which does not resolve in here.
        if !self.server.is_empty() {
            env.push(("NOOK_SERVER", &self.server));
        }
        env
    }

    /// Remove the container, and with it every process the job started.
    ///
    /// Stronger than the `PR_SET_PDEATHSIG` the direct-spawn path relies on
    /// (MAIN-506): killing a `docker exec` client leaves the process inside
    /// running, so removing the container is what actually ends a run — nested
    /// daemon, compose stack and all.
    pub fn stop(&self) {
        let _ = docker(&["rm", "-f", &self.name]);
        // The host rules go before the network does: they name its subnet, and
        // Docker reuses subnets, so a leftover rule would police somebody
        // else's job.
        self.remove_host_policy();
        // After the container, never before: a network with an endpoint on it
        // refuses to go, and the leftover then collides with the next run's.
        let _ = docker(&["network", "rm", &self.network]);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One object a `docker … ls` reported, with the job it belongs to.
///
/// `job_id` is `None` for anything unlabelled — the owner's own dev stack, a
/// sibling checkout's compose project, a container somebody started by hand —
/// and that is precisely what [`orphans`] refuses to select (AC-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub job_id: Option<String>,
}

/// Parse `{{.Names}}\t{{.Labels}}` (or `{{.Name}}\t{{.Labels}}`) output.
///
/// Docker renders `.Labels` as `k=v,k=v`, so a value containing a comma would
/// be misread — a job id is a UUID and cannot, and nothing else writes this
/// label. A line with no tab at all is an object with no labels, which is the
/// case that must survive rather than the case that must parse.
pub fn parse_listing(out: &str) -> Vec<Listed> {
    out.lines()
        .filter_map(|line| {
            let (name, labels) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            (!name.is_empty()).then(|| Listed {
                name: name.to_string(),
                job_id: job_label(labels),
            })
        })
        .collect()
}

fn job_label(labels: &str) -> Option<String> {
    label_value(labels, JOB_LABEL)
}

/// One label's value out of docker's `k=v,k=v` rendering.
///
/// Shared with `compose.rs`, which reads compose's own labels off the same
/// column (MAIN-630): the comma caveat above is the same caveat there, and two
/// copies of this parser would be two places to learn it.
pub fn label_value(labels: &str, key: &str) -> Option<String> {
    labels
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Every listed object carrying the job label whose job this node is not
/// running (AC-1).
///
/// Two conditions, and the first is the whole of AC-5: an object with no
/// `nook.job` label is never selected, whatever it is called or how much it
/// looks like ours. The second is the node's own running set, which after a
/// restart is EMPTY — so every leftover is an orphan, which is exactly the
/// crash case this exists for (AC-6).
pub fn orphans<'a>(listed: &'a [Listed], running: &HashSet<String>) -> Vec<&'a Listed> {
    listed
        .iter()
        .filter(|o| o.job_id.as_ref().is_some_and(|id| !running.contains(id)))
        .collect()
}

/// List every container carrying the job label, running or exited.
///
/// `-a` because a job container is `--rm` and may already have exited without
/// the daemon having reaped it; an exited container still holds its anonymous
/// volume and still owns an endpoint on the network, which is what makes the
/// network removal below fail if it is skipped.
pub fn list_containers_args() -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("label={JOB_LABEL}"),
        "--format".into(),
        "{{.Names}}\t{{.Labels}}".into(),
    ]
}

/// List EVERY network, labelled or not.
///
/// Unfiltered, deliberately: AC-3 needs the SUBNETS of the networks live jobs
/// hold, and a job started by an agent from before this label existed has an
/// unlabelled one whose rules must still be spared. Reading a subnet removes
/// nothing — [`orphans`] is the only thing that decides what goes, and it
/// requires the label.
pub fn list_networks_args() -> Vec<String> {
    vec![
        "network".into(),
        "ls".into(),
        "--format".into(),
        "{{.Name}}\t{{.Labels}}".into(),
    ]
}

/// Remove an orphaned job container AND the anonymous volume it carries (AC-1).
///
/// `-v` is the entire volume claim. A `build` job's nested daemon stores a whole
/// image cache on the anonymous `/var/lib/docker` volume `run_args` declares,
/// and without this flag that volume outlives the container that named it with
/// nothing left on the machine able to identify it. Docker's own answer to that
/// state is `docker volume prune`, which NG-2 forbids for a good reason: it
/// takes every other unused volume on the machine with it.
pub fn remove_container_args(name: &str) -> Vec<String> {
    vec!["rm".into(), "-f".into(), "-v".into(), name.into()]
}

pub fn remove_network_args(name: &str) -> Vec<String> {
    vec!["network".into(), "rm".into(), name.into()]
}

/// The rules in [`HOST_CHAIN`] that police a subnet no live job holds (AC-3).
///
/// Read back from `iptables -S NOOK-SANDBOX`, whose output is the exact spec
/// each rule was added with — which is what makes a `-D` of it match, since
/// `iptables` deletes a whole rule and never a prefix of one. Every rule in this
/// chain is one of ours and every one names its job's subnet with `-s`
/// (see [`host_rules`]), so "no live job has this subnet" is the whole test.
///
/// Keyed on the subnet rather than the job id because netfilter records
/// neither: a rule carries no label, and the only thing it says about itself is
/// what it policed. That is also the hazard [`Sandbox::stop`] names — Docker
/// reuses subnets, so a rule left behind polices whichever job is handed the
/// range next.
pub fn orphan_rules(saved: &str, live_subnets: &HashSet<String>) -> Vec<Vec<String>> {
    saved
        .lines()
        .filter_map(|line| {
            let spec: Vec<String> = line.split_whitespace().map(str::to_string).collect();
            if spec.len() < 3 || spec[0] != "-A" || spec[1] != HOST_CHAIN {
                return None;
            }
            let subnet = rule_source(&spec)?;
            (!live_subnets.contains(subnet)).then(|| spec[2..].to_vec())
        })
        .collect()
}

/// The `-s` argument of a saved rule, and the subnet it polices.
///
/// A rule without one polices the whole machine and is not a rule this module
/// ever wrote, so it is left alone rather than guessed at.
fn rule_source(spec: &[String]) -> Option<&str> {
    spec.iter()
        .position(|t| t == "-s")
        .and_then(|i| spec.get(i + 1))
        .map(String::as_str)
}

/// Reclaim every job container, network, anonymous volume and firewall rule left
/// behind by a node that died before [`Sandbox::stop`] could run (MAIN-617).
///
/// `stop` is reached only through `Drop`, so it covers exactly one case:
/// `loop_job::run` returning. A node that is KILLED — crash, OOM, `systemctl
/// restart nook-node`, a dockerd hiccup mid-run — strands the container, its
/// user-defined network, its host firewall rules and, for a `build`, an
/// anonymous volume holding an entire nested image cache. Nothing else in this
/// tree would ever remove any of it, and the operator's remedy was
/// `docker system prune -a --volumes` by hand — far too broad on a machine that
/// also runs their own work.
///
/// `running` is this node process's own set of running job ids, and it is
/// authoritative (NG-4): no control-plane round trip, the same argument
/// `build_worktrees_held` makes about what a node knows. After a restart it is
/// empty, which is what makes the crash case work (AC-6).
///
/// Best effort throughout (AC-9): every failure is logged and the next object is
/// still attempted, exactly as `loop_job::reconcile` does.
pub fn sweep_orphans(running: &HashSet<String>) {
    if let Some(detail) = containerised() {
        tracing::debug!(%detail, "no job sandboxes to sweep: this node starts none");
        return;
    }
    let Some(_pass) = SweepGuard::claim() else {
        return;
    };

    let containers = match docker_args(&list_containers_args()).map(|o| parse_listing(&o)) {
        Ok(listed) => listed,
        Err(e) => {
            tracing::warn!(error = %e, "could not list job containers to sweep");
            Vec::new()
        }
    };
    for orphan in orphans(&containers, running) {
        // The container FIRST, and its network only after: a network with a live
        // endpoint on it refuses to be removed, and the leftover then collides
        // with the next run's. `Sandbox::stop`'s order, for `stop`'s reason.
        report("container", &orphan.name, orphan.job_id.as_deref(), || {
            docker_args(&remove_container_args(&orphan.name))
        });
    }

    let networks = match docker_args(&list_networks_args()).map(|o| parse_listing(&o)) {
        Ok(listed) => listed,
        Err(e) => {
            tracing::warn!(error = %e, "could not list job networks to sweep");
            return;
        }
    };
    // Rules before the network they name, and after the container: `stop`'s
    // ordering again, and the reused-subnet hazard it exists for.
    sweep_host_rules(&networks, running);
    for orphan in orphans(&networks, running) {
        report("network", &orphan.name, orphan.job_id.as_deref(), || {
            docker_args(&remove_network_args(&orphan.name))
        });
    }
}

/// AC-8: every swept object is named, by kind, name and job id, so a reclaim is
/// auditable after the fact. AC-9: a failure is one line and the sweep goes on.
fn report(
    kind: &str,
    name: &str,
    job: Option<&str>,
    remove: impl FnOnce() -> Result<String, String>,
) {
    let job = job.unwrap_or("unknown");
    match remove() {
        Ok(_) => tracing::info!(kind, name, job, "swept an orphaned job sandbox object"),
        Err(e) => {
            tracing::warn!(kind, name, job, error = %e, "could not sweep an orphaned job sandbox object")
        }
    }
}

/// Delete the [`HOST_CHAIN`] rules of jobs that are no longer running (AC-3).
///
/// The live subnets come from the networks of the RUNNING jobs, by name and
/// whether or not those networks carry the label, because a rule of a live job
/// must be spared on any reading of the evidence. A running job whose network
/// does not exist yet has no rules yet either — `Sandbox::start` creates the
/// network before it installs a rule — so its absence is not a gap. Anything
/// else unreadable leaves the live set INCOMPLETE, and the pass is then
/// abandoned rather than run on a guess: the cost of being wrong here is a live
/// job losing its egress policy, which is the whole of MAIN-611.
fn sweep_host_rules(networks: &[Listed], running: &HashSet<String>) {
    let present: HashSet<&str> = networks.iter().map(|n| n.name.as_str()).collect();
    let mut live: HashSet<String> = HashSet::new();
    for job in running {
        let net = network_name(job);
        if !present.contains(net.as_str()) {
            continue;
        }
        match network_subnet(&net) {
            Ok(subnet) => {
                live.insert(subnet);
            }
            Err(e) => {
                tracing::warn!(
                    network = %net, error = %e,
                    "not sweeping any host firewall rule this pass: a live job's \
                     subnet is unreadable, so no rule can be shown to be an orphan"
                );
                return;
            }
        }
    }
    // Read before the networks are removed, so a swept rule can still say which
    // job it belonged to (AC-8). A rule whose network is already gone cannot:
    // netfilter records no label, and nothing else on the machine remembers.
    let owner: HashMap<String, String> = orphans(networks, running)
        .into_iter()
        .filter_map(|n| {
            let job = n.job_id.clone()?;
            network_subnet(&n.name).ok().map(|subnet| (subnet, job))
        })
        .collect();

    let image = image();
    let saved = match host_iptables(&image, &["-S".into(), HOST_CHAIN.into()]) {
        Ok(saved) => saved,
        Err(e) => {
            // No chain is the ordinary state of a node that has never run a job.
            tracing::debug!(error = %e, "no {HOST_CHAIN} chain to sweep");
            return;
        }
    };
    for rule in orphan_rules(&saved, &live) {
        let subnet = rule_source(&rule).unwrap_or_default().to_string();
        let mut args = vec!["-D".to_string(), HOST_CHAIN.to_string()];
        args.extend(rule);
        report(
            "firewall rule",
            &args[2..].join(" "),
            owner.get(&subnet).map(String::as_str),
            || host_iptables(&image, &args),
        );
    }
}

/// One sweep at a time.
///
/// Connect and the ten-minute inventory can fire together — the timer's first
/// tick is immediate — and two passes racing would each try to remove what the
/// other already had, turning an ordinary reclaim into a log full of "No such
/// container". A reconnect storm is the same shape.
struct SweepGuard;

static SWEEPING: AtomicBool = AtomicBool::new(false);

impl SweepGuard {
    fn claim() -> Option<SweepGuard> {
        if SWEEPING.swap(true, Ordering::SeqCst) {
            tracing::debug!("a job sandbox sweep is already running");
            return None;
        }
        Some(SweepGuard)
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        SWEEPING.store(false, Ordering::SeqCst);
    }
}

/// Single-quote for `sh`, the only form that needs no knowledge of what is
/// inside. A tmux launch line carries a `GH_TOKEN`, so this is not cosmetic.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `docker` argv that runs one `iptables` command against the HOST's
/// tables.
///
/// **`--entrypoint` is load-bearing, not tidiness.** The sandbox image's
/// entrypoint ends in `exec tail -f /dev/null`, so without this the iptables
/// arguments are passed *to that script*, which ignores them and blocks
/// forever — and because the first such call happens inside `Sandbox::start`,
/// every job on the node then hangs at launch. Measured on 2026-08-15: one
/// `docker run … iptables -N` sat for ten minutes and queued every later
/// container start on the machine behind it.
///
/// `--net=host` puts the helper in the HOST's network namespace, which is the
/// whole point: that is where the rules must live for a job to be unable to
/// reach them. `NET_ADMIN` alone is enough to write them; the node itself is an
/// ordinary user and could not.
pub fn host_iptables_args(image: &str, args: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--net=host".into(),
        "--cap-add".into(),
        "NET_ADMIN".into(),
        "--entrypoint".into(),
        "iptables".into(),
        image.into(),
    ];
    argv.extend(args.iter().cloned());
    argv
}

/// The subnet Docker assigned a job's network.
fn network_subnet(net: &str) -> Result<String, String> {
    let out = docker(&[
        "network",
        "inspect",
        net,
        "--format",
        "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
    ])
    .map_err(|e| format!("could not read the job network's subnet: {e}"))?;
    first_ipv4(&out).ok_or_else(|| {
        // Fail closed: with no subnet there is nothing to key the host policy
        // on, and a job with no policy must not start.
        format!("the job network reported no IPv4 subnet to police (got {out:?})")
    })
}

/// The first IPv4 entry in a space-separated list of IPAM values.
///
/// A network can carry more than one IPAM config — a dual-stack daemon gives
/// new networks a v4 and a v6 one — and the template that emitted them used to
/// have no separator, so the two arrived CONCATENATED
/// (`172.30.0.0/16fd00:…/64`). Every `iptables` call then rejected the string,
/// `Sandbox::start` failed, and the node refused every job with a parse error
/// that said nothing about IPv6.
///
/// v4 specifically, because the policy is `iptables` and not `ip6tables`: see
/// the note in [`host_rules`] about what that leaves unpoliced.
fn first_ipv4(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .find(|v| v.contains('.') && !v.contains(':'))
        .map(str::to_string)
}

/// One `iptables` invocation against the HOST's tables, for a caller that has
/// no [`Sandbox`] — the sweep runs on a node that has just started and holds
/// none.
fn host_iptables(image: &str, args: &[String]) -> Result<String, String> {
    docker_args(&host_iptables_args(image, args))
}

/// [`docker`] for an argv built by one of the pure functions above.
fn docker_args(args: &[String]) -> Result<String, String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker(&refs)
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            err
        };
        return Err(err);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Is this node itself a container (NG-5)?
///
/// A containerised node mounts no Docker socket and sets no `DOCKER_HOST`
/// (docker-compose.yml), so it cannot run a build at all and there is nothing
/// here to confine. It keeps claiming the spec/review/decompose work it already
/// does rather than going dark the day this ships.
pub fn containerised() -> Option<String> {
    if Path::new("/.dockerenv").exists() {
        return Some("/.dockerenv is present".into());
    }
    if Path::new("/run/.containerenv").exists() {
        return Some("/run/.containerenv is present".into());
    }
    let cgroup = std::fs::read_to_string("/proc/1/cgroup").unwrap_or_default();
    if cgroup.contains("docker") || cgroup.contains("containerd") || cgroup.contains("kubepods") {
        return Some("pid 1 is in a container cgroup".into());
    }
    None
}

/// The image a job container is started from when nothing names one: the
/// PUBLISHED image at this agent's OWN version (MAIN-643 AC-2).
///
/// Derived from `CARGO_PKG_VERSION` rather than being a floating tag, because
/// the two used to drift silently and the drift is invisible: a `latest` built
/// by hand on some earlier afternoon carried an older `nook` than the agent
/// running it, and `nook builds outcome` — a run's last act — is what that
/// older binary executes. Version-matched by construction, an agent and the box
/// it runs jobs in cannot disagree unless an operator says so.
pub fn default_image() -> String {
    format!("{IMAGE_REPO}:{}", env!("CARGO_PKG_VERSION"))
}

/// The image an operator NAMED with `NOOK_SANDBOX_IMAGE`, if any.
///
/// Kept apart from [`image`] because "which image" and "who chose it" are
/// different questions, and the second one decides whether this node may pull
/// (AC-5).
pub fn configured_image() -> Option<String> {
    match std::env::var("NOOK_SANDBOX_IMAGE") {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// The image this node starts job containers from.
pub fn image() -> String {
    configured_image().unwrap_or_else(default_image)
}

/// The isolation mode this node is configured for.
pub fn isolation() -> Isolation {
    Isolation::parse(&std::env::var("NOOK_SANDBOX_ISOLATION").unwrap_or_default())
}

/// How far this process has got with the one automatic pull (AC-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullState {
    /// Nothing pulled yet — the next probe that finds the image missing starts
    /// one.
    Untried,
    /// A pull is in flight on a thread of its own.
    Running,
    /// A pull ran and reported success. Ordinarily the image is present at the
    /// next probe, which re-arms this to [`PullState::Untried`] — so an image
    /// later removed under a long-lived node is fetched again.
    ///
    /// It is a distinct state rather than that re-arm happening here because
    /// of the case where it is NOT present: a pull that says it worked and
    /// leaves no image would otherwise be pulled again every heartbeat,
    /// forever. Re-arming only on a probe that SAW the image is what bounds it.
    Succeeded,
    /// A pull ran and failed. Not retried in this process: a node whose pull
    /// was refused reports why and waits for an operator or a restart, rather
    /// than hammering a registry every heartbeat.
    Failed {
        reason: SandboxUnavailable,
        detail: String,
    },
}

static PULL: Mutex<PullState> = Mutex::new(PullState::Untried);

/// Everything [`probe`] observed, so that the decision made from it is a pure
/// function — the shape [`run_args`] and [`egress_script`] already use, and for
/// the same reason: a decision table you can only exercise by having Docker,
/// a registry and a missing image is one nobody re-checks.
pub struct Observed<'a> {
    /// How this node concluded it is itself a container, if it did.
    pub containerised: Option<String>,
    /// The daemon's complaint, when it did not answer at all.
    pub docker_error: Option<String>,
    pub image: &'a str,
    pub image_present: bool,
    /// `NOOK_SANDBOX_IMAGE` named this image, so the operator owns it (AC-5).
    pub image_configured: bool,
    pub pull: &'a PullState,
    pub isolation: Isolation,
}

/// What to report, and whether to start the one automatic pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub capability: SandboxCapability,
    pub start_pull: bool,
}

/// The decision table behind [`probe`] (AC-3, AC-4, AC-5, AC-6).
pub fn decide(obs: &Observed<'_>) -> Decision {
    let settled = |capability| Decision {
        capability,
        start_pull: false,
    };
    if let Some(detail) = obs.containerised.clone() {
        return settled(SandboxCapability::Exempt { detail });
    }
    if let Some(e) = &obs.docker_error {
        return settled(SandboxCapability::Unavailable {
            detail: format!("no Docker daemon on this node ({e})"),
            reason: SandboxUnavailable::NoDocker,
        });
    }
    if obs.image_present {
        return settled(SandboxCapability::Ready {
            image: format!("{} ({})", obs.image, obs.isolation.as_str()),
        });
    }
    // An operator who named an image owns it (AC-5). Pulling it would be this
    // node guessing that a private tag it cannot see is meant to come from a
    // registry, and an air-gapped install (NG-5) is exactly the case where the
    // guess is wrong and the error message about it is the whole answer.
    if obs.image_configured {
        return settled(SandboxCapability::Unavailable {
            detail: format!(
                "NOOK_SANDBOX_IMAGE names {}, which is not on this node — an image you \
                 name is never pulled automatically, so build or pull it yourself, or \
                 unset the variable to use the published {}",
                obs.image,
                default_image()
            ),
            reason: SandboxUnavailable::NotPresent,
        });
    }
    match obs.pull {
        PullState::Untried => Decision {
            capability: SandboxCapability::Pulling {
                image: obs.image.to_string(),
            },
            start_pull: true,
        },
        PullState::Running => settled(SandboxCapability::Pulling {
            image: obs.image.to_string(),
        }),
        // The pull said it worked and the image is still not here. Reported
        // rather than pulled again: a loop with no ceiling is worse than a
        // node saying plainly that its Docker is not behaving.
        PullState::Succeeded => settled(SandboxCapability::Unavailable {
            detail: format!(
                "pulling {} reported success but the image is still not on this node — \
                 check this machine's Docker",
                obs.image
            ),
            reason: SandboxUnavailable::Unknown,
        }),
        PullState::Failed { reason, detail } => settled(SandboxCapability::Unavailable {
            detail: detail.clone(),
            reason: *reason,
        }),
    }
}

/// Which of AC-6's reasons a failed `docker pull` was.
///
/// The registry's own wording is the only signal there is — Docker reports a
/// pull failure as one stderr line and no code — so this reads it, most
/// specific first: a "manifest unknown" is unambiguous, while "denied" alone
/// covers both a private image and one that does not exist and is therefore
/// the weaker match.
pub fn classify_pull_failure(image: &str, err: &str) -> (SandboxUnavailable, String) {
    let low = err.to_ascii_lowercase();
    if low.contains("manifest unknown") || low.contains("not found") {
        (
            SandboxUnavailable::NotPublished,
            format!(
                "{image} is not published — the release carrying this agent version did \
                 not publish a job sandbox image ({err})"
            ),
        )
    } else if low.contains("unauthorized")
        || low.contains("authentication required")
        || low.contains("denied")
    {
        (
            SandboxUnavailable::NoCredentials,
            format!(
                "the registry refused {image} for want of a credential — `docker login \
                 ghcr.io` on this node ({err})"
            ),
        )
    } else {
        (
            SandboxUnavailable::PullRefused,
            format!("pulling {image} failed ({err})"),
        )
    }
}

/// Start the one automatic pull, on a thread of its own.
///
/// A pull of this image is minutes long and [`probe`] runs on every heartbeat,
/// so doing it inline would stall the node's whole report — which is why AC-4
/// wants a state to report meanwhile rather than a blocking call.
fn start_pull(image: String) {
    let mut state = PULL.lock().unwrap_or_else(|e| e.into_inner());
    if !matches!(*state, PullState::Untried) {
        return;
    }
    *state = PullState::Running;
    drop(state);
    std::thread::spawn(move || {
        tracing::info!(%image, "pulling the job sandbox image");
        let outcome = match docker(&["pull", &image]) {
            Ok(_) => {
                tracing::info!(%image, "pulled the job sandbox image; this node can take build work");
                PullState::Succeeded
            }
            Err(e) => {
                let (reason, detail) = classify_pull_failure(&image, &e);
                tracing::warn!(%image, %detail, "could not pull the job sandbox image");
                PullState::Failed { reason, detail }
            }
        };
        *PULL.lock().unwrap_or_else(|e| e.into_inner()) = outcome;
    });
}

/// What this node reports on heartbeat (AC-9), and what the dispatcher's
/// fail-closed gate reads (AC-8).
///
/// Probed rather than assumed, and probed the way every other capability here
/// is: ask the tool, do not read a config file and hope.
pub fn probe() -> SandboxCapability {
    // Asked before anything else, and answered without spawning Docker: a
    // containerised node has none to ask.
    if let Some(detail) = containerised() {
        return SandboxCapability::Exempt { detail };
    }
    let image = image();
    let docker_error = docker(&["version", "--format", "{{.Server.Version}}"]).err();
    let image_present = docker_error.is_none() && docker(&["image", "inspect", &image]).is_ok();
    let pull = {
        let mut state = PULL.lock().unwrap_or_else(|e| e.into_inner());
        // Re-armed here rather than in the pulling thread, so that a pull which
        // claimed success without producing an image cannot re-arm itself into
        // an unbounded retry. Seeing the image is the evidence; the pull's own
        // exit code is not.
        if image_present {
            *state = PullState::Untried;
        }
        state.clone()
    };
    let decision = decide(&Observed {
        containerised: None,
        docker_error,
        image: &image,
        image_present,
        image_configured: configured_image().is_some(),
        pull: &pull,
        isolation: isolation(),
    });
    if decision.start_pull {
        start_pull(image);
    }
    decision.capability
}

/// A control-plane URL's host and port. One parser, because the allow list and
/// the `--add-host` alias must name the SAME host — two readings of one URL is
/// how a policy comes to permit an address the agent never uses.
pub fn host_and_port(server: &str) -> Option<(String, String)> {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, p),
        _ => (
            hostport,
            if server.starts_with("https") {
                "443"
            } else {
                "80"
            },
        ),
    };
    (!host.is_empty()).then(|| (host.to_string(), port.to_string()))
}

/// The control-plane URL as the AGENT must spell it.
///
/// A loopback URL is the HOST's loopback; inside the container the same string
/// means the container's own, where nothing listens — so every `nook` call in a
/// sandboxed agent died with `Connection refused` and the run ended at its
/// preflight. Rewriting the host onto [`HOST_ALIAS`] is what makes the token
/// the agent was issued usable against the board that issued it.
///
/// Any other host is returned unchanged: a real control plane resolves the same
/// inside the container as out.
pub fn server_for_container(server: &str) -> String {
    let Some((host, _)) = host_and_port(server) else {
        return server.to_string();
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return server.to_string();
    }
    // Replaced in the AUTHORITY only. A path or a query may legitimately repeat
    // the host name, and rewriting those would silently edit an unrelated value.
    let (scheme, rest) = match server.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), server),
    };
    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let port = authority
        .rsplit_once(':')
        .and_then(|(_, p)| (!p.is_empty() && p.chars().all(|c| c.is_ascii_digit())).then_some(p));
    match port {
        Some(p) => format!("{scheme}{HOST_ALIAS}:{p}{tail}"),
        None => format!("{scheme}{HOST_ALIAS}{tail}"),
    }
}

/// What one resolved address contributes to the allow list, or `None` when it
/// cannot be expressed as a rule.
///
/// IPv4 ONLY, because `host_rules` is IPv4. `iptables` handed an IPv6 literal
/// does not warn and skip it — it fails the whole install with "getaddrinfo:
/// Address family for hostname not supported", and `Sandbox::start` treats that
/// as fatal, so the node claims work and then fails every job (MAIN-648).
/// Deciding it here keeps the knowledge of what a rule can express in one place.
fn allow_entry(ip: IpAddr) -> Option<String> {
    let v4 = match ip {
        IpAddr::V4(v4) => v4,
        // `::ffff:a.b.c.d` is an IPv4 address wearing a v6 spelling, and it is
        // what a dual-stack resolver commonly returns for an IPv4-only host.
        // Unwrap it rather than drop it.
        IpAddr::V6(v6) => v6.to_ipv4_mapped()?,
    };
    // A loopback control plane is not reachable from inside at all; the
    // container reaches the host through its gateway, and the run args add
    // `host.docker.internal` for the name.
    Some(if v4.is_loopback() {
        HOST_GATEWAY.to_string()
    } else {
        v4.to_string()
    })
}

/// A resolver answer the IPv4 rules can express nothing from: every address in
/// it was a real IPv6 one.
///
/// Named rather than left as an empty `Vec`, because the two are the same value
/// and not the same fact: "this name resolves to nothing" and "this name
/// resolves only to addresses `iptables` cannot take" send an operator to
/// different places (MAIN-648, AC-4).
#[derive(Debug, PartialEq, Eq)]
struct Ipv6OnlyAnswer {
    skipped: usize,
}

/// The allow list one resolver answer yields.
///
/// Split from [`control_plane_allow`] because that one RESOLVES and this one
/// only decides: the decision is over addresses, so it is testable without a
/// network and without depending on what DNS hands this machine today.
fn allow_from_addrs(
    addrs: impl IntoIterator<Item = IpAddr>,
) -> Result<Vec<String>, Ipv6OnlyAnswer> {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for ip in addrs {
        match allow_entry(ip) {
            Some(entry) => out.push(entry),
            None => skipped += 1,
        }
    }
    out.sort();
    out.dedup();
    if out.is_empty() && skipped > 0 {
        return Err(Ipv6OnlyAnswer { skipped });
    }
    Ok(out)
}

/// The addresses the egress policy lets through: the control plane, resolved
/// now, on this node.
///
/// BY ADDRESS (AC-5), which is the point — "it is private" is never the reason
/// a packet is allowed.
pub fn control_plane_allow(server: &str) -> Vec<String> {
    let Some((host, port)) = host_and_port(server) else {
        return Vec::new();
    };
    let Ok(addrs) = format!("{host}:{port}").to_socket_addrs() else {
        return Vec::new();
    };
    match allow_from_addrs(addrs.map(|a| a.ip())) {
        Ok(out) => out,
        // An empty allow list on its own reads as "no control plane to permit"
        // and yields a policy that silently blocks the job's own control plane.
        // Say which of the two it was.
        Err(Ipv6OnlyAnswer { skipped }) => {
            tracing::warn!(
                host = %host,
                skipped_v6 = skipped,
                "the control plane resolves only to IPv6, and the host egress rules are IPv4 — \
                 the job container cannot reach it"
            );
            Vec::new()
        }
    }
}

/// The escape suite (AC-11).
///
/// Every assertion here is a real attack run from inside a REAL job container,
/// each asserted to FAIL. A sandbox nobody attacked is a sandbox nobody has
/// evidence for, and a unit-test fake would be evidence about the fake.
///
/// It needs Docker and the job image, so it is opt-in — `NOOK_SANDBOX_E2E=1`,
/// with `NOOK_SANDBOX_IMAGE` naming the image to attack. Without the flag it
/// returns early rather than passing vacuously, the same shape `TestBed` uses
/// for the database. Build the image first:
///
/// ```text
/// docker build -f deploy/docker/job-sandbox.Dockerfile -t nook-job-sandbox:latest .
/// NOOK_SANDBOX_E2E=1 cargo test --bin nook escapes -- --test-threads=1 --nocapture
/// ```
#[cfg(test)]
mod escapes {
    use super::*;
    use std::process::Command;

    fn enabled() -> bool {
        std::env::var("NOOK_SANDBOX_E2E").as_deref() == Ok("1")
    }

    /// Run a shell line INSIDE the job container, as the agent.
    fn inside(sb: &Sandbox, line: &str) -> (bool, String) {
        let out = Command::new("docker")
            .args(["exec", "-u", "1000:1000", sb.name(), "sh", "-c", line])
            .output()
            .expect("docker exec");
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    }

    /// A live sandbox over a scratch checkout, with a decoy sibling and a decoy
    /// node identity beside it on the host — the two things AC-11 says must be
    /// unreachable.
    struct Bed {
        sandbox: Sandbox,
        root: PathBuf,
    }

    impl Bed {
        /// `tag` names the TEST, and it is not decoration: the job id and the
        /// temp root are both derived from it, and `Sandbox::start` opens with
        /// `docker rm -f <name>` while `Bed::drop` removes the root. Keyed on
        /// the process id alone — as this was — two tests running concurrently
        /// tore down each other's container and checkout, and the failure read
        /// as a flaky sandbox rather than a harness collision.
        fn new(tag: &str, profile: Profile) -> Bed {
            Bed::with_references(tag, profile, false)
        }

        /// `referenced` adds a THIRD checkout beside the two above and passes it
        /// as a card `@slug` reference (MAIN-632). The sibling is left exactly
        /// where it was: the pair is the whole point of AC-6 — what the card
        /// named is readable, what it did not name is not.
        fn with_references(tag: &str, profile: Profile, referenced: bool) -> Bed {
            let root =
                std::env::temp_dir().join(format!("nook-escape-{}-{tag}", std::process::id()));
            let worktree = root.join("worktrees/build-mine");
            std::fs::create_dir_all(&worktree).expect("worktree");
            std::fs::create_dir_all(root.join("worktrees/build-someone-else")).expect("sibling");
            std::fs::write(
                root.join("worktrees/build-someone-else/SECRET"),
                "another card's checkout",
            )
            .expect("sibling file");
            std::fs::write(root.join("node.toml"), "join_token = \"NODE-IDENTITY\"")
                .expect("identity");
            std::fs::write(worktree.join("README"), "the job's own tree").expect("readme");
            let referenced_dir = root.join("checkouts/nook-web");
            let references = if referenced {
                std::fs::create_dir_all(&referenced_dir).expect("referenced checkout");
                std::fs::write(referenced_dir.join("CONTRACT"), "the other side's shape")
                    .expect("referenced file");
                vec![referenced_dir.clone()]
            } else {
                Vec::new()
            };
            let spec = SandboxSpec {
                job_id: format!("escape-{}-{tag}", std::process::id()),
                image: image(),
                profile,
                isolation: isolation(),
                worktree: worktree.clone(),
                gitdir: None,
                claude_dir: None,
                caches: Vec::new(),
                references,
                ports: Vec::new(),
                allow: Vec::new(),
                add_hosts: Vec::new(),
                server: String::new(),
                agent_uid: 1000,
                agent_gid: 1000,
            };
            let sandbox = Sandbox::start(&spec).expect("the job sandbox must start");
            Bed { sandbox, root }
        }

        fn worktree(&self) -> PathBuf {
            self.root.join("worktrees/build-mine")
        }

        fn referenced(&self) -> PathBuf {
            self.root.join("checkouts/nook-web")
        }

        fn sibling(&self) -> PathBuf {
            self.root.join("worktrees/build-someone-else")
        }
    }

    impl Drop for Bed {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// AC-11, the filesystem half: `$HOME`, a sibling checkout, and the node's
    /// own identity are ABSENT — not unreadable, absent.
    #[test]
    fn the_host_filesystem_is_not_there_to_read() {
        if !enabled() {
            return;
        }
        let bed = Bed::new("fs", profile_for("spec"));
        let sb = &bed.sandbox;

        let home = std::env::var("HOME").expect("HOME");
        let (ok, out) = inside(sb, &format!("ls -a {home}"));
        assert!(
            !ok || !out.contains(".ssh"),
            "the host's home directory is readable from inside: {out}"
        );

        let (_, out) = inside(sb, "cat ~/.config/nook/node.toml 2>&1");
        assert!(
            !out.contains("NODE-IDENTITY"),
            "the node's own identity reached the agent (AC-7): {out}"
        );
        let (_, out) = inside(sb, &format!("cat {}/node.toml 2>&1", bed.root.display()));
        assert!(
            !out.contains("NODE-IDENTITY"),
            "the node's identity is readable by its host path: {out}"
        );

        // One level up from the checkout: the job's own tree and nothing else.
        let (_, out) = inside(sb, &format!("ls {}/..", bed.worktree().display()));
        assert!(
            !out.contains("build-someone-else"),
            "a sibling checkout is visible from inside (AC-2): {out}"
        );
        assert!(
            out.contains("build-mine"),
            "the job cannot see its own checkout, so this proves nothing: {out}"
        );

        // …and the checkout itself IS there, read-write, or the box is useless.
        let (ok, out) = inside(
            sb,
            &format!(
                "cd {} && touch WROTE && cat README",
                bed.worktree().display()
            ),
        );
        assert!(ok && out.contains("the job's own tree"), "{out}");
        assert!(
            bed.worktree().join("WROTE").exists(),
            "the write did not reach the host tree"
        );
    }

    /// MAIN-632 AC-5/AC-6, against a real container: what the CARD named is
    /// readable and not writable, and what it did not name is still absent.
    ///
    /// The two assertions have to be in one test. "A referenced checkout is
    /// readable" passing on its own would be satisfied by mounting the whole
    /// clone cache, which is precisely the regression AC-6 is guarding, and the
    /// sibling here is the same decoy `the_host_filesystem_is_not_there_to_read`
    /// uses — so the pair says the mount list is the card's list.
    #[test]
    fn a_referenced_checkout_is_readable_and_a_sibling_is_not() {
        if !enabled() {
            return;
        }
        let bed = Bed::with_references("refs", profile_for("spec"), true);
        let sb = &bed.sandbox;

        let (ok, out) = inside(sb, &format!("cat {}/CONTRACT", bed.referenced().display()));
        assert!(
            ok && out.contains("the other side's shape"),
            "the card named this repo and the run cannot read it (AC-5): {out}"
        );

        // …and cannot write to it. NG-1 says a referenced repo takes no writes,
        // and a rule the agent is merely asked to observe is not a rule.
        let (ok, out) = inside(
            sb,
            &format!("touch {}/WROTE 2>&1", bed.referenced().display()),
        );
        assert!(
            !ok,
            "a referenced checkout is writable — the mount is not read-only: {out}"
        );
        assert!(
            !bed.referenced().join("WROTE").exists(),
            "a write reached the referenced checkout on the host"
        );

        // AC-6: naming one workspace does not open the machine. The sibling is
        // the same decoy the filesystem escape test uses.
        let (_, out) = inside(sb, &format!("cat {}/SECRET 2>&1", bed.sibling().display()));
        assert!(
            !out.contains("another card's checkout"),
            "an UNREFERENCED sibling checkout became readable once references \
             were mounted (AC-6): {out}"
        );
    }

    /// AC-11, the network half: the LAN is unreachable and the public internet
    /// is not (AC-6 — a policy that breaks package installs gets turned off).
    #[test]
    fn the_lan_is_unreachable_and_the_internet_is_not() {
        if !enabled() {
            return;
        }
        let bed = Bed::new("lan", profile_for("spec"));
        let sb = &bed.sandbox;

        for target in lan_targets() {
            let (ok, out) = inside(
                sb,
                &format!("timeout 5 sh -c 'echo > /dev/tcp/{target}/22' 2>&1"),
            );
            assert!(!ok, "a socket to {target} succeeded from inside: {out}");
            let (ok, out) = inside(sb, &format!("timeout 5 ping -c1 -W2 {target} 2>&1"));
            assert!(!ok, "ping {target} succeeded from inside: {out}");
        }

        // DNS and outbound HTTPS both work, or an operator turns this off.
        let (ok, out) = inside(sb, "getent hosts api.github.com || nslookup api.github.com");
        assert!(ok, "DNS does not resolve inside the sandbox (AC-6): {out}");
        let (ok, out) = inside(
            sb,
            "curl -sS -o /dev/null -w '%{http_code}' https://api.github.com/zen",
        );
        assert!(
            ok && out.starts_with('2'),
            "outbound HTTPS to an external API failed (AC-6): {out}"
        );
        // The registry half of AC-6. A policy that silently breaks package
        // installs is one an operator turns off, so the thing a dependency
        // install actually talks to is checked rather than assumed.
        let (ok, out) = inside(
            sb,
            "curl -sS -o /dev/null -w '%{http_code}' https://registry.npmjs.org/left-pad",
        );
        assert!(
            ok && out.starts_with('2'),
            "the npm registry is unreachable, so `npm install` would fail \
             inside a job (AC-6): {out}"
        );
    }

    /// The addresses AC-11 says must be unreachable, plus **this machine's own
    /// default gateway** — the "ping the router" case.
    ///
    /// The literals alone are a weak assertion: nothing may answer at
    /// `192.168.1.1` on this network, so the connection would fail with or
    /// without a policy. The real gateway is an RFC1918 address that DOES
    /// answer, which is what makes the test discriminating.
    fn lan_targets() -> Vec<String> {
        let mut t: Vec<String> = ["192.168.1.1", "10.0.0.1", "172.16.0.1", "169.254.169.254"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Ok(out) = Command::new("sh")
            .args(["-c", "ip route show default | awk '{print $3; exit}'"])
            .output()
        {
            let gw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !gw.is_empty() && !t.contains(&gw) {
                t.push(gw);
            }
        }
        t
    }

    /// AC-11 for the kind that has Docker, and the gap the first cut left.
    ///
    /// A container the NESTED daemon starts is FORWARDed rather than locally
    /// generated, so it never meets the job container's own OUTPUT chain. The
    /// policy that catches it is the host's, keyed on the job network's subnet
    /// — and a `build` job is precisely the kind that runs a repository's own
    /// untrusted build steps.
    #[test]
    fn the_lan_is_unreachable_from_a_nested_container() {
        if !enabled() {
            return;
        }
        let bed = Bed::new("nested", profile_for("build"));
        let sb = &bed.sandbox;

        for target in lan_targets() {
            let (ok, out) = inside(
                sb,
                &format!("docker run --rm busybox timeout 5 nc -w 3 {target} 22 2>&1"),
            );
            assert!(!ok, "a nested container opened a socket to {target}: {out}");
            let (ok, out) = inside(
                sb,
                &format!("docker run --rm busybox ping -c1 -W2 {target} 2>&1"),
            );
            assert!(!ok, "a nested container pinged {target}: {out}");
        }

        // …and the nested container still reaches the internet, or AC-6 is
        // broken for exactly the kind that installs dependencies.
        let (ok, out) = inside(
            sb,
            "docker run --rm busybox wget -q -T 20 -O /dev/null https://registry.npmjs.org/left-pad && echo REACHED",
        );
        assert!(
            ok && out.contains("REACHED"),
            "a nested container cannot reach the npm registry, so a build's \
             dependency install would fail (AC-6): {out}"
        );
    }

    /// The second bypass: the agent is in the nested daemon's group by design,
    /// so it can run a container as root in the job container's OWN network
    /// namespace and flush the in-container chain. The host's rules live in a
    /// namespace the job has no route into, so the LAN stays unreachable.
    #[test]
    fn flushing_the_containers_own_firewall_does_not_open_the_lan() {
        if !enabled() {
            return;
        }
        let bed = Bed::new("flush", profile_for("build"));
        let sb = &bed.sandbox;

        // busybox carries no `iptables`, so the flush has to be installed — and
        // it must genuinely RUN, or this test passes without exercising the
        // vector at all (measured: it did, before this).
        let (_, out) = inside(
            sb,
            "docker run --rm --net=host --cap-add=NET_ADMIN -u 0 alpine sh -c \
             'apk add --no-cache iptables >/dev/null 2>&1 && iptables -F OUTPUT && \
              iptables -S OUTPUT && echo FLUSHED'",
        );
        assert!(
            out.contains("FLUSHED"),
            "the flush never ran, so this proves nothing about the vector: {out}"
        );
        assert!(
            !out.contains("REJECT"),
            "the container's own OUTPUT chain still holds the policy, so the \
             flush did not take and the assertions below are vacuous: {out}"
        );

        for target in lan_targets() {
            let (ok, out) = inside(
                sb,
                &format!("timeout 5 sh -c 'echo > /dev/tcp/{target}/22' 2>&1"),
            );
            assert!(
                !ok,
                "after flushing the container's own OUTPUT chain, {target} became \
                 reachable — the policy was only in the container: {out}"
            );
        }
    }

    /// AC-11, the Docker half: the daemon the job talks to is its OWN, so
    /// `-v /:/host` mounts a scratch filesystem and never the machine's.
    #[test]
    fn docker_inside_the_job_cannot_reach_the_host_root() {
        if !enabled() {
            return;
        }
        let bed = Bed::new("docker", profile_for("build"));
        let sb = &bed.sandbox;

        // A socket at the ordinary path is EXPECTED — the nested daemon's. What
        // must not be true is that it is the HOST's, and the daemon's own id is
        // the only thing that settles it.
        let host_id = String::from_utf8_lossy(
            &Command::new("docker")
                .args(["info", "--format", "{{.ID}}"])
                .output()
                .expect("host docker")
                .stdout,
        )
        .trim()
        .to_string();
        let (ok, inner_id) = inside(sb, "docker info --format '{{.ID}}'");
        assert!(ok, "the nested daemon does not answer (AC-4): {inner_id}");
        assert!(
            !inner_id.trim().is_empty() && inner_id.trim() != host_id,
            "the job is talking to the HOST's Docker daemon (AC-3): {inner_id}"
        );

        // `-v /:/host` MUST work — AC-4 is that the daemon is real — and must
        // mount the NESTED root. Two markers settle which root it got, because
        // "it looks like a Linux root" is true of both.
        let host_marker =
            std::env::temp_dir().join(format!("nook-host-marker-{}", std::process::id()));
        std::fs::write(&host_marker, "the real host").expect("host marker");
        // In /tmp: the agent is not root inside either, and `/` is not its to
        // write — itself a small proof the box is doing something.
        let job_marker = format!("/tmp/nook-job-marker-{}", std::process::id());
        let (ok, out) = inside(sb, &format!("touch {job_marker}"));
        assert!(ok, "could not place the in-container marker: {out}");

        // The exit status is deliberately NOT asserted: the `cat` is SUPPOSED to
        // miss, and the shell reports its status. The two markers are the
        // check, and between them they say both halves — the daemon really ran
        // a container, and the root it mounted was not the machine's.
        let (_, out) = inside(
            sb,
            &format!(
                "docker run --rm -v /:/host busybox sh -c 'ls /host{job_marker}; \
                 cat /host{} 2>&1'",
                host_marker.display()
            ),
        );
        let _ = std::fs::remove_file(&host_marker);
        assert!(
            out.contains(&job_marker),
            "`-v /:/host` did not mount this container's root, so this proves \
             nothing about which root it DID mount: {out}"
        );
        assert!(
            !out.contains("the real host"),
            "`docker run -v /:/host` inside the job read a file from the REAL \
             host root — AC-3 has failed: {out}"
        );
    }
}

/// The sweep, run against REAL Docker objects (MAIN-617).
///
/// AC-5 asks for a test that RUNS the sweep with an unlabelled container present
/// and watches it survive, and that is deliberately more than the unit tests
/// above can say: they prove the selection, and the selection is not what
/// deletes things on somebody's machine. So this starts a container shaped
/// exactly like a job's, the operator's own beside it, and calls the real
/// [`sweep_orphans`].
///
/// Opt-in for `escapes`' reason — it needs a daemon and the job image — and
/// returns early rather than passing vacuously without the flag:
///
/// ```text
/// NOOK_SANDBOX_E2E=1 cargo test --bin nook sweep_e2e -- --test-threads=1
/// ```
#[cfg(test)]
mod sweep_e2e {
    use super::*;

    fn enabled() -> bool {
        std::env::var("NOOK_SANDBOX_E2E").as_deref() == Ok("1")
    }

    /// A container and network labelled exactly as a job's are, carrying the
    /// `build` profile's anonymous volume — plus an unlabelled container of the
    /// operator's own beside it.
    struct Bed {
        job: String,
        keepme: String,
    }

    impl Bed {
        fn new(tag: &str) -> Bed {
            let job = format!("sweep-{}-{tag}", std::process::id());
            let keepme = format!("nook-sweep-keepme-{}-{tag}", std::process::id());
            let img = image();
            docker_args(&network_create_args(&job)).expect("the job network");
            docker(&[
                "run",
                "-d",
                "--name",
                &container_name(&job),
                "--label",
                &format!("{JOB_LABEL}={job}"),
                "--network",
                &network_name(&job),
                // What AC-1 is about: an anonymous volume that, once its
                // container is gone, nothing on the machine can name again.
                "-v",
                "/var/lib/docker",
                &img,
            ])
            .expect("the job container");
            docker(&["run", "-d", "--name", &keepme, &img]).expect("the operator's own container");
            Bed { job, keepme }
        }

        fn is_up(name: &str) -> bool {
            docker(&["inspect", "--format", "{{.State.Running}}", name]).as_deref() == Ok("true")
        }
    }

    impl Drop for Bed {
        fn drop(&mut self) {
            let _ = docker(&["rm", "-f", "-v", &container_name(&self.job)]);
            let _ = docker(&["rm", "-f", "-v", &self.keepme]);
            let _ = docker(&["network", "rm", &network_name(&self.job)]);
        }
    }

    fn volumes() -> HashSet<String> {
        docker(&["volume", "ls", "-q"])
            .map(|o| o.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn the_sweep_reclaims_a_dead_jobs_objects_and_touches_nothing_else() {
        if !enabled() {
            return;
        }
        let before = volumes();
        let bed = Bed::new("orphan");
        let anonymous: HashSet<String> = volumes().difference(&before).cloned().collect();
        assert!(
            !anonymous.is_empty(),
            "no anonymous volume was created, so AC-1 cannot be observed here"
        );

        // A LIVE job survives its own sweep: the ten-minute pass runs while jobs
        // are building, and removing one of their containers would kill the run.
        let live: HashSet<String> = [bed.job.clone()].into_iter().collect();
        sweep_orphans(&live);
        assert!(
            Bed::is_up(&container_name(&bed.job)),
            "the sweep removed the container of a job this node is running"
        );

        // …and with an EMPTY running set — the state of a node that has just
        // restarted, which is the crash case (AC-6) — it goes.
        sweep_orphans(&HashSet::new());
        assert!(
            docker(&["inspect", &container_name(&bed.job)]).is_err(),
            "the orphaned job container survived the sweep (AC-1)"
        );
        assert!(
            docker(&["network", "inspect", &network_name(&bed.job)]).is_err(),
            "the orphaned job network survived the sweep (AC-2)"
        );
        let after = volumes();
        let left: Vec<&String> = anonymous.intersection(&after).collect();
        assert!(
            left.is_empty(),
            "the anonymous /var/lib/docker volume outlived its container — \
             `docker rm` was issued without `-v` (AC-1): {left:?}"
        );

        // AC-5: the operator's own container is untouched, having been present
        // for both passes.
        assert!(
            Bed::is_up(&bed.keepme),
            "the sweep removed an unlabelled container — the owner's own work is \
             not this node's to reclaim (AC-5)"
        );
    }
}

/// Source-inspecting guards (AC-10).
///
/// The confinement is only as good as the claim that every job agent goes
/// through it, and that claim is about CALL SITES, not about behaviour any
/// runtime test can observe: a future edit that adds a second spawn beside the
/// wrapped one would pass every functional test in this tree and quietly run an
/// agent on the owner's home directory. Asserting "a sandbox exists somewhere"
/// is exactly what would let that regress, so these read the files.
///
/// The shape is `tmux.rs`'s (its `the_shim_and_its_workspace_id_are_set_in_the
/// _same_place`), for the same reason it exists there.
#[cfg(test)]
mod guards {
    use std::fs;

    fn source(file: &str) -> String {
        let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
        let whole = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        match whole.find("\n#[cfg(test)]") {
            Some(i) => whole[..i].to_string(),
            None => whole,
        }
    }

    /// Every launch of a loop-job agent passes a sandbox.
    ///
    /// `loop_job.rs` holds both adapters' call sites, and the only two things
    /// that start an agent are `StreamingSession::spawn` and
    /// `tmux::new_job_session`. Each must name `sandbox` in its arguments.
    #[test]
    fn every_job_agent_launch_in_loop_job_passes_a_sandbox() {
        let src = source("loop_job.rs");
        let mut checked = 0;
        for opener in ["StreamingSession::spawn(", "crate::tmux::new_job_session("] {
            for args in call_arguments(&src, opener) {
                assert!(
                    args.contains("sandbox"),
                    "a job agent is launched without a sandbox — MAIN-611 AC-1 \
                     requires every loop-job launch to be confined:\n{opener}{args})"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 2,
            "the guard found {checked} agent launches in loop_job.rs — it is \
             matching on names that have been renamed, and is asserting nothing"
        );

        // …and the launches get the RUN's sandbox, not a literal `None`. The
        // check above sees the name `sandbox` in the argument list, which a
        // caller passing `None` one level up would still satisfy — so pin the
        // chain from `start_sandbox` to both drivers.
        assert_eq!(
            call_arguments(&src, "start_sandbox(").len(),
            1,
            "`start_sandbox` is called once, from `run`; a second call site is \
             a second, unreviewed way to decide whether an agent is confined"
        );
        for driver in ["drive_streaming(", "drive_session("] {
            let calls = call_arguments(&src, driver);
            assert!(
                !calls.is_empty(),
                "{driver} was renamed and this guard now \
                    asserts nothing"
            );
            for args in calls {
                assert!(
                    args.contains("sandbox.as_ref()"),
                    "a driver is invoked without this run's sandbox:\n{driver}{args})"
                );
            }
        }
    }

    /// Every argument list passed to `opener`, balanced across line breaks —
    /// which every one of these calls has. A DEFINITION (`fn opener(`) is
    /// skipped: it names parameters rather than passing arguments.
    fn call_arguments(src: &str, opener: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(i) = src[from..].find(opener) {
            let at = from + i;
            let start = at + opener.len();
            if src[..at].ends_with("fn ") {
                from = start;
                continue;
            }
            let mut depth = 1usize;
            let mut end = start;
            for (offset, c) in src[start..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = start + offset;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push(src[start..end].to_string());
            from = start;
        }
        out
    }

    /// The wrap lives in `spawn` itself, not in its callers.
    ///
    /// A caller-side wrap is what let `new_session` and `new_job_session`
    /// diverge in tmux.rs; the same mistake here is an agent nobody confined.
    #[test]
    fn the_streaming_adapter_wraps_inside_spawn() {
        let src = source("job_adapter.rs");
        let body = src
            .split("\n    pub fn spawn(")
            .nth(1)
            .expect("StreamingSession::spawn must exist");
        let body = body
            .split("\n    /// Send a human turn")
            .next()
            .unwrap_or(body);
        assert!(
            body.contains("sb.exec_command("),
            "spawn no longer runs the runtime inside the job container:\n{body}"
        );
        assert!(
            body.contains("Command::new(runtime)"),
            "the unconfined branch is gone, so `chat.rs` — a human's own \
             conversation, which NG-2 exempts — has nothing to spawn with"
        );
    }

    /// NG-2: no node code path prunes Docker wholesale.
    ///
    /// The operator's remedy before MAIN-617 was `docker system prune -a
    /// --volumes` by hand, which on a machine that also runs the owner's own
    /// work removes far more than NookOS ever created — their stopped
    /// containers, their unused volumes, their build cache. The sweep exists so
    /// nobody needs it, and a future edit reaching for it would be short,
    /// plausible and destructive: exactly the class a source guard catches and
    /// no functional test would.
    #[test]
    fn nothing_here_prunes_docker_wholesale() {
        for file in ["sandbox.rs", "loop_job.rs", "conn.rs", "compose.rs"] {
            let src = code(file);
            // Stripping comments must not strip the file: a `code` that ever
            // returned nothing would make every assertion below pass on an
            // empty string.
            assert!(
                src.lines().filter(|l| !l.trim().is_empty()).count() > 50,
                "{file} read as {} lines of code — this guard is asserting \
                 nothing",
                src.lines().count()
            );
            for (n, line) in src.lines().enumerate() {
                let lower = line.to_ascii_lowercase();
                assert!(
                    !(lower.contains("prune") && lower.contains("docker")),
                    "{file}:{} reaches for a docker prune — MAIN-617 NG-2: this \
                     node removes only what carries the nook.job label:\n{line}",
                    n + 1
                );
            }
        }
        // The same claim for a multi-line argv, which the line test above cannot
        // see. Only this file builds the sweep's argv, and `git worktree prune`
        // — which loop_job.rs legitimately runs — never appears here, so the
        // bare token is unambiguous.
        assert!(
            !code("sandbox.rs").contains("\"prune\""),
            "sandbox.rs passes `prune` to docker — MAIN-617 NG-2"
        );
    }

    /// A file's CODE: prose is not a call site, and the guards above would
    /// otherwise be tripped by the very comments that explain why they exist.
    fn code(file: &str) -> String {
        source(file)
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Nothing outside `sandbox.rs` may build a `docker run`/`docker exec` for
    /// a job: one place decides the flags, or the security argument is spread
    /// across files nobody re-reads together.
    #[test]
    fn the_container_flags_are_decided_in_one_file() {
        for file in ["loop_job.rs", "job_adapter.rs", "tmux.rs"] {
            let src = source(file);
            assert!(
                !src.contains("--privileged"),
                "{file} builds its own container flags — they belong in \
                 sandbox.rs, where run_args is unit-tested against AC-2/AC-3"
            );
            assert!(
                !src.contains("/var/run/docker.sock"),
                "{file} names the host's Docker socket"
            );
        }
    }
}

#[cfg(test)]
mod tests {

    /// MAIN-648: the host rules are IPv4, so a resolver answer that includes a
    /// real IPv6 address must not become one. `iptables` handed an v6 literal
    /// fails the whole install — "getaddrinfo: Address family for hostname not
    /// supported" — and a sandbox whose policy did not apply is not a sandbox,
    /// so the node claims work and then fails every job.
    #[test]
    fn only_ipv4_reaches_the_host_rules() {
        use std::net::IpAddr;

        // An ordinary A record.
        assert_eq!(
            super::allow_entry("104.21.96.102".parse::<IpAddr>().unwrap()),
            Some("104.21.96.102".to_string())
        );

        // A real AAAA answer cannot be expressed as an IPv4 rule. Skipped, not
        // stringified into one.
        assert_eq!(
            super::allow_entry("2606:4700:3035::6815:6066".parse::<IpAddr>().unwrap()),
            None
        );

        // `::ffff:a.b.c.d` is an IPv4 address wearing a v6 spelling — the shape a
        // dual-stack resolver returns for an IPv4-only host. Unwrapped, not dropped.
        assert_eq!(
            super::allow_entry("::ffff:10.12.29.201".parse::<IpAddr>().unwrap()),
            Some("10.12.29.201".to_string())
        );

        // A loopback control plane is the gateway, in either spelling: the
        // container's own loopback is not the host's.
        assert_eq!(
            super::allow_entry("127.0.0.1".parse::<IpAddr>().unwrap()),
            Some(super::HOST_GATEWAY.to_string())
        );
        assert_eq!(
            super::allow_entry("::ffff:127.0.0.1".parse::<IpAddr>().unwrap()),
            Some(super::HOST_GATEWAY.to_string())
        );
    }

    /// MAIN-648 over a WHOLE resolver answer, which is the shape the bug
    /// arrived in: the failing node's answer was mixed, so one usable A record
    /// was never the thing in question — the AAAA beside it was.
    #[test]
    fn a_mixed_resolver_answer_yields_only_its_ipv4_addresses() {
        use std::net::IpAddr;

        let answer = |ips: &[&str]| {
            ips.iter()
                .map(|s| s.parse::<IpAddr>().unwrap())
                .collect::<Vec<_>>()
        };

        // The measured answer for the node that could not start a job.
        let out = super::allow_from_addrs(answer(&[
            "104.21.96.102",
            "2606:4700:3035::6815:6066",
            "172.67.176.149",
            "2606:4700:3036::ac43:b095",
        ]))
        .expect("a mixed answer has IPv4 in it");
        assert_eq!(out, vec!["104.21.96.102", "172.67.176.149"]);
        // The whole point: nothing here can reach `iptables` as a v6 literal.
        assert!(
            out.iter().all(|e| !e.contains(':')),
            "an IPv6 literal reached the host rules: {out:?}"
        );

        // azul's answer — mapped v6 only — is unchanged by all of this.
        assert_eq!(
            super::allow_from_addrs(answer(&["10.12.29.201", "::ffff:10.12.29.201"])),
            Ok(vec!["10.12.29.201".to_string()])
        );

        // A loopback control plane is the gateway in either family, and the two
        // spellings collapse to one rule rather than two identical ones.
        assert_eq!(
            super::allow_from_addrs(answer(&["127.0.0.1", "::ffff:127.0.0.1"])),
            Ok(vec![super::HOST_GATEWAY.to_string()])
        );

        // v6-only: named, so the caller can say WHY the list is empty instead of
        // installing a policy that silently permits nothing.
        assert_eq!(
            super::allow_from_addrs(answer(&[
                "2606:4700:3035::6815:6066",
                "2606:4700:3036::ac43:b095"
            ])),
            Err(super::Ipv6OnlyAnswer { skipped: 2 })
        );

        // Resolving to nothing at all is a different fact, and stays the empty
        // list it always was.
        assert_eq!(super::allow_from_addrs(answer(&[])), Ok(Vec::new()));
    }
    use super::*;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            job_id: "job-1".into(),
            image: "nook-job-sandbox:latest".into(),
            profile: profile_for("build"),
            isolation: Isolation::Unprivileged,
            worktree: PathBuf::from("/home/ryan/.nook/clone-cache/cp/worktrees/build-1"),
            gitdir: Some(PathBuf::from(
                "/home/ryan/.nook/clone-cache/cp/acme_app.git",
            )),
            claude_dir: Some(PathBuf::from("/home/ryan/.claude")),
            caches: vec![Mount {
                host: PathBuf::from("/home/ryan/.cargo/registry"),
                container: PathBuf::from("/home/ryan/.cargo/registry"),
            }],
            references: Vec::new(),
            ports: vec![4201, 4202],
            allow: vec!["203.0.113.7".into()],
            add_hosts: Vec::new(),
            server: String::new(),
            agent_uid: 1000,
            agent_gid: 1000,
        }
    }

    /// AC-3, and the one this whole card stands on. A container holding the
    /// host's socket can `docker run -v /:/host` and undo every other line of
    /// this module in one command.
    #[test]
    fn the_host_docker_socket_is_never_mounted() {
        let args = run_args(&spec()).join(" ");
        // The job's `docker` finds the NESTED daemon at the ordinary path
        // inside its own container. Naming a daemon anywhere else — a socket
        // that arrived as a mount, a TCP daemon on the host — is the same hole
        // by another name.
        assert!(
            !args.contains("DOCKER_HOST"),
            "the job was pointed at a daemon rather than left to find its own: {args}"
        );
        let mounts: Vec<&str> = args
            .split_whitespace()
            .zip(args.split_whitespace().skip(1))
            .filter(|(a, _)| *a == "-v")
            .map(|(_, v)| v)
            .collect();
        assert!(
            mounts.iter().all(|m| !m.contains("docker.sock")),
            "the host's Docker socket reached a job container as a mount — a \
             container holding it can `docker run -v /:/host` and undo every \
             other line of this module: {mounts:?}"
        );
    }

    /// AC-12 again, from the other side: a kind with no daemon does not get
    /// one started for it.
    #[test]
    fn a_kind_without_docker_starts_no_daemon() {
        let mut sp = spec();
        sp.profile = profile_for("review");
        let args = run_args(&sp).join(" ");
        assert!(args.contains("NOOK_SANDBOX_DOCKER=0"), "{args}");
    }

    /// AC-2: what is mounted is the checkout, its repository, the agent's own
    /// session, and what the WORKSPACE asked for. Nothing else — and in
    /// particular no path that is merely an ancestor of one of those.
    #[test]
    fn only_the_declared_paths_are_mounted() {
        let args = run_args(&spec());
        let mounts: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-v")
            .map(|(_, v)| v)
            // The nested daemon's anonymous volume has no host side to check;
            // `only_the_nested_daemons_storage_is_anonymous` covers it.
            .filter(|v| v.contains(':'))
            .collect();
        assert_eq!(
            mounts,
            vec![
                "/home/ryan/.nook/clone-cache/cp/worktrees/build-1:/home/ryan/.nook/clone-cache/cp/worktrees/build-1",
                "/home/ryan/.nook/clone-cache/cp/acme_app.git:/home/ryan/.nook/clone-cache/cp/acme_app.git",
                "/home/ryan/.claude:/nook-claude",
                "/home/ryan/.cargo/registry:/home/ryan/.cargo/registry",
            ]
        );
        // The clone-cache ROOT holds every sibling checkout on the machine, and
        // mounting it instead of the one worktree is the easiest way to lose
        // AC-2 without noticing.
        for m in &mounts {
            assert!(
                !m.starts_with("/home/ryan/.nook/clone-cache/cp:")
                    && !m.starts_with("/home/ryan:")
                    && !m.starts_with("/:"),
                "a mount reaches outside the job's own checkout: {m}"
            );
        }
    }

    /// MAIN-632 AC-5: a card's `@slug` references are mounted at their own host
    /// paths and READ-ONLY — every one of them, and nothing writable.
    ///
    /// Asserted over the composed argv rather than a live container because
    /// that is where the claim lives: `:ro` is a suffix on a string, and a
    /// future edit that drops it would leave every escape test still passing
    /// (the mount would work, it would just also accept writes).
    #[test]
    fn every_reference_is_mounted_read_only() {
        let mut sp = spec();
        sp.references = vec![
            PathBuf::from("/home/ryan/.nook/clone-cache/cp/nook-web"),
            PathBuf::from("/home/ryan/.nook/clone-cache/cp/nook-api"),
        ];
        let args = run_args(&sp);
        let mounts: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-v")
            .map(|(_, v)| v)
            .collect();

        for path in &sp.references {
            let p = path.display();
            assert!(
                mounts.iter().any(|m| **m == format!("{p}:{p}:ro")),
                "the reference at {p} is not mounted read-only at its own path: {mounts:?}"
            );
            assert!(
                !mounts.iter().any(|m| **m == format!("{p}:{p}")),
                "the reference at {p} is ALSO mounted writable — NG-1 says a \
                 referenced repo takes no writes: {mounts:?}"
            );
        }
        assert_eq!(
            mounts.iter().filter(|m| m.ends_with(":ro")).count(),
            sp.references.len(),
            "read-only is for references and nothing else — the checkout, its \
             repository and the workspace's caches are all written to: {mounts:?}"
        );
    }

    /// A card that names no workspace mounts nothing extra: the reference list
    /// is the card's, so an empty one is an empty one (AC-6).
    #[test]
    fn no_references_means_no_extra_mounts() {
        let with_none = run_args(&spec());
        assert!(
            !with_none.iter().any(|a| a.ends_with(":ro")),
            "a card with no references still got a read-only mount: {with_none:?}"
        );
    }

    /// The one mount with no host path must stay the one mount with no host
    /// path: a bare `-v /some/path` is an anonymous volume, and a bare
    /// `-v /:/host` is the end of AC-2.
    #[test]
    fn only_the_nested_daemons_storage_is_anonymous() {
        let args = run_args(&spec());
        let anonymous: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-v")
            .map(|(_, v)| v)
            .filter(|v| !v.contains(':'))
            .collect();
        assert_eq!(anonymous, vec!["/var/lib/docker"]);

        let mut without = spec();
        without.profile = profile_for("spec");
        assert!(
            !run_args(&without).contains(&"/var/lib/docker".to_string()),
            "a kind with no daemon was given a daemon's storage"
        );
    }

    /// A private `/tmp` is part of AC-2 — a shared one is a channel between
    /// jobs and a place to leave a credential.
    #[test]
    fn tmp_is_private_to_the_job() {
        let args = run_args(&spec());
        let i = args.iter().position(|a| a == "--tmpfs").expect("a tmpfs");
        assert!(args[i + 1].starts_with("/tmp:"), "{:?}", args[i + 1]);
        assert!(args[i + 1].contains("nosuid"), "{:?}", args[i + 1]);
    }

    /// The default box gets no host devices and no `CAP_SYS_ADMIN`: a
    /// `--privileged` container has the host's `/dev`, so `mount /dev/sda1`
    /// undoes AC-2 without ever touching a Docker socket.
    /// `--privileged` is opt-in, never the default and never what an
    /// unreadable setting falls back to: it is the mode that hands a job the
    /// host's block devices.
    #[test]
    fn the_default_box_is_not_privileged() {
        assert_eq!(Isolation::parse(""), Isolation::Unprivileged);
        assert_eq!(Isolation::parse("nonsense"), Isolation::Unprivileged);
        assert_eq!(Isolation::parse("rootless"), Isolation::Unprivileged);
        assert_eq!(Isolation::parse(" Privileged "), Isolation::Privileged);
        let args = isolation_args(profile_for("build"), Isolation::Unprivileged);
        assert!(!args.iter().any(|a| a == "--privileged"), "{args:?}");
        assert!(
            args.iter().any(|a| a == "NET_ADMIN"),
            "the firewall needs it"
        );
        // No `--device`: a job container is handed no hardware at all, which is
        // what makes the host's disk unreachable even with CAP_SYS_ADMIN.
        assert!(!args.iter().any(|a| a == "--device"), "{args:?}");
    }

    /// The helper must override the image's ENTRYPOINT, or it inherits
    /// `tail -f /dev/null` and hangs every job launch on the node.
    #[test]
    fn the_host_firewall_helper_overrides_the_images_entrypoint() {
        let argv = host_iptables_args("nook-job-sandbox:latest", &["-S".into()]);
        let line = argv.join(" ");
        assert!(
            line.contains("--entrypoint iptables"),
            "without this the image's entrypoint swallows the arguments and \
             blocks forever: {line}"
        );
        let img = argv
            .iter()
            .position(|a| a == "nook-job-sandbox:latest")
            .expect("the image");
        assert_eq!(argv[img + 1], "-S", "arguments follow the image: {line}");
        assert!(
            line.contains("--net=host"),
            "the rules must land on the HOST: {line}"
        );
        assert!(line.contains("--cap-add NET_ADMIN"), "{line}");
        assert!(
            !line.contains("--privileged"),
            "NET_ADMIN is enough: {line}"
        );
    }

    /// A dual-stack daemon gives a new network two IPAM configs, and the
    /// concatenated string that produced made every `iptables` call fail —
    /// which fails the job, on a node whose operator is given no hint that
    /// IPv6 is involved.
    #[test]
    fn a_dual_stack_network_yields_its_ipv4_subnet_alone() {
        assert_eq!(
            first_ipv4("172.30.0.0/16 fd00:dead:beef::/64 "),
            Some("172.30.0.0/16".to_string())
        );
        // …in either order, since the template's order is the daemon's.
        assert_eq!(
            first_ipv4("fd00:dead:beef::/64 172.30.0.0/16"),
            Some("172.30.0.0/16".to_string())
        );
        assert_eq!(first_ipv4("172.30.0.1 "), Some("172.30.0.1".to_string()));
        // v6-only leaves nothing this policy can key on, and the caller must
        // fail closed rather than police a string iptables will reject.
        assert_eq!(first_ipv4("fd00:dead:beef::/64"), None);
        assert_eq!(first_ipv4("   "), None);
    }

    /// AC-5's ENFORCEMENT point. The in-container chain below is defence in
    /// depth; these are the rules a `build` job cannot evade, and they are
    /// keyed on the job network's SUBNET so a nested container's SNAT'ed
    /// traffic is caught by the same rule as the job container's own.
    #[test]
    fn the_host_policy_drops_every_private_range_for_the_job_subnet() {
        let rules = host_rules("172.30.0.0/16", &["203.0.113.7".into()], "4442", None);
        let flat: Vec<String> = rules.iter().map(|r| r.join(" ")).collect();
        // Two allow forms per address, both scoped to the control-plane PORT:
        // the ORIGINAL destination (a control plane published on this machine is
        // DNAT'ed before the filter table sees it) and the current one. Address
        // alone would open every other service on the gateway.
        assert_eq!(
            flat[0],
            "-s 172.30.0.0/16 -p tcp -m conntrack --ctorigdst 203.0.113.7 --ctorigdstport 4442 -j RETURN"
        );
        assert_eq!(
            flat[1],
            "-s 172.30.0.0/16 -d 203.0.113.7 -p tcp --dport 4442 -j RETURN"
        );
        for (i, range) in PRIVATE_RANGES.iter().enumerate() {
            assert_eq!(
                flat[i + 2],
                format!(
                    "-s 172.30.0.0/16 -d {range} -j REJECT --reject-with icmp-admin-prohibited"
                )
            );
        }
        assert_eq!(flat.len(), PRIVATE_RANGES.len() + 2);
        // Every rule names the job's own subnet: a rule without one would
        // police the whole machine, and a rule with somebody else's would
        // police the wrong job.
        assert!(
            flat.iter().all(|r| r.starts_with("-s 172.30.0.0/16 ")),
            "{flat:?}"
        );
    }

    /// The exception precedes the drops here too — a RETURN after a REJECT is
    /// a rule that never runs.
    #[test]
    fn the_host_policy_lets_the_control_plane_through_before_it_drops() {
        let rules = host_rules("10.9.0.0/16", &["192.168.1.20".into()], "8080", None);
        let flat: Vec<String> = rules.iter().map(|r| r.join(" ")).collect();
        let allow = flat
            .iter()
            .position(|r| r.contains("192.168.1.20"))
            .expect("the exception");
        let first_drop = flat
            .iter()
            .position(|r| r.contains("REJECT"))
            .expect("a drop");
        assert!(allow < first_drop, "{flat:?}");
        // …and the range around it stays dropped: allowed BY ADDRESS, never
        // because the range is private.
        assert!(
            flat.iter()
                .any(|r| r.contains("-d 192.168.0.0/16 -j REJECT")),
            "{flat:?}"
        );
    }

    /// A loopback control plane is reachable only through the job network's
    /// gateway, and only the host can resolve that — so with no gateway known
    /// the rule is DROPPED rather than guessed at.
    #[test]
    fn the_host_gateway_exception_needs_a_resolved_gateway() {
        let with = host_rules(
            "10.9.0.0/16",
            &[HOST_GATEWAY.into()],
            "4442",
            Some("10.9.0.1"),
        );
        assert_eq!(
            with[0].join(" "),
            "-s 10.9.0.0/16 -p tcp -m conntrack --ctorigdst 10.9.0.1 --ctorigdstport 4442 -j RETURN"
        );
        assert_eq!(
            with[1].join(" "),
            "-s 10.9.0.0/16 -d 10.9.0.1 -p tcp --dport 4442 -j RETURN"
        );
        let without = host_rules("10.9.0.0/16", &[HOST_GATEWAY.into()], "4442", None);
        assert!(
            !without
                .iter()
                .any(|r| r.contains(&HOST_GATEWAY.to_string())),
            "the placeholder must never reach iptables: {without:?}"
        );
        assert_eq!(without.len(), PRIVATE_RANGES.len());
    }

    /// Teardown must be able to name every rule setup added, or a leftover
    /// polices whichever job Docker next hands the subnet to.
    #[test]
    fn every_rule_added_is_reproducible_for_deletion() {
        let allow = vec!["203.0.113.7".into(), "198.51.100.4".into()];
        assert_eq!(
            host_rules("172.30.0.0/16", &allow, "4442", Some("172.30.0.1")),
            host_rules("172.30.0.0/16", &allow, "4442", Some("172.30.0.1")),
            "the delete pass rebuilds the add pass's rules exactly"
        );
    }

    /// AC-5 in the order that makes it true: loopback and the nested bridges
    /// first (so DNS and the job's own compose stack work), the control plane
    /// by address, then the four ranges.
    #[test]
    fn egress_drops_every_private_range_and_allows_the_control_plane_by_address() {
        let s = egress_script(&["203.0.113.7".into()], "4442");
        for range in PRIVATE_RANGES {
            assert!(
                s.contains(&format!("-d {range} -j REJECT")),
                "{range} is not dropped:\n{s}"
            );
        }
        let allow = s
            .find("-d 203.0.113.7 -p tcp --dport 4442 -j ACCEPT")
            .expect("the control plane, port-scoped");
        let first_drop = s.find("-j REJECT").expect("a drop");
        assert!(
            allow < first_drop,
            "the exception must precede the drops, or it never applies:\n{s}"
        );
        assert!(
            s.contains("-o lo -j ACCEPT"),
            "Docker's embedded DNS resolver is on 127.0.0.11; AC-6 fails \
             without this:\n{s}"
        );
        assert!(
            !s.contains("-d 0.0.0.0/0 -j ACCEPT"),
            "a blanket accept would make the drops unreachable:\n{s}"
        );
    }

    /// The policy allows an ADDRESS, never a range, and never "it is private".
    #[test]
    fn a_private_control_plane_is_allowed_as_one_host_not_as_a_range() {
        let s = egress_script(&["192.168.1.20".into()], "8080");
        assert!(s.contains("-d 192.168.1.20 -p tcp --dport 8080 -j ACCEPT"));
        assert!(
            s.contains("-d 192.168.0.0/16 -j REJECT"),
            "the range around the exception stays dropped:\n{s}"
        );
    }

    /// AC-12: a new loop kind must pick a profile, not acquire one by falling
    /// through a match. The table is the mechanism; this is what keeps it
    /// complete.
    #[test]
    fn every_known_loop_kind_has_a_profile() {
        for kind in crate::capabilities::KNOWN_LOOP_KINDS {
            assert!(
                PROFILES.iter().any(|p| p.kind == *kind),
                "loop kind {kind:?} has no sandbox profile — add a row to \
                 PROFILES saying whether it runs containers"
            );
        }
    }

    /// AC-12: `build` is the only kind that runs containers today, and the
    /// others are not merely cheaper — they keep Docker's seccomp and AppArmor
    /// profiles, which nesting has to relax.
    #[test]
    fn only_build_is_given_a_nested_daemon() {
        assert!(profile_for("build").nested_docker);
        for kind in [
            "review",
            "spec",
            "decompose",
            "epic-run",
            // Read-only AND externally driven (MAIN-331) — the kind with the
            // strongest claim to the strict box, not the weakest.
            "investigate",
            "something-new",
        ] {
            let p = profile_for(kind);
            assert!(!p.nested_docker, "{kind} was handed a Docker daemon");
            let args = isolation_args(p, Isolation::Privileged);
            assert_eq!(
                args,
                vec!["--cap-add".to_string(), "NET_ADMIN".to_string()],
                "{kind} got a nesting relaxation it never asked for: {args:?}"
            );
        }
    }

    /// The mounts and the egress policy do NOT vary by kind (AC-12): only the
    /// daemon does.
    #[test]
    fn a_kind_without_docker_is_confined_exactly_the_same_way() {
        // Host mounts only: the nested daemon's anonymous volume is the one
        // thing the profile is ALLOWED to change, and it has no host side.
        let mounts = |p: Profile| -> Vec<String> {
            let mut s = spec();
            s.profile = p;
            let args = run_args(&s);
            args.iter()
                .enumerate()
                .filter(|(i, _)| *i > 0 && args[i - 1] == "-v")
                .map(|(_, v)| v.clone())
                .filter(|v| v.contains(':'))
                .collect()
        };
        assert_eq!(mounts(profile_for("build")), mounts(profile_for("spec")));
        let mut s = spec();
        s.profile = profile_for("spec");
        let args = run_args(&s).join(" ");
        assert!(!args.contains("docker.sock"), "{args}");
        assert!(args.contains("--tmpfs"), "{args}");
    }

    #[test]
    fn leased_ports_are_published_so_a_human_can_still_open_the_stack() {
        let args = run_args(&spec()).join(" ");
        assert!(args.contains("-p 4201:4201"));
        assert!(args.contains("-p 4202:4202"));
    }

    #[test]
    fn a_control_plane_url_yields_one_host_and_port() {
        assert_eq!(
            host_and_port("https://nook.example.com"),
            Some(("nook.example.com".into(), "443".into()))
        );
        assert_eq!(
            host_and_port("http://localhost:8080"),
            Some(("localhost".into(), "8080".into()))
        );
        assert_eq!(
            host_and_port("http://cp.internal/api/v1"),
            Some(("cp.internal".into(), "80".into()))
        );
        assert_eq!(host_and_port(""), None);
    }

    /// A loopback control plane is rewritten onto the alias; anything else is
    /// left exactly as the operator wrote it.
    ///
    /// The bug this pins: `localhost` reached the agent verbatim, resolved to
    /// the CONTAINER's loopback, and every `nook` call inside a sandboxed run
    /// died with `Connection refused` — the run then "completed" having filed
    /// nothing. `--add-host localhost:host-gateway` cannot fix it, because
    /// Docker's own `127.0.0.1 localhost` is written first and wins.
    #[test]
    fn a_loopback_control_plane_is_spelled_differently_inside() {
        assert_eq!(
            server_for_container("http://localhost:4442"),
            format!("http://{HOST_ALIAS}:4442")
        );
        assert_eq!(
            server_for_container("http://127.0.0.1:8080/api"),
            format!("http://{HOST_ALIAS}:8080/api")
        );
        assert_eq!(
            server_for_container("http://localhost"),
            format!("http://{HOST_ALIAS}")
        );
        // Not loopback: unchanged, including the port and the path.
        assert_eq!(
            server_for_container("https://nook.example.com"),
            "https://nook.example.com"
        );
        assert_eq!(
            server_for_container("https://nook.example.com:8443/x"),
            "https://nook.example.com:8443/x"
        );
        // A host that merely CONTAINS the word is not loopback.
        assert_eq!(
            server_for_container("https://localhost.example.com"),
            "https://localhost.example.com"
        );
        assert_eq!(server_for_container(""), "");
    }

    /// The host alias resolves to THIS network's gateway, not to Docker's own
    /// host proxy.
    ///
    /// The bug this pins: on Docker Desktop `host-gateway` is a 192.168/16
    /// address, which `host_rules` drops — so the agent resolved the control
    /// plane to an address the very same policy forbade it to reach.
    #[test]
    fn the_host_alias_is_pinned_to_the_address_the_policy_allows() {
        let mut hosts = vec![
            format!("{HOST_ALIAS}:host-gateway"),
            "already.pinned:203.0.113.9".to_string(),
        ];
        pin_host_alias(&mut hosts, "172.26.0.1");
        assert_eq!(hosts[0], format!("{HOST_ALIAS}:172.26.0.1"));
        assert_eq!(hosts[1], "already.pinned:203.0.113.9");
    }

    #[test]
    fn a_container_name_survives_an_awkward_job_id() {
        assert_eq!(container_name("abc/def:1"), "nook-job-abc_def_1");
    }

    /// One machine's `docker ps`, as MAIN-617 has to read it: a live job, a job
    /// a killed node left behind, and the owner's own work.
    fn listing() -> Vec<Listed> {
        parse_listing(
            "nook-job-live\tnook.job=live,com.docker.compose.project=x\n\
             nook-job-dead\tnook.job=dead\n\
             keepme\t\n\
             nook-os-postgres-1\tcom.docker.compose.project=nook-os\n",
        )
    }

    /// AC-1: given a listing and a running set, the sweep selects exactly the
    /// orphans — no live job, and nothing unlabelled.
    #[test]
    fn the_sweep_selects_exactly_the_orphaned_job_objects() {
        let listed = listing();
        let running: HashSet<String> = ["live".to_string()].into_iter().collect();
        let names: Vec<&str> = orphans(&listed, &running)
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(names, vec!["nook-job-dead"]);
    }

    /// AC-5, and the reason this module removes by LABEL and never by name: the
    /// owner's own container, their compose stack, and even a container NAMED
    /// exactly like a job's are all left alone. A sweep on a developer's laptop
    /// that took one of those would be worse than the leak it fixes.
    #[test]
    fn an_unlabelled_container_is_never_swept_whatever_it_is_called() {
        let mut listed = listing();
        // A container NAMED like a job's is the strongest form of the case: it
        // looks exactly like ours and makes no claim to be.
        listed.push(Listed {
            name: container_name("not-really-a-job"),
            job_id: None,
        });
        let survivors: Vec<&str> = listed
            .iter()
            .filter(|l| l.job_id.is_none())
            .map(|l| l.name.as_str())
            .collect();
        let swept = orphans(&listed, &HashSet::new());
        for name in survivors {
            assert!(
                !swept.iter().any(|o| o.name == name),
                "{name} carries no {JOB_LABEL} label and must survive the sweep"
            );
        }
        // …and the daemon is asked for the label too, so an unlabelled object is
        // never even listed. Belt and braces: the filter is what keeps a busy
        // machine's listing small, the label test above is what decides.
        assert!(
            list_containers_args().contains(&format!("label={JOB_LABEL}")),
            "{:?}",
            list_containers_args()
        );
    }

    /// AC-6, the crash case: after a restart the running set is empty, so every
    /// labelled leftover is an orphan. This is the whole point of the ticket —
    /// `Sandbox::stop` already covers every case in which the node survives.
    #[test]
    fn a_restarted_node_treats_every_labelled_container_as_an_orphan() {
        let listed = listing();
        let names: Vec<&str> = orphans(&listed, &HashSet::new())
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(names, vec!["nook-job-live", "nook-job-dead"]);
    }

    /// AC-1's volume claim, as an assertion about a vector.
    ///
    /// Without `-v` a `build` job's anonymous `/var/lib/docker` volume — a whole
    /// nested image cache — outlives the container that named it, with nothing
    /// left on the machine able to identify it. The only remedy left would be
    /// `docker volume prune`, which NG-2 forbids.
    #[test]
    fn removing_an_orphan_takes_its_anonymous_volume_with_it() {
        assert_eq!(
            remove_container_args("nook-job-dead"),
            vec!["rm", "-f", "-v", "nook-job-dead"]
        );
        assert_eq!(
            remove_network_args("nook-job-dead-net"),
            vec!["network", "rm", "nook-job-dead-net"]
        );
    }

    /// A job's network carries the label the sweep matches on (AC-2/AC-5) —
    /// otherwise an orphaned network could only be removed by NAME, which is
    /// what AC-5 refuses.
    #[test]
    fn a_job_network_is_labelled_for_the_job_that_owns_it() {
        let args = network_create_args("job-1");
        assert_eq!(
            args,
            vec![
                "network",
                "create",
                "--label",
                "nook.job=job-1",
                "nook-job-job-1-net"
            ]
        );
        // The same label the container carries, so one sweep finds both.
        let container = run_args(&spec()).join(" ");
        assert!(container.contains("--label nook.job=job-1"), "{container}");
    }

    /// The listing parser, on the two shapes Docker actually emits: a labelled
    /// object and one with no labels at all.
    #[test]
    fn a_listing_line_yields_its_job_id_or_none() {
        let listed = listing();
        assert_eq!(listed[0].job_id.as_deref(), Some("live"));
        assert_eq!(listed[2].name, "keepme");
        assert_eq!(listed[2].job_id, None);
        assert_eq!(listed[3].job_id, None);
        // A label whose key merely CONTAINS ours is a different label.
        let other = parse_listing("x\tnook.jobs=1,not.nook.job=2");
        assert_eq!(other[0].job_id, None);
    }

    /// AC-3: a rule is deleted only when no live job holds its subnet, and the
    /// spec deleted is byte-for-byte the one `iptables -S` printed — `-D`
    /// matches a whole rule, so anything less silently matches nothing.
    #[test]
    fn only_rules_for_subnets_no_live_job_holds_are_deleted() {
        let saved = "\
-N NOOK-SANDBOX
-A NOOK-SANDBOX -s 172.30.0.0/16 -d 10.0.0.0/8 -j REJECT --reject-with icmp-admin-prohibited
-A NOOK-SANDBOX -s 172.31.0.0/16 -d 10.0.0.0/8 -j REJECT --reject-with icmp-admin-prohibited
-A NOOK-SANDBOX -s 172.31.0.0/16 -d 172.30.0.1 -p tcp --dport 4442 -j RETURN
-A DOCKER-USER -j NOOK-SANDBOX
";
        let live: HashSet<String> = ["172.30.0.0/16".to_string()].into_iter().collect();
        let picked: Vec<String> = orphan_rules(saved, &live)
            .iter()
            .map(|r| r.join(" "))
            .collect();
        assert_eq!(
            picked,
            vec![
                "-s 172.31.0.0/16 -d 10.0.0.0/8 -j REJECT --reject-with icmp-admin-prohibited",
                "-s 172.31.0.0/16 -d 172.30.0.1 -p tcp --dport 4442 -j RETURN",
            ],
            "the live job's rules must be untouched, and a rule in another \
             chain is not this sweep's to delete"
        );
        // Nothing live: every rule in the chain is an orphan (AC-6 again).
        assert_eq!(orphan_rules(saved, &HashSet::new()).len(), 3);
        // …and every rule this module writes IS selectable, or the sweep would
        // silently leave the policy behind.
        let rules = host_rules("10.9.0.0/16", &["203.0.113.7".into()], "4442", None);
        let saved: String = rules
            .iter()
            .map(|r| format!("-A {HOST_CHAIN} {}\n", r.join(" ")))
            .collect();
        assert_eq!(orphan_rules(&saved, &HashSet::new()).len(), rules.len());
        assert!(orphan_rules(&saved, &["10.9.0.0/16".to_string()].into()).is_empty());
    }

    /// A tmux launch line is readable with `capture-pane`, so a credential must
    /// travel as a NAME the shell already holds and never as a value written
    /// into the command.
    #[test]
    fn a_launch_line_forwards_credentials_by_name_and_never_by_value() {
        let sb = Sandbox {
            name: "nook-job-x".into(),
            network: "nook-job-x-net".into(),
            subnet: "172.30.0.0/16".into(),
            image: "nook-job-sandbox:test".into(),
            allow: Vec::new(),
            allow_port: "4442".into(),
            server: "http://host.docker.internal:4442".into(),
            uid: 1000,
            gid: 1000,
        };
        let line = sb.exec_shell_line("claude", Path::new("/w"), &["GH_TOKEN", "NOOK_JOB_ID"]);
        assert!(line.contains("-e 'GH_TOKEN'"), "{line}");
        assert!(
            !line.contains("GH_TOKEN="),
            "a value reached the line: {line}"
        );
        assert!(line.contains("-u 1000:1000"), "{line}");
    }

    #[test]
    fn the_agent_gets_its_own_home_and_the_mounted_claude_session() {
        let sb = Sandbox {
            name: "nook-job-x".into(),
            network: "nook-job-x-net".into(),
            subnet: "172.30.0.0/16".into(),
            image: "nook-job-sandbox:test".into(),
            allow: Vec::new(),
            allow_port: "4442".into(),
            server: "http://host.docker.internal:4442".into(),
            uid: 1000,
            gid: 1000,
        };
        let cmd = sb.exec_shell_line("claude", Path::new("/w"), &[]);
        assert!(cmd.contains("'HOME=/home/agent'"), "{cmd}");
        assert!(cmd.contains("'CLAUDE_CONFIG_DIR=/nook-claude'"), "{cmd}");
    }

    /// The failure MAIN-630 was reported from, verbatim.
    const BIND_FAILED: &str = "docker: Error response from daemon: failed to set up container \
         networking: driver failed programming external connectivity on endpoint \
         nook-job-019f840f-2d80-7163-b4b1-8b1e12d7e0d3: Bind for 0.0.0.0:4389 failed: port is \
         already allocated";

    /// The `docker ps` of the machine it was observed on: the card's own
    /// pre-sandbox stack, still up.
    const PS: &str = "nook-build-019f8-main-611-web-1\t0.0.0.0:4389->5173/tcp, [::]:4389->5173/tcp\tcom.docker.compose.project=nook-build-019f8-main-611,com.docker.compose.service=web\n\
         nook-build-019f8-main-611-postgres-1\t5432/tcp\tcom.docker.compose.project=nook-build-019f8-main-611,com.docker.compose.service=postgres";

    /// AC-4: the message names the port AND the container holding it, so
    /// nobody needs `docker ps` to learn what is squatting.
    #[test]
    fn a_bind_failure_names_the_container_holding_the_port() {
        let said = describe_bind_conflict(BIND_FAILED, PS).expect("a bind conflict is recognised");
        assert!(said.contains("4389"), "{said}");
        assert!(said.contains("nook-build-019f8-main-611-web-1"), "{said}");
        assert!(
            said.contains("docker compose -p nook-build-019f8-main-611 down"),
            "the compose project is the name a person acts on: {said}"
        );
    }

    /// The proxy's spelling of the same event, and a holder outside compose.
    #[test]
    fn the_other_spelling_and_a_container_of_nobodys() {
        let err = "driver failed programming external connectivity on endpoint nook-job-x: \
                   listen tcp4 0.0.0.0:4389: bind: address already in use";
        let ps = "some-database\t0.0.0.0:4389->5432/tcp\torg.opencontainers.image.title=Postgres";
        let said = describe_bind_conflict(err, ps).expect("a bind conflict is recognised");
        assert_eq!(said, "host port 4389 is held by container `some-database`");
    }

    /// A port nothing on this daemon publishes is worth saying so: no
    /// `compose down` will free it.
    #[test]
    fn a_port_no_container_holds_says_so() {
        let said = describe_bind_conflict(BIND_FAILED, "").expect("still a bind conflict");
        assert!(said.contains("outside this Docker daemon"), "{said}");
    }

    /// Only the port BOUND on the host counts: a container merely exposing
    /// 4389 internally is holding nothing.
    #[test]
    fn an_exposed_port_is_not_a_bound_one() {
        let ps = "inner\t4389/tcp\tcom.docker.compose.project=theirs";
        assert_eq!(holder_of_port(ps, "4389"), None);
        assert_eq!(
            holder_of_port("bound\t0.0.0.0:4389->4389/tcp\t", "4389"),
            Some(PortHolder {
                container: "bound".into(),
                project: None,
            })
        );
    }

    /// Anything that is not a bind conflict is reported exactly as Docker gave
    /// it — a failure this does not understand is never decorated with a guess.
    #[test]
    fn another_failure_is_left_alone() {
        for err in [
            "docker: Error response from daemon: no such image: nook-job-sandbox:latest",
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock",
            "",
        ] {
            assert_eq!(bind_conflict_port(err), None, "{err}");
            assert_eq!(describe_bind_conflict(err, PS), None, "{err}");
        }
    }
}

/// The image a node reaches for, and what it does when that image is absent
/// (MAIN-643). Pure functions throughout: no Docker, no registry.
#[cfg(test)]
mod image_tests {
    use super::*;

    fn observed<'a>(image: &'a str, pull: &'a PullState) -> Observed<'a> {
        Observed {
            containerised: None,
            docker_error: None,
            image,
            image_present: false,
            image_configured: false,
            pull,
            isolation: Isolation::Unprivileged,
        }
    }

    /// AC-2: the default is the PUBLISHED image at this agent's own version,
    /// so a self-updated agent and its box cannot drift apart.
    #[test]
    fn the_default_image_is_this_agents_own_version() {
        assert_eq!(
            default_image(),
            format!(
                "ghcr.io/nook-os/nook-job-sandbox:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        // Not a floating tag, which is the whole bug: `latest` was built once
        // by hand and carried an older `nook` than the agent running it.
        assert!(!default_image().ends_with(":latest"));
    }

    /// AC-3, first row: the image is absent, so a pull is started rather than
    /// the node declaring defeat.
    #[test]
    fn an_absent_image_is_pulled_before_anything_is_declared() {
        let d = decide(&observed(
            "ghcr.io/nook-os/nook-job-sandbox:1.2.3",
            &PullState::Untried,
        ));
        assert!(d.start_pull);
        assert_eq!(
            d.capability,
            SandboxCapability::Pulling {
                image: "ghcr.io/nook-os/nook-job-sandbox:1.2.3".into()
            }
        );
    }

    /// AC-3, second row: the pull landed, so the next probe sees the image and
    /// the node is Ready.
    #[test]
    fn a_pull_that_succeeded_leaves_the_node_ready() {
        let mut obs = observed("nook-job-sandbox:1.2.3", &PullState::Untried);
        obs.image_present = true;
        let d = decide(&obs);
        assert!(!d.start_pull);
        assert_eq!(
            d.capability,
            SandboxCapability::Ready {
                image: "nook-job-sandbox:1.2.3 (unprivileged)".into()
            }
        );
    }

    /// AC-3, third row: a pull that failed is where `Unavailable` comes from —
    /// and it is not retried, so nothing hammers the registry per heartbeat.
    #[test]
    fn a_pull_that_failed_is_unavailable_and_not_retried() {
        let pull = PullState::Failed {
            reason: SandboxUnavailable::NotPublished,
            detail: "nothing published it".into(),
        };
        let d = decide(&observed("nook-job-sandbox:1.2.3", &pull));
        assert!(!d.start_pull);
        assert_eq!(
            d.capability,
            SandboxCapability::Unavailable {
                detail: "nothing published it".into(),
                reason: SandboxUnavailable::NotPublished,
            }
        );
    }

    /// A pull that reports success and leaves no image is reported, not pulled
    /// again — the retry loop it would otherwise sit in has no ceiling, and a
    /// node quietly hammering a registry every 15s is worse than one saying its
    /// Docker is misbehaving.
    #[test]
    fn a_pull_that_produced_no_image_is_not_pulled_again() {
        let d = decide(&observed("nook-job-sandbox:1.2.3", &PullState::Succeeded));
        assert!(!d.start_pull);
        let SandboxCapability::Unavailable { detail, .. } = d.capability else {
            panic!("a pull that produced nothing leaves the node Unavailable");
        };
        assert!(detail.contains("nook-job-sandbox:1.2.3"), "{detail}");
        assert!(detail.contains("still not on this node"), "{detail}");
    }

    /// ...and the ordinary path still re-arms, so an image removed under a
    /// long-lived node is fetched again rather than latching Unavailable.
    #[test]
    fn a_pull_that_produced_the_image_leaves_it_ready() {
        let mut obs = observed("nook-job-sandbox:1.2.3", &PullState::Succeeded);
        obs.image_present = true;
        let d = decide(&obs);
        assert!(!d.start_pull);
        assert!(matches!(d.capability, SandboxCapability::Ready { .. }));
    }

    /// AC-4: warming up and broken are DIFFERENT values, not one string. A node
    /// three minutes into a pull must not read as one an operator has to fix.
    #[test]
    fn pulling_is_a_different_state_from_failed() {
        let pulling = decide(&observed("i", &PullState::Running)).capability;
        let failed = decide(&observed(
            "i",
            &PullState::Failed {
                reason: SandboxUnavailable::PullRefused,
                detail: "connection reset".into(),
            },
        ))
        .capability;
        assert!(matches!(pulling, SandboxCapability::Pulling { .. }));
        assert!(matches!(failed, SandboxCapability::Unavailable { .. }));
        assert_ne!(pulling, failed);
        // Both refuse work — NG-4's fail-closed rule is untouched — but they
        // say different things about how long that lasts.
        assert!(!pulling.may_run_loop_work());
        assert!(!failed.may_run_loop_work());
        assert_ne!(pulling.refusal(), failed.refusal());
    }

    /// AC-5: an image an operator NAMED is theirs. Never pulled, never
    /// replaced by the published default.
    #[test]
    fn a_configured_image_is_never_pulled() {
        let mut obs = observed("registry.internal/our-sandbox:pinned", &PullState::Untried);
        obs.image_configured = true;
        let d = decide(&obs);
        assert!(
            !d.start_pull,
            "an operator's own image was pulled behind their back"
        );
        let SandboxCapability::Unavailable { detail, reason } = d.capability else {
            panic!("a named image that is absent is Unavailable");
        };
        assert_eq!(reason, SandboxUnavailable::NotPresent);
        assert!(detail.contains("registry.internal/our-sandbox:pinned"));
        assert!(detail.contains("NOOK_SANDBOX_IMAGE"));
    }

    /// The states that never reach the image at all.
    #[test]
    fn a_containerised_node_and_a_dead_daemon_short_circuit() {
        let mut obs = observed("i", &PullState::Untried);
        obs.containerised = Some("/.dockerenv is present".into());
        let d = decide(&obs);
        assert!(!d.start_pull);
        assert!(matches!(d.capability, SandboxCapability::Exempt { .. }));

        let mut obs = observed("i", &PullState::Untried);
        obs.docker_error = Some("Cannot connect to the Docker daemon".into());
        let d = decide(&obs);
        assert!(!d.start_pull, "a node with no daemon tried to pull with it");
        assert!(matches!(
            d.capability,
            SandboxCapability::Unavailable {
                reason: SandboxUnavailable::NoDocker,
                ..
            }
        ));
    }

    /// AC-6: each way a pull can fail maps to the reason an operator acts on.
    #[test]
    fn every_pull_failure_names_the_action_it_needs() {
        let cases = [
            (
                "manifest unknown: manifest unknown",
                SandboxUnavailable::NotPublished,
            ),
            (
                "manifest for ghcr.io/nook-os/nook-job-sandbox:0.6.13 not found",
                SandboxUnavailable::NotPublished,
            ),
            (
                "Head \"https://ghcr.io/v2/...\": unauthorized",
                SandboxUnavailable::NoCredentials,
            ),
            (
                "denied: requested access to the resource is denied",
                SandboxUnavailable::NoCredentials,
            ),
            (
                "dial tcp: lookup ghcr.io: no such host",
                SandboxUnavailable::PullRefused,
            ),
            ("", SandboxUnavailable::PullRefused),
        ];
        for (err, want) in cases {
            let (got, detail) = classify_pull_failure("the-image", err);
            assert_eq!(got, want, "{err}");
            assert!(detail.contains("the-image"), "{err} lost the image name");
        }
    }
}
