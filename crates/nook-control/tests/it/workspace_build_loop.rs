//! MAIN-461/MAIN-641: a workspace's whole BUILD loop on one route, and its
//! Builds rows.
//!
//! The thing worth pinning is still the THREE states — unset (default 1), an
//! explicit 0 (builds off for this repo), and n — which must stay
//! distinguishable all the way through the API, because the CLI's
//! "unset (default 1)" and "0 (off)" are different facts about who decided
//! what. MAIN-641 gave that column its single name, `concurrency`, beside the
//! switch and the pin it was always read with, so the partial-update rule is
//! now what keeps the three settings from clobbering each other. The listing
//! test also pins the cross-dialect key join (`||` + CAST), which is the one
//! piece of SQL here neither engine has run before.
//!
//! Engine-neutral (MAIN-264): nothing here names a `sqlx` type, so the same
//! file runs on whichever engine `DATABASE_URL` selects.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::{build_loop_status, get_build_loop, set_build_loop};
use nook_control::services::kanban::KanbanProvider;
use nook_db::Db;
use nook_testkit::TestBed;
use nook_types::*;
use serde_json::json;
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

/// A PATCH body from the JSON a caller would actually send, so an absent key is
/// absent here too — which is the whole distinction under test.
fn patch(v: serde_json::Value) -> Json<SetBuildLoopSettingsRequest> {
    Json(serde_json::from_value(v).expect("request body"))
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
    assert_eq!(fresh.concurrency, None, "a new workspace is unset, not 1");

    let set = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "concurrency": 2 })),
    )
    .await
    .expect("set")
    .0;
    assert_eq!(set.concurrency, Some(2));

    // `0` is a value — the kill-switch — not an absence.
    let off = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "concurrency": 0 })),
    )
    .await
    .expect("set 0")
    .0;
    assert_eq!(off.concurrency, Some(0));

    // ...and `null` is the way back to "nobody decided".
    let cleared = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "concurrency": null })),
    )
    .await
    .expect("clear")
    .0;
    assert_eq!(cleared.concurrency, None, "null clears back to unset");

    // A negative value is refused, naming the field.
    let bad = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "concurrency": -1 })),
    )
    .await;
    assert!(bad.is_err(), "-1 must be refused, got {bad:?}");

    bed.teardown().await;
}

/// MAIN-641 AC-3: an ABSENT key leaves its setting alone. The one route now
/// carries the switch, the pin and the ceiling, so `nook builds scale` must be
/// unable to disturb a loop somebody enabled — and vice versa.
#[tokio::test]
async fn an_absent_field_leaves_its_setting_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blpartial").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let all = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "enabled": true, "node": node.0.to_string(), "concurrency": 3 })),
    )
    .await
    .expect("set everything")
    .0;
    assert!(all.enabled);
    assert_eq!(all.node_id, Some(node));
    assert!(
        all.node_name.is_some(),
        "the pin's name is joined for the caller"
    );
    assert_eq!(all.concurrency, Some(3));
    assert_eq!(
        all.enabled_by,
        Some(user),
        "turning it on records the caller"
    );

    // The ceiling alone: the switch and the pin are not mentioned, so they stay.
    let scaled = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "concurrency": 5 })),
    )
    .await
    .expect("scale")
    .0;
    assert_eq!(scaled.concurrency, Some(5));
    assert!(scaled.enabled, "scaling did not touch the switch");
    assert_eq!(scaled.node_id, Some(node), "scaling did not touch the pin");
    assert_eq!(
        scaled.enabled_by,
        Some(user),
        "nor the identity its runs are requested by"
    );

    // `null` on the pin UNPINS, and says nothing about the ceiling — the
    // three-state distinction `double_option` exists for.
    let unpinned = set_build_loop(
        State(state.clone()),
        auth,
        Path(ws),
        patch(json!({ "node": null })),
    )
    .await
    .expect("unpin")
    .0;
    assert_eq!(unpinned.node_id, None, "null unpins");
    assert_eq!(unpinned.concurrency, Some(5), "and leaves the ceiling");
    assert!(unpinned.enabled);

    bed.teardown().await;
}

/// The enabler is stamped by a caller who touches the SWITCH or the PIN, and by
/// nobody else.
///
/// MAIN-641 routed the ceiling through this handler, which put two ordinary
/// writes — `nook builds scale` and Mission Control's ceiling editor — on the
/// code path that re-stamps. `build_loop_enabled_by` is not cosmetic:
/// `auto_fire_identity` hands it to `converge_builds` as `requested_by` and
/// `select_executor` scopes eligibility to nodes that identity OWNS, so a
/// member with no machines typing a number would silently strand every
/// subsequent run at "no eligible executor" while the switch still read on.
#[tokio::test]
async fn a_ceiling_only_write_does_not_take_over_the_loop() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blenabler").await;
    let (alice, alices_person) = bed.user(tenant, "owner").await;
    let (bob, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let azul = bed.node(tenant, alices_person).await;
    let state = bed.app_state().await;

    let on = set_build_loop(
        State(state.clone()),
        user_ctx(alice, tenant),
        Path(ws),
        patch(json!({ "enabled": true, "node": azul.0.to_string() })),
    )
    .await
    .expect("alice enables it")
    .0;
    assert_eq!(on.enabled_by, Some(alice));

    // Bob owns no node. Typing a ceiling must not make him the identity every
    // auto-fired run is requested by.
    let scaled = set_build_loop(
        State(state.clone()),
        user_ctx(bob, tenant),
        Path(ws),
        patch(json!({ "concurrency": 3 })),
    )
    .await
    .expect("bob scales it")
    .0;
    assert_eq!(scaled.concurrency, Some(3), "the ceiling is his to set");
    assert_eq!(
        scaled.enabled_by,
        Some(alice),
        "…but the loop is still Alice's, or its runs stop being placeable"
    );

    // Moving the pin IS the statement that makes him answerable, and still is.
    let repinned = set_build_loop(
        State(state.clone()),
        user_ctx(bob, tenant),
        Path(ws),
        patch(json!({ "node": null })),
    )
    .await
    .expect("bob unpins")
    .0;
    assert_eq!(
        repinned.enabled_by,
        Some(bob),
        "touching the pin still records the person now answerable (MAIN-385 AC-2)"
    );

    bed.teardown().await;
}

/// MAIN-641 AC-5: the authorization the three old routes carried, on the two
/// new ones. A node credential may not declare desired state, and a caller who
/// cannot reach the workspace is told it does not exist rather than that they
/// may not have it.
#[tokio::test]
async fn a_node_may_not_write_and_a_stranger_gets_404() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blauth").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let node_auth = AuthCtx {
        principal: Principal::Node(NodeId::new()),
        ..user_ctx(user, tenant)
    };
    let refused = set_build_loop(
        State(state.clone()),
        node_auth,
        Path(ws),
        patch(json!({ "enabled": true })),
    )
    .await;
    assert!(
        refused.is_err(),
        "a job requested by a machine resolves to no person, so no node would \
         ever be eligible for it — got {refused:?}"
    );

    // A workspace of ANOTHER tenant is not visible, and the answer says so the
    // way a stranger's does: not found, never forbidden.
    let other = bed.tenant("blauthother").await;
    let (stranger, _) = bed.user(other, "owner").await;
    let stranger_auth = user_ctx(stranger, other);
    for got in [
        get_build_loop(State(state.clone()), stranger_auth, Path(ws))
            .await
            .err(),
        build_loop_status(State(state.clone()), stranger_auth, Path(ws))
            .await
            .err(),
    ] {
        assert!(
            matches!(got, Some(nook_control::error::ApiError::NotFound)),
            "a caller who may not reach the workspace gets 404, got {got:?}"
        );
    }

    bed.teardown().await;
}

/// MAIN-641 AC-4: the status sub-resource is the ONE place the declaration is
/// resolved. The config route reports the column raw, and the two answers
/// together are what let a reader tell "nobody decided" from "off".
#[tokio::test]
async fn only_status_resolves_the_declaration() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("blresolve").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let unset = build_loop_status(State(state.clone()), auth, Path(ws))
        .await
        .expect("status")
        .0;
    assert_eq!(unset.desired, 1, "null resolves to the default of one");

    for (declared, desired) in [(0, 0), (4, 4)] {
        let _ = set_build_loop(
            State(state.clone()),
            auth,
            Path(ws),
            patch(json!({ "concurrency": declared })),
        )
        .await
        .expect("set");
        let decl = get_build_loop(State(state.clone()), auth, Path(ws))
            .await
            .expect("read")
            .0;
        assert_eq!(decl.concurrency, Some(declared), "reported raw");
        let status = build_loop_status(State(state.clone()), auth, Path(ws))
            .await
            .expect("status")
            .0;
        assert_eq!(status.desired, desired);
    }

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
