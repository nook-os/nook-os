//! Direct messages (MAIN-257).
//!
//! A DM is a channel with `owner_type = 'dm'` and a participant list, so this
//! trait shares tables with [`super::channels`]. What it owns is the DM-shaped
//! questions: who may message whom, which conversation already exists for a
//! given set of people, and what a DM looks like in a list.
//!
//! The org boundary is the same one org channels use (MAIN-113 AC-4), and it is
//! enforced twice on purpose — once to scope the people picker, once to gate
//! `open` — so a DM cannot be created cross-org even by posting an id the
//! picker never offered.

use async_trait::async_trait;

use super::RepoResult;
use chrono::{DateTime, Utc};
use nook_db::dialect::type_mapping;
use nook_db::{params, Db, DbPool};
use uuid::Uuid;

/// A person the caller may start a DM with.
#[derive(Debug, Clone)]
pub struct PersonEntry {
    pub person_id: Uuid,
    pub display_name: String,
}

/// A DM's participant, whose display name may be missing if no user row backs
/// the person any more.
#[derive(Debug, Clone)]
pub struct Participant {
    pub person_id: Uuid,
    pub display_name: Option<String>,
}

#[async_trait]
pub trait DmRepository: Send + Sync {
    /// The person behind a user. Reads `users` — nook-control's data,
    /// unreachable from this crate (see the module note on `repo/mod.rs`).
    async fn person_of(&self, user: Uuid) -> RepoResult<Option<Uuid>>;

    /// May `me` message `other`? True when `other` belongs to a tenant under
    /// any org `me` belongs to.
    async fn may_dm(&self, me: Uuid, other: Uuid) -> RepoResult<bool>;

    /// Everyone in the caller's org(s) except the caller. `DISTINCT ON` folds a
    /// person's several tenant users into one entry, so somebody in two of an
    /// org's tenants appears once.
    async fn people_in_my_orgs(&self, me: Uuid) -> RepoResult<Vec<PersonEntry>>;

    /// The caller's DMs, newest first. A non-participant never sees one.
    async fn my_dms(&self, me: Uuid) -> RepoResult<Vec<Uuid>>;

    /// The DM whose participant set is *exactly* `persons`, if it exists.
    ///
    /// Count-equality plus "every participant is in the set" gives exact
    /// equality, because the set is deduped: |members| = N and members ⊆ set
    /// with |set| = N implies members = set. This is what makes open-or-create
    /// idempotent rather than spawning a second conversation.
    async fn find_exact(&self, persons: &[Uuid]) -> RepoResult<Option<Uuid>>;

    /// Create the channel and its participant rows together. One transaction:
    /// a DM with no participants would be invisible to everyone including its
    /// creator, so a partial write must not be reachable.
    async fn open(
        &self,
        id: Uuid,
        creator_person: Uuid,
        slug: &str,
        persons: &[Uuid],
    ) -> RepoResult<()>;

    async fn created_at(&self, channel: Uuid) -> RepoResult<Option<DateTime<Utc>>>;

    async fn participants(&self, channel: Uuid) -> RepoResult<Vec<Participant>>;

    /// Unread from the other participants since the reader's cursor — the same
    /// semantics a channel uses: the reader's own messages and deleted ones do
    /// not count, and no cursor row means everything counts.
    async fn unread_count(&self, channel: Uuid, reader: Uuid) -> RepoResult<i64>;
}

pub struct DbDmRepository {
    db: DbPool,
}

impl DbDmRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl DmRepository for DbDmRepository {
    async fn person_of(&self, user: Uuid) -> RepoResult<Option<Uuid>> {
        // `(Option<Uuid>,)`, not a bare `Uuid`: `person_id` is nullable on
        // pre-MAIN-130 user rows, and `/api/me` documents `null` for those. A
        // non-null decode would turn that documented null into a 500.
        let row: Option<(Option<Uuid>,)> = self
            .db
            .query_opt("SELECT person_id FROM users WHERE id = $1", params![user])
            .await?;
        Ok(row.and_then(|(p,)| p))
    }

    async fn may_dm(&self, me: Uuid, other: Uuid) -> RepoResult<bool> {
        self.db
            .query_scalar::<bool>(
                "SELECT EXISTS(
                     SELECT 1 FROM users u
                     JOIN tenants t ON t.id = u.tenant_id
                     WHERE u.person_id = $2
                       AND t.org_id IN (
                           SELECT t2.org_id FROM users u2
                           JOIN tenants t2 ON t2.id = u2.tenant_id
                           WHERE u2.person_id = $1
                       )
                 )",
                params![me, other],
            )
            .await
            .map_err(Into::into)
    }

    async fn people_in_my_orgs(&self, me: Uuid) -> RepoResult<Vec<PersonEntry>> {
        let rows: Vec<(Uuid, String)> = self
            .db
            .query_all(
                "SELECT DISTINCT ON (u.person_id) u.person_id, u.display_name
                   FROM users u
                   JOIN tenants t ON t.id = u.tenant_id
                  WHERE u.person_id <> $1
                    AND t.org_id IN (
                        SELECT t2.org_id FROM users u2
                        JOIN tenants t2 ON t2.id = u2.tenant_id
                        WHERE u2.person_id = $1
                    )
                  ORDER BY u.person_id, u.display_name",
                params![me],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(person_id, display_name)| PersonEntry {
                person_id,
                display_name,
            })
            .collect())
    }

    async fn my_dms(&self, me: Uuid) -> RepoResult<Vec<Uuid>> {
        self.db
            .query_scalar_all::<Uuid>(
                "SELECT c.id FROM chat_channels c
                   JOIN chat_channel_participants p ON p.channel_id = c.id
                  WHERE c.owner_type = 'dm' AND p.person_id = $1
                  ORDER BY c.created_at DESC",
                params![me],
            )
            .await
            .map_err(Into::into)
    }

    async fn find_exact(&self, persons: &[Uuid]) -> RepoResult<Option<Uuid>> {
        let n = persons.len() as i64;
        self.db
            .query_scalar_opt::<Uuid>(
                "SELECT c.id FROM chat_channels c
                   JOIN chat_channel_participants p ON p.channel_id = c.id
                  WHERE c.owner_type = 'dm'
                  GROUP BY c.id
                 HAVING count(*) = $2
                    AND count(*) FILTER (WHERE p.person_id = ANY($1)) = $2
                  LIMIT 1",
                params![persons.to_vec(), n],
            )
            .await
            .map_err(Into::into)
    }

    async fn open(
        &self,
        id: Uuid,
        creator_person: Uuid,
        slug: &str,
        persons: &[Uuid],
    ) -> RepoResult<()> {
        let mut tx = self.db.begin().await.map_err(nook_db::DbError::from)?;
        // owner_id = the creating person; name is empty (the UI names a DM by
        // its counterparts). The generated slug satisfies the
        // (owner_type, owner_id, slug) uniqueness constraint without any
        // human-facing slug.
        tx.exec(
            "INSERT INTO chat_channels (id, owner_type, owner_id, name, slug)
             VALUES ($1, 'dm', $2, '', $3)",
            params![id, creator_person, slug],
        )
        .await?;
        for &p in persons {
            tx.exec(
                "INSERT INTO chat_channel_participants (channel_id, person_id) VALUES ($1, $2)",
                params![id, p],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn created_at(&self, channel: Uuid) -> RepoResult<Option<DateTime<Utc>>> {
        self.db
            .query_scalar_opt::<DateTime<Utc>>(
                "SELECT created_at FROM chat_channels WHERE id = $1",
                params![channel],
            )
            .await
            .map_err(Into::into)
    }

    async fn participants(&self, channel: Uuid) -> RepoResult<Vec<Participant>> {
        let rows: Vec<(Uuid, Option<String>)> = self
            .db
            .query_all(
                "SELECT DISTINCT ON (pp.person_id) pp.person_id, u.display_name
                   FROM chat_channel_participants pp
                   LEFT JOIN users u ON u.person_id = pp.person_id
                  WHERE pp.channel_id = $1
                  ORDER BY pp.person_id, u.display_name",
                params![channel],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(person_id, display_name)| Participant {
                person_id,
                display_name,
            })
            .collect())
    }

    async fn unread_count(&self, channel: Uuid, reader: Uuid) -> RepoResult<i64> {
        self.db
            .query_scalar::<i64>(
                &format!(
                    "SELECT count(*) FROM chat_messages m
                      WHERE m.channel_id = $1
                        AND m.author_id <> $2
                        AND m.deleted_at IS NULL
                        AND m.created_at > COALESCE(
                            (SELECT r.last_read_at FROM chat_read_cursors r
                               WHERE r.channel_id = $1 AND r.user_id = $2),
                            {ninf})",
                    ninf = type_mapping(self.db.engine()).cast("'-infinity'", "timestamptz")
                ),
                params![channel, reader],
            )
            .await
            .map_err(Into::into)
    }
}
