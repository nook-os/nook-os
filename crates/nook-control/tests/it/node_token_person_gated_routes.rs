//! The MAIN-577 audit's other half: routes that resolve the CALLER's person to
//! gate an action on a machine, reached from an unguarded `AuthCtx`.
//!
//! `AuthCtx::require_node_owner` / `require_node_may_use` answer a node
//! principal first — "a node token can only act on its own machine" — and only
//! then fall through to the person rule. Three routes call
//! `require_person_owns_node` / `require_person_may_use_node` DIRECTLY instead,
//! skipping that leg, so a node token's borrowed tenant-owner identity was
//! resolved into the owner's person and passed the ownership check on every
//! machine the owner owns.
//!
//! The nodes on the fixture are the owner's own, because that is the case that
//! used to PASS; a stranger's machine was refused either way and would prove
//! nothing.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::routes::{nodes, runtime_auth, workspaces};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn ctx(user: UserId, tenant: TenantId, principal: Principal) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal,
        cookie_session: false,
    }
}

#[track_caller]
fn assert_refused(what: &str, err: ApiError) {
    assert!(
        matches!(err, ApiError::ForbiddenMsg(_)),
        "{what}: a node principal must be refused by `require_user`, got {err:?}"
    );
    assert_eq!(
        err.into_response().status(),
        StatusCode::FORBIDDEN,
        "{what}: the refusal must be the same 403 the rest of the API gives a node token"
    );
}

#[tokio::test]
async fn a_node_token_cannot_act_on_the_owners_machines_through_the_person_rule() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping node-token person-gate test — no DATABASE_URL");
        return;
    };
    let state = bed.app_state().await;

    let tenant = bed.tenant("gate").await;
    let (owner, owner_person) = bed.user(tenant, "owner").await;
    // A machine the OWNER owns — what the borrowed identity used to unlock.
    let machine = bed.node(tenant, owner_person).await;
    let workspace = bed.workspace(tenant).await;
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_url = $1 WHERE id = $2",
            params!["https://example.test/repo.git".to_string(), workspace],
        )
        .await
        .expect("give the workspace a stored remote");

    // Exactly what `node_token_ctx` builds: the tenant owner's user id, marked
    // as a node. `NodeId` is a DIFFERENT machine's, so a route that confines a
    // node to itself refuses on that basis and one that resolves the person
    // does not.
    let node_ctx = ctx(owner, tenant, Principal::Node(NodeId::new()));
    let user_ctx = ctx(owner, tenant, Principal::User);

    assert_refused(
        "authorize a runtime on a node",
        nodes::authorize(
            State(state.clone()),
            node_ctx,
            Path(machine),
            Json(AuthorizeRuntimeRequest {
                runtime: "claude".into(),
            }),
        )
        .await
        .expect_err("a node token must not start a device-flow login on the owner's machine"),
    );

    assert_refused(
        "start a sessionless runtime authorization",
        runtime_auth::start(
            State(state.clone()),
            node_ctx,
            Json(runtime_auth::RuntimeAuthRequest {
                runtime: "claude".into(),
                node_ids: vec![machine],
            }),
        )
        .await
        .expect_err("a node token must not start a runtime-auth flow on the owner's machines"),
    );

    assert_refused(
        "clone a workspace onto a node",
        workspaces::clone_to_node(
            State(state.clone()),
            node_ctx,
            Path(workspace),
            Json(WorkspaceCloneRequest {
                node_id: machine,
                credential_id: None,
            }),
        )
        .await
        .expect_err("a node token must not write a checkout onto the owner's machine"),
    );

    // The user leg is untouched: the owner still gets past the principal check
    // and is judged on ownership as before. These fail further in (the node is
    // offline, the runtime has no descriptor here) — what matters is that the
    // refusal is no longer the 403 above.
    for (what, err) in [
        (
            "authorize",
            nodes::authorize(
                State(state.clone()),
                user_ctx,
                Path(machine),
                Json(AuthorizeRuntimeRequest {
                    runtime: "claude".into(),
                }),
            )
            .await
            .err(),
        ),
        (
            "clone",
            workspaces::clone_to_node(
                State(state.clone()),
                user_ctx,
                Path(workspace),
                Json(WorkspaceCloneRequest {
                    node_id: machine,
                    credential_id: None,
                }),
            )
            .await
            .err(),
        ),
    ] {
        if let Some(ApiError::ForbiddenMsg(m)) = &err {
            assert!(
                !m.contains("sign in as a user"),
                "{what}: the owner must reach the ownership rule, not the principal check"
            );
        }
    }

    bed.teardown().await;
}
