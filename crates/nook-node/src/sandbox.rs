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

use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nook_types::SandboxCapability;

/// The image a job container is started from. Built by
/// `deploy/docker/job-sandbox.Dockerfile`: the operator node's toolchain plus a
/// nested Docker daemon. Override with `NOOK_SANDBOX_IMAGE`.
pub const DEFAULT_IMAGE: &str = "nook-job-sandbox:latest";

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

/// The job's own Docker network, named for it so a leftover is identifiable.
pub fn network_name(job_id: &str) -> String {
    format!("{}-net", container_name(job_id))
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
        format!("nook.job={}", spec.job_id),
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
        docker(&["network", "create", &net])
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
            sb.stop();
            return Err(format!("could not start the job sandbox: {e}"));
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
        let argv = host_iptables_args(&self.image, args);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        docker(&refs)
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

/// The image this node starts job containers from.
pub fn image() -> String {
    match std::env::var("NOOK_SANDBOX_IMAGE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_IMAGE.to_string(),
    }
}

/// The isolation mode this node is configured for.
pub fn isolation() -> Isolation {
    Isolation::parse(&std::env::var("NOOK_SANDBOX_ISOLATION").unwrap_or_default())
}

/// What this node reports on heartbeat (AC-9), and what the dispatcher's
/// fail-closed gate reads (AC-8).
///
/// Probed rather than assumed, and probed the way every other capability here
/// is: ask the tool, do not read a config file and hope.
pub fn probe() -> SandboxCapability {
    if let Some(detail) = containerised() {
        return SandboxCapability::Exempt { detail };
    }
    let image = image();
    if let Err(e) = docker(&["version", "--format", "{{.Server.Version}}"]) {
        return SandboxCapability::Unavailable {
            detail: format!("no Docker daemon on this node ({e})"),
        };
    }
    if docker(&["image", "inspect", &image]).is_err() {
        return SandboxCapability::Unavailable {
            detail: format!(
                "the job sandbox image {image} is not present — build it with \
                 deploy/docker/job-sandbox.Dockerfile or set NOOK_SANDBOX_IMAGE"
            ),
        };
    }
    SandboxCapability::Ready {
        image: format!("{image} ({})", isolation().as_str()),
    }
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

/// The addresses the egress policy lets through: the control plane, resolved
/// now, on this node.
///
/// BY ADDRESS (AC-5), which is the point — "it is private" is never the reason
/// a packet is allowed. A control plane on loopback (the dev stack) is reached
/// through the container's default gateway instead, since the container's own
/// loopback is not the host's.
pub fn control_plane_allow(server: &str) -> Vec<String> {
    let Some((host, port)) = host_and_port(server) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(addrs) = format!("{host}:{port}").to_socket_addrs() {
        for a in addrs {
            match a.ip() {
                // A loopback control plane is not reachable from inside at all;
                // the container reaches the host through its gateway, and the
                // run args add `host.docker.internal` for the name.
                IpAddr::V4(v4) if v4.is_loopback() => out.push(HOST_GATEWAY.into()),
                ip => out.push(ip.to_string()),
            }
        }
    }
    out.sort();
    out.dedup();
    out
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
            let spec = SandboxSpec {
                job_id: format!("escape-{}-{tag}", std::process::id()),
                image: image(),
                profile,
                isolation: isolation(),
                worktree: worktree.clone(),
                gitdir: None,
                claude_dir: None,
                caches: Vec::new(),
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
}
