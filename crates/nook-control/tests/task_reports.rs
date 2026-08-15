//! MAIN-603: generic Markdown metadata on a card.
//!
//! Handlers are driven directly, as the rest of this suite does. What is under
//! test is the CONTRACT — that a key addresses one report and re-running
//! replaces it, that each limit refuses in its own words, and that a card
//! nobody may see has no report list either. The scope gate is exercised
//! through the shipped router in `scoped_tokens.rs`, where the rest of MAIN-602
//! is.
//!
//! Nothing here is Postgres-shaped, so it runs on both engines.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::task_reports::{delete, list, put};
use nook_control::services::task_reports::{MAX_BODY_BYTES, MAX_REPORTS_PER_TASK};
use nook_control::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

fn status_of(err: nook_control::error::ApiError) -> StatusCode {
    axum::response::IntoResponse::into_response(err).status()
}

async fn board_in(bed: &TestBed, tenant: TenantId) -> BoardId {
    let id: BoardId = bed
        .db()
        .query_scalar(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
             VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
            params![
                BoardId::new(),
                tenant,
                format!("R{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            params![Uuid::now_v7(), id],
        )
        .await
        .expect("column");
    id
}

async fn task_on(state: &AppState, tenant: TenantId, board: BoardId, creator: UserId) -> TaskItem {
    state
        .kanban
        .get("local")
        .expect("local provider")
        .create_task(
            tenant,
            board,
            Some(creator),
            CreateTaskRequest {
                title: "a ticket".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: None,
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("create task")
}

async fn write(
    state: &AppState,
    who: AuthCtx,
    task: TaskId,
    key: &str,
    title: &str,
    body_md: &str,
) -> Result<TaskReport, nook_control::error::ApiError> {
    put(
        State(state.clone()),
        who,
        Path((task.0.to_string(), key.to_string())),
        Json(PutTaskReportRequest {
            title: title.into(),
            body_md: body_md.into(),
        }),
    )
    .await
    .map(|Json(r)| r)
}

async fn reports(state: &AppState, who: AuthCtx, task: TaskId) -> Vec<TaskReport> {
    let Json(rows) = list(State(state.clone()), who, Path(task.0.to_string()))
        .await
        .expect("the listing answers");
    rows
}

/// AC-1: the key is the address, so a second `PUT` replaces. The clock moves,
/// the creation stamp does not — which is what makes AC-10's "visibly stale"
/// read the right number.
#[tokio::test]
async fn writing_the_same_key_twice_replaces_rather_than_appends() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("reports").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    let first = write(&state, ctx(user, tenant), task.id, "build", "Build", "one")
        .await
        .expect("the first write");
    let second = write(&state, ctx(user, tenant), task.id, "build", "Build", "two")
        .await
        .expect("the second write");

    let rows = reports(&state, ctx(user, tenant), task.id).await;
    assert_eq!(rows.len(), 1, "one key is one report: {rows:?}");
    assert_eq!(rows[0].body_md, "two", "the second content wins");
    assert_eq!(first.id, second.id, "the row was updated, not replaced");
    assert_eq!(
        first.created_at, second.created_at,
        "the first write is when this key appeared"
    );
    assert!(
        second.updated_at >= first.updated_at,
        "and the update stamp moves: {} → {}",
        first.updated_at,
        second.updated_at
    );

    // AC-10: who wrote it, recorded.
    assert_eq!(rows[0].author_id, Some(user));
    assert_eq!(rows[0].author_type, "user");

    bed.teardown().await;
}

/// AC-5: most recently updated first. Written oldest-first so an accidental
/// insertion-order listing would pass the count and fail here.
#[tokio::test]
async fn the_listing_puts_the_freshest_report_first() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("reports-order").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    for key in ["first", "second", "third"] {
        write(&state, ctx(user, tenant), task.id, key, key, "x")
            .await
            .expect("written");
    }
    // Re-writing the OLDEST key must move it to the front — the order is by
    // update, not by creation.
    write(
        &state,
        ctx(user, tenant),
        task.id,
        "first",
        "First",
        "again",
    )
    .await
    .expect("rewritten");

    let keys: Vec<String> = reports(&state, ctx(user, tenant), task.id)
        .await
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(keys.first().map(String::as_str), Some("first"), "{keys:?}");
    assert_eq!(keys.len(), 3, "{keys:?}");

    bed.teardown().await;
}

/// AC-3 and AC-7: three different refusals, each naming its own rule. A single
/// "bad request" for all three is the failure this asserts against.
#[tokio::test]
async fn every_limit_refuses_in_its_own_words() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("reports-limits").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    let bad_key = write(&state, ctx(user, tenant), task.id, "Build Report", "T", "x")
        .await
        .expect_err("a key with a space and a capital");
    let msg = bad_key.to_string();
    assert!(msg.contains("lowercase letters, digits and '-'"), "{msg}");
    assert_eq!(status_of(bad_key), StatusCode::BAD_REQUEST);

    let big = "a".repeat(MAX_BODY_BYTES + 1);
    let too_big = write(&state, ctx(user, tenant), task.id, "big", "T", &big)
        .await
        .expect_err("one byte over the body limit");
    let msg = too_big.to_string();
    assert!(msg.contains("64.0 KB"), "names the limit: {msg}");
    assert_eq!(status_of(too_big), StatusCode::BAD_REQUEST);

    for n in 0..MAX_REPORTS_PER_TASK {
        write(
            &state,
            ctx(user, tenant),
            task.id,
            &format!("r{n}"),
            "T",
            "x",
        )
        .await
        .unwrap_or_else(|e| panic!("report {n} of the allowance: {e}"));
    }
    let overflow = write(&state, ctx(user, tenant), task.id, "r20", "T", "x")
        .await
        .expect_err("the twenty-first");
    let msg = overflow.to_string();
    assert!(msg.contains(&MAX_REPORTS_PER_TASK.to_string()), "{msg}");
    assert_eq!(status_of(overflow), StatusCode::BAD_REQUEST);

    // ...and a full card can still have an existing report corrected, or the
    // producer that filled it could never fix its own output.
    write(&state, ctx(user, tenant), task.id, "r0", "T", "corrected")
        .await
        .expect("replacing is not adding");
    assert_eq!(
        reports(&state, ctx(user, tenant), task.id).await.len() as i64,
        MAX_REPORTS_PER_TASK
    );

    bed.teardown().await;
}

/// AC-4: stored as given. The body here is exactly the kind of content the
/// server must not touch — a GFM table, raw HTML, a link — and every byte of it
/// comes back.
#[tokio::test]
async fn the_body_is_stored_byte_for_byte() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("reports-verbatim").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    let body = "| a | b |\n|---|---|\n| 1 | 2 |\n\n<script>alert(1)</script>\n\nhttps://example.com MAIN-1";
    write(&state, ctx(user, tenant), task.id, "verbatim", "V", body)
        .await
        .expect("written");

    let rows = reports(&state, ctx(user, tenant), task.id).await;
    assert_eq!(
        rows[0].body_md, body,
        "nothing extracted, nothing rewritten, nothing stripped"
    );

    bed.teardown().await;
}

/// AC-2: delete removes exactly one key, and a key that is not there is a 404
/// rather than a silent success.
#[tokio::test]
async fn deleting_removes_one_key_and_missing_is_a_404() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("reports-delete").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    for key in ["keep", "drop"] {
        write(&state, ctx(user, tenant), task.id, key, key, "x")
            .await
            .expect("written");
    }
    let code = delete(
        State(state.clone()),
        ctx(user, tenant),
        Path((task.id.0.to_string(), "drop".into())),
    )
    .await
    .expect("the delete answers");
    assert_eq!(code, StatusCode::NO_CONTENT);

    let keys: Vec<String> = reports(&state, ctx(user, tenant), task.id)
        .await
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(keys, vec!["keep".to_string()]);

    let missing = delete(
        State(state.clone()),
        ctx(user, tenant),
        Path((task.id.0.to_string(), "drop".into())),
    )
    .await
    .expect_err("already gone");
    assert_eq!(status_of(missing), StatusCode::NOT_FOUND);

    bed.teardown().await;
}

/// AC-8: a report follows its card's visibility. Both routes refuse, and both
/// refuse with a 404 — a 403 would confirm the card exists, which is the leak.
#[tokio::test]
async fn a_card_the_reader_cannot_see_has_no_reports_either() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("reports-visibility").await;
    let (owner, _) = bed.user(tenant, "owner").await;
    let (stranger, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, owner).await;
    write(&state, ctx(owner, tenant), task.id, "secret", "S", "x")
        .await
        .expect("written while the card is public");

    bed.db()
        .exec(
            "UPDATE tasks SET visibility = 'private' WHERE id = $1",
            params![task.id],
        )
        .await
        .expect("made private");

    // The stranger sees neither the list...
    let listed = list(
        State(state.clone()),
        ctx(stranger, tenant),
        Path(task.id.0.to_string()),
    )
    .await
    .expect_err("a private card is a 404, not an empty list");
    assert_eq!(status_of(listed), StatusCode::NOT_FOUND);

    // ...nor the item route, in either direction.
    let written = write(&state, ctx(stranger, tenant), task.id, "secret", "S", "y")
        .await
        .expect_err("nor may they write onto it");
    assert_eq!(status_of(written), StatusCode::NOT_FOUND);

    let removed = delete(
        State(state.clone()),
        ctx(stranger, tenant),
        Path((task.id.0.to_string(), "secret".into())),
    )
    .await
    .expect_err("nor delete from it");
    assert_eq!(status_of(removed), StatusCode::NOT_FOUND);

    // The owner is unaffected — the refusal is about the reader, not the card.
    assert_eq!(reports(&state, ctx(owner, tenant), task.id).await.len(), 1);

    bed.teardown().await;
}

/// Another tenant's uuid resolves to nothing. A task id is not an
/// authorisation, and a distinguishable answer here would be a probe.
#[tokio::test]
async fn another_tenant_reaches_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let mine = bed.tenant("reports-mine").await;
    let (me, _) = bed.user(mine, "owner").await;
    let theirs = bed.tenant("reports-theirs").await;
    let (them, _) = bed.user(theirs, "owner").await;
    let board = board_in(&bed, mine).await;
    let task = task_on(&state, mine, board, me).await;
    write(&state, ctx(me, mine), task.id, "build", "B", "x")
        .await
        .expect("written");

    let err = list(
        State(state.clone()),
        ctx(them, theirs),
        Path(task.id.0.to_string()),
    )
    .await
    .expect_err("another tenant's card");
    assert_eq!(status_of(err), StatusCode::NOT_FOUND);

    bed.teardown().await;
}
