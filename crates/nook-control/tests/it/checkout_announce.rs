//! MAIN-317 AC-2: a checkout the RECONCILER created announces itself, exactly
//! as a discovered one does.
//!
//! Workspace env is `workspace_secrets`, sealed with a password the control
//! plane never sees, so the server cannot deliver it to a new checkout. What it
//! can do is ANNOUNCE — record `workspace.checkout_added` — and an unlocked
//! browser replays the unlock, which is what actually writes the file. That was
//! the owner's 2026-08-04 re-scoping of this AC.
//!
//! Discovery had always announced. The reconciler's clone-on-demand had not:
//! it pins the checkout row itself with `associate_clone` rather than waiting
//! for the node's next scan, and in taking that shortcut it stepped around the
//! announce entirely — so a checkout the reconciler created never announced,
//! and no browser ever learned to push env to it.
//!
//! These pin the OUTCOME rather than the call: the two paths must leave the
//! same event behind, and the guard that makes announcing unconditionally safe
//! must actually hold.

use nook_control::services::discovery;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

struct Fixture {
    tenant: TenantId,
    node: NodeId,
    workspace: WorkspaceId,
    remote: String,
}

async fn seed(bed: &TestBed, with_secret: bool) -> Fixture {
    let tenant = bed.tenant("m317").await;
    let (_user, person) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person).await;
    let workspace = bed.workspace(tenant).await;
    let remote = format!("git@github.com:acme/m317-{}.git", Uuid::now_v7().simple());
    bed.db()
        .exec(
            "UPDATE workspaces SET git_remote_normalized = $2 WHERE id = $1",
            params![workspace, discovery::normalize_remote(&remote)],
        )
        .await
        .expect("remote");

    if with_secret {
        // Sealed, exactly as the real thing is — the control plane holds
        // ciphertext it cannot read, which is the whole reason it announces
        // instead of delivering.
        bed.db()
            .exec(
                "INSERT INTO workspace_secrets
                   (id, tenant_id, workspace_id, name, content_enc, kdf_salt, verifier)
                 VALUES ($1, $2, $3, '.env', 'ciphertext', 'salt', 'verifier')",
                params![Uuid::now_v7(), tenant, workspace],
            )
            .await
            .expect("secret");
    }

    Fixture {
        tenant,
        node,
        workspace,
        remote,
    }
}

/// The `workspace.checkout_added` events for a workspace, as `(path,)`.
async fn announced(bed: &TestBed, f: &Fixture) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT payload->>'path' FROM events
             WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'workspace.checkout_added'
             ORDER BY occurred_at, id",
            params![f.tenant, f.workspace],
        )
        .await
        .expect("events")
}

fn discovered(path: &str, remote: &str) -> nook_proto::DiscoveredWorkspace {
    nook_proto::DiscoveredWorkspace {
        path: path.into(),
        name: "acme/m317".into(),
        git_remote_url: Some(remote.into()),
        branch: Some("main".into()),
        dirty: false,
        worktree: false,
        root_segment: None,
    }
}

#[tokio::test]
async fn a_reconciler_created_checkout_announces_like_a_discovered_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed, true).await;
    let state = bed.app_state().await;

    // The path discovery takes: the node reports a checkout it found.
    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![discovered("/w/discovered", &f.remote)],
    )
    .await
    .expect("discovery");
    assert_eq!(
        announced(&bed, &f).await,
        vec!["/w/discovered".to_string()],
        "discovery has always announced"
    );

    // The path clone-on-demand takes, with the arguments `start_clone` passes
    // from its success arm.
    nook_control::services::secrets::announce_new_checkout(
        &state,
        f.tenant,
        f.workspace,
        f.node,
        "/w/cloned",
    )
    .await;

    assert_eq!(
        announced(&bed, &f).await,
        vec!["/w/discovered".to_string(), "/w/cloned".to_string()],
        "and a reconciler-created checkout announces the same way"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_workspace_with_no_secrets_announces_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed, false).await;
    let state = bed.app_state().await;

    // This guard is what makes the reconciler's call safe to make
    // unconditionally: `associate_clone` is idempotent and cannot report
    // whether the row was new, so the announce is not gated on newness the way
    // discovery's is. A workspace with nothing to deliver announces nothing
    // either way.
    discovery::reconcile(
        &state,
        f.tenant,
        f.node,
        vec![discovered("/w/discovered", &f.remote)],
    )
    .await
    .expect("discovery");
    nook_control::services::secrets::announce_new_checkout(
        &state,
        f.tenant,
        f.workspace,
        f.node,
        "/w/cloned",
    )
    .await;

    assert!(
        announced(&bed, &f).await.is_empty(),
        "no secrets, nothing to announce, from either path"
    );

    bed.teardown().await;
}
