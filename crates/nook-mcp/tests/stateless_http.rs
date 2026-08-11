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

use nook_mcp::{McpCaller, NookBackend, TaskQuery};
use nook_types::{
    CreateUserNote, CreateUserNoteFolder, Event, Node, Note, Session, TaskItem, UpdateUserNote,
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
    fn list_workspaces() -> anyhow::Result<Vec<WorkspaceDetail>>;
    fn list_nodes() -> anyhow::Result<Vec<Node>>;
    fn list_sessions(bool) -> anyhow::Result<Vec<Session>>;
    fn start_session(String, Option<String>, String) -> anyhow::Result<Session>;
    fn send_to_session(String, String) -> anyhow::Result<()>;
    fn read_session(String, u32) -> anyhow::Result<String>;
    fn kill_session(String) -> anyhow::Result<()>;
    fn get_activity(Option<String>, i64) -> anyhow::Result<Vec<Event>>;
    fn get_notes(String) -> anyhow::Result<Vec<Note>>;
    fn append_note(String, String) -> anyhow::Result<Note>;
    fn create_task(String, Option<String>, Option<String>) -> anyhow::Result<TaskItem>;
    fn clone_repo(String, Option<String>) -> anyhow::Result<String>;
    fn create_project(String, Option<String>) -> anyhow::Result<String>;
    fn add_worktree(String, String, Option<String>) -> anyhow::Result<String>;
    fn dispatch_task(McpCaller, String) -> anyhow::Result<TaskItem>;
    fn start_work(McpCaller, String, Option<String>, Option<String>) -> anyhow::Result<Session>;
    fn move_task(String, String) -> anyhow::Result<TaskItem>;
    fn submit_pr(String, Option<String>) -> anyhow::Result<TaskItem>;
    fn list_tasks(TaskQuery) -> anyhow::Result<Vec<TaskItem>>;
    fn get_task(String) -> anyhow::Result<Value>;
    fn claim_task(String, Option<String>) -> anyhow::Result<TaskItem>;
    fn release_task(String) -> anyhow::Result<TaskItem>;
    fn comment_task(String, String, Option<String>) -> anyhow::Result<Value>;
    fn set_task_description(String, String) -> anyhow::Result<TaskItem>;
    fn add_label(String, String) -> anyhow::Result<Value>;
    fn remove_label(String, String) -> anyhow::Result<Value>;
    fn set_priority(String, i32) -> anyhow::Result<TaskItem>;
    fn set_task_parent(String, Option<String>) -> anyhow::Result<TaskItem>;
    fn link_tasks(String, String, String) -> anyhow::Result<Value>;
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
/// nothing more. `extra` adds the header a case is actually about.
async fn post(body: Value, extra: &[(&str, &str)]) -> (StatusCode, String) {
    let router = nook_mcp::router(Arc::new(NeverCalled), vec![HOST.to_string()]);
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
