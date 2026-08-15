//! The machine that enrolled before anybody existed becomes the first person's
//! (MAIN-398).
//!
//! A desktop install joins its bundled node on first launch, which is by
//! definition before the person has claimed the instance — so the node lands
//! with `owner_person_id` NULL. `require_person_may_use_node` refuses an
//! owner-less node to EVERYONE, so without this the app would show one node
//! online and refuse every session on it. `tests/desktop_local_session.rs`
//! proves the whole path with real processes; this one pins the rule, and the
//! bound on it — a machine somebody already owns is never reassigned.

use nook_control::auth::require_person_may_use_node;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

async fn insert_node(db: &DbPool, tenant: TenantId, owner: Option<Uuid>) -> NodeId {
    let id = NodeId::new();
    db.exec(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
         VALUES ($1, $2, $3, $4, 'online', $5)",
        params![
            id,
            tenant,
            format!("n-{}", id.0.simple()),
            format!("h-{}", id.0.simple()),
            owner
        ],
    )
    .await
    .expect("node");
    id
}

async fn owner_of(db: &DbPool, node: NodeId) -> Option<Uuid> {
    db.query_scalar(
        "SELECT owner_person_id FROM nodes WHERE id = $1",
        params![node],
    )
    .await
    .expect("node owner")
}

#[tokio::test]
async fn the_first_person_adopts_the_machine_that_joined_before_them() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("local-install").await;
    let bundled = insert_node(&bed.db(), tenant, None).await;
    let (claimer, claimer_person) = bed.user(tenant, "owner").await;

    // The state a desktop first launch is actually in: an online node nobody
    // can use.
    assert!(
        require_person_may_use_node(&state, tenant, Some(claimer), bundled)
            .await
            .is_err(),
        "an owner-less node must be refused — that is the bug being fixed"
    );

    let moved = state
        .nodes
        .adopt_ownerless(tenant, claimer_person)
        .await
        .expect("adopt");
    assert_eq!(moved, 1);
    assert_eq!(owner_of(&bed.db(), bundled).await, Some(claimer_person));
    require_person_may_use_node(&state, tenant, Some(claimer), bundled)
        .await
        .expect("the person who claimed the instance can now start a session on it");

    bed.teardown().await;
}

/// The bound: adoption only ever fills a NULL. A machine with an owner is not
/// handed to whoever signs in next, and neither is another tenant's.
#[tokio::test]
async fn adoption_never_takes_a_machine_that_already_has_an_owner() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("mine").await;
    let other = bed.tenant("theirs").await;
    let (_, mine) = bed.user(tenant, "owner").await;
    let (_, theirs) = bed.user(other, "owner").await;

    let already_owned = insert_node(&bed.db(), tenant, Some(theirs)).await;
    let elsewhere = insert_node(&bed.db(), other, None).await;
    let ownerless = insert_node(&bed.db(), tenant, None).await;

    assert_eq!(
        state
            .nodes
            .adopt_ownerless(tenant, mine)
            .await
            .expect("adopt"),
        1,
        "only the owner-less node in this tenant moves"
    );
    assert_eq!(owner_of(&bed.db(), ownerless).await, Some(mine));
    assert_eq!(
        owner_of(&bed.db(), already_owned).await,
        Some(theirs),
        "an owned machine is never reassigned"
    );
    assert_eq!(
        owner_of(&bed.db(), elsewhere).await,
        None,
        "another tenant's machine is out of reach"
    );

    bed.teardown().await;
}
