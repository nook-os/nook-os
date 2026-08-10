//! MAIN-119: a node records its owner (person) — the join-token minter, else
//! the tenant owner — set at join/enroll and backfilled for existing rows.
//! Nothing is enforced yet; this only proves the value is stored and returned.
//! Set `DATABASE_URL`.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use axum::extract::State;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::state::AppState;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// Mint a join token as `minter`, returning its plaintext (created_by = minter).
async fn mint_token(state: &AppState, minter: UserId, tenant: TenantId) -> String {
    nook_control::routes::join::create_join_token(State(state.clone()), auth(minter, tenant))
        .await
        .expect("mint token")
        .0
        .token
}

async fn owner_of(db: &DbPool, node: NodeId) -> Option<Uuid> {
    db.query_scalar(
        "SELECT owner_person_id FROM nodes WHERE id = $1",
        params![node],
    )
    .await
    .expect("node owner")
}

/// Set (or clear) a node's `shared` flag, scoped to the one row (MAIN-136).
async fn set_shared_flag(db: &DbPool, node: NodeId, shared: bool) {
    db.exec(
        "UPDATE nodes SET shared = $2 WHERE id = $1",
        params![node, shared],
    )
    .await
    .expect("set shared");
}

/// A node with a chosen (or NULL) owner, so the spawn guard can be exercised
/// directly without a join. `TestBed::node` always sets an owner; this keeps the
/// ownerless (`None`) path the guard tests need.
async fn insert_node(db: &DbPool, tenant: TenantId, owner: Option<Uuid>) -> NodeId {
    let id = NodeId::new();
    db.exec(
        "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
         VALUES ($1, $2, $3, $4, 'offline', $5)",
        params![
            id,
            tenant,
            format!("n-{}", id.0.simple()),
            // unique — node_token_hash is UNIQUE
            format!("h-{}", id.0.simple()),
            owner
        ],
    )
    .await
    .expect("node");
    id
}

#[tokio::test]
async fn join_sets_owner_to_the_minter_or_tenant_owner() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping node-owner test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, member_person) = bed.user(tenant, "member").await;

    // A token minted by the MEMBER → the node is owned by the member's person
    // (not the tenant owner), proving the minter is used (AC-2).
    let token = mint_token(&state, member, tenant).await;
    let joined = nook_control::routes::join::join(
        State(state.clone()),
        Json(JoinRequest {
            token,
            name: format!("m-node-{}", Uuid::now_v7().simple()),
            hostname: "host-a".into(),
            platform: "linux".into(),
        }),
    )
    .await
    .expect("join")
    .0;
    assert_eq!(
        owner_of(&bed.db(), joined.node_id).await,
        Some(member_person),
        "the token minter's person owns the node"
    );

    // A LEGACY token (no recorded minter) falls back to the tenant owner (AC-2).
    let token = mint_token(&state, member, tenant).await;
    // Simulate a legacy token that recorded no minter. This tenant's only
    // still-unused token is the one just minted.
    bed.db()
        .exec(
            "UPDATE join_tokens SET created_by = NULL WHERE tenant_id = $1 AND used_at IS NULL",
            params![tenant],
        )
        .await
        .expect("null created_by");
    let joined = nook_control::routes::join::join(
        State(state.clone()),
        Json(JoinRequest {
            token,
            name: format!("legacy-node-{}", Uuid::now_v7().simple()),
            hostname: "host-b".into(),
            platform: "linux".into(),
        }),
    )
    .await
    .expect("legacy join")
    .0;
    assert_eq!(
        owner_of(&bed.db(), joined.node_id).await,
        Some(owner_person),
        "a minterless token falls back to the tenant owner's person"
    );
    let _ = owner; // (the owner user exists so the fallback has a person to find)

    bed.teardown().await;
}

#[tokio::test]
async fn enroll_sets_owner_to_the_minter() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let (_owner, _) = bed.user(tenant, "owner").await;
    let (member, member_person) = bed.user(tenant, "member").await;

    // A CSR, exactly as the agent produces one.
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    let csr_pem = params.serialize_request(&key).unwrap().pem().unwrap();

    let token = mint_token(&state, member, tenant).await;
    // enroll mints the tenant CA on first use and returns the signed cert; we
    // only care that the node it creates is owned by the minter.
    let _ = nook_control::routes::join::enroll(
        State(state.clone()),
        Json(EnrollRequest {
            token,
            csr_pem,
            name: Some(format!("enrolled-{}", Uuid::now_v7().simple())),
        }),
    )
    .await
    .expect("enroll");

    let owner: Option<Uuid> = bed.db().query_scalar("SELECT owner_person_id FROM nodes WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 1", params![tenant])
    .await
    .expect("enrolled node");
    assert_eq!(
        owner,
        Some(member_person),
        "enroll records the minter as owner"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn backfill_fills_ownerless_nodes_with_the_tenant_owner() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("no").await;
    let (_owner, owner_person) = bed.user(tenant, "owner").await;

    // An owner-less node, as an existing row would be before the migration.
    let node = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status)
         VALUES ($1, $2, $3, $4, 'offline')",
            params![
                node,
                tenant,
                format!("legacy-{}", node.0.simple()),
                format!("h-{}", node.0.simple())
            ],
        )
        .await
        .expect("ownerless node");
    assert_eq!(owner_of(&bed.db(), node).await, None, "starts owner-less");

    // The 0016 backfill statement (still scoped to this node): resolves the
    // tenant owner's person. `AS n` rather than a bare `n`, because SQLite's
    // UPDATE grammar requires the keyword and Postgres accepts it either way —
    // one spelling both engines read (MAIN-472).
    bed.db()
        .exec(
            "UPDATE nodes AS n SET owner_person_id = (
             SELECT u.person_id FROM users u
             WHERE u.tenant_id = n.tenant_id AND u.role = 'owner'
             ORDER BY u.created_at LIMIT 1)
         WHERE n.owner_person_id IS NULL AND n.id = $1",
            params![node],
        )
        .await
        .expect("backfill");

    assert_eq!(
        owner_of(&bed.db(), node).await,
        Some(owner_person),
        "backfill resolves the tenant owner's person"
    );

    bed.teardown().await;
}

// ── MAIN-130: session-start authorization (owner-only, no exceptions) ─────────

/// The shared guard: a session spawns only for the person who owns the node.
/// The owner passes; a member, an admin, and an ownerless node all refuse; and
/// the MCP path (no acting identity) is refused rather than run as the tenant
/// owner.
#[tokio::test]
async fn spawn_guard_is_owner_only_with_no_role_bypass() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, _) = bed.user(tenant, "member").await;
    let (admin, _) = bed.user(tenant, "admin").await;

    let node = insert_node(&bed.db(), tenant, Some(owner_person)).await;

    // The owner may spawn.
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(owner), node)
            .await
            .is_ok(),
        "the node's owner must be allowed to start a session"
    );
    // A member who does not own it is refused — this is the vuln, closed.
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(member), node)
            .await
            .is_err(),
        "a non-owner member reached a teammate's machine"
    );
    // An admin gets NO bypass (NG-3, AC-2).
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(admin), node)
            .await
            .is_err(),
        "admin/owner roles must not bypass node ownership"
    );
    // The MCP path carries no acting identity → refused, never the tenant owner.
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, None, node)
            .await
            .is_err(),
        "a session with no acting identity (MCP) must be refused (AC-3)"
    );

    // An ownerless node refuses everyone, including the tenant owner.
    let orphan = insert_node(&bed.db(), tenant, None).await;
    assert!(
        nook_control::auth::require_person_owns_node(&state, tenant, Some(owner), orphan)
            .await
            .is_err(),
        "an ownerless node must refuse everyone"
    );

    bed.teardown().await;
}

/// The two direct session-spawn routes refuse a non-owner. The guard runs
/// before any session machinery, so a member is turned away by
/// `POST /sessions` and `POST /nodes/{id}/terminal` alike.
#[tokio::test]
async fn session_routes_refuse_a_non_owner() {
    use axum::extract::Path;
    use axum::Json;

    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let (_owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, _) = bed.user(tenant, "member").await;
    let node = insert_node(&bed.db(), tenant, Some(owner_person)).await;

    // POST /sessions as the member → refused before create_session.
    let created = nook_control::routes::sessions::create(
        State(state.clone()),
        auth(member, tenant),
        Json(CreateSessionRequest {
            workspace_id: WorkspaceId::new(),
            node_id: node,
            runtime: "bash".into(),
            name: None,
            path: None,
        }),
    )
    .await;
    assert!(
        created.is_err(),
        "a non-owner must not be able to start a session via POST /sessions"
    );

    // POST /nodes/{id}/terminal as the member → refused.
    let opened = nook_control::routes::sessions::open_terminal(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        None,
    )
    .await;
    assert!(
        opened.is_err(),
        "a non-owner must not be able to open a terminal via /nodes/{{id}}/terminal"
    );

    bed.teardown().await;
}

/// A node credential keeps its machine-only confinement at the spawn routes —
/// it may act on itself, not on a peer (AC-4). This is the same lateral-movement
/// boundary `require_node_self` guards, preserved through the new owner guard.
#[tokio::test]
async fn a_node_credential_still_reaches_only_itself() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let self_id = NodeId::new();
    let other_id = NodeId::new();
    let node_ctx = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(self_id.0),
        tenant_id: tenant,
        principal: Principal::Node(self_id),
        cookie_session: false,
    };
    assert!(
        node_ctx.require_node_owner(&state, self_id).await.is_ok(),
        "a node must still be able to act on itself"
    );
    assert!(
        node_ctx.require_node_owner(&state, other_id).await.is_err(),
        "a node token must not reach another machine"
    );

    bed.teardown().await;
}

// ── MAIN-136: shared nodes are usable (owner-or-shared), management owner-only ──

/// The asymmetry the epic step is about: a member may START a session on a node
/// its owner has SHARED, but may never MANAGE it. Owner-or-shared for use;
/// owner-only for the share toggle. Sharing grants no bypass of the
/// missing-identity rule, so the no-actor MCP path is refused even on a shared
/// node. Unsharing turns new starts away again (existing sessions untouched).
#[tokio::test]
async fn shared_node_relaxes_use_but_not_management() {
    use axum::extract::Path;

    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, _) = bed.user(tenant, "member").await;
    let node = insert_node(&bed.db(), tenant, Some(owner_person)).await;

    // Unshared: only the owner may use it. The member is refused, and the
    // message now names shared machines as the exception (AC-1).
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(owner), node)
            .await
            .is_ok(),
        "the owner may always use their own node"
    );
    let refused =
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(member), node)
            .await
            .expect_err("a member cannot use an UNSHARED teammate node");
    assert!(
        format!("{refused:?}").contains("shared"),
        "the refusal names shared machines as the exception, got {refused:?}"
    );

    // Share it → the member may now use it, via the free helper AND the AuthCtx
    // method the session-start routes actually call.
    set_shared_flag(&bed.db(), node, true).await;
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(member), node)
            .await
            .is_ok(),
        "a member may use a SHARED teammate node (AC-2)"
    );
    assert!(
        auth(member, tenant)
            .require_node_may_use(&state, node)
            .await
            .is_ok(),
        "the session-route method also allows a member on a shared node"
    );
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(owner), node)
            .await
            .is_ok(),
        "the owner is unaffected by sharing"
    );
    // Sharing grants NO bypass of the missing-identity rule: MCP (None) refused.
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, None, node)
            .await
            .is_err(),
        "a shared node still refuses the no-actor MCP path (AC-1)"
    );

    // NG-1: the member can USE the shared node but cannot MANAGE it — the share
    // toggle stays owner-only even though the member now sees and uses it.
    let toggled = nook_control::routes::nodes::set_shared(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        Json(SetSharedRequest { shared: false }),
    )
    .await;
    assert!(
        toggled.is_err(),
        "a non-owner must not toggle sharing, even on a node shared with them (NG-1)"
    );
    let still: bool = bed
        .db()
        .query_scalar("SELECT shared FROM nodes WHERE id = $1", params![node])
        .await
        .expect("shared flag");
    assert!(still, "the refused toggle must not have unshared the node");

    // Unshare → the member's NEW starts are refused again (AC-2).
    set_shared_flag(&bed.db(), node, false).await;
    assert!(
        nook_control::auth::require_person_may_use_node(&state, tenant, Some(member), node)
            .await
            .is_err(),
        "unsharing refuses the member's new session starts again (AC-2)"
    );

    bed.teardown().await;
}

// ── MAIN-126: node-scoped authorize gate ─────────────────────────────────────

/// A PERSONAL node is authorizable only by its owner; a SHARED node only by a
/// node-manager. Unauthorized callers are refused (403) BEFORE anything is
/// launched; an authorized caller passes the gate (and here fails only because
/// the test node is offline — never a 403).
#[tokio::test]
async fn authorize_is_owner_only_for_personal_and_manager_only_for_shared() {
    use axum::extract::Path;
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("no").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    let (member, _) = bed.user(tenant, "member").await;
    let node = insert_node(&bed.db(), tenant, Some(owner_person)).await;

    let req = || {
        Json(AuthorizeRuntimeRequest {
            runtime: "claude".into(),
        })
    };
    let forbidden = |e: &nook_control::error::ApiError| {
        matches!(
            e,
            nook_control::error::ApiError::Forbidden
                | nook_control::error::ApiError::ForbiddenMsg(_)
        )
    };

    // Personal node: a non-owner member is refused.
    let denied = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        denied.as_ref().err().is_some_and(forbidden),
        "a non-owner must not authorize a personal node: {denied:?}"
    );
    // The owner passes the gate — the only failure is the offline node, not a 403.
    let owner_try = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(owner, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        owner_try.as_ref().err().is_some_and(|e| !forbidden(e)),
        "the owner passes the personal-node gate: {owner_try:?}"
    );

    // Shared node: it now takes a node-manager, not the owner-as-person.
    set_shared_flag(&bed.db(), node, true).await;
    let member_shared = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        member_shared.as_ref().err().is_some_and(forbidden),
        "a member without node.manage must not authorize a shared node: {member_shared:?}"
    );
    // Grant node.manage (operator role) → passes the gate.
    bed.db()
        .exec(
            // Generated here rather than by `gen_random_uuid()`, which is
            // Postgres-only and is not what this test is about.
            "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id)
         VALUES ($1, 'user', $2, 'operator', 'deployment', NULL)",
            params![Uuid::now_v7(), member.0],
        )
        .await
        .unwrap();
    let member_admin = nook_control::routes::nodes::authorize(
        State(state.clone()),
        auth(member, tenant),
        Path(node),
        req(),
    )
    .await;
    assert!(
        member_admin.as_ref().err().is_some_and(|e| !forbidden(e)),
        "a node-manager passes the shared-node gate: {member_admin:?}"
    );

    bed.teardown().await;
}

/// MAIN-119's `COALESCE`, which nothing covered until MAIN-252 moved the
/// statement and went looking.
///
/// A machine that re-runs `nook join` — with anyone's token — must keep the
/// owner it already has. Without `COALESCE(nodes.owner_person_id, EXCLUDED.…)`
/// the upsert writes `EXCLUDED.owner_person_id` and the machine silently
/// changes hands, which then changes who may start sessions on it. Dropping
/// the COALESCE is a one-line edit that every other test in this file ignores.
#[tokio::test]
async fn a_re_join_with_someone_elses_token_does_not_transfer_the_machine() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping node-owner test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("rejoin").await;
    let (alice, alice_person) = bed.user(tenant, "member").await;
    let (bob, bob_person) = bed.user(tenant, "member").await;
    assert_ne!(alice_person, bob_person);

    let name = format!("workshop-{}", Uuid::now_v7().simple());
    let join_as = |user, name: String| {
        let state = state.clone();
        async move {
            let token = mint_token(&state, user, tenant).await;
            nook_control::routes::join::join(
                State(state.clone()),
                Json(JoinRequest {
                    token,
                    name,
                    hostname: "host".into(),
                    platform: "linux".into(),
                }),
            )
            .await
            .expect("join")
            .0
        }
    };

    let first = join_as(alice, name.clone()).await;
    assert_eq!(
        owner_of(&bed.db(), first.node_id).await,
        Some(alice_person),
        "alice enrolled it, so alice owns it"
    );

    // Bob re-enrols the SAME machine (same tenant + name) with his own token.
    let second = join_as(bob, name.clone()).await;
    assert_eq!(
        second.node_id, first.node_id,
        "same tenant + name is the same machine — ON CONFLICT heals the row"
    );
    assert_eq!(
        owner_of(&bed.db(), second.node_id).await,
        Some(alice_person),
        "a re-join must NOT hand alice's machine to bob"
    );

    bed.teardown().await;
}
