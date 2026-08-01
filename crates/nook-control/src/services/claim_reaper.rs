//! The board-card claim reaper (MAIN-229): the safety net the *card* layer was
//! missing. `job_reaper` rescues loop jobs whose executor went dark; this
//! rescues the card itself, so a crashed or wedged agent can no longer strand a
//! ticket in In Progress forever (the MAIN-196 stranding).
//!
//! **A lease is the whole fence.** `tasks.claim_expires_at` is non-NULL only for
//! a card that entered `started` through the agent claim / start-work path. A
//! card a human dragged there has no lease, and every query below tests
//! `claim_expires_at IS NOT NULL` — so a hand-arranged board is not merely
//! *unlikely* to be disturbed, it is outside the candidate set (AC-7).
//!
//! **Two actions, no third (NG-5).** Either requeue a leased card whose worker
//! is provably gone — its bound session exited/errored, or that session's node
//! stopped reporting — or, for the wedged-but-alive shape session liveness
//! cannot see, escalate to a human with a label and a notification. The
//! escalated card is never moved: a genuinely long build must not be killed on a
//! timer, so the cap raises a hand rather than a knife.
//!
//! Both scans are safe on every replica. The requeue is one guarded conditional
//! `UPDATE` that clears the lease it matched on; the escalation's exactly-once
//! fence is the `(task_id, label_id)` primary key, so the replica that actually
//! inserted the label is the one that comments and notifies.

use std::time::Duration;

use nook_types::{TaskId, TenantId};

use crate::error::ApiResult;
use crate::repo::tasks::{CappedClaim, LapsedClaim};
use crate::state::AppState;

/// The label an escalation raises. Also what the build loop's own skills treat
/// as "a human must look at this", which is why it is this name and not a new one.
const ESCALATION_LABEL: &str = "needs-human-review";

/// Spawn the reaper. Fire-and-forget like the loop-job reaper; the process exits
/// on shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let (grace, cap, scan) = (
            state.cfg.claim_session_grace_secs,
            state.cfg.max_claim_secs,
            state.cfg.claim_reap_scan_secs,
        );
        tracing::info!(
            session_grace_secs = grace,
            max_claim_secs = cap,
            scan_secs = scan,
            "claim reaper started"
        );
        run(state, grace, scan).await;
    });
}

async fn run(state: AppState, session_grace_secs: u64, scan_secs: u64) {
    let interval = Duration::from_secs(scan_secs.max(1));
    loop {
        tokio::time::sleep(interval).await;
        // Deliberately NOT gated on `loops.enabled` (MAIN-239), unlike its two
        // sibling reapers: a lease is written by start-work, which a person can
        // drive from the board with loops off, and a safety net that is only
        // armed sometimes is not one. The scan is a partial-index read over
        // leased cards, so with no claims outstanding it touches nothing.
        match reap_lapsed_claims(&state, session_grace_secs as i64).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(reaped = n, "requeued cards whose claim holder went away"),
            Err(e) => tracing::warn!(error = %e, "claim reaper scan failed"),
        }
        match escalate_capped_claims(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(escalated = n, "claims past the cap escalated to a human"),
            Err(e) => tracing::warn!(error = %e, "claim escalation scan failed"),
        }
    }
}

/// Requeue every leased card whose worker is provably gone. Returns how many.
pub async fn reap_lapsed_claims(state: &AppState, session_grace_secs: i64) -> ApiResult<u64> {
    let lapsed = state.tasks.reap_lapsed_claims(session_grace_secs).await?;

    for l in &lapsed {
        announce_requeue(state, l).await;
    }
    Ok(lapsed.len() as u64)
}

/// Label + notify every leased card past its cap that the requeue did not take —
/// the agent is stuck but its session still looks alive. The card is left
/// exactly where it is (AC-4).
pub async fn escalate_capped_claims(state: &AppState) -> ApiResult<u64> {
    let capped = state.tasks.capped_claims().await?;

    let mut escalated = 0;
    for c in &capped {
        // The label insert is the race: only the replica that actually attached
        // it announces, so a card is escalated once however many replicas scan.
        if !state
            .tasks
            .attach_label_once(c.tenant, c.task, ESCALATION_LABEL)
            .await?
        {
            continue;
        }
        announce_escalation(state, c).await;
        escalated += 1;
    }
    Ok(escalated)
}

/// The three signals a requeue owes a human: an event for the activity feed, a
/// comment on the card saying what happened to it, and a notification.
async fn announce_requeue(state: &AppState, l: &LapsedClaim) {
    let reason = match l.session_status.as_str() {
        "exited" | "error" => format!("session {} {}", l.session, l.session_status),
        _ => format!("session {} node-offline", l.session),
    };
    let body = format!("claim lapsed — {reason}; requeued to To-Do");

    crate::events::record(
        state,
        l.tenant,
        crate::events::EventDraft::new("task.claim_lapsed").payload(serde_json::json!({
            "task_id": l.task,
            "session_id": l.session,
            "session_status": l.session_status,
            "node_last_seen_at": l.node_last_seen_at,
        })),
    )
    .await;

    let _ = state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant: l.tenant,
            task: l.task,
            author_type: "system".into(),
            author_id: None,
            author_name: "Claim reaper".into(),
            body_md: body.clone(),
        })
        .await;

    raise(state, l.tenant, l.task, "Claim lapsed", &body).await;
    state.registry.publish(
        l.tenant,
        nook_proto::UiEvent::TaskChanged { task_id: l.task },
    );
}

async fn announce_escalation(state: &AppState, c: &CappedClaim) {
    let body = format!(
        "claim held past the cap (expired {}) with a live session — left in place for a human",
        c.claim_expires_at.to_rfc3339()
    );

    crate::events::record(
        state,
        c.tenant,
        crate::events::EventDraft::new("task.claim_escalated").payload(serde_json::json!({
            "task_id": c.task,
            "claim_expires_at": c.claim_expires_at,
        })),
    )
    .await;

    let _ = state
        .tasks
        .create_comment(crate::repo::tasks::NewComment {
            tenant: c.tenant,
            task: c.task,
            author_type: "system".into(),
            author_id: None,
            author_name: "Claim reaper".into(),
            body_md: body.clone(),
        })
        .await;

    raise(state, c.tenant, c.task, "Claim needs review", &body).await;
    state.registry.publish(
        c.tenant,
        nook_proto::UiEvent::TaskChanged { task_id: c.task },
    );
}

/// A warning-level notification linking the card. A private card's title never
/// reaches the tenant-wide bell (MAIN-76), so the title is not interpolated —
/// the key and the deep link are enough to act on.
async fn raise(state: &AppState, tenant: TenantId, task: TaskId, title: &str, body: &str) {
    let key = state
        .tasks
        .task_ref(task)
        .await
        .ok()
        .flatten()
        .map(|(key, number, _title)| match key {
            Some(k) => format!("{k}-{number}"),
            None => format!("#{number}"),
        })
        .unwrap_or_else(|| task.to_string());
    let base = state.cfg.public_base_url.trim_end_matches('/');

    crate::services::notify::raise(
        state,
        tenant,
        crate::services::notify::Draft::new(format!("{title}: {key}"))
            .level("warning")
            .kind("task.claim")
            .body(body.to_string())
            .link(format!("{base}/board?task={task}"))
            .payload(serde_json::json!({ "task_id": task, "key": key })),
    )
    .await;
}
