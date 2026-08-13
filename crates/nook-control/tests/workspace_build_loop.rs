//! MAIN-461: the ceiling on a workspace's BUILD runs, and its Builds rows.
//!
//! `review-loop`'s twin, and the same thing is worth pinning: THREE states —
//! unset (default 1), an explicit 0 (builds off for this repo), and n — that
//! must stay distinguishable all the way through the API, because the CLI's
//! "unset (default 1)" and "0 (off)" are different facts about who decided
//! what. The listing test also pins the cross-dialect key join (`||` + CAST),
//! which is the one piece of SQL here neither engine has run before.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type, so the same
//! file runs on whichever engine `DATABASE_URL` selects.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::{get_build_loop, set_build_loop};
use nook_control::services::kanban::KanbanProvider;
use nook_db::Db;
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

fn req(v: serde_json::Value) -> Json<SetBuildLoopRequest> {
    Json(SetBuildLoopRequest { max_replicas: v })
}

#[tokio::test]
async fn the_three_states_stay_three() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("buildloop").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let fresh = get_build_loop(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert_eq!(fresh.max_replicas, None, "a new workspace is unset, not 1");

    let set = set_build_loop(State(state.clone()), auth, Path(ws), req(2.into()))
        .await
        .expect("set")
        .0;
    assert_eq!(set.max_replicas, Some(2));

    // `0` is a value — the kill-switch — not an absence.
    let off = set_build_loop(State(state.clone()), auth, Path(ws), req(0.into()))
        .await
        .expect("set 0")
        .0;
    assert_eq!(off.max_replicas, Some(0));

    // ...and `null` is the way back to "nobody decided".
    let cleared = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        req(serde_json::Value::Null),
    )
    .await
    .expect("clear")
    .0;
    assert_eq!(cleared.max_replicas, None, "null clears back to unset");

    // A negative or non-integer value is refused, naming the field.
    let bad = set_build_loop(State(state.clone()), auth, Path(ws), req((-1).into())).await;
    assert!(bad.is_err(), "-1 must be refused, got {bad:?}");

    bed.teardown().await;
}

/// The Builds panel's rows: build runs only, newest first, each naming its
/// card by KEY — the join `LoopJob` cannot provide. This is also the only
/// place the `||`/CAST concat runs, so it is the dialect pin.
#[tokio::test]
async fn the_builds_listing_names_the_card_by_key() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("buildlist").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    // A board + card, through the same provider every board test uses.
    let board = BoardId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b','BL','local')",
            nook_db::params![board, tenant],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            nook_db::params![Uuid::now_v7(), board],
        )
        .await
        .expect("column");
    let provider = nook_control::services::kanban::LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };
    let task = provider
        .create_task(
            tenant,
            board,
            Some(user),
            CreateTaskRequest {
                title: "a card".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("card");

    state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "build".into(),
            target_task_id: Some(task.id),
            workspace_id: Some(ws),
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: None,
            review_head_sha: None,
            build_fingerprint: None,
            review_forced: false,
        })
        .await
        .expect("build run");

    // Noise the filters must exclude: a REVIEW run in this workspace, and a
    // build run in a DIFFERENT workspace of the same tenant.
    let other_ws = bed.workspace(tenant).await;
    state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(ws),
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(7),
            review_head_sha: Some("aaa".into()),
            build_fingerprint: None,
            review_forced: false,
        })
        .await
        .expect("review run");
    let stray = provider
        .create_task(
            tenant,
            board,
            Some(user),
            CreateTaskRequest {
                title: "other repo card".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: Some(other_ws),
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("stray card");
    state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "build".into(),
            target_task_id: Some(stray.id),
            workspace_id: Some(other_ws),
            requested_by: user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: None,
            review_head_sha: None,
            build_fingerprint: None,
            review_forced: false,
        })
        .await
        .expect("other-ws build");

    let rows = state
        .jobs
        .list_builds_for_workspace(tenant, user, ws, &nook_testkit::first_page(50))
        .await
        .expect("list")
        .rows;
    assert_eq!(rows.len(), 1, "build kind + this workspace only");
    let key = rows[0].task_key.as_deref().expect("the card has a key");
    assert_eq!(
        key,
        format!("BL-{}", task.number.expect("numbered")),
        "the row names the card the way a human does"
    );

    bed.teardown().await;
}

/// The review's must-fix (MAIN-265/MAIN-86): a PRIVATE card's key is the
/// owner's — its build run lists KEYLESS to a non-owner, keyed to the owner.
/// The run row itself stays: the workspace's history is not the secret, the
/// card's identity is.
///
/// The BRANCH is the same secret wearing a different hat (MAIN-557):
/// `start-work` defaults it to `slugify(title)`, so leaking it would leak more
/// than the key this test was written to withhold. The INITIATOR joins them by
/// owner ruling (AC-3a): a row naming nobody must not start naming WHO has a
/// private card building here.
#[tokio::test]
async fn a_private_cards_key_branch_and_initiator_are_withheld_from_a_non_owner() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("bpriv").await;
    let (alice, _) = bed.user(tenant, "member").await;
    let (bob, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let board = BoardId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b','BP','local')",
            nook_db::params![board, tenant],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            nook_db::params![Uuid::now_v7(), board],
        )
        .await
        .expect("column");
    let provider = nook_control::services::kanban::LocalBoardProvider {
        repo: std::sync::Arc::new(nook_control::repo::tasks::DbTaskRepository::new(bed.db())),
    };
    let secret = provider
        .create_task(
            tenant,
            board,
            Some(alice),
            CreateTaskRequest {
                title: "hidden".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: Some(ws),
                priority: None,
                type_: None,
                visibility: Some("private".into()),
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("private card");
    // The branch a `start-work` on this card would have written: the card's
    // title, slugified. Nothing else in the fixture sets one.
    bed.db()
        .exec(
            "UPDATE tasks SET branch = $2 WHERE id = $1",
            nook_db::params![secret.id, "hidden"],
        )
        .await
        .expect("branch");
    state
        .jobs
        .create(nook_control::repo::jobs::NewLoopJob {
            id: JobId::new(),
            tenant,
            kind: "build".into(),
            target_task_id: Some(secret.id),
            workspace_id: Some(ws),
            requested_by: alice,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: None,
            review_head_sha: None,
            build_fingerprint: None,
            review_forced: false,
        })
        .await
        .expect("build run");

    let to_bob = state
        .jobs
        .list_builds_for_workspace(tenant, bob, ws, &nook_testkit::first_page(50))
        .await
        .expect("list as bob")
        .rows;
    assert_eq!(to_bob.len(), 1, "the run row is workspace history");
    assert!(
        to_bob[0].task_key.is_none(),
        "…but the private card's key is withheld from a non-owner"
    );
    assert!(
        to_bob[0].branch.is_none(),
        "…and so is its branch, which carries the card's title verbatim"
    );
    assert!(
        to_bob[0].initiator.is_none(),
        "…and so is its initiator: who has a private card building is the \
         card's secret too (AC-3a)"
    );

    let to_alice = state
        .jobs
        .list_builds_for_workspace(tenant, alice, ws, &nook_testkit::first_page(50))
        .await
        .expect("list as alice")
        .rows;
    assert!(
        to_alice[0].task_key.is_some(),
        "the owner still sees their card named"
    );
    assert_eq!(
        to_alice[0].branch.as_deref(),
        Some("hidden"),
        "the owner still sees their card's branch"
    );
    assert!(
        to_alice[0].initiator.is_some(),
        "the owner still sees who raised the run"
    );

    bed.teardown().await;
}
