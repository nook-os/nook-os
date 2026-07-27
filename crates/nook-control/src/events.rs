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
        if let Some(draft) = notable(state.cfg.public_base_url.as_str(), event) {
            crate::services::notify::raise(state, tenant_id, draft).await;
        }
    }
    event
}

/// The escalation labels — the only label changes worth interrupting a human
/// for (MAIN-91 AC-2). An agent applies one of these to say "I am stuck and
/// need a person"; every other label is ordinary board hygiene and stays
/// silent.
pub const ESCALATION_LABELS: [&str; 3] = ["blocked", "spec-blocked", "needs-human-review"];

/// The authoritative catalog of every event kind the bell can raise (MAIN-91
/// AC-3). `notable()` gates on this list, so it is complete by construction —
/// an uncatalogued kind cannot notify — and a settings UI can render it as a
/// per-kind/per-group checklist compatible with the channel `kinds` prefix
/// filter. Keep an entry here for every arm `notable()` phrases; the
/// completeness test proves the two agree.
pub fn catalog() -> Vec<nook_types::NotificationKind> {
    // group = the dotted prefix the channel `kinds` filter already matches on.
    let k = |id: &str, label: &str, description: &str| nook_types::NotificationKind {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        group: id
            .split_once('.')
            .map(|(g, _)| format!("{g}."))
            .unwrap_or_else(|| id.into()),
    };
    vec![
        k("node.connected", "Node connected", "A node came online."),
        k(
            "node.disconnected",
            "Node disconnected",
            "A node went offline.",
        ),
        k("node.error", "Node error", "A node reported an error."),
        k(
            "git.clone_finished",
            "Clone finished",
            "A repository clone completed, successfully or not.",
        ),
        k(
            "session.exited",
            "Session ended",
            "A terminal session ended.",
        ),
        k(
            "task.pr_submitted",
            "PR submitted",
            "A pull request was recorded against a task.",
        ),
        k(
            "task.work_started",
            "Work started",
            "A task moved into active work.",
        ),
        k("task.claimed", "Task claimed", "Someone claimed a task."),
        k(
            "task.comment.created",
            "New comment",
            "Someone commented on a non-private task.",
        ),
        k(
            "task.label.added",
            "Escalation label",
            "An escalation label (blocked, spec-blocked, needs-human-review) was added to a task.",
        ),
        k(
            "task.automation_failed",
            "Automation failed",
            "A board automation action failed.",
        ),
        k(
            "skill.install_failed",
            "Skill install failed",
            "A node could not install a skill.",
        ),
        k(
            "hooks.install_failed",
            "Hooks install failed",
            "A node could not apply the managed hook set.",
        ),
    ]
}

/// Which events become notifications, and how to phrase them.
///
/// Curated rather than "everything": an inbox that receives every event is one
/// nobody reads, and the whole value of a bell icon is that a number on it
/// means something. Everything not listed still lands in the activity log,
/// which is the complete record.
///
/// Gated on [`catalog`] first, so the catalog is the single authoritative list
/// of what can fire (MAIN-91 AC-3): a kind absent from it never raises, and the
/// completeness test proves every catalogued kind is phrased below.
pub fn notable(base_url: &str, event: &Event) -> Option<crate::services::notify::Draft> {
    use crate::services::notify::Draft;

    if !catalog().iter().any(|k| k.id == event.kind) {
        return None;
    }

    let base = base_url.trim_end_matches('/');
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
        // A new comment on a NON-PRIVATE task (MAIN-91 AC-1). The excerpt is put
        // in the event payload by `create_comment` ONLY for a non-private card,
        // so a private card's comment carries no body here and is not notable —
        // privacy holds at this bridge without a DB read (NG-4).
        "task.comment.created" => {
            let excerpt = text("excerpt")?;
            let who = text("author").unwrap_or("someone");
            let key = text("key").unwrap_or_default();
            Draft::new(if key.is_empty() {
                "New comment".to_string()
            } else {
                format!("New comment on {key}")
            })
            .level("info")
            .body(format!("{who}: {excerpt}"))
        }
        // A label was added — notable ONLY for the escalation labels (AC-2).
        // Named by label + task key, never the title, so an escalation on a
        // private card does not leak its title (the key is already tenant-
        // visible in activity per MAIN-76).
        "task.label.added" => {
            let label = text("label").unwrap_or_default();
            if !ESCALATION_LABELS.contains(&label) {
                return None;
            }
            let key = text("key").unwrap_or_default();
            Draft::new(format!("Task labeled {label}"))
                .level("warning")
                .body(if key.is_empty() {
                    format!("A task was labeled {label}")
                } else {
                    format!("{key} was labeled {label}")
                })
        }
        // Board automation (MAIN-73 AC-5): an action failed. Error level, and the
        // payload's `task_id` gives the branch below a `/board?task=…` deep link.
        "task.automation_failed" => {
            Draft::new("Automation action failed")
                .level("error")
                .body(format!(
                    "{}: {}",
                    text("action").unwrap_or("action"),
                    text("error").unwrap_or_default()
                ))
        }
        "skill.install_failed" => Draft::new("A node could not learn a skill")
            .level("error")
            .body(text("error").unwrap_or_default().to_string()),
        "hooks.install_failed" => Draft::new("A node could not apply the managed hooks")
            .level("error")
            .body(text("error").unwrap_or_default().to_string()),
        // Unreachable: the catalog gate above rejects any kind not phrased here.
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

#[cfg(test)]
mod tests {
    use super::*;
    use nook_types::{Event, EventId, TenantId};

    fn ev(kind: &str, payload: Value) -> Event {
        Event {
            id: EventId::new(),
            tenant_id: TenantId(Uuid::nil()),
            occurred_at: chrono::Utc::now(),
            kind: kind.into(),
            actor_type: None,
            actor_id: None,
            workspace_id: None,
            node_id: None,
            session_id: None,
            payload,
        }
    }

    /// Every kind the catalog advertises must actually be phrasable by
    /// `notable()` — a catalogued kind it drops to `None` would be a promise the
    /// bell never keeps. Because `notable()` gates on the catalog, the reverse
    /// holds by construction (an uncatalogued kind cannot notify), so together
    /// this makes the catalog the authoritative, complete list (AC-3). Adding an
    /// arm to `notable()` without a catalog entry makes it dead — and adding a
    /// catalog entry without an arm fails this test.
    #[test]
    fn catalog_and_notable_agree() {
        // Rich enough to satisfy the gated arms (a comment needs an excerpt; an
        // escalation label needs its name).
        let payload = serde_json::json!({
            "excerpt": "hi", "author": "ada", "label": "blocked",
            "key": "MAIN-1", "task_id": Uuid::nil().to_string(),
        });
        for k in catalog() {
            let d = notable("http://x", &ev(&k.id, payload.clone()));
            let d = d.unwrap_or_else(|| panic!("catalogued kind {:?} is not notable", k.id));
            assert_eq!(
                d.kind, k.id,
                "notable must tag the draft with the event kind"
            );
            assert!(
                k.id.starts_with(&k.group),
                "{} not in group {}",
                k.id,
                k.group
            );
            assert!(
                k.group.ends_with('.'),
                "group {:?} should end with '.'",
                k.group
            );
            assert!(
                !k.label.is_empty() && !k.description.is_empty(),
                "{} needs a label and description",
                k.id
            );
        }
        // No duplicate ids — a checklist that lists the same kind twice.
        let ids: Vec<String> = catalog().into_iter().map(|k| k.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate catalog ids: {ids:?}");
    }

    /// An uncatalogued kind never notifies — the gate is what makes the catalog
    /// authoritative (AC-3/AC-4). `task.created` and `task.moved` are recorded
    /// all the time and must stay silent.
    #[test]
    fn uncatalogued_kinds_are_silent() {
        for kind in ["task.created", "task.moved", "user.login", "nonsense"] {
            assert!(
                notable("http://x", &ev(kind, serde_json::json!({}))).is_none(),
                "{kind} must not notify"
            );
        }
    }

    /// A label add notifies for the three escalation labels and nothing else
    /// (AC-2).
    #[test]
    fn only_escalation_labels_notify() {
        for label in ESCALATION_LABELS {
            let d = notable(
                "http://x",
                &ev("task.label.added", serde_json::json!({ "label": label })),
            );
            assert!(d.is_some(), "{label} should notify");
        }
        for label in ["frontend", "agent-ready", "bug", "urgent"] {
            let d = notable(
                "http://x",
                &ev("task.label.added", serde_json::json!({ "label": label })),
            );
            assert!(d.is_none(), "{label} must stay silent");
        }
    }

    /// A comment carries an excerpt only for a non-private card (set by
    /// `create_comment`); without one there is nothing to notify, so a private
    /// card's comment is silent at the bridge (AC-1, NG-4).
    #[test]
    fn a_comment_without_an_excerpt_is_silent() {
        // Non-private: excerpt present → notable, deep-linked to the card.
        let d = notable(
            "http://x",
            &ev(
                "task.comment.created",
                serde_json::json!({ "excerpt": "let's ship", "author": "ada", "key": "MAIN-3", "task_id": "MAIN-3" }),
            ),
        )
        .expect("a non-private comment is notable");
        assert_eq!(d.kind, "task.comment.created");
        assert!(
            d.link.unwrap().contains("board?task="),
            "deep-linked to the card"
        );
        assert!(d.body.contains("let's ship"));

        // Private: no excerpt → silent.
        let d = notable(
            "http://x",
            &ev(
                "task.comment.created",
                serde_json::json!({ "author": "ada", "task_id": "MAIN-4" }),
            ),
        );
        assert!(d.is_none(), "a private card's comment must not notify");
    }
}
