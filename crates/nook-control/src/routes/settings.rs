use axum::extract::{Path, State};
use axum::Json;
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::repo::admin::SettingWrite;
use crate::state::AppState;

#[utoipa::path(get, path = "/api/v1/settings",
    operation_id = "list_settings", responses((status = 200, body = [Setting])))]
pub async fn list(State(state): State<AppState>, auth: AuthCtx) -> ApiResult<Json<Vec<Setting>>> {
    // Tenant-scoped settings plus the caller's user-scoped ones.
    let settings = state
        .settings
        .visible_to(auth.tenant_id, auth.user_id)
        .await?;
    Ok(Json(settings))
}

#[utoipa::path(put, path = "/api/v1/settings/{key}",
    operation_id = "put_setting",
    params(("key" = String, Path,)),
    request_body = UpdateSettingRequest,
    responses(
        (status = 200, body = Setting),
        // `email.inbound` claims an address no other tenant may hold (MAIN-329).
        (status = 409)))]
pub async fn put(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(key): Path<String>,
    Json(req): Json<UpdateSettingRequest>,
) -> ApiResult<Json<Setting>> {
    let scope = req.scope.unwrap_or_else(|| "user".into());
    if scope != "tenant" && scope != "user" {
        return Err(ApiError::BadRequest(
            "scope must be 'tenant' or 'user'".into(),
        ));
    }
    // A TENANT-scoped setting is everybody's, so writing one is administering
    // the tenant — `loops.enabled` and `sessions.reconcile.enabled` decide
    // whether the fleet runs agents and converges sessions for the whole team.
    // This was ungated: any signed-in member could turn either on.
    //
    // Safe to require only now that a tenant owner/admin actually HOLDS
    // `tenant.manage` — `has_permission` derives the `tenant_admin` binding
    // from `users.role` rather than relying on 0001's one-time backfill, which
    // nothing had maintained. Gating this before that fix would have locked out
    // every owner of a tenant created since.
    //
    // A USER-scoped setting is your own preference and stays ungated.
    if scope == "tenant" {
        auth.require(
            &state,
            crate::auth::perm::Permission::TenantManage,
            crate::auth::perm::Scope::Tenant(auth.tenant_id),
        )
        .await?;
    }

    // Key-specific validation, for the keys whose VALUE carries an invariant the
    // settings table cannot express. `email.inbound` routes real mail by
    // address, so a second tenant claiming one already in use would take
    // delivery of somebody else's support mail (MAIN-329). `email.reply_policy`
    // decides whether a drafted reply reaches a customer, and an unrecognised
    // value there reads as the safe default — so a tenant meaning to opt in
    // would believe they had and see nothing sent (MAIN-332).
    crate::services::email_inbound::validate_setting(&state, auth.tenant_id, &key, &req.value)
        .await?;
    crate::services::email_links::reply::validate_setting(&key, &req.value)?;

    let user_id = (scope == "user").then_some(auth.user_id);
    let setting = state
        .settings
        .put(SettingWrite {
            tenant: auth.tenant_id,
            scope,
            user: user_id,
            key,
            value: req.value,
        })
        .await?;
    Ok(Json(setting))
}
