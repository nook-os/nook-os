//! What `GET /api/v1/workspaces/mentionable` answers (MAIN-633 AC-4).
//!
//! Through `build_router` with a session cookie, like the collection's own
//! suite: the two things worth pinning are properties of the STACK, not of the
//! handler. That the literal segment still routes here rather than into
//! `/workspaces/{id}` is one — a handler test would pass with the route mounted
//! in the wrong order — and that the tenant a cookie names is the only tenant a
//! menu can see is the other.
//!
//! Needs a database: `NOOK_REQUIRE_DB=1` in the suite.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

/// The route's own cap. Stated here rather than imported so that raising it in
/// the handler has to be a deliberate edit in two places.
const MENTION_LIMIT: usize = 10;

async fn signed_in(bed: &TestBed, tenant: TenantId) -> Uuid {
    let (user, _) = bed.user(tenant, "owner").await;
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'owner')",
            params![Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");
    let sid = Uuid::new_v4();
    let expires = nook_db::dialect::time_math(bed.db().engine()).now_plus_scaled("$4", "1 hour");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
                 VALUES ($1, $2, $3, {expires})"
            ),
            params![sid, user, tenant, 1_i32],
        )
        .await
        .expect("session");
    sid
}

async fn workspace(bed: &TestBed, tenant: TenantId, slug: &str, name: &str) -> WorkspaceId {
    let id = WorkspaceId::new();
    bed.db()
        .exec(
            "INSERT INTO workspaces (id, tenant_id, name, slug) VALUES ($1, $2, $3, $4)",
            params![id, tenant, name, slug],
        )
        .await
        .expect("create workspace");
    id
}

async fn menu(bed: &TestBed, sid: Uuid, query: &str) -> (StatusCode, Vec<WorkspaceMention>) {
    let req = Request::builder()
        .uri(format!("/api/v1/workspaces/mentionable?q={query}"))
        .header(header::COOKIE, format!("nook_session={sid}"));
    let resp = nook_control::routes::build_router(bed.app_state().await)
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("the route answers");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn a_prefix_narrows_the_menu_to_the_workspaces_it_opens() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mention-prefix").await;
    let sid = signed_in(&bed, tenant).await;
    let web = workspace(&bed, tenant, "nook-web", "Nook Web").await;
    workspace(&bed, tenant, "nook-api", "Nook API").await;
    workspace(&bed, tenant, "billing", "Billing").await;

    // A bare `@` is the whole tenant, alphabetically — what a picker opens on.
    let (status, all) = menu(&bed, sid, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        all.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
        ["billing", "nook-api", "nook-web"]
    );

    let (_, narrowed) = menu(&bed, sid, "nook-w").await;
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].workspace_id, web);
    assert_eq!(narrowed[0].name, "Nook Web");

    // The NAME matches too, case-folded, because "which repo was that" is
    // answered by the name far more often than by the slug.
    let (_, by_name) = menu(&bed, sid, "Bill").await;
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].slug, "billing");

    // A prefix, not a substring: `web` must not surface `nook-web`, or `@e`
    // offers most of the tenant.
    let (_, mid) = menu(&bed, sid, "web").await;
    assert!(
        mid.is_empty(),
        "a middle-of-the-word match is not offered: {mid:?}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_menu_is_capped_however_many_workspaces_a_tenant_has() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mention-cap").await;
    let sid = signed_in(&bed, tenant).await;
    for i in 0..MENTION_LIMIT + 5 {
        workspace(&bed, tenant, &format!("repo-{i:02}"), &format!("Repo {i}")).await;
    }

    let (status, rows) = menu(&bed, sid, "repo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.len(), MENTION_LIMIT);
    // Capped by an ORDER, so the same keystrokes always list the same rows —
    // a cap over an unordered read is a menu that reshuffles as you type.
    assert_eq!(rows[0].slug, "repo-00");
    assert_eq!(rows[MENTION_LIMIT - 1].slug, "repo-09");

    bed.teardown().await;
}

/// NG-3, at the endpoint: a slug is a slug within one tenant or it is nothing.
/// The menu must not even be able to NAME another tenant's repo, or the picker
/// becomes a way to enumerate them.
#[tokio::test]
async fn a_menu_never_carries_another_tenants_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mention-mine").await;
    let theirs = bed.tenant("mention-theirs").await;
    let sid = signed_in(&bed, mine).await;
    workspace(&bed, mine, "shared-name", "Shared").await;
    workspace(&bed, theirs, "shared-name-too", "Shared Too").await;

    let (status, rows) = menu(&bed, sid, "shared").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
        ["shared-name"]
    );

    bed.teardown().await;
}
