//! Authentication: generic OIDC (any standards-compliant IdP) + opaque
//! server-side sessions. Identity always belongs to the customer's IdP.

pub mod password;

pub mod perm;
pub mod scopes;
pub mod session_guard;

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::{Cookie, CookieJar, Key, SameSite};
use openidconnect::core::CoreProviderMetadata;
use openidconnect::IssuerUrl;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use nook_types::{AuthSessionId, TenantId, UserId};

use crate::error::ApiError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "nook_session";
pub const FLOW_COOKIE: &str = "nook_oidc_flow";

/// Discovered IdP metadata, cached at startup. The OIDC client itself is
/// rebuilt per request from this (pure construction, no network).
pub struct OidcContext {
    pub metadata: CoreProviderMetadata,
    pub http: openidconnect::reqwest::Client,
}

impl OidcContext {
    pub async fn discover(issuer_url: &str) -> anyhow::Result<Self> {
        let http = openidconnect::reqwest::ClientBuilder::new()
            // Never follow redirects during token exchange (OIDC spec hygiene).
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()?;
        let metadata =
            CoreProviderMetadata::discover_async(IssuerUrl::new(issuer_url.to_string())?, &http)
                .await?;
        Ok(Self { metadata, http })
    }
}

/// The instance's OIDC discovery state, hot-swappable after boot (MAIN-169).
///
/// Discovery used to run once at startup: if the IdP was unreachable at that
/// moment, OIDC login stayed disabled for the whole life of the process and the
/// login page silently fell back to a local password form. This holds the
/// *configured* issuer separately from the *discovered* context, so the three
/// observable states are distinguishable and discovery can succeed later without
/// a restart:
///
/// - **not configured** — no issuer set: `configured() == false`.
/// - **configured and usable** — discovery has succeeded: `current()` is `Some`.
/// - **configured but degraded** — the IdP was unreachable and discovery has
///   not yet succeeded: `degraded() == true`, `current()` is `None`.
///
/// A background task retries while degraded, and [`login`] triggers an immediate
/// on-demand attempt, so recovery needs no operator action.
pub struct OidcState {
    /// The configured issuer, present only when OIDC is fully configured
    /// (`Config::oidc_configured()`). `None` means "no identity provider".
    issuer: Option<String>,
    /// The discovered context, `None` until discovery first succeeds. A cheap
    /// `std::sync::RwLock` — reads (every login/callback/providers) clone the
    /// `Arc` and release at once; the single write happens on discovery.
    ctx: std::sync::RwLock<Option<Arc<OidcContext>>>,
    /// Single-flight for on-demand + background discovery: held across the
    /// network round-trip so a burst of concurrent logins makes ONE attempt at
    /// the IdP, not one per request.
    discovering: tokio::sync::Mutex<()>,
}

impl OidcState {
    /// Build from config, optionally seeding an already-discovered context (the
    /// one-shot attempt the server still makes at boot). When OIDC is not fully
    /// configured the issuer is `None` and every state query reports "not
    /// configured", regardless of any seed.
    pub fn new(cfg: &crate::config::Config, discovered: Option<OidcContext>) -> Self {
        let issuer = if cfg.oidc_configured() {
            cfg.oidc_issuer_url.clone()
        } else {
            None
        };
        let ctx = std::sync::RwLock::new(match (&issuer, discovered) {
            (Some(_), Some(c)) => Some(Arc::new(c)),
            _ => None,
        });
        Self {
            issuer,
            ctx,
            discovering: tokio::sync::Mutex::new(()),
        }
    }

    /// Is an identity provider configured on this instance at all?
    pub fn configured(&self) -> bool {
        self.issuer.is_some()
    }

    /// The usable context, or `None` when not configured or still degraded.
    pub fn current(&self) -> Option<Arc<OidcContext>> {
        self.ctx.read().expect("oidc ctx lock").clone()
    }

    /// Configured, but discovery has not yet succeeded — the IdP is unreachable.
    pub fn degraded(&self) -> bool {
        self.configured() && self.current().is_none()
    }

    /// Attempt discovery now, storing and returning the context on success.
    /// `Ok(None)` means "not configured" (nothing to discover); `Err` means the
    /// IdP was unreachable. Single-flight and idempotent: an already-discovered
    /// context short-circuits without a network call, and concurrent callers
    /// coalesce onto one attempt.
    pub async fn discover_now(&self) -> anyhow::Result<Option<Arc<OidcContext>>> {
        let Some(issuer) = self.issuer.clone() else {
            return Ok(None);
        };
        if let Some(c) = self.current() {
            return Ok(Some(c));
        }
        let _guard = self.discovering.lock().await;
        // Another caller may have discovered while we waited for the lock.
        if let Some(c) = self.current() {
            return Ok(Some(c));
        }
        let ctx = Arc::new(OidcContext::discover(&issuer).await?);
        *self.ctx.write().expect("oidc ctx lock") = Some(ctx.clone());
        tracing::info!(issuer = %issuer, "OIDC discovery complete");
        Ok(Some(ctx))
    }
}

#[cfg(test)]
mod oidc_state_tests {
    use super::*;
    use nook_infra::Config;

    fn cfg_with_issuer(issuer: Option<&str>) -> Config {
        let mut c = Config::for_test();
        c.oidc_issuer_url = issuer.map(str::to_string);
        // `oidc_configured()` also requires a client id and a redirect url.
        c.oidc_client_id = issuer.map(|_| "test-client".to_string());
        c.oidc_redirect_url = issuer.map(|_| "https://nook.example.test/callback".to_string());
        c
    }

    #[test]
    fn not_configured_is_never_degraded() {
        let s = OidcState::new(&cfg_with_issuer(None), None);
        assert!(!s.configured());
        assert!(!s.degraded());
        assert!(s.current().is_none());
    }

    #[test]
    fn configured_without_discovery_is_degraded() {
        let s = OidcState::new(&cfg_with_issuer(Some("https://idp.example.test")), None);
        assert!(s.configured());
        assert!(
            s.degraded(),
            "configured but not yet discovered => degraded"
        );
        assert!(s.current().is_none());
    }

    #[tokio::test]
    async fn on_demand_discovery_against_an_unreachable_idp_stays_degraded() {
        // A host that will not resolve/connect stands in for the outage.
        let s = OidcState::new(
            &cfg_with_issuer(Some("https://idp.invalid.nonexistent.test")),
            None,
        );
        let err = s.discover_now().await;
        assert!(
            err.is_err(),
            "discovery must fail against an unreachable IdP"
        );
        assert!(s.degraded(), "a failed attempt leaves the state degraded");
        assert!(s.current().is_none());
    }

    #[tokio::test]
    async fn discover_now_is_a_noop_when_not_configured() {
        let s = OidcState::new(&cfg_with_issuer(None), None);
        let out = s
            .discover_now()
            .await
            .expect("not-configured is not an error");
        assert!(out.is_none());
    }
}

/// In-flight OIDC login state, carried in an encrypted short-lived cookie.
#[derive(Serialize, Deserialize)]
pub struct FlowState {
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub next: String,
}

/// What is calling: a person at a browser, or a machine presenting its node
/// token.
///
/// The distinction matters because a node token lives in a file on a machine
/// whose whole job is running other people's code. It is a service credential,
/// not a stand-in for the owner, and the things only a human should do —
/// setting the vault password, enrolling machines — are refused to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    /// A signed-in person (session cookie).
    User,
    /// A joined machine. Authenticated by its client CERTIFICATE where the
    /// connection has one, and by its node token otherwise — the token path is
    /// transitional and disappears once every machine has enrolled.
    ///
    /// Either way the confinement is the same, which is the point: rebasing
    /// identity onto certificates must not change *what a machine may do*,
    /// only how it proves who it is.
    Node(nook_types::NodeId),
}

/// The authenticated caller. Every tenant-scoped query takes its tenant_id
/// from here and nowhere else.
#[derive(Debug, Clone, Copy)]
pub struct AuthCtx {
    pub session_id: AuthSessionId,
    pub user_id: UserId,
    pub tenant_id: TenantId,
    pub principal: Principal,
    /// True only when this caller is backed by a real `sessions_auth` row (a
    /// browser cookie session) — the kind a tenant switch can move. A user
    /// token is also `Principal::User` but sets this `false`, so `switch_tenant`
    /// can tell "structurally cannot switch" (token → 400) from "the session
    /// row vanished mid-request" (cookie → 401) without inferring it from
    /// `rows_affected`.
    pub cookie_session: bool,
}

impl AuthCtx {
    /// Build a caller from a verified client certificate.
    ///
    /// `verify_node_cert` has already checked the chain, validity, revocation
    /// and — critically — that the certificate's tenant matches the node
    /// record it names. This just carries that decision into the type the rest
    /// of the API authorises against, so `require_node_self` keeps working
    /// unchanged: the certificate proves *who*, tenant scoping decides *what*.
    pub async fn from_node_cert(state: &AppState, cert_der: &[u8]) -> Result<Self, ApiError> {
        let id = crate::ca::verify_node_cert(&*state.tenant_cas, &*state.nodes, cert_der)
            .await
            .map_err(|e| ApiError::ForbiddenMsg(e.to_string()))?;

        // A node acts as the tenant's owner for attribution, exactly as the
        // node-token path does — it is a machine credential, not a person, and
        // `require_user` is what keeps it from becoming one.
        let owner = state.identity.tenant_owner_user_id(id.tenant_id).await?;

        Ok(AuthCtx {
            session_id: AuthSessionId(id.node_id),
            user_id: UserId(owner.unwrap_or(id.node_id)),
            tenant_id: TenantId(id.tenant_id),
            principal: Principal::Node(nook_types::NodeId(id.node_id)),
            cookie_session: false,
        })
    }

    /// Confine a machine credential to its own machine.
    ///
    /// A node token authenticates the machine it sits on. Letting it act on a
    /// *different* node turns one compromised box into every box: starting a
    /// session is running a command, and the fleet is exactly the set of
    /// machines you did not want that to reach. Humans are unrestricted —
    /// driving other nodes is the entire point of the control plane.
    pub fn require_node_self(&self, node_id: nook_types::NodeId) -> Result<(), ApiError> {
        match self.principal {
            Principal::User => Ok(()),
            Principal::Node(self_id) if self_id == node_id => Ok(()),
            Principal::Node(_) => Err(ApiError::ForbiddenMsg(
                "a node token can only act on its own machine — sign in as a user \
                 to drive another node"
                    .into(),
            )),
        }
    }

    /// A machine acting on ITSELF, with no human path through — for credential
    /// DELIVERY (MAIN-367).
    ///
    /// [`Self::require_node_self`] deliberately waves signed-in humans through,
    /// because driving other machines is what the control plane is for. That is
    /// the wrong shape for a route that hands back a decrypted private ssh key:
    /// a person has no reason to pull one out of the control plane, and the shim
    /// that genuinely needs it runs ON the node and can present the node's own
    /// credential.
    ///
    /// Owner's ruling on MAIN-367 (2026-08-03): the delivery endpoint exists —
    /// AC-7 forbids the key reaching a browser, a log or an event, not the one
    /// channel the shim needs — **and it is machine-only**. Before this, the
    /// route used `require_node_may_use`, whose user leg calls
    /// [`require_person_may_use_node`] and returns `Ok` for any member of a
    /// tenant the node is SHARED with. A shared operator node is the normal
    /// case, so a session cookie was enough to read any workspace's key off it.
    ///
    /// Named once, and separately, so a future delivery route cannot reach for
    /// the laxer guard by accident — the two differ by one word and by every
    /// human on the tenant.
    pub fn require_node_is_self(&self, node_id: nook_types::NodeId) -> Result<(), ApiError> {
        match self.principal {
            Principal::Node(self_id) if self_id == node_id => Ok(()),
            _ => Err(ApiError::ForbiddenMsg(
                "this endpoint delivers key material to a machine acting on \
                 itself; a user credential cannot fetch a workspace's key"
                    .into(),
            )),
        }
    }

    /// Confine a SESSION SPAWN to the machine's owner (MAIN-130).
    ///
    /// `require_node_self` waved every signed-in human straight through, so any
    /// invited member could open a shell/agent on a teammate's box. Starting a
    /// session is running a program on that machine, so it now requires that the
    /// caller's PERSON owns the node (`nodes.owner_person_id`, set at join by
    /// MAIN-119). There is no admin or owner bypass — the epic's rule is
    /// ownership, full stop.
    ///
    /// A node credential keeps its existing machine-only confinement — that
    /// lateral-movement boundary is unchanged (AC-4); this only tightens the
    /// human path from "any user" to "the owner".
    pub async fn require_node_owner(
        &self,
        state: &AppState,
        node_id: nook_types::NodeId,
    ) -> Result<(), ApiError> {
        if let Principal::Node(self_id) = self.principal {
            return if self_id == node_id {
                Ok(())
            } else {
                Err(ApiError::ForbiddenMsg(
                    "a node token can only act on its own machine".into(),
                ))
            };
        }
        require_person_owns_node(state, self.tenant_id, Some(self.user_id), node_id).await
    }

    /// Session-start authorization: the caller may run on this machine if they
    /// OWN it or it is SHARED (MAIN-136). The user-facing counterpart of
    /// `require_node_owner`, used by the session-start routes so a member can
    /// use a teammate's shared node — while management stays owner-only (NG-1).
    ///
    /// A node credential keeps its existing machine-only confinement unchanged:
    /// a node token acts on its own machine and nothing else — sharing is a
    /// human affordance, not a lateral-movement widening.
    pub async fn require_node_may_use(
        &self,
        state: &AppState,
        node_id: nook_types::NodeId,
    ) -> Result<(), ApiError> {
        if let Principal::Node(self_id) = self.principal {
            return if self_id == node_id {
                Ok(())
            } else {
                Err(ApiError::ForbiddenMsg(
                    "a node token can only act on its own machine".into(),
                ))
            };
        }
        require_person_may_use_node(state, self.tenant_id, Some(self.user_id), node_id).await
    }

    /// Require a tenant owner or admin.
    ///
    /// CA lifecycle — generating, staging, promoting, retiring, revoking a node
    /// — is authority over every machine in a tenant, so it is gated on role
    /// rather than on merely being signed in. The role columns have existed
    /// since 0001 with a CHECK constraint and have never been enforced; this is
    /// where they start meaning something.
    ///
    /// Scoped to the caller's OWN tenant by construction: `tenant_id` comes
    /// from the authenticated context, never from the request, so a tenant
    /// admin has no way to name someone else's tenant.
    pub async fn require_tenant_admin(&self, state: &AppState) -> Result<(), ApiError> {
        // `require_user` first, so a node credential still gets its specific
        // "sign in as a user" message rather than the generic role refusal; the
        // role decision itself reuses the one `is_tenant_admin` query (MAIN-137).
        self.require_user()?;
        if self.is_tenant_admin(state.identity.as_ref()).await? {
            Ok(())
        } else {
            Err(ApiError::ForbiddenMsg(
                "this needs tenant owner or admin".into(),
            ))
        }
    }

    /// Is this caller a tenant owner or admin? The boolean form of
    /// `require_tenant_admin`, for scoping a listing rather than gating an
    /// action (MAIN-132): a member sees only their own resources, an admin the
    /// whole tenant. A node credential is not a role-holder — `false`.
    pub async fn is_tenant_admin(
        &self,
        repo: &dyn crate::repo::identity::IdentityRepository,
    ) -> Result<bool, ApiError> {
        if !matches!(self.principal, Principal::User) {
            return Ok(false);
        }
        let role = repo.role_in_tenant(self.user_id, self.tenant_id).await?;
        Ok(matches!(role.as_deref(), Some("owner") | Some("admin")))
    }

    /// Refuse machine credentials.
    ///
    /// For operations that grant lasting power rather than doing today's work:
    /// taking over the secret vault, enrolling more machines, removing a node.
    /// A stolen node token should let an attacker do what that node already
    /// does — not become the account.
    pub fn require_user(&self) -> Result<(), ApiError> {
        match self.principal {
            Principal::User => Ok(()),
            Principal::Node(_) => Err(ApiError::ForbiddenMsg(
                "a node token cannot do this — sign in as a user".into(),
            )),
        }
    }
}

/// The person behind a user row — the cross-tenant identity that outlives any
/// single tenant membership. Promoted here from the notebook (MAIN-130) so the
/// notebook and the node-ownership check resolve the person exactly one way.
pub async fn person_id_of(state: &AppState, user_id: UserId) -> Result<Uuid, ApiError> {
    state
        .identity
        .person_id_of(user_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Spawn authorization: a session starts on a machine only for the PERSON who
/// owns it (MAIN-130). The single chokepoint the HTTP session routes and
/// `start_work` (HTTP + MCP) all call, so the rule has one definition.
///
/// `actor` is the acting user, or `None` when the caller carries no per-user
/// identity — today only the MCP path, whose sessions are `created_by = NULL`.
/// A `None` actor is REFUSED rather than run as the tenant owner: MCP per-user
/// identity (MAIN-102) has to land before MCP can spawn (AC-3). An ownerless
/// node (`owner_person_id IS NULL`) refuses everyone. A node absent from the
/// caller's tenant is a 404, not a 403 — the same not-found the routes already
/// give, so ownership does not become an existence oracle.
pub async fn require_person_owns_node(
    state: &AppState,
    tenant: TenantId,
    actor: Option<UserId>,
    node_id: nook_types::NodeId,
) -> Result<(), ApiError> {
    let Some(actor) = actor else {
        return Err(ApiError::ForbiddenMsg(
            "starting a session needs an acting identity; the MCP path has none \
             until per-user MCP identity lands (MAIN-102), and it must not run as \
             the tenant owner"
                .into(),
        ));
    };
    // Unscoped (MAIN-353): ownership is keyed on the PERSON, so "is this my
    // machine?" is answerable from any org that person belongs to.
    let owner = state.identity.node_owner_person_unscoped(node_id.0).await?;
    let person = person_id_of(state, actor).await?;
    if let Some(Some(o)) = owner {
        if o == person {
            return Ok(());
        }
    }
    // Only reached on refusal, so the extra lookup costs nothing in the common
    // case; it exists solely to pick the status.
    let in_caller_tenant = state
        .identity
        .node_owner_and_shared(node_id.0, tenant)
        .await?
        .is_some();
    Err(refusal(
        in_caller_tenant,
        "sessions start only on your own machines",
    ))
}

/// Which refusal a not-yours node earns: `403` inside the caller's own tenant,
/// `404` everywhere else (MAIN-353 AC-8).
///
/// Before MAIN-353 this fell out of the lookup being tenant-scoped — a foreign
/// node simply was not found. Widening the owner leg to unscoped removed that
/// for free, and a plain `Forbidden` on the wider lookup turns reachability into
/// an existence oracle: from org B you could probe any id and read 403 as "that
/// machine exists in an org you cannot see".
///
/// Inside the caller's tenant a 403 leaks nothing — they can already list the
/// node — and it stays the honest, debuggable answer, so a mis-dispatch onto an
/// unowned node still reads as "not yours" rather than "no such machine".
fn refusal(in_caller_tenant: bool, msg: &str) -> ApiError {
    if in_caller_tenant {
        ApiError::ForbiddenMsg(msg.into())
    } else {
        ApiError::NotFound
    }
}

/// Session-start authorization that also allows a SHARED node (MAIN-136). Same
/// shape and same hard edges as [`require_person_owns_node`], but a node with
/// `shared = true` is usable by any member of the tenant, not only its owner —
/// the epic's "team-usable" step. This relaxes USE only; node *management*
/// (share/unshare, update, remove) stays strictly owner-gated via
/// `require_person_owns_node`/`require_node_owner` (NG-1).
///
/// The edges are unchanged: a `None` actor (the MCP path) is REFUSED — sharing
/// grants no bypass of the missing-per-user-identity rule (MAIN-102); a node
/// absent from the caller's tenant is a 404, not a 403 (no existence oracle);
/// and a non-shared node the caller does not own is refused, now with a message
/// that names shared machines as the exception.
pub async fn require_person_may_use_node(
    state: &AppState,
    tenant: TenantId,
    actor: Option<UserId>,
    node_id: nook_types::NodeId,
) -> Result<(), ApiError> {
    let Some(actor) = actor else {
        return Err(ApiError::ForbiddenMsg(
            "starting a session needs an acting identity; the MCP path has none \
             until per-user MCP identity lands (MAIN-102), and it must not run as \
             the tenant owner"
                .into(),
        ));
    };
    // The SHARED leg stays tenant-local (MAIN-353 AC-5): sharing is a grant to
    // *this* team, and a person carrying it into another org would be handing
    // someone else's machine to an org its owner never consented to. So this
    // lookup keeps its tenant scope.
    let row = state
        .identity
        .node_owner_and_shared(node_id.0, tenant)
        .await?;
    if let Some((_, true)) = row {
        // Shared is a team grant: any member of the tenant may run here. (A node
        // with no owner cannot be shared — 0017's invariant — so `shared` already
        // implies a consenting owner.)
        return Ok(());
    }
    // The OWNER leg travels. Asked unscoped, so the owner reaches their own
    // machine from any of their orgs.
    let owner = state.identity.node_owner_person_unscoped(node_id.0).await?;
    let person = person_id_of(state, actor).await?;
    if let Some(Some(o)) = owner {
        if o == person {
            return Ok(());
        }
    }
    // `row` already answered "is it in the caller's tenant?" — no second query.
    Err(refusal(
        row.is_some(),
        "sessions start only on your own or shared machines",
    ))
}

impl FromRequestParts<AppState> for AuthCtx {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A client certificate outranks everything else: it is the strongest
        // credential on offer and the only one that cannot be replayed from a
        // stolen file alone. Checked first so a machine that has enrolled is
        // identified by what it proved in the handshake, not by a token it
        // also happens to still carry.
        if let Some(cert) = parts.extensions.get::<crate::agent_tls::PeerCertificate>() {
            return AuthCtx::from_node_cert(state, &cert.0).await;
        }

        // Browsers authenticate with the session cookie; everything else
        // presents a bearer token. The prefix says which kind, because the two
        // are not interchangeable: a user token IS the person (it can drive any
        // machine they own), a node token is only the machine it sits on.
        if let Some(bearer) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            if bearer.starts_with(USER_TOKEN_PREFIX) {
                let (ctx, grant) = user_token_ctx(state, bearer).await?;
                let ctx = retenant(state, ctx, parts).await?;
                // THE chokepoint (MAIN-602), and the reason it lives in the
                // extractor: every REST route resolves an `AuthCtx`, so a route
                // added tomorrow is covered without anyone remembering to cover
                // it — and `required_scope` defaults to refusing, so the cost of
                // forgetting is a scoped token that cannot reach the new route.
                // Runs after `retenant` so the narrowing is checked against the
                // tenant the request is actually answered in.
                scopes::authorize(state, ctx.tenant_id, grant, parts).await?;
                return Ok(ctx);
            }
            let ctx = node_token_ctx(state, bearer).await?;
            return retenant(state, ctx, parts).await;
        }

        // WebSockets cannot carry an Authorization header — the browser API has
        // no way to set one — and a cross-origin socket sends no cookie either.
        // The subprotocol field is the one header a client controls, so a token
        // rides there. Chosen over `?access_token=` deliberately: query strings
        // end up in access logs and proxy traces, and this is a credential that
        // can drive every machine its owner has.
        if let Some(token) = parts
            .headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.split(',')
                    .map(str::trim)
                    .skip_while(|p| *p != WS_BEARER_PROTOCOL)
                    .nth(1)
            })
        {
            return if token.starts_with(USER_TOKEN_PREFIX) {
                let (ctx, grant) = user_token_ctx(state, token).await?;
                // A socket is not on the scoped surface: `required_scope` names
                // no websocket path, so this refuses every scoped token by the
                // same default-deny rule the HTTP path uses rather than by a
                // second one written here.
                scopes::authorize(state, ctx.tenant_id, grant, parts).await?;
                Ok(ctx)
            } else {
                node_token_ctx(state, token).await
            };
        }

        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
        let sid: Uuid = jar
            .get(SESSION_COOKIE)
            .and_then(|c| c.value().parse().ok())
            .ok_or(ApiError::Unauthorized)?;

        // One round-trip resolves the session AND its active-membership grant,
        // keeping 401 (no/expired session) distinct from 403 (grant revoked).
        // The query lives in `nook-auth` so the control plane and chat validate
        // sessions identically (MAIN-48 AC-4); see that crate for why the
        // membership check is a SELECT column rather than a WHERE clause.
        //
        // A memberless session is a real state now (MAIN-98: a local invitee who
        // has signed in but not yet accepted). It stays a 403 here — distinct
        // from the 401 an unauthenticated caller gets — so every tenant-scoped
        // route refuses it; the ONE route that serves such a caller (accept) uses
        // `IdentityCtx`, which resolves without the membership check.
        let r = nook_auth::resolve_session(&state.db, sid)
            .await
            .map_err(auth_err)?;
        Ok(AuthCtx {
            session_id: AuthSessionId(r.session_id),
            user_id: UserId(r.user_id),
            tenant_id: TenantId(r.tenant_id),
            principal: Principal::User,
            cookie_session: true,
        })
    }
}

/// A signed-in caller who may NOT yet belong to any tenant (MAIN-98).
///
/// The deliberate exception to `AuthCtx`'s "a session without a membership is a
/// 403": the narrow set of routes a person can legitimately reach before they
/// belong to a tenant — today only accepting an invite. A local invitee
/// registers, verifies, and signs in with a real session but no `tenant_members`
/// row; this context authenticates them so `accept_core` can do the joining,
/// while every ordinary route keeps requiring `AuthCtx` (membership).
///
/// A node credential is refused: a machine does not accept invites.
pub struct IdentityCtx {
    pub session_id: AuthSessionId,
    pub user_id: UserId,
    pub tenant_id: TenantId,
    pub cookie_session: bool,
    /// Whether this caller is already a member of the session's tenant.
    pub is_member: bool,
}

impl FromRequestParts<AppState> for IdentityCtx {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A user bearer token is minted for a tenant its owner belongs to, so it
        // is always a member — resolve it and mark it so.
        if let Some(bearer) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            if bearer.starts_with(USER_TOKEN_PREFIX) {
                let r = nook_auth::resolve_bearer(&state.db, bearer)
                    .await
                    .map_err(auth_err)?;
                return Ok(IdentityCtx {
                    session_id: AuthSessionId(r.session_id),
                    user_id: UserId(r.user_id),
                    tenant_id: TenantId(r.tenant_id),
                    cookie_session: false,
                    is_member: true,
                });
            }
            // A node token is not a person; it cannot accept an invite.
            return Err(ApiError::Unauthorized);
        }

        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
        let sid: Uuid = jar
            .get(SESSION_COOKIE)
            .and_then(|c| c.value().parse().ok())
            .ok_or(ApiError::Unauthorized)?;
        let (r, is_member) = nook_auth::resolve_session_identity(&state.db, sid)
            .await
            .map_err(auth_err)?;
        Ok(IdentityCtx {
            session_id: AuthSessionId(r.session_id),
            user_id: UserId(r.user_id),
            tenant_id: TenantId(r.tenant_id),
            cookie_session: true,
            is_member,
        })
    }
}

/// Map the shared crate's rejection to this service's error type, preserving
/// the 401/403/500 split exactly.
fn auth_err(e: nook_auth::AuthError) -> ApiError {
    match e {
        nook_auth::AuthError::Unauthorized => ApiError::Unauthorized,
        nook_auth::AuthError::Forbidden => ApiError::Forbidden,
        nook_auth::AuthError::Db(e) => ApiError::Db(e),
    }
}

/// User tokens are self-describing, so the server never has to guess which
/// table to look in — and a leaked one is recognizable in a log or a paste.
pub const USER_TOKEN_PREFIX: &str = "nook_user_";

/// Names the WebSocket subprotocol that carries a bearer token. The client
/// sends `[WS_BEARER_PROTOCOL, <token>]`; the server must echo the first back
/// or the browser aborts the connection.
pub const WS_BEARER_PROTOCOL: &str = "nook.bearer";

/// Resolve a user token to the person who owns it.
///
/// The result is `Principal::User`: this credential stands in for a human, so
/// it may drive any machine in their tenant. That is the whole reason it
/// exists — a node token deliberately cannot, and scripts still need to.
/// Revocation is a row delete, and expiry (if set) is enforced here.
/// The header a caller uses to act in one of their OTHER tenants.
pub const TENANT_HEADER: &str = "x-nook-tenant";
/// The session a node token is acting FOR. Not a credential — the node token is
/// the credential; this only says which of the machine's own jobs the request
/// belongs to, and the control plane checks that claim against its own records.
pub const SESSION_HEADER: &str = "x-nook-session";

/// Re-scope a caller to the tenant they asked for, if they are a member of it.
///
/// A tenant is this system's namespace, and the token's own `tenant_id` is the
/// HOME one — the default, unchanged when nothing asks otherwise, so every
/// existing caller behaves exactly as before.
///
/// Membership is re-read per request rather than baked into the credential.
/// That is the whole point: a token minted before you joined an org can reach
/// it, and one minted before you LEFT cannot — no re-issue, no cached list to
/// go stale, which is the failure a stored tenant list would have.
///
/// NODES ARE NEVER RE-SCOPED. A node token is confined to its own machine on
/// purpose; letting a header move it would hand every compromised box a way
/// into every tenant its owner belongs to. The header is ignored for them
/// rather than refused, because a node has no business sending it and a hard
/// error would only turn a harmless stray header into an outage.
fn want_header(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// `retenant` for integration tests. The scoping decision is the whole point of
/// this module and it is unreachable otherwise: every other route into it needs
/// a real credential, which is not what these tests are about.
#[doc(hidden)]
pub async fn retenant_for_test(
    state: &AppState,
    ctx: AuthCtx,
    parts: &mut Parts,
) -> Result<AuthCtx, ApiError> {
    retenant(state, ctx, parts).await
}

async fn retenant(state: &AppState, ctx: AuthCtx, parts: &Parts) -> Result<AuthCtx, ApiError> {
    // The node branch runs FIRST and without needing the tenant header: a
    // session's scope comes from the session, so `NOOK_TENANT_ID` is not what
    // carries it and a request with no header must still be scoped.
    if let Principal::Node(node) = ctx.principal {
        // Cross-tenant placement (MAIN-353) runs one org's checkout on another
        // org's machine, and until now the session there could not ACT in the
        // tenant it belonged to: its only credential says "I am void", and void
        // belongs to somebody else. Placement crossed tenants; identity did not.
        //
        // The scope comes from the SESSION, not from the header. The node token
        // proves which machine; the session id proves which work that machine
        // was given; the control plane checks the session is live and running
        // HERE. Together they authorise exactly the job already assigned — a
        // node reaches a tenant only while it is running that tenant's session,
        // and cannot name one it was never given.
        //
        // Deliberately NOT "a node may name any tenant it hosts a checkout for".
        // That would leave a shared box able to reach every tenant that ever
        // placed work on it, long after the work ended.
        let claimed = parts
            .headers
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .and_then(|v| uuid::Uuid::parse_str(v).ok());
        let Some(sid) = claimed else {
            return Ok(ctx); // no session claimed — the machine's own tenant
        };
        let Some(session) = state
            .sessions
            .by_id_unscoped(nook_types::SessionId(sid))
            .await?
        else {
            return Ok(ctx); // a session we do not know is not authority for anything
        };
        if session.node_id != node {
            return Err(ApiError::ForbiddenMsg(
                "that session is not running on this machine".into(),
            ));
        }
        // A finished session is not a licence. Its row keeps the tenant, and
        // honouring it would let a machine act for work that ended.
        if !crate::session_status::is_live(&session.status) {
            return Ok(ctx);
        }
        // The header, when present, has to agree with the session — a request
        // naming a tenant the session is not in was going to be answered about
        // the wrong one.
        if let Some(w) = want_header(parts) {
            let agrees = session.tenant_id.0.to_string().eq_ignore_ascii_case(&w);
            if !agrees && uuid::Uuid::parse_str(&w).is_ok() {
                return Err(ApiError::ForbiddenMsg(format!(
                    "this session belongs to another tenant than '{w}' — a node token \
                     acts only for the sessions it is running"
                )));
            }
        }
        let Some(owner) = state
            .identity
            .tenant_owner_user_id(session.tenant_id.0)
            .await?
        else {
            // No user resolvable in the tenant we would be writing to. Swapping
            // the tenant while keeping THIS one's user is the identity bug
            // itself, so decline the swap entirely: the caller stays in the
            // machine's own tenant, which is wrong but consistent, and `whoami`
            // now reports the mismatch rather than implying it worked.
            tracing::warn!(
                tenant = %session.tenant_id,
                "no owner for the session's tenant — not re-scoping"
            );
            return Ok(ctx);
        };
        return Ok(AuthCtx {
            tenant_id: session.tenant_id,
            user_id: UserId(owner),
            ..ctx
        });
    }

    let Some(want) = want_header(parts) else {
        return Ok(ctx);
    };
    let want = want.as_str();
    let memberships = state.identity.memberships_of(ctx.user_id).await?;
    // Slug or id, because a person types the slug and a script holds the id —
    // `NOOK_TENANT_ID` in a session is the latter.
    let found = memberships.iter().find(|m| {
        m.slug.eq_ignore_ascii_case(want) || m.tenant_id.0.to_string().eq_ignore_ascii_case(want)
    });
    let Some(m) = found else {
        // Deliberately NOT 404: whether a tenant exists is not something a
        // non-member gets to learn from a probe.
        return Err(ApiError::ForbiddenMsg(format!(
            "not a member of tenant '{want}'"
        )));
    };

    // THE USER ID MOVES TOO, and forgetting that is a real bug rather than a
    // tidiness point: a person has one `users` row PER TENANT, so the id that
    // identifies them in `hein` identifies nobody in `engineering-team`.
    //
    // Swapping only `tenant_id` left every check that reads `user_id` asking
    // about the wrong row. `GET /sessions` was the one that showed it —
    // `is_tenant_admin` said no, so the list fell back to "sessions you
    // created", matched the home-tenant id against none of them, and returned
    // an empty list for a tenant with sessions plainly running. Empty, not
    // an error, which is the worst shape for a scoping bug to take.
    //
    // `user_in_tenant` is both halves at once: it resolves the sibling row
    // through `person_id` AND returns None unless the grant is live, so a
    // membership revoked a moment ago cannot be re-scoped into.
    let Some(user_id) = state
        .identity
        .user_in_tenant(ctx.user_id, m.tenant_id)
        .await?
    else {
        return Err(ApiError::ForbiddenMsg(format!(
            "not a member of tenant '{want}'"
        )));
    };
    Ok(AuthCtx {
        tenant_id: m.tenant_id,
        user_id,
        ..ctx
    })
}

async fn user_token_ctx(
    state: &AppState,
    token: &str,
) -> Result<(AuthCtx, scopes::TokenGrant), ApiError> {
    // Shared with chat via `nook-auth`: the token hash + lookup + last-used
    // touch are one implementation, so the two services cannot drift. The
    // *scoped* variant, because this service enforces the narrowing — see
    // `resolve_bearer` for why the plain one refuses a scoped token outright.
    let g = nook_auth::resolve_bearer_grant(&state.db, token)
        .await
        .map_err(auth_err)?;
    let grant = scopes::TokenGrant::from_stored(g.scopes.as_deref(), g.workspace_id);
    let r = g.resolved;
    Ok((
        AuthCtx {
            // No browser session behind this; the token id (in `session_id`) gives
            // anything keyed by session something stable and unique to hold.
            session_id: AuthSessionId(r.session_id),
            user_id: UserId(r.user_id),
            tenant_id: TenantId(r.tenant_id),
            principal: Principal::User,
            // A bearer token, not a sessions_auth row: a switch cannot move it.
            cookie_session: false,
        },
        grant,
    ))
}

/// Resolve a node token to its tenant.
///
/// It borrows the owner's user id so tenant-scoped queries and event
/// attribution keep working, but it is marked as a node — see
/// `AuthCtx::require_user`, which is what stops that borrowed identity from
/// becoming a way to take the account over.
async fn node_token_ctx(state: &AppState, token: &str) -> Result<AuthCtx, ApiError> {
    let hash = crate::seed::hash_token(token);
    let node = state.identity.node_by_token_hash(&hash).await?;
    let (node_id, tenant_id) = node.ok_or(ApiError::Unauthorized)?;
    let owner = state.identity.tenant_owner_user_id(tenant_id).await?;
    Ok(AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(owner.unwrap_or_else(Uuid::nil)),
        tenant_id: TenantId(tenant_id),
        principal: Principal::Node(nook_types::NodeId(node_id)),
        cookie_session: false,
    })
}

pub fn session_cookie(state: &AppState, session_id: AuthSessionId) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, session_id.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.cfg.public_base_url.starts_with("https"))
        .max_age(cookie::time::Duration::hours(state.cfg.session_ttl_hours))
        .build()
}

pub fn removal_cookie(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/")
        .max_age(cookie::time::Duration::ZERO)
        .build()
}

/// Create a server-side auth session and return its id (the cookie value).
pub async fn create_auth_session(
    state: &AppState,
    user_id: UserId,
    tenant_id: TenantId,
) -> Result<AuthSessionId, ApiError> {
    let id = AuthSessionId::new();
    state
        .identity
        .create_auth_session(id, user_id, tenant_id, state.cfg.session_ttl_hours as i32)
        .await?;
    Ok(id)
}

/// Key used by `PrivateCookieJar` (encrypted flow cookie).
pub fn cookie_key(secret: &str) -> Key {
    Key::derive_from(secret.as_bytes())
}
