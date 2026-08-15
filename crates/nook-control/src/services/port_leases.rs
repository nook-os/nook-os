//! Port leases for parallel sessions (MAIN-301).
//!
//! Two worktrees of one app contend for the same hardcoded port, so only one
//! could run at a time — the ceiling on parallel engineering. A session now
//! leases ports from its node's range and its runtime reads them from env.
//!
//! **The workspace declares WHAT it needs; this end decides WHICH numbers
//! satisfy it.** A repo says `web -> PORT`, `api -> API_PORT`, `debug ->
//! DEBUG_PORT`; the broker knows nothing about any of those names. Baking
//! `NOOK_PORT` into the allocator was the first cut, and it meant a second
//! listener was unrepresentable and every new framework was a change here. The
//! declaration lives on the workspace ([`PortRequirement`]), so a Next.js app,
//! an ASP.NET service and a Rust backend all lease from one node without this
//! end learning anything about any of them.
//!
//! **Reclaim is lazy for a SESSION.** Nothing frees a session's lease when it
//! ends, is killed or is reaped: allocation drops the rows of non-live sessions
//! on the node first, so a dead session's ports come back at the moment
//! somebody needs one (AC-4). The first cut got that from a partial index over
//! live statuses, which only worked while the lease lived on the session row;
//! moving it into the allocator keeps the property now that a session holds
//! several.
//!
//! **A BUILD's lease is the opposite, and deliberately (MAIN-552 AC-3/AC-4).**
//! It is held by the CARD whose worktree the stack lives in, so the lazy sweep
//! — which is over sessions — cannot reach it. A build's stack outlives its run
//! by design, and a port freed while containers are still bound to it is worse
//! than one never leased. It is handed back explicitly, when the stack comes
//! down: `stack_reaper`, or a human pruning the worktree.
//!
//! The range is the operator's if they set one, else what the node advertises.
//! A node that reports neither leases nothing, and that is a working session
//! without ports rather than an error: not every session runs a server.

use nook_types::{
    BrowsableTarget, LeasedPort, NodeId, PortRange, PortRequirement, SessionId, TaskId, TenantId,
    WorkspaceId,
};

use crate::error::{ApiError, ApiResult};
use crate::repo::sessions::{LeaseHolder, NewPortLease};
use crate::state::AppState;

/// How many times to retry when another session takes the port we just read.
/// Small on purpose: each retry is one more free port down the range, so a
/// caller losing three in a row means genuine contention, not a stuck loop.
const RACE_RETRIES: u32 = 3;

/// What a workspace gets when it has declared nothing.
///
/// One optional listener on `NOOK_PORT` — the convention CLAUDE.md documents
/// and the dogfood flow uses, expressed as DATA rather than as a branch in the
/// allocator. A workspace that declares its own list replaces this entirely;
/// one that declares an EMPTY list means "binds nothing", which is a different
/// statement and is honoured as one.
///
/// Deliberately `required: false`: a plain shell session in a repo that happens
/// to be undeclared must not fail to start because a node ran out of ports.
pub fn default_requirements() -> Vec<PortRequirement> {
    vec![PortRequirement {
        name: "web".into(),
        env: "NOOK_PORT".into(),
        protocol: "tcp".into(),
        required: false,
        // Every runtime: the zero-config default must keep behaving as it does
        // today for a repo that has declared nothing at all (AC-4).
        runtimes: Vec::new(),
        // A workspace that has declared nothing has not said it serves a UI,
        // and guessing would hand a recorder a port nothing is listening on.
        browsable: false,
        path: "/".into(),
    }]
}

/// The requirements in force for a workspace: its own declaration, else the
/// default. A declaration that does not parse is treated as absent and logged —
/// guessing at a shape this build does not understand would converge on
/// something nobody asked for.
pub async fn requirements_of(
    state: &AppState,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
) -> ApiResult<Vec<PortRequirement>> {
    let Some(id) = workspace else {
        // An ad-hoc terminal belongs to no repo, so there is nothing to declare
        // for it and nothing to bind. It leases nothing.
        return Ok(Vec::new());
    };
    let Some(ws) = state.workspaces.get(tenant, id).await? else {
        return Ok(default_requirements());
    };
    match ws.port_requirements {
        None => Ok(default_requirements()),
        Some(raw) => match serde_json::from_value::<Vec<PortRequirement>>(raw) {
            Ok(reqs) => Ok(reqs),
            Err(e) => {
                tracing::warn!(workspace = %id, error = %e, "unreadable port requirements — using the default");
                Ok(default_requirements())
            }
        },
    }
}

/// What can a person open in this workspace, in declaration order (MAIN-596)?
///
/// **The one definition of the question.** Every caller that wants a frontend —
/// a recorder, a link in the UI, a smoke check — asks here rather than reading
/// the declaration and re-deriving the rule, because the rule has three parts
/// (which listeners, in what order, under what path) and three callers deriving
/// it independently is three chances to disagree about a repo with two
/// frontends.
///
/// Reads the requirements IN FORCE, so it inherits their precedence: a
/// `.nook.toml` wins where it declares, the stored workspace value fills in
/// where it does not. It answers about the DECLARATION and not about any
/// session, so it returns the variable rather than a number — the caller that
/// has a session resolves the number from its leases.
pub async fn browsable_targets(
    state: &AppState,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
) -> ApiResult<Vec<BrowsableTarget>> {
    Ok(requirements_of(state, tenant, workspace)
        .await?
        .into_iter()
        .filter(|r| r.browsable)
        .map(|r| BrowsableTarget {
            name: r.name,
            env: r.env,
            path: r.path,
        })
        .collect())
}

/// The range in force for a node, and where it came from.
pub async fn range_of(state: &AppState, node: NodeId) -> ApiResult<(Option<PortRange>, String)> {
    let Some(n) = state.nodes.by_id_any_tenant_or_none(node).await? else {
        return Ok((None, "none".into()));
    };
    if let (Some(start), Some(end)) = (n.port_range_start, n.port_range_end) {
        return Ok((Some(PortRange { start, end }), "operator".into()));
    }
    Ok(match advertised(&n.capabilities) {
        Some(r) => (Some(r), "node".into()),
        None => (None, "none".into()),
    })
}

/// The ports this node must never lease, lowest first.
///
/// Read separately from the range rather than folded into `range_of`, because a
/// node with NO range still has a meaningful exclusion list: the operator can
/// rule ports out before ever handing the machine a range, and the two are set
/// by different commands at different times.
pub async fn exclusions_of(state: &AppState, node: NodeId) -> ApiResult<Vec<i32>> {
    let Some(n) = state.nodes.by_id_any_tenant_or_none(node).await? else {
        return Ok(Vec::new());
    };
    // Stored as JSON because it is a list on a column, and read defensively:
    // a hand-edited row with a string or a null in it must cost the operator
    // that entry, not the whole exclusion list.
    let mut out: Vec<i32> = n
        .port_exclusions
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_i64())
                .map(|x| x as i32)
                .collect()
        })
        .unwrap_or_default();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// What the node itself reported, if anything.
///
/// Read as ONE FIELD, not by deserialising the whole `Capabilities` struct.
/// That struct has required fields, so a node whose report is missing any of
/// them — an older agent, a partial write — would silently lose its port range
/// too. The range should survive its neighbours.
pub fn advertised(capabilities: &serde_json::Value) -> Option<PortRange> {
    let pair = capabilities.get("port_range")?.as_array()?;
    let start = pair.first()?.as_i64()?;
    let end = pair.get(1)?.as_i64()?;
    if start <= 0 || end < start {
        return None;
    }
    Some(PortRange {
        start: start as i32,
        end: end as i32,
    })
}

/// A REQUIRED listener that could not be satisfied.
///
/// Returned rather than thrown, because the two callers want opposite things
/// from it and only one of them is an error. A session cannot start without the
/// port its app binds, so [`lease_for`] renders this as the `400` it always did
/// — byte for byte. A BUILD run must not fail on it: the job stays `queued`
/// with a typed reason (MAIN-552 AC-6) and is placed once a lease frees, which
/// is a wait rather than a loss.
#[derive(Debug, Clone)]
pub struct Refusal {
    /// The listener's name and env var, so a reader can act: widen the node's
    /// range, or free a lease on the Nodes page.
    pub listener: String,
    pub env: String,
    /// The whole sentence, cause included.
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What a session got, and what it asked for and did not get.
///
/// The second half used to be dropped on the floor. A consumer reads its port
/// from an env var, and an ABSENT var has two opposite meanings — "this repo was
/// cloned outside nook, use your default" and "the node ran out, your default is
/// the shared literal every other session also falls back to". Nothing
/// distinguished them, which is what turned `required = false` into a silent
/// collision (MAIN-377).
#[derive(Debug, Default, Clone)]
pub struct Leased {
    pub ports: Vec<LeasedPort>,
    /// Declared listeners that could not be leased, in declaration order. Only
    /// ever optional ones — a required listener refuses the session instead.
    pub unsatisfied: Vec<String>,
}

/// Is this listener for a session running `runtime`?
///
/// EMPTY means every runtime, and that default is load-bearing: an untouched
/// `.nook.toml` must keep leasing exactly what it leases today (MAIN-378 AC-4),
/// so saying nothing has to mean "all", never "none".
///
/// Matched case-insensitively because a declaration is hand-written and
/// `Bash` meaning something different from `bash` would be a trap with no
/// upside.
pub fn wants(req: &PortRequirement, runtime: &str) -> bool {
    req.runtimes.is_empty() || req.runtimes.iter().any(|r| r.eq_ignore_ascii_case(runtime))
}

/// What a RESTART should report as unsatisfied — and what a build's dispatch
/// reports, for the same reason (MAIN-552).
///
/// A restart keeps the ports it already holds rather than re-leasing (MAIN-301),
/// so nothing comes back from the allocator and the set has to be derived: a
/// declared listener the holder has no lease for is one it never got. A build's
/// run is dispatched from the leases stored on its card rather than from the
/// allocation, so it is in exactly that position.
///
/// Deriving it means REPRODUCING the allocator's rules, and the first cut did
/// not — which is the whole reason this is a function with tests rather than a
/// filter inlined in the route:
///
/// * **No range → nothing reported.** `requirements_of` never returns empty for
///   a workspace-backed session (an undeclared workspace still gets the default
///   listener), and on a range-less node nothing was ever leased — so every
///   listener fell through the filter. A session that started clean would come
///   back reporting everything unsatisfied, and the `.nook.toml` guard this card
///   documents would then exit non-zero on a machine where nothing had changed.
/// * **Optional only.** A required listener is refused by the allocator at
///   start, so it can never be "unsatisfied" — that is the invariant `Leased`
///   states. One ADDED to the declaration after the session started holds no
///   lease here; reporting it would break that invariant, and refusing the
///   restart would fail a session that succeeds today.
pub async fn unsatisfied_on_restart(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    runtime: &str,
    held: &[LeasedPort],
) -> ApiResult<Vec<String>> {
    let (range, _) = range_of(state, node).await?;
    if range.is_none() {
        return Ok(Vec::new());
    }
    Ok(requirements_of(state, tenant, workspace)
        .await?
        .into_iter()
        // Same runtime filter the allocator applied: a listener this runtime
        // never asks for was not "unsatisfied", it was not wanted.
        .filter(|r| wants(r, runtime))
        .filter(|r| !r.required)
        .filter(|r| !held.iter().any(|l| l.name == r.name))
        .map(|r| r.name)
        .collect())
}

/// Lease every port a session's workspace declares.
///
/// Returns what was leased, in declaration order — empty when the node offers
/// no range or the workspace declares no listeners, both of which are working
/// sessions rather than failures.
///
/// A REQUIRED listener that cannot be satisfied is an error; an optional one is
/// skipped. That distinction is the whole reason `required` is on the wire: a
/// `debug` port going unleased should not stop the app starting, and the app's
/// own port going unleased should not start a session that then collides on a
/// hardcoded default — the exact failure this card exists to remove.
pub async fn lease_for(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    session: SessionId,
    runtime: &str,
) -> ApiResult<Leased> {
    lease_for_avoiding(state, tenant, node, workspace, session, runtime, &[]).await
}

/// The runtime a BUILD run's ports are declared for.
///
/// A build is an agent session in a worktree, so it takes what the workspace
/// declares for `claude` — the same string the executor actually launches
/// (MAIN-378's runtime filter). Naming a build-specific runtime here would be
/// the second declaration AC-1 forbids.
pub const BUILD_RUNTIME: &str = "claude";

/// Lease the ports a BUILD run's stack binds (MAIN-552).
///
/// The same rule and the same code path a session uses — the only differences
/// are WHO holds it and what a refusal means. The holder is the CARD, not the
/// job: its worktree outlives the run (MAIN-480), so a repair pass gets the
/// numbers its stack is still bound to back (AC-5) rather than a fresh set the
/// running containers know nothing about.
///
/// `Ok(Err(refusal))` is a required listener that could not be satisfied, which
/// the caller turns into a queued job rather than a failed one (AC-6).
pub async fn lease_for_build(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    task: TaskId,
) -> ApiResult<Result<Leased, Refusal>> {
    lease_owned(
        state,
        tenant,
        node,
        workspace,
        LeaseHolder::Build(task),
        BUILD_RUNTIME,
        &[],
    )
    .await
}

/// The same, minus ports a node has just told us it could not bind.
///
/// `avoid` is NEITHER a lease nor an exclusion, and keeping it out of both is
/// the point: it is one attempt's knowledge, true for the next few seconds on
/// one machine. Writing it to the node would turn a transient clash into
/// permanent policy, and the operator never said so.
pub async fn lease_for_avoiding(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    session: SessionId,
    runtime: &str,
    avoid: &[i32],
) -> ApiResult<Leased> {
    // A session's contract is unchanged (MAIN-552 NG-2): a required listener it
    // cannot get is still the same `400`, with the same sentence.
    match lease_owned(
        state,
        tenant,
        node,
        workspace,
        LeaseHolder::Session(session),
        runtime,
        avoid,
    )
    .await?
    {
        Ok(leased) => Ok(leased),
        Err(refusal) => Err(ApiError::BadRequest(refusal.message)),
    }
}

/// The allocation itself, for whichever holder (MAIN-552).
///
/// One body, because AC-1 is that a build leases by the same rule a session
/// does — a second copy of this would be a second rule the moment either was
/// touched.
async fn lease_owned(
    state: &AppState,
    tenant: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    holder: LeaseHolder,
    runtime: &str,
    avoid: &[i32],
) -> ApiResult<Result<Leased, Refusal>> {
    let reqs: Vec<PortRequirement> = requirements_of(state, tenant, workspace)
        .await?
        .into_iter()
        .filter(|r| wants(r, runtime))
        .collect();
    if reqs.is_empty() {
        // Either the repo declares nothing, or nothing it declares is for this
        // runtime. Both are working sessions without ports, not failures.
        return Ok(Ok(Leased::default()));
    }
    let (range, _) = range_of(state, node).await?;
    let Some(range) = range else {
        // No range is not an exhausted range: the node never offered ports at
        // all. Even a required listener gets a clear refusal rather than the
        // "every port is taken" message, which would send a reader hunting for
        // leases that do not exist.
        if let Some(r) = reqs.iter().find(|r| r.required) {
            return Ok(Err(Refusal {
                listener: r.name.clone(),
                env: r.env.clone(),
                message: format!(
                    "this workspace needs a port for `{}` ({}), and this node advertises no port range",
                    r.name, r.env
                ),
            }));
        }
        // AC-3: a node that offers no ports is a working session without them,
        // not one that lost a race. Nothing is reported as unsatisfied.
        return Ok(Ok(Leased::default()));
    };

    // What this holder already holds. A requirement it has a lease for keeps
    // that port: re-leasing is the same holder coming back — a session
    // restarting, a repair pass on the same card — and handing it a different
    // number would break every URL and config that pointed at the old one, and
    // for a build, the containers already bound to it.
    let held = match holder {
        LeaseHolder::Session(id) => state.sessions.leases_of(id).await?,
        // On THIS node. A session belongs to one machine and a card does not,
        // so without that scope a card holding rows on another node would have
        // those numbers handed back as "already held" and the run would bind
        // ports nothing here holds — the collision this card removes, coming
        // back through the reuse path.
        LeaseHolder::Build(id) => state.sessions.build_leases_of(node, id).await?,
    };
    let mut excluded = exclusions_of(state, node).await?;
    excluded.extend_from_slice(avoid);
    excluded.sort_unstable();
    excluded.dedup();

    let mut leased: Vec<LeasedPort> = Vec::new();
    let mut unsatisfied: Vec<String> = Vec::new();
    // What THIS call wrote, so a refusal part-way through can undo exactly it
    // (MAIN-552). Not the whole set: a repair pass on a card whose stack is up
    // can be refused on a listener ADDED to the declaration since, and dropping
    // its existing leases would free ports live containers are bound to.
    let mut taken_here: Vec<String> = Vec::new();
    for req in reqs {
        if let Some(existing) = held.iter().find(|l| l.name == req.name) {
            leased.push(existing.clone());
            continue;
        }
        let one = match lease_one(state, node, holder, &range, &excluded, &req).await? {
            Ok(port) => port,
            // Contention, not exhaustion. A session is told to try again; a
            // build waits queued and is placed on the next pass.
            Err(refusal) => {
                undo(state, node, holder, &taken_here).await;
                return Ok(Err(refusal));
            }
        };
        match one {
            Some(port) => {
                taken_here.push(req.name.clone());
                leased.push(LeasedPort {
                    name: req.name,
                    env: req.env,
                    port,
                })
            }
            None if req.required => {
                // Name the CAUSE, not just the symptom. Once exclusions exist,
                // "every port is leased" can be flatly untrue — the ports may
                // be sitting free and ruled out — and that sends a reader
                // hunting through sessions instead of at the list they set.
                let inside = excluded
                    .iter()
                    .filter(|p| **p >= range.start && **p <= range.end)
                    .count();
                let because = if inside > 0 {
                    format!(
                        "every port in {}–{} is either leased or excluded ({inside} excluded on this node)",
                        range.start, range.end
                    )
                } else {
                    format!(
                        "every port in {}–{} is leased on this node",
                        range.start, range.end
                    )
                };
                undo(state, node, holder, &taken_here).await;
                return Ok(Err(Refusal {
                    listener: req.name.clone(),
                    env: req.env.clone(),
                    message: format!("no free port for `{}` ({}): {because}", req.name, req.env),
                }));
            }
            // Optional and unsatisfiable: the session starts without it — and
            // now SAYS so, to the session and to whoever is reading the log.
            None => {
                tracing::warn!(
                    listener = %req.name,
                    env = %req.env,
                    %node,
                    workspace = ?workspace,
                    range = %format!("{}-{}", range.start, range.end),
                    "optional listener went unleased — the session starts without it, \
                     and its consumer must not fall back to a shared default"
                );
                unsatisfied.push(req.name);
            }
        }
    }
    Ok(Ok(Leased {
        ports: leased,
        unsatisfied,
    }))
}

/// Give back what a refused allocation had already taken (MAIN-552).
///
/// **Builds only, and the asymmetry is the defect this repairs.** A session's
/// partial set is collected by the lazy reclaim — the session row exists, the
/// failed start makes it non-live, and the allocator's own first step drops it.
/// A build has no such sweep, deliberately (AC-4), and none of its three
/// release routes can reach a card that never ran: the two worktree-keyed ones
/// find no tree, and `release_ports_of_stackless_build` needs the
/// `executor_node_id` a job that never claimed does not have. So eleven
/// required listeners against six free ports used to leave six ports held by a
/// card that never started, for every build AND every human session on the
/// machine, until somebody deleted the card.
///
/// Best effort: the refusal is the caller's answer either way, and a failed
/// undo is a leak to log rather than a reason to turn a wait into an error.
async fn undo(state: &AppState, node: NodeId, holder: LeaseHolder, names: &[String]) {
    if names.is_empty() || matches!(holder, LeaseHolder::Session(_)) {
        return;
    }
    if let Err(e) = state
        .sessions
        .release_lease_names(node, holder.id(), names)
        .await
    {
        tracing::warn!(
            %node, listeners = %names.join(","), error = %e,
            "a refused build allocation could not give back the ports it had taken"
        );
    }
}

/// One requirement, with the allocation race retried. `Ok(None)` is exhaustion
/// — the caller decides whether that is fatal, because only it knows whether
/// the requirement was required. `Ok(Some(Err(..)))` is losing the race three
/// times, which is contention rather than exhaustion and says so.
async fn lease_one(
    state: &AppState,
    node: NodeId,
    holder: LeaseHolder,
    range: &PortRange,
    excluded: &[i32],
    req: &PortRequirement,
) -> ApiResult<Result<Option<i32>, Refusal>> {
    for _ in 0..RACE_RETRIES {
        let held = state.sessions.reclaim_and_held_ports(node).await?;
        // The LOWEST free port, not any free port, so the lease list a human
        // reads is dense instead of scattered. Computed here rather than in SQL
        // because the range is bounded and `generate_series` is Postgres-only —
        // one round trip either way, and this one runs on both engines.
        let Some(port) =
            (range.start..=range.end).find(|p| !held.contains(p) && !excluded.contains(p))
        else {
            return Ok(Ok(None));
        };
        if state
            .sessions
            .add_lease(NewPortLease {
                holder,
                node,
                name: req.name.clone(),
                env: req.env.clone(),
                port,
            })
            .await?
        {
            return Ok(Ok(Some(port)));
        }
        // Lost the race — the unique index refused it. Ask again; the next read
        // sees the winner's port taken.
    }
    Ok(Err(Refusal {
        listener: req.name.clone(),
        env: req.env.clone(),
        message: format!(
            "could not lease a port for `{}`: too many sessions starting at once, try again",
            req.name
        ),
    }))
}
