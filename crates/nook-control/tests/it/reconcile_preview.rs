//! MAIN-431: the dry-run preview and the migrated status blocker.
//!
//! What needs a real database here is the route glue: tenant scoping, the
//! four reads feeding the planner, structured reasons surviving
//! serialization, and — the endpoint's whole contract — that it writes
//! nothing. The blocker RULES are unit-tested beside the planner and
//! deliberately not re-proved here.

use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_db::Db;
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

fn spec(selector: &[(&str, &str)], tolerations: &[(&str, &str)], count: u32) -> SessionSpec {
    SessionSpec {
        runtime: "claude".into(),
        node_selector: selector
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        tolerations: tolerations
            .iter()
            .map(|(k, e)| Toleration {
                key: k.to_string(),
                effect: e.to_string(),
            })
            .collect(),
        replicas: Replicas::Count { count },
    }
}

async fn preview(
    state: &nook_control::state::AppState,
    who: AuthCtx,
    ws: WorkspaceId,
    candidate: SessionSpec,
) -> Result<ReconcilePreview, ApiError> {
    nook_control::routes::workspaces::reconcile_preview(
        axum::extract::State(state.clone()),
        who,
        axum::extract::Path(ws),
        axum::Json(ReconcilePreviewRequest { spec: candidate }),
    )
    .await
    .map(|j| j.0)
}

fn online(state: &nook_control::state::AppState, tenant: TenantId, node: NodeId) {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    state.registry.register_node(
        node,
        nook_control::ws::registry::NodeHandle {
            tenant_id: tenant,
            tx,
        },
    );
}

/// The heart of the card: an excluded node reports EVERY ground, structurally,
/// and the whole call leaves the database exactly as it found it.
#[tokio::test]
async fn blocked_nodes_carry_every_ground_and_nothing_is_written() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("preview").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let good = bed.node(tenant, person).await;
    let bad = bed.node(tenant, person).await;
    let state = bed.app_state().await;

    // `good`: online, labelled to match, holding the checkout.
    online(&state, tenant, good);
    let placed = nook_control::routes::nodes::set_placement(
        axum::extract::State(state.clone()),
        ctx(user, tenant),
        axum::extract::Path(good),
        axum::Json(SetNodePlacementRequest {
            labels: [("role".to_string(), "eu".to_string())].into(),
            taints: vec![],
        }),
    )
    .await
    .expect("label good");
    // The label this whole case turns on: if it did not land, the preview
    // below would be asserting against a node that never matched the selector.
    assert_eq!(placed.labels.get("role").map(String::as_str), Some("eu"));
    state
        .workspaces
        .associate_clone(tenant, good, ws, "/w/repo", "repo.git", "repo")
        .await
        .expect("checkout");

    // `bad`: offline (never registered), missing the selector label, tainted.
    let tainted = nook_control::routes::nodes::set_placement(
        axum::extract::State(state.clone()),
        ctx(user, tenant),
        axum::extract::Path(bad),
        axum::Json(SetNodePlacementRequest {
            labels: Default::default(),
            taints: vec![NodeTaint {
                key: "gpu".into(),
                effect: "NoSchedule".into(),
            }],
        }),
    )
    .await
    .expect("taint bad");
    // Same reason: an unapplied taint would make `bad` blocked for the wrong
    // reason, and the preview's blocker list would still look right.
    assert_eq!(
        tainted
            .taints
            .iter()
            .map(|t| t.key.as_str())
            .collect::<Vec<_>>(),
        vec!["gpu"]
    );

    let sessions_before: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM sessions WHERE tenant_id = $1",
            nook_db::params![tenant],
        )
        .await
        .expect("count");

    let got = preview(
        &state,
        ctx(user, tenant),
        ws,
        spec(&[("role", "eu")], &[], 2),
    )
    .await
    .expect("preview");

    assert_eq!(got.matched.len(), 1);
    assert_eq!(got.matched[0].node_id, good);
    assert!(
        !got.matched[0].node_name.is_empty(),
        "names ride along, not just ids"
    );
    assert!(got.needs_clone.is_empty());
    assert_eq!(got.ineligible.len(), 1);
    let b = &got.ineligible[0];
    assert_eq!(b.node_id, bad);
    // Every ground, in the fixed order: offline → selector → taint. (No
    // runtime ground: the testkit node reported no runtimes, and empty means
    // unknown, not incapable.)
    assert_eq!(
        b.reasons,
        vec![
            NodeBlocker::Offline,
            NodeBlocker::SelectorMismatch {
                key: "role".into(),
                wanted: "eu".into(),
                actual: None,
            },
            NodeBlocker::UntoleratedTaint {
                key: "gpu".into(),
                effect: "NoSchedule".into(),
            },
        ]
    );
    // The wire shape is the internally-tagged one the UI switches on.
    let json = serde_json::to_value(&b.reasons[2]).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({"kind": "untolerated_taint", "key": "gpu", "effect": "NoSchedule"})
    );

    // Writes nothing: no session appeared, and the workspace still has no spec.
    let sessions_after: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM sessions WHERE tenant_id = $1",
            nook_db::params![tenant],
        )
        .await
        .expect("count");
    assert_eq!(sessions_before, sessions_after);
    let saved: Option<serde_json::Value> = bed
        .db()
        .query_scalar(
            "SELECT session_spec FROM workspaces WHERE id = $1",
            nook_db::params![ws],
        )
        .await
        .expect("read spec");
    assert_eq!(saved, None, "a preview never saves the candidate");

    bed.teardown().await;
}

/// AC-9: eligible-but-cloneless is a separate answer from excluded.
#[tokio::test]
async fn an_eligible_node_without_a_checkout_is_needs_clone_not_ineligible() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("preview").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    online(&state, tenant, node);

    let got = preview(&state, ctx(user, tenant), ws, spec(&[], &[], 1))
        .await
        .expect("preview");
    assert!(got.matched.is_empty());
    assert!(got.ineligible.is_empty());
    assert_eq!(got.needs_clone.len(), 1);
    assert_eq!(got.needs_clone[0].node_id, node);
    assert_eq!(got.needs_clone[0].reason, NodeBlocker::NeedsClone);

    bed.teardown().await;
}

/// AC-12: tenant-scoped like every workspace read; no owner gate.
#[tokio::test]
async fn another_tenants_workspace_is_not_found_and_members_may_read() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("preview").await;
    let (member, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;
    let other = bed.tenant("previewb").await;
    let (outsider, _) = bed.user(other, "owner").await;
    let state = bed.app_state().await;

    let refused = preview(&state, ctx(outsider, other), ws, spec(&[], &[], 1)).await;
    assert!(
        matches!(refused, Err(ApiError::NotFound)),
        "another tenant's workspace reads as absent, never as forbidden"
    );

    // A plain member — not the owner — gets an answer: previews decide nothing.
    preview(&state, ctx(member, tenant), ws, spec(&[], &[], 1))
        .await
        .expect("member preview");

    bed.teardown().await;
}

/// AC-13: an impossible candidate is refused by name, before any read.
#[tokio::test]
async fn an_invalid_candidate_spec_is_a_400_naming_the_problem() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("preview").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let mut blank = spec(&[], &[], 1);
    blank.runtime = "  ".into();
    let refused = preview(&state, ctx(user, tenant), ws, blank).await;
    assert!(
        matches!(refused, Err(ApiError::BadRequest(ref m)) if m.contains("runtime")),
        "a blank runtime is named: {refused:?}"
    );

    let refused = preview(
        &state,
        ctx(user, tenant),
        ws,
        spec(&[], &[("gpu", "PreferNoSchedule")], 1),
    )
    .await;
    assert!(
        matches!(refused, Err(ApiError::BadRequest(ref m)) if m.contains("PreferNoSchedule")),
        "a non-NoSchedule effect is named: {refused:?}"
    );

    bed.teardown().await;
}

/// AC-6 / NG-6: `reconcile-status` reports the same needs-clone nodes it
/// always did — the reason just stopped being a string.
#[tokio::test]
async fn reconcile_status_blockers_are_structural_and_unchanged_in_content() {
    use nook_control::repo::admin::SettingWrite;

    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("preview").await;
    let (user, person) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let node = bed.node(tenant, person).await;
    let state = bed.app_state().await;
    online(&state, tenant, node);
    state
        .settings
        .put(SettingWrite {
            tenant,
            scope: "tenant".into(),
            user: None,
            key: nook_control::services::session_reconcile::KEY.into(),
            value: serde_json::json!(true),
        })
        .await
        .expect("enable reconcile");

    let got = nook_control::routes::workspaces::reconcile_status(
        axum::extract::State(state.clone()),
        ctx(user, tenant),
        axum::extract::Path(ws),
    )
    .await
    .expect("status")
    .0;
    assert_eq!(got.blocked.len(), 1, "still exactly the needs-clone node");
    assert_eq!(got.blocked[0].node_id, node);
    assert_eq!(got.blocked[0].reason, NodeBlocker::NeedsClone);
    assert_eq!(
        serde_json::to_value(&got.blocked[0].reason).expect("serialize"),
        serde_json::json!({"kind": "needs_clone"})
    );

    bed.teardown().await;
}
