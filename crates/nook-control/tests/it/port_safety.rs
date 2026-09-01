//! Which workspaces are held to one session per node, and why (MAIN-361).
//!
//! The planner's arithmetic is unit-tested beside the planner. What needs a
//! database is the DERIVATION — the question "has this workspace said what it
//! binds", asked of the stored declaration on every read.
//!
//! The distinction that carries this card is between *no declaration* and *a
//! declaration of nothing*. They are one keystroke apart in `.nook.toml` and one
//! JSON character apart in the column, and they mean opposite things: the first
//! is "nook does not know what this repo binds" and the second is the repo
//! saying it binds nothing at all. Getting them the wrong way round either caps
//! a workspace that was explicit, or fails to cap one that never said anything.

use nook_control::services::session_reconcile::{port_safety, PortSafety};
use nook_testkit::TestBed;
use nook_types::*;

async fn declare(bed: &TestBed, tenant: TenantId, ws: WorkspaceId, value: serde_json::Value) {
    bed.app_state()
        .await
        .workspaces
        .set_port_requirements(tenant, ws, Some(value))
        .await
        .expect("declare");
}

#[tokio::test]
async fn a_workspace_that_has_said_nothing_is_capped() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Undeclared,
        "a fresh workspace has declared nothing"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn declaring_a_listener_lifts_the_cap_and_nothing_else_is_needed() {
    // AC-8. The cap is not a state to clear — it is an answer that changes.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    declare(
        &bed,
        tenant,
        ws,
        serde_json::json!([{ "name": "web", "env": "PORT", "protocol": "tcp", "required": false }]),
    )
    .await;

    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Declared
    );

    bed.teardown().await;
}

#[tokio::test]
async fn declaring_zero_listeners_is_a_statement_and_is_safe() {
    // AC-1's escape hatch, and NG-5's reason for not adding a setting: saying
    // "this repo binds nothing" is exactly as good as naming a listener, and it
    // lives where somebody can see it rather than in a toggle nobody remembers.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    declare(&bed, tenant, ws, serde_json::json!([])).await;

    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Declared,
        "an empty declaration IS a declaration"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_hand_configured_workspace_is_safe_with_no_file_in_its_repo() {
    // AC-1, and the assertion that makes this card independent of the
    // `.nook.toml` card: the check is on the STORED declaration, never on
    // whether a file exists. Nothing here touches a checkout.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    // Exactly what `PUT /workspaces/{id}/ports` stores — no repo involved.
    declare(
        &bed,
        tenant,
        ws,
        serde_json::json!([{ "name": "api", "env": "API_PORT", "protocol": "tcp", "required": true }]),
    )
    .await;

    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Declared
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_stored_json_null_is_not_a_declaration() {
    // The column being absent and holding JSON `null` mean the same thing, and
    // a `null` reading as "declared" would silently un-cap a workspace that
    // never said anything.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    declare(&bed, tenant, ws, serde_json::Value::Null).await;

    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Undeclared
    );

    bed.teardown().await;
}

#[tokio::test]
async fn clearing_a_declaration_puts_the_cap_back() {
    // The derivation runs on every read (AC-2), so this needs no invalidation
    // and no event — the next answer is simply different.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("safety").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    declare(
        &bed,
        tenant,
        ws,
        serde_json::json!([{ "name": "web", "env": "PORT", "protocol": "tcp", "required": false }]),
    )
    .await;
    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Declared
    );

    state
        .workspaces
        .set_port_requirements(tenant, ws, None)
        .await
        .expect("clear");

    assert_eq!(
        port_safety(&state, tenant, ws).await.expect("derive"),
        PortSafety::Undeclared
    );

    bed.teardown().await;
}
