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
//! **Reclaim is lazy, and there is still no release path.** Nothing frees a
//! lease when a session ends, is killed or is reaped: allocation drops the rows
//! of non-live sessions on the node first, so a dead session's ports come back
//! at the moment somebody needs one (AC-4). The first cut got that from a
//! partial index over live statuses, which only worked while the lease lived on
//! the session row; moving it into the allocator keeps the property now that a
//! session holds several.
//!
//! The range is the operator's if they set one, else what the node advertises.
//! A node that reports neither leases nothing, and that is a working session
//! without ports rather than an error: not every session runs a server.

use nook_types::{
    LeasedPort, NodeId, PortRange, PortRequirement, SessionId, TenantId, WorkspaceId,
};

use crate::error::{ApiError, ApiResult};
use crate::repo::sessions::NewPortLease;
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

/// What a RESTART should report as unsatisfied.
///
/// A restart keeps the ports it already holds rather than re-leasing (MAIN-301),
/// so nothing comes back from the allocator and the set has to be derived: a
/// declared listener this session holds no lease for is one it never got.
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
    held: &[LeasedPort],
) -> ApiResult<Vec<String>> {
    let (range, _) = range_of(state, node).await?;
    if range.is_none() {
        return Ok(Vec::new());
    }
    Ok(requirements_of(state, tenant, workspace)
        .await?
        .into_iter()
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
) -> ApiResult<Leased> {
    lease_for_avoiding(state, tenant, node, workspace, session, &[]).await
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
    avoid: &[i32],
) -> ApiResult<Leased> {
    let reqs = requirements_of(state, tenant, workspace).await?;
    if reqs.is_empty() {
        return Ok(Leased::default());
    }
    let (range, _) = range_of(state, node).await?;
    let Some(range) = range else {
        // No range is not an exhausted range: the node never offered ports at
        // all. Even a required listener gets a clear refusal rather than the
        // "every port is taken" message, which would send a reader hunting for
        // leases that do not exist.
        if let Some(r) = reqs.iter().find(|r| r.required) {
            return Err(ApiError::BadRequest(format!(
                "this workspace needs a port for `{}` ({}), and this node advertises no port range",
                r.name, r.env
            )));
        }
        // AC-3: a node that offers no ports is a working session without them,
        // not one that lost a race. Nothing is reported as unsatisfied.
        return Ok(Leased::default());
    };

    // What this session already holds. A requirement it has a lease for keeps
    // that port: re-leasing is the same session coming back — a restart, a
    // retry — and handing it a different number would break every URL and
    // config that pointed at the old one.
    let held = state.sessions.leases_of(session).await?;
    let mut excluded = exclusions_of(state, node).await?;
    excluded.extend_from_slice(avoid);
    excluded.sort_unstable();
    excluded.dedup();

    let mut leased: Vec<LeasedPort> = Vec::new();
    let mut unsatisfied: Vec<String> = Vec::new();
    for req in reqs {
        if let Some(existing) = held.iter().find(|l| l.name == req.name) {
            leased.push(existing.clone());
            continue;
        }
        match lease_one(state, node, session, &range, &excluded, &req).await? {
            Some(port) => leased.push(LeasedPort {
                name: req.name,
                env: req.env,
                port,
            }),
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
                return Err(ApiError::BadRequest(format!(
                    "no free port for `{}` ({}): {because}",
                    req.name, req.env
                )));
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
    Ok(Leased {
        ports: leased,
        unsatisfied,
    })
}

/// One requirement, with the allocation race retried. `None` is exhaustion —
/// the caller decides whether that is fatal, because only it knows whether the
/// requirement was required.
async fn lease_one(
    state: &AppState,
    node: NodeId,
    session: SessionId,
    range: &PortRange,
    excluded: &[i32],
    req: &PortRequirement,
) -> ApiResult<Option<i32>> {
    for _ in 0..RACE_RETRIES {
        let held = state.sessions.reclaim_and_held_ports(node).await?;
        // The LOWEST free port, not any free port, so the lease list a human
        // reads is dense instead of scattered. Computed here rather than in SQL
        // because the range is bounded and `generate_series` is Postgres-only —
        // one round trip either way, and this one runs on both engines.
        let Some(port) =
            (range.start..=range.end).find(|p| !held.contains(p) && !excluded.contains(p))
        else {
            return Ok(None);
        };
        if state
            .sessions
            .add_lease(NewPortLease {
                session,
                node,
                name: req.name.clone(),
                env: req.env.clone(),
                port,
            })
            .await?
        {
            return Ok(Some(port));
        }
        // Lost the race — the unique index refused it. Ask again; the next read
        // sees the winner's port taken.
    }
    Err(ApiError::BadRequest(format!(
        "could not lease a port for `{}`: too many sessions starting at once, try again",
        req.name
    )))
}
