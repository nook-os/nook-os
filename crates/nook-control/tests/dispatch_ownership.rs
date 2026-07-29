//! MAIN-131: auto-dispatch places work only on the requester's OWN nodes.
//! `schedule::pick` filters candidates to nodes whose `owner_person_id` equals
//! the acting person; an unowned node is never chosen, however well-resourced,
//! and no acting identity (the MCP path) yields the no-eligible-node error
//! rather than a tenant-wide pick. Set `DATABASE_URL`.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use nook_control::state::AppState;
use nook_control::ws::registry::NodeHandle;
use nook_proto::ControlToNode;
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A node owned by `owner` with a chosen resource sample, then registered ONLINE
/// in the registry so `schedule::pick` treats it as a live candidate.
async fn online_node(state: &AppState, tenant: TenantId, owner: Uuid, free_mem_gb: u64) -> NodeId {
    let id = NodeId::new();
    let resources = serde_json::json!({
        "cpu_percent": 0.0,
        "mem_used": 0,
        "mem_total": free_mem_gb,
        "load_avg1": 0.0,
        "active_sessions": 0,
    });
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id, resources)
         VALUES ($1, $2, $3, $4, 'online', $5, $6)",
    )
    .bind(id)
    .bind(tenant)
    .bind(format!("n-{}", id.0.simple()))
    .bind(format!("h-{}", id.0.simple()))
    .bind(owner)
    .bind(resources)
    .execute(state.db.pg())
    .await
    .expect("node");
    let (tx, _rx) = tokio::sync::mpsc::channel::<ControlToNode>(4);
    state.registry.register_node(
        id,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    id
}

#[tokio::test]
async fn pick_chooses_an_owned_node_and_never_a_teammates() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping dispatch-ownership test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("di").await;
    let (me, my_person) = bed.user(tenant, "member").await;
    let (_teammate, their_person) = bed.user(tenant, "member").await;

    // My modest node vs the teammate's much bigger, idle one — both online.
    let mine = online_node(&state, tenant, my_person, 8).await;
    let _theirs = online_node(&state, tenant, their_person, 64).await;

    let picked = nook_control::services::schedule::pick(&state, tenant, Some(me), None)
        .await
        .expect("an owned node is online")
        .node_id();
    assert_eq!(
        picked, mine,
        "placement must choose my node, never the teammate's better-resourced one"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn no_owned_node_online_is_the_explicit_error() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("di").await;
    let (me, _my_person) = bed.user(tenant, "member").await;
    let (_teammate, their_person) = bed.user(tenant, "member").await;

    // Only the teammate has an online node; I own none.
    let _theirs = online_node(&state, tenant, their_person, 64).await;

    let err = nook_control::services::schedule::pick(&state, tenant, Some(me), None)
        .await
        .expect_err("I own no online node");
    assert!(
        err.to_string().contains("no eligible node of yours"),
        "the caller gets the explicit no-owned-node message, got: {err}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn no_acting_identity_is_refused_not_widened() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("di").await;
    let (_teammate, their_person) = bed.user(tenant, "member").await;

    // A node exists and is online, but the caller (MCP) has no acting person.
    let _theirs = online_node(&state, tenant, their_person, 64).await;

    let err = nook_control::services::schedule::pick(&state, tenant, None, None)
        .await
        .expect_err("no acting person → no eligible node");
    assert!(
        err.to_string().contains("no eligible node of yours"),
        "a None actor must be refused, never widened to a tenant-wide pick, got: {err}"
    );

    bed.teardown().await;
}
