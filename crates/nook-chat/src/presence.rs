//! Presence and typing — ephemeral, connection-derived, zero schema (MAIN-115).
//!
//! A person is online while they hold at least one authorized chat socket;
//! the registry counts sockets per person and only the EDGES (first open, last
//! close) are announced. Typing is a client-throttled `POST` fanned out to the
//! channel's subscribers. Neither writes a row anywhere: presence dies with
//! the connections it counts, and a typing frame's whole life is the ~4s the
//! client shows it.
//!
//! Authorization is split by scope (owner ruling on MAIN-115, option (a)):
//! `Typing` carries a channel and rides the existing per-event channel gate;
//! `Presence` is a person↔person relation the channel gate cannot answer, so
//! the socket routes it through [`visible`] — shared tenant, shared org
//! channel, or shared DM, never a global roster (AC-4).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use nook_types::ChatServerMessage;
use uuid::Uuid;

use crate::{channels, AppState, Caller};
use nook_errors::ApiError;

/// Announce a presence edge to local subscribers and, best effort, to peer
/// instances over the bus. Ephemeral: there is no row to refetch, so the whole
/// frame travels in the NOTIFY payload (tiny, far under the ~8 KB cap).
pub(crate) async fn announce(state: &AppState, person: Uuid, online: bool) {
    let event = ChatServerMessage::Presence { person, online };
    state.registry.publish_local(event.clone());
    crate::bus::publish_ephemeral(&state.db, state.registry.instance(), &event).await;
}

/// May this caller see a person-scoped event (AC-4)? The `None` arm of the
/// socket's per-event gate — everything channel-scoped goes through
/// `channels::access` and never reaches here.
pub(crate) async fn visible(state: &AppState, caller: &Caller, event: &ChatServerMessage) -> bool {
    match event {
        ChatServerMessage::Presence { person, .. } => state
            .channels
            .presence_peer(caller.user_id, *person)
            .await
            .unwrap_or(false),
        // Channel-scoped variants are routed by `event_channel` before this is
        // consulted; refusing here keeps a future mis-route fail-closed.
        _ => false,
    }
}

/// `POST /api/channels/{id}/typing` — the caller is typing here (AC-2).
///
/// The socket stays receive-only; this is the REST half of the contract. The
/// server persists nothing and relays one frame to the channel's authorized
/// subscribers; the CLIENT throttles (at most one call per ~3s while typing)
/// and expires the indicator locally (~4s) — both documented on the
/// [`ChatServerMessage::Typing`] variant, so a lost stop signal cannot stick.
pub(crate) async fn typing(
    State(state): State<AppState>,
    caller: Caller,
    Path(channel_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    channels::access(&*state.channels, channel_id, &caller).await?;
    let Some((person, display_name)) = state
        .channels
        .person_ref_of(caller.user_id)
        .await
        .map_err(|_| crate::internal())?
    else {
        // An authenticated caller whose user row is gone mid-session.
        return Err(ApiError::Forbidden);
    };
    let event = ChatServerMessage::Typing {
        channel_id,
        person,
        display_name,
    };
    state.registry.publish_local(event.clone());
    crate::bus::publish_ephemeral(&state.db, state.registry.instance(), &event).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nook_db::{params, Db, DbPool};

    async fn new_tenant(db: &DbPool) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $2)",
            params![id, format!("t-{}", id.simple())],
        )
        .await
        .unwrap();
        id
    }

    async fn add_user_person(db: &DbPool, tenant: Uuid, person: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'U', $4, 'member')",
            params![
                id,
                tenant,
                person,
                format!("u-{}@example.test", id.simple())
            ],
        )
        .await
        .unwrap();
        id
    }

    async fn tenant_channel(db: &DbPool, tenant: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
             VALUES ($1, 'tenant', $2, 'c', $3)",
            params![id, tenant, format!("c-{}", id.simple())],
        )
        .await
        .unwrap();
        id
    }

    async fn dm(db: &DbPool, participants: &[Uuid]) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
             VALUES ($1, 'dm', $1, 'dm', $2)",
            params![id, format!("d-{}", id.simple())],
        )
        .await
        .unwrap();
        for p in participants {
            db.exec(
                "INSERT INTO chat_channel_participants (channel_id, person_id) VALUES ($1, $2)",
                params![id, p],
            )
            .await
            .unwrap();
        }
        id
    }

    fn caller(user: Uuid, tenant: Uuid) -> Caller {
        Caller {
            user_id: user,
            tenant_id: tenant,
            cookie_session: false,
            credential: crate::content::Credential::Session(Uuid::now_v7()),
        }
    }

    /// AC-2/AC-4's positive half end to end: a typing POST relays ONE frame to
    /// the firehose, carrying the typist's person and display name, and the
    /// channel gate the socket applies admits a subscriber and refuses a
    /// stranger — "receives nothing" decided exactly where the pump decides it.
    #[tokio::test]
    async fn typing_is_relayed_once_and_gated_by_channel_membership() {
        let Some(st) = crate::testdb::chat_test("the typing fan-out test").await else {
            return;
        };
        let db = st.db.clone();
        let home = new_tenant(&db).await;
        let elsewhere = new_tenant(&db).await;
        let typist = add_user_person(&db, home, Uuid::now_v7()).await;
        let stranger = add_user_person(&db, elsewhere, Uuid::now_v7()).await;
        let channel = tenant_channel(&db, home).await;

        let mut rx = st.registry.subscribe_all();
        let status = typing(State(st.clone()), caller(typist, home), Path(channel))
            .await
            .expect("typing accepted");
        assert_eq!(status, StatusCode::NO_CONTENT);

        let frame = rx.try_recv().expect("one frame relayed");
        let ChatServerMessage::Typing {
            channel_id,
            person: _,
            display_name,
        } = &frame
        else {
            panic!("expected a Typing frame, got {frame:?}");
        };
        assert_eq!(*channel_id, channel);
        assert_eq!(display_name.as_deref(), Some("U"));
        assert!(rx.try_recv().is_err(), "exactly one frame, nothing stored");

        // The socket's own gate: channel-scoped, so the existing access rule
        // answers — member in, stranger out.
        let gate = crate::registry::event_channel(&frame).expect("typing is channel-scoped");
        assert!(channels::access(&*st.channels, gate, &caller(typist, home))
            .await
            .is_ok());
        assert!(
            channels::access(&*st.channels, gate, &caller(stranger, elsewhere))
                .await
                .is_err()
        );

        // A stranger cannot POST either: the same access rule guards the way in.
        let refused = typing(
            State(st.clone()),
            caller(stranger, elsewhere),
            Path(channel),
        )
        .await;
        assert!(
            refused.is_err(),
            "a non-member cannot make a channel say they type"
        );

        st.teardown().await;
    }

    /// AC-4: presence goes to people who SHARE something — a tenant, an org
    /// channel, a DM — and to nobody else. The rule the socket's person-scoped
    /// arm applies, asked of the real schema.
    #[tokio::test]
    async fn presence_is_visible_to_sharers_and_nobody_else() {
        let Some(st) = crate::testdb::chat_test("the presence visibility test").await else {
            return;
        };
        let db = st.db.clone();
        let subject_person = Uuid::now_v7();

        let home = new_tenant(&db).await;
        let _subject_user = add_user_person(&db, home, subject_person).await;
        let teammate = add_user_person(&db, home, Uuid::now_v7()).await;

        let far = new_tenant(&db).await;
        let far_person = Uuid::now_v7();
        let outsider = add_user_person(&db, far, far_person).await;

        assert!(
            st.channels
                .presence_peer(teammate, subject_person)
                .await
                .unwrap(),
            "a shared tenant makes presence visible"
        );
        assert!(
            !st.channels
                .presence_peer(outsider, subject_person)
                .await
                .unwrap(),
            "no shared scope, no presence — never a global roster"
        );

        // A DM between the two persons opens visibility across tenants.
        dm(&db, &[subject_person, far_person]).await;
        assert!(
            st.channels
                .presence_peer(outsider, subject_person)
                .await
                .unwrap(),
            "a shared DM makes presence visible"
        );

        // And the frame itself routes through the person-scoped arm.
        let event = ChatServerMessage::Presence {
            person: subject_person,
            online: true,
        };
        assert_eq!(crate::registry::event_channel(&event), None);
        assert!(visible(&st, &caller(teammate, home), &event).await);

        st.teardown().await;
    }

    /// `announce` publishes the edge locally — what a viewer's socket on this
    /// instance receives the moment a first socket opens or a last one closes.
    #[tokio::test]
    async fn announce_reaches_local_subscribers() {
        let Some(st) = crate::testdb::chat_test("the presence announce test").await else {
            return;
        };
        let person = Uuid::now_v7();
        let mut rx = st.registry.subscribe_all();
        announce(&st, person, true).await;
        match rx.try_recv().expect("the edge was published") {
            ChatServerMessage::Presence {
                person: p,
                online: true,
            } => assert_eq!(p, person),
            other => panic!("expected Presence online, got {other:?}"),
        }
        st.teardown().await;
    }
}
