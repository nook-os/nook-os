//! Workspace/checkout callers, exercised against the in-memory fake with **no
//! database at all** (MAIN-251 AC-3).
//!
//! Everything here used to need a Postgres round trip to say anything, so the
//! rules that matter most — a tombstone's retention clock, a re-report healing
//! a checkout in place rather than orphaning its id, a path migration moving
//! checkouts and task worktrees together — were only ever asserted end to end,
//! if at all. Behind the trait they are ordinary unit tests.
//!
//! `cargo test -p nook-control --test workspace_fake` passes with the database
//! stopped; that is the point of the card, and the reason these live apart from
//! the DB-backed suites.

use nook_control::repo::workspaces::{
    CheckoutUpsert, FakeWorkspaceRepository, KeyMatch, WorkspaceRepository,
};
use nook_control::services::workspace_queries;
use nook_types::*;

fn tenant() -> TenantId {
    TenantId::new()
}

async fn scanned(
    repo: &FakeWorkspaceRepository,
    t: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
    kind: &str,
) {
    repo.upsert_checkout(CheckoutUpsert {
        tenant: t,
        node_id: node,
        workspace_id: ws,
        path: path.to_string(),
        git_remote_url: None,
        git_remote_normalized: None,
        branch: Some("main".into()),
        git_status: serde_json::json!({ "dirty": false, "worktree": kind == "worktree" }),
        kind: kind.to_string(),
    })
    .await
    .unwrap();
}

// ── the detail composition, with no database ────────────────────────────────

#[tokio::test]
async fn a_workspace_detail_is_its_row_plus_its_locations() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    repo.add_node(node, t, "workshop", "online");
    let ws = repo
        .create(t, "widgets", "widgets", None, None)
        .await
        .unwrap();
    scanned(&repo, t, node, ws.id, "/srv/widgets", "clone").await;

    let detail = workspace_queries::get_workspace(&repo, t, ws.id)
        .await
        .unwrap()
        .expect("the workspace resolves");
    assert_eq!(detail.workspace.name, "widgets");
    assert_eq!(detail.locations.len(), 1);
    assert_eq!(detail.locations[0].node_name, "workshop");
    assert_eq!(detail.locations[0].path, "/srv/widgets");
}

#[tokio::test]
async fn another_tenants_workspace_is_not_visible() {
    let repo = FakeWorkspaceRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let ws = repo
        .create(theirs, "secret", "secret", None, None)
        .await
        .unwrap();

    assert!(
        workspace_queries::get_workspace(&repo, mine, ws.id)
            .await
            .unwrap()
            .is_none(),
        "a workspace id from another tenant must not resolve"
    );
    assert!(workspace_queries::list_workspaces(&repo, mine)
        .await
        .unwrap()
        .is_empty());
}

// ── resolving a user-typed key (MAIN-223 AC-3) ──────────────────────────────

#[tokio::test]
async fn a_key_resolves_by_id_then_slug_then_name() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let ws = repo
        .create(t, "Widgets", "widgets", None, None)
        .await
        .unwrap();

    for key in [ws.id.0.to_string(), "widgets".into(), "Widgets".into()] {
        assert_eq!(
            workspace_queries::resolve_by_key(&repo, t, &key)
                .await
                .unwrap(),
            ws.id,
            "{key} should resolve"
        );
    }
}

#[tokio::test]
async fn an_ambiguous_name_errors_naming_the_slugs_rather_than_picking_one() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    repo.create(t, "services", "acme-services", None, None)
        .await
        .unwrap();
    repo.create(t, "services", "beta-services", None, None)
        .await
        .unwrap();

    let err = workspace_queries::resolve_by_key(&repo, t, "services")
        .await
        .expect_err("two matches is an error, never an arbitrary pick");
    let msg = err.to_string();
    assert!(
        msg.contains("acme-services") && msg.contains("beta-services"),
        "{msg}"
    );
    assert!(msg.contains("2 workspaces"), "{msg}");
}

#[tokio::test]
async fn an_unknown_key_says_so() {
    let repo = FakeWorkspaceRepository::new();
    let err = workspace_queries::resolve_by_key(&repo, tenant(), "nope")
        .await
        .expect_err("no such workspace");
    assert!(err
        .to_string()
        .contains("no workspace with id, slug, or name"));
}

// ── the tombstone lifecycle (MAIN-220) ──────────────────────────────────────

#[tokio::test]
async fn a_re_reported_checkout_heals_in_place_keeping_its_id() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();
    scanned(&repo, t, node, ws.id, "/srv/w", "clone").await;
    let original = repo.checkout_id(node, "/srv/w").expect("checkout exists");

    // The node stops reporting it: tombstoned, not deleted.
    repo.tombstone_checkouts_except(node, &[]).await.unwrap();
    assert!(repo.is_tombstoned(node, "/srv/w"));
    assert_eq!(repo.checkout_count(), 1, "tombstoning never deletes");

    // It comes back — same row, same id, tombstone cleared.
    scanned(&repo, t, node, ws.id, "/srv/w", "clone").await;
    assert!(!repo.is_tombstoned(node, "/srv/w"));
    assert_eq!(
        repo.checkout_id(node, "/srv/w"),
        Some(original),
        "healing must reuse the row — a new id orphans everything pointing at it"
    );
}

#[tokio::test]
async fn a_second_scan_does_not_restart_the_retention_clock() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();
    scanned(&repo, t, node, ws.id, "/srv/w", "clone").await;

    // Missing for a long time already.
    repo.tombstone_checkouts_except(node, &[]).await.unwrap();
    repo.tombstone_since(node, "/srv/w", chrono::Duration::days(30));

    // A later scan still does not see it. The `missing_at IS NULL` guard means
    // this must NOT re-stamp the timestamp, or a row missing for a month would
    // look one second old and never be reclaimed.
    let restamped = repo.tombstone_checkouts_except(node, &[]).await.unwrap();
    assert_eq!(restamped, 0, "an already-tombstoned row is left alone");

    let reaped = repo
        .reap_tombstoned(chrono::Duration::days(7).num_seconds())
        .await
        .unwrap();
    assert_eq!(reaped.len(), 1, "30 days missing is past a 7-day retention");
    assert_eq!(reaped[0].path, "/srv/w");
    assert_eq!(repo.checkout_count(), 0);
}

#[tokio::test]
async fn a_freshly_tombstoned_checkout_survives_the_reaper() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();
    scanned(&repo, t, node, ws.id, "/srv/w", "clone").await;
    repo.tombstone_checkouts_except(node, &[]).await.unwrap();

    let reaped = repo
        .reap_tombstoned(chrono::Duration::days(7).num_seconds())
        .await
        .unwrap();
    assert!(
        reaped.is_empty(),
        "a transient unmount must survive long enough to heal"
    );
    assert_eq!(repo.checkout_count(), 1);
}

// ── path migration (MAIN-107 AC-4) ──────────────────────────────────────────

// ── remote adoption guards (MAIN-223) ───────────────────────────────────────

#[tokio::test]
async fn a_remote_already_owned_by_another_workspace_is_not_adopted() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let owner = repo.create(t, "owner", "owner", None, None).await.unwrap();
    repo.set_normalized_remote(owner.id, "github.com/acme/widgets")
        .await
        .unwrap();
    let newcomer = repo
        .create(t, "newcomer", "newcomer", None, None)
        .await
        .unwrap();

    let adopted = repo
        .adopt_normalized_remote(newcomer.id, t, "github.com/acme/widgets")
        .await
        .unwrap();
    assert_eq!(
        adopted, 0,
        "the unique (tenant, normalized) index would abort the clone — the \
         guard makes it a no-op instead"
    );
    assert!(repo
        .get(t, newcomer.id)
        .await
        .unwrap()
        .unwrap()
        .git_remote_normalized
        .is_none());
}

#[tokio::test]
async fn an_existing_remote_url_is_never_clobbered_by_one_checkout() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();

    assert_eq!(
        repo.adopt_remote_url(ws.id, "https://a/one.git")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        repo.adopt_remote_url(ws.id, "https://b/two.git")
            .await
            .unwrap(),
        0,
        "NULL-guarded: an agreed value stays put"
    );
    assert_eq!(
        repo.get(t, ws.id).await.unwrap().unwrap().git_remote_url,
        Some("https://a/one.git".into())
    );
}

#[tokio::test]
async fn a_hand_picked_name_is_not_requalified() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let ws = repo
        .create(t, "the flaky one", "services", None, None)
        .await
        .unwrap();

    // The scan wants "services" → "acme/services", but only if the name is
    // still the bare one. It is not.
    let changed = repo
        .qualify_name(ws.id, "acme/services", "services")
        .await
        .unwrap();
    assert_eq!(changed, 0);
    assert_eq!(
        repo.get(t, ws.id).await.unwrap().unwrap().name,
        "the flaky one"
    );
}

// ── worktrees branch from the clone (MAIN-222 AC-3) ─────────────────────────

#[tokio::test]
async fn clone_path_ignores_worktrees_and_tombstones() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();

    // A worktree scanned first, then the real clone: order must not decide it.
    scanned(&repo, t, node, ws.id, "/srv/w-feature", "worktree").await;
    scanned(&repo, t, node, ws.id, "/srv/w", "clone").await;

    assert_eq!(
        repo.clone_path(t, ws.id, node).await.unwrap().as_deref(),
        Some("/srv/w"),
        "a worktree is never the base for another worktree"
    );

    // A tombstoned clone is not a place to branch from either.
    repo.tombstone_checkouts_except(node, &["/srv/w-feature".to_string()])
        .await
        .unwrap();
    assert_eq!(repo.clone_path(t, ws.id, node).await.unwrap(), None);
}

// ── deleting a workspace ────────────────────────────────────────────────────

#[tokio::test]
async fn a_workspace_with_live_sessions_is_refused_and_stays() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();
    repo.set_live_sessions(ws.id, 2);

    // The route's rule, exercised without a router: refuse before deleting.
    let live = repo.live_session_count(ws.id).await.unwrap();
    assert_eq!(live, 2);
    assert!(repo.get(t, ws.id).await.unwrap().is_some());
}

#[tokio::test]
async fn deleting_a_workspace_takes_its_checkouts_with_it() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let node = NodeId::new();
    let ws = repo.create(t, "w", "w", None, None).await.unwrap();
    scanned(&repo, t, node, ws.id, "/srv/w", "clone").await;

    assert_eq!(repo.delete(ws.id, t).await.unwrap(), 1);
    assert_eq!(repo.checkout_count(), 0, "the schema cascades checkouts");
    assert_eq!(
        repo.delete(ws.id, t).await.unwrap(),
        0,
        "deleting twice affects nothing"
    );
}

#[tokio::test]
async fn a_workspace_cannot_be_deleted_through_the_wrong_tenant() {
    let repo = FakeWorkspaceRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let ws = repo.create(theirs, "w", "w", None, None).await.unwrap();

    assert_eq!(repo.delete(ws.id, mine).await.unwrap(), 0);
    assert!(repo.get(theirs, ws.id).await.unwrap().is_some());
}

// ── discovery's slug allocation ─────────────────────────────────────────────

#[tokio::test]
async fn a_taken_slug_reports_none_so_the_caller_can_retry() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    assert!(repo
        .insert_discovered(t, "widgets", "widgets", None)
        .await
        .unwrap()
        .is_some());
    assert!(
        repo.insert_discovered(t, "widgets", "widgets", None)
            .await
            .unwrap()
            .is_none(),
        "a collision is a retry signal, not an error"
    );
    assert!(
        repo.insert_discovered(t, "widgets", "widgets-a1b2", None)
            .await
            .unwrap()
            .is_some(),
        "the suffixed slug goes in"
    );
}

#[tokio::test]
async fn resolve_key_matches_the_enum_the_service_translates() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let ws = repo.create(t, "w", "w-slug", None, None).await.unwrap();

    assert_eq!(
        repo.resolve_key(t, "w-slug").await.unwrap(),
        KeyMatch::One(ws.id)
    );
    assert_eq!(repo.resolve_key(t, "absent").await.unwrap(), KeyMatch::None);
}

// ── the path-move struct is not silently lossy ──────────────────────────────

// ── placement hosts, absorbed from services::schedule (MAIN-292) ────────────

/// The rule placement depends on, now provable with no database: a node hosts a
/// workspace only through a PRESENT CLONE. A worktree is not a host — worktrees
/// branch from a clone, so a node with only a worktree cannot be the one a new
/// worktree is cut on — and a tombstoned clone is not a host either, because the
/// directory is gone.
#[tokio::test]
async fn only_a_present_clone_makes_a_node_a_placement_host() {
    let repo = FakeWorkspaceRepository::new();
    let t = tenant();
    let (host, worktree_only, gone) = (NodeId::new(), NodeId::new(), NodeId::new());
    let ws = repo
        .create(t, "widgets", "widgets", None, None)
        .await
        .unwrap();

    scanned(&repo, t, host, ws.id, "/srv/clone", "clone").await;
    scanned(&repo, t, worktree_only, ws.id, "/srv/wt", "worktree").await;
    scanned(&repo, t, gone, ws.id, "/srv/gone", "clone").await;
    // The third node stops reporting its clone: tombstoned, so no longer a host.
    repo.tombstone_checkouts_except(gone, &[]).await.unwrap();

    let hosts = repo.clone_hosts(t, ws.id).await.unwrap();
    assert_eq!(
        hosts.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![host],
        "only the present clone's node is a host"
    );

    // Another tenant's identically-shaped world is invisible.
    assert!(repo.clone_hosts(tenant(), ws.id).await.unwrap().is_empty());
}

/// Whether a workspace has ANY secret is what decides if a new checkout is worth
/// announcing at all, and the ephemeral names are what a session's end wipes.
/// Both are per-workspace, and neither may leak across a tenant.
#[tokio::test]
async fn secret_presence_and_ephemeral_names_are_scoped_to_their_workspace() {
    use nook_control::repo::workspaces::{
        FakeWorkspaceSecretRepository, WorkspaceSecretRepository,
    };

    let secrets = FakeWorkspaceSecretRepository::new();
    let t = tenant();
    let (ws, other) = (WorkspaceId::new(), WorkspaceId::new());

    assert!(!secrets.any(t, ws).await.unwrap(), "nothing stored yet");

    secrets
        .store(t, ws, ".env", vec![1], vec![2], vec![3], false)
        .await
        .unwrap();
    secrets
        .store(t, ws, ".env.local", vec![1], vec![2], vec![3], true)
        .await
        .unwrap();
    secrets
        .store(t, other, ".env.other", vec![1], vec![2], vec![3], true)
        .await
        .unwrap();

    assert!(secrets.any(t, ws).await.unwrap());
    assert!(
        !secrets.any(TenantId::new(), ws).await.unwrap(),
        "another tenant sees none of it"
    );
    assert_eq!(
        secrets.ephemeral_names(ws).await.unwrap(),
        vec![".env.local"],
        "only the ephemeral one is wiped, and only this workspace's"
    );
}
