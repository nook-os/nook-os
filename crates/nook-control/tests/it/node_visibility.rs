//! MAIN-132: `GET /nodes` and `GET /nodes/{id}` scope by role. A member sees
//! only nodes whose `owner_person_id` is theirs (a non-owned id 404s); a tenant
//! owner/admin sees the whole fleet; a node token's view is unchanged; and
//! `/auth/me` carries the caller's role + person_id. Set `DATABASE_URL`.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::state::AppState;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

#[tokio::test]
async fn members_see_only_their_own_admins_see_all() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping node-visibility test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nv").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, member_person) = bed.user(tenant, "member").await;

    let my_node = bed.node(tenant, member_person).await;
    let owner_node = bed.node(tenant, owner_person).await;

    // The member sees ONLY their own node.
    let mine = nook_control::routes::nodes::list(State(state.clone()), user_ctx(member, tenant))
        .await
        .expect("list")
        .0;
    let mine_ids: Vec<NodeId> = mine.iter().map(|n| n.id).collect();
    assert!(mine_ids.contains(&my_node), "member sees their own node");
    assert!(
        !mine_ids.contains(&owner_node),
        "member must NOT see a teammate's node"
    );

    // The owner (admin role) sees both.
    let all = nook_control::routes::nodes::list(State(state.clone()), user_ctx(owner, tenant))
        .await
        .expect("list")
        .0;
    let all_ids: Vec<NodeId> = all.iter().map(|n| n.id).collect();
    assert!(
        all_ids.contains(&my_node) && all_ids.contains(&owner_node),
        "an owner/admin sees the whole fleet"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn get_one_404s_for_a_member_on_a_non_owned_node() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nv").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, member_person) = bed.user(tenant, "member").await;

    let my_node = bed.node(tenant, member_person).await;
    let owner_node = bed.node(tenant, owner_person).await;

    // A member reading their own node: ok. Reading a teammate's: 404.
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(member, tenant),
            Path(my_node)
        )
        .await
        .is_ok(),
        "member may read their own node"
    );
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(member, tenant),
            Path(owner_node)
        )
        .await
        .is_err(),
        "a non-owned node 404s for a member"
    );
    // The owner may read the member's node.
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(owner, tenant),
            Path(my_node)
        )
        .await
        .is_ok(),
        "an owner/admin may read any node in the tenant"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_node_token_sees_the_whole_fleet_unchanged() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nv").await;
    let (_owner, owner_person) = bed.user(tenant, "owner").await;
    let (_member, member_person) = bed.user(tenant, "member").await;

    let a = bed.node(tenant, owner_person).await;
    let b = bed.node(tenant, member_person).await;

    // A node credential's listing is unchanged (whole tenant), per AC-1.
    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: tenant,
        principal: Principal::Node(a),
        cookie_session: false,
    };
    let listed = nook_control::routes::nodes::list(State(state.clone()), node_ctx)
        .await
        .expect("list")
        .0;
    let ids: Vec<NodeId> = listed.iter().map(|n| n.id).collect();
    assert!(
        ids.contains(&a) && ids.contains(&b),
        "a node token still sees every node in its tenant"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn me_carries_role_and_person_id() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nv").await;
    let (member, member_person) = bed.user(tenant, "member").await;

    let me = nook_control::routes::auth::me(State(state.clone()), user_ctx(member, tenant))
        .await
        .expect("me")
        .0;
    assert_eq!(
        me.person_id, member_person,
        "me carries the caller's person id"
    );
    assert_eq!(
        me.user.role, "member",
        "me carries the caller's tenant role"
    );

    bed.teardown().await;
}

// ── MAIN-135: shared nodes (owner-set flag + team visibility) ─────────────────

/// An ownerless node (owner_person_id NULL), for the "cannot share" case.
async fn add_ownerless_node(db: &DbPool, tenant: TenantId) -> NodeId {
    let id = NodeId::new();
    db.exec(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $4, 'offline')",
        params![
            id,
            tenant,
            format!("n-{}", id.0.simple()),
            format!("h-{}", id.0.simple())
        ],
    )
    .await
    .expect("ownerless node");
    id
}

async fn set_shared(
    state: &AppState,
    ctx: AuthCtx,
    node: NodeId,
    shared: bool,
) -> Result<Node, nook_control::error::ApiError> {
    nook_control::routes::nodes::set_shared(
        State(state.clone()),
        ctx,
        Path(node),
        axum::Json(SetSharedRequest { shared }),
    )
    .await
    .map(|j| j.0)
}

async fn member_sees(state: &AppState, member: UserId, tenant: TenantId, node: NodeId) -> bool {
    nook_control::routes::nodes::list(State(state.clone()), user_ctx(member, tenant))
        .await
        .expect("list")
        .0
        .iter()
        .any(|n| n.id == node)
}

#[tokio::test]
async fn sharing_a_node_makes_it_visible_to_the_team_and_unsharing_reverts() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping shared-node test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nv").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, _member_person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, owner_person).await;

    // Before sharing: the member cannot see the owner's node (MAIN-132).
    assert!(
        !member_sees(&state, member, tenant, node).await,
        "an unshared, non-owned node is invisible to the member"
    );
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(member, tenant),
            Path(node)
        )
        .await
        .is_err(),
        "single-get of an unshared, non-owned node 404s"
    );

    // The owner shares it.
    let updated = set_shared(&state, user_ctx(owner, tenant), node, true)
        .await
        .expect("owner may share their node");
    assert!(updated.shared, "the flag is set");

    // Now the member sees it in the list and single-get (AC-4).
    assert!(
        member_sees(&state, member, tenant, node).await,
        "a shared node is visible to the team"
    );
    assert!(
        nook_control::routes::nodes::get_one(
            State(state.clone()),
            user_ctx(member, tenant),
            Path(node)
        )
        .await
        .is_ok(),
        "single-get of a shared node succeeds for the team"
    );

    // Unsharing reverts visibility (AC-6).
    set_shared(&state, user_ctx(owner, tenant), node, false)
        .await
        .expect("owner may unshare");
    assert!(
        !member_sees(&state, member, tenant, node).await,
        "unsharing removes the node from the member's view again"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn only_the_owner_may_toggle_shared() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("nv").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, _) = bed.user(tenant, "member").await;
    let (admin, _) = bed.user(tenant, "admin").await;

    // A shared node the member can SEE but does not own → 403 on toggle.
    let shared_node = bed.node(tenant, owner_person).await;
    set_shared(&state, user_ctx(owner, tenant), shared_node, true)
        .await
        .expect("owner shares");
    assert!(
        set_shared(&state, user_ctx(member, tenant), shared_node, false)
            .await
            .is_err(),
        "a non-owner who can see the node still cannot toggle it (403)"
    );

    // A node the member CANNOT see (not owned, not shared) → 404, no oracle.
    let hidden_node = bed.node(tenant, owner_person).await;
    assert!(
        set_shared(&state, user_ctx(member, tenant), hidden_node, true)
            .await
            .is_err(),
        "toggling an invisible node is refused (404 — no existence oracle)"
    );

    // An admin who is not the owner is refused — ownership, not role (AC-2).
    assert!(
        set_shared(&state, user_ctx(admin, tenant), shared_node, false)
            .await
            .is_err(),
        "an admin who does not own the node cannot share it"
    );

    // An ownerless node cannot be shared — nobody to consent (AC-3). Toggle as
    // the admin, who (unscoped) passes visibility and is caught by the owner gate.
    let orphan = add_ownerless_node(&bed.db(), tenant).await;
    assert!(
        set_shared(&state, user_ctx(admin, tenant), orphan, true)
            .await
            .is_err(),
        "an ownerless node cannot be shared"
    );

    bed.teardown().await;
}
