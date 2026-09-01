//! What happens to a finished card's build worktree (MAIN-537).
//!
//! Every test runs on a private `nook_testkit::TestBed`. Set `DATABASE_URL`.
//!
//! The node half — `docker compose down`, `git worktree remove`, `git branch
//! -d` — is unit-tested in `nook-node` against a real git repository, because a
//! live node and a docker daemon are not things a test can stand up. What is
//! decidable HERE is everything that decides: which trees are pruned, in which
//! order, what the card is told, and above all what is NOT pruned. The node
//! transport is a trait for exactly that reason, and the double below records
//! what it was asked for.
//!
//! The refusals are asserted as hard as the success, deliberately: a bug in
//! those paths destroys a running build's working directory or orphans a
//! compose stack nothing can name, where a bug in the success path only wastes
//! disk.

use std::sync::Mutex;

use async_trait::async_trait;
use nook_control::services::stack_reaper::{self, NodeOps};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;

/// A worktree path of the shape the node's `build_dirname` produces — the only
/// shape from which a compose project name can be derived.
fn build_worktree(key: &str) -> String {
    format!(
        "/root/.nook/clone-cache/host/worktrees/build-019f840f-2d80-7163-b4b1-8b1e12d7e0d3-{key}"
    )
}

#[derive(Default)]
struct Asked {
    downs: Vec<String>,
    removals: Vec<String>,
}

/// A node that answers as instructed and remembers what it was asked.
struct Double {
    down: Result<Option<String>, String>,
    removal: Result<String, String>,
    asked: Mutex<Asked>,
}

impl Double {
    fn new(down: Result<Option<String>, String>, removal: Result<String, String>) -> Self {
        Self {
            down,
            removal,
            asked: Mutex::new(Asked::default()),
        }
    }

    fn working() -> Self {
        Self::new(
            Ok(Some("nook-build-x".into())),
            Ok("removed worktree (2.4 GiB reclaimed); deleted branch main-537-x".into()),
        )
    }

    fn asked(&self) -> (Vec<String>, Vec<String>) {
        let a = self.asked.lock().unwrap();
        (a.downs.clone(), a.removals.clone())
    }
}

#[async_trait]
impl NodeOps for Double {
    async fn stack_down(
        &self,
        _node: NodeId,
        projects: &[String],
    ) -> Result<Option<String>, String> {
        self.asked.lock().unwrap().downs.push(projects.join(","));
        self.down.clone()
    }

    async fn remove_worktree(&self, _node: NodeId, path: &str) -> Result<String, String> {
        self.asked.lock().unwrap().removals.push(path.to_string());
        self.removal.clone()
    }
}

struct Fixture {
    tenant: TenantId,
    user: UserId,
    board: BoardId,
    node: NodeId,
}

async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("wtprune").await;
    let (user, person) = bed.user(tenant, "member").await;
    let node = bed.node(tenant, person).await;
    let board = BoardId::new();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1,$2,'b',$3,'local')",
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
        board,
        node,
    }
}

async fn column(bed: &TestBed, f: &Fixture, name: &str, kind: &str, position: i32) -> ColumnId {
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

async fn card_with_worktree(bed: &TestBed, f: &Fixture, col: ColumnId, key: &str) -> TaskId {
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by,
                                worktree_path, worktree_node_id)
             VALUES ($1,$2,$3,$4,'card','task',$5,$6,$7)",
            params![
                id,
                f.tenant,
                f.board,
                col,
                f.user,
                build_worktree(key),
                f.node
            ],
        )
        .await
        .expect("task");
    id
}

async fn recorded_worktree(bed: &TestBed, id: TaskId) -> Option<String> {
    let row: Option<(Option<String>,)> = bed
        .db()
        .query_opt("SELECT worktree_path FROM tasks WHERE id = $1", params![id])
        .await
        .expect("read record");
    row.and_then(|r| r.0)
}

async fn reaper_said(bed: &TestBed, id: TaskId) -> Vec<String> {
    let rows: Vec<(String, String)> = bed
        .db()
        .query_all(
            "SELECT author_name, body_md FROM task_comments WHERE task_id = $1",
            params![id],
        )
        .await
        .expect("comments");
    rows.into_iter()
        .filter(|(who, _)| who == "Stack reaper")
        .map(|(_, body)| body)
        .collect()
}

/// AC-2 and AC-5, the whole path: the card is over, the stack comes down, the
/// tree goes with it, the record is released, and the card says what it got back.
#[tokio::test]
async fn a_finished_cards_worktree_is_pruned_and_reported() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-505").await;
    let state = bed.app_state().await;
    let node = Double::working();

    stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
        .await
        .expect("reap");

    let (downs, removals) = node.asked();
    assert_eq!(downs.len(), 1, "the stack comes down");
    assert_eq!(
        removals,
        vec![build_worktree("MAIN-505")],
        "and then the tree goes"
    );
    assert_eq!(
        recorded_worktree(&bed, task).await,
        None,
        "the record is released once the tree is gone"
    );
    let said = reaper_said(&bed, task).await;
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("2.4 GiB reclaimed"), "{}", said[0]);
    assert!(said[0].contains("deleted branch main-537-x"), "{}", said[0]);
    bed.teardown().await;
}

/// The amended AC-2, and most of the disk: a build that never booted the stack
/// has nothing to bring down, and wiring the prune to "something came down"
/// would leave exactly those trees on the machine for good.
#[tokio::test]
async fn a_card_whose_build_never_booted_a_stack_is_pruned_too() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-524").await;
    let state = bed.app_state().await;
    let node = Double::new(Ok(None), Ok("removed worktree (2.6 GiB reclaimed)".into()));

    stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
        .await
        .expect("reap");

    assert_eq!(
        node.asked().1.len(),
        1,
        "nothing came down; the tree still goes"
    );
    assert_eq!(recorded_worktree(&bed, task).await, None);
    assert!(
        reaper_said(&bed, task).await[0].contains("no build stack was up"),
        "the card should say the stack was already absent"
    );
    bed.teardown().await;
}

/// AC-3. Leaving a worktree wastes disk and is recoverable; removing it would
/// leave containers whose project name is derived from that very directory.
#[tokio::test]
async fn a_stack_that_will_not_come_down_keeps_its_worktree() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-515").await;
    let state = bed.app_state().await;
    let node = Double::new(Err("docker unavailable".into()), Ok("unreachable".into()));

    stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
        .await
        .expect("a refusal is not an error");

    assert!(
        node.asked().1.is_empty(),
        "the removal must never be attempted after a failed down"
    );
    assert_eq!(
        recorded_worktree(&bed, task).await,
        Some(build_worktree("MAIN-515")),
        "the record still names the tree, because the tree is still there"
    );
    let said = reaper_said(&bed, task).await;
    assert!(said[0].contains("docker unavailable"), "{}", said[0]);
    assert!(said[0].contains("KEPT"), "{}", said[0]);
    bed.teardown().await;
}

/// A removal that fails leaves the record alone as well: it is the only thing
/// that still names the directory, and the directory is provably still there.
#[tokio::test]
async fn a_worktree_that_will_not_go_keeps_its_record() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-527").await;
    let state = bed.app_state().await;
    let node = Double::new(
        Ok(None),
        Err("prune failed: Permission denied (os error 13)".into()),
    );

    stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
        .await
        .expect("a refusal is not an error");

    assert_eq!(
        recorded_worktree(&bed, task).await,
        Some(build_worktree("MAIN-527")),
    );
    assert!(
        reaper_said(&bed, task).await[0].contains("Permission denied"),
        "the reason a prune failed is the whole point of reporting it"
    );
    bed.teardown().await;
}

/// AC-6, the case that destroys work rather than wasting disk: a card can reach
/// Done while its build is still running — a human moving it, or the merge
/// reconciler landing a PR the run is still amending — and that tree is the
/// run's working directory.
#[tokio::test]
async fn a_card_with_a_live_run_keeps_everything() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-528").await;
    bed.db()
        .exec(
            "INSERT INTO loop_jobs (id, tenant_id, kind, target_task_id, requested_by, state)
             VALUES ($1,$2,'build',$3,$4,'running')",
            params![uuid::Uuid::now_v7(), f.tenant, task, f.user],
        )
        .await
        .expect("a running build");
    let state = bed.app_state().await;
    let node = Double::working();

    stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
        .await
        .expect("reap");

    assert_eq!(
        node.asked(),
        (vec![], vec![]),
        "neither the stack nor the tree may be touched under a live run"
    );
    assert_eq!(
        recorded_worktree(&bed, task).await,
        Some(build_worktree("MAIN-528"))
    );
    assert!(reaper_said(&bed, task).await[0].contains("still using its build worktree"));
    bed.teardown().await;
}

/// AC-6's other half, and MAIN-480's policy restated: a card in review has a
/// finished build and a live worktree, and a repair run reuses both.
#[tokio::test]
async fn a_card_that_is_not_finished_is_never_pruned() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let review = column(&bed, &f, "In Review", "review", 2).await;
    let task = card_with_worktree(&bed, &f, review, "MAIN-537").await;
    let state = bed.app_state().await;
    let node = Double::working();

    stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
        .await
        .expect("reap");

    assert_eq!(node.asked(), (vec![], vec![]));
    assert_eq!(
        recorded_worktree(&bed, task).await,
        Some(build_worktree("MAIN-537"))
    );
    assert!(
        reaper_said(&bed, task).await.is_empty(),
        "and nothing is said"
    );
    bed.teardown().await;
}

/// A human's own checkout is not a build worktree and no sweep here reaches it:
/// only a build-shaped directory names a compose project, and that is the whole
/// eligibility test.
#[tokio::test]
async fn a_checkout_that_is_not_a_builds_is_left_alone() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let id = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, type, created_by,
                                worktree_path, worktree_node_id)
             VALUES ($1,$2,$3,$4,'card','task',$5,'/home/ryan/nook-os',$6)",
            params![id, f.tenant, f.board, done, f.user, f.node],
        )
        .await
        .expect("task");
    let state = bed.app_state().await;
    let node = Double::working();

    stack_reaper::reap_for_task_with(&state, f.tenant, id, &node)
        .await
        .expect("reap");

    assert_eq!(node.asked(), (vec![], vec![]));
    assert_eq!(
        recorded_worktree(&bed, id).await,
        Some("/home/ryan/nook-os".to_string())
    );
    bed.teardown().await;
}

/// AC-4: the trees already leaked belong to cards that finished while nothing
/// was listening, and the node's periodic inventory is where they are found.
/// Only the ones it actually HOLDS — a record whose directory is already gone
/// sends the node on no errand and the card no comment.
#[tokio::test]
async fn the_inventory_collects_the_trees_of_cards_that_are_over() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let dropped = column(&bed, &f, "Canceled", "canceled", 4).await;
    let review = column(&bed, &f, "In Review", "review", 2).await;
    let merged = card_with_worktree(&bed, &f, done, "MAIN-524").await;
    let canceled = card_with_worktree(&bed, &f, dropped, "MAIN-525").await;
    let in_review = card_with_worktree(&bed, &f, review, "MAIN-409").await;
    let gone = card_with_worktree(&bed, &f, done, "MAIN-201").await;
    let state = bed.app_state().await;

    let held = vec![
        build_worktree("MAIN-524"),
        build_worktree("MAIN-525"),
        build_worktree("MAIN-409"),
    ];
    let pruned = stack_reaper::sweep_worktrees_on_node(&state, f.node, &held)
        .await
        .expect("sweep");

    assert_eq!(pruned, 2, "both finished cards, and neither of the others");
    // The offline node refuses, so the trees survive — what is asserted here is
    // WHICH cards the sweep decided about, which is the sweep's whole job.
    for over in [merged, canceled] {
        assert!(
            !reaper_said(&bed, over).await.is_empty(),
            "a finished card's tree should have been acted on"
        );
    }
    for untouched in [in_review, gone] {
        assert!(
            reaper_said(&bed, untouched).await.is_empty(),
            "an unfinished card, and a tree the node does not hold, are not the sweep's business"
        );
    }
    bed.teardown().await;
}

/// The inventory repeats every ten minutes, so a card whose node is down for a
/// day would collect 144 identical comments. A reason that CHANGES is news; the
/// same reason twice is not.
#[tokio::test]
async fn a_repeated_refusal_is_only_said_once() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let done = column(&bed, &f, "Done", "completed", 3).await;
    let task = card_with_worktree(&bed, &f, done, "MAIN-439").await;
    let state = bed.app_state().await;
    let node = Double::new(Err("node is offline".into()), Ok("never asked".into()));

    for _ in 0..3 {
        stack_reaper::reap_for_task_with(&state, f.tenant, task, &node)
            .await
            .expect("reap");
    }

    assert_eq!(reaper_said(&bed, task).await.len(), 1);
    bed.teardown().await;
}
