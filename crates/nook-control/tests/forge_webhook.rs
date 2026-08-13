//! The signed GitHub receiver (MAIN-554).
//!
//! Every delivery here goes through the REAL router — `routes::build_router` —
//! rather than by calling the handler. Two of the properties under test are
//! properties of the wiring and not of the function: the 8 MiB body limit is a
//! layer, and "unauthenticated" means the route resolves with no `AuthCtx`
//! extractor in front of it. A test that called `hooks::github` directly would
//! pass with the route unmounted, or mounted behind auth.
//!
//! Bodies are the checked-in fixtures, signed at runtime — nothing here holds a
//! precomputed signature, so a change to the scheme fails loudly instead of
//! matching a stale constant. The scheme itself is checked against GitHub's own
//! published vector in `services::forge_webhook`'s unit tests.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::Router;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::build_router;
use nook_control::routes::workspaces::{clear_webhook_secret, get_webhook, set_webhook_secret};
use nook_control::services::forge_webhook as hook;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;
use uuid::Uuid;

const PING: &str = include_str!("fixtures/github/ping.json");
const PR_CLOSED: &str = include_str!("fixtures/github/pull_request.closed.json");

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// A workspace that is a checkout of `github.com/acme/api` — the repository
/// both fixtures name.
async fn acme_workspace(bed: &TestBed, tenant: TenantId) -> WorkspaceId {
    let ws = bed.workspace(tenant).await;
    let touched = bed
        .db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $2, git_remote_normalized = $3 WHERE id = $1",
            params![ws, "git@github.com:acme/api.git", "github.com/acme/api"],
        )
        .await
        .expect("give the workspace a GitHub remote");
    // An UPDATE that matched nothing would leave a remote-less workspace, and
    // "nothing to compare" passes the consistency assert vacuously — which
    // would make the mismatch assertions below prove nothing.
    assert_eq!(touched, 1, "the workspace took the remote");
    ws
}

/// Seal a secret onto the workspace the way the PUT route does.
async fn give_secret(bed: &TestBed, tenant: TenantId, ws: WorkspaceId, secret: &str) {
    let state = bed.app_state().await;
    let sealed = state.vault.encrypt(secret.as_bytes()).expect("seal");
    assert!(state
        .workspaces
        .set_webhook_secret_sealed(tenant, ws, Some(sealed))
        .await
        .expect("store"));
}

/// One delivery, as GitHub would send it.
struct Delivery<'a> {
    event: &'a str,
    id: &'a str,
    body: Vec<u8>,
    /// The signature header to send, or `None` to send none at all.
    signature: Option<String>,
}

impl<'a> Delivery<'a> {
    fn signed(event: &'a str, id: &'a str, body: impl Into<Vec<u8>>, secret: &str) -> Self {
        let body = body.into();
        let signature = Some(hook::sign(secret, &body));
        Self {
            event,
            id,
            body,
            signature,
        }
    }
}

async fn post(app: Router, ws: WorkspaceId, d: Delivery<'_>) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/hooks/github/{}", ws.0))
        .header("content-type", "application/json")
        .header(hook::EVENT_HEADER, d.event)
        .header(hook::DELIVERY_HEADER, d.id);
    if let Some(sig) = &d.signature {
        req = req.header(hook::SIGNATURE_HEADER, sig);
    }
    let res = app
        .oneshot(req.body(Body::from(d.body)).unwrap())
        .await
        .expect("the receiver answers");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Every recorded status for a workspace, oldest first.
async fn statuses(bed: &TestBed, ws: WorkspaceId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT status FROM forge_deliveries WHERE workspace_id = $1 ORDER BY received_at",
            params![ws],
        )
        .await
        .expect("read the deliveries")
}

/// The fixture with `repository.full_name` replaced — the whole body is
/// re-serialized and re-signed, so this is a different delivery in every
/// respect that matters and not a patched signature.
fn with_repo(fixture: &str, full_name: &str) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_str(fixture).expect("fixture parses");
    v["repository"]["full_name"] = serde_json::Value::String(full_name.to_string());
    serde_json::to_vec(&v).expect("re-serialize")
}

/// AC-2, AC-5: a signed delivery is recorded once and answered 202; the
/// redelivery GitHub's own UI offers writes no second row and is answered 200.
#[tokio::test]
async fn a_signed_delivery_is_recorded_once_and_a_redelivery_is_recognised() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hookpr").await;
    let ws = acme_workspace(&bed, tenant).await;
    give_secret(&bed, tenant, ws, "s3cret").await;
    let app = build_router(bed.app_state().await);

    let (status, body) = post(
        app.clone(),
        ws,
        Delivery::signed("pull_request", "d-1", PR_CLOSED, "s3cret"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(statuses(&bed, ws).await, vec![hook::STATUS_RECEIVED]);

    let (status, body) = post(
        app,
        ws,
        Delivery::signed("pull_request", "d-1", PR_CLOSED, "s3cret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a redelivery is not a new delivery");
    assert!(body.contains("\"duplicate\":true"), "{body}");
    assert_eq!(
        statuses(&bed, ws).await.len(),
        1,
        "the second delivery wrote no row"
    );

    bed.teardown().await;
}

/// AC-4/AC-5: the signature is the gate. A tampered body, the wrong secret and
/// an absent header are each a 401 that records nothing.
#[tokio::test]
async fn an_unsigned_or_badly_signed_delivery_is_401_and_records_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hook401").await;
    let ws = acme_workspace(&bed, tenant).await;
    give_secret(&bed, tenant, ws, "s3cret").await;
    let app = build_router(bed.app_state().await);

    // Signed correctly, then the body is changed underneath the signature.
    let mut tampered = Delivery::signed("pull_request", "d-t", PR_CLOSED, "s3cret");
    tampered.body = with_repo(PR_CLOSED, "acme/api");

    let attempts = vec![
        tampered,
        Delivery::signed("pull_request", "d-w", PR_CLOSED, "wrong-secret"),
        Delivery {
            event: "pull_request",
            id: "d-n",
            body: PR_CLOSED.as_bytes().to_vec(),
            signature: None,
        },
        // `notify::sign`'s framing — a real signature of a real body, in the
        // scheme this endpoint deliberately does not speak.
        Delivery {
            event: "pull_request",
            id: "d-o",
            body: PR_CLOSED.as_bytes().to_vec(),
            signature: Some(nook_control::services::notify::sign(
                "s3cret",
                PR_CLOSED,
                1_700_000_000,
            )),
        },
    ];
    for attempt in attempts {
        let id = attempt.id.to_string();
        let (status, body) = post(app.clone(), ws, attempt).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{id}: {body}");
    }
    assert!(
        statuses(&bed, ws).await.is_empty(),
        "a refused delivery records nothing"
    );

    bed.teardown().await;
}

/// AC-5: an id that names no workspace, and one that names a workspace nobody
/// configured a secret on, are both 404 — "there is nothing here", which is the
/// true statement and the one an operator can act on.
#[tokio::test]
async fn an_unknown_workspace_or_one_with_no_secret_is_404() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hook404").await;
    let unconfigured = acme_workspace(&bed, tenant).await;
    let app = build_router(bed.app_state().await);

    for ws in [WorkspaceId::new(), unconfigured] {
        let (status, body) = post(
            app.clone(),
            ws,
            Delivery::signed("pull_request", "d-404", PR_CLOSED, "s3cret"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
    assert!(statuses(&bed, unconfigured).await.is_empty());

    bed.teardown().await;
}

/// AC-5: the repository is a consistency assert, folded for case.
///
/// The case half is the one that would ship broken and stay broken:
/// `git_remote_normalized` is lowercased by `normalize_remote` while GitHub
/// reports its canonical casing, so a repo genuinely named `Acme/API` would
/// have every correct delivery refused under an exact compare.
#[tokio::test]
async fn a_foreign_repository_is_422_and_recorded_while_a_case_difference_is_accepted() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hookrepo").await;
    let ws = acme_workspace(&bed, tenant).await;
    give_secret(&bed, tenant, ws, "s3cret").await;
    let app = build_router(bed.app_state().await);

    let (status, body) = post(
        app.clone(),
        ws,
        Delivery::signed(
            "pull_request",
            "d-foreign",
            with_repo(PR_CLOSED, "someone-else/other"),
            "s3cret",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        statuses(&bed, ws).await,
        vec![hook::STATUS_ERROR],
        "a mismatch is recorded, not dropped — an operator has to be able to see it"
    );

    let (status, body) = post(
        app,
        ws,
        Delivery::signed(
            "pull_request",
            "d-case",
            with_repo(PR_CLOSED, "Acme/API"),
            "s3cret",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        statuses(&bed, ws).await,
        vec![hook::STATUS_ERROR, hook::STATUS_RECEIVED]
    );

    bed.teardown().await;
}

/// AC-6: `ping` is accepted and filed as deliberately ignored, so pressing
/// **Redeliver ping** in GitHub is a working end-to-end setup test.
#[tokio::test]
async fn a_ping_is_accepted_and_recorded_as_ignored() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hookping").await;
    let ws = acme_workspace(&bed, tenant).await;
    give_secret(&bed, tenant, ws, "s3cret").await;
    let app = build_router(bed.app_state().await);

    let (status, body) = post(app, ws, Delivery::signed("ping", "d-ping", PING, "s3cret")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(statuses(&bed, ws).await, vec![hook::STATUS_IGNORED]);

    bed.teardown().await;
}

/// AC-7: the route's own 8 MiB limit, not axum's 2 MiB default.
///
/// The default is what makes this worth a test: a 3 MiB `check_suite` from a
/// big repo would be refused with no row and no log, and GitHub would show a
/// red 413 nobody here could explain. The body is signed correctly, so a 413
/// can only be the limit.
#[tokio::test]
async fn a_body_over_eight_mebibytes_is_413_and_records_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hookbig").await;
    let ws = acme_workspace(&bed, tenant).await;
    give_secret(&bed, tenant, ws, "s3cret").await;
    let app = build_router(bed.app_state().await);

    let mut oversized: serde_json::Value = serde_json::from_str(PR_CLOSED).unwrap();
    oversized["padding"] = serde_json::Value::String("x".repeat(8 * 1024 * 1024));
    let body = serde_json::to_vec(&oversized).unwrap();
    assert!(body.len() > 8 * 1024 * 1024);

    let (status, _) = post(
        app.clone(),
        ws,
        Delivery::signed("pull_request", "d-big", body, "s3cret"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(statuses(&bed, ws).await.is_empty());

    // …and a delivery inside the limit but over axum's 2 MiB default is
    // accepted, which is the half that proves the limit was RAISED and not
    // merely present.
    let mut large: serde_json::Value = serde_json::from_str(PR_CLOSED).unwrap();
    large["padding"] = serde_json::Value::String("x".repeat(4 * 1024 * 1024));
    let body = serde_json::to_vec(&large).unwrap();
    let (status, body) = post(
        app,
        ws,
        Delivery::signed("pull_request", "d-4mb", body, "s3cret"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    bed.teardown().await;
}

/// Tenant isolation: a delivery is scoped by the workspace in its own path, and
/// the tenant comes out of that row. Another tenant's secret opens nothing, and
/// a delivery for A never lands on B.
#[tokio::test]
async fn a_delivery_signed_for_one_workspace_cannot_write_a_row_for_another() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant_a = bed.tenant("hooka").await;
    let tenant_b = bed.tenant("hookb").await;
    let ws_a = acme_workspace(&bed, tenant_a).await;
    let ws_b = acme_workspace(&bed, tenant_b).await;
    give_secret(&bed, tenant_a, ws_a, "secret-a").await;
    give_secret(&bed, tenant_b, ws_b, "secret-b").await;
    let app = build_router(bed.app_state().await);

    // A's secret against B's path: same repository, same payload, still 401.
    let (status, body) = post(
        app.clone(),
        ws_b,
        Delivery::signed("pull_request", "d-cross", PR_CLOSED, "secret-a"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(statuses(&bed, ws_b).await.is_empty());

    let (status, _) = post(
        app,
        ws_a,
        Delivery::signed("pull_request", "d-own", PR_CLOSED, "secret-a"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(statuses(&bed, ws_a).await.len(), 1);
    assert!(
        statuses(&bed, ws_b).await.is_empty(),
        "A's delivery is A's row"
    );

    // The row is scoped to the workspace's OWN tenant, never the other's.
    let owner: TenantId = bed
        .db()
        .query_scalar(
            "SELECT tenant_id FROM forge_deliveries WHERE workspace_id = $1",
            params![ws_a],
        )
        .await
        .expect("the row's tenant");
    assert_eq!(owner, tenant_a);

    bed.teardown().await;
}

/// AC-3: the secret is returned by the call that generates it and by nothing
/// else, and DELETE clears it.
#[tokio::test]
async fn the_secret_is_shown_exactly_once_and_never_read_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("hooksec").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let fresh = get_webhook(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert!(!fresh.set, "a new workspace receives nothing");
    assert!(
        fresh
            .delivery_url
            .contains(&format!("/api/v1/hooks/github/{}", ws.0)),
        "the URL an operator pastes into GitHub: {}",
        fresh.delivery_url
    );

    let generated = set_webhook_secret(State(state.clone()), auth, Path(ws))
        .await
        .expect("generate")
        .0;
    assert!(!generated.secret.is_empty());
    assert_eq!(generated.delivery_url, fresh.delivery_url);

    // The value is sealed at rest — the plaintext must not be recoverable from
    // the row without the vault, which is the part the type system cannot prove.
    let sealed: Option<Vec<u8>> = bed
        .db()
        .query_scalar(
            "SELECT webhook_secret_enc FROM workspaces WHERE id = $1",
            params![ws],
        )
        .await
        .expect("sealed read");
    let sealed = sealed.expect("present");
    assert!(!String::from_utf8_lossy(&sealed).contains(&generated.secret));
    assert_eq!(
        state.vault.decrypt_string(&sealed).expect("unseal"),
        generated.secret
    );

    // …and the read path reports the fact and nothing else. There is no field
    // on `WorkspaceWebhookState` to carry the secret, so the check that matters
    // is that a later reader cannot get it back at all.
    let after = get_webhook(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert!(after.set);

    // Rotation replaces rather than adds.
    let rotated = set_webhook_secret(State(state.clone()), auth, Path(ws))
        .await
        .expect("rotate")
        .0;
    assert_ne!(rotated.secret, generated.secret);

    let cleared = clear_webhook_secret(State(state.clone()), auth, Path(ws))
        .await
        .expect("clear")
        .0;
    assert!(!cleared.set);
    assert!(!state
        .workspaces
        .has_webhook_secret(tenant, ws)
        .await
        .expect("read"));

    bed.teardown().await;
}

/// Another tenant's workspace is a 404 on all three verbs — a uuid is not an
/// authorisation.
#[tokio::test]
async fn another_tenants_workspace_cannot_be_configured() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant_a = bed.tenant("hookxa").await;
    let tenant_b = bed.tenant("hookxb").await;
    let (user_b, _) = bed.user(tenant_b, "owner").await;
    let ws_a = bed.workspace(tenant_a).await;
    let state = bed.app_state().await;
    let auth_b = user_ctx(user_b, tenant_b);

    assert!(get_webhook(State(state.clone()), auth_b, Path(ws_a))
        .await
        .is_err());
    assert!(set_webhook_secret(State(state.clone()), auth_b, Path(ws_a))
        .await
        .is_err());
    assert!(
        clear_webhook_secret(State(state.clone()), auth_b, Path(ws_a))
            .await
            .is_err()
    );
    assert!(!state
        .workspaces
        .has_webhook_secret(tenant_a, ws_a)
        .await
        .expect("read"));

    bed.teardown().await;
}
