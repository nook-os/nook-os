//! Session callers against the in-memory fake, with **no database at all**
//! (MAIN-253 AC-3).
//!
//! Two rules here fail quietly rather than loudly, which is why they are worth
//! pinning: a member's session list must never show a teammate's terminal
//! (MAIN-133), and a status write must never resurrect a session that already
//! exited — a detach event arriving after the process died would otherwise mark
//! a dead session `detached` and leave it in the active list forever.
//!
//! `cargo test -p nook-control --test session_fake` passes with the database
//! stopped.

use nook_control::repo::sessions::{
    FakeSessionRepository, NewSession, SessionFilter, SessionRepository,
};
use nook_control::repo::workspaces::{
    CheckoutUpsert, FakeWorkspaceRepository, WorkspaceRepository,
};
use nook_control::services::session_queries::{hydrate_checkouts, list_sessions};
use nook_types::*;

fn tenant() -> TenantId {
    TenantId::new()
}

async fn start(
    repo: &FakeSessionRepository,
    t: TenantId,
    node: NodeId,
    workspace: Option<WorkspaceId>,
    creator: Option<UserId>,
    name: &str,
) -> Session {
    repo.create(NewSession {
        tenant: t,
        workspace_id: workspace,
        node_id: node,
        name: name.to_string(),
        runtime: "bash".into(),
        created_by: creator,
        checkout_id: None,
        managed: false,
        managed_purpose: ManagedPurpose::Access,
    })
    .await
    .unwrap()
}

// ── the creator scope (MAIN-133) ────────────────────────────────────────────

#[tokio::test]
async fn a_member_sees_only_their_own_sessions_and_an_admin_sees_all() {
    let sessions = FakeSessionRepository::new();
    let workspaces = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let (alice, bob) = (UserId::new(), UserId::new());

    start(&sessions, t, node, None, Some(alice), "alice's shell").await;
    start(&sessions, t, node, None, Some(bob), "bob's shell").await;
    // A legacy/MCP session with no recorded creator.
    start(&sessions, t, node, None, None, "legacy").await;

    let names = |v: Vec<Session>| {
        let mut n: Vec<String> = v.into_iter().map(|s| s.name).collect();
        n.sort();
        n
    };

    let mine = list_sessions(&sessions, &workspaces, t, None, false, Some(alice))
        .await
        .unwrap();
    assert_eq!(
        names(mine),
        vec!["alice's shell"],
        "a member's own view — never a teammate's terminal, and `NULL = user` is \
         never true so the legacy row stays out too"
    );

    let all = list_sessions(&sessions, &workspaces, t, None, false, None)
        .await
        .unwrap();
    assert_eq!(
        names(all),
        vec!["alice's shell", "bob's shell", "legacy"],
        "the unscoped view is every session, legacy rows included"
    );
}

#[tokio::test]
async fn another_tenants_sessions_are_invisible_and_untouchable() {
    let sessions = FakeSessionRepository::new();
    let workspaces = FakeWorkspaceRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let s = start(&sessions, theirs, NodeId::new(), None, None, "theirs").await;

    assert!(
        list_sessions(&sessions, &workspaces, mine, None, false, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(sessions.get(mine, s.id).await.unwrap().is_none());
    assert!(sessions
        .rename(s.id, mine, "renamed")
        .await
        .unwrap()
        .is_none());
    assert_eq!(sessions.delete(s.id, mine).await.unwrap(), 0);
    assert_eq!(sessions.count(), 1, "a wrong-tenant delete removes nothing");
}

#[tokio::test]
async fn the_active_filter_drops_finished_sessions() {
    let sessions = FakeSessionRepository::new();
    let workspaces = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();

    let live = start(&sessions, t, node, None, None, "live").await;
    let done = start(&sessions, t, node, None, None, "done").await;
    let failed = start(&sessions, t, node, None, None, "failed").await;
    sessions.force_status(done.id, "exited");
    sessions.force_status(failed.id, "error");

    let active = list_sessions(&sessions, &workspaces, t, None, true, None)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, live.id);
}

#[tokio::test]
async fn the_workspace_filter_narrows_to_one_repo() {
    let sessions = FakeSessionRepository::new();
    let workspaces = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let (a, b) = (WorkspaceId::new(), WorkspaceId::new());

    let in_a = start(&sessions, t, node, Some(a), None, "a").await;
    start(&sessions, t, node, Some(b), None, "b").await;
    // An ad-hoc terminal belongs to no workspace at all.
    start(&sessions, t, node, None, None, "terminal").await;

    let only_a = list_sessions(&sessions, &workspaces, t, Some(a), false, None)
        .await
        .unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].id, in_a.id);
}

// ── the live-status guard ───────────────────────────────────────────────────

#[tokio::test]
async fn a_late_detach_cannot_resurrect_a_finished_session() {
    let sessions = FakeSessionRepository::new();
    let t = tenant();
    let s = start(&sessions, t, NodeId::new(), None, None, "s").await;

    // The process died and the socket recorded it.
    sessions.force_status(s.id, "exited");

    // A detach event arrives afterwards — the last browser tab closing.
    let changed = sessions.mark_viewer_presence(s.id, false).await.unwrap();
    assert_eq!(
        changed, 0,
        "no row matched: the session is already finished"
    );
    assert_eq!(
        sessions.status_snapshot(s.id).as_deref(),
        Some("exited"),
        "without the `status IN (live…)` guard this would read `detached` and \
         the session would sit in the active list forever"
    );
}

/// Viewer presence moves a session between `detached` and `running`, and only
/// between those two.
#[tokio::test]
async fn a_viewer_arriving_and_leaving_moves_a_live_session() {
    let sessions = FakeSessionRepository::new();
    let t = tenant();
    let s = start(&sessions, t, NodeId::new(), None, None, "s").await;

    for (from, watched, to) in [
        ("detached", true, "running"),
        ("running", false, "detached"),
    ] {
        sessions.force_status(s.id, from);
        assert_eq!(
            sessions.mark_viewer_presence(s.id, watched).await.unwrap(),
            1
        );
        assert_eq!(sessions.status_snapshot(s.id).as_deref(), Some(to));
    }
}

/// The fix for the "session has no terminal yet" loop (MAIN-363): a browser tab
/// opening is not evidence that a process started. Only the node's
/// `SessionStarted` — which also carries the tmux name — may leave `starting`.
#[tokio::test]
async fn a_viewer_cannot_promote_a_session_that_never_started() {
    let sessions = FakeSessionRepository::new();
    let t = tenant();
    let s = start(&sessions, t, NodeId::new(), None, None, "s").await;

    assert_eq!(sessions.status_snapshot(s.id).as_deref(), Some("starting"));
    assert_eq!(
        sessions.mark_viewer_presence(s.id, true).await.unwrap(),
        0,
        "attaching must not lift `starting`; the row still has no tmux session"
    );
    assert_eq!(sessions.status_snapshot(s.id).as_deref(), Some("starting"));
}

#[tokio::test]
async fn a_restart_clears_the_previous_runs_error_and_end_time() {
    let sessions = FakeSessionRepository::new();
    let t = tenant();
    let s = start(&sessions, t, NodeId::new(), None, None, "s").await;
    sessions.mark_failed_to_start(s.id).await.unwrap();
    assert_eq!(sessions.status_snapshot(s.id).as_deref(), Some("error"));

    let restarted = sessions.mark_restarting(s.id).await.unwrap();
    assert_eq!(restarted.status, "starting");
    assert_eq!(restarted.error, None, "a restart is not still failed");
    assert_eq!(restarted.ended_at, None);
}

// ── checkout binding (MAIN-222) ─────────────────────────────────────────────

#[tokio::test]
async fn a_sessions_checkout_summary_is_filled_from_its_binding() {
    let sessions = FakeSessionRepository::new();
    let workspaces = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    workspaces.add_node(node, t, "workshop", "online");
    let ws = workspaces
        .create(t, "widgets", "widgets", None, None)
        .await
        .unwrap();
    workspaces
        .upsert_checkout(CheckoutUpsert {
            tenant: t,
            node_id: node,
            workspace_id: ws.id,
            path: "/srv/widgets".into(),
            git_remote_url: None,
            git_remote_normalized: None,
            branch: Some("main".into()),
            git_status: serde_json::json!({}),
            kind: "clone".into(),
        })
        .await
        .unwrap();
    let checkout = workspaces
        .present_checkout_id_at(node, "/srv/widgets")
        .await
        .unwrap()
        .expect("the checkout is present");

    let bound = sessions
        .create(NewSession {
            tenant: t,
            workspace_id: Some(ws.id),
            node_id: node,
            name: "bound".into(),
            runtime: "bash".into(),
            created_by: None,
            checkout_id: Some(checkout),
            managed: false,
            managed_purpose: ManagedPurpose::Access,
        })
        .await
        .unwrap();
    // An ad-hoc terminal has no binding and must stay `None`.
    start(&sessions, t, node, None, None, "terminal").await;

    let mut rows = sessions.list(t, SessionFilter::default()).await.unwrap();
    hydrate_checkouts(&workspaces, &mut rows).await.unwrap();

    let hydrated = rows.iter().find(|s| s.id == bound.id).unwrap();
    let summary = hydrated.checkout.as_ref().expect("bound session gets one");
    assert_eq!(summary.path, "/srv/widgets");
    assert_eq!(summary.node_name, "workshop");
    assert_eq!(summary.branch.as_deref(), Some("main"));

    let ad_hoc = rows.iter().find(|s| s.name == "terminal").unwrap();
    assert!(ad_hoc.checkout.is_none(), "no binding, no summary");
}

#[tokio::test]
async fn a_tombstoned_checkout_is_not_offered_to_start_a_session_in() {
    let workspaces = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let ws = workspaces.create(t, "w", "w", None, None).await.unwrap();
    for (path, kind) in [("/srv/w", "clone"), ("/srv/w-feat", "worktree")] {
        workspaces
            .upsert_checkout(CheckoutUpsert {
                tenant: t,
                node_id: node,
                workspace_id: ws.id,
                path: path.into(),
                git_remote_url: None,
                git_remote_normalized: None,
                branch: None,
                git_status: serde_json::json!({}),
                kind: kind.into(),
            })
            .await
            .unwrap();
    }

    // Both present: the named worktree resolves, and the default is the clone.
    assert_eq!(
        workspaces
            .present_checkout_path(t, ws.id, node, "/srv/w-feat")
            .await
            .unwrap()
            .as_deref(),
        Some("/srv/w-feat")
    );
    assert_eq!(
        workspaces.present_clone(ws.id, node).await.unwrap(),
        workspaces
            .present_checkout_id_at(node, "/srv/w")
            .await
            .unwrap()
            .map(|id| (id, "/srv/w".to_string()))
    );

    // The node stops reporting them — the directories are gone.
    workspaces
        .tombstone_checkouts_except(node, &[])
        .await
        .unwrap();

    assert_eq!(
        workspaces
            .present_checkout_path(t, ws.id, node, "/srv/w-feat")
            .await
            .unwrap(),
        None,
        "starting a shell there would land nowhere"
    );
    assert_eq!(workspaces.present_clone(ws.id, node).await.unwrap(), None);
    assert_eq!(
        workspaces
            .present_checkout_id_at(node, "/srv/w")
            .await
            .unwrap(),
        None,
        "and a new session must not be bound to a row that no longer exists"
    );
}

// ── ordinary lifecycle ──────────────────────────────────────────────────────

#[tokio::test]
async fn renaming_and_deleting_are_tenant_scoped() {
    let sessions = FakeSessionRepository::new();
    let t = tenant();
    let s = start(&sessions, t, NodeId::new(), None, None, "before").await;

    let renamed = sessions.rename(s.id, t, "after").await.unwrap().unwrap();
    assert_eq!(renamed.name, "after");
    assert_eq!(sessions.delete(s.id, t).await.unwrap(), 1);
    assert_eq!(
        sessions.delete(s.id, t).await.unwrap(),
        0,
        "deleting twice affects nothing"
    );
}

#[tokio::test]
async fn an_unscoped_lookup_finds_a_session_the_tenant_scoped_one_would_not() {
    let sessions = FakeSessionRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let s = start(&sessions, theirs, NodeId::new(), None, None, "s").await;

    // The content paths need the row before they know whose it is; they then
    // authorize on what comes back. That is why the method says `unscoped`.
    assert!(sessions.by_id_unscoped(s.id).await.unwrap().is_some());
    assert!(sessions.get(mine, s.id).await.unwrap().is_none());
}

// ── the ephemeral-secret wipe's guard, absorbed from services::secrets (MAIN-292)

/// An ending session must not pull an ephemeral secret file out from under a
/// sibling still running in the same workspace. `live_siblings` is that guard,
/// so it counts only LIVE sessions, never itself, and never another workspace's.
#[tokio::test]
async fn live_siblings_counts_only_other_live_sessions_in_the_workspace() {
    let repo = FakeSessionRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let (ws, other_ws) = (WorkspaceId::new(), WorkspaceId::new());

    let ending = start(&repo, t, node, Some(ws), None, "ending").await;
    assert_eq!(
        repo.live_siblings(ws, ending.id).await.unwrap(),
        0,
        "a session is never its own sibling"
    );

    let live = start(&repo, t, node, Some(ws), None, "still working").await;
    repo.force_status(live.id, "detached");
    let dead = start(&repo, t, node, Some(ws), None, "finished").await;
    repo.force_status(dead.id, "exited");
    start(&repo, t, node, Some(other_ws), None, "elsewhere").await;

    assert_eq!(
        repo.live_siblings(ws, ending.id).await.unwrap(),
        1,
        "the detached sibling counts; the exited one and the other workspace's do not"
    );

    // Once the sibling ends too, the last one out may wipe.
    repo.mark_ended(t, live.id).await.unwrap();
    assert_eq!(repo.live_siblings(ws, ending.id).await.unwrap(), 0);
}
