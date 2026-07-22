//! The AI dispatcher. Small. Focused.
//!
//! It prioritizes, routes, and summarizes — it recommends and humans approve.
//! It never codes, edits, deploys, or acts autonomously. Milestone 1 ships a
//! rule-based backend (no LLM calls); an LLM backend slots in behind the same
//! trait when configured.

use async_trait::async_trait;
use nook_types::{
    BoardColumn, DispatchItem, DispatchSuggestion, Event, NodeId, NodeResources, TaskItem,
};

/// Resource-aware triage: pick the online node best able to take new work.
/// Ranks by most free memory, then lowest 1-min load, then fewest active
/// sessions. Deterministic — the human can always override by forcing Todo.
pub fn pick_node(candidates: &[(NodeId, NodeResources)]) -> Option<NodeId> {
    candidates
        .iter()
        .max_by(|(_, a), (_, b)| {
            let free = |r: &NodeResources| r.mem_total.saturating_sub(r.mem_used);
            free(a)
                .cmp(&free(b))
                .then(b.load_avg1.total_cmp(&a.load_avg1))
                .then(b.active_sessions.cmp(&a.active_sessions))
        })
        .map(|(id, _)| *id)
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("dispatcher backend '{0}' is not configured")]
    NotConfigured(&'static str),
    #[error("{0}")]
    Internal(String),
}

/// Everything the dispatcher is allowed to see when making a suggestion.
pub struct DispatchContext {
    pub tasks: Vec<TaskItem>,
    pub columns: Vec<BoardColumn>,
    pub active_sessions: usize,
    pub online_nodes: usize,
}

#[async_trait]
pub trait DispatcherBackend: Send + Sync {
    fn id(&self) -> &'static str;
    async fn suggest(&self, ctx: DispatchContext) -> Result<DispatchSuggestion, DispatchError>;
    async fn summarize_events(&self, events: Vec<Event>) -> Result<String, DispatchError>;
}

/// Deterministic prioritization: in-progress work first, then backlog order.
pub struct RuleBasedDispatcher;

#[async_trait]
impl DispatcherBackend for RuleBasedDispatcher {
    fn id(&self) -> &'static str {
        "rule-based"
    }

    async fn suggest(&self, ctx: DispatchContext) -> Result<DispatchSuggestion, DispatchError> {
        let col_rank = |task: &TaskItem| -> (u8, i32) {
            let name = ctx
                .columns
                .iter()
                .find(|c| c.id == task.column_id)
                .map(|c| c.name.to_lowercase())
                .unwrap_or_default();
            let rank = if name.contains("progress") || name.contains("doing") {
                0
            } else if name.contains("done") || name.contains("complete") {
                2
            } else {
                1
            };
            (rank, task.position)
        };

        let mut open: Vec<&TaskItem> = ctx.tasks.iter().filter(|t| col_rank(t).0 < 2).collect();
        open.sort_by_key(|t| col_rank(t));

        let items: Vec<DispatchItem> = open
            .iter()
            .take(5)
            .map(|t| {
                let (rank, _) = col_rank(t);
                DispatchItem {
                    task_id: Some(t.id),
                    title: t.title.clone(),
                    rationale: if rank == 0 {
                        "already in progress — finish it before starting new work".into()
                    } else {
                        "next in backlog order".into()
                    },
                    suggested_runtime: None,
                    workspace_id: t.workspace_id,
                }
            })
            .collect();

        let headline = if items.is_empty() {
            "Board is clear — nothing waiting.".to_string()
        } else {
            format!(
                "{} open task{} · {} active session{} · {} node{} online",
                open.len(),
                if open.len() == 1 { "" } else { "s" },
                ctx.active_sessions,
                if ctx.active_sessions == 1 { "" } else { "s" },
                ctx.online_nodes,
                if ctx.online_nodes == 1 { "" } else { "s" },
            )
        };

        Ok(DispatchSuggestion { headline, items })
    }

    async fn summarize_events(&self, events: Vec<Event>) -> Result<String, DispatchError> {
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        for e in &events {
            let family = e.kind.split('.').next().unwrap_or("other");
            *counts.entry(family).or_default() += 1;
        }
        Ok(counts
            .into_iter()
            .map(|(family, n)| format!("{n} {family} event{}", if n == 1 { "" } else { "s" }))
            .collect::<Vec<_>>()
            .join(", "))
    }
}

/// Placeholder for a future LLM-backed dispatcher. Never panics — returns
/// NotConfigured until a model is wired up explicitly.
pub struct LlmDispatcher;

#[async_trait]
impl DispatcherBackend for LlmDispatcher {
    fn id(&self) -> &'static str {
        "llm"
    }
    async fn suggest(&self, _ctx: DispatchContext) -> Result<DispatchSuggestion, DispatchError> {
        Err(DispatchError::NotConfigured("llm"))
    }
    async fn summarize_events(&self, _events: Vec<Event>) -> Result<String, DispatchError> {
        Err(DispatchError::NotConfigured("llm"))
    }
}
