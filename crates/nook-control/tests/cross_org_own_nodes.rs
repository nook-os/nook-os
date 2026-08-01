//! Your own machine, reachable from every org you belong to (MAIN-353).
//!
//! The rule is one line — a node is reachable by the PERSON who owns it, in any
//! tenant that person is a member of — and the whole risk is one edge: it must
//! widen for the owner and for nobody else. `another_orgs_admin_gets_a_404` and
//! `a_shared_node_does_not_travel` are that edge; the rest would be worth far
//! less without them.
//!
//! Needs Postgres: `DATABASE_URL` (`NOOK_REQUIRE_DB=1` in the suite).

use axum::extract::{Path, State};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::error::ApiError;
use nook_control::routes::nodes::{get_one, list};
use nook_control::ws::registry::NodeHandle;
use nook_db::{params, Db};
use nook_proto::ControlToNode;
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

/// A user in `tenant` for an EXISTING person — how one human holds membership
/// of two orgs.
async fn member(bed: &TestBed, tenant: TenantId, person: Uuid, role: &str) -> UserId {
    let user = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, $5)",
            params![
                user,
                tenant,
                person,
                format!("u-{}@example.test", user.0.simple()),
                role.to_string()
            ],
        )
        .await
        .expect("member");
    user
}

async fn node(bed: &TestBed, tenant: TenantId, name: &str, owner: Uuid, shared: bool) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status,
                                owner_person_id, shared)
             VALUES ($1,$2,$3,$4,'online',$5,$6)",
            params![
                id,
                tenant,
                name.to_string(),
                format!("h-{}", id.0.simple()),
                owner,
                shared
            ],
        )
        .await
        .expect("node");
    id
}

fn names(v: &[Node]) -> Vec<String> {
    v.iter().map(|n| n.name.clone()).collect()
}

/// AC-2: the machine follows its owner, and says where it is homed.
#[tokio::test]
async fn your_node_is_listed_from_your_other_org_and_labelled_with_its_home() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (ryan_in_a, person) = bed.user(a, "member").await;
    let ryan_in_b = member(&bed, b, person, "member").await;
    node(&bed, a, "ryan-box", person, false).await;
    let state = bed.app_state().await;

    // Acting in B: his own machine is there, tagged with its home tenant.
    let from_b = list(State(state.clone()), ctx(ryan_in_b, b))
        .await
        .expect("list")
        .0;
    assert_eq!(names(&from_b), vec!["ryan-box"]);
    assert_eq!(from_b[0].tenant_id, a, "still homed in A");
    assert!(
        from_b[0].home_tenant.is_some(),
        "and labelled as foreign, so a UI can say so: {:?}",
        from_b[0].home_tenant
    );

    // Acting in A: unchanged, and NOT labelled — it is not foreign from here.
    let from_a = list(State(state), ctx(ryan_in_a, a)).await.expect("list").0;
    assert_eq!(names(&from_a), vec!["ryan-box"]);
    assert_eq!(from_a[0].home_tenant, None);

    bed.teardown().await;
}

/// AC-3, the leak edge. An admin of B — whose in-tenant view is the whole fleet
/// — must not see ryan's machine, and must not be able to fetch it by id.
#[tokio::test]
async fn another_orgs_admin_gets_a_404() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (_ryan_a, ryan) = bed.user(a, "member").await;
    let (admin_of_b, _p1) = bed.user(b, "owner").await;
    let (member_of_b, _p2) = bed.user(b, "member").await;
    let n = node(&bed, a, "ryan-box", ryan, false).await;
    let state = bed.app_state().await;

    for who in [admin_of_b, member_of_b] {
        let listed = list(State(state.clone()), ctx(who, b))
            .await
            .expect("list")
            .0;
        assert!(
            names(&listed).is_empty(),
            "a foreign machine is nobody's but its owner's: {:?}",
            names(&listed)
        );
        // By id it is a 404, not a 403 — reachability must not become an
        // existence oracle (AC-8).
        let err = get_one(State(state.clone()), ctx(who, b), Path(n))
            .await
            .expect_err("not visible");
        assert!(
            matches!(err, ApiError::NotFound),
            "expected 404, got {err:?}"
        );
    }

    bed.teardown().await;
}

/// AC-5's sibling on the see path: `shared` is a grant to ONE team. It does not
/// travel with a person into another org.
#[tokio::test]
async fn a_shared_node_does_not_travel() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (_owner_a, owner_person) = bed.user(a, "member").await;
    let (_other_a, other_person) = bed.user(a, "member").await;
    // A member of A who is ALSO a member of B — the person who could carry it.
    let other_in_b = member(&bed, b, other_person, "member").await;
    node(&bed, a, "a-shared", owner_person, true).await;
    let state = bed.app_state().await;

    let from_b = list(State(state), ctx(other_in_b, b))
        .await
        .expect("list")
        .0;
    assert!(
        names(&from_b).is_empty(),
        "a node shared with org A is not shared with org B: {:?}",
        names(&from_b)
    );

    bed.teardown().await;
}

/// A node in A that ryan does NOT own is invisible from B, by list and by id.
#[tokio::test]
async fn a_node_you_do_not_own_is_invisible_from_your_other_org() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (_ryan_a, ryan) = bed.user(a, "member").await;
    let (_someone, someone_else) = bed.user(a, "member").await;
    let ryan_in_b = member(&bed, b, ryan, "member").await;
    let theirs = node(&bed, a, "not-mine", someone_else, false).await;
    let state = bed.app_state().await;

    let from_b = list(State(state.clone()), ctx(ryan_in_b, b))
        .await
        .expect("list")
        .0;
    assert!(names(&from_b).is_empty());
    let err = get_one(State(state), ctx(ryan_in_b, b), Path(theirs))
        .await
        .expect_err("not yours");
    assert!(matches!(err, ApiError::NotFound));

    bed.teardown().await;
}

/// AC-4 + AC-6: the owner may act on their machine from the other org, and the
/// session that results belongs to the ACTING tenant — so its content is walled
/// to B by the existing membership rule, with no guard change (NG-4).
#[tokio::test]
async fn the_owner_may_use_it_from_the_other_org_and_the_session_is_the_acting_tenants() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (_ryan_a, ryan) = bed.user(a, "member").await;
    let ryan_in_b = member(&bed, b, ryan, "member").await;
    let n = node(&bed, a, "ryan-box", ryan, false).await;
    let state = bed.app_state().await;

    // The use-path chokepoint allows it from B…
    nook_control::auth::require_person_owns_node(&state, b, Some(ryan_in_b), n)
        .await
        .expect("the owner may use their own machine from another org");

    // The node has to be reachable for a terminal to start — the `_rx` is held
    // so the channel stays open; nothing reads the StartSession frame.
    let (tx, _rx) = tokio::sync::mpsc::channel::<ControlToNode>(4);
    state
        .registry
        .register_node(n, NodeHandle { tenant_id: a, tx });

    // …and a session recorded from B carries tenant B, not the node's home.
    let session = nook_control::services::session_queries::create_ad_hoc_session(
        &state,
        b,
        Some(ryan_in_b),
        n,
        "bash",
        None,
    )
    .await
    .expect("terminal");
    assert_eq!(
        session.tenant_id, b,
        "the session belongs to the org the work was done from"
    );

    // AC-7: the activity event lands in B's feed, not A's.
    let in_b: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM events WHERE tenant_id = $1 AND session_id = $2",
            params![b, session.id],
        )
        .await
        .expect("events in b");
    let in_a: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM events WHERE tenant_id = $1 AND session_id = $2",
            params![a, session.id],
        )
        .await
        .expect("events in a");
    assert!(in_b > 0, "org B's feed records the work done in org B");
    assert_eq!(in_a, 0, "org A's feed never shows org B's work");

    bed.teardown().await;
}

/// AC-5: the SHARED leg stays tenant-local. A member of A who is not the owner
/// may use a shared node while acting in A, and may not from B.
#[tokio::test]
async fn shared_is_usable_at_home_and_not_from_another_org() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (_owner_a, owner_person) = bed.user(a, "member").await;
    let (other_in_a, other_person) = bed.user(a, "member").await;
    let other_in_b = member(&bed, b, other_person, "member").await;
    let n = node(&bed, a, "a-shared", owner_person, true).await;
    let state = bed.app_state().await;

    nook_control::auth::require_person_may_use_node(&state, a, Some(other_in_a), n)
        .await
        .expect("shared is usable inside the tenant it was shared with");

    let err = nook_control::auth::require_person_may_use_node(&state, b, Some(other_in_b), n)
        .await
        .expect_err("shared does not travel");
    // NotFound specifically, not "some refusal" — see the AC-8 test below.
    assert!(matches!(err, ApiError::NotFound), "got {err:?}");

    bed.teardown().await;
}

/// AC-8: reachability must not become an existence oracle.
///
/// The owner leg is unscoped now, so both chokepoints see nodes they could not
/// see before. If a foreign node they do not own came back `403` while a
/// nonexistent id came back `404`, the status alone would confirm that a given
/// id names a real machine in an org the caller cannot see — the exact probe
/// the tenant-scoped lookup used to make impossible for free.
///
/// Both statuses matter, so both are asserted: `404` for anything outside the
/// caller's tenant, and the unchanged `403` inside it, where the caller can
/// already list the node and a not-yours refusal leaks nothing.
#[tokio::test]
async fn a_foreign_node_you_do_not_own_is_a_404_not_a_403() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (a, b) = (bed.tenant("orga").await, bed.tenant("orgb").await);
    let (_stranger_in_a, stranger_person) = bed.user(a, "member").await;
    let (_probe_in_a, probe_person) = bed.user(a, "member").await;
    let probe_in_b = member(&bed, b, probe_person, "member").await;
    // Two nodes in A owned by someone else: one shared, one not. Shared is the
    // sharper case — the shared leg is tenant-local, so from B it must read as
    // absent rather than as "exists, but not shared with you".
    let theirs = node(&bed, a, "a-theirs", stranger_person, false).await;
    let theirs_shared = node(&bed, a, "a-theirs-shared", stranger_person, true).await;
    let nowhere = NodeId::new();
    let state = bed.app_state().await;

    for (label, target) in [
        ("private", theirs),
        ("shared", theirs_shared),
        ("nonexistent", nowhere),
    ] {
        for chokepoint in ["owns", "may_use"] {
            let err = match chokepoint {
                "owns" => {
                    nook_control::auth::require_person_owns_node(
                        &state,
                        b,
                        Some(probe_in_b),
                        target,
                    )
                    .await
                }
                _ => {
                    nook_control::auth::require_person_may_use_node(
                        &state,
                        b,
                        Some(probe_in_b),
                        target,
                    )
                    .await
                }
            }
            .expect_err("not the caller's machine");
            assert!(
                matches!(err, ApiError::NotFound),
                "{chokepoint} leaked the existence of a {label} node from another org: {err:?}"
            );
        }
    }

    // Inside the caller's OWN tenant the 403 is unchanged: they can list the
    // node already, so naming the rule costs nothing and is what makes a
    // mis-dispatch debuggable.
    let probe_at_home = member(&bed, a, probe_person, "member").await;
    let err = nook_control::auth::require_person_owns_node(&state, a, Some(probe_at_home), theirs)
        .await
        .expect_err("not their machine");
    assert!(matches!(err, ApiError::ForbiddenMsg(_)), "got {err:?}");

    bed.teardown().await;
}
