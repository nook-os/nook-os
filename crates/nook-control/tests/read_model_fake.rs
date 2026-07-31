//! MAIN-304 AC-3: the read model, driven against its in-memory fake.
//!
//! No database, no `TestBed`, no `DATABASE_URL` — which is the point of the
//! repository chain and the thing an inline query could never offer. These
//! assert the CONTRACT the trait promises, so a future `DbReadModelRepository`
//! that stopped honouring one has something to disagree with.
//!
//! What they deliberately do not do is re-assert the SQL. The Postgres arm is
//! covered by `activity_visibility` and `overview_visibility`, which drive the
//! real queries against a real database; duplicating that here against a fake
//! would only prove the fake agrees with itself.

use nook_control::repo::read_model::{
    EventScopeIds, EventsQuery, FakeReadModelRepository, NewEvent, OverviewTaskRow,
    ReadModelRepository,
};
use nook_types::*;
use uuid::Uuid;

fn draft(kind: &str) -> NewEvent {
    NewEvent {
        kind: kind.to_string(),
        actor_type: None,
        actor_id: None,
        workspace_id: None,
        node_id: None,
        session_id: None,
        payload: serde_json::json!({}),
    }
}

fn query(tenant: TenantId) -> EventsQuery {
    EventsQuery {
        tenant,
        workspace: None,
        kind_prefix: None,
        before: None,
        limit: 50,
        scope: None,
    }
}

#[tokio::test]
async fn an_event_is_recorded_and_comes_back_on_the_feed() {
    let repo = FakeReadModelRepository::new();
    let tenant = TenantId(Uuid::now_v7());

    let stored = repo
        .record_event(tenant, draft("task.created"))
        .await
        .expect("record");
    assert_eq!(stored.tenant_id, tenant);
    assert_eq!(stored.kind, "task.created");
    assert_eq!(repo.event_count(), 1);

    let page = repo.events_page(query(tenant)).await.expect("page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, stored.id);
}

#[tokio::test]
async fn the_feed_is_tenant_scoped() {
    let repo = FakeReadModelRepository::new();
    let (a, b) = (TenantId(Uuid::now_v7()), TenantId(Uuid::now_v7()));
    repo.record_event(a, draft("a.thing")).await.unwrap();
    repo.record_event(b, draft("b.thing")).await.unwrap();

    let page = repo.events_page(query(a)).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].kind, "a.thing");
}

#[tokio::test]
async fn a_member_scope_sees_only_its_own_actions_nodes_and_sessions() {
    // The rule this pins is MAIN-134's: the page filter and the live bus's
    // `ActivityScope::allows` are the same three-way match, so an event reaches
    // a member if THEY caused it, it is on THEIR node, or on THEIR session.
    let repo = FakeReadModelRepository::new();
    let tenant = TenantId(Uuid::now_v7());
    let (me, node, session) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    let mine = NewEvent {
        actor_id: Some(me),
        ..draft("i.did.this")
    };
    let on_my_node = NewEvent {
        node_id: Some(NodeId(node)),
        ..draft("my.node")
    };
    let on_my_session = NewEvent {
        session_id: Some(SessionId(session)),
        ..draft("my.session")
    };
    let a_strangers = NewEvent {
        actor_id: Some(Uuid::now_v7()),
        ..draft("not.mine")
    };
    for e in [mine, on_my_node, on_my_session, a_strangers] {
        repo.record_event(tenant, e).await.unwrap();
    }

    let scoped = EventsQuery {
        scope: Some(EventScopeIds {
            user_ids: vec![me],
            node_ids: vec![node],
            session_ids: vec![session],
        }),
        ..query(tenant)
    };
    let mut kinds: Vec<String> = repo
        .events_page(scoped)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    kinds.sort();
    assert_eq!(kinds, ["i.did.this", "my.node", "my.session"]);
}

#[tokio::test]
async fn an_empty_member_scope_sees_nothing_rather_than_everything() {
    // Fails CLOSED. A person with no resolvable resources must see an empty
    // feed, never the tenant's — the inverse would be a silent leak.
    let repo = FakeReadModelRepository::new();
    let tenant = TenantId(Uuid::now_v7());
    repo.record_event(
        tenant,
        NewEvent {
            actor_id: Some(Uuid::now_v7()),
            ..draft("somebody.else")
        },
    )
    .await
    .unwrap();

    let scoped = EventsQuery {
        scope: Some(EventScopeIds::default()),
        ..query(tenant)
    };
    assert!(repo.events_page(scoped).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_feed_honours_kind_prefix_and_limit() {
    let repo = FakeReadModelRepository::new();
    let tenant = TenantId(Uuid::now_v7());
    for kind in ["task.created", "task.moved", "node.joined"] {
        repo.record_event(tenant, draft(kind)).await.unwrap();
    }

    let by_prefix = EventsQuery {
        kind_prefix: Some("task.".into()),
        ..query(tenant)
    };
    assert_eq!(repo.events_page(by_prefix).await.unwrap().len(), 2);

    let capped = EventsQuery {
        limit: 1,
        ..query(tenant)
    };
    assert_eq!(repo.events_page(capped).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_scope_lookups_are_tenant_scoped_too() {
    let repo = FakeReadModelRepository::new();
    let (mine, other) = (TenantId(Uuid::now_v7()), TenantId(Uuid::now_v7()));
    let person = Uuid::now_v7();
    let (my_node, their_node) = (Uuid::now_v7(), Uuid::now_v7());
    repo.add_owned_node(mine, person, my_node);
    repo.add_owned_node(other, person, their_node);

    // The same person owning a node in another tenant must not widen this
    // tenant's scope — that is how a scope becomes a cross-tenant leak.
    assert_eq!(
        repo.node_ids_owned_by(mine, person).await.unwrap(),
        vec![my_node]
    );

    let user = Uuid::now_v7();
    let session = Uuid::now_v7();
    repo.add_created_session(mine, user, session);
    assert_eq!(
        repo.session_ids_created_by(mine, &[user]).await.unwrap(),
        vec![session]
    );
    // A user id that created nothing contributes nothing.
    assert!(repo
        .session_ids_created_by(mine, &[Uuid::now_v7()])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn the_overview_reads_round_trip_through_the_fake() {
    let repo = FakeReadModelRepository::new();
    let tenant = TenantId(Uuid::now_v7());
    let checkout = NodeWorkspaceId(Uuid::now_v7());

    repo.add_checkout_task(OverviewTaskRow {
        checkout_id: checkout,
        key: "MAIN-9".into(),
        title: "a ticket".into(),
        column_type: "started".into(),
    });

    let tasks = repo.overview_checkout_tasks(tenant, None).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].key, "MAIN-9");
    assert_eq!(tasks[0].checkout_id, checkout);

    // Nothing pushed in means nothing comes out — the caller's grouping code
    // must cope with an empty read model rather than assume rows exist.
    assert!(repo.overview_workspaces(tenant).await.unwrap().is_empty());
    assert!(repo
        .overview_checkouts(tenant, None)
        .await
        .unwrap()
        .is_empty());
}
