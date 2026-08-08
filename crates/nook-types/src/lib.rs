//! Domain types for NookOS. Rust owns the types: everything here derives
//! `ToSchema` and flows through OpenAPI into generated TypeScript.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Strongly-typed UUID newtypes. `value_type = String, format = Uuid` keeps the
/// generated OpenAPI/TS surface a plain string.
macro_rules! id_type {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(
                Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
                Serialize, Deserialize, sqlx::Type, ToSchema,
            )]
            // The ONLY sqlx left in this crate, and it is not row mapping —
            // MAIN-327 removed all of that. It is the BIND side, kept because 22
            // allow-listed integration tests still bind raw sqlx against
            // `bed.pool` (`.bind(tenant_id)`), and they are MAIN-267's to
            // convert, not this card's. When that card lands, this derive and
            // the sqlx dependency go with it and nothing else here changes.
            #[sqlx(transparent)]
            #[schema(value_type = String, format = Uuid)]
            pub struct $name(pub Uuid);

            impl $name {
                pub fn new() -> Self { Self(Uuid::now_v7()) }
            }

            impl Default for $name {
                fn default() -> Self { Self::new() }
            }

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    self.0.fmt(f)
                }
            }

            impl std::str::FromStr for $name {
                type Err = uuid::Error;
                fn from_str(s: &str) -> Result<Self, Self::Err> {
                    Ok(Self(Uuid::parse_str(s)?))
                }
            }

            // Bind as a dispatch parameter (MAIN-205). Implementing nook-db's
            // IntoDbValue (a foreign trait) for this local newtype is allowed by
            // the orphan rule where `From<Id> for DbValue` was not. The newtype
            // wraps a plain `Uuid`, so this encodes exactly as binding the
            // newtype did — `params![id]` needs no `.0` at the site.
            impl nook_db::IntoDbValue for $name {
                fn into_db_value(self) -> nook_db::DbValue {
                    nook_db::DbValue::Uuid(Some(self.0))
                }
            }
            impl nook_db::IntoDbValue for &$name {
                fn into_db_value(self) -> nook_db::DbValue {
                    nook_db::DbValue::Uuid(Some(self.0))
                }
            }
            // Read back out of a row (MAIN-327). The mirror of IntoDbValue, and
            // the same orphan-rule trick: implementing nook-db's FromDbColumn
            // here is what lets `#[derive(FromDbRow)]` map a `TenantId` field
            // without nook-types naming sqlx at all. It delegates to `Uuid`, so
            // it decodes exactly as the old transparent newtype did.
            impl nook_db::FromDbColumn for $name {
                fn from_db_column(
                    row: &nook_db::DbRow,
                    name: &str,
                ) -> Result<Self, nook_db::DbError> {
                    Ok(Self(row.get::<Uuid>(name)?))
                }
                fn from_db_column_at(
                    row: &nook_db::DbRow,
                    index: usize,
                ) -> Result<Self, nook_db::DbError> {
                    Ok(Self(row.get_at::<Uuid>(index)?))
                }
            }
            // `Option<$name>` can't impl IntoDbValue here (orphan rule: the local
            // type is covered by foreign `Option`); such sites pass
            // `opt.map(|x| x.0)` to reach the typed `Option<Uuid>` arm.
        )+
    };
}

id_type!(
    TenantId,
    UserId,
    IdentityId,
    AuthSessionId,
    JoinTokenId,
    NodeId,
    WorkspaceId,
    NodeWorkspaceId,
    SessionId,
    BoardId,
    ColumnId,
    TaskId,
    EventId,
    NoteId,
    ThemeId,
    SettingId,
    GitCredentialId,
    UserNoteId,
    UserNoteFolderId,
    JobId,
    JobTranscriptId,
    InteractionId,
);

// ── Tenancy ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Tenant {
    pub id: TenantId,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Role values: `owner` | `admin` | `member` (TEXT CHECK in the schema).
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct User {
    pub id: UserId,
    pub tenant_id: TenantId,
    pub display_name: String,
    pub email: String,
    pub avatar_url: Option<String>,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A tenant the caller belongs to, and the role they hold in it.
///
/// Membership is deliberately its own concept: a user has one *current*
/// tenant (`users.tenant_id`) and may reach several, which is what teams will
/// be — not a new mechanism, just more rows.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TenantMembership {
    pub id: TenantId,
    pub name: String,
    pub slug: String,
    /// `owner` | `admin` | `member`.
    pub role: String,
    /// The tenant this session is scoped to right now.
    pub current: bool,
    pub created_at: DateTime<Utc>,
}

/// One member OF a tenant, for the members panel — distinct from
/// `TenantMembership` (a tenant the caller belongs to). Keyed by
/// `principal_id`, the `users.id`/`tenant_members.principal_id` used to change
/// the role or remove them.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct TenantMemberItem {
    pub principal_id: Uuid,
    pub email: String,
    pub display_name: String,
    /// `owner` | `admin` | `member`.
    pub role: String,
    pub joined_at: DateTime<Utc>,
}

/// One page of any paginated list — THE pagination wire contract (QOL sprint
/// 2026-08). Every list endpoint returns this shape; the request half is
/// [`PageQuery`]. `next_cursor` is an OPAQUE token: pass it back verbatim as
/// `after`, never parse it — the opacity is what lets the server pick the
/// mechanism (keyset vs offset) per request. Null means end of list.
///
/// The server half — cursor codec, validation, SQL skeleton — is
/// `nook_db::paging`; the React half is `usePagedList` + `PagedPanel`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Page<T> {
    pub rows: Vec<T>,
    /// Opaque continuation token — pass back verbatim as `after`; null = end.
    pub next_cursor: Option<String>,
}

impl<T> From<nook_db::paging::DbPage<T>> for Page<T> {
    fn from(p: nook_db::paging::DbPage<T>) -> Self {
        Page {
            rows: p.rows,
            next_cursor: p.next_cursor,
        }
    }
}

/// The request half of the pagination contract: one query-string shape for
/// every list. Which fields `q` searches and which keys `sort` accepts differ
/// per endpoint and are documented on it; an unknown `sort`, a bad `dir` or a
/// stale cursor is a 400.
#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams)]
pub struct PageQuery {
    /// Case-insensitive substring; the searched fields differ per list.
    pub q: Option<String>,
    /// Opaque cursor from the previous page's `next_cursor`.
    pub after: Option<String>,
    /// Page size (default 50, clamped 1..=200).
    pub limit: Option<i64>,
    /// Sort key from the endpoint's documented set. Absent = newest first.
    pub sort: Option<String>,
    /// `asc` | `desc`. Defaults ascending under `sort`, newest-first without.
    pub dir: Option<String>,
}

impl PageQuery {
    /// Validate against a list's sort allowlist (`key -> output column`).
    /// The error is the caller's 400.
    pub fn args(
        &self,
        sorts: &[(&str, &str)],
    ) -> Result<nook_db::paging::PageArgs, nook_db::paging::PageError> {
        nook_db::paging::PageArgs::parse(
            self.q.as_deref(),
            self.after.as_deref(),
            self.limit,
            self.sort.as_deref(),
            self.dir.as_deref(),
            sorts,
        )
    }
}

/// Change a member's role. `member` ↔ `admin` for any owner/admin; `owner`
/// (a co-owner / transfer) is owner-only.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ChangeMemberRoleRequest {
    pub role: String,
}

/// The signed-in caller with their tenant.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    pub user: User,
    pub tenant: Tenant,
    /// The caller's person id — the cross-tenant identity a node's
    /// `owner_person_id` is keyed on. Exposed so the UI can mirror the
    /// server's node-visibility rule (own vs. teammate's) without a second
    /// request (MAIN-132). The tenant role rides on `user.role`.
    pub person_id: Uuid,
    /// Every tenant this person belongs to (from `tenant_members`), with the
    /// active one marked `current`. Carried on `me` so the UI can render a
    /// tenant switcher without a second request. A person in exactly one tenant
    /// gets a one-element list, and the UI shows a plain label for that case.
    #[serde(default)]
    pub tenants: Vec<TenantMembership>,
    /// What this caller may do, so a UI can hide what it cannot offer rather
    /// than rendering a button that 403s.
    #[serde(default)]
    pub capability: Capability,
}

/// Switch the browser session's active tenant. The caller must be a member of
/// the target tenant, or the endpoint returns 403.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SwitchTenantRequest {
    pub tenant_id: TenantId,
}

/// A pending/accepted/revoked invitation into a tenant. `accept_url` is set only
/// on the create response (the link to hand out); the token is never listed.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Invite {
    pub id: Uuid,
    pub email: String,
    /// `member` | `admin`.
    pub role: String,
    /// `pending` | `accepted` | `revoked`.
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// The accept link, returned only when the invite is created (never listed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[db(default)]
    pub accept_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateInviteRequest {
    pub email: String,
    /// `member` | `admin`. `owner` is never invitable (NG-3).
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AcceptInviteRequest {
    pub token: String,
}

/// Register a local account against a pending invite (MAIN-98). The email is
/// never here — it comes from the invite, so a client cannot register an
/// address it was not invited as. Username and password follow the ordinary
/// local-account rules; registration creates the account but does NOT accept the
/// invite (that stays a separate, verified step).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RegisterInviteRequest {
    pub token: String,
    pub name: String,
    pub username: String,
    pub password: String,
}

/// The outcome of an invite registration. Deliberately generic and identical
/// whether or not an account already existed for the invite's email, so the
/// endpoint never discloses whether an address is registered (AC-3).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RegisterInviteResult {
    pub message: String,
}

/// The outcome of accepting (or failing to accept) an invite. Whichever tenant
/// the person ends up in, the UI switches/refetches to it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AcceptInviteResult {
    /// True when the person is now a member of the invited tenant.
    pub accepted: bool,
    /// The tenant to land in — the shared one on success, else the person's own.
    pub tenant_id: TenantId,
    pub message: String,
}

/// Unauthenticated preview of an invite, so the `/accept` landing can name who
/// invited a signed-out visitor into which tenant before they sign in.
///
/// Every non-usable token — missing, expired, revoked, or already accepted —
/// returns the SAME `valid: false` shell with empty fields, so the response
/// reveals nothing that distinguishes them. The email is MASKED
/// (`r…@example.com`): enough for the invitee to recognise their own address,
/// not enough to harvest it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct InvitePreview {
    /// The token is pending and unexpired — the landing may show the invite.
    pub valid: bool,
    /// Inviting tenant's display name. Empty when `valid` is false.
    pub tenant: String,
    /// Inviter's display name. Empty when `valid` is false.
    pub inviter: String,
    /// The invitee's email, masked. Empty when `valid` is false.
    pub email: String,
}

/// Unauthenticated sign-in capabilities, so the login screen only offers what
/// this instance actually supports.
/// Hand an identity provider's ID token to the control plane, get one of ours.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct OidcExchangeRequest {
    pub id_token: String,
    /// Shown in the tokens list, so a person can tell which client to revoke.
    #[serde(default)]
    pub client_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OidcExchangeResponse {
    /// Shown once. Behaves like any other user token.
    pub token: String,
    pub user: User,
    pub tenant: Tenant,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct LocalAuthStatus {
    /// Local sign-in is possible: the tenant is undecided, or already local.
    pub available: bool,
    /// No account exists yet, so the first visitor can claim this instance.
    pub needs_bootstrap: bool,
    /// "oidc" | "local" | null when nobody has signed in yet.
    #[serde(default)]
    pub mode: Option<String>,
    /// At least one user on this instance has a local password set. This is the
    /// break-glass signal (MAIN-169 AC-5): during an OIDC outage the login page
    /// offers the password form ONLY when an existing local credential can use
    /// it — never registration, and never on an instance that has none.
    #[serde(default)]
    pub has_local_credentials: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LocalLoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LocalRegisterRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current: String,
    pub next: String,
}

/// Whether the signed-in user's email is verified, and whether a local
/// verification round-trip applies to them (MAIN-30).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EmailVerificationStatus {
    pub email: String,
    pub verified: bool,
    /// True for a local account that can request a verification email. OIDC
    /// users are verified upstream and cannot request one here (NG-1).
    pub can_request: bool,
}

/// The outcome of requesting a verification email — best-effort send, so a
/// mail-transport failure is reported here rather than failing the request.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RequestVerificationResult {
    pub sent: bool,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ConfirmVerificationRequest {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfirmVerificationResult {
    pub verified: bool,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AuthProviders {
    /// An OIDC identity provider is configured AND usable (discovery has
    /// succeeded). False both when no IdP is configured and when one is
    /// configured but currently unreachable — `oidc_degraded` tells those apart.
    pub oidc: bool,
    /// An OIDC identity provider is configured but its discovery document is
    /// currently unreachable (MAIN-169). The login page shows a retry notice
    /// where the IdP button sits, and never presents a local password form as
    /// though it were the instance's only sign-in method.
    #[serde(default)]
    pub oidc_degraded: bool,
    /// The dev/CI escape hatch is enabled (never in production).
    pub dev_login: bool,
    /// Username and password held in this database.
    #[serde(default)]
    pub local: bool,
    /// The identity provider itself, for clients that must talk to it
    /// directly.
    ///
    /// A desktop app cannot receive the browser redirect this control plane
    /// registered, so it uses the device authorization grant against the IdP.
    /// It learns where to go from here rather than from its own configuration:
    /// the operator sets the IdP up once, on the server, and every client
    /// follows.
    #[serde(default)]
    pub oidc_issuer: Option<String>,
    /// Where a native client starts a device authorization.
    ///
    /// Read from the IdP's discovery document. `None` means the provider does
    /// not advertise one — in which case no compliant client can start the
    /// flow, whatever else the provider supports.
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    /// Public client id for native clients. Distinct from the control plane's
    /// own client, which is confidential and must not ship inside an app.
    #[serde(default)]
    pub device_client_id: Option<String>,
}

// ── Nodes ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct GpuInfo {
    pub vendor: String,
    pub model: String,
}

/// What a node reports about itself on registration. The control plane never
/// inspects a machine — the node describes its own capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Capabilities {
    pub hostname: String,
    pub platform: String,
    pub architecture: String,
    pub cpus: u32,
    pub memory: u64,
    #[serde(default)]
    pub gpus: Vec<GpuInfo>,
    pub docker: bool,
    pub tmux: bool,
    pub git: Option<String>,
    /// The agent's own version. Reported like everything else here, so
    /// "which machines are behind?" needs no column of its own — this whole
    /// struct is already stored as jsonb on the node.
    #[serde(default)]
    pub agent_version: Option<String>,
    /// Detected runtime executables: "claude", "hermes", "codex", "bash", ...
    #[serde(default)]
    pub runtimes: Vec<String>,
    /// This node's SSH public key (generated locally; the private half never
    /// leaves the machine). Add it as a deploy key to clone private repos.
    #[serde(default)]
    pub ssh_public_key: Option<String>,
    /// Whether this node is the deployment's shared operator node — a machine
    /// the stack ships (MAIN-125) rather than a person's own. Reported so later
    /// executor selection can tell it apart from personal nodes; surfaced in
    /// `nook get nodes` and the Nodes UI. Set by `NOOK_SHARED_OPERATOR` on that
    /// container; false everywhere else.
    #[serde(default)]
    pub shared_operator: bool,
    /// Which loop stages this node will execute (MAIN-142): any of `spec`,
    /// `decompose`, `review`, `epic-run`, `build`. Set by `NOOK_LOOP_KINDS`.
    ///
    /// **Empty means the node accepts NO loop jobs** — the safe default, so a
    /// machine that never opted in cannot be handed agent work by an upgrade.
    /// It is the node's own declaration and the control plane treats it as
    /// exactly that: a filter it applies, never a permission it trusts. The
    /// shared-operator build wall does not consult this list at all.
    #[serde(default)]
    pub loop_kinds: Vec<String>,
    /// How many loop jobs this node will hold at once (MAIN-142), from
    /// `NOOK_MAX_LOOP_JOBS`. `0` disables claiming entirely. Absent from an
    /// older node's report, which reads as "unspecified" rather than zero —
    /// see `nook_control::services::jobs::CAPACITY_WHEN_UNREPORTED`.
    #[serde(default)]
    pub max_loop_jobs: Option<u32>,
    /// The port range this node offers sessions, `[start, end]` inclusive
    /// (MAIN-301). Absent from a node too old to report one, which reads as
    /// "no ports to lease" rather than a guessed range — inventing one would
    /// hand out ports something else on that machine is already using.
    #[serde(default)]
    pub port_range: Option<(u16, u16)>,
    /// Agent authorization profiles this node reports (MAIN-126): one per
    /// runtime-specific auth target (Claude Code, Hermes → Nous Portal, …), each
    /// with a state probed from the runtime's own CLI — never inferred from a
    /// credential file. Empty when nothing to authorize is installed.
    #[serde(default)]
    pub runtime_auth: Vec<AuthProfile>,
}

/// The authorization state of one runtime profile (MAIN-126). Four states, kept
/// distinct on purpose: `Unavailable` (the runtime binary is not installed) is
/// not the same as `NotAuthorized` (installed, probe says signed out), and
/// neither is `Unknown` (the probe failed or its output was unrecognised) —
/// only a probe that positively confirms a login is `Authorized`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    Authorized,
    NotAuthorized,
    Unknown,
    Unavailable,
}

/// One agent-authorization profile as reported by a node (MAIN-126). A profile
/// is a runtime-specific authentication target, not just a runtime: a runtime
/// with several providers (Hermes) contributes one profile per provider it
/// supports here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthProfile {
    /// Stable identifier, e.g. `claude` or `hermes-portal`.
    pub id: String,
    /// Human label, e.g. `Claude Code` or `Hermes → Nous Portal`.
    pub label: String,
    /// The runtime executable this profile authorizes (`claude`, `hermes`).
    pub runtime: String,
    pub state: AuthState,
    /// The signed-in account, when the probe reports one.
    #[serde(default)]
    pub identity: Option<String>,
}

/// Live resource sample a node reports on each heartbeat, so both humans and
/// the triage scheduler can see which machine can take the workload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct NodeResources {
    /// Overall CPU utilization, 0–100.
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// 1-minute load average (0 on platforms without it).
    pub load_avg1: f64,
    /// NookOS-managed sessions currently alive on the node.
    pub active_sessions: u32,
}

/// Status values: `online` | `offline`.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Node {
    pub id: NodeId,
    pub tenant_id: TenantId,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub capabilities: serde_json::Value,
    /// Latest heartbeat resource sample (see `NodeResources`); `{}` until first.
    pub resources: serde_json::Value,
    pub status: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The person who owns this node — its join-token minter, else the tenant
    /// owner (MAIN-119). Session-start is confined to this person (MAIN-130).
    pub owner_person_id: Option<Uuid>,
    /// Whether the owner has designated this node team-usable (MAIN-135). A
    /// shared node is VISIBLE to the whole team; it is not yet usable by them —
    /// session-start stays owner-only until a later unit of the epic.
    pub shared: bool,
    /// Operator-set labels, `{"key": "value"}` (MAIN-314). The DERIVED `os` and
    /// `arch` labels are not in here: they are computed from what the node
    /// reports, so storing them would let them drift from the truth.
    pub labels: serde_json::Value,
    /// Operator-set taints, `[{"key": …, "effect": …}]` (MAIN-314).
    pub taints: serde_json::Value,
    /// The owner has declined operator-authorize on this machine (MAIN-276).
    ///
    /// Default `false`: authorizing a runtime is the deployment operator's by
    /// default, because it is their hardware. This is the owner's veto on that
    /// one capability, and it is theirs alone to set.
    ///
    /// It says nothing about whether work may RUN here — authorize and
    /// permit-work are separate gates, and MAIN-278 owns the second.
    #[serde(default)]
    pub operator_authorize_optout: bool,
    /// The name of this node's HOME tenant, set only when that is not the
    /// tenant you are acting in (MAIN-353) — your own machine, reached from
    /// another of your orgs. `None` for the ordinary case, so a UI can render
    /// the badge on presence rather than by comparing ids.
    ///
    /// Computed per response, never stored: it is a fact about the *viewer's*
    /// position, not about the node.
    #[serde(default)]
    #[db(skip)]
    pub home_tenant: Option<String>,
    /// An operator's port range for this node, overriding what it advertises
    /// (MAIN-301). Both `None` means "use the node's own"; the pair is set and
    /// cleared together, which the API enforces.
    #[serde(default)]
    pub port_range_start: Option<i32>,
    #[serde(default)]
    pub port_range_end: Option<i32>,
    /// Ports inside the range this node must never lease (MAIN-301 follow-on).
    /// Operator POLICY — "something else owns this number here" — so it is
    /// durable, unlike a live-occupancy snapshot, which is stale as soon as it
    /// is taken and is deliberately not modelled.
    #[serde(default)]
    pub port_exclusions: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A node's refusal to take work unless the work tolerates it (MAIN-314).
///
/// The inverse of a label: a label says what a node IS, a taint says what it
/// will not accept. Keeping them apart is what stops "has X" and "refuses X"
/// becoming the same question.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct NodeTaint {
    pub key: String,
    /// What the taint does to unmatched work. `NoSchedule` today; the field is
    /// a string rather than an enum so a later effect does not break the wire.
    pub effect: String,
}

/// Everything placement reads off a node (MAIN-314). No scheduling here — the
/// reconciler that consumes this is a later child of the epic.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NodePlacement {
    /// Derived labels MERGED over the operator's, which is what a scheduler
    /// matches on. `os` and `arch` are always present; a custom label of the
    /// same name loses, because a node cannot be relabelled into another OS.
    pub labels: std::collections::BTreeMap<String, String>,
    /// Just the operator's own, so a UI can edit them without stripping the
    /// derived ones by round-tripping.
    pub custom_labels: std::collections::BTreeMap<String, String>,
    pub taints: Vec<NodeTaint>,
}

/// A node's port situation (MAIN-301): the range sessions lease from, where it
/// came from, and what is currently held.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NodePorts {
    /// The range in force — the operator's override if set, else the node's
    /// own. `None` when neither exists, which is why leasing is optional
    /// rather than a failure.
    pub range: Option<PortRange>,
    /// Where `range` came from, so the UI can say "reported by the node" vs
    /// "set here" instead of showing two numbers with no provenance.
    pub source: String,
    /// The node's own advertisement, kept separate so clearing the override in
    /// the UI shows what it will fall back to.
    pub advertised: Option<PortRange>,
    /// Live sessions holding a port on this node, lowest port first.
    pub leases: Vec<PortLease>,
    /// Ports the operator has ruled out inside the range, lowest first.
    #[serde(default)]
    pub excluded: Vec<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct PortRange {
    pub start: i32,
    pub end: i32,
}

/// One port a workspace needs, by NAME and by the env var its runtime reads
/// (MAIN-301).
///
/// The declaration is the workspace's, not the control plane's. Baking `PORT`
/// / `NOOK_PORT` / `API_PORT` into the broker would have meant every new
/// framework needing a change here; instead the repo says what it needs and
/// the broker only decides *which* numbers satisfy it. That is what lets a
/// Next.js app, an ASP.NET service and a Rust backend all lease from the same
/// node without the control plane knowing anything about any of them.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PortRequirement {
    /// Stable identity for this listener within the workspace — `web`, `api`,
    /// `debug`. What a lease is keyed on, so re-leasing is idempotent and a
    /// renamed env var does not orphan the old lease.
    pub name: String,
    /// The environment variable the session's runtime reads the number from.
    pub env: String,
    /// `tcp` or `udp`. Carried because a declaration that cannot say which is
    /// not a description of what the app binds; nothing dispatches on it yet.
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// Whether the session should refuse to start when this one cannot be
    /// leased. A `debug` listener is usually optional; the app's own port is
    /// usually not.
    ///
    /// **What each setting costs, because the default is inherited silently.**
    /// `true` refuses the session with a message naming the listener — loud,
    /// recoverable. `false` starts it and leaves the variable UNSET, which used
    /// to be indistinguishable from "this repo was cloned outside nook" and so
    /// sent apps to their hardcoded defaults — the shared literals every other
    /// session also falls back to. A session that skipped an optional listener
    /// now carries `NOOK_PORTS_UNSATISFIED` naming it (MAIN-377), so `false` is
    /// safe for an app that checks it and still unsafe for one that does not.
    #[serde(default)]
    pub required: bool,
    /// Which session runtimes this listener is for. EMPTY means every runtime,
    /// which is the default and is what keeps an untouched `.nook.toml`
    /// leasing exactly what it leases today (MAIN-378 AC-4).
    ///
    /// The opt-in that fixes the ceiling. A declaration belongs to the
    /// WORKSPACE, so before this every session in a repo leased the whole set —
    /// a shell and an agent as much as the session actually running the app.
    /// Eleven listeners against a 100-port range is nine concurrent sessions,
    /// and most of them bind nothing.
    ///
    /// DECLARED, never guessed (AC-3): nothing inspects the process tree or
    /// waits to see whether something binds. The repo says which runtimes need
    /// the port, in a form its author can read.
    #[serde(default)]
    pub runtimes: Vec<String>,
}

fn default_protocol() -> String {
    "tcp".into()
}

/// A port actually leased to a session, as the node is told about it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LeasedPort {
    pub name: String,
    pub env: String,
    pub port: i32,
}

/// One held port: which session has it, which requirement it satisfies, and
/// enough about that session for a human to decide whether releasing it is
/// safe.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PortLease {
    pub session_id: SessionId,
    pub session_name: String,
    pub status: String,
    /// The requirement's name and env var — so the UI can say *which* listener
    /// holds the port rather than just that something does.
    pub name: String,
    pub env: String,
    pub port: i32,
}

/// `PUT /workspaces/{id}/ports` — replace a workspace's port requirements.
///
/// A full replacement, not a patch: a partial update of a set is ambiguous
/// about deletion (the same reason `SetNodePlacementRequest` replaces).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetPortRequirementsRequest {
    /// `null` clears the declaration, returning the workspace to the default
    /// single `NOOK_PORT` listener; an empty list means "this workspace binds
    /// nothing", which is a different statement and is honoured as one.
    pub requirements: Option<Vec<PortRequirement>>,
}

/// `PUT /nodes/{id}/ports` — set or clear the operator's range. Both `None`
/// clears it back to whatever the node advertises.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetNodePortsRequest {
    #[serde(default)]
    pub start: Option<i32>,
    #[serde(default)]
    pub end: Option<i32>,
}

/// Ports to rule out on a node. An EMPTY list clears them — there is no
/// "unset" here, because a separate absent/empty distinction is exactly what
/// makes the range endpoint's both-or-neither rule easy to trip over.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetNodePortExclusionsRequest {
    #[serde(default)]
    pub ports: Vec<i32>,
}

/// Replace a node's operator-set labels and taints (MAIN-314). Both fields are
/// a full replacement, not a patch: a partial update of a set is ambiguous
/// about deletion.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetNodePlacementRequest {
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub taints: Vec<NodeTaint>,
}

/// How many sessions a workspace wants (MAIN-315).
///
/// Tagged rather than a bare integer, because "one per matching node" and
/// "exactly one" are different intents a number cannot tell apart — and a
/// reconciler that guessed would be wrong on a fleet that grows.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Replicas {
    /// Exactly `count` sessions, anywhere that matches.
    Count { count: u32 },
    /// Exactly one — the common case, spelled so it cannot be typo'd into two.
    Single,
    /// One on every node that matches the selector; grows with the fleet.
    All,
}

/// Work's agreement to run somewhere despite a taint (MAIN-315).
///
/// Its own type rather than a re-use of the node-side taint: they are the two
/// halves of one negotiation, and a shared struct makes it easy to pass one
/// where the other belongs. Same fields today, free to diverge later.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Toleration {
    pub key: String,
    pub effect: String,
}

/// A workspace's declared desired session state (MAIN-315) — the Deployment
/// analog. Absent entirely means the workspace is unmanaged.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionSpec {
    /// The runtime every session it manages should run — `claude`, `bash`, …
    pub runtime: String,
    /// Labels a node must carry to be eligible. Empty matches every node.
    #[serde(default)]
    pub node_selector: std::collections::BTreeMap<String, String>,
    /// Taints this work accepts. A node whose taint is untolerated is not
    /// eligible however well its labels match.
    #[serde(default)]
    pub tolerations: Vec<Toleration>,
    pub replicas: Replicas,
}

/// A node the reconciler cannot use yet, and why (MAIN-319).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReconcileBlocker {
    pub node_id: NodeId,
    pub node_name: String,
    /// `needs_clone` — matches the selector and tolerates the taints, but the
    /// workspace's checkout is not on it yet (MAIN-317 clones it).
    pub reason: String,
}

/// Desired versus actual for one workspace (MAIN-319 AC-3).
///
/// Computed by the SAME planner the reconciler runs, so the number on screen is
/// the number the loop is acting on rather than a second opinion about it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReconcileStatus {
    /// Whether reconciling is on for this tenant. A workspace can declare a
    /// spec with the switch off, and then nothing converges — which looks
    /// identical to "broken" unless the UI says so.
    pub enabled: bool,
    /// `false` when the workspace declares no spec at all: unmanaged, which is
    /// not the same as managed-and-wanting-zero.
    pub managed: bool,
    pub desired: u32,
    /// Live managed sessions right now.
    pub running: u32,
    /// `desired - placed`: asked for more than the fleet can host.
    pub shortfall: u32,
    /// This workspace declares no ports, so it is held to one session per node
    /// (MAIN-361), whatever its spec asks for.
    ///
    /// Carried separately from `blocked` because it is a different condition
    /// with a different fix: `needs_clone` clears itself when a clone lands and
    /// an ineligible fleet clears when a node matches, but this one clears only
    /// when somebody says what the repo binds. A UI that folded them together
    /// would offer "wait" for a state that never resolves on its own.
    #[serde(default)]
    pub port_capped: bool,
    /// Nodes that match but cannot be used yet.
    pub blocked: Vec<ReconcileBlocker>,
    /// How many nodes match the selector and tolerate the taints.
    pub eligible: u32,
}

/// Set or clear a workspace's [`SessionSpec`] (MAIN-315). `spec: null` clears
/// it, returning the workspace to unmanaged — which is why the field is an
/// explicit Option rather than an absent key meaning "leave alone".
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetSessionSpecRequest {
    pub spec: Option<SessionSpec>,
}

/// A workspace's review-loop declaration (MAIN-445), as the API reports it.
///
/// `max_replicas: null` is UNSET — the build's default ceiling of one applies.
/// It is a different statement from `0`, which is an explicit "do not review
/// this repo", and the two must stay distinguishable all the way out to the
/// caller; flattening them here is what would make the CLI unable to say
/// "unset (default 1)".
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewLoopDeclaration {
    pub max_replicas: Option<i32>,
}

/// A review run's conclusion, sent by the run itself (MAIN-455).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReviewVerdictRequest {
    /// `approved` | `changes_requested` | `needs_human` | `skipped`.
    pub verdict: String,
    /// The verdict body posted under `Loop review of <sha>`. Required unless
    /// the verdict is `skipped`, which posts nothing — the earlier review it
    /// defers to is already on the PR.
    #[serde(default)]
    pub body: Option<String>,
}

/// What a manual "review this workspace now" actually did (MAIN-455).
///
/// Not a single job: the manual path converges exactly as the reconciler does
/// — one directed run per pull request that is owed one — so the honest answer
/// is the set it raised and the reasons anything was not.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewRaiseResult {
    /// The runs this call raised, one per owed pull request.
    pub raised: Vec<LoopJob>,
    /// PRs already being reviewed right now — covered, not skipped.
    pub live: u32,
    /// PRs owed a run that the workspace's ceiling held back this pass.
    pub withheld: u32,
}

/// Desired versus actual for a workspace's REVIEW LOOP (MAIN-447 AC-4).
///
/// Separate from [`ReconcileStatus`], which reports the workspace's own
/// `SessionSpec` and deliberately excludes this purpose. Two declarations
/// converge per workspace and they must not be able to describe each other.
///
/// Computed by the SAME planner the reconciler runs, for the same reason
/// [`ReconcileStatus`] is: a ceiling above what the fleet can place has to read
/// as shortfall rather than as silent success, and a second calculation would
/// drift the moment `review_loop_spec` changes — which MAIN-448 will do when a
/// forge starts counting open PRs.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewLoopStatus {
    /// `sessions.reconcile.enabled` for the tenant.
    ///
    /// Both gates are reported separately, rather than as one `enabled`, because
    /// the UI's job is to say WHICH switch is off — the two live in different
    /// places and a person who turns on the wrong one has learnt nothing.
    pub reconcile_enabled: bool,
    /// `loops.enabled` for the tenant. The review loop is agent work, so it
    /// answers to this as well.
    pub loops_enabled: bool,
    /// What the ceiling resolves to: `null` -> 1, `0` -> 0, `N` -> N.
    pub desired: u32,
    /// Live review-loop sessions right now.
    pub running: u32,
    /// `desired - placed`: asked for more reviewers than the fleet can host.
    pub shortfall: u32,
    /// This workspace declares no ports, so it is held to one session per node
    /// (MAIN-361). Carried separately from `blocked` because it is the one
    /// cause of shortfall that never clears on its own.
    pub port_capped: bool,
    /// Nodes that match `role=loop` but cannot be used yet.
    pub blocked: Vec<ReconcileBlocker>,
    /// How many nodes match the selector and tolerate the taints.
    pub eligible: u32,
}

/// Set or clear a workspace's review-loop ceiling (MAIN-445).
///
/// The field is a raw JSON value on purpose. Typed as `Option<i32>` it would be
/// axum's 422 that answered `{"max_replicas": "3"}` or `-1`, and AC-2 asks for
/// a 400 NAMING the field for anything that is not a non-negative integer — so
/// the parse has to happen where the error message can be written.
///
/// The key itself is required, matching [`SetSessionSpecRequest`]: an absent
/// key is a malformed request, not a silent "leave it alone".
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetReviewLoopRequest {
    #[schema(value_type = Option<i32>)]
    pub max_replicas: serde_json::Value,
}

/// Toggle a node's `shared` designation (MAIN-135). Owner-only at the route.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetSharedRequest {
    pub shared: bool,
}

/// The owner's veto on operator-authorize for one machine (MAIN-276 AC-6).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetOperatorAuthorizeOptoutRequest {
    pub optout: bool,
}

// ── Skills ───────────────────────────────────────────────────────────────────

/// A skill taught to the whole fleet.
///
/// Stored by the control plane rather than pushed and forgotten, so that a node
/// which was offline when it was taught — or which joins next week — converges
/// on register instead of quietly being the one machine that never learned it.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Skill {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    /// Becomes a path component on every machine: `<skills>/<name>/SKILL.md`.
    pub name: String,
    pub content: String,
    /// Of `content`. Lets a node skip a write it already has, and lets an
    /// operator see whether two machines really do hold the same thing.
    pub sha256: String,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_by: Option<uuid::Uuid>,
}

/// The same thing without its body — a list of twenty skills should not ship
/// twenty documents to draw a table.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct SkillSummary {
    pub id: uuid::Uuid,
    pub name: String,
    pub sha256: String,
    /// Bytes, so the UI can show a size without holding the content.
    pub size: i64,
    pub updated_at: DateTime<Utc>,
    /// Display name of whoever last taught it, resolved for the fleet-manager
    /// panel (MAIN-106). `None` when the user row is gone (e.g. left the tenant).
    #[serde(default)]
    pub updated_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TeachRequest {
    /// Omitted means "derive it": from the document's own frontmatter `name:`,
    /// falling back to the filename. Explicit wins, because a file called
    /// SKILL.md says nothing about what it teaches.
    #[serde(default)]
    pub name: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TeachResponse {
    pub skill: SkillSummary,
    /// Nodes the fan-out actually reached. The rest converge on reconnect.
    pub delivered_to: Vec<String>,
    /// Nodes known to this tenant that were offline, named rather than
    /// counted — "3 nodes were offline" is not something an operator can act
    /// on, and silence about them would be worse.
    pub offline: Vec<String>,
}

/// A piece of centrally-managed fleet content — the managed `nookos` skill or
/// the managed hook set (MAIN-78). The control plane seeds it from the binary's
/// embedded defaults and holds it as the source of truth the rest of the
/// fleet-controlled-skills epic reads from.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct ManagedContent {
    pub id: uuid::Uuid,
    /// `skill` or `hooks`.
    pub kind: String,
    /// The skill name (e.g. `nookos`), or `default` for the single hook set.
    pub name: String,
    /// The apply-ready body: a `SKILL.md` for a skill, or the
    /// `~/.claude/settings.json` `hooks` fragment (JSON) for the hook set.
    pub content: String,
    /// Of `content` — lets a node skip an apply it already has.
    pub sha256: String,
    /// Monotonic: bumped when a newer shipped default refreshes the row (or, in
    /// a later sub-ticket, when an operator edits it).
    pub version: i64,
    pub updated_at: DateTime<Utc>,
}

// ── Workspaces ───────────────────────────────────────────────────────────────

/// Pin or unpin a workspace's git credential (MAIN-367). `null` unpins, which
/// returns the workspace to the node's own key.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SetWorkspaceCredentialRequest {
    #[serde(default)]
    pub credential_id: Option<GitCredentialId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub tenant_id: TenantId,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    /// The repository this workspace is — the clone URL, lifted off the scattered
    /// checkout rows so the UI can say what repo it is and clone-to-node needs no
    /// re-supplied URL (MAIN-223). NULL when the workspace's checkouts disagree
    /// or none carry a remote.
    pub git_remote_url: Option<String>,
    /// The normalized form of [`Self::git_remote_url`] used to match a discovered
    /// checkout back to its workspace (host+path, scheme/creds/`.git` stripped).
    pub git_remote_normalized: Option<String>,
    /// The desired session state (MAIN-315), or `None` for an UNMANAGED
    /// workspace. Nothing reconciles it yet.
    pub session_spec: Option<serde_json::Value>,
    /// The workspace's declared port requirements (MAIN-301), as stored JSON —
    /// `[{"name":"web","env":"PORT",…}]`. `None` means undeclared, which falls
    /// back to the default single listener; an empty array means "binds
    /// nothing" and is honoured as the different statement it is.
    #[serde(default)]
    pub port_requirements: Option<serde_json::Value>,
    /// Which stored ssh key this repo clones and fetches with (MAIN-367).
    ///
    /// The ID only — the private half never leaves the control plane except as
    /// transient material delivered for a single git command. `None` means
    /// unpinned, and unpinned falls back to the node's own generated key, which
    /// is what public repos and local paths have always used.
    #[serde(default)]
    pub git_credential_id: Option<GitCredentialId>,
    /// The CEILING on always-on review loops for this repo (MAIN-445).
    ///
    /// A ceiling rather than a count: the target is
    /// `desired = min(open_prs, max_replicas)`, so reviewers scale to the work
    /// and stop here. Nothing can measure open PRs yet, so today the ceiling IS
    /// the count — but the name has to be honest now, or the forge changes what
    /// a shipped field means.
    ///
    /// `None` is UNSET, not zero: the build's default ceiling of one applies,
    /// which is what every workspace reads on upgrade. `Some(0)` is an explicit
    /// "off" — a managed review session it already has is stopped, and unlike a
    /// repo idling at zero it never scales back up. `Some(n)` allows at most n;
    /// placement beyond one per node is MAIN-446's, so n>1 currently reports
    /// shortfall rather than being silently capped.
    #[serde(default)]
    pub review_loop_max_replicas: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A workspace checked out at a path on a particular node — the join table
/// that lets one workspace exist on many machines.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct NodeWorkspace {
    pub id: NodeWorkspaceId,
    pub tenant_id: TenantId,
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub git_remote_url: Option<String>,
    pub git_branch: Option<String>,
    pub git_status: serde_json::Value,
    pub discovered_at: DateTime<Utc>,
    pub last_scanned_at: DateTime<Utc>,
}

/// A workspace location as presented to the UI (node join included).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceLocation {
    pub node_id: NodeId,
    pub node_name: String,
    pub node_status: String,
    pub path: String,
    pub git_branch: Option<String>,
    pub dirty: bool,
    /// This checkout is a linked git worktree of the workspace's primary repo.
    #[serde(default)]
    pub worktree: bool,
}

// ── Sessions ─────────────────────────────────────────────────────────────────

/// What a MANAGED session exists to do (MAIN-326).
///
/// The reconciler runs two declarations per workspace, and this is what keeps
/// them apart: `sessions_one_managed_per_checkout_purpose` is unique on
/// `(checkout_id, managed_purpose)`, so a repo's clone can hold both a person's
/// terminal and the always-on review loop without either counting as the
/// other's replica. Without it the second declaration's session would be read
/// as a duplicate of the first and stopped on the next pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManagedPurpose {
    /// A terminal for a person to attach to — everything children 1-6 of the
    /// declarative-sessions epic reconcile, and what an unmanaged session's
    /// `NOT NULL` column reads as.
    #[default]
    Access,
    /// The control plane's own always-on review loop for the repo, run on a
    /// `role=loop` node. Declared by the control plane, never by a workspace.
    ReviewLoop,
}

impl ManagedPurpose {
    /// The stored form. A plain string column rather than an enum type, so
    /// adding a purpose is a code change and not a migration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::ReviewLoop => "review_loop",
        }
    }

    /// Decode a stored value, falling back to [`Access`](Self::Access) on
    /// anything this build does not know.
    ///
    /// The fallback cannot mis-steer the reconciler: it selects by purpose in
    /// SQL (`live_managed`), so a row written by a newer build is simply not
    /// returned to a planner that never asked for it — it is left running
    /// rather than adopted and stopped. This decode only ever decides what the
    /// session API *reports*, and reporting the default beats failing the whole
    /// row read and making the session vanish from every list.
    fn from_stored(raw: &str) -> Self {
        match raw {
            "review_loop" => Self::ReviewLoop,
            _ => Self::Access,
        }
    }
}

impl std::fmt::Display for ManagedPurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl nook_db::IntoDbValue for ManagedPurpose {
    fn into_db_value(self) -> nook_db::DbValue {
        nook_db::DbValue::Text(Some(self.as_str().to_string()))
    }
}

impl nook_db::FromDbColumn for ManagedPurpose {
    fn from_db_column(row: &nook_db::DbRow, name: &str) -> Result<Self, nook_db::DbError> {
        Ok(Self::from_stored(&row.get::<String>(name)?))
    }
    fn from_db_column_at(row: &nook_db::DbRow, index: usize) -> Result<Self, nook_db::DbError> {
        Ok(Self::from_stored(&row.get_at::<String>(index)?))
    }
}

/// Status values: `starting` | `running` | `detached` | `exited` | `error`.
/// Runtime is an open string: "claude", "hermes", "codex", "bash", "zsh", ...
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Session {
    pub id: SessionId,
    pub tenant_id: TenantId,
    /// The workspace this session runs in, or `None` for an ad-hoc terminal —
    /// a plain shell opened on a machine with no project behind it, running in
    /// the node's home directory.
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: NodeId,
    pub name: String,
    pub runtime: String,
    pub tmux_session: Option<String>,
    pub status: String,
    /// Why the session failed to start, when it did.
    pub error: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// The checkout row this session runs in (MAIN-222). NULL for an ad-hoc
    /// `$HOME` terminal, or once the checkout it started in has been pruned.
    pub checkout_id: Option<NodeWorkspaceId>,
    /// Who owns this session: the reconciler (`true`, started for the
    /// workspace's [`SessionSpec`]) or a person (`false`, hand-started).
    ///
    /// Stored since MAIN-316 and surfaced here by MAIN-318, because it is the
    /// one thing about a session a caller cannot work out for itself — a
    /// hand-started terminal in a managed workspace, on an eligible node, with
    /// the spec's runtime, looks exactly like a replica.
    ///
    /// It decides which removal a UI may offer. Killing a managed session is
    /// not removing it: the next reconcile pass sees a checkout with no live
    /// managed session and starts another. Removing one means editing the
    /// declaration — lower the workspace's replicas — and the only honest way
    /// to offer that button is to know which kind of session this is.
    #[serde(default)]
    pub managed: bool,
    /// What the reconciler is keeping this session for (MAIN-326). Meaningless
    /// on a hand-started session, where it reads [`ManagedPurpose::Access`]
    /// because the column is `NOT NULL`.
    #[serde(default)]
    pub managed_purpose: ManagedPurpose,
    /// The ports leased to this session (MAIN-301), one per satisfied
    /// [`PortRequirement`], each delivered into the session as its own env
    /// var. Empty when the node offers no range or the workspace declares no
    /// listeners. Not a stored column — the leases are their own rows, joined
    /// in by the session endpoints.
    #[serde(default)]
    #[db(skip)]
    pub leased_ports: Vec<LeasedPort>,
    /// Denormalised summary of `checkout_id` for the UI — filled by the session
    /// endpoints, not a stored column, so it is absent from a raw `FROM sessions`
    /// row (hence `#[db(skip)]`).
    #[serde(default)]
    #[db(skip)]
    pub checkout: Option<CheckoutSummary>,
    /// Whether the session's node holds a LIVE WebSocket right now — the honest
    /// signal `nodes.status` cannot give, because a seeded/synthetic node can
    /// read `online` in the database while never having connected. `None` when
    /// not computed (list rows); the detail endpoint fills it from the registry
    /// so the UI can tell "the terminal is streaming" from "its node is gone"
    /// instead of retrying a dead attach forever. Not stored (`#[db(skip)]`).
    #[serde(default)]
    #[db(skip)]
    pub node_online: Option<bool>,
    /// Which slice of the repo's work this managed session owns (MAIN-446).
    ///
    /// Stored on the row, not derived, for two reasons the planner depends on.
    /// It is part of the unique index, so it is what lets several reviewers
    /// share one clone instead of reading as each other's duplicate. And it is
    /// what a RESTART re-sends: a reviewer that came back as shard 0 when it
    /// had been shard 2 would re-review another reviewer's PRs and skip its
    /// own.
    ///
    /// `0 of 1` on everything else — a hand-started terminal, and every row
    /// that predates this column.
    #[serde(default)]
    pub managed_shard: i32,
    /// How many shards the declaration was divided into when this session was
    /// placed (MAIN-446). The divisor half of `managed_shard`; the two are only
    /// meaningful together.
    ///
    /// Stored rather than re-read from the workspace's ceiling, so a session
    /// keeps partitioning the way it was told to. A ceiling that CHANGES makes
    /// every live session's divisor stale, which the planner handles by
    /// replacing them — a re-partition is a deliberate, visible event, not
    /// something a running agent discovers mid-review.
    #[serde(default = "one_shard")]
    pub managed_shards: i32,
}

/// The divisor of an unsharded session: one shard, which is every PR.
fn one_shard() -> i32 {
    1
}

/// One reviewer's slice of a repo's open PRs (MAIN-446).
///
/// Travels together because neither half means anything alone, and because the
/// two must not be able to disagree on the wire — `index` is only ever valid
/// against the `of` it was computed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardAssignment {
    /// This session's shard, `0..of`.
    pub index: u32,
    /// How many shards the work is divided into. Never zero.
    pub of: u32,
}

impl ShardAssignment {
    /// The whole of the work: the only assignment an unsharded session has, and
    /// what every non-review session is.
    pub const SOLO: Self = Self { index: 0, of: 1 };

    /// Does this shard own that PR? The partition rule itself, in one place, so
    /// the reconciler's tests and the skill's text cannot describe different
    /// arithmetic (AC-3/AC-4).
    pub fn owns(self, pr_number: u64) -> bool {
        self.of <= 1 || pr_number % self.of as u64 == self.index as u64
    }
}

/// Where a session runs, in the shape the UI needs to show it (MAIN-222 AC-5):
/// the checkout's id, path, branch, kind, and the node it lives on.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct CheckoutSummary {
    pub id: NodeWorkspaceId,
    pub path: String,
    pub branch: Option<String>,
    /// `clone` | `worktree` | `mirror`.
    pub kind: String,
    pub node_name: String,
}

// ── Kanban ───────────────────────────────────────────────────────────────────

/// Provider values: `local` | `jira` | `github` | `linear` | `trello`.
/// External boards remain authoritative; NookOS federates.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Board {
    pub id: BoardId,
    pub tenant_id: TenantId,
    pub workspace_id: Option<WorkspaceId>,
    pub name: String,
    /// The prefix in `NOOK-42`. Unique per tenant, derived from the name when
    /// not given, and immutable once assigned — it is written into PR bodies
    /// and branch names, which no rename can reach back and fix.
    #[serde(default)]
    pub key: Option<String>,
    pub provider: String,
    /// Per-board automation (MAIN-73): a map from column TYPE to an ordered list
    /// of actions that fire when a task enters a column of that type. `{}` when
    /// unset. Validated server-side on write.
    #[serde(default)]
    pub automation: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct BoardColumn {
    pub id: ColumnId,
    pub board_id: BoardId,
    pub name: String,
    pub position: i32,
    /// What this column MEANS, independent of what it is called:
    /// `backlog` | `unstarted` | `started` | `review` | `completed` | `canceled`.
    ///
    /// Automation targets the type so that renaming "In Progress" to "Doing"
    /// is a cosmetic change rather than a broken loop. The name is for people.
    #[serde(default = "default_column_type")]
    pub r#type: String,
}

fn default_column_type() -> String {
    "unstarted".into()
}

/// The default issue type — a task read back before/without a type is a `task`
/// (MAIN-59 AC-5), matching the column's DB default.
fn default_task_type() -> String {
    "task".into()
}

/// The default task visibility — `team` reproduces today's behaviour (visible to
/// the whole tenant), matching the column's DB default (MAIN-76).
fn default_visibility() -> String {
    "team".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct TaskItem {
    /// Issue type: one of `task`, `bug`, `epic`, `story`, `chore` (MAIN-59).
    /// Named `type_` because `type` is a Rust keyword; the wire/column name is
    /// `type`.
    #[serde(rename = "type", default = "default_task_type")]
    #[db(rename = "type")]
    pub type_: String,
    pub id: TaskId,
    pub tenant_id: TenantId,
    pub board_id: BoardId,
    pub column_id: ColumnId,
    pub title: String,
    pub description: Option<String>,
    pub position: i32,
    pub external_id: Option<String>,
    pub external_url: Option<String>,
    pub assignee_user_id: Option<UserId>,
    /// Who may see this card (MAIN-76): `private` (creator + assignee only),
    /// `team` (the whole tenant — the default), or `org`. Enforced server-side
    /// on every read and claim path, not by an RBAC permission.
    #[serde(default = "default_visibility")]
    pub visibility: String,
    /// The per-tenant `users.id` of the creator. `None` for rows created before
    /// visibility existed (ownerless team cards).
    #[serde(default)]
    pub created_by: Option<UserId>,
    pub workspace_id: Option<WorkspaceId>,
    /// The epic this task hangs off, or `None` for a top-level task (MAIN-81).
    /// The parent is always a `type='epic'` task on the same board; an epic
    /// never has a parent (no nesting). Cleared to `None` if the epic is deleted.
    #[serde(default)]
    pub parent_task_id: Option<TaskId>,
    /// Node the triage scheduler chose (or you forced) to run this work.
    pub assigned_node_id: Option<NodeId>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_node_id: Option<NodeId>,
    /// The checkout this task's working directory **is** (MAIN-225) — id is
    /// identity, path is an attribute. Set to the worktree's `node_workspaces`
    /// row once discovery has scanned it; `NULL` until then, or with no worktree.
    /// Written alongside the legacy `worktree_path`/`worktree_node_id`, which
    /// item 7 will retire.
    pub checkout_id: Option<NodeWorkspaceId>,
    pub session_id: Option<SessionId>,
    /// When this card's **agent** claim lapses (MAIN-229). Set only by the agent
    /// claim / start-work path; `None` for a card a human put in progress by
    /// hand, and that `None` is the fence the claim reaper is confined to — an
    /// unleased card is never examined, moved or labelled by it.
    #[serde(default)]
    pub claim_expires_at: Option<DateTime<Utc>>,
    pub pr_url: Option<String>,
    /// Transient dispatch signal (MAIN-227), not a stored column: `true` when the
    /// just-assigned node has no clone checkout of the task's workspace, so the
    /// caller must clone there before start-work. Set only on the `dispatch`
    /// result; `false` (its default) on every other read.
    #[serde(default)]
    #[db(skip)]
    pub needs_clone: bool,
    /// `0` none, `1` urgent, `2` high, `3` medium, `4` low — Linear's
    /// convention, so values port cleanly. Note `0` sorts LAST: "nobody set a
    /// priority" is not a claim that the work is least important.
    #[serde(default)]
    pub priority: i32,
    /// Per-board sequence behind the human key. `None` only for a task created
    /// before keys existed and not yet backfilled.
    #[serde(default)]
    pub number: Option<i32>,
    /// `NOOK-42` — the board's key and this task's number. Computed, not
    /// stored: storing it would let it disagree with the two columns it is
    /// made of.
    #[serde(default)]
    #[db(skip)]
    pub key: Option<String>,
    /// Absolute deep link into the web UI, so an agent reporting "filed
    /// NOOK-42" can give a human something to click.
    #[serde(default)]
    #[db(skip)]
    pub url: Option<String>,
    /// The parent epic's human key (`NOOK-7`), when this task has a parent.
    /// Computed like `key`, so a reader can show "under NOOK-7" without a second
    /// lookup (MAIN-81).
    #[serde(default)]
    #[db(skip)]
    pub parent_key: Option<String>,
    /// Every label on this task. Populated by one query for a whole board
    /// rather than one per task.
    #[serde(default)]
    #[db(skip)]
    pub labels: Vec<Label>,
    /// When this task was archived off the board, or `None` while it is live.
    /// Archived tasks are hidden from the board by default and excluded from the
    /// agent pick query, but the row is preserved and unarchiving clears this.
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Labels, comments, relations ─────────────────────────────────────────────

/// A tenant-wide label. `agent-ready` is the human approval gate: the one
/// signal that says an agent may pick this up, and deliberately not something
/// an agent can apply to itself.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Label {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub name: String,
    pub color: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateLabelRequest {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

/// Durable discussion on a task: the builder's blocking question, the
/// reviewer's verdict, the human's answer.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct TaskComment {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub task_id: TaskId,
    /// `user` | `agent` | `system`.
    pub author_type: String,
    #[serde(default)]
    pub author_id: Option<Uuid>,
    /// Denormalised, so an agent with no users row — and a deleted user —
    /// still render with attribution.
    pub author_name: String,
    pub body_md: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateCommentRequest {
    pub body_md: String,
    /// How an agent signs its work, e.g. `"loop-build on azul"`.
    ///
    /// NookOS has no separate agent identity — an agent acts under a person's
    /// token, so the honest record is "this user's credential, used by this
    /// tool". Supplying a name says which tool; it does not grant anything,
    /// and the underlying `author_id` remains the real user.
    #[serde(default)]
    pub author_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateCommentRequest {
    pub body_md: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct TaskRelation {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub from_task: TaskId,
    pub to_task: TaskId,
    /// `blocks` | `relates` | `duplicates`.
    pub kind: String,
    pub created_at: DateTime<Utc>,
}

/// The other end of a relation, with enough to render it without a second
/// fetch.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct RelatedTask {
    pub relation_id: Uuid,
    pub id: TaskId,
    #[serde(default)]
    pub key: Option<String>,
    pub title: String,
    pub kind: String,
    /// The column type of the other task — what makes a blocker resolved.
    pub column_type: String,
}

/// One child ticket of an epic, for the epic's detail (MAIN-81). `done`/`total`
/// is derivable by the reader: a child is done when its `column_type` is
/// `completed` or `canceled`.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct EpicChild {
    pub id: TaskId,
    #[serde(default)]
    pub key: Option<String>,
    pub title: String,
    #[serde(rename = "type")]
    #[db(rename = "type")]
    pub type_: String,
    pub priority: i32,
    /// The column TYPE the child sits in — so progress is derivable inline.
    pub column_type: String,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateRelationRequest {
    pub to_task: TaskId,
    pub kind: String,
}

/// One whole issue: what the loop reads before it starts work.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TaskDetail {
    pub task: TaskItem,
    pub comments: Vec<TaskComment>,
    /// Tasks that must finish before this one can start.
    pub blocked_by: Vec<RelatedTask>,
    /// Tasks waiting on this one.
    pub blocking: Vec<RelatedTask>,
    /// Non-blocking links (`relates`, `duplicates`), both directions.
    pub related: Vec<RelatedTask>,
    /// Derived from the blockers' column types, never stored — a stored flag
    /// would drift the moment a blocker moved.
    pub is_blocked: bool,
    /// When this task is an epic, the tickets filed under it (MAIN-81). Empty
    /// for a non-epic or a childless epic.
    #[serde(default)]
    pub children: Vec<EpicChild>,
}

/// `POST /tasks/{id}/claim` — take the work without racing another agent.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct ClaimTaskRequest {
    /// Move the task here at the same time, by column TYPE. Omit to claim
    /// without moving.
    #[serde(default)]
    pub column_type: Option<String>,
    /// Claim on behalf of this user rather than the caller. For a human
    /// assigning work; agents omit it.
    #[serde(default)]
    pub assignee_user_id: Option<UserId>,
    /// The session this claim was made from (MAIN-142 AC-4), so the control
    /// plane can refuse a build-loop claim running on a shared operator. The
    /// CLI fills it from `NOOK_SESSION_ID` when it has one; a claim made
    /// outside a session simply has none, and is out of the check's reach.
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

/// One account you can sign in as, in dev mode only.
///
/// Exists so a person can switch between users without inventing credentials —
/// testing "what does an operator see that a member does not" is impossible if
/// becoming the other person is hard.
// FromRow is hand-written (see the impls near end of file): `deployment_roles`
// is a Postgres text[] with no SQLite `Decode`, so the derive can't cover both
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct DevAccount {
    pub email: String,
    pub display_name: String,
    pub tenant_slug: String,
    /// Role keys held at the deployment scope — so the picker can show which
    /// of these accounts is the operator without you having to remember.
    #[serde(default)]
    pub deployment_roles: Vec<String>,
}

/// The dev account picker's response: a capped page plus the full match count,
/// so the UI can show a "N more — refine" hint when the list is truncated
/// (MAIN-221 AC-4).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DevAccountsResponse {
    pub accounts: Vec<DevAccount>,
    /// Total accounts matching the (optional) search — may exceed
    /// `accounts.len()` when the page was capped.
    pub total: i64,
}

/// Result of the dev-only `purge-test-tenants` sweep (MAIN-221 AC-3).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PurgeTestTenantsResponse {
    /// How many `test-%` tenants were deleted (0 on a second, idempotent run).
    pub deleted: i64,
}

// ── Orgs, roles, and the operator surface ────────────────────────────────────

/// An org: the layer between a deployment and its tenants.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Org {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

/// What the signed-in caller may do, so a UI can hide what it cannot offer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Capability {
    /// Holds an operator binding somewhere — drives whether the operator
    /// section appears at all.
    pub operator: bool,
    /// Permission keys held at the deployment scope.
    #[serde(default)]
    pub deployment: Vec<String>,
    /// The org this caller's tenant belongs to, for reading its policy.
    #[serde(default)]
    pub org_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct OperatorOrg {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub tenants: i64,
}

/// A tenant as an operator sees it.
///
/// The first block is always visible: existence, counts, load. Everything
/// after is `Option` and stays `None` unless the org opted in — policy ADDS
/// these fields rather than filtering them out, so forgetting to add one
/// leaves it absent instead of leaking it.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct OperatorTenant {
    pub id: TenantId,
    pub slug: String,
    #[serde(default)]
    pub org_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub members: i64,
    pub nodes: i64,
    pub active_sessions: i64,
    pub workspaces: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[db(skip)]
    pub repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[db(skip)]
    pub task_titles: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct OperatorNode {
    pub id: NodeId,
    pub name: String,
    pub platform: String,
    pub status: String,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
    pub resources: serde_json::Value,
    pub tenant_id: TenantId,
    pub tenant_slug: String,
    pub active_sessions: i64,
}

/// An audit row. Kinds, actors and times — never payloads, which can carry the
/// very metadata policy exists to gate.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct OperatorAuditEntry {
    pub id: EventId,
    pub kind: String,
    #[serde(default)]
    pub actor_type: Option<String>,
    #[serde(default)]
    pub actor_id: Option<Uuid>,
    pub tenant_id: TenantId,
    pub tenant_slug: String,
    pub occurred_at: DateTime<Utc>,
}

// The per-list `*Page` structs died in the QOL pagination sweep — every list
// returns [`Page<T>`] now, so a new list cannot invent its own page shape.

/// One policy-gated field with its current state and plain-language meaning.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PolicyField {
    pub field: String,
    /// Written for a person, not a developer — every user is shown this.
    pub description: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateOrgRequest {
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RenameOrgRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MoveTenantRequest {
    pub org_id: Uuid,
}

/// Who holds what, for the roles table.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct BindingRow {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role_key: String,
    pub scope_type: String,
    #[serde(default)]
    pub scope_id: Option<Uuid>,
    /// The org or tenant slug the binding is scoped to, when it has one.
    #[serde(default)]
    pub scope_label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Grant (or revoke) a deployment-scoped role.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GrantRequest {
    pub email: String,
    /// `operator` | `org_admin` | …
    pub role: String,
    #[serde(default)]
    pub revoke: bool,
    /// Grant over ONE tenant instead of the whole deployment.
    ///
    /// Absent keeps the old behaviour exactly — deployment scope — so every
    /// existing caller is unchanged. Present is what lets somebody be the admin
    /// of one team without being handed the entire deployment, which until now
    /// was the only thing this endpoint could do.
    #[serde(default)]
    pub tenant_id: Option<TenantId>,
}

/// The switches an operator can throw for a tenant OTHER than the one they are
/// acting in.
///
/// `settings::put` writes to the caller's active tenant, which meant turning
/// loops on for somebody else's team required switching into it first — and if
/// their team was never set up, nothing ran and nothing said why.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TenantSwitches {
    pub tenant_id: TenantId,
    pub tenant_name: String,
    /// `loops.enabled` — whether this tenant dispatches loop jobs at all
    /// (MAIN-239). Default off, which is why a fresh team's loops never fire.
    pub loops_enabled: bool,
    /// `sessions.reconcile.enabled` — whether the reconciler converges this
    /// tenant's workspaces onto its nodes. Default off.
    pub reconcile_enabled: bool,
}

/// Throw one of them.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetTenantSwitchRequest {
    /// `loops` or `reconcile` — a closed set, not a settings key, so this
    /// endpoint can never become a way to write arbitrary settings into
    /// somebody else's tenant.
    pub switch: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetPolicyRequest {
    pub field: String,
    pub enabled: bool,
}

// ── Notifications ────────────────────────────────────────────────────────────

/// Something a person should see. Distinct from an `Event`, which is the
/// complete record of what happened and is never marked read.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Notification {
    pub id: Uuid,
    pub tenant_id: TenantId,
    /// `None` means everyone in the tenant.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// `info` | `success` | `warning` | `error`.
    pub level: String,
    pub title: String,
    pub body: String,
    /// The dotted event kind that produced it, or `custom`.
    pub kind: String,
    /// Where clicking it should go.
    #[serde(default)]
    pub link: Option<String>,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub read_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationPage {
    pub notifications: Vec<Notification>,
    pub unread: i64,
}

/// Raise a notification by hand — what `nook notify` and an agent's finish
/// hook both call.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NotifyRequest {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    /// `info` | `success` | `warning` | `error`. Defaults to `info`.
    #[serde(default)]
    pub level: Option<String>,
    /// Defaults to `custom`. Channels filter on this.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
    /// The session this notification is about, when it comes from an agent hook.
    /// The control plane turns it into a deep link to the terminal (using its
    /// own public URL, which the node does not know), so clicking "an agent is
    /// waiting on you" opens the session — and external channels get a real URL,
    /// not a path. An explicit `link` still wins.
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// A configured delivery channel.
///
/// `config` is deliberately absent: it holds bot tokens and webhook URLs, and
/// a channel list is the sort of thing a UI fetches often and logs freely.
/// What a person needs to see is that it exists, whether it works, and what it
/// is filtered to.
// FromRow hand-written (see impls near end of file): `levels`/`kinds` are
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct NotificationChannel {
    pub id: Uuid,
    pub tenant_id: TenantId,
    /// `webhook` | `slack` | `discord` | `telegram` | `twilio` | `ntfy`.
    pub kind: String,
    pub name: String,
    pub enabled: bool,
    pub levels: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default)]
    pub last_ok_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateChannelRequest {
    pub kind: String,
    pub name: String,
    /// Provider-specific. Write-only: it is never read back.
    pub config: serde_json::Value,
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateChannelRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Omit to keep the stored secrets untouched.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub levels: Option<Vec<String>>,
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

/// What a channel kind needs, so the UI can build a form without hardcoding it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChannelKind {
    pub id: String,
    pub label: String,
    pub description: String,
    pub fields: Vec<ChannelField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChannelField {
    pub name: String,
    pub label: String,
    pub placeholder: String,
    /// Masked in the UI and never read back.
    pub secret: bool,
    pub required: bool,
}

/// One entry in the notification-kind catalog: an event kind the bell can
/// raise. The authoritative list a settings UI renders as a checklist, so a
/// per-channel `kinds` filter can be built from real options rather than a
/// free-text guess. `group` is the dotted prefix (`task.`, `node.`) the filter
/// already prefix-matches on, so a UI can offer per-group as well as per-kind.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationKind {
    pub id: String,
    pub label: String,
    pub description: String,
    pub group: String,
}

// ── Activity ─────────────────────────────────────────────────────────────────

/// Everything produces events. Kind is an open dotted string:
/// "node.connected", "session.started", "task.moved", "user.login", ...
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Event {
    pub id: EventId,
    pub tenant_id: TenantId,
    pub occurred_at: DateTime<Utc>,
    pub kind: String,
    pub actor_type: Option<String>,
    pub actor_id: Option<Uuid>,
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: Option<NodeId>,
    pub session_id: Option<SessionId>,
    pub payload: serde_json::Value,
}

// ── Notes ────────────────────────────────────────────────────────────────────

/// Kind values: `rolling` | `briefing` | `decision` | free-form.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Note {
    pub id: NoteId,
    pub tenant_id: TenantId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub content_md: String,
    pub kind: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Personal notebook (MAIN-66) ──────────────────────────────────────────────
// A separate, PERSON-owned resource — distinct from the workspace `Note` above.
// Keyed by `person_id` (a plain uuid, the platform-issued value), so a person
// sees the same notebook signed into any of their orgs. Note bodies are stored
// encrypted; only the decrypted `UserNote` carries `content_md`.

/// A folder in a person's notebook. `parent_id: None` is a root folder.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct UserNoteFolder {
    pub id: UserNoteFolderId,
    #[schema(value_type = String, format = Uuid)]
    pub person_id: Uuid,
    pub parent_id: Option<UserNoteFolderId>,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A note's metadata — the tree/list item and the search result. Never carries
/// the body: that is encrypted at rest and fetched (decrypted) one note at a
/// time. `path` is the plaintext folder path for display and search.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct UserNoteSummary {
    pub id: UserNoteId,
    pub folder_id: Option<UserNoteFolderId>,
    pub title: String,
    /// Plaintext folder path, e.g. "Work/Ideas". Empty for a root note.
    pub path: String,
    /// Whether this note is zero-knowledge sealed (MAIN-100). Titles and paths
    /// stay plaintext either way, so search still finds sealed notes.
    #[serde(default)]
    pub sealed: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The client-decrypt contract for a sealed note (MAIN-100). A browser derives
/// the key with WebCrypto `PBKDF2-HMAC-SHA256(passphrase, salt, iterations)` →
/// 32-byte AES-256 key, then `AES-256-GCM` opens `ciphertext`, whose first 12
/// bytes are the nonce and the rest the GCM body+tag. All byte fields base64.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SealedBlob {
    /// KDF salt, base64. 16 bytes.
    pub salt: String,
    /// PBKDF2 iteration count (matches the server's `crypto::KDF_ITERATIONS`).
    pub iterations: u32,
    /// `nonce(12) || AES-256-GCM(body)`, base64. The client-produced sealed
    /// ciphertext; the server stores it vault-wrapped and cannot open it.
    pub ciphertext: String,
}

/// A single note. Unsealed notes carry the decrypted `content_md`; a sealed
/// note carries `sealed: true` and the `blob` instead — the server returns the
/// ciphertext it cannot open, for the browser to decrypt (MAIN-100).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserNote {
    pub id: UserNoteId,
    pub folder_id: Option<UserNoteFolderId>,
    pub title: String,
    /// Present iff the note is not sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_md: Option<String>,
    /// True when the body is zero-knowledge sealed and `blob` is present.
    #[serde(default)]
    pub sealed: bool,
    /// Present iff `sealed`. The client-decrypt contract for the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<SealedBlob>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Whether a person has set their notebook app password (MAIN-100).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotebookVaultStatus {
    pub configured: bool,
    pub created_at: Option<DateTime<Utc>>,
}

/// Seal a note (MAIN-100). The client produces the blob locally from its app
/// password; `passphrase` proves that password against the person vault (the
/// `require_app_password` pattern). The server never sees the note plaintext.
/// All byte fields base64.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SealNoteRequest {
    /// KDF salt the client sealed under, base64.
    pub salt: String,
    /// One-way verifier of the client's derived key, base64.
    pub verifier: String,
    /// `nonce(12) || AES-256-GCM(body)`, base64 — the sealed body.
    pub ciphertext: String,
    /// The app password, to authorize the seal against the person vault.
    pub passphrase: String,
}

/// Unseal a note (MAIN-100): the client decrypted the sealed body locally and
/// sends back the recovered plaintext, converting the row to a normal
/// server-encrypted note. `passphrase` is verified against the person vault.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UnsealNoteRequest {
    pub content_md: String,
    pub passphrase: String,
}

/// Create a note. Body defaults to empty; `folder_id` None places it at root.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateUserNote {
    pub title: String,
    #[serde(default)]
    pub content_md: String,
    #[serde(default)]
    pub folder_id: Option<UserNoteFolderId>,
}

/// Update a note. Each field absent = leave alone. `folder_id` is tri-state:
/// absent = leave, `null` = move to root, an id = move into that folder.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateUserNote {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content_md: Option<String>,
    #[serde(default)]
    pub folder_id: Option<Option<UserNoteFolderId>>,
}

/// Create a folder. `parent_id` None makes it a root folder.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateUserNoteFolder {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<UserNoteFolderId>,
}

/// Update a folder. Rename and/or move; `parent_id` is tri-state like a note's
/// `folder_id` (absent = leave, `null` = move to root, an id = move under it).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateUserNoteFolder {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<Option<UserNoteFolderId>>,
}

// ── Themes ───────────────────────────────────────────────────────────────────

/// Design tokens applied as CSS custom properties. Every visual aspect is
/// configurable; unknown keys pass through untouched so theme packs can extend.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ThemeTokens {
    pub colors: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub fonts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub spacing: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub effects: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Theme {
    pub id: ThemeId,
    /// NULL = built-in theme shipped with NookOS.
    pub tenant_id: Option<TenantId>,
    pub name: String,
    pub slug: String,
    pub tokens: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

// ── Settings ─────────────────────────────────────────────────────────────────

/// Scope values: `tenant` | `user`.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Setting {
    pub id: SettingId,
    pub tenant_id: TenantId,
    pub scope: String,
    pub user_id: Option<UserId>,
    pub key: String,
    pub value: serde_json::Value,
}

// ── API request/response DTOs ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceDetail {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub locations: Vec<WorkspaceLocation>,
}

// ── Mission Control overview (MAIN-226) ──────────────────────────────────────

/// The whole fleet in one payload: every workspace the caller can see anything
/// of, its checkouts on visible nodes, and the active sessions bound to each —
/// the hierarchy repo → node → checkout → sessions. Visibility composes the
/// existing node (own+shared) and session (MAIN-133) rules exactly; it grants no
/// new powers, and a workspace with nothing visible is simply absent.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Overview {
    pub workspaces: Vec<OverviewWorkspace>,
    /// Ad-hoc `$HOME` terminals with no workspace — surfaced so no running
    /// session is invisible on the fleet view.
    pub loose_sessions: Vec<Session>,
}

/// One repository on the overview: its identity plus every visible checkout and
/// the sessions that could not be pinned under a visible checkout.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OverviewWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    pub git_remote_url: Option<String>,
    pub git_remote_normalized: Option<String>,
    pub checkouts: Vec<OverviewCheckout>,
    /// Sessions in this workspace not bound to a listed checkout (its checkout
    /// was pruned, or sits on a node the caller cannot see) — kept so work is
    /// never lost from the view.
    pub unbound_sessions: Vec<Session>,
}

/// A single checkout on a node, with the sessions running in it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OverviewCheckout {
    pub id: NodeWorkspaceId,
    pub node_id: NodeId,
    pub node_name: String,
    pub node_status: String,
    pub path: String,
    pub branch: Option<String>,
    /// `clone` | `worktree` | `mirror` — drives the kind badge and gates the
    /// "+ worktree" action (clone-only).
    pub kind: String,
    pub dirty: bool,
    /// When set, this checkout has vanished from disk (MAIN-220 tombstone). The
    /// UI ghosts it rather than hiding it.
    pub missing_at: Option<DateTime<Utc>>,
    pub sessions: Vec<Session>,
    /// The ticket(s) this checkout is working (MAIN-230) — what turns "some
    /// worktree on builder-1" into "MAIN-42 on builder-1". Empty for a checkout
    /// no task points at. Visibility-filtered exactly like any other read of a
    /// card, so a private ticket never surfaces here.
    #[serde(default)]
    pub tasks: Vec<OverviewTask>,
}

/// A ticket as Mission Control needs it: enough to label a row, link to the
/// board card, and tell done from in-flight. Deliberately not the whole
/// `TaskItem` — this rides on every checkout in the fleet.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OverviewTask {
    /// The board key (`MAIN-42`) — the label, and the `/board?task=` target.
    pub key: String,
    pub title: String,
    /// The task's column TYPE (`started`, `review`, `completed`, …), not its
    /// name: a board that renames "In Progress" must not restyle the chip.
    pub column_type: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub description: Option<String>,
    /// The git remote this workspace is a checkout of. With it set, the session
    /// reconciler's clone-on-demand can materialise the repo on eligible nodes —
    /// the declarative "New Workspace" path. Omitted for a bare/empty project.
    #[serde(default)]
    pub git_remote_url: Option<String>,
}

/// A new label for a workspace. The name is what people read; the slug, the
/// checkouts on disk and the git remote are its identity, and none of them
/// move — which is what makes this safe to do on a whim.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RenameWorkspaceRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BoardDetail {
    pub board: Board,
    pub columns: Vec<BoardColumn>,
    pub tasks: Vec<TaskItem>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateBoardRequest {
    pub name: String,
    /// Omit to derive one from the name.
    #[serde(default)]
    pub key: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateBoardRequest {
    pub name: String,
    /// Change the prefix in `NOOK-42`.
    ///
    /// Normally immutable — it is written into PR bodies and branch names that
    /// no rename can reach back and fix — but settable, because a key derived
    /// from a board name is sometimes just wrong ("NookOS Bootstrap" derives
    /// "NOOKO") and living with it forever is worse than an explicit change a
    /// person chose.
    #[serde(default)]
    pub key: Option<String>,
    /// Replace the board's automation rules (MAIN-73). Omitted leaves them
    /// unchanged; a value is validated (known kinds, valid column types, sound
    /// config) before it is stored.
    #[serde(default)]
    pub automation: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateColumnRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateColumnRequest {
    pub name: Option<String>,
    pub position: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: Option<String>,
    pub column_id: Option<ColumnId>,
    /// Place by semantic state instead of by id — what automation wants, since
    /// it knows "the backlog" but not which uuid that is today.
    #[serde(default)]
    pub column_type: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Issue type (MAIN-59). Omitted → defaults to `task`; an invalid value is
    /// rejected. Named `type_`; the wire name is `type`.
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    /// Who may see this card (MAIN-76): `private`/`team`/`org`. Omitted →
    /// defaults to `team`; an invalid value is rejected.
    #[serde(default)]
    pub visibility: Option<String>,
    /// File this task under an epic (MAIN-81): a uuid OR a key (`NOOK-7`),
    /// tenant-scoped. Must resolve to a `type='epic'` task on the same board.
    /// Omitted → top-level.
    #[serde(default)]
    pub parent: Option<String>,
    /// Label NAMES, created for the tenant if new. Names rather than ids
    /// because a filer knows `agent-ready`, not its uuid.
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateTaskRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub column_id: Option<ColumnId>,
    #[serde(default)]
    pub column_type: Option<String>,
    pub position: Option<i32>,
    pub assignee_user_id: Option<UserId>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Change the issue type (MAIN-59). Absent leaves it unchanged; an invalid
    /// value is rejected. Named `type_`; the wire name is `type`.
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    /// Which workspace this task belongs to. Absent leaves it alone, `null`
    /// clears it, an id sets it.
    ///
    /// Nested because those are three cases, not two, and every other field
    /// here only has two. A confined `/loop-build` agent claims only tasks in
    /// its own workspace, so an unscoped task is one no loop will ever pick
    /// up — and until this field existed there was no way to scope one after
    /// filing it. Clearing has to be expressible too, or a wrong answer is
    /// permanent.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, nullable)]
    pub workspace_id: Option<Option<WorkspaceId>>,
    /// Change who may see this card (MAIN-76): `private`/`team`/`org`. Absent
    /// leaves it unchanged; an invalid value is rejected.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Re-file under an epic, or detach (MAIN-81). Absent = unchanged, `null` =
    /// detach (become top-level), a uuid/key = move under that epic (validated
    /// like `create`). Tri-state for the same reason `workspace_id` is.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, nullable)]
    pub parent: Option<Option<String>>,
    /// Optimistic-concurrency precondition. When set, the update applies only
    /// if the task's current `updated_at` still equals this; a mismatch makes
    /// NO change and returns `409 Conflict` (the body changed under the caller).
    /// The body-editing surfaces send it; a move / other unguarded PATCH leaves
    /// it absent and behaves exactly as before (MAIN-36).
    #[serde(default)]
    pub expected_updated_at: Option<DateTime<Utc>>,
}

/// One bulk action applied to a batch of backlog tasks (MAIN-154). One action
/// per call; the server loops with one transaction per task and returns a
/// per-id result. `action` is one of `agent_ready` | `move_column` | `priority`
/// | `type` | `assignee` | `archive`; `value` carries the action's argument
/// (e.g. `on`/`off`, a column type, `0`–`4`, a task type, a user uuid or empty
/// to unassign) and is absent for `archive`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BulkTaskRequest {
    pub task_ids: Vec<String>,
    pub action: String,
    #[serde(default)]
    pub value: Option<String>,
}

/// The outcome for one task in a bulk batch: `ok`, or `skipped` with a reason
/// (an epic that can't take the action, or an id the caller cannot see).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkTaskItemResult {
    /// The id/key exactly as supplied in the request.
    pub id: String,
    /// `ok` | `skipped`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The result of a bulk action: one entry per requested id, plus counts for the
/// UI's one-line summary ("14 updated, 2 skipped").
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkTaskResponse {
    pub results: Vec<BulkTaskItemResult>,
    pub updated: usize,
    pub skipped: usize,
}

/// Deserialize a field that can be absent, null, or a value.
///
/// `Option<Option<T>>` on its own does not do this: serde applies a JSON
/// `null` to the OUTER option, so "clear it" and "do not touch it" both
/// arrive as `None` and the caller cannot tell them apart. Going through
/// `Deserialize` for the inner option and wrapping the result in `Some`
/// reserves the outer `None` for a field that was never sent.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// Pass as `before` to fetch the next (older) page.
    pub next_cursor: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateNoteRequest {
    pub title: Option<String>,
    pub content_md: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateNoteRequest {
    pub title: Option<String>,
    pub content_md: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    pub workspace_id: WorkspaceId,
    pub node_id: NodeId,
    pub runtime: String,
    pub name: Option<String>,
    /// Pin the session to a specific checkout path (e.g. a worktree). When
    /// omitted, the workspace's first checkout on the node is used.
    pub path: Option<String>,
}

/// Open an ad-hoc terminal on a machine — a shell with no workspace, running in
/// the node's home directory. What you reach for when you just want a prompt on
/// a box, not to start work on a project.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateTerminalRequest {
    /// The runtime to run — `bash` by default, but any the node has installed.
    #[serde(default)]
    pub runtime: Option<String>,
    /// Name the session; defaults to something like "bash · <node>".
    #[serde(default)]
    pub name: Option<String>,
}

/// Launch a runtime's login flow on a node (MAIN-126). The node maps `runtime`
/// to an allowlisted login command; the caller never supplies arguments.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthorizeRuntimeRequest {
    /// The runtime to authorize — `claude`, `hermes`.
    pub runtime: String,
}

/// The node the resource-aware scheduler chose for "Auto" placement.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ScheduledNode {
    pub node_id: NodeId,
    pub node_name: String,
}

/// Sent by `nook join` (unauthenticated; the join token IS the credential).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JoinRequest {
    pub token: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JoinResponse {
    pub node_id: NodeId,
    pub node_name: String,
    /// Long-lived node credential; shown once, stored hashed.
    pub node_token: String,
    /// The slug of the tenant this node joined. The node scopes its default
    /// workspace root by it so a `nook join` lands checkouts under the tenant
    /// directory from the very first config, before enrolment (MAIN-347). Empty
    /// only from a control plane predating this field.
    #[serde(default)]
    pub tenant_slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateJoinTokenResponse {
    /// Shown exactly once; only a hash is stored.
    pub token: String,
    pub expires_at: DateTime<Utc>,
    /// SHA-256 of the certificate the joining machine should expect to see,
    /// so it can pin the server before handing over anything. `None` when the
    /// control plane does not terminate TLS itself (dev, or TLS at the edge),
    /// in which case there is nothing honest to pin to.
    #[serde(default)]
    pub ca_fingerprint: Option<String>,
    /// Where the joining machine should point its **agent** connection.
    ///
    /// Not always the API's address. The agent listener terminates TLS in the
    /// control-plane process — only it can judge a client certificate against
    /// the right tenant's CA — so it cannot sit behind the proxy that fronts
    /// the API, and deployments routinely give it its own name. A node told
    /// only the API address would enrol against a URL it must not use.
    #[serde(default)]
    pub agent_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateSettingRequest {
    pub value: serde_json::Value,
    /// `tenant` (default) or `user`.
    pub scope: Option<String>,
}

// ── Git status/diff (relayed from the node over its WebSocket) ───────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GitFileStatus {
    /// Porcelain status code, e.g. " M", "??", "A ".
    pub status: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GitStatusResponse {
    /// `false` when the checkout is not a git repository — a "+ New empty
    /// project" directory, say. Everything below is then empty for a reason
    /// that is not "nothing has changed", and the UI hides the panel instead
    /// of reporting a clean tree.
    pub is_repo: bool,
    pub branch: Option<String>,
    pub dirty: bool,
    pub files: Vec<GitFileStatus>,
    /// Unified diff of the working tree (truncated by the node if huge).
    pub diff: String,
}

// ── Git operations & vault DTOs ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CloneRequest {
    pub url: String,
    /// Directory name; derived from the URL when omitted.
    pub name: Option<String>,
    /// Tenant git credential to clone with (private repos over SSH).
    pub credential_id: Option<GitCredentialId>,
    /// Return as soon as the node has been asked, instead of waiting for the
    /// clone to finish. Progress arrives as activity events carrying `job_id`.
    #[serde(default)]
    pub background: bool,
}

/// Clone a workspace's *stored* remote onto a node (MAIN-223 AC-2). Unlike
/// [`CloneRequest`], the caller supplies no URL — the workspace already knows it —
/// and the resulting checkout is associated with this workspace by id.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WorkspaceCloneRequest {
    pub node_id: NodeId,
    /// Tenant git credential to clone with (private repos over SSH).
    pub credential_id: Option<GitCredentialId>,
}

/// A long-running operation the caller can watch instead of blocking on.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JobAccepted {
    pub job_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WorktreeRequest {
    pub node_id: NodeId,
    pub branch: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RemoveWorktreeRequest {
    pub node_id: NodeId,
    pub path: String,
}

/// One checkout's on-disk move: where it was and where it now lives (MAIN-107).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MigratePathPair {
    pub old: String,
    pub new: String,
}

/// Rewrite a node's durable path records after `nook migrate-workspaces` has
/// moved its checkouts on disk (MAIN-107). This is a coordinated rename, NOT a
/// rediscovery: `node_workspaces.path` and `tasks.worktree_path` are rewritten
/// in one transaction with row identity preserved, so no checkout looks new,
/// no `.env` is re-delivered, and no `worktree_path` goes stale.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MigratePathsRequest {
    pub pairs: Vec<MigratePathPair>,
}

/// How many rows the coordinated rewrite touched, per table.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MigratePathsResponse {
    pub node_workspaces_updated: u32,
    pub tasks_updated: u32,
}

/// Renaming a session (tabs are named things people recognize).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateSessionRequest {
    pub name: String,
}

/// Commit everything in a checkout, from the git panel.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GitCommitRequest {
    /// Which machine's checkout — a workspace can exist on several.
    pub node_id: NodeId,
    pub message: String,
    /// Which paths to stage (MAIN-325). `None` stages everything, which is what
    /// every caller did before selective staging existed — so an old client, or
    /// one that simply does not care, keeps the behaviour it had.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

/// Push a checkout's current branch.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GitPushRequest {
    pub node_id: NodeId,
    /// Tenant git credential to push with. Omit to use the node's own key.
    #[serde(default)]
    pub credential_id: Option<GitCredentialId>,
}

/// A tenant CA as an admin sees it. Never the private key — it is not
/// exportable, by design.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TenantCaSummary {
    pub id: String,
    /// `staged` | `active` | `retiring`.
    pub state: String,
    pub fingerprint: String,
    pub not_after: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// Machines still holding an unexpired leaf from this CA — the number that
    /// says whether it can be retired yet.
    pub nodes_holding_leaves: i64,
}

// ── Node enrolment (mTLS) ────────────────────────────────────────────────────

/// First contact: trade a join token for a certificate.
///
/// The node generates its keypair locally and sends only a CSR — the private
/// key never leaves the machine, so the control plane cannot leak what it was
/// never given.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EnrollRequest {
    /// `nook_join_…`, which is what decides whose CA signs this.
    pub token: String,
    pub csr_pem: String,
    /// Name for a machine enrolling for the first time.
    #[serde(default)]
    pub name: Option<String>,
}

/// Renewal: a node asks for a fresh certificate using the key it already has.
///
/// Deliberately no join token. Tokens are for a machine with no key yet;
/// requiring one at renewal would mean expiry costs a manual re-join, which is
/// exactly what must never happen to a laptop that was closed for a month.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RenewRequest {
    pub node_id: NodeId,
    pub csr_pem: String,
}

/// A certificate plus the trust the node needs to verify its peer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnrollResponse {
    pub node_id: NodeId,
    pub cert_pem: String,
    /// EVERY CA this tenant trusts, not just the signer. A node that refreshed
    /// only its own certificate would stay pinned to a CA being retired, which
    /// is what turns a rotation into an outage.
    pub ca_bundle: Vec<String>,
    pub not_after: DateTime<Utc>,
    /// The slug of the tenant this node enrolled into. The node scopes its
    /// default workspace root by it (`~/.nook/workspace/<tenant_slug>/…`) so two
    /// tenants sharing one control-plane host never collide on disk. Empty only
    /// from an older control plane that predates this field; the node falls back
    /// to its host slug then (MAIN-347).
    #[serde(default)]
    pub tenant_slug: String,
}

/// Asking for a personal access token.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct CreateUserTokenRequest {
    /// What it's for ("laptop cli", "ci"). Shown in the list you revoke from.
    #[serde(default)]
    pub name: Option<String>,
    /// Expire it after this many days. Omit for a token that doesn't expire.
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// The one and only time the token itself is readable.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateUserTokenResponse {
    /// `nook_user_…` — store it now; the server keeps only its hash.
    pub token: String,
    pub id: String,
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A personal access token as listed back — everything except the secret.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct UserToken {
    pub id: String,
    pub name: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Keystrokes for a session. What a script sends instead of attaching a
/// terminal.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SessionInputRequest {
    pub text: String,
    /// Press Enter afterwards. Defaults to true: an unsubmitted prompt is
    /// almost never what a caller wanted.
    #[serde(default)]
    pub enter: Option<bool>,
}

/// How much of a session's screen to read back.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct SessionOutputRequest {
    /// Scrollback lines above the visible screen (0–2000). Default 0.
    #[serde(default)]
    pub history_lines: Option<u32>,
}

/// A session's current screen, with enough context to know what you're
/// looking at — `runtime` is how a caller tells a claude shell from a bash one.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionOutputResponse {
    /// "claude" | "hermes" | "codex" | "bash" | …
    pub runtime: String,
    /// `starting` | `running` | `detached` | `exited` | `error`.
    pub status: String,
    pub text: String,
}

/// One terminal inside a session (a tmux window).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct SessionWindow {
    pub index: u32,
    pub name: String,
    pub active: bool,
    /// Panes in this window — >1 means it's split.
    #[serde(default)]
    pub panes: u32,
}

/// A hook reporting what the agent in a session is doing.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReportAgentStateRequest {
    /// `running` | `waiting` | `idle`.
    pub state: String,
    /// The tmux window the agent runs in, so the right terminal chip lights up.
    #[serde(default)]
    pub window: Option<u32>,
}

/// One session's current agent state, for seeding the UI on load.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentStateItem {
    pub session_id: SessionId,
    #[serde(default)]
    pub window: Option<u32>,
    pub state: String,
}

/// Deleting a workspace. Records always go; the checkouts on disk only go
/// when explicitly asked for (and if they stay, discovery re-adds them).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct DeleteWorkspaceRequest {
    /// Also delete the checkout directories on every online node.
    #[serde(default)]
    pub delete_files: bool,
}

/// What a workspace delete actually did.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeleteWorkspaceResponse {
    pub deleted: bool,
    /// Checkouts removed from disk.
    pub checkouts_removed: usize,
    /// Checkouts left behind (node offline, or removal failed) — these will
    /// be rediscovered.
    pub checkouts_remaining: usize,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InitProjectRequest {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StartWorkRequest {
    pub node_id: Option<NodeId>,
    pub runtime: String,
    pub branch: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartWorkResponse {
    pub task: TaskItem,
    pub session: Session,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubmitPrRequest {
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MoveTaskRequest {
    pub column: String,
}

/// Outcome of a long-running git operation on a node.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpResponse {
    pub ok: bool,
    pub path: Option<String>,
    pub message: String,
}

/// A tenant git credential — only the public half is ever returned.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct GitCredential {
    pub id: GitCredentialId,
    pub tenant_id: TenantId,
    pub name: String,
    pub kind: String,
    pub public_key: String,
    pub created_at: DateTime<Utc>,
}

// ── Loop jobs (MAIN-127) ─────────────────────────────────────────────────────
//
// A durable unit of detached loop work — a spec interview or an epic
// decomposition — that rides the generic work queue. This slice is the record
// and its lifecycle only; executor selection, node execution and interaction
// bridging are later tickets in the chain.

/// A loop job's lifecycle position. `queued` on create; `completed`, `failed`,
/// and `canceled` are terminal. The service layer enforces which transitions
/// are legal — the wire type just carries the current value.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct LoopJob {
    pub id: JobId,
    pub tenant_id: TenantId,
    /// `spec` (fill in a ticket), `decompose` (break down an epic), or `review`
    /// (review a repository).
    pub kind: String,
    /// The ticket a spec job targets, or the epic a decompose job breaks down.
    /// `None` for a `review` job, which is about a repository and has no ticket
    /// — migration 0040 made the column nullable and constrains the pair, so a
    /// borrowed task id here would be a lie the executor has to work around.
    pub target_task_id: Option<TaskId>,
    /// Where the work happens. Required when `target_task_id` is `None`; the
    /// database CHECK enforces exactly one of the two being present.
    pub workspace_id: Option<WorkspaceId>,
    pub requested_by: UserId,
    /// One of `queued|claimed|running|waiting_on_human|completed|failed|canceled`.
    pub state: String,
    /// The node that claimed the job (MAIN-160); `None` until then.
    pub executor_node_id: Option<NodeId>,
    /// The job this one re-runs (AC-5); `None` for an original.
    pub predecessor_job_id: Option<JobId>,
    /// Why the job could not yet be placed on an executor (MAIN-160): the
    /// specific gate that failed while it waits `queued`. `None` once claimed.
    #[serde(default)]
    pub queued_reason: Option<String>,
    /// The general idea the run starts from (MAIN-231) — the human's opening
    /// brief, set at create time and carried into the executor's session.
    /// `None` when the job was opened with nothing but its ticket.
    #[serde(default)]
    pub seed: Option<String>,
    /// The pull request a `review` run is about, and the head it was raised
    /// for. `None` for every other kind — a spec run is about a ticket.
    ///
    /// The head is the wakeup rule: a PR whose head has not moved since the
    /// last completed run for it is owed nothing. Without it the only available
    /// question was "does this repo have PRs", which is why the old design
    /// needed a timer.
    #[serde(default)]
    pub review_pr_number: Option<i64>,
    #[serde(default)]
    pub review_head_sha: Option<String>,
    /// What a review run CONCLUDED: `approved` | `changes_requested` |
    /// `needs_human` | `skipped`. `None` means it concluded nothing — however
    /// the process exited — and such a run does not count as having reviewed
    /// its head.
    #[serde(default)]
    pub review_verdict: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One append-only transcript line on a job — the conversation/output captured
/// where the work lives. Written by the executor (MAIN-161); read here.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct LoopJobTranscriptEntry {
    pub id: JobTranscriptId,
    pub job_id: JobId,
    /// `system` | `agent` | `human` — where the line came from.
    pub source: String,
    pub content: String,
    pub at: DateTime<Utc>,
}

/// A job with its transcript — the read model behind `GET /api/v1/jobs/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LoopJobDetail {
    #[serde(flatten)]
    pub job: LoopJob,
    pub transcript: Vec<LoopJobTranscriptEntry>,
}

/// Open a job against a ticket or epic. `decompose` requires the target to be a
/// `type='epic'` task; `spec` targets any task. The workspace is derived from
/// the target, and `requested_by` from the caller.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateLoopJobRequest {
    pub kind: String,
    /// The target ticket, as a UUID **or** a board key (`MAIN-42`) — resolved
    /// server-side like every other task-addressed route (MAIN-209), so the Loop
    /// panel (which opens by key) and CLI/MCP callers can both target by key.
    pub target_task_id: String,
    /// The general idea to start from (MAIN-231) — free text, optional. Stored
    /// on the job, delivered into the run's session as its opening brief, and
    /// echoed as the first `human` transcript line. Omit it and the run starts
    /// from the ticket alone, exactly as before.
    #[serde(default)]
    pub seed: Option<String>,
}

/// Raise a `review` job against a WORKSPACE (MAIN-408) — the manual half of
/// AC-2, beside the board-signal sweep of AC-1.
///
/// Separate from [`CreateLoopJobRequest`] because the two address different
/// things: that one names a ticket, this one names a repository. Folding them
/// into one request with two optional targets would make "exactly one of these"
/// a runtime check in the service layer, which is precisely what migration
/// 0040 moved into a database constraint.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateReviewJobRequest {
    /// The workspace to review, as a UUID or its name — resolved server-side.
    pub workspace_id: String,
    /// The opening brief, exactly as on a spec job. Optional.
    #[serde(default)]
    pub seed: Option<String>,
}

/// Send an unsolicited steering message to a live job (MAIN-231) — the input
/// half of the loop. Additive to, and independent of, the interaction ask/answer
/// model: a human may say something the agent never asked for.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateJobMessageRequest {
    pub body: String,
}

// ── Interactions (MAIN-159) ──────────────────────────────────────────────────
//
// A durable human interaction: an explicit ask, persisted with authorization,
// announced over channels (transport only), answerable from any surface,
// resumable. Subject-generic by design — a loop job and/or a ticket — so the
// loop-jobs chain is the first consumer, not the only one.

/// A pending/answered/canceled ask for a human. Its subject (a job and/or the
/// ticket it is anchored to) governs who may see and answer it.
// FromRow hand-written (see impls near end of file): `choices` is a Postgres
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct Interaction {
    pub id: InteractionId,
    pub tenant_id: TenantId,
    /// The loop job this pauses on, if any (the MAIN-127 chain is the first
    /// consumer). `None` for a subject-only or standalone ask.
    pub job_id: Option<JobId>,
    /// The ticket the interaction is anchored to — the subject visibility that
    /// governs who may answer. Derived from a job's target when a job is named.
    pub task_id: Option<TaskId>,
    pub prompt: String,
    /// Optional structured choices the answer is expected to be one of.
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    /// `pending` | `answered` | `canceled`.
    pub state: String,
    /// The executor node that requested it, if any (the anti-spoof anchor).
    pub requested_by_node_id: Option<NodeId>,
    /// The session that requested it, if any.
    pub requested_by_session_id: Option<SessionId>,
    /// The user whose answer won — set once, by the first authorized answer.
    pub answered_by: Option<UserId>,
    pub response: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
}

/// Create an interaction — the executor-scoped ask behind `nook interactions
/// ask`. When `job_id` is set the creating node must be that job's executor
/// (anti-spoof, AC-4); the subject ticket is then the job's target.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateInteractionRequest {
    pub prompt: String,
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    #[serde(default)]
    pub job_id: Option<JobId>,
    /// Anchor directly to a ticket when there is no job. Ignored when `job_id`
    /// is set (the job's target wins).
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// The requesting session, carried from `NOOK_SESSION_ID` when present.
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

/// Answer a pending interaction — the one endpoint the CLI and web both call.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AnswerInteractionRequest {
    pub response: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateGitCredentialRequest {
    pub name: String,
    /// Paste an existing private key (OpenSSH PEM)…
    pub private_key: Option<String>,
    /// …or let the server generate an ed25519 keypair.
    #[serde(default)]
    pub generate: bool,
}

/// A workspace secret file (e.g. .env). Content only present on single-get.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceSecret {
    pub name: String,
    pub updated_at: DateTime<Utc>,
    pub content: Option<String>,
    /// Sealed with a passphrase — reading it needs that passphrase.
    #[serde(default)]
    pub protected: bool,
    /// Removed from checkouts when the session ends.
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PutSecretRequest {
    pub content: String,
    /// The app password. Required, not optional: a `.env` moves between
    /// machines, so it is sealed with something the server never stores
    /// before the app key wraps it. A database dump plus `SECRETS_KEY` must
    /// never be enough to read one.
    pub passphrase: String,
    /// Wipe the synced file from checkouts when the session ends.
    #[serde(default)]
    pub ephemeral: bool,
}

/// Adopt a file that already exists in a checkout into the vault.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ImportSecretRequest {
    /// The app password. Same rule as saving: nothing enters the vault
    /// unsealed.
    pub passphrase: String,
    #[serde(default)]
    pub ephemeral: bool,
}

/// Whether an import left a `.env` on disk that the vault hasn't adopted yet.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SecretOnDisk {
    pub found: bool,
    /// Which checkout it was found in.
    pub checkout_path: Option<String>,
    /// Already stored in the vault, so there's nothing to adopt.
    pub in_vault: bool,
}

/// One improvement someone asked for, and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize, nook_db::FromDbRow, ToSchema)]
pub struct FeedbackItem {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub body: String,
    /// queued | delivered | submitted | dropped
    pub status: String,
    pub pr_url: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubmitFeedbackRequest {
    pub body: String,
    /// Where this feedback should be worked on. Remembered for next time;
    /// required only until one has been chosen.
    pub workspace_id: Option<WorkspaceId>,
    /// Runtime for the feedback session (defaults to claude).
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdateFeedbackRequest {
    pub status: Option<String>,
    pub pr_url: Option<String>,
}

/// Where feedback goes — the first-run question this answers is "which repo?"
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FeedbackTarget {
    pub configured: bool,
    pub workspace_id: Option<WorkspaceId>,
    pub workspace_name: Option<String>,
    pub git_remote: Option<String>,
    pub session_name: String,
    /// Branch the agent is told to work on, so improvements land somewhere
    /// isolated and deployable rather than on whatever was checked out.
    pub branch: Option<String>,
    /// What the agent should do with the change once it works — reviewed,
    /// pushed, PR'd, left uncommitted. Overrides the built-in wording.
    pub instructions: Option<String>,
    /// True when the instructions came from `.nook-feedback.md` in the repo
    /// rather than from this setting, so the UI can say where they live.
    pub instructions_from_repo: bool,
}

/// Point feedback at a repo and a branch. Separate from submitting, so the
/// target can be changed at any time rather than only on the first send.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetFeedbackTargetRequest {
    pub workspace_id: WorkspaceId,
    /// Empty means "leave the agent to pick"; a name pins every change to it.
    #[serde(default)]
    pub branch: Option<String>,
    /// Empty falls back to `.nook-feedback.md` in the repo, then to the
    /// built-in wording.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// A node binary this control plane can hand out. One per platform it was
/// built with — the fleet stays on one version because the server only offers
/// the build it shipped with.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NodeArtifact {
    /// `linux` | `darwin`, as `uname -s` lowercased.
    pub os: String,
    /// `x86_64` | `aarch64`, normalized from `uname -m`.
    pub arch: String,
    /// Human label for the picker ("macOS · Apple silicon").
    pub label: String,
    pub filename: String,
    /// Where to download it — a GitHub release asset. The control plane no
    /// longer hosts binaries, so it deliberately reports neither size nor
    /// checksum: it cannot attest to bytes it does not serve, and a stale
    /// digest is worse than none.
    pub url: String,
}

/// Everything the "add node" flow needs: what to download, and where the
/// one-shot installer lives.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NodeReleases {
    /// Version of the control plane, which is the version of these binaries.
    pub version: String,
    /// URL of the generated install script.
    pub install_url: String,
    /// This instance as the caller reached it — what a new machine should use.
    pub base_url: String,
    pub artifacts: Vec<NodeArtifact>,
}

/// Whether this user has an app password (the key that seals their secrets).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VaultStatus {
    pub configured: bool,
    pub created_at: Option<DateTime<Utc>>,
    /// How many passkeys can unlock this vault. Non-zero means the UI should
    /// reach for a passkey before asking anyone to type a password.
    #[serde(default)]
    pub passkeys: i64,
}

/// A passkey enrolled to unlock the vault.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VaultPasskey {
    pub id: uuid::Uuid,
    /// Base64url WebAuthn credential id, so the browser can ask for this
    /// specific passkey.
    pub credential_id: String,
    pub label: String,
    /// The app password sealed under the passkey-derived key, base64. Only
    /// the browser that holds the passkey can open it.
    pub wrapped_secret: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Enrolling a passkey. The wrapping happens in the browser; the server only
/// ever sees the sealed blob.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AddPasskeyRequest {
    pub credential_id: String,
    #[serde(default)]
    pub label: String,
    pub wrapped_secret: String,
}

/// Setting or checking the app password.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetVaultPassphraseRequest {
    pub passphrase: String,
}

/// Unlocking a protected secret.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OpenSecretRequest {
    pub passphrase: String,
}

// ── Dispatcher ───────────────────────────────────────────────────────────────

/// The dispatcher recommends; humans approve. It never codes, edits, or
/// deploys — suggestions are the entire output surface.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DispatchSuggestion {
    pub headline: String,
    pub items: Vec<DispatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DispatchItem {
    pub task_id: Option<TaskId>,
    pub title: String,
    pub rationale: String,
    pub suggested_runtime: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

// ── Team chat (MAIN-49) ──────────────────────────────────────────────────────
//
// v1 channels are tenant-owned; the DTOs stay owner-agnostic on the wire (the
// tenant scope is enforced server-side), so org channels are a later addition
// with no shape change. `id`s are UUID v7 so history keysets on them.

/// A chat channel visible to a member.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatChannel {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    /// `"tenant"` (shared by a tenant, the default) or `"org"` (shared across
    /// every tenant under an org — MAIN-112). Drives the org badge in the UI.
    pub owner_type: String,
    /// Archived channels are hidden from the default list and refuse new posts.
    pub archived: bool,
    /// The category this channel is grouped under (MAIN-178), or `None` when
    /// uncategorized. Deleting a category resets this to `None` for its channels.
    #[serde(default)]
    pub category_id: Option<Uuid>,
    /// Ordering position within its category (or the uncategorized bucket) —
    /// the sidebar orders by it (MAIN-178). `0` until an admin arranges it.
    #[serde(default)]
    pub position: i32,
    /// Messages posted by others after the caller's read cursor (MAIN-117). The
    /// API returns the real count with the caller's own messages excluded; the
    /// UI caps the display ("99+"). `0` when the caller is caught up, and on
    /// responses that don't compute it (create/update return a fresh channel).
    #[serde(default)]
    pub unread_count: i64,
    pub created_at: DateTime<Utc>,
}

/// A channel category (MAIN-178): a Discord-style group shared across a
/// tenant/org, ordered by `position`. Admin-defined; DMs are never categorized.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatCategory {
    pub id: Uuid,
    pub name: String,
    /// `"tenant"` or `"org"`, scoped exactly like a channel.
    pub owner_type: String,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

/// Create a category. `owner` is `"tenant"` (default) or `"org"`, like a channel.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateChatCategory {
    pub name: String,
    #[serde(default)]
    pub owner: Option<String>,
}

/// Rename a category (MAIN-178).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateChatCategory {
    pub name: String,
}

/// Reorder categories (MAIN-178): the new order, as category ids. Each id's
/// `position` becomes its index; ids outside the caller's scope are ignored.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReorderChatCategories {
    pub ordered_ids: Vec<Uuid>,
}

/// Place a channel (MAIN-178): set its category (`None` = uncategorized) and its
/// ordering position within that group.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatChannelPlacement {
    #[serde(default)]
    pub category_id: Option<Uuid>,
    pub position: i32,
}

/// A posted message. `id` is a UUID v7, so history paginates by keyset on it
/// (AC-2/AC-4), like the rest of NookOS.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatMessage {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_id: Uuid,
    /// The author's display name, resolved from `public.users` by `author_id` —
    /// so an org channel shows names for authors in other tenants (MAIN-112
    /// AC-4). `None` only if the author row is gone.
    pub author_name: Option<String>,
    pub body: String,
    /// The message this one replies to, if any (MAIN-114). `None` for a top-level
    /// message; a set value is always a top-level parent (one level, no nesting).
    #[serde(default)]
    pub parent_message_id: Option<Uuid>,
    /// How many replies this message has — populated for parents in channel
    /// history so the UI can show a thread affordance without an N+1 (AC-3). `0`
    /// for a reply or a childless message.
    #[serde(default)]
    pub reply_count: i64,
    /// When the latest reply landed, for ordering/preview; `None` with no replies.
    #[serde(default)]
    pub last_reply_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// This message's reactions, aggregated per emoji (MAIN-116 AC-2). Empty for
    /// a message with no reactions (and for a deleted one).
    #[serde(default)]
    pub reactions: Vec<ChatReactionAggregate>,
    /// When the body was last edited (MAIN-116 AC-3); `None` if never — the UI's
    /// "(edited)" marker. A deleted message carries the redacted placeholder body
    /// and `deleted = true` (AC-4); its real content is never sent.
    #[serde(default)]
    pub edited_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deleted: bool,
}

/// One emoji's reaction tally on a message (MAIN-116 AC-2): how many reacted and
/// whether the requesting caller is one of them (so a click toggles).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatReactionAggregate {
    pub emoji: String,
    pub count: i64,
    /// Whether the caller has this reaction — the UI highlights it and a click
    /// removes rather than adds.
    pub reacted: bool,
}

/// One page of channel history, newest-first, keyset-paginated on message id.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatMessagePage {
    pub messages: Vec<ChatMessage>,
    /// Pass as `before=` to fetch the next (older) page; `None` at the end.
    pub next_cursor: Option<Uuid>,
}

/// A message thread (MAIN-114): the parent message plus a keyset page of its
/// replies. Replies page newest-first on id like channel history — pass the last
/// reply's id as `before=` for the next (older) page; the client orders them
/// oldest-first for reading.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatThread {
    pub parent: ChatMessage,
    pub replies: Vec<ChatMessage>,
    /// `None` when the oldest reply has been reached.
    pub next_cursor: Option<Uuid>,
}

/// Create a channel: a human name. The slug is derived server-side.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateChatChannel {
    pub name: String,
    /// `"tenant"` (default) or `"org"`. An org channel is owned by the caller's
    /// tenant's org and needs tenant owner/admin to create (MAIN-112 AC-3).
    #[serde(default)]
    pub owner: Option<String>,
}

/// Rename and/or archive a channel. Absent fields are left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateChatChannel {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub archived: Option<bool>,
}

/// Post a message to a channel. `parent_message_id`, when set, makes this a
/// threaded reply (MAIN-114) — the parent must be in the same channel and must
/// not itself be a reply (one level, no nesting).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PostChatMessage {
    pub body: String,
    #[serde(default)]
    pub parent_message_id: Option<Uuid>,
}

/// Edit a message's body (MAIN-116 AC-3). Author-only, validated like a post.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateChatMessage {
    pub body: String,
}

/// A person the caller may address in a DM (MAIN-113 AC-4): the stable
/// cross-tenant `person_id` and a display name resolved from `public.users`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PersonRef {
    pub person_id: Uuid,
    pub display_name: String,
}

/// A direct-message conversation the caller belongs to (MAIN-113). The UI names
/// it by its *other* participants' display names — DM channels carry no
/// human-facing channel name of their own.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DmSummary {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub participants: Vec<PersonRef>,
    /// Unread messages from the other participant(s) after the caller's read
    /// cursor (MAIN-117) — same semantics as [`ChatChannel::unread_count`].
    #[serde(default)]
    pub unread_count: i64,
}

/// Open (or reuse) a DM with these persons — the creator is always included, so
/// the effective set is `{caller} ∪ person_ids`, 2–8 people (MAIN-113 AC-2).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenDmRequest {
    pub person_ids: Vec<Uuid>,
}

/// What the chat websocket pushes to a subscribed client (AC-3). Adjacently
/// tagged for clean generated TypeScript, like the node protocol.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ChatServerMessage {
    /// A new message posted to the subscribed channel.
    Message(ChatMessage),
    /// An existing message changed — an edit, a soft delete, or a reaction
    /// toggle (MAIN-116 AC-5). Carries the message's current state (redacted +
    /// reaction-aggregated); the client replaces it in place by `id`. Delivered
    /// on the message's own channel, so a reply update reaches the thread too.
    MessageUpdated(ChatMessage),
}
