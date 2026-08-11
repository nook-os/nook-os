//! In-memory repositories (MAIN-257 AC-3).
//!
//! These are not stubs that return canned answers: each holds the same rows the
//! database would and answers from them, so a caller test exercises the real
//! branching — a cross-tenant caller is refused because the fake's channel is
//! owned by another tenant, not because a flag said "refuse".
//!
//! What they deliberately do NOT reproduce is the database's own bookkeeping:
//! no FKs, no cascade, no ordering guarantees beyond what a method documents.
//! A test that needs those is a database test, and the suite has those already.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use super::channels::{CategoryRow, ChannelOwner, ChannelRepository, ChannelRow, OwnerScope};
use super::dms::{DmRepository, Participant, PersonEntry};
use super::messages::{
    MessageMeta, MessageParent, MessageRepository, MessageRow, NewMessage, Page, ReactionRow,
};
use super::{RepoError, RepoResult};

/// A fixed instant, so a fake's rows are comparable across runs.
fn epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).unwrap()
}

// ── channels ────────────────────────────────────────────────────────────────

/// A channel as the fake stores it: the row a caller sees, plus the owner and
/// participant facts the scope checks read.
#[derive(Debug, Clone)]
pub(crate) struct FakeChannel {
    pub row: ChannelRow,
    pub owner_id: Uuid,
    /// Persons in a DM. Empty for tenant/org channels.
    pub participants: Vec<Uuid>,
}

#[derive(Default)]
pub(crate) struct FakeChannelRepository {
    inner: Mutex<ChannelState>,
}

#[derive(Default)]
struct ChannelState {
    channels: Vec<FakeChannel>,
    categories: Vec<(CategoryRow, Uuid)>,
    /// `user -> person`. Chat resolves a person before every membership answer.
    persons: HashMap<Uuid, Uuid>,
    /// `user -> (tenant, role)`.
    roles: HashMap<Uuid, (Uuid, String)>,
    /// `tenant -> org`.
    orgs: HashMap<Uuid, Uuid>,
    read_cursors: Vec<(Uuid, Uuid)>,
}

impl FakeChannelRepository {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a user: which person they are, which tenant they are in, and
    /// what role they hold there.
    pub(crate) fn with_user(self, user: Uuid, person: Uuid, tenant: Uuid, role: &str) -> Self {
        {
            let mut st = self.inner.lock().unwrap();
            st.persons.insert(user, person);
            st.roles.insert(user, (tenant, role.to_string()));
        }
        self
    }

    pub(crate) fn with_tenant_in_org(self, tenant: Uuid, org: Uuid) -> Self {
        self.inner.lock().unwrap().orgs.insert(tenant, org);
        self
    }

    /// A tenant- or org-owned channel. `owner_type` is `"tenant"` or `"org"`.
    pub(crate) fn with_channel(self, id: Uuid, owner_type: &str, owner_id: Uuid) -> Self {
        self.push(id, owner_type, owner_id, Vec::new(), None)
    }

    pub(crate) fn with_archived_channel(self, id: Uuid, owner_type: &str, owner_id: Uuid) -> Self {
        self.push(id, owner_type, owner_id, Vec::new(), Some(epoch()))
    }

    /// A DM, reachable only by the persons listed.
    pub(crate) fn with_dm(self, id: Uuid, participants: &[Uuid]) -> Self {
        self.push(id, "dm", Uuid::nil(), participants.to_vec(), None)
    }

    fn push(
        self,
        id: Uuid,
        owner_type: &str,
        owner_id: Uuid,
        participants: Vec<Uuid>,
        archived_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.inner.lock().unwrap().channels.push(FakeChannel {
            row: ChannelRow {
                id,
                name: format!("c-{}", id.simple()),
                slug: format!("c-{}", id.simple()),
                owner_type: owner_type.to_string(),
                archived_at,
                category_id: None,
                position: 0,
                unread_count: 0,
                created_at: epoch(),
            },
            owner_id,
            participants,
        });
        self
    }
}

impl ChannelState {
    fn person_of(&self, user: Uuid) -> Option<Uuid> {
        self.persons.get(&user).copied()
    }

    /// Every tenant under `org`, from the tenant→org map.
    fn tenants_in(&self, org: Uuid) -> Vec<Uuid> {
        self.orgs
            .iter()
            .filter(|(_, o)| **o == org)
            .map(|(t, _)| *t)
            .collect()
    }

    /// A caller sees their tenant's rows plus their org's — the predicate every
    /// scoped read shares.
    fn in_scope(&self, owner_type: &str, owner_id: Uuid, tenant: Uuid) -> bool {
        match owner_type {
            "tenant" => owner_id == tenant,
            "org" => self.orgs.get(&tenant) == Some(&owner_id),
            _ => false,
        }
    }
}

#[async_trait]
impl ChannelRepository for FakeChannelRepository {
    async fn owner_of(&self, channel: Uuid) -> RepoResult<Option<ChannelOwner>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .channels
            .iter()
            .find(|c| c.row.id == channel)
            .map(|c| ChannelOwner {
                owner_type: c.row.owner_type.clone(),
                owner_id: c.owner_id,
                archived_at: c.row.archived_at,
            }))
    }

    async fn org_of_tenant(&self, tenant: Uuid) -> RepoResult<Option<Uuid>> {
        Ok(self.inner.lock().unwrap().orgs.get(&tenant).copied())
    }

    async fn person_in_org(&self, user: Uuid, org: Uuid) -> RepoResult<bool> {
        let st = self.inner.lock().unwrap();
        let Some(person) = st.person_of(user) else {
            return Ok(false);
        };
        let tenants = st.tenants_in(org);
        // Any user backed by the same person, in any of that org's tenants.
        Ok(st
            .roles
            .iter()
            .any(|(u, (t, _))| tenants.contains(t) && st.person_of(*u) == Some(person)))
    }

    async fn tenant_role(&self, user: Uuid, tenant: Uuid) -> RepoResult<Option<String>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .roles
            .get(&user)
            .filter(|(t, _)| *t == tenant)
            .map(|(_, r)| r.clone()))
    }

    async fn person_is_participant(&self, channel: Uuid, user: Uuid) -> RepoResult<bool> {
        let st = self.inner.lock().unwrap();
        let Some(person) = st.person_of(user) else {
            return Ok(false);
        };
        Ok(st
            .channels
            .iter()
            .find(|c| c.row.id == channel)
            .is_some_and(|c| c.participants.contains(&person)))
    }

    async fn presence_peer(&self, viewer_user: Uuid, person: Uuid) -> RepoResult<bool> {
        let st = self.inner.lock().unwrap();
        let Some(my_person) = st.person_of(viewer_user) else {
            return Ok(false);
        };
        let my_tenant = st.roles.get(&viewer_user).map(|(t, _)| *t);
        let in_tenant = |t: &Uuid, p: Uuid| {
            st.roles
                .iter()
                .any(|(u, (ut, _))| ut == t && st.persons.get(u) == Some(&p))
        };
        let same_tenant = my_tenant.is_some_and(|t| in_tenant(&t, person));
        let my_org = my_tenant.and_then(|t| st.orgs.get(&t)).copied();
        let org_share = my_org.is_some_and(|org| {
            st.channels
                .iter()
                .any(|c| c.row.owner_type == "org" && c.owner_id == org)
                && st.roles.iter().any(|(u, (t, _))| {
                    st.orgs.get(t) == Some(&org) && st.persons.get(u) == Some(&person)
                })
        });
        let dm_share = st
            .channels
            .iter()
            .any(|c| c.participants.contains(&my_person) && c.participants.contains(&person));
        Ok(same_tenant || org_share || dm_share)
    }

    async fn person_ref_of(&self, user: Uuid) -> RepoResult<Option<(Uuid, Option<String>)>> {
        let st = self.inner.lock().unwrap();
        // The channel fake registers no display names; `None` is the honest
        // answer and the frame contract allows it.
        Ok(st.person_of(user).map(|p| (p, None)))
    }

    async fn create(&self, owner: OwnerScope, name: &str, slug: &str) -> RepoResult<ChannelRow> {
        let mut st = self.inner.lock().unwrap();
        // The real constraint is on (owner, slug), not the display name.
        if st
            .channels
            .iter()
            .any(|c| c.owner_id == owner.owner_id && c.row.slug == slug)
        {
            return Err(RepoError::Conflict);
        }
        let row = ChannelRow {
            id: Uuid::now_v7(),
            name: name.to_string(),
            slug: slug.to_string(),
            owner_type: owner.owner_type.to_string(),
            archived_at: None,
            category_id: None,
            position: 0,
            unread_count: 0,
            created_at: epoch(),
        };
        st.channels.push(FakeChannel {
            row: row.clone(),
            owner_id: owner.owner_id,
            participants: Vec::new(),
        });
        Ok(row)
    }

    async fn list(
        &self,
        tenant: Uuid,
        include_archived: bool,
        _reader: Uuid,
    ) -> RepoResult<Vec<ChannelRow>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .channels
            .iter()
            .filter(|c| st.in_scope(&c.row.owner_type, c.owner_id, tenant))
            .filter(|c| include_archived || c.row.archived_at.is_none())
            .map(|c| c.row.clone())
            .collect())
    }

    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        archived: Option<bool>,
    ) -> RepoResult<Option<ChannelRow>> {
        let mut st = self.inner.lock().unwrap();
        let Some(c) = st.channels.iter_mut().find(|c| c.row.id == id) else {
            return Ok(None);
        };
        if let Some(n) = name {
            c.row.name = n;
        }
        if let Some(a) = archived {
            c.row.archived_at = a.then(epoch);
        }
        Ok(Some(c.row.clone()))
    }

    async fn category_matches_channel(
        &self,
        category: Uuid,
        channel: Uuid,
    ) -> RepoResult<Option<bool>> {
        let st = self.inner.lock().unwrap();
        let (Some((cat, cat_owner)), Some(ch)) = (
            st.categories.iter().find(|(c, _)| c.id == category),
            st.channels.iter().find(|c| c.row.id == channel),
        ) else {
            return Ok(None);
        };
        Ok(Some(
            cat.owner_type == ch.row.owner_type && *cat_owner == ch.owner_id,
        ))
    }

    async fn place(
        &self,
        id: Uuid,
        category: Option<Uuid>,
        position: i32,
    ) -> RepoResult<Option<ChannelRow>> {
        let mut st = self.inner.lock().unwrap();
        let Some(c) = st.channels.iter_mut().find(|c| c.row.id == id) else {
            return Ok(None);
        };
        c.row.category_id = category;
        c.row.position = position;
        Ok(Some(c.row.clone()))
    }

    async fn mark_read(&self, channel: Uuid, user: Uuid) -> RepoResult<()> {
        let mut st = self.inner.lock().unwrap();
        if !st.read_cursors.contains(&(channel, user)) {
            st.read_cursors.push((channel, user));
        }
        Ok(())
    }

    async fn categories(&self, tenant: Uuid) -> RepoResult<Vec<CategoryRow>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .categories
            .iter()
            .filter(|(c, owner)| st.in_scope(&c.owner_type, *owner, tenant))
            .map(|(c, _)| c.clone())
            .collect())
    }

    async fn category_count(&self, owner: OwnerScope) -> RepoResult<i64> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .categories
            .iter()
            .filter(|(c, o)| c.owner_type == owner.owner_type && *o == owner.owner_id)
            .count() as i64)
    }

    async fn create_category(
        &self,
        owner: OwnerScope,
        name: &str,
        position: i32,
    ) -> RepoResult<CategoryRow> {
        let mut st = self.inner.lock().unwrap();
        let row = CategoryRow {
            id: Uuid::now_v7(),
            name: name.to_string(),
            owner_type: owner.owner_type.to_string(),
            position,
            created_at: epoch(),
        };
        st.categories.push((row.clone(), owner.owner_id));
        Ok(row)
    }

    async fn rename_category(
        &self,
        id: Uuid,
        tenant: Uuid,
        name: &str,
    ) -> RepoResult<Option<CategoryRow>> {
        let mut st = self.inner.lock().unwrap();
        let scoped = st
            .categories
            .iter()
            .any(|(c, o)| c.id == id && st.in_scope(&c.owner_type, *o, tenant));
        if !scoped {
            return Ok(None);
        }
        let (row, _) = st.categories.iter_mut().find(|(c, _)| c.id == id).unwrap();
        row.name = name.to_string();
        Ok(Some(row.clone()))
    }

    async fn delete_category(&self, id: Uuid, tenant: Uuid) -> RepoResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let doomed: Vec<Uuid> = st
            .categories
            .iter()
            .filter(|(c, o)| c.id == id && st.in_scope(&c.owner_type, *o, tenant))
            .map(|(c, _)| c.id)
            .collect();
        st.categories.retain(|(c, _)| !doomed.contains(&c.id));
        Ok(doomed.len() as u64)
    }

    async fn reorder_categories(&self, tenant: Uuid, ordered: &[Uuid]) -> RepoResult<()> {
        let mut st = self.inner.lock().unwrap();
        for (pos, id) in ordered.iter().enumerate() {
            let scoped = st
                .categories
                .iter()
                .any(|(c, o)| c.id == *id && st.in_scope(&c.owner_type, *o, tenant));
            if !scoped {
                continue;
            }
            if let Some((row, _)) = st.categories.iter_mut().find(|(c, _)| c.id == *id) {
                row.position = pos as i32;
            }
        }
        Ok(())
    }
}

// ── messages ────────────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct FakeMessageRepository {
    inner: Mutex<MessageState>,
}

#[derive(Default)]
struct MessageState {
    messages: Vec<MessageRow>,
    /// `(message, user, emoji)`.
    reactions: Vec<(Uuid, Uuid, String)>,
    /// `(message, prior_body, action, actor)` — the revision trail.
    revisions: Vec<(Uuid, String, String, Uuid)>,
}

impl FakeMessageRepository {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_message(self, id: Uuid, channel: Uuid, author: Uuid, body: &str) -> Self {
        self.push(id, channel, author, body, None, false)
    }

    pub(crate) fn with_reply(self, id: Uuid, parent: Uuid, channel: Uuid, author: Uuid) -> Self {
        self.push(id, channel, author, "reply", Some(parent), false)
    }

    pub(crate) fn with_deleted_message(self, id: Uuid, channel: Uuid, author: Uuid) -> Self {
        self.push(id, channel, author, "secret", None, true)
    }

    fn push(
        self,
        id: Uuid,
        channel: Uuid,
        author: Uuid,
        body: &str,
        parent: Option<Uuid>,
        deleted: bool,
    ) -> Self {
        self.inner.lock().unwrap().messages.push(MessageRow {
            id,
            channel_id: channel,
            author_id: author,
            author_name: None,
            body: body.to_string(),
            parent_message_id: parent,
            reply_count: 0,
            last_reply_at: None,
            created_at: epoch(),
            edited_at: None,
            deleted_at: deleted.then(epoch),
            kind: None,
        });
        self
    }

    pub(crate) fn with_reaction(self, message: Uuid, user: Uuid, emoji: &str) -> Self {
        self.inner
            .lock()
            .unwrap()
            .reactions
            .push((message, user, emoji.to_string()));
        self
    }

    /// The stored body, unredacted — how a test proves a soft delete kept the
    /// row rather than removing it.
    pub(crate) fn body_of(&self, message: Uuid) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .messages
            .iter()
            .find(|m| m.id == message)
            .map(|m| m.body.clone())
    }

    /// The revision trail, as `(action, prior_body)` pairs.
    pub(crate) fn revisions_of(&self, message: Uuid) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap()
            .revisions
            .iter()
            .filter(|(m, _, _, _)| *m == message)
            .map(|(_, prior, action, _)| (action.clone(), prior.clone()))
            .collect()
    }
}

impl MessageState {
    /// Fill a row's thread rollups the way the reply-aware projection does.
    fn with_rollups(&self, m: &MessageRow) -> MessageRow {
        let replies: Vec<&MessageRow> = self
            .messages
            .iter()
            .filter(|r| r.parent_message_id == Some(m.id))
            .collect();
        MessageRow {
            reply_count: replies.len() as i64,
            last_reply_at: replies.iter().map(|r| r.created_at).max(),
            ..m.clone()
        }
    }
}

/// Newest-first on id, then the keyset cursor and limit — the same page shape
/// the SQL produces.
fn page_of(mut rows: Vec<MessageRow>, page: Page) -> Vec<MessageRow> {
    rows.sort_by_key(|m| std::cmp::Reverse(m.id));
    rows.into_iter()
        .filter(|m| page.before.is_none_or(|before| m.id < before))
        .take(page.limit.max(0) as usize)
        .collect()
}

#[async_trait]
impl MessageRepository for FakeMessageRepository {
    async fn reactions_for(
        &self,
        messages: &[Uuid],
        viewer: Option<Uuid>,
    ) -> RepoResult<Vec<ReactionRow>> {
        let st = self.inner.lock().unwrap();
        let mut grouped: Vec<ReactionRow> = Vec::new();
        for (m, u, e) in st.reactions.iter() {
            if !messages.contains(m) {
                continue;
            }
            match grouped
                .iter_mut()
                .find(|r| r.message_id == *m && r.emoji == *e)
            {
                Some(r) => {
                    r.count += 1;
                    r.reacted |= viewer == Some(*u);
                }
                None => grouped.push(ReactionRow {
                    message_id: *m,
                    emoji: e.clone(),
                    count: 1,
                    reacted: viewer == Some(*u),
                }),
            }
        }
        grouped.sort_by(|a, b| (a.message_id, &a.emoji).cmp(&(b.message_id, &b.emoji)));
        Ok(grouped)
    }

    async fn parent_of(&self, message: Uuid) -> RepoResult<Option<MessageParent>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .messages
            .iter()
            .find(|m| m.id == message)
            .map(|m| MessageParent {
                channel_id: m.channel_id,
                parent_message_id: m.parent_message_id,
            }))
    }

    async fn meta(&self, message: Uuid) -> RepoResult<Option<MessageMeta>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .messages
            .iter()
            .find(|m| m.id == message)
            .map(|m| MessageMeta {
                channel_id: m.channel_id,
                author_id: m.author_id,
                deleted_at: m.deleted_at,
            }))
    }

    async fn post(&self, new: NewMessage) -> RepoResult<MessageRow> {
        let mut st = self.inner.lock().unwrap();
        let row = MessageRow {
            id: Uuid::now_v7(),
            channel_id: new.channel_id,
            author_id: new.author_id,
            author_name: None,
            body: new.body,
            parent_message_id: new.parent_message_id,
            reply_count: 0,
            last_reply_at: None,
            created_at: epoch(),
            edited_at: None,
            deleted_at: None,
            kind: new.kind,
        };
        st.messages.push(row.clone());
        Ok(row)
    }

    async fn get(&self, message: Uuid) -> RepoResult<Option<MessageRow>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .messages
            .iter()
            .find(|m| m.id == message)
            .map(|m| st.with_rollups(m)))
    }

    async fn history(&self, channel: Uuid, page: Page) -> RepoResult<Vec<MessageRow>> {
        let st = self.inner.lock().unwrap();
        let rows = st
            .messages
            .iter()
            .filter(|m| m.channel_id == channel && m.parent_message_id.is_none())
            .map(|m| st.with_rollups(m))
            .collect();
        Ok(page_of(rows, page))
    }

    async fn replies(&self, parent: Uuid, page: Page) -> RepoResult<Vec<MessageRow>> {
        let st = self.inner.lock().unwrap();
        // Replies use the cheap projection: no rollups of their own.
        let rows = st
            .messages
            .iter()
            .filter(|m| m.parent_message_id == Some(parent))
            .cloned()
            .collect();
        Ok(page_of(rows, page))
    }

    async fn add_reaction(&self, message: Uuid, user: Uuid, emoji: &str) -> RepoResult<()> {
        let mut st = self.inner.lock().unwrap();
        let key = (message, user, emoji.to_string());
        if !st.reactions.contains(&key) {
            st.reactions.push(key);
        }
        Ok(())
    }

    async fn remove_reaction(&self, message: Uuid, user: Uuid, emoji: &str) -> RepoResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.reactions
            .retain(|(m, u, e)| !(*m == message && *u == user && e == emoji));
        Ok(())
    }

    async fn edit(&self, message: Uuid, body: &str, actor: Uuid) -> RepoResult<()> {
        let mut st = self.inner.lock().unwrap();
        let Some(m) = st.messages.iter().find(|m| m.id == message) else {
            return Ok(());
        };
        // The revision records what was actually stored, before the update.
        let prior = m.body.clone();
        st.revisions
            .push((message, prior, "edit".to_string(), actor));
        let m = st.messages.iter_mut().find(|m| m.id == message).unwrap();
        m.body = body.to_string();
        m.edited_at = Some(epoch());
        Ok(())
    }

    async fn soft_delete(&self, message: Uuid, actor: Uuid) -> RepoResult<()> {
        let mut st = self.inner.lock().unwrap();
        let Some(m) = st.messages.iter().find(|m| m.id == message) else {
            return Ok(());
        };
        let prior = m.body.clone();
        st.revisions
            .push((message, prior, "delete".to_string(), actor));
        let m = st.messages.iter_mut().find(|m| m.id == message).unwrap();
        m.deleted_at = Some(epoch());
        Ok(())
    }
}

// ── direct messages ─────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct FakeDmRepository {
    inner: Mutex<DmState>,
}

#[derive(Default)]
struct DmState {
    /// `user -> (person, display_name, org)`.
    users: HashMap<Uuid, (Uuid, String, Uuid)>,
    /// `dm -> participant persons`.
    dms: Vec<(Uuid, Vec<Uuid>)>,
    /// `(dm, reader) -> unread`.
    unread: HashMap<(Uuid, Uuid), i64>,
}

impl FakeDmRepository {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_user(self, user: Uuid, person: Uuid, display_name: &str, org: Uuid) -> Self {
        self.inner
            .lock()
            .unwrap()
            .users
            .insert(user, (person, display_name.to_string(), org));
        self
    }

    pub(crate) fn with_dm(self, id: Uuid, persons: &[Uuid]) -> Self {
        self.inner.lock().unwrap().dms.push((id, persons.to_vec()));
        self
    }

    pub(crate) fn with_unread(self, dm: Uuid, reader: Uuid, n: i64) -> Self {
        self.inner.lock().unwrap().unread.insert((dm, reader), n);
        self
    }
}

impl DmState {
    fn orgs_of_person(&self, person: Uuid) -> Vec<Uuid> {
        self.users
            .values()
            .filter(|(p, _, _)| *p == person)
            .map(|(_, _, o)| *o)
            .collect()
    }
}

#[async_trait]
impl DmRepository for FakeDmRepository {
    async fn person_of(&self, user: Uuid) -> RepoResult<Option<Uuid>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .users
            .get(&user)
            .map(|(p, _, _)| *p))
    }

    async fn may_dm(&self, me: Uuid, other: Uuid) -> RepoResult<bool> {
        let st = self.inner.lock().unwrap();
        let mine = st.orgs_of_person(me);
        Ok(st.orgs_of_person(other).iter().any(|o| mine.contains(o)))
    }

    async fn people_in_my_orgs(&self, me: Uuid) -> RepoResult<Vec<PersonEntry>> {
        let st = self.inner.lock().unwrap();
        let mine = st.orgs_of_person(me);
        let mut out: Vec<PersonEntry> = Vec::new();
        for (person, name, org) in st.users.values() {
            // One entry per person, however many of the org's tenants they are
            // in — the `DISTINCT ON` the SQL does.
            if *person == me || !mine.contains(org) || out.iter().any(|e| e.person_id == *person) {
                continue;
            }
            out.push(PersonEntry {
                person_id: *person,
                display_name: name.clone(),
            });
        }
        out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        Ok(out)
    }

    async fn my_dms(&self, me: Uuid) -> RepoResult<Vec<Uuid>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .dms
            .iter()
            .filter(|(_, persons)| persons.contains(&me))
            .map(|(id, _)| *id)
            .collect())
    }

    async fn find_exact(&self, persons: &[Uuid]) -> RepoResult<Option<Uuid>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .dms
            .iter()
            .find(|(_, have)| {
                have.len() == persons.len() && have.iter().all(|p| persons.contains(p))
            })
            .map(|(id, _)| *id))
    }

    async fn open(
        &self,
        id: Uuid,
        _creator_person: Uuid,
        _slug: &str,
        persons: &[Uuid],
    ) -> RepoResult<()> {
        // Channel and participants land together: there is no partial state a
        // reader could observe, which is the transaction the SQL impl uses.
        self.inner.lock().unwrap().dms.push((id, persons.to_vec()));
        Ok(())
    }

    async fn created_at(&self, channel: Uuid) -> RepoResult<Option<DateTime<Utc>>> {
        let st = self.inner.lock().unwrap();
        Ok(st.dms.iter().any(|(id, _)| *id == channel).then(epoch))
    }

    async fn participants(&self, channel: Uuid) -> RepoResult<Vec<Participant>> {
        let st = self.inner.lock().unwrap();
        let Some((_, persons)) = st.dms.iter().find(|(id, _)| *id == channel) else {
            return Ok(Vec::new());
        };
        Ok(persons
            .iter()
            .map(|p| Participant {
                person_id: *p,
                display_name: st
                    .users
                    .values()
                    .find(|(person, _, _)| person == p)
                    .map(|(_, name, _)| name.clone()),
            })
            .collect())
    }

    async fn unread_count(&self, channel: Uuid, reader: Uuid) -> RepoResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .unread
            .get(&(channel, reader))
            .copied()
            .unwrap_or(0))
    }
}
