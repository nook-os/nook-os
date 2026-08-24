use std::sync::Arc;

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use nook_db::DbPool;

use crate::auth::{OidcContext, OidcState};
use crate::config::Config;
use crate::crypto::Vault;
use crate::services::kanban::KanbanRegistry;
use crate::ws::registry::Registry;
use nook_dispatcher::{DispatcherBackend, RuleBasedDispatcher};

#[derive(Clone)]
pub struct AppState {
    pub db: DbPool,
    /// Identity/auth data access behind its trait (MAIN-246). Handed out as
    /// `Arc<dyn …>` so a test can build an `AppState` on the in-memory fake and
    /// exercise the callers with no database at all.
    pub identity: Arc<dyn crate::repo::identity::IdentityRepository>,
    /// Task/board data access behind its trait (MAIN-248). Same contract.
    pub tasks: Arc<dyn crate::repo::tasks::TaskRepository>,
    /// Invite data access behind its trait (MAIN-250). Same contract.
    pub invites: Arc<dyn crate::repo::invites::InviteRepository>,
    /// Workspaces and their checkouts (MAIN-251). Same contract.
    pub workspaces: Arc<dyn crate::repo::workspaces::WorkspaceRepository>,
    /// Sessions: creation, status transitions, the two list shapes (MAIN-253).
    pub sessions: Arc<dyn crate::repo::sessions::SessionRepository>,
    /// Tenant git credentials (MAIN-251).
    pub git_credentials: Arc<dyn crate::repo::workspaces::GitCredentialRepository>,
    /// Sealed per-workspace secret files (MAIN-251).
    pub workspace_secrets: Arc<dyn crate::repo::workspaces::WorkspaceSecretRepository>,
    /// Loop jobs and their transcripts (MAIN-255).
    pub jobs: Arc<dyn crate::repo::jobs::LoopJobRepository>,
    /// Durable ask/answer between a running job and a human (MAIN-255).
    pub interactions: Arc<dyn crate::repo::jobs::InteractionRepository>,
    /// Nodes: identity, sharing, the liveness lease (MAIN-252). Same contract.
    pub nodes: Arc<dyn crate::repo::nodes::NodeRepository>,
    /// Single-use enrolment tokens (MAIN-252).
    pub join_tokens: Arc<dyn crate::repo::nodes::JoinTokenRepository>,
    /// The per-tenant certificate authority (MAIN-252).
    pub tenant_cas: Arc<dyn crate::repo::nodes::TenantCaRepository>,
    /// The notification inbox and its channels (MAIN-256).
    pub notifications: Arc<dyn crate::repo::notifications::NotificationRepository>,
    /// The cross-cutting read model (MAIN-304): the activity-event writer, the
    /// activity feed, and the Mission Control overview — three surfaces that
    /// belong to no single aggregate.
    pub read_model: Arc<dyn crate::repo::read_model::ReadModelRepository>,
    /// Product feedback and the settings that configure its surface (MAIN-256).
    pub feedback: Arc<dyn crate::repo::notifications::FeedbackRepository>,
    /// Notes and folders — personal notebook and workspace notes (MAIN-254).
    pub notebook: Arc<dyn crate::repo::notebook::NotebookRepository>,
    /// The app password, its passkeys, and the notebook's per-note seal
    /// (MAIN-254). Named `vaults` because `vault` is the crypto key holder.
    pub vaults: Arc<dyn crate::repo::notebook::VaultRepository>,
    /// The operator console: orgs, role bindings, and its four lists
    /// (MAIN-258).
    pub operator: Arc<dyn crate::repo::admin::OperatorRepository>,
    /// The shipped skill and hook set. The content stays immutable; only its
    /// data access moved (MAIN-258).
    pub managed: Arc<dyn crate::repo::admin::ManagedContentRepository>,
    /// What a tenant has taught its fleet (MAIN-258).
    pub skills: Arc<dyn crate::repo::admin::SkillRepository>,
    /// What people have uploaded (MAIN-532). Rows only — the bytes live in
    /// `user_content_store`, under a prefix of their own.
    pub user_content: Arc<dyn crate::repo::user_content::UserContentRepository>,
    /// Which uploads hang off which ticket or comment (MAIN-533). The join
    /// MAIN-532's store deliberately knows nothing about.
    pub attachments: Arc<dyn crate::repo::attachments::TaskAttachmentRepository>,
    /// What automation has written on a card (MAIN-603). Markdown Nook stores
    /// and never parses.
    pub task_reports: Arc<dyn crate::repo::task_reports::TaskReportRepository>,
    /// The chain from a support email to its ticket, run and PR (MAIN-330).
    pub email_links: Arc<dyn crate::repo::email_links::EmailLinkRepository>,
    /// What GitHub has delivered (MAIN-554). Written by the receiver and read
    /// by nobody yet — this card records, its children act.
    pub forge_deliveries: Arc<dyn crate::repo::forge_deliveries::ForgeDeliveryRepository>,
    /// The IMAP mailbox a tenant polls, and the ledger of what it has already
    /// ingested (MAIN-333). The sealed credential lives here; `vault` is what
    /// opens it.
    pub email_pollers: Arc<dyn crate::repo::email_pollers::EmailPollerRepository>,
    /// Named secret items — tenant, workspace and node scoped (MAIN-625). The
    /// envelope-sealed values live here; `vault` is what opens them.
    pub secret_items: Arc<dyn crate::repo::secret_items::SecretItemRepository>,
    /// Tenant- and user-scoped settings rows (MAIN-258).
    pub settings: Arc<dyn crate::repo::admin::SettingRepository>,
    /// Org-visibility policy rows (MAIN-305).
    pub org_policy: Arc<dyn crate::repo::admin::OrgPolicyRepository>,
    /// The theme catalogue (MAIN-258).
    pub themes: Arc<dyn crate::repo::admin::ThemeRepository>,
    pub cfg: Arc<Config>,
    /// OIDC discovery state — configured/usable/degraded, hot-swappable after
    /// boot so an IdP that was down at startup recovers without a restart
    /// (MAIN-169). Readers call `state.oidc.current()`.
    pub oidc: Arc<OidcState>,
    pub kanban: Arc<KanbanRegistry>,
    pub registry: Arc<Registry>,
    /// Deliveries a sessionless authorize flow is still waiting to hear about
    /// (MAIN-290). In memory because a flow lives only as long as the process
    /// that started it — the credential was never persisted either.
    pub pending_deliveries: crate::services::runtime_auth_flow::SharedPendingDeliveries,
    pub dispatcher: Arc<dyn DispatcherBackend>,
    pub vault: Vault,
    /// Where node binaries are READ from — a directory or an object store,
    /// decided by config at boot. On disk that directory is the image's baked
    /// dist, which nothing writes at runtime (MAIN-598).
    pub artifacts: Arc<dyn crate::storage::ArtifactStore>,
    /// Where an upload's bytes live. A separate store from `artifacts` on disk,
    /// the same one on S3 — see `nook_infra::storage` for why.
    pub user_content_store: Arc<dyn crate::storage::ArtifactStore>,
    /// Why the boot probe could not use `user_content_store`, when it could not
    /// (MAIN-598). `Some` is what turns an upload into a 503 instead of a 500;
    /// the detail itself never leaves the boot log.
    pub user_content_store_error: Option<String>,
    /// How outbound email leaves the control plane — a real SMTP relay, or the
    /// capture/log fallback when none is configured. Decided by config at boot.
    pub mailer: Arc<dyn crate::mailer::Mailer>,
    /// A swappable key/value cache — in-memory today. First consumer: the
    /// per-person tenants list `/auth/me` carries. Decided by config at boot.
    pub cache: Arc<dyn crate::cache::Cache>,
    /// The durable work queue — the zero-infra Postgres backend today. Anything
    /// in the control plane enqueues through here; there is no worker draining
    /// it yet (MAIN-147). Decided by config at boot.
    pub queue: Arc<dyn crate::queue::Queue>,
    /// Recently validated MCP bearer tokens (hash → validated-at), so OIDC
    /// access-token checks don't hit the IdP's userinfo endpoint per request.
    /// Cache of validated OIDC MCP bearer tokens → (when validated, the resolved
    /// caller). Only OIDC tokens are cached here — the personal-access-token
    /// door is a single indexed lookup and needs no userinfo round-trip to save;
    /// each entry is a person the token resolved to (MAIN-102).
    pub mcp_auth_cache: Arc<dashmap::DashMap<u64, (std::time::Instant, nook_mcp::McpCaller)>>,
    /// How much review work each repo has, cached behind a TTL (MAIN-448).
    ///
    /// Shared rather than owned by the reconcile loop, because the review-loop
    /// STATUS endpoint has to report the number the loop is acting on. Two
    /// caches would mean two answers to "how many reviewers does this repo
    /// want", and the UI would be reporting the one nobody converges toward.
    pub review_demand: Arc<crate::services::forge::ReviewDemand>,
    /// Throttles the PR hygiene pass (MAIN-476) to the forge cache's rhythm —
    /// its per-PR detail reads must not run at the reconciler's poll cadence.
    pub pr_hygiene: Arc<crate::services::pr_hygiene::Hygiene>,
    /// Per-tenant budget for `POST /notify`, which node tokens may call.
    pub notify_limit: Arc<crate::services::notify::RateLimiter>,
    /// Per-IP budget for the UNAUTHENTICATED invite preview. Keyed by a uuid
    /// derived from the resolved client IP (see `crate::client_ip`), so a
    /// signed-out endpoint that does real DB work cannot be hammered anonymously.
    pub preview_limit: Arc<crate::services::notify::RateLimiter>,
    cookie_key: Key,
}

impl AppState {
    pub async fn new(db: DbPool, cfg: Config, oidc: Option<OidcContext>) -> Self {
        // Discovery state is built from config; `oidc` seeds the already-
        // discovered context from the boot-time attempt (MAIN-169).
        let oidc = Arc::new(OidcState::new(&cfg, db.clone(), oidc));
        let cookie_key = crate::auth::cookie_key(&cfg.session_secret);
        let vault = Vault::from_env(&cfg.session_secret).expect("vault init failed");
        // Two stores and one verdict: the baked dist, the upload directory,
        // and whether the second could actually be written at boot (MAIN-598).
        let storage = crate::storage::from_config(&cfg).await;
        // The configured transport, wrapped in the send guards (enable /
        // category / quota) so every provider is gated identically (MAIN-52).
        let transport: Arc<dyn crate::mailer::Mailer> = Arc::from(crate::mailer::from_config(&cfg));
        let mailer: Arc<dyn crate::mailer::Mailer> = Arc::new(crate::mailer::GuardedMailer::new(
            transport,
            db.clone(),
            &cfg,
        ));
        // A swappable key/value cache; first consumer is the tenants list (MAIN-27).
        let cache: Arc<dyn crate::cache::Cache> = Arc::from(crate::cache::from_config(&cfg));
        // The durable work queue; database-backed today (MAIN-147).
        let queue: Arc<dyn crate::queue::Queue> =
            Arc::from(crate::queue::from_config(&cfg, db.clone()).await);
        // Built once and shared: the kanban registry's local provider reads
        // through the same repository the services do.
        let tasks: Arc<dyn crate::repo::tasks::TaskRepository> =
            Arc::new(crate::repo::tasks::DbTaskRepository::new(db.clone()));
        Self {
            pending_deliveries: Arc::new(
                crate::services::runtime_auth_flow::PendingDeliveries::new(),
            ),
            identity: Arc::new(crate::repo::identity::DbIdentityRepository::new(db.clone())),
            invites: Arc::new(crate::repo::invites::DbInviteRepository::new(db.clone())),
            workspaces: Arc::new(crate::repo::workspaces::DbWorkspaceRepository::new(
                db.clone(),
            )),
            sessions: Arc::new(crate::repo::sessions::DbSessionRepository::new(db.clone())),
            jobs: Arc::new(crate::repo::jobs::DbLoopJobRepository::new(db.clone())),
            interactions: Arc::new(crate::repo::jobs::DbInteractionRepository::new(db.clone())),
            git_credentials: Arc::new(crate::repo::workspaces::DbGitCredentialRepository::new(
                db.clone(),
            )),
            workspace_secrets: Arc::new(crate::repo::workspaces::DbWorkspaceSecretRepository::new(
                db.clone(),
            )),
            nodes: Arc::new(crate::repo::nodes::DbNodeRepository::new(db.clone())),
            join_tokens: Arc::new(crate::repo::nodes::DbJoinTokenRepository::new(db.clone())),
            tenant_cas: Arc::new(crate::repo::nodes::DbTenantCaRepository::new(db.clone())),
            notebook: Arc::new(crate::repo::notebook::DbNotebookRepository::new(db.clone())),
            user_content: Arc::new(crate::repo::user_content::DbUserContentRepository::new(
                db.clone(),
            )),
            attachments: Arc::new(crate::repo::attachments::DbTaskAttachmentRepository::new(
                db.clone(),
            )),
            task_reports: Arc::new(crate::repo::task_reports::DbTaskReportRepository::new(
                db.clone(),
            )),
            email_links: Arc::new(crate::repo::email_links::DbEmailLinkRepository::new(
                db.clone(),
            )),
            forge_deliveries: Arc::new(
                crate::repo::forge_deliveries::DbForgeDeliveryRepository::new(db.clone()),
            ),
            email_pollers: Arc::new(crate::repo::email_pollers::DbEmailPollerRepository::new(
                db.clone(),
            )),
            secret_items: Arc::new(crate::repo::secret_items::DbSecretItemRepository::new(
                db.clone(),
            )),
            notifications: Arc::new(crate::repo::notifications::DbNotificationRepository::new(
                db.clone(),
            )),
            feedback: Arc::new(crate::repo::notifications::DbFeedbackRepository::new(
                db.clone(),
            )),
            read_model: Arc::new(crate::repo::read_model::DbReadModelRepository::new(
                db.clone(),
            )),
            vaults: Arc::new(crate::repo::notebook::DbVaultRepository::new(db.clone())),
            operator: Arc::new(crate::repo::admin::DbOperatorRepository::new(db.clone())),
            managed: Arc::new(crate::repo::admin::DbManagedContentRepository::new(
                db.clone(),
            )),
            skills: Arc::new(crate::repo::admin::DbSkillRepository::new(db.clone())),
            settings: Arc::new(crate::repo::admin::DbSettingRepository::new(db.clone())),
            org_policy: Arc::new(crate::repo::admin::DbOrgPolicyRepository::new(db.clone())),
            themes: Arc::new(crate::repo::admin::DbThemeRepository::new(db.clone())),
            kanban: Arc::new(KanbanRegistry::new(tasks.clone())),
            tasks,
            artifacts: storage.artifacts,
            user_content_store: storage.user_content,
            user_content_store_error: storage.user_content_error,
            mailer,
            cache,
            queue,
            registry: Arc::new(Registry::new()),
            dispatcher: Arc::new(RuleBasedDispatcher),
            vault,
            db,
            cfg: Arc::new(cfg),
            oidc,
            mcp_auth_cache: Arc::new(dashmap::DashMap::new()),
            review_demand: Arc::new(crate::services::forge::ReviewDemand::from_env()),
            pr_hygiene: Arc::new(crate::services::pr_hygiene::Hygiene::new(
                std::time::Duration::from_secs(60),
            )),
            notify_limit: Arc::new(Default::default()),
            preview_limit: Arc::new(Default::default()),
            cookie_key,
        }
    }
}

/// Lets `PrivateCookieJar` pull its encryption key from state.
impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_key.clone()
    }
}
