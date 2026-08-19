//! Removing a worktree is `DELETE /workspaces/{id}/worktrees/{checkout_id}`
//! (MAIN-642).
//!
//! Every assertion goes through `build_router` with a session cookie, because
//! what changed is the WIRE: a handler test cannot tell that
//! `POST …/worktrees/remove` stopped being routed, nor that the id in the path
//! is what identifies the tree. It also makes the scoping assertions mean
//! something — a caller reaches this route with a tenant, and the tenant comes
//! from the credential rather than from an argument.
//!
//! The fake node is a registered `NodeHandle` whose receiver a spawned task
//! drains and answers, which is the only way `request_op` resolves without a
//! real socket.
//!
//! Needs a database: `NOOK_REQUIRE_DB=1` in the suite.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use nook_control::state::AppState;
use nook_control::ws::registry::{NodeHandle, OpPayload};
use nook_db::dialect::type_mapping;
use nook_db::{params, Db};
use nook_proto::ControlToNode;
use nook_testkit::TestBed;
use nook_types::*;
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

/// A tenant member with a live session cookie, and the person id node ownership
/// keys on.
async fn signed_in(bed: &TestBed, tenant: TenantId) -> (Uuid, Uuid) {
    let (user, person) = bed.user(tenant, "owner").await;
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
    (sid, person)
}

/// A node owned by `owner`, registered online, with the channel the control
/// plane's ops arrive on.
async fn online_node(
    bed: &TestBed,
    state: &AppState,
    tenant: TenantId,
    owner: Uuid,
) -> (NodeId, mpsc::Receiver<ControlToNode>) {
    let id = bed.node(tenant, owner).await;
    let (tx, rx) = mpsc::channel::<ControlToNode>(4);
    state.registry.register_node(
        id,
        NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
    (id, rx)
}

async fn checkout(
    bed: &TestBed,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    kind: &str,
) -> NodeWorkspaceId {
    let id = NodeWorkspaceId::new();
    let now = type_mapping(bed.engine()).now();
    bed.db()
        .exec(
            &format!(
                "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind, last_scanned_at)
                 VALUES ($1, $2, $3, $4, $5, $6, {now})"
            ),
            params![id, tenant, node, ws, path, kind],
        )
        .await
        .expect("checkout");
    id
}

/// One request through the real router. It is built from the SAME `AppState`
/// the fake node registered on — `bed.app_state()` makes a fresh registry each
/// call, so a router built from a second one would find no node at all.
async fn request(
    state: &AppState,
    sid: Uuid,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("nook_session={sid}"));
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = nook_control::routes::build_router(state.clone())
        .oneshot(req.body(body).unwrap())
        .await
        .expect("the route answers");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn delete(state: &AppState, sid: Uuid, ws: WorkspaceId, id: NodeWorkspaceId) -> StatusCode {
    request(
        state,
        sid,
        Method::DELETE,
        &format!("/api/v1/workspaces/{ws}/worktrees/{id}"),
        None,
    )
    .await
    .0
}

/// Answer the one op the node is about to be sent, and hand back what it said.
fn answering(
    state: &AppState,
    mut rx: mpsc::Receiver<ControlToNode>,
) -> tokio::task::JoinHandle<ControlToNode> {
    let registry = state.registry.clone();
    tokio::spawn(async move {
        let msg = rx.recv().await.expect("the node is asked");
        let request_id = match &msg {
            ControlToNode::RemoveWorktree { request_id, .. } => *request_id,
            ControlToNode::AddWorktree { request_id, .. } => *request_id,
            other => panic!("unexpected op: {other:?}"),
        };
        registry.complete_op(
            request_id,
            OpPayload {
                ok: true,
                path: Some("/repo/wt".into()),
                message: "done".into(),
            },
        );
        msg
    })
}

#[tokio::test]
async fn delete_removes_the_tree_the_checkout_id_names() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("wtdel").await;
    let (sid, person) = signed_in(&bed, tenant).await;
    let (node, rx) = online_node(&bed, &state, tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let tree = checkout(&bed, tenant, node, ws, "/repo/wt", "worktree").await;

    let node_said = answering(&state, rx);
    let (status, body) = request(
        &state,
        sid,
        Method::DELETE,
        &format!("/api/v1/workspaces/{ws}/worktrees/{tree}"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    // The same `OpResponse` the POST answered with.
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["path"], serde_json::json!("/repo/wt"));
    assert_eq!(body["message"], serde_json::json!("done"));

    // AC-2/AC-5: the path came from the row, and the op is unchanged.
    match node_said.await.expect("the answering task") {
        ControlToNode::RemoveWorktree {
            worktree_path,
            delete_branch,
            ..
        } => {
            assert_eq!(worktree_path, "/repo/wt");
            assert!(!delete_branch, "a human removing a checkout said nothing");
        }
        other => panic!("expected RemoveWorktree, got {other:?}"),
    }

    bed.teardown().await;
}

/// AC-3: a real checkout id under a workspace that is not the one in the path
/// is not found there — and nothing is asked of the node.
#[tokio::test]
async fn a_checkout_of_another_workspace_is_not_found() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("wtws").await;
    let (sid, person) = signed_in(&bed, tenant).await;
    let (node, mut rx) = online_node(&bed, &state, tenant, person).await;
    let mine = bed.workspace(tenant).await;
    let theirs = bed.workspace(tenant).await;
    let their_tree = checkout(&bed, tenant, node, theirs, "/repo/theirs", "worktree").await;

    assert_eq!(
        delete(&state, sid, mine, their_tree).await,
        StatusCode::NOT_FOUND
    );
    // An unknown id is answered identically, which is what makes the 404 above
    // carry no information about the other workspace.
    assert_eq!(
        delete(&state, sid, mine, NodeWorkspaceId::new()).await,
        StatusCode::NOT_FOUND
    );
    assert!(
        rx.try_recv().is_err(),
        "the node was asked to remove a tree"
    );

    bed.teardown().await;
}

/// AC-3: and the same across tenants, where the caller may not even see the row.
#[tokio::test]
async fn a_checkout_of_another_tenant_is_not_found() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let mine = bed.tenant("wtmine").await;
    let other = bed.tenant("wtother").await;
    let (sid, person) = signed_in(&bed, mine).await;
    let (node, mut rx) = online_node(&bed, &state, mine, person).await;
    let ws = bed.workspace(mine).await;
    // Same workspace id in the path, but the ROW belongs to the other tenant —
    // so only the tenant scoping can refuse this one.
    let other_ws = bed.workspace(other).await;
    let their_tree = checkout(&bed, other, node, other_ws, "/repo/other", "worktree").await;

    assert_eq!(
        delete(&state, sid, ws, their_tree).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete(&state, sid, other_ws, their_tree).await,
        StatusCode::NOT_FOUND
    );
    assert!(
        rx.try_recv().is_err(),
        "the node was asked to remove a tree"
    );

    bed.teardown().await;
}

/// AC-4: the node check survives, and it runs AFTER the scoped lookup — so a
/// caller who may not use the machine is refused, while an id outside their
/// workspace is merely absent.
#[tokio::test]
async fn a_caller_who_may_not_use_the_node_is_refused_after_the_lookup() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("wtauth").await;
    let (_owner_sid, owner_person) = signed_in(&bed, tenant).await;
    let (stranger_sid, _stranger) = signed_in(&bed, tenant).await;
    let (node, mut rx) = online_node(&bed, &state, tenant, owner_person).await;
    let ws = bed.workspace(tenant).await;
    let tree = checkout(&bed, tenant, node, ws, "/repo/wt", "worktree").await;

    assert_eq!(
        delete(&state, stranger_sid, ws, tree).await,
        StatusCode::FORBIDDEN
    );
    // The lookup ran first, so an id the stranger cannot resolve is a 404 and
    // never the 403 that would confirm the row exists.
    let elsewhere = bed.workspace(tenant).await;
    assert_eq!(
        delete(&state, stranger_sid, elsewhere, tree).await,
        StatusCode::NOT_FOUND
    );
    assert!(
        rx.try_recv().is_err(),
        "the node was asked to remove a tree"
    );

    bed.teardown().await;
}

/// AC-1: the POST sub-path is gone from the route table. Nothing removes a tree
/// through it, and the id it would have to name is a checkout id now, not the
/// literal `remove`.
#[tokio::test]
async fn the_post_remove_sub_path_is_no_longer_routed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("wtpost").await;
    let (sid, person) = signed_in(&bed, tenant).await;
    let (node, mut rx) = online_node(&bed, &state, tenant, person).await;
    let ws = bed.workspace(tenant).await;
    checkout(&bed, tenant, node, ws, "/repo/wt", "worktree").await;

    let (status, _) = request(
        &state,
        sid,
        Method::POST,
        &format!("/api/v1/workspaces/{ws}/worktrees/remove"),
        Some(serde_json::json!({ "node_id": node, "path": "/repo/wt" })),
    )
    .await;
    // 405 rather than 404: `remove` is now just a checkout id that parses, so the
    // path pattern still matches and it is the METHOD the router has nothing for.
    // Either way nothing removes a tree through it, which is what AC-1 asks.
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        rx.try_recv().is_err(),
        "the node was asked to remove a tree"
    );

    bed.teardown().await;
}

/// AC-6/NG-1: creating one is untouched, and the collection has no `GET`.
#[tokio::test]
async fn creating_a_worktree_is_unchanged() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let state = bed.app_state().await;
    let tenant = bed.tenant("wtadd").await;
    let (sid, person) = signed_in(&bed, tenant).await;
    let (node, rx) = online_node(&bed, &state, tenant, person).await;
    let ws = bed.workspace(tenant).await;
    checkout(&bed, tenant, node, ws, "/repo", "clone").await;

    let node_said = answering(&state, rx);
    let (status, body) = request(
        &state,
        sid,
        Method::POST,
        &format!("/api/v1/workspaces/{ws}/worktrees"),
        Some(serde_json::json!({ "node_id": node, "branch": "feature" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    match node_said.await.expect("the answering task") {
        ControlToNode::AddWorktree {
            repo_path, branch, ..
        } => {
            assert_eq!(repo_path, "/repo");
            assert_eq!(branch, "feature");
        }
        other => panic!("expected AddWorktree, got {other:?}"),
    }

    let (status, _) = request(
        &state,
        sid,
        Method::GET,
        &format!("/api/v1/workspaces/{ws}/worktrees"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "no GET was added to the collection"
    );

    bed.teardown().await;
}
