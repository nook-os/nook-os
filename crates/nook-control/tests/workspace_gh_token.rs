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

/// MAIN-469: the paste is exercised against the workspace's repository before
/// it is sealed.
///
/// The stub stands in for GitHub — `forge_token_check.rs` holds the refusals to
/// their wording; what is proven HERE is the route's half of it: which tokens
/// reach the vault, and what happens to the one already in it.
mod paste_validation {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::Router;
    use nook_db::{params, Db};
    use std::sync::{Arc, Mutex};

    const GOOD: &str = "github_pat_good";
    const READ_ONLY: &str = "github_pat_readonly";

    /// How many requests the stub answered — the guard against a check that
    /// silently did nothing.
    ///
    /// `check_access` fails OPEN by design: a forge it cannot reach stores the
    /// token unverified rather than blocking an operator during an outage. That
    /// makes "the stub was never reached" and "the token was fine" the same
    /// observable, and it is how the first version of this test passed locally
    /// and failed in CI — the stub had been spawned inside another
    /// `#[tokio::test]`, whose runtime dropped and took the server task with it.
    /// Counting proves the refusal came from an answered request.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<usize>>);

    fn writes(headers: &HeaderMap) -> bool {
        headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .ends_with(GOOD)
    }

    async fn repo_read(State(seen): State<Seen>) -> (StatusCode, String) {
        *seen.0.lock().unwrap() += 1;
        (StatusCode::OK, r#"{"full_name":"acme/api"}"#.into())
    }

    async fn write_probe(State(seen): State<Seen>, headers: HeaderMap) -> (StatusCode, String) {
        *seen.0.lock().unwrap() += 1;
        if writes(&headers) {
            // The body was refused, not the token — which is the pass.
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                r#"{"message":"Validation Failed"}"#.into(),
            )
        } else {
            (
                StatusCode::FORBIDDEN,
                r#"{"message":"Resource not accessible by personal access token"}"#.into(),
            )
        }
    }

    /// A GitHub that answers for the whole of THIS test.
    ///
    /// Spawned by the test that uses it, never shared through a `OnceCell`: a
    /// task spawned in one `#[tokio::test]` dies with that test's runtime, so a
    /// shared stub is alive or dead depending on which test ran first.
    async fn github_stub() -> Seen {
        let seen = Seen::default();
        let app = Router::new()
            .route("/repos/{owner}/{name}", get(repo_read))
            .route("/repos/{owner}/{name}/labels", post(write_probe))
            .route("/repos/{owner}/{name}/pulls", post(write_probe))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Process-wide, which is why exactly ONE test in this binary sets it.
        std::env::set_var("NOOK_GITHUB_API_BASE", format!("http://{addr}"));
        seen
    }

    async fn github_workspace(bed: &TestBed, tenant: TenantId) -> WorkspaceId {
        let ws = bed.workspace(tenant).await;
        let touched = bed
            .db()
            .exec(
                "UPDATE workspaces SET git_remote_url = 'git@github.com:acme/api.git' WHERE id = $1",
                params![ws],
            )
            .await
            .expect("give the workspace a GitHub remote");
        // An UPDATE that matched nothing would leave a remote-less workspace,
        // which skips the check entirely and makes every assertion below vacuous.
        assert_eq!(touched, 1, "the workspace took the remote");
        ws
    }

    /// Both pastes in ONE test, deliberately: they share a stub whose lifetime
    /// is this test's runtime, and the environment variable pointing at it is
    /// process-wide.
    #[tokio::test]
    async fn a_token_that_cannot_post_a_verdict_never_reaches_the_vault() {
        let Some(mut bed) = TestBed::new().await else {
            return;
        };
        let seen = github_stub().await;
        let tenant = bed.tenant("ghcheck").await;
        let (user, _) = bed.user(tenant, "owner").await;
        let ws = github_workspace(&bed, tenant).await;
        let state = bed.app_state().await;
        let auth = user_ctx(user, tenant);

        let stored = set_gh_token(State(state.clone()), auth, Path(ws), req(Some(GOOD)))
            .await
            .expect("a token that can deliver a verdict is stored")
            .0;
        assert!(stored.set);

        // A fine-grained PAT defaults to read-only, and that is the paste this
        // card exists to catch: it authenticates and lists PRs, then dies at
        // delivery after a whole review has run.
        let refusal = set_gh_token(State(state.clone()), auth, Path(ws), req(Some(READ_ONLY)))
            .await
            .expect_err("a read-only token is refused");
        let said = format!("{refusal:?}");
        assert!(
            said.contains("Issues: write"),
            "the refusal names the missing permission: {said}"
        );

        // …and the WORKING token is still there. Checking before sealing is
        // what makes a bad paste cost nothing: the alternative replaces a live
        // credential with a dead one and stops the loop.
        let sealed = state
            .workspaces
            .gh_token_sealed(tenant, ws)
            .await
            .expect("sealed read")
            .expect("the previous token survives a refused paste");
        assert_eq!(state.vault.decrypt_string(&sealed).expect("unseal"), GOOD);

        assert!(
            *seen.0.lock().unwrap() > 0,
            "the verdict above must come from a forge that answered, not from a \
             check that quietly failed open"
        );

        bed.teardown().await;
    }

    #[tokio::test]
    async fn a_workspace_with_no_github_remote_stores_what_it_is_given() {
        let Some(mut bed) = TestBed::new().await else {
            return;
        };
        let tenant = bed.tenant("ghlocal").await;
        let (user, _) = bed.user(tenant, "owner").await;
        // A local bare repo (`/workspace/nook-dogfood.git`) is a supported
        // remote with no forge behind it. There is nothing to check the token
        // against, and refusing it would make such a workspace unable to hold
        // one at all — so this reaches no forge, and needs no stub.
        let ws = bed.workspace(tenant).await;
        let state = bed.app_state().await;

        let set = set_gh_token(
            State(state.clone()),
            user_ctx(user, tenant),
            Path(ws),
            req(Some(READ_ONLY)),
        )
        .await
        .expect("nothing to check is not a failure")
        .0;
        assert!(set.set);

        bed.teardown().await;
    }
}
