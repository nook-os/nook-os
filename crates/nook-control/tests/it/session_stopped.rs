//! MAIN-415: `stopped` — declared, resumable, costing nothing.
//!
//! The whole feature is one distinction held in two places at once: a stopped
//! session **satisfies its workspace's declaration** and **occupies nothing**.
//! Get the first wrong and the reconciler starts a replacement within a poll
//! interval, so Stop silently undoes itself. Get the second wrong and the
//! machine stays busy for a session that is not running, which is the reason
//! stopping exists at all.
//!
//! These pin both answers, and the third thing that can quietly ruin it: the
//! node reporting the tmux it was told to kill must not rewrite `stopped` to
//! `exited`, or "you stopped it" and "it crashed" become the same row.

use nook_control::repo::sessions::NewSession;
use nook_control::session_status;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    checkout: NodeWorkspaceId,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("m415").await;
    let (_user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    let checkout = NodeWorkspaceId(Uuid::now_v7());
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path)
             VALUES ($1, $2, $3, $4, $5)",
            params![
                checkout,
                tenant,
                node,
                workspace,
                format!("/w/{}", checkout.0.simple())
            ],
        )
        .await
        .expect("checkout");
    Fixture {
        tenant,
        node,
        workspace,
        checkout,
    }
}

async fn managed_session(bed: &TestBed, f: &Fixture) -> SessionId {
    bed.app_state()
        .await
        .sessions
        .create(NewSession {
            tenant: f.tenant,
            workspace_id: Some(f.workspace),
            node_id: f.node,
            name: "bash (managed)".into(),
            runtime: "bash".into(),
            created_by: None,
            checkout_id: Some(f.checkout),
            managed: true,
            managed_purpose: ManagedPurpose::Access,
            managed_shard: 0,
            managed_shards: 1,
            interface: SessionInterface::Terminal,
        })
        .await
        .expect("session")
        .id
}

#[tokio::test]
async fn a_stopped_managed_session_still_satisfies_the_declaration() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let id = managed_session(&bed, &f).await;

    let before = state
        .sessions
        .live_managed(f.tenant, f.workspace, None)
        .await
        .unwrap();
    assert_eq!(before.len(), 1, "the running session satisfies it");

    assert_eq!(state.sessions.mark_stopped(f.tenant, id).await.unwrap(), 1);

    // AC-3, and the reason Stop sticks at all. `live_managed` is what the
    // reconciler counts; if a stopped session dropped out of it, the next pass
    // would start a replacement and the stop would look like it never happened.
    let after = state
        .sessions
        .live_managed(f.tenant, f.workspace, None)
        .await
        .unwrap();
    assert_eq!(
        after.len(),
        1,
        "a stopped managed session still counts — nothing to replace"
    );
    assert_eq!(after[0].id, id, "and it is the same session, not a new one");

    bed.teardown().await;
}

#[tokio::test]
async fn a_stopped_session_occupies_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let id = managed_session(&bed, &f).await;

    // A lease it holds while it is running.
    bed.db()
        .exec(
            "INSERT INTO session_port_leases (id, node_id, session_id, name, env, port)
             VALUES ($1, $2, $3, 'web', 'NOOK_WEB_PORT', 4242)",
            params![Uuid::now_v7(), f.node, id],
        )
        .await
        .expect("lease");

    assert_eq!(
        state.nodes.live_session_ids(f.node).await.unwrap(),
        vec![id],
        "a running session is on the node"
    );

    state.sessions.mark_stopped(f.tenant, id).await.unwrap();

    // AC-4. Nothing releases a lease when a session stops — the allocator drops
    // the rows of non-live sessions as its first step, so the port comes back
    // the moment somebody asks for one. `stopped` not being LIVE is what makes
    // that true, with no cleanup path to forget.
    let free = state.sessions.reclaim_and_held_ports(f.node).await.unwrap();
    assert!(
        !free.contains(&4242),
        "the stopped session's port is no longer held: {free:?}"
    );
    let held: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM session_port_leases WHERE session_id = $1",
            params![id],
        )
        .await
        .unwrap();
    assert_eq!(held, 0, "its lease row is gone");

    assert!(
        state
            .nodes
            .live_session_ids(f.node)
            .await
            .unwrap()
            .is_empty(),
        "and it no longer counts against the node"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn the_node_reporting_a_dead_tmux_does_not_turn_a_stop_into_a_crash() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let id = managed_session(&bed, &f).await;

    state.sessions.mark_stopped(f.tenant, id).await.unwrap();
    // Exactly what arrives moments later: stopping kills the tmux, and the node
    // reports the death it was asked to cause. Without the LIVE guard on
    // `mark_session_exited` every Stop would land as `exited` — AC-6 gone, and
    // the reconciler would replace the session too.
    state.nodes.mark_session_exited(id, f.node).await.unwrap();

    let status: String = bed
        .db()
        .query_scalar("SELECT status FROM sessions WHERE id = $1", params![id])
        .await
        .unwrap();
    assert_eq!(status, session_status::STOPPED, "still stopped, not exited");

    bed.teardown().await;
}

/// The guard has to be *legible to its caller*, not merely effective.
///
/// The LIVE guard protected the row and nothing else, because the websocket
/// handler threw the result away — so a deliberate Stop still published
/// `exited` to the UI and recorded a `session.exited` event, which notifies as
/// "Session ended" at warning level. Stopping a handful of terminals raised a
/// crash alert for every one of them.
///
/// The fix reads the rows-affected count to decide whether this report ENDED
/// anything, so that count is now load-bearing rather than incidental. This
/// pins it in both directions: a stop reports nothing, a real crash reports
/// one. A future change that makes the update unconditional would keep the
/// row-status test above green and break only this one.
#[tokio::test]
async fn mark_session_exited_reports_whether_it_ended_anything() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    // A session that really crashed: the report ends it, and says so.
    let crashed = managed_session(&bed, &f).await;
    assert_eq!(
        state
            .nodes
            .mark_session_exited(crashed, f.node)
            .await
            .unwrap(),
        1,
        "an unannounced death IS an ending — the caller must be told so it can \
         publish `exited` and notify"
    );

    // A session we stopped ourselves: the node's report is the echo of our own
    // kill, and must read as "ended nothing".
    let stopped = managed_session(&bed, &f).await;
    state
        .sessions
        .mark_stopped(f.tenant, stopped)
        .await
        .unwrap();
    assert_eq!(
        state
            .nodes
            .mark_session_exited(stopped, f.node)
            .await
            .unwrap(),
        0,
        "a Stop's own echo must not read as an ending, or the handler announces \
         a crash that never happened"
    );

    // And a repeat of the same report — a redelivery, or a second node message
    // for a session already ended — is not a second ending either.
    assert_eq!(
        state
            .nodes
            .mark_session_exited(crashed, f.node)
            .await
            .unwrap(),
        0,
        "the second report of one death must not notify twice"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn stopping_something_already_dead_changes_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let id = managed_session(&bed, &f).await;

    state.sessions.mark_ended(f.tenant, id).await.unwrap();
    assert_eq!(
        state.sessions.mark_stopped(f.tenant, id).await.unwrap(),
        0,
        "a dead session cannot be stopped"
    );
    let status: String = bed
        .db()
        .query_scalar("SELECT status FROM sessions WHERE id = $1", params![id])
        .await
        .unwrap();
    assert_eq!(status, "exited", "and it is not relabelled as stopped");

    bed.teardown().await;
}

#[tokio::test]
async fn a_stopped_session_can_be_started_again() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;
    let id = managed_session(&bed, &f).await;

    state.sessions.mark_stopped(f.tenant, id).await.unwrap();
    // AC-2. `restart` is unchanged by this card and needs no status guard added
    // — this asserts that, rather than assuming it.
    let back = state.sessions.mark_restarting(id).await.unwrap();
    assert_eq!(back.status, "starting");
    assert!(
        back.ended_at.is_none(),
        "and it is not still marked as ended"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_workspace_with_no_checkout_is_still_unsatisfied() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    // NG-2's guard. `live_managed` decides whether the reconciler has work to
    // do, and it keeps requiring `checkout_id IS NOT NULL` — a session with no
    // checkout has never satisfied a declaration and does not start to now that
    // `stopped` counts. So a workspace still waiting for a clone is still seen
    // as unsatisfied, and cloning behaves exactly as it did.
    let adhoc = state
        .sessions
        .create(NewSession {
            tenant: f.tenant,
            workspace_id: Some(f.workspace),
            node_id: f.node,
            name: "no checkout".into(),
            runtime: "bash".into(),
            created_by: None,
            checkout_id: None,
            managed: true,
            managed_purpose: ManagedPurpose::Access,
            managed_shard: 0,
            managed_shards: 1,
            interface: SessionInterface::Terminal,
        })
        .await
        .expect("session")
        .id;
    state.sessions.mark_stopped(f.tenant, adhoc).await.unwrap();

    assert!(
        state
            .sessions
            .live_managed(f.tenant, f.workspace, None)
            .await
            .unwrap()
            .is_empty(),
        "a checkout-less session satisfies nothing, stopped or otherwise"
    );

    bed.teardown().await;
}
