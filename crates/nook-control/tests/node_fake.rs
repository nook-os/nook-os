//! Node, enrolment and CA callers against the in-memory fakes, with **no
//! database at all** (MAIN-252 AC-3).
//!
//! The rules worth protecting here are the ones whose failure is quiet: a
//! re-join silently transferring a machine to a different person, a disconnect
//! clearing a lease another instance had already taken, a runtime-auth merge
//! replacing the whole capabilities blob, a failed CA promotion leaving the
//! tenant with no signer. All of those needed Postgres to say anything before;
//! behind the traits they are ordinary unit tests.
//!
//! `cargo test -p nook-control --test node_fake` passes with the database
//! stopped.

use nook_control::ca;
use nook_control::repo::nodes::{
    FakeJoinTokenRepository, FakeNodeRepository, FakeTenantCaRepository, IssuedLeaf,
    JoinTokenRepository, JoiningNode, NodeRepository, ReportedCapabilities, TenantCaRepository,
};
use nook_types::*;
use uuid::Uuid;

fn tenant() -> TenantId {
    TenantId::new()
}

fn joining(t: TenantId, name: &str, owner: Option<Uuid>) -> JoiningNode {
    JoiningNode {
        tenant: t,
        name: name.to_string(),
        hostname: "box.local".into(),
        platform: "linux".into(),
        token_hash: format!("hash-{name}"),
        owner_person_id: owner,
    }
}

// ── visibility (MAIN-132 / MAIN-135) ────────────────────────────────────────

#[tokio::test]
async fn a_member_sees_their_own_nodes_plus_shared_ones_and_an_admin_sees_all() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    repo.add(t, "alice-box", Some(alice), false);
    repo.add(t, "bob-box", Some(bob), false);
    repo.add(t, "operator", Some(bob), true);

    let names = |v: Vec<Node>| v.into_iter().map(|n| n.name).collect::<Vec<_>>();

    assert_eq!(
        names(repo.list(t, Some(alice), Some(alice)).await.unwrap()),
        vec!["alice-box", "operator"],
        "own nodes plus the shared one — never a teammate's private machine"
    );
    assert_eq!(
        names(repo.list(t, None, None).await.unwrap()),
        vec!["alice-box", "bob-box", "operator"],
        "the unscoped view is the whole fleet"
    );
}

#[tokio::test]
async fn another_tenants_node_is_invisible_and_untouchable() {
    let repo = FakeNodeRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let id = repo.add(theirs, "theirs", None, true);

    assert!(repo.get(mine, id).await.unwrap().is_none());
    assert!(!repo.exists_in_tenant(id, mine).await.unwrap());
    assert_eq!(repo.delete(mine, id).await.unwrap(), 0);
    assert_eq!(repo.revoke(id, mine).await.unwrap(), 0);
    assert_eq!(repo.count(), 1, "a wrong-tenant delete removes nothing");
}

// ── re-joining a machine (MAIN-119) ─────────────────────────────────────────

#[tokio::test]
async fn a_re_join_keeps_the_node_id_and_never_transfers_the_owner() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());

    let first = repo
        .upsert_joining(joining(t, "workshop", Some(alice)))
        .await
        .unwrap();

    // Bob re-enrols the same machine with his own token.
    let second = repo
        .upsert_joining(joining(t, "workshop", Some(bob)))
        .await
        .unwrap();

    assert_eq!(
        first, second,
        "a re-join heals the row — it is the same machine"
    );
    assert_eq!(repo.count(), 1, "and does not duplicate it");
    assert_eq!(
        repo.get(t, first).await.unwrap().unwrap().owner_person_id,
        Some(alice),
        "COALESCE: a re-enrol must not silently hand someone else's machine over"
    );
}

#[tokio::test]
async fn a_node_that_had_no_owner_can_still_acquire_one() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let alice = Uuid::now_v7();
    let id = repo
        .upsert_joining(joining(t, "orphan", None))
        .await
        .unwrap();

    repo.upsert_joining(joining(t, "orphan", Some(alice)))
        .await
        .unwrap();
    assert_eq!(
        repo.get(t, id).await.unwrap().unwrap().owner_person_id,
        Some(alice),
        "COALESCE fills a NULL owner — it only refuses to overwrite a real one"
    );
}

// ── join tokens ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_expired_token_is_indistinguishable_from_an_absent_one() {
    let repo = FakeJoinTokenRepository::new();
    let t = tenant();
    repo.issue(
        t,
        "h1",
        UserId::new(),
        chrono::Utc::now() + chrono::Duration::hours(24),
    )
    .await
    .unwrap();

    assert!(
        repo.consume("h1").await.unwrap().is_some(),
        "a fresh token spends"
    );

    repo.issue(
        t,
        "h2",
        UserId::new(),
        chrono::Utc::now() + chrono::Duration::hours(24),
    )
    .await
    .unwrap();
    repo.expire("h2");
    assert!(repo.consume("h2").await.unwrap().is_none(), "expired");
    assert!(
        repo.consume("never-issued").await.unwrap().is_none(),
        "absent"
    );
}

#[tokio::test]
async fn consuming_a_token_hands_back_what_it_enrols_into() {
    let repo = FakeJoinTokenRepository::new();
    let (t, minter) = (tenant(), UserId::new());
    repo.issue(
        t,
        "h",
        minter,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();

    let spent = repo.consume("h").await.unwrap().expect("spends");
    assert_eq!(spent.tenant, t);
    assert_eq!(
        spent.created_by,
        Some(minter),
        "the minter is who ends up owning the node"
    );
    assert!(repo.is_used("h"), "spending marks it used");
}

/// `join_tokens.created_by` is nullable, and a token that recorded no minter is
/// the case enrolment falls back to the tenant owner for. Typing it non-null is
/// not a compile error — it is a runtime decode failure on exactly the legacy
/// rows, which is how it slipped past a full build during MAIN-252.
#[tokio::test]
async fn a_legacy_token_with_no_minter_still_spends() {
    let repo = FakeJoinTokenRepository::new();
    let t = tenant();
    repo.issue_legacy(t, "legacy");

    let spent = repo.consume("legacy").await.unwrap().expect("spends");
    assert_eq!(spent.tenant, t);
    assert_eq!(
        spent.created_by, None,
        "no minter — the caller falls back to the tenant owner's person"
    );
}

// ── the ownership lease ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_disconnect_does_not_clear_a_lease_another_instance_has_taken() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let id = repo.add(t, "n", None, false);
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
    // Bring it online first, or the offline assertion below passes vacuously —
    // a node seeded `offline` is `offline` whatever the release does.
    repo.record_capabilities(
        id,
        ReportedCapabilities {
            capabilities: serde_json::json!({}),
            hostname: "box".into(),
            platform: "linux".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(repo.status_of(id).as_deref(), Some("online"));

    repo.take_lease(id, first, 30.0).await.unwrap();
    // The node reconnects elsewhere; that instance takes the lease.
    repo.take_lease(id, second, 30.0).await.unwrap();
    assert_eq!(repo.owning_instance(id), Some(second));

    // Only now does the first instance notice the socket dropped.
    repo.release_lease(id, first).await.unwrap();
    assert_eq!(
        repo.owning_instance(id),
        Some(second),
        "the stale release must not steal the live instance's claim"
    );
    assert_ne!(
        repo.status_of(id).as_deref(),
        Some("offline"),
        "nor mark a node offline that is in fact connected elsewhere"
    );
}

#[tokio::test]
async fn the_owning_instance_can_release_its_own_lease() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let id = repo.add(t, "n", None, false);
    let me = Uuid::now_v7();

    repo.take_lease(id, me, 30.0).await.unwrap();
    repo.release_lease(id, me).await.unwrap();
    assert_eq!(repo.owning_instance(id), None);
    assert_eq!(repo.status_of(id).as_deref(), Some("offline"));
}

// ── capabilities merging (MAIN-126 / MAIN-201) ──────────────────────────────

#[tokio::test]
async fn a_runtime_auth_re_probe_merges_rather_than_replacing_capabilities() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let id = repo.add(t, "n", None, false);
    repo.set_capabilities(
        id,
        serde_json::json!({ "hostname": "box", "runtimes": ["bash", "claude"] }),
    );

    repo.merge_runtime_auth(
        id,
        &serde_json::json!([{ "runtime": "claude", "state": "authorized" }]),
    )
    .await
    .unwrap();

    let caps = repo.get(t, id).await.unwrap().unwrap().capabilities;
    assert_eq!(
        caps["runtimes"],
        serde_json::json!(["bash", "claude"]),
        "a whole-blob replace would have erased everything the node reported"
    );
    assert_eq!(caps["runtime_auth"][0]["state"], "authorized");
}

#[tokio::test]
async fn a_register_report_brings_a_node_online() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let id = repo.add(t, "n", None, false);

    repo.record_capabilities(
        id,
        ReportedCapabilities {
            capabilities: serde_json::json!({ "runtimes": ["bash"] }),
            hostname: "workshop.local".into(),
            platform: "linux".into(),
        },
    )
    .await
    .unwrap();

    let node = repo.get(t, id).await.unwrap().unwrap();
    assert_eq!(node.status, "online");
    assert_eq!(node.hostname, "workshop.local");
    assert!(node.last_seen_at.is_some());
}

// ── session reconciliation on reconnect ─────────────────────────────────────

#[tokio::test]
async fn sessions_whose_tmux_is_gone_are_exited_and_the_survivors_left_alone() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let node = repo.add(t, "n", None, false);
    let other_node = repo.add(t, "other", None, false);
    let (alive, dead, no_tmux, elsewhere) = (
        SessionId::new(),
        SessionId::new(),
        SessionId::new(),
        SessionId::new(),
    );

    repo.add_session(alive, t, node, Some("nook-alive"));
    repo.add_session(dead, t, node, Some("nook-dead"));
    repo.add_session(no_tmux, t, node, None);
    repo.add_session(elsewhere, t, other_node, Some("nook-elsewhere"));

    // The node reconnects and reports only one live tmux session.
    let n = repo
        .expire_sessions_missing_from_tmux(node, &["nook-alive".to_string()])
        .await
        .unwrap();

    assert_eq!(n, 2, "the dead one and the one that never got a tmux name");
    assert_eq!(repo.session_status(alive).as_deref(), Some("running"));
    assert_eq!(repo.session_status(dead).as_deref(), Some("exited"));
    assert_eq!(repo.session_status(no_tmux).as_deref(), Some("exited"));
    assert_eq!(
        repo.session_status(elsewhere).as_deref(),
        Some("running"),
        "another machine's sessions are not this node's to declare dead"
    );
}

#[tokio::test]
async fn a_session_failure_records_the_reason() {
    let repo = FakeNodeRepository::new();
    let t = tenant();
    let node = repo.add(t, "n", None, false);
    let s = SessionId::new();
    repo.add_session(s, t, node, None);

    repo.mark_session_failed(s, t, "runtime not installed")
        .await
        .unwrap();
    assert_eq!(repo.session_status(s).as_deref(), Some("error"));
    assert_eq!(
        repo.session_error(s).as_deref(),
        Some("runtime not installed")
    );

    // Wrong tenant matches nothing.
    assert_eq!(repo.mark_session_exited(s, tenant()).await.unwrap(), 0);
    assert_eq!(repo.session_status(s).as_deref(), Some("error"));
}

// ── the CA callers, running with no database ────────────────────────────────

#[tokio::test]
async fn a_failed_promotion_leaves_the_tenant_with_its_signer() {
    let cas = FakeTenantCaRepository::new();
    let t = tenant();
    let active = cas
        .insert(t, "active", "PEM", vec![], "fp-a", chrono::Utc::now())
        .await
        .unwrap();

    // Promoting something that is not staged must roll the whole thing back —
    // demote-then-promote would otherwise leave the tenant unable to sign.
    let err = ca::promote(&cas, t, Uuid::now_v7())
        .await
        .expect_err("nothing staged to promote");
    assert!(err.to_string().contains("no staged CA"), "{err}");
    assert_eq!(
        cas.state_of(active.id).as_deref(),
        Some("active"),
        "the current signer must survive a failed promotion"
    );
}

#[tokio::test]
async fn promoting_a_staged_ca_demotes_the_old_signer_to_retiring() {
    let cas = FakeTenantCaRepository::new();
    let t = tenant();
    let old = cas
        .insert(t, "active", "PEM", vec![], "fp-old", chrono::Utc::now())
        .await
        .unwrap();
    let new = cas
        .insert(t, "staged", "PEM", vec![], "fp-new", chrono::Utc::now())
        .await
        .unwrap();

    ca::promote(&cas, t, new.id).await.unwrap();
    assert_eq!(cas.state_of(new.id).as_deref(), Some("active"));
    assert_eq!(
        cas.state_of(old.id).as_deref(),
        Some("retiring"),
        "still trusted, just no longer issuing"
    );
    assert_eq!(
        ca::trust_bundle(&cas, t).await.unwrap().len(),
        2,
        "a rotation is not an outage — both stay in the bundle"
    );
}

#[tokio::test]
async fn a_ca_with_live_leaves_cannot_be_retired() {
    let cas = FakeTenantCaRepository::new();
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let old = cas
        .insert(t, "retiring", "PEM", vec![], "fp", chrono::Utc::now())
        .await
        .unwrap();
    let node = nodes.add(t, "n", None, false);
    nodes.set_leaf(
        node,
        old.id,
        chrono::Utc::now() + chrono::Duration::days(30),
    );

    assert_eq!(ca::live_leaves(&nodes, t, old.id).await.unwrap(), 1);
    let err = ca::retire(&cas, &nodes, t, old.id)
        .await
        .expect_err("retiring it would lock that machine out mid-rotation");
    assert!(err.to_string().contains("1 node(s) still hold"), "{err}");
    assert!(cas.state_of(old.id).is_some(), "and it stays in the bundle");
}

#[tokio::test]
async fn a_revoked_or_expired_leaf_does_not_hold_a_ca_open() {
    let cas = FakeTenantCaRepository::new();
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let old = cas
        .insert(t, "retiring", "PEM", vec![], "fp", chrono::Utc::now())
        .await
        .unwrap();

    let revoked = nodes.add(t, "revoked", None, false);
    nodes.set_leaf(
        revoked,
        old.id,
        chrono::Utc::now() + chrono::Duration::days(30),
    );
    nodes.revoke(revoked, t).await.unwrap();

    let expired = nodes.add(t, "expired", None, false);
    nodes.set_leaf(
        expired,
        old.id,
        chrono::Utc::now() - chrono::Duration::days(1),
    );

    assert_eq!(
        ca::live_leaves(&nodes, t, old.id).await.unwrap(),
        0,
        "neither a revoked machine nor an expired certificate blocks retirement"
    );
    ca::retire(&cas, &nodes, t, old.id).await.unwrap();
    assert!(cas.state_of(old.id).is_none(), "retired");
}

#[tokio::test]
async fn the_active_signer_cannot_be_retired_even_with_no_leaves() {
    let cas = FakeTenantCaRepository::new();
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let active = cas
        .insert(t, "active", "PEM", vec![], "fp", chrono::Utc::now())
        .await
        .unwrap();

    let err = ca::retire(&cas, &nodes, t, active.id)
        .await
        .expect_err("the guard is in the statement, not a runbook");
    assert!(err.to_string().contains("no retirable CA"), "{err}");
    assert_eq!(cas.state_of(active.id).as_deref(), Some("active"));
}

#[tokio::test]
async fn a_trust_bundle_is_scoped_to_its_tenant() {
    let cas = FakeTenantCaRepository::new();
    let (a, b) = (tenant(), tenant());
    cas.insert(a, "active", "PEM-A", vec![], "fp-a", chrono::Utc::now())
        .await
        .unwrap();
    cas.insert(b, "active", "PEM-B", vec![], "fp-b", chrono::Utc::now())
        .await
        .unwrap();

    let bundle = ca::trust_bundle(&cas, a).await.unwrap();
    assert_eq!(bundle.len(), 1);
    assert_eq!(bundle[0].fingerprint, "fp-a");
}

// ── certificate bookkeeping ─────────────────────────────────────────────────

#[tokio::test]
async fn recording_a_leaf_makes_the_node_count_against_its_ca() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let id = nodes.add(t, "n", None, false);
    let ca_id = Uuid::now_v7();

    nodes
        .record_issued_leaf(
            id,
            IssuedLeaf {
                ca_id,
                not_after: chrono::Utc::now() + chrono::Duration::days(30),
                cert_pem: "CERT".into(),
                public_key_pem: "PUB".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(nodes.live_leaf_count(t, ca_id).await.unwrap(), 1);
    let identity = nodes.cert_identity(id).await.unwrap().unwrap();
    assert_eq!(identity.public_key_pem.as_deref(), Some("PUB"));
    assert!(identity.revoked_at.is_none());
}

#[tokio::test]
async fn revocation_is_visible_to_the_renewal_path() {
    let nodes = FakeNodeRepository::new();
    let t = tenant();
    let id = nodes.add(t, "n", None, false);

    assert_eq!(nodes.revoke(id, t).await.unwrap(), 1);
    let identity = nodes.cert_identity(id).await.unwrap().unwrap();
    assert!(
        identity.revoked_at.is_some(),
        "renewal must outrank 'my certificate expired' — else a compromised \
         machine just waits and re-enrols"
    );
}

// ── your own machine, from any of your orgs (MAIN-353) ──────────────────────
//
// The rule is one line — a node is reachable by the person who owns it, in any
// tenant that person is a member of — and its whole risk is the edge below:
// this must widen for the OWNER and for nobody else, ever.

/// The see path (AC-2): ryan's machine homed in A shows up while he is acting
/// in B, and A's own view is unchanged.
#[tokio::test]
async fn your_own_node_follows_you_into_your_other_org() {
    let repo = FakeNodeRepository::new();
    let (a, b) = (tenant(), tenant());
    let ryan = Uuid::now_v7();
    let n = repo.add(a, "ryan-box", Some(ryan), false);

    let names = |v: Vec<Node>| v.into_iter().map(|x| x.name).collect::<Vec<_>>();

    // Acting in B, where he has no nodes of his own at all.
    assert_eq!(
        names(repo.list(b, Some(ryan), Some(ryan)).await.unwrap()),
        vec!["ryan-box"],
        "his own machine is reachable from his other org"
    );
    // Acting in A: exactly as before.
    assert_eq!(
        names(repo.list(a, Some(ryan), Some(ryan)).await.unwrap()),
        vec!["ryan-box"]
    );
    // And it is still homed in A — the widening moves no rows.
    assert_eq!(
        repo.by_id_any_tenant(n).await.unwrap().unwrap().tenant_id,
        a,
        "reachability is authorization, not a change of home"
    );
}

/// AC-3, the edge that turns a reachability feature into a leak. A tenant-B
/// ADMIN — whose in-tenant view is the whole fleet (`owner = None`) — must not
/// thereby see a foreign machine belonging to someone else.
#[tokio::test]
async fn another_orgs_admin_never_sees_someone_elses_foreign_node() {
    let repo = FakeNodeRepository::new();
    let (a, b) = (tenant(), tenant());
    let (ryan, admin_of_b, member_of_b) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    repo.add(a, "ryan-box", Some(ryan), false);
    repo.add(b, "b-box", Some(admin_of_b), false);

    let names = |v: Vec<Node>| v.into_iter().map(|x| x.name).collect::<Vec<_>>();

    // The admin's fleet view of B: `owner = None` is "the whole fleet OF B".
    assert_eq!(
        names(repo.list(b, None, Some(admin_of_b)).await.unwrap()),
        vec!["b-box"],
        "the fleet view is the fleet of THIS tenant — ryan's machine is not in it"
    );
    // A plain member of B, likewise.
    assert_eq!(
        names(
            repo.list(b, Some(member_of_b), Some(member_of_b))
                .await
                .unwrap()
        ),
        Vec::<String>::new()
    );
    // And a shared machine in A does not travel either — sharing is a grant to
    // A's team, not a passport (AC-5's sibling on the see path).
    repo.add(a, "a-shared", Some(ryan), true);
    assert_eq!(
        names(repo.list(b, None, Some(admin_of_b)).await.unwrap()),
        vec!["b-box"],
        "a node shared with org A is not shared with org B"
    );
}

/// The owner leg must not be widened by the fleet view. An admin acting with
/// `owner = None` and no person still gets only their tenant.
#[tokio::test]
async fn the_owner_leg_is_never_widened_by_a_fleet_view() {
    let repo = FakeNodeRepository::new();
    let (a, b) = (tenant(), tenant());
    let ryan = Uuid::now_v7();
    repo.add(a, "ryan-box", Some(ryan), false);

    // No person at all (a node token): nothing foreign, whatever the scope.
    assert!(repo.list(b, None, None).await.unwrap().is_empty());
    assert!(repo.list(b, Some(ryan), None).await.unwrap().is_empty());
}
