//! The `/mcp` transport answers a bare POST — no `Mcp-Session-Id`, no
//! handshake, no `MCP-Protocol-Version` (MAIN-524). Driven through the real
//! router, because the bug was in how the transport was configured and a test
//! of anything smaller would have passed while the connector still saw 422.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use nook_mcp::{BuildRunQuery, McpCaller, NookBackend, TaskQuery};
use nook_types::{
    AttachmentContent, CreateUserNote, CreateUserNoteFolder, Event, LoopRunLookup, LoopRunSummary,
    Node, Note, Session, TaskAttachment, TaskItem, TunnelView, UpdateUserNote,
    UpdateUserNoteFolder, UserNote, UserNoteFolder, UserNoteFolderId, UserNoteId, UserNoteSummary,
    WorkspaceDetail,
};

const HOST: &str = "nook.example.test";

/// A backend that panics if anything reaches it: none of these requests is a
/// tool call, so every one of them must be answered by the transport and the
/// handler alone.
struct NeverCalled;

macro_rules! never_called {
    ($( fn $name:ident ( $($ty:ty),* ) -> $ret:ty; )*) => {
        #[async_trait]
        impl NookBackend for NeverCalled {
            $( async fn $name(&self $(, _: $ty)*) -> $ret {
                unreachable!(concat!("the backend is not reachable from ", stringify!($name)))
            } )*
        }
    };
}

never_called! {
    fn list_workspaces(McpCaller) -> anyhow::Result<Vec<WorkspaceDetail>>;
    fn list_nodes(McpCaller) -> anyhow::Result<Vec<Node>>;
    fn list_sessions(McpCaller, bool) -> anyhow::Result<Vec<Session>>;
    fn start_session(McpCaller, String, Option<String>, String) -> anyhow::Result<Session>;
    fn send_to_session(McpCaller, String, String) -> anyhow::Result<()>;
    fn read_session(McpCaller, String, u32) -> anyhow::Result<String>;
    fn kill_session(McpCaller, String) -> anyhow::Result<()>;
    fn get_activity(McpCaller, Option<String>, i64) -> anyhow::Result<Vec<Event>>;
    fn get_notes(McpCaller, String) -> anyhow::Result<Vec<Note>>;
    fn append_note(McpCaller, String, String) -> anyhow::Result<Note>;
    fn create_task(McpCaller, String, Option<String>, Option<String>) -> anyhow::Result<TaskItem>;
    fn clone_repo(McpCaller, String, Option<String>) -> anyhow::Result<String>;
    fn create_project(McpCaller, String, Option<String>) -> anyhow::Result<String>;
    fn add_worktree(McpCaller, String, String, Option<String>) -> anyhow::Result<String>;
    fn dispatch_task(McpCaller, String) -> anyhow::Result<TaskItem>;
    fn start_work(McpCaller, String, Option<String>, Option<String>) -> anyhow::Result<Session>;
    fn move_task(McpCaller, String, String) -> anyhow::Result<TaskItem>;
    fn submit_pr(McpCaller, String, Option<String>) -> anyhow::Result<TaskItem>;
    fn list_tasks(McpCaller, TaskQuery) -> anyhow::Result<Vec<TaskItem>>;
    fn get_task(McpCaller, String) -> anyhow::Result<Value>;
    fn claim_task(McpCaller, String, Option<String>) -> anyhow::Result<TaskItem>;
    fn release_task(McpCaller, String) -> anyhow::Result<TaskItem>;
    fn comment_task(McpCaller, String, String, Option<String>, bool) -> anyhow::Result<Value>;
    fn set_task_description(McpCaller, String, String) -> anyhow::Result<TaskItem>;
    fn add_label(McpCaller, String, String) -> anyhow::Result<Value>;
    fn remove_label(McpCaller, String, String) -> anyhow::Result<Value>;
    fn set_priority(McpCaller, String, i32) -> anyhow::Result<TaskItem>;
    fn set_task_parent(McpCaller, String, Option<String>) -> anyhow::Result<TaskItem>;
    fn link_tasks(McpCaller, String, String, String) -> anyhow::Result<Value>;
    fn list_task_attachments(McpCaller, String) -> anyhow::Result<Vec<TaskAttachment>>;
    fn read_task_attachment(McpCaller, String) -> anyhow::Result<AttachmentContent>;
    fn list_build_runs(McpCaller, BuildRunQuery) -> anyhow::Result<Vec<LoopRunSummary>>;
    fn get_build_run(McpCaller, String, u32) -> anyhow::Result<LoopRunLookup>;
    fn open_tunnel(McpCaller, String, u16) -> anyhow::Result<TunnelView>;
    fn list_tunnels(McpCaller) -> anyhow::Result<Vec<TunnelView>>;
    fn stop_tunnel(McpCaller, String) -> anyhow::Result<()>;
    fn notebook_list_notes(Uuid, Option<String>) -> anyhow::Result<Vec<UserNoteSummary>>;
    fn notebook_get_note(Uuid, UserNoteId) -> anyhow::Result<UserNote>;
    fn notebook_create_note(Uuid, CreateUserNote) -> anyhow::Result<UserNote>;
    fn notebook_update_note(Uuid, UserNoteId, UpdateUserNote) -> anyhow::Result<UserNote>;
    fn notebook_delete_note(Uuid, UserNoteId) -> anyhow::Result<()>;
    fn notebook_list_folders(Uuid) -> anyhow::Result<Vec<UserNoteFolder>>;
    fn notebook_create_folder(Uuid, CreateUserNoteFolder) -> anyhow::Result<UserNoteFolder>;
    fn notebook_update_folder(
        Uuid,
        UserNoteFolderId,
        UpdateUserNoteFolder
    ) -> anyhow::Result<UserNoteFolder>;
    fn notebook_delete_folder(Uuid, UserNoteFolderId) -> anyhow::Result<()>;
}

/// POST one JSON-RPC message with the headers a spec-following client sends and
/// nothing more, as a caller whose OIDC token resolved. `extra` adds the header
/// a case is actually about.
async fn post(body: Value, extra: &[(&str, &str)]) -> (StatusCode, String) {
    post_as(Some(a_caller()), body, extra).await
}

/// A resolved MCP identity, as `mcp_auth` inserts one into the request's
/// extensions. `post_as(None, …)` is a request that resolved nobody.
fn a_caller() -> McpCaller {
    McpCaller {
        person_id: Uuid::now_v7(),
        user_id: nook_types::UserId::new(),
        tenant_id: nook_types::TenantId::new(),
    }
}

async fn post_as(
    caller: Option<McpCaller>,
    body: Value,
    extra: &[(&str, &str)],
) -> (StatusCode, String) {
    let router = nook_mcp::router(Arc::new(NeverCalled), vec![HOST.to_string()]);
    // The same place `mcp_auth` puts it: the request's extensions, which the
    // transport forwards into every tool's request context.
    let router = match caller {
        Some(c) => router.layer(axum::Extension(c)),
        None => router,
    };
    let mut req = Request::builder()
        .method("POST")
        .uri("/")
        .header("host", HOST)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    for (name, value) in extra {
        req = req.header(*name, *value);
    }
    let response = router
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    // The transport answers in SSE frames, so the body is only complete once
    // the stream ends; a bound keeps a hung stream a failure, not a hang.
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("the response stream ended")
    .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The single JSON-RPC message out of an SSE body.
fn sse_payload(body: &str) -> Value {
    let data = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or_else(|| panic!("an SSE data frame in: {body}"));
    serde_json::from_str(data).unwrap()
}

/// MAIN-592 AC-5: `tools/list` is filtered per request, so a request that
/// resolved no caller is offered nothing rather than a menu of 47 tools it would
/// be refused on calling. Served, not 422'd or 401'd: the credential is still
/// valid (NG-6), it simply reaches nothing.
#[tokio::test]
async fn tools_list_offers_a_static_token_only_what_it_can_use() {
    let (status, body) = post_as(
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let tools = sse_payload(&body)["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("a tool list in: {body}"))
        .clone();
    assert!(
        tools.is_empty(),
        "every tool on this surface is tenant- or person-scoped, so an \
         unresolved caller is offered none of them, got: {tools:?}"
    );
}

/// MAIN-524 AC-2/AC-6: the exact request the connector sends — no session id,
/// no protocol-version header — and it is served, not 422'd.
#[tokio::test]
async fn tools_list_without_a_session_header_returns_the_tools() {
    let (status, body) = post(
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let tools = sse_payload(&body)["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("a tool list in: {body}"))
        .clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"list_nodes"),
        "the real tool set, got: {names:?}"
    );
}

/// No session is consulted at all, so an id we never issued is not a 404
/// either — which is what makes a client survive a control-plane restart.
#[tokio::test]
async fn tools_list_with_an_unknown_session_id_is_still_served() {
    let (status, body) = post(
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        &[("mcp-session-id", "a-session-that-never-existed")],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(sse_payload(&body)["result"]["tools"].is_array());
}

/// AC-3: a client that does handshake is unaffected, and still learns who it
/// is talking to.
#[tokio::test]
async fn initialize_still_reports_our_server_info() {
    let (status, body) = post(
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0"},
            },
        }),
        &[],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let result = sse_payload(&body)["result"].clone();
    assert!(
        result["serverInfo"]["name"].is_string(),
        "server info: {result}"
    );
    assert!(
        result["instructions"].as_str().unwrap().contains("NookOS"),
        "our instructions: {result}"
    );
    assert!(result["capabilities"]["tools"].is_object(), "{result}");
}

/// AC-5: the MAIN-190 allowlist is untouched, so a Host we do not answer for is
/// still refused before anything else looks at the request.
#[tokio::test]
async fn a_host_outside_the_allowlist_is_still_refused() {
    let router = nook_mcp::router(Arc::new(NeverCalled), vec![HOST.to_string()]);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("host", "attacker.example")
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
