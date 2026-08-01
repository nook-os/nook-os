//! Port leases for parallel sessions (MAIN-301).
//!
//! The claim under test is the one the card exists for: two sessions of one app
//! on one node get DIFFERENT ports. The rest protects the two properties that
//! make that safe to rely on — that a dead session's ports come back with
//! nothing having released them, and that the workspace's DECLARATION is what
//! decides how many ports and which env vars, so the broker never learns what
//! `PORT` means to anybody.
//!
//! Runs on both engines: the allocator picks the free port in Rust rather than
//! with `generate_series`, which is Postgres-only.

use nook_control::repo::sessions::NewSession;
use nook_control::services::port_leases;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

/// A node advertising `range`, or none at all.
async fn node_with(bed: &TestBed, tenant: TenantId, range: Option<(u16, u16)>) -> NodeId {
    let id = NodeId::new();
    let caps = match range {
        Some((a, b)) => serde_json::json!({ "port_range": [a, b] }),
        None => serde_json::json!({}),
    };
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, capabilities)
             VALUES ($1,$2,$3,$4,'online',$5)",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                caps
            ],
        )
        .await
        .expect("node");
    id
}

async fn session_on(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    name: &str,
) -> SessionId {
    bed.app_state()
        .await
        .sessions
        .create(NewSession {
            tenant,
            workspace_id: workspace,
            node_id: node,
            name: name.into(),
            runtime: "bash".into(),
            created_by: None,
            checkout_id: None,
            managed: false,
        })
        .await
        .expect("session")
        .id
}

async fn status(bed: &TestBed, id: SessionId, s: &str) {
    bed.db()
        .exec(
            "UPDATE sessions SET status = $2 WHERE id = $1",
            params![id, s.to_string()],
        )
        .await
        .expect("status");
}

/// Declare requirements on a workspace.
async fn declare(bed: &TestBed, tenant: TenantId, ws: WorkspaceId, reqs: &[(&str, &str, bool)]) {
    let value: Vec<PortRequirement> = reqs
        .iter()
        .map(|(name, env, required)| PortRequirement {
            name: (*name).into(),
            env: (*env).into(),
            protocol: "tcp".into(),
            required: *required,
        })
        .collect();
    bed.app_state()
        .await
        .workspaces
        .set_port_requirements(tenant, ws, Some(serde_json::to_value(&value).unwrap()))
        .await
        .expect("declare");
}

fn ports(leased: &[LeasedPort]) -> Vec<i32> {
    leased.iter().map(|l| l.port).collect()
}

/// THE demonstrable win (AC-3): two sessions on one node, two ports.
#[tokio::test]
async fn two_sessions_on_one_node_get_different_ports() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4010))).await;
    let state = bed.app_state().await;

    let a = session_on(&bed, tenant, node, Some(ws), "worktree-a").await;
    let b = session_on(&bed, tenant, node, Some(ws), "worktree-b").await;

    let pa = port_leases::lease_for(&state, tenant, node, Some(ws), a)
        .await
        .expect("lease a");
    let pb = port_leases::lease_for(&state, tenant, node, Some(ws), b)
        .await
        .expect("lease b");
    assert_eq!(ports(&pa), vec![4000]);
    assert_eq!(ports(&pb), vec![4001], "the second gets its own");

    bed.teardown().await;
}

/// The generalization (owner-approved, 2026-08-01): a workspace declares SEVERAL
/// named listeners and gets one lease each, carrying the env var IT chose.
///
/// This is the test the first cut could not have had: `sessions.leased_port` was
/// one column, so "a web port and an api port" was unrepresentable, and the env
/// name was a constant in the node.
#[tokio::test]
async fn a_workspace_declaring_three_listeners_gets_three_ports() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4010))).await;
    declare(
        &bed,
        tenant,
        ws,
        &[
            ("web", "PORT", true),
            ("api", "API_PORT", true),
            ("debug", "DEBUG_PORT", false),
        ],
    )
    .await;
    let state = bed.app_state().await;
    let s = session_on(&bed, tenant, node, Some(ws), "multi").await;

    let leased = port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("lease");
    assert_eq!(ports(&leased), vec![4000, 4001, 4002]);
    assert_eq!(
        leased.iter().map(|l| l.env.as_str()).collect::<Vec<_>>(),
        vec!["PORT", "API_PORT", "DEBUG_PORT"],
        "the env vars are the WORKSPACE's, not this end's"
    );

    // And a second session of the same repo gets three MORE, none of them shared.
    let t = session_on(&bed, tenant, node, Some(ws), "multi-2").await;
    let second = port_leases::lease_for(&state, tenant, node, Some(ws), t)
        .await
        .expect("lease 2");
    assert_eq!(ports(&second), vec![4003, 4004, 4005]);

    bed.teardown().await;
}

/// An undeclared workspace still gets the documented single `NOOK_PORT` — the
/// default is DATA, so zero-config keeps working while the allocator stays
/// ignorant of the name.
#[tokio::test]
async fn an_undeclared_workspace_gets_the_default_listener() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4010))).await;
    let state = bed.app_state().await;
    let s = session_on(&bed, tenant, node, Some(ws), "plain").await;

    let leased = port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("lease");
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].env, "NOOK_PORT");

    bed.teardown().await;
}

/// Declaring an EMPTY list is not the same as declaring nothing: it says this
/// repo binds no ports, and it is honoured rather than defaulted.
#[tokio::test]
async fn declaring_nothing_and_declaring_none_are_different() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4010))).await;
    bed.app_state()
        .await
        .workspaces
        .set_port_requirements(tenant, ws, Some(serde_json::json!([])))
        .await
        .expect("declare none");
    let state = bed.app_state().await;
    let s = session_on(&bed, tenant, node, Some(ws), "headless").await;

    assert!(port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("lease")
        .is_empty());

    bed.teardown().await;
}

/// AC-4, the no-leak property, and the reason there is no release path: a
/// session that exits frees its ports with nothing having run.
#[tokio::test]
async fn a_dead_sessions_ports_come_back_with_nothing_releasing_them() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4001))).await;
    declare(&bed, tenant, ws, &[("web", "PORT", true)]).await;
    let state = bed.app_state().await;

    let first = session_on(&bed, tenant, node, Some(ws), "first").await;
    assert_eq!(
        ports(
            &port_leases::lease_for(&state, tenant, node, Some(ws), first)
                .await
                .expect("lease")
        ),
        vec![4000]
    );

    // It dies — killed, crashed, reaped; the status is all that changes.
    status(&bed, first, "exited").await;

    let next = session_on(&bed, tenant, node, Some(ws), "next").await;
    assert_eq!(
        ports(
            &port_leases::lease_for(&state, tenant, node, Some(ws), next)
                .await
                .expect("lease")
        ),
        vec![4000],
        "the dead session's port is reused, and nothing released it"
    );

    bed.teardown().await;
}

/// AC-4's other half: exhaustion of a REQUIRED listener is a clear error, never
/// a silent fallback onto a hardcoded port.
#[tokio::test]
async fn exhausting_the_range_is_an_error_for_a_required_listener() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4000))).await;
    declare(&bed, tenant, ws, &[("web", "PORT", true)]).await;
    let state = bed.app_state().await;

    let a = session_on(&bed, tenant, node, Some(ws), "a").await;
    port_leases::lease_for(&state, tenant, node, Some(ws), a)
        .await
        .expect("the only port");

    let b = session_on(&bed, tenant, node, Some(ws), "b").await;
    let err = port_leases::lease_for(&state, tenant, node, Some(ws), b)
        .await
        .expect_err("the range is full");
    let msg = format!("{err:?}");
    assert!(msg.contains("web"), "the message names the listener: {msg}");

    bed.teardown().await;
}

/// An OPTIONAL listener that cannot be satisfied is skipped, not fatal — which
/// is the whole reason `required` is on the wire.
#[tokio::test]
async fn an_optional_listener_is_skipped_when_the_range_is_full() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4000))).await;
    declare(
        &bed,
        tenant,
        ws,
        &[("web", "PORT", true), ("debug", "DEBUG_PORT", false)],
    )
    .await;
    let state = bed.app_state().await;
    let s = session_on(&bed, tenant, node, Some(ws), "one-port-node").await;

    let leased = port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("the required one is satisfiable");
    assert_eq!(ports(&leased), vec![4000]);
    assert_eq!(
        leased[0].env, "PORT",
        "and the optional one is simply absent"
    );

    bed.teardown().await;
}

/// A node with no range leases nothing, and that is a working session rather
/// than a failure — unless something REQUIRED a port, which is a different
/// message so a reader is not sent hunting for leases that do not exist.
#[tokio::test]
async fn a_node_with_no_range_leases_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, None).await;
    let state = bed.app_state().await;
    let s = session_on(&bed, tenant, node, Some(ws), "no-range").await;

    // Undeclared, so the default listener — which is optional.
    assert!(port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("no range is not an error")
        .is_empty());

    declare(&bed, tenant, ws, &[("web", "PORT", true)]).await;
    let t = session_on(&bed, tenant, node, Some(ws), "needs-one").await;
    let err = port_leases::lease_for(&state, tenant, node, Some(ws), t)
        .await
        .expect_err("a required listener on a node with no range");
    assert!(
        format!("{err:?}").contains("advertises no port range"),
        "and it says so, rather than claiming exhaustion: {err:?}"
    );

    bed.teardown().await;
}

/// The operator's range overrides the node's advertisement.
#[tokio::test]
async fn the_operator_range_wins_over_the_advertised_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4010))).await;
    bed.db()
        .exec(
            "UPDATE nodes SET owner_person_id = $2 WHERE id = $1",
            params![node, person],
        )
        .await
        .expect("own it");
    let state = bed.app_state().await;
    state
        .nodes
        .set_port_range(node, tenant, Some(5000), Some(5001))
        .await
        .expect("set range");

    let s = session_on(&bed, tenant, node, Some(ws), "override").await;
    assert_eq!(
        ports(
            &port_leases::lease_for(&state, tenant, node, Some(ws), s)
                .await
                .expect("lease")
        ),
        vec![5000]
    );

    bed.teardown().await;
}

/// The AC-6 escape hatch, and the defect the review caught: releasing is scoped
/// to the node whose owner authorized it.
///
/// Two nodes, one tenant. Releasing against node A must not touch a lease held
/// on node B — the caller was authorized as A's owner, and scoping by session id
/// alone let that authorization reach somebody else's machine.
#[tokio::test]
async fn releasing_a_lease_is_scoped_to_the_node_it_was_authorized_for() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let a = node_with(&bed, tenant, Some((4000, 4010))).await;
    let b = node_with(&bed, tenant, Some((4000, 4010))).await;
    let state = bed.app_state().await;

    let on_b = session_on(&bed, tenant, b, Some(ws), "on-b").await;
    port_leases::lease_for(&state, tenant, b, Some(ws), on_b)
        .await
        .expect("lease on b");

    // A's owner tries to release a session that lives on B.
    let freed = state
        .sessions
        .release_leases(a, on_b)
        .await
        .expect("release");
    assert_eq!(freed, 0, "node A cannot free a port held on node B");
    assert_eq!(
        state.sessions.leases_on(b).await.expect("leases").len(),
        1,
        "B's lease is untouched"
    );

    // Against its own node it works, which is what makes the hatch useful.
    assert_eq!(
        state
            .sessions
            .release_leases(b, on_b)
            .await
            .expect("release"),
        1
    );

    bed.teardown().await;
}

/// Re-leasing a requirement replaces its lease rather than stacking a second —
/// a restart keeps one port per listener.
#[tokio::test]
async fn re_leasing_the_same_requirement_is_idempotent() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ports").await;
    let ws = bed.workspace(tenant).await;
    let node = node_with(&bed, tenant, Some((4000, 4010))).await;
    declare(&bed, tenant, ws, &[("web", "PORT", true)]).await;
    let state = bed.app_state().await;
    let s = session_on(&bed, tenant, node, Some(ws), "restarter").await;

    let first = port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("lease");
    let again = port_leases::lease_for(&state, tenant, node, Some(ws), s)
        .await
        .expect("lease again");
    assert_eq!(ports(&first), ports(&again));
    assert_eq!(
        state.sessions.leases_of(s).await.expect("leases").len(),
        1,
        "one row per listener, however many times it is leased"
    );

    bed.teardown().await;
}
