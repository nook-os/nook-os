//! A build run leases the ports it binds — the ALLOCATION half (MAIN-552).
//!
//! Every test runs on a private `nook_testkit::TestBed`, and this suite is
//! covered on BOTH engines: it drives `port_leases::lease_for_build` directly
//! and never `jobs::select_executor`, so everything this card introduced — the
//! widened table, its two conflict targets, the holder-scoped reads and
//! deletes, the reclaim's blindness to a build — is exercised on SQLite too.
//! The placement gate lives in `build_port_leases_placement`, which is
//! allow-listed behind MAIN-546.
//!
//! The one that matters most is [`the_non_live_session_sweep_leaves_a_live_builds_lease_alone`]:
//! the allocator's first step drops the rows of non-live sessions on the node,
//! and a build's stack outlives its run by design. If a future change makes
//! that sweep reach a build's lease, this card has silently undone itself and
//! no other test in the suite would notice.

mod common;

use std::sync::Mutex;

use async_trait::async_trait;
use common::build_ports::*;
use nook_control::services::stack_reaper::{self, NodeOps};
use nook_control::services::{jobs, port_leases};
use nook_control::state::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Take the ports a build's stack binds, the way placement does.
async fn lease(
    state: &AppState,
    f: &Fixture,
    node: NodeId,
    task: TaskId,
) -> Result<port_leases::Leased, port_leases::Refusal> {
    port_leases::lease_for_build(state, f.tenant, node, Some(f.workspace), task)
        .await
        .expect("the allocator answers")
}

/// A node that answers as instructed, so the reaper's decision is testable
/// without a docker daemon.
struct Double {
    down: Result<Option<String>, String>,
    asked: Mutex<Vec<String>>,
}

impl Double {
    fn new(down: Result<Option<String>, String>) -> Self {
        Self {
            down,
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl NodeOps for Double {
    async fn stack_down(
        &self,
        _node: NodeId,
        projects: &[String],
    ) -> Result<Option<String>, String> {
        self.asked.lock().unwrap().push(projects.join(","));
        self.down.clone()
    }

    async fn remove_worktree(&self, _node: NodeId, _path: &str) -> Result<String, String> {
        Ok("removed worktree (2.4 GiB reclaimed)".into())
    }
}

/// AC-1: a build leases what its WORKSPACE declares, by name and by the env var
/// the workspace chose — no build-specific list anywhere.
#[tokio::test]
async fn a_build_leases_its_workspaces_declaration() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-declare").await;
    declare(
        &bed,
        &f,
        &[
            ("web", "NOOK_WEB_PORT", true),
            ("api", "NOOK_API_PORT", true),
        ],
    )
    .await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");

    assert_eq!(
        held(&state, node, task).await,
        vec![
            ("NOOK_API_PORT".to_string(), 4201),
            ("NOOK_WEB_PORT".to_string(), 4200),
        ],
        "one lease per declared listener, under the workspace's own variable names"
    );

    bed.teardown().await;
}

/// AC-3, first half: the run ending frees nothing. A build's stack outlives its
/// run by design (MAIN-480), and a port handed back while containers are still
/// bound to it is worse than one never leased.
#[tokio::test]
async fn the_lease_survives_the_run_ending() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-survives").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = claimed_build_job(&bed, &f, task, node).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");
    let before = held(&state, node, task).await;
    assert_eq!(before.len(), 1);

    // What a real run does first: report the worktree it cut. That record is
    // what says a stack may be up in it.
    with_worktree(&bed, task, node, "MAIN-552").await;
    conclude(&state, f.tenant, job, true).await;

    assert_eq!(
        held(&state, node, task).await,
        before,
        "the run is over; its stack is not, so the lease stands"
    );
    assert_eq!(
        state
            .sessions
            .reclaim_and_held_ports(node)
            .await
            .expect("held ports"),
        vec![4200],
        "the allocator still sees the port as taken"
    );

    bed.teardown().await;
}

/// AC-4, and the reason this file exists. The allocator's first step drops the
/// rows of NON-LIVE SESSIONS on the node; run it beside a dead session while a
/// build's stack is alive and only the session's lease may go.
#[tokio::test]
async fn the_non_live_session_sweep_leaves_a_live_builds_lease_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-sweep").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");
    let build_port = held(&state, node, task).await;

    // A human session on the same node that then dies — exactly what the sweep
    // is FOR, so the sweep provably ran.
    let session = state
        .sessions
        .create(nook_control::repo::sessions::NewSession {
            tenant: f.tenant,
            workspace_id: Some(f.workspace),
            node_id: node,
            name: "a terminal".into(),
            runtime: "bash".into(),
            created_by: None,
            checkout_id: None,
            managed: false,
            managed_purpose: ManagedPurpose::Access,
            managed_shard: 0,
            managed_shards: 1,
            interface: SessionInterface::Terminal,
        })
        .await
        .expect("session")
        .id;
    port_leases::lease_for(&state, f.tenant, node, Some(f.workspace), session, "bash")
        .await
        .expect("session lease");
    bed.db()
        .exec(
            "UPDATE sessions SET status = 'exited' WHERE id = $1",
            params![session],
        )
        .await
        .expect("exit");

    let still_held = state
        .sessions
        .reclaim_and_held_ports(node)
        .await
        .expect("sweep");

    assert!(
        state
            .sessions
            .leases_of(session)
            .await
            .expect("session leases")
            .is_empty(),
        "the sweep must still reclaim a dead session's ports"
    );
    assert_eq!(
        held(&state, node, task).await,
        build_port,
        "a build's lease is not a session's and the sweep must not reach it — \
         its stack is still bound to that port"
    );
    assert_eq!(
        still_held,
        vec![4200],
        "and the build's port is still reported as taken"
    );

    bed.teardown().await;
}

/// AC-5: a repair run is a different job on the same card, in the same worktree,
/// talking to the same containers — so it gets the same numbers back.
#[tokio::test]
async fn a_second_run_on_the_same_card_gets_the_same_ports() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-repair").await;
    declare(
        &bed,
        &f,
        &[
            ("web", "NOOK_WEB_PORT", true),
            ("api", "NOOK_API_PORT", true),
        ],
    )
    .await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    let first = claimed_build_job(&bed, &f, task, node).await;
    lease(&state, &f, node, task).await.expect("first");
    let original = held(&state, node, task).await;
    with_worktree(&bed, task, node, "MAIN-552").await;
    conclude(&state, f.tenant, first, true).await;

    // A second card leasing in between, so "the same ports" cannot pass by the
    // allocator simply starting from the bottom of the range again.
    let other = card(&bed, &f, todo, 2).await;
    lease(&state, &f, node, other).await.expect("other");
    assert_ne!(
        held(&state, node, other).await,
        original,
        "a different card gets different ports"
    );

    lease(&state, &f, node, task).await.expect("repair");
    assert_eq!(
        held(&state, node, task).await,
        original,
        "the repair pass finds its own stack's ports, not a fresh pair"
    );

    bed.teardown().await;
}

/// AC-3, second half: the stack coming down is what frees the ports.
#[tokio::test]
async fn the_stack_reaper_releases_the_lease() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-reaper").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let done = column(&bed, &f, "Done", "completed", 1).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = claimed_build_job(&bed, &f, task, node).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");
    assert_eq!(held(&state, node, task).await.len(), 1);
    with_worktree(&bed, task, node, "MAIN-552").await;
    conclude(&state, f.tenant, job, true).await;

    bed.db()
        .exec(
            "UPDATE tasks SET column_id = $2 WHERE id = $1",
            params![task, done],
        )
        .await
        .expect("done");

    let ops = Double::new(Ok(Some("nook-build-x".into())));
    stack_reaper::reap_for_task_with(&state, f.tenant, task, &ops)
        .await
        .expect("reap");

    assert_eq!(
        ops.asked.lock().unwrap().len(),
        1,
        "the stack was actually brought down"
    );
    assert!(
        held(&state, node, task).await.is_empty(),
        "the stack is down, so the ports it bound come back"
    );

    bed.teardown().await;
}

/// The other half of AC-3's ordering: a `down` that FAILS keeps the worktree,
/// so it must keep the leases too. The containers are still listening.
#[tokio::test]
async fn a_stack_that_would_not_come_down_keeps_its_lease() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-nodown").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let done = column(&bed, &f, "Done", "completed", 1).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = claimed_build_job(&bed, &f, task, node).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");
    let before = held(&state, node, task).await;
    with_worktree(&bed, task, node, "MAIN-552").await;
    conclude(&state, f.tenant, job, true).await;
    bed.db()
        .exec(
            "UPDATE tasks SET column_id = $2 WHERE id = $1",
            params![task, done],
        )
        .await
        .expect("done");

    let ops = Double::new(Err("the daemon is not answering".into()));
    stack_reaper::reap_for_task_with(&state, f.tenant, task, &ops)
        .await
        .expect("reap");

    assert_eq!(
        held(&state, node, task).await,
        before,
        "the stack is still up, so its ports are still bound"
    );

    bed.teardown().await;
}

/// AC-6's allocator half: a REQUIRED listener with no free port is refused, and
/// the refusal names the listener and its variable.
#[tokio::test]
async fn an_unsatisfiable_required_listener_is_refused_by_name() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-refuse").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    // A range of exactly one port, taken by a card that got there first.
    let node = build_node(&bed, &f, Some((4200, 4200))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let state = bed.app_state().await;

    let first = card(&bed, &f, todo, 1).await;
    lease(&state, &f, node, first).await.expect("first");
    assert_eq!(
        held(&state, node, first).await.len(),
        1,
        "the range is full"
    );

    let second = card(&bed, &f, todo, 2).await;
    let refusal = lease(&state, &f, node, second)
        .await
        .expect_err("the second card cannot be satisfied");

    assert_eq!(refusal.listener, "web");
    assert_eq!(refusal.env, "NOOK_WEB_PORT");
    assert!(
        held(&state, node, second).await.is_empty(),
        "a refused build holds nothing"
    );

    bed.teardown().await;
}

/// A REFUSAL PART-WAY THROUGH gives back what it had already taken.
///
/// The rows are written one requirement at a time, so a declaration whose
/// fourth listener has no port has already written three. A session's partial
/// set is collected by the lazy reclaim; a build's is not (AC-4), and no
/// release route can reach a card that never ran — so those ports were held
/// against every build and every human session on the machine until somebody
/// deleted the card.
///
/// Two listeners against one free port, which is the smallest shape that can
/// tell a real rollback from `an_unsatisfiable_required_listener_is_refused_by_name`
/// passing vacuously because nothing had been leased yet.
#[tokio::test]
async fn a_refusal_part_way_through_gives_back_what_it_took() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-rollback").await;
    declare(
        &bed,
        &f,
        &[
            ("web", "NOOK_WEB_PORT", true),
            ("api", "NOOK_API_PORT", true),
        ],
    )
    .await;
    let node = build_node(&bed, &f, Some((4200, 4200))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    let refusal = lease(&state, &f, node, task)
        .await
        .expect_err("two listeners cannot fit in one port");
    assert_eq!(
        refusal.listener, "api",
        "the second listener is the one that fails"
    );

    assert!(
        held(&state, node, task).await.is_empty(),
        "the `web` lease taken before the refusal is given back, not stranded"
    );
    assert!(
        state
            .sessions
            .reclaim_and_held_ports(node)
            .await
            .expect("held")
            .is_empty(),
        "so the port is free to the next asker — a shortage must not deadlock \
         the node against every other build and session on it"
    );

    bed.teardown().await;
}

/// A repair pass refused on a NEWLY DECLARED listener keeps the leases its
/// running stack is bound to.
///
/// The rollback is scoped to what THIS call wrote, and this is why: the card's
/// existing set belongs to containers that are still listening, and freeing
/// them would hand live ports to the next asker — the collision this card
/// removes, arriving through its own repair.
#[tokio::test]
async fn a_refused_repair_keeps_the_leases_its_stack_is_bound_to() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-rollback-scope").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4200))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("the first pass");
    let running = held(&state, node, task).await;
    assert_eq!(running.len(), 1);

    // The repo adds a listener while the card's stack is up, and the node has
    // no port left for it.
    declare(
        &bed,
        &f,
        &[
            ("web", "NOOK_WEB_PORT", true),
            ("api", "NOOK_API_PORT", true),
        ],
    )
    .await;
    lease(&state, &f, node, task)
        .await
        .expect_err("the new listener cannot be satisfied");

    assert_eq!(
        held(&state, node, task).await,
        running,
        "the ports the stack is already bound to are untouched"
    );

    bed.teardown().await;
}

/// A card's leases on ANOTHER node are not reused as if they were this node's.
///
/// `leases_of` is safe without a node filter because a session belongs to one
/// machine. A card does not, and the reuse happens before any worktree exists —
/// so MAIN-480's pin is not yet holding the card anywhere. Without the scope
/// the run would bind numbers nothing on this node holds, and this node's
/// allocator would hand the same ones to the next session that asked.
#[tokio::test]
async fn a_cards_leases_on_another_node_are_not_reused_here() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-node-scope").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let a = build_node(&bed, &f, Some((4200, 4210))).await;
    let b = build_node(&bed, &f, Some((4300, 4310))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    lease(&state, &f, a, task).await.expect("on a");
    assert_eq!(
        held(&state, a, task).await,
        vec![("NOOK_WEB_PORT".into(), 4200)]
    );

    lease(&state, &f, b, task).await.expect("on b");
    assert_eq!(
        held(&state, b, task).await,
        vec![("NOOK_WEB_PORT".to_string(), 4300)],
        "B's lease comes from B's range — A's number is not B's to hand out"
    );
    assert_eq!(
        state
            .sessions
            .reclaim_and_held_ports(b)
            .await
            .expect("held on b"),
        vec![4300],
        "and B's allocator knows the port is taken, so the next asker cannot have it"
    );
    assert_eq!(
        held(&state, a, task).await,
        vec![("NOOK_WEB_PORT".to_string(), 4200)],
        "A's row is A's own and was not rewritten to a port out of B's range —          a lease describes exactly one machine"
    );

    bed.teardown().await;
}

/// AC-8: whatever holds a build's lease is not a session, so nothing offers it
/// as one. The lease still SHOWS on the node, which is what makes "why is 4200
/// taken" answerable.
#[tokio::test]
async fn a_builds_lease_is_visible_on_the_node_but_is_not_a_session() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-visible").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");

    assert!(
        state
            .sessions
            .list(f.tenant, Default::default())
            .await
            .expect("sessions")
            .is_empty(),
        "a build is not something a human opens a terminal on, so it is not a session row"
    );

    let leases = state.sessions.leases_on(node).await.expect("leases");
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].holder_kind, "build");
    assert_eq!(leases[0].holder_id, task.0, "the card is the holder");
    assert_eq!(leases[0].port, 4200);

    // …and the escape hatch reaches it by that same id, so an operator is never
    // left with a lease they can see and cannot free.
    let freed = state
        .sessions
        .release_leases_by_holder(node, leases[0].holder_id)
        .await
        .expect("release");
    assert_eq!(freed, 1);
    assert!(held(&state, node, task).await.is_empty());

    bed.teardown().await;
}

/// A node with no range leases nothing, exactly as it leases nothing for a
/// session — a working build without ports rather than a refusal. The optional
/// default listener must not turn a rangeless node into a refused build.
#[tokio::test]
async fn a_node_with_no_range_leases_nothing_and_refuses_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-norange").await;
    let node = build_node(&bed, &f, None).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let state = bed.app_state().await;

    let leased = lease(&state, &f, node, task)
        .await
        .expect("not every machine advertises ports, and that is not a refusal");
    assert!(leased.ports.is_empty());
    assert!(held(&state, node, task).await.is_empty());

    bed.teardown().await;
}

/// AC-2, on the wire: the ports the card holds are what the executor is told,
/// under the variable names the workspace chose. This is the whole delivery —
/// the node exports what it is handed and recognises none of the names, so a
/// message that carried nothing is a stack on compose's defaults.
#[tokio::test]
async fn the_leased_ports_ride_the_run_to_the_executor() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-wire").await;
    declare(
        &bed,
        &f,
        &[
            ("web", "NOOK_WEB_PORT", true),
            ("debug", "NOOK_DEBUG_PORT", false),
        ],
    )
    .await;
    let node = build_node(&bed, &f, Some((4200, 4200))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = claimed_build_job(&bed, &f, task, node).await;
    let state = bed.app_state().await;
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path,
                                          git_remote_url, git_branch)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            params![
                Uuid::now_v7(),
                f.tenant,
                node,
                f.workspace,
                "/checkouts/x",
                "git@example.test:acme/repo.git",
                "main"
            ],
        )
        .await
        .expect("node_workspace");

    lease(&state, &f, node, task).await.expect("leased");

    let (tx, mut rx) = mpsc::channel(4);
    state.registry.register_node(
        node,
        nook_control::ws::registry::NodeHandle {
            tenant_id: f.tenant,
            tx,
        },
    );
    let claimed = state
        .jobs
        .get(f.tenant, job)
        .await
        .expect("read back")
        .expect("job");
    jobs::dispatch_to_node(&state, f.tenant, &claimed)
        .await
        .expect("dispatch");

    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob {
            ports,
            unsatisfied_ports,
            ..
        } => {
            assert_eq!(
                ports,
                vec![LeasedPort {
                    name: "web".into(),
                    env: "NOOK_WEB_PORT".into(),
                    port: 4200,
                }],
                "the required listener is delivered under the workspace's own variable"
            );
            assert_eq!(
                unsatisfied_ports,
                vec!["debug".to_string()],
                "and the optional one it did not get is NAMED, so its consumer does \
                 not read an absent variable as `cloned outside nook, use the default`"
            );
        }
        other => panic!("expected RunLoopJob, got {other:?}"),
    }

    bed.teardown().await;
}

/// The other side of AC-3: a run that ended without ever making a tree left no
/// stack, so its ports come back at once. Holding them would be a leak nothing
/// else clears — the stack reaper acts on a recorded worktree, and there is
/// none.
#[tokio::test]
async fn a_run_that_never_made_a_tree_hands_its_ports_back() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bpl-stackless").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = claimed_build_job(&bed, &f, task, node).await;
    let state = bed.app_state().await;

    lease(&state, &f, node, task).await.expect("leased");
    assert_eq!(held(&state, node, task).await.len(), 1);

    // Node offline before the run started, executor reaper, a cancel — every
    // one of them lands on a terminal transition with no worktree recorded.
    conclude(&state, f.tenant, job, false).await;

    assert!(
        held(&state, node, task).await.is_empty(),
        "no tree means no stack means nothing is bound to those ports"
    );
    assert!(
        state
            .sessions
            .reclaim_and_held_ports(node)
            .await
            .expect("held")
            .is_empty(),
        "and the next build may have them"
    );

    bed.teardown().await;
}
