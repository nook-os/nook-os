//! The control plane's implementation of the MCP tool surface. Delegates to
//! the same service layer the REST handlers use.
//!
//! Every tenant-scoped method reads its tenant and its actor from the
//! `McpCaller` the request resolved to, and nothing else. Two helpers here
//! used to answer both questions from the INSTANCE instead — the first tenant
//! and its owner — which served that tenant's board to every caller and
//! attributed every MCP write to a person who had not made it (MAIN-592).
//! Nothing in this file may reach `identity::first_tenant` or
//! `identity::first_user` again; `tests/mcp_tenant_isolation.rs` fails the
//! build if it does.

use async_trait::async_trait;
use nook_mcp::{McpCaller, NookBackend};
use nook_proto::ControlToNode;
use nook_types::*;

use crate::services::{activity_queries, notebook_queries, session_queries, workspace_queries};
use crate::state::AppState;

pub struct McpBackend {
    pub state: AppState,
}

impl McpBackend {
    /// Resolve a workspace by **id or slug** (both unique), falling back to name
    /// as a documented convenience that errors on ambiguity rather than silently
    /// picking one (MAIN-223 AC-3). The old `slug = $2 OR name = $2` conflated the
    /// two and returned an arbitrary row when a name matched several workspaces.
    pub async fn resolve_workspace(
        &self,
        tenant: TenantId,
        key: &str,
    ) -> anyhow::Result<WorkspaceId> {
        workspace_queries::resolve_by_key(&*self.state.workspaces, tenant, key).await
    }

    /// Resolve a node by name, or auto-pick an online node when omitted.
    async fn resolve_node(&self, tenant: TenantId, name: Option<String>) -> anyhow::Result<NodeId> {
        let nodes = self.state.nodes.list_ids_and_names(tenant).await?;
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
    async fn list_workspaces(&self, caller: McpCaller) -> anyhow::Result<Vec<WorkspaceDetail>> {
        let tenant = caller.tenant_id;
        Ok(workspace_queries::list_workspaces(&*self.state.workspaces, tenant).await?)
    }

    async fn list_nodes(&self, caller: McpCaller) -> anyhow::Result<Vec<Node>> {
        let tenant = caller.tenant_id;
        // The caller's whole fleet, not just the machines they own: an agent
        // asked "where can this run" wants every node its tenant has. The
        // narrowing that matters is the tenant, and that now comes from the
        // caller (MAIN-592) rather than from the instance.
        Ok(self.state.nodes.list(tenant, None, None).await?)
    }

    async fn list_sessions(
        &self,
        caller: McpCaller,
        active_only: bool,
    ) -> anyhow::Result<Vec<Session>> {
        let tenant = caller.tenant_id;
        // MCP acts tenant-wide, not as one member — all sessions (MAIN-133).
        Ok(session_queries::list_sessions(
            &*self.state.sessions,
            &*self.state.workspaces,
            tenant,
            None,
            active_only,
            None,
        )
        .await?)
    }

    async fn start_session(
        &self,
        caller: McpCaller,
        workspace: String,
        node: Option<String>,
        runtime: String,
    ) -> anyhow::Result<Session> {
        let tenant = caller.tenant_id;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;

        // Pick the requested node, or any online node with a checkout.
        let detail =
            workspace_queries::get_workspace(&*self.state.workspaces, tenant, workspace_id)
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
                // MCP opens terminals (MAIN-502 AC-7): the tool's contract is
                // unchanged, and a chat has no MCP-facing surface yet.
                interface: nook_types::SessionInterface::Terminal,
            },
        )
        .await?;
        Ok(session)
    }

    async fn send_to_session(
        &self,
        caller: McpCaller,
        session_id: String,
        text: String,
    ) -> anyhow::Result<()> {
        use base64::Engine;
        let tenant = caller.tenant_id;
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        let session = session_queries::get_session(&*self.state.sessions, tenant, id).await?;
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

    async fn read_session(
        &self,
        caller: McpCaller,
        session_id: String,
        history_lines: u32,
    ) -> anyhow::Result<String> {
        let tenant = caller.tenant_id;
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        let session = session_queries::get_session(&*self.state.sessions, tenant, id).await?;
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

    async fn kill_session(&self, caller: McpCaller, session_id: String) -> anyhow::Result<()> {
        let tenant = caller.tenant_id;
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        let session = session_queries::get_session(&*self.state.sessions, tenant, id).await?;
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
        caller: McpCaller,
        workspace: Option<String>,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let tenant = caller.tenant_id;
        let workspace_id = match workspace {
            Some(w) => Some(self.resolve_workspace(tenant, &w).await?),
            None => None,
        };
        let page = activity_queries::events_page(
            &*self.state.read_model,
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

    async fn get_notes(&self, caller: McpCaller, workspace: String) -> anyhow::Result<Vec<Note>> {
        let tenant = caller.tenant_id;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        Ok(notebook_queries::list_notes(&*self.state.notebook, tenant, workspace_id).await?)
    }

    async fn append_note(
        &self,
        caller: McpCaller,
        workspace: String,
        content: String,
    ) -> anyhow::Result<Note> {
        let tenant = caller.tenant_id;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        let existing =
            notebook_queries::latest_rolling_note(&*self.state.notebook, tenant, workspace_id)
                .await?;

        let note = match existing {
            Some(note) => {
                notebook_queries::append_to_note(
                    &*self.state.notebook,
                    note.id,
                    format!("\n{content}"),
                )
                .await?
            }
            None => {
                notebook_queries::create_note(
                    &*self.state.notebook,
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
        caller: McpCaller,
        title: String,
        description: Option<String>,
        parent: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
        let boards = self.state.kanban.all_boards(tenant).await?;
        let board = boards
            .first()
            .ok_or_else(|| anyhow::anyhow!("no boards exist yet"))?;
        let provider = self
            .state
            .kanban
            .get(&board.provider)
            .ok_or_else(|| anyhow::anyhow!("provider missing"))?;
        let creator = caller.user_id;
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

    async fn clone_repo(
        &self,
        caller: McpCaller,
        url: String,
        node: Option<String>,
    ) -> anyhow::Result<String> {
        let tenant = caller.tenant_id;
        let node_id = self.resolve_node(tenant, node).await?;
        let tenant_slug = crate::services::tenant_slug(&self.state, tenant).await;
        self.run_op(
            node_id,
            |request_id| ControlToNode::CloneRepo {
                request_id,
                url,
                dest_name: None,
                ssh_key: None,
                tenant_slug,
            },
            90,
        )
        .await
    }

    async fn create_project(
        &self,
        caller: McpCaller,
        name: String,
        node: Option<String>,
    ) -> anyhow::Result<String> {
        let tenant = caller.tenant_id;
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
        caller: McpCaller,
        workspace: String,
        branch: String,
        node: Option<String>,
    ) -> anyhow::Result<String> {
        let tenant = caller.tenant_id;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        let node_id = self.resolve_node(tenant, node).await?;
        let repo_path: String = workspace_queries::clone_path_on_node(
            &*self.state.workspaces,
            tenant,
            workspace_id,
            node_id,
        )
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

    async fn move_task(
        &self,
        caller: McpCaller,
        task_id: String,
        column: String,
    ) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        Ok(crate::services::taskwork::move_task(&self.state, tenant, id, &column).await?)
    }

    async fn submit_pr(
        &self,
        caller: McpCaller,
        task_id: String,
        pr_url: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        Ok(crate::services::taskwork::submit_pr(&self.state, tenant, id, pr_url).await?)
    }

    async fn set_task_description(
        &self,
        caller: McpCaller,
        task: String,
        description: String,
    ) -> anyhow::Result<TaskItem> {
        use crate::services::kanban::ProviderError;
        let tenant = caller.tenant_id;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        // Through the task's OWN board provider, not a hardwired local one
        // (MAIN-86 AC-2).
        let provider = self.provider_for_task(tenant, id).await?;
        let viewer = caller.user_id;
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

    async fn list_tasks(
        &self,
        caller: McpCaller,
        f: nook_mcp::TaskQuery,
    ) -> anyhow::Result<Vec<TaskItem>> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
        let rows = crate::routes::task_query::pick(
            &self.state,
            tenant,
            viewer,
            crate::routes::task_query::TaskFilter {
                // MCP is a person's surface, not a machine's: no node narrowing.
                node: None,
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
                // And finished work (MAIN-464), same shape.
                done: f.done,
                limit: f.limit,
                cursor: None,
            },
        )
        .await?;
        Ok(rows)
    }

    async fn get_task(&self, caller: McpCaller, task: String) -> anyhow::Result<serde_json::Value> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let detail = crate::routes::task_detail::detail(&self.state, tenant, viewer, id).await?;
        Ok(serde_json::to_value(detail)?)
    }

    async fn list_task_attachments(
        &self,
        caller: McpCaller,
        task: String,
    ) -> anyhow::Result<Vec<TaskAttachment>> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
        Ok(
            crate::services::attachments::list_thread_readable(&self.state, tenant, viewer, &task)
                .await?,
        )
    }

    async fn read_task_attachment(
        &self,
        caller: McpCaller,
        attachment: String,
    ) -> anyhow::Result<AttachmentContent> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
        // Parsed here rather than let the repository match nothing: an id that
        // is not a uuid is a caller mistake and deserves to say so, while a
        // well-formed id that finds nothing is the 404 AC-4 is about.
        let id: uuid::Uuid = attachment
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("not an attachment id: {attachment}"))?;
        Ok(crate::services::attachments::read_content(&self.state, tenant, viewer, id).await?)
    }

    async fn claim_task(
        &self,
        caller: McpCaller,
        task: String,
        column_type: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
        let user = caller.user_id;
        Ok(
            crate::routes::task_query::claim_inner(&self.state, tenant, user, &task, column_type)
                .await?,
        )
    }

    async fn release_task(&self, caller: McpCaller, task: String) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
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
        caller: McpCaller,
        task: String,
        body_md: String,
        author_name: Option<String>,
        clear_escalation: bool,
    ) -> anyhow::Result<serde_json::Value> {
        let tenant = caller.tenant_id;
        let user = caller.user_id;
        // The same refusal the REST door makes, from the same string (MAIN-584
        // AC-5), and before anything is written. Only when unblocking: an
        // ordinary MCP comment keeps whatever it did before (NG-4).
        if clear_escalation && body_md.trim().is_empty() {
            anyhow::bail!(crate::routes::task_detail::UNBLOCK_NEEDS_A_RULING);
        }
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
        // AC-11: this door published only a UI refresh, so nothing it said ever
        // reached the activity feed. It is also the door where an agent can
        // clear its OWN stop, which is precisely the write a human must be able
        // to audit afterwards.
        crate::services::tasks::record_comment_created(
            &self.state,
            tenant,
            id,
            user,
            &name,
            &body_md,
        )
        .await?;
        if clear_escalation {
            crate::services::tasks::unblock(&self.state, tenant, user, id).await?;
        }
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(serde_json::to_value(row)?)
    }

    async fn add_label(
        &self,
        caller: McpCaller,
        task: String,
        label: String,
    ) -> anyhow::Result<serde_json::Value> {
        let tenant = caller.tenant_id;
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

    async fn remove_label(
        &self,
        caller: McpCaller,
        task: String,
        label: String,
    ) -> anyhow::Result<serde_json::Value> {
        let tenant = caller.tenant_id;
        let id =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &task).await?;
        let name = label.trim().to_lowercase();
        crate::services::tasks::detach_label(&self.state, tenant, id, &name).await?;
        self.state
            .registry
            .publish(tenant, nook_proto::UiEvent::TaskChanged { task_id: id });
        Ok(serde_json::json!({ "task": task, "label": name, "removed": true }))
    }

    async fn set_priority(
        &self,
        caller: McpCaller,
        task: String,
        priority: i32,
    ) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
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
        caller: McpCaller,
        task: String,
        parent: Option<String>,
    ) -> anyhow::Result<TaskItem> {
        let tenant = caller.tenant_id;
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
        let viewer = caller.user_id;
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
        caller: McpCaller,
        from: String,
        to: String,
        kind: String,
    ) -> anyhow::Result<serde_json::Value> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;
        let f =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &from).await?;
        let t = crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &to).await?;
        let row =
            crate::routes::task_detail::link(&self.state, tenant, viewer, f, t, &kind).await?;
        Ok(serde_json::to_value(row)?)
    }

    // ── Build runs (MAIN-525) ───────────────────────────────────────────────
    // Scoped by the CALLER's tenant, not the instance's first one: a run's
    // transcript quotes source and credentials-adjacent output, so "which
    // tenant is this" has to be answered by the authenticated identity rather
    // than by a fallback (AC-5). Both tools refuse the static-token path
    // before they get here — `require_caller` has nothing to hand them.

    async fn list_build_runs(
        &self,
        caller: McpCaller,
        q: nook_mcp::BuildRunQuery,
    ) -> anyhow::Result<Vec<LoopRunSummary>> {
        let tenant = caller.tenant_id;
        let workspace = self.resolve_workspace(tenant, &q.workspace).await?;
        let limit = q.limit.unwrap_or(RUNS_PAGE_DEFAULT).clamp(1, RUNS_PAGE_MAX);
        // No `kind` means builds — the question this surface exists to answer.
        // `any` is the deliberate widening to review and spec runs.
        let kind = match q.kind.as_deref().map(str::trim) {
            None | Some("") => Some(crate::services::jobs::BUILD_KIND),
            Some("any") => None,
            Some(k) => Some(k),
        };
        let mut runs = self
            .state
            .jobs
            .list_runs_for_workspace(tenant, caller.user_id, workspace, kind, q.live_only, limit)
            .await?;
        for run in &mut runs {
            run.elapsed_seconds = elapsed_seconds(run);
        }
        Ok(runs)
    }

    async fn get_build_run(
        &self,
        caller: McpCaller,
        run: String,
        tail_lines: u32,
    ) -> anyhow::Result<LoopRunLookup> {
        let tenant = caller.tenant_id;
        let viewer = caller.user_id;

        // A uuid is a RUN id first, because that is what the list hands back.
        // Falling back to a card is not a guess: a uuid that is not a run may
        // still be a card, and answering "nothing has run this yet" beats a
        // not-found for a card that plainly exists.
        if let Ok(id) = run.parse::<JobId>() {
            if self.state.jobs.get(tenant, id).await?.is_some() {
                let detail = crate::services::jobs::get(&self.state, tenant, viewer, id).await?;
                let found = self.run_detail(tenant, detail, tail_lines).await?;
                return Ok(LoopRunLookup {
                    queried: run,
                    task_key: found.run.task_key.clone(),
                    summary: run_summary_line(&found),
                    run: Some(found),
                });
            }
        }

        // Newest first (`list_for_task` orders by the time-ordered v7 id), so
        // the card's latest run is the one a "how is it going" is asking about.
        // It also carries the visibility gate: a card this viewer may not see
        // is a not-found here, never an empty answer that admits it exists.
        let task =
            crate::services::tasks::resolve_id(self.state.tasks.as_ref(), tenant, &run).await?;
        let runs = crate::services::jobs::list_for_task(&self.state, tenant, viewer, task).await?;
        let key = self.state.tasks.key_of(tenant, task).await?;
        let named = key.clone().unwrap_or_else(|| run.clone());

        let Some(job) = runs.into_iter().next() else {
            // AC-4: an ordinary empty answer naming the card. Nothing having
            // built a card is a legitimate reply, and the common one.
            return Ok(LoopRunLookup {
                queried: run,
                task_key: key,
                run: None,
                summary: format!("{named}: nothing has run this card yet"),
            });
        };
        let detail = crate::services::jobs::get(&self.state, tenant, viewer, job.id).await?;
        let found = self.run_detail(tenant, detail, tail_lines).await?;
        Ok(LoopRunLookup {
            queried: run,
            task_key: key,
            summary: run_summary_line(&found),
            run: Some(found),
        })
    }

    // ── Tunnels (MAIN-11) ───────────────────────────────────────────────────
    // Every one of these calls `routes::tunnels`, so the naming, the collision
    // handling, the `TUNNEL_DOMAIN` requirement and the node gate are the ones
    // the CLI already goes through (AC-4). Scoped by the CALLER's tenant, never
    // the instance's first one — a tunnel publishes a running app, so which
    // tenant may open one has to come from the authenticated identity (NG-3).

    async fn open_tunnel(
        &self,
        caller: McpCaller,
        session_id: String,
        port: u16,
    ) -> anyhow::Result<TunnelView> {
        let id: SessionId = session_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad session id"))?;
        // The machine is the SESSION's, exactly as `nook tunnel` reads it from
        // node.toml: an MCP caller is not in the session, so naming the node
        // again would only be a second chance to name the wrong one. A session
        // in another tenant is not found here, before anything is opened.
        let session = self
            .state
            .sessions
            .by_id_unscoped(id)
            .await?
            .filter(|s| s.tenant_id == caller.tenant_id)
            .ok_or(crate::error::ApiError::NotFound)?;
        Ok(crate::routes::tunnels::open(
            &self.state,
            auth_ctx(&caller),
            CreateTunnelRequest {
                port,
                node_id: Some(session.node_id),
                session_id: Some(session.id),
            },
        )
        .await?)
    }

    async fn list_tunnels(&self, caller: McpCaller) -> anyhow::Result<Vec<TunnelView>> {
        Ok(crate::routes::tunnels::live(
            &self.state,
            auth_ctx(&caller),
        )?)
    }

    async fn stop_tunnel(&self, caller: McpCaller, label: String) -> anyhow::Result<()> {
        Ok(crate::routes::tunnels::close(&self.state, auth_ctx(&caller), &label).await?)
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

    async fn notebook_create_folder(
        &self,
        person: uuid::Uuid,
        req: CreateUserNoteFolder,
    ) -> anyhow::Result<UserNoteFolder> {
        Ok(crate::routes::notebook::create_folder_for(&self.state, person, req).await?)
    }

    async fn notebook_update_folder(
        &self,
        person: uuid::Uuid,
        id: UserNoteFolderId,
        req: UpdateUserNoteFolder,
    ) -> anyhow::Result<UserNoteFolder> {
        Ok(crate::routes::notebook::update_folder_for(&self.state, person, id, req).await?)
    }

    async fn notebook_delete_folder(
        &self,
        person: uuid::Uuid,
        id: UserNoteFolderId,
    ) -> anyhow::Result<()> {
        Ok(crate::routes::notebook::delete_folder_for(&self.state, person, id).await?)
    }
}

/// The MCP caller as the `AuthCtx` the control plane's own gates take, so a tool
/// can call a route's logic instead of restating its rules (MAIN-11 AC-4).
///
/// An OIDC bearer is a person with no browser session behind it — the same
/// shape `nook_user_` tokens already resolve to, which is why the principal is
/// a user, `cookie_session` is false, and the session id is nil rather than
/// invented.
fn auth_ctx(caller: &McpCaller) -> crate::auth::AuthCtx {
    crate::auth::AuthCtx {
        session_id: AuthSessionId(uuid::Uuid::nil()),
        user_id: caller.user_id,
        tenant_id: caller.tenant_id,
        principal: crate::auth::Principal::User,
        cookie_session: false,
    }
}

// ── The build-run status read model (MAIN-525) ───────────────────────────────

/// Newest-first page size when the caller does not say, and the ceiling when
/// they do. A repo that is built all day accumulates a run per card per push,
/// and none of the older ones tell a chat client anything the newest does not.
const RUNS_PAGE_DEFAULT: i64 = 20;
const RUNS_PAGE_MAX: i64 = 100;

/// Where "give me more transcript" stops (AC-3). The bound is the whole point:
/// a run that narrates for an hour must not be able to return a payload that
/// swamps the client that asked.
const MAX_TAIL_LINES: u32 = 2_000;

impl McpBackend {
    /// A run plus the joins the wire shape needs: its card's key, its
    /// executor's name, its repo, the PR, and a bounded transcript tail.
    /// Visibility was decided upstream — `jobs::get` refuses a run whose card
    /// the viewer may not see — so nothing here re-checks it.
    async fn run_detail(
        &self,
        tenant: TenantId,
        detail: LoopJobDetail,
        tail_lines: u32,
    ) -> anyhow::Result<LoopRunDetail> {
        let job = detail.job;
        let card = match job.target_task_id {
            Some(t) => self.state.tasks.get_row(tenant, t).await?,
            None => None,
        };
        let task_key = match job.target_task_id {
            Some(t) => self.state.tasks.key_of(tenant, t).await?,
            None => None,
        };
        let executor_node = match job.executor_node_id {
            Some(n) => self.state.nodes.name_of(n).await?,
            None => None,
        };
        let workspace = match job.workspace_id {
            Some(w) => self.state.workspaces.get(tenant, w).await?,
            None => None,
        };
        // A build run's PR is the one recorded on its card; a review run's is
        // the one it was raised for, which lives on the job as a number and
        // becomes a URL through the workspace's own remote.
        let pr_url = card.as_ref().and_then(|c| c.pr_url.clone()).or_else(|| {
            let pr = job.review_pr_number?;
            let repo = workspace
                .as_ref()?
                .git_remote_url
                .as_deref()
                .and_then(crate::services::forge::github_repo)?;
            Some(format!(
                "https://github.com/{}/{}/pull/{pr}",
                repo.owner, repo.name
            ))
        });

        let mut run = LoopRunSummary {
            id: job.id,
            kind: job.kind,
            state: job.state,
            task_key,
            executor_node,
            started_at: job.created_at,
            updated_at: job.updated_at,
            elapsed_seconds: 0,
        };
        run.elapsed_seconds = elapsed_seconds(&run);
        Ok(LoopRunDetail {
            run,
            workspace: workspace.map(|w| w.name),
            pr_url,
            outcome: job.build_outcome.or(job.review_verdict),
            transcript: tail_transcript(detail.transcript, tail_lines),
        })
    }
}

/// How long a run has been going, or how long it took. A live run is measured
/// against now; a finished one against its last lifecycle movement, which is
/// when it finished.
fn elapsed_seconds(run: &LoopRunSummary) -> i64 {
    let end = if crate::services::jobs::is_terminal(&run.state) {
        run.updated_at
    } else {
        chrono::Utc::now()
    };
    (end - run.started_at).num_seconds().max(0)
}

/// Lines one transcript entry occupies. An empty entry still occupies one — a
/// budget that counted it as free could return unboundedly many of them.
fn entry_lines(entry: &LoopJobTranscriptEntry) -> u32 {
    entry.content.lines().count().max(1) as u32
}

/// The tail of a transcript within a LINE budget (AC-3), and the truth about
/// what it left out.
///
/// Whole entries, newest first, while they fit: half an entry is a mangled
/// answer, and the counts below say plainly that there was more. The one
/// exception is a single entry larger than the entire budget, which is trimmed
/// to its last lines rather than dropped — a noisy run must not answer "how is
/// it going" with nothing at all.
fn tail_transcript(
    mut entries: Vec<LoopJobTranscriptEntry>,
    tail_lines: u32,
) -> LoopRunTranscriptTail {
    let budget = tail_lines.clamp(1, MAX_TAIL_LINES);
    let total_lines: u32 = entries.iter().map(entry_lines).sum();
    let total = entries.len();

    let mut first_kept = total;
    let mut lines = 0u32;
    while first_kept > 0 {
        let n = entry_lines(&entries[first_kept - 1]);
        if lines + n > budget {
            break;
        }
        lines += n;
        first_kept -= 1;
    }

    if total > 0 && first_kept == total {
        entries.drain(..total - 1);
        let content = std::mem::take(&mut entries[0].content);
        let kept: Vec<&str> = content.lines().rev().take(budget as usize).collect();
        entries[0].content = kept.into_iter().rev().collect::<Vec<_>>().join("\n");
        lines = budget;
    } else {
        entries.drain(..first_kept);
    }

    let truncated = lines < total_lines;
    LoopRunTranscriptTail {
        entries,
        lines,
        total_lines,
        truncated,
        note: truncated.then(|| {
            format!(
                "showing the last {lines} of {total_lines} transcript lines — \
                 ask again with a larger tail_lines for more"
            )
        }),
    }
}

/// One sentence a model can relay instead of re-deriving it from the fields.
fn run_summary_line(d: &LoopRunDetail) -> String {
    let who = d
        .run
        .task_key
        .clone()
        .unwrap_or_else(|| d.run.id.to_string());
    let node = d
        .run
        .executor_node
        .as_deref()
        .map(|n| format!(" on {n}"))
        .unwrap_or_default();
    let outcome = d
        .outcome
        .as_deref()
        .map(|o| format!(", concluded {o}"))
        .unwrap_or_default();
    let pr = d
        .pr_url
        .as_deref()
        .map(|u| format!(" — {u}"))
        .unwrap_or_default();
    format!(
        "{who}: {} run {}{node}, {} elapsed{outcome}{pr}",
        d.run.kind,
        d.run.state,
        human_elapsed(d.run.elapsed_seconds),
    )
}

fn human_elapsed(secs: i64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}
