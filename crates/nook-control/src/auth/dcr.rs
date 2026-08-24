//! Dynamic Client Registration (RFC 7591) — configure OIDC with an issuer alone
//! (MAIN-651).
//!
//! An operator used to have to visit the IdP, create a client by hand, copy an
//! id and a secret out of it and set three variables. On an IdP that implements
//! RFC 7591 none of that is necessary: the instance registers itself, once, and
//! remembers what it was issued.
//!
//! **The client we ask for is PUBLIC** — `token_endpoint_auth_method: none`,
//! with the authorization code bound by the PKCE challenge the login flow
//! already sends. That is OAuth 2.1's shape for this, and it is what lets the
//! registration be remembered in an ordinary table: there is no secret in it, so
//! there is nothing to encrypt and nothing to leak. An IdP that will not issue a
//! public client is refused rather than accommodated — see [`register`].
//!
//! Nothing here runs when `OIDC_CLIENT_ID` is set. An operator who names a
//! client has made a choice, and registering a second one behind their back
//! would leave two clients where they configured one.

use openidconnect::core::{CoreClientAuthMethod, CoreProviderMetadata};
use serde::Deserialize;

/// What the IdP issued. No secret, by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredClient {
    pub client_id: String,
}

/// The subset of RFC 7591 §3.2.1's response this cares about.
#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// Will this IdP issue a public client?
///
/// **Absence is not permission.** RFC 8414 §2 says an omitted
/// `token_endpoint_auth_methods_supported` defaults to `client_secret_basic`, so
/// a provider that says nothing is saying "confidential", not "anything".
/// Registering against it would produce a client we cannot authenticate as.
pub fn public_client_supported(methods: Option<&Vec<CoreClientAuthMethod>>) -> bool {
    methods.is_some_and(|m| m.contains(&CoreClientAuthMethod::None))
}

/// The RFC 7591 registration request, as a value so it can be asserted on
/// without a network.
pub fn registration_body(redirect_uri: &str, client_name: &str) -> serde_json::Value {
    serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "web",
    })
}

/// Why registration was not attempted, in the words an operator needs to act on.
/// Both arms name what to set, because the fallback is always the same: create
/// a client at the IdP and configure it explicitly.
pub fn unavailable_reason(metadata: &CoreProviderMetadata) -> Option<String> {
    if metadata.registration_endpoint().is_none() {
        return Some(
            "the IdP does not advertise a registration_endpoint — create a client there \
             and set OIDC_CLIENT_ID"
                .into(),
        );
    }
    if !public_client_supported(metadata.token_endpoint_auth_methods_supported()) {
        return Some(
            "the IdP does not offer public clients (token_endpoint_auth_methods_supported \
             has no \"none\") — create a client there and set OIDC_CLIENT_ID and \
             OIDC_CLIENT_SECRET"
                .into(),
        );
    }
    None
}

/// Register this instance at the IdP and return the id it issued.
///
/// A response carrying a `client_secret`, or naming any auth method other than
/// `none`, is REJECTED: the IdP registered something other than what was asked
/// for, and honouring it would mean holding a credential this design has nowhere
/// to put. Better to fail here, naming the variables to set, than to persist
/// half a confidential client and fail every login afterwards.
pub async fn register(
    http: &openidconnect::reqwest::Client,
    metadata: &CoreProviderMetadata,
    redirect_uri: &str,
    client_name: &str,
) -> anyhow::Result<RegisteredClient> {
    if let Some(reason) = unavailable_reason(metadata) {
        anyhow::bail!("cannot register a client automatically: {reason}");
    }
    let endpoint = metadata
        .registration_endpoint()
        .expect("checked by unavailable_reason");

    let res = http
        .post(endpoint.as_str())
        .json(&registration_body(redirect_uri, client_name))
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("registration endpoint returned {status}: {}", body.trim());
    }

    let parsed: RegistrationResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("registration response was not RFC 7591 JSON: {e}"))?;
    check_public(&parsed)?;
    Ok(RegisteredClient {
        client_id: parsed.client_id,
    })
}

fn check_public(res: &RegistrationResponse) -> anyhow::Result<()> {
    if res.client_secret.is_some() {
        anyhow::bail!(
            "the IdP issued a confidential client (it returned a client_secret) although a \
             public one was requested — set OIDC_CLIENT_ID and OIDC_CLIENT_SECRET explicitly"
        );
    }
    match res.token_endpoint_auth_method.as_deref() {
        None | Some("none") => Ok(()),
        Some(other) => anyhow::bail!(
            "the IdP registered the client as {other}, not the requested \"none\" — set \
             OIDC_CLIENT_ID and OIDC_CLIENT_SECRET explicitly"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openidconnect::core::CoreProviderMetadata;

    fn parsed(json: &str) -> RegistrationResponse {
        serde_json::from_str(json).expect("test fixture parses")
    }

    #[test]
    fn a_public_client_is_offered_only_when_none_is_listed() {
        assert!(public_client_supported(Some(&vec![
            CoreClientAuthMethod::ClientSecretPost,
            CoreClientAuthMethod::None,
        ])));
        assert!(!public_client_supported(Some(&vec![
            CoreClientAuthMethod::ClientSecretBasic
        ])));
        // RFC 8414 §2: an omitted field means client_secret_basic, never "any".
        assert!(!public_client_supported(None));
    }

    #[test]
    fn the_request_asks_for_a_public_pkce_client() {
        let body = registration_body("https://nook.example.test/cb", "NookOS");
        assert_eq!(body["token_endpoint_auth_method"], "none");
        assert_eq!(body["redirect_uris"][0], "https://nook.example.test/cb");
        assert_eq!(body["response_types"][0], "code");
    }

    #[test]
    fn a_confidential_registration_is_refused_however_it_is_signalled() {
        // A secret we have nowhere to keep...
        let err = check_public(&parsed(r#"{"client_id":"a","client_secret":"s"}"#)).unwrap_err();
        assert!(err.to_string().contains("confidential"), "{err}");
        // ...and an auth method that contradicts the request, even without one.
        let err = check_public(&parsed(
            r#"{"client_id":"a","token_endpoint_auth_method":"client_secret_post"}"#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("client_secret_post"), "{err}");
        // The shape actually asked for, and the shape that omits the echo.
        check_public(&parsed(
            r#"{"client_id":"a","token_endpoint_auth_method":"none"}"#,
        ))
        .unwrap();
        check_public(&parsed(r#"{"client_id":"a"}"#)).unwrap();
    }

    #[test]
    fn each_refusal_names_what_to_set_instead() {
        let bare = CoreProviderMetadata::new(
            openidconnect::IssuerUrl::new("https://idp.example.test".into()).unwrap(),
            openidconnect::AuthUrl::new("https://idp.example.test/auth".into()).unwrap(),
            openidconnect::JsonWebKeySetUrl::new("https://idp.example.test/jwks".into()).unwrap(),
            vec![],
            vec![],
            vec![],
            openidconnect::EmptyAdditionalProviderMetadata {},
        );
        // No registration endpoint at all.
        let reason = unavailable_reason(&bare).expect("refused");
        assert!(reason.contains("OIDC_CLIENT_ID"), "{reason}");
        assert!(reason.contains("registration_endpoint"), "{reason}");

        // Registration offered, but confidential clients only.
        let confidential = bare
            .clone()
            .set_registration_endpoint(Some(
                openidconnect::RegistrationUrl::new("https://idp.example.test/reg".into()).unwrap(),
            ))
            .set_token_endpoint_auth_methods_supported(Some(vec![
                CoreClientAuthMethod::ClientSecretBasic,
            ]));
        let reason = unavailable_reason(&confidential).expect("refused");
        assert!(reason.contains("OIDC_CLIENT_SECRET"), "{reason}");

        // Both present: nothing to report.
        let ok = confidential
            .set_token_endpoint_auth_methods_supported(Some(vec![CoreClientAuthMethod::None]));
        assert_eq!(unavailable_reason(&ok), None);
    }
}
