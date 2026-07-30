//! MAIN-261: one definition of task visibility, proven to agree.
//!
//! "A private card is visible only to its creator or assignee" is written out
//! five times: once in Rust (`services::tasks::visible_by_cols`) and four times
//! as hand-transcribed SQL — in `pick_tasks`, `epic_children`, `related_tasks`
//! and the Mission Control overview join. Nothing made them agree. They were
//! copies, and the failure mode of a copy that drifts is a LEAK: a private card
//! shown to a stranger, which is silent, not a crash.
//!
//! So this file does not re-state the rule a sixth time. It takes the Rust
//! predicate as the oracle and drives the same case matrix through every real
//! SQL path, asserting outcome-for-outcome equality. A future edit to any one
//! predicate fails here naming the site that disagreed.
//!
//! NG-1 is binding: the SQL is deliberately NOT deduplicated. Proving agreement
//! is what makes a later single-definition refactor safe to attempt.
//!
//! Runs against a private `nook_testkit::TestBed`. Set `DATABASE_URL`.

use nook_control::repo::tasks::{DbTaskRepository, NewTask, PickParams, TaskRepository};
use nook_control::services::overview_queries::overview;
use nook_control::services::tasks::visible_by_cols;
use nook_testkit::TestBed;
use nook_types::*;
use sqlx::PgPool;

/// One task shape under test: the three inputs the rule actually reads.
struct Case {
    /// Names the row in a failure message.
    name: &'static str,
    visibility: &'static str,
    created_by: Option<UserId>,
    assignee: Option<UserId>,
    /// Filled in once the row exists.
    id: TaskId,
    key: String,
}

/// A viewer, named for the failure message.
struct Viewer {
    name: &'static str,
    id: UserId,
}

/// What every SQL site answered for one viewer, keyed by task id.
struct Observed {
    site: &'static str,
    visible: Vec<TaskId>,
}

impl Observed {
    fn saw(&self, t: TaskId) -> bool {
        self.visible.contains(&t)
    }
}

async fn checkout(
    db: &PgPool,
    tenant: TenantId,
    node: NodeId,
    ws: WorkspaceId,
    path: &str,
) -> NodeWorkspaceId {
    let id = NodeWorkspaceId::new();
    sqlx::query(
        "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path, kind)
         VALUES ($1, $2, $3, $4, $5, 'clone')",
    )
    .bind(id)
    .bind(tenant)
    .bind(node)
    .bind(ws)
    .bind(path)
    .execute(db)
    .await
    .expect("checkout");
    id
}

/// Every task key the overview payload exposes, across all checkouts.
fn overview_task_keys(ov: &Overview) -> Vec<String> {
    ov.workspaces
        .iter()
        .flat_map(|w| w.checkouts.iter())
        .flat_map(|c| c.tasks.iter().map(|t| t.key.clone()))
        .collect()
}

#[tokio::test]
async fn every_sql_visibility_predicate_agrees_with_the_rust_one() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (cases, viewers, repo, tenant, epic, anchor, db) = fixture(&mut bed).await;

    let mut disagreements = Vec::new();

    for v in &viewers {
        for observed in observe_all(&repo, tenant, epic, anchor, v.id).await {
            for c in &cases {
                // The oracle. Note it takes no role: `admin` is passed here as an
                // ordinary UserId, because visibility is ownership, not authority
                // (MAIN-76 NG-3). If a SQL site ever grants admins a bypass, the
                // admin rows below are what catches it.
                let want = visible_by_cols(c.visibility, c.created_by, c.assignee, v.id);
                let got = observed.saw(c.id);
                if want != got {
                    disagreements.push(format!(
                        "{site}: {case} seen by {viewer} — Rust says {want}, SQL says {got}",
                        site = observed.site,
                        case = c.name,
                        viewer = v.name,
                    ));
                }
            }
        }

        // The overview is keyed by board key, not id, so it is compared apart.
        let ov = overview(&db, tenant, None, None, Some(v.id))
            .await
            .expect("overview");
        let keys = overview_task_keys(&ov);
        for c in &cases {
            let want = visible_by_cols(c.visibility, c.created_by, c.assignee, v.id);
            let got = keys.contains(&c.key);
            if want != got {
                disagreements.push(format!(
                    "overview: {case} seen by {viewer} — Rust says {want}, SQL says {got}",
                    case = c.name,
                    viewer = v.name,
                ));
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "a SQL visibility predicate has drifted from services::tasks::visible_by_cols:\n  {}",
        disagreements.join("\n  ")
    );

    // Guard against the matrix silently emptying: 4 shapes x 4 viewers x 5 sites.
    assert_eq!(cases.len() * viewers.len(), 16, "the case matrix is intact");

    bed.teardown().await;
}

/// AC-3, made permanent instead of one-off.
///
/// The ticket asks for the harness to be proven by editing a production
/// predicate and watching the test go red. That was done (see the PR), but a
/// demonstration nobody can re-run is not a guard: the next person to change a
/// predicate has only my word that the comparison bites. So the SAME matrix is
/// run here against a deliberately broken copy of the `epic_children` predicate
/// — one that drops the `created_by` leg, the exact shape of a bad transcription
/// — and this test fails if that mutant is NOT caught.
///
/// The mutant is SQL in a test file, which the chain's NG-4 allows; no
/// production predicate is touched, so NG-1 holds.
#[tokio::test]
async fn the_agreement_check_catches_a_dropped_predicate_leg() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let (cases, viewers, _repo, _tenant, epic, _anchor, _db) = fixture(&mut bed).await;

    let mut caught = Vec::new();
    for v in &viewers {
        // `epic_children`, verbatim, minus `OR t.created_by = $2`.
        let rows: Vec<(TaskId,)> = sqlx::query_as(
            "SELECT t.id FROM tasks t
              WHERE t.parent_task_id = $1
                AND (t.visibility <> 'private' OR t.assignee_user_id = $2)",
        )
        .bind(epic)
        .bind(v.id)
        .fetch_all(&bed.pool)
        .await
        .expect("mutant epic_children");
        let seen: Vec<TaskId> = rows.into_iter().map(|r| r.0).collect();

        for c in &cases {
            let want = visible_by_cols(c.visibility, c.created_by, c.assignee, v.id);
            if want != seen.contains(&c.id) {
                caught.push(format!("{} seen by {}", c.name, v.name));
            }
        }
    }

    assert!(
        !caught.is_empty(),
        "the comparison did NOT notice a predicate missing its `created_by` leg — \
         the matrix cannot be distinguishing creator-only visibility, so the \
         agreement test above is not actually load-bearing"
    );

    bed.teardown().await;
}

type Fixture = (
    Vec<Case>,
    Vec<Viewer>,
    DbTaskRepository,
    TenantId,
    TaskId,
    TaskId,
    nook_db::DbPool,
);

/// Four task shapes reachable from all five sites, and four viewers.
///
/// Every case task is simultaneously: on the board (so `pick_tasks` sees it), a
/// child of one epic (`epic_children`), related to one anchor
/// (`related_tasks`), and bound to a checkout (the overview join). One row per
/// shape, therefore, rather than four near-identical rows per site — which is
/// what makes "the sites disagree" the only thing a failure can mean.
async fn fixture(bed: &mut TestBed) -> Fixture {
    let tenant = bed.tenant("vis").await;
    let (alice, person) = bed.user(tenant, "member").await;
    let (bob, _) = bed.user(tenant, "member").await;
    let (carol, _) = bed.user(tenant, "member").await;
    let (dana, _) = bed.user(tenant, "admin").await;

    let db = bed.db();
    let repo = DbTaskRepository::new(db.clone());
    let board = repo
        .create_board(tenant, None, "Visibility", "VIS")
        .await
        .expect("board");
    let col = repo
        .create_column(board.id, "In Progress", 0, "started")
        .await
        .expect("column");

    let node = bed.node(tenant, person).await;
    let ws = bed.workspace(tenant).await;
    let co = checkout(&bed.pool, tenant, node, ws, "/srv/vis").await;

    let mk = |title: &str, type_: &str, visibility: &str, created_by: Option<UserId>| NewTask {
        tenant,
        board: board.id,
        column_id: col.id,
        title: title.to_string(),
        description: None,
        position: 0,
        workspace_id: None,
        priority: 2,
        type_: type_.to_string(),
        visibility: visibility.to_string(),
        created_by: created_by.map(|u| u.0),
        parent_task_id: None,
        labels: vec![],
    };

    // The epic every case hangs off, and the anchor every case relates to. Both
    // are `org` so they are never themselves filtered — a case must be invisible
    // because of its OWN row, never because its parent vanished.
    let epic = repo
        .create_task(mk("epic", "epic", "org", Some(alice)))
        .await
        .expect("epic")
        .id;
    let anchor = repo
        .create_task(mk("anchor", "task", "org", Some(alice)))
        .await
        .expect("anchor")
        .id;

    let shapes: [(&'static str, &'static str, Option<UserId>, Option<UserId>); 4] = [
        (
            "private/alice-creator/bob-assignee",
            "private",
            Some(alice),
            Some(bob),
        ),
        (
            "team/alice-creator/bob-assignee",
            "team",
            Some(alice),
            Some(bob),
        ),
        (
            "org/alice-creator/bob-assignee",
            "org",
            Some(alice),
            Some(bob),
        ),
        // Nobody owns it: the row no viewer at all may see. This is the case a
        // predicate written as `created_by IS NULL OR …` would wrongly pass.
        ("private/unowned", "private", None, None),
    ];

    let mut cases = Vec::new();
    for (name, visibility, created_by, assignee) in shapes {
        let t = repo
            .create_task(mk(name, "task", visibility, created_by))
            .await
            .expect("case task");
        // create_task takes no assignee, parent or checkout; set them directly.
        // Tests keep raw DB access (repository-chain NG-4).
        sqlx::query(
            "UPDATE tasks SET assignee_user_id = $2, parent_task_id = $3, checkout_id = $4
              WHERE id = $1",
        )
        .bind(t.id)
        .bind(assignee.map(|u| u.0))
        .bind(epic)
        .bind(co)
        .execute(&bed.pool)
        .await
        .expect("wire the case row");
        repo.upsert_relation(tenant, anchor, t.id, "relates")
            .await
            .expect("relation");
        cases.push(Case {
            name,
            visibility,
            created_by,
            assignee,
            id: t.id,
            // `create_task` returns the row, not the board join, so `key` is
            // None there; the overview builds the same `KEY-number` string.
            key: format!(
                "{}-{}",
                board.key.as_deref().expect("board key"),
                t.number.expect("task number")
            ),
        });
    }

    let viewers = vec![
        Viewer {
            name: "alice (creator)",
            id: alice,
        },
        Viewer {
            name: "bob (assignee)",
            id: bob,
        },
        Viewer {
            name: "carol (stranger)",
            id: carol,
        },
        // An admin is a stranger here on purpose: role grants no bypass of a
        // private card, and a SQL site that added one would show up as a leak.
        Viewer {
            name: "dana (admin, neither creator nor assignee)",
            id: dana,
        },
    ];

    (cases, viewers, repo, tenant, epic, anchor, db)
}

/// The three id-keyed SQL sites, for one viewer.
async fn observe_all(
    repo: &DbTaskRepository,
    tenant: TenantId,
    epic: TaskId,
    anchor: TaskId,
    viewer: UserId,
) -> Vec<Observed> {
    let picked = repo
        .pick_tasks(
            tenant,
            viewer,
            PickParams {
                limit: 500,
                ..Default::default()
            },
        )
        .await
        .expect("pick_tasks");
    let children = repo
        .epic_children(epic, viewer)
        .await
        .expect("epic_children");
    let related = repo
        .related_tasks(anchor, viewer)
        .await
        .expect("related_tasks");

    vec![
        Observed {
            site: "pick_tasks",
            visible: picked.into_iter().map(|t| t.id).collect(),
        },
        Observed {
            site: "epic_children",
            visible: children.into_iter().map(|c| c.id).collect(),
        },
        Observed {
            site: "related_tasks",
            visible: related.into_iter().map(|r| r.id).collect(),
        },
    ]
}
