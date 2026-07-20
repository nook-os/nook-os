//! Multi-instance coordination tests: two `Registry` instances against one
//! Postgres, exercising lease takeover and cross-instance routing over the
//! LISTEN/NOTIFY bus.
//!
//! Needs a running Postgres: set `DATABASE_URL`. Skips cleanly without one so
//! `cargo test` stays green on a machine that has no database — except where
//! `NOOK_REQUIRE_DB` is set (CI), which turns the skip into a failure.

use std::sync::Arc;
use std::time::Duration;

use nook_control::ws::registry::{NodeHandle, OpPayload, Registry};
use nook_proto::ControlToNode;
use nook_types::{NodeId, TenantId};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::test_pool;

/// Insert a throwaway tenant + node row; returns the node id.
async fn seed_node(pool: &PgPool) -> (TenantId, NodeId) {
    let tenant = TenantId::new();
    let node = NodeId::new();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(tenant)
        .bind(format!("t-{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await
        .expect("seed tenant");
    sqlx::query(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $3, 'online')",
    )
    .bind(node)
    .bind(tenant)
    .bind(format!("n-{}", Uuid::now_v7().simple()))
    .execute(pool)
    .await
    .expect("seed node");
    (tenant, node)
}

async fn claim_lease(pool: &PgPool, node: NodeId, instance: Uuid) {
    sqlx::query(
        "UPDATE nodes SET owning_instance_id = $2,
            lease_expires_at = now() + interval '45 seconds'
         WHERE id = $1",
    )
    .bind(node)
    .bind(instance)
    .execute(pool)
    .await
    .expect("claim lease");
}

async fn cleanup(pool: &PgPool, tenant: TenantId) {
    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant)
        .execute(pool)
        .await;
}

/// Both instances see the current owner via the lease table, and a takeover
/// (node reconnects to the other instance) flips routing.
#[tokio::test]
async fn lease_takeover_flips_ownership() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let (tenant, node) = seed_node(&pool).await;

    let a = Arc::new(Registry::new());
    let b = Arc::new(Registry::new());

    // Instance A owns the node.
    claim_lease(&pool, node, a.instance_id()).await;
    a.refresh_lease_cache(&pool).await;
    b.refresh_lease_cache(&pool).await;
    assert!(a.node_online(node), "owner sees node online");
    assert!(b.node_online(node), "peer sees node online via lease");

    // Takeover: the node reconnects to B (last writer wins).
    claim_lease(&pool, node, b.instance_id()).await;
    a.refresh_lease_cache(&pool).await;
    b.refresh_lease_cache(&pool).await;
    assert!(a.node_online(node), "peer A sees node online via B's lease");

    // Expired lease means offline everywhere.
    sqlx::query("UPDATE nodes SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();
    a.refresh_lease_cache(&pool).await;
    b.refresh_lease_cache(&pool).await;
    assert!(!a.node_online(node), "expired lease reads offline");
    assert!(!b.node_online(node), "expired lease reads offline");

    cleanup(&pool, tenant).await;
}

/// A message sent on instance A for a node whose socket lives on instance B
/// crosses the bus and lands in B's local node channel.
#[tokio::test]
async fn send_to_node_routes_across_instances() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let (tenant, node) = seed_node(&pool).await;

    let a = Arc::new(Registry::new());
    let b = Arc::new(Registry::new());
    a.start_bus(pool.clone());
    b.start_bus(pool.clone());

    // B holds the node's "socket" (a test channel).
    let (tx, mut node_rx) = tokio::sync::mpsc::channel::<ControlToNode>(16);
    b.register_node(
        node,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    claim_lease(&pool, node, b.instance_id()).await;
    a.refresh_lease_cache(&pool).await;

    // A sends; the frame must arrive on B's channel via NOTIFY.
    assert!(a.send_to_node(node, ControlToNode::RescanWorkspaces));
    let got = tokio::time::timeout(Duration::from_secs(5), node_rx.recv())
        .await
        .expect("bus delivery timed out")
        .expect("channel open");
    assert!(matches!(got, ControlToNode::RescanWorkspaces));

    cleanup(&pool, tenant).await;
}

/// Request/response round-trip across instances: A issues request_op for a
/// node held by B; B's node answers; A's pending oneshot resolves.
#[tokio::test]
async fn op_reply_routes_back_to_requester() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set / postgres unreachable");
        return;
    };
    let (tenant, node) = seed_node(&pool).await;

    let a = Arc::new(Registry::new());
    let b = Arc::new(Registry::new());
    a.start_bus(pool.clone());
    b.start_bus(pool.clone());

    let (tx, mut node_rx) = tokio::sync::mpsc::channel::<ControlToNode>(16);
    b.register_node(
        node,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    claim_lease(&pool, node, b.instance_id()).await;
    a.refresh_lease_cache(&pool).await;

    // A asks for a clone on B's node.
    let rx = a
        .request_op(node, |request_id| ControlToNode::CloneRepo {
            request_id,
            url: "https://example.com/repo.git".into(),
            dest_name: None,
            ssh_key: None,
        })
        .expect("request routed");

    // B's "node" receives the request and answers through B's registry —
    // exactly what ws/node.rs does with a real OpResult frame.
    let request_id = match tokio::time::timeout(Duration::from_secs(5), node_rx.recv())
        .await
        .expect("request delivery timed out")
        .expect("channel open")
    {
        ControlToNode::CloneRepo { request_id, .. } => request_id,
        other => panic!("unexpected message: {other:?}"),
    };
    b.complete_op(
        request_id,
        OpPayload {
            ok: true,
            path: Some("/workspace/repo".into()),
            message: "cloned".into(),
        },
    );

    // A's oneshot resolves with the payload.
    let op = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("reply timed out")
        .expect("oneshot open");
    assert!(op.ok);
    assert_eq!(op.message, "cloned");
    assert_eq!(op.path.as_deref(), Some("/workspace/repo"));

    cleanup(&pool, tenant).await;
}
