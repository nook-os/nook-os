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
    pub cfg: Arc<Config>,
    /// OIDC discovery state — configured/usable/degraded, hot-swappable after
    /// boot so an IdP that was down at startup recovers without a restart
    /// (MAIN-169). Readers call `state.oidc.current()`.
    pub oidc: Arc<OidcState>,
    pub kanban: Arc<KanbanRegistry>,
    pub registry: Arc<Registry>,
    pub dispatcher: Arc<dyn DispatcherBackend>,
    pub vault: Vault,
    /// Where node binaries are read from and written to — a directory or an
    /// object store, decided by config at boot.
    pub artifacts: Arc<dyn crate::storage::ArtifactStore>,
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
    /// caller). Only OIDC tokens are cached here (the static `MCP_TOKEN` is a
    /// direct compare, needing no userinfo round-trip); each entry is a person
    /// the token resolved to (MAIN-102).
    pub mcp_auth_cache: Arc<dashmap::DashMap<u64, (std::time::Instant, nook_mcp::McpCaller)>>,
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
        let oidc = Arc::new(OidcState::new(&cfg, oidc));
        let cookie_key = crate::auth::cookie_key(&cfg.session_secret);
        let vault = Vault::from_env(&cfg.session_secret).expect("vault init failed");
        let artifacts: Arc<dyn crate::storage::ArtifactStore> =
            Arc::from(crate::storage::from_config(&cfg).await);
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
            identity: Arc::new(crate::repo::identity::DbIdentityRepository::new(db.clone())),
            invites: Arc::new(crate::repo::invites::DbInviteRepository::new(db.clone())),
            kanban: Arc::new(KanbanRegistry::new(tasks.clone())),
            tasks,
            artifacts,
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
