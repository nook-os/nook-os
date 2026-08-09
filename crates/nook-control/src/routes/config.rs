use axum::extract::State;
use axum::Json;

use crate::auth::AuthCtx;
use crate::state::AppState;

/// GET /api/v1/config — the deployment's optional-feature configuration.
///
/// Signed-in, unlike `/auth/providers`: the login screen has to know what it
/// may offer before anyone is authenticated, whereas nothing here is needed
/// until a person is already inside the app. That lets it carry the Giphy key
/// (MAIN-171) without publishing the operator's key to anonymous callers.
#[utoipa::path(
    get,
    path = "/api/v1/config",
    operation_id = "app_config",
    responses(
        (status = 200, body = nook_types::AppConfig),
        (status = 401, description = "not signed in")
    )
)]
pub async fn app_config(
    State(state): State<AppState>,
    _auth: AuthCtx,
) -> Json<nook_types::AppConfig> {
    Json(nook_types::AppConfig {
        giphy_key: state.cfg.giphy_key.clone(),
    })
}
