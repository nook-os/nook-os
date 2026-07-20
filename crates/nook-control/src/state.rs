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
    /// Recently validated MCP bearer tokens (hash → validated-at), so OIDC
    /// access-token checks don't hit the IdP's userinfo endpoint per request.
    pub mcp_auth_cache: Arc<dashmap::DashMap<u64, std::time::Instant>>,
    cookie_key: Key,
}

impl AppState {
    pub fn new(db: PgPool, cfg: Config, oidc: Option<OidcContext>) -> Self {
        let cookie_key = crate::auth::cookie_key(&cfg.session_secret);
        let vault = Vault::from_env(&cfg.session_secret).expect("vault init failed");
        Self {
            kanban: Arc::new(KanbanRegistry::new(db.clone())),
            registry: Arc::new(Registry::new()),
            dispatcher: Arc::new(RuleBasedDispatcher),
            vault,
            db,
            cfg: Arc::new(cfg),
            oidc: oidc.map(Arc::new),
            mcp_auth_cache: Arc::new(dashmap::DashMap::new()),
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
