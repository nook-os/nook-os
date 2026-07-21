//! The control plane's implementation of the MCP tool surface. Delegates to
//! the same service layer the REST handlers use.

use async_trait::async_trait;
use nook_mcp::NookBackend;
use nook_proto::ControlToNode;
use nook_types::*;

use crate::services::core;
use crate::state::AppState;

pub struct McpBackend {
    pub state: AppState,
}

impl McpBackend {
    /// M1: the MCP token maps to the instance's first tenant (dev). Per-user
    /// MCP OAuth is post-M1.
    async fn tenant(&self) -> anyhow::Result<TenantId> {
        let (id,): (TenantId,) =
            sqlx::query_as("SELECT id FROM tenants ORDER BY created_at LIMIT 1")
                .fetch_one(&self.state.db)
                .await?;
        Ok(id)
    }

    async fn resolve_workspace(
        &self,
        tenant: TenantId,
        name_or_slug: &str,
    ) -> anyhow::Result<WorkspaceId> {
        let row: Option<(WorkspaceId,)> = sqlx::query_as(
            "SELECT id FROM workspaces WHERE tenant_id = $1 AND (slug = $2 OR name = $2)",
        )
        .bind(tenant)
        .bind(name_or_slug)
        .fetch_optional(&self.state.db)
        .await?;
        row.map(|(id,)| id)
            .ok_or_else(|| anyhow::anyhow!("no workspace named '{name_or_slug}'"))
    }

    /// Resolve a node by name, or auto-pick an online node when omitted.
    async fn resolve_node(&self, tenant: TenantId, name: Option<String>) -> anyhow::Result<NodeId> {
        let nodes: Vec<(NodeId, String)> =
            sqlx::query_as("SELECT id, name FROM nodes WHERE tenant_id = $1")
                .bind(tenant)
                .fetch_all(&self.state.db)
                .await?;
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
}

#[async_trait]
impl NookBackend for McpBackend {
    async fn list_workspaces(&self) -> anyhow::Result<Vec<WorkspaceDetail>> {
        let tenant = self.tenant().await?;
        Ok(core::list_workspaces(&self.state.db, tenant).await?)
    }

    async fn list_nodes(&self) -> anyhow::Result<Vec<Node>> {
        let tenant = self.tenant().await?;
        Ok(core::list_nodes(&self.state.db, tenant).await?)
    }

    async fn list_sessions(&self, active_only: bool) -> anyhow::Result<Vec<Session>> {
        let tenant = self.tenant().await?;
        Ok(core::list_sessions(&self.state.db, tenant, None, active_only).await?)
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
        let detail = core::get_workspace(&self.state.db, tenant, workspace_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workspace vanished"))?;
        let location = detail
            .locations
            .iter()
            .filter(|l| l.node_status == "online")
            .find(|l| node.as_deref().is_none_or(|n| l.node_name == n))
            .ok_or_else(|| anyhow::anyhow!("no online node has this workspace checked out"))?;

        let session = core::create_session(
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
        let session: Option<Session> =
            sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant)
                .fetch_optional(&self.state.db)
                .await?;
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
        let session: Option<Session> =
            sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant)
                .fetch_optional(&self.state.db)
                .await?;
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
        let session: Option<Session> =
            sqlx::query_as("SELECT * FROM sessions WHERE id = $1 AND tenant_id = $2")
                .bind(id)
                .bind(tenant)
                .fetch_optional(&self.state.db)
                .await?;
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
        let page =
            core::events_page(&self.state.db, tenant, workspace_id, None, None, limit).await?;
        Ok(page.events)
    }

    async fn get_notes(&self, workspace: String) -> anyhow::Result<Vec<Note>> {
        let tenant = self.tenant().await?;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        Ok(core::list_notes(&self.state.db, tenant, workspace_id).await?)
    }

    async fn append_note(&self, workspace: String, content: String) -> anyhow::Result<Note> {
        let tenant = self.tenant().await?;
        let workspace_id = self.resolve_workspace(tenant, &workspace).await?;
        let existing: Option<Note> = sqlx::query_as(
            "SELECT * FROM notes WHERE tenant_id = $1 AND workspace_id = $2 AND kind = 'rolling'
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(tenant)
        .bind(workspace_id)
        .fetch_optional(&self.state.db)
        .await?;

        let note = match existing {
            Some(note) => {
                sqlx::query_as(
                    "UPDATE notes SET content_md = content_md || $2, updated_at = now()
                     WHERE id = $1 RETURNING *",
                )
                .bind(note.id)
                .bind(format!("\n{content}"))
                .fetch_one(&self.state.db)
                .await?
            }
            None => {
                core::create_note(
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
        let task = provider
            .create_task(
                tenant,
                board.id,
                CreateTaskRequest {
                    title,
                    description,
                    column_id: None,
                    workspace_id: None,
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
        let (repo_path,): (String,) = sqlx::query_as(
            "SELECT path FROM node_workspaces
             WHERE tenant_id = $1 AND workspace_id = $2 AND node_id = $3
             ORDER BY discovered_at LIMIT 1",
        )
        .bind(tenant)
        .bind(workspace_id)
        .bind(node_id)
        .fetch_optional(&self.state.db)
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

    async fn dispatch_task(&self, task_id: String) -> anyhow::Result<TaskItem> {
        let tenant = self.tenant().await?;
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        Ok(crate::services::taskwork::dispatch(&self.state, tenant, id).await?)
    }

    async fn start_work(
        &self,
        task_id: String,
        runtime: Option<String>,
        node: Option<String>,
    ) -> anyhow::Result<Session> {
        let tenant = self.tenant().await?;
        let id: TaskId = task_id
            .parse()
            .map_err(|_| anyhow::anyhow!("bad task id"))?;
        let node_id = match node {
            Some(n) => Some(self.resolve_node(tenant, Some(n)).await?),
            None => None,
        };
        let (_, session) = crate::services::taskwork::start_work(
            &self.state,
            tenant,
            None,
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
}
