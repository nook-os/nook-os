//! MAIN-456: a workspace's own forge token.
//!
//! What is worth a real database here is the SECRET's handling, not the CRUD:
//! the token must go in through the vault and never come out through any read
//! path, and one tenant's workspace must be unreachable from another. The
//! precedence (workspace over fleet) is exercised where it lives — the forge
//! and the run env — not re-proven here.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::workspaces::{get_gh_token, set_gh_token};
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

fn req(token: Option<&str>) -> Json<SetWorkspaceGhTokenRequest> {
    Json(SetWorkspaceGhTokenRequest {
        token: token.map(str::to_string),
    })
}

#[tokio::test]
async fn the_token_is_sealed_stored_and_never_echoed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ghtok").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let fresh = get_gh_token(State(state.clone()), auth, Path(ws))
        .await
        .expect("read")
        .0;
    assert!(!fresh.set, "a new workspace holds no token");

    let set = set_gh_token(
        State(state.clone()),
        auth,
        Path(ws),
        req(Some("gho_secret123")),
    )
    .await
    .expect("set")
    .0;
    assert!(set.set);

    // The read path reports the FACT and nothing else — `WorkspaceGhTokenState`
    // has no token field to leak, which the type system already proves; what it
    // cannot prove is the STORAGE, so check the sealed bytes directly: the
    // plaintext must not be recoverable from the row without the vault.
    let sealed = state
        .workspaces
        .gh_token_sealed(tenant, ws)
        .await
        .expect("sealed read")
        .expect("present");
    assert!(
        !String::from_utf8_lossy(&sealed).contains("gho_secret123"),
        "the token must be sealed at rest, not stored as bytes of itself"
    );
    // …and the vault the services use gets the plaintext back.
    assert_eq!(
        state.vault.decrypt_string(&sealed).expect("unseal"),
        "gho_secret123"
    );

    let cleared = set_gh_token(State(state.clone()), auth, Path(ws), req(None))
        .await
        .expect("clear")
        .0;
    assert!(!cleared.set, "null clears back to the fleet fallback");

    bed.teardown().await;
}

#[tokio::test]
async fn a_blank_token_clears_rather_than_stores_an_empty_secret() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("ghblank").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;
    let auth = user_ctx(user, tenant);

    let _ = set_gh_token(State(state.clone()), auth, Path(ws), req(Some("gho_x")))
        .await
        .expect("set");
    // An empty credential is worse than none — `gh` prefers it over a logged-in
    // account — so whitespace is a CLEAR, the same rule the tmux export applies.
    let blank = set_gh_token(State(state.clone()), auth, Path(ws), req(Some("   ")))
        .await
        .expect("blank")
        .0;
    assert!(!blank.set);

    bed.teardown().await;
}

#[tokio::test]
async fn another_tenants_workspace_is_not_found() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant_a = bed.tenant("gha").await;
    let tenant_b = bed.tenant("ghb").await;
    let (user_b, _) = bed.user(tenant_b, "owner").await;
    let ws_a = bed.workspace(tenant_a).await;
    let state = bed.app_state().await;

    // Tenant B can neither set nor read tenant A's token state — a foreign
    // workspace is a 404, not an empty answer.
    let auth_b = user_ctx(user_b, tenant_b);
    assert!(
        set_gh_token(State(state.clone()), auth_b, Path(ws_a), req(Some("gho_x")))
            .await
            .is_err()
    );
    assert!(get_gh_token(State(state.clone()), auth_b, Path(ws_a))
        .await
        .is_err());

    bed.teardown().await;
}
