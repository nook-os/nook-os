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
}

/// What the reconciler needs to know about a repository's review queue.
///
/// One operation, because one is all this card needs and a trait with methods
/// nobody calls is a shape guess rather than a seam. Build status, check runs
/// and mergeability are deliberately absent (NG-2).
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
            http: reqwest::Client::new(),
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

/// The verdict labels the loop maintains on a PR. Exactly one is present after
/// a posted verdict — adding one means removing the other two, or a PR ends up
/// simultaneously approved and changes-requested the first time a verdict
/// changes.
const VERDICT_LABELS: [&str; 3] = [
    "loop-approved",
    "loop-changes-requested",
    "needs-human-review",
];

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
    ) -> anyhow::Result<()> {
        let base = format!(
            "https://api.github.com/repos/{}/{}/issues/{pr}",
            repo.owner, repo.name
        );
        self.send(
            self.http.post(format!("{base}/comments")).json(
                &serde_json::json!({ "body": format!("Loop review of {head_sha}\n\n{body}") }),
            ),
        )
        .await?;
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
            })
        })
        .collect()
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
            { "number": 1, "draft": false, "head": { "sha": "aaa" } },
            { "number": 2, "draft": true, "head": { "sha": "bbb" } },
            // Absent `draft` is not a draft — an older API shape must not read
            // as one and quietly shrink the queue.
            { "number": 3, "head": { "sha": "ccc" } },
            // No head sha: undroppable otherwise, because nothing could ever
            // say whether it had changed.
            { "number": 4 },
        ]);
        // Numbers AND heads, because the head is what a per-PR wakeup compares.
        assert_eq!(
            needing_review(prs.as_array().unwrap()),
            vec![
                PullRequest {
                    number: 1,
                    head_sha: "aaa".into()
                },
                PullRequest {
                    number: 3,
                    head_sha: "ccc".into()
                },
            ]
        );
        assert!(needing_review(&[]).is_empty());
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
