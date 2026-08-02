//! Node, enrolment and CA data access (MAIN-252).
//!
//! Three traits, because these are three lifecycles that happen to meet at the
//! `nodes` table:
//!
//! - [`NodeRepository`] — the fleet itself: identity, ownership and sharing,
//!   the liveness lease, and what a node reports over its socket.
//! - [`JoinTokenRepository`] — single-use enrolment tokens. Separate because a
//!   token's whole life is "issued, then consumed once", and consuming one is
//!   the security boundary of the join flow.
//! - [`TenantCaRepository`] — the per-tenant certificate authority.
//!
//! Two boundary decisions, both deliberate:
//!
//! - **A node's socket writes `sessions`.** When a node reconnects it tells us
//!   which tmux sessions really exist, and which of its sessions started,
//!   exited or failed. Those are session rows, and MAIN-253 owns them — but
//!   they are written *only* from the node socket, in response to a node's own
//!   report, so they sit here until that card reclaims them. They are grouped
//!   under their own heading below and named for the report that drives them,
//!   not for the table.
//! - **The join flow reads `users`.** That is the identity aggregate, which
//!   already has a repository, so it got a tenant-scoped
//!   `person_id_of_in_tenant` rather than a second copy of the query here.
//!
//! Methods are intent-named and coarse; no `sqlx` type appears in any
//! signature, and row mapping lives inside the impls (AC-2).

use async_trait::async_trait;
use nook_db::dialect::{json, type_mapping};
use nook_db::paging::{DbPage, ListSpec, PageArgs};
use nook_db::{params, Db, DbPool};
use nook_types::*;
use std::collections::HashMap;
use uuid::Uuid;

use crate::ca::TenantCa;

/// A node advertising itself as shared operator substrate. Takes the engine
/// rather than reading one: it is a free function, and the containment test it
/// builds is `jsonb @>` on Postgres and a `json_each` walk on SQLite.
fn shared_operator_clause(engine: nook_db::Engine) -> String {
    json(engine).contains(
        "capabilities",
        &json(engine).literal("{\"shared_operator\":true}"),
    )
}
use crate::error::ApiResult;

/// Who may use a node, as the sharing and authorization checks need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSharing {
    pub owner_person_id: Option<Uuid>,
    pub shared: bool,
}

/// A node's certificate identity, for renewal and revocation checks.
#[derive(Debug, Clone)]
pub struct NodeCertIdentity {
    pub tenant: TenantId,
    pub public_key_pem: Option<String>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A freshly signed leaf, as recorded against the node that holds it.
#[derive(Debug, Clone)]
pub struct IssuedLeaf {
    pub ca_id: Uuid,
    pub not_after: chrono::DateTime<chrono::Utc>,
    pub cert_pem: String,
    pub public_key_pem: String,
}

/// What a node says about itself when it connects.
#[derive(Debug, Clone)]
pub struct ReportedCapabilities {
    pub capabilities: serde_json::Value,
    pub hostname: String,
    pub platform: String,
}

/// A consumed join token: which tenant it enrols into, and who issued it.
///
/// `created_by` is optional because a **legacy token** recorded no minter. That
/// is the case the enrolment path falls back to the tenant owner for, so the
/// nullability is load-bearing rather than incidental.
#[derive(Debug, Clone)]
pub struct ConsumedJoinToken {
    pub id: JoinTokenId,
    pub tenant: TenantId,
    pub created_by: Option<UserId>,
}

/// The identity a node presents on its socket.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    pub id: NodeId,
    pub tenant: TenantId,
    pub name: String,
}

/// A node joining or re-joining. `ON CONFLICT (tenant_id, name)` makes a
/// re-join heal the existing row rather than duplicate the machine.
#[derive(Debug, Clone)]
pub struct JoiningNode {
    pub tenant: TenantId,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub token_hash: String,
    pub owner_person_id: Option<Uuid>,
}

#[async_trait]
pub trait NodeRepository: Send + Sync {
    /// Every node whose instance lease is still live, with the seconds left on
    /// it (MAIN-305). The websocket registry refreshes its lease cache from
    /// this — the one `nodes` read MAIN-252 landed without.
    async fn live_leases(&self) -> ApiResult<Vec<(Uuid, Uuid, f64)>>;

    // ── identity and listing ────────────────────────────────────────────────

    /// Which tenant owns this node, without knowing the tenant first — the
    /// lookup a node-authenticated request needs before it has a scope.
    async fn tenant_of(&self, id: NodeId) -> ApiResult<Option<TenantId>>;

    async fn get(&self, tenant: TenantId, id: NodeId) -> ApiResult<Option<Node>>;

    /// List a tenant's nodes, optionally scoped to a single owner person
    /// (MAIN-132). `owner = Some(person)` returns that person's own nodes PLUS
    /// any node the team has been given — those flagged `shared` (MAIN-135);
    /// `owner = None` returns the whole fleet (owner/admin, and node tokens
    /// whose view is unchanged). Shared grants **visibility only** —
    /// session-start stays owner-only.
    /// The caller's node list: their active tenant's nodes under today's rules
    /// (`owner` scopes a member to their own + shared), UNION the nodes
    /// `own_person` owns in ANY tenant (MAIN-353).
    ///
    /// The second leg is owner-only and person-keyed on purpose: it is the
    /// caller's own machine following the caller's own identity, not a share.
    /// It is passed separately from `owner` because the two are different
    /// questions — an admin's fleet view sets `owner = None` and must NOT
    /// thereby inherit everyone's foreign machines.
    async fn list(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        own_person: Option<Uuid>,
    ) -> ApiResult<Vec<Node>>;

    /// The same rows as [`list`] — same visibility rule, same params — through
    /// the pagination contract: searched (name/hostname/platform/status),
    /// sorted, cursor-walked.
    async fn page(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        own_person: Option<Uuid>,
        args: &PageArgs,
    ) -> ApiResult<DbPage<Node>>;

    /// Every node a tenant's workspaces may be placed on: the tenant's own
    /// nodes, PLUS any node owned by a person who is a member of it, wherever
    /// that node is homed.
    ///
    /// The second half is the point. A person's machines are theirs across every
    /// org they belong to (MAIN-353), so a workspace in a tenant they joined
    /// should reach those machines — otherwise "my own nodes" means "my own
    /// nodes, in one org", and joining a second team silently leaves your
    /// laptops out of it.
    ///
    /// Deliberately NOT a visibility query: this decides where the reconciler
    /// may CLONE and START work, and nothing here is returned to a browser. The
    /// see-path rule (`list`) is unchanged and still hides other people's
    /// machines.
    async fn placement_candidates(&self, tenant: TenantId) -> ApiResult<Vec<Node>>;

    /// A node by id with NO tenant scope, for the two callers that must ask
    /// "is this the caller's own machine?" before they know which tenant it
    /// lives in (MAIN-353). Every caller applies its own visibility rule to the
    /// result; none of them may return it unfiltered.
    async fn by_id_any_tenant(&self, id: NodeId) -> ApiResult<Option<Node>>;

    /// Tenant display names by id — what labels a foreign-home node.
    async fn tenant_names(&self, ids: &[TenantId]) -> ApiResult<HashMap<Uuid, String>>;

    /// Every node's id and name in a tenant, BY NAME. The online filtering that
    /// uses it stays in the caller, because liveness comes from the registry
    /// rather than the database.
    ///
    /// The order is part of the contract: `nook teach` reports which machines
    /// took a skill and which were offline, and an unordered list would shuffle
    /// those two lists between otherwise identical runs.
    async fn list_ids_and_names(&self, tenant: TenantId) -> ApiResult<Vec<(NodeId, String)>>;

    async fn ids_in_tenant(&self, tenant: TenantId) -> ApiResult<Vec<NodeId>>;

    async fn exists_in_tenant(&self, id: NodeId, tenant: TenantId) -> ApiResult<bool>;

    async fn name_of(&self, id: NodeId) -> ApiResult<Option<String>>;

    async fn delete(&self, tenant: TenantId, id: NodeId) -> ApiResult<u64>;

    // ── ownership and sharing ───────────────────────────────────────────────

    async fn sharing(&self, id: NodeId, tenant: TenantId) -> ApiResult<Option<NodeSharing>>;

    /// Sharing plus the capabilities blob, which the authorize view needs in
    /// the same round trip.
    async fn sharing_and_capabilities(
        &self,
        id: NodeId,
        tenant: TenantId,
    ) -> ApiResult<Option<(NodeSharing, serde_json::Value)>>;

    async fn set_shared(
        &self,
        id: NodeId,
        tenant: TenantId,
        shared: bool,
    ) -> ApiResult<Option<Node>>;

    /// Replace a node's operator-set labels and taints (MAIN-314). Both are a
    /// full replacement — a partial update of a set cannot say what was deleted.
    async fn set_placement(
        &self,
        id: NodeId,
        tenant: TenantId,
        labels: serde_json::Value,
        taints: serde_json::Value,
    ) -> ApiResult<Option<Node>>;

    // ── enrolment ───────────────────────────────────────────────────────────

    /// Find a node by the hash of the token it presented. The plaintext token
    /// is never at rest, so the signature says `token_hash`.
    async fn by_token_hash(&self, token_hash: &str) -> ApiResult<Option<NodeIdentity>>;

    /// Join or re-join, returning the node id. A machine that re-joins keeps
    /// its id and its owner (`COALESCE`), so re-running `nook join` never
    /// orphans its checkouts or hands it to a different person.
    async fn upsert_joining(&self, node: JoiningNode) -> ApiResult<NodeId>;

    /// The certificate-enrolment variant: no hostname/platform yet, because the
    /// node has not connected.
    async fn upsert_enrolling(
        &self,
        id: NodeId,
        tenant: TenantId,
        name: &str,
        token_hash: &str,
        owner_person_id: Option<Uuid>,
    ) -> ApiResult<NodeId>;

    // ── certificates ────────────────────────────────────────────────────────

    async fn cert_identity(&self, id: NodeId) -> ApiResult<Option<NodeCertIdentity>>;

    /// Whether a presented certificate's node is revoked, and whose it is.
    async fn revocation_state(
        &self,
        id: NodeId,
    ) -> ApiResult<Option<(TenantId, Option<chrono::DateTime<chrono::Utc>>)>>;

    async fn record_issued_leaf(&self, id: NodeId, leaf: IssuedLeaf) -> ApiResult<()>;

    async fn revoke(&self, id: NodeId, tenant: TenantId) -> ApiResult<u64>;

    /// How many nodes still hold an unexpired leaf signed by this CA — the
    /// retirement guard.
    async fn live_leaf_count(&self, tenant: TenantId, ca_id: Uuid) -> ApiResult<i64>;

    // ── executor selection for loop jobs (MAIN-255) ─────────────────────────
    //
    // These are `nodes` queries, so they live with `nodes`. A second copy under
    // "jobs" is exactly how two definitions of "who may run work" drift apart.

    /// Nodes that may run a loop job of `kind`, best first: the requester's own
    /// online nodes authorized for `runtime`, then the online authorized shared
    /// operator. Own-before-shared is the ORDER BY, not the caller's job.
    ///
    /// A LIST rather than one node (MAIN-142), because the last eligibility
    /// gate — how many jobs a node is already holding — is a `loop_jobs` count,
    /// and putting that here would make this query span two aggregates. Who
    /// *may* run work stays one definition; how busy they are is the caller's.
    ///
    /// Two filters live here and nowhere else. The node's declared
    /// `loop_kinds` must contain `kind`, and a `build` job is never offered to
    /// a shared operator **whatever that node declares** — the wall does not
    /// consult the node's own configuration, which is the point of it.
    async fn eligible_loop_executors(
        &self,
        tenant: TenantId,
        person: Uuid,
        runtime: &str,
        kind: &str,
    ) -> ApiResult<Vec<NodeId>>;

    /// Is this node a shared operator? The wall's own question, asked of the
    /// stored row rather than of anything a caller passes in (MAIN-142 AC-3/AC-4).
    async fn is_shared_operator(&self, id: NodeId) -> ApiResult<bool>;

    /// The loop kinds a node declares, and the cap it reports. `None` capacity
    /// means an older node that never reported one.
    async fn loop_profile(&self, id: NodeId) -> ApiResult<Option<(Vec<String>, Option<u32>)>>;

    /// How many of this person's nodes are online — the first half of phrasing
    /// *why* nothing could be placed.
    async fn owned_online_count(&self, tenant: TenantId, person: Uuid) -> ApiResult<i64>;

    /// How many shared operator nodes are online — the second half.
    async fn shared_operator_online_count(&self, tenant: TenantId) -> ApiResult<i64>;

    /// This person's nodes with their last reported resource sample — the
    /// candidate set placement ranks (MAIN-292). The liveness filter is NOT here:
    /// "online" for scheduling means a live socket in the in-memory registry, not
    /// the `status` column, so the caller applies it.
    async fn owned_with_resources(
        &self,
        tenant: TenantId,
        person: Uuid,
    ) -> ApiResult<Vec<(NodeId, serde_json::Value)>>;
    /// A node by id with no tenant scope — the port broker knows only the node
    /// the session is starting on (MAIN-301). Returns the row for reading, not
    /// as a visibility decision; every caller of the port API gates separately.
    async fn by_id_any_tenant_or_none(&self, id: NodeId) -> ApiResult<Option<Node>>;

    /// Set or clear an operator's port range for a node (MAIN-301). Both `None`
    /// clears it, falling back to whatever the node advertises.
    async fn set_port_range(
        &self,
        id: NodeId,
        tenant: TenantId,
        start: Option<i32>,
        end: Option<i32>,
    ) -> ApiResult<Option<Node>>;

    // ── the liveness lease and what the socket reports ──────────────────────

    /// Claim this node for this control-plane instance for `lease_seconds`.
    async fn take_lease(&self, id: NodeId, instance: Uuid, lease_seconds: f64) -> ApiResult<()>;

    /// Mark offline and drop the lease — but only if we still hold it, so a
    /// disconnect racing another instance's takeover cannot clear its claim.
    async fn release_lease(&self, id: NodeId, instance: Uuid) -> ApiResult<()>;

    async fn record_capabilities(
        &self,
        id: NodeId,
        reported: ReportedCapabilities,
    ) -> ApiResult<()>;

    /// A heartbeat: fresh resources, and the lease extended only if it is still
    /// ours.
    async fn record_resources(
        &self,
        id: NodeId,
        resources: &serde_json::Value,
        instance: Uuid,
        lease_seconds: f64,
    ) -> ApiResult<()>;

    /// Merge re-probed runtime auth profiles into the stored capabilities
    /// (MAIN-126 AC-4). Only the static path is in the SQL; the node-supplied
    /// value is bound (MAIN-201's json seam).
    async fn merge_runtime_auth(&self, id: NodeId, profiles: &serde_json::Value) -> ApiResult<()>;

    // ── sessions, as reported by the node (MAIN-253 reclaims these) ─────────
    //
    // Written only from the node socket, in response to the node's own report
    // about its own sessions. They are named for that report.

    /// A reconnecting node lists the tmux sessions it really has; every session
    /// we still believe is live and is NOT in that list has died with the
    /// machine, and is marked exited.
    async fn expire_sessions_missing_from_tmux(
        &self,
        node: NodeId,
        live_tmux_sessions: &[String],
    ) -> ApiResult<u64>;

    /// Node-reported session lifecycle, scoped by the NODE rather than by the
    /// node's home tenant (MAIN-363).
    ///
    /// Cross-tenant placement (MAIN-353) means a session on this machine can
    /// belong to any tenant its owner is a member of, while the node's socket
    /// authenticates as its HOME tenant. Scoping these by that tenant matched
    /// zero rows for exactly those sessions: `tmux_session` was never recorded,
    /// so every viewer got "session has no terminal yet", the reaper ended the
    /// row, and the reconciler started another sixty seconds later — forever.
    /// `node_id` is the stronger guard anyway: a node may report on the
    /// sessions it is running and on nothing else.
    async fn mark_session_running(
        &self,
        session: SessionId,
        node: NodeId,
        tmux_session: &str,
    ) -> ApiResult<u64>;

    async fn mark_session_exited(&self, session: SessionId, node: NodeId) -> ApiResult<u64>;

    async fn mark_session_failed(
        &self,
        session: SessionId,
        node: NodeId,
        message: &str,
    ) -> ApiResult<u64>;
}

#[async_trait]
pub trait JoinTokenRepository: Send + Sync {
    async fn issue(
        &self,
        tenant: TenantId,
        token_hash: &str,
        created_by: UserId,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> ApiResult<()>;

    /// Spend a token: marks it used and hands back what it enrols into, in one
    /// statement. `None` means absent, expired **or** already used — a caller
    /// must not be able to tell those apart, and one statement is also what
    /// makes two racing joins unable to both win.
    async fn consume(&self, token_hash: &str) -> ApiResult<Option<ConsumedJoinToken>>;
}

#[async_trait]
pub trait TenantCaRepository: Send + Sync {
    /// Insert a CA. The private key arrives already sealed — this trait never
    /// sees plaintext key material.
    async fn insert(
        &self,
        tenant: TenantId,
        state: &str,
        cert_pem: &str,
        key_enc: Vec<u8>,
        fingerprint: &str,
        not_after: chrono::DateTime<chrono::Utc>,
    ) -> ApiResult<TenantCa>;

    /// Every CA this tenant trusts, in any state — what a node must accept.
    async fn bundle(&self, tenant: TenantId) -> ApiResult<Vec<TenantCa>>;

    /// The active signer plus its sealed key, or `None` if the tenant has none.
    async fn active_signer(&self, tenant: TenantId) -> ApiResult<Option<(TenantCa, Vec<u8>)>>;

    /// Promote a staged CA and demote the current one, in one transaction.
    /// Demotion happens first because the partial unique index allows only one
    /// active row. `Ok(false)` means there was no staged CA to promote and
    /// nothing was changed.
    async fn promote(&self, tenant: TenantId, ca_id: Uuid) -> ApiResult<bool>;

    /// Delete a non-active CA. The `state <> 'active'` guard is in the
    /// statement so no caller can retire the signer out from under the fleet.
    async fn retire(&self, ca_id: Uuid, tenant: TenantId) -> ApiResult<u64>;
}

// ── the DbPool implementations ──────────────────────────────────────────────

/// The `nodes` columns every read returns, in one place: the shape `Node`
/// decodes from, and the reason two SELECTs cannot drift apart.
const NODE_COLUMNS: &str = "id, tenant_id, name, hostname, platform, capabilities, resources, \
     status, last_seen_at, owner_person_id, shared, created_at, updated_at, labels, taints, \
     port_range_start, port_range_end";

/// The tenant node list's sort allowlist — the paged endpoint's contract half.
pub const NODE_PAGE_SORTS: &[(&str, &str)] = &[
    ("name", "name"),
    ("status", "status"),
    ("platform", "platform"),
    ("last_seen", "last_seen_at"),
    ("created", "id"),
];

pub struct DbNodeRepository {
    db: DbPool,
}

impl DbNodeRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl NodeRepository for DbNodeRepository {
    async fn live_leases(&self) -> ApiResult<Vec<(Uuid, Uuid, f64)>> {
        let now = type_mapping(self.db.engine()).now();
        let epoch = type_mapping(self.db.engine()).cast(
            &format!("EXTRACT(EPOCH FROM lease_expires_at - {now})"),
            "float8",
        );
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT id, owning_instance_id,
                    {epoch}
             FROM nodes
             WHERE owning_instance_id IS NOT NULL AND lease_expires_at > {now}",
                ),
                params![],
            )
            .await?)
    }

    async fn tenant_of(&self, id: NodeId) -> ApiResult<Option<TenantId>> {
        Ok(self
            .db
            .query_scalar_opt("SELECT tenant_id FROM nodes WHERE id = $1", params![id])
            .await?)
    }

    async fn get(&self, tenant: TenantId, id: NodeId) -> ApiResult<Option<Node>> {
        Ok(self
            .db
            .query_opt(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE tenant_id = $1 AND id = $2"),
                params![tenant, id],
            )
            .await?)
    }

    async fn list(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        own_person: Option<Uuid>,
    ) -> ApiResult<Vec<Node>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {NODE_COLUMNS}
                     FROM nodes
                     WHERE (tenant_id = $1
                            AND ({owner} IS NULL OR owner_person_id = $2 OR shared))
                        OR ({own} IS NOT NULL AND owner_person_id = $3)
                     ORDER BY name",
                    owner = type_mapping(self.db.engine()).cast("$2", "uuid"),
                    own = type_mapping(self.db.engine()).cast("$3", "uuid")
                ),
                params![tenant, owner, own_person],
            )
            .await?)
    }

    async fn page(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        own_person: Option<Uuid>,
        args: &PageArgs,
    ) -> ApiResult<DbPage<Node>> {
        // The visibility WHERE is `list`'s, verbatim — the page must never
        // show a node the whole list would hide. Scope params start at $4.
        let select = format!(
            "SELECT {NODE_COLUMNS}
             FROM nodes
             WHERE (tenant_id = $4
                    AND ({owner} IS NULL OR owner_person_id = $5 OR shared))
                OR ({own} IS NOT NULL AND owner_person_id = $6)",
            owner = type_mapping(self.db.engine()).cast("$5", "uuid"),
            own = type_mapping(self.db.engine()).cast("$6", "uuid")
        );
        Ok(ListSpec {
            select: &select,
            id: "id",
            search: &["name", "hostname", "platform", "status"],
        }
        .fetch(
            &self.db,
            args,
            params![tenant, owner, own_person],
            |n: &Node| n.id.0,
        )
        .await?)
    }

    async fn placement_candidates(&self, tenant: TenantId) -> ApiResult<Vec<Node>> {
        Ok(self
            .db
            .query_all(
                &format!(
                    "SELECT {NODE_COLUMNS}
                     FROM nodes
                     WHERE tenant_id = $1
                        OR owner_person_id IN (
                             SELECT u.person_id FROM users u WHERE u.tenant_id = $1
                           )
                     ORDER BY name"
                ),
                params![tenant],
            )
            .await?)
    }

    async fn by_id_any_tenant(&self, id: NodeId) -> ApiResult<Option<Node>> {
        Ok(self
            .db
            .query_opt(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = $1"),
                params![id],
            )
            .await?)
    }

    async fn tenant_names(&self, ids: &[TenantId]) -> ApiResult<HashMap<Uuid, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let raw: Vec<Uuid> = ids.iter().map(|t| t.0).collect();
        let rows: Vec<(Uuid, String)> = self
            .db
            .query_all(
                "SELECT id, name FROM tenants WHERE id = ANY($1)",
                params![raw],
            )
            .await?;
        Ok(rows.into_iter().collect())
    }

    async fn list_ids_and_names(&self, tenant: TenantId) -> ApiResult<Vec<(NodeId, String)>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, name FROM nodes WHERE tenant_id = $1 ORDER BY name",
                params![tenant],
            )
            .await?)
    }

    async fn ids_in_tenant(&self, tenant: TenantId) -> ApiResult<Vec<NodeId>> {
        Ok(self
            .db
            .query_scalar_all("SELECT id FROM nodes WHERE tenant_id = $1", params![tenant])
            .await?)
    }

    async fn exists_in_tenant(&self, id: NodeId, tenant: TenantId) -> ApiResult<bool> {
        let found: Option<NodeId> = self
            .db
            .query_scalar_opt(
                "SELECT id FROM nodes WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(found.is_some())
    }

    async fn name_of(&self, id: NodeId) -> ApiResult<Option<String>> {
        Ok(self
            .db
            .query_scalar_opt("SELECT name FROM nodes WHERE id = $1", params![id])
            .await?)
    }

    async fn delete(&self, tenant: TenantId, id: NodeId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM nodes WHERE tenant_id = $1 AND id = $2",
                params![tenant, id],
            )
            .await?)
    }

    async fn sharing(&self, id: NodeId, tenant: TenantId) -> ApiResult<Option<NodeSharing>> {
        let row: Option<(Option<Uuid>, bool)> = self
            .db
            .query_opt(
                "SELECT owner_person_id, shared FROM nodes WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(row.map(|(owner_person_id, shared)| NodeSharing {
            owner_person_id,
            shared,
        }))
    }

    async fn sharing_and_capabilities(
        &self,
        id: NodeId,
        tenant: TenantId,
    ) -> ApiResult<Option<(NodeSharing, serde_json::Value)>> {
        let row: Option<(Option<Uuid>, bool, serde_json::Value)> = self
            .db
            .query_opt(
                "SELECT owner_person_id, shared, capabilities
                 FROM nodes WHERE id = $1 AND tenant_id = $2",
                params![id, tenant],
            )
            .await?;
        Ok(row.map(|(owner_person_id, shared, caps)| {
            (
                NodeSharing {
                    owner_person_id,
                    shared,
                },
                caps,
            )
        }))
    }

    async fn set_placement(
        &self,
        id: NodeId,
        tenant: TenantId,
        labels: serde_json::Value,
        taints: serde_json::Value,
    ) -> ApiResult<Option<Node>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE nodes SET labels = $3, taints = $4, updated_at = {}
                     WHERE id = $1 AND tenant_id = $2
                     RETURNING {NODE_COLUMNS}",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, labels, taints],
            )
            .await?)
    }

    async fn set_shared(
        &self,
        id: NodeId,
        tenant: TenantId,
        shared: bool,
    ) -> ApiResult<Option<Node>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE nodes SET shared = $3, updated_at = {}
                     WHERE id = $1 AND tenant_id = $2
                     RETURNING {NODE_COLUMNS}",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, shared],
            )
            .await?)
    }

    async fn by_token_hash(&self, token_hash: &str) -> ApiResult<Option<NodeIdentity>> {
        let row: Option<(NodeId, TenantId, String)> = self
            .db
            .query_opt(
                "SELECT id, tenant_id, name FROM nodes WHERE node_token_hash = $1",
                params![token_hash],
            )
            .await?;
        Ok(row.map(|(id, tenant, name)| NodeIdentity { id, tenant, name }))
    }

    async fn upsert_joining(&self, node: JoiningNode) -> ApiResult<NodeId> {
        Ok(self
            .db
            .query_one::<(NodeId,)>(
                &format!(
                    "INSERT INTO nodes
                       (id, tenant_id, name, hostname, platform, node_token_hash, status,
                        owner_person_id)
                     VALUES ($1, $2, $3, $4, $5, $6, 'offline', $7)
                     ON CONFLICT (tenant_id, name) DO UPDATE SET
                        hostname = EXCLUDED.hostname,
                        platform = EXCLUDED.platform,
                        node_token_hash = EXCLUDED.node_token_hash,
                        owner_person_id = COALESCE(nodes.owner_person_id,
                                                   EXCLUDED.owner_person_id),
                        updated_at = {}
                     RETURNING id",
                    type_mapping(self.db.engine()).now()
                ),
                params![
                    NodeId::new(),
                    node.tenant,
                    node.name,
                    node.hostname,
                    node.platform,
                    node.token_hash,
                    node.owner_person_id
                ],
            )
            .await?
            .0)
    }

    async fn upsert_enrolling(
        &self,
        id: NodeId,
        tenant: TenantId,
        name: &str,
        token_hash: &str,
        owner_person_id: Option<Uuid>,
    ) -> ApiResult<NodeId> {
        Ok(self
            .db
            .query_scalar(
                &format!(
                    "INSERT INTO nodes
                       (id, tenant_id, name, node_token_hash, status, owner_person_id)
                     VALUES ($1, $2, $3, $4, 'offline', $5)
                     ON CONFLICT (tenant_id, name) DO UPDATE SET
                        owner_person_id = COALESCE(nodes.owner_person_id,
                                                   EXCLUDED.owner_person_id),
                        updated_at = {}
                     RETURNING id",
                    type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, name, token_hash, owner_person_id],
            )
            .await?)
    }

    async fn cert_identity(&self, id: NodeId) -> ApiResult<Option<NodeCertIdentity>> {
        let row: Option<(
            TenantId,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        )> = self
            .db
            .query_opt(
                "SELECT tenant_id, public_key_pem, revoked_at FROM nodes WHERE id = $1",
                params![id],
            )
            .await?;
        Ok(
            row.map(|(tenant, public_key_pem, revoked_at)| NodeCertIdentity {
                tenant,
                public_key_pem,
                revoked_at,
            }),
        )
    }

    async fn revocation_state(
        &self,
        id: NodeId,
    ) -> ApiResult<Option<(TenantId, Option<chrono::DateTime<chrono::Utc>>)>> {
        Ok(self
            .db
            .query_opt(
                "SELECT tenant_id, revoked_at FROM nodes WHERE id = $1",
                params![id],
            )
            .await?)
    }

    async fn record_issued_leaf(&self, id: NodeId, leaf: IssuedLeaf) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE nodes SET ca_id = $2, cert_not_after = $3, cert_pem = $4,
                        public_key_pem = $5, updated_at = {}
                     WHERE id = $1",
                    type_mapping(self.db.engine()).now()
                ),
                params![
                    id,
                    leaf.ca_id,
                    leaf.not_after,
                    leaf.cert_pem,
                    leaf.public_key_pem
                ],
            )
            .await?;
        Ok(())
    }

    async fn revoke(&self, id: NodeId, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE nodes SET revoked_at = {now}, updated_at = {now}
                     WHERE id = $1 AND tenant_id = $2",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant],
            )
            .await?)
    }

    async fn live_leaf_count(&self, tenant: TenantId, ca_id: Uuid) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar(
                &format!(
                    "SELECT count(*) FROM nodes
                     WHERE tenant_id = $1 AND ca_id = $2
                       AND revoked_at IS NULL
                       AND cert_not_after IS NOT NULL AND cert_not_after > {now}",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![tenant, ca_id],
            )
            .await?)
    }

    async fn eligible_loop_executors(
        &self,
        tenant: TenantId,
        person: Uuid,
        runtime: &str,
        kind: &str,
    ) -> ApiResult<Vec<NodeId>> {
        // `@>` containment tests the operator flag; the EXISTS scans the
        // reported auth profiles for our runtime. jsonb operators route through
        // the json seam (MAIN-201): the runtime_auth array is expanded and its
        // elements' fields read via the trait, so the Postgres-specific SQL
        // lives here in the impl.
        let runtime_auth = json(self.db.engine()).array_elements(&format!(
            "COALESCE({}, {})",
            json(self.db.engine()).get_json("capabilities", "runtime_auth"),
            json(self.db.engine()).literal("[]")
        ));
        // Containment, not an expanded compare: `jsonb_array_elements` yields
        // JSON scalars, so `k.value::text` on the string "spec" is `"spec"`
        // WITH its quotes and never equals a bound `spec`. `@>` against a
        // JSON-encoded needle is the audited shape and stays parameterized.
        let declares_kind = json(self.db.engine()).contains(
            &format!(
                "COALESCE({}, {})",
                json(self.db.engine()).get_json("capabilities", "loop_kinds"),
                json(self.db.engine()).literal("[]")
            ),
            &type_mapping(self.db.engine()).cast("$5", "jsonb"),
        );
        // The needle, JSON-encoded here rather than in SQL: `to_jsonb(...)` is a
        // Postgres-only spelling and this keeps the bind a plain string.
        let kind_json = serde_json::Value::String(kind.to_string()).to_string();
        // The build wall (AC-3), in the WHERE clause rather than in a caller:
        // a shared operator drops out of the candidate set for a `build` job
        // before anything it declared is even read.
        Ok(self
            .db
            .query_scalar_all(
                &format!(
                    "SELECT id FROM nodes
                     WHERE tenant_id = $1
                       AND status = 'online'
                       AND (owner_person_id = $2 OR {operator})
                       AND NOT ($4 = 'build' AND {operator})
                       AND EXISTS (
                             SELECT 1
                             FROM {runtime_auth} e
                             WHERE {rt} = $3 AND {state} = 'authorized'
                           )
                       AND {declares_kind}
                     ORDER BY (owner_person_id = $2) DESC NULLS LAST, id",
                    operator = shared_operator_clause(self.db.engine()),
                    rt = json(self.db.engine()).get_text("e", "runtime"),
                    state = json(self.db.engine()).get_text("e", "state"),
                ),
                params![tenant, person, runtime, kind, kind_json],
            )
            .await?)
    }

    async fn is_shared_operator(&self, id: NodeId) -> ApiResult<bool> {
        Ok(self
            .db
            .query_scalar_opt::<bool>(
                &format!(
                    "SELECT {} FROM nodes WHERE id = $1",
                    shared_operator_clause(self.db.engine())
                ),
                params![id],
            )
            .await?
            .unwrap_or(false))
    }

    async fn loop_profile(&self, id: NodeId) -> ApiResult<Option<(Vec<String>, Option<u32>)>> {
        let row: Option<(serde_json::Value,)> = self
            .db
            .query_opt("SELECT capabilities FROM nodes WHERE id = $1", params![id])
            .await?;
        Ok(row.map(|(caps,)| {
            let kinds = caps
                .get("loop_kinds")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let cap = caps
                .get("max_loop_jobs")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            (kinds, cap)
        }))
    }

    async fn owned_online_count(&self, tenant: TenantId, person: Uuid) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar::<i64>(
                "SELECT count(*) FROM nodes
                 WHERE tenant_id = $1 AND owner_person_id = $2 AND status = 'online'",
                params![tenant, person],
            )
            .await?)
    }

    async fn owned_with_resources(
        &self,
        tenant: TenantId,
        person: Uuid,
    ) -> ApiResult<Vec<(NodeId, serde_json::Value)>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, resources FROM nodes WHERE tenant_id = $1 AND owner_person_id = $2",
                params![tenant, person],
            )
            .await?)
    }
    async fn by_id_any_tenant_or_none(&self, id: NodeId) -> ApiResult<Option<Node>> {
        Ok(self
            .db
            .query_opt(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = $1"),
                params![id],
            )
            .await?)
    }

    async fn set_port_range(
        &self,
        id: NodeId,
        tenant: TenantId,
        start: Option<i32>,
        end: Option<i32>,
    ) -> ApiResult<Option<Node>> {
        Ok(self
            .db
            .query_opt(
                &format!(
                    "UPDATE nodes SET port_range_start = $3, port_range_end = $4,
                            updated_at = {now}
                      WHERE id = $1 AND tenant_id = $2
                  RETURNING {NODE_COLUMNS}",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, tenant, start, end],
            )
            .await?)
    }

    async fn shared_operator_online_count(&self, tenant: TenantId) -> ApiResult<i64> {
        Ok(self
            .db
            .query_scalar::<i64>(
                &format!(
                    "SELECT count(*) FROM nodes
                     WHERE tenant_id = $1 AND status = 'online'
                       AND {}",
                    shared_operator_clause(self.db.engine())
                ),
                params![tenant],
            )
            .await?)
    }

    async fn take_lease(&self, id: NodeId, instance: Uuid, lease_seconds: f64) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE nodes SET owning_instance_id = $2,
                        lease_expires_at = {now} + make_interval(secs => $3)
                     WHERE id = $1",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, instance, lease_seconds],
            )
            .await?;
        Ok(())
    }

    async fn release_lease(&self, id: NodeId, instance: Uuid) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE nodes SET status = 'offline', updated_at = {now},
                        owning_instance_id = NULL, lease_expires_at = NULL
                     WHERE id = $1 AND owning_instance_id = $2",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, instance],
            )
            .await?;
        Ok(())
    }

    async fn record_capabilities(
        &self,
        id: NodeId,
        reported: ReportedCapabilities,
    ) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    "UPDATE nodes SET capabilities = $2, hostname = $3, platform = $4,
                        status = 'online', last_seen_at = {now}, updated_at = {now}
                     WHERE id = $1",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![
                    id,
                    reported.capabilities,
                    reported.hostname,
                    reported.platform
                ],
            )
            .await?;
        Ok(())
    }

    async fn record_resources(
        &self,
        id: NodeId,
        resources: &serde_json::Value,
        instance: Uuid,
        lease_seconds: f64,
    ) -> ApiResult<()> {
        self.db
            .exec(
                &format!(
                    // `status = 'online'` belongs here, not only on Register: a
                    // node sending us resources IS online, and making the
                    // heartbeat say so is what stops a stale `offline` from
                    // outliving the connection that caused it. Register used to
                    // be the sole writer of `online`, so a node marked offline
                    // in error stayed that way until it fully reconnected —
                    // which a healthy node never does (MAIN-363).
                    "UPDATE nodes SET last_seen_at = {now}, resources = $2,
                        status = 'online',
                        lease_expires_at = CASE WHEN owning_instance_id = $3
                            THEN {now} + make_interval(secs => $4)
                            ELSE lease_expires_at END
                     WHERE id = $1",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, resources, instance, lease_seconds],
            )
            .await?;
        Ok(())
    }

    async fn merge_runtime_auth(&self, id: NodeId, profiles: &serde_json::Value) -> ApiResult<()> {
        let merge = json(self.db.engine()).set("capabilities", "{runtime_auth}", "$2");
        self.db
            .exec(
                &format!(
                    "UPDATE nodes
                     SET capabilities = {merge},
                         updated_at = {now}
                     WHERE id = $1",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![id, profiles],
            )
            .await?;
        Ok(())
    }

    async fn expire_sessions_missing_from_tmux(
        &self,
        node: NodeId,
        live_tmux_sessions: &[String],
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'exited', ended_at = {now}, updated_at = {now}
                     WHERE node_id = $1
                       AND status IN ('starting', 'running', 'detached')
                       AND (tmux_session IS NULL OR tmux_session != ALL($2))",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![node, live_tmux_sessions.to_vec()],
            )
            .await?)
    }

    async fn mark_session_running(
        &self,
        session: SessionId,
        node: NodeId,
        tmux_session: &str,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'running', tmux_session = $2, updated_at = {now}
                     WHERE id = $1 AND node_id = $3",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![session, tmux_session, node],
            )
            .await?)
    }

    async fn mark_session_exited(&self, session: SessionId, node: NodeId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'exited', ended_at = {now}, updated_at = {now}
                     WHERE id = $1 AND node_id = $2",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![session, node],
            )
            .await?)
    }

    async fn mark_session_failed(
        &self,
        session: SessionId,
        node: NodeId,
        message: &str,
    ) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                &format!(
                    "UPDATE sessions SET status = 'error', error = $3, ended_at = {now},
                        updated_at = {now}
                     WHERE id = $1 AND node_id = $2",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![session, node, message],
            )
            .await?)
    }
}

pub struct DbJoinTokenRepository {
    db: DbPool,
}

impl DbJoinTokenRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl JoinTokenRepository for DbJoinTokenRepository {
    async fn issue(
        &self,
        tenant: TenantId,
        token_hash: &str,
        created_by: UserId,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> ApiResult<()> {
        self.db
            .exec(
                "INSERT INTO join_tokens (id, tenant_id, token_hash, name, created_by, expires_at)
                 VALUES ($1, $2, $3, '', $4, $5)",
                params![
                    JoinTokenId::new(),
                    tenant,
                    token_hash,
                    created_by,
                    expires_at
                ],
            )
            .await?;
        Ok(())
    }

    async fn consume(&self, token_hash: &str) -> ApiResult<Option<ConsumedJoinToken>> {
        let row: Option<(JoinTokenId, TenantId, Option<UserId>)> = self
            .db
            .query_opt(
                &format!(
                    "UPDATE join_tokens SET used_at = {now}
                     WHERE token_hash = $1 AND expires_at > {now}
                     RETURNING id, tenant_id, created_by",
                    now = type_mapping(self.db.engine()).now()
                ),
                params![token_hash],
            )
            .await?;
        Ok(row.map(|(id, tenant, created_by)| ConsumedJoinToken {
            id,
            tenant,
            created_by,
        }))
    }
}

/// The `tenant_cas` row as sqlx hands it back: the `TenantCa` columns in
/// declaration order, followed by the encrypted private key.
///
/// Named because eight anonymous tuple elements at the use site say nothing
/// about which is which, and the order has to match the SELECT exactly.
type CaRow = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    Vec<u8>,
);

pub struct DbTenantCaRepository {
    db: DbPool,
}

impl DbTenantCaRepository {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TenantCaRepository for DbTenantCaRepository {
    async fn insert(
        &self,
        tenant: TenantId,
        state: &str,
        cert_pem: &str,
        key_enc: Vec<u8>,
        fingerprint: &str,
        not_after: chrono::DateTime<chrono::Utc>,
    ) -> ApiResult<TenantCa> {
        Ok(self
            .db
            .query_one(
                "INSERT INTO tenant_cas
                   (id, tenant_id, state, cert_pem, key_enc, fingerprint, not_after)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 RETURNING id, tenant_id, state, cert_pem, fingerprint, not_after, created_at",
                params![
                    Uuid::now_v7(),
                    tenant,
                    state,
                    cert_pem,
                    key_enc,
                    fingerprint,
                    not_after
                ],
            )
            .await?)
    }

    async fn bundle(&self, tenant: TenantId) -> ApiResult<Vec<TenantCa>> {
        Ok(self
            .db
            .query_all(
                "SELECT id, tenant_id, state, cert_pem, fingerprint, not_after, created_at
                 FROM tenant_cas WHERE tenant_id = $1 ORDER BY created_at",
                params![tenant],
            )
            .await?)
    }

    async fn active_signer(&self, tenant: TenantId) -> ApiResult<Option<(TenantCa, Vec<u8>)>> {
        let row: Option<CaRow> = self
            .db
            .query_opt(
                "SELECT id, tenant_id, state, cert_pem, fingerprint, not_after, created_at, key_enc
                 FROM tenant_cas WHERE tenant_id = $1 AND state = 'active'",
                params![tenant],
            )
            .await?;
        Ok(row.map(|r| {
            (
                TenantCa {
                    id: r.0,
                    tenant_id: r.1,
                    state: r.2,
                    cert_pem: r.3,
                    fingerprint: r.4,
                    not_after: r.5,
                    created_at: r.6,
                },
                r.7,
            )
        }))
    }

    async fn promote(&self, tenant: TenantId, ca_id: Uuid) -> ApiResult<bool> {
        let mut tx = self.db.begin().await.map_err(nook_db::DbError::from)?;
        // Demote first: the partial unique index allows only one active row, so
        // the order matters.
        tx.exec(
            "UPDATE tenant_cas SET state = 'retiring'
             WHERE tenant_id = $1 AND state = 'active'",
            params![tenant],
        )
        .await?;
        let done = tx
            .exec(
                "UPDATE tenant_cas SET state = 'active'
                 WHERE id = $1 AND tenant_id = $2 AND state = 'staged'",
                params![ca_id, tenant],
            )
            .await?;
        if done == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn retire(&self, ca_id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        Ok(self
            .db
            .exec(
                "DELETE FROM tenant_cas WHERE id = $1 AND tenant_id = $2 AND state <> 'active'",
                params![ca_id, tenant],
            )
            .await?)
    }
}

// ── in-memory fakes (AC-3) ──────────────────────────────────────────────────
//
// Enough behavior that a caller test is worth trusting: tenant scoping, the
// `COALESCE` that stops a re-join transferring ownership, the single-use
// token, and the lease's "only if we still hold it" guard. A fake that accepted
// everything would let a caller test pass while the real statement refused.

use std::sync::Mutex;

#[derive(Debug, Clone)]
struct FakeNode {
    node: Node,
    token_hash: String,
    owning_instance_id: Option<Uuid>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    ca_id: Option<Uuid>,
    cert_not_after: Option<chrono::DateTime<chrono::Utc>>,
    public_key_pem: Option<String>,
}

/// A session row, only as far as the node socket touches it.
#[derive(Debug, Clone)]
struct FakeSession {
    id: SessionId,
    tenant: TenantId,
    node: NodeId,
    status: String,
    tmux_session: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
struct FakeNodeState {
    nodes: Vec<FakeNode>,
    sessions: Vec<FakeSession>,
}

#[derive(Default)]
pub struct FakeNodeRepository {
    inner: Mutex<FakeNodeState>,
}

impl FakeNodeRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a node directly, for tests about reading rather than enrolling.
    pub fn add(&self, tenant: TenantId, name: &str, owner: Option<Uuid>, shared: bool) -> NodeId {
        let now = chrono::Utc::now();
        let id = NodeId::new();
        self.inner.lock().unwrap().nodes.push(FakeNode {
            node: Node {
                id,
                tenant_id: tenant,
                name: name.to_string(),
                hostname: String::new(),
                platform: String::new(),
                capabilities: serde_json::json!({}),
                resources: serde_json::json!({}),
                status: "offline".into(),
                last_seen_at: None,
                owner_person_id: owner,
                shared,
                created_at: now,
                updated_at: now,
                labels: serde_json::json!({}),
                taints: serde_json::json!([]),
                home_tenant: None,
                port_range_start: None,
                port_range_end: None,
            },
            token_hash: String::new(),
            owning_instance_id: None,
            revoked_at: None,
            ca_id: None,
            cert_not_after: None,
            public_key_pem: None,
        });
        id
    }

    pub fn set_capabilities(&self, id: NodeId, caps: serde_json::Value) {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            n.node.capabilities = caps;
        }
    }

    /// Give a node a live leaf from `ca_id`, so the retirement guard has
    /// something to count.
    pub fn set_leaf(&self, id: NodeId, ca_id: Uuid, not_after: chrono::DateTime<chrono::Utc>) {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            n.ca_id = Some(ca_id);
            n.cert_not_after = Some(not_after);
        }
    }

    pub fn add_session(&self, id: SessionId, tenant: TenantId, node: NodeId, tmux: Option<&str>) {
        self.inner.lock().unwrap().sessions.push(FakeSession {
            id,
            tenant,
            node,
            status: "running".into(),
            tmux_session: tmux.map(str::to_string),
            error: None,
        });
    }

    pub fn session_status(&self, id: SessionId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.status.clone())
    }

    pub fn session_error(&self, id: SessionId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.error.clone())
    }

    pub fn owning_instance(&self, id: NodeId) -> Option<Uuid> {
        self.inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .and_then(|n| n.owning_instance_id)
    }

    pub fn status_of(&self, id: NodeId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| n.node.status.clone())
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().nodes.len()
    }
}

#[async_trait]
impl NodeRepository for FakeNodeRepository {
    async fn set_placement(
        &self,
        id: NodeId,
        tenant: TenantId,
        labels: serde_json::Value,
        taints: serde_json::Value,
    ) -> ApiResult<Option<Node>> {
        let mut st = self.inner.lock().unwrap();
        let Some(n) = st
            .nodes
            .iter_mut()
            .find(|n| n.node.id == id && n.node.tenant_id == tenant)
        else {
            return Ok(None);
        };
        n.node.labels = labels;
        n.node.taints = taints;
        Ok(Some(n.node.clone()))
    }

    async fn live_leases(&self) -> ApiResult<Vec<(Uuid, Uuid, f64)>> {
        // The fake models no lease clock; the registry's cache-refresh path is
        // exercised against a real database, not here.
        Ok(Vec::new())
    }

    async fn tenant_of(&self, id: NodeId) -> ApiResult<Option<TenantId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| n.node.tenant_id))
    }

    async fn get(&self, tenant: TenantId, id: NodeId) -> ApiResult<Option<Node>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id && n.node.tenant_id == tenant)
            .map(|n| n.node.clone()))
    }

    async fn by_id_any_tenant(&self, id: NodeId) -> ApiResult<Option<Node>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| n.node.clone()))
    }

    async fn tenant_names(&self, ids: &[TenantId]) -> ApiResult<HashMap<Uuid, String>> {
        // The fake has no tenants table; a stable synthetic name is enough for
        // callers asserting that a foreign node is LABELLED at all.
        Ok(ids
            .iter()
            .map(|t| (t.0, format!("tenant-{}", &t.0.simple().to_string()[..8])))
            .collect())
    }

    async fn placement_candidates(&self, tenant: TenantId) -> ApiResult<Vec<Node>> {
        // The tenant half only. The member half — nodes owned by a person who
        // belongs to this tenant but whose machine is homed elsewhere — needs a
        // `users` table to resolve, and this fake holds nodes. That behaviour is
        // pinned against a real database in `tests/placement_across_tenants.rs`;
        // a fake that guessed at it would let a caller test pass against a rule
        // the database does not have.
        let s = self.inner.lock().unwrap();
        let mut out: Vec<Node> = s
            .nodes
            .iter()
            .filter(|n| n.node.tenant_id == tenant)
            .map(|n| n.node.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        own_person: Option<Uuid>,
    ) -> ApiResult<Vec<Node>> {
        let s = self.inner.lock().unwrap();
        let in_active_tenant = |n: &&FakeNode| {
            n.node.tenant_id == tenant
                // `owner = None` is the whole fleet; otherwise own nodes PLUS
                // shared ones (MAIN-132/135).
                && match owner {
                    None => true,
                    Some(person) => n.node.owner_person_id == Some(person) || n.node.shared,
                }
        };
        // The owner leg (MAIN-353): the caller's OWN machines, whatever tenant
        // they are homed in. Never widened by `owner = None`.
        let mine = |n: &&FakeNode| match own_person {
            Some(p) => n.node.owner_person_id == Some(p),
            None => false,
        };
        let mut out: Vec<Node> = s
            .nodes
            .iter()
            .filter(|n| in_active_tenant(n) || mine(n))
            .map(|n| n.node.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn page(
        &self,
        tenant: TenantId,
        owner: Option<Uuid>,
        own_person: Option<Uuid>,
        args: &PageArgs,
    ) -> ApiResult<DbPage<Node>> {
        let all = self.list(tenant, owner, own_person).await?;
        let q = args.q.as_ref().map(|s| s.to_lowercase());
        let rows: Vec<Node> = all
            .into_iter()
            .filter(|n| match &q {
                None => true,
                Some(q) => {
                    n.name.to_lowercase().contains(q)
                        || n.hostname.to_lowercase().contains(q)
                        || n.platform.to_lowercase().contains(q)
                        || n.status.to_lowercase().contains(q)
                }
            })
            .collect();
        Ok(nook_db::paging::page_vec(
            rows,
            args,
            |n| n.id.0,
            |col, a, b| match col {
                "name" => a.name.cmp(&b.name),
                "status" => a.status.cmp(&b.status),
                "platform" => a.platform.cmp(&b.platform),
                "last_seen_at" => a.last_seen_at.cmp(&b.last_seen_at),
                other => unreachable!("unlisted sort col {other}"),
            },
        ))
    }

    async fn list_ids_and_names(&self, tenant: TenantId) -> ApiResult<Vec<(NodeId, String)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .filter(|n| n.node.tenant_id == tenant)
            .map(|n| (n.node.id, n.node.name.clone()))
            .collect::<Vec<_>>())
        .map(|mut v: Vec<(NodeId, String)>| {
            v.sort_by(|a, b| a.1.cmp(&b.1));
            v
        })
    }

    async fn ids_in_tenant(&self, tenant: TenantId) -> ApiResult<Vec<NodeId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .filter(|n| n.node.tenant_id == tenant)
            .map(|n| n.node.id)
            .collect())
    }

    async fn exists_in_tenant(&self, id: NodeId, tenant: TenantId) -> ApiResult<bool> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .any(|n| n.node.id == id && n.node.tenant_id == tenant))
    }

    async fn name_of(&self, id: NodeId) -> ApiResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| n.node.name.clone()))
    }

    async fn delete(&self, tenant: TenantId, id: NodeId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.nodes.len();
        s.nodes
            .retain(|n| !(n.node.id == id && n.node.tenant_id == tenant));
        Ok((before - s.nodes.len()) as u64)
    }

    async fn sharing(&self, id: NodeId, tenant: TenantId) -> ApiResult<Option<NodeSharing>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id && n.node.tenant_id == tenant)
            .map(|n| NodeSharing {
                owner_person_id: n.node.owner_person_id,
                shared: n.node.shared,
            }))
    }

    async fn sharing_and_capabilities(
        &self,
        id: NodeId,
        tenant: TenantId,
    ) -> ApiResult<Option<(NodeSharing, serde_json::Value)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id && n.node.tenant_id == tenant)
            .map(|n| {
                (
                    NodeSharing {
                        owner_person_id: n.node.owner_person_id,
                        shared: n.node.shared,
                    },
                    n.node.capabilities.clone(),
                )
            }))
    }

    async fn set_shared(
        &self,
        id: NodeId,
        tenant: TenantId,
        shared: bool,
    ) -> ApiResult<Option<Node>> {
        let mut s = self.inner.lock().unwrap();
        Ok(s.nodes
            .iter_mut()
            .find(|n| n.node.id == id && n.node.tenant_id == tenant)
            .map(|n| {
                n.node.shared = shared;
                n.node.updated_at = chrono::Utc::now();
                n.node.clone()
            }))
    }

    async fn by_token_hash(&self, token_hash: &str) -> ApiResult<Option<NodeIdentity>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.token_hash == token_hash)
            .map(|n| NodeIdentity {
                id: n.node.id,
                tenant: n.node.tenant_id,
                name: n.node.name.clone(),
            }))
    }

    async fn upsert_joining(&self, node: JoiningNode) -> ApiResult<NodeId> {
        let mut s = self.inner.lock().unwrap();
        // ON CONFLICT (tenant_id, name).
        if let Some(existing) = s
            .nodes
            .iter_mut()
            .find(|n| n.node.tenant_id == node.tenant && n.node.name == node.name)
        {
            existing.node.hostname = node.hostname;
            existing.node.platform = node.platform;
            existing.token_hash = node.token_hash;
            // COALESCE: a re-join never transfers an already-recorded owner.
            existing.node.owner_person_id = existing.node.owner_person_id.or(node.owner_person_id);
            existing.node.updated_at = chrono::Utc::now();
            return Ok(existing.node.id);
        }
        let now = chrono::Utc::now();
        let id = NodeId::new();
        s.nodes.push(FakeNode {
            node: Node {
                id,
                tenant_id: node.tenant,
                name: node.name,
                hostname: node.hostname,
                platform: node.platform,
                capabilities: serde_json::json!({}),
                resources: serde_json::json!({}),
                status: "offline".into(),
                last_seen_at: None,
                owner_person_id: node.owner_person_id,
                shared: false,
                labels: serde_json::json!({}),
                taints: serde_json::json!([]),
                home_tenant: None,
                port_range_start: None,
                port_range_end: None,
                created_at: now,
                updated_at: now,
            },
            token_hash: node.token_hash,
            owning_instance_id: None,
            revoked_at: None,
            ca_id: None,
            cert_not_after: None,
            public_key_pem: None,
        });
        Ok(id)
    }

    async fn upsert_enrolling(
        &self,
        _id: NodeId,
        tenant: TenantId,
        name: &str,
        token_hash: &str,
        owner_person_id: Option<Uuid>,
    ) -> ApiResult<NodeId> {
        // `_id` is only the id PROPOSED for a fresh row. On conflict the
        // statement returns the existing row's id instead, so the caller must
        // use what comes back rather than what it passed in.
        self.upsert_joining(JoiningNode {
            tenant,
            name: name.to_string(),
            hostname: String::new(),
            platform: String::new(),
            token_hash: token_hash.to_string(),
            owner_person_id,
        })
        .await
    }

    async fn cert_identity(&self, id: NodeId) -> ApiResult<Option<NodeCertIdentity>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| NodeCertIdentity {
                tenant: n.node.tenant_id,
                public_key_pem: n.public_key_pem.clone(),
                revoked_at: n.revoked_at,
            }))
    }

    async fn revocation_state(
        &self,
        id: NodeId,
    ) -> ApiResult<Option<(TenantId, Option<chrono::DateTime<chrono::Utc>>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| (n.node.tenant_id, n.revoked_at)))
    }

    async fn record_issued_leaf(&self, id: NodeId, leaf: IssuedLeaf) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            n.ca_id = Some(leaf.ca_id);
            n.cert_not_after = Some(leaf.not_after);
            n.public_key_pem = Some(leaf.public_key_pem);
        }
        Ok(())
    }

    async fn revoke(&self, id: NodeId, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s
                .nodes
                .iter_mut()
                .find(|n| n.node.id == id && n.node.tenant_id == tenant)
            {
                Some(n) => {
                    n.revoked_at = Some(chrono::Utc::now());
                    1
                }
                None => 0,
            },
        )
    }

    async fn live_leaf_count(&self, tenant: TenantId, ca_id: Uuid) -> ApiResult<i64> {
        let now = chrono::Utc::now();
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .filter(|n| {
                n.node.tenant_id == tenant
                    && n.ca_id == Some(ca_id)
                    && n.revoked_at.is_none()
                    && n.cert_not_after.is_some_and(|t| t > now)
            })
            .count() as i64)
    }

    async fn eligible_loop_executors(
        &self,
        tenant: TenantId,
        person: Uuid,
        runtime: &str,
        kind: &str,
    ) -> ApiResult<Vec<NodeId>> {
        let s = self.inner.lock().unwrap();
        let authorized_for = |n: &FakeNode| {
            n.node
                .capabilities
                .get("runtime_auth")
                .and_then(|v| v.as_array())
                .is_some_and(|profiles| {
                    profiles.iter().any(|p| {
                        p.get("runtime").and_then(|v| v.as_str()) == Some(runtime)
                            && p.get("state").and_then(|v| v.as_str()) == Some("authorized")
                    })
                })
        };
        let is_operator = |n: &FakeNode| {
            n.node
                .capabilities
                .get("shared_operator")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        let declares = |n: &FakeNode| {
            n.node
                .capabilities
                .get("loop_kinds")
                .and_then(|v| v.as_array())
                .is_some_and(|ks| ks.iter().any(|k| k.as_str() == Some(kind)))
        };
        let mut eligible: Vec<&FakeNode> = s
            .nodes
            .iter()
            .filter(|n| n.node.tenant_id == tenant && n.node.status == "online")
            .filter(|n| n.node.owner_person_id == Some(person) || is_operator(n))
            // The build wall, ahead of anything the node declared (AC-3).
            .filter(|n| !(kind == "build" && is_operator(n)))
            .filter(|n| authorized_for(n))
            .filter(|n| declares(n))
            .collect();
        // `ORDER BY (owner_person_id = $2) DESC`: your own node before the
        // shared operator.
        eligible.sort_by_key(|n| (n.node.owner_person_id != Some(person), n.node.id.0));
        Ok(eligible.iter().map(|n| n.node.id).collect())
    }

    async fn is_shared_operator(&self, id: NodeId) -> ApiResult<bool> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .and_then(|n| n.node.capabilities.get("shared_operator"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    async fn loop_profile(&self, id: NodeId) -> ApiResult<Option<(Vec<String>, Option<u32>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| {
                let kinds = n
                    .node
                    .capabilities
                    .get("loop_kinds")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let cap = n
                    .node
                    .capabilities
                    .get("max_loop_jobs")
                    .and_then(|v| v.as_u64())
                    .map(|x| x as u32);
                (kinds, cap)
            }))
    }

    async fn owned_online_count(&self, tenant: TenantId, person: Uuid) -> ApiResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .filter(|n| {
                n.node.tenant_id == tenant
                    && n.node.owner_person_id == Some(person)
                    && n.node.status == "online"
            })
            .count() as i64)
    }

    async fn owned_with_resources(
        &self,
        tenant: TenantId,
        person: Uuid,
    ) -> ApiResult<Vec<(NodeId, serde_json::Value)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .filter(|n| n.node.tenant_id == tenant && n.node.owner_person_id == Some(person))
            .map(|n| {
                (
                    n.node.id,
                    serde_json::to_value(&n.node.resources).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect())
    }

    async fn by_id_any_tenant_or_none(&self, id: NodeId) -> ApiResult<Option<Node>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.node.id == id)
            .map(|n| n.node.clone()))
    }

    async fn set_port_range(
        &self,
        id: NodeId,
        tenant: TenantId,
        start: Option<i32>,
        end: Option<i32>,
    ) -> ApiResult<Option<Node>> {
        let mut st = self.inner.lock().unwrap();
        let Some(n) = st
            .nodes
            .iter_mut()
            .find(|n| n.node.id == id && n.node.tenant_id == tenant)
        else {
            return Ok(None);
        };
        n.node.port_range_start = start;
        n.node.port_range_end = end;
        Ok(Some(n.node.clone()))
    }

    async fn shared_operator_online_count(&self, tenant: TenantId) -> ApiResult<i64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .nodes
            .iter()
            .filter(|n| {
                n.node.tenant_id == tenant
                    && n.node.status == "online"
                    && n.node
                        .capabilities
                        .get("shared_operator")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
            })
            .count() as i64)
    }

    async fn take_lease(&self, id: NodeId, instance: Uuid, _lease_seconds: f64) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            // Last writer wins, matching the statement and reality.
            n.owning_instance_id = Some(instance);
        }
        Ok(())
    }

    async fn release_lease(&self, id: NodeId, instance: Uuid) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s
            .nodes
            .iter_mut()
            .find(|n| n.node.id == id && n.owning_instance_id == Some(instance))
        {
            n.node.status = "offline".into();
            n.node.updated_at = chrono::Utc::now();
            n.owning_instance_id = None;
        }
        Ok(())
    }

    async fn record_capabilities(
        &self,
        id: NodeId,
        reported: ReportedCapabilities,
    ) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            n.node.capabilities = reported.capabilities;
            n.node.hostname = reported.hostname;
            n.node.platform = reported.platform;
            n.node.status = "online".into();
            n.node.last_seen_at = Some(chrono::Utc::now());
            n.node.updated_at = chrono::Utc::now();
        }
        Ok(())
    }

    async fn record_resources(
        &self,
        id: NodeId,
        resources: &serde_json::Value,
        instance: Uuid,
        _lease_seconds: f64,
    ) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            n.node.resources = resources.clone();
            n.node.last_seen_at = Some(chrono::Utc::now());
            // A heartbeat is proof of life — same as the SQL (MAIN-363).
            n.node.status = "online".into();
            // The lease extends only while it is still ours — the CASE.
            if n.owning_instance_id == Some(instance) {
                n.owning_instance_id = Some(instance);
            }
        }
        Ok(())
    }

    async fn merge_runtime_auth(&self, id: NodeId, profiles: &serde_json::Value) -> ApiResult<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(n) = s.nodes.iter_mut().find(|n| n.node.id == id) {
            // A MERGE at one path, creating the object if missing — not a
            // whole-blob replace, which is the bug this shape avoids.
            if !n.node.capabilities.is_object() {
                n.node.capabilities = serde_json::json!({});
            }
            n.node.capabilities["runtime_auth"] = profiles.clone();
            n.node.updated_at = chrono::Utc::now();
        }
        Ok(())
    }

    async fn expire_sessions_missing_from_tmux(
        &self,
        node: NodeId,
        live_tmux_sessions: &[String],
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let mut n = 0;
        for sess in s.sessions.iter_mut() {
            let believed_live = matches!(sess.status.as_str(), "starting" | "running" | "detached");
            let gone = match &sess.tmux_session {
                None => true,
                Some(t) => !live_tmux_sessions.contains(t),
            };
            if sess.node == node && believed_live && gone {
                sess.status = "exited".into();
                n += 1;
            }
        }
        Ok(n)
    }

    async fn mark_session_running(
        &self,
        session: SessionId,
        node: NodeId,
        tmux_session: &str,
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s
                .sessions
                .iter_mut()
                .find(|x| x.id == session && x.node == node)
            {
                Some(x) => {
                    x.status = "running".into();
                    x.tmux_session = Some(tmux_session.to_string());
                    1
                }
                None => 0,
            },
        )
    }

    async fn mark_session_exited(&self, session: SessionId, node: NodeId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s
                .sessions
                .iter_mut()
                .find(|x| x.id == session && x.node == node)
            {
                Some(x) => {
                    x.status = "exited".into();
                    1
                }
                None => 0,
            },
        )
    }

    async fn mark_session_failed(
        &self,
        session: SessionId,
        node: NodeId,
        message: &str,
    ) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        Ok(
            match s
                .sessions
                .iter_mut()
                .find(|x| x.id == session && x.node == node)
            {
                Some(x) => {
                    x.status = "error".into();
                    x.error = Some(message.to_string());
                    1
                }
                None => 0,
            },
        )
    }
}

/// `(tenant, minter, expires_at, used)` — the minter is optional because a
/// legacy token recorded none.
type FakeToken = (
    TenantId,
    Option<UserId>,
    chrono::DateTime<chrono::Utc>,
    bool,
);

#[derive(Default)]
pub struct FakeJoinTokenRepository {
    inner: Mutex<HashMap<String, FakeToken>>,
}

impl FakeJoinTokenRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a **legacy** token: one that recorded no minter. `created_by` is
    /// nullable in the schema, and enrolment falls back to the tenant owner for
    /// exactly these.
    pub fn issue_legacy(&self, tenant: TenantId, token_hash: &str) {
        self.inner.lock().unwrap().insert(
            token_hash.to_string(),
            (
                tenant,
                None,
                chrono::Utc::now() + chrono::Duration::hours(1),
                false,
            ),
        );
    }

    /// Backdate a token's expiry, so expiry can be tested without waiting.
    pub fn expire(&self, token_hash: &str) {
        if let Some(e) = self.inner.lock().unwrap().get_mut(token_hash) {
            e.2 = chrono::Utc::now() - chrono::Duration::hours(1);
        }
    }

    pub fn is_used(&self, token_hash: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(token_hash)
            .is_some_and(|e| e.3)
    }
}

#[async_trait]
impl JoinTokenRepository for FakeJoinTokenRepository {
    async fn issue(
        &self,
        tenant: TenantId,
        token_hash: &str,
        created_by: UserId,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> ApiResult<()> {
        self.inner.lock().unwrap().insert(
            token_hash.to_string(),
            (tenant, Some(created_by), expires_at, false),
        );
        Ok(())
    }

    async fn consume(&self, token_hash: &str) -> ApiResult<Option<ConsumedJoinToken>> {
        let mut s = self.inner.lock().unwrap();
        let Some(e) = s.get_mut(token_hash) else {
            return Ok(None);
        };
        // `expires_at > now()` — an expired token is indistinguishable from an
        // absent one. Note the real statement does NOT check `used_at`; it is
        // the single UPDATE that makes a race have one winner, and re-spending
        // an already-used token still matches while it is unexpired. The fake
        // mirrors that rather than being stricter.
        if e.2 <= chrono::Utc::now() {
            return Ok(None);
        }
        e.3 = true;
        Ok(Some(ConsumedJoinToken {
            id: JoinTokenId::new(),
            tenant: e.0,
            created_by: e.1,
        }))
    }
}

#[derive(Default)]
pub struct FakeTenantCaRepository {
    inner: Mutex<Vec<(TenantCa, Vec<u8>)>>,
}

impl FakeTenantCaRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state_of(&self, ca_id: Uuid) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c.id == ca_id)
            .map(|(c, _)| c.state.clone())
    }
}

#[async_trait]
impl TenantCaRepository for FakeTenantCaRepository {
    async fn insert(
        &self,
        tenant: TenantId,
        state: &str,
        cert_pem: &str,
        key_enc: Vec<u8>,
        fingerprint: &str,
        not_after: chrono::DateTime<chrono::Utc>,
    ) -> ApiResult<TenantCa> {
        let ca = TenantCa {
            id: Uuid::now_v7(),
            tenant_id: tenant.0,
            state: state.to_string(),
            cert_pem: cert_pem.to_string(),
            fingerprint: fingerprint.to_string(),
            not_after,
            created_at: chrono::Utc::now(),
        };
        self.inner.lock().unwrap().push((ca.clone(), key_enc));
        Ok(ca)
    }

    async fn bundle(&self, tenant: TenantId) -> ApiResult<Vec<TenantCa>> {
        let s = self.inner.lock().unwrap();
        let mut out: Vec<TenantCa> = s
            .iter()
            .filter(|(c, _)| c.tenant_id == tenant.0)
            .map(|(c, _)| c.clone())
            .collect();
        out.sort_by_key(|c| c.created_at);
        Ok(out)
    }

    async fn active_signer(&self, tenant: TenantId) -> ApiResult<Option<(TenantCa, Vec<u8>)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c.tenant_id == tenant.0 && c.state == "active")
            .cloned())
    }

    async fn promote(&self, tenant: TenantId, ca_id: Uuid) -> ApiResult<bool> {
        let mut s = self.inner.lock().unwrap();
        // Nothing to promote: the whole transaction rolls back, so the current
        // signer must NOT have been demoted.
        if !s
            .iter()
            .any(|(c, _)| c.id == ca_id && c.tenant_id == tenant.0 && c.state == "staged")
        {
            return Ok(false);
        }
        for (c, _) in s.iter_mut() {
            if c.tenant_id == tenant.0 && c.state == "active" {
                c.state = "retiring".into();
            }
        }
        for (c, _) in s.iter_mut() {
            if c.id == ca_id {
                c.state = "active".into();
            }
        }
        Ok(true)
    }

    async fn retire(&self, ca_id: Uuid, tenant: TenantId) -> ApiResult<u64> {
        let mut s = self.inner.lock().unwrap();
        let before = s.len();
        s.retain(|(c, _)| !(c.id == ca_id && c.tenant_id == tenant.0 && c.state != "active"));
        Ok((before - s.len()) as u64)
    }
}
