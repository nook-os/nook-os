//! Keeping the email chain current as the work moves (MAIN-330 AC-2).
//!
//! The pipeline writes the link and its run itself, because it holds the row it
//! just created. A PR is different: it is opened by somebody who has never heard
//! of an email — a build run concluding, or a human submitting from the board —
//! and reaches the card through two separate paths. This is the one function
//! both call, so a chain cannot be completed on one path and left short on the
//! other.

use nook_types::*;

use crate::state::AppState;

/// Record the PR on every chain that ends at this card.
///
/// **Best effort, and deliberately so.** The caller has already recorded the PR
/// where it matters — on the card, which is what the reviewer and the board
/// read. A link that failed to pick it up is a gap in a cross-reference, and
/// failing the PR submission over it would trade something that matters for
/// something that does not. The gap is logged rather than swallowed.
///
/// A card with no chain — the overwhelming majority — updates nothing and says
/// nothing.
pub async fn record_pr(state: &AppState, tenant: TenantId, task: TaskId, pr_url: &str) {
    match state.email_links.set_pr_ref(tenant, task, pr_url).await {
        Ok(0) => {}
        Ok(n) => {
            tracing::debug!(%tenant, %task, pr = pr_url, links = n, "email chain now names its PR")
        }
        Err(e) => tracing::warn!(
            %tenant, %task, pr = pr_url, error = %e,
            "the PR is on the card but its email chain still names none"
        ),
    }
}

/// The read-only investigate run's two calls (MAIN-331): reading the message it
/// was seeded from, and reporting what it found.
///
/// **Both are addressed by the JOB, never by the link.** The run knows its own
/// id — the control plane put it in its environment — and knows nothing else,
/// so a run cannot read a message it was not seeded from and cannot write onto
/// another message's chain. That is the same confinement `record_build_outcome`
/// gets from `NOOK_JOB_ID`, for a surface where the content is somebody's
/// support mail rather than a PR URL.
///
/// The vault lives here rather than in the repository: sealing is a decision
/// about content, and the storage layer holds bytes.
pub mod investigation {
    use nook_types::*;

    use crate::error::{ApiError, ApiResult};
    use crate::services::jobs::INVESTIGATE_KIND;
    use crate::state::AppState;

    /// The run's chain, with the two refusals that make the surface read-only
    /// and confined: a job of any other kind has no business here, and a job
    /// with no chain is a 404 rather than somebody else's row.
    async fn chain(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
    ) -> ApiResult<EmailLink> {
        let row = state
            .jobs
            .get(tenant, job)
            .await?
            .ok_or(ApiError::NotFound)?;
        if row.kind != INVESTIGATE_KIND {
            return Err(ApiError::BadRequest(
                "only an investigate run reads a support message or reports an investigation"
                    .into(),
            ));
        }
        state
            .email_links
            .by_job(tenant, viewer, job)
            .await?
            .ok_or(ApiError::NotFound)
    }

    /// The original message, decrypted (AC-4).
    ///
    /// Lossy UTF-8 rather than a refusal: a mail transport moves 7- or 8-bit
    /// text and encodes anything else, so a byte that will not decode is a
    /// malformed delivery — and an investigation of one should read what did
    /// arrive rather than be handed an error about it.
    ///
    /// Nothing here logs the plaintext, and nothing may be added that does:
    /// this function returns the only copy the control plane makes, straight to
    /// the run that asked.
    pub async fn message(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
    ) -> ApiResult<DecryptedMessage> {
        let link = chain(state, tenant, viewer, job).await?;
        let sealed = state
            .user_content_store
            .get(&link.storage_key)
            .await
            .map_err(|e| {
                ApiError::Internal(anyhow::anyhow!(
                    "the sealed original of this chain is unreadable: {e}"
                ))
            })?;
        let raw = state.vault.decrypt(&sealed).map_err(|e| {
            ApiError::Internal(anyhow::anyhow!("unsealing the original message: {e}"))
        })?;
        Ok(DecryptedMessage {
            message: String::from_utf8_lossy(&raw).into_owned(),
        })
    }

    /// Record the findings and the sealed draft (AC-2).
    ///
    /// Both halves in one write, because they are one report: a run that
    /// managed the analysis and not the reply has not done the job, and half a
    /// report on the record reads exactly like a whole one.
    pub async fn record(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
        report: &InvestigationReport,
    ) -> ApiResult<EmailLink> {
        let findings = report.findings.trim();
        let draft = report.draft_reply.trim();
        if findings.is_empty() || draft.is_empty() {
            return Err(ApiError::BadRequest(
                "an investigation reports both findings and a draft reply".into(),
            ));
        }
        let link = chain(state, tenant, viewer, job).await?;

        // Sealed BEFORE the write, so a vault failure leaves the row untouched
        // rather than storing findings beside a draft that never landed.
        let sealed = state
            .vault
            .encrypt(draft.as_bytes())
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("sealing the draft reply: {e}")))?;

        state
            .email_links
            .set_investigation(tenant, link.id, findings, sealed)
            .await?;

        state
            .email_links
            .by_job(tenant, viewer, job)
            .await?
            .ok_or(ApiError::NotFound)
    }
}
