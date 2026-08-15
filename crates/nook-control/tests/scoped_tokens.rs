//! MAIN-602 — a credential that can do less than its owner.
//!
//! Driven through the router that ships, because the whole claim is about
//! WIRING: the rules themselves are unit-tested in `nook_control::auth::scopes`
//! with no database at all, and what can only be checked here is that every
//! request really passes the gate — that a scope is enforced on the path it
//! names, that a workspace narrowing follows the card rather than the caller's
//! word for it, and that the `/mcp` door resolves a scoped token to a real
//! caller instead of waving it through.
//!
//! Two workspaces throughout, because a one-workspace test cannot tell a
//! narrowing that works from one that is never consulted.
//!
//! Needs a database: set `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).
//! Nothing here is Postgres-shaped, so it runs on **both** engines and the
//! SQLite leg (`./test.sh rust --sqlite`) covers it — which is how the uuid
//! decode in `list_user_tokens` was caught. Do not narrow this to one engine.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::{BoardId, TaskId, TenantId, UserId, WorkspaceId};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const HOST: &str = "localhost:8080";

struct Fixture {
    tenant: TenantId,
    user: UserId,
    ws_a: WorkspaceId,
    ws_b: WorkspaceId,
    task_a: TaskId,
    task_b: TaskId,
    /// An unscoped `nook_user_` token: the credential that existed before this
    /// ticket, and the one NG-1 promises is untouched.
    full: String,
}

async fn fixture(bed: &TestBed, state: &AppState) -> Fixture {
    let tenant = bed.tenant("scoped").await;
    let (user, _person) = bed.user(tenant, "owner").await;
    state
        .identity
        .grant_membership(tenant, user, "owner")
        .await
        .expect("membership");
    let ws_a = bed.workspace(tenant).await;
    let ws_b = bed.workspace(tenant).await;
    let board = seed_board(bed, tenant).await;
    let task_a = seed_task(bed, tenant, board, Some(ws_a), 1).await;
    let task_b = seed_task(bed, tenant, board, Some(ws_b), 2).await;
    let full = mint_raw(state, tenant, user, None, None).await;
    Fixture {
        tenant,
        user,
        ws_a,
        ws_b,
        task_a,
        task_b,
        full,
    }
}

async fn seed_board(bed: &TestBed, tenant: TenantId) -> BoardId {
    let board = BoardId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
             VALUES ($1, $2, 'b', $3, 'local')",
            params![
                board,
                tenant,
                format!("S{}", &board.0.simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Triage', 0, 'unstarted')",
            params![Uuid::now_v7(), board],
        )
        .await
        .expect("column");
    board
}

async fn seed_task(
    bed: &TestBed,
    tenant: TenantId,
    board: BoardId,
    workspace: Option<WorkspaceId>,
    number: i32,
) -> TaskId {
    let column: Uuid = bed
        .db()
        .query_scalar_opt(
            "SELECT id FROM board_columns WHERE board_id = $1",
            params![board],
        )
        .await
        .expect("column lookup")
        .expect("a column");
    let id = TaskId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, workspace_id, number)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            params![
                id,
                tenant,
                board,
                column,
                format!("card {number}"),
                workspace.map(|w| w.0),
                number
            ],
        )
        .await
        .expect("task");
    id
}

/// Mint straight through the repo — used for the credentials a test STARTS
/// with, so a failure in the mint route cannot masquerade as a failure in the
/// gate. The mint route itself is exercised on its own below.
async fn mint_raw(
    state: &AppState,
    tenant: TenantId,
    user: UserId,
    scopes: Option<&str>,
    workspace: Option<WorkspaceId>,
) -> String {
    let token = format!("nook_user_{}", Uuid::now_v7().simple());
    state
        .identity
        .create_user_token(nook_control::repo::identity::NewUserToken {
            id: Uuid::now_v7(),
            tenant,
            user_id: user,
            token_hash: nook_auth::hash_token(&token),
            name: "scoped test".into(),
            expires_at: None,
            scopes: scopes.map(str::to_string),
            workspace_id: workspace,
        })
        .await
        .expect("mint token");
    token
}

fn req(method: &str, token: &str, path: &str, body: Option<Value>) -> Request<Body> {
    let b = Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::HOST, HOST)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    match body {
        Some(v) => b.body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, String) {
    let res = nook_control::routes::build_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn a_comment() -> Value {
    json!({ "body_md": "the report", "request_changes": false, "clear_escalation": false })
}

/// AC-1 + AC-7: the case the ticket opened on. A token that may write a report
/// in one workspace writes it there, and the same token is refused on a card in
/// another workspace — by the narrowing, named, and not by a 404 that would send
/// the caller looking for a card that plainly exists.
#[tokio::test]
async fn a_narrowed_report_token_writes_where_it_may_and_nowhere_else() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;
    let token = mint_raw(
        &state,
        f.tenant,
        f.user,
        Some("reports:write"),
        Some(f.ws_a),
    )
    .await;

    let (status, body) = send(
        &state,
        req(
            "POST",
            &token,
            &format!("/api/v1/tasks/{}/comments", f.task_a.0),
            Some(a_comment()),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "allowed in its own workspace: {body}"
    );

    let (status, body) = send(
        &state,
        req(
            "POST",
            &token,
            &format!("/api/v1/tasks/{}/comments", f.task_b.0),
            Some(a_comment()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not in another one: {body}");
    assert!(
        body.contains(&f.ws_a.to_string()),
        "the refusal names the workspace it IS narrowed to: {body}"
    );

    bed.teardown().await;
}

/// AC-7: a scope it does not hold is a 403 NAMING the scope. Both shapes: a
/// route on the scoped surface that needs a different verb, and a route that is
/// not on the surface at all.
#[tokio::test]
async fn a_missing_scope_is_a_403_that_says_which_scope() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;
    let token = mint_raw(&state, f.tenant, f.user, Some("reports:write"), None).await;

    let (status, body) = send(
        &state,
        req(
            "PATCH",
            &token,
            &format!("/api/v1/tasks/{}", f.task_a.0),
            Some(json!({ "title": "renamed" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.contains("tasks:write"),
        "names the missing scope: {body}"
    );

    // Default deny: nothing maps `/api/v1/nodes`, so no scoped token reaches it
    // however many scopes it holds.
    let everything = mint_raw(
        &state,
        f.tenant,
        f.user,
        Some("reports:write tasks:read tasks:write mcp"),
        None,
    )
    .await;
    let (status, body) = send(&state, req("GET", &everything, "/api/v1/nodes", None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("scoped token"), "says why: {body}");

    bed.teardown().await;
}

/// AC-1: a narrowed LISTING is narrowed rather than refused — and a listing that
/// asks for another workspace outright is refused rather than quietly answered
/// about the wrong one.
///
/// This is also the guard on the extractor ordering the pin depends on (see
/// `scopes::authorize`): if `AuthCtx` stops running before `RawQuery` in
/// `task_query::query`, the rewrite never reaches the handler and the second
/// assertion here fails. A narrowing that silently stops applying returns the
/// whole tenant, which is the failure that must never be quiet.
#[tokio::test]
async fn a_narrowed_listing_returns_only_its_own_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;
    let token = mint_raw(&state, f.tenant, f.user, Some("tasks:read"), Some(f.ws_a)).await;

    let (status, body) = send(&state, req("GET", &token, "/api/v1/tasks", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("card 1"), "its own workspace's card: {body}");
    assert!(!body.contains("card 2"), "and only that one: {body}");

    let (status, body) = send(
        &state,
        req(
            "GET",
            &token,
            &format!("/api/v1/tasks?workspace={}", f.ws_b),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "asking about another workspace is refused, not silently rewritten: {body}"
    );

    bed.teardown().await;
}

/// AC-2 + AC-3, at the mint: an unknown scope is a 400 naming it, a workspace
/// the minter cannot reach is refused, and a scoped credential cannot mint at
/// all — which is what makes escalation structurally impossible rather than
/// merely checked.
#[tokio::test]
async fn minting_refuses_an_unknown_scope_a_foreign_workspace_and_a_scoped_minter() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;

    let (status, body) = send(
        &state,
        req(
            "POST",
            &f.full,
            "/api/v1/tokens",
            Some(json!({ "name": "ci", "scopes": ["reports:destroy"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("reports:destroy"), "names it: {body}");
    assert!(body.contains("reports:write"), "and the real set: {body}");

    // A workspace belonging to somebody else: `resolve_by_key` is scoped to the
    // minter's tenant, so this is "no such workspace", never a grant.
    let other_tenant = bed.tenant("elsewhere").await;
    let theirs = bed.workspace(other_tenant).await;
    let (status, body) = send(
        &state,
        req(
            "POST",
            &f.full,
            "/api/v1/tokens",
            Some(
                json!({ "name": "ci", "scopes": ["tasks:read"], "workspace": theirs.to_string() }),
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And the credential that would escalate cannot even ask.
    let scoped = mint_raw(&state, f.tenant, f.user, Some("reports:write"), None).await;
    let (status, body) = send(
        &state,
        req(
            "POST",
            &scoped,
            "/api/v1/tokens",
            Some(json!({ "name": "wider", "scopes": ["tasks:write", "mcp"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    bed.teardown().await;
}

/// AC-3, at the shape the emptiness guard used to let through: a scoped
/// credential minting a request with **no scopes at all** is asking for a FULL
/// token — the widest thing there is, not the narrowest — so the minter check has
/// to run before anything branches on what was asked for.
///
/// Reached by calling the handler directly. `required_scope` maps no path to
/// `POST /tokens`, so a scoped token cannot get here over HTTP today; that is
/// exactly why this is the belt to those braces, and why the test drives the
/// function rather than the route.
#[tokio::test]
async fn a_scoped_credential_cannot_mint_an_unscoped_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;

    // The credential's own row id is what `minter_grant` reads, and `AuthCtx`
    // carries it in `session_id` — so this is the context a scoped bearer
    // resolves to, not a hand-made one.
    let id = Uuid::now_v7();
    state
        .identity
        .create_user_token(nook_control::repo::identity::NewUserToken {
            id,
            tenant: f.tenant,
            user_id: f.user,
            token_hash: nook_auth::hash_token(&format!("nook_user_{}", Uuid::now_v7().simple())),
            name: "narrow minter".into(),
            expires_at: None,
            scopes: Some("reports:write".into()),
            workspace_id: None,
        })
        .await
        .expect("mint the minter");
    let scoped_ctx = nook_control::auth::AuthCtx {
        session_id: nook_types::AuthSessionId(id),
        user_id: f.user,
        tenant_id: f.tenant,
        principal: nook_control::auth::Principal::User,
        cookie_session: false,
    };

    for (label, body) in [
        ("no scopes at all — a FULL token", json!({ "name": "wide" })),
        (
            "an empty list — the same thing",
            json!({ "name": "wide", "scopes": [] }),
        ),
    ] {
        let err = nook_control::routes::tokens::create(
            axum::extract::State(state.clone()),
            scoped_ctx,
            Some(axum::Json(serde_json::from_value(body).expect("a request"))),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("{label}: a scoped credential must not mint one"));
        let msg = err.to_string();
        assert!(
            msg.contains("unscoped"),
            "{label}: refusal names what was refused: {msg}"
        );
    }

    // And a cookie session — the credential that IS its person — still mints.
    let full_ctx = nook_control::auth::AuthCtx {
        cookie_session: true,
        ..scoped_ctx
    };
    let minted = nook_control::routes::tokens::create(
        axum::extract::State(state.clone()),
        full_ctx,
        Some(axum::Json(
            serde_json::from_value(json!({ "name": "fine" })).unwrap(),
        )),
    )
    .await
    .expect("a full credential mints exactly as before");
    assert!(
        minted.0.scopes.is_empty(),
        "and what it mints is still unscoped (NG-1)"
    );

    bed.teardown().await;
}

/// `mcp` and a workspace cannot both mean something, so the pair is refused at
/// mint rather than stored as a scope that is silently inert — the same reason
/// an unknown scope is refused (AC-2).
#[tokio::test]
async fn mcp_cannot_be_narrowed_to_a_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;

    let (status, body) = send(
        &state,
        req(
            "POST",
            &f.full,
            "/api/v1/tokens",
            Some(json!({ "name": "both", "scopes": ["mcp"], "workspace": f.ws_a.to_string() })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("tenant-wide"), "says why: {body}");

    // Either half on its own is fine.
    for good in [
        json!({ "name": "mcp only", "scopes": ["mcp"] }),
        json!({ "name": "narrowed", "scopes": ["tasks:read"], "workspace": f.ws_a.to_string() }),
    ] {
        let (status, body) = send(&state, req("POST", &f.full, "/api/v1/tokens", Some(good))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    bed.teardown().await;
}

/// AC-9 + AC-8: the value is shown once at mint, and the listing then says what
/// the token may do and where — which is what makes "which of these do I
/// revoke?" answerable.
#[tokio::test]
async fn a_minted_token_is_shown_once_and_lists_its_scopes_and_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;

    let (status, body) = send(
        &state,
        req(
            "POST",
            &f.full,
            "/api/v1/tokens",
            Some(json!({
                "name": "ci reports",
                "scopes": ["reports:write"],
                "workspace": f.ws_a.to_string(),
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let minted: Value = serde_json::from_str(&body).expect("a token");
    let value = minted["token"].as_str().expect("the value, once");
    assert!(value.starts_with("nook_user_"), "{value}");
    assert_eq!(minted["scopes"], json!(["reports:write"]));
    assert_eq!(minted["workspace_id"], json!(f.ws_a.to_string()));

    let (status, body) = send(&state, req("GET", &f.full, "/api/v1/tokens", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<Value> = serde_json::from_str(&body).expect("a list");
    let row = listed
        .iter()
        .find(|t| t["id"] == minted["id"])
        .unwrap_or_else(|| panic!("the token we just minted, in: {body}"));
    assert_eq!(row["scopes"], json!(["reports:write"]));
    assert_eq!(row["workspace_id"], json!(f.ws_a.to_string()));
    assert!(
        row["workspace_slug"].is_string(),
        "a slug a person can read: {row}"
    );
    assert!(
        !body.contains(value),
        "the listing never carries the value back"
    );

    // It works, and then it does not: revocation is the next request, not the
    // next cache expiry.
    let path = format!("/api/v1/tasks/{}/comments", f.task_a.0);
    let (status, _) = send(&state, req("POST", value, &path, Some(a_comment()))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &state,
        req(
            "DELETE",
            &f.full,
            &format!("/api/v1/tokens/{}", minted["id"].as_str().expect("id")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&state, req("POST", value, &path, Some(a_comment()))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked, immediately");

    bed.teardown().await;
}

/// AC-6: the `/mcp` door. A token carrying `mcp` resolves to a real caller — so
/// `tools/list` offers it the tenant-scoped tool set, which a request that
/// resolved nobody is never offered — and one without the scope is refused by
/// the same gate rather than sent round the OAuth discovery loop.
#[tokio::test]
async fn the_mcp_door_takes_a_scoped_token_and_refuses_one_without_the_scope() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;

    let listed = mcp_tools(
        &state,
        &mint_raw(&state, f.tenant, f.user, Some("mcp"), None).await,
    )
    .await;
    assert!(
        listed.iter().any(|t| t == "list_workspaces"),
        "a resolved caller is offered the tenant tools: {listed:?}"
    );

    let no_mcp = mint_raw(&state, f.tenant, f.user, Some("tasks:read"), None).await;
    let (status, body) = send(&state, mcp_req(&no_mcp)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("mcp"), "names the missing scope: {body}");

    // A narrowing the MCP surface cannot express is refused at the door rather
    // than admitted and then quietly ignored.
    let narrowed = mint_raw(&state, f.tenant, f.user, Some("mcp"), Some(f.ws_a)).await;
    let (status, body) = send(&state, mcp_req(&narrowed)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("tenant-wide"), "says why: {body}");

    bed.teardown().await;
}

fn mcp_req(token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(axum::http::header::HOST, HOST)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(
            axum::http::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
        ))
        .unwrap()
}

async fn mcp_tools(state: &AppState, token: &str) -> Vec<String> {
    let (status, body) = send(state, mcp_req(token)).await;
    assert_eq!(status, StatusCode::OK, "the door opened: {body}");
    let data = body
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap_or(&body);
    let payload: Value = serde_json::from_str(data).unwrap_or_else(|_| panic!("json in: {body}"));
    payload["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("a tool list in: {body}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// NG-1: an unscoped `nook_user_` token is exactly what it was. Asserted on the
/// routes the gate would have taken away — one off the scoped surface entirely,
/// and one whose scope it would have had to hold.
#[tokio::test]
async fn an_unscoped_token_keeps_every_bit_of_its_reach() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let f = fixture(&bed, &state).await;

    for (method, path) in [
        ("GET", "/api/v1/nodes".to_string()),
        ("GET", "/api/v1/tokens".to_string()),
        ("GET", format!("/api/v1/tasks/{}", f.task_b.0)),
    ] {
        let (status, body) = send(&state, req(method, &f.full, &path, None)).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
    }

    let (status, body) = send(
        &state,
        req(
            "PATCH",
            &f.full,
            &format!("/api/v1/tasks/{}", f.task_a.0),
            Some(json!({ "title": "renamed by a full token" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    bed.teardown().await;
}
