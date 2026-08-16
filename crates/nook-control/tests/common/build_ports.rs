//! Fixtures shared by the two build-port-lease suites (MAIN-552).
//!
//! Two suites because they needed different things from the SQLite leg. The
//! ALLOCATION half — this card's own SQL: the widened table, its two conflict
//! targets, the LEFT JOINs, the holder-scoped delete — always ran on both
//! engines. The PLACEMENT half drives `jobs::select_executor`, which reaches
//! `eligible_loop_executors`'s `json_each(…) e` and was allow-listed with the
//! binaries waiting on MAIN-546; that card landed and both halves are covered.
//!
//! Splitting them is what kept AC-9 honest while the exclusion existed:
//! everything this card introduced was exercised on SQLite, and only the part
//! blocked by somebody else's defect was excused.
#![allow(dead_code)]

use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A worktree path of the shape the node's `build_dirname` produces — the only
/// shape a compose project name can be derived from, and so the only one the
/// stack reaper acts on.
pub fn build_worktree(key: &str) -> String {
    format!(
        "/root/.nook/clone-cache/host/worktrees/build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-{key}"
    )
}

pub struct Fixture {
    pub tenant: TenantId,
    pub user: UserId,
    pub person: Uuid,
    pub board: BoardId,
    pub workspace: WorkspaceId,
}

pub async fn fixture(bed: &TestBed, hint: &str) -> Fixture {
    let tenant = bed.tenant(hint).await;
    let (user, person) = bed.user(tenant, "owner").await;
    let workspace = bed.workspace(tenant).await;
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
            // The RANDOM tail of the v7 uuid: two boards made in one test share
            // a timestamp prefix and would collide on a prefix-derived key.
            params![
                board,
                tenant,
                format!("B{}", &board.0.simple().to_string()[26..32]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    Fixture {
        tenant,
        user,
        person,
        board,
        workspace,
    }
}

pub async fn column(bed: &TestBed, f: &Fixture, name: &str, kind: &str, position: i32) -> ColumnId {
    let id = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1,$2,$3,$4,$5)",
            params![id, f.board, name, position, kind],
        )
        .await
        .expect("column");
    id
}

pub async fn card(bed: &TestBed, f: &Fixture, col: ColumnId, number: i32) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by,
                                workspace_id, number)
             VALUES ($1,$2,$3,$4,'card','task',$5,$6,$7)",
            params![id, f.tenant, f.board, col, f.user, f.workspace, number],
        )
        .await
        .expect("task");
    id
}

/// Record the card's build worktree on `node` — what makes it a build tree the
/// reaper will act on, and what says a stack may be up in it.
pub async fn with_worktree(bed: &TestBed, task: TaskId, node: NodeId, key: &str) {
    bed.db()
        .exec(
            "UPDATE tasks SET worktree_path = $2, worktree_node_id = $3 WHERE id = $1",
            params![task, build_worktree(key), node],
        )
        .await
        .expect("worktree");
}

/// Capabilities for a node that may take build work: `claude` authorized, the
/// kind declared, and a port range to lease from.
pub fn build_caps(range: Option<(u16, u16)>) -> serde_json::Value {
    let mut c = serde_json::json!({
        "loop_kinds": ["build"],
        "sandbox": { "state": "ready", "image": "nook-job-sandbox:test" },
        "runtime_auth": [
            { "id": "claude", "label": "Claude Code", "runtime": "claude", "state": "authorized" }
        ]
    });
    if let Some((a, b)) = range {
        c["port_range"] = serde_json::json!([a, b]);
    }
    c
}

/// An online node owned by the fixture's person, wearing `role=build`.
pub async fn build_node(bed: &TestBed, f: &Fixture, range: Option<(u16, u16)>) -> NodeId {
    let id = NodeId::new();
    bed.db()
        .exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, status, owner_person_id,
                                capabilities, labels)
             VALUES ($1,$2,$3,$4,'online',$5,$6,$7)",
            params![
                id,
                f.tenant,
                format!("n-{}", id.0.simple()),
                format!("h-{}", id.0.simple()),
                f.person,
                build_caps(range),
                serde_json::json!({ "role": "build" })
            ],
        )
        .await
        .expect("node");
    id
}

pub async fn queued_build_job(bed: &TestBed, f: &Fixture, task: TaskId) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, workspace_id,
                                    requested_by, state)
             VALUES ($1,$2,'build',$3,$4,$5,'queued')",
            params![id, f.tenant, task, f.workspace, f.user],
        )
        .await
        .expect("build job");
    id
}

/// A build job already CLAIMED by `node`, without going through placement.
///
/// The allocation suite is about what a lease does, not about how a job gets
/// placed, and `select_executor` is what pulls `eligible_loop_executors` — and
/// MAIN-546's SQLite break — into a test that has no interest in either.
pub async fn claimed_build_job(bed: &TestBed, f: &Fixture, task: TaskId, node: NodeId) -> JobId {
    let id = JobId::new();
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, workspace_id,
                                    requested_by, state, executor_node_id)
             VALUES ($1,$2,'build',$3,$4,$5,'claimed',$6)",
            params![id, f.tenant, task, f.workspace, f.user, node],
        )
        .await
        .expect("build job");
    id
}

/// Declare what the workspace binds.
pub async fn declare(bed: &TestBed, f: &Fixture, reqs: &[(&str, &str, bool)]) {
    let value: Vec<PortRequirement> = reqs
        .iter()
        .map(|(name, env, required)| PortRequirement {
            name: (*name).into(),
            env: (*env).into(),
            protocol: "tcp".into(),
            required: *required,
            runtimes: Vec::new(),
            browsable: false,
            path: "/".into(),
        })
        .collect();
    bed.app_state()
        .await
        .workspaces
        .set_port_requirements(
            f.tenant,
            f.workspace,
            Some(serde_json::to_value(&value).unwrap()),
        )
        .await
        .expect("declare");
}

/// What the card holds on a node, as `(env, port)` pairs — the shape the run's
/// environment is written from.
pub async fn held(state: &AppState, node: NodeId, task: TaskId) -> Vec<(String, i32)> {
    state
        .sessions
        .build_leases_of(node, task)
        .await
        .expect("build leases")
        .into_iter()
        .map(|l| (l.env, l.port))
        .collect()
}

/// End a run the way a real one ends: `claimed` is not a terminal's neighbour,
/// so a job reaches `completed` through `running` — which is also the state a
/// dispatched run is actually in.
pub async fn conclude(state: &AppState, tenant: TenantId, job: JobId, ok: bool) {
    nook_control::services::jobs::transition(state, tenant, job, "running")
        .await
        .expect("running");
    nook_control::services::jobs::finish(state, tenant, job, ok, "done")
        .await
        .expect("finish");
}
