//! Everything produces events: chronological, searchable, auditable.

use nook_types::{Event, EventId, NodeId, SessionId, TenantId, WorkspaceId};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub struct EventDraft {
    pub kind: &'static str,
    pub actor_type: Option<&'static str>,
    pub actor_id: Option<Uuid>,
    pub workspace_id: Option<WorkspaceId>,
    pub node_id: Option<NodeId>,
    pub session_id: Option<SessionId>,
    pub payload: Value,
}

impl EventDraft {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            actor_type: None,
            actor_id: None,
            workspace_id: None,
            node_id: None,
            session_id: None,
            payload: Value::Object(Default::default()),
        }
    }

    pub fn actor(mut self, actor_type: &'static str, id: Uuid) -> Self {
        self.actor_type = Some(actor_type);
        self.actor_id = Some(id);
        self
    }

    pub fn workspace(mut self, id: WorkspaceId) -> Self {
        self.workspace_id = Some(id);
        self
    }

    pub fn node(mut self, id: NodeId) -> Self {
        self.node_id = Some(id);
        self
    }

    pub fn session(mut self, id: SessionId) -> Self {
        self.session_id = Some(id);
        self
    }

    pub fn payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }
}

/// Record an event and push it to live UI subscribers. Failures are logged,
/// never fatal — activity is observability, not a transaction participant.
pub async fn record(
    state: &crate::state::AppState,
    tenant_id: TenantId,
    draft: EventDraft,
) -> Option<Event> {
    let event = insert(&state.db, tenant_id, draft).await;
    if let Some(event) = &event {
        state.registry.publish(
            tenant_id,
            nook_proto::UiEvent::Activity {
                event: event.clone(),
            },
        );
        // Some events are worth interrupting somebody for. Deciding that HERE,
        // once, is what makes every notification channel work without any call
        // site knowing they exist — recording an event is the only thing a
        // feature has to do to be notifiable.
        if let Some(draft) = notable(state, event) {
            crate::services::notify::raise(state, tenant_id, draft).await;
        }
    }
    event
}

/// Which events become notifications, and how to phrase them.
///
/// Curated rather than "everything": an inbox that receives every event is one
/// nobody reads, and the whole value of a bell icon is that a number on it
/// means something. Everything not listed still lands in the activity log,
/// which is the complete record.
fn notable(
    state: &crate::state::AppState,
    event: &Event,
) -> Option<crate::services::notify::Draft> {
    use crate::services::notify::Draft;

    let base = state.cfg.public_base_url.trim_end_matches('/');
    let text = |k: &str| -> Option<&str> { event.payload.get(k).and_then(|v| v.as_str()) };
    let title = text("title").unwrap_or_default();

    let d = match event.kind.as_str() {
        "node.disconnected" => Draft::new("Node disconnected")
            .level("warning")
            .body(text("name").unwrap_or("a node").to_string()),
        "node.connected" => Draft::new("Node connected")
            .level("success")
            .body(text("hostname").unwrap_or("a node").to_string()),
        "node.error" => Draft::new("Node error")
            .level("error")
            .body(text("message").unwrap_or_default().to_string()),
        "git.clone_finished" => Draft::new("Clone finished")
            .level(
                if event.payload.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                    "error"
                } else {
                    "success"
                },
            )
            .body(text("message").unwrap_or_default().to_string()),
        "session.exited" => Draft::new("Session ended").level("warning"),
        "task.pr_submitted" => Draft::new("PR submitted")
            .level("success")
            .body(text("pr_url").unwrap_or_default().to_string()),
        "task.work_started" => Draft::new("Work started")
            .level("info")
            .body(title.to_string()),
        "task.claimed" => Draft::new("Task claimed")
            .level("info")
            .body(title.to_string()),
        "skill.install_failed" => Draft::new("A node could not learn a skill")
            .level("error")
            .body(text("error").unwrap_or_default().to_string()),
        _ => return None,
    };

    let d = d.kind(event.kind.clone()).payload(event.payload.clone());
    // Somewhere to go. A notification you cannot act on is a notification you
    // learn to ignore.
    Some(match (event.session_id, event.payload.get("task_id")) {
        (Some(sid), _) => d.link(format!("{base}/sessions/{sid}")),
        (None, Some(t)) => d.link(format!(
            "{base}/board?task={}",
            t.as_str().unwrap_or_default()
        )),
        _ => d.link(format!("{base}/activity")),
    })
}

/// Insert only (no live publish) — for contexts without an `AppState`, e.g.
/// seeding.
pub async fn insert(db: &PgPool, tenant_id: TenantId, draft: EventDraft) -> Option<Event> {
    let res: Result<Event, sqlx::Error> = sqlx::query_as(
        "INSERT INTO events (id, tenant_id, kind, actor_type, actor_id, workspace_id, node_id, session_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING *",
    )
    .bind(EventId::new())
    .bind(tenant_id)
    .bind(draft.kind)
    .bind(draft.actor_type)
    .bind(draft.actor_id)
    .bind(draft.workspace_id)
    .bind(draft.node_id)
    .bind(draft.session_id)
    .bind(&draft.payload)
    .fetch_one(db)
    .await;

    match res {
        Ok(event) => Some(event),
        Err(e) => {
            tracing::warn!(error = %e, kind = draft.kind, "failed to record event");
            None
        }
    }
}
