//! A workspace reaches its members' own machines, wherever those are homed.
//!
//! MAIN-353 made a person's nodes reachable from every org they belong to — but
//! only on the SEE and USE paths; its NG-3 deliberately left placement home-
//! tenant. The result in production: join somebody's tenant, create a workspace,
//! and it never clones to your own laptops. "My own nodes" that a second org's
//! work never reaches is not what owning them was for.
//!
//! What this pins is the candidate set the reconciler places on. The scope rule
//! it replaces was "nodes homed in this tenant"; the rule now is "nodes homed in
//! this tenant, OR owned by somebody who belongs to it".

use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A second user row for an existing person — one human, two orgs.
async fn member(bed: &TestBed, tenant: TenantId, person: Uuid) -> UserId {
    let user = UserId::new();
    bed.db()
        .exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, 'member')",
            params![
                user,
                tenant,
                person,
                format!("u-{}@example.test", user.0.simple())
            ],
        )
        .await
        .expect("member");
    user
}

async fn named_node(bed: &TestBed, tenant: TenantId, name: &str, owner: Option<Uuid>) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id)
             VALUES ($1,$2,$3,$4,'online',$5)",
            params![
                id,
                tenant,
                name.to_string(),
                format!("h-{}", id.0.simple()),
                owner
            ],
        )
        .await
        .expect("node");
    id
}

async fn candidates(bed: &TestBed, tenant: TenantId) -> Vec<String> {
    let mut names: Vec<String> = bed
        .app_state()
        .await
        .nodes
        .placement_candidates(tenant)
        .await
        .expect("candidates")
        .into_iter()
        .map(|n| n.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn my_own_machine_is_a_candidate_in_every_tenant_i_belong_to() {
    // THE reported failure: a fresh workspace in a tenant I joined never
    // reached my own nodes, because placement only looked at that tenant's.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (_me_here, person) = bed.user(mine, "owner").await;
    member(&bed, theirs, person).await;

    // My laptop, homed in MY tenant.
    named_node(&bed, mine, "my-laptop", Some(person)).await;
    // Their shared box, homed in theirs.
    named_node(&bed, theirs, "their-box", None).await;

    assert_eq!(
        candidates(&bed, theirs).await,
        vec!["my-laptop", "their-box"],
        "their workspaces may place on my machine, because I am one of them"
    );
    assert_eq!(
        candidates(&bed, mine).await,
        vec!["my-laptop"],
        "and my own tenant is unchanged"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_stranger_s_machine_is_never_a_candidate() {
    // The edge that makes the widening safe to rely on: membership is what
    // brings a machine in, not merely existing somewhere in the deployment.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let theirs = bed.tenant("theirs").await;
    let elsewhere = bed.tenant("elsewhere").await;
    let (_u, stranger) = bed.user(elsewhere, "owner").await;
    named_node(&bed, elsewhere, "stranger-box", Some(stranger)).await;
    named_node(&bed, theirs, "their-box", None).await;

    assert_eq!(
        candidates(&bed, theirs).await,
        vec!["their-box"],
        "a person who is not a member brings nothing"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn leaving_a_tenant_takes_your_machines_out_of_its_reach() {
    // Membership is read live, so the set narrows again by itself. Nothing has
    // to be un-provisioned when somebody leaves.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (_me, person) = bed.user(mine, "owner").await;
    let seat = member(&bed, theirs, person).await;
    named_node(&bed, mine, "my-laptop", Some(person)).await;

    assert_eq!(candidates(&bed, theirs).await, vec!["my-laptop"]);

    bed.db()
        .exec("DELETE FROM users WHERE id = $1", params![seat])
        .await
        .expect("leave");

    assert!(
        candidates(&bed, theirs).await.is_empty(),
        "no membership, no reach"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn an_ownerless_node_in_another_tenant_stays_out() {
    // `owner_person_id IS NULL` must not match the member subquery. A node
    // nobody owns belongs to its tenant and to nowhere else.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (_me, person) = bed.user(mine, "owner").await;
    member(&bed, theirs, person).await;
    named_node(&bed, mine, "unowned-in-mine", None).await;
    named_node(&bed, theirs, "their-box", None).await;

    assert_eq!(
        candidates(&bed, theirs).await,
        vec!["their-box"],
        "an ownerless node in another tenant is not reachable from here"
    );

    bed.teardown().await;
}
