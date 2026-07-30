//! Direct messages (MAIN-113): person-to-person and small-group conversations.
//!
//! A DM is a `dm`-owner_type channel (owned by its creating person) whose
//! members are recorded in `chat_channel_participants`, keyed by the stable
//! cross-tenant `person_id` the org-channels work established. Access is
//! participants-only and resolved by person, so the same person reaches a DM
//! from any of their tenants and a tenant admin who is not a participant has no
//! special access. The message view, composer, WS and history all reuse
//! [`crate::channels::access`], which gained a `dm` branch — so posting, reading
//! and subscribing inside a DM work exactly as in a channel.

use axum::extract::{Json, State};
use axum::http::StatusCode;
use nook_types::{DmSummary, OpenDmRequest, PersonRef};
use uuid::Uuid;

use crate::{AppState, Caller, ChatError};

/// A DM has at least the creator plus one other, and at most eight people.
const MIN_PARTICIPANTS: usize = 2;
const MAX_PARTICIPANTS: usize = 8;

/// The caller's person — the identity a DM keys on (MAIN-130).
async fn person_of(
    repo: &dyn crate::repo::dms::DmRepository,
    user_id: Uuid,
) -> Result<Uuid, ChatError> {
    repo.person_of(user_id)
        .await
        .map_err(|_| ChatError::Internal)?
        .ok_or(ChatError::Forbidden)
}

/// May `me` DM `other`? Yes iff `other` has a user in a tenant under one of
/// `me`'s orgs — the same org boundary org channels use (AC-4). This both scopes
/// the picker and gates `open`, so a DM can never be opened cross-org.
async fn dmable(
    repo: &dyn crate::repo::dms::DmRepository,
    me: Uuid,
    other: Uuid,
) -> Result<bool, ChatError> {
    repo.may_dm(me, other)
        .await
        .map_err(|_| ChatError::Internal)
}

/// The people the caller may start a DM with (AC-4): every person in the
/// caller's org(s), minus the caller, with a display name. `DISTINCT ON` folds a
/// person's several tenant users down to one entry.
pub async fn people(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<PersonRef>>, ChatError> {
    let me = person_of(&*state.dms, caller.user_id).await?;
    let rows = state
        .dms
        .people_in_my_orgs(me)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|p| PersonRef {
                person_id: p.person_id,
                display_name: p.display_name,
            })
            .collect(),
    ))
}

/// The caller's DMs (AC-3 list): `dm` channels whose participants include the
/// caller's person, newest first. Non-participants never see a DM here.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<DmSummary>>, ChatError> {
    let me = person_of(&*state.dms, caller.user_id).await?;
    let ids = state
        .dms
        .my_dms(me)
        .await
        .map_err(|_| ChatError::Internal)?;

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(summary(&*state.dms, id, caller.user_id).await?);
    }
    Ok(Json(out))
}

/// Open or reuse a DM with the requested persons (AC-2). The creator is always a
/// participant; an existing DM with *exactly* that set is returned rather than
/// duplicated, otherwise the channel and its participant rows are created in one
/// transaction.
pub async fn open(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<OpenDmRequest>,
) -> Result<(StatusCode, Json<DmSummary>), ChatError> {
    let me = person_of(&*state.dms, caller.user_id).await?;

    // Canonical participant set: creator ∪ requested, deduped and sorted so the
    // set-equality match below is order-independent.
    let mut persons = req.person_ids.clone();
    persons.push(me);
    persons.sort();
    persons.dedup();
    if persons.len() < MIN_PARTICIPANTS || persons.len() > MAX_PARTICIPANTS {
        return Err(ChatError::BadRequest(format!(
            "a DM has {MIN_PARTICIPANTS}–{MAX_PARTICIPANTS} participants"
        )));
    }
    // Every other participant must be reachable in the caller's org (AC-4): no
    // cross-org DMs, and unknown persons are rejected here.
    for &p in &persons {
        if p != me && !dmable(&*state.dms, me, p).await? {
            return Err(ChatError::Forbidden);
        }
    }

    // Open-or-create: an existing DM with this exact participant set wins.
    if let Some(id) = find_exact(&*state.dms, &persons).await? {
        return Ok((
            StatusCode::OK,
            Json(summary(&*state.dms, id, caller.user_id).await?),
        ));
    }

    let id = Uuid::now_v7();
    let slug = format!("dm-{}", id.simple());
    state
        .dms
        .open(id, me, &slug, &persons)
        .await
        .map_err(|_| ChatError::Internal)?;

    Ok((
        StatusCode::CREATED,
        Json(summary(&*state.dms, id, caller.user_id).await?),
    ))
}

/// A `dm` channel whose participant set is *exactly* `persons`. Count-equality
/// plus "every participant is in the set" gives exact equality, since the set is
/// deduped: |members| = N and members ⊆ set with |set| = N ⇒ members = set.
async fn find_exact(
    repo: &dyn crate::repo::dms::DmRepository,
    persons: &[Uuid],
) -> Result<Option<Uuid>, ChatError> {
    repo.find_exact(persons)
        .await
        .map_err(|_| ChatError::Internal)
}

/// A DM's summary: its id, creation time, and participants with display names.
async fn summary(
    repo: &dyn crate::repo::dms::DmRepository,
    id: Uuid,
    reader: Uuid,
) -> Result<DmSummary, ChatError> {
    let created_at = repo
        .created_at(id)
        .await
        .map_err(|_| ChatError::Internal)?
        .ok_or(ChatError::NotFound)?;
    let rows = repo
        .participants(id)
        .await
        .map_err(|_| ChatError::Internal)?;
    // Unread from the other participant(s) since the reader's cursor (MAIN-117),
    // same semantics as a channel: the reader's own messages and deleted ones
    // don't count, and no cursor row means everything counts.
    let unread_count = repo
        .unread_count(id, reader)
        .await
        .map_err(|_| ChatError::Internal)?;
    Ok(DmSummary {
        id,
        created_at,
        participants: rows
            .into_iter()
            .map(|p| PersonRef {
                person_id: p.person_id,
                display_name: p.display_name.unwrap_or_default(),
            })
            .collect(),
        unread_count,
    })
}

#[cfg(test)]
mod tests {
    use super::{list, open, people};
    use crate::{channels, AppState, Caller, ChatError};
    use axum::extract::{Json, State};
    use nook_db::{params, Db, DbPool};
    use nook_types::OpenDmRequest;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;
    use std::sync::Arc;
    use uuid::Uuid;

    async fn pool(url: &str, search_path: &str) -> DbPool {
        let opts = PgConnectOptions::from_str(url)
            .unwrap()
            .options([("search_path", search_path)]);
        nook_db::EnginePool::from_pg(
            PgPoolOptions::new()
                .max_connections(4)
                .connect_with(opts)
                .await
                .unwrap(),
        )
    }

    // DB-backed; a no-op without NOOK_REQUIRE_DB=1, matching the suite convention.
    async fn setup() -> Option<AppState> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            eprintln!("skipping DM test — no NOOK_REQUIRE_DB");
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let bootstrap = pool(&url, "public").await;
        crate::ensure_chat_schema(&bootstrap).await.unwrap();
        nook_control::MIGRATOR.run(bootstrap.pg()).await.unwrap();
        let db = pool(&url, "chat,public").await;
        crate::MIGRATOR.run(db.pg()).await.unwrap();
        Some(AppState {
            channels: Arc::new(crate::repo::channels::DbChannelRepository::new(db.clone())),
            messages: Arc::new(crate::repo::messages::DbMessageRepository::new(db.clone())),
            dms: Arc::new(crate::repo::dms::DbDmRepository::new(db.clone())),
            db,
            registry: Arc::new(crate::registry::Registry::new()),
        })
    }

    fn caller(user: Uuid, tenant: Uuid) -> Caller {
        Caller {
            user_id: user,
            tenant_id: tenant,
            cookie_session: false,
        }
    }

    async fn new_org(db: &DbPool) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO public.orgs (id, name, slug) VALUES ($1, $2, $2)",
            params![id, format!("o-{}", id.simple())],
        )
        .await
        .unwrap();
        id
    }

    async fn tenant_in_org(db: &DbPool, org: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO public.tenants (id, name, slug, org_id) VALUES ($1, $2, $2, $3)",
            params![id, format!("t-{}", id.simple()), org],
        )
        .await
        .unwrap();
        id
    }

    /// A user for `person` in `tenant`. Returns the user id.
    async fn user(db: &DbPool, tenant: Uuid, person: Uuid, name: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO public.users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, $4, $5, 'member')",
            params![
                id,
                tenant,
                person,
                name,
                format!("u-{}@example.test", id.simple())
            ],
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn open_reuses_an_existing_dm_for_the_same_participant_set() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let t = tenant_in_org(&state.db, org).await;
        let (pa, pb) = (Uuid::now_v7(), Uuid::now_v7());
        let ua = user(&state.db, t, pa, "Ana").await;
        user(&state.db, t, pb, "Bo").await;

        let (code1, first) = open(
            State(state.clone()),
            caller(ua, t),
            Json(OpenDmRequest {
                person_ids: vec![pb],
            }),
        )
        .await
        .expect("open dm");
        assert_eq!(code1, axum::http::StatusCode::CREATED);
        assert_eq!(first.0.participants.len(), 2, "creator + one other");

        // Opening again — order of person_ids should not matter — returns the SAME
        // conversation, not a duplicate.
        let (code2, second) = open(
            State(state.clone()),
            caller(ua, t),
            Json(OpenDmRequest {
                person_ids: vec![pb],
            }),
        )
        .await
        .expect("reopen dm");
        assert_eq!(code2, axum::http::StatusCode::OK);
        assert_eq!(second.0.id, first.0.id, "open-or-create is idempotent");
    }

    #[tokio::test]
    async fn access_is_participants_only_with_no_admin_backdoor() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let t = tenant_in_org(&state.db, org).await;
        let (pa, pb, pc) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let ua = user(&state.db, t, pa, "Ana").await;
        user(&state.db, t, pb, "Bo").await;
        // C is an OWNER of the tenant but not a participant — must still be refused.
        let uc = Uuid::now_v7();
        state
            .db
            .exec(
                "INSERT INTO public.users (id, tenant_id, person_id, display_name, email, role)
             VALUES ($1, $2, $3, 'Cy', $4, 'owner')",
                params![uc, t, pc, format!("u-{}@example.test", uc.simple())],
            )
            .await
            .unwrap();

        let (_, dm) = open(
            State(state.clone()),
            caller(ua, t),
            Json(OpenDmRequest {
                person_ids: vec![pb],
            }),
        )
        .await
        .unwrap();

        // A participant may access; the tenant owner who is not in it may not.
        assert!(channels::access(&*state.channels, dm.0.id, &caller(ua, t))
            .await
            .is_ok());
        assert!(matches!(
            channels::access(&*state.channels, dm.0.id, &caller(uc, t)).await,
            Err(ChatError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn a_participant_reaches_a_dm_from_another_org_tenant() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let t1 = tenant_in_org(&state.db, org).await;
        let t2 = tenant_in_org(&state.db, org).await;
        let (pa, pp) = (Uuid::now_v7(), Uuid::now_v7());
        let ua = user(&state.db, t1, pa, "Ana").await;
        // The SAME person pp has a user in both org tenants.
        let up1 = user(&state.db, t1, pp, "Pat").await;
        let up2 = user(&state.db, t2, pp, "Pat").await;

        let (_, dm) = open(
            State(state.clone()),
            caller(ua, t1),
            Json(OpenDmRequest {
                person_ids: vec![pp],
            }),
        )
        .await
        .unwrap();

        // pp reaches the DM as their t1 user AND as their t2 user (person-keyed).
        assert!(
            channels::access(&*state.channels, dm.0.id, &caller(up1, t1))
                .await
                .is_ok()
        );
        assert!(
            channels::access(&*state.channels, dm.0.id, &caller(up2, t2))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cross_org_dm_is_refused_and_picker_is_org_scoped() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let t = tenant_in_org(&state.db, org).await;
        let other_org = new_org(&state.db).await;
        let t_out = tenant_in_org(&state.db, other_org).await;
        let (pa, pb, pout) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let ua = user(&state.db, t, pa, "Ana").await;
        user(&state.db, t, pb, "Bo").await;
        user(&state.db, t_out, pout, "Ozzy").await;

        // A DM to someone in another org is refused (AC-4).
        assert!(matches!(
            open(
                State(state.clone()),
                caller(ua, t),
                Json(OpenDmRequest {
                    person_ids: vec![pout]
                }),
            )
            .await,
            Err(ChatError::Forbidden)
        ));

        // The picker lists org members (pb) and never the outsider (pout) or self.
        let Json(picker) = people(State(state.clone()), caller(ua, t)).await.unwrap();
        let ids: Vec<Uuid> = picker.iter().map(|p| p.person_id).collect();
        assert!(ids.contains(&pb), "an org peer is offered");
        assert!(!ids.contains(&pout), "a cross-org person is never offered");
        assert!(!ids.contains(&pa), "the caller is not in their own picker");
    }

    #[tokio::test]
    async fn dms_are_excluded_from_the_channel_list() {
        let Some(state) = setup().await else { return };
        let org = new_org(&state.db).await;
        let t = tenant_in_org(&state.db, org).await;
        let (pa, pb) = (Uuid::now_v7(), Uuid::now_v7());
        let ua = user(&state.db, t, pa, "Ana").await;
        user(&state.db, t, pb, "Bo").await;

        let (_, dm) = open(
            State(state.clone()),
            caller(ua, t),
            Json(OpenDmRequest {
                person_ids: vec![pb],
            }),
        )
        .await
        .unwrap();

        // The DM shows in the caller's DM list...
        let Json(dms) = list(State(state.clone()), caller(ua, t)).await.unwrap();
        assert!(
            dms.iter().any(|d| d.id == dm.0.id),
            "DM is listed for a member"
        );

        // ...but never in the tenant/org CHANNEL list (AC-6).
        let Json(channels) = channels::list(
            State(state.clone()),
            caller(ua, t),
            axum::extract::Query(channels::ListQuery {
                include_archived: true,
            }),
        )
        .await
        .unwrap();
        assert!(
            !channels.iter().any(|c| c.id == dm.0.id),
            "a DM never appears in the channel list"
        );
    }
}

/// The DM rules against an in-memory [`FakeDmRepository`] — no database
/// (MAIN-257 AC-3).
///
/// Open-or-create hinges on `find_exact` being *exact* set equality: a near
/// miss must create a new conversation, not join an existing one. That is worth
/// proving without a database in the loop, because it is a property of the
/// participant set rather than of any query plan.
#[cfg(test)]
mod fake_tests {
    use super::*;
    use crate::repo::fakes::FakeDmRepository;

    #[tokio::test]
    async fn find_exact_matches_the_whole_set_and_nothing_near_it() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let pair = Uuid::now_v7();
        let repo = FakeDmRepository::new().with_dm(pair, &[a, b]);

        let mut set = vec![b, a];
        set.sort();
        assert_eq!(
            find_exact(&repo, &set).await.unwrap(),
            Some(pair),
            "the match is order-independent"
        );
        assert_eq!(
            find_exact(&repo, &[a, b, c]).await.unwrap(),
            None,
            "a superset opens a new conversation rather than joining the pair"
        );
        assert_eq!(
            find_exact(&repo, &[a]).await.unwrap(),
            None,
            "so does a subset"
        );
    }

    #[tokio::test]
    async fn the_org_boundary_gates_who_may_be_messaged() {
        let (org, other_org) = (Uuid::now_v7(), Uuid::now_v7());
        let (me, mate, stranger) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let repo = FakeDmRepository::new()
            .with_user(Uuid::now_v7(), me, "me", org)
            .with_user(Uuid::now_v7(), mate, "mate", org)
            .with_user(Uuid::now_v7(), stranger, "stranger", other_org);

        assert!(dmable(&repo, me, mate).await.unwrap());
        assert!(
            !dmable(&repo, me, stranger).await.unwrap(),
            "no cross-org DM, even by posting a person id the picker never offered"
        );

        let offered = people_in_my_orgs(&repo, me).await;
        assert_eq!(offered, vec!["mate".to_string()], "and the picker agrees");
    }

    /// The picker's display names, in order — the shape `people()` maps.
    async fn people_in_my_orgs(repo: &FakeDmRepository, me: Uuid) -> Vec<String> {
        use crate::repo::dms::DmRepository;
        repo.people_in_my_orgs(me)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.display_name)
            .collect()
    }

    #[tokio::test]
    async fn a_summary_names_every_participant_and_counts_the_readers_unread() {
        let org = Uuid::now_v7();
        let (p1, p2) = (Uuid::now_v7(), Uuid::now_v7());
        let (u1, u2) = (Uuid::now_v7(), Uuid::now_v7());
        let dm = Uuid::now_v7();
        let repo = FakeDmRepository::new()
            .with_user(u1, p1, "ada", org)
            .with_user(u2, p2, "grace", org)
            .with_dm(dm, &[p1, p2])
            .with_unread(dm, u1, 3);

        let s = summary(&repo, dm, u1).await.unwrap();
        assert_eq!(s.id, dm);
        assert_eq!(s.participants.len(), 2);
        assert_eq!(s.unread_count, 3);
        assert_eq!(
            summary(&repo, dm, u2).await.unwrap().unread_count,
            0,
            "unread is per reader, not per conversation"
        );
    }

    #[tokio::test]
    async fn a_dm_that_does_not_exist_has_no_summary() {
        let repo = FakeDmRepository::new();
        assert!(matches!(
            summary(&repo, Uuid::now_v7(), Uuid::now_v7()).await,
            Err(ChatError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_person_with_no_user_row_is_refused_rather_than_erroring() {
        let repo = FakeDmRepository::new();
        assert!(matches!(
            person_of(&repo, Uuid::now_v7()).await,
            Err(ChatError::Forbidden)
        ));
    }
}
