//! Operator and admin data access (MAIN-258).
//!
//! Five surfaces, five traits, because they share a page in the UI and nothing
//! else:
//!
//! - [`OperatorRepository`] — the operator console. It owns `orgs` and
//!   `role_bindings` outright, plus the four keyset-paginated read-models the
//!   console lists.
//! - [`ManagedContentRepository`] — `managed_content`: the shipped skill and
//!   hook set.
//! - [`SkillRepository`] — `skills`: what a tenant has taught its fleet.
//! - [`SettingRepository`] — the `settings` rows the settings page renders.
//! - [`ThemeRepository`] — `themes`.
//!
//! **The operator pages read five other aggregates' tables and that is fine.**
//! `tenants_page` counts a tenant's users, nodes, sessions and workspaces;
//! `nodes_page` joins tenants; `bindings_page` joins users, orgs and tenants.
//! These are read-models, not ownership: nothing here writes another
//! aggregate's rows, and each is a single projection the console renders whole.
//! Splitting them across five repositories would turn one query into five plus
//! a join in Rust, which is how an N+1 gets written. The rule the chain
//! actually enforces — one aggregate owns each table's *writes* — holds.
//!
//! **What did NOT come here.** The tenant-scoped reads the operator surfaces
//! needed went to their owning aggregate instead, where they belong and where
//! the next reader will look: workspace names to `WorkspaceRepository`,
//! operator-visible task titles to `TaskRepository`, a deployment-wide user
//! lookup by email to `IdentityRepository`, and node revoke/remove/list to
//! `NodeRepository`, which already had them.
//!
//! **`orgs` and the tenant→org edge live together.** No other aggregate touches
//! `orgs`, and moving a tenant between orgs is an operator action, so the org
//! graph is owned here in one piece rather than split across `IdentityRepository`
//! (which owns tenants) and this one. A tenant's `org_id` is the only tenant
//! column this trait writes, and it is named for that.

use async_trait::async_trait;
use nook_db::dialect::type_mapping;
use nook_db::{params, CiMatch, Db, DbPool, Postgres, TypeMapping};
use nook_types::*;
use uuid::Uuid;

use crate::error::ApiResult;

/// One page of a console list: `after` is the last id already seen, so rows
/// come back strictly older. `limit` is clamped by the caller.
#[derive(Debug, Clone, Copy)]
pub struct Keyset {
    pub after: Option<Uuid>,
    pub limit: i64,
}

/// A managed row as it is stored, reduced to what a node needs to apply it.
#[derive(Debug, Clone)]
pub struct ManagedPayload {
    pub name: String,
    pub content: String,
    pub sha256: String,
}

/// What the seeder needs to decide whether a shipped default has moved: the
/// installed version, and the default sha it was installed from.
#[derive(Debug, Clone)]
pub struct ManagedDefaultState {
    pub version: i64,
    pub default_sha256: String,
}

/// A skill to teach. `sha256` is the caller's digest of `content` — computed
/// once by the caller because it is also what the response reports.
#[derive(Debug, Clone)]
pub struct TaughtSkill {
    pub tenant: TenantId,
    pub name: String,
    pub content: String,
    pub sha256: String,
    pub updated_by: Uuid,
}

/// A setting to write. `user` is `Some` for a user-scoped row and `None` for a
/// tenant-scoped one — the same distinction the `scope` column records, kept as
/// one value so the two can never disagree.
#[derive(Debug, Clone)]
pub struct SettingWrite {
    pub tenant: TenantId,
    pub scope: String,
    pub user: Option<UserId>,
    pub key: String,
    pub value: serde_json::Value,
}

// ── operator console ────────────────────────────────────────────────────────

#[async_trait]
pub trait OperatorRepository: Send + Sync {
    // ── orgs ────────────────────────────────────────────────────────────────

    /// Every org, with its tenant count, by name.
    async fn orgs(&self) -> ApiResult<Vec<OperatorOrg>>;

    async fn create_org(&self, name: &str, slug: &str) -> ApiResult<OperatorOrg>;

    /// Rename, returning `None` when there is no such org so the caller can 404
    /// rather than report a rename that did not happen.
    async fn rename_org(&self, id: Uuid, name: &str) -> ApiResult<Option<OperatorOrg>>;

    /// A tenant's current org and slug. The org is `Option` because a tenant
    /// need not belong to one; the caller authorizes against *both* ends of a
    /// move, so it needs the "from" before it can decide.
    async fn tenant_org_and_slug(
        &self,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, String)>>;

    async fn move_tenant_to_org(&self, tenant: TenantId, org: Uuid) -> ApiResult<()>;

    // ── the console's four lists ────────────────────────────────────────────
    //
    // Each is keyset-paginated on a UUID v7 id walked `id DESC`, and each takes
    // the same optional case-insensitive search. What differs is which columns
    // the search reaches — named per method rather than passed in, so no caller
    // can widen a surface's search into a column it must not expose.

    /// The operator audit trail: operator, rbac, node and user events only.
    ///
    /// Kinds, actors and times — never payloads, which can carry a branch name
    /// or a task title this surface must not hand over.
    async fn audit_page(
        &self,
        q: Option<String>,
        page: Keyset,
    ) -> ApiResult<Vec<OperatorAuditEntry>>;

    /// Tenants at minimum visibility: that it exists, and its member, node,
    /// active-session and workspace counts. The policy-gated fields
    /// (`repositories`, `task_titles`) are NOT selected here — the caller adds
    /// them per opted-in org, so a missed filter cannot leak them.
    async fn tenants_page(&self, q: Option<String>, page: Keyset)
        -> ApiResult<Vec<OperatorTenant>>;

    async fn nodes_page(&self, q: Option<String>, page: Keyset) -> ApiResult<Vec<OperatorNode>>;

    async fn bindings_page(&self, q: Option<String>, page: Keyset) -> ApiResult<Vec<BindingRow>>;

    // ── role bindings ───────────────────────────────────────────────────────

    /// Grant a deployment-scoped role, idempotently — granting twice is not an
    /// error and does not duplicate the binding.
    async fn grant_deployment_role(
        &self,
        subject: Uuid,
        role: &str,
        granted_by: Uuid,
    ) -> ApiResult<()>;

    async fn revoke_deployment_role(&self, subject: Uuid, role: &str) -> ApiResult<()>;
}

pub struct DbOperatorRepository {
    db: DbPool,
}

impl DbOperatorRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

/// `%term%` for a `LIKE`/`ILIKE` bound at `$2`, the shape every console search
/// uses. Kept as one expression so a surface cannot accidentally anchor its
/// search differently from its siblings.
const SEARCH_PATTERN: &str = "'%' || $2 || '%'";

#[async_trait]
impl OperatorRepository for DbOperatorRepository {
    async fn orgs(&self) -> ApiResult<Vec<OperatorOrg>> {
        Ok(self
            .db
            .query_all(
                "SELECT o.id, o.name, o.slug, o.created_at,
                (SELECT count(*) FROM tenants t WHERE t.org_id = o.id) AS tenants
         FROM orgs o ORDER BY o.name",
                params![],
            )
            .await?)
    }

    async fn create_org(&self, name: &str, slug: &str) -> ApiResult<OperatorOrg> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "INSERT INTO orgs (id, name, slug) VALUES ($1, $2, $3)
         RETURNING id, name, slug, created_at, {} AS tenants",
                    Postgres.cast("0", "bigint")
                ),
                params![Uuid::now_v7(), name, slug],
            )
            .await?)
    }

    async fn rename_org(&self, id: Uuid, name: &str) -> ApiResult<Option<OperatorOrg>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE orgs SET name = $2, updated_at = {} WHERE id = $1
         RETURNING id, name, slug, created_at,
                   (SELECT count(*) FROM tenants t WHERE t.org_id = orgs.id) AS tenants",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, name],
            )
            .await?)
    }

    async fn tenant_org_and_slug(
        &self,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, String)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT org_id, slug FROM tenants WHERE id = $1",
                params![tenant],
            )
            .await?)
    }

    async fn move_tenant_to_org(&self, tenant: TenantId, org: Uuid) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE tenants SET org_id = $2, updated_at = {} WHERE id = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![tenant, org],
            )
            .await?;
        Ok(())
    }

    async fn audit_page(
        &self,
        q: Option<String>,
        page: Keyset,
    ) -> ApiResult<Vec<OperatorAuditEntry>> {
        let term = Postgres.cast("$2", "text");
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT e.id, e.kind, e.actor_type, e.actor_id, e.tenant_id,
                t.slug AS tenant_slug, e.occurred_at
         FROM events e JOIN tenants t ON t.id = e.tenant_id
         WHERE (e.kind LIKE 'operator.%' OR e.kind LIKE 'rbac.%'
                OR e.kind LIKE 'node.%'  OR e.kind LIKE 'user.%')
           AND ({term} IS NULL OR (
                    {m_kind}
                 OR {m_slug}
                 OR {m_atype}
                 OR {m_aid}))
           AND ({cursor} IS NULL OR e.id < $3)
         ORDER BY e.id DESC
         LIMIT $1",
                    cursor = Postgres.cast("$3", "uuid"),
                    m_kind = Postgres.ci_match("e.kind", SEARCH_PATTERN),
                    m_slug = Postgres.ci_match("t.slug", SEARCH_PATTERN),
                    m_atype = Postgres.ci_match("e.actor_type", SEARCH_PATTERN),
                    m_aid = Postgres.ci_match(&Postgres.cast("e.actor_id", "text"), SEARCH_PATTERN)
                ),
                params![page.limit, q, page.after],
            )
            .await?)
    }

    async fn tenants_page(
        &self,
        q: Option<String>,
        page: Keyset,
    ) -> ApiResult<Vec<OperatorTenant>> {
        let term = Postgres.cast("$2", "text");
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT t.id, t.slug, t.org_id, t.created_at,
                (SELECT count(*) FROM users u WHERE u.tenant_id = t.id)    AS members,
                (SELECT count(*) FROM nodes n WHERE n.tenant_id = t.id)    AS nodes,
                (SELECT count(*) FROM sessions s
                  WHERE s.tenant_id = t.id
                    AND s.status IN ('starting','running','detached'))     AS active_sessions,
                (SELECT count(*) FROM workspaces w WHERE w.tenant_id = t.id) AS workspaces
         FROM tenants t
         WHERE ({term} IS NULL OR {m_slug} OR {m_name})
           AND ({cursor} IS NULL OR t.id < $3)
         ORDER BY t.id DESC
         LIMIT $1",
                    cursor = Postgres.cast("$3", "uuid"),
                    m_slug = Postgres.ci_match("t.slug", SEARCH_PATTERN),
                    m_name = Postgres.ci_match("t.name", SEARCH_PATTERN)
                ),
                params![page.limit, q, page.after],
            )
            .await?)
    }

    async fn nodes_page(&self, q: Option<String>, page: Keyset) -> ApiResult<Vec<OperatorNode>> {
        let term = Postgres.cast("$2", "text");
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT n.id, n.name, n.platform, n.status, n.last_seen_at, n.resources,
                n.tenant_id, t.slug AS tenant_slug,
                (SELECT count(*) FROM sessions s
                  WHERE s.node_id = n.id
                    AND s.status IN ('starting','running','detached')) AS active_sessions
         FROM nodes n JOIN tenants t ON t.id = n.tenant_id
         WHERE ({term} IS NULL OR (
                    {m_name}
                 OR {m_slug}
                 OR {m_platform}
                 OR {m_status}))
           AND ({cursor} IS NULL OR n.id < $3)
         ORDER BY n.id DESC
         LIMIT $1",
                    cursor = Postgres.cast("$3", "uuid"),
                    m_name = Postgres.ci_match("n.name", SEARCH_PATTERN),
                    m_slug = Postgres.ci_match("t.slug", SEARCH_PATTERN),
                    m_platform = Postgres.ci_match("n.platform", SEARCH_PATTERN),
                    m_status = Postgres.ci_match("n.status", SEARCH_PATTERN)
                ),
                params![page.limit, q, page.after],
            )
            .await?)
    }

    async fn bindings_page(&self, q: Option<String>, page: Keyset) -> ApiResult<Vec<BindingRow>> {
        let term = Postgres.cast("$2", "text");
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT b.id, u.email, u.display_name, b.role_key, b.scope_type, b.scope_id,
                COALESCE(o.slug, t.slug) AS scope_label, b.created_at
         FROM role_bindings b
         JOIN users u ON u.id = b.subject_id
         LEFT JOIN orgs o    ON b.scope_type = 'org'    AND o.id = b.scope_id
         LEFT JOIN tenants t ON b.scope_type = 'tenant' AND t.id = b.scope_id
         WHERE ({term} IS NULL OR (
                    {m_email}
                 OR {m_role}
                 OR {m_scope}
                 OR {m_label}))
           AND ({cursor} IS NULL OR b.id < $3)
         ORDER BY b.id DESC
         LIMIT $1",
                    cursor = Postgres.cast("$3", "uuid"),
                    m_email = Postgres.ci_match("u.email", SEARCH_PATTERN),
                    m_role = Postgres.ci_match("b.role_key", SEARCH_PATTERN),
                    m_scope = Postgres.ci_match("b.scope_type", SEARCH_PATTERN),
                    m_label = Postgres.ci_match("COALESCE(o.slug, t.slug)", SEARCH_PATTERN)
                ),
                params![page.limit, q, page.after],
            )
            .await?)
    }

    async fn grant_deployment_role(
        &self,
        subject: Uuid,
        role: &str,
        granted_by: Uuid,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO role_bindings (id, subject_type, subject_id, role_key, scope_type, scope_id, created_by)
             VALUES ($1, 'user', $2, $3, 'deployment', NULL, $4)
             ON CONFLICT DO NOTHING",
                params![Uuid::now_v7(), subject, role, granted_by],
            )
            .await?;
        Ok(())
    }

    async fn revoke_deployment_role(&self, subject: Uuid, role: &str) -> ApiResult<()> {
        self.db
            .exec(
                "DELETE FROM role_bindings
             WHERE subject_id = $1 AND role_key = $2 AND scope_type = 'deployment'",
                params![subject, role],
            )
            .await?;
        Ok(())
    }
}

// ── managed content ─────────────────────────────────────────────────────────

#[async_trait]
pub trait ManagedContentRepository: Send + Sync {
    /// The installed version and the default it was installed from, or `None`
    /// if this row has never been seeded. The seeder's whole decision rests on
    /// these two values, so they are read together.
    async fn default_state(&self, kind: &str, name: &str)
        -> ApiResult<Option<ManagedDefaultState>>;

    /// First install: version 1, content equal to the default.
    async fn install_default(
        &self,
        kind: &str,
        name: &str,
        content: &str,
        default_sha: &str,
    ) -> ApiResult<()>;

    /// The shipped default advanced: overwrite the row with it and bump the
    /// version. The one path that discards a stored edit, on purpose — a newer
    /// default is meant to win.
    async fn refresh_to_default(
        &self,
        kind: &str,
        name: &str,
        content: &str,
        default_sha: &str,
        next_version: i64,
    ) -> ApiResult<()>;

    /// Every row of a kind, by name — what the read endpoints render.
    async fn list_kind(&self, kind: &str) -> ApiResult<Vec<ManagedContent>>;

    async fn get(&self, kind: &str, name: &str) -> ApiResult<Option<ManagedContent>>;

    /// The apply-shaped projection: just what a node needs to write the file.
    async fn payloads_of_kind(&self, kind: &str) -> ApiResult<Vec<ManagedPayload>>;

    async fn payload(&self, kind: &str, name: &str) -> ApiResult<Option<ManagedPayload>>;
}

pub struct DbManagedContentRepository {
    db: DbPool,
}

impl DbManagedContentRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

/// The columns the API type needs. `default_sha256` is deliberately absent: it
/// is seeder bookkeeping, not something a caller renders.
const MANAGED_COLS: &str = "id, kind, name, content, sha256, version, updated_at";

#[async_trait]
impl ManagedContentRepository for DbManagedContentRepository {
    async fn default_state(
        &self,
        kind: &str,
        name: &str,
    ) -> ApiResult<Option<ManagedDefaultState>> {
        let row: Option<(i64, String)> = self
            .db
            .query_opt(
                "SELECT version, default_sha256 FROM managed_content WHERE kind = $1 AND name = $2",
                params![kind, name],
            )
            .await?;
        Ok(row.map(|(version, default_sha256)| ManagedDefaultState {
            version,
            default_sha256,
        }))
    }

    async fn install_default(
        &self,
        kind: &str,
        name: &str,
        content: &str,
        default_sha: &str,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO managed_content
                   (id, kind, name, content, sha256, version, default_sha256)
                 VALUES ($1, $2, $3, $4, $5, 1, $5)
                 ON CONFLICT (kind, name) DO NOTHING",
                params![Uuid::now_v7(), kind, name, content, default_sha],
            )
            .await?;
        Ok(())
    }

    async fn refresh_to_default(
        &self,
        kind: &str,
        name: &str,
        content: &str,
        default_sha: &str,
        next_version: i64,
    ) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE managed_content
                    SET content = $3, sha256 = $4, version = $5,
                        default_sha256 = $4, updated_at = {}
                  WHERE kind = $1 AND name = $2",
                    // Engine-selected (MAIN-196). This branch only runs on an
                    // UPGRADE — a shipped default whose sha moved — which is
                    // why a fresh boot never reached it and the first pass
                    // missed it. On SQLite `now()` is a syntax error, so the
                    // second boot of a single-machine deployment after any
                    // change to a managed skill would have died here.
                    nook_db::dialect::type_mapping(self.db.engine()).now()
                ),
                params![kind, name, content, default_sha, next_version],
            )
            .await?;
        Ok(())
    }

    async fn list_kind(&self, kind: &str) -> ApiResult<Vec<ManagedContent>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {MANAGED_COLS} FROM managed_content WHERE kind = $1 ORDER BY name"
                ),
                params![kind],
            )
            .await?)
    }

    async fn get(&self, kind: &str, name: &str) -> ApiResult<Option<ManagedContent>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "SELECT {MANAGED_COLS} FROM managed_content WHERE kind = $1 AND name = $2"
                ),
                params![kind, name],
            )
            .await?)
    }

    async fn payloads_of_kind(&self, kind: &str) -> ApiResult<Vec<ManagedPayload>> {
        let rows: Vec<(String, String, String)> = self
            .db
            .query_all(
                "SELECT name, content, sha256 FROM managed_content WHERE kind = $1 ORDER BY name",
                params![kind],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(name, content, sha256)| ManagedPayload {
                name,
                content,
                sha256,
            })
            .collect())
    }

    async fn payload(&self, kind: &str, name: &str) -> ApiResult<Option<ManagedPayload>> {
        let row: Option<(String, String)> = self
            .db
            .query_opt(
                "SELECT content, sha256 FROM managed_content WHERE kind = $1 AND name = $2",
                params![kind, name],
            )
            .await?;
        Ok(row.map(|(content, sha256)| ManagedPayload {
            name: name.to_string(),
            content,
            sha256,
        }))
    }
}

// ── taught skills ───────────────────────────────────────────────────────────

#[async_trait]
pub trait SkillRepository: Send + Sync {
    /// The tenant's skills with their sizes and who last touched them — no
    /// content, which is what makes this the list rather than a bulk read.
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<SkillSummary>>;

    async fn get(&self, tenant: TenantId, name: &str) -> ApiResult<Option<Skill>>;

    /// Teach or re-teach. Re-teaching the same name replaces it, because "I
    /// improved the skill, push it everywhere" is the common case and it has to
    /// be one verb.
    async fn teach(&self, skill: TaughtSkill) -> ApiResult<SkillSummary>;

    /// Returns the number of rows removed, so the caller can 404 on zero.
    async fn forget(&self, tenant: TenantId, name: &str) -> ApiResult<u64>;

    /// Everything this tenant knows, in the shape a freshly-connected node
    /// applies. The node skips writes whose sha it already has.
    async fn payloads_for(&self, tenant: TenantId) -> ApiResult<Vec<ManagedPayload>>;
}

pub struct DbSkillRepository {
    db: DbPool,
}

impl DbSkillRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SkillRepository for DbSkillRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<SkillSummary>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT s.id, s.name, s.sha256, {} AS size, s.updated_at,
                u.display_name AS updated_by
         FROM skills s
         LEFT JOIN users u ON u.id = s.updated_by
         WHERE s.tenant_id = $1 ORDER BY s.name",
                    Postgres.cast("length(s.content)", "bigint")
                ),
                params![tenant],
            )
            .await?)
    }

    async fn get(&self, tenant: TenantId, name: &str) -> ApiResult<Option<Skill>> {
        Ok(self
            .db
            .query_opt(
                "SELECT id, tenant_id, name, content, sha256, updated_at, updated_by
         FROM skills WHERE tenant_id = $1 AND name = $2",
                params![tenant, name],
            )
            .await?)
    }

    async fn teach(&self, skill: TaughtSkill) -> ApiResult<SkillSummary> {
        Ok(self
            .db
            .query_one(
                &format!(
                    "INSERT INTO skills (id, tenant_id, name, content, sha256, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, name) DO UPDATE
           SET content = EXCLUDED.content,
               sha256 = EXCLUDED.sha256,
               updated_at = {now},
               updated_by = EXCLUDED.updated_by
         RETURNING id, name, sha256, {size} AS size, updated_at,
           (SELECT display_name FROM users WHERE id = $6) AS updated_by",
                    now = type_mapping(self.db.engine()).now(),
                    size = Postgres.cast("length(content)", "bigint"),
                ),
                params![
                    Uuid::now_v7(),
                    skill.tenant,
                    &skill.name,
                    &skill.content,
                    &skill.sha256,
                    skill.updated_by
                ],
            )
            .await?)
    }

    async fn forget(&self, tenant: TenantId, name: &str) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM skills WHERE tenant_id = $1 AND name = $2",
                params![tenant, name],
            )
            .await?)
    }

    async fn payloads_for(&self, tenant: TenantId) -> ApiResult<Vec<ManagedPayload>> {
        let rows: Vec<(String, String, String)> = self
            .db
            .query_all(
                "SELECT name, content, sha256 FROM skills WHERE tenant_id = $1",
                params![tenant],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(name, content, sha256)| ManagedPayload {
                name,
                content,
                sha256,
            })
            .collect())
    }
}

// ── settings ────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SettingRepository: Send + Sync {
    /// The tenant's settings plus this caller's user-scoped ones — never
    /// another user's, which is why the reader is a parameter and not implied.
    async fn visible_to(&self, tenant: TenantId, user: UserId) -> ApiResult<Vec<Setting>>;

    /// Upsert on `(tenant, scope, user, key)`.
    async fn put(&self, write: SettingWrite) -> ApiResult<Setting>;
}

pub struct DbSettingRepository {
    db: DbPool,
}

impl DbSettingRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SettingRepository for DbSettingRepository {
    async fn visible_to(&self, tenant: TenantId, user: UserId) -> ApiResult<Vec<Setting>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM settings
         WHERE tenant_id = $1 AND (scope = 'tenant' OR (scope = 'user' AND user_id = $2))
         ORDER BY key",
                params![tenant, user],
            )
            .await?)
    }

    async fn put(&self, write: SettingWrite) -> ApiResult<Setting> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO settings (id, tenant_id, scope, user_id, key, value)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, scope, user_id, key)
         DO UPDATE SET value = EXCLUDED.value
         RETURNING *",
                params![
                    SettingId::new(),
                    write.tenant,
                    &write.scope,
                    write.user.map(|x| x.0),
                    &write.key,
                    &write.value
                ],
            )
            .await?)
    }
}

// ── themes ──────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ThemeRepository: Send + Sync {
    /// The built-ins (`tenant_id IS NULL`) plus this tenant's own.
    async fn visible_to(&self, tenant: TenantId) -> ApiResult<Vec<Theme>>;

    /// One theme by slug, scoped the same way — another tenant's theme is
    /// simply not found.
    async fn by_slug(&self, slug: &str, tenant: TenantId) -> ApiResult<Option<Theme>>;
}

pub struct DbThemeRepository {
    db: DbPool,
}

impl DbThemeRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ThemeRepository for DbThemeRepository {
    async fn visible_to(&self, tenant: TenantId) -> ApiResult<Vec<Theme>> {
        Ok(self
            .db
            .query_all(
                "SELECT * FROM themes WHERE tenant_id IS NULL OR tenant_id = $1 ORDER BY name",
                params![tenant],
            )
            .await?)
    }

    async fn by_slug(&self, slug: &str, tenant: TenantId) -> ApiResult<Option<Theme>> {
        Ok(self
            .db
            .query_opt(
                "SELECT * FROM themes WHERE slug = $1 AND (tenant_id IS NULL OR tenant_id = $2)",
                params![slug, tenant],
            )
            .await?)
    }
}

// ── in-memory fakes (AC-3) ──────────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting, which for this
// aggregate means the rules that are easy to get wrong and invisible when they
// are: the seeder's three-way decision on a shipped default, the theme
// catalogue's "built-ins plus mine", the settings read's "mine, never another
// user's", and the keyset walk every console list shares.

use std::sync::Mutex;

/// Newest-first on id, then the cursor and limit — the `ORDER BY id DESC`
/// keyset the four console lists share, applied once so a fake cannot disagree
/// with its siblings about what a page is.
fn keyset_page<T, F>(mut rows: Vec<T>, page: Keyset, id_of: F) -> Vec<T>
where
    F: Fn(&T) -> Uuid,
{
    rows.sort_by_key(|r| std::cmp::Reverse(id_of(r)));
    rows.into_iter()
        .filter(|r| page.after.is_none_or(|after| id_of(r) < after))
        .take(page.limit.max(0) as usize)
        .collect()
}

/// Case-insensitive substring, the shape `SEARCH_PATTERN` produces.
fn matches(haystack: &str, needle: &Option<String>) -> bool {
    needle
        .as_ref()
        .is_none_or(|n| haystack.to_lowercase().contains(&n.to_lowercase()))
}

#[derive(Default)]
struct FakeOperatorState {
    orgs: Vec<OperatorOrg>,
    /// `tenant -> (org, slug)`.
    tenants: Vec<(TenantId, Option<Uuid>, String)>,
    tenant_rows: Vec<OperatorTenant>,
    node_rows: Vec<OperatorNode>,
    binding_rows: Vec<BindingRow>,
    audit_rows: Vec<OperatorAuditEntry>,
    /// `(subject, role)` — deployment-scoped bindings only, which is all this
    /// trait grants.
    deployment_roles: Vec<(Uuid, String)>,
}

#[derive(Default)]
pub struct FakeOperatorRepository {
    inner: Mutex<FakeOperatorState>,
}

impl FakeOperatorRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_org(&self, id: Uuid, name: &str, slug: &str) {
        self.inner.lock().unwrap().orgs.push(OperatorOrg {
            id,
            name: name.to_string(),
            slug: slug.to_string(),
            created_at: chrono::Utc::now(),
            tenants: 0,
        });
    }

    pub fn add_tenant(&self, id: TenantId, org: Option<Uuid>, slug: &str) {
        self.inner
            .lock()
            .unwrap()
            .tenants
            .push((id, org, slug.to_string()));
    }

    pub fn add_tenant_row(&self, row: OperatorTenant) {
        self.inner.lock().unwrap().tenant_rows.push(row);
    }

    pub fn add_node_row(&self, row: OperatorNode) {
        self.inner.lock().unwrap().node_rows.push(row);
    }

    pub fn add_binding_row(&self, row: BindingRow) {
        self.inner.lock().unwrap().binding_rows.push(row);
    }

    pub fn add_audit_row(&self, row: OperatorAuditEntry) {
        self.inner.lock().unwrap().audit_rows.push(row);
    }

    /// The deployment roles held, so a test can assert a grant landed without
    /// reading it back through a list.
    pub fn roles_of(&self, subject: Uuid) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .deployment_roles
            .iter()
            .filter(|(s, _)| *s == subject)
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// Which org a tenant sits in now — how a test sees a move.
    pub fn org_of(&self, tenant: TenantId) -> Option<Uuid> {
        self.inner
            .lock()
            .unwrap()
            .tenants
            .iter()
            .find(|(t, _, _)| *t == tenant)
            .and_then(|(_, org, _)| *org)
    }
}

#[async_trait]
impl OperatorRepository for FakeOperatorRepository {
    async fn orgs(&self) -> ApiResult<Vec<OperatorOrg>> {
        let st = self.inner.lock().unwrap();
        let mut orgs = st.orgs.clone();
        for o in &mut orgs {
            o.tenants = st
                .tenants
                .iter()
                .filter(|(_, org, _)| *org == Some(o.id))
                .count() as i64;
        }
        orgs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(orgs)
    }

    async fn create_org(&self, name: &str, slug: &str) -> ApiResult<OperatorOrg> {
        let mut st = self.inner.lock().unwrap();
        let org = OperatorOrg {
            id: Uuid::now_v7(),
            name: name.to_string(),
            slug: slug.to_string(),
            created_at: chrono::Utc::now(),
            tenants: 0,
        };
        st.orgs.push(org.clone());
        Ok(org)
    }

    async fn rename_org(&self, id: Uuid, name: &str) -> ApiResult<Option<OperatorOrg>> {
        let mut st = self.inner.lock().unwrap();
        let tenants = st
            .tenants
            .iter()
            .filter(|(_, org, _)| *org == Some(id))
            .count() as i64;
        let Some(org) = st.orgs.iter_mut().find(|o| o.id == id) else {
            return Ok(None);
        };
        org.name = name.to_string();
        org.tenants = tenants;
        Ok(Some(org.clone()))
    }

    async fn tenant_org_and_slug(
        &self,
        tenant: TenantId,
    ) -> ApiResult<Option<(Option<Uuid>, String)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .tenants
            .iter()
            .find(|(t, _, _)| *t == tenant)
            .map(|(_, org, slug)| (*org, slug.clone())))
    }

    async fn move_tenant_to_org(&self, tenant: TenantId, org: Uuid) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        if let Some(row) = st.tenants.iter_mut().find(|(t, _, _)| *t == tenant) {
            row.1 = Some(org);
        }
        Ok(())
    }

    async fn audit_page(
        &self,
        q: Option<String>,
        page: Keyset,
    ) -> ApiResult<Vec<OperatorAuditEntry>> {
        let st = self.inner.lock().unwrap();
        let rows: Vec<OperatorAuditEntry> = st
            .audit_rows
            .iter()
            .filter(|r| {
                matches(&r.kind, &q)
                    || matches(&r.tenant_slug, &q)
                    || matches(r.actor_type.as_deref().unwrap_or(""), &q)
            })
            .cloned()
            .collect();
        Ok(keyset_page(rows, page, |r| r.id.0))
    }

    async fn tenants_page(
        &self,
        q: Option<String>,
        page: Keyset,
    ) -> ApiResult<Vec<OperatorTenant>> {
        let st = self.inner.lock().unwrap();
        let rows: Vec<OperatorTenant> = st
            .tenant_rows
            .iter()
            .filter(|r| matches(&r.slug, &q))
            .cloned()
            .collect();
        Ok(keyset_page(rows, page, |r| r.id.0))
    }

    async fn nodes_page(&self, q: Option<String>, page: Keyset) -> ApiResult<Vec<OperatorNode>> {
        let st = self.inner.lock().unwrap();
        let rows: Vec<OperatorNode> = st
            .node_rows
            .iter()
            .filter(|r| {
                matches(&r.name, &q) || matches(&r.tenant_slug, &q) || matches(&r.status, &q)
            })
            .cloned()
            .collect();
        Ok(keyset_page(rows, page, |r| r.id.0))
    }

    async fn bindings_page(&self, q: Option<String>, page: Keyset) -> ApiResult<Vec<BindingRow>> {
        let st = self.inner.lock().unwrap();
        let rows: Vec<BindingRow> = st
            .binding_rows
            .iter()
            .filter(|r| matches(&r.email, &q) || matches(&r.role_key, &q))
            .cloned()
            .collect();
        Ok(keyset_page(rows, page, |r| r.id))
    }

    async fn grant_deployment_role(
        &self,
        subject: Uuid,
        role: &str,
        _granted_by: Uuid,
    ) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        let key = (subject, role.to_string());
        // ON CONFLICT DO NOTHING: granting twice is not an error and does not
        // duplicate the binding.
        if !st.deployment_roles.contains(&key) {
            st.deployment_roles.push(key);
        }
        Ok(())
    }

    async fn revoke_deployment_role(&self, subject: Uuid, role: &str) -> ApiResult<()> {
        self.inner
            .lock()
            .unwrap()
            .deployment_roles
            .retain(|(s, r)| !(*s == subject && r == role));
        Ok(())
    }
}

/// A managed row as the fake stores it — including `default_sha256`, which the
/// API type omits and the seeder's whole decision rests on.
#[derive(Clone)]
struct FakeManagedRow {
    row: ManagedContent,
    default_sha256: String,
}

#[derive(Default)]
pub struct FakeManagedContentRepository {
    inner: Mutex<Vec<FakeManagedRow>>,
}

impl FakeManagedContentRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stored content and version for a row, so a test can assert what the
    /// seeder did rather than what it returned.
    pub fn stored(&self, kind: &str, name: &str) -> Option<(String, i64)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.row.kind == kind && r.row.name == name)
            .map(|r| (r.row.content.clone(), r.row.version))
    }

    /// Simulate an operator edit: change the content without touching
    /// `default_sha256`, which is the state the "unchanged default" rule has to
    /// preserve.
    pub fn edit(&self, kind: &str, name: &str, content: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(r) = st
            .iter_mut()
            .find(|r| r.row.kind == kind && r.row.name == name)
        {
            r.row.content = content.to_string();
        }
    }
}

#[async_trait]
impl ManagedContentRepository for FakeManagedContentRepository {
    async fn default_state(
        &self,
        kind: &str,
        name: &str,
    ) -> ApiResult<Option<ManagedDefaultState>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.row.kind == kind && r.row.name == name)
            .map(|r| ManagedDefaultState {
                version: r.row.version,
                default_sha256: r.default_sha256.clone(),
            }))
    }

    async fn install_default(
        &self,
        kind: &str,
        name: &str,
        content: &str,
        default_sha: &str,
    ) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        // ON CONFLICT (kind, name) DO NOTHING.
        if st.iter().any(|r| r.row.kind == kind && r.row.name == name) {
            return Ok(());
        }
        st.push(FakeManagedRow {
            row: ManagedContent {
                id: Uuid::now_v7(),
                kind: kind.to_string(),
                name: name.to_string(),
                content: content.to_string(),
                sha256: default_sha.to_string(),
                version: 1,
                updated_at: chrono::Utc::now(),
            },
            default_sha256: default_sha.to_string(),
        });
        Ok(())
    }

    async fn refresh_to_default(
        &self,
        kind: &str,
        name: &str,
        content: &str,
        default_sha: &str,
        next_version: i64,
    ) -> ApiResult<()> {
        let mut st = self.inner.lock().unwrap();
        if let Some(r) = st
            .iter_mut()
            .find(|r| r.row.kind == kind && r.row.name == name)
        {
            r.row.content = content.to_string();
            r.row.sha256 = default_sha.to_string();
            r.row.version = next_version;
            r.default_sha256 = default_sha.to_string();
            r.row.updated_at = chrono::Utc::now();
        }
        Ok(())
    }

    async fn list_kind(&self, kind: &str) -> ApiResult<Vec<ManagedContent>> {
        let st = self.inner.lock().unwrap();
        let mut rows: Vec<ManagedContent> = st
            .iter()
            .filter(|r| r.row.kind == kind)
            .map(|r| r.row.clone())
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows)
    }

    async fn get(&self, kind: &str, name: &str) -> ApiResult<Option<ManagedContent>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.row.kind == kind && r.row.name == name)
            .map(|r| r.row.clone()))
    }

    async fn payloads_of_kind(&self, kind: &str) -> ApiResult<Vec<ManagedPayload>> {
        Ok(self
            .list_kind(kind)
            .await?
            .into_iter()
            .map(|r| ManagedPayload {
                name: r.name,
                content: r.content,
                sha256: r.sha256,
            })
            .collect())
    }

    async fn payload(&self, kind: &str, name: &str) -> ApiResult<Option<ManagedPayload>> {
        Ok(self.get(kind, name).await?.map(|r| ManagedPayload {
            name: r.name,
            content: r.content,
            sha256: r.sha256,
        }))
    }
}

#[derive(Default)]
pub struct FakeSkillRepository {
    inner: Mutex<Vec<Skill>>,
}

impl FakeSkillRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[async_trait]
impl SkillRepository for FakeSkillRepository {
    async fn list(&self, tenant: TenantId) -> ApiResult<Vec<SkillSummary>> {
        let st = self.inner.lock().unwrap();
        let mut rows: Vec<SkillSummary> = st
            .iter()
            .filter(|s| s.tenant_id == tenant)
            .map(|s| SkillSummary {
                id: s.id,
                name: s.name.clone(),
                sha256: s.sha256.clone(),
                size: s.content.len() as i64,
                updated_at: s.updated_at,
                updated_by: None,
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows)
    }

    async fn get(&self, tenant: TenantId, name: &str) -> ApiResult<Option<Skill>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.tenant_id == tenant && s.name == name)
            .cloned())
    }

    async fn teach(&self, skill: TaughtSkill) -> ApiResult<SkillSummary> {
        let mut st = self.inner.lock().unwrap();
        // ON CONFLICT (tenant_id, name) DO UPDATE: re-teaching replaces.
        let existing = st
            .iter()
            .position(|s| s.tenant_id == skill.tenant && s.name == skill.name);
        let row = Skill {
            id: existing.map(|i| st[i].id).unwrap_or_else(Uuid::now_v7),
            tenant_id: skill.tenant,
            name: skill.name.clone(),
            content: skill.content.clone(),
            sha256: skill.sha256.clone(),
            updated_at: chrono::Utc::now(),
            updated_by: Some(skill.updated_by),
        };
        match existing {
            Some(i) => st[i] = row.clone(),
            None => st.push(row.clone()),
        }
        Ok(SkillSummary {
            id: row.id,
            name: row.name,
            sha256: row.sha256,
            size: row.content.len() as i64,
            updated_at: row.updated_at,
            updated_by: None,
        })
    }

    async fn forget(&self, tenant: TenantId, name: &str) -> ApiResult<u64> {
        let mut st = self.inner.lock().unwrap();
        let before = st.len();
        st.retain(|s| !(s.tenant_id == tenant && s.name == name));
        Ok((before - st.len()) as u64)
    }

    async fn payloads_for(&self, tenant: TenantId) -> ApiResult<Vec<ManagedPayload>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.tenant_id == tenant)
            .map(|s| ManagedPayload {
                name: s.name.clone(),
                content: s.content.clone(),
                sha256: s.sha256.clone(),
            })
            .collect())
    }
}

#[derive(Default)]
pub struct FakeSettingRepository {
    inner: Mutex<Vec<Setting>>,
}

impl FakeSettingRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SettingRepository for FakeSettingRepository {
    async fn visible_to(&self, tenant: TenantId, user: UserId) -> ApiResult<Vec<Setting>> {
        let st = self.inner.lock().unwrap();
        let mut rows: Vec<Setting> = st
            .iter()
            .filter(|s| {
                s.tenant_id == tenant
                    && (s.scope == "tenant" || (s.scope == "user" && s.user_id == Some(user)))
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(rows)
    }

    async fn put(&self, write: SettingWrite) -> ApiResult<Setting> {
        let mut st = self.inner.lock().unwrap();
        let user = write.user;
        // The upsert key is the whole tuple, which is what keeps one user's
        // row from overwriting another's under the same key.
        let existing = st.iter().position(|s| {
            s.tenant_id == write.tenant
                && s.scope == write.scope
                && s.user_id == user
                && s.key == write.key
        });
        let row = Setting {
            id: existing.map(|i| st[i].id).unwrap_or_else(SettingId::new),
            tenant_id: write.tenant,
            scope: write.scope,
            user_id: user,
            key: write.key,
            value: write.value,
        };
        match existing {
            Some(i) => st[i] = row.clone(),
            None => st.push(row.clone()),
        }
        Ok(row)
    }
}

#[derive(Default)]
pub struct FakeThemeRepository {
    inner: Mutex<Vec<Theme>>,
}

impl FakeThemeRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// A built-in (`tenant_id` NULL) or a tenant's own.
    pub fn add(&self, slug: &str, name: &str, tenant: Option<TenantId>) {
        self.inner.lock().unwrap().push(Theme {
            id: ThemeId(Uuid::now_v7()),
            tenant_id: tenant,
            name: name.to_string(),
            slug: slug.to_string(),
            tokens: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        });
    }
}

#[async_trait]
impl ThemeRepository for FakeThemeRepository {
    async fn visible_to(&self, tenant: TenantId) -> ApiResult<Vec<Theme>> {
        let st = self.inner.lock().unwrap();
        let mut rows: Vec<Theme> = st
            .iter()
            .filter(|t| t.tenant_id.is_none() || t.tenant_id == Some(tenant))
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows)
    }

    async fn by_slug(&self, slug: &str, tenant: TenantId) -> ApiResult<Option<Theme>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.slug == slug && (t.tenant_id.is_none() || t.tenant_id == Some(tenant)))
            .cloned())
    }
}
