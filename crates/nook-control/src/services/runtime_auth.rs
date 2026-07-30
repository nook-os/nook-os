//! Obtaining a runtime credential, without a login session (MAIN-282, C1).
//!
//! Today a node is authorized by spawning `claude auth login` in a terminal
//! session and having a person drive it there. That couples authorization to a
//! live session on a specific machine, and it is why an out-of-band credential
//! change never reflects (MAIN-285) and why a fleet cannot be authorized once
//! and used everywhere.
//!
//! This is the **source** half of the replacement: the control plane runs the
//! device authorization grant (RFC 8628) itself and ends up holding a
//! credential in memory. Moving that credential to a node is C2; the endpoint
//! and UI that drive this are C3/C4.
//!
//! ## The two pieces
//!
//! - A [`RuntimeAuthDescriptor`] says what one runtime's flow needs — the
//!   provider's two endpoints, the client id, scopes, how fast to poll, and how
//!   to turn the provider's token response into the bytes that runtime expects
//!   in its credential file. Registering a runtime is adding one of these; the
//!   driver never learns a runtime's name.
//! - [`DeviceFlow`] drives the grant against a descriptor. It is two calls
//!   rather than one — [`DeviceFlow::begin`] returns the code a person has to
//!   see, and [`DeviceFlow::wait`] blocks until they approve it — because the
//!   caller must be able to *show* the code while the poll is still running.
//!
//! ## The credential never lands anywhere
//!
//! [`RuntimeCredential`] is returned by value and nothing here can write it:
//! the driver holds a descriptor and an HTTP client, and no database pool, no
//! path, and no store. `RuntimeCredential`'s `Debug` redacts the payload, so it
//! cannot reach a log by accident either — a log line is a persistence sink
//! that nobody remembers choosing (epic NG-2, AC-3).
//!
//! The payload is **opaque past this boundary**: `{ runtime, payload }` and
//! nothing more, which is what lets C2 be built without knowing that `claude`
//! wants `.credentials.json`.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// How a token response becomes the bytes a runtime wants on disk.
///
/// A function rather than an enum of known shapes: a runtime whose credential
/// file looks like nothing we have seen is then still "a descriptor addition
/// only" (AC-5), which is the property the epic is buying.
pub type Materialize = fn(&TokenResponse) -> Result<Vec<u8>, RuntimeAuthError>;

/// Everything the flow needs for one runtime, and nothing about any other.
#[derive(Clone)]
pub struct RuntimeAuthDescriptor {
    /// The runtime this authorizes — `claude`, `hermes`, … Carried through to
    /// the credential so C2 knows which installer to use, and never
    /// interpreted here.
    pub runtime: &'static str,
    /// RFC 8628 §3.1 — where a device authorization is requested.
    pub device_authorization_endpoint: String,
    /// RFC 8628 §3.4 — where the device code is exchanged for a token.
    pub token_endpoint: String,
    pub client_id: String,
    pub scopes: String,
    /// The floor for polling. The provider's own `interval` wins when it asks
    /// for something slower, and `slow_down` raises it further (RFC 8628 §3.5).
    pub poll_interval: Duration,
    pub materialize: Materialize,
}

/// What the provider said when the device authorization was requested.
///
/// `user_code` and `verification_uri` are what a person needs to see;
/// `device_code` is the secret half and never leaves this crate.
#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Some providers send a URI with the code already embedded, so the browser
    /// needs no typing at all. Show it when it is there.
    pub verification_uri_complete: Option<String>,
    pub expires_in: Duration,
    interval: Duration,
}

impl PendingAuthorization {
    /// The link to put in front of a person: the pre-filled one when the
    /// provider offers it, otherwise the plain one.
    pub fn link(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// A runtime credential, in flight.
///
/// `Debug` deliberately does not print `payload`. This type exists to be
/// carried from a provider to a node and dropped; anything that renders it —
/// a tracing field, a `dbg!`, an error chain — would be persisting it
/// somewhere nobody chose.
#[derive(Clone)]
pub struct RuntimeCredential {
    pub runtime: String,
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for RuntimeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeCredential")
            .field("runtime", &self.runtime)
            .field(
                "payload",
                &format_args!("<{} bytes redacted>", self.payload.len()),
            )
            .finish()
    }
}

/// The provider's token response, as RFC 8628 §3.5 describes it.
///
/// Kept permissive on purpose: providers add fields, and a strict struct would
/// turn a provider's harmless addition into a failed login. The extra fields
/// are preserved in `raw` so a materializer can use whatever its runtime wants.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// The whole body, for materializers that need a field this struct does not
    /// name.
    #[serde(skip)]
    pub raw: serde_json::Value,
}

/// How a device flow ends when it does not end with a credential.
///
/// Each variant is a distinct UI state (AC-4): "the code ran out, start again",
/// "you declined", "the runtime replied with something we cannot use", and
/// "we could not reach it at all". Collapsing them into one error is what makes
/// an authorization screen say "something went wrong".
#[derive(Debug, thiserror::Error)]
pub enum RuntimeAuthError {
    /// RFC 8628 `expired_token`, or the deadline passed with no answer.
    #[error("the code expired before it was approved")]
    Expired,
    /// RFC 8628 `access_denied` — a person said no.
    #[error("the request was declined at the provider")]
    Denied,
    /// The provider answered, and the answer was not usable — an unrecognised
    /// `error`, or a success with nothing in it.
    #[error("the provider refused: {0}")]
    Provider(String),
    /// The provider could not be reached, or its reply was not the shape RFC
    /// 8628 describes.
    #[error("cannot complete the device flow: {0}")]
    Transport(String),
}

/// The device authorization grant, run by the control plane.
pub struct DeviceFlow {
    descriptor: RuntimeAuthDescriptor,
    http: reqwest::Client,
}

impl DeviceFlow {
    pub fn new(descriptor: RuntimeAuthDescriptor) -> Self {
        Self {
            descriptor,
            http: reqwest::Client::new(),
        }
    }

    /// The runtime this flow authorizes.
    pub fn runtime(&self) -> &str {
        self.descriptor.runtime
    }

    /// RFC 8628 §3.1 — ask the provider to start an authorization.
    ///
    /// Returns as soon as the provider answers, so the caller can put the code
    /// on screen while [`wait`](Self::wait) is still polling.
    pub async fn begin(&self) -> Result<PendingAuthorization, RuntimeAuthError> {
        let body: DeviceStart = self
            .http
            .post(&self.descriptor.device_authorization_endpoint)
            .form(&[
                ("client_id", self.descriptor.client_id.as_str()),
                ("scope", self.descriptor.scopes.as_str()),
            ])
            .send()
            .await
            .map_err(|e| RuntimeAuthError::Transport(e.to_string()))?
            .json()
            .await
            .map_err(|e| {
                RuntimeAuthError::Transport(format!(
                    "the device authorization reply was not what RFC 8628 describes: {e}"
                ))
            })?;

        // The provider's interval is a floor it is asking us to respect; ours is
        // a floor we impose. Take the slower of the two rather than whichever
        // happens to be configured — polling faster than asked gets a client
        // rate-limited, which then looks like the provider being broken.
        let interval = self
            .descriptor
            .poll_interval
            .max(Duration::from_secs(body.interval.max(1)));

        Ok(PendingAuthorization {
            device_code: body.device_code,
            user_code: body.user_code,
            verification_uri: body.verification_uri,
            verification_uri_complete: body.verification_uri_complete,
            expires_in: Duration::from_secs(body.expires_in.unwrap_or(600)),
            interval,
        })
    }

    /// RFC 8628 §3.4/§3.5 — poll until somebody approves, or it ends.
    ///
    /// The credential comes back by value and is written nowhere.
    pub async fn wait(
        &self,
        pending: &PendingAuthorization,
    ) -> Result<RuntimeCredential, RuntimeAuthError> {
        let mut interval = pending.interval;
        let deadline = Instant::now() + pending.expires_in;

        loop {
            if Instant::now() >= deadline {
                // A deadline reached without the provider saying so is the same
                // outcome as `expired_token` to anyone looking at it.
                return Err(RuntimeAuthError::Expired);
            }
            tokio::time::sleep(interval).await;

            let resp = self.exchange(&pending.device_code).await?;

            // A response carrying no `error` is the success case; the
            // materializer decides whether it actually contains what the
            // runtime needs.
            if resp.error.is_none() {
                let payload = (self.descriptor.materialize)(&resp)?;
                return Ok(RuntimeCredential {
                    runtime: self.descriptor.runtime.to_string(),
                    payload,
                });
            }

            match resp.error.as_deref() {
                Some("authorization_pending") => {}
                // An instruction, not a complaint (RFC 8628 §3.5): each
                // `slow_down` adds five seconds, permanently, for this flow.
                Some("slow_down") => interval += Duration::from_secs(5),
                Some("access_denied") => return Err(RuntimeAuthError::Denied),
                Some("expired_token") => return Err(RuntimeAuthError::Expired),
                Some(other) => return Err(RuntimeAuthError::Provider(other.to_string())),
                None => unreachable!("handled above"),
            }
        }
    }

    /// One token-endpoint round trip.
    async fn exchange(&self, device_code: &str) -> Result<TokenResponse, RuntimeAuthError> {
        let raw: serde_json::Value = self
            .http
            .post(&self.descriptor.token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", self.descriptor.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(|e| RuntimeAuthError::Transport(e.to_string()))?
            .json()
            .await
            .map_err(|e| {
                RuntimeAuthError::Transport(format!("the token endpoint replied unusably: {e}"))
            })?;

        // Parsed twice on purpose: once into the named fields, once kept whole,
        // so a materializer can reach a field this struct does not know about
        // without every provider needing a struct change here.
        let mut parsed: TokenResponse = serde_json::from_value(raw.clone()).map_err(|e| {
            RuntimeAuthError::Transport(format!("the token response was not JSON we can read: {e}"))
        })?;
        parsed.raw = raw;
        Ok(parsed)
    }
}

/// RFC 8628 §3.2, the fields we use.
#[derive(Debug, Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
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

// ── the registry ────────────────────────────────────────────────────────────

/// The `claude` descriptor (AC-1), used by default.
///
/// Endpoints and client id come from the environment rather than being compiled
/// in: they are the provider's, they differ per deployment, and baking them
/// here would make a provider change a release.
pub fn claude_descriptor() -> Option<RuntimeAuthDescriptor> {
    Some(RuntimeAuthDescriptor {
        runtime: "claude",
        device_authorization_endpoint: std::env::var("NOOK_CLAUDE_DEVICE_AUTH_ENDPOINT").ok()?,
        token_endpoint: std::env::var("NOOK_CLAUDE_TOKEN_ENDPOINT").ok()?,
        client_id: std::env::var("NOOK_CLAUDE_CLIENT_ID").ok()?,
        scopes: std::env::var("NOOK_CLAUDE_SCOPES")
            .unwrap_or_else(|_| "org:create_api_key user:profile user:inference".into()),
        poll_interval: Duration::from_secs(5),
        materialize: materialize_token_json,
    })
}

/// The default materializer: the provider's token response, verbatim JSON.
///
/// This is what a runtime that stores "whatever the provider sent" wants —
/// `claude`'s `.credentials.json` among them. A runtime needing a different
/// shape supplies its own function; that is the whole point of AC-5.
pub fn materialize_token_json(token: &TokenResponse) -> Result<Vec<u8>, RuntimeAuthError> {
    if token.access_token.is_none() && token.id_token.is_none() {
        // A 200 with neither token is the provider agreeing to something it did
        // not then do. Failing here rather than shipping empty bytes is what
        // stops a node installing a credential that cannot work.
        return Err(RuntimeAuthError::Provider(
            "the token response carried neither an access_token nor an id_token".into(),
        ));
    }
    serde_json::to_vec(&token.raw)
        .map_err(|e| RuntimeAuthError::Transport(format!("cannot serialize the credential: {e}")))
}

/// Look up a runtime's descriptor. `None` for a runtime we cannot authorize
/// this way — the caller refuses rather than guessing at endpoints.
pub fn descriptor_for(runtime: &str) -> Option<RuntimeAuthDescriptor> {
    match runtime {
        "claude" => claude_descriptor(),
        _ => None,
    }
}
