//! Running a loop job as a Kubernetes Pod (MAIN-623, MAIN-655).
//!
//! The parallel of [`crate::sandbox`], for a cluster instead of a local Docker.
//! Everything that decides what a job may reach is a PURE FUNCTION here, for the
//! reason `sandbox::run_args` is one: the security argument should be something
//! a test can read, not something a reviewer has to trust.
//!
//! **What a kind gets is data, and it is the SAME data.** The profile table is
//! `sandbox::PROFILES` — not a second copy — so a new loop kind is one row that
//! answers for both executors at once. Two tables would be two answers to one
//! question, and the day they disagreed the disagreement would be silent.
//!
//! **The node's own credentials are never in a job Pod.** A `docker exec`
//! inherits nothing and this inherits nothing either: what the agent gets is
//! what is written here.

use nook_k8s::types::{
    Container, EmptyDirVolumeSource, EnvFromSource, EnvVar, ObjectMeta, Pod, PodSecurityContext,
    PodSpec, SecretEnvSource, SecretVolumeSource, SecurityContext, Toleration, Volume, VolumeMount,
};
use nook_types::{AuthProfile, AuthState};

// The label is `sandbox`'s (MAIN-617), not a second spelling of it. It marks
// "an object NookOS created for this job" and that claim is the same one
// whether the object is a container or a Pod -- two constants would be two
// answers to one question, silent on the day they drifted.
use crate::sandbox::{self, AGENT_HOME, CLAUDE_DIR, JOB_LABEL};

/// The container the agent runs in. One name, because the log follower and the
/// orphan sweep both address it.
pub const AGENT_CONTAINER: &str = "agent";

/// Which NODE created a Pod.
///
/// The Docker sweep can key on the job label alone because a Docker daemon is
/// implicitly one machine's. A NAMESPACE is not: `values.yaml` defaults every
/// install to `nook-jobs` and documents `replicas` as a knob, so two agents
/// sharing that namespace would each read the other's RUNNING job Pods as
/// orphans and delete them mid-run. This is what makes "mine" mean mine.
pub const NODE_LABEL: &str = "nook.node";

/// The volume the credential Secret arrives on. One name, because the volume
/// and the mount that reads it have to agree.
pub const CREDENTIALS_VOLUME: &str = "nook-credentials";

/// Where that Secret is mounted — a private path, and NOT [`CLAUDE_DIR`]
/// (MAIN-672).
///
/// `CLAUDE_CONFIG_DIR` is not a credential store: it is claude's read-write
/// working directory, and it holds `projects/`, `sessions/`,
/// `shell-snapshots/`, a refreshed `.credentials.json` and half a dozen other
/// things claude maintains. Mounting a Secret there — which the kubelet always
/// projects read-only, whatever the spec asks for — gave the agent a directory
/// it could not create a single file in, so no agent could run in a Pod at all.
/// So the Secret lands HERE, the seed of [`SESSION_VOLUME`], and the agent
/// never sees this path.
pub const CREDENTIALS_SEED_DIR: &str = "/nook-credentials";

/// The WRITABLE volume `CLAUDE_CONFIG_DIR` actually names (MAIN-672 AC-1).
///
/// An `emptyDir`, seeded from [`CREDENTIALS_SEED_DIR`] by [`pod_command`] before
/// the agent starts. It dies with the Pod, which is what keeps AC-3 true
/// without any Role change: a token claude refreshes into it is this Pod's
/// copy and has nowhere to travel back to. The Secret stays the snapshot it
/// was, and goes stale when its refresh token expires — see [`exit_refusal`]
/// and the chart README.
pub const SESSION_VOLUME: &str = "nook-session";

/// The mode every file in the credential volume is projected with (MAIN-669
/// AC-1).
///
/// Owner-read and nothing else. The Pod's command replaces the image's
/// entrypoint — the one that would have created the unprivileged `agent` uid —
/// so the agent here IS the container's root, which is the owner a Secret
/// volume's files already belong to. Anything with a group or other bit set
/// would hand the session to every other uid in the container for no gain.
pub const CREDENTIALS_MODE: i32 = 0o400;

/// The runtime a job Pod's credential Secret authorizes (MAIN-669 AC-6).
///
/// ONE runtime because the seed is one directory: it becomes [`CLAUDE_DIR`],
/// which is what `CLAUDE_CONFIG_DIR` names, and that is claude's configuration
/// directory and no other runtime's. A second runtime would need a second
/// volume, not a second entry here.
pub const CREDENTIALS_RUNTIME: &str = "claude";

/// What the seed step exits with when the session it was handed is already dead
/// (MAIN-672 AC-5).
///
/// 78 is `sysexits.h`'s `EX_CONFIG`, which is what this is: a human has to
/// re-seed the Secret, and no amount of retrying will do it for them.
pub const SESSION_EXPIRED_EXIT: i32 = 78;

/// The sentence the seed step prints before exiting with
/// [`SESSION_EXPIRED_EXIT`], and the one [`exit_refusal`] looks for.
///
/// Shared rather than written twice: the classifier requires BOTH the status
/// and this line, so that a `78` from anything else — claude's own exit codes
/// are not ours to reserve — is not read as an expired session.
pub const SESSION_EXPIRED_MARKER: &str = "nook: the seeded Claude session is expired";

/// Where a job Pod checks the workspace out.
///
/// Under [`AGENT_HOME`] rather than beside it: the Pod mounts no storage (see
/// [`job_pod`] — a credential Secret and the `emptyDir` seeded from it are all
/// it can ever mount), so the checkout is the container's own writable layer
/// and it belongs where the agent's other state does.
pub const POD_WORKDIR: &str = "/home/agent/workspace";

/// How long a Pod may take to reach `Running` before the job is handed back.
///
/// Generous, because it covers a cold image pull on a node that has never run
/// one. Bounded, because the alternative is a job that occupies an executor
/// slot forever while a cluster that will never schedule it stays silent —
/// which is `AtCapacity` starvation with no queue entry to see it in.
const START_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

/// How often the start and exit polls ask the apiserver.
const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a run keeps re-reading a Pod the apiserver will not answer for.
///
/// Much shorter than [`START_BUDGET`]: this one is spent with a job already
/// concluded and its transcript already on the card, so the only thing still
/// wanted is the exit code — and the result record is usually a better answer
/// anyway (see `AgentEnd::conclude`).
const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// How this node runs loop jobs, read once from the environment.
///
/// Opt-in and ALL-OR-NOTHING: `NOOK_EXECUTOR=kubernetes` turns it on, and every
/// other value it needs is then required. A half-configured executor is a loud
/// error rather than a node that joins, claims work and refuses each job in
/// turn — the same reasoning `parse_loop_kinds` uses for a mistyped kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorConfig {
    /// The namespace job Pods are created in — the one the Role is bound to.
    pub namespace: String,
    /// The image a job Pod runs.
    pub image: String,
    /// `None` where the cluster has no dedicated build pool, which is what
    /// makes a build refuse rather than land beside other work.
    pub build_pool: Option<BuildPool>,
    /// The Secret a job Pod reads its credentials from (AC-10, MAIN-669).
    ///
    /// SCAFFOLDING, and named as such: a human creates this Secret by hand, and
    /// nothing in this tree writes one — the executor's Role grants no `secrets`
    /// verb at all, so the node could not if it wanted to. MAIN-337/339 own the
    /// real credential path; until they land this is the whole of it.
    ///
    /// `None` means a job Pod gets no credentials. That is the DEFAULT, and it
    /// is a working state rather than a broken one — the node simply reports
    /// the loop runtime unauthorized (see [`delivered_runtime_auth`]) and is
    /// sent no loop work, instead of claiming jobs it cannot run.
    pub credentials_secret: Option<String>,
}

impl ExecutorConfig {
    /// `Ok(None)` means "not in Pod-executor mode", which is every host node and
    /// the containerised operator nodes that run jobs as local processes.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// The pure half, so the rules are testable without touching the process
    /// environment — which no test can change safely while others run.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let trimmed = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        match trimmed("NOOK_EXECUTOR").as_deref() {
            None => return Ok(None),
            Some("kubernetes") => {}
            Some(other) => {
                return Err(format!(
                    "NOOK_EXECUTOR: unknown executor {other:?} — expected \"kubernetes\""
                ))
            }
        }

        let required = |k: &str| {
            trimmed(k).ok_or_else(|| {
                format!("{k} is required when NOOK_EXECUTOR=kubernetes, and is unset")
            })
        };
        let namespace = required("NOOK_EXECUTOR_NAMESPACE")?;
        let image = required("NOOK_JOB_IMAGE")?;

        // The build pool is optional as a WHOLE and indivisible in its parts: a
        // selector without a taint puts builds on a pool nothing keeps other
        // work off, and a taint without a selector lets a build go anywhere. A
        // half-declared pool is the shape that reads as protection and is not.
        let selector = trimmed("NOOK_BUILD_POOL_SELECTOR");
        let taint = trimmed("NOOK_BUILD_POOL_TAINT");
        let build_pool = match (selector, taint) {
            (None, None) => None,
            (Some(sel), Some(taint_key)) => {
                let (selector_key, selector_value) = sel.split_once('=').ok_or_else(|| {
                    format!("NOOK_BUILD_POOL_SELECTOR must be key=value, got {sel:?}")
                })?;
                Some(BuildPool {
                    selector_key: selector_key.trim().to_string(),
                    selector_value: selector_value.trim().to_string(),
                    taint_key,
                })
            }
            (sel, _) => {
                let missing = if sel.is_some() {
                    "NOOK_BUILD_POOL_TAINT"
                } else {
                    "NOOK_BUILD_POOL_SELECTOR"
                };
                return Err(format!(
                    "a build pool needs both NOOK_BUILD_POOL_SELECTOR and \
                     NOOK_BUILD_POOL_TAINT; {missing} is unset. A selector alone puts builds \
                     on a pool nothing keeps other work off, and a taint alone lets a build \
                     schedule anywhere."
                ));
            }
        };

        Ok(Some(Self {
            namespace,
            image,
            build_pool,
            credentials_secret: trimmed("NOOK_JOB_CREDENTIALS_SECRET"),
        }))
    }

    /// What this node reports as its sandbox (AC-2).
    ///
    /// A node running in a Pod is already `Exempt` — its cgroup says so — but
    /// "pid 1 is in a container cgroup" describes the NODE and says nothing
    /// about what a job gets. This says what a job gets, which is the question
    /// `nook get nodes` is being asked.
    pub fn sandbox_detail(&self) -> String {
        let pool = match &self.build_pool {
            Some(p) => format!("; builds pinned to {}={}", p.selector_key, p.selector_value),
            None => String::from("; no build pool, so builds are refused"),
        };
        format!(
            "in-cluster executor: each job runs in its own Pod in {}{pool}",
            self.namespace
        )
    }

    /// Connect to the apiserver and build the executor this config describes.
    ///
    /// One place, because both callers — a run starting and the orphan sweep —
    /// have to agree about the namespace, the image and this node's own name,
    /// and two spellings of that would be two answers to one question.
    pub async fn executor(&self, node: String, server: String) -> nook_k8s::Result<PodExecutor> {
        let conn = nook_k8s::connect().await?;
        Ok(PodExecutor::new(
            nook_k8s::Pods::new(conn.client, &self.namespace),
            node,
            server,
            self.clone(),
        ))
    }
}

/// Where a build's Pods are allowed to run (MAIN-655 AC-4).
///
/// A build Pod is privileged, so it must never share a node with anything else.
/// The taint is what keeps other work OFF those nodes; the selector is what
/// keeps builds ON them. Both are needed and neither implies the other: a
/// toleration alone permits a build anywhere, and a selector alone lets other
/// workloads onto the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPool {
    pub selector_key: String,
    pub selector_value: String,
    pub taint_key: String,
}

/// Everything the builder needs. No cluster, no client, no I/O.
#[derive(Debug, Clone)]
pub struct PodJobSpec {
    pub job_id: String,
    /// This node's name, under [`NODE_LABEL`] — what makes the orphan sweep's
    /// "mine" mean mine in a namespace two agents may share.
    pub node: String,
    /// The loop kind, which selects the profile. Never branched on directly.
    pub kind: String,
    pub image: String,
    /// The control plane, as spelled from inside the cluster.
    pub server: String,
    /// The job's own variables, written after the base contract so a job cannot
    /// quietly redefine `NOOK_SANDBOX`.
    ///
    /// **No credential may be in here** — these become literal `value:` fields
    /// on the Pod, which `get pods` returns and `kubectl describe pod` prints,
    /// and this executor's own Role grants `pods get/list/watch` in the
    /// namespace. Credentials come through [`Self::credentials_secret`], which
    /// at least keeps them behind `get secrets`. `loop_job::pod_env` is what
    /// holds them back, and a test pins it.
    pub env: Vec<(String, String)>,
    /// A hand-created Secret, read as environment variables and copied into the
    /// writable [`CLAUDE_DIR`] before the agent starts (AC-10, MAIN-669,
    /// MAIN-672). `None` delivers no credentials at all.
    pub credentials_secret: Option<String>,
    /// What the container runs — the agent runtime, its flags, and the opening
    /// turn among them.
    ///
    /// An ARGUMENT rather than a write to stdin, and that is forced rather than
    /// chosen: the host path sends the skill command through the child's stdin
    /// pipe, and a Pod has no such pipe without `pods/attach`, which the Role
    /// deliberately does not grant. Empty leaves the image's own entrypoint
    /// alone.
    pub command: Vec<String>,
    /// `None` on a cluster with no dedicated build pool — which is why
    /// [`job_pod`] refuses a build rather than scheduling one loose.
    pub build_pool: Option<BuildPool>,
}

/// Why a Pod could not be described. Refusals, not failures: the caller turns
/// these into `JobRefused`, which leaves the card's strike budget alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A build with nowhere safe to put it. Scheduling it on the general pool
    /// would put a privileged container beside ordinary workloads, which is the
    /// one thing the pool exists to prevent.
    NoBuildPool,
    /// The cluster is full, or the apiserver briefly blinked. Clears itself, so
    /// the job waits in the queue and the node keeps claiming.
    Transient(String),
    /// A human has to change something — an absent Role, a missing namespace,
    /// unusable credentials. Retrying cannot fix it, so the node stops claiming
    /// rather than refusing every job it is handed in turn.
    Configuration(String),
}

impl Refusal {
    /// Should the node keep claiming loop work after this?
    ///
    /// The distinction is the whole reason these are two variants. A quota
    /// clears on its own and the queue is the right place to wait; a missing
    /// Role never clears, and a node that keeps claiming into one becomes a
    /// black hole that refuses in turn everything the dispatcher sends it.
    pub fn keep_claiming(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBuildPool => write!(
                f,
                "this executor has no dedicated build pool configured, and a build Pod is \
                 privileged — it will not be scheduled beside other work"
            ),
            Self::Transient(m) | Self::Configuration(m) => write!(f, "{m}"),
        }
    }
}

/// Which kind of refusal a failed create is (AC-7).
///
/// **A Pod that never started cannot have failed the card.** Every reason a
/// create fails here is the cluster's or the deployment's — a quota, an absent
/// Role, an unreachable apiserver — and none of them is evidence about the
/// work. Failing would spend a strike on the card for the cluster's state,
/// which is the mistake `QueuedReason::SandboxUnavailable` already exists to
/// avoid on host nodes.
///
/// Total over the error type rather than matching a few variants and defaulting
/// the rest, because that default would decide silently for every variant added
/// later. `Configuration` is the safe fall-through: it stops the node claiming,
/// which is fail-closed, the way an unavailable sandbox is.
pub fn refusal_for(err: &nook_k8s::Error) -> Refusal {
    use nook_k8s::Error as E;
    let message = err.to_string();
    match err {
        E::QuotaExceeded { .. } | E::Unreachable { .. } => Refusal::Transient(message),
        // The apiserver's own back-pressure. `429` is what it returns when its
        // priority-and-fairness queues are full and `503` when it is starting
        // or losing quorum; both are the textbook thing to retry, and reading
        // them as configuration would cordon a node for a blip nobody has to
        // fix. Every OTHER code keeps the fail-closed fall-through below.
        E::Api {
            code: 429 | 500 | 502 | 503 | 504,
            ..
        } => Refusal::Transient(message),
        _ => Refusal::Configuration(message),
    }
}

/// Which Pods the orphan sweep will even LOOK at (AC-8).
///
/// A pure function because it is the whole of the node-scoping argument, and a
/// selector built inline is one no test can read. A namespace is SHARED — the
/// chart defaults every install to `nook-jobs` and documents `replicas` as a
/// knob — so a bare `nook.job` selector returns a sibling agent's Pods, and
/// [`orphans`] would then report every one of that sibling's RUNNING jobs as a
/// leftover and delete it mid-run.
///
/// The narrowing has to be HERE rather than in `orphans`: this is what the
/// apiserver filters on, so a Pod belonging to another node is never fetched at
/// all and cannot be mistaken for anything.
pub fn sweep_selector(node: &str) -> String {
    format!("{JOB_LABEL},{NODE_LABEL}={}", label_value(node))
}

/// The Pods this executor owns that no live job accounts for (AC-8).
///
/// Pure, and separate from the deleting, because the decision is the
/// interesting half: an agent that restarts mid-run must RECONCILE, not clean
/// up. A Pod whose job is still live is a run this node is holding — deleting
/// it would kill work the control plane believes is in flight — and a Pod whose
/// job is not is the leftover of a run that ended while nobody was listening.
///
/// A Pod carrying no job label is left ALONE. This namespace may hold objects
/// this executor did not create, and the label is what marks the ones it did;
/// sweeping unlabelled Pods would make somebody else's workload its business.
pub fn orphans<'a>(
    pods: impl IntoIterator<Item = &'a Pod>,
    live: &std::collections::HashSet<String>,
) -> Vec<String> {
    pods.into_iter()
        .filter_map(|pod| {
            let job = pod.metadata.labels.as_ref()?.get(JOB_LABEL)?;
            let name = pod.metadata.name.as_ref()?;
            (!live.contains(job)).then(|| name.clone())
        })
        .collect()
}

/// The Pod's name.
///
/// Deliberately NOT [`sandbox::container_name`], which is the same idea for
/// Docker: Docker accepts `_` and any length, a DNS-1123 label accepts neither,
/// so sharing one function would mean the stricter engine silently deciding the
/// looser one's names. Folded rather than rejected — a job whose id has an
/// underscore should run, not fail on a naming rule.
pub fn pod_name(job_id: &str) -> String {
    label_value(&format!("nook-job-{job_id}"))
}

/// Fold an arbitrary string into something a DNS-1123 label accepts.
///
/// Folded rather than rejected, everywhere it is used: a node whose name has a
/// dot and a job whose id has an underscore should both run, not fail on a
/// naming rule they had no say in.
fn label_value(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect();
    out.truncate(63);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The `node -e` program the seed step checks the session's life with.
///
/// **Node.js is the base image's contract, not a new dependency**: `claude` is
/// an npm package installed by `operator-node.Dockerfile`, so a job image that
/// can run the agent can run this. It is still guarded by `command -v` below —
/// a check that cannot run must not refuse a job it knows nothing about.
///
/// Only `refreshTokenExpiresAt` is consulted, and that is the whole subtlety of
/// AC-5. An expired ACCESS token is the ordinary state of a seeded session and
/// claude renews it unprompted, which is exactly what AC-1's writable directory
/// is for; an expired REFRESH token is the one there is no renewing, because
/// the credential that would buy a new pair is itself dead.
///
/// A file it cannot parse, or one with no OAuth block, exits 0: a shape we do
/// not recognise is not evidence, and letting claude try and say so beats
/// refusing on a guess.
///
/// Double quotes throughout, deliberately — the program is embedded in a
/// single-quoted shell word, so one apostrophe in it would end the string.
const SESSION_LIFE_CHECK: &str = concat!(
    r#"var o;try{o=JSON.parse(require("fs").readFileSync(process.env.CLAUDE_CONFIG_DIR"#,
    r#"+"/.credentials.json","utf8")).claudeAiOauth}catch(e){process.exit(0)}"#,
    r#"if(!o||!o.refreshTokenExpiresAt||o.refreshTokenExpiresAt>Date.now())process.exit(0);"#,
    r#"console.error("nook: the seeded Claude session is expired: its refresh token "#,
    r#"expired at "+new Date(o.refreshTokenExpiresAt).toISOString()+", so nothing in "#,
    r#"this Pod can renew it. Re-seed the credentials Secret from a machine where "#,
    r#"claude is logged in.");process.exit(1)"#,
);

/// What a job Pod runs: seed the session, check the workspace out, then become
/// the agent.
///
/// The clone is HERE and not on the node, and that is forced rather than
/// chosen. A Pod mounts no storage (see [`job_pod`]), so the node's clone cache
/// and its per-job worktree — the whole first half of `loop_job::run` — are simply
/// not reachable from inside one. NG-2 settles what to do instead: every job Pod
/// starts from a fresh clone, with no PVC and no warm state to reuse.
///
/// **The seed step is what makes `CLAUDE_CONFIG_DIR` usable** (MAIN-672 AC-1).
/// A Secret volume is read-only however it is asked for, and claude's
/// configuration directory is somewhere it writes constantly, so the Secret is
/// mounted out of the way at [`CREDENTIALS_SEED_DIR`] and copied into the
/// `emptyDir` at [`CLAUDE_DIR`] before the agent starts. Emitted only when a
/// Secret is configured, which is the same condition [`job_pod`] mounts the two
/// volumes on — a test drives both from one flag, because a Pod that mounted a
/// seed nothing copied would be the empty directory MAIN-669 already fixed once.
///
/// `exec` on the last line, so the agent is pid 1's own process: without it the
/// shell stays as the container's root process and a `SIGTERM` from the delete
/// reaches the shell rather than the agent.
///
/// `--depth 1` because a loop job reads one commit's worth of tree. A `build`
/// would want the history, and a build cannot be placed here (AC-9).
pub fn pod_command(launch: &AgentLaunch<'_>) -> Vec<String> {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let mut script = String::from("set -e\n");
    if launch.seeded_session {
        // The Secret's KEYS and nothing else. `*` misses the dotfiles that are
        // the entire payload (`.claude.json`, `.credentials.json`) and `.*`
        // would drag in the `..data` symlink and the timestamped directory
        // behind it — a second copy of the credential, in the writable
        // directory, that nothing would ever refresh. `.[!.]*` is the one glob
        // that means "hidden, but not a `..` name".
        //
        // `-L` because every entry in a Secret volume is a symlink into
        // `..data`; copying the links would leave the agent a directory of
        // dangling pointers.
        script.push_str(&format!(
            "mkdir -p {claude}\nfor f in {seed}/* {seed}/.[!.]*; do\n  \
             [ -f \"$f\" ] || continue\n  cp -L \"$f\" {claude}/\ndone\n",
            claude = quote(CLAUDE_DIR),
            seed = quote(CREDENTIALS_SEED_DIR),
        ));
        // The copy inherits the Secret's 0400, and claude has to WRITE
        // `.credentials.json` back when it refreshes the pair. Owner only: the
        // point of [`CREDENTIALS_MODE`] was never the write bit.
        script.push_str(&format!("chmod -R u+rwX {}\n", quote(CLAUDE_DIR)));
        // AC-5. A session whose refresh token has expired will fail every job
        // this Pod is given, so the job is REFUSED here rather than failed
        // several turns later out of the card's strike budget.
        script.push_str(&format!(
            "if command -v node >/dev/null 2>&1; then\n  node -e '{}' || exit {}\nfi\n",
            SESSION_LIFE_CHECK, SESSION_EXPIRED_EXIT,
        ));
    }
    if !launch.repo_url.is_empty() {
        script.push_str(&format!(
            "git clone --depth 1 --branch {} {} {}\n",
            quote(launch.branch),
            quote(launch.repo_url),
            quote(POD_WORKDIR),
        ));
    } else {
        script.push_str(&format!("mkdir -p {}\n", quote(POD_WORKDIR)));
    }
    script.push_str(&format!("cd {}\n", quote(POD_WORKDIR)));
    script.push_str("exec ");
    script.push_str(&quote(launch.runtime));
    for arg in launch.args {
        script.push(' ');
        script.push_str(&quote(arg));
    }
    script.push('\n');
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

/// The agent launch a Pod is built around — the runtime, its argv (the opening
/// turn among them), and the checkout to make first.
#[derive(Debug, Clone, Copy)]
pub struct AgentLaunch<'a> {
    pub runtime: &'a str,
    pub args: &'a [String],
    /// Empty for a run with no repository to clone, which leaves the agent an
    /// empty working directory rather than a failed container.
    pub repo_url: &'a str,
    pub branch: &'a str,
    /// Whether a credential Secret is configured, and so whether the command
    /// begins by seeding [`CLAUDE_DIR`] from it (MAIN-672 AC-1).
    ///
    /// The SAME `Option::is_some` that decides the Pod's volumes. Said twice
    /// because the command and the Pod are built by different callers, and
    /// pinned by a test so the two spellings cannot come apart.
    pub seeded_session: bool,
}

/// Whether a Pod has got going yet, and whether it ever will.
///
/// Pure, because this is the whole of AC-7 that a create call cannot see: a
/// quota rejection comes back from `create`, but an image that does not exist
/// and a pool nothing can schedule onto are both accepted at create and only
/// admitted to afterwards, in the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartVerdict {
    /// The container is running, or has already run.
    Started,
    /// Still scheduling or pulling. Nothing is wrong yet.
    Waiting,
    /// It will not start. The job goes back to the queue (AC-7).
    Refused(Refusal),
}

/// Reasons a container is `Waiting` for that mean it will not stop waiting.
///
/// An `ErrImagePull` is deliberately absent: the FIRST pull failure can be a
/// registry blip, and Kubernetes says `ImagePullBackOff` once it has retried —
/// which is the point at which a human has to look at it.
const HOPELESS_WAITING: &[&str] = &[
    "ImagePullBackOff",
    "InvalidImageName",
    "ErrImageNeverPull",
    "CreateContainerConfigError",
    "CreateContainerError",
];

pub fn start_verdict(pod: &Pod) -> StartVerdict {
    let Some(status) = pod.status.as_ref() else {
        return StartVerdict::Waiting;
    };
    if matches!(
        status.phase.as_deref(),
        Some("Running" | "Succeeded" | "Failed")
    ) {
        return StartVerdict::Started;
    }
    for cs in status.container_statuses.iter().flatten() {
        let Some(waiting) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) else {
            continue;
        };
        let reason = waiting.reason.as_deref().unwrap_or_default();
        if HOPELESS_WAITING.contains(&reason) {
            return StartVerdict::Refused(Refusal::Configuration(format!(
                "the job Pod will not start: {reason}{}",
                waiting
                    .message
                    .as_deref()
                    .map(|m| format!(" — {m}"))
                    .unwrap_or_default()
            )));
        }
    }
    // Nothing will schedule it. A cluster that is full clears itself, so this
    // is the shortage the queue is for rather than a deployment to fix.
    for cond in status.conditions.iter().flatten() {
        if cond.type_ == "PodScheduled"
            && cond.status == "False"
            && cond.reason.as_deref() == Some("Unschedulable")
        {
            return StartVerdict::Refused(Refusal::Transient(format!(
                "no node can take this job Pod: {}",
                cond.message.as_deref().unwrap_or("Unschedulable")
            )));
        }
    }
    StartVerdict::Waiting
}

/// What the agent container exited with, once it has.
///
/// `None` while it is still running, which is what the exit poll waits out. A
/// Pod in a terminal phase with no container status left to read is reported as
/// a failure rather than a success: the run's own result record is the evidence
/// of success, and this is only the fallback for when none arrived.
pub fn exit_code(pod: &Pod) -> Option<i32> {
    let status = pod.status.as_ref()?;
    for cs in status.container_statuses.iter().flatten() {
        if cs.name != AGENT_CONTAINER {
            continue;
        }
        if let Some(t) = cs.state.as_ref().and_then(|s| s.terminated.as_ref()) {
            return Some(t.exit_code);
        }
    }
    match status.phase.as_deref() {
        Some("Succeeded") => Some(0),
        Some("Failed") => Some(-1),
        _ => None,
    }
}

/// The environment contract, identical to the Docker sandbox's
/// ([`sandbox`]'s `base_env`) plus the job id.
///
/// Written by VALUE, never forwarded by name: the process that builds this Pod
/// is the node, and the node's own environment is not the job's.
fn base_env(spec: &PodJobSpec) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), AGENT_HOME.to_string()),
        ("CLAUDE_CONFIG_DIR".to_string(), CLAUDE_DIR.to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("NOOK_SANDBOX".to_string(), "1".to_string()),
        ("NOOK_JOB_ID".to_string(), spec.job_id.clone()),
    ];
    if !spec.server.is_empty() {
        env.push(("NOOK_SERVER".to_string(), spec.server.clone()));
    }
    env
}

/// The credential Secret and the writable directory seeded from it, or nothing
/// (MAIN-669 AC-1/AC-2, MAIN-672 AC-1).
///
/// **A subscription Claude session is a DIRECTORY, not an environment
/// variable** — `.claude.json` beside `.credentials.json` — which is why the
/// Docker sandbox bind-mounts one at `CLAUDE_CONFIG_DIR`. A Pod had no
/// equivalent, so `claude` found an empty directory, the node reported the
/// runtime unauthorized, and every job for it stayed queued forever.
///
/// **TWO volumes, because a Secret cannot be the agent's config directory.**
/// MAIN-669 mounted the Secret straight at [`CLAUDE_DIR`], and a Secret volume
/// is read-only whatever the spec asks for — so the agent got 0400 files in a
/// directory it could not add `projects/`, `sessions/` or a refreshed
/// `.credentials.json` to, and no agent could start. The Secret therefore
/// mounts read-only and out of sight at [`CREDENTIALS_SEED_DIR`], and
/// [`pod_command`] copies it into the `emptyDir` the agent is pointed at. The
/// Secret's own mount is still read-only; it is the COPY that is writable,
/// which is what keeps the refreshed pair inside the Pod (AC-3).
///
/// **`optional: true` on the Secret volume, while the `env_from` beside it stays
/// `optional: false`, and the pairing is the whole of MAIN-669's AC-4.** A
/// missing Secret has to REFUSE the job by name; the two seams fail differently
/// and only one of them says anything a caller can read. A failed Secret *mount*
/// leaves the Pod in `ContainerCreating` with the reason in an Event — a resource
/// the executor's Role deliberately cannot read (NG-3) — so the run would sit
/// there until [`START_BUDGET`] expired ten minutes later and be handed back as a
/// generic timeout. A missing `env_from` Secret is `CreateContainerConfigError`
/// within seconds, which [`start_verdict`] already reads as a configuration
/// refusal carrying the apiserver's own words. So the env seam is the DETECTOR
/// and the volume is the DELIVERY, and neither is redundant.
fn credential_volumes(secret: &str) -> (Vec<Volume>, Vec<VolumeMount>) {
    (
        vec![
            Volume {
                name: CREDENTIALS_VOLUME.to_string(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(secret.to_string()),
                    default_mode: Some(CREDENTIALS_MODE),
                    optional: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Volume {
                name: SESSION_VOLUME.to_string(),
                // Node-local and Pod-lifetime. Not a PVC: a session that
                // outlived its Pod would be a credential store this ticket does
                // not build (NG-1), and every Pod re-seeds from the Secret in
                // under a second anyway.
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            },
        ],
        vec![
            VolumeMount {
                name: CREDENTIALS_VOLUME.to_string(),
                mount_path: CREDENTIALS_SEED_DIR.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: SESSION_VOLUME.to_string(),
                mount_path: CLAUDE_DIR.to_string(),
                // Writable, and matching what the Docker sandbox does with the
                // same path (`sandbox::run_args` binds it with no `:ro`). The
                // two adapters are meant to give an agent the same world, and
                // this is the one property that decides whether it starts.
                read_only: Some(false),
                ..Default::default()
            },
        ],
    )
}

/// Whether a concluded Pod refused the job rather than failing it (MAIN-672
/// AC-5).
///
/// **A seeded session that has already expired is the cluster's state, not the
/// card's**, exactly as a missing Secret is — so it goes back to the queue with
/// the strike budget untouched, the way `QueuedReason::SandboxUnavailable` does
/// on a host node. `Configuration`, not `Transient`: a refresh token does not
/// come back by waiting, and a node that kept claiming into one would refuse in
/// turn everything the dispatcher sent it.
///
/// BOTH the status and the marker line are required. [`SESSION_EXPIRED_EXIT`]
/// is `EX_CONFIG`, which is a status claude is free to use for its own reasons;
/// the seed step runs before the agent does, so when it is ours the marker is
/// the only output there is.
pub fn exit_refusal(code: Option<i32>, tail: &str) -> Option<Refusal> {
    if code != Some(SESSION_EXPIRED_EXIT) {
        return None;
    }
    let said = tail
        .lines()
        .find(|l| l.contains(SESSION_EXPIRED_MARKER))?
        .trim();
    Some(Refusal::Configuration(said.to_string()))
}

/// What this node reports for the runtimes a job Pod's Secret carries
/// (MAIN-669 AC-6).
///
/// **A probe on the node answers the wrong question here.** `probe_all` runs
/// `claude auth status` in the agent's own pod, which holds no session and
/// generally not even the binary, so it reports `Unavailable` — and the control
/// plane's `runtime_authorized` gate then refuses to place any loop job on this
/// node, leaving every one of them queued with *"no eligible executor"*. What
/// that gate is asking is whether a job started HERE can authenticate, and
/// under a Pod executor the Secret answers it, not the node.
///
/// Only with a Secret configured. Without one the probed state stands, so a
/// cluster node nobody has given credentials is passed over rather than
/// claiming work it would fail in turn.
pub fn delivered_runtime_auth(
    profiles: Vec<AuthProfile>,
    cfg: &ExecutorConfig,
) -> Vec<AuthProfile> {
    let Some(secret) = cfg.credentials_secret.as_deref() else {
        return profiles;
    };
    profiles
        .into_iter()
        .map(|p| {
            if p.runtime != CREDENTIALS_RUNTIME {
                return p;
            }
            AuthProfile {
                state: AuthState::Authorized,
                // A terminal on THIS node would sign in a container that runs
                // no jobs. The credential has to arrive by delivery, so the UI
                // must offer the device flow instead of a session (MAIN-650).
                device_flow: true,
                // Not an account: this node cannot read the Secret and has no
                // way to learn whose session is in it. Saying where the
                // credential came from is the true thing available, and it is
                // what an operator looking at an authorized cluster node with
                // no login of its own needs to see.
                identity: Some(format!("from Secret {secret}")),
                ..p
            }
        })
        .collect()
}

/// Land a delivered credential in the Secret job Pods read (MAIN-650).
///
/// On a host node a delivered credential goes to a FILE, and the agent that
/// reads it runs on that machine. Neither half is true here: the agent is a Pod
/// somewhere else in the cluster, and the only thing it reads is the Secret this
/// node names. So the same delivery has to end somewhere else, or authorizing a
/// cluster node writes a file that nothing will ever open — which is exactly
/// what it did, and why seeding this Secret was a `kubectl` step an operator had
/// to perform with credentials they obtained some other way.
///
/// The payload stays opaque, as it is on the file path: what is decided here is
/// the destination and the KEY, never the contents. The key is
/// `runtime_auth::credential_file`'s, so the name in the Secret is the name the
/// runtime will look for once it is projected into a Pod.
///
/// Returns where it went, for the delivery report an operator reads.
pub async fn deliver_credential_to_secret(runtime: &str, payload: &[u8]) -> anyhow::Result<String> {
    let cfg = ExecutorConfig::from_env()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("this node is not a Pod executor"))?;
    let secret = cfg.credentials_secret.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "this node names no executor.credentialsSecret, so a delivered credential has \
             nowhere to go that a job would ever read"
        )
    })?;
    let key = crate::runtime_auth::credential_file(runtime)
        .ok_or_else(|| anyhow::anyhow!("no credential layout is known for runtime `{runtime}`"))?;

    let conn = nook_k8s::connect().await?;
    let creds = nook_k8s::Credentials::new(conn.client, &cfg.namespace, secret);
    creds
        .upsert(std::collections::BTreeMap::from([(
            key.to_string(),
            payload.to_vec(),
        )]))
        .await?;
    Ok(format!("Secret {}/{} key {key}", cfg.namespace, secret))
}

/// Describe the Pod this job runs in.
///
/// The three properties worth reading off the result, because they are the
/// whole security argument:
///
/// 1. **No host path, for any kind.** The only `volumes` entries a Pod can ever
///    have are [`credential_volumes`]' pair — the credential Secret and the
///    `emptyDir` seeded from it — and only when a Secret is configured. A
///    container holding the host's Docker socket can start a privileged sibling
///    and undo everything else, and the way to never do that is for `hostPath`
///    to appear nowhere this function can reach.
/// 2. **Privilege only where the profile declares a nested daemon**, which is
///    `build` alone. Every other kind gets `allowPrivilegeEscalation: false`.
/// 3. **A privileged Pod is pinned to the build pool**, or refused.
pub fn job_pod(spec: &PodJobSpec) -> Result<Pod, Refusal> {
    let profile = sandbox::profile_for(&spec.kind);

    // The nested daemon is the only thing that varies by kind, and it is what
    // privilege is FOR. Reading it from the shared table rather than matching on
    // the kind is what makes "a new kind adds a row" true here too.
    let privileged = profile.nested_docker;

    let pool = match (privileged, &spec.build_pool) {
        (true, None) => return Err(Refusal::NoBuildPool),
        (true, Some(p)) => Some(p),
        (false, _) => None,
    };

    let mut env: Vec<(String, String)> = base_env(spec);
    env.extend(spec.env.iter().cloned());

    // Built together from one Option so the seams cannot drift apart: the
    // volumes alone would deliver a session and go silent when the Secret is
    // absent, and the env alone is what a Pod had before MAIN-669 — no session
    // at all. The same Option is what `pod_command` seeds on. See
    // [`credential_volumes`].
    let credentials = spec.credentials_secret.as_deref().map(credential_volumes);

    let container = Container {
        name: AGENT_CONTAINER.to_string(),
        image: Some(spec.image.clone()),
        command: (!spec.command.is_empty()).then(|| spec.command.clone()),
        env: Some(
            env.into_iter()
                .map(|(name, value)| EnvVar {
                    name,
                    value: Some(value),
                    value_from: None,
                })
                .collect(),
        ),
        // The credential seam (AC-10), and the ORDER matters: Kubernetes applies
        // `env_from` first and lets `env` override it, so a Secret key colliding
        // with `NOOK_JOB_ID` or `NOOK_SERVER` loses. A hand-created Secret is
        // scaffolding, and scaffolding must not be able to redirect a run at the
        // wrong control plane.
        //
        // `optional: false` deliberately, and it is what makes AC-4 hold for the
        // MOUNT as well: a named Secret that does not exist keeps the Pod in
        // `CreateContainerConfigError`, which `start_verdict` reads as a
        // configuration refusal naming it — far better than an agent that
        // starts, silently has no credentials, and burns a pass finding out.
        // Keys a Secret carries for the session rather than the environment
        // (`.claude.json`) are not legal variable names; the kubelet skips
        // those with an event and starts the container.
        env_from: spec.credentials_secret.as_ref().map(|name| {
            vec![EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: name.clone(),
                    optional: Some(false),
                }),
                ..Default::default()
            }]
        }),
        volume_mounts: credentials.as_ref().map(|(_, m)| m.clone()),
        security_context: Some(SecurityContext {
            privileged: Some(privileged),
            // Belt and braces with `privileged`, which already implies it: a
            // future edit that drops privilege must not silently leave this
            // permissive.
            allow_privilege_escalation: Some(privileged),
            ..Default::default()
        }),
        ..Default::default()
    };

    let pod_spec = PodSpec {
        containers: vec![container],
        volumes: credentials.map(|(v, _)| v),
        // A loop job runs once. Restarting it here would re-run an agent the
        // control plane already believes finished, against a card whose state
        // has moved on.
        restart_policy: Some("Never".to_string()),
        node_selector: pool.map(|p| {
            std::collections::BTreeMap::from([(p.selector_key.clone(), p.selector_value.clone())])
        }),
        tolerations: pool.map(|p| {
            vec![Toleration {
                key: Some(p.taint_key.clone()),
                operator: Some("Exists".to_string()),
                effect: Some("NoSchedule".to_string()),
                ..Default::default()
            }]
        }),
        security_context: Some(PodSecurityContext::default()),
        ..Default::default()
    };

    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(pod_name(&spec.job_id)),
            labels: Some(std::collections::BTreeMap::from([
                (JOB_LABEL.to_string(), spec.job_id.clone()),
                (NODE_LABEL.to_string(), label_value(&spec.node)),
                (
                    "app.kubernetes.io/managed-by".to_string(),
                    "nook".to_string(),
                ),
            ])),
            ..Default::default()
        },
        spec: Some(pod_spec),
        status: None,
    })
}

/// A blocking [`std::io::Read`] fed from somewhere else — the shape
/// `pump_events` needs, over a stream that only exists asynchronously.
///
/// The two halves of a Pod run disagree about colour: the log stream is an
/// `AsyncBufRead` and the event pump is an ordinary blocking reader on the run
/// thread. Rather than make the pump async — which would spread `async` through
/// `loop_job`, the most synchronous code in the node, for one caller — the
/// bytes cross once, here.
///
/// A closed channel is EOF, not an error. The run ends when the Pod's output
/// ends, and that is the ordinary way for it to finish.
pub struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    /// What arrived but did not fit in the caller's buffer. `Read` may be asked
    /// for one byte at a time and a chunk from the network is whatever size the
    /// network chose, so the two have to be decoupled.
    rest: Vec<u8>,
}

impl ChannelReader {
    pub fn new(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            rest: Vec::new(),
        }
    }
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.rest.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.rest = chunk,
                // Every sender is gone: the stream is over. `Ok(0)` is EOF,
                // which is what ends `BufRead::lines` cleanly.
                Err(_) => return Ok(0),
            }
        }
        let n = self.rest.len().min(buf.len());
        buf[..n].copy_from_slice(&self.rest[..n]);
        self.rest.drain(..n);
        Ok(n)
    }
}

/// Reconcile this node's job Pods, from a caller that must not block.
///
/// Fire-and-forget, and that is the requirement rather than a shortcut: the
/// callers are the node's reconnect path and its ten-minute tick, and the node
/// socket has ONE reader — awaiting cluster I/O in either would freeze every
/// terminal on the machine, which is the failure `RescanWorkspaces` already
/// carries a comment about.
///
/// Silent when this node is not a Pod executor, which is almost every node.
pub fn spawn_orphan_sweep(node: String, live: std::collections::HashSet<String>) {
    let Ok(Some(cfg)) = ExecutorConfig::from_env() else {
        return;
    };
    // No runtime means no cluster call to make. `try_current` rather than
    // `current`, because the latter panics and a housekeeping sweep must never
    // be the thing that takes a node down.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        match cfg.executor(node, String::new()).await {
            Ok(exec) => {
                exec.sweep_orphans(&live).await;
            }
            Err(e) => tracing::warn!(error = %e, "cannot reach the cluster to reconcile job Pods"),
        }
    });
}

/// The cluster-side half: create the Pod, follow it, and make sure it is gone.
///
/// Thin on purpose. Every decision worth arguing about is one of the pure
/// functions above, so what is left here is the I/O that cannot be unit-tested
/// without a cluster — and there is deliberately not much of it.
pub struct PodExecutor {
    pods: nook_k8s::Pods,
    node: String,
    server: String,
    /// The whole executor configuration rather than a copy of three of its
    /// fields: a Pod's image, its build pool and its credential Secret are one
    /// decision, and unpacking them here is how the copy drifts from the source.
    cfg: ExecutorConfig,
}

impl PodExecutor {
    pub fn new(pods: nook_k8s::Pods, node: String, server: String, cfg: ExecutorConfig) -> Self {
        Self {
            pods,
            node,
            server,
            cfg,
        }
    }

    /// Start a job's Pod, or say why not.
    ///
    /// Every failure path returns a [`Refusal`] — see [`refusal_for`] for why a
    /// create failure is never the card's fault.
    pub async fn start(
        &self,
        job_id: &str,
        kind: &str,
        env: Vec<(String, String)>,
        command: Vec<String>,
    ) -> Result<String, Refusal> {
        let pod = job_pod(&PodJobSpec {
            job_id: job_id.to_string(),
            node: self.node.clone(),
            kind: kind.to_string(),
            image: self.cfg.image.clone(),
            server: self.server.clone(),
            env,
            command,
            build_pool: self.cfg.build_pool.clone(),
            credentials_secret: self.cfg.credentials_secret.clone(),
        })?;
        let name = pod
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| pod_name(job_id));
        self.pods.create(&pod).await.map_err(|e| refusal_for(&e))?;
        Ok(name)
    }

    /// Wait until the Pod's container is running, or hand the job back.
    ///
    /// Between `create` and this, the two failures AC-7 names that a create
    /// call cannot see: an image that will not pull, and a pool nothing can
    /// schedule onto. Both are refusals — a Pod that never ran cannot have
    /// failed the card — and [`start_verdict`] is where the reading lives.
    pub async fn await_start(&self, name: &str) -> Result<(), Refusal> {
        let deadline = std::time::Instant::now() + START_BUDGET;
        loop {
            let pod = self.pods.get(name).await.map_err(|e| refusal_for(&e))?;
            match start_verdict(&pod) {
                StartVerdict::Started => return Ok(()),
                StartVerdict::Refused(r) => return Err(r),
                StartVerdict::Waiting => {}
            }
            if std::time::Instant::now() >= deadline {
                // Transient: a cluster busy enough to take ten minutes over a
                // Pod is a shortage, and the queue is where a job waits one out.
                return Err(Refusal::Transient(format!(
                    "the job Pod did not start within {}s",
                    START_BUDGET.as_secs()
                )));
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// Wait for the agent container to exit, and report its status.
    ///
    /// `None` where the apiserver stopped answering: the run's own result
    /// record is the evidence of an outcome, and this is only the crash-honesty
    /// fallback for when none arrived.
    pub async fn await_exit(&self, name: &str) -> Option<i32> {
        // A blip is RIDDEN OUT rather than given up on, the way `await_start`
        // rides one out. Returning `None` on the first failed read turns an
        // apiserver hiccup into "the agent died without an exit status" and a
        // reported failure, for a run that may well have succeeded — and this
        // poll runs at exactly the moment a run is ending, so the window is
        // small but it is the expensive one to be wrong in.
        let mut give_up_at: Option<std::time::Instant> = None;
        loop {
            match self.pods.get(name).await {
                Ok(pod) => {
                    give_up_at = None;
                    if let Some(code) = exit_code(&pod) {
                        return Some(code);
                    }
                }
                // Deleted underneath us — a cancel, or somebody's kubectl. There
                // is no status left to read and never will be, so this one does
                // not retry.
                Err(nook_k8s::Error::NotFound { .. }) => return None,
                Err(e) => {
                    let deadline =
                        *give_up_at.get_or_insert_with(|| std::time::Instant::now() + READ_BUDGET);
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            pod = %name, error = %e,
                            "gave up reading a job Pod's exit status"
                        );
                        return None;
                    }
                    tracing::debug!(pod = %name, error = %e, "retrying a job Pod's status");
                }
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// Follow a Pod's output as something [`job_adapter::pump_events`] can read.
    ///
    /// Spawns the reading and hands back the other end of a channel, so the run
    /// thread blocks on bytes rather than on a runtime. The task ends when the
    /// stream does, and dropping the sender is what the reader sees as EOF —
    /// so a Pod that exits, a stream that breaks and a cancelled run all end
    /// the pump the same way, rather than only the tidy one doing so.
    pub async fn follow(&self, name: &str) -> nook_k8s::Result<ChannelReader> {
        use futures_util::AsyncBufReadExt;

        let stream = self.pods.follow_logs(name).await?;
        let (tx, rx) = std::sync::mpsc::channel();
        let pod = name.to_string();
        tokio::spawn(async move {
            let mut lines = stream.lines();
            while let Some(line) = futures_util::StreamExt::next(&mut lines).await {
                match line {
                    // The newline goes back on: `lines()` ate it, and the event
                    // pump splits on it.
                    Ok(mut l) => {
                        l.push('\n');
                        if tx.send(l.into_bytes()).is_err() {
                            // The run stopped reading. Nothing to report -- it
                            // concluded, and the Pod is being removed.
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(pod = %pod, error = %e, "job Pod log stream ended early");
                        return;
                    }
                }
            }
        });
        Ok(ChannelReader::new(rx))
    }

    /// Remove a job's Pod. Idempotent by intent: a Pod already gone is the
    /// state this asks for, so a `NotFound` is success rather than an error to
    /// report up an exit path that is often itself a failure path (AC-5).
    pub async fn stop(&self, job_id: &str) {
        let name = pod_name(job_id);
        // `Pods::delete` already reads an absent Pod as success, so there is no
        // `NotFound` arm here — one guarantee, in one place.
        if let Err(e) = self.pods.delete(&name).await {
            tracing::warn!(pod = %name, error = %e, "could not delete a job Pod");
        }
    }

    /// Reconcile this executor's Pods against the jobs it is actually running
    /// (AC-8), and return the ones it removed.
    ///
    /// Called on reconnect, where the node already reconciles tmux sessions and
    /// held worktrees. The decision is [`orphans`]; this is the doing.
    pub async fn sweep_orphans(&self, live: &std::collections::HashSet<String>) -> Vec<String> {
        let pods = match self.pods.list_labelled(&sweep_selector(&self.node)).await {
            Ok(pods) => pods,
            Err(e) => {
                tracing::warn!(error = %e, "could not list job Pods to reconcile");
                return Vec::new();
            }
        };
        let mut removed = Vec::new();
        for name in orphans(&pods, live) {
            match self.pods.delete(&name).await {
                Ok(()) => removed.push(name),
                Err(e) => tracing::warn!(pod = %name, error = %e, "could not delete an orphan"),
            }
        }
        if !removed.is_empty() {
            tracing::info!(count = removed.len(), "removed job Pods with no live job");
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: &str) -> PodJobSpec {
        PodJobSpec {
            job_id: "0198f0aa-1111-7000-8000-abcdefabcdef".into(),
            node: "azul".into(),
            kind: kind.into(),
            image: "ghcr.io/nook-os/job-sandbox:test".into(),
            server: "https://control.nook.svc:8080".into(),
            env: vec![("NOOK_JOB_SEED".into(), "do the thing".into())],
            command: vec!["claude".into(), "-p".into(), "/nook-review MAIN-1".into()],
            build_pool: Some(BuildPool {
                selector_key: "nook.io/pool".into(),
                selector_value: "build".into(),
                taint_key: "nook.io/build-only".into(),
            }),
            credentials_secret: None,
        }
    }

    fn container(pod: &Pod) -> &Container {
        &pod.spec.as_ref().unwrap().containers[0]
    }

    /// By NAME rather than by index: a Pod now carries two volumes, and a test
    /// that reads `[0]` asserts an ordering nothing promises.
    fn volume<'a>(pod: &'a Pod, name: &str) -> &'a Volume {
        pod.spec
            .as_ref()
            .unwrap()
            .volumes
            .as_ref()
            .and_then(|vs| vs.iter().find(|v| v.name == name))
            .unwrap_or_else(|| panic!("no volume named {name}"))
    }

    fn mount<'a>(pod: &'a Pod, name: &str) -> &'a VolumeMount {
        container(pod)
            .volume_mounts
            .as_ref()
            .and_then(|ms| ms.iter().find(|m| m.name == name))
            .unwrap_or_else(|| panic!("nothing mounts the volume {name}"))
    }

    fn env_of(pod: &Pod) -> std::collections::BTreeMap<String, String> {
        container(pod)
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect()
    }

    /// The contract a job agent is written against, and it is the SAME one the
    /// Docker sandbox provides — an agent must not be able to tell which
    /// executor started it.
    #[test]
    fn the_environment_contract_matches_the_docker_sandbox() {
        let env = env_of(&job_pod(&spec("review")).unwrap());
        assert_eq!(env["HOME"], AGENT_HOME);
        assert_eq!(env["CLAUDE_CONFIG_DIR"], CLAUDE_DIR);
        assert_eq!(env["NOOK_SANDBOX"], "1");
        assert_eq!(env["NOOK_SERVER"], "https://control.nook.svc:8080");
        assert_eq!(env["NOOK_JOB_ID"], "0198f0aa-1111-7000-8000-abcdefabcdef");
        // The job's own variables reach it too.
        assert_eq!(env["NOOK_JOB_SEED"], "do the thing");
    }

    /// An empty server says nothing rather than saying nothing usefully: the
    /// escape suite attacks the box without talking to a board, and a
    /// `NOOK_SERVER=` would have an agent try to reach the empty string.
    #[test]
    fn an_empty_server_is_omitted_rather_than_written_blank() {
        let mut s = spec("spec");
        s.server = String::new();
        assert!(!env_of(&job_pod(&s).unwrap()).contains_key("NOOK_SERVER"));
    }

    const KINDS: &[&str] = &[
        "build",
        "review",
        "spec",
        "decompose",
        "epic-run",
        "investigate",
    ];

    /// The property the whole design rests on. A container holding the host's
    /// Docker socket can start a privileged sibling and undo every other
    /// control, so no kind gets one — and with no Secret configured the way
    /// that is guaranteed is still to mount nothing at all (MAIN-669 AC-2).
    #[test]
    fn no_kind_gets_a_host_socket_or_any_host_path() {
        for kind in KINDS {
            let pod = job_pod(&spec(kind)).unwrap();
            let p = pod.spec.as_ref().unwrap();
            assert!(p.volumes.is_none(), "{kind} has volumes");
            assert!(container(&pod).volume_mounts.is_none(), "{kind} mounts");
            assert_ne!(p.host_network, Some(true), "{kind} on the host network");
            assert_ne!(p.host_pid, Some(true), "{kind} in the host PID namespace");
        }
    }

    /// …and with one configured, the credential Secret and the writable copy
    /// seeded from it are the ONLY things that may appear (MAIN-669 AC-2,
    /// MAIN-672 AC-1). A `hostPath` is what the assertion above really
    /// excludes, and the mounts arriving must not become the door it comes back
    /// through.
    #[test]
    fn a_configured_secret_and_its_copy_are_the_only_volumes_any_kind_may_have() {
        for kind in KINDS {
            let mut s = spec(kind);
            s.credentials_secret = Some("nook-job-credentials".into());
            let pod = job_pod(&s).unwrap();
            let p = pod.spec.as_ref().unwrap();

            let volumes = p.volumes.as_ref().unwrap_or_else(|| panic!("{kind}"));
            assert_eq!(volumes.len(), 2, "{kind}: not the Secret and its copy");
            assert!(
                volumes.iter().all(|v| v.host_path.is_none()),
                "{kind} mounts a host path"
            );
            assert!(
                volumes
                    .iter()
                    .all(|v| v.secret.is_some() || v.empty_dir.is_some()),
                "{kind} mounts something that is neither the Secret nor its copy"
            );

            let mounts = container(&pod).volume_mounts.as_ref().unwrap();
            assert_eq!(mounts.len(), 2, "{kind} mounts more than those two");
            assert_ne!(p.host_network, Some(true), "{kind} on the host network");
            assert_ne!(p.host_pid, Some(true), "{kind} in the host PID namespace");
        }
    }

    /// MAIN-672 AC-1 and AC-7, and the correction of what this test used to
    /// assert.
    ///
    /// It asserted `read_only: Some(true)` on the directory the AGENT is
    /// pointed at, and that is the defect: `CLAUDE_CONFIG_DIR` is not a
    /// credential store but claude's read-WRITE working directory — it creates
    /// `projects/`, `sessions/` and `shell-snapshots/` in it and rewrites
    /// `.credentials.json` there every time it refreshes the OAuth pair. A
    /// Secret volume is read-only whatever the spec asks for, so nothing could
    /// start. The Secret is still read-only, at a private path of its own; what
    /// `CLAUDE_CONFIG_DIR` names is the writable copy.
    #[test]
    fn the_secret_is_read_only_and_what_claude_is_pointed_at_is_writable() {
        let mut s = spec("spec");
        s.credentials_secret = Some("nook-job-credentials".into());
        let pod = job_pod(&s).unwrap();

        let seed = volume(&pod, CREDENTIALS_VOLUME);
        let source = seed.secret.as_ref().expect("a Secret volume source");
        assert_eq!(source.secret_name.as_deref(), Some("nook-job-credentials"));
        // Owner-read and nothing else: a session readable by every uid in the
        // container is one an unprivileged process in it could copy out.
        assert_eq!(source.default_mode, Some(0o400));
        let mode = source.default_mode.unwrap();
        assert_eq!(mode & 0o007, 0, "world-readable: {mode:o}");
        assert_eq!(mode & 0o070, 0, "group-readable: {mode:o}");

        let seed_mount = mount(&pod, CREDENTIALS_VOLUME);
        assert_eq!(
            seed_mount.read_only,
            Some(true),
            "the Secret became writable"
        );
        assert_eq!(seed_mount.mount_path, CREDENTIALS_SEED_DIR);
        assert_ne!(
            seed_mount.mount_path, CLAUDE_DIR,
            "the Secret is back over the agent's own directory"
        );

        let session = volume(&pod, SESSION_VOLUME);
        assert!(session.empty_dir.is_some(), "the copy is not an emptyDir");
        assert!(session.secret.is_none(), "the copy is the Secret again");
        let session_mount = mount(&pod, SESSION_VOLUME);
        assert_ne!(
            session_mount.read_only,
            Some(true),
            "claude cannot write its own configuration directory"
        );
        // The mount path and the variable are ONE decision. Asserted against
        // the env rather than against the constant, because an agent reads the
        // variable and a mount somewhere else would leave it looking at an
        // empty directory — the exact failure MAIN-669 exists to end.
        assert_eq!(session_mount.mount_path, CLAUDE_DIR);
        assert_eq!(env_of(&pod)["CLAUDE_CONFIG_DIR"], session_mount.mount_path);
    }

    /// The seams are one decision, and MAIN-669's AC-4 rests on their pairing:
    /// the `env_from` is the DETECTOR — a missing Secret there is
    /// `CreateContainerConfigError` in seconds — and the volume is the
    /// DELIVERY, optional because a failed Secret MOUNT reports itself only in
    /// an Event, which this executor's Role cannot read, and would otherwise
    /// hang the run until the start budget expired ten minutes later.
    ///
    /// MAIN-672 AC-7 adds the third: the delivery is only a delivery once
    /// [`pod_command`] has copied it somewhere writable, so an edit that drops
    /// any of the three fails here rather than in a cluster.
    #[test]
    fn the_secret_is_delivered_as_files_and_detected_as_environment() {
        let mut s = spec("review");
        s.credentials_secret = Some("nook-job-credentials".into());
        let pod = job_pod(&s).unwrap();

        let secret_ref = container(&pod).env_from.as_ref().expect("an env_from")[0]
            .secret_ref
            .clone()
            .expect("a secret ref");
        assert_eq!(secret_ref.name, "nook-job-credentials");
        assert_eq!(secret_ref.optional, Some(false), "the detector went quiet");

        let source = volume(&pod, CREDENTIALS_VOLUME).secret.as_ref().unwrap();
        assert_eq!(source.optional, Some(true), "a failed mount hangs the run");
        assert_eq!(
            source.secret_name.as_deref(),
            Some(secret_ref.name.as_str())
        );

        // And with none configured, no half appears.
        let pod = job_pod(&spec("review")).unwrap();
        assert!(container(&pod).env_from.is_none());
        assert!(pod.spec.as_ref().unwrap().volumes.is_none());
    }

    /// MAIN-672 AC-1. The Pod that mounts a seed is the Pod whose command
    /// copies it, and the two are built by different callers from the same
    /// `Option` — so this drives both from one flag. A seed nothing copied
    /// would leave the agent the empty `CLAUDE_CONFIG_DIR` MAIN-669 already
    /// fixed once.
    #[test]
    fn the_pod_that_mounts_a_seed_is_the_pod_whose_command_copies_it() {
        for configured in [false, true] {
            let mut s = spec("spec");
            s.credentials_secret = configured.then(|| "nook-job-credentials".to_string());
            let pod = job_pod(&s).unwrap();
            let mounted = pod.spec.as_ref().unwrap().volumes.is_some();

            let args = ["-p".to_string(), "/nook-spec MAIN-1".to_string()];
            let script = pod_command(&AgentLaunch {
                runtime: "claude",
                args: &args,
                repo_url: "https://git.example/acme.git",
                branch: "main",
                seeded_session: s.credentials_secret.is_some(),
            })
            .pop()
            .unwrap();
            let copies = script.contains(CREDENTIALS_SEED_DIR);

            assert_eq!(mounted, configured, "the mount disagreed");
            assert_eq!(copies, configured, "the copy disagreed");
            // The seed is read BEFORE the clone and the `exec`: a session that
            // arrives after the agent has started is one it never sees.
            if configured {
                let seed = script.find(CREDENTIALS_SEED_DIR).unwrap();
                assert!(seed < script.find("git clone").unwrap(), "{script}");
                assert!(seed < script.find("exec ").unwrap(), "{script}");
                // Copied, never symlinked back: a link into a read-only Secret
                // is a writable directory the writes still fail in.
                assert!(script.contains("cp -L"), "{script}");
                assert!(!script.contains("ln -s"), "{script}");
                // The `..data` symlink and the timestamped directory behind it
                // are the Secret volume's own plumbing; copying them would put
                // a second, never-refreshed credential in the writable copy.
                assert!(script.contains(".[!.]*"), "dotfiles unmatched: {script}");
                assert!(
                    !script.contains("/.*"),
                    "`..data` would be copied: {script}"
                );
            }
        }
    }

    /// MAIN-672 AC-5. A seeded session whose REFRESH token has expired will
    /// fail every job this Pod is ever handed, so it is a refusal — the
    /// cluster's state, like a missing Secret, and not the card's. Failing
    /// would spend a strike from a budget that exists to catch an agent which
    /// cannot do the work.
    #[test]
    fn an_expired_seeded_session_refuses_the_job_rather_than_failing_it() {
        let said = format!(
            "{SESSION_EXPIRED_MARKER}: its refresh token expired at \
             2026-08-29T14:06:12.389Z, so nothing in this Pod can renew it."
        );
        let refusal = exit_refusal(Some(SESSION_EXPIRED_EXIT), &said).expect("a refusal");
        // Configuration, not transient: a refresh token does not come back by
        // waiting, and a node that kept claiming into one would refuse in turn
        // everything the dispatcher sent it.
        assert!(
            !refusal.keep_claiming(),
            "waiting cannot renew a dead token"
        );
        // Naming the expiry is the point — an operator has to know the Secret
        // is stale, not merely that something went wrong.
        assert!(refusal.to_string().contains("2026-08-29"), "{refusal}");

        // An ordinary agent failure is still a failure. `EX_CONFIG` is not ours
        // to reserve, so the status alone must not hand a card back.
        assert_eq!(
            exit_refusal(Some(SESSION_EXPIRED_EXIT), "claude: bad flag"),
            None
        );
        assert_eq!(exit_refusal(Some(1), &said), None);
        assert_eq!(exit_refusal(Some(0), &said), None);
        assert_eq!(exit_refusal(None, &said), None);
    }

    /// MAIN-672 AC-6. The two executors are meant to give an agent the same
    /// world, and this is the one property that decides whether it starts —
    /// they diverged on it invisibly until a live container was inspected, so
    /// the agreement is asserted rather than described.
    #[test]
    fn both_executors_give_the_agent_a_writable_claude_config_dir() {
        let mut s = spec("spec");
        s.credentials_secret = Some("nook-job-credentials".into());
        let pod = job_pod(&s).unwrap();
        assert_ne!(mount(&pod, SESSION_VOLUME).read_only, Some(true));

        let docker = sandbox::run_args(&sandbox::SandboxSpec {
            job_id: "job-1".into(),
            image: "nook-job-sandbox:latest".into(),
            profile: sandbox::profile_for("spec"),
            isolation: sandbox::Isolation::Unprivileged,
            worktree: std::path::PathBuf::from("/cache/worktrees/build-1"),
            gitdir: None,
            claude_dir: Some(std::path::PathBuf::from("/home/ryan/.nook-secrets/claude")),
            caches: Vec::new(),
            references: Vec::new(),
            ports: Vec::new(),
            allow: Vec::new(),
            add_hosts: Vec::new(),
            server: String::new(),
            agent_uid: 1000,
            agent_gid: 1000,
        });
        let claude_bind = docker
            .iter()
            .find(|a| {
                a.ends_with(&format!(":{CLAUDE_DIR}")) || a.contains(&format!(":{CLAUDE_DIR}:"))
            })
            .unwrap_or_else(|| {
                panic!("the docker sandbox stopped mounting {CLAUDE_DIR}: {docker:?}")
            });
        assert!(
            !claude_bind.ends_with(":ro"),
            "the docker sandbox made {CLAUDE_DIR} read-only; the Pod did not: {claude_bind}"
        );
    }

    /// AC-6. The gate `nook get nodes` reports and the dispatcher enforces asks
    /// whether a job started here can authenticate. A probe inside the agent's
    /// own pod answers a different question — it has no session and often no
    /// binary — and answering that one is why a cluster node claimed nothing.
    #[test]
    fn a_cluster_node_reports_the_authorization_its_job_pods_will_have() {
        let signed_out = || {
            vec![
                AuthProfile {
                    id: "claude".into(),
                    label: "Claude Code".into(),
                    runtime: "claude".into(),
                    state: AuthState::Unavailable,
                    identity: None,
                    device_flow: false,
                },
                AuthProfile {
                    id: "codex".into(),
                    label: "Codex CLI".into(),
                    runtime: "codex".into(),
                    state: AuthState::Unavailable,
                    identity: None,
                    device_flow: false,
                },
            ]
        };
        let mut cfg = ExecutorConfig {
            namespace: "nook-jobs".into(),
            image: "img:1".into(),
            build_pool: None,
            credentials_secret: None,
        };

        // No Secret: the probe stands, so the dispatcher passes this node over
        // rather than sending it work it would fail in turn.
        let reported = delivered_runtime_auth(signed_out(), &cfg);
        assert!(reported.iter().all(|p| p.state != AuthState::Authorized));

        cfg.credentials_secret = Some("nook-job-credentials".into());
        let reported = delivered_runtime_auth(signed_out(), &cfg);
        let claude = reported
            .iter()
            .find(|p| p.runtime == CREDENTIALS_RUNTIME)
            .expect("the loop runtime");
        assert_eq!(claude.state, AuthState::Authorized);
        // Where it came from, not who it is: this node cannot read the Secret.
        assert_eq!(
            claude.identity.as_deref(),
            Some("from Secret nook-job-credentials")
        );

        // One mount is one runtime's configuration directory. A Secret at
        // CLAUDE_CONFIG_DIR says nothing about codex, and claiming otherwise
        // would offer a runtime the Pod cannot authenticate.
        let codex = reported.iter().find(|p| p.runtime == "codex").unwrap();
        assert_eq!(codex.state, AuthState::Unavailable);
    }

    /// Privilege follows the nested daemon, and the nested daemon is declared by
    /// exactly one profile row. Asserted for every kind, so adding one that
    /// quietly turns it on fails here.
    #[test]
    fn only_the_kind_that_declares_a_daemon_is_privileged() {
        for kind in ["review", "spec", "decompose", "epic-run", "investigate"] {
            let pod = job_pod(&spec(kind)).unwrap();
            let sc = container(&pod).security_context.as_ref().unwrap();
            assert_eq!(sc.privileged, Some(false), "{kind} is privileged");
            assert_eq!(sc.allow_privilege_escalation, Some(false), "{kind}");
        }
        let pod = job_pod(&spec("build")).unwrap();
        let sc = container(&pod).security_context.as_ref().unwrap();
        assert_eq!(sc.privileged, Some(true), "build needs its nested daemon");
    }

    /// A privileged Pod must not land beside ordinary work. The selector puts
    /// builds ON the pool and the toleration is what lets them past the taint
    /// that keeps everything else OFF it; neither alone is sufficient.
    #[test]
    fn a_build_is_pinned_to_its_pool_and_nothing_else_is() {
        let pod = job_pod(&spec("build")).unwrap();
        let p = pod.spec.as_ref().unwrap();
        assert_eq!(
            p.node_selector.as_ref().unwrap()["nook.io/pool"],
            "build",
            "a build must be kept ON the pool"
        );
        let tol = &p.tolerations.as_ref().unwrap()[0];
        assert_eq!(tol.key.as_deref(), Some("nook.io/build-only"));
        assert_eq!(tol.effect.as_deref(), Some("NoSchedule"));

        // Every other kind carries neither, so it can never be scheduled onto
        // the pool a privileged container is running on.
        for kind in ["review", "spec", "investigate"] {
            let pod = job_pod(&spec(kind)).unwrap();
            let p = pod.spec.as_ref().unwrap();
            assert!(p.node_selector.is_none(), "{kind} selects a pool");
            assert!(p.tolerations.is_none(), "{kind} tolerates a taint");
        }
    }

    /// With nowhere safe to put it, a build is REFUSED rather than scheduled
    /// loose. Refused and not failed: a cluster that has not been given a build
    /// pool is a configuration gap, and spending a card's strike budget on it
    /// would blame the card for the cluster.
    #[test]
    fn a_build_with_no_pool_is_refused_and_the_others_still_run() {
        let mut s = spec("build");
        s.build_pool = None;
        assert_eq!(job_pod(&s), Err(Refusal::NoBuildPool));
        assert!(Refusal::NoBuildPool.to_string().contains("privileged"));

        // The absence of a pool says nothing about the unprivileged kinds.
        let mut s = spec("review");
        s.build_pool = None;
        assert!(job_pod(&s).is_ok());
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    /// Opt-in, and unset is the overwhelmingly common case: every host node and
    /// every containerised operator that runs jobs as local processes.
    #[test]
    fn no_executor_variable_means_this_is_not_a_pod_executor() {
        assert_eq!(ExecutorConfig::from_vars(vars(&[])), Ok(None));
        // A typo must not read as "off" -- that would silently keep a node in
        // the mode the operator was trying to leave.
        let err = ExecutorConfig::from_vars(vars(&[("NOOK_EXECUTOR", "k8s")])).unwrap_err();
        assert!(err.contains("unknown executor"), "{err}");
    }

    /// All-or-nothing: a half-configured executor is a loud error, not a node
    /// that joins and then refuses each job it is handed.
    #[test]
    fn the_executor_needs_a_namespace_and_an_image() {
        let err = ExecutorConfig::from_vars(vars(&[("NOOK_EXECUTOR", "kubernetes")])).unwrap_err();
        assert!(err.contains("NOOK_EXECUTOR_NAMESPACE"), "{err}");

        let err = ExecutorConfig::from_vars(vars(&[
            ("NOOK_EXECUTOR", "kubernetes"),
            ("NOOK_EXECUTOR_NAMESPACE", "nook-jobs"),
        ]))
        .unwrap_err();
        assert!(err.contains("NOOK_JOB_IMAGE"), "{err}");
    }

    /// The dangerous shape, and the reason the pool is indivisible: either half
    /// alone READS as protection and is not. A selector with no taint puts
    /// builds on a pool nothing keeps other work off; a taint with no selector
    /// lets a build schedule anywhere at all.
    #[test]
    fn half_a_build_pool_is_refused_because_it_looks_like_protection() {
        let base = [
            ("NOOK_EXECUTOR", "kubernetes"),
            ("NOOK_EXECUTOR_NAMESPACE", "nook-jobs"),
            ("NOOK_JOB_IMAGE", "ghcr.io/nook-os/job-sandbox:1"),
        ];

        let mut only_selector = base.to_vec();
        only_selector.push(("NOOK_BUILD_POOL_SELECTOR", "nook.io/pool=build"));
        let err = ExecutorConfig::from_vars(vars(&only_selector)).unwrap_err();
        assert!(err.contains("NOOK_BUILD_POOL_TAINT"), "{err}");

        let mut only_taint = base.to_vec();
        only_taint.push(("NOOK_BUILD_POOL_TAINT", "nook.io/build-only"));
        let err = ExecutorConfig::from_vars(vars(&only_taint)).unwrap_err();
        assert!(err.contains("NOOK_BUILD_POOL_SELECTOR"), "{err}");

        // Neither is fine -- it means "this cluster runs no builds", and a build
        // is then refused by job_pod rather than mis-scheduled.
        let cfg = ExecutorConfig::from_vars(vars(&base)).unwrap().unwrap();
        assert_eq!(cfg.build_pool, None);
        assert!(cfg.sandbox_detail().contains("builds are refused"));

        // Both, and the selector must actually be key=value.
        let mut both = base.to_vec();
        both.push(("NOOK_BUILD_POOL_SELECTOR", "nook.io/pool=build"));
        both.push(("NOOK_BUILD_POOL_TAINT", "nook.io/build-only"));
        let cfg = ExecutorConfig::from_vars(vars(&both)).unwrap().unwrap();
        assert_eq!(
            cfg.build_pool,
            Some(BuildPool {
                selector_key: "nook.io/pool".into(),
                selector_value: "build".into(),
                taint_key: "nook.io/build-only".into(),
            })
        );

        let mut malformed = base.to_vec();
        malformed.push(("NOOK_BUILD_POOL_SELECTOR", "nook.io/pool"));
        malformed.push(("NOOK_BUILD_POOL_TAINT", "nook.io/build-only"));
        assert!(ExecutorConfig::from_vars(vars(&malformed))
            .unwrap_err()
            .contains("key=value"));
    }

    /// AC-2. The sandbox column is asked what a JOB gets; "pid 1 is in a
    /// container cgroup" answers a question about the node instead.
    #[test]
    fn the_reported_detail_describes_the_job_not_the_node() {
        let cfg = ExecutorConfig::from_vars(vars(&[
            ("NOOK_EXECUTOR", "kubernetes"),
            ("NOOK_EXECUTOR_NAMESPACE", "nook-jobs"),
            ("NOOK_JOB_IMAGE", "img:1"),
            ("NOOK_BUILD_POOL_SELECTOR", "nook.io/pool=build"),
            ("NOOK_BUILD_POOL_TAINT", "nook.io/build-only"),
        ]))
        .unwrap()
        .unwrap();
        let detail = cfg.sandbox_detail();
        assert!(detail.contains("its own Pod"), "{detail}");
        assert!(detail.contains("nook-jobs"), "{detail}");
        assert!(detail.contains("nook.io/pool=build"), "{detail}");
    }

    /// With no stdin pipe, the opening turn has to reach the agent some other
    /// way, and the only way left is the command line. Asserted because the
    /// failure is silent: a Pod that starts with the image's default entrypoint
    /// runs an agent that was never told what to do.
    #[test]
    fn the_opening_turn_travels_as_an_argument_not_on_stdin() {
        let pod = job_pod(&spec("review")).unwrap();
        let cmd = container(&pod).command.as_ref().expect("a command");
        assert_eq!(cmd[0], "claude");
        assert!(cmd.iter().any(|a| a.contains("/nook-review")), "{cmd:?}");

        // Empty leaves the image's entrypoint alone rather than running "".
        let mut s = spec("review");
        s.command = Vec::new();
        assert!(container(&job_pod(&s).unwrap()).command.is_none());
    }

    /// The bridge the event pump reads through. Chunk boundaries are the
    /// network's business and line boundaries are the protocol's, so the two
    /// must not have to agree.
    #[test]
    fn the_channel_reader_decouples_chunks_from_reads() {
        use std::io::{BufRead, BufReader, Read};

        let (tx, rx) = std::sync::mpsc::channel();
        // One event split across chunks, and two events sharing one.
        tx.send(b"{\"a\":1}\n{\"b\"".to_vec()).unwrap();
        tx.send(b":2}\n".to_vec()).unwrap();
        drop(tx);

        let lines: Vec<String> = BufReader::new(ChannelReader::new(rx))
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);

        // A one-byte buffer must still make progress rather than spin or lose
        // the remainder of a chunk.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(b"xy".to_vec()).unwrap();
        drop(tx);
        let mut r = ChannelReader::new(rx);
        let mut one = [0u8; 1];
        assert_eq!(r.read(&mut one).unwrap(), 1);
        assert_eq!(one[0], b'x');
        assert_eq!(r.read(&mut one).unwrap(), 1);
        assert_eq!(one[0], b'y');
        // Senders gone and nothing buffered: EOF, not an error.
        assert_eq!(r.read(&mut one).unwrap(), 0);
    }

    fn op() -> Box<nook_k8s::Operation> {
        Box::new(nook_k8s::Operation::new("create", "pods", "nook-jobs"))
    }

    /// AC-7. A Pod that never started cannot have failed the card, so every
    /// create failure refuses. What differs is whether the node should keep
    /// claiming: a quota clears itself, a missing Role never does.
    #[test]
    fn a_create_failure_refuses_and_says_whether_to_keep_claiming() {
        let full = nook_k8s::Error::QuotaExceeded {
            operation: op(),
            message: "exceeded quota".into(),
        };
        assert!(matches!(refusal_for(&full), Refusal::Transient(_)));
        assert!(refusal_for(&full).keep_claiming(), "a quota clears itself");

        let blinked = nook_k8s::Error::Unreachable {
            operation: op(),
            message: "connection refused".into(),
        };
        assert!(refusal_for(&blinked).keep_claiming());

        // A Role that does not grant the verb never fixes itself, and a node
        // that keeps claiming into one refuses in turn everything it is sent.
        let rbac = nook_k8s::Error::Forbidden {
            operation: op(),
            message: "pods is forbidden".into(),
        };
        assert!(matches!(refusal_for(&rbac), Refusal::Configuration(_)));
        assert!(!refusal_for(&rbac).keep_claiming());

        // The apiserver's own back-pressure. `429` is a full priority-and-
        // fairness queue and `503` an apiserver starting or losing quorum;
        // reading either as configuration would cordon a node for a blip
        // nobody has to fix.
        for code in [429, 500, 502, 503, 504] {
            let busy = nook_k8s::Error::Api {
                operation: op(),
                code,
                reason: "TooManyRequests".into(),
                message: "please try again later".into(),
            };
            assert!(refusal_for(&busy).keep_claiming(), "{code} cordoned a node");
        }
        // …and every OTHER code keeps the fail-closed reading. A `409` is a
        // name collision this executor cannot resolve by waiting.
        let conflict = nook_k8s::Error::Api {
            operation: op(),
            code: 409,
            reason: "AlreadyExists".into(),
            message: "pods \"nook-job-1\" already exists".into(),
        };
        assert!(!refusal_for(&conflict).keep_claiming());

        // The fall-through is the fail-closed one: an error nobody has
        // classified is not one to assume will clear.
        let unknown = nook_k8s::Error::NoCredentials;
        assert!(!refusal_for(&unknown).keep_claiming());

        // Whatever the verdict, the operator gets the apiserver's own words.
        assert!(refusal_for(&full).to_string().contains("exceeded quota"));
    }

    /// AC-8. An agent that restarts mid-run RECONCILES rather than cleans up.
    #[test]
    fn the_sweep_keeps_live_jobs_and_never_touches_what_it_does_not_own() {
        let mine = |job: &str, name: &str| {
            let mut p = job_pod(&spec("review")).unwrap();
            p.metadata.name = Some(name.to_string());
            p.metadata.labels = Some(std::collections::BTreeMap::from([(
                JOB_LABEL.to_string(),
                job.to_string(),
            )]));
            p
        };
        // Somebody else's workload, in the same namespace, with no job label.
        let mut stranger = job_pod(&spec("review")).unwrap();
        stranger.metadata.name = Some("someone-elses-pod".into());
        stranger.metadata.labels = None;

        let pods = vec![
            mine("live-job", "pod-a"),
            mine("ended-job", "pod-b"),
            stranger,
        ];
        let live = std::collections::HashSet::from(["live-job".to_string()]);

        // Only the Pod whose job ended. The live one is work the control plane
        // believes is in flight, and the unlabelled one is not ours to judge.
        assert_eq!(orphans(&pods, &live), vec!["pod-b".to_string()]);

        // Nothing live: every Pod this executor owns is a leftover, and the
        // stranger is STILL untouched.
        assert_eq!(
            orphans(&pods, &std::collections::HashSet::new()),
            vec!["pod-a".to_string(), "pod-b".to_string()]
        );
    }

    /// The label is how the executor finds its own Pods again after a restart
    /// (AC-8), so it carries the job id verbatim — the NAME is normalised for
    /// Kubernetes, and the two must not be confused for one another.
    #[test]
    fn the_label_carries_the_job_id_and_the_name_is_a_legal_dns_label() {
        let pod = job_pod(&spec("review")).unwrap();
        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(labels[JOB_LABEL], "0198f0aa-1111-7000-8000-abcdefabcdef");

        // A run-once Pod: restarting it would re-run an agent the control plane
        // already concluded, against a card that has moved on.
        assert_eq!(
            pod.spec.as_ref().unwrap().restart_policy.as_deref(),
            Some("Never")
        );

        // Normalisation folds rather than rejects: an id should not be able to
        // fail a job on a naming rule.
        let n = pod_name("AB_cd/ef.");
        assert_eq!(n, "nook-job-ab-cd-ef");
        assert!(n.len() <= 63 && !n.ends_with('-'));
        assert!(n
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }

    /// A namespace is SHARED — `values.yaml` defaults every install to
    /// `nook-jobs` — so "a Pod carrying `nook.job`" does not mean "a Pod of
    /// mine". Without this label two agents in one namespace would each read
    /// the other's RUNNING job Pods as orphans and delete them mid-run.
    #[test]
    fn a_pod_says_which_node_made_it_so_a_sweep_cannot_take_a_siblings_work() {
        let labels = job_pod(&spec("review")).unwrap().metadata.labels.unwrap();
        assert_eq!(labels[NODE_LABEL], "azul");

        // A node name is a human's, not a DNS label's; it is folded rather than
        // rejected, exactly as a job id is.
        let mut s = spec("review");
        s.node = "Azul.local".into();
        let labels = job_pod(&s).unwrap().metadata.labels.unwrap();
        assert_eq!(labels[NODE_LABEL], "azul-local");
    }

    /// A Pod mounts no storage, so the node's clone cache and its per-job
    /// worktree are unreachable from inside one: the checkout happens IN the
    /// Pod, from a fresh clone (NG-2).
    #[test]
    fn the_pod_clones_its_own_checkout_and_then_becomes_the_agent() {
        let args = ["-p".to_string(), "/nook-spec MAIN-1".to_string()];
        let script = pod_command(&AgentLaunch {
            runtime: "claude",
            args: &args,
            repo_url: "https://github.com/nook-os/nook-os.git",
            branch: "main",
            seeded_session: false,
        });
        assert_eq!(script[0], "/bin/sh");
        assert_eq!(script[1], "-c");
        let body = &script[2];
        assert!(body.contains("git clone"), "{body}");
        assert!(body.contains("'main'"), "{body}");
        assert!(body.contains(POD_WORKDIR), "{body}");
        // `exec`, so the agent becomes the container's own process: without it
        // the delete's SIGTERM reaches the shell and not the agent.
        assert!(body.contains("\nexec 'claude'"), "{body}");
        assert!(body.contains("'/nook-spec MAIN-1'"), "{body}");

        // A repo-less run gets an empty working directory rather than a
        // container that dies in its first line.
        let bare = pod_command(&AgentLaunch {
            runtime: "claude",
            args: &args,
            repo_url: "",
            branch: "main",
            seeded_session: false,
        });
        assert!(!bare[2].contains("git clone"), "{}", bare[2]);
        assert!(bare[2].contains("mkdir -p"), "{}", bare[2]);
    }

    /// The command is assembled into a SHELL script, so anything that reaches
    /// it unquoted is an injection: a branch name is attacker-influenced (a card
    /// names it) and the opening turn carries a card's own seed text.
    #[test]
    fn every_word_of_the_command_is_quoted_against_the_shell() {
        let args = ["; rm -rf /".to_string()];
        let script = pod_command(&AgentLaunch {
            runtime: "claude",
            args: &args,
            repo_url: "https://example.invalid/r.git",
            branch: "'; touch /pwned; '",
            seeded_session: true,
        });
        let body = &script[2];
        // Neither payload can leave its quotes: a `'` inside is closed, escaped
        // and reopened, so the shell sees one word.
        assert!(!body.contains("; touch /pwned;\n"), "{body}");
        assert!(body.contains(r"'\''"), "the quoting did not escape a quote");
        assert!(body.contains("'; rm -rf /'"), "{body}");
    }

    fn pod_with_status(status: serde_json::Value) -> Pod {
        let mut pod = job_pod(&spec("review")).unwrap();
        pod.status = Some(serde_json::from_value(status).expect("a PodStatus"));
        pod
    }

    /// AC-7's other half: a quota rejection comes back from `create`, but an
    /// image that does not exist and a pool nothing can schedule onto are both
    /// ACCEPTED at create and only admitted to in the status.
    #[test]
    fn a_pod_that_will_never_start_is_told_apart_from_one_still_starting() {
        // Still pulling. Nothing is wrong yet.
        let pulling = pod_with_status(serde_json::json!({
            "phase": "Pending",
            "containerStatuses": [{
                "name": AGENT_CONTAINER, "ready": false, "restartCount": 0,
                "image": "img", "imageID": "",
                "state": { "waiting": { "reason": "ContainerCreating" } },
            }],
        }));
        assert_eq!(start_verdict(&pulling), StartVerdict::Waiting);

        // A first pull failure can be a registry blip, and Kubernetes says
        // `ImagePullBackOff` once it has retried — which is when a human has to
        // look. So `ErrImagePull` alone is still Waiting.
        let blip = pod_with_status(serde_json::json!({
            "phase": "Pending",
            "containerStatuses": [{
                "name": AGENT_CONTAINER, "ready": false, "restartCount": 0,
                "image": "img", "imageID": "",
                "state": { "waiting": { "reason": "ErrImagePull" } },
            }],
        }));
        assert_eq!(start_verdict(&blip), StartVerdict::Waiting);

        let hopeless = pod_with_status(serde_json::json!({
            "phase": "Pending",
            "containerStatuses": [{
                "name": AGENT_CONTAINER, "ready": false, "restartCount": 0,
                "image": "img", "imageID": "",
                "state": { "waiting": {
                    "reason": "ImagePullBackOff",
                    "message": "Back-off pulling image \"nope:1\"",
                } },
            }],
        }));
        match start_verdict(&hopeless) {
            // Configuration: a human has to fix the image reference, and
            // retrying cannot.
            StartVerdict::Refused(r) => {
                assert!(!r.keep_claiming(), "an unpullable image will not clear");
                assert!(r.to_string().contains("ImagePullBackOff"), "{r}");
            }
            other => panic!("an unpullable image was not refused: {other:?}"),
        }

        // Nothing can schedule it. A full cluster clears itself, so the job
        // waits in the queue and this node keeps claiming.
        let unschedulable = pod_with_status(serde_json::json!({
            "phase": "Pending",
            "conditions": [{
                "type": "PodScheduled", "status": "False",
                "reason": "Unschedulable",
                "message": "0/3 nodes are available: insufficient cpu",
            }],
        }));
        match start_verdict(&unschedulable) {
            StartVerdict::Refused(r) => {
                assert!(r.keep_claiming(), "a full cluster clears itself");
                assert!(r.to_string().contains("insufficient cpu"), "{r}");
            }
            other => panic!("an unschedulable Pod was not refused: {other:?}"),
        }

        for phase in ["Running", "Succeeded", "Failed"] {
            let started = pod_with_status(serde_json::json!({ "phase": phase }));
            assert_eq!(start_verdict(&started), StartVerdict::Started, "{phase}");
        }
        // A Pod the apiserver has not filled in yet is not a verdict.
        assert_eq!(
            start_verdict(&job_pod(&spec("review")).unwrap()),
            StartVerdict::Waiting
        );
    }

    /// AC-4. A Secret that was never created, or whose name has a typo in it, is
    /// the cluster's state and not the card's — so the job is REFUSED and goes
    /// back to the queue, rather than failing and spending a strike from a
    /// budget that exists to catch an agent that cannot do the work.
    ///
    /// The `env_from` is what turns it into this status within seconds, and the
    /// refusal carries the kubelet's own sentence, so the operator is told which
    /// Secret is missing rather than that "the job Pod will not start".
    #[test]
    fn a_missing_credentials_secret_refuses_the_job_rather_than_failing_it() {
        for message in [
            "secret \"nook-job-credentials\" not found",
            "secret \"nook-job-credentails\" not found",
        ] {
            let pod = pod_with_status(serde_json::json!({
                "phase": "Pending",
                "containerStatuses": [{
                    "name": AGENT_CONTAINER, "ready": false, "restartCount": 0,
                    "image": "img", "imageID": "",
                    "state": { "waiting": {
                        "reason": "CreateContainerConfigError",
                        "message": message,
                    } },
                }],
            }));
            match start_verdict(&pod) {
                StartVerdict::Refused(r) => {
                    // Configuration, not transient: a Secret nobody has created
                    // will not appear by waiting, and a node that kept claiming
                    // into one would refuse in turn everything it was sent.
                    assert!(!r.keep_claiming(), "waiting cannot create a Secret");
                    assert!(r.to_string().contains(message), "{r}");
                }
                other => panic!("a missing Secret was not refused: {other:?}"),
            }
        }
    }

    /// The crash-honesty fallback's input (MAIN-161 AC-4): with no result
    /// record, what the run reports comes from here, so `None` while it is
    /// still running has to be distinguishable from an exit of zero.
    #[test]
    fn the_exit_code_is_the_agent_containers_and_none_until_it_has_one() {
        let running = pod_with_status(serde_json::json!({
            "phase": "Running",
            "containerStatuses": [{
                "name": AGENT_CONTAINER, "ready": true, "restartCount": 0,
                "image": "img", "imageID": "",
                "state": { "running": { "startedAt": "2026-09-02T00:00:00Z" } },
            }],
        }));
        assert_eq!(exit_code(&running), None);

        let done = pod_with_status(serde_json::json!({
            "phase": "Failed",
            "containerStatuses": [{
                "name": AGENT_CONTAINER, "ready": false, "restartCount": 0,
                "image": "img", "imageID": "",
                "state": { "terminated": { "exitCode": 7, "finishedAt": null } },
            }],
        }));
        assert_eq!(exit_code(&done), Some(7));

        // A terminal Pod whose container status is gone is reported as a
        // failure, never as a success: the result record is the evidence of
        // success and this is only the fallback for when none arrived.
        let stripped = pod_with_status(serde_json::json!({ "phase": "Failed" }));
        assert_eq!(exit_code(&stripped), Some(-1));
        assert_eq!(
            exit_code(&pod_with_status(
                serde_json::json!({ "phase": "Succeeded" })
            )),
            Some(0)
        );
    }

    /// This module HAS A CALLER, and the whole ticket turns on it.
    ///
    /// The shape `sandbox::guards` set, for the reason it exists: the cluster
    /// half cannot be unit-tested without a cluster, so what a test CAN pin is
    /// that `loop_job` reaches for it. A previous pass of this ticket shipped
    /// every function below with nothing calling any of them — a node that
    /// advertised Pod-per-job confinement and ran every job as a process inside
    /// its own pod. That is the defect this guard makes impossible to reland.
    #[test]
    fn loop_job_actually_runs_a_job_as_a_pod() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/loop_job.rs");
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        // The tests at the foot of that file must not be able to satisfy this.
        let src = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => &src[..],
        };

        for reached in [
            // AC-3: a Pod is created for the job.
            "exec.start(",
            // AC-7: and a create that fails refuses rather than fails.
            "k8s_exec::refusal_for(",
            // AC-4: its output becomes the card's transcript.
            "exec.follow(",
            // AC-5: it is deleted on the way out, from `Drop`.
            "exec.stop(",
            // …and the run's conclusion carries the Pod's own exit status.
            "exec.await_exit(",
            // AC-8: the orphan sweep still hangs off the one seam.
            "k8s_exec::spawn_orphan_sweep(",
            // The Pod spec is this module's, not a second one built inline.
            "k8s_exec::pod_command(",
        ] {
            assert!(
                src.contains(reached),
                "loop_job.rs no longer reaches `{reached}` — the Pod executor is \
                 unreachable again, and a node in this mode would run every job as \
                 a local process while advertising Pod-per-job confinement"
            );
        }

        // …and it is reached from `run`, not merely defined somewhere. A helper
        // nothing calls is exactly what this guard exists to catch.
        assert!(
            src.contains("run_in_pod("),
            "nothing dispatches a job to the Pod executor"
        );
        assert!(
            src.matches("fn run_in_pod(").count() == 1 && src.matches("run_in_pod(").count() >= 2,
            "`run_in_pod` is defined and never called"
        );
    }

    /// What the node ADVERTISES and what it DOES are decided by one function.
    ///
    /// `nook get nodes` prints the sandbox column and the fail-closed dispatch
    /// gate reads it, so a node reporting confinement it does not provide is
    /// worse than one reporting none — and that is precisely what an earlier
    /// pass of this ticket shipped: `capabilities.rs` said "each job runs in its
    /// own Pod" while every job ran as a process inside the agent's own pod.
    /// Both ends now ask `ExecutorConfig::from_env()`, and one answer cannot
    /// disagree with itself.
    #[test]
    fn the_node_advertises_the_executor_it_actually_uses() {
        let read = |file: &str| {
            let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
        };
        for (file, what) in [
            ("capabilities.rs", "reports the sandbox"),
            ("loop_job.rs", "decides how a job runs"),
        ] {
            assert!(
                read(file).contains("ExecutorConfig::from_env()"),
                "{file}, which {what}, no longer resolves the executor the way \
                 the other end does — the node can now claim a confinement it \
                 does not provide"
            );
        }
    }

    /// AC-10. A Pod's `env` is not a private place, so no credential goes in it.
    ///
    /// `get pods` returns every `value:` and `kubectl describe pod` prints
    /// them — and this executor's own Role grants `pods get/list/watch` across
    /// the namespace. Writing the fleet's GitHub token there would publish it to
    /// every principal with pod-read, a far lower bar than `get secrets`, and
    /// store it in etcd under a resource that is not encrypted at rest by
    /// default. So credentials arrive by reference or not at all.
    #[test]
    fn credentials_are_referenced_from_a_secret_and_never_written_into_the_pod() {
        // With none configured, a Pod gets no credential source whatsoever —
        // the literal reading of "no credential path ships here".
        let bare = job_pod(&spec("review")).unwrap();
        assert!(container(&bare).env_from.is_none());

        let mut s = spec("review");
        s.credentials_secret = Some("nook-job-credentials".into());
        let pod = job_pod(&s).unwrap();

        let from = container(&pod)
            .env_from
            .as_ref()
            .expect("a credential seam");
        let secret = from[0].secret_ref.as_ref().expect("a secretRef");
        assert_eq!(secret.name, "nook-job-credentials");
        // A named Secret that is absent must stop the Pod, not start an agent
        // that quietly has no credentials and spends a pass finding out.
        assert_eq!(secret.optional, Some(false));

        // Not a value anywhere: the seam is a REFERENCE, and every ordinary
        // pair still carries its own literal value rather than going through it.
        for var in container(&pod).env.as_ref().unwrap() {
            assert!(
                var.value_from.is_none(),
                "{} reaches for a value source it does not have",
                var.name
            );
        }
        // The env the node DOES write is the non-secret contract, and it must
        // still be there — withholding credentials must not withhold the job id.
        let env = env_of(&pod);
        assert_eq!(env["NOOK_JOB_ID"], "0198f0aa-1111-7000-8000-abcdefabcdef");
        assert_eq!(env["NOOK_SANDBOX"], "1");
    }

    /// The seam is opt-in and, unlike the executor's other settings, absent is
    /// a WORKING state rather than a misconfiguration: AC-10 ships no credential
    /// path, so a cluster with no Secret is the default rather than an error.
    #[test]
    fn the_credential_secret_is_optional_where_the_namespace_and_image_are_not() {
        let base = [
            ("NOOK_EXECUTOR", "kubernetes"),
            ("NOOK_EXECUTOR_NAMESPACE", "nook-jobs"),
            ("NOOK_JOB_IMAGE", "img:1"),
        ];
        let cfg = ExecutorConfig::from_vars(vars(&base)).unwrap().unwrap();
        assert_eq!(cfg.credentials_secret, None);

        let mut with = base.to_vec();
        with.push(("NOOK_JOB_CREDENTIALS_SECRET", "nook-job-credentials"));
        let cfg = ExecutorConfig::from_vars(vars(&with)).unwrap().unwrap();
        assert_eq!(
            cfg.credentials_secret.as_deref(),
            Some("nook-job-credentials")
        );
    }

    /// AC-8's sharp edge, and the only part of it that was asserted by a comment
    /// rather than a test.
    ///
    /// `orphans` deliberately does not look at the node label — the narrowing
    /// happens at the APISERVER, so a sibling's Pod is never fetched and cannot
    /// be mistaken for anything. That makes this selector the whole guarantee,
    /// and `values.yaml` defaulting every install to one `nook-jobs` namespace
    /// is what makes it load-bearing.
    #[test]
    fn the_sweep_only_asks_for_this_nodes_own_pods() {
        let selector = sweep_selector("azul");
        assert!(selector.contains(JOB_LABEL), "{selector}");
        assert!(
            selector.contains(&format!("{NODE_LABEL}=azul")),
            "the sweep would fetch every node's job Pods: {selector}"
        );

        // The same folding the LABEL gets, or the selector would ask for a value
        // no Pod carries and the sweep would silently reclaim nothing.
        let mut s = spec("review");
        s.node = "Azul.local".into();
        let labelled = job_pod(&s).unwrap().metadata.labels.unwrap();
        assert!(
            sweep_selector("Azul.local")
                .contains(&format!("{NODE_LABEL}={}", labelled[NODE_LABEL])),
            "the selector and the label disagree about this node's name"
        );
    }

    /// AC-9, and the card says "asserted, not assumed".
    ///
    /// `build` is the one kind whose Pod is privileged, so it is never in the
    /// DEFAULTS: an install that says nothing about builds must not acquire the
    /// ability to run one, and an upgrade must not hand privileged containers to
    /// a general node pool. Two documents decide that — the image's default and
    /// the chart's list — and they are the two an install actually reads, so
    /// both are checked.
    ///
    /// Since MAIN-655 builds ARE reachable here, but only deliberately: an
    /// operator adds `build` to `loopKinds` and names a `buildPool`, and the
    /// chart refuses to render one without the other. This test guards the
    /// default, not the possibility.
    ///
    /// This is not the WALL either: `jobs::kind_wall_refusal` refuses build work
    /// on a shared operator that reports no isolated pool, whatever a node
    /// declares. It is the statement of intent that keeps the wall from ever
    /// being the only thing standing between a card and a privileged Pod.
    #[test]
    fn a_build_is_never_offered_to_this_executor_by_default() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("the workspace root");

        let dockerfile =
            std::fs::read_to_string(root.join("deploy/docker/operator-node.Dockerfile"))
                .expect("the operator-node image");
        let declared = dockerfile
            .lines()
            .find_map(|l| l.trim().strip_prefix("ENV NOOK_LOOP_KINDS="))
            .expect("the image declares its loop kinds");
        assert!(
            !declared.split(',').any(|k| k.trim() == "build"),
            "the operator-node image offers to run builds: {declared}"
        );

        let values = std::fs::read_to_string(root.join("charts/nook-operator-node/values.yaml"))
            .expect("the chart's values");
        let kinds: Vec<&str> = values
            .split("\nloopKinds:")
            .nth(1)
            .expect("loopKinds")
            .lines()
            .skip(1)
            .map_while(|l| l.trim().strip_prefix("- "))
            .collect();
        assert!(
            !kinds.is_empty(),
            "the chart's loopKinds could not be read — this asserts nothing"
        );
        assert!(
            !kinds.contains(&"build"),
            "the chart offers to run builds on a cluster executor: {kinds:?}"
        );

        // And the kinds it DOES declare are ones this executor can actually
        // run — a kind whose profile wants a nested daemon would be refused at
        // `job_pod` on every job, which is a node that claims work it cannot do.
        for kind in kinds {
            assert!(
                job_pod(&{
                    let mut s = spec(kind);
                    s.build_pool = None;
                    s
                })
                .is_ok(),
                "{kind} is declared but needs a build pool to run"
            );
        }
    }
}
