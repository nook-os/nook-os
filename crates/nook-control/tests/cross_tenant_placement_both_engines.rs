//! `owned_online_elsewhere` executes on WHICHEVER engine the bed is running
//! (MAIN-515) — the divergence guard for the one new query.
//!
//! Separate from `executor_selection.rs` deliberately, and it is worth saying
//! why rather than leaving it to look like taste. That binary is on the SQLite
//! CI allow-list because `eligible_loop_executors` itself does not run on
//! SQLite yet: it builds `FROM json_each(…) e WHERE json_extract(e, …)`, and on
//! SQLite the alias of a table-valued function is not a column, so every test
//! that reaches placement dies with `no such column: e`. Fixing that is the
//! dialect sweep's, not this card's — but the query THIS card adds is
//! engine-neutral as written, and it would be dishonest to file it behind a
//! failure it does not share. So it gets a home both legs can run.
//!
//! What keeps this file engine-neutral: every insert binds through `params!`
//! (no raw sqlx), no interval arithmetic, and the only JSON it touches is the
//! `shared_operator` containment test, which the seam already renders for both.

use nook_db::{params, Db};
use nook_types::*;
use uuid::Uuid;

use nook_testkit::TestBed;

/// A node with an explicit tenant, owner, status and capabilities.
async fn node(
    bed: &TestBed,
    tenant: TenantId,
    owner: Option<Uuid>,
    status: &str,
    operator: bool,
) -> NodeId {
    let id = NodeId::new();
    let caps = if operator {
        serde_json::json!({ "shared_operator": true })
    } else {
        serde_json::json!({})
    };
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status,
                                owner_person_id, capabilities)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            params![
                id,
                tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                status.to_string(),
                owner,
                caps
            ],
        )
        .await
        .expect("node");
    id
}

/// The split the queued reason turns on: what crosses to me here, and what was
/// refused because of where it is joined. Both counts, on both engines.
#[tokio::test]
async fn owned_online_elsewhere_splits_crossing_from_refused() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let a = bed.tenant("xta").await;
    let b = bed.tenant("xtb").await;
    let (_user, me) = bed.user(a, "owner").await;
    let someone_else = Uuid::now_v7();

    node(&bed, a, Some(me), "online", false).await;
    node(&bed, a, Some(me), "online", true).await;
    // Neither of these is mine-and-online-elsewhere.
    node(&bed, a, Some(me), "offline", false).await;
    node(&bed, a, Some(someone_else), "online", false).await;
    node(&bed, a, None, "online", true).await;
    // In B itself, so not "elsewhere" at all.
    node(&bed, b, Some(me), "online", false).await;

    let state = bed.app_state().await;
    assert_eq!(
        state
            .nodes
            .owned_online_elsewhere(b, me)
            .await
            .expect("elsewhere"),
        (1, 1),
        "one machine that crosses into B, one shared operator that does not"
    );
    assert_eq!(
        state
            .nodes
            .owned_online_elsewhere(a, me)
            .await
            .expect("elsewhere from A"),
        (1, 0),
        "…and read from A, the one I hold in B"
    );
    assert_eq!(
        state
            .nodes
            .owned_online_elsewhere(b, someone_else)
            .await
            .expect("elsewhere for them"),
        (1, 0),
        "the split is per-person: none of my rows land in their counts"
    );

    bed.teardown().await;
}

/// A person with everything at home has nothing elsewhere — the ordinary
/// single-tenant case, which must stay a plain zero rather than an accident of
/// the `<>` comparison on a TEXT tenant id.
#[tokio::test]
async fn a_single_tenant_owner_has_nothing_elsewhere() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let t = bed.tenant("solo").await;
    let (_user, me) = bed.user(t, "owner").await;
    node(&bed, t, Some(me), "online", false).await;
    node(&bed, t, None, "online", true).await;

    let state = bed.app_state().await;
    assert_eq!(
        state
            .nodes
            .owned_online_elsewhere(t, me)
            .await
            .expect("elsewhere"),
        (0, 0)
    );

    bed.teardown().await;
}
