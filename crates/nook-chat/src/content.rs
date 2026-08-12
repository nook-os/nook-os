//! Forgetting the bytes behind an attachment (MAIN-535 AC-6).
//!
//! Chat records *that* a message carries a file; the file itself lives in the
//! control plane's user-content store (MAIN-532). So deleting a message has one
//! half chat can do — its own rows — and one half it cannot: the bytes.
//!
//! **Chat does not reach the store directly, and that is not squeamishness.**
//! The shipped default backend is a directory, and chat runs in its own
//! container with its own filesystem: a `DiskStore` opened here would look
//! perfectly healthy and delete nothing, because the object it is asked for was
//! written on the other side of a container boundary. Only the control plane can
//! honestly answer "these bytes are gone".
//!
//! **So chat asks, carrying the CALLER's own credential.** No service identity,
//! no shared secret, no privilege chat holds when nobody is asking — the
//! `DELETE /api/v1/user-content/{id}` that runs is exactly the one the person
//! would have got by deleting the file themselves, authorized by the same rule
//! (uploader, or a tenant owner/admin). That rule is also why this always
//! succeeds where it should: an attachment can only ever be its author's own
//! upload (`uploads_of`), and a message is deleted by its author or an admin.
//!
//! Failure is logged, not surfaced. The message is soft-deleted and its
//! attachment rows are gone either way; what an unreachable control plane costs
//! is an object nobody can see and nobody can reach — the same orphan the
//! upload path already tolerates in the other direction.

use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

/// What the caller arrived with, kept so a request can be re-made as them.
///
/// Deliberately the credential and not a token minted from it: chat is
/// forwarding the caller's own authority for the length of one request, which
/// is a very different thing from holding authority of its own.
#[derive(Clone)]
pub(crate) enum Credential {
    Bearer(String),
    Session(Uuid),
}

/// Hand-written so a token cannot reach a log through a stray `{:?}`. Which
/// KIND of credential it is stays visible — that is the part worth debugging.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Self::Session(_) => f.write_str("Session(<redacted>)"),
        }
    }
}

#[async_trait]
pub(crate) trait ContentStore: Send + Sync {
    /// Remove an upload and its bytes, as `caller`. Idempotent from chat's side:
    /// content that is already gone is success, because the state asked for is
    /// the state that holds.
    async fn forget(&self, content_id: Uuid, caller: &Credential) -> anyhow::Result<()>;
}

/// The control plane's user-content routes over HTTP.
pub(crate) struct ControlPlaneContent {
    client: reqwest::Client,
    /// Origin only — `http://control-plane:8080` in compose, whatever the
    /// deployment's internal name is elsewhere.
    origin: String,
}

/// How long to wait for the control plane before giving up on one object.
///
/// A default `reqwest::Client` has NO timeout, and this call is awaited inline
/// in the delete path, once per attachment, before the deletion is broadcast.
/// Against a control plane that black-holes packets rather than refusing the
/// connection that turns a best-effort tidy-up into a delete that never
/// returns and peers that never learn the message is gone. Generous enough for
/// a slow object store, short enough that a dead peer costs seconds.
const FORGET_TIMEOUT: Duration = Duration::from_secs(10);

impl ControlPlaneContent {
    pub(crate) fn new(origin: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(FORGET_TIMEOUT)
                .build()
                // A client that will not build is a broken TLS backend, not a
                // configuration a deployment can fix at runtime; the default
                // one still deletes, it just cannot time out.
                .unwrap_or_else(|_| reqwest::Client::new()),
            origin: origin.trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait]
impl ContentStore for ControlPlaneContent {
    async fn forget(&self, content_id: Uuid, caller: &Credential) -> anyhow::Result<()> {
        let url = format!("{}/api/v1/user-content/{content_id}", self.origin);
        let req = self.client.delete(&url);
        let req = match caller {
            Credential::Bearer(token) => req.bearer_auth(token),
            // The cookie's value IS the plaintext session id — the same thing
            // the browser sends, so no other header is needed to be this
            // caller.
            Credential::Session(id) => req.header(
                reqwest::header::COOKIE,
                format!("{}={id}", nook_auth::SESSION_COOKIE),
            ),
        };
        let res = req.send().await?;
        // 404 is already-gone, which is the state the caller asked for.
        if res.status().is_success() || res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        anyhow::bail!("control plane refused the delete: {}", res.status())
    }
}

/// Records what it was asked to forget instead of asking anybody (tests).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingContent {
    forgotten: std::sync::Mutex<Vec<Uuid>>,
}

#[cfg(test)]
impl RecordingContent {
    pub(crate) fn forgotten(&self) -> Vec<Uuid> {
        self.forgotten.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl ContentStore for RecordingContent {
    async fn forget(&self, content_id: Uuid, _caller: &Credential) -> anyhow::Result<()> {
        self.forgotten.lock().unwrap().push(content_id);
        Ok(())
    }
}
