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
    }
    event
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
