use std::sync::Arc;

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use sqlx::PgPool;

use crate::auth::OidcContext;
use crate::config::Config;
use crate::crypto::Vault;
use crate::services::kanban::KanbanRegistry;
use crate::ws::registry::Registry;
use nook_dispatcher::{DispatcherBackend, RuleBasedDispatcher};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cfg: Arc<Config>,
    pub oidc: Option<Arc<OidcContext>>,
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
    /// Recently validated MCP bearer tokens (hash → validated-at), so OIDC
    /// access-token checks don't hit the IdP's userinfo endpoint per request.
    pub mcp_auth_cache: Arc<dashmap::DashMap<u64, std::time::Instant>>,
    /// Per-tenant budget for `POST /notify`, which node tokens may call.
    pub notify_limit: Arc<crate::services::notify::RateLimiter>,
    cookie_key: Key,
}

impl AppState {
    pub async fn new(db: PgPool, cfg: Config, oidc: Option<OidcContext>) -> Self {
        let cookie_key = crate::auth::cookie_key(&cfg.session_secret);
        let vault = Vault::from_env(&cfg.session_secret).expect("vault init failed");
        let artifacts: Arc<dyn crate::storage::ArtifactStore> =
            Arc::from(crate::storage::from_config(&cfg).await);
        let mailer: Arc<dyn crate::mailer::Mailer> = Arc::from(crate::mailer::from_config(&cfg));
        Self {
            artifacts,
            mailer,
            kanban: Arc::new(KanbanRegistry::new(db.clone())),
            registry: Arc::new(Registry::new()),
            dispatcher: Arc::new(RuleBasedDispatcher),
            vault,
            db,
            cfg: Arc::new(cfg),
            oidc: oidc.map(Arc::new),
            mcp_auth_cache: Arc::new(dashmap::DashMap::new()),
            notify_limit: Arc::new(Default::default()),
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
