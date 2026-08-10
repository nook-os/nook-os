//! The rules of the tunnel HTTP surface, as pure functions (MAIN-403).
//!
//! Everything here decides; nothing here does I/O. That split is deliberate:
//! this is the security-critical half of MAIN-9 — who may reach a tunnel, and
//! what of NookOS's own credentials must never travel with the request — and a
//! rule that needs a database, a node and wildcard DNS to exercise is a rule
//! nobody tests. The wiring that calls these lives in `routes::tunnels`.
//!
//! Two credentials reach a tunnel and they are minted the same way: a **grant**,
//! issued at the apex once a session has been verified and carried across the
//! subdomain boundary in a URL, and the host-only **cookie** the subdomain
//! exchanges it for. Both are signed with the instance's session secret and
//! carry their own expiry, so neither needs a table — which is what lets a
//! tunnel, whose whole existence is in memory, authenticate without one.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

/// The host-only cookie a tunnel host authenticates with.
pub const TUNNEL_COOKIE: &str = "nook_tunnel";

/// Reserved on every tunnel host: nothing under it is ever forwarded upstream
/// (MAIN-403 AC-5). Without the reservation an app behind a tunnel could serve
/// its own `/__nook/tunnel/grant` and read the grants meant for the surface.
pub const RESERVED_PREFIX: &str = "/__nook/";

/// Where a grant is exchanged for the tunnel cookie, on the tunnel host.
pub const GRANT_PATH: &str = "/__nook/tunnel/grant";

/// Where the apex verifies the session and issues a grant. Under `/api` because
/// that is the prefix a deployment's edge proxy already routes to the control
/// plane — the SPA's own origin serves everything else.
pub const AUTHORIZE_PATH: &str = "/api/v1/tunnels/authorize";

/// Every cookie and bearer token NookOS itself issues starts with this, which
/// is what makes [`forwarded_headers`] able to strip them all by rule rather
/// than by a list that goes stale the next time one is added.
const CREDENTIAL_PREFIX: &str = "nook_";

/// How long a grant is good for. Seconds, because it is redeemed by the very
/// next request the browser makes; anything longer is only a wider window for
/// the copy left in a proxy log to be replayed.
pub const GRANT_TTL_SECS: i64 = 60;

/// How long the tunnel cookie lasts before the browser is bounced through the
/// apex again.
///
/// An hour rather than the session's own lifetime, because this credential is
/// self-contained: nothing revokes it, so its expiry IS the revocation window.
/// The re-authorization is a redirect the person never sees, and it re-checks
/// the session and the membership against the database.
pub const COOKIE_TTL_SECS: i64 = 3600;

/// Signing purposes. Distinct strings so a cookie cannot be replayed as a
/// grant: same key, same claims shape, and only this separating them.
const PURPOSE_GRANT: &str = "nook.tunnel.grant.v1";
const PURPOSE_COOKIE: &str = "nook.tunnel.cookie.v1";

/// Which tunnel a request is for, extracted from its `Host`.
///
/// `None` means "not a tunnel host" and the request goes on to the API. A host
/// that IS under the zone always yields a label — including a multi-label one
/// like `a.b.<zone>`, which resolves to no tunnel and gets the 404 page. That
/// is the point of AC-1: nothing under the wildcard falls through to the
/// control plane's own surface, not even something that cannot be a tunnel.
pub fn tunnel_label(host: &str, domain: &str) -> Option<String> {
    if domain.is_empty() {
        return None;
    }
    let host = host.trim();
    // A bracketed IPv6 literal is somebody dialling this server by address, not
    // a name under the zone; splitting it on ':' would also mangle it.
    if host.starts_with('[') {
        return None;
    }
    let host = host
        .split(':')
        .next()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let label = host.strip_suffix(domain)?.strip_suffix('.')?;
    (!label.is_empty()).then(|| label.to_string())
}

/// What a signed tunnel credential says. Small on purpose: everything in here
/// is checked, and anything that is not checked has no business being signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The tunnel this credential opens. Bound because both credentials are
    /// host-only but the SIGNATURE is not: without the label, a cookie minted
    /// for one tunnel would verify at another.
    pub label: String,
    pub user: Uuid,
    pub tenant: Uuid,
    /// Unix seconds.
    pub exp: i64,
    /// Grants only: the id the redemption ledger spends, making it single-use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<Uuid>,
}

fn tag(secret: &str, purpose: &str, payload: &str) -> Vec<u8> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    // The purpose is INSIDE the signed material, with a separator, so no two
    // purposes can be made to sign the same bytes.
    mac.update(purpose.as_bytes());
    mac.update(b".");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn mint(secret: &str, purpose: &str, claims: &Claims) -> String {
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims are plain data"));
    let sig = URL_SAFE_NO_PAD.encode(tag(secret, purpose, &payload));
    format!("{payload}.{sig}")
}

fn open(secret: &str, purpose: &str, token: &str, now: i64) -> Option<Claims> {
    let (payload, sig) = token.split_once('.')?;
    let sig = URL_SAFE_NO_PAD.decode(sig).ok()?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(purpose.as_bytes());
    mac.update(b".");
    mac.update(payload.as_bytes());
    // `verify_slice` rather than comparing the tags ourselves: it is
    // constant-time, and a byte-by-byte compare here is a timing oracle on a
    // signature an attacker gets to retry.
    mac.verify_slice(&sig).ok()?;
    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (claims.exp > now).then_some(claims)
}

/// Mint the single-use grant the apex hands back to a tunnel host.
pub fn mint_grant(secret: &str, label: &str, user: Uuid, tenant: Uuid, now: i64) -> String {
    mint(
        secret,
        PURPOSE_GRANT,
        &Claims {
            label: label.to_string(),
            user,
            tenant,
            exp: now + GRANT_TTL_SECS,
            jti: Some(Uuid::now_v7()),
        },
    )
}

/// Verify a grant. `None` covers every way it can be unusable — tampered,
/// expired, minted as a cookie, or for another tunnel — because the tunnel host
/// answers all of them identically and telling them apart only helps a prober.
pub fn open_grant(secret: &str, token: &str, label: &str, now: i64) -> Option<Claims> {
    open(secret, PURPOSE_GRANT, token, now).filter(|c| c.label == label && c.jti.is_some())
}

/// Mint the host-only cookie value a redeemed grant becomes.
pub fn mint_cookie(secret: &str, label: &str, user: Uuid, tenant: Uuid, now: i64) -> String {
    mint(
        secret,
        PURPOSE_COOKIE,
        &Claims {
            label: label.to_string(),
            user,
            tenant,
            exp: now + COOKIE_TTL_SECS,
            jti: None,
        },
    )
}

pub fn open_cookie(secret: &str, token: &str, now: i64) -> Option<Claims> {
    open(secret, PURPOSE_COOKIE, token, now)
}

/// What a request presented, after the parts that need the database have been
/// resolved. The decision below is pure in terms of this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Nothing usable: no cookie, no bearer token.
    None,
    /// A tunnel cookie whose signature and expiry verified.
    Cookie { label: String, tenant: Uuid },
    /// A user token that resolved to a person, and whether that person is a
    /// live member of the tunnel's tenant.
    Bearer { member: bool },
    /// A bearer token that resolved to nobody — revoked, expired, or a node
    /// token, which is a machine credential and not a person at a tunnel.
    BadBearer,
}

/// How a request to a tunnel host is answered before anything is forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allow,
    /// Send the browser to the apex to prove who it is. This is the
    /// no-credential case: a redirect rather than a 401 body, because the
    /// person is signed in to NookOS already and the bounce is invisible.
    Authorize,
    /// A credential that did not resolve. Answered rather than redirected —
    /// bouncing a `curl` holding a dead token into a sign-in page helps nobody.
    Unauthorized,
    /// Resolved to a person who is not a member of the tunnel's tenant.
    Forbidden,
}

/// Membership of the tunnel's tenant is the whole policy (MAIN-9 NG-6): there
/// is no per-path or per-person rule inside a tunnel, and adding one is its own
/// decision (MAIN-403 NG-3).
pub fn decide(cred: &Credential, label: &str, tenant: Uuid) -> Access {
    match cred {
        Credential::Bearer { member: true } => Access::Allow,
        Credential::Bearer { member: false } => Access::Forbidden,
        Credential::BadBearer => Access::Unauthorized,
        Credential::Cookie {
            label: l,
            tenant: t,
        } if l == label && *t == tenant => Access::Allow,
        // A cookie for a different tunnel, or one minted before this label was
        // reused by another tenant, is not a refusal: the person may well be
        // entitled here, so send them round the grant flow to find out.
        Credential::Cookie { .. } | Credential::None => Access::Authorize,
    }
}

/// Headers that describe the HOP rather than the message, in either direction.
/// A proxy that copies these across is describing a connection that does not
/// exist — `transfer-encoding: chunked` on a body this server re-frames is the
/// one that actually breaks.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The headers a tunnel request carries to the node — with every NookOS
/// credential removed (MAIN-403 AC-4).
///
/// A tunnel exposes somebody's local port. Whatever is listening there is
/// ordinary development code, and handing it the cookie that opens the control
/// plane would make every tunnelled app a way to become its author. So the
/// session cookie and the tunnel cookie are stripped, and so is a bearer token
/// that is ours — recognised by the `nook_` prefix every one of them carries,
/// rather than by naming the two that exist today.
///
/// An app's OWN cookies and its own `Authorization` header survive: the point
/// is to remove what authenticates to NookOS, not to break what authenticates
/// to the app.
pub fn forwarded_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let name = name.as_str();
        // `content-length` goes with the hop-by-hop set on the way out: the
        // body is re-framed into the tunnel frame and the node's client
        // recomputes it, so a copied one could only ever disagree.
        if HOP_BY_HOP.contains(&name) || name == "content-length" {
            continue;
        }
        // A header value that is not text cannot cross the JSON frame; dropping
        // it is the only option that is not a lie about what was sent.
        let Ok(value) = value.to_str() else { continue };
        match name {
            "cookie" => {
                if let Some(kept) = strip_nook_cookies(value) {
                    out.push((name.to_string(), kept));
                }
            }
            "authorization" => {
                if !is_nook_credential(value) {
                    out.push((name.to_string(), value.to_string()));
                }
            }
            _ => out.push((name.to_string(), value.to_string())),
        }
    }
    out
}

/// Everything in a `Cookie` header except NookOS's own, or `None` when that
/// leaves nothing — an empty `Cookie:` header is not the same as no header.
fn strip_nook_cookies(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("").trim();
            !name.starts_with(CREDENTIAL_PREFIX)
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

fn is_nook_credential(authorization: &str) -> bool {
    authorization
        .strip_prefix("Bearer ")
        .is_some_and(|t| t.trim_start().starts_with(CREDENTIAL_PREFIX))
}

/// The headers of the node's answer, as this server will send them on.
///
/// `content-length` SURVIVES here, unlike on the way out: the node streams the
/// upstream's bytes verbatim (its client decompresses nothing), so a length the
/// app stated is still true — and dropping it would cost every download its
/// progress bar. Hop-by-hop headers do not survive, and `set-cookie` is
/// narrowed to the tunnel host by [`host_only_cookie`].
pub fn upstream_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .filter(|(name, _)| !HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str()))
        .map(|(name, value)| {
            if name.eq_ignore_ascii_case("set-cookie") {
                (name, host_only_cookie(&value))
            } else {
                (name, value)
            }
        })
        .collect()
}

/// Force an upstream `Set-Cookie` to be host-only by dropping its `Domain`.
///
/// A tunnel host is a subdomain of the apex, and a browser lets a subdomain set
/// a cookie for its parent domain. Left alone, an app behind a tunnel could
/// therefore write `nook_session` onto the apex — session fixation, from code
/// whose only intended power is to serve a page. The app's own cookies still
/// work: host-only is what they would have been on their own origin anyway.
pub fn host_only_cookie(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .map(str::trim)
        .filter(|attr| !attr.to_ascii_lowercase().starts_with("domain="))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One of the surface's own pages — a 404 for a tunnel that is not there, a 502
/// for a node that is (MAIN-403 AC-6).
///
/// Deliberately a rendered page and not `ApiError`'s JSON: whoever is looking at
/// this is a person in a browser who typed a URL an agent handed them, and
/// `{"error":"not found"}` tells them nothing about which of the two things went
/// wrong. Self-contained, because the SPA is not served from this host.
pub fn page(status: StatusCode, heading: &str, detail: &str) -> Response {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{heading}</title><style>\
         html{{background:#0b0b0c;color:#ffb000;font:13px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace}}\
         body{{margin:0;display:flex;min-height:100vh;align-items:center;justify-content:center}}\
         main{{max-width:44rem;padding:2rem;border:1px solid #4a3400}}\
         h1{{font-size:15px;margin:0 0 .75rem;letter-spacing:.08em;text-transform:uppercase}}\
         p{{margin:0;color:#c98f14}}\
         </style></head><body><main><h1>{heading}</h1><p>{detail}</p></main></body></html>",
        heading = escape(heading),
        detail = escape(detail),
    );
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Enough escaping for text this server composes from a node's name, a port and
/// an upstream error string. None of it is trusted markup, and the page has no
/// attribute interpolation, so the five characters are the whole job.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_800_000_000;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_subdomain_of_the_zone_is_a_tunnel_and_the_apex_is_not() {
        assert_eq!(
            tunnel_label("api-azul-3000.nook.example", "nook.example").as_deref(),
            Some("api-azul-3000")
        );
        // The apex serves the app, not a tunnel.
        assert_eq!(tunnel_label("nook.example", "nook.example"), None);
        // A name that merely ENDS with the zone's text is a different host.
        assert_eq!(tunnel_label("evilnook.example", "nook.example"), None);
        // Somewhere else entirely, and the unconfigured case.
        assert_eq!(tunnel_label("other.test", "nook.example"), None);
        assert_eq!(tunnel_label("api.nook.example", ""), None);
    }

    #[test]
    fn host_matching_ignores_case_port_and_the_root_dot() {
        assert_eq!(
            tunnel_label("API.Nook.Example:8080", "nook.example").as_deref(),
            Some("api")
        );
        assert_eq!(
            tunnel_label("api.nook.example.", "nook.example").as_deref(),
            Some("api")
        );
        // An address, not a name under the zone.
        assert_eq!(tunnel_label("[::1]:8080", "nook.example"), None);
    }

    #[test]
    fn anything_under_the_zone_is_claimed_even_when_it_cannot_be_a_tunnel() {
        // AC-1: a host under the wildcard must never fall through to the API,
        // so this yields a label (which resolves to nothing → the 404 page)
        // rather than `None` (which would mean "let the control plane have it").
        assert_eq!(
            tunnel_label("a.b.nook.example", "nook.example").as_deref(),
            Some("a.b")
        );
    }

    #[test]
    fn a_grant_verifies_once_it_is_minted_and_not_after_it_expires() {
        let (user, tenant) = (Uuid::now_v7(), Uuid::now_v7());
        let g = mint_grant(SECRET, "api", user, tenant, NOW);
        let claims = open_grant(SECRET, &g, "api", NOW).expect("a fresh grant verifies");
        assert_eq!(claims.user, user);
        assert_eq!(claims.tenant, tenant);
        assert!(
            claims.jti.is_some(),
            "a grant carries the id it is spent by"
        );
        assert!(
            open_grant(SECRET, &g, "api", NOW + GRANT_TTL_SECS).is_none(),
            "expiry is checked"
        );
    }

    #[test]
    fn a_credential_for_one_tunnel_does_not_open_another() {
        let (user, tenant) = (Uuid::now_v7(), Uuid::now_v7());
        let g = mint_grant(SECRET, "api", user, tenant, NOW);
        assert!(
            open_grant(SECRET, &g, "admin", NOW).is_none(),
            "the label is bound into the signature, and checked"
        );
    }

    #[test]
    fn a_cookie_cannot_be_replayed_as_a_grant() {
        // Same key, same claims shape: only the purpose separates them, which is
        // exactly why the purpose is inside the signed material.
        let (user, tenant) = (Uuid::now_v7(), Uuid::now_v7());
        let c = mint_cookie(SECRET, "api", user, tenant, NOW);
        assert!(open_grant(SECRET, &c, "api", NOW).is_none());
        let g = mint_grant(SECRET, "api", user, tenant, NOW);
        assert!(open_cookie(SECRET, &g, NOW).is_none());
    }

    #[test]
    fn a_tampered_or_foreign_credential_is_refused() {
        let (user, tenant) = (Uuid::now_v7(), Uuid::now_v7());
        let c = mint_cookie(SECRET, "api", user, tenant, NOW);
        // Another instance's secret.
        assert!(open_cookie("another-secret-entirely-0000000", &c, NOW).is_none());
        // The payload edited to name a different tenant, signature left alone —
        // which is the forgery a plain base64 blob would have allowed.
        let forged = Claims {
            label: "api".into(),
            user,
            tenant: Uuid::now_v7(),
            exp: NOW + COOKIE_TTL_SECS,
            jti: None,
        };
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).expect("claims are plain data"));
        let sig = c.split_once('.').unwrap().1;
        assert!(open_cookie(SECRET, &format!("{payload}.{sig}"), NOW).is_none());
        // Not a token at all.
        assert!(open_cookie(SECRET, "nonsense", NOW).is_none());
    }

    #[test]
    fn the_decision_is_redirect_forbidden_or_allow() {
        let tenant = Uuid::now_v7();
        // AC-7's three cases, in one place.
        assert_eq!(
            decide(&Credential::None, "api", tenant),
            Access::Authorize,
            "no credential bounces to the apex rather than failing"
        );
        assert_eq!(
            decide(&Credential::Bearer { member: false }, "api", tenant),
            Access::Forbidden,
            "signed in, not a member of the tunnel's tenant"
        );
        assert_eq!(
            decide(&Credential::Bearer { member: true }, "api", tenant),
            Access::Allow,
            "a bearer token is accepted with no browser flow at all"
        );
        assert_eq!(
            decide(&Credential::BadBearer, "api", tenant),
            Access::Unauthorized
        );
        assert_eq!(
            decide(
                &Credential::Cookie {
                    label: "api".into(),
                    tenant
                },
                "api",
                tenant
            ),
            Access::Allow
        );
    }

    #[test]
    fn a_cookie_from_another_tunnel_or_tenant_earns_no_access() {
        let tenant = Uuid::now_v7();
        assert_eq!(
            decide(
                &Credential::Cookie {
                    label: "other".into(),
                    tenant
                },
                "api",
                tenant
            ),
            Access::Authorize
        );
        assert_eq!(
            decide(
                &Credential::Cookie {
                    label: "api".into(),
                    tenant: Uuid::now_v7()
                },
                "api",
                tenant
            ),
            Access::Authorize
        );
    }

    #[test]
    fn neither_nook_cookie_survives_to_upstream() {
        // AC-4, asserted on what is FORWARDED rather than on the answer.
        let h = headers(&[(
            "cookie",
            "theme=dark; nook_session=abc; app_sid=42; nook_tunnel=xyz",
        )]);
        let out = forwarded_headers(&h);
        let cookie = out
            .iter()
            .find(|(k, _)| k == "cookie")
            .map(|(_, v)| v.as_str())
            .expect("the app's own cookies still travel");
        assert_eq!(cookie, "theme=dark; app_sid=42");
        assert!(!cookie.contains("nook_"));
    }

    #[test]
    fn a_cookie_header_of_nothing_but_credentials_is_removed_entirely() {
        let h = headers(&[("cookie", "nook_session=abc; nook_tunnel=xyz")]);
        assert!(
            !forwarded_headers(&h).iter().any(|(k, _)| k == "cookie"),
            "an empty Cookie: header would be a new header, not a stripped one"
        );
    }

    #[test]
    fn a_nook_bearer_token_is_stripped_and_the_apps_own_is_not() {
        let ours = headers(&[("authorization", "Bearer nook_user_abc123")]);
        assert!(!forwarded_headers(&ours)
            .iter()
            .any(|(k, _)| k == "authorization"));
        // A node token has a different prefix and the same rule catches it.
        let node = headers(&[("authorization", "Bearer nook_node_abc123")]);
        assert!(!forwarded_headers(&node)
            .iter()
            .any(|(k, _)| k == "authorization"));
        // Whatever the app behind the tunnel authenticates with is not ours.
        let theirs = headers(&[("authorization", "Bearer eyJhbGciOi.something")]);
        assert!(forwarded_headers(&theirs)
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer eyJhbGciOi.something"));
    }

    #[test]
    fn hop_by_hop_headers_do_not_travel_but_the_rest_do() {
        let h = headers(&[
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("content-length", "12"),
            ("host", "api.nook.example"),
            ("accept", "text/html"),
        ]);
        let out = forwarded_headers(&h);
        let names: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!names.contains(&"connection"));
        assert!(!names.contains(&"transfer-encoding"));
        assert!(!names.contains(&"content-length"));
        // The tunnel host is what the app should think it is serving.
        assert!(out
            .iter()
            .any(|(k, v)| k == "host" && v == "api.nook.example"));
        assert!(names.contains(&"accept"));
    }

    #[test]
    fn an_upstream_cookie_cannot_be_widened_onto_the_apex() {
        assert_eq!(
            host_only_cookie("sid=1; Domain=nook.example; Path=/; HttpOnly"),
            "sid=1; Path=/; HttpOnly"
        );
        assert_eq!(
            host_only_cookie("sid=1; domain=.nook.example"),
            "sid=1",
            "the attribute is case-insensitive and a leading dot changes nothing"
        );
        // A cookie that was already host-only passes through untouched.
        assert_eq!(host_only_cookie("sid=1; Path=/"), "sid=1; Path=/");
    }

    #[test]
    fn an_upstream_response_keeps_its_length_and_loses_its_hop_headers() {
        let out = upstream_headers(vec![
            ("Content-Length".into(), "12".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Set-Cookie".into(), "sid=1; Domain=nook.example".into()),
            ("Content-Type".into(), "text/plain".into()),
        ]);
        let names: Vec<String> = out.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert!(names.contains(&"content-length".to_string()));
        assert!(!names.contains(&"transfer-encoding".to_string()));
        assert!(!names.contains(&"connection".to_string()));
        assert!(out.iter().any(|(k, v)| k == "Set-Cookie" && v == "sid=1"));
    }

    #[tokio::test]
    async fn a_page_is_html_carrying_its_status_and_escapes_what_it_is_given() {
        let res = page(StatusCode::BAD_GATEWAY, "Nothing is listening", "<script>");
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("&lt;script&gt;"), "{body}");
        assert!(!body.contains("<script>"));
    }
}
