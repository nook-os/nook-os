//! MAIN-91: comments and escalation labels become notable, and the kind
//! catalog is exposed. End-to-end through the real routes against a live
//! Postgres — a comment on a team card raises a notification, a comment on a
//! private card does not, `blocked` notifies while an ordinary label is silent,
//! and the catalog route lists every notable kind. Set `DATABASE_URL`.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::services::identity::{login_identity, IdentityClaims};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

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

async fn seed_label(bed: &TestBed, tenant: TenantId, name: &str) {
    // Ensure the label exists — idempotent, because a freshly-seeded tenant
    // already carries the agent-loop labels (`agent-ready`, `blocked`) from
    // `seed::run`, and this helper only needs the row present, not net-new.
    bed.db()
        .exec(
            "INSERT INTO labels (id, tenant_id, name) VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, name) DO NOTHING",
            params![Uuid::now_v7(), tenant, name],
        )
        .await
        .expect("seed label");
}

/// Notifications for a tenant of a given kind — the fan-out inserts the row
/// synchronously in `raise` before spawning delivery, so it is queryable the
/// moment the route returns.
async fn notes(bed: &TestBed, tenant: TenantId, kind: &str) -> Vec<Notification> {
    bed.db()
        .query_all(
            "SELECT id, tenant_id, user_id, level, title, body, kind, link, payload,
                read_at, created_at
         FROM notifications WHERE tenant_id = $1 AND kind = $2 ORDER BY created_at",
            params![tenant, kind],
        )
        .await
        .expect("notes")
}

#[tokio::test]
async fn comments_and_escalation_labels_notify() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping notifications test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let sub = format!("owner-{}", Uuid::now_v7().simple());
    let (owner, tenant) = login_identity(&state, claims(&sub, "Ada"))
        .await
        .expect("owner signs in");
    let a = owner.id;

    let board: BoardId = bed
        .db()
        .query_scalar(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
         VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
            params![
                BoardId::new(),
                tenant.id,
                format!("N{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            params![Uuid::now_v7(), board],
        )
        .await
        .expect("column");

    let team = make_task(&state, tenant.id, board, a, "shared work", "team").await;
    let secret = make_task(&state, tenant.id, board, a, "hush hush", "private").await;

    // ── AC-1: a comment on a TEAM card raises a deep-linked notification ─────
    let _ = nook_control::routes::task_detail::create_comment(
        State(state.clone()),
        auth(a, tenant.id),
        Path(team.id.to_string()),
        Json(CreateCommentRequest {
            body_md: "can someone review this?".into(),
            author_name: None,
        }),
    )
    .await
    .expect("comment on team card");

    let comment_notes = notes(&bed, tenant.id, "task.comment.created").await;
    assert_eq!(comment_notes.len(), 1, "one comment notification");
    let n = &comment_notes[0];
    assert!(
        n.body.contains("can someone review this?"),
        "the excerpt rides in the body: {:?}",
        n.body
    );
    assert!(
        n.link
            .as_deref()
            .unwrap_or_default()
            .contains("board?task="),
        "deep-linked to the card: {:?}",
        n.link
    );

    // ── AC-1: a comment on a PRIVATE card raises nothing ────────────────────
    let _ = nook_control::routes::task_detail::create_comment(
        State(state.clone()),
        auth(a, tenant.id),
        Path(secret.id.to_string()),
        Json(CreateCommentRequest {
            body_md: "for my eyes only".into(),
            author_name: None,
        }),
    )
    .await
    .expect("comment on private card");
    assert_eq!(
        notes(&bed, tenant.id, "task.comment.created").await.len(),
        1,
        "a private card's comment must not add a notification"
    );

    // ── AC-2: an escalation label notifies; an ordinary one is silent ───────
    seed_label(&bed, tenant.id, "blocked").await;
    seed_label(&bed, tenant.id, "frontend").await;

    let _ = nook_control::routes::labels::add(
        State(state.clone()),
        auth(a, tenant.id),
        Path((team.id.to_string(), "blocked".into())),
    )
    .await
    .expect("add blocked");
    let label_notes = notes(&bed, tenant.id, "task.label.added").await;
    assert_eq!(label_notes.len(), 1, "blocked raises one notification");
    assert!(
        label_notes[0].body.contains("blocked"),
        "the label is named: {:?}",
        label_notes[0].body
    );

    let _ = nook_control::routes::labels::add(
        State(state.clone()),
        auth(a, tenant.id),
        Path((team.id.to_string(), "frontend".into())),
    )
    .await
    .expect("add frontend");
    assert_eq!(
        notes(&bed, tenant.id, "task.label.added").await.len(),
        1,
        "an ordinary label must stay silent"
    );

    bed.teardown().await;
}

/// AC-3: the catalog route lists every notable kind with label, description,
/// and group — including the two new ones — and nothing uncatalogued.
#[tokio::test]
async fn catalog_route_lists_every_notable_kind() {
    let catalog = nook_control::events::catalog();
    let ids: Vec<&str> = catalog.iter().map(|k| k.id.as_str()).collect();
    for expected in [
        "task.comment.created",
        "task.label.added",
        "task.claimed",
        "node.connected",
    ] {
        assert!(ids.contains(&expected), "catalog missing {expected}");
    }
    for k in &catalog {
        assert!(!k.label.is_empty(), "{} needs a label", k.id);
        assert!(!k.description.is_empty(), "{} needs a description", k.id);
        assert!(
            k.id.starts_with(&k.group),
            "{} not in group {}",
            k.id,
            k.group
        );
    }
    // The handler returns exactly the catalog.
    let Json(served) = nook_control::routes::notifications::notification_kinds().await;
    assert_eq!(
        served.len(),
        catalog.len(),
        "route serves the whole catalog"
    );
}

/// The inbox's `(user_id IS NULL OR user_id = $2)` scope, which nothing covered
/// until MAIN-256 moved the statement and went looking.
///
/// A notification addressed to one person can carry a private card's title, a
/// review verdict, a failure someone would rather not broadcast. Lose that
/// clause and every member of the tenant reads every other member's inbox —
/// silently, because the list still looks right to whoever is testing it. The
/// tenant-wide (`user_id IS NULL`) rows must keep reaching everyone, which is
/// why the fix is a scope and not simply `user_id = $2`.
#[tokio::test]
async fn one_persons_inbox_never_shows_another_persons_notifications() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("inbox").await;
    let (alice, _) = bed.user(tenant, "member").await;
    let (bob, _) = bed.user(tenant, "member").await;

    use nook_control::repo::notifications::{NewNotification, NotificationFilter};
    let raise = |to: Option<UserId>, title: &str| {
        let state = state.clone();
        let title = title.to_string();
        async move {
            state
                .notifications
                .raise(NewNotification {
                    tenant,
                    user_id: to.map(|u| u.0),
                    level: "info".into(),
                    title,
                    body: String::new(),
                    kind: "test".into(),
                    link: None,
                    payload: serde_json::json!({}),
                })
                .await
                .expect("raise")
        }
    };
    raise(Some(alice), "for alice").await;
    raise(Some(bob), "for bob").await;
    raise(None, "for everyone").await;

    let seen = |who: UserId| {
        let state = state.clone();
        async move {
            let mut t: Vec<String> = state
                .notifications
                .list(
                    tenant,
                    who,
                    NotificationFilter {
                        unread_only: false,
                        limit: 50,
                    },
                )
                .await
                .expect("list")
                .into_iter()
                .map(|n| n.title)
                .collect();
            t.sort();
            t
        }
    };

    assert_eq!(
        seen(alice).await,
        vec!["for alice", "for everyone"],
        "alice sees her own and the tenant-wide one — never bob's"
    );
    assert_eq!(
        seen(bob).await,
        vec!["for bob", "for everyone"],
        "and the same holds the other way round"
    );
    assert_eq!(
        state
            .notifications
            .unread_count(tenant, alice)
            .await
            .expect("count"),
        2,
        "the badge counts the same rows the list shows"
    );

    bed.teardown().await;
}
