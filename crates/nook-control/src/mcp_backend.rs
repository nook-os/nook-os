//! The control plane's implementation of the MCP tool surface. Delegates to
//! the same service layer the REST handlers use.

use async_trait::async_trait;
use nook_mcp::{McpCaller, NookBackend};
use nook_proto::ControlToNode;
use nook_types::*;

use crate::services::{
    activity_queries, node_queries, notebook_queries, session_queries, workspace_queries,
};
use crate::state::AppState;

pub struct McpBackend {
    pub state: AppState,
}

impl McpBackend {
    /// M1: the MCP token maps to the instance's first tenant (dev). Per-user
    /// MCP OAuth is post-M1.
    async fn tenant(&self) -> anyhow::Result<TenantId> {
        crate::services::identity::first_tenant(self.state.identity.as_ref()).await
    }

    /// Who an MCP call acts as.
    ///
    /// There is no per-user MCP identity yet — the token maps to the instance,
    /// so the honest answer is the tenant's owner. Recorded rather than left
    /// null so a comment or a claim has an author that can be revoked.
    async fn user(&self) -> anyhow::Result<UserId> {
        let tenant = self.tenant().await?;
        crate::services::identity::first_user(self.state.identity.as_ref(), tenant).await
    }

    /// Resolve a workspace by **id or slug** (both unique), falling back to name
    /// as a documented convenience that errors on ambiguity rather than silently
    /// picking one (MAIN-223 AC-3). The old `slug = $2 OR name = $2` conflated the
    /// two and returned an arbitrary row when a name matched several workspaces.
    pub async fn resolve_workspace(
        &self,
        tenant: TenantId,
        key: &str,
    ) -> anyhow::Result<WorkspaceId> {
        workspace_queries::resolve_by_key(&self.state.db, tenant, key).await
    }

    /// Resolve a node by name, or auto-pick an online node when omitted.
    async fn resolve_node(&self, tenant: TenantId, name: Option<String>) -> anyhow::Result<NodeId> {
        let nodes = node_queries::list_ids_and_names(&self.state.db, tenant).await?;
        let online: Vec<(NodeId, String)> = nodes
            .into_iter()
            .filter(|(id, _)| self.state.registry.node_online(*id))
            .collect();
        match name {
            Some(n) => online
                .into_iter()
                .find(|(_, nm)| *nm == n)
                .map(|(id, _)| id)
                .ok_or_else(|| anyhow::anyhow!("no online node named '{n}'")),
            None => online
                .into_iter()
                .next()
                .map(|(id, _)| id)
                .ok_or_else(|| anyhow::anyhow!("no online node available")),
        }
    }

    /// Await a long-running node op with a timeout.
    async fn run_op(
        &self,
        node_id: NodeId,
        build: impl FnOnce(uuid::Uuid) -> ControlToNode,
        secs: u64,
    ) -> anyhow::Result<String> {
        let rx = self
            .state
            .registry
            .request_op(node_id, build)
            .ok_or_else(|| anyhow::anyhow!("node is offline"))?;
        let op = tokio::time::timeout(std::time::Duration::from_secs(secs), rx)
            .await
            .map_err(|_| anyhow::anyhow!("node did not answer in time"))?
            .map_err(|_| anyhow::anyhow!("node disconnected"))?;
        if !op.ok {
            anyhow::bail!("{}", op.message);
        }
        Ok(op.message)
    }

    /// Resolve the `KanbanProvider` that owns a task — look up the task's
    /// board, read its `provider` string, and get the instance from the
    /// registry, exactly as `create_task` does for a new task. The single
    /// resolver every MCP task *mutation* routes through, so none hardwires
    /// `LocalBoardProvider` and each task is mutated through its own board's
    /// provider the moment a non-local one exists (MAIN-86 AC-1/AC-2).
    async fn provider_for_task(
        &self,
        tenant: TenantId,
        task_id: TaskId,
    ) -> anyhow::Result<std::sync::Arc<dyn crate::services::kanban::KanbanProvider>> {
        let provider: String = crate::services::tasks::board_provider_for_task(
            self.state.tasks.as_ref(),
            tenant,
            task_id,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such task"))?;
        self.state
            .kanban
            .get(&provider)
            .ok_or_else(|| anyhow::anyhow!("provider {provider:?} missing"))
    }
}

#[async_trait]
impl NookBackend for McpBackend {
    async fn list_workspaces(&self) -> anyhow::Result<Vec<WorkspaceDetail>> {
        let tenant = self.tenant().await?;
        Ok(workspace_queries::list_workspaces(&self.state.db, tenant).await?)
    }

    async fn list_nodes(&self) -> anyhow::Result<Vec<Node>> {
        let tenant = self.tenant().await?;
        // MCP acts for the tenant, not a single member — the whole fleet.
        Ok(node_queries::list_nodes(&self.state.db, tenant, None).await?)
    }

    async fn list_sessions(&self, active_only: bool) -> anyhow::Result<Vec<Session>> {
        let tenant = self.tenant().await?;
        // MCP acts tenant-wide, not as one member — all sessions (MAIN-133).
        Ok(session_queries::list_sessions(&self.state.db, tenant, None, active_only, None).await?)
    }

    async fn start_session(
        &self,
        workspace: String,
        node: Option<String>,
        runtime: String,
    ) -> anyhow::Result<Session> {
        let tenant = self.tenant().await?;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;

        // Pick the requested node, or any online node with a checkout.
        let detail = workspace_queries::get_workspace(&self.state.db, tenant, workspace_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workspace vanished"))?;
        let location = detail
            .locations
            .iter()
            .filter(|l| l.node_status == "online")
            .find(|l| node.as_deref().is_none_or(|n| l.node_name == n))
            .ok_or_else(|| anyhow::anyhow!("no online node has this workspace checked out"))?;

        let session = session_queries::create_session(
            &self.state,
            tenant,
            None,
            CreateSessionRequest {
                workspace_id,
                node_id: location.node_id,
                runtime,
                name: None,
                path: None,
            },
        )
        .await?;
        Ok(session)
    }

    async fn send_to_session(&self, session_id: String, text: String) -> anyhow::Result<()> {
        use base64::Engine;
        let tenant = self.tenant().await?;
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        let session = session_queries::get_session(&self.state.db, tenant, id).await?;
        let session = session.ok_or_else(|| anyhow::anyhow!("no such session"))?;
        // Ensure the node has a live PTY for this session first — after a node
        // restart the session map is empty and raw input would be dropped.
        // AttachSession is idempotent and re-establishes the PTY from tmux.
        self.state.registry.send_to_node(
            session.node_id,
            ControlToNode::AttachSession {
                session_id: id,
                tmux_session: session.tmux_session.clone(),
            },
        );
        let sent = self.state.registry.send_to_node(
            session.node_id,
            ControlToNode::SessionInput {
                session_id: id,
                data_b64: base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
            },
        );
        if !sent {
            anyhow::bail!("session's node is offline");
        }
        crate::events::record(
            &self.state,
            tenant,
            crate::events::EventDraft::new("session.task_injected").session(id),
        )
        .await;
        Ok(())
    }

    async fn read_session(&self, session_id: String, history_lines: u32) -> anyhow::Result<String> {
        let tenant = self.tenant().await?;
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        let session = session_queries::get_session(&self.state.db, tenant, id).await?;
        let session = session.ok_or_else(|| anyhow::anyhow!("no such session"))?;
        let tmux_session = session
            .tmux_session
            .clone()
            .ok_or_else(|| anyhow::anyhow!("session has no tmux session yet"))?;
        self.run_op(
            session.node_id,
            |request_id| ControlToNode::CaptureSession {
                request_id,
                tmux_session,
                history_lines: history_lines.min(2000),
            },
            10,
        )
        .await
    }

    async fn kill_session(&self, session_id: String) -> anyhow::Result<()> {
        let tenant = self.tenant().await?;
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        let session = session_queries::get_session(&self.state.db, tenant, id).await?;
        let session = session.ok_or_else(|| anyhow::anyhow!("no such session"))?;
        if !self.state.registry.send_to_node(
            session.node_id,
            ControlToNode::KillSession { session_id: id },
        ) {
            anyhow::bail!("session's node is offline");
        }
        crate::events::record(
            &self.state,
            tenant,
            crate::events::EventDraft::new("session.kill_requested")
                .actor("mcp", uuid::Uuid::nil())
                .session(id)
                .node(session.node_id),
        )
        .await;
        Ok(())
    }

    async fn get_activity(
        &self,
        workspace: Option<String>,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let tenant = self.tenant().await?;
        let workspace_id = match workspace {
            Some(w) => Some(self.resolve_workspace(tenant, &w).await?),
            None => None,
        };
        let page = activity_queries::events_page(
            &self.state.db,
            tenant,
            workspace_id,
            None,
            None,
            limit,
            &activity_queries::ActivityScope::All,
        )
        .await?;
        Ok(page.events)
    }

    async fn get_notes(&self, workspace: String) -> anyhow::Result<Vec<Note>> {
        let tenant = self.tenant().await?;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        Ok(notebook_queries::list_notes(&self.state.db, tenant, workspace_id).await?)
    }

    async fn append_note(&self, workspace: String, content: String) -> anyhow::Result<Note> {
        let tenant = self.tenant().await?;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        let existing =
            notebook_queries::latest_rolling_note(&self.state.db, tenant, workspace_id).await?;

        let note = match existing {
            Some(note) => {
                notebook_queries::append_to_note(&self.state.db, note.id, format!("\n{content}"))
                    .await?
            }
            None => {
                notebook_queries::create_note(
                    &self.state.db,
                    tenant,
                    workspace_id,
                    CreateNoteRequest {
                        title: None,
                        content_md: content,
                        kind: Some("rolling".into()),
                    },
                )
                .await?
            }
        };
        Ok(note)
    }

    async fn create_task(
        &self,
        title: String,
        description: Option<String>,
        parent: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let boards = self.state.kanban.all_boards(tenant).await?;
        let board = boards
            .first()
            .ok_or_else(|| anyhow::anyhow!("no boards exist yet"))?;
        let provider = self
            .state
            .kanban
            .get(&board.provider)
            .ok_or_else(|| anyhow::anyhow!("provider missing"))?;
        let creator = self.user().await?;
        let task = provider
            .create_task(
                tenant,
                board.id,
                Some(creator),
                CreateTaskRequest {
                    title,
                    description,
                    column_id: None,
                    column_type: None,
                    workspace_id: None,
                    priority: None,
                    type_: None,
                    // Omitted → `team`, the tenant-visible default (MAIN-76).
                    visibility: None,
                    parent,
                    // Never `agent-ready`: an agent that could label its own
                    // work ready would be approving it, and that gate is the
                    // load-bearing safety property of the whole loop.
                    labels: vec![],
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(task)
    }

    async fn clone_repo(&self, url: String, node: Option<String>) -> anyhow::Result<String> {
        let tenant = self.tenant().await?;
        let node_id = self.resolve_node(tenant, node).await?;
        self.run_op(
            node_id,
            |request_id| ControlToNode::CloneRepo {
                request_id,
                url,
                dest_name: None,
                ssh_key: None,
            },
            90,
        )
        .await
    }

    async fn create_project(&self, name: String, node: Option<String>) -> anyhow::Result<String> {
        let tenant = self.tenant().await?;
        let node_id = self.resolve_node(tenant, node).await?;
        self.run_op(
            node_id,
            |request_id| ControlToNode::InitProject { request_id, name },
            30,
        )
        .await
    }

    async fn add_worktree(
        &self,
        workspace: String,
        branch: String,
        node: Option<String>,
    ) -> anyhow::Result<String> {
        let tenant = self.tenant().await?;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        let node_id = self.resolve_node(tenant, node).await?;
        let repo_path: String =
            workspace_queries::clone_path_on_node(&self.state.db, tenant, workspace_id, node_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("workspace has no checkout on that node"))?;
        self.run_op(
            node_id,
            |request_id| ControlToNode::AddWorktree {
                request_id,
                repo_path,
                branch,
            },
            30,
        )
        .await
    }

    async fn dispatch_task(&self, caller: McpCaller, task_id: String) -> anyhow::Result<TaskItem> {
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        // The authenticated caller is both the viewer (visibility) and the acting
        // person the scheduler places for (MAIN-223 AC-4): auto-placement is
        // confined to nodes that person may use, exactly as on the HTTP route. A
        // static-token call never reaches here — the tool refuses it first.
        Ok(crate::services::taskwork::dispatch(
            &self.state,
            caller.tenant_id,
            caller.user_id,
            Some(caller.user_id),
            id,
        )
        .await?)
    }

    async fn start_work(
        &self,
        caller: McpCaller,
        task_id: String,
        runtime: Option<String>,
        node: Option<String>,
    ) -> anyhow::Result<Session> {
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        let node_id = match node {
            Some(n) => Some(self.resolve_node(caller.tenant_id, Some(n)).await?),
            None => None,
        };
        // Spawn authorization runs against the caller's own/shared nodes (AC-4).
        let (_, session) = crate::services::taskwork::start_work(
            &self.state,
            caller.tenant_id,
            caller.user_id,
            Some(caller.user_id),
            id,
            crate::services::taskwork::StartWork {
                node_id,
                runtime: runtime.unwrap_or_else(|| "bash".into()),
                branch: None,
                workspace_id: None,
            },
        )
        .await?;
        Ok(session)
    }

    async fn move_task(&self, task_id: String, column: String) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        Ok(crate::services::taskwork::move_task(&self.state, tenant, id, &column).await?)
    }

    async fn submit_pr(&self, task_id: String, pr_url: Option<String>) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        Ok(crate::services::taskwork::submit_pr(&self.state, tenant, id, pr_url).await?)
    }

    async fn set_task_description(
        &self,
        task: String,
        description: String,
    ) -> anyhow::Result<TaskItem> {
        use crate::services::kanban::ProviderError;
        let tenant = self.tenant().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        // Through the task's OWN board provider, not a hardwired local one
        // (MAIN-86 AC-2).
        let provider = self.provider_for_task(tenant, id).await?;
        let viewer = self.user().await?;
        // Read-guard-retry: base the write on the version just read, and on a
        // concurrent edit re-read and try again a bounded number of times, so an
        // agent's body edit never silently clobbers a human's change (AC-3).
        for _ in 0..5 {
            let cur =
                crate::services::tasks::get_row(self.state.tasks.as_ref(), tenant, id).await?;
            let Some(cur) = cur else {
                anyhow::bail!("no such task");
            };
            let req = UpdateTaskRequest {
                title: None,
                description: Some(description.clone()),
                column_id: None,
                column_type: None,
                position: None,
                assignee_user_id: None,
                priority: None,
                type_: None,
                visibility: None,
                workspace_id: None,
                parent: None,
                expected_updated_at: Some(cur.updated_at),
            };
            // No parent change here, so the viewer only gates a parent check
            // that never runs — None is safe.
            match provider.update_task(tenant, None, id, req).await {
                Ok(t) => {
                    self.state
                        .registry
                        .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
                    return Ok(crate::services::tasks::enrich_one(
                        self.state.tasks.as_ref(),
                        &self.state.cfg.public_base_url,
                        viewer,
                        t,
                    )
                    .await?);
                }
                Err(ProviderError::Api(crate::error::ApiError::Conflict(_))) => continue,
                Err(ProviderError::Api(e)) => anyhow::bail!("{e}"),
                Err(e) => anyhow::bail!("{e}"),
            }
        }
        anyhow::bail!(
            "the task body kept changing under concurrent edits — read it again and retry"
        )
    }

    // ── The agent loop ──────────────────────────────────────────────────────
    //
    // These delegate to the same services the HTTP routes use rather than
    // reimplementing the queries. Two implementations of "which tasks are
    // pickable" would drift, and the one an agent uses is the one that decides
    // what work happens.

    async fn list_tasks(&self, f: nook_mcp::TaskQuery) -> anyhow::Result<Vec<TaskItem>> {
        let tenant = self.tenant().await?;
        let viewer = self.user().await?;
        let rows = crate::routes::task_query::pick(
            &self.state,
            tenant,
            viewer,
            crate::routes::task_query::TaskFilter {
                board: f.board,
                label: f.label,
                not_label: f.not_label,
                assignee: f.assignee,
                column_type: f.column_type,
                priority: f.priority,
                // Type filtering isn't exposed over MCP's pick (parity with q).
                type_: Vec::new(),
                // Visibility filtering isn't exposed over MCP's pick either.
                visibility: Vec::new(),
                is_blocked: f.is_blocked,
                parent: f.parent,
                workspace: None,
                q: None,
                // Archived work is off the board and never pickable over MCP either.
                archived: None,
                // Backlog excluded by default here too (MAIN-80); opt in via the tool.
                backlog: f.backlog,
                limit: f.limit,
                cursor: None,
            },
        )
        .await?;
        Ok(rows)
    }

    async fn get_task(&self, task: String) -> anyhow::Result<serde_json::Value> {
        let tenant = self.tenant().await?;
        let viewer = self.user().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let detail = crate::routes::task_detail::detail(&self.state, tenant, viewer, id).await?;
        Ok(serde_json::to_value(detail)?)
    }

    async fn claim_task(
        &self,
        task: String,
        column_type: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let user = self.user().await?;
        Ok(
            crate::routes::task_query::claim_inner(&self.state, tenant, user, &task, column_type)
                .await?,
        )
    }

    async fn release_task(&self, task: String) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let viewer = self.user().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let t =
            crate::services::tasks::clear_assignee(self.state.tasks.as_ref(), tenant, id).await?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(crate::services::tasks::enrich_one(
            self.state.tasks.as_ref(),
            &self.state.cfg.public_base_url,
            viewer,
            t,
        )
        .await?)
    }

    async fn comment_task(
        &self,
        task: String,
        body_md: String,
        author_name: Option<String>,
    ) -> anyhow::Result<serde_json::Value> {
        let tenant = self.tenant().await?;
        let user = self.user().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        // `agent` here, not `user`: MCP is the one caller we DO know is a tool
        // rather than a person typing. The author_id remains the real user
        // whose token authorised it, so the record stays honest about both.
        let name = author_name.unwrap_or_else(|| "agent (mcp)".into());
        let row = crate::services::tasks::insert_agent_comment(
            self.state.tasks.as_ref(),
            tenant,
            id,
            user.0,
            &name,
            &body_md,
        )
        .await?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(serde_json::to_value(row)?)
    }

    async fn add_label(&self, task: String, label: String) -> anyhow::Result<serde_json::Value> {
        let tenant = self.tenant().await?;
        // Belt and braces with the tool-layer refusal. A backend that would
        // happily apply `agent-ready` is one bug away from an agent approving
        // its own work, and this is the property the whole design rests on.
        if label.trim().eq_ignore_ascii_case("agent-ready") {
            anyhow::bail!(
                "`agent-ready` is the human approval gate and cannot be applied by an agent"
            );
        }
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let name = label.trim().to_lowercase();
        crate::services::tasks::attach_label(self.state.tasks.as_ref(), tenant, id, &name).await?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(serde_json::json!({ "task": task, "label": name, "added": true }))
    }

    async fn remove_label(&self, task: String, label: String) -> anyhow::Result<serde_json::Value> {
        let tenant = self.tenant().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let name = label.trim().to_lowercase();
        crate::services::tasks::detach_label(self.state.tasks.as_ref(), tenant, id, &name).await?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(serde_json::json!({ "task": task, "label": name, "removed": true }))
    }

    async fn set_priority(&self, task: String, priority: i32) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let viewer = self.user().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let t = crate::services::tasks::set_priority_row(
            self.state.tasks.as_ref(),
            tenant,
            id,
            priority.clamp(0, 4),
        )
        .await?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(crate::services::tasks::enrich_one(
            self.state.tasks.as_ref(),
            &self.state.cfg.public_base_url,
            viewer,
            t,
        )
        .await?)
    }

    async fn set_task_parent(
        &self,
        task: String,
        parent: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        // Through the task's OWN board provider, not a hardwired local one
        // (MAIN-86 AC-1).
        let provider = self.provider_for_task(tenant, id).await?;
        // Through the provider so the epic validation (same board, type=epic,
        // no nesting) applies. The tool always changes the parent: `Some(value)`
        // files under that epic, `None` detaches — both are an explicit change.
        let req = UpdateTaskRequest {
            title: None,
            description: None,
            column_id: None,
            column_type: None,
            position: None,
            assignee_user_id: None,
            priority: None,
            type_: None,
            visibility: None,
            workspace_id: None,
            parent: Some(parent),
            expected_updated_at: None,
        };
        // The caller is the viewer, so a parent that is a private epic they
        // cannot see is refused (MAIN-76/81).
        let viewer = self.user().await?;
        let t = provider
            .update_task(tenant, Some(viewer), id, req)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(crate::services::tasks::enrich_one(
            self.state.tasks.as_ref(),
            &self.state.cfg.public_base_url,
            viewer,
            t,
        )
        .await?)
    }

    async fn link_tasks(
        &self,
        from: String,
        to: String,
        kind: String,
    ) -> anyhow::Result<serde_json::Value> {
        let tenant = self.tenant().await?;
        let viewer = self.user().await?;
        let f =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &from).await?;
        let t = crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &to).await?;
        let row =
            crate::routes::task_detail::link(&self.state, tenant, viewer, f, t, &kind).await?;
        Ok(serde_json::to_value(row)?)
    }

    // ── Notebook (person-scoped; MAIN-102) ──────────────────────────────────
    // These take the caller's own resolved `person` (never the first-user
    // fallback) and route through the notebook module's service paths, so
    // validation, encryption and the sealed-note exclusion all apply here too.

    async fn notebook_list_notes(
        &self,
        person: uuid::Uuid,
        q: Option<String>,
    ) -> anyhow::Result<Vec<UserNoteSummary>> {
        Ok(
            crate::routes::notebook::list_notes_for(
                &self.state,
                person,
                q.as_deref().unwrap_or(""),
            )
            .await?,
        )
    }

    async fn notebook_get_note(
        &self,
        person: uuid::Uuid,
        id: UserNoteId,
    ) -> anyhow::Result<UserNote> {
        Ok(crate::routes::notebook::get_note_for(&self.state, person, id).await?)
    }

    async fn notebook_create_note(
        &self,
        person: uuid::Uuid,
        req: CreateUserNote,
    ) -> anyhow::Result<UserNote> {
        Ok(crate::routes::notebook::create_note_for(&self.state, person, req).await?)
    }

    async fn notebook_update_note(
        &self,
        person: uuid::Uuid,
        id: UserNoteId,
        req: UpdateUserNote,
    ) -> anyhow::Result<UserNote> {
        Ok(crate::routes::notebook::update_note_for(&self.state, person, id, req).await?)
    }

    async fn notebook_delete_note(&self, person: uuid::Uuid, id: UserNoteId) -> anyhow::Result<()> {
        crate::routes::notebook::delete_note_for(&self.state, person, id).await?;
        Ok(())
    }

    async fn notebook_list_folders(
        &self,
        person: uuid::Uuid,
    ) -> anyhow::Result<Vec<UserNoteFolder>> {
        Ok(crate::routes::notebook::list_folders_for(&self.state, person).await?)
    }
}
