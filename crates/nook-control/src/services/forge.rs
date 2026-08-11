//! The forge seam (MAIN-448): how much review work a repository has.
//!
//! ## The control plane holds the forge, deliberately
//!
//! Everywhere else, talking to GitHub is the NODE's job — `nook-review` runs
//! `gh` inside a session, and the control plane only decides where that session
//! runs. This module is the exception, and the reason is scale-to-zero.
//!
//! `review_loop_max_replicas` is a ceiling, and the target shape is
//! `desired = min(open_prs, ceiling)`. Deciding whether to run a reviewer AT
//! ALL therefore requires knowing whether there is work — and if the answer
//! came from the reviewers themselves, a repo at zero reviewers would have
//! nobody left to report that a PR had appeared. It could never come back. The
//! knowledge has to live where the decision is made.
//!
//! ## What crosses this seam
//!
//! One number, and nothing else. [`Forge`] is the whole surface: no PR ids, no
//! JSON shapes, no `gh`, no api.github.com outside [`GithubForge`]. A second
//! forge lands as a second implementation and nothing above here changes.
//!
//! Read-only, and it stays read-only (NG-4): verdicts are still posted by
//! `nook-review` on the node, with the node's own credential.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nook_types::WorkspaceId;

/// One repository, as the forge names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

/// One open pull request, as a unit of review work.
///
/// The head sha is what makes a wakeup PER-PR rather than per-repo: a run is
/// owed for a PR whose head has moved since the last completed run for it, and
/// owed for nothing otherwise. A count could only ever say "this repo has PRs",
/// which is why the count-based version needed a timer to decide when to look
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub head_sha: String,
    /// The PR's label names — how a repair item is recognized (MAIN-458):
    /// `loop-changes-requested` at a head no repair run has answered. The
    /// hygiene pass (MAIN-476) reads the same list the other way around: no
    /// verdict label means nothing routes the PR, however the labels came to
    /// be missing.
    pub labels: Vec<String>,
}

/// Where a pull request ended up, as the per-PR read reports it.
///
/// Three states, not two, because the board reconciler (MAIN-491) owes a
/// different answer to each: merged completes the card, closed-unmerged raises
/// a hand, and open is simply not finished. Read from the PR itself and never
/// inferred from absence from the open-PR list — that list is filtered (drafts)
/// and paginated, so "not in it" is not a statement about how a PR ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    Open,
    Merged,
    ClosedUnmerged,
}

/// One pull request's merge-relevant detail, fetched one PR at a time — the
/// list endpoint does not carry `mergeable`, so this is a second, per-PR read.
///
/// `mergeable` is GitHub's tri-state: `Some(false)` is a real conflict,
/// `Some(true)` is clean, and `None` means GitHub is still computing — treat it
/// as unknown and ask again on a later pass, never as either answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrDetails {
    pub mergeable: Option<bool>,
    /// The PR description — it carries the `Closes KEY` line that joins the PR
    /// to its board card.
    pub body: String,
    pub merge_state: MergeState,
}

/// One merged pull request, as the body join's search space (MAIN-491 AC-2):
/// a card whose `pr_url` was never recorded is found by reading `Closes KEY`
/// out of what actually merged recently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPr {
    pub number: u64,
    pub body: String,
    pub merged_at: chrono::DateTime<chrono::Utc>,
}

/// The latest COMPLETED run of a repository's own CI on its default branch
/// (MAIN-543).
///
/// COMPLETED is the whole point: a run still in progress is not a verdict on
/// anything, and treating one as a failure-in-waiting would extend a pause past
/// the push that fixes it (AC-4). The forge is asked for completed runs only,
/// so an unfinished one cannot reach here to be misread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRun {
    /// The branch the run is for — read from the repository rather than
    /// assumed, because `main` is a convention and not a fact.
    pub branch: String,
    /// The workflow's display name, for the sentence a human reads.
    pub workflow: String,
    /// GitHub's own word: `success`, `failure`, `cancelled`, `timed_out`, …
    pub conclusion: String,
    /// Where to go and look.
    pub url: String,
    pub head_sha: String,
}

/// What the reconciler needs to know about a repository's review queue.
///
/// Started as one operation (MAIN-448 kept build status, check runs and
/// mergeability deliberately absent). MAIN-476 widened it: a CONFLICTING PR
/// must re-enter the repair queue, and a verdict label stripped outside the
/// loop must be restored, so the hygiene pass needs per-PR detail and the two
/// writes the reviewer's verdict path already performs. The write methods
/// default to a loud refusal so a read-only forge stays read-only by simply
/// not implementing them.
#[async_trait::async_trait]
pub trait Forge: Send + Sync {
    /// The open pull requests a reviewer would pick up, with their heads.
    ///
    /// An error is an OUTAGE, never an empty list. The caller's whole failure
    /// policy depends on those being different values, so an implementation that
    /// cannot reach the forge must return `Err` rather than report an empty
    /// queue — otherwise an outage reads as "everything is reviewed" and the
    /// reviewers scale to zero exactly when they are needed.
    async fn prs_needing_review(&self, repo: &Repo) -> anyhow::Result<Vec<PullRequest>>;

    /// How many, which is all MAIN-448's sizing ever wanted. DERIVED, so the
    /// count can never disagree with the list it came from.
    async fn open_prs_needing_review(&self, repo: &Repo) -> anyhow::Result<u32> {
        Ok(self.prs_needing_review(repo).await?.len() as u32)
    }

    /// One PR's merge detail — see [`PrDetails`].
    async fn pr_details(&self, repo: &Repo, number: u64) -> anyhow::Result<PrDetails> {
        let _ = (repo, number);
        anyhow::bail!("this forge cannot read pull request details")
    }

    /// The latest COMPLETED run of the repository's CI on its DEFAULT branch,
    /// or `None` when there is nothing to read.
    ///
    /// The failure policy here is the opposite of `prs_needing_review`'s, and
    /// deliberately so. There, an empty list read as "nothing to review" and
    /// stopped every reviewer, so an implementation that cannot answer must
    /// `Err`. Here the caller PAUSES on a definite failure and dispatches on
    /// everything else, so the direction that costs is a false red — which is
    /// why `None` (no CI configured, nothing completed yet) is a legitimate
    /// answer rather than an error, and why a forge that reads no CI at all
    /// says `None` rather than bailing. An `Err` still means outage, and the
    /// caller fails open on it just the same (MAIN-543 AC-3).
    async fn default_branch_ci(&self, repo: &Repo) -> anyhow::Result<Option<CiRun>> {
        let _ = repo;
        Ok(None)
    }

    /// The pull requests merged at or after `since`, for the body join.
    ///
    /// An error is an OUTAGE with the same force as `prs_needing_review`'s: an
    /// empty list here reads as "nothing merged recently", which is precisely
    /// the claim a forge that cannot answer must not make.
    async fn merged_prs_since(
        &self,
        repo: &Repo,
        since: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<MergedPr>> {
        let _ = (repo, since);
        anyhow::bail!("this forge cannot read merged pull requests")
    }

    /// The bodies of a PR's issue comments, for once-per-head dedupe. An error
    /// must stay an error — an empty list here means "prove it was never said",
    /// and a forge that cannot read comments must not fake that proof.
    async fn issue_comment_bodies(&self, repo: &Repo, number: u64) -> anyhow::Result<Vec<String>> {
        let _ = (repo, number);
        anyhow::bail!("this forge cannot read comments")
    }

    /// Post one comment on the PR.
    async fn comment(&self, repo: &Repo, number: u64, body: &str) -> anyhow::Result<()> {
        let _ = (repo, number, body);
        anyhow::bail!("this forge cannot write comments")
    }

    /// Put exactly one verdict label on the PR, removing the other two — the
    /// same replace the verdict path performs, so a PR can never carry two.
    async fn set_verdict_label(&self, repo: &Repo, number: u64, label: &str) -> anyhow::Result<()> {
        let _ = (repo, number, label);
        anyhow::bail!("this forge cannot write labels")
    }
}

/// Is this remote a GitHub repository, and which one?
///
/// Reuses `discovery::normalize_remote`, which already reduces every URL shape
/// the fleet stores — scp-style, https, ssh, with or without credentials and
/// `.git` — to `host/path`. Parsing the raw URL again here would be a second
/// implementation of a thing that has one.
///
/// `None` for a local path (`/workspace/nook-dogfood.git`), a self-hosted
/// forge, or anything with more or fewer than two path segments. That is the
/// "no forge configured" case, and it is a supported state, not a failure.
pub fn github_repo(remote: &str) -> Option<Repo> {
    let normalized = crate::services::discovery::normalize_remote(remote);
    let (host, path) = normalized.split_once('/')?;
    if host != "github.com" {
        return None;
    }
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(Repo {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// The environment variables carrying the fleet's GitHub credential.
///
/// The SAME names, in the same order, that `nook_node::config::fleet_gh_token`
/// reads (MAIN-407) — this is that credential, reached from the other process,
/// not a second mechanism (NG-5). The list is duplicated because the two
/// binaries share no crate that could hold it; `the_token_variables_match_the_nodes`
/// below reads the node's source and fails if they ever diverge.
const TOKEN_VARS: [&str; 3] = ["NOOK_GH_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

/// The fleet token, or `None` when this deployment has not been given one.
///
/// Empty and whitespace are not credentials, and the emptiness test is INSIDE
/// the search for the reason the node states: compose always SETS
/// `NOOK_GH_TOKEN`, to the empty string when the operator supplied nothing, so
/// rejecting empties only at the end would let that shadow a real `GH_TOKEN`.
fn fleet_gh_token() -> Option<String> {
    TOKEN_VARS
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|t| !t.trim().is_empty())
}

/// GitHub, over its REST API.
pub struct GithubForge {
    http: reqwest::Client,
    token: String,
}

impl GithubForge {
    /// Build one from the fleet credential, or `None` when there is no token.
    ///
    /// No token means NO FORGE, not an unauthenticated one. An anonymous client
    /// cannot see a private repo's PRs at all and would report zero — which is
    /// the one answer that must never be guessed, because it reads as "nothing
    /// to review" and stops every reviewer. Without a token the deployment
    /// falls back to the pre-MAIN-448 behaviour: the ceiling is the count.
    /// A forge speaking as one WORKSPACE's own token (MAIN-456).
    pub fn from_token(token: &str) -> Self {
        Self {
            // The same 10s bound `from_env` carries, for the same reason: the
            // hygiene pass runs inside the serial reconcile loop, and a forge
            // that never answers must not hold every tenant behind it.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            token: token.to_string(),
        }
    }

    pub fn from_env() -> Option<Self> {
        let token = fleet_gh_token()?;
        Some(Self {
            // Ten seconds, matching `notify`'s client and for the same reason: a
            // forge that never answers must not hold the reconcile pass open.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            token,
        })
    }
}

/// One page is all we read. The answer is clamped by the workspace's ceiling,
/// which is a single-digit number, so a repo with more than this many open PRs
/// and a page-two count would still place the same reviewers.
const PAGE: usize = 100;

/// The comment-pagination bound: past this, `issue_comment_bodies` fails
/// closed rather than pretending it read everything. Two thousand comments on
/// one PR is far outside anything the loop produces.
const MAX_COMMENT_PAGES: usize = 20;

/// The verdict labels the loop maintains on a PR. Exactly one is present after
/// a posted verdict — adding one means removing the other two, or a PR ends up
/// simultaneously approved and changes-requested the first time a verdict
/// changes.
pub(crate) const VERDICT_LABELS: [&str; 3] = [
    "loop-approved",
    "loop-changes-requested",
    "needs-human-review",
];

/// The label a recorded verdict puts on a PR — the same mapping
/// `jobs::record_verdict` validates against. `None` for `skipped`, which posts
/// nothing, and for anything unrecognised: restoration (MAIN-476 AC-3) only
/// ever re-applies a label this deployment's own verdict path could have set.
pub fn verdict_label(verdict: &str) -> Option<&'static str> {
    match verdict {
        "approved" => Some("loop-approved"),
        "changes_requested" => Some("loop-changes-requested"),
        "needs_human" => Some("needs-human-review"),
        _ => None,
    }
}

impl GithubForge {
    /// Deliver a review verdict to the PR: the `Loop review of <sha>` comment
    /// and the matching label, replacing whichever verdict label was there.
    ///
    /// This USED to be the agent's job — a sequence of `gh` calls the skill
    /// asked it to perform, which is the last place a mood could misformat the
    /// comment or forget a label (NG-4 of MAIN-448, overturned 2026-08-08).
    /// Code posts now; the agent only concludes.
    ///
    /// An error is an error: an unposted verdict must fail the caller loudly,
    /// because a verdict recorded in the database but missing from the PR is
    /// invisible to every human working in GitHub.
    pub async fn post_verdict(
        &self,
        repo: &Repo,
        pr: u64,
        head_sha: &str,
        label: &str,
        body: &str,
        forced: bool,
    ) -> anyhow::Result<()> {
        // Idempotent per (PR, head, verdict) — MAIN-477 AC-1. Redelivery of the
        // same conclusion (a retry, an outage replay, a clobber recovery) must
        // not stack near-identical comments; the labels are still re-asserted
        // below, because they are cheap and self-deduplicating. A FAILED
        // pre-check falls through to posting: a flaky read must not block
        // delivery, and the duplicate is the failure that costs less.
        let duplicate = match self.delivered_facts(repo, pr).await {
            Ok((comments, labels)) => {
                already_delivered(&comments, &labels, head_sha, label, forced)
            }
            Err(e) => {
                tracing::warn!(pr, error = %e, "verdict dedupe pre-check failed — posting anyway");
                false
            }
        };
        if !duplicate {
            self.issue_comment(repo, pr, &format!("Loop review of {head_sha}\n\n{body}"))
                .await?;
        }
        self.replace_verdict_label(repo, pr, label).await
    }

    /// The two facts the verdict dedupe reads: every comment body on the PR —
    /// `issue_comment_bodies` walks ALL pages, so the newest comment, which is
    /// the one being looked for, is never off the end of page one — and the
    /// PR's current label names.
    async fn delivered_facts(
        &self,
        repo: &Repo,
        pr: u64,
    ) -> anyhow::Result<(Vec<String>, Vec<String>)> {
        let comments = self.issue_comment_bodies(repo, pr).await?;
        let issue = self.get_json(self.issue_base(repo, pr)).await?;
        let labels = issue
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok((comments, labels))
    }

    fn issue_base(&self, repo: &Repo, pr: u64) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/issues/{pr}",
            repo.owner, repo.name
        )
    }

    async fn issue_comment(&self, repo: &Repo, pr: u64, body: &str) -> anyhow::Result<()> {
        self.send(
            self.http
                .post(format!("{}/comments", self.issue_base(repo, pr)))
                .json(&serde_json::json!({ "body": body })),
        )
        .await
    }

    async fn replace_verdict_label(&self, repo: &Repo, pr: u64, label: &str) -> anyhow::Result<()> {
        let base = self.issue_base(repo, pr);
        for old in VERDICT_LABELS.iter().filter(|l| **l != label) {
            // 404 here means "was not set", which is the desired state, not a
            // failure.
            let resp = self
                .authed(self.http.delete(format!("{base}/labels/{old}")))
                .send()
                .await?;
            if !resp.status().is_success() && resp.status().as_u16() != 404 {
                anyhow::bail!("removing label {old}: {}", resp.status());
            }
        }
        self.send(
            self.http
                .post(format!("{base}/labels"))
                .json(&serde_json::json!({ "labels": [label] })),
        )
        .await?;
        Ok(())
    }

    /// GET one JSON document, with the same error shape as `send`.
    async fn get_json(&self, url: String) -> anyhow::Result<serde_json::Value> {
        let resp = self.authed(self.http.get(url)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "{} {}",
                status.as_u16(),
                body.trim().chars().take(300).collect::<String>()
            );
        }
        Ok(resp.json().await?)
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "nook-control")
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> anyhow::Result<()> {
        let resp = self.authed(rb).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "{} {}",
                status.as_u16(),
                body.trim().chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Forge for GithubForge {
    async fn prs_needing_review(&self, repo: &Repo) -> anyhow::Result<Vec<PullRequest>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/pulls?state=open&per_page={PAGE}",
            repo.owner, repo.name
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            // GitHub refuses a request with no User-Agent.
            .header("User-Agent", "nook-control")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "{} {}",
                status.as_u16(),
                body.trim().chars().take(300).collect::<String>()
            );
        }
        let prs: Vec<serde_json::Value> = resp.json().await?;
        Ok(needing_review(&prs))
    }

    async fn pr_details(&self, repo: &Repo, number: u64) -> anyhow::Result<PrDetails> {
        let json = self
            .get_json(format!(
                "https://api.github.com/repos/{}/{}/pulls/{number}",
                repo.owner, repo.name
            ))
            .await?;
        Ok(PrDetails {
            // `null` while GitHub is still computing — kept as unknown.
            mergeable: json.get("mergeable").and_then(|v| v.as_bool()),
            body: json
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default()
                .to_string(),
            merge_state: merge_state(&json),
        })
    }

    async fn merged_prs_since(
        &self,
        repo: &Repo,
        since: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<MergedPr>> {
        // Sorted by update time so the window's PRs are at the front — one page
        // of a hundred is a week's merges for any repo this loop runs on, and
        // the join it feeds is a backstop for cards whose `pr_url` path already
        // covers the ordinary case.
        let json = self
            .get_json(format!(
                "https://api.github.com/repos/{}/{}/pulls\
                 ?state=closed&sort=updated&direction=desc&per_page={PAGE}",
                repo.owner, repo.name
            ))
            .await?;
        let prs = json
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("closed pull requests: expected an array"))?;
        Ok(merged_since(prs, since))
    }

    async fn default_branch_ci(&self, repo: &Repo) -> anyhow::Result<Option<CiRun>> {
        // Two reads, because the default branch is a repository fact we must
        // not guess: this fleet's own repo is `main`, and a workspace pointing
        // at one whose trunk is `master` or `develop` would otherwise be
        // measured against a branch that does not exist — an empty answer that
        // reads as green forever.
        let repo_json = self
            .get_json(format!(
                "https://api.github.com/repos/{}/{}",
                repo.owner, repo.name
            ))
            .await?;
        let branch = repo_json
            .get("default_branch")
            .and_then(|b| b.as_str())
            .ok_or_else(|| anyhow::anyhow!("repository carries no default_branch"))?
            .to_string();
        // `status=completed` is what makes AC-4 structural rather than a filter
        // we could forget: an in-progress run is never in this answer.
        // Built through `Url` rather than formatted, because a branch name may
        // contain `/` and a hand-built query would silently truncate it.
        let url = reqwest::Url::parse_with_params(
            &format!(
                "https://api.github.com/repos/{}/{}/actions/runs",
                repo.owner, repo.name
            ),
            &[
                ("branch", branch.as_str()),
                ("status", "completed"),
                ("per_page", "1"),
            ],
        )?;
        let runs = self.get_json(url.to_string()).await?;
        Ok(latest_completed_run(&runs, &branch))
    }

    async fn issue_comment_bodies(&self, repo: &Repo, number: u64) -> anyhow::Result<Vec<String>> {
        // ALL pages, not page one. GitHub returns issue comments OLDEST-first
        // with no way to ask for the tail, and the marker this feeds sits on
        // the newest comments — a one-page read would make the dedupe fail
        // PERMANENTLY on a PR with more than a hundred comments, posting a
        // fresh comment every pass. The cap fails CLOSED: a thread too big to
        // read fully is an error, and the caller then posts nothing rather
        // than risking a repeat it cannot rule out.
        let mut bodies = Vec::new();
        for page in 1..=MAX_COMMENT_PAGES {
            let json = self
                .get_json(format!(
                    "{}/comments?per_page={PAGE}&page={page}",
                    self.issue_base(repo, number)
                ))
                .await?;
            let arr = json
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("comments: expected an array"))?;
            let count = arr.len();
            bodies.extend(
                arr.iter()
                    .filter_map(|c| c.get("body").and_then(|b| b.as_str()))
                    .map(str::to_string),
            );
            if count < PAGE {
                return Ok(bodies);
            }
        }
        anyhow::bail!(
            "more than {} comments — cannot prove a marker absent",
            MAX_COMMENT_PAGES * PAGE
        )
    }

    async fn comment(&self, repo: &Repo, number: u64, body: &str) -> anyhow::Result<()> {
        self.issue_comment(repo, number, body).await
    }

    async fn set_verdict_label(&self, repo: &Repo, number: u64, label: &str) -> anyhow::Result<()> {
        self.replace_verdict_label(repo, number, label).await
    }
}

/// Which of a repository's open PRs a reviewer would actually pick up.
///
/// **Drafts are out, everything else is in.** `nook-review` skips a draft
/// outright, so counting one would place a reviewer with nothing to do; every
/// other open PR is at least a candidate.
///
/// It is deliberately no finer than that. The skill also skips a PR it has
/// already reviewed at the current head — but establishing that means reading
/// every PR's comments, and being WRONG in that direction stops reviewers that
/// should be running. An over-count costs an idle agent doing a no-op pass; an
/// under-count is a repo that silently goes unreviewed, which AC-3 names as the
/// failure to design against. So the cheap, inclusive definition wins.
fn needing_review(prs: &[serde_json::Value]) -> Vec<PullRequest> {
    prs.iter()
        .filter(|pr| pr.get("draft").and_then(|d| d.as_bool()) != Some(true))
        .filter_map(|pr| {
            Some(PullRequest {
                number: pr.get("number")?.as_u64()?,
                // A PR whose head we cannot read cannot be compared against its
                // last run, so it is dropped rather than guessed at: an item we
                // cannot tell has changed would re-run on every pass forever.
                head_sha: pr.get("head")?.get("sha")?.as_str()?.to_string(),
                labels: pr
                    .get("labels")
                    .and_then(|l| l.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|l| l.get("name")?.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// How a PR ended, from the PR document itself (MAIN-491 AC-9).
///
/// `merged` is the authority and it is checked FIRST: a merged PR is also
/// `state: "closed"`, so reading the state field first would call every merge a
/// closed-unmerged one. An absent `merged` on a closed PR is the honest
/// reading of a shape we do not recognise — closed, and not provably merged —
/// which routes it to a human rather than to Done.
fn merge_state(pr: &serde_json::Value) -> MergeState {
    if pr.get("merged").and_then(|m| m.as_bool()) == Some(true) {
        return MergeState::Merged;
    }
    match pr.get("state").and_then(|s| s.as_str()) {
        Some("closed") => MergeState::ClosedUnmerged,
        _ => MergeState::Open,
    }
}

/// The merged PRs in a closed-PR listing, newest merge first.
///
/// The list endpoint carries `merged_at` but not `merged`, and a non-null
/// `merged_at` is exactly what "merged" means there — a closed-unmerged PR has
/// `merged_at: null` and simply falls out.
fn merged_since(prs: &[serde_json::Value], since: chrono::DateTime<chrono::Utc>) -> Vec<MergedPr> {
    let mut merged: Vec<MergedPr> = prs
        .iter()
        .filter_map(|pr| {
            let merged_at = pr
                .get("merged_at")?
                .as_str()?
                .parse::<chrono::DateTime<chrono::Utc>>()
                .ok()?;
            if merged_at < since {
                return None;
            }
            Some(MergedPr {
                number: pr.get("number")?.as_u64()?,
                body: pr
                    .get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or_default()
                    .to_string(),
                merged_at,
            })
        })
        .collect();
    merged.sort_by_key(|m| std::cmp::Reverse(m.merged_at));
    merged
}

/// The one run in a `?status=completed&per_page=1` answer, as [`CiRun`].
///
/// `None` for an empty list, which is the repo with no CI, no runs on this
/// branch yet, or Actions disabled — all of them "nothing to read" rather than
/// "everything is fine", and all of them dispatch (MAIN-543 AC-3). A run whose
/// `conclusion` is `null` is dropped for the same reason: the forge answered
/// with a shape we cannot read as a verdict, and a guess in either direction is
/// worse than no answer.
fn latest_completed_run(json: &serde_json::Value, branch: &str) -> Option<CiRun> {
    let run = json.get("workflow_runs")?.as_array()?.first()?;
    Some(CiRun {
        branch: branch.to_string(),
        workflow: run
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("CI")
            .to_string(),
        conclusion: run.get("conclusion")?.as_str()?.to_string(),
        url: run
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string(),
        head_sha: run
            .get("head_sha")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// How long a count stands before it is asked for again.
///
/// The reconcile pass runs every ten seconds and there is one forge call per
/// workspace, so without this a fleet of twenty repos would spend its whole
/// rate limit on a question whose answer changes when somebody opens a PR. A
/// minute is the delay between opening a PR and a reviewer starting, which is
/// the right order for work a human is about to wait on anyway.
const TTL: Duration = Duration::from_secs(60);

/// What we last learned about one workspace.
struct Cached {
    /// The last LIST the forge actually answered. `None` means it has never
    /// answered — a fresh boot into an outage.
    ///
    /// The list, not a count, because a per-PR wakeup needs the items and a
    /// second cache holding the same answer in a different shape is how the two
    /// come to disagree. The count is `.len()`.
    items: Option<Vec<PullRequest>>,
    fetched: Instant,
    /// Whether the last attempt failed, so the log speaks once per transition
    /// rather than once per pass.
    failing: bool,
}

/// The reconciler's view of review demand: cached counts, and a failure policy.
///
/// **A forge failure is not a scale-down (AC-4).** An outage that read as "no
/// open PRs" would stop every reviewer in the fleet, and they would stay
/// stopped for as long as it lasted — the loudest possible failure, arriving as
/// silence. So an error returns the last count we were told, and a fleet that
/// has never had an answer falls back to the ceiling, which is what a
/// deployment with no forge at all runs (AC-6).
pub struct ReviewDemand {
    forge: Option<Box<dyn Forge>>,
    ttl: Duration,
    seen: Mutex<HashMap<WorkspaceId, Cached>>,
}

impl ReviewDemand {
    /// The real thing: GitHub if this deployment has a token, otherwise a
    /// [`ReviewDemand`] that answers `None` for every workspace — which is
    /// exactly the pre-forge behaviour.
    pub fn from_env() -> Self {
        match GithubForge::from_env() {
            Some(f) => {
                tracing::info!(
                    "forge: GitHub (fleet token present) — review loops scale to open PRs"
                );
                Self::new(Some(Box::new(f)), TTL)
            }
            None => {
                tracing::info!(
                    "forge: none (no NOOK_GH_TOKEN) — review loops run at their declared ceiling"
                );
                Self::new(None, TTL)
            }
        }
    }

    pub fn new(forge: Option<Box<dyn Forge>>, ttl: Duration) -> Self {
        Self {
            forge,
            ttl,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// How many open PRs this workspace has, or `None` when there is nothing to
    /// ask.
    ///
    /// `None` is the answer for a workspace with no remote, a remote that is not
    /// a forge this build knows, a deployment with no token, and a forge that
    /// has failed every time we have asked. All four mean the same thing to the
    /// caller — *we do not know, so run what the repo declared* — and collapsing
    /// them here keeps that judgement in one place instead of four.
    pub async fn open_prs(
        &self,
        workspace: WorkspaceId,
        remote: Option<&str>,
        token: Option<&str>,
    ) -> Option<u32> {
        Some(self.prs(workspace, remote, token).await?.len() as u32)
    }

    /// The open PRs themselves — what a per-PR wakeup converges on. Same cache,
    /// same failure policy; `open_prs` is this, counted.
    pub async fn prs(
        &self,
        workspace: WorkspaceId,
        remote: Option<&str>,
        token: Option<&str>,
    ) -> Option<Vec<PullRequest>> {
        // The workspace's own token OUTRANKS the deployment's forge: a tenant
        // that configured its identity asks GitHub as itself (MAIN-456). The
        // deployment forge — fleet env, or a test's injected fake — is the
        // fallback, and no token plus no forge is the pre-forge "unknown".
        let own;
        let forge: &dyn Forge = match token {
            Some(t) => {
                own = GithubForge::from_token(t);
                &own
            }
            None => self.forge.as_deref()?,
        };
        let repo = github_repo(remote?)?;

        if let Some(fresh) = self.fresh(workspace) {
            return fresh;
        }
        match forge.prs_needing_review(&repo).await {
            Ok(items) => {
                self.record_ok(workspace, items.clone(), &repo);
                Some(items)
            }
            Err(e) => self.record_err(workspace, &repo, &e),
        }
    }

    /// The deployment forge itself, for callers that need a per-PR read with
    /// the same credential and the same test seam (MAIN-459: the outcome
    /// call's `Closes` check). `None` is the no-forge deployment.
    pub fn forge(&self) -> Option<&dyn Forge> {
        self.forge.as_deref()
    }

    /// Forget one workspace's cached answer, so the next ask hits the forge.
    ///
    /// The manual path calls this: a person clicking "review now" right after
    /// opening a PR must not be told "nothing owed" by a list fetched up to a
    /// TTL ago. The reconciler never calls it — its cadence is what the TTL is
    /// FOR.
    pub fn forget(&self, workspace: WorkspaceId) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(&workspace);
        }
    }

    /// The cached answer if it is still inside the TTL. The outer `Option` is
    /// "we have something to say", the inner one is what we would say.
    fn fresh(&self, workspace: WorkspaceId) -> Option<Option<Vec<PullRequest>>> {
        let seen = self.seen.lock().ok()?;
        let cached = seen.get(&workspace)?;
        (cached.fetched.elapsed() < self.ttl).then(|| cached.items.clone())
    }

    fn record_ok(&self, workspace: WorkspaceId, items: Vec<PullRequest>, repo: &Repo) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        let was_failing = seen.get(&workspace).is_some_and(|c| c.failing);
        if was_failing {
            tracing::info!(
                %workspace, repo = %format!("{}/{}", repo.owner, repo.name),
                open_prs = items.len(),
                "forge recovered — review loops are scaling to the real count again"
            );
        }
        seen.insert(
            workspace,
            Cached {
                items: Some(items),
                fetched: Instant::now(),
                failing: false,
            },
        );
    }

    /// Report the failure ONCE, keep the last known count, and stamp the clock
    /// so a hard-down forge is asked once per TTL rather than once per pass.
    fn record_err(
        &self,
        workspace: WorkspaceId,
        repo: &Repo,
        error: &anyhow::Error,
    ) -> Option<Vec<PullRequest>> {
        let Ok(mut seen) = self.seen.lock() else {
            return None;
        };
        let last = seen.get(&workspace).and_then(|c| c.items.clone());
        let was_failing = seen.get(&workspace).is_some_and(|c| c.failing);
        if !was_failing {
            tracing::warn!(
                %workspace, repo = %format!("{}/{}", repo.owner, repo.name),
                error = %error,
                last_known_open_prs = ?last.as_ref().map(Vec::len),
                "forge unreachable — holding the last known review demand; \
                 this is NOT a scale-down"
            );
        }
        seen.insert(
            workspace,
            Cached {
                items: last.clone(),
                fetched: Instant::now(),
                failing: true,
            },
        );
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use uuid::Uuid;

    fn ws() -> WorkspaceId {
        WorkspaceId(Uuid::from_u128(7))
    }

    fn runs(body: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "workflow_runs": [body] })
    }

    #[test]
    fn a_completed_run_reads_as_its_conclusion() {
        let got = latest_completed_run(
            &runs(serde_json::json!({
                "name": "CI",
                "conclusion": "failure",
                "html_url": "https://github.com/acme/api/actions/runs/9",
                "head_sha": "deadbeef",
            })),
            "main",
        )
        .expect("a run");
        assert_eq!(got.conclusion, "failure");
        assert_eq!(got.workflow, "CI");
        assert_eq!(got.branch, "main");
        assert_eq!(got.head_sha, "deadbeef");
    }

    #[test]
    fn nothing_to_read_is_none_rather_than_a_verdict() {
        // A repo with no CI, or none completed on this branch yet.
        assert_eq!(
            latest_completed_run(&serde_json::json!({ "workflow_runs": [] }), "main"),
            None
        );
        // Actions disabled answers a shape with no list at all.
        assert_eq!(latest_completed_run(&serde_json::json!({}), "main"), None);
        // A run the forge reports with no conclusion is not a verdict either.
        assert_eq!(
            latest_completed_run(&runs(serde_json::json!({ "conclusion": null })), "main"),
            None
        );
    }

    /// A forge whose answers the test dictates, counting how often it was asked.
    struct Fake {
        answers: Mutex<Vec<anyhow::Result<u32>>>,
        calls: Arc<AtomicUsize>,
    }

    impl Fake {
        /// Not `new`: it hands back the call counter alongside the forge, and a
        /// `new` that returns a tuple is the one clippy asks about.
        fn answering(answers: Vec<anyhow::Result<u32>>) -> (Box<dyn Forge>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Box::new(Fake {
                    answers: Mutex::new(answers),
                    calls: calls.clone(),
                }),
                calls,
            )
        }
    }

    #[async_trait::async_trait]
    impl Forge for Fake {
        async fn prs_needing_review(&self, _repo: &Repo) -> anyhow::Result<Vec<PullRequest>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut a = self.answers.lock().unwrap();
            if a.is_empty() {
                anyhow::bail!("fake ran out of answers");
            }
            // The fake still answers in COUNTS, because that is what the sizing
            // tests are about; the items are synthesized so the one real
            // implementation of "count" (the trait's default) is what runs.
            a.remove(0).map(|n| {
                (0..n)
                    .map(|i| PullRequest {
                        number: i as u64 + 1,
                        head_sha: format!("sha{i}"),
                        labels: vec![],
                    })
                    .collect()
            })
        }
    }

    const REMOTE: &str = "git@github.com:nook-os/nook-os.git";

    #[test]
    fn every_url_shape_the_fleet_stores_resolves_to_one_repo() {
        let expected = Repo {
            owner: "nook-os".into(),
            name: "nook-os".into(),
        };
        for url in [
            "git@github.com:nook-os/nook-os.git",
            "https://github.com/nook-os/nook-os.git",
            "https://github.com/nook-os/nook-os",
            "ssh://git@github.com/nook-os/nook-os.git",
            "https://user:pass@github.com/nook-os/nook-os.git",
            "https://github.com/nook-os/nook-os/",
        ] {
            assert_eq!(github_repo(url).as_ref(), Some(&expected), "{url}");
        }
    }

    /// AC-6's detection, at the level it is actually made. A local path is what
    /// the dogfood workspace has, and it must read as "no forge" rather than as
    /// a repo we then fail to reach.
    #[test]
    fn anything_that_is_not_a_github_repo_is_no_forge() {
        for url in [
            "/workspace/nook-dogfood.git",
            "git.hein.network:repositories/nookos.git",
            "https://gitlab.com/nook-os/nook-os.git",
            "https://github.com/nook-os",
            "https://github.com/nook-os/nook-os/tree/main",
            "",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    /// Drafts are the one exclusion, and it is the skill's own rule: it skips
    /// them, so a reviewer placed for one would have nothing to do.
    #[test]
    fn drafts_do_not_count_and_everything_else_does() {
        let prs = serde_json::json!([
            { "number": 1, "draft": false, "head": { "sha": "aaa" },
              "labels": [ { "name": "loop-approved" }, { "name": "bug" } ] },
            { "number": 2, "draft": true, "head": { "sha": "bbb" } },
            // Absent `draft` is not a draft — an older API shape must not read
            // as one and quietly shrink the queue.
            { "number": 3, "head": { "sha": "ccc" } },
            // No head sha: undroppable otherwise, because nothing could ever
            // say whether it had changed.
            { "number": 4 },
        ]);
        // Numbers AND heads, because the head is what a per-PR wakeup compares;
        // labels ride along for the hygiene pass, absent reading as none.
        assert_eq!(
            needing_review(prs.as_array().unwrap()),
            vec![
                PullRequest {
                    number: 1,
                    head_sha: "aaa".into(),
                    labels: vec!["loop-approved".into(), "bug".into()],
                },
                PullRequest {
                    number: 3,
                    head_sha: "ccc".into(),
                    labels: vec![],
                },
            ]
        );
        assert!(needing_review(&[]).is_empty());
    }

    /// MAIN-491 AC-9. `merged` outranks `state`, because a merged PR is also a
    /// closed one — reading `state` first would call every merge a rejection
    /// and route finished work to a human instead of to Done.
    #[test]
    fn how_a_pr_ended_is_read_from_the_pr_and_merged_wins() {
        let s = |v: serde_json::Value| merge_state(&v);
        assert_eq!(
            s(serde_json::json!({ "state": "closed", "merged": true })),
            MergeState::Merged
        );
        assert_eq!(
            s(serde_json::json!({ "state": "closed", "merged": false })),
            MergeState::ClosedUnmerged
        );
        assert_eq!(
            s(serde_json::json!({ "state": "open", "merged": false })),
            MergeState::Open
        );
        // An unrecognised shape is not a merge. Closed-and-not-provably-merged
        // raises a hand; anything else is simply unfinished.
        assert_eq!(
            s(serde_json::json!({ "state": "closed" })),
            MergeState::ClosedUnmerged
        );
        assert_eq!(s(serde_json::json!({})), MergeState::Open);
    }

    /// The window is a filter over `merged_at`, and a closed-unmerged PR has
    /// none — so it falls out rather than needing a second test.
    #[test]
    fn the_merged_listing_keeps_only_merges_inside_the_window() {
        let since = "2026-08-02T00:00:00Z".parse().unwrap();
        let prs = serde_json::json!([
            { "number": 1, "merged_at": "2026-08-08T10:00:00Z", "body": "Closes MAIN-1" },
            { "number": 2, "merged_at": null, "body": "rejected" },
            { "number": 3, "merged_at": "2026-07-01T10:00:00Z", "body": "too old" },
            // Newer than #1, so it sorts first — the caller reads newest-first.
            { "number": 4, "merged_at": "2026-08-09T10:00:00Z" },
        ]);
        let got = merged_since(prs.as_array().unwrap(), since);
        assert_eq!(got.iter().map(|m| m.number).collect::<Vec<_>>(), vec![4, 1]);
        assert_eq!(got[1].body, "Closes MAIN-1");
        assert_eq!(got[0].body, "", "an absent body reads as empty, not a drop");
    }

    #[tokio::test]
    async fn no_forge_configured_answers_none_without_asking_anything() {
        let (fake, calls) = Fake::answering(vec![Ok(5)]);
        let d = ReviewDemand::new(Some(fake), TTL);
        // No remote at all, and a remote that is not a forge.
        assert_eq!(d.open_prs(ws(), None, None).await, None);
        assert_eq!(
            d.open_prs(ws(), Some("/workspace/local.git"), None).await,
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_deployment_with_no_token_never_asks() {
        let d = ReviewDemand::new(None, TTL);
        assert_eq!(d.open_prs(ws(), Some(REMOTE), None).await, None);
    }

    #[tokio::test]
    async fn the_count_is_cached_for_its_ttl() {
        let (fake, calls) = Fake::answering(vec![Ok(4), Ok(9)]);
        let d = ReviewDemand::new(Some(fake), Duration::from_secs(300));
        assert_eq!(d.open_prs(ws(), Some(REMOTE), None).await, Some(4));
        assert_eq!(
            d.open_prs(ws(), Some(REMOTE), None).await,
            Some(4),
            "still cached"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one call, not one per pass"
        );
    }

    #[tokio::test]
    async fn an_expired_entry_is_asked_again() {
        let (fake, calls) = Fake::answering(vec![Ok(4), Ok(9)]);
        let d = ReviewDemand::new(Some(fake), Duration::ZERO);
        assert_eq!(d.open_prs(ws(), Some(REMOTE), None).await, Some(4));
        assert_eq!(d.open_prs(ws(), Some(REMOTE), None).await, Some(9));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// AC-4, the load-bearing one. The forge going down must not read as an
    /// empty queue, because an empty queue stops every reviewer in the fleet.
    #[tokio::test]
    async fn a_forge_failure_holds_the_last_count_instead_of_reporting_zero() {
        let (fake, _) = Fake::answering(vec![
            Ok(3),
            Err(anyhow::anyhow!("503 upstream")),
            Err(anyhow::anyhow!("503 upstream")),
            Ok(1),
        ]);
        let d = ReviewDemand::new(Some(fake), Duration::ZERO);
        assert_eq!(d.open_prs(ws(), Some(REMOTE), None).await, Some(3));
        assert_eq!(
            d.open_prs(ws(), Some(REMOTE), None).await,
            Some(3),
            "outage holds"
        );
        assert_eq!(
            d.open_prs(ws(), Some(REMOTE), None).await,
            Some(3),
            "still holds"
        );
        assert_eq!(
            d.open_prs(ws(), Some(REMOTE), None).await,
            Some(1),
            "recovered"
        );
    }

    /// An outage that starts before we ever get an answer falls back to "we do
    /// not know", which the caller turns into the declared ceiling — the same
    /// thing a deployment with no forge runs. Booting into an outage must not
    /// mean booting with no reviewers.
    #[tokio::test]
    async fn a_failure_with_nothing_cached_is_unknown_not_zero() {
        let (fake, _) = Fake::answering(vec![Err(anyhow::anyhow!("network down"))]);
        let d = ReviewDemand::new(Some(fake), Duration::ZERO);
        assert_eq!(d.open_prs(ws(), Some(REMOTE), None).await, None);
    }

    /// Two processes read the fleet credential and they must read the same
    /// variables. There is no crate both can share, so the node's source is the
    /// authority and this test is the link — a rename there fails here.
    #[test]
    fn the_token_variables_match_the_nodes() {
        let node = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../nook-node/src/config.rs"
        ))
        .expect("the node's config must be readable");
        let list = node
            .split("pub fn fleet_gh_token()")
            .nth(1)
            .expect("nook-node must still have fleet_gh_token");
        for var in TOKEN_VARS {
            assert!(
                list.contains(var),
                "the node no longer reads {var}; the control plane's forge would \
                 look for a credential the fleet does not provision"
            );
        }
    }
}

/// Has THIS conclusion already been delivered to the PR? True only when both
/// halves agree: our own `Loop review of <head>` comment exists AND the PR
/// currently carries the same verdict label. A changed verdict at the same
/// head (needs_human after changes_requested, say) posts normally, because
/// the label half differs (MAIN-477 AC-1).
pub fn already_delivered(
    comment_bodies: &[String],
    labels: &[String],
    head_sha: &str,
    label: &str,
    forced: bool,
) -> bool {
    // A FORCED re-review always posts (MAIN-473). Forcing exists for the case
    // where the evidence went stale while the conclusion stood — the head has
    // not moved and the verdict word is the same, which is precisely what this
    // dedupe reads as "already said". Suppressing it would leave the human who
    // asked for the re-review looking at silence, and that is the regression
    // this bypass exists to prevent.
    if forced {
        return false;
    }
    let marker = format!("Loop review of {head_sha}");
    if !comment_bodies.iter().any(|b| b.starts_with(&marker)) {
        return false;
    }
    // The label tells apart "same verdict" (ours present → duplicate) from
    // "verdict CHANGED at this head" (a DIFFERENT verdict label present →
    // post normally). NO verdict label at all is the mid-delivery failure —
    // comment landed, labels did not — and reposting the comment there is
    // the exact accretion this exists to stop; the caller re-asserts labels
    // unconditionally, which is all that case still needs.
    let ours = labels.iter().any(|l| l == label);
    let another = labels
        .iter()
        .any(|l| VERDICT_LABELS.contains(&l.as_str()) && l != label);
    ours || !another
}

#[cfg(test)]
mod verdict_dedupe_tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// MAIN-473: a FORCED re-review is never a duplicate. Its whole purpose is
    /// the unchanged-head, unchanged-verdict case — stale evidence under a
    /// standing conclusion — so the dedupe that is right for every other
    /// redelivery would make forcing do nothing at all, silently.
    #[test]
    fn a_forced_re_review_always_posts() {
        let comments = v(&["Loop review of abc123\n\nSummary: fine"]);
        let approved = v(&["loop-approved"]);
        assert!(
            already_delivered(&comments, &approved, "abc123", "loop-approved", false),
            "unforced, this is the duplicate AC-1 suppresses"
        );
        assert!(
            !already_delivered(&comments, &approved, "abc123", "loop-approved", true),
            "the same facts, forced, must post"
        );
    }

    /// MAIN-477 AC-1/AC-4: same head + same verdict is a duplicate; a new
    /// head or a changed verdict is not.
    #[test]
    fn redelivery_is_a_duplicate_but_new_facts_are_not() {
        let comments = v(&["Loop review of abc123\n\nSummary: fine"]);
        let approved = v(&["loop-approved"]);
        assert!(already_delivered(
            &comments,
            &approved,
            "abc123",
            "loop-approved",
            false
        ));
        // A push: new head, no comment for it yet.
        assert!(!already_delivered(
            &comments,
            &approved,
            "def456",
            "loop-approved",
            false
        ));
        // Same head, but the verdict CHANGED since — the label half disagrees.
        assert!(!already_delivered(
            &comments,
            &approved,
            "abc123",
            "loop-changes-requested",
            false
        ));
        // Nothing delivered at all.
        assert!(!already_delivered(
            &v(&[]),
            &v(&[]),
            "abc123",
            "loop-approved",
            false
        ));
        // The MID-DELIVERY failure: comment landed, labels did not. Reposting
        // the comment is the accretion this exists to stop — the caller
        // re-asserts labels either way.
        assert!(already_delivered(
            &comments,
            &v(&[]),
            "abc123",
            "loop-approved",
            false
        ));
        // An unrelated (non-verdict) label does not read as a changed verdict.
        assert!(already_delivered(
            &comments,
            &v(&["enhancement"]),
            "abc123",
            "loop-approved",
            false
        ));
    }
}
