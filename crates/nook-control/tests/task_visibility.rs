//! Per-task visibility (MAIN-76): the read filter, the assignee share, the
//! claim guard, board-detail filtering, the update-authority guard, and the
//! operator title exclusion — all through the real code paths against a live
//! Postgres. Set `DATABASE_URL`.

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::config::Config;
use nook_control::error::ApiError;
use nook_control::routes::task_query::{claim_inner, query_rows, TaskFilter};
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_control::state::AppState;
use nook_types::*;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

mod common;
use common::test_pool;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn test_config() -> Config {
    Config {
        app_env: "test".into(),
        bind: "127.0.0.1:0".into(),
        shutdown_grace_secs: 25,
        public_base_url: "http://localhost:8080".into(),
        web_origin: "http://localhost:5173".into(),
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        oidc_issuer_url: None,
        oidc_client_id: None,
        oidc_device_client_id: None,
        oidc_device_authorization_endpoint: None,
        oidc_client_secret: None,
        oidc_redirect_url: None,
        oidc_scopes: "openid profile email".into(),
        session_secret: "0".repeat(64),
        session_ttl_hours: 168,
        default_tenant_name: format!("test-{}", Uuid::now_v7().simple()),
        auth_dev_mode: true,
        mcp_token: None,
        dev_join_token: None,
        dist_dir: "/nonexistent".into(),
        agent_bind: "127.0.0.1:0".into(),
        agent_public_url: None,
        agent_tls_cert: None,
        agent_tls_key: None,
        releases_repo: "nook-os/nook-os".into(),
        artifact_store: "disk".into(),
        artifact_prefix: "nook".into(),
        artifact_redirect: false,
        s3_bucket: None,
        s3_endpoint: None,
        s3_region: None,
        s3_access_key_id: None,
        s3_secret_access_key: None,
        s3_path_style: true,
        cache_provider: "memory".into(),
        mail_provider: "capture".into(),
        smtp_host: None,
        smtp_port: 587,
        smtp_tls: "starttls".into(),
        smtp_from: "NookOS <no-reply@localhost>".into(),
        smtp_username: None,
        smtp_password: None,
        postmark_token: None,
        postmark_api_url: "https://api.postmarkapp.com/email".into(),
        mail_from: "NookOS <no-reply@localhost>".into(),
        mail_send_enabled: false,
        mail_notifications_enabled: false,
        mail_max_per_month: Some(100),
        mail_max_per_day: None,
        trusted_proxies: Vec::new(),
    }
}

fn claims(subject: &str, name: &str) -> IdentityClaims {
    IdentityClaims {
        issuer: "test-idp".into(),
        subject: subject.into(),
        email: Some(format!("{subject}@example.test")),
        email_verified: false,
        display_name: Some(name.into()),
        avatar_url: None,
        raw_claims: serde_json::json!({}),
    }
}

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// Add another user to an existing tenant (a distinct person), the way a real
/// second/third member would exist. Returns their per-tenant user id.
async fn add_member(db: &PgPool, tenant: TenantId, name: &str, role: &str) -> UserId {
    let id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
         VALUES ($1, $2, gen_random_uuid(), $3, $4, $5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(name)
    .bind(format!("{}-{}@example.test", name, id.0.simple()))
    .bind(role)
    .execute(db)
    .await
    .expect("member");
    id
}

/// Create a task on `board` via the real provider, with a creator + visibility.
async fn make_task(
    state: &AppState,
    tenant: TenantId,
    board: BoardId,
    creator: UserId,
    title: &str,
    visibility: &str,
) -> TaskItem {
    let provider = state.kanban.get("local").expect("local provider");
    provider
        .create_task(
            tenant,
            board,
            Some(creator),
            CreateTaskRequest {
                title: title.into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: None,
                priority: None,
                type_: None,
                visibility: Some(visibility.into()),
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("create task")
}

async fn list_ids(
    state: &AppState,
    tenant: TenantId,
    viewer: UserId,
    board: BoardId,
) -> Vec<TaskId> {
    let f = TaskFilter {
        board: Some(board.to_string()),
        limit: Some(200),
        ..Default::default()
    };
    query_rows(&state.db, tenant, viewer, &f)
        .await
        .expect("list")
        .into_iter()
        .map(|t| t.id)
        .collect()
}

#[tokio::test]
async fn visibility_governs_read_claim_board_and_update() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping task-visibility test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    // One tenant, three people: A owns it (also the creator under test), B and C
    // are ordinary members.
    let sub = format!("owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Owner"))
        .await
        .expect("owner signs in");
    let a = owner.id;
    let b = add_member(&pool, tenant.id, "bob", "member").await;
    let c = add_member(&pool, tenant.id, "carol", "member").await;

    // A board to hang cards on.
    let board: BoardId = sqlx::query_scalar(
        "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
    )
    .bind(BoardId::new())
    .bind(tenant.id)
    .bind(format!("V{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase())
    .fetch_one(&pool)
    .await
    .expect("board");
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Todo', 0, 'unstarted')",
    )
    .bind(Uuid::now_v7())
    .bind(board)
    .execute(&pool)
    .await
    .expect("column");

    // A team card (the default) and a private card, both created by A.
    let team = make_task(&state, tenant.id, board, a, "team card", "team").await;
    let private = make_task(&state, tenant.id, board, a, "secret", "private").await;

    // ── Read (AC-3): team is visible to everyone; private only to A ─────────
    let a_ids = list_ids(&state, tenant.id, a, board).await;
    assert!(
        a_ids.contains(&team.id) && a_ids.contains(&private.id),
        "A sees both"
    );
    for other in [b, c] {
        let ids = list_ids(&state, tenant.id, other, board).await;
        assert!(
            ids.contains(&team.id),
            "team card is visible to every member"
        );
        assert!(
            !ids.contains(&private.id),
            "private card is hidden from non-owners"
        );
    }

    // ── Assignee share (AC-7): assign the private card to C → C now sees it ──
    sqlx::query("UPDATE tasks SET assignee_user_id = $2 WHERE id = $1")
        .bind(private.id)
        .bind(c)
        .execute(&pool)
        .await
        .expect("assign");
    assert!(
        list_ids(&state, tenant.id, c, board)
            .await
            .contains(&private.id),
        "the assignee sees the private card"
    );
    assert!(
        !list_ids(&state, tenant.id, b, board)
            .await
            .contains(&private.id),
        "a third member still does not"
    );

    // ── Board detail (AC-3) filters the same way ────────────────────────────
    let detail = nook_control::routes::boards::get_one(
        State(state.clone()),
        auth(b, tenant.id),
        Path(board),
    )
    .await
    .expect("board detail")
    .0;
    let detail_ids: Vec<TaskId> = detail.tasks.iter().map(|t| t.id).collect();
    assert!(
        detail_ids.contains(&team.id),
        "board shows the team card to B"
    );
    assert!(
        !detail_ids.contains(&private.id),
        "board hides the private card from B"
    );

    // ── Claim (AC-9): team claimable by any member; private only by owner ───
    // A fresh unclaimed private card owned by A.
    let p2 = make_task(&state, tenant.id, board, a, "mine only", "private").await;
    assert!(
        claim_inner(&state, tenant.id, b, &p2.id.to_string(), None)
            .await
            .is_err(),
        "B's agent cannot claim A's private card, even by id"
    );
    assert!(
        claim_inner(&state, tenant.id, a, &p2.id.to_string(), None)
            .await
            .is_ok(),
        "A can claim their own private card"
    );
    // A fresh team card is claimable by any member's agent.
    let t2 = make_task(&state, tenant.id, board, a, "shared work", "team").await;
    assert!(
        claim_inner(&state, tenant.id, b, &t2.id.to_string(), None)
            .await
            .is_ok(),
        "a team card is claimable by another user's agent (visibility never blocks shared work)"
    );

    // ── Update authority (AC-5): non-owner cannot alter a private card ──────
    let deny = nook_control::routes::boards::update_task(
        State(state.clone()),
        auth(b, tenant.id),
        Path(p2.id.to_string()),
        axum::Json(UpdateUserVis::title("hijack")),
    )
    .await;
    assert!(deny.is_err(), "B cannot update A's private card");
    // The creator can change its visibility.
    let ok = nook_control::routes::boards::update_task(
        State(state.clone()),
        auth(a, tenant.id),
        Path(p2.id.to_string()),
        axum::Json(UpdateUserVis::visibility("team")),
    )
    .await
    .expect("A changes its own card")
    .0;
    assert_eq!(ok.visibility, "team", "creator set it to team");

    // ── Relations must not leak a private task (MAIN-76 review DEFECT 2) ─────
    // A fresh private card owned by A, and a team card. A non-owner (B) must not
    // be able to link to the private card, and must not see it surface through a
    // relation on the team card.
    let secret2 = make_task(&state, tenant.id, board, a, "hush", "private").await;
    let shared = make_task(&state, tenant.id, board, a, "open work", "team").await;

    // B cannot create a relation to A's private card (NotFound).
    let leak = nook_control::routes::task_detail::link(
        &state, tenant.id, b, shared.id, secret2.id, "relates",
    )
    .await;
    assert!(leak.is_err(), "a non-owner cannot link to a private card");

    // A (the owner) can link them.
    nook_control::routes::task_detail::link(&state, tenant.id, a, shared.id, secret2.id, "relates")
        .await
        .expect("owner links its own cards");

    // Now the shared card's detail: A sees the private relation; B does not.
    let a_detail = nook_control::routes::task_detail::detail(&state, tenant.id, a, shared.id)
        .await
        .expect("A detail");
    assert!(
        a_detail.related.iter().any(|r| r.id == secret2.id),
        "the owner sees the private related card"
    );
    let b_detail = nook_control::routes::task_detail::detail(&state, tenant.id, b, shared.id)
        .await
        .expect("B detail");
    assert!(
        !b_detail.related.iter().any(|r| r.id == secret2.id),
        "a non-owner never sees the private related card's title/key"
    );

    // ── Operator exclusion (AC-4): the titles projection omits private ──────
    let titles: Vec<String> = sqlx::query_scalar(
        "SELECT title FROM tasks WHERE tenant_id = $1 AND visibility <> 'private' ORDER BY title",
    )
    .bind(tenant.id)
    .fetch_all(&pool)
    .await
    .expect("titles");
    assert!(
        titles.contains(&"team card".to_string()),
        "team titles are visible to operators"
    );
    assert!(
        !titles.contains(&"secret".to_string()),
        "private titles never reach an operator"
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant.id)
        .execute(&pool)
        .await;
}

/// MAIN-85: changing a card's `visibility` is gated to the card's owner
/// (creator ∪ assignee) or a tenant owner/admin. Everyone else is refused 403,
/// with the whole PATCH dropped — while every OTHER edit a member could already
/// make stays exactly as permissive as before. Drives the real `update_task`
/// route so the gate is tested where it lives, against live Postgres.
#[tokio::test]
async fn visibility_change_is_gated_to_owner_or_admin() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping visibility-gate test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    // A owns the tenant and creates the cards. B is an ordinary member (neither
    // creator nor assignee nor admin). ADM is a tenant admin who is likewise
    // neither creator nor assignee. ASG will be made a card's assignee.
    let sub = format!("owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Owner"))
        .await
        .expect("owner signs in");
    let a = owner.id;
    let b = add_member(&pool, tenant.id, "bob", "member").await;
    let adm = add_member(&pool, tenant.id, "admin", "admin").await;
    let asg = add_member(&pool, tenant.id, "asa", "member").await;

    let board: BoardId = sqlx::query_scalar(
        "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
    )
    .bind(BoardId::new())
    .bind(tenant.id)
    .bind(format!("G{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase())
    .fetch_one(&pool)
    .await
    .expect("board");
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Todo', 0, 'unstarted')",
    )
    .bind(Uuid::now_v7())
    .bind(board)
    .execute(&pool)
    .await
    .expect("column");

    // Helper: current visibility of a card, read straight from the row.
    async fn vis(pool: &PgPool, id: TaskId) -> String {
        sqlx::query_scalar("SELECT visibility FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("visibility")
    }
    async fn title_of(pool: &PgPool, id: TaskId) -> String {
        sqlx::query_scalar("SELECT title FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("title")
    }
    let patch = |st: &AppState, who: UserId, id: TaskId, req: UpdateTaskRequest| {
        let st = st.clone();
        async move {
            nook_control::routes::boards::update_task(
                State(st),
                auth(who, tenant.id),
                Path(id.to_string()),
                axum::Json(req),
            )
            .await
        }
    };

    // ── AC-1: a non-owner, non-admin member cannot change scope ─────────────
    let team = make_task(&state, tenant.id, board, a, "shared plan", "team").await;
    let denied = patch(&state, b, team.id, UpdateUserVis::visibility("private")).await;
    assert!(
        matches!(denied, Err(ApiError::ForbiddenMsg(_))),
        "a non-owner, non-admin member is refused 403, got {denied:?}"
    );
    assert_eq!(
        vis(&pool, team.id).await,
        "team",
        "a refused visibility change leaves the card untouched"
    );

    // ── AC-1: the creator may change scope ──────────────────────────────────
    let asc = patch(&state, a, team.id, UpdateUserVis::visibility("org"))
        .await
        .expect("creator changes scope");
    assert_eq!(asc.0.visibility, "org", "creator set it to org");

    // ── AC-1: the assignee may change scope (owner = creator ∪ assignee) ─────
    sqlx::query("UPDATE tasks SET assignee_user_id = $2 WHERE id = $1")
        .bind(team.id)
        .bind(asg)
        .execute(&pool)
        .await
        .expect("assign");
    let by_assignee = patch(&state, asg, team.id, UpdateUserVis::visibility("team"))
        .await
        .expect("assignee changes scope");
    assert_eq!(by_assignee.0.visibility, "team", "assignee set it to team");

    // ── AC-1: a tenant admin (neither creator nor assignee) may change scope ─
    let admcard = make_task(&state, tenant.id, board, a, "admin can flip", "team").await;
    let by_admin = patch(
        &state,
        adm,
        admcard.id,
        UpdateUserVis::visibility("private"),
    )
    .await
    .expect("tenant admin changes scope");
    assert_eq!(by_admin.0.visibility, "private", "admin set it to private");

    // ── AC-2: the gate fires only on an actual change ───────────────────────
    // A non-owner member editing another field (title) with NO visibility set
    // still succeeds — other-field authority is unchanged (NG-4).
    let move_card = make_task(&state, tenant.id, board, a, "movable", "team").await;
    let field_only = patch(
        &state,
        b,
        move_card.id,
        UpdateUserVis::title("renamed by member"),
    )
    .await
    .expect("a member may edit a visible team card's other fields");
    assert_eq!(
        field_only.0.title, "renamed by member",
        "the title edit applied"
    );
    // A non-owner sending the SAME visibility value it already has is not blocked.
    let same_vis = patch(&state, b, move_card.id, UpdateUserVis::visibility("team"))
        .await
        .expect("round-tripping the current visibility is never gated");
    assert_eq!(same_vis.0.visibility, "team", "unchanged, and allowed");

    // ── AC-3: a refused change drops the WHOLE request, other fields included ─
    let before = title_of(&pool, move_card.id).await;
    let combo = patch(
        &state,
        b,
        move_card.id,
        UpdateUserVis::visibility_and_title("private", "hijacked title"),
    )
    .await;
    assert!(
        matches!(combo, Err(ApiError::ForbiddenMsg(_))),
        "changing visibility + title as a non-owner is refused 403, got {combo:?}"
    );
    assert_eq!(
        title_of(&pool, move_card.id).await,
        before,
        "the title in the refused request was NOT applied"
    );
    assert_eq!(
        vis(&pool, move_card.id).await,
        "team",
        "and neither was the visibility change"
    );

    // ── AC-4: a private card the caller cannot see is 404, never 403 ────────
    let secret = make_task(&state, tenant.id, board, a, "hidden", "private").await;
    let unseen = patch(&state, b, secret.id, UpdateUserVis::visibility("team")).await;
    assert!(
        matches!(unseen, Err(ApiError::NotFound)),
        "a private card is NotFound (404) to a non-owner, not a 403, got {unseen:?}"
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant.id)
        .execute(&pool)
        .await;
}

/// Small builders for `UpdateTaskRequest` in the two update assertions above.
struct UpdateUserVis;
impl UpdateUserVis {
    fn title(t: &str) -> UpdateTaskRequest {
        UpdateTaskRequest {
            title: Some(t.into()),
            ..blank_update()
        }
    }
    fn visibility(v: &str) -> UpdateTaskRequest {
        UpdateTaskRequest {
            visibility: Some(v.into()),
            ..blank_update()
        }
    }
    /// A PATCH that changes visibility AND another field in one request — the
    /// all-or-nothing case (MAIN-85 AC-3): a refusal must drop the title too.
    fn visibility_and_title(v: &str, t: &str) -> UpdateTaskRequest {
        UpdateTaskRequest {
            visibility: Some(v.into()),
            title: Some(t.into()),
            ..blank_update()
        }
    }
}

fn blank_update() -> UpdateTaskRequest {
    UpdateTaskRequest {
        title: None,
        description: None,
        column_id: None,
        column_type: None,
        position: None,
        assignee_user_id: None,
        priority: None,
        type_: None,
        visibility: None,
        workspace_id: None,
        parent: None,
        expected_updated_at: None,
    }
}

/// MAIN-86 AC-3: a child's derived `parent_key` is blanked for a viewer who
/// cannot see its PRIVATE parent epic, and restored the moment the parent
/// becomes visible — so a private epic's `BOARD-N` never leaks onto a card a
/// non-owner can open. Drives the real `enrich`, the one path every board and
/// detail read goes through.
#[tokio::test]
async fn private_parent_key_is_redacted_from_children_for_non_owners() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool().await else {
        eprintln!("skipping parent-key redaction test — no DATABASE_URL");
        return;
    };
    let state = AppState::new(pool.clone(), test_config(), None).await;

    // O owns the tenant and the epic; V is an ordinary member who is neither the
    // epic's creator nor its assignee.
    let sub = format!("owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Owner"))
        .await
        .expect("owner signs in");
    let o = owner.id;
    let v = add_member(&pool, tenant.id, "vic", "member").await;

    let board_key = format!("E{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase();
    let board: BoardId = sqlx::query_scalar(
        "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
    )
    .bind(BoardId::new())
    .bind(tenant.id)
    .bind(&board_key)
    .fetch_one(&pool)
    .await
    .expect("board");
    sqlx::query(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Todo', 0, 'unstarted')",
    )
    .bind(Uuid::now_v7())
    .bind(board)
    .execute(&pool)
    .await
    .expect("column");

    // A PRIVATE epic owned by O, and a TEAM child filed under it.
    let epic = make_task(&state, tenant.id, board, o, "roadmap", "private").await;
    let child = make_task(&state, tenant.id, board, o, "do the thing", "team").await;
    sqlx::query("UPDATE tasks SET parent_task_id = $2 WHERE id = $1")
        .bind(child.id)
        .bind(epic.id)
        .execute(&pool)
        .await
        .expect("file the child under the epic");
    let expected = format!("{board_key}-{}", epic.number.expect("epic has a number"));

    async fn row(pool: &PgPool, id: TaskId) -> TaskItem {
        sqlx::query_as("SELECT * FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("row")
    }
    let enrich_as = |viewer: UserId, t: TaskItem| {
        let db = state.db.clone();
        let base = state.cfg.public_base_url.clone();
        async move {
            nook_control::services::tasks::enrich(&db, &base, viewer, vec![t])
                .await
                .expect("enrich")
                .pop()
                .expect("one task")
        }
    };

    // O (the epic's owner) sees the parent key.
    let as_o = enrich_as(o, row(&pool, child.id).await).await;
    assert_eq!(
        as_o.parent_key.as_deref(),
        Some(expected.as_str()),
        "the owner sees the private epic's key on its child"
    );

    // V (non-owner) sees the child as parentless — the key is redacted.
    let as_v = enrich_as(v, row(&pool, child.id).await).await;
    assert_eq!(
        as_v.parent_key, None,
        "a private epic's key is not exposed on a child a non-owner can see"
    );
    // NG-1: only the DERIVED key is blanked — the row's parent linkage stands.
    assert_eq!(
        as_v.parent_task_id,
        Some(epic.id),
        "the parent_task_id itself is unchanged"
    );

    // Flip the epic to team-visible → the key returns for V (non-regression).
    sqlx::query("UPDATE tasks SET visibility = 'team' WHERE id = $1")
        .bind(epic.id)
        .execute(&pool)
        .await
        .expect("flip epic to team");
    let as_v2 = enrich_as(v, row(&pool, child.id).await).await;
    assert_eq!(
        as_v2.parent_key.as_deref(),
        Some(expected.as_str()),
        "a team epic's key appears on its children for any tenant viewer"
    );

    let _ = sqlx::query("DELETE FROM tenants WHERE id = $1 AND slug <> 'dev'")
        .bind(tenant.id)
        .execute(&pool)
        .await;
}
