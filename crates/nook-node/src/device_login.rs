//! Signing in without pasting a token.
//!
//! The device authorization grant (RFC 8628), run against the **identity
//! provider** rather than against NookOS. The provider owns identity, has the
//! approval screen people already recognise, and implements the grant; a
//! control plane reimplementing it would be inserting itself into an exchange
//! it has no part in.
//!
//! The shape:
//!
//!   1. ask the control plane where its identity provider is
//!   2. ask the provider to start a device authorization
//!   3. show the code, open the browser
//!   4. poll the provider until somebody approves
//!   5. hand the resulting ID token to the control plane for one of its own
//!
//! Step 5 is the only part NookOS decides: whether it trusts the assertion.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::style;

#[derive(Debug, Deserialize)]
struct Providers {
    #[serde(default)]
    oidc: bool,
    #[serde(default)]
    oidc_issuer: Option<String>,
    #[serde(default)]
    device_authorization_endpoint: Option<String>,
    #[serde(default)]
    device_client_id: Option<String>,
}

/// The provider's own discovery document. Only the token endpoint is needed.
#[derive(Debug, Deserialize)]
struct ProviderMetadata {
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    /// Some providers send this; it embeds the code so the browser needs no
    /// typing at all.
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default)]
    expires_in: Option<u64>,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Exchanged {
    token: String,
}

/// Run the whole flow and return a NookOS user token.
pub async fn login(server: &str) -> Result<String> {
    let server = server.trim_end_matches('/');
    let http = reqwest::Client::new();

    // ---- 1. where is the identity provider?
    //
    // Asked, not configured. A client that carried its own copy of the issuer
    // would need reconfiguring every time an operator changed theirs.
    let providers: Providers = http
        .get(format!("{server}/api/v1/auth/providers"))
        .send()
        .await
        .with_context(|| format!("cannot reach {server}"))?
        .json()
        .await
        .context("unexpected reply from /auth/providers")?;

    if !providers.oidc {
        bail!(
            "this control plane has no identity provider — sign in with a username \
             and password instead, or paste a token from the web UI"
        );
    }
    let endpoint = providers.device_authorization_endpoint.context(
        "the identity provider does not advertise a device_authorization_endpoint, \
         so there is nowhere to begin. Set OIDC_DEVICE_AUTHORIZATION_ENDPOINT on \
         the control plane, or add it to the provider's discovery document.",
    )?;
    let client_id = providers.device_client_id.context(
        "no public client is configured for native sign-in. Register one at your \
         identity provider and set OIDC_DEVICE_CLIENT_ID on the control plane.",
    )?;

    // ---- 2. start the authorization
    let start: DeviceStart = http
        .post(&endpoint)
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", "openid profile email"),
        ])
        .send()
        .await
        .with_context(|| format!("cannot reach {endpoint}"))?
        .json()
        .await
        .context(
            "the identity provider's device authorization reply was not what RFC 8628 describes",
        )?;

    // ---- 3. tell the person what to do
    let link = start
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| start.verification_uri.clone());

    println!();
    println!("  {}", style::bold("Approve this device"));
    println!();
    println!("    {}", style::accent(&link));
    println!("    code: {}", style::bold(&start.user_code));
    if let Some(secs) = start.expires_in {
        println!(
            "    {}",
            style::dim(&format!("expires in {} minutes", secs / 60))
        );
    }
    println!();
    open_browser(&link);

    // ---- 4. wait for approval
    //
    // The token endpoint is READ from the provider's discovery document, not
    // built from the issuer. `{issuer}/token` is right for this provider and a
    // guess for any other, and a guess that happens to work is the kind that
    // fails on somebody else's deployment.
    let issuer = providers
        .oidc_issuer
        .as_deref()
        .context("the control plane did not say which identity provider it uses")?
        .trim_end_matches('/');
    let meta: ProviderMetadata = http
        .get(format!("{issuer}/.well-known/openid-configuration"))
        .send()
        .await
        .with_context(|| format!("cannot reach {issuer}"))?
        .json()
        .await
        .context("the identity provider's discovery document was unreadable")?;

    let id_token = poll(&http, &meta.token_endpoint, &client_id, &start).await?;

    // ---- 5. trade it for a credential of this control plane's own
    let exchanged: Exchanged = http
        .post(format!("{server}/api/v1/auth/oidc/exchange"))
        .json(&serde_json::json!({
            "id_token": id_token,
            "client_name": client_label(),
        }))
        .send()
        .await
        .context("cannot reach the control plane to complete sign-in")?
        .error_for_status()
        .context("the control plane refused that identity token")?
        .json()
        .await
        .context("unexpected reply from /auth/oidc/exchange")?;

    Ok(exchanged.token)
}

/// Poll the provider until somebody approves, or time runs out.
async fn poll(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    start: &DeviceStart,
) -> Result<String> {
    let mut interval = std::time::Duration::from_secs(start.interval.max(1));
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(start.expires_in.unwrap_or(600));

    print!("  waiting");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    while std::time::Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        print!(".");
        let _ = std::io::stdout().flush();

        let resp: TokenResponse = http
            .post(token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", start.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await
            .context("cannot reach the identity provider's token endpoint")?
            .json()
            .await
            .context("the token endpoint replied with something unexpected")?;

        if let Some(t) = resp.id_token {
            println!();
            return Ok(t);
        }
        match resp.error.as_deref() {
            // The two states that mean "keep going". `slow_down` is an
            // instruction, not a complaint: ignoring it gets a client
            // rate-limited, which looks like the provider being broken.
            Some("authorization_pending") => {}
            Some("slow_down") => interval += std::time::Duration::from_secs(5),
            Some("access_denied") => {
                println!();
                bail!("that request was declined at the identity provider")
            }
            Some("expired_token") => {
                println!();
                bail!("the code expired before it was approved — run this again")
            }
            Some(other) => {
                println!();
                bail!("the identity provider refused: {other}")
            }
            None => {
                println!();
                bail!("the token endpoint returned neither a token nor an error")
            }
        }
    }
    println!();
    bail!("timed out waiting for approval")
}

/// Best effort — the link is printed either way, so a headless box is fine.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Names the token in the list an operator revokes from.
fn client_label() -> String {
    let host = sysinfo::System::host_name().unwrap_or_else(|| "unknown".into());
    format!("nook cli on {host}")
}
