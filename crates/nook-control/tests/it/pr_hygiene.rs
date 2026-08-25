//! MAIN-476 end to end: a conflicted or label-stripped PR re-enters the repair
//! queue by itself. Through the real `heal` pass against a live database, with
//! a fake forge whose answers the test dictates — the forge writes and the card
//! mirror are asserted on, and the once-per-head rule is exercised as two
//! consecutive passes.
//!
//! Setup + teardown run through `nook_testkit::TestBed` (MAIN-156).

use std::collections::HashMap;
use std::sync::Mutex;

use nook_control::repo::jobs::NewLoopJob;
use nook_control::services::forge::{Forge, MergeState, PrDetails, PullRequest, Repo};
use nook_control::services::pr_hygiene;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A forge whose PR detail the test dictates and whose writes it can read back.
#[derive(Default)]
struct FakeForge {
    details: HashMap<u64, PrDetails>,
    labels: Mutex<HashMap<u64, Vec<String>>>,
    comments: Mutex<HashMap<u64, Vec<String>>>,
}

impl FakeForge {
    fn labels_of(&self, pr: u64) -> Vec<String> {
        self.labels
            .lock()
            .unwrap()
            .get(&pr)
            .cloned()
            .unwrap_or_default()
    }
    fn comments_of(&self, pr: u64) -> Vec<String> {
        self.comments
            .lock()
            .unwrap()
            .get(&pr)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Forge for FakeForge {
    async fn prs_needing_review(&self, _repo: &Repo) -> anyhow::Result<Vec<PullRequest>> {
        anyhow::bail!("the heal pass receives its PR list; it must not re-fetch")
    }
    async fn pr_details(&self, _repo: &Repo, number: u64) -> anyhow::Result<PrDetails> {
        self.details
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no details staged for PR {number}"))
    }
    async fn issue_comment_bodies(&self, _repo: &Repo, number: u64) -> anyhow::Result<Vec<String>> {
        Ok(self.comments_of(number))
    }
    async fn comment(&self, _repo: &Repo, number: u64, body: &str) -> anyhow::Result<()> {
        self.comments
            .lock()
            .unwrap()
            .entry(number)
            .or_default()
            .push(body.to_string());
        Ok(())
    }
    async fn set_verdict_label(
        &self,
        _repo: &Repo,
        number: u64,
        label: &str,
    ) -> anyhow::Result<()> {
        let mut labels = self.labels.lock().unwrap();
        let entry = labels.entry(number).or_default();
        entry.retain(|l| {
            !matches!(
                l.as_str(),
                "loop-approved" | "loop-changes-requested" | "needs-human-review"
            )
        });
        entry.push(label.to_string());
        Ok(())
    }
}

fn repo() -> Repo {
    Repo {
        owner: "acme".into(),
        name: "api".into(),
    }
}

fn pr(number: u64, head: &str, labels: &[&str]) -> PullRequest {
    PullRequest {
        number,
        head_sha: head.into(),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        base_ref: "main".into(),
    }
}

struct Fixture {
    tenant: TenantId,
    user: UserId,
    ws: WorkspaceId,
    key: String,
    task: TaskId,
}

/// A tenant, a workspace, and one board card the PR's `Closes` line points at.
async fn fixture(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("hygiene").await;
    let (user, _) = bed.user(tenant, "member").await;
    let ws = bed.workspace(tenant).await;

    let board = BoardId(Uuid::now_v7());
    let key = format!("B{}", &board.0.simple().to_string()[26..]).to_uppercase();
    bed.db()
        .exec(
            "INSERT INTO boards (id, tenant_id, name, key, provider) VALUES ($1, $2, 'b', $3, 'local')",
            params![board, tenant, key.clone()],
        )
        .await
        .unwrap();
    let col = ColumnId::new();
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'In Review', 0, 'review')",
            params![col, board],
        )
        .await
        .unwrap();
    let task = TaskId::new();
    bed.db()
        .exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, number)
             VALUES ($1, $2, $3, $4, 'stuck pr card', 53)",
            params![task, tenant, board, col],
        )
        .await
        .unwrap();
    Fixture {
        tenant,
        user,
        ws,
        key: format!("{key}-53"),
        task,
    }
}

async fn card_labels(bed: &TestBed, task: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT l.name FROM task_labels tl JOIN labels l ON l.id = tl.label_id
             WHERE tl.task_id = $1 ORDER BY l.name",
            params![task],
        )
        .await
        .unwrap()
}

async fn card_comments(bed: &TestBed, task: TaskId) -> Vec<String> {
    bed.db()
        .query_scalar_all(
            "SELECT body_md FROM task_comments WHERE task_id = $1 ORDER BY created_at",
            params![task],
        )
        .await
        .unwrap()
}

/// A completed review run with a recorded verdict, planted straight into the
/// ledger — the restore reads facts, not the state machine that made them.
async fn recorded_verdict(
    bed: &TestBed,
    state: &nook_control::state::AppState,
    f: &Fixture,
    pr: i64,
    head: &str,
    verdict: &str,
) {
    let job = state
        .jobs
        .create(NewLoopJob {
            id: JobId(Uuid::now_v7()),
            tenant: f.tenant,
            kind: "review".into(),
            target_task_id: None,
            workspace_id: Some(f.ws),
            requested_by: f.user,
            seed: None,
            predecessor_job_id: None,
            review_pr_number: Some(pr),
            review_head_sha: Some(head.into()),
            build_fingerprint: None,
            review_forced: false,
        })
        .await
        .unwrap();
    bed.db()
        .exec(
            "UPDATE loop_jobs SET state = 'completed', review_verdict = $2 WHERE id = $1",
            params![job.id, verdict],
        )
        .await
        .unwrap();
}

/// AC-1 + AC-2 + AC-4's once-rule: the conflicted PR gains the label and ONE
/// comment, the card mirrors it, and a second pass at the same head adds
/// nothing anywhere.
#[tokio::test]
async fn a_conflicted_pr_is_requeued_once_and_mirrored_to_its_card() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    let mut forge = FakeForge::default();
    forge.details.insert(
        5,
        PrDetails {
            mergeable: Some(false),
            body: format!("What changed\n\nCloses {}\n\nRisk: Low", f.key),
            merge_state: MergeState::Open,
            head_sha: String::new(),
        },
    );
    // The card still says loop-approved — the exact drift from PR #353's story.
    state
        .tasks
        .attach_label(f.tenant, f.task, "loop-approved")
        .await
        .unwrap();

    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "aaa", &["loop-approved"])],
    )
    .await
    .unwrap();
    assert_eq!((healed.marked, healed.restored), (1, 0));
    assert_eq!(
        healed.recorded, 1,
        "MAIN-516: the label alone was never the repair queue"
    );

    let comments = forge.comments_of(5);
    assert_eq!(comments.len(), 1, "one conflict comment");
    assert!(comments[0].contains("Loop conflict check of aaa"));
    assert!(comments[0].contains("rebase required"));
    assert_eq!(
        forge.labels_of(5),
        vec!["loop-changes-requested"],
        "the label replaced loop-approved, exactly as a verdict would"
    );

    let card = card_comments(&bed, f.task).await;
    assert_eq!(card.len(), 1, "one card comment");
    assert!(card[0].contains("Loop conflict check of aaa"));
    assert!(card[0].contains("https://github.com/acme/api/pull/5"));
    let labels = card_labels(&bed, f.task).await;
    assert!(labels.contains(&"loop-changes-requested".to_string()));
    assert!(
        !labels.contains(&"loop-approved".to_string()),
        "the card's stale loop-approved is replaced"
    );

    // Second pass, same head, label now present: nothing anywhere. This is the
    // once-per-head rule doing its job.
    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "aaa", &["loop-changes-requested"])],
    )
    .await
    .unwrap();
    assert_eq!((healed.marked, healed.restored), (0, 0));
    assert_eq!(forge.comments_of(5).len(), 1, "no second PR comment");
    assert_eq!(
        card_comments(&bed, f.task).await.len(),
        1,
        "no second card comment"
    );

    // The label stripped at the SAME head while the comment stands: the label
    // comes back, the comment does not repeat, the card is not re-told.
    //
    // It comes back through the RESTORE heal now (MAIN-516), not through a
    // second conflict check: the first pass recorded a `changes_requested` at
    // this head, and a recorded verdict for the current head is exactly what
    // the restore reads. Same label, same silence, one fewer forge round trip —
    // which counter moves is the whole difference.
    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "aaa", &[])],
    )
    .await
    .unwrap();
    assert_eq!((healed.restored, healed.marked), (1, 0));
    assert_eq!(
        forge.labels_of(5),
        vec!["loop-changes-requested"],
        "restored from the conflict's own recorded verdict"
    );
    assert_eq!(forge.comments_of(5).len(), 1, "still one PR comment");
    assert_eq!(
        card_comments(&bed, f.task).await.len(),
        1,
        "still one card comment"
    );

    bed.teardown().await;
}

/// AC-4: the rebase cleared the conflict — the next poll sees a clean PR at a
/// new head and adds nothing.
#[tokio::test]
async fn a_rebase_that_clears_the_conflict_lifts_the_state() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    let mut forge = FakeForge::default();
    forge.details.insert(
        5,
        PrDetails {
            mergeable: Some(true),
            body: format!("Closes {}", f.key),
            merge_state: MergeState::Open,
            head_sha: String::new(),
        },
    );
    // The builder's repair removed the label when it pushed the rebase.
    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "bbb", &[])],
    )
    .await
    .unwrap();
    assert_eq!((healed.marked, healed.restored), (0, 0));
    assert!(forge.comments_of(5).is_empty());
    assert!(forge.labels_of(5).is_empty());
    assert!(card_comments(&bed, f.task).await.is_empty());

    bed.teardown().await;
}

/// AC-3: a verdict label stripped outside the loop is restored from the verdict
/// this deployment recorded for the CURRENT head — and the NEWEST one wins when
/// the ledger holds several.
#[tokio::test]
async fn a_stripped_verdict_label_is_restored_from_the_recorded_verdict() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    // An older approved, then a newer changes_requested at the same head: the
    // restore must speak with the newest voice.
    recorded_verdict(&bed, &state, &f, 5, "aaa", "approved").await;
    recorded_verdict(&bed, &state, &f, 5, "aaa", "changes_requested").await;

    let mut forge = FakeForge::default();
    forge.details.insert(
        5,
        PrDetails {
            mergeable: Some(true),
            body: String::new(),
            merge_state: MergeState::Open,
            head_sha: String::new(),
        },
    );
    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "aaa", &[])],
    )
    .await
    .unwrap();
    assert_eq!((healed.restored, healed.marked), (1, 0));
    assert_eq!(forge.labels_of(5), vec!["loop-changes-requested"]);
    assert!(forge.comments_of(5).is_empty(), "a restore is silent");

    bed.teardown().await;
}

/// AC-3's boundary, both halves: a verdict recorded for another head restores
/// nothing, and a PR with no recorded verdict is never labeled at all.
#[tokio::test]
async fn no_applicable_verdict_never_labels() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    // PR 5: a verdict, but for a head this PR no longer has. PR 6: nothing.
    recorded_verdict(&bed, &state, &f, 5, "old", "approved").await;

    let mut forge = FakeForge::default();
    for n in [5, 6] {
        forge.details.insert(
            n,
            PrDetails {
                mergeable: Some(true),
                body: String::new(),
                merge_state: MergeState::Open,
                head_sha: String::new(),
            },
        );
    }
    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "new", &[]), pr(6, "fff", &[])],
    )
    .await
    .unwrap();
    assert_eq!((healed.restored, healed.marked), (0, 0));
    assert!(forge.labels_of(5).is_empty());
    assert!(forge.labels_of(6).is_empty());

    bed.teardown().await;
}

/// The partial-failure repair: the PR comment landed on an earlier pass but
/// the card mirror did not (label write 403'd, mirror errored — any split).
/// The mirror is deduped against the CARD's own comments, not against whether
/// this iteration posted the PR comment, so the card catches up on the next
/// pass instead of being stranded at PR #353's split-brain forever.
#[tokio::test]
async fn a_card_that_missed_its_mirror_catches_up_without_a_new_pr_comment() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed).await;
    let state = bed.app_state().await;

    let mut forge = FakeForge::default();
    forge.details.insert(
        5,
        PrDetails {
            mergeable: Some(false),
            body: format!("Closes {}", f.key),
            merge_state: MergeState::Open,
            head_sha: String::new(),
        },
    );
    // The PR side was already announced on a previous pass; the card was not.
    forge
        .comments
        .lock()
        .unwrap()
        .entry(5)
        .or_default()
        .push("Loop conflict check of aaa\n\nconflicts with the base branch".into());

    let healed = pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "aaa", &[])],
    )
    .await
    .unwrap();
    assert_eq!(healed.marked, 1);
    assert_eq!(
        forge.comments_of(5).len(),
        1,
        "the already-announced head gets no second PR comment"
    );
    let card = card_comments(&bed, f.task).await;
    assert_eq!(card.len(), 1, "the card mirror catches up");
    assert!(card[0].contains("Loop conflict check of aaa"));

    // And it stays idempotent: another pass adds nothing anywhere.
    pr_hygiene::heal(
        &state,
        &forge,
        &repo(),
        f.tenant,
        f.user,
        f.ws,
        &[pr(5, "aaa", &["loop-changes-requested"])],
    )
    .await
    .unwrap();
    assert_eq!(forge.comments_of(5).len(), 1);
    assert_eq!(card_comments(&bed, f.task).await.len(), 1);

    bed.teardown().await;
}
