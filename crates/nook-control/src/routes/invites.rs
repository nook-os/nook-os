//! Tenant invites (MAIN-6). An owner/admin invites an email into their active
//! tenant; the invitee accepts by signing in as that email and POSTing the
//! opaque token, which links them into the tenant via `person_id` (so the MAIN-4
//! switcher immediately offers it). Emailing the link is MAIN-7 — here it is
//! returned by the API and copied in the UI.

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use nook_db::{params, Db, Postgres, TimeMath, TypeMapping};
use nook_types::*;
use serde::Deserialize;
use std::net::SocketAddr;

use crate::auth::{AuthCtx, IdentityCtx};
use crate::error::{ApiError, ApiResult};
use crate::seed::hash_token;
use crate::services::identity::email_is_verified;
use crate::state::AppState;

/// Guard: the action targets the caller's ACTIVE tenant, and the caller is an
/// owner/admin of it. Managing another tenant needs switching to it first.
async fn require_admin_of(state: &AppState, auth: &AuthCtx, tenant: TenantId) -> ApiResult<()> {
    if tenant != auth.tenant_id {
        return Err(ApiError::ForbiddenMsg(
            "switch to a tenant before managing its invites".into(),
        ));
    }
    auth.require_tenant_admin(state).await
}

fn new_token() -> String {
    use rand::distr::Alphanumeric;
    use rand::Rng;
    let body: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    format!("inv_{body}")
}

/// Minimal HTML escaping for values dropped into the HTML body — a tenant name
/// or a display name must never be able to inject markup into the email.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Mask an email for the unauthenticated preview: keep the first character of
/// the local part and the whole domain, replace the rest of the local part with
/// `…`. `ryan@example.com` → `r…@example.com`. Enough for an invitee to
/// recognise their own address, not enough to harvest one from a link. Pure, so
/// the masking rule is unit-testable.
fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => {
            let first = local.chars().next().unwrap_or('*');
            format!("{first}…@{domain}")
        }
        // Not an address we can split — mask the whole thing rather than leak it.
        _ => "…".into(),
    }
}

/// Compose the invite email: subject, plain-text body, and a minimal HTML body
/// (AC-4). Pure, so the content — the accept link, the tenant, and who invited
/// them (AC-2) — is unit-testable without a mailer.
fn invite_email(tenant: &str, inviter: &str, accept_url: &str) -> (String, String, String) {
    let subject = format!("You're invited to {tenant} on NookOS");
    let text = format!(
        "{inviter} invited you to join {tenant} on NookOS.\n\n\
         Accept the invitation:\n\n\
         {accept_url}\n\n\
         If you weren't expecting this, you can ignore this email. \
         The invitation expires in 14 days."
    );
    let (t, i, u) = (esc(tenant), esc(inviter), esc(accept_url));
    let html = format!(
        "<div style=\"font-family:-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;\
         font-size:15px;line-height:1.6;color:#111;max-width:520px\">\
         <p><strong>{i}</strong> invited you to join <strong>{t}</strong> on NookOS.</p>\
         <p style=\"margin:24px 0\">\
         <a href=\"{u}\" style=\"background:#111;color:#fff;text-decoration:none;\
         padding:10px 18px;border-radius:6px;display:inline-block\">Accept the invitation</a></p>\
         <p style=\"color:#666;font-size:13px\">Or open this link:<br>\
         <a href=\"{u}\">{u}</a><br><br>\
         If you weren't expecting this, you can ignore this email. \
         The invitation expires in 14 days.</p></div>"
    );
    (subject, text, html)
}

/// Enqueue the invite accept-link email (MAIN-149), best-effort: a failure to
/// enqueue never fails the API call — the invite still stands and the copy-link
/// path works regardless. We render here and hand the worker the message; the
/// send guards (enable / category / quota) and the sent/held decision now run
/// in the worker, so this no longer blocks on SMTP.
async fn send_invite_email(
    state: &AppState,
    tenant_id: TenantId,
    to: &str,
    tenant: &str,
    inviter: &str,
    accept_url: &str,
) {
    let (subject, text, html) = invite_email(tenant, inviter, accept_url);
    let job = crate::mailer::EmailJob::new(
        to,
        subject,
        text,
        Some(html),
        crate::mailer::Category::Transactional,
    );
    match crate::services::queue::enqueue_email(state, tenant_id.0, &job).await {
        Ok(()) => tracing::info!(to, "invite email queued"),
        Err(e) => tracing::error!(
            error = %e,
            to,
            "invite email failed to enqueue (best-effort; the invite still stands)"
        ),
    }
}

/// The tenant's display name for the email, with a neutral fallback.
async fn tenant_display_name(state: &AppState, tenant: TenantId) -> String {
    state
        .db
        .query_scalar_opt::<String>("SELECT name FROM tenants WHERE id = $1", params![tenant])
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "a NookOS tenant".into())
}

/// The inviter's display name for the email, with a neutral fallback.
async fn inviter_display_name(state: &AppState, user_id: uuid::Uuid) -> String {
    state
        .db
        .query_scalar_opt::<String>(
            "SELECT display_name FROM users WHERE id = $1",
            params![user_id],
        )
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Someone".into())
}

fn validated_role(role: Option<&str>) -> ApiResult<&str> {
    match role.unwrap_or("member") {
        r @ ("member" | "admin") => Ok(r),
        // `owner` is never invitable (NG-3).
        other => Err(ApiError::BadRequest(format!(
            "invite role must be member or admin, not {other:?}"
        ))),
    }
}

/// `POST /api/v1/tenants/{id}/invites` — create a pending invite, returning the
/// accept URL. Re-inviting the same email replaces the existing pending invite
/// (AC-2). owner/admin only.
#[utoipa::path(post, path = "/api/v1/tenants/{id}/invites",
    operation_id = "create_invite",
    params(("id" = String, Path,)),
    request_body = CreateInviteRequest,
    responses((status = 200, body = Invite), (status = 403)))]
pub async fn create(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
    Json(req): Json<CreateInviteRequest>,
) -> ApiResult<Json<Invite>> {
    require_admin_of(&state, &auth, tenant).await?;
    let role = validated_role(req.role.as_deref())?;
    let email = req.email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::BadRequest("a valid email is required".into()));
    }

    // Replace any existing pending invite for this email so re-inviting does not
    // stack (AC-2); the partial unique index also enforces one pending.
    state
        .db
        .exec(
            "DELETE FROM invites
         WHERE tenant_id = $1 AND status = 'pending' AND lower(email) = lower($2)",
            params![tenant, email],
        )
        .await?;

    // Only the hash is stored; the plaintext rides in the accept link (AC-9).
    let token = new_token();
    let mut invite: Invite = state
        .db
        .query_one(
            &format!(
                "INSERT INTO invites (id, tenant_id, email, role, token_hash, status, invited_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'pending', $6, {expiry})
         RETURNING id, email, role, status, created_at, expires_at",
                expiry = Postgres.now_plus("14 days")
            ),
            params![
                uuid::Uuid::now_v7(),
                tenant,
                email,
                role,
                hash_token(&token),
                auth.user_id.0
            ],
        )
        .await?;

    // The accept link points at the web app, which drives sign-in then calls the
    // accept endpoint (MAIN-7 will also email this).
    invite.accept_url = Some(format!(
        "{}/accept?token={token}",
        state.cfg.web_origin.trim_end_matches('/')
    ));

    // Email the accept link too (AC-2), best-effort — a mail failure never fails
    // this call, and the invite is still returned for the copy-link path (AC-1).
    let tenant_name = tenant_display_name(&state, tenant).await;
    let inviter = inviter_display_name(&state, auth.user_id.0).await;
    send_invite_email(
        &state,
        tenant,
        email,
        &tenant_name,
        &inviter,
        invite.accept_url.as_deref().unwrap_or_default(),
    )
    .await;

    Ok(Json(invite))
}

/// `GET /api/v1/tenants/{id}/invites` — pending invites (never the token).
/// owner/admin only.
#[utoipa::path(get, path = "/api/v1/tenants/{id}/invites",
    operation_id = "list_invites",
    params(("id" = String, Path,)),
    responses((status = 200, body = [Invite]), (status = 403)))]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path(tenant): Path<TenantId>,
) -> ApiResult<Json<Vec<Invite>>> {
    require_admin_of(&state, &auth, tenant).await?;
    let rows: Vec<Invite> = state
        .db
        .query_all(
            "SELECT id, email, role, status, created_at, expires_at
         FROM invites WHERE tenant_id = $1 AND status = 'pending'
         ORDER BY created_at DESC",
            params![tenant],
        )
        .await?;
    Ok(Json(rows))
}

/// `DELETE /api/v1/tenants/{id}/invites/{invite}` — revoke a pending invite; its
/// link stops working. owner/admin only.
#[utoipa::path(delete, path = "/api/v1/tenants/{id}/invites/{invite}",
    operation_id = "revoke_invite",
    params(("id" = String, Path,), ("invite" = String, Path,)),
    responses((status = 204), (status = 403), (status = 404)))]
pub async fn revoke(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((tenant, invite)): Path<(TenantId, uuid::Uuid)>,
) -> ApiResult<axum::http::StatusCode> {
    require_admin_of(&state, &auth, tenant).await?;
    let res = state
        .db
        .exec(
            "UPDATE invites SET status = 'revoked'
         WHERE id = $1 AND tenant_id = $2 AND status = 'pending'",
            params![invite, tenant],
        )
        .await?;
    if res == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /api/v1/tenants/{id}/invites/{invite}/resend` — re-email the pending
/// invite's accept link (AC-5). owner/admin only.
#[utoipa::path(post, path = "/api/v1/tenants/{id}/invites/{invite}/resend",
    operation_id = "resend_invite",
    params(("id" = String, Path,), ("invite" = String, Path,)),
    responses((status = 200, body = Invite), (status = 403), (status = 404)))]
pub async fn resend(
    State(state): State<AppState>,
    auth: AuthCtx,
    Path((tenant, invite_id)): Path<(TenantId, uuid::Uuid)>,
) -> ApiResult<Json<Invite>> {
    require_admin_of(&state, &auth, tenant).await?;

    // Only the token's hash is stored (AC-9 from MAIN-6), so the original
    // plaintext is unrecoverable — a resend issues a FRESH token, invalidating
    // the old link, and re-stamps the expiry before emailing the new link.
    let token = new_token();
    let mut invite: Invite = state
        .db
        .query_opt(
            &format!(
                "UPDATE invites
            SET token_hash = $1, expires_at = {expiry}
          WHERE id = $2 AND tenant_id = $3 AND status = 'pending'
      RETURNING id, email, role, status, created_at, expires_at",
                expiry = Postgres.now_plus("14 days")
            ),
            params![hash_token(&token), invite_id, tenant],
        )
        .await?
        .ok_or(ApiError::NotFound)?;

    invite.accept_url = Some(format!(
        "{}/accept?token={token}",
        state.cfg.web_origin.trim_end_matches('/')
    ));
    let tenant_name = tenant_display_name(&state, tenant).await;
    let inviter = inviter_display_name(&state, auth.user_id.0).await;
    send_invite_email(
        &state,
        tenant,
        &invite.email,
        &tenant_name,
        &inviter,
        invite.accept_url.as_deref().unwrap_or_default(),
    )
    .await;

    Ok(Json(invite))
}

/// `POST /api/v1/invites/accept` — the signed-in person consumes a token. On a
/// match (pending, unexpired, email equals the signed-in email) they are added
/// to the tenant with the invite's role — linked by `person_id`, keeping their
/// personal tenant — and the invite becomes `accepted`. Any failure leaves the
/// invite untouched and returns the caller to their own tenant with a message
/// (AC-4/5); already-a-member is a no-op success (AC-6).
#[utoipa::path(post, path = "/api/v1/invites/accept",
    operation_id = "accept_invite",
    request_body = AcceptInviteRequest,
    responses((status = 200, body = AcceptInviteResult)))]
pub async fn accept(
    State(state): State<AppState>,
    id: IdentityCtx,
    Json(req): Json<AcceptInviteRequest>,
) -> ApiResult<Json<AcceptInviteResult>> {
    // Identity-only (MAIN-98): a local invitee reaches this route signed in but
    // not yet a member of any tenant. `accept_core` still enforces every check —
    // pending+unexpired invite, email match, verified email, invited role.
    let result = accept_core(&state.db, id.user_id.0, id.tenant_id, &req.token).await?;

    // A local invitee's session points at the tenant they registered in, where
    // until now they had no membership. On a successful accept move the cookie
    // session onto the accepted tenant, so their very next request is
    // member-scoped. Scoped to the memberless case, so an ordinary member
    // accepting from their own tenant (the OIDC flow) keeps its session.
    if result.accepted && id.cookie_session && !id.is_member {
        let _ = state
            .db
            .exec(
                "UPDATE sessions_auth SET tenant_id = $2 WHERE id = $1",
                params![id.session_id.0, result.tenant_id],
            )
            .await;
    }
    Ok(Json(result))
}

/// The accept logic, split from the handler so it can be tested against a real
/// database without an `AuthCtx`. `fallback_tenant` is where a declined/no-op
/// caller stays (their own active tenant).
pub async fn accept_core(
    db: &nook_db::DbPool,
    user_id: uuid::Uuid,
    fallback_tenant: TenantId,
    token: &str,
) -> ApiResult<AcceptInviteResult> {
    let decline = |msg: &str| {
        Ok(AcceptInviteResult {
            accepted: false,
            tenant_id: fallback_tenant,
            message: msg.to_string(),
        })
    };

    // Who is accepting — email, name, and the cross-tenant person key.
    let (my_email, my_name, my_person): (String, String, uuid::Uuid) = db
        .query_one(
            "SELECT email, display_name, person_id FROM users WHERE id = $1",
            params![user_id],
        )
        .await?;

    // Look up by the token's hash — the plaintext is never at rest (AC-9).
    let invite: Option<(uuid::Uuid, TenantId, String, String, String)> = db
        .query_opt(
            "SELECT id, tenant_id, email, role, status FROM invites WHERE token_hash = $1",
            params![hash_token(token)],
        )
        .await?;
    let Some((invite_id, tenant, invite_email, role, status)) = invite else {
        return decline("this invite link is not valid");
    };

    let email_matches = my_email.to_lowercase() == invite_email.to_lowercase();

    // Already a member (by person_id) → no-op success. Consume a still-pending
    // invite only when it was addressed to THIS person's email (AC-10) —
    // otherwise the invite belongs to someone else and must stay pending rather
    // than be burned by whoever happens to click the link.
    let existing_member: Option<UserId> = db
        .query_scalar_opt(
            "SELECT u.id FROM users u
         JOIN tenant_members m
           ON m.tenant_id = u.tenant_id AND m.principal_type = 'user' AND m.principal_id = u.id
         WHERE u.tenant_id = $1 AND u.person_id = $2
         LIMIT 1",
            params![tenant, my_person],
        )
        .await?;
    if existing_member.is_some() {
        if status == "pending" && email_matches {
            let _ = db
                .exec(
                    "UPDATE invites SET status = 'accepted' WHERE id = $1",
                    params![invite_id],
                )
                .await;
        }
        return Ok(AcceptInviteResult {
            accepted: true,
            tenant_id: tenant,
            message: "you are already a member of this tenant".into(),
        });
    }

    // Consumable only when pending, unexpired, and to THIS person's email.
    if status != "pending" {
        return decline("this invite has already been used or revoked");
    }
    let fresh: bool = db
        .query_scalar(
            &format!(
                "SELECT expires_at > {} FROM invites WHERE id = $1",
                Postgres.now()
            ),
            params![invite_id],
        )
        .await?;
    if !fresh {
        return decline("this invite has expired");
    }
    if !email_matches {
        return decline("this invite was sent to a different email address");
    }

    // The email must be VERIFIED, not merely equal — email equality is the
    // MAIN-12 root cause. An unverified accepter is declined and the invite is
    // NOT consumed (AC-8), so it stays valid until they verify.
    if !email_is_verified(db, UserId(user_id)).await? {
        return decline("verify your email address first, then open the invite link again");
    }

    // Add the per-tenant user row carrying this person_id (or reuse one that
    // exists by email), then the membership grant, then consume the invite.
    let user_id: uuid::Uuid = match db
        .query_scalar_opt::<uuid::Uuid>(
            "SELECT id FROM users WHERE tenant_id = $1 AND lower(email) = lower($2) LIMIT 1",
            params![tenant, &invite_email],
        )
        .await?
    {
        Some(id) => id,
        None => {
            db.query_scalar::<uuid::Uuid>(
                "INSERT INTO users (id, tenant_id, display_name, email, role, person_id)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
                params![
                    uuid::Uuid::now_v7(),
                    tenant,
                    &my_name,
                    &invite_email,
                    &role,
                    my_person
                ],
            )
            .await?
        }
    };
    db.exec(
        "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
         VALUES ($1, $2, 'user', $3, $4)
         ON CONFLICT (tenant_id, principal_type, principal_id) DO NOTHING",
        params![uuid::Uuid::now_v7(), tenant, user_id, &role],
    )
    .await?;
    db.exec(
        "UPDATE invites SET status = 'accepted' WHERE id = $1",
        params![invite_id],
    )
    .await?;

    Ok(AcceptInviteResult {
        accepted: true,
        tenant_id: tenant,
        message: "welcome — you have joined the tenant".into(),
    })
}

#[derive(Deserialize)]
pub struct PreviewParams {
    pub token: String,
}

/// `GET /api/v1/invites/preview?token=…` — UNAUTHENTICATED (MAIN-97 AC-1).
///
/// Lets the signed-out `/accept` landing say "«Inviter» invited you to «tenant»"
/// before the visitor authenticates, so the invite is not lost to a generic
/// login screen. Returns the tenant name, inviter name, a MASKED invitee email,
/// and validity — but ONLY for a pending, unexpired token.
///
/// Every other token (missing, expired, revoked, accepted) returns the SAME
/// generic `valid: false` shell: no field distinguishes them, and the handler
/// does the SAME three queries regardless of the outcome so timing does not
/// leak which case it was.
///
/// Rate-limited per client IP (resolved via `crate::client_ip`, which only
/// believes `X-Forwarded-For` from a configured trusted proxy) → 429, because
/// an unauthenticated endpoint that touches the database must not be a free
/// anonymous amplifier.
#[utoipa::path(get, path = "/api/v1/invites/preview",
    operation_id = "preview_invite",
    params(("token" = String, Query,)),
    responses((status = 200, body = InvitePreview), (status = 429)))]
pub async fn preview(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<PreviewParams>,
) -> ApiResult<Json<InvitePreview>> {
    // Resolve the real client IP, honoring XFF only from a trusted proxy, then
    // spend one token from that IP's bucket. A stable uuid derived from the IP
    // reuses the tenant-keyed limiter without a second limiter type.
    let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let client_ip = crate::client_ip::resolve_client_ip(peer.ip(), xff, &state.cfg.trusted_proxies);
    let ip_key = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, client_ip.to_string().as_bytes());
    if !state.preview_limit.allow(TenantId(ip_key)) {
        return Err(ApiError::TooManyRequests(
            "too many invite lookups from your address — try again shortly".into(),
        ));
    }

    // One lookup by token hash. `invited_by` is nullable, and validity is
    // computed in SQL so the row is the same shape whatever the status.
    let row: Option<(TenantId, String, Option<uuid::Uuid>, bool)> = state
        .db
        .query_opt(
            &format!(
                "SELECT tenant_id, email, invited_by, (status = 'pending' AND expires_at > {})
         FROM invites WHERE token_hash = $1",
                Postgres.now()
            ),
            params![hash_token(&params.token)],
        )
        .await?;

    // Do the SAME follow-up work regardless of whether the token was found or
    // usable, so a missing/expired/revoked/accepted token cannot be told apart
    // by timing. A missing row resolves the two name lookups against nil ids
    // (both return their neutral fallbacks), which we then discard.
    let (tenant_id, email, inviter_id, valid) =
        row.unwrap_or((TenantId(uuid::Uuid::nil()), String::new(), None, false));
    let tenant = tenant_display_name(&state, tenant_id).await;
    let inviter = inviter_display_name(&state, inviter_id.unwrap_or_else(uuid::Uuid::nil)).await;

    if !valid {
        // The generic invalid response — identical for every non-usable token.
        return Ok(Json(InvitePreview::default()));
    }
    Ok(Json(InvitePreview {
        valid: true,
        tenant,
        inviter,
        email: mask_email(&email),
    }))
}

/// `POST /api/v1/invites/register` — create a LOCAL account against a pending
/// invite (MAIN-98). Unauthenticated: possession of the invite link is the
/// ticket. The account is created with the INVITE's email (never the client's),
/// unverified; a verification email is sent; the invite stays pending, because
/// registration and acceptance are separate steps (AC-1/AC-5).
///
/// Anti-enumeration (AC-3): every failure that could reveal whether an email
/// already has an account returns the SAME generic, success-shaped result — a
/// duplicate email is indistinguishable from a fresh registration. A bad or
/// duplicate USERNAME is reported, because that is about the username the
/// invitee chose and leaks nothing about the invite's email. Rate-limited per IP.
#[utoipa::path(post, path = "/api/v1/invites/register",
    operation_id = "register_invite",
    request_body = RegisterInviteRequest,
    responses((status = 200, body = RegisterInviteResult), (status = 400), (status = 429)))]
pub async fn register(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RegisterInviteRequest>,
) -> ApiResult<Json<RegisterInviteResult>> {
    // Per-IP rate limit — an unauthenticated, account-creating endpoint must not
    // be a free anonymous amplifier (reuses the preview limiter, IP-keyed).
    let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let client_ip = crate::client_ip::resolve_client_ip(peer.ip(), xff, &state.cfg.trusted_proxies);
    let ip_key = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, client_ip.to_string().as_bytes());
    if !state.preview_limit.allow(TenantId(ip_key)) {
        return Err(ApiError::TooManyRequests(
            "too many attempts from your address — try again shortly".into(),
        ));
    }

    let generic = || {
        Ok(Json(RegisterInviteResult {
            message:
                "If the invite is valid, check your email to verify your account, then sign in."
                    .into(),
        }))
    };

    // The invite: tenant, the email the account MUST use, the role acceptance
    // will apply, and whether it is pending + unexpired.
    let invite: Option<(TenantId, String, String, bool)> = state
        .db
        .query_opt(
            &format!(
                "SELECT tenant_id, email, role, (status = 'pending' AND expires_at > {})
         FROM invites WHERE token_hash = $1",
                Postgres.now()
            ),
            params![hash_token(&req.token)],
        )
        .await?;
    let Some((tenant, email, role, true)) = invite else {
        return Err(ApiError::BadRequest(
            "this invite link is not valid or has expired".into(),
        ));
    };

    // Local registration only where local auth is available — never on an
    // OIDC-claimed tenant (AC-3/NG-2).
    use crate::services::local_auth::{self, AuthMode};
    if matches!(
        local_auth::mode_of(&state.db, tenant).await?,
        Some(AuthMode::Oidc)
    ) {
        return Err(ApiError::BadRequest(
            "this instance signs in through an identity provider; local registration is unavailable"
                .into(),
        ));
    }

    // Anti-enumeration: if an account already exists for the invite's email, do
    // not create or touch anything and return the SAME generic result a fresh
    // registration returns (AC-3). Hash the password anyway so timing matches.
    let exists: Option<uuid::Uuid> = state
        .db
        .query_scalar_opt(
            "SELECT id FROM users WHERE tenant_id = $1 AND lower(email) = lower($2) LIMIT 1",
            params![tenant, &email],
        )
        .await?;
    if exists.is_some() {
        let _ = crate::auth::password::hash(&req.password);
        return generic();
    }

    // Create the local account — invite's email, unverified, NO membership. A
    // duplicate/invalid username is a clean 400 (about the chosen username).
    let user = local_auth::register_invited(
        &state.db,
        tenant,
        &req.username,
        &email,
        &req.name,
        &req.password,
        &role,
    )
    .await?;

    // Send the verification email (best-effort; a mail failure is neither fatal
    // nor disclosed). The invite is deliberately left pending (AC-5).
    let _ = crate::routes::verify_email::request_core(&state, user.id).await;
    crate::events::record(
        &state,
        tenant,
        crate::events::EventDraft::new("invite.registered").actor("user", user.id.0),
    )
    .await;

    generic()
}

#[cfg(test)]
mod tests {
    /// Source guards: create/list/revoke are admin-gated; owner is not invitable.
    fn body(name: &str) -> &'static str {
        include_str!("invites.rs")
            .split(&format!("pub async fn {name}("))
            .nth(1)
            .expect("fn")
            .split("\npub async fn ")
            .next()
            .expect("body")
    }
    #[test]
    fn management_is_admin_gated_and_owner_is_not_invitable() {
        for f in ["create", "list", "revoke", "resend"] {
            assert!(
                body(f).contains("require_admin_of"),
                "{f} must be admin-gated"
            );
        }
        assert!(
            super::validated_role(Some("owner")).is_err(),
            "owner not invitable (NG-3)"
        );
        assert!(super::validated_role(Some("admin")).is_ok());
        assert!(super::validated_role(None).is_ok(), "defaults to member");
    }

    use super::{invite_email, mask_email};

    #[test]
    fn mask_email_keeps_first_char_and_domain_only() {
        assert_eq!(mask_email("ryan@example.com"), "r…@example.com");
        // A single-char local part still only shows that one char.
        assert_eq!(mask_email("a@b.co"), "a…@b.co");
        // Something that isn't an address is masked wholesale, never leaked.
        assert_eq!(mask_email("not-an-email"), "…");
        assert_eq!(mask_email("@nope.com"), "…");
        assert_eq!(mask_email("nolocal@"), "…");
    }

    #[test]
    fn invite_email_carries_the_link_tenant_inviter_and_escapes_html() {
        let (subject, text, html) = invite_email(
            "Acme <Web>",
            "Dana \"D\" Lee",
            "https://nook.example/accept?token=abc",
        );
        // Subject names the tenant; text names the inviter and carries the link.
        assert!(subject.contains("Acme <Web>"));
        assert!(text.contains("Dana \"D\" Lee invited you"));
        assert!(text.contains("https://nook.example/accept?token=abc"));
        // The HTML links the same token URL (AC-4).
        assert!(html.contains("href=\"https://nook.example/accept?token=abc\""));
        // Values dropped into HTML are escaped, so a name/tenant cannot inject
        // markup.
        assert!(html.contains("Acme &lt;Web&gt;"));
        assert!(!html.contains("Acme <Web>"));
        assert!(html.contains("&quot;D&quot;"));
    }
}

#[cfg(test)]
mod db_tests {
    use super::accept_core;
    use crate::seed::hash_token;
    use nook_db::{params, Db, DbPool, Json};
    use nook_db::{Postgres, TypeMapping};
    use nook_types::TenantId;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    async fn pool() -> Option<DbPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&std::env::var("DATABASE_URL").ok()?)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(nook_db::EnginePool::from_pg(db))
    }
    async fn tenant(db: &DbPool, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1,$2,$3)",
            params![id, name, format!("{name}-{id}")],
        )
        .await
        .unwrap();
        id
    }
    /// A users row (a person, by person_id) in a tenant.
    async fn user(db: &DbPool, tenant: Uuid, email: &str, person: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        db.exec("INSERT INTO users (id,tenant_id,display_name,email,role,person_id) VALUES ($1,$2,'P',$3,'owner',$4)",
            params![id, tenant, email, person]).await.unwrap();
        id
    }
    async fn invite(db: &DbPool, tenant: Uuid, email: &str, days: i64) -> String {
        let token = format!("inv_{}", Uuid::new_v4().simple());
        // Stored hashed at rest (AC-9); the helper hands back the plaintext.
        db.exec(
            &format!(
                "INSERT INTO invites (id,tenant_id,email,role,token_hash,status,expires_at)
             VALUES ($1,$2,$3,'member',$4,'pending', {now} + make_interval(days => {days}))",
                now = Postgres.now(),
                days = Postgres.cast("$5", "int")
            ),
            params![
                Uuid::new_v4(),
                tenant,
                email,
                hash_token(&token),
                days as i32
            ],
        )
        .await
        .unwrap();
        token
    }

    /// Mark a user's email verified (a verified identity), so an accept can pass
    /// the AC-8 gate.
    async fn verify(db: &DbPool, user_id: Uuid, email: &str) {
        let sql = format!(
            "INSERT INTO identities (id,user_id,issuer,subject,email,raw_claims,email_verified_at)
             VALUES ($1,$2,'local',$3,$4,{}, {now})",
            nook_db::Postgres.literal("{}"),
            now = Postgres.now()
        );
        db.exec(
            &sql,
            params![Uuid::now_v7(), user_id, user_id.to_string(), email],
        )
        .await
        .unwrap();
    }
    async fn is_member(db: &DbPool, tenant: Uuid, person: Uuid) -> bool {
        let n: i64 = db
            .query_scalar(
                "SELECT count(*) FROM users u JOIN tenant_members m
               ON m.tenant_id=u.tenant_id AND m.principal_type='user' AND m.principal_id=u.id
             WHERE u.tenant_id=$1 AND u.person_id=$2",
                params![tenant, person],
            )
            .await
            .unwrap();
        n > 0
    }
    async fn cleanup(db: &DbPool, tenants: &[Uuid]) {
        for t in tenants {
            for tbl in ["invites", "tenant_members", "users"] {
                let _ = db
                    .exec(&format!("DELETE FROM {tbl} WHERE tenant_id=$1"), params![t])
                    .await;
            }
            let _ = db.exec("DELETE FROM tenants WHERE id=$1", params![t]).await;
        }
    }

    #[tokio::test]
    async fn accept_consumes_only_on_match_and_is_idempotent() {
        let Some(db) = pool().await else { return };
        let shared = tenant(&db, "shared").await;
        let home = tenant(&db, "home").await;
        // Separate tenant for the expired case: the good invite reuses this
        // email in `shared`, and one-pending-per-email forbids two there.
        let stale = tenant(&db, "stale").await;
        let person = Uuid::new_v4();
        let me = user(&db, home, "invitee@i6.test", person).await;

        // Wrong email invite → declined, no membership.
        let wrong = invite(&db, shared, "someone-else@i6.test", 7).await;
        let r_wrong = accept_core(&db, me, TenantId(home), &wrong).await.unwrap();

        // Expired invite (own tenant) → declined.
        let expired = invite(&db, stale, "invitee@i6.test", -1).await;
        let r_expired = accept_core(&db, me, TenantId(home), &expired)
            .await
            .unwrap();

        // Unknown token → declined.
        let r_unknown = accept_core(&db, me, TenantId(home), "inv_nope")
            .await
            .unwrap();
        let member_before = is_member(&db, shared, person).await;

        // Good invite, matching email, but the accepter is NOT verified →
        // declined and the invite is NOT consumed (AC-8).
        let good = invite(&db, shared, "invitee@i6.test", 7).await;
        let r_unverified = accept_core(&db, me, TenantId(home), &good).await.unwrap();
        let member_after_unverified = is_member(&db, shared, person).await;
        // The token is stored hashed, never in plaintext (AC-9).
        let stored_hash: String = db
            .query_scalar(
                "SELECT token_hash FROM invites WHERE tenant_id=$1 AND status='pending' AND lower(email)=lower($2)",
                params![shared, "invitee@i6.test"],
            )
            .await
            .unwrap();

        // Verify the address; the same link now works (AC-8).
        verify(&db, me, "invitee@i6.test").await;
        let r_ok = accept_core(&db, me, TenantId(home), &good).await.unwrap();
        let member_after = is_member(&db, shared, person).await;
        // Idempotent: second accept is a no-op success; token can't be reused for a NEW membership.
        let r_again = accept_core(&db, me, TenantId(home), &good).await.unwrap();
        let member_rows: i64 = db
            .query_scalar(
                "SELECT count(*) FROM tenant_members m JOIN users u ON u.id=m.principal_id
             WHERE u.tenant_id=$1 AND u.person_id=$2",
                params![shared, person],
            )
            .await
            .unwrap();

        cleanup(&db, &[shared, home, stale]).await;

        assert!(
            !r_wrong.accepted && r_wrong.tenant_id == TenantId(home),
            "email mismatch declined"
        );
        assert!(!r_expired.accepted, "expired declined");
        assert!(!r_unknown.accepted, "unknown token declined");
        assert!(!member_before, "no membership before a valid accept");
        assert!(
            !r_unverified.accepted,
            "an unverified accepter is declined (AC-8)"
        );
        assert!(
            !member_after_unverified,
            "an unverified accept neither joins nor consumes the invite (AC-8)"
        );
        assert_eq!(
            stored_hash,
            hash_token(&good),
            "token stored hashed, not plaintext (AC-9)"
        );
        assert_ne!(
            stored_hash, good,
            "the plaintext token is never at rest (AC-9)"
        );
        assert!(
            r_ok.accepted && r_ok.tenant_id == TenantId(shared),
            "a verified accept lands in shared"
        );
        assert!(member_after, "membership created");
        assert!(
            r_again.accepted,
            "second accept is a no-op success (idempotent)"
        );
        assert_eq!(member_rows, 1, "no duplicate membership from re-accept");
    }

    /// AC-10: a member who presents an invite addressed to a DIFFERENT email
    /// gets the already-member no-op success, but the invite is NOT consumed —
    /// it stays pending for the person it was actually for.
    #[tokio::test]
    async fn already_member_does_not_burn_a_mismatched_email_invite() {
        let Some(db) = pool().await else { return };
        let shared = tenant(&db, "amshare").await;
        let home = tenant(&db, "amhome").await;
        let person = Uuid::new_v4();
        // `me` is already a member of `shared` (a users row + grant there).
        let me = user(&db, home, "me@i10.test", person).await;
        let mine_in_shared = user(&db, shared, "me@i10.test", person).await;
        db.exec(
            "INSERT INTO tenant_members (id,tenant_id,principal_type,principal_id,role)
             VALUES ($1,$2,'user',$3,'member')",
            params![Uuid::new_v4(), shared, mine_in_shared],
        )
        .await
        .unwrap();

        // An invite in `shared` for SOMEONE ELSE's email.
        let others = invite(&db, shared, "colleague@i10.test", 7).await;
        let r = accept_core(&db, me, TenantId(home), &others).await.unwrap();

        let still_pending: i64 = db
            .query_scalar(
                "SELECT count(*) FROM invites WHERE tenant_id=$1 AND status='pending' AND lower(email)=lower($2)",
                params![shared, "colleague@i10.test"],
            )
            .await
            .unwrap();

        cleanup(&db, &[shared, home]).await;

        assert!(
            r.accepted,
            "already-a-member is still a no-op success (AC-6)"
        );
        assert_eq!(
            still_pending, 1,
            "a mismatched-email invite is NOT consumed by an existing member (AC-10)"
        );
    }

    /// AC-8: acceptance is gated on a VERIFIED email. An accepter whose email
    /// EQUALS the invite's but is not verified is declined, and the invite is
    /// NOT consumed — the same link works once the address is verified. Pinned
    /// on its own (the omnibus test also asserts it) so the gate is a clear,
    /// hard-to-weaken regression signal, mirroring the AC-10 test above.
    #[tokio::test]
    async fn an_unverified_accepter_is_declined_and_the_invite_survives() {
        let Some(db) = pool().await else { return };
        let shared = tenant(&db, "ac8share").await;
        let home = tenant(&db, "ac8home").await;
        let person = Uuid::new_v4();
        // The accepter's email EQUALS the invite email, but they are NOT
        // verified — no `verify(...)`, so no identity carries `email_verified_at`.
        let me = user(&db, home, "invitee@i8.test", person).await;

        // A pending, unexpired invite in `shared` for that exact email.
        let token = invite(&db, shared, "invitee@i8.test", 7).await;

        // Unverified accept → declined, and nothing joins.
        let declined = accept_core(&db, me, TenantId(home), &token).await.unwrap();
        let member_after_decline = is_member(&db, shared, person).await;
        let still_pending: i64 = db
            .query_scalar(
                "SELECT count(*) FROM invites WHERE tenant_id=$1 AND status='pending' AND lower(email)=lower($2)",
                params![shared, "invitee@i8.test"],
            )
            .await
            .unwrap();

        // Verifying the address is the ONLY thing that was missing: the SAME
        // token now accepts — proving the decline was the AC-8 gate, not a
        // mismatch or an expiry.
        verify(&db, me, "invitee@i8.test").await;
        let accepted = accept_core(&db, me, TenantId(home), &token).await.unwrap();
        let member_after_verify = is_member(&db, shared, person).await;

        cleanup(&db, &[shared, home]).await;

        assert!(
            !declined.accepted,
            "an unverified accepter is declined (AC-8)"
        );
        assert_eq!(
            declined.tenant_id,
            TenantId(home),
            "the declined accepter stays in their own tenant"
        );
        assert!(
            declined
                .message
                .to_lowercase()
                .contains("verify your email"),
            "declined for the verification gate specifically, got: {:?}",
            declined.message
        );
        assert!(
            !member_after_decline,
            "no membership is created for an unverified accept (AC-8)"
        );
        assert_eq!(
            still_pending, 1,
            "the invite is NOT consumed — it stays pending until the email is verified (AC-8)"
        );
        assert!(
            accepted.accepted && accepted.tenant_id == TenantId(shared),
            "once verified, the SAME link accepts — the gate was the only blocker"
        );
        assert!(
            member_after_verify,
            "the verified accept creates the membership"
        );
    }
}
