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
