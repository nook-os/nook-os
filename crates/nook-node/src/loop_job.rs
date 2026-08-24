//! Node-side loop-job runner (MAIN-161).
//!
//! On a `RunLoopJob` the node materializes the workspace from a node-local
//! clone cache, cuts a per-job worktree, launches the `claude` runtime in it,
//! and drives the matching skill (`nook-spec` / `nook-epic`) by typing its
//! slash-command at the ticket. Every chunk of the session's output is streamed
//! back as a `JobTranscript` — recorded verbatim, never interpreted (NG-2) —
//! and the session's end (or a timeout / launch failure) is reported as
//! `JobFinished` with a reason or the transcript tail for crash honesty (AC-4).
//!
//! All of this is blocking (git, tmux, a PTY), so `run` is meant to be invoked
//! under `tokio::task::spawn_blocking`, mirroring the git-op pattern in `conn`.

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nook_proto::NodeToControl;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::mpsc::Sender;

use crate::config::NodeConfig;

/// A loop job stops itself here. An hour is long enough for a genuine
/// spec/epic pass and short enough that a wedged session frees the machine the
/// same afternoon rather than holding a worktree open forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How often the wait loop re-checks whether the session is still alive.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// The runtime needs a beat to come up before it can take input. We do not (and
/// must not, NG-2) read its output to decide when it is ready — so wait a fixed
/// moment, then type the skill command.
const SKILL_STARTUP_DELAY: Duration = Duration::from_secs(5);
/// How much of the tail to keep for the finish message.
const TAIL_BYTES: usize = 4096;

/// Everything a `RunLoopJob` carries, moved into the blocking runner.
pub struct LoopJob {
    pub job_id: String,
    pub kind: String,
    /// The pull request a `review` run owns (MAIN-455): what the agent is told
    /// it is reviewing, and the session it resumes.
    pub review_pr_number: Option<u64>,
    /// A human forced this review at an already-verdicted head (MAIN-473);
    /// exported as `NOOK_REVIEW_FORCED` so the skill's skip-check stands
    /// aside for exactly this run.
    pub review_forced: bool,
    /// The workspace's own forge token (MAIN-456); outranks the node's fleet
    /// env when set.
    pub gh_token: Option<String>,
    /// The control plane's advertised API base URL (MAIN-465). The run's
    /// `NOOK_SERVER` when present; absent, this node's own `cfg.server`.
    pub server_url: Option<String>,
    pub target_task_key: String,
    pub repo_url: String,
    pub branch: String,
    /// The human's opening brief (MAIN-231), if the job was seeded with one.
    pub seed: Option<String>,
    /// Exported into the job's session so git authenticates with the
    /// workspace's key (MAIN-367).
    pub workspace_id: Option<String>,
    /// The workspace's pinned key, for the clone cache — which runs before any
    /// session exists and so cannot use the shim `workspace_id` enables.
    pub ssh_key: Option<String>,
    /// The credential the AGENT acts with — scoped to the job's tenant, issued
    /// as its initiator. Without it `nook` inside the agent reads this machine's
    /// login file and acts as whoever last logged in here, in THEIR tenant.
    pub nook_token: Option<String>,
    /// The ports the control plane leased this run (MAIN-552), each exported as
    /// the variable the WORKSPACE named — so `./scripts/dev-up.sh` in the
    /// worktree binds them instead of `docker-compose.yml`'s `${VAR:-default}`
    /// fallbacks, which every other stack on this machine also falls back to.
    ///
    /// This end recognises none of the names and must not: it exports what it
    /// was handed, exactly as `tmux::spawn` does for a session.
    pub ports: Vec<nook_types::LeasedPort>,
    /// Optional listeners that went unleased, exported as
    /// `NOOK_PORTS_UNSATISFIED` — the same name a session gets (MAIN-377), so a
    /// consumer telling "not leased" from "not under nook" reads one variable.
    pub unsatisfied_ports: Vec<String>,
    /// The tenant- and workspace-scoped secret items this run's agent gets
    /// (MAIN-625 AC-6). Already filtered by the control plane, which is the
    /// only end that knows the scopes — a node-scoped item never reaches here.
    pub secrets: Vec<nook_types::SecretEnv>,
    /// The workspaces the run's card names with `@slug` (MAIN-632), each with
    /// its checkout path on THIS node where the control plane found one.
    ///
    /// Mounted READ-ONLY (AC-5) and named in the brief (AC-7). A reference with
    /// no path is a repo this machine does not hold: nothing is mounted and the
    /// brief says so, because a reference is not a placement constraint (NG-2).
    pub references: Vec<nook_types::WorkspaceRef>,
}

/// Worktree directory names of jobs running on this node right now, so
/// `reconcile` can tell a live worktree apart from an orphan left by a crash.
fn running_jobs() -> &'static Mutex<HashSet<String>> {
    static JOBS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Job IDS of the jobs running on this node right now, so the sandbox sweep can
/// tell a live job's container from one a killed node left behind (MAIN-617).
///
/// A second set beside `running_jobs`, not a replacement for it: that one holds
/// WORKTREE DIRECTORY names, which a review run shares across every run of one
/// PR and a build run across every pass of one card (`warm_identity`). A
/// container is named and labelled for the JOB, so only a job id can identify
/// one — and two runs of one PR would otherwise be indistinguishable.
fn running_job_ids() -> &'static Mutex<HashSet<String>> {
    static IDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Reclaim the Docker objects and firewall rules of jobs this node is no longer
/// running (MAIN-617). Best effort and never fatal, exactly like [`reconcile`].
///
/// Beside `reconcile` rather than inside it because they sweep different things
/// for different reasons: `reconcile` deletes worktree DIRECTORIES and its
/// build-worktree exemption is a card-lifecycle rule (MAIN-480), while this
/// removes only what carries the `nook.job` label. Nothing here touches a
/// worktree.
pub fn sweep_job_sandboxes() {
    let running = running_job_ids()
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    crate::sandbox::sweep_orphans(&running);
}

/// How many loop jobs are running here right now (MAIN-505).
///
/// LOOP jobs only, which is the whole distinction the update cordon turns on: a
/// terminal session is tmux's and outlives a restart, so it is not in this set
/// and must never hold an update back (AC-5).
pub fn in_flight() -> u32 {
    running_jobs().lock().map(|s| s.len() as u32).unwrap_or(0)
}

fn note(out: &Sender<NodeToControl>, job_id: &str, content: impl Into<String>) {
    let _ = out.blocking_send(NodeToControl::JobTranscript {
        job_id: job_id.to_string(),
        source: "system".into(),
        content: content.into(),
    });
}

fn drain_notes(out: &Sender<NodeToControl>, job_id: &str, notes: &mut Vec<String>) {
    for content in notes.drain(..) {
        note(out, job_id, content);
    }
}

fn finished(out: &Sender<NodeToControl>, job_id: &str, ok: bool, message: impl Into<String>) {
    let _ = out.blocking_send(NodeToControl::JobFinished {
        job_id: job_id.to_string(),
        ok,
        message: message.into(),
    });
}

/// Report a run the node REFUSED to launch (MAIN-482 AC-6).
///
/// Distinct from `finished(ok=false)` because the board consequence differs: a
/// refused run never reached its agent, so nothing will ever report an outcome
/// for it — and the outcome handler is the only thing that releases the loop's
/// claim. Saying "refused" rather than "failed" is what lets the control plane
/// give the card back instead of leaving it claimed with nothing running.
fn refused(out: &Sender<NodeToControl>, job_id: &str, reason: impl Into<String>) {
    let _ = out.blocking_send(NodeToControl::JobRefused {
        job_id: job_id.to_string(),
        reason: reason.into(),
    });
}

/// Where every control plane's clone cache lives, above the per-cp split.
///
/// Named once because `resources::watched_paths` samples free space on the
/// filesystem holding it (MAIN-618): a second spelling of this path would keep
/// watching the old one if the root ever moved, and the job-cache row would
/// then be silently dropped — the disk gate failing OPEN on exactly the
/// directory it exists to watch.
pub(crate) fn cache_root() -> PathBuf {
    PathBuf::from(crate::config::expand_path("~/.nook/clone-cache"))
}

/// The node-local clone cache root, per control plane so two control planes on
/// one machine never share a mirror (mirrors MAIN-58's per-cp isolation).
fn cache_base(server: &str) -> PathBuf {
    cache_root().join(crate::config::cp_slug(server))
}

/// tmux names and worktree dirs are keyed by a filesystem/tmux-safe slug of the
/// (opaque) job id.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn job_dirname(job_id: &str) -> String {
    sanitize(job_id)
}

/// The worktree directory a REVIEW run works in: stable per (workspace, PR),
/// where every other kind gets a per-job path.
///
/// Stability is the whole point, and it is what per-job paths made impossible:
/// Claude Code buckets its sessions by working directory, so a worktree named
/// after the job id put every run in a brand-new empty bucket and there was
/// never anything to resume — confirmed live, two runs of one PR, two
/// unrelated agent sessions. Safe to share across runs because 0046's unique
/// index means no two live runs ever hold the same PR.
fn review_dirname(workspace_id: &str, pr: u64) -> String {
    sanitize(&format!("review-{workspace_id}-pr{pr}"))
}

/// The agent session a REVIEW run pins on its first pass and resumes on every
/// later one: UUIDv5 over (workspace, PR), so the same PR always names the same
/// session without anything having to be stored or looked up.
fn review_session_id(workspace_id: &str, pr: u64) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("nook-review:{workspace_id}:{pr}").as_bytes(),
    )
    .to_string()
}

/// The worktree directory a BUILD run works in: stable per (workspace, card),
/// for the reason `review_dirname` is stable per PR (MAIN-460 AC-2) — Claude
/// Code buckets its sessions by working directory, so a repair pass in a fresh
/// per-job path could never resume building the thing. Safe to share across
/// runs because 0050's unique index means no two live runs ever hold the card.
fn build_dirname(workspace_id: &str, task_key: &str) -> String {
    sanitize(&format!("build-{workspace_id}-{task_key}"))
}

/// The agent session a BUILD run pins on its first pass and resumes after:
/// UUIDv5 over (workspace, card) — `review_session_id`'s twin (MAIN-460 AC-1).
fn build_session_id(workspace_id: &str, task_key: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("nook-build:{workspace_id}:{task_key}").as_bytes(),
    )
    .to_string()
}

/// Which stable identity this run keeps, if any: `(dirname, session_id)`.
///
/// One decision for both facts, because they MUST agree — the session bucket
/// is keyed on the working directory, so a stable session in a per-job dir
/// (or the reverse) is a warm layer that never warms. `None` means per-job:
/// spec and decompose runs neither resume nor leave a tree behind.
fn warm_identity(
    kind: &str,
    review_pr: Option<u64>,
    workspace_id: Option<&str>,
    task_key: &str,
) -> Option<(String, String)> {
    match (review_pr, workspace_id) {
        (Some(pr), Some(ws)) => Some((review_dirname(ws, pr), review_session_id(ws, pr))),
        (None, Some(ws)) if kind == "build" && !task_key.is_empty() => {
            Some((build_dirname(ws, task_key), build_session_id(ws, task_key)))
        }
        _ => None,
    }
}

/// Does the agent already hold a session under this id?
///
/// Answered from the filesystem — `<config>/projects/*/<id>.jsonl` — because it
/// decides which FLAG the launch gets: `--resume` on a session that exists,
/// `--session-id` to pin one that does not. Guessing wrong is not recoverable
/// after launch: resuming a missing session fails the run, and pinning an
/// existing id collides with it. The scan crosses every project bucket rather
/// than deriving this run's, so the answer does not depend on reproducing
/// Claude Code's path-munging scheme.
/// Where this node keeps its Claude session — the agent's warm transcripts and
/// the loop skills both. Resolved once, because it is now also what the job
/// sandbox mounts (MAIN-611 AC-7), and two answers would mean an agent that
/// resumes a session the node cannot see.
fn claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude")
        })
}

fn agent_session_exists(session_id: &str) -> bool {
    let config = claude_config_dir();
    let file = format!("{session_id}.jsonl");
    let Ok(projects) = std::fs::read_dir(config.join("projects")) else {
        return false;
    };
    projects
        .flatten()
        .any(|bucket| bucket.path().join(&file).is_file())
}

/// Move a session file that refused to resume out of the agent's way, so the
/// derived id can be pinned fresh and the NEXT pass resumes a working session
/// instead of paying the resume-fail-relaunch tax forever. Renamed, not
/// deleted — the transcript may still be worth a human's read.
fn quarantine_agent_session(session_id: &str) {
    let config = claude_config_dir();
    let file = format!("{session_id}.jsonl");
    let Ok(projects) = std::fs::read_dir(config.join("projects")) else {
        return;
    };
    for bucket in projects.flatten() {
        let path = bucket.path().join(&file);
        if path.is_file() {
            let _ = std::fs::rename(&path, bucket.path().join(format!("{session_id}.corrupt")));
        }
    }
}

fn job_tmux_name(job_id: &str) -> String {
    format!("{}job_{}", crate::tmux::SESSION_PREFIX, sanitize(job_id))
}

/// A single directory name for a repo's mirror, e.g. `acme/services` →
/// `acme__services`.
fn repo_slug(repo_url: &str) -> String {
    crate::gitops::repo_path_from_url(repo_url)
        .unwrap_or_else(|| "repo".into())
        .replace('/', "__")
}

/// Ensure a fresh bare mirror of `repo_url` under `base`. Returns the mirror path.
///
/// Takes the workspace's key because THIS is the step a private repo dies on.
/// The job's session gets `GIT_SSH_COMMAND` and `NOOK_WORKSPACE_ID` (MAIN-367),
/// so git typed inside it authenticates — but the mirror is built here, in the
/// node process, before any session exists. Passing `None` meant the node's own
/// generated key, which no private repo authorizes, and the job ended at
/// "preparing workspace" with `Permission denied (publickey)` no matter how
/// correctly the credential was pinned.
///
/// `None` still means the node's own reach, which is right for a public repo or
/// a local path. The key lives in a 0600 file for the length of the git command
/// and `TransientKey`'s Drop removes it — including on the error paths, which is
/// why the guard is bound rather than passed inline.
/// The clone cache for this repo, refreshed — but never created (MAIN-406).
///
/// Fails rather than cloning when the mirror is absent. That is the difference
/// between "run a review where the repo already lives" and "fetch any repo a
/// job names onto a shared machine", and it is why review does not reuse
/// `ensure_mirror_in`.
fn existing_mirror_in(
    base: &Path,
    repo_url: &str,
    ssh_key: Option<&str>,
    own_worktree: &Path,
    notes: &mut Vec<String>,
) -> Result<PathBuf, String> {
    let cache = base.join(format!("{}.git", repo_slug(repo_url)));
    if !cache.join("HEAD").exists() {
        return Err(format!(
            "no clone cache at {} for {repo_url}",
            cache.display()
        ));
    }
    adopt_mirror(&cache, repo_url, ssh_key, own_worktree, notes)?;
    Ok(cache)
}

fn ensure_mirror_in(
    base: &Path,
    repo_url: &str,
    ssh_key: Option<&str>,
    own_worktree: &Path,
    notes: &mut Vec<String>,
) -> Result<PathBuf, String> {
    let cache = base.join(format!("{}.git", repo_slug(repo_url)));
    if cache.join("HEAD").exists() {
        // The fetch needs the key too: a mirror that cloned once still pulls
        // from the same private remote on every later job.
        adopt_mirror(&cache, repo_url, ssh_key, own_worktree, notes)?;
    } else {
        std::fs::create_dir_all(base)
            .map_err(|e| format!("cannot create {}: {e}", base.display()))?;
        crate::gitops::run_git_remote(
            &["clone", "--mirror", repo_url, &cache.to_string_lossy()],
            None,
            ssh_key,
        )?;
    }
    Ok(cache)
}

/// Take over an existing mirror for THIS job: point it at the URL the job
/// carries, then fetch (MAIN-629).
///
/// A mirror pinned its remote when it was created and nothing ever reconciled
/// it, so every later run fetched through the mirror's stored URL rather than
/// the workspace's current one. A workspace moved from HTTPS to SSH — the
/// ordinary way to start using a deploy key — kept fetching over HTTPS on every
/// node already holding a mirror; the delivered key is irrelevant to an HTTPS
/// remote, so git asked for a username, found no tty, and the run died reading
/// like a credential fault. It is invisible on a public repo, whose HTTPS fetch
/// succeeds anonymously, and fatal on a private one.
fn adopt_mirror(
    cache: &Path,
    repo_url: &str,
    ssh_key: Option<&str>,
    own_worktree: &Path,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    reconcile_mirror_remote(cache, repo_url, notes)?;
    fetch_mirror(cache, ssh_key, own_worktree)
        .map_err(|e| explain_fetch_failure(cache, repo_url, e))
}

/// Set the mirror's `origin` to the job's URL when the two name different
/// remotes, then clear anything that would rewrite it back. Never a re-clone:
/// the objects a mirror holds are this repo's objects whichever URL reached
/// them.
///
/// **The stored value is the one compared, never `git remote get-url`**
/// (MAIN-646). That command reports the url AFTER `insteadOf` rewriting, so on
/// a mirror carrying such a rule this read back HTTPS however correct the
/// stored ssh remote was: the comparison found a difference, `set-url` wrote
/// the ssh form, and reading it again still said HTTPS. That is why the same
/// mirror was "repaired" by hand three times and misdiagnosed as an agent
/// clobbering the config.
fn reconcile_mirror_remote(
    cache: &Path,
    repo_url: &str,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let stored = stored_remote(cache)?;
    let effective = if normalized_remote(&stored) == normalized_remote(repo_url) {
        stored
    } else {
        crate::gitops::run_git(
            &["remote", "set-url", "origin", repo_url],
            Some(cache),
            None,
        )?;
        tracing::info!(
            mirror = %cache.display(),
            from = %stored,
            to = %repo_url,
            "repointed a clone-cache mirror at the workspace's current remote"
        );
        repo_url.to_string()
    };
    clear_redirecting_rewrites(cache, &effective, notes)
}

/// The remote as the config STORES it, before any `insteadOf` rewriting.
///
/// Named in the error because `git config` says nothing at all when the key is
/// absent — where `git remote get-url` used to name the missing remote itself.
fn stored_remote(cache: &Path) -> Result<String, String> {
    crate::gitops::run_git(&["config", "--get", "remote.origin.url"], Some(cache), None)
        .map_err(|e| format!("cannot read remote.origin.url in {}: {e}", cache.display()))
}

/// One `url.<base>.insteadOf = <prefix>` rule: git replaces `prefix` with
/// `base` in any URL that starts with it, and applies the LONGEST matching
/// prefix only.
struct RewriteRule {
    /// As git spells it back — `url.<base>.insteadof` — so it can be unset and
    /// named to a person by the same string.
    key: String,
    base: String,
    prefix: String,
}

impl RewriteRule {
    fn rewrite(&self, url: &str) -> Option<String> {
        url.strip_prefix(&self.prefix)
            .map(|rest| format!("{}{rest}", self.base))
    }

    /// Which rule git will actually use for `url`: the longest matching prefix
    /// wins, and nothing is applied after it.
    ///
    /// **Earliest on a tie**, which is why the index is part of the key.
    /// git's `alias_url` compares `longest->len < candidate.len`, strictly, so
    /// an equal-length prefix never displaces the rule already chosen — while
    /// `max_by_key` alone returns the LAST of equal maxima. Taking the later
    /// rule is wrong in both directions it can be wrong: where the earlier one
    /// redirects, it is judged harmless and left in place, so the fetch still
    /// goes over HTTPS and MAIN-646 survives its own fix; where the earlier one
    /// is the harmless respelling, a rule git never applies is deleted, which
    /// is the mutation AC-3 and NG-3 exist to prevent.
    fn effective(rules: &[Self], url: &str) -> Option<usize> {
        rules
            .iter()
            .enumerate()
            .filter(|(_, r)| url.starts_with(&r.prefix))
            .max_by_key(|(i, r)| (r.prefix.len(), std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
    }
}

/// Every `insteadOf` rule the mirror's config carries, in the given scope.
///
/// `--null` because a rule's value is an arbitrary URL prefix: with git's
/// default output a key and its value are separated by a space, which a value
/// may contain. Records are `key\nvalue\0`.
///
/// An absent match makes `git config` exit 1, which `run_git` reports as an
/// error — indistinguishable here from a real one, and both mean the same
/// thing to this caller: no rule to worry about. A config the process cannot
/// read at all has already failed the `remote.origin.url` read above.
fn rewrite_rules(cache: &Path, scope: &[&str]) -> Vec<RewriteRule> {
    let mut args = vec!["config"];
    args.extend_from_slice(scope);
    args.extend(["--null", "--get-regexp", r"^url\..*\.insteadof$"]);
    let Ok(out) = crate::gitops::run_git(&args, Some(cache), None) else {
        return Vec::new();
    };
    out.split('\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let (key, prefix) = record.split_once('\n')?;
            let base = key
                .strip_prefix("url.")?
                .strip_suffix(".insteadof")?
                .to_string();
            Some(RewriteRule {
                key: key.to_string(),
                base,
                prefix: prefix.to_string(),
            })
        })
        .collect()
}

/// Drop the rules in the mirror's OWN config that would send `remote`
/// somewhere else (MAIN-646).
///
/// `gh` writes `url.https://github.com/.insteadOf git@github.com:` when it
/// authenticates over HTTPS in a repo, and a linked worktree shares the
/// mirror's config — so one `gh` call by one build agent redirects every later
/// fetch for every card on that repo, past a deploy key that is then
/// irrelevant. This runs before every fetch rather than once because `gh` will
/// write it again (AC-5).
///
/// A rule is only dropped when it actually redirects: the URL it produces has
/// to name a DIFFERENT remote than the one the workspace chose, so
/// `git@github.com:` → `ssh://git@github.com/` (the same place, spelled the
/// other way) survives, as does any rule whose prefix this remote does not
/// match. Longest-prefix-first and looping, because removing the rule in force
/// exposes the next one.
///
/// `--local` on both halves: a rule inherited from the operator's own global
/// config is not nook's to delete (NG-3). One that is in force and not ours
/// makes the fetch fail, and `explain_fetch_failure` names it.
fn clear_redirecting_rewrites(
    cache: &Path,
    remote: &str,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    // Shrinking a list read ONCE, rather than re-reading after each removal:
    // that bounds the loop by the number of rules whatever git's exit status
    // says. Re-reading would hang forever on a `--unset-all` that matched
    // nothing and still reported success.
    let mut rules = rewrite_rules(cache, &["--local"]);
    while let Some(i) = RewriteRule::effective(&rules, remote) {
        let Some(rewritten) = rules[i].rewrite(remote) else {
            return Ok(());
        };
        if normalized_remote(&rewritten) == normalized_remote(remote) {
            return Ok(());
        }
        let rule = rules.remove(i);
        crate::gitops::run_git(
            &[
                "config",
                "--local",
                "--unset-all",
                &rule.key,
                &exact_value_pattern(&rule.prefix),
            ],
            Some(cache),
            None,
        )
        .map_err(|e| {
            format!(
                "cannot remove the git rewrite rule {} = {} from {}: {e}",
                rule.key,
                rule.prefix,
                cache.display()
            )
        })?;
        // `--unset-all` drops EVERY value matching the anchored pattern, so a
        // key holding the same value twice (`git config --add`, which appends
        // where a plain set replaces) is fully cleared by that one call. The
        // list has to follow it: asking again for a value already gone exits 5
        // with an empty stderr, which would fail a job on a mirror whose config
        // this pass had just made correct.
        rules.retain(|r| r.key != rule.key || r.prefix != rule.prefix);
        notes.push(format!(
            "removed the git rewrite rule {} = {} from the clone cache at {}: it sent this \
             workspace's remote {remote} to {rewritten}, past the credential pinned to the \
             remote the workspace chose",
            rule.key,
            rule.prefix,
            cache.display()
        ));
    }
    Ok(())
}

/// `git config --unset-all` matches its value argument as a POSIX extended
/// regex, so a literal has to be escaped and anchored — `git@github.com:`
/// unescaped is a pattern whose dots match any character.
fn exact_value_pattern(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('^');
    for ch in value.chars() {
        if r"\^$.[]|()*+?{}".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('$');
    out
}

/// A remote URL reduced to the place it names, so the spellings of one remote
/// compare equal and only a real move rewrites the mirror's config.
///
/// Cosmetic here means: a trailing slash or `.git`, the host's letter case, and
/// scp-style `git@host:owner/repo` against its `ssh://git@host/owner/repo`
/// form. A change of SCHEME is not cosmetic — HTTPS and SSH authenticate
/// differently, and that difference is the whole reason this reconciles. A
/// string with no scheme and no scp colon is a local path, which is compared
/// as written.
fn normalized_remote(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest.to_string()),
        None => match scp_style(trimmed) {
            Some((authority, path)) => ("ssh".to_string(), format!("{authority}/{path}")),
            None => return strip_git_suffix(trimmed).to_string(),
        },
    };
    let rest = rest.trim_end_matches('/');
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    // A hostname is case-insensitive; a repository path is not, so only the
    // host half is folded.
    let authority = match authority.split_once('@') {
        Some((user, host)) => format!("{user}@{}", host.to_ascii_lowercase()),
        None => authority.to_ascii_lowercase(),
    };
    format!(
        "{scheme}://{authority}/{}",
        strip_git_suffix(path.trim_end_matches('/'))
    )
}

/// git's own rule for the scp-style remote: a colon before any slash.
fn scp_style(url: &str) -> Option<(&str, &str)> {
    let (authority, path) = url.split_once(':')?;
    (!authority.is_empty() && !authority.contains('/')).then_some((authority, path))
}

fn strip_git_suffix(path: &str) -> &str {
    path.strip_suffix(".git").unwrap_or(path)
}

/// Name both remotes when a fetch dies for want of a credential (MAIN-629
/// AC-4), and any rewrite rule still standing between them (MAIN-646 AC-6).
///
/// git's own message says only what it could not read; which remote it was
/// reading, and which one this job expected, is what tells a stale mirror apart
/// from a genuinely missing credential — and when the two agree, that is the
/// reader learning the URL is not the fault. Two agreeing URLs and a failing
/// fetch is exactly the shape MAIN-646 wore, so the rule clause is what stops
/// that reading from being a dead end: a rule left here is one `--local` could
/// not remove, which means it is the operator's own global or system config.
///
/// The STORED remote, matching what reconciliation compares — the rewritten
/// value is reported separately, as the rule's doing, rather than passed off as
/// what the config says.
fn explain_fetch_failure(cache: &Path, repo_url: &str, err: String) -> String {
    if !is_credential_failure(&err) {
        return err;
    }
    let stored = stored_remote(cache).unwrap_or_else(|_| "<unreadable>".to_string());
    let mut said = format!(
        "{err} — the mirror at {} stores origin {stored}; this job carries {repo_url}",
        cache.display()
    );
    let rules = rewrite_rules(cache, &[]);
    if let Some(rule) = RewriteRule::effective(&rules, &stored).map(|i| &rules[i]) {
        if let Some(rewritten) = rule.rewrite(&stored) {
            said.push_str(&format!(
                "; the rewrite rule {} = {} is in force, so git fetched {rewritten}",
                rule.key, rule.prefix
            ));
        }
    }
    said
}

fn is_credential_failure(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("could not read username")
        || lower.contains("could not read password")
        || lower.contains("authentication failed")
        || lower.contains("permission denied")
        || lower.contains("could not read from remote repository")
}

/// Fetch into the mirror, healing the one wedge a run may heal itself
/// (MAIN-466 AC-2): git refuses to fetch into a branch any linked worktree has
/// checked out, and a review worktree from before the detached-head fix pins
/// the workspace branch at a stable path that outlives its run — so the moment
/// the branch moves on the remote, every later fetch fails, forever. The
/// refusal names the worktree holding the branch; when that is THIS run's own
/// tree, it was about to be removed and recreated anyway (see `run`), so remove
/// it now and retry the fetch once. Any other refusal stays an error: the
/// pinning tree belongs to a live concurrent job, and removing it is not
/// recovery.
fn fetch_mirror(cache: &Path, ssh_key: Option<&str>, own_worktree: &Path) -> Result<(), String> {
    match crate::gitops::run_git_remote(&["fetch", "--prune"], Some(cache), ssh_key) {
        Err(e) if fetch_refused_by(&e, own_worktree) => {
            remove_job_worktree(cache, own_worktree)?;
            crate::gitops::run_git_remote(&["fetch", "--prune"], Some(cache), ssh_key).map(|_| ())
        }
        r => r.map(|_| ()),
    }
}

/// Matched on the path, not just the phrase: git names the worktree holding
/// the branch (`… checked out at '<path>'`), and only OUR OWN path makes
/// removal safe. The quotes are part of the match — a bare substring would let
/// `…-pr1` claim a refusal naming `…-pr10`, a sibling PR of the same repo.
fn fetch_refused_by(err: &str, worktree: &Path) -> bool {
    err.contains("refusing to fetch into")
        && err.contains(&format!("'{}'", worktree.to_string_lossy()))
}

/// The `[worktree]` section of a repo's own `.nook.toml` (MAIN-481 AC-2).
///
/// Read from the NEW worktree rather than the source checkout: it is the tree's
/// own tracked file, so a branch that changes the rule takes effect on the pass
/// that checks it out, not one pass late.
///
/// `#[serde(default)]` and no `deny_unknown_fields`, matching
/// `services/repo_settings.rs`'s contract exactly: the next setting is a new
/// section, and an older node reading a newer file keeps working.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
struct WorktreeSettings {
    /// Opt a repo out entirely. Default on: a build worktree with no `.env` is
    /// the failure this exists to prevent.
    copy_ignored: bool,
    /// Gitignore-style patterns carved OUT of the copy — the vendor directory
    /// too big to be worth it, the log nobody wants.
    exclude: Vec<String>,
    /// Directories deleted when a build run CONCLUDES (MAIN-493 AC-1) — the
    /// compiler's output, which the next pass regenerates and no reader ever
    /// wants again. Repo-root-relative paths, not gitignore patterns: this
    /// names directories to delete, so an accidental `*` must not be able to
    /// mean "most of the tree".
    ///
    /// Defaults to cargo's `target`, the directory that actually filled the
    /// machine. A repo that builds elsewhere names its own; `reclaim = []`
    /// opts out. Either way `reclaim_build_output` deletes nothing git does
    /// not IGNORE, so a wrong entry costs a no-op rather than the source.
    reclaim: Vec<String>,
}

impl Default for WorktreeSettings {
    fn default() -> Self {
        Self {
            copy_ignored: true,
            exclude: Vec::new(),
            reclaim: vec!["target".into()],
        }
    }
}

/// What a repo asks of its job sandbox (MAIN-611 AC-2).
///
/// A DECLARATION, like the port listeners beside it: nothing is mounted because
/// the node happens to have it. A repo that wants its package cache warm across
/// runs names the path here and gets that path and nothing else.
#[derive(Debug, Default, serde::Deserialize)]
struct SandboxSettings {
    /// Host paths mounted read-write into the job container, at the same path.
    /// `~` is expanded; a path that does not exist is skipped rather than
    /// failing the run, so a declaration can be shared across machines.
    #[serde(default)]
    caches: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RepoSettingsFile {
    #[serde(default)]
    worktree: WorktreeSettings,
    #[serde(default)]
    sandbox: SandboxSettings,
}

/// Parse a repo's `.nook.toml`. A missing, unreadable or malformed file is the
/// DEFAULT, never an error: seeding is an optimisation, and a typo in a settings
/// file must not fail a build run.
fn worktree_settings(worktree: &Path) -> WorktreeSettings {
    repo_settings(worktree).worktree
}

/// The declared sandbox caches, resolved to paths that exist on this node.
fn sandbox_caches(worktree: &Path) -> Vec<crate::sandbox::Mount> {
    repo_settings(worktree)
        .sandbox
        .caches
        .iter()
        .map(|c| PathBuf::from(crate::config::expand_path(c)))
        .filter(|p| p.exists())
        .map(|p| crate::sandbox::Mount {
            host: p.clone(),
            container: p,
        })
        .collect()
}

fn repo_settings(worktree: &Path) -> RepoSettingsFile {
    let Ok(text) = std::fs::read_to_string(worktree.join(".nook.toml")) else {
        return RepoSettingsFile::default();
    };
    toml::from_str::<RepoSettingsFile>(&text).unwrap_or_default()
}

/// The workspace's PRIMARY checkout on this node — the one holding the `.env`
/// and the warm vendor directories a fresh worktree lacks.
///
/// Matched by remote, because that is the identity `discovery` itself gives a
/// workspace. Linked worktrees are skipped: they are as bare as the tree being
/// seeded, so copying from one would be copying nothing.
fn primary_checkout_for(cfg: &NodeConfig, repo_url: &str) -> Option<PathBuf> {
    primary_checkout_in(&cfg.workspace_roots, repo_url)
}

fn primary_checkout_in(roots: &[String], repo_url: &str) -> Option<PathBuf> {
    let want = same_repo_key(repo_url);
    crate::discovery::scan(roots)
        .into_iter()
        .filter(|w| !w.worktree)
        .find(|w| {
            w.git_remote_url
                .as_deref()
                .is_some_and(|u| same_repo_key(u) == want)
        })
        .map(|w| PathBuf::from(w.path))
}

/// Two remote URLs naming one repository, compared loosely enough to survive
/// the shapes the same repo is written in (`.git` suffix, trailing slash,
/// case). Deliberately not the control plane's `normalize_remote`: nothing
/// links these two binaries, and a seed that occasionally declines to find a
/// checkout costs a cold build, where a wrong match would copy a stranger's
/// `.env` into this repo's tree.
fn same_repo_key(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase()
}

/// Every ignored entry in `source`, as git itself enumerates them.
///
/// `--directory` collapses a wholly-ignored directory to one entry, so
/// `node_modules/` is a single line rather than fifty thousand.
fn ignored_entries(source: &Path) -> Result<Vec<String>, String> {
    Ok(crate::gitops::run_git(
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        Some(source),
        None,
    )?
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect())
}

/// Which of `entries` the repo's `exclude` patterns match.
///
/// Asked of GIT, in a throwaway repository holding ONLY those patterns, so the
/// answer is real gitignore semantics — `**`, negation, anchoring, directory
/// suffixes — rather than a glob matcher that agrees with git on the easy cases
/// and diverges on the ones a user reaches for patterns to express.
///
/// `core.excludesFile=/dev/null` is load-bearing: without it `check-ignore`
/// also honours the NODE's global ignore file, so a `.env` line in one
/// operator's `~/.config/git/ignore` would silently carve `.env` out of every
/// repo's seed — the exact file this exists to carry — and make node-level git
/// config a second home for a setting NG-1 puts only in `.nook.toml`.
///
/// An error is an ERROR, never an empty set. Failing open would copy the very
/// entries the repo asked to drop while the transcript still read "seeded N";
/// the caller says so out loud and seeds nothing instead.
fn excluded_by(patterns: &[String], entries: &[String]) -> Result<HashSet<String>, String> {
    let mut out = HashSet::new();
    if patterns.is_empty() || entries.is_empty() {
        return Ok(out);
    }
    let tmp = std::env::temp_dir().join(format!("nook-seed-{}", uuid::Uuid::now_v7().simple()));
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&tmp);
    };
    std::fs::create_dir_all(&tmp).map_err(|e| format!("exclude scratch repo: {e}"))?;
    let init = crate::gitops::run_git(&["init", "-q"], Some(&tmp), None)
        .map_err(|e| format!("exclude scratch repo: {e}"))
        .and_then(|_| {
            std::fs::write(tmp.join(".git/info/exclude"), patterns.join("\n") + "\n")
                .map_err(|e| format!("exclude patterns: {e}"))
        });
    if let Err(e) = init {
        cleanup();
        return Err(e);
    }
    // Batched, because one argv holding every ignored entry of a large checkout
    // can pass ARG_MAX — and a failure there would look exactly like "nothing
    // matched" if this returned a bare set.
    for chunk in entries.chunks(500) {
        let mut args: Vec<&str> = vec![
            "-c",
            "core.excludesFile=/dev/null",
            "check-ignore",
            "--no-index",
        ];
        args.extend(chunk.iter().map(String::as_str));
        match crate::gitops::run_git(&args, Some(&tmp), None) {
            Ok(matched) => out.extend(
                matched
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string),
            ),
            // `check-ignore` exits 1 with nothing on stderr when NOTHING in the
            // batch matched. That is an answer, not a failure.
            Err(e) if e.trim().is_empty() => {}
            Err(e) => {
                cleanup();
                return Err(format!("deciding excludes: {e}"));
            }
        }
    }
    cleanup();
    Ok(out)
}

/// Copy a file, directory or symlink, never overwriting what is already there
/// and never FOLLOWING a link.
///
/// `symlink_metadata` and not `metadata`: a followed link reports the type of
/// its target, so a symlink to a directory would be walked as one. Ignored
/// directories are exactly where symlink farms live — `node_modules`, `.venv`,
/// pnpm layouts — and a mutual pair there recursed until the OS refused the
/// path length, after materialising thousands of real directories on the node.
/// A dangling link was the mirror bug: `exists()` said no, `copy` failed
/// `ENOENT`, and the whole seed died on it.
fn copy_into(src: &Path, dest: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    // `symlink_metadata` again rather than `exists()`, which follows and so
    // reports a dangling link as absent — then refuses to overwrite it anyway.
    if std::fs::symlink_metadata(dest).is_ok() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dest)?;
        // The node ships for linux and macos; anywhere else a link is skipped
        // rather than guessed at, which costs a cold build and nothing worse.
        #[cfg(not(unix))]
        let _ = target;
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_into(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(src, dest)?;
    Ok(())
}

/// Seed a FRESH build worktree with the ignored files of the workspace's
/// primary checkout (MAIN-481).
///
/// The copied set is exactly what git IGNORES — `.env`, vendor and build
/// directories. Untracked-but-not-ignored files are deliberately not copied:
/// MAIN-480's clean-case sweep deletes that class on the next pass, so seeding
/// it would hand the tree something designed to be thrown away, while ignored
/// files are the one class both tickets always preserve.
///
/// Returns how many entries were copied, or the reason nothing was.
fn seed_worktree(source: &Path, dest: &Path, settings: &WorktreeSettings) -> Result<usize, String> {
    if !settings.copy_ignored {
        return Ok(0);
    }
    let entries = ignored_entries(source)?;
    let skip = excluded_by(&settings.exclude, &entries)?;
    let mut copied = 0;
    let mut failed = Vec::new();
    for entry in entries {
        let rel = entry.trim_end_matches('/');
        if rel.is_empty() || skip.contains(&entry) || skip.contains(rel) {
            continue;
        }
        let from = source.join(rel);
        let to = dest.join(rel);
        if std::fs::symlink_metadata(&from).is_err() || std::fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        match copy_into(&from, &to) {
            Ok(()) => copied += 1,
            Err(e) => {
                // The tree is never re-seeded (AC-4/NG-5), so a half-copied
                // `node_modules` would outlive the card and be worse than the
                // cold build this falls back to. Take the partial away and
                // carry on with the entries that can still land.
                let _ = std::fs::remove_dir_all(&to);
                let _ = std::fs::remove_file(&to);
                failed.push(format!("{rel} ({e})"));
            }
        }
    }
    if !failed.is_empty() {
        return Err(format!(
            "copied {copied}; could not copy {}: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    Ok(copied)
}

/// What a concluded build run gave back to the disk (MAIN-493).
#[derive(Debug, Default, PartialEq, Eq)]
struct Reclaimed {
    /// Declared paths that are gone, as the repo spelled them.
    removed: Vec<String>,
    /// Freed bytes, counted before the delete — and on a partial delete, the
    /// difference, so a directory half of which is root-owned still reports
    /// what it actually gave back.
    bytes: u64,
    /// Declared paths that could not be reclaimed, each with its reason. Never
    /// fatal: the run has already concluded, and disk is not worth a red pass.
    refused: Vec<String>,
}

/// Delete a concluded BUILD run's build output, keeping the worktree, its
/// branch and its git state (MAIN-493 AC-1).
///
/// MAIN-480 keeps a build worktree until the card's work merges, which is right
/// for the source and the branch and very wrong for `target/`: cargo names an
/// artifact for its unit CONFIGURATION, never its content, so every
/// differently-configured build adds a whole artifact set and removes nothing.
/// One worktree reached 120 GB and filled the machine on 2026-08-09. Nothing in
/// there is ever read again — the next pass rebuilds what it needs — so the
/// output goes and the tree stays. `docs/build-artifact-growth.md` has the
/// measurements, and why unifying the builds at the source saves nothing.
///
/// Two guards, and both are the point. A declared path must resolve INSIDE the
/// worktree, and git must IGNORE it: a `reclaim` entry naming tracked source is
/// then a no-op rather than the thing that deletes the branch's only copy of an
/// hour's work.
fn reclaim_build_output(worktree: &Path, settings: &WorktreeSettings) -> Reclaimed {
    let mut out = Reclaimed::default();
    let wanted: Vec<String> = settings
        .reclaim
        .iter()
        .filter_map(|r| normalized_reclaim_entry(r))
        .collect();
    if wanted.is_empty() {
        return out;
    }
    // Asked once, of git, for the whole list — and only of paths that exist, so
    // a tree that never built does not shell out at all (AC-4).
    let present: Vec<(String, PathBuf)> = wanted
        .into_iter()
        .filter_map(|rel| reclaimable_dir(worktree, &rel).map(|dir| (rel, dir)))
        .collect();
    if present.is_empty() {
        return out;
    }
    let names: Vec<String> = present.iter().map(|(rel, _)| rel.clone()).collect();
    let ignored = ignored_by_repo(worktree, &names);
    for (rel, dir) in present {
        if !ignored.contains(&rel) {
            out.refused.push(format!(
                "{rel} (this repo does not ignore it — reclaim names build output, never source)"
            ));
            continue;
        }
        let before = dir_bytes(&dir);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                out.removed.push(rel);
                out.bytes += before;
            }
            Err(e) => {
                // A container writes some of these as root, so a partial delete
                // is the expected failure rather than an exotic one. Count what
                // did go and name what did not.
                out.bytes += before.saturating_sub(dir_bytes(&dir));
                out.refused.push(format!("{rel} ({e})"));
            }
        }
    }
    out
}

/// A `reclaim` entry as a plain relative directory path, or `None` when it is
/// not one this may act on at all.
///
/// A leading `/` is the repo root, exactly as the neighbouring `exclude`
/// patterns spell it — never the machine's root, which is what `Path::join`
/// would otherwise make of it. `..` has no such second reading and is dropped:
/// an entry that climbs out of the tree is a mistake, not an instruction.
fn normalized_reclaim_entry(raw: &str) -> Option<String> {
    let rel = raw
        .trim()
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string();
    if rel.is_empty() {
        return None;
    }
    Path::new(&rel)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        .then_some(rel)
}

/// The directory a normalized entry names, if it is a real directory inside
/// this worktree.
///
/// `symlink_metadata` and a canonicalized containment check, together: a
/// `target` symlinked to a shared cache elsewhere on the node would otherwise
/// hand `remove_dir_all` a path outside the tree entirely.
fn reclaimable_dir(worktree: &Path, rel: &str) -> Option<PathBuf> {
    let dir = worktree.join(rel);
    if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let root = std::fs::canonicalize(worktree).ok()?;
    std::fs::canonicalize(&dir)
        .ok()?
        .starts_with(&root)
        .then_some(dir)
}

/// Which of `rels` this repository's own ignore rules cover.
///
/// Without `--no-index`, git answers "not ignored" for anything TRACKED, which
/// is exactly the protection wanted: a directory whose contents are in the
/// index is source, whatever it is called.
///
/// `core.excludesFile=/dev/null` for the same reason `excluded_by` sets it — an
/// operator's global ignore file must not decide what a repo deletes.
fn ignored_by_repo(worktree: &Path, rels: &[String]) -> HashSet<String> {
    let mut args: Vec<&str> = vec!["-c", "core.excludesFile=/dev/null", "check-ignore", "--"];
    args.extend(rels.iter().map(String::as_str));
    // An error is an EMPTY set, never everything: `check-ignore` exits 1 when
    // nothing matched, and a git that failed for any other reason must reclaim
    // nothing rather than guess.
    crate::gitops::run_git(&args, Some(worktree), None)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Bytes held by a directory tree, links counted as links rather than followed.
pub(crate) fn dir_bytes(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// The transcript line for a reclaim, or `None` when there is nothing to say —
/// a tree that never built is the common case and deserves silence.
fn reclaim_note(r: &Reclaimed) -> Option<String> {
    let mut parts = Vec::new();
    if !r.removed.is_empty() {
        parts.push(format!(
            "reclaimed {} of build output ({})",
            human_bytes(r.bytes),
            r.removed.join(", ")
        ));
    }
    if !r.refused.is_empty() {
        parts.push(format!("could not reclaim {}", r.refused.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Reclaim a concluded build run's output and say what happened, at every place
/// a build run can end (MAIN-493 AC-1/AC-2).
///
/// Keyed on the conclusion of THIS run, on THIS run's own worktree, and never
/// on a timer or a scan of the worktree directory: a tree was observed being
/// created mid-sweep on 2026-08-09 (MAIN-210), so "delete what is not on my
/// list" is the one shape that can race a live build. A run holds its own tree
/// exclusively, so this cannot.
fn reclaim_and_note(out: &Sender<NodeToControl>, job_id: &str, worktree: &Path) {
    let reclaimed = reclaim_build_output(worktree, &worktree_settings(worktree));
    if let Some(msg) = reclaim_note(&reclaimed) {
        note(out, job_id, msg);
    }
}

pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Is this a BUILD run's worktree directory? Only those outlive their run
/// (MAIN-480 AC-1); review keeps a stable PATH but is rebuilt every pass, and
/// everything else is per-job.
fn is_build_dirname(name: &str) -> bool {
    name.starts_with("build-")
}

/// The branch an existing worktree has checked out, if any.
///
/// A build tree is normally ON one: the skill creates the card's branch and
/// works there, so this is the state every pass after the first finds.
fn attached_branch(worktree: &Path) -> Option<String> {
    crate::gitops::run_git(&["symbolic-ref", "--short", "HEAD"], Some(worktree), None)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Detach an existing build worktree at its current commit, BEFORE the mirror
/// is fetched (MAIN-480).
///
/// This is not tidiness, it is what keeps the tree alive. A linked worktree of
/// a `--mirror` clone shares that repo's `refs/heads/*`, and git refuses —
/// fatally — to fetch into a branch a worktree holds. `fetch_mirror`'s MAIN-466
/// self-heal recognises OUR OWN path in that refusal and REMOVES the tree to
/// unwedge itself, which was correct when a worktree lasted one run and is
/// exactly wrong now that it must outlive the pass. Freeing the branch first
/// means the refusal never happens.
///
/// Detaching keeps the working tree and the branch ref untouched: uncommitted
/// work stays, unpushed commits stay reachable from the branch, and the skill
/// re-attaches on its next step.
fn detach_worktree(worktree: &Path) -> Option<String> {
    let branch = attached_branch(worktree)?;
    crate::gitops::run_git(&["checkout", "--detach"], Some(worktree), None).ok()?;
    Some(branch)
}

/// The commit a pass should start from — "the pushed head" of the ticket's
/// rule, resolved against the mirror AFTER it has been fetched.
///
/// The card's branch if origin still has it, else the default branch. Both
/// cases are ordinary: a pass that pushed leaves its branch on origin, and a
/// pass that never pushed has its local branch PRUNED out of the mirror by the
/// fetch — which is precisely why the fallback is not an error.
fn pushed_head(cache: &Path, card_branch: Option<&str>, default_branch: &str) -> String {
    if let Some(b) = card_branch {
        let r = format!("refs/heads/{b}");
        if crate::gitops::run_git(&["rev-parse", "--verify", "--quiet", &r], Some(cache), None)
            .is_ok_and(|o| !o.trim().is_empty())
        {
            return r;
        }
    }
    format!("refs/heads/{default_branch}")
}

/// What an existing build worktree must have done to the tree before this pass
/// may reset it (MAIN-480 AC-2).
///
/// The rule is one sentence: **reset only when there is nothing to lose.** A
/// pass that finished cleanly ends pushed, so its tree holds nothing origin
/// does not already have and starting from the pushed head costs nothing. A
/// pass that DIED mid-flight leaves the only copy of that work here, and the
/// hour it represents is not recoverable from anywhere else — so the tree is
/// left exactly as found and the agent, which resumes a session that remembers
/// what it was doing, continues.
#[derive(Debug, PartialEq, Eq)]
enum TreeState {
    /// Nothing uncommitted and nothing the pushed head lacks: safe to reset.
    Clean,
    /// Uncommitted changes and/or commits origin does not have.
    Interrupted { uncommitted: bool, unpushed: bool },
}

impl TreeState {
    /// The transcript line for this state — the operator's only view of which
    /// branch of the rule ran, so both cases say so out loud.
    fn note(&self) -> String {
        match self {
            TreeState::Clean => {
                "reusing this card's worktree — clean, reset to the pushed head".into()
            }
            TreeState::Interrupted {
                uncommitted,
                unpushed,
            } => {
                let what = match (uncommitted, unpushed) {
                    (true, true) => "uncommitted changes and unpushed commits",
                    (true, false) => "uncommitted changes",
                    _ => "unpushed commits",
                };
                format!(
                    "resuming interrupted work — this card's worktree holds {what}; \
                     leaving the tree exactly as the previous run left it"
                )
            }
        }
    }
}

/// Classify an existing worktree against the head it would be reset to.
///
/// Called AFTER `detach_worktree` and the mirror fetch, which is what makes the
/// unpushed half answerable at all: in a `--mirror` clone `refs/heads/*` is
/// both the local branch and origin's copy of it, so the question can only be
/// asked once the fetch has made that ref origin's truth — and only of a HEAD
/// that no longer holds the branch. Asking it of an attached HEAD (against
/// `--glob=refs/heads/*`, which excludes HEAD's own branch) silently answers
/// "nothing unpushed" for every real build tree.
fn classify_tree(worktree: &Path, pushed: &str) -> TreeState {
    // `-uno`: modifications to TRACKED files only. Untracked files are NOT the
    // interrupted signal — the rule sweeps them in the clean case, so counting
    // them here would make that sweep unreachable AND wedge a tree forever the
    // first time a pass left a stray file behind.
    let uncommitted = crate::gitops::run_git(
        &["status", "--porcelain", "--untracked-files=no"],
        Some(worktree),
        None,
    )
    .map(|o| !o.trim().is_empty())
    .unwrap_or(false);
    let unpushed = crate::gitops::run_git(
        &["rev-list", "--count", "HEAD", &format!("^{pushed}")],
        Some(worktree),
        None,
    )
    .map(|o| o.trim() != "0" && !o.trim().is_empty())
    .unwrap_or(false);
    if uncommitted || unpushed {
        TreeState::Interrupted {
            uncommitted,
            unpushed,
        }
    } else {
        TreeState::Clean
    }
}

/// Put a CLEAN existing worktree back on the head this pass should start from:
/// DETACHED at `pushed`, with untracked-not-ignored leftovers swept.
///
/// `--detach` and not a bare reset: a plain `reset --hard` on an attached HEAD
/// moves the card's own BRANCH to whatever it resets to — on a repair pass that
/// would drag the branch off the PR's commit — and leaves the tree holding a
/// ref, which re-arms the fetch wedge `detach_worktree` exists to avoid.
///
/// `clean -fd` and never `-fdx`: the ignored files are the warm layer — the
/// `.env` a run needs and the vendor directories that make the build fast — and
/// deleting them would make persistence pointless.
fn reset_clean_worktree(worktree: &Path, pushed: &str) -> Result<Vec<String>, String> {
    crate::gitops::run_git(&["checkout", "--detach", pushed], Some(worktree), None)?;
    crate::gitops::run_git(&["reset", "--hard", pushed], Some(worktree), None)?;
    // Named before they go. Untracked files are swept rather than treated as
    // interrupted work (see `classify_tree`), which is the one edge where this
    // rule can discard something a previous pass made — so it is never silent.
    let doomed: Vec<String> = crate::gitops::run_git(&["clean", "-nd"], Some(worktree), None)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("Would remove "))
        .map(str::to_string)
        .collect();
    crate::gitops::run_git(&["clean", "-fd"], Some(worktree), None)?;
    Ok(doomed)
}

/// The commit a fresh build worktree starts from: the mirror's own `HEAD`,
/// which a `--mirror` clone symrefs to the repository's real default branch
/// (MAIN-480 AC-3).
///
/// Asking the mirror is the whole point. `branch` arrives from the control
/// plane's `resolve_repo`, which reads `node_workspaces.git_branch` — whatever
/// branch the node's primary clone happened to have checked out when discovery
/// last scanned it. A colleague's feature branch became the base for every
/// build worktree that way; the repository's own answer cannot.
fn default_branch_name(cache: &Path) -> Option<String> {
    crate::gitops::run_git(&["symbolic-ref", "--short", "HEAD"], Some(cache), None)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Why a build run may not launch its agent here (MAIN-482).
///
/// "No automated builder ever works on the primary clone or the default
/// branch" was prose in the build skill until now — a rule that costs tokens
/// on every pass and that an agent can reason its way around. These are the
/// same rule as code, checked at the moment the node controls.
#[derive(Debug, PartialEq, Eq)]
enum LaunchRefusal {
    /// The working directory is not a worktree of this node's own loop clone
    /// cache. Anything else is somebody's checkout.
    OutsideCache,
    /// It IS a path discovery reports as a `node_workspaces` checkout — the
    /// primary clone, or a human's worktree. The escape hatch for parallel
    /// work while the loop runs is exactly this directory (NG-2).
    KnownCheckout,
    /// HEAD is attached to the repository's default branch. Detached (the
    /// clean start) and attached to the card's own branch (resuming
    /// interrupted work) are both legitimate.
    OnDefaultBranch(String),
}

impl LaunchRefusal {
    /// The transcript's whole account of the refusal: what was refused, and the
    /// working directory it was refused for.
    fn message(&self, worktree: &Path) -> String {
        let wd = worktree.display();
        match self {
            Self::OutsideCache => format!(
                "refusing to build in {wd}: a build run works only in a worktree of this \
                 node's own loop clone cache"
            ),
            Self::KnownCheckout => format!(
                "refusing to build in {wd}: that is a checkout this node reports to the \
                 control plane — the primary clone or a human's worktree — and it is \
                 reserved for people, not builders"
            ),
            Self::OnDefaultBranch(branch) => format!(
                "refusing to build in {wd}: HEAD is attached to {branch}, the repository's \
                 default branch. A build pass starts detached at the pushed head, or on \
                 the card's own branch"
            ),
        }
    }
}

/// Is `path` a proper descendant of `base`? Both are compared canonicalized
/// where the filesystem allows it, so a symlinked home (`/home` → `/var/home`)
/// or a `..` in a configured root cannot make a legitimate worktree read as an
/// outsider.
fn is_inside(path: &Path, base: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (path, base) = (real(path), real(base));
    path != base && path.starts_with(&base)
}

/// May a BUILD run launch its agent in `worktree`? `None` is yes.
///
/// Pure, so every case in AC-5 is a unit test rather than a live node: the
/// caller supplies the cache's worktree base, the checkout paths discovery
/// reports, the branch HEAD is attached to (`None` = detached), and the
/// repository's default branch.
fn build_launch_refusal(
    worktree: &Path,
    wt_base: &Path,
    checkouts: &[PathBuf],
    attached: Option<&str>,
    default_branch: &str,
) -> Option<LaunchRefusal> {
    if !is_inside(worktree, wt_base) {
        return Some(LaunchRefusal::OutsideCache);
    }
    // Equality, not containment: a cache worktree is a descendant of the cache
    // base, and a containment test against a root somebody pointed discovery at
    // could swallow the very tree this is protecting.
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let here = real(worktree);
    if checkouts.iter().any(|c| real(c) == here) {
        return Some(LaunchRefusal::KnownCheckout);
    }
    if attached.is_some_and(|b| b == default_branch) {
        return Some(LaunchRefusal::OnDefaultBranch(default_branch.to_string()));
    }
    None
}

/// Every checkout path this node reports to the control plane — the rows
/// `node_workspaces` is built from, which is the same list AC-1 refuses to
/// build in. Read at launch rather than cached: a human can make a worktree
/// between one pass and the next.
fn known_checkout_paths(cfg: &NodeConfig) -> Vec<PathBuf> {
    crate::discovery::scan(&cfg.workspace_roots)
        .into_iter()
        .map(|w| PathBuf::from(w.path))
        .collect()
}

/// Add a per-job worktree off `cache`, into `<wt_base>/<job>` so concurrent jobs
/// on the same workspace get distinct trees.
///
/// `detach` is for review worktrees, and it is load-bearing (MAIN-466 AC-1):
/// their stable path outlives the run, and an attached checkout pins `branch`
/// in the mirror so every later fetch is refused (`fetch_mirror`). Detached,
/// no ref is held and the mirror always fast-forwards.
///
/// Attached is three attempts, because two worktrees cannot check out the same
/// branch: the branch as-is (the lone-job case) → the branch tip detached (a
/// second concurrent job on the *same* branch, which git refuses to check out
/// twice) → creating the branch if it isn't present locally (mirroring
/// `gitops::add_worktree`). Either way the tree is based on `branch`.
fn add_job_worktree_in(
    wt_base: &Path,
    cache: &Path,
    branch: &str,
    dirname: &str,
    detach: bool,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(wt_base)
        .map_err(|e| format!("cannot create {}: {e}", wt_base.display()))?;
    let dest = wt_base.join(sanitize(dirname));
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
    }
    let dest_str = dest.to_string_lossy().to_string();
    let detached: [&[&str]; 1] = [&["worktree", "add", "--detach", &dest_str, branch]];
    let attached: [&[&str]; 3] = [
        &["worktree", "add", &dest_str, branch],
        &["worktree", "add", "--detach", &dest_str, branch],
        &["worktree", "add", "-b", branch, &dest_str],
    ];
    let attempts: &[&[&str]] = if detach { &detached } else { &attached };
    let mut last = String::new();
    for args in attempts {
        match crate::gitops::run_git(args, Some(cache), None) {
            Ok(_) => return Ok(dest),
            Err(e) => last = e,
        }
    }
    Err(format!("worktree add failed: {last}"))
}

/// Remove a per-job worktree through git so the mirror's metadata stays
/// consistent; fall back to deleting the dir and pruning the admin state.
fn remove_job_worktree(cache: &Path, worktree: &Path) -> Result<(), String> {
    let wt = worktree.to_string_lossy().to_string();
    match crate::gitops::run_git(&["worktree", "remove", "--force", &wt], Some(cache), None) {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_dir_all(worktree);
            let _ = crate::gitops::run_git(&["worktree", "prune"], Some(cache), None);
            if worktree.exists() {
                Err(e)
            } else {
                Ok(())
            }
        }
    }
}

/// Every build worktree this node currently holds, as absolute paths — what
/// `LoopWorktreesHeld` reports so the control plane can order the removal of
/// the ones it no longer records (MAIN-480 AC-1) and of those whose card is
/// over (MAIN-537 AC-4).
///
/// **A tree this node is building in right now is withheld.** The control plane
/// removes any reported tree no card records, and there is a window between
/// `git worktree add` and it persisting `LoopWorktreeReady` in which that is
/// true of a live run's own working directory. MAIN-537 put this report on a
/// ten-minute timer, so the window went from being sampled once per connect to
/// 144 times a day — small odds either way, and the loss is a build's work.
/// This node knows what it is running; nothing else does.
pub fn build_worktrees_held(cfg: &NodeConfig) -> Vec<String> {
    let running = running_jobs().lock().map(|s| s.clone()).unwrap_or_default();
    held_build_worktrees_in(&cache_base(&cfg.server).join("worktrees"), &running)
}

fn held_build_worktrees_in(wt_base: &Path, running: &HashSet<String>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(wt_base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| e.file_name().to_str().is_some_and(is_build_dirname))
        .filter(|e| !e.file_name().to_str().is_some_and(|d| running.contains(d)))
        .map(|e| e.path().to_string_lossy().to_string())
        .collect()
}

/// Best-effort cleanup of orphaned job worktrees on (re)connect (AC-4/AC-6):
/// prune each known mirror's worktree admin, then delete any worktree dir whose
/// job is no longer running. Never fatal.
///
/// **Build worktrees are exempt** (MAIN-480 AC-1). "No running job" is the
/// normal state of a build tree between its passes, so the running-set test —
/// correct for every other kind — would delete exactly the directories that are
/// supposed to survive. Whether a build tree is still wanted is a control-plane
/// fact (does a card still record it?), so the node reports what it holds via
/// `build_worktrees_held` and removes one only when told to.
pub fn reconcile(cfg: &NodeConfig) {
    reconcile_in(&cache_base(&cfg.server));
}

fn reconcile_in(base: &Path) {
    let base = base.to_path_buf();
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.extension().and_then(|s| s.to_str()) == Some("git") {
                let _ = crate::gitops::run_git(&["worktree", "prune"], Some(&p), None);
            }
        }
    }
    let running = running_jobs().lock().map(|s| s.clone()).unwrap_or_default();
    let wt_base = base.join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&wt_base) {
        for entry in entries.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if p.is_dir() && !running.contains(name) && !is_build_dirname(name) {
                let _ = std::fs::remove_dir_all(&p);
            }
        }
    }
}

/// Flatten text to one tmux-typeable line. `send_keys -l` sends the string
/// literally, so an embedded newline would submit half a message; collapsing
/// every run of whitespace to a single space keeps a multi-line brief intact as
/// one prompt.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The card's `@slug` references as this MACHINE can honour them (MAIN-632).
///
/// The path the control plane sent comes from a `node_workspaces` row, which
/// records what this node last reported rather than what is on its disk now. A
/// path that has since gone must not reach `docker run`: Docker CREATES a
/// missing bind source, as root, on the owner's own machine — so a reference
/// whose checkout is not there is downgraded to unavailable here, and the brief
/// says so (AC-8) instead of the agent finding an empty directory.
fn resolvable_references(mut refs: Vec<nook_types::WorkspaceRef>) -> Vec<nook_types::WorkspaceRef> {
    for r in &mut refs {
        if !r.path.as_deref().is_some_and(|p| Path::new(p).is_dir()) {
            r.path = None;
        }
    }
    refs
}

/// What the agent is TOLD it has (MAIN-632 AC-7/AC-8) — one line, appended to
/// the opening turn, because a run that is handed a repo and not told about it
/// will never look.
///
/// Two sentences, and the second is not an afterthought: a reference the
/// executor holds no checkout of is the ordinary case on a fleet where a
/// workspace lives on some machines and not others, and an agent that is told
/// only about what it got would read the silence as "the reference did not
/// resolve" and go on guessing.
///
/// Empty when the card names nothing, which is nearly every card.
fn references_brief(refs: &[nook_types::WorkspaceRef]) -> String {
    let (here, absent): (Vec<_>, Vec<_>) = refs.iter().partition(|r| r.path.is_some());
    let mut out = String::new();
    if !here.is_empty() {
        let list: Vec<String> = here
            .iter()
            .map(|r| format!("@{} at {}", r.slug, r.path.as_deref().unwrap_or_default()))
            .collect();
        out.push_str(&format!(
            "This card references other repositories, checked out for you inside this \
             container and mounted READ-ONLY — read them for context, never write to them: {}.",
            list.join("; ")
        ));
    }
    if !absent.is_empty() {
        let list: Vec<String> = absent
            .iter()
            .map(|r| match r.git_remote_url.as_deref() {
                Some(url) => format!("@{} ({url})", r.slug),
                None => format!("@{}", r.slug),
            })
            .collect();
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!(
            "It also references {}, which this executor holds no checkout of — \
             they are not available to read on this run.",
            list.join("; ")
        ));
    }
    out
}

/// The opening turn: the skill's slash command, the human's seed, and what the
/// run was given to read.
///
/// Pure so the whole brief is a string a test can assert (AC-7), and shared by
/// both execution paths so the streaming agent and a tmux one cannot come to be
/// told different things about the same run.
fn opening_line(
    skill: &str,
    target: &str,
    seed: Option<&str>,
    references: &[nook_types::WorkspaceRef],
) -> String {
    let mut line = format!("/{skill} {target}");
    if let Some(s) = seed.filter(|s| !s.trim().is_empty()) {
        line.push(' ');
        line.push_str(s);
    }
    let refs = references_brief(references);
    if !refs.is_empty() {
        line.push(' ');
        line.push_str(&refs);
    }
    line
}

/// Deliver a human's steering message into a running job's session (MAIN-231):
/// type it at the live agent, exactly as the skill command was typed. The tmux
/// name is derived from the job id, so no cross-thread bookkeeping is needed —
/// if the session is gone the message has nowhere to land and that is reported,
/// never silently swallowed.
pub fn deliver_message(job_id: &str, body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("empty message".into());
    }

    // Streaming first (MAIN-240): write the turn as structured input. Note the
    // message is NOT flattened here — the tmux path had to collapse newlines
    // because `send_keys` would submit at the first one; structured input
    // carries a multi-line brief intact.
    {
        let handle = stream_writers()
            .lock()
            .ok()
            .and_then(|w| w.get(job_id).cloned());
        if let Some(stdin) = handle {
            return crate::job_adapter::write_turn(&stdin, body.trim());
        }
    }

    // Fallback: a tmux-driven job still gets its keys typed (NG-1).
    let line = one_line(body);
    if line.is_empty() {
        return Err("empty message".into());
    }
    let tmux_name = job_tmux_name(job_id);
    if !crate::tmux::session_exists(&tmux_name) {
        return Err("no live session for this job on this node".into());
    }
    crate::tmux::send_keys(&tmux_name, &line).map_err(|e| e.to_string())
}

/// Whether this JOB can act on GitHub (MAIN-406 AC-4 / MAIN-143 AC-5).
///
/// The credential the run will actually use, in the run's own precedence
/// (MAIN-456): the job's workspace token first — validated with `GH_TOKEN`
/// set in the check's env, exactly how the run env delivers it — and the
/// node's ambient reach only when the job carries none. Validating the node
/// instead of the job is the MAIN-468 prod failure: the operator's stored
/// login was revoked, the vault token was valid, and every review run died
/// at this check on the 5-minute backoff while carrying a credential that
/// worked. No fallback when a carried token is rejected, for the same
/// reason: the run WOULD use that token, so "the node could" is not an
/// answer.
///
/// The ambient lookup is [`crate::config::fleet_gh_token`], the same one the
/// session export uses, so this cannot come to disagree with what a session
/// receives. Checked as a PREFLIGHT rather than left to the skill: the
/// skill's own escalation is a comment on a PR, which is precisely what it
/// cannot post without this.
///
/// `pub(crate)` since MAIN-448: the managed review SESSION needs the identical
/// preflight, and a second copy of "can this node reach GitHub" is two answers
/// waiting to disagree about one machine.
pub(crate) fn gh_is_authenticated(delivered: Option<&str>) -> Result<(), String> {
    gh_preflight(delivered, crate::config::fleet_gh_token(), gh_auth_status)
}

/// What one `gh auth status` said, with the credential under test in its env.
enum GhProbe {
    Authorized,
    Refused,
    NoGh,
}

fn gh_auth_status(env_token: Option<&str>) -> GhProbe {
    let mut cmd = std::process::Command::new("gh");
    cmd.args(["auth", "status"]);
    if let Some(t) = env_token {
        // `GH_TOKEN` outranks both `GITHUB_TOKEN` and any stored login in gh's
        // own precedence, so setting it makes the probe answer for THIS token
        // — the same override the run env performs.
        cmd.env("GH_TOKEN", t);
    }
    match cmd.output() {
        Ok(o) if o.status.success() => GhProbe::Authorized,
        Ok(_) => GhProbe::Refused,
        Err(_) => GhProbe::NoGh,
    }
}

/// The decision, apart from the shelling-out, so AC-3's cases are testable
/// without a network or a real `gh`: which credential gets probed, and which
/// failure names which cause.
fn gh_preflight(
    delivered: Option<&str>,
    fleet: Option<String>,
    probe: impl Fn(Option<&str>) -> GhProbe,
) -> Result<(), String> {
    if let Some(token) = delivered.filter(|t| !t.trim().is_empty()) {
        return match probe(Some(token)) {
            GhProbe::Authorized => Ok(()),
            GhProbe::Refused => Err("the workspace token was rejected by GitHub".into()),
            GhProbe::NoGh => {
                Err("the job carries a workspace token but gh is not on PATH to use it".into())
            }
        };
    }
    if fleet.is_some() {
        return Ok(());
    }
    match probe(None) {
        GhProbe::Authorized => Ok(()),
        GhProbe::Refused => Err(
            "no credential: the job carries none and gh is installed but not authenticated".into(),
        ),
        GhProbe::NoGh => Err("no GitHub token (NOOK_GH_TOKEN) and no gh on PATH".into()),
    }
}

/// Which skill a job kind runs (MAIN-406 AC-2), or `None` when this build has
/// no mapping for it.
///
/// One place a reader can find, rather than a condition at the launch site.
///
/// `Option`, not a defaulted string, so the drift is VISIBLE. The first cut
/// returned `&str` with a `_ => "nook-spec"` arm, which made the test that was
/// supposed to catch an unmapped kind vacuous: every input satisfied it,
/// including the two that were already wrong. `epic-run` and `build` were
/// resolving to `nook-spec` with a green suite. The fallback still exists —
/// applied at the call site, so behaviour is unchanged (NG-4) — but it is no
/// longer able to hide from the test.
fn skill_for(kind: &str) -> Option<&'static str> {
    match kind {
        "spec" => Some("nook-spec"),
        "decompose" => Some("nook-epic"),
        "review" => Some("nook-review"),
        // The MERGE authority (MAIN-144): one manually-enqueued pass over one
        // epic's children. Same headless run machinery as everything else.
        "epic-run" => Some("nook-epic-runner"),
        "build" => Some("nook-build"),
        // The READ-ONLY one (MAIN-331): it reproduces and explains a support
        // report, and reports its findings onto the email chain. No branch, no
        // PR, and no forge credential to open one with.
        "investigate" => Some("nook-investigate"),
        _ => None,
    }
}

/// Kinds a node may advertise that deliberately have no mapping here YET.
///
/// Empty since MAIN-383 mapped `build`, the last unmapped kind — kept (with
/// its guard test) so the NEXT new kind must either get an arm above or a
/// recorded owner here, never a silent fall-through to the spec skill.
// Read only by the test below — the record is the point, not a runtime lookup.
// Scoped to the non-test build, where it genuinely is unused: in the test
// build it IS read, and a blanket allow there would hide a real orphan.
#[cfg_attr(not(test), allow(dead_code))]
const UNMAPPED_KINDS: &[(&str, &str)] = &[];

/// Whether this kind may CREATE the clone cache, or only use one already there.
///
/// **This was `kind != "review"` and is now true for every kind — a deliberate
/// relaxation of a rule whose threat model no longer holds.**
///
/// MAIN-406 refused a review job the right to clone because "review this repo"
/// arrived from a board signal and could name any repo, on a machine several
/// tenants share — an unbounded fetch. The property that bar protected now
/// holds by construction on EVERY path that raises a review: the reconciler
/// resolves the repo from a workspace's own `git_remote_url`, and the manual
/// path (`POST /api/v1/reviews`, still routed) requires a user token and
/// resolves its workspace by key inside the caller's tenant — so no caller,
/// manual or automatic, can name a remote the tenant never registered. Same
/// standing as the `spec` and `decompose` runs that have always cached freely.
///
/// Keeping the refusal made the feature unbuildable rather than safe: the
/// checkout clone-on-demand lands is a WORKING TREE in the node's workspace
/// root, and a job reads a bare mirror under `~/.nook/clone-cache`. They are
/// different things in different places, so "the repo is already cached" was
/// never true for a repo nothing had run a job on — every review failed with
/// "no clone cache", forever, however many times it was retried.
fn may_create_cache(_kind: &str) -> bool {
    true
}

/// The forge credential a run of this kind may hold (MAIN-331 AC-3).
///
/// An `investigate` run is READ-ONLY by contract, so it gets none — neither the
/// workspace's, which the control plane already withheld, nor this machine's
/// fleet token. Withholding only the first would be theatre: the fallback is on
/// this side, so the node would hand back exactly what the control plane
/// refused, and an unauthenticated `gh` is the difference between a run that
/// cannot open a PR and one that is merely asked not to.
///
/// Every other kind keeps the search order MAIN-407/456 gave it: the
/// workspace's own identity first, the fleet's only if there is none.
fn forge_token(kind: &str, delivered: Option<String>) -> Option<String> {
    match kind {
        "investigate" => None,
        _ => delivered.or_else(crate::config::fleet_gh_token),
    }
}

/// Which server the RUN's `nook` CLI dials: the advertised API outranks the
/// dialing address (MAIN-465). The job's token was minted by the control plane
/// that raised the run, and it should be spent — and reported — against that
/// plane's canonical URL, not the internal name this node happens to reach it
/// by. A deployment that advertises nothing keeps today's behavior exactly.
fn run_server<'a>(server_url: Option<&'a str>, cfg_server: &'a str) -> &'a str {
    match server_url {
        Some(u) if !u.trim().is_empty() => u,
        _ => cfg_server,
    }
}

/// Run one loop job to completion. Blocking; call under `spawn_blocking`.
pub fn run(cfg: NodeConfig, out: Sender<NodeToControl>, job: LoopJob) {
    let LoopJob {
        job_id,
        kind,
        review_pr_number,
        review_forced,
        gh_token,
        server_url,
        target_task_key,
        repo_url,
        branch,
        seed,
        workspace_id,
        ssh_key,
        nook_token,
        ports,
        unsatisfied_ports,
        secrets,
        references,
    } = job;
    // Which forge credential this run gets, decided ONCE (see `forge_token`).
    let gh_token = forge_token(&kind, gh_token);
    // …and what this MACHINE can actually honour of the card's `@slug`
    // references, decided once for the same reason: the mount list and the
    // brief must not be able to disagree about which repo the run was given.
    let references = resolvable_references(references);
    // A review run keeps ONE working directory per (workspace, PR), and a
    // build run one per (workspace, card) — the agent-session bucket is keyed
    // on it (see `warm_identity`). Everything else stays per-job.
    let warm = warm_identity(
        &kind,
        review_pr_number,
        workspace_id.as_deref(),
        &target_task_key,
    );
    let stable = warm.is_some();
    let dirname = warm
        .as_ref()
        .map(|(d, _)| d.clone())
        .unwrap_or_else(|| job_dirname(&job_id));
    if let Ok(mut s) = running_jobs().lock() {
        s.insert(dirname.clone());
    }
    // Registered HERE rather than beside `start_sandbox`, so there is no instant
    // between `docker run` and this node knowing about the container in which a
    // sweep could see it and call it an orphan (MAIN-617).
    if let Ok(mut s) = running_job_ids().lock() {
        s.insert(job_id.clone());
    }

    let base = cache_base(&cfg.server);
    let wt_base = base.join("worktrees");
    // Where THIS run's worktree will live — known before the fetch, which is
    // what lets `fetch_mirror` tell a self-inflicted wedge from someone else's.
    let own_worktree = wt_base.join(&dirname);

    note(
        &out,
        &job_id,
        format!("preparing workspace from {repo_url} @ {branch}"),
    );

    // BEFORE the mirror is fetched, free any branch this card's tree holds.
    //
    // A build worktree outlives its run ON the card's branch — that is what the
    // skill leaves behind — and a linked worktree of a `--mirror` clone shares
    // that repo's `refs/heads/*`, so the next `fetch --prune` is refused
    // fatally. `fetch_mirror`'s MAIN-466 self-heal answers its own refusal by
    // DELETING the tree, which was right when a tree lasted one run and would
    // silently undo this whole ticket. Detaching first means the refusal never
    // arises; the branch name is kept because it names the head this pass
    // should start from.
    let keeps_tree = is_build_dirname(&dirname);
    let card_branch = if keeps_tree && own_worktree.exists() {
        detach_worktree(&own_worktree)
    } else {
        None
    };
    // Collected rather than emitted inline because the mirror functions run
    // below the sender: a rewrite rule removed on the way to a fetch that then
    // failed is exactly when the reader needs the line, so they are drained on
    // the error paths too.
    let mut mirror_notes: Vec<String> = Vec::new();
    let cache = if may_create_cache(&kind) {
        match ensure_mirror_in(
            &base,
            &repo_url,
            ssh_key.as_deref(),
            &own_worktree,
            &mut mirror_notes,
        ) {
            Ok(c) => c,
            Err(e) => {
                drain_notes(&out, &job_id, &mut mirror_notes);
                finished(&out, &job_id, false, format!("clone cache failed: {e}"));
                unregister(&dirname, &job_id);
                return;
            }
        }
    } else {
        match existing_mirror_in(
            &base,
            &repo_url,
            ssh_key.as_deref(),
            &own_worktree,
            &mut mirror_notes,
        ) {
            Ok(c) => c,
            Err(e) => {
                drain_notes(&out, &job_id, &mut mirror_notes);
                // Names the workspace AND the node, because the reader of this
                // message is deciding WHERE to place the job next, and "no
                // checkout" without either is unactionable (AC-3).
                finished(
                    &out,
                    &job_id,
                    false,
                    format!(
                        "{e} — workspace {} has no checkout on node {}; a review job \
                         uses the existing clone cache rather than cloning on demand",
                        workspace_id.as_deref().unwrap_or("<unknown>"),
                        cfg.node_name
                    ),
                );
                unregister(&dirname, &job_id);
                return;
            }
        }
    };
    drain_notes(&out, &job_id, &mut mirror_notes);
    // A BUILD worktree OUTLIVES its run (MAIN-480 AC-1), so an existing one is
    // this card's own working state, not a crash leftover to clear. Every other
    // kind keeps the old behaviour exactly: the path is stable but the tree is
    // not, a leftover is cleared because the unique index says no live run
    // holds it, and one that will NOT clear still fails the job loudly in
    // `add_job_worktree_in` rather than being quietly reused.
    let mut adopted = keeps_tree && own_worktree.exists();
    if own_worktree.exists() && !adopted {
        let _ = remove_job_worktree(&cache, &own_worktree);
    }
    // The repository's OWN default branch, asked of the mirror — never the
    // `branch` the control plane resolved from a primary clone's checked-out
    // branch (AC-3).
    let default_branch = default_branch_name(&cache).unwrap_or_else(|| branch.clone());
    // "The pushed head": the card's branch as origin now has it, else the
    // default branch. Resolved after the fetch, so it is origin's answer rather
    // than this tree's.
    let pushed = pushed_head(&cache, card_branch.as_deref(), &default_branch);
    if adopted {
        // The lifecycle rule, decided here and nowhere else (AC-2).
        let state = classify_tree(&own_worktree, &pushed);
        note(&out, &job_id, state.note());
        match state {
            TreeState::Clean => match reset_clean_worktree(&own_worktree, &pushed) {
                Ok(swept) if !swept.is_empty() => note(
                    &out,
                    &job_id,
                    format!(
                        "swept {} untracked leftover(s): {}",
                        swept.len(),
                        swept.join(", ")
                    ),
                ),
                Ok(_) => {}
                Err(e) => {
                    // A tree that will not reset is git-level broken, which is
                    // the one case AC-1 allows recreation for. Say so — a
                    // silent rebuild looks identical to the bug this fixes.
                    note(
                        &out,
                        &job_id,
                        format!("this card's worktree could not be reset ({e}) — recreating it"),
                    );
                    let _ = remove_job_worktree(&cache, &own_worktree);
                    adopted = false;
                }
            },
            // Divergence is NAMED, never resolved here (NG-6): the agent holds
            // the context to rebase or discard it, this code does not.
            TreeState::Interrupted { .. } => note(
                &out,
                &job_id,
                format!(
                    "origin's {pushed} may have moved since that work — reconcile it in the \
                     tree; nothing here rebases on your behalf"
                ),
            ),
        }
    }
    // A STABLE worktree (review or build) is created DETACHED, so an attached
    // checkout cannot pin `branch` in the mirror and wedge every later fetch
    // (MAIN-466) — the skill makes its own branch anyway.
    let worktree = if adopted {
        own_worktree.clone()
    } else {
        match add_job_worktree_in(&wt_base, &cache, &default_branch, &dirname, stable) {
            Ok(w) => w,
            Err(e) => {
                finished(&out, &job_id, false, format!("worktree setup failed: {e}"));
                unregister(&dirname, &job_id);
                return;
            }
        }
    };
    // A FRESH build tree has only tracked files — no `.env`, no vendor
    // directories — so a first pass either dies on missing local config or pays
    // a cold build. Seed it from the workspace's primary checkout on this node
    // (MAIN-481). Creation only: MAIN-480's persistence keeps it warm after,
    // and re-copying would fight the tree the agent has been working in.
    //
    // Never fatal. A missing checkout or a failed copy costs a cold build; it
    // must not cost the run, so both are said out loud and the pass continues.
    if keeps_tree && !adopted {
        let settings = worktree_settings(&worktree);
        // Checked BEFORE the scan: `primary_checkout_for` walks every workspace
        // root and shells out to git per candidate, which is pure waste for a
        // repo that has opted out.
        if !settings.copy_ignored {
            note(
                &out,
                &job_id,
                "this repo sets `[worktree] copy_ignored = false` — seeding nothing",
            );
        } else {
            match primary_checkout_for(&cfg, &repo_url) {
                Some(source) => match seed_worktree(&source, &worktree, &settings) {
                    Ok(0) => note(
                        &out,
                        &job_id,
                        format!("nothing to seed from {}", source.display()),
                    ),
                    Ok(n) => note(
                        &out,
                        &job_id,
                        format!("seeded {n} ignored entr(ies) from {}", source.display()),
                    ),
                    Err(e) => note(&out, &job_id, format!("seeding this worktree failed: {e}")),
                },
                None => note(
                    &out,
                    &job_id,
                    format!(
                        "no primary checkout of {repo_url} under this node's workspace roots — \
                     nothing to seed, so this build starts cold (no .env, no vendor dirs)"
                    ),
                ),
            }
        }
    }
    // The launch guard (MAIN-482 AC-1/AC-2). Deliberately here — after every
    // step that can still MOVE the tree (adoption, reset, seeding) and before
    // anything that commits the platform to this directory, so a refused run
    // neither pins the card to a bad checkout nor starts an agent in one.
    //
    // Build only (AC-4): review, spec and epic-run trees are rebuilt every
    // pass and a human's session is governed by the skill, not by this.
    if kind == "build" {
        if let Some(refusal) = build_launch_refusal(
            &worktree,
            &wt_base,
            &known_checkout_paths(&cfg),
            attached_branch(&worktree).as_deref(),
            &default_branch,
        ) {
            refused(&out, &job_id, refusal.message(&worktree));
            unregister(&dirname, &job_id);
            return;
        }
    }

    // Tell the control plane where this card works (AC-4). It records the path
    // on the card, which is what pins later passes here, what `prune-worktree`
    // addresses, and what stops `reconcile` treating the tree as an orphan.
    if keeps_tree {
        let _ = out.blocking_send(NodeToControl::LoopWorktreeReady {
            job_id: job_id.clone(),
            path: worktree.to_string_lossy().to_string(),
        });
    }

    // The fallback lives here, not in the map (see `skill_for`): `spec` is the
    // original kind and an unknown one is refused upstream by
    // `capabilities::KNOWN_LOOP_KINDS`, so a kind reaching this line is one
    // this build advertises.
    let skill = skill_for(&kind).unwrap_or("nook-spec");

    // A review pass with no GitHub credential reads every PR as "nothing to
    // review" and exits zero — a silent empty pass, which MAIN-143 AC-5 names as
    // the thing never to allow. Provisioning the token is split 3's (NG-2); this
    // card's duty is to fail cleanly when it is absent rather than report a pass
    // that examined nothing.
    if kind == "review" || kind == "epic-run" {
        if let Err(e) = gh_is_authenticated(gh_token.as_deref()) {
            finished(
                &out,
                &job_id,
                false,
                format!(
                    "{e} — a review pass needs a GitHub credential (GH_TOKEN, or a \
                     logged-in gh). Without one every PR reads as \"nothing to \
                     review\" and the job would report success having examined none."
                ),
            );
            unregister(&dirname, &job_id);
            return;
        }
    }

    // AC-5: a skill the agent has never heard of makes `/nook-spec` ordinary
    // prose. The agent reads it, does nothing in particular, and the job
    // "succeeds" having produced no ticket — the exact silent no-op this card
    // exists to remove. Refuse before launching anything.
    if !crate::wizard::skills::is_installed(skill) {
        // A conclusion is a conclusion whatever the outcome (MAIN-493 AC-1), and
        // this one leaves a build tree behind holding the previous pass's output.
        if keeps_tree {
            reclaim_and_note(&out, &job_id, &worktree);
        }
        finished(
            &out,
            &job_id,
            false,
            format!(
                "skill {skill} not installed on this node — the loop skills ship \
                 with the agent binary and are written on `nook run`; this node \
                 is running a build that predates them, or could not write its \
                 agent skill directory"
            ),
        );
        unregister(&dirname, &job_id);
        return;
    }

    // THE CONFINEMENT (MAIN-611). Built here — after every check that can still
    // refuse the run, before anything that launches an agent — so a refused run
    // never starts a container and a launched agent is never outside one.
    //
    // Fail closed (AC-8): a host node that cannot set this up refuses the run
    // rather than falling back to an unconfined launch. It is a `refused`, not
    // a failure: nothing about the card is wrong, so nothing of the card's
    // strike budget should be spent, and the dispatcher's own gate normally
    // stops the job being placed here at all.
    let sandbox = match start_sandbox(&cfg, &kind, &job_id, &worktree, &cache, &ports, &references)
    {
        Ok(sb) => sb,
        Err(e) => {
            if keeps_tree {
                reclaim_and_note(&out, &job_id, &worktree);
            }
            refused(&out, &job_id, e);
            unregister(&dirname, &job_id);
            return;
        }
    };

    let tmux_name = job_tmux_name(&job_id);
    note(
        &out,
        &job_id,
        match sandbox.as_ref() {
            Some(sb) => format!(
                "launching claude in {} to run /{skill} {target_task_key} — confined to \
                 container {}",
                worktree.display(),
                sb.name()
            ),
            None => format!(
                "launching claude in {} to run /{skill} {target_task_key}",
                worktree.display()
            ),
        },
    );

    // Which execution strategy this runtime gets (MAIN-240). Claude speaks
    // stream-json, so it runs headless and the transcript comes from real
    // events; anything else keeps the tmux/PTY path untouched (NG-1).
    let (ok, message) = match crate::job_adapter::adapter_for(RUNTIME) {
        crate::job_adapter::Adapter::Streaming => drive_streaming(
            &out,
            &job_id,
            &worktree,
            RunBrief {
                skill,
                target: &target_task_key,
                seed: seed.as_deref(),
                review_pr: review_pr_number,
                review_forced,
                build_task: (kind == "build").then_some(target_task_key.as_str()),
                warm_session: warm.as_ref().map(|(_, sid)| sid.as_str()),
                ports: &ports,
                unsatisfied_ports: &unsatisfied_ports,
                secrets: &secrets,
                references: &references,
                sandbox: sandbox.as_ref(),
            },
            AgentIdentity {
                token: nook_token.as_deref(),
                server: run_server(server_url.as_deref(), &cfg.server),
                workspace_id: workspace_id.as_deref(),
                gh_token: gh_token.as_deref(),
            },
        ),
        crate::job_adapter::Adapter::Tmux => drive_session(
            &out,
            &job_id,
            &tmux_name,
            &worktree,
            skill,
            &target_task_key,
            seed.as_deref(),
            &references,
            workspace_id.as_deref(),
            &secrets,
            sandbox.as_ref(),
        ),
    };
    // Removing the container is what actually ends the run's processes — the
    // agent, its nested daemon, and any compose stack it left up inside. Done
    // before the tree is reclaimed, so nothing inside is still writing to it.
    drop(sandbox);

    // A BUILD worktree is NOT cleaned up here: it is this card's workplace
    // until the work merges (MAIN-480 AC-1), and deleting it at the end of
    // every pass is what made a repair start from scratch while its warm agent
    // session pointed at a directory that no longer existed. Removal is the
    // control plane's call now — `prune-worktree`, or the orphan sweep on
    // reconnect. Every other kind is cleaned up exactly as before (NG-2).
    //
    // What the tree does NOT keep is its build output (MAIN-493). The source
    // and the branch are what MAIN-480 was protecting; `target/` rode along
    // with them and reached 120 GB.
    if keeps_tree {
        reclaim_and_note(&out, &job_id, &worktree);
    } else if let Err(e) = remove_job_worktree(&cache, &worktree) {
        note(&out, &job_id, format!("worktree cleanup: {e}"));
    }
    unregister(&dirname, &job_id);
    finished(&out, &job_id, ok, message);
}

/// Put this run's agent in its own container, or refuse the run (MAIN-611).
///
/// `Ok(None)` is the ONE case that legitimately runs unconfined: a node that is
/// itself a container (NG-5). It mounts no Docker socket and sets no
/// `DOCKER_HOST`, so it cannot run a build at all and there is nothing here to
/// confine — refusing would take the shared operator's spec, review and
/// epic-run work offline the day this shipped, for no security gained.
///
/// Every other answer is `Ok(Some)` or an error. There is no fallback to a
/// direct launch: an agent whose instructions are untrusted input either runs
/// in the box or does not run.
#[allow(clippy::too_many_arguments)]
fn start_sandbox(
    cfg: &NodeConfig,
    kind: &str,
    job_id: &str,
    worktree: &Path,
    cache: &Path,
    ports: &[nook_types::LeasedPort],
    references: &[nook_types::WorkspaceRef],
) -> Result<Option<crate::sandbox::Sandbox>, String> {
    use crate::sandbox;
    let capability = sandbox::probe();
    if let nook_types::SandboxCapability::Exempt { .. } = capability {
        return Ok(None);
    }
    if let Some(detail) = capability.refusal() {
        return Err(format!(
            "this node cannot confine a loop-job agent, so it will not run one: \
             {detail}. Until it can, the agent would run as {} with that user's \
             whole home directory, credentials and LAN.",
            cfg.node_name
        ));
    }
    let server = cfg.server.clone();
    let mut add_hosts = Vec::new();
    let allow = sandbox::control_plane_allow(&server);
    // A control plane on the host's loopback (the dev stack) is reachable only
    // through the container's gateway, and its NAME has to resolve there too or
    // the agent's `nook` cannot reach the board it holds a token for.
    //
    // The alias is `HOST_ALIAS`, never the URL's own host: aliasing `localhost`
    // is written into `/etc/hosts` BEHIND Docker's own `127.0.0.1 localhost`,
    // the resolver takes the first match, and the entry does nothing. So the
    // URL handed to the agent is rewritten onto the alias to match.
    if allow.iter().any(|a| a == sandbox::HOST_GATEWAY) {
        add_hosts.push(format!("{}:host-gateway", sandbox::HOST_ALIAS));
    }
    let spec = sandbox::SandboxSpec {
        job_id: job_id.to_string(),
        image: sandbox::image(),
        profile: sandbox::profile_for(kind),
        isolation: sandbox::isolation(),
        worktree: worktree.to_path_buf(),
        // A linked worktree's `.git` is a FILE pointing into the mirror, so the
        // container gets source and no repository without this. The mirror of
        // this one repo — never the clone-cache root above it, which holds every
        // sibling checkout on the machine (AC-2).
        gitdir: Some(cache.to_path_buf()),
        claude_dir: Some(claude_config_dir()).filter(|d| d.is_dir()),
        caches: sandbox_caches(worktree),
        // Only what the CARD named, and only what this node holds — which is
        // what keeps AC-6 true: an unreferenced sibling checkout is not in this
        // list, so it is not in the container.
        references: references
            .iter()
            .filter_map(|r| r.path.as_deref().map(PathBuf::from))
            .collect(),
        ports: ports
            .iter()
            .filter_map(|p| u16::try_from(p.port).ok())
            .collect(),
        allow,
        add_hosts,
        server: sandbox::server_for_container(&server),
        // The container is root — the nested daemon and the firewall need it —
        // and the AGENT is not. What it writes into the bind-mounted checkout is
        // owned by the node's user, so the prune that follows can delete it
        // (MAIN-537).
        agent_uid: unsafe { libc::getuid() },
        agent_gid: unsafe { libc::getgid() },
    };
    sandbox::Sandbox::start(&spec).map(Some)
}

/// Live streaming sessions, so a steering message can be written to the right
/// agent's stdin — the structured replacement for `tmux send-keys` (MAIN-240).
///
/// Keyed by job id and holding only the stdin handle: the reader thread owns
/// the rest, and handing a writer around is all delivery needs.
type StreamWriters = std::collections::HashMap<String, crate::job_adapter::SharedStdin>;
fn stream_writers() -> &'static Mutex<StreamWriters> {
    static W: OnceLock<Mutex<StreamWriters>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn register_stream(job_id: &str, session: &crate::job_adapter::StreamingSession) {
    if let Ok(mut w) = stream_writers().lock() {
        w.insert(job_id.to_string(), session.stdin_handle());
    }
}

fn unregister_stream(job_id: &str) {
    if let Ok(mut w) = stream_writers().lock() {
        w.remove(job_id);
    }
}

/// The runtime a loop job drives. One constant because this slice has one; a
/// future job kind that needs another would carry it on the job.
const RUNTIME: &str = "claude";

/// Streaming execution (MAIN-240): run the agent headless and build the
/// transcript from structured events.
///
/// On a node restart mid-job this does NOT auto-resume. AC-5 allows either
/// resume or an honest failure, and the honest failure is already built and
/// tested: the process dies with the node, the job stays `running` with no
/// executor, and MAIN-164's reaper fails it within the grace window so it can
/// be re-run. Auto-resume would need the job's runtime session id to survive
/// the restart — it is recorded on the transcript (`agent session <id>`) so an
/// operator can resume by hand today, and a future ticket has what it needs.
///
/// The shape mirrors `drive_session` on purpose — same signature, same
/// `(ok, message)` contract, same transcript stream back to the control plane —
/// so the two adapters are interchangeable from `run`'s point of view and a
/// skill cannot tell which one is driving it.
/// What the agent needs to act as the JOB rather than as the machine it happens
/// to be running on. One struct because the three travel together and mean
/// nothing apart — and because passing them loose pushed `drive_streaming` past
/// clippy's argument limit, which was a fair complaint about the shape.
struct AgentIdentity<'a> {
    /// The job's tenant-scoped token, issued as its initiator. `None` means
    /// minting failed upstream and the agent falls back to this machine's login
    /// — the old behaviour, and the bug.
    token: Option<&'a str>,
    /// The control plane that issued `token`. A token is only meaningful against
    /// its issuer, so the two always travel together.
    server: &'a str,
    /// So `nook get workspace git-ssh` can name the repo it authenticates for.
    workspace_id: Option<&'a str>,
    /// The workspace's own forge token (MAIN-456); outranks the node's fleet
    /// env, so a tenant that configured its identity never speaks as the fleet.
    gh_token: Option<&'a str>,
}

/// What a run is ABOUT: which skill, on which target, from which brief, and —
/// for a review — which pull request.
///
/// Grouped for the reason [`AgentIdentity`] is: these travel together and
/// nothing reads one without the others, and loose they push `drive_streaming`
/// past clippy's argument limit — a fair complaint about the shape both times.
struct RunBrief<'a> {
    skill: &'a str,
    target: &'a str,
    seed: Option<&'a str>,
    /// The pull request a review run owns (MAIN-455): what the agent is told it
    /// is reviewing, and the session it resumes.
    review_pr: Option<u64>,
    /// A human forced this review at an already-verdicted head (MAIN-473).
    review_forced: bool,
    /// The ticket a build run owns (MAIN-383 AC-5): same contract as
    /// `review_pr`, for the kind whose unit is a card rather than a PR.
    build_task: Option<&'a str>,
    /// The warm session this run continues, from the ONE `warm_identity`
    /// decision in `run()` — threaded rather than re-derived so the session
    /// and the worktree cannot disagree by construction. `None` is a per-job
    /// session.
    warm_session: Option<&'a str>,
    /// The ports the control plane leased this run (MAIN-552), each under the
    /// variable the WORKSPACE named. Exported verbatim: this end recognises
    /// none of the names and must not, exactly as `tmux::spawn` does not.
    ports: &'a [nook_types::LeasedPort],
    /// Optional listeners that went unleased, under `NOOK_PORTS_UNSATISFIED`.
    unsatisfied_ports: &'a [String],
    /// The run's secret items (MAIN-625 AC-6), each under the name it was
    /// stored with. Exported verbatim: this end recognises none of them, the
    /// same property that keeps the port names a workspace's business.
    secrets: &'a [nook_types::SecretEnv],
    /// The workspaces the card names with `@slug` (MAIN-632), as this node can
    /// honour them. Named in the opening turn so the agent knows what it was
    /// handed and what it was not.
    references: &'a [nook_types::WorkspaceRef],
    /// The container this run's agent is confined to (MAIN-611 AC-1). `None`
    /// only on a node the sandbox profile exempts (NG-5).
    sandbox: Option<&'a crate::sandbox::Sandbox>,
}

/// The pairs a run's delivered secret items contribute to its environment
/// (MAIN-625 AC-6).
///
/// Pure, and for the reason [`crate::sandbox::run_args`] is: "a job's agent is
/// handed its workspace's secrets" is a claim about a vector, so it is a unit
/// test rather than a container run. Trivial by design — the control plane has
/// already applied the scope rules, and this end recognising a name is exactly
/// what it must never start doing.
fn secret_env(secrets: &[nook_types::SecretEnv]) -> Vec<(&str, &str)> {
    secrets
        .iter()
        .map(|s| (s.name.as_str(), s.value.as_str()))
        .collect()
}

fn drive_streaming(
    out: &Sender<NodeToControl>,
    job_id: &str,
    worktree: &Path,
    brief: RunBrief<'_>,
    identity: AgentIdentity<'_>,
) -> (bool, String) {
    let RunBrief {
        skill,
        target,
        seed,
        review_pr,
        review_forced,
        build_task,
        warm_session,
        ports,
        unsatisfied_ports,
        secrets,
        references,
        sandbox,
    } = brief;
    use crate::job_adapter;

    // A warm run continues ITS item's agent session — a review its PR's
    // (MAIN-455 AC-3), a build its card's (MAIN-460 AC-1) — so a second look
    // keeps the tree and the earlier reasoning instead of rebuilding both. The
    // id comes from `run()`'s single `warm_identity` decision, the first pass
    // pins it, and every later pass resumes it — decided by whether the
    // session file exists, never by hope. Still best effort across machines:
    // the session lives on ONE node, so a run placed elsewhere pins its own
    // and is a cold start, not a failure.
    let (args, resumed) = match warm_session {
        Some(sid) if agent_session_exists(sid) => (job_adapter::claude_resume_args(sid), true),
        Some(sid) => (job_adapter::claude_stream_args(sid), false),
        None => (job_adapter::claude_stream_args(job_id), false),
    };
    // Secrets go in FIRST, before every variable below (MAIN-625 AC-6).
    // `docker exec -e` and the direct spawn both take the LAST value for a
    // name, so this ordering is what stops a secret shadowing the run's own
    // credential, its leased ports, or the id it reports under. The names nook
    // sets for itself are refused at write time as well — belt and braces,
    // because only one of the two survives somebody adding a variable here.
    let mut env: Vec<(&str, &str)> = secret_env(secrets);
    env.extend([
        ("NOOK_JOB_ID", job_id),
        // The agent runs headless with `--dangerously-skip-permissions`
        // (job_adapter), which Claude Code refuses under root "for security
        // reasons" — and the node runs as root. But a per-job worktree on a
        // confined node genuinely IS a sandbox, and `IS_SANDBOX=1` is exactly
        // how Claude Code sanctions the flag there. Without it the agent exits 1
        // on launch and the run fails before it does anything.
        ("IS_SANDBOX", "1"),
    ]);
    if let Some(s) = seed.filter(|s| !s.trim().is_empty()) {
        env.push(("NOOK_JOB_SEED", s));
    }
    // The one PR this run owns. It replaces MAIN-446's shard pair: a run that is
    // told its item needs no arithmetic to work out which slice of the repo is
    // its own, and cannot pick a PR another run is already on.
    let pr_env;
    if let Some(pr) = review_pr {
        pr_env = pr.to_string();
        env.push(("NOOK_REVIEW_PR", &pr_env));
    }
    // A human forced this run at an already-verdicted head (MAIN-473): the
    // skill's already-reviewed skip-check stands aside for exactly this run.
    if review_forced {
        env.push(("NOOK_REVIEW_FORCED", "1"));
    }
    // The one ticket this run owns — `NOOK_REVIEW_PR`'s twin for builds
    // (MAIN-383 AC-5): the skill reads which card it was enqueued for instead
    // of re-deriving it from a pick.
    if let Some(key) = build_task.filter(|k| !k.is_empty()) {
        env.push(("NOOK_BUILD_TASK", key));
    }
    // The fleet's GitHub credential, under the name `gh` actually reads — the
    // SAME mapping the tmux path has done since MAIN-407, which this path
    // never got. Without it the node holds `NOOK_GH_TOKEN` while the agent's
    // `gh auth status` fails, and what happens next is agent improvisation:
    // one run noticed the fleet variable and exported it by hand, the next run
    // did not and died at preflight. A credential must not depend on the
    // agent's mood. Absent, nothing is exported at all — an empty `GH_TOKEN`
    // out-prefers a logged-in account, same reasoning as `tmux::spawn`.
    //
    // The fallback is resolved in `run` rather than here (MAIN-331): a
    // read-only run must reach this line with nothing to export, and a fallback
    // at the point of export would hand it the fleet's token regardless of what
    // the control plane withheld.
    let gh_env;
    if let Some(t) = identity.gh_token {
        gh_env = t.to_string();
        env.push(("GH_TOKEN", &gh_env));
    }
    // The agent's own identity, in the JOB's tenant. `AuthConfig::load` reads a
    // FILE, so without this `nook` inside the agent acts as whoever last ran
    // `nook login` on this machine — on a shared operator node, one human in one
    // tenant, which is how a job for another tenant's workspace listed the wrong
    // boards and drafted against the wrong one.
    //
    // The URL is spelled as the agent must use it where it RUNS. A sandboxed
    // agent is in a container, and this node's own spelling of a loopback
    // control plane means the CONTAINER's loopback there — nothing listens on
    // it, so `nook` preflight fails and the run ends having done nothing.
    let server_env = match sandbox {
        Some(_) => crate::sandbox::server_for_container(identity.server),
        None => identity.server.to_string(),
    };
    if let Some(t) = identity.token.filter(|t| !t.trim().is_empty()) {
        env.push(("NOOK_TOKEN", t));
        env.push(("NOOK_SERVER", &server_env));
    }
    // The ports this run leased (MAIN-552), each under the variable the
    // WORKSPACE named — so `dev-up.sh` in the worktree binds them instead of
    // compose's `${VAR:-default}` fallbacks, which every other stack on this
    // machine also falls back to. Nothing here recognises any of the names, the
    // same property that lets `tmux::spawn` serve a Next.js app and a Rust
    // backend without learning either.
    let port_values: Vec<(String, String)> = ports
        .iter()
        .map(|p| (p.env.clone(), p.port.to_string()))
        .collect();
    for (env_name, value) in &port_values {
        env.push((env_name.as_str(), value.as_str()));
    }
    // An ABSENT variable has two opposite meanings — "cloned outside nook, use
    // your default" and "the node ran out" — and only this distinguishes them
    // (MAIN-377). Same name a session gets, so a consumer reads one variable.
    let skipped = unsatisfied_ports.join(",");
    if !skipped.is_empty() {
        env.push(("NOOK_PORTS_UNSATISFIED", &skipped));
    }
    // The streaming adapter spawns the agent directly and never touches tmux,
    // so it never inherited what `tmux.rs` exports. `nook get workspace git-ssh`
    // needs this to name the repo it is authenticating for; without it, git
    // inside the agent silently falls back to the node's own key.
    if let Some(w) = identity.workspace_id.filter(|w| !w.trim().is_empty()) {
        env.push(("NOOK_WORKSPACE_ID", w));
        // …and the OTHER half of MAIN-367's mechanism, without which the
        // variable above feeds a shim nothing invokes: git resolves its ssh
        // through the shim, which pulls the workspace's pinned key — so a
        // build's `git push` speaks as the workspace, not as the node
        // (MAIN-460 AC-3). The shim falls through to plain ssh when nothing
        // is pinned, so this changes nothing for public repos and local
        // paths — the same reasoning as `tmux::spawn`'s export, and the two
        // must stay together the way tmux.rs's guard test says.
        env.push(("GIT_SSH_COMMAND", "nook get workspace git-ssh"));
    }

    let mut end = match run_agent_once(
        out, job_id, worktree, &args, &env, skill, target, seed, references, sandbox,
    ) {
        Ok(e) => e,
        Err(e) => return (false, e),
    };
    // A failed RESUME is a cold start, never a failed run (MAIN-460 AC-4). An
    // agent that exits before its first result record has ALMOST always loaded
    // nothing — a corrupt or foreign session file dies at launch — though a
    // crash mid-turn lands here too; the second launch then meets the skill's
    // own clean-tree preflight, which bounds the damage to one wasted pass.
    // The bad file is QUARANTINED first so the retry can pin the derived id:
    // left in place, every later pass for this item would pay the same
    // resume-fail-relaunch tax forever, warming nothing.
    if resumed && end.outcome.is_none() {
        note(
            out,
            job_id,
            "resuming the previous session failed — starting cold",
        );
        if let Some(sid) = warm_session {
            quarantine_agent_session(sid);
        }
        let cold = job_adapter::claude_stream_args(warm_session.unwrap_or(job_id));
        end = match run_agent_once(
            out, job_id, worktree, &cold, &env, skill, target, seed, references, sandbox,
        ) {
            Ok(e) => e,
            Err(e) => return (false, e),
        };
    }

    match end.outcome {
        Some((ok, message)) => (ok, message),
        // The stream ended without a result record: fall back to the exit code
        // and the tail, the same crash-honesty rule the tmux path uses (AC-4 of
        // MAIN-161) rather than reporting a success nobody observed.
        None => {
            let reason = match end.code {
                Some(0) => "the agent exited without a result record".to_string(),
                Some(c) => format!("the agent exited with status {c}"),
                None => "the agent died without an exit status".to_string(),
            };
            (
                false,
                if end.tail.is_empty() {
                    reason
                } else {
                    format!("{reason}\n{}", end.tail)
                },
            )
        }
    }
}

/// How one agent launch ended: the result record if one arrived, else the raw
/// exit facts for the crash-honesty fallback.
struct AgentEnd {
    outcome: Option<(bool, String)>,
    code: Option<i32>,
    tail: String,
}

/// One launch of the agent: spawn, send the skill command, pump events, wait.
/// `Err` is a launch that never got going (no process, no stdout, no first
/// send); `Ok` with `outcome: None` is an agent that started and died without
/// a result record — the shape a failed `--resume` produces, and the fact the
/// cold-start retry keys on.
// The launch's parameters, already grouped as `LoopJob` on the wire — the
// same reason `drive_session` carries this allow.
#[allow(clippy::too_many_arguments)]
fn run_agent_once(
    out: &Sender<NodeToControl>,
    job_id: &str,
    worktree: &Path,
    args: &[String],
    env: &[(&str, &str)],
    skill: &str,
    target: &str,
    seed: Option<&str>,
    references: &[nook_types::WorkspaceRef],
    sandbox: Option<&crate::sandbox::Sandbox>,
) -> Result<AgentEnd, String> {
    use crate::job_adapter::{self, Event, StreamingSession, TurnState};
    let mut session = StreamingSession::spawn(RUNTIME, args, worktree, env, sandbox)?;
    register_stream(job_id, &session);

    let Some(stdout) = session.take_stdout() else {
        session.kill();
        unregister_stream(job_id);
        return Err("the agent produced no stdout".into());
    };

    // The opening turn is the skill command — the same line the tmux path
    // types, now sent as structured input.
    let opening = opening_line(skill, target, seed, references);
    if let Err(e) = session.send(&opening) {
        session.kill();
        unregister_stream(job_id);
        return Err(format!("could not send the skill command: {e}"));
    }

    // Pump events on this thread; the child owns the pace.
    let tail = session.tail.clone();
    let stdin_for_close = session.stdin_handle();
    let mut turn = TurnState::default();
    let mut outcome: Option<(bool, String)> = None;
    let tx = out.clone();
    let id = job_id.to_string();

    job_adapter::pump_events(stdout, tail, |ev| match ev {
        Event::SessionStarted { session_id } => {
            // On the transcript, which is durable (MAIN-127) — an in-memory
            // copy would die with the very restart it is meant to survive.
            // This is the id an operator resumes with by hand (see AC-5 above).
            note(&tx, &id, format!("agent session {session_id}"));
        }
        Event::UserEcho(_) => {
            // DELIBERATELY not recorded. `--replay-user-messages` hands our own
            // turn back, and the control plane has already written that line:
            // `jobs::post_message` appends it on send, and a job's seed is
            // appended at create. Appending here too is why a steering message
            // appeared twice and read as the agent parroting you.
            //
            // The control plane has to be the one that records it. It is the
            // only end that can: a QUEUED job has no executor to echo anything,
            // an offline node never echoes, and the REST call must return the
            // entry it created. It also already distinguishes delivered from
            // not-delivered with its own system line, so recording on the echo
            // bought nothing the transcript did not already say.
            //
            // The echo is still parsed rather than ignored — `TurnState` sees
            // every event, and a silent hole in the vocabulary is how the next
            // record type becomes a surprise.
        }
        Event::AssistantText(text) => {
            if let Some(now) = turn.observe(&Event::AssistantText(text.clone())) {
                report_turn(&tx, &id, now);
            }
            let _ = tx.blocking_send(NodeToControl::JobTranscript {
                job_id: id.clone(),
                source: "agent".into(),
                content: text,
            });
        }
        Event::ToolUse { name } => {
            if let Some(now) = turn.observe(&Event::ToolUse { name: name.clone() }) {
                report_turn(&tx, &id, now);
            }
            let _ = tx.blocking_send(NodeToControl::JobTranscript {
                job_id: id.clone(),
                source: "agent".into(),
                content: format!("· {name}"),
            });
        }
        Event::TurnStarted => {
            if let Some(now) = turn.observe(&Event::TurnStarted) {
                report_turn(&tx, &id, now);
            }
        }
        Event::TurnEnded { ok, message } => {
            if let Some(now) = turn.observe(&Event::TurnEnded { ok, message: None }) {
                report_turn(&tx, &id, now);
            }
            outcome = Some((ok, message.unwrap_or_else(|| "turn complete".into())));
            // The run's result: no more turns are coming, so let the agent
            // exit. Without this the agent blocks reading stdin while we block
            // reading its stdout (see `close_stdin`).
            job_adapter::close_stdin(&stdin_for_close);
        }
        // A headless run launches with `--dangerously-skip-permissions`, so
        // this should never arrive (MAIN-502). If it does, DENY it: nobody is
        // here to answer, and the runtime blocks until it gets a reply — so
        // ignoring it would hang the job forever with no sign of why, which is
        // the one outcome worse than a failed pass.
        Event::PermissionRequest(req) => {
            let _ = tx.blocking_send(NodeToControl::JobTranscript {
                job_id: id.clone(),
                source: "system".into(),
                content: format!(
                    "the agent asked permission for {} — denied: a loop run has nobody to ask",
                    req.tool_name
                ),
            });
            let _ = job_adapter::write_line(
                &stdin_for_close,
                &job_adapter::permission_response_line(&req, false),
            );
        }
        Event::Ignored => {}
    });

    // A stream that died mid-turn would otherwise leave the UI showing
    // "working" forever.
    if turn.active() {
        report_turn(out, job_id, false);
    }

    let code = session.wait();
    unregister_stream(job_id);

    Ok(AgentEnd {
        outcome,
        code,
        tail: session.tail_text(),
    })
}

/// Tell the control plane whether a turn is in flight (AC-2). A real signal
/// off real events — the thing screen-scraping could only guess at.
fn report_turn(out: &Sender<NodeToControl>, job_id: &str, active: bool) {
    let _ = out.blocking_send(NodeToControl::JobTurn {
        job_id: job_id.to_string(),
        active,
    });
}

/// Both registries, always together: a job id left behind here would make the
/// sweep spare that job's container forever, which is the leak this replaced.
fn unregister(dirname: &str, job_id: &str) {
    if let Ok(mut s) = running_jobs().lock() {
        s.remove(dirname);
    }
    if let Ok(mut s) = running_job_ids().lock() {
        s.remove(job_id);
    }
}

/// Launch the runtime, stream its PTY output verbatim, drive the skill, and wait
/// for the session to end (or time out). Returns `(ok, message)` for
/// `JobFinished`.
// One session launch's parameters, already grouped as `LoopJob` on the wire —
// the same reason `new_job_session` carries this allow.
#[allow(clippy::too_many_arguments)]
fn drive_session(
    out: &Sender<NodeToControl>,
    job_id: &str,
    tmux_name: &str,
    worktree: &Path,
    skill: &str,
    target: &str,
    seed: Option<&str>,
    references: &[nook_types::WorkspaceRef],
    workspace_id: Option<&str>,
    secrets: &[nook_types::SecretEnv],
    sandbox: Option<&crate::sandbox::Sandbox>,
) -> (bool, String) {
    let cwd = worktree.to_string_lossy().to_string();
    // Where the launched shell records the runtime's exit code (AC-4). A sibling
    // of the worktree, so `git worktree remove` doesn't take it, and read once
    // the session ends.
    let status_file = worktree.with_extension("nook-status");
    let status_path = status_file.to_string_lossy().to_string();
    // The session id doubles as the job id here — a loop job has no separate
    // session row, and `nook` inside only needs *a* stable id plus NOOK_JOB_ID.
    if let Err(e) = crate::tmux::new_job_session(
        tmux_name,
        &cwd,
        120,
        40,
        "claude",
        job_id,
        job_id,
        &status_path,
        seed,
        workspace_id,
        // No NOOK_TENANT_ID for a loop job, deliberately. It already carries a
        // tenant-SCOPED token (`NOOK_TOKEN`, issued as the job's initiator), and
        // an identity beats a hint: `nook` inside acts in the job's tenant
        // because of who it is, not because of a variable it was told. Adding a
        // second source of truth here is how the two drift.
        None,
        secrets,
        sandbox,
    ) {
        return (false, format!("could not launch session: {e}"));
    }

    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = crate::tmux::kill_session(tmux_name);
            return (false, format!("openpty failed: {e}"));
        }
    };

    let mut cmd = CommandBuilder::new("tmux");
    // Like the session attach in sessions.rs, this spawns tmux directly through
    // portable_pty and so must carry the node's `-L <socket>` itself, BEFORE
    // `attach` — the job's session lives on that private server (MAIN-108 AC-2).
    if let Some(sock) = crate::tmux::socket_name() {
        cmd.args(["-L", sock]);
    }
    cmd.args(["attach", "-t", tmux_name]);
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = crate::tmux::kill_session(tmux_name);
            return (false, format!("attach failed: {e}"));
        }
    };
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = crate::tmux::kill_session(tmux_name);
            return (false, e.to_string());
        }
    };
    // Keep the PTY master alive for the session's lifetime; dropping it early
    // would tear the attach client down.
    let _master = pair.master;

    // Output pump: PTY → JobTranscript. A bounded tail is kept for the finish
    // message. The chunk is recorded exactly as it arrives — never parsed (NG-2).
    let tail = Arc::new(Mutex::new(VecDeque::<u8>::new()));
    let pump = {
        let out = out.clone();
        let job_id = job_id.to_string();
        let tail = tail.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Ok(mut t) = tail.lock() {
                            t.extend(chunk.iter().copied());
                            while t.len() > TAIL_BYTES {
                                t.pop_front();
                            }
                        }
                        let content = String::from_utf8_lossy(chunk).into_owned();
                        let frame = NodeToControl::JobTranscript {
                            job_id: job_id.clone(),
                            source: "agent".into(),
                            content,
                        };
                        if out.blocking_send(frame).is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };

    // Let the runtime come up, then drive the skill by typing its slash command.
    // A seeded job (MAIN-231) puts the human's brief on that same line, so the
    // skill receives it as its argument — the session env carries the verbatim
    // text for anything that wants the original line breaks.
    std::thread::sleep(SKILL_STARTUP_DELAY);
    // Flattened AFTER composing, not before: `send_keys -l` submits at the
    // first newline, and the reference brief is composed from paths that could
    // carry one.
    let line = one_line(&opening_line(skill, target, seed, references));
    if let Err(e) = crate::tmux::send_keys(tmux_name, &line) {
        note(out, job_id, format!("could not send skill command: {e}"));
    }

    // Wait for the session to end, enforcing the timeout.
    let start = Instant::now();
    let mut timed_out = false;
    loop {
        if !crate::tmux::session_exists(tmux_name) {
            break;
        }
        if start.elapsed() >= DEFAULT_TIMEOUT {
            timed_out = true;
            let _ = crate::tmux::kill_session(tmux_name);
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = child.wait();
    let _ = pump.join();

    let tail_str = tail
        .lock()
        .ok()
        .map(|t| String::from_utf8_lossy(&t.iter().copied().collect::<Vec<u8>>()).into_owned())
        .unwrap_or_default();

    if timed_out {
        (
            false,
            format!("timed out after {} minutes", DEFAULT_TIMEOUT.as_secs() / 60),
        )
    } else {
        // The launched shell wrote the runtime's exit code to the sentinel: a
        // non-zero code is an honest failure (AC-4). An absent/unreadable status
        // (killed, or never written) is not proof of failure — treat it as a
        // clean end, same as an interactive session that surfaces no exit code.
        let status = read_exit_status(&status_file);
        let ok = exit_is_ok(status);
        let tail = tail_str.trim_end();
        let reason = match status {
            Some(0) => "loop session ended".to_string(),
            Some(code) => format!("agent exited with status {code}"),
            None => "session ended abnormally — no exit status recorded (killed)".to_string(),
        };
        let message = if tail.is_empty() {
            reason
        } else {
            format!("{reason}\n{tail}")
        };
        (ok, message)
    }
}

/// Read and consume the runtime's exit-status sentinel. `None` when the file is
/// absent or unparseable — the session was killed or the shell never wrote it.
fn read_exit_status(path: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(path).ok();
    let _ = std::fs::remove_file(path);
    raw?.split_whitespace().next()?.parse::<i32>().ok()
}

/// A job succeeded only if the launched shell recorded a **zero** exit status
/// (AC-4). The shell writes `$?` on ANY normal end of the runtime — zero or not —
/// so an ABSENT status is not "unknown", it is abnormal death: the session was
/// killed before the shell could write it (external `tmux kill`, OOM, the node
/// tearing the pane down). That is a failure, not a clean end.
fn exit_is_ok(status: Option<i32>) -> bool {
    matches!(status, Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(slug: &str, path: Option<&str>) -> nook_types::WorkspaceRef {
        nook_types::WorkspaceRef {
            workspace_id: nook_types::WorkspaceId::new(),
            name: format!("The {slug}"),
            slug: slug.into(),
            git_remote_url: Some(format!("git@example.test:acme/{slug}.git")),
            path: path.map(str::to_string),
        }
    }

    /// MAIN-632 AC-7: the run is TOLD what it has — every referenced repo, the
    /// path it is at inside the container, and that it is read-only. An agent
    /// handed a checkout and not told about it will never look at it.
    #[test]
    fn the_brief_names_each_reference_its_path_and_that_it_is_read_only() {
        let refs = vec![
            reference("nook-web", Some("/checkouts/nook-web")),
            reference("nook-api", Some("/checkouts/nook-api")),
        ];
        let line = opening_line("nook-build", "MAIN-42", None, &refs);

        assert!(line.starts_with("/nook-build MAIN-42 "), "{line}");
        for r in &refs {
            let path = r.path.as_deref().unwrap();
            assert!(line.contains(path), "the brief omits {path}: {line}");
            assert!(
                line.contains(&format!("@{}", r.slug)),
                "the brief omits @{}: {line}",
                r.slug
            );
        }
        assert!(
            line.contains("READ-ONLY"),
            "the brief does not say the mounts are read-only, so the agent will \
             discover it by failing to write: {line}"
        );
    }

    /// MAIN-632 AC-8: a reference this executor holds no checkout of is named
    /// as a gap, with the reason. Saying nothing would read as "the mention did
    /// not resolve", and the agent would go back to guessing.
    #[test]
    fn the_brief_names_a_reference_the_executor_could_not_provide() {
        let line = opening_line(
            "nook-build",
            "MAIN-42",
            None,
            &[
                reference("nook-web", Some("/checkouts/nook-web")),
                reference("nook-api", None),
            ],
        );
        assert!(line.contains("/checkouts/nook-web"), "{line}");
        assert!(line.contains("@nook-api"), "{line}");
        assert!(
            line.contains("no checkout"),
            "the brief does not say WHY the reference is unavailable: {line}"
        );
        assert!(
            !line.contains("@nook-api at"),
            "an unavailable reference was given a path: {line}"
        );
    }

    /// The seed still reaches the skill, and the reference brief follows it —
    /// the human's own words are the argument, not an afterthought behind a
    /// machine-generated sentence.
    #[test]
    fn the_seed_stays_the_skills_argument() {
        let line = opening_line(
            "nook-spec",
            "MAIN-42",
            Some("focus on the retry path"),
            &[reference("nook-web", Some("/checkouts/nook-web"))],
        );
        assert!(
            line.starts_with("/nook-spec MAIN-42 focus on the retry path "),
            "{line}"
        );
    }

    /// Nearly every card names nothing, and that card's opening turn must be
    /// byte-for-byte what it was before this shipped.
    #[test]
    fn a_card_with_no_references_says_nothing_extra() {
        assert_eq!(
            opening_line("nook-spec", "MAIN-42", Some("do it"), &[]),
            "/nook-spec MAIN-42 do it"
        );
        assert_eq!(
            opening_line("nook-spec", "MAIN-42", None, &[]),
            "/nook-spec MAIN-42"
        );
    }

    /// MAIN-632 AC-8, the node's half: the control plane's path came from a
    /// `node_workspaces` row, which records what this node last REPORTED. A
    /// path that has since gone must not reach `docker run` — Docker creates a
    /// missing bind source, as root, on the owner's own machine.
    #[test]
    fn a_reference_whose_checkout_has_gone_is_downgraded_here() {
        let present = std::env::temp_dir().join(format!(
            "nook-632-present-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&present).expect("present checkout");

        let refs = resolvable_references(vec![
            reference("here", present.to_str()),
            reference("gone", Some("/definitely/not/a/path/on/this/machine")),
        ]);

        assert_eq!(refs[0].path.as_deref(), present.to_str());
        assert_eq!(
            refs[1].path, None,
            "a path that is not on the disk stays out of the mount list"
        );
        let _ = std::fs::remove_dir_all(&present);
    }

    /// MAIN-505 AC-5: what gates an agent update is the LOOP-job registry and
    /// nothing else. Terminal sessions are tmux's — they are never registered
    /// here, so a machine full of them still reads zero and updates promptly.
    #[test]
    fn in_flight_counts_registered_loop_jobs_only() {
        let key = "build-in-flight-probe";
        assert!(!running_jobs().lock().unwrap().contains(key));
        let before = in_flight();
        running_jobs().lock().unwrap().insert(key.to_string());
        assert_eq!(in_flight(), before + 1);
        unregister(key, "job-in-flight-probe");
        assert_eq!(in_flight(), before);
    }

    /// MAIN-617: a run registers its JOB ID as well as its worktree name, and
    /// gives both back. The sweep's whole safety argument is that a live job is
    /// in this set — a registration that leaked would spare a dead job's
    /// container forever, and one that never happened would delete a live one's.
    #[test]
    fn a_run_registers_and_releases_its_job_id() {
        let dir = "build-sweep-probe";
        let job = "0199-sweep-probe";
        assert!(!running_job_ids().lock().unwrap().contains(job));
        running_jobs().lock().unwrap().insert(dir.to_string());
        running_job_ids().lock().unwrap().insert(job.to_string());
        unregister(dir, job);
        assert!(!running_job_ids().lock().unwrap().contains(job));
        assert!(!running_jobs().lock().unwrap().contains(dir));
    }

    /// MAIN-625 AC-6: the environment handed to a job's agent carries the
    /// items the control plane selected, verbatim and in order.
    #[test]
    fn a_runs_environment_carries_its_delivered_secrets() {
        let delivered = vec![
            nook_types::SecretEnv {
                name: "FLEET_KEY".into(),
                value: "from-the-tenant".into(),
            },
            nook_types::SecretEnv {
                name: "REPO_KEY".into(),
                value: "from-the-workspace".into(),
            },
        ];
        assert_eq!(
            secret_env(&delivered),
            vec![
                ("FLEET_KEY", "from-the-tenant"),
                ("REPO_KEY", "from-the-workspace"),
            ]
        );
        // A run for a workspace with nothing set gains no variables at all —
        // the overwhelmingly common case, and it must stay free.
        assert!(secret_env(&[]).is_empty());
    }

    // ── MAIN-481: seeding a fresh build worktree ───────────────────────────

    /// A checkout holding one of each class the seed must tell apart.
    fn checkout_with_every_class(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join(".gitignore"), ".env\n*.log\nvendor/\n").unwrap();
        std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
        commit_all(dir, "init");
        // Ignored: the warm layer the seed exists to carry.
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.join("build.log"), "noise\n").unwrap();
        std::fs::create_dir_all(dir.join("vendor/pkg")).unwrap();
        std::fs::write(dir.join("vendor/pkg/lib.rs"), "// expensive\n").unwrap();
        // Untracked but NOT ignored: a human's stray file, never copied.
        std::fs::write(dir.join("notes.txt"), "my notes\n").unwrap();
    }

    /// MAIN-481 AC-1: ignored files come across — including a whole directory —
    /// and untracked-not-ignored files do not, because MAIN-480's sweep deletes
    /// that class anyway.
    #[test]
    fn the_seed_copies_ignored_files_and_leaves_stray_ones_behind() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-copy-{}", uuid::Uuid::now_v7().simple()));
        let source = tmp.join("primary");
        let dest = tmp.join("worktree");
        checkout_with_every_class(&source);
        std::fs::create_dir_all(&dest).unwrap();

        let copied = seed_worktree(&source, &dest, &WorktreeSettings::default()).expect("seed");

        assert!(dest.join(".env").exists(), "the config a run needs");
        assert!(
            dest.join("vendor/pkg/lib.rs").exists(),
            "a wholly ignored directory comes as one entry, contents and all"
        );
        assert!(dest.join("build.log").exists());
        assert!(
            !dest.join("notes.txt").exists(),
            "untracked-not-ignored is the class the next pass sweeps — never seed it"
        );
        assert!(
            !dest.join("README.md").exists(),
            "tracked files arrive from git, not from the copy"
        );
        assert_eq!(copied, 3);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-481 AC-2: `exclude` carves patterns out, with real gitignore
    /// semantics — the answer comes from git, not a hand-rolled glob.
    #[test]
    fn exclude_patterns_carve_entries_out_of_the_copy() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-excl-{}", uuid::Uuid::now_v7().simple()));
        let source = tmp.join("primary");
        let dest = tmp.join("worktree");
        checkout_with_every_class(&source);
        std::fs::create_dir_all(&dest).unwrap();

        let settings = WorktreeSettings {
            copy_ignored: true,
            exclude: vec!["*.log".into(), "vendor/".into()],
            ..WorktreeSettings::default()
        };
        seed_worktree(&source, &dest, &settings).expect("seed");

        assert!(dest.join(".env").exists(), "not excluded, still copied");
        assert!(!dest.join("build.log").exists(), "excluded by `*.log`");
        assert!(!dest.join("vendor").exists(), "excluded by `vendor/`");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-481 AC-2: a repo can opt out entirely.
    #[test]
    fn copy_ignored_false_seeds_nothing() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-optout-{}", uuid::Uuid::now_v7().simple()));
        let source = tmp.join("primary");
        let dest = tmp.join("worktree");
        checkout_with_every_class(&source);
        std::fs::create_dir_all(&dest).unwrap();

        let settings = WorktreeSettings {
            copy_ignored: false,
            exclude: vec![],
            ..WorktreeSettings::default()
        };
        assert_eq!(seed_worktree(&source, &dest, &settings).expect("seed"), 0);
        assert!(!dest.join(".env").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-481 AC-1: what the tree already has wins. The worktree is the live
    /// working state; the seed is a convenience and must never overwrite it.
    #[test]
    fn the_seed_never_overwrites_what_the_worktree_already_has() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-keep-{}", uuid::Uuid::now_v7().simple()));
        let source = tmp.join("primary");
        let dest = tmp.join("worktree");
        checkout_with_every_class(&source);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join(".env"), "MINE=1\n").unwrap();

        seed_worktree(&source, &dest, &WorktreeSettings::default()).expect("seed");

        assert_eq!(
            std::fs::read_to_string(dest.join(".env")).unwrap(),
            "MINE=1\n",
            "the worktree's own file survives the seed"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-481 AC-2: the settings are the repo's own, and anything unreadable
    /// or unknown falls back to the default rather than failing a build.
    #[test]
    fn the_worktree_section_is_read_and_unknown_keys_are_tolerated() {
        let tmp =
            std::env::temp_dir().join(format!("nook-481-cfg-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&tmp).unwrap();

        assert_eq!(
            worktree_settings(&tmp),
            WorktreeSettings::default(),
            "no file at all is the default, not a refusal"
        );

        std::fs::write(
            tmp.join(".nook.toml"),
            "[ports]\nname = \"web\"\n\n[worktree]\ncopy_ignored = false\nexclude = [\"a\"]\nfuture_key = 3\n",
        )
        .unwrap();
        assert_eq!(
            worktree_settings(&tmp),
            WorktreeSettings {
                copy_ignored: false,
                exclude: vec!["a".into()],
                reclaim: vec!["target".into()],
            },
            "an unknown key and an unrelated section are both tolerated, and a \
             key this file does not mention keeps its default"
        );

        // MAIN-493: naming the key is how a repo opts out, and an empty list has
        // to mean "nothing", not "the default" — which is what a bare
        // `unwrap_or_default` on the field would have made it.
        std::fs::write(tmp.join(".nook.toml"), "[worktree]\nreclaim = []\n").unwrap();
        assert_eq!(worktree_settings(&tmp).reclaim, Vec::<String>::new());

        std::fs::write(tmp.join(".nook.toml"), "this is not toml {{{").unwrap();
        assert_eq!(
            worktree_settings(&tmp),
            WorktreeSettings::default(),
            "a broken settings file costs the seed, never the run"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// One repository written two ways is still one repository — the shapes a
    /// remote is stored in must not decide whether a tree gets seeded.
    #[test]
    fn two_remote_url_shapes_of_one_repo_match() {
        assert_eq!(
            same_repo_key("https://github.com/o/r.git"),
            same_repo_key("https://github.com/o/r/")
        );
        assert_ne!(
            same_repo_key("https://github.com/o/r"),
            same_repo_key("https://github.com/o/other")
        );
    }

    /// MAIN-481 AC-1: a symlink is RECREATED, never followed.
    ///
    /// The bug this pins filled a node's disk: a mutual pair inside an ignored
    /// directory (`node_modules` is where these live) was walked as real
    /// directories until the OS refused the path length. Its mirror, a dangling
    /// link, aborted the entire seed on `ENOENT`.
    #[test]
    fn symlinks_are_recreated_rather_than_walked() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-link-{}", uuid::Uuid::now_v7().simple()));
        let source = tmp.join("primary");
        let dest = tmp.join("worktree");
        checkout_with_every_class(&source);
        std::fs::create_dir_all(source.join("vendor/a")).unwrap();
        std::fs::create_dir_all(source.join("vendor/b")).unwrap();
        #[cfg(unix)]
        {
            // The cycle, and a link pointing at nothing.
            std::os::unix::fs::symlink("../b", source.join("vendor/a/blink")).unwrap();
            std::os::unix::fs::symlink("../a", source.join("vendor/b/alink")).unwrap();
            std::os::unix::fs::symlink("gone", source.join("vendor/dangling")).unwrap();
        }
        std::fs::create_dir_all(&dest).unwrap();

        let copied = seed_worktree(&source, &dest, &WorktreeSettings::default())
            .expect("a symlink farm must not fail the seed");
        assert!(copied > 0);

        #[cfg(unix)]
        {
            let link = std::fs::symlink_metadata(dest.join("vendor/a/blink")).expect("blink");
            assert!(link.file_type().is_symlink(), "recreated as a link");
            assert!(
                std::fs::symlink_metadata(dest.join("vendor/dangling")).is_ok(),
                "a dangling link is copied as a link, not an error"
            );
            // A correctly recreated pair still RESOLVES through — that is what
            // links do — so the thing to pin is that nothing was materialised:
            // the buggy walk turned each of these into real nested directories
            // and made thousands of them.
            let a: Vec<_> = std::fs::read_dir(dest.join("vendor/a"))
                .unwrap()
                .flatten()
                .collect();
            assert_eq!(a.len(), 1, "one entry, the link itself");
            assert!(
                std::fs::symlink_metadata(a[0].path())
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "and it is a link, not a walked copy of the cycle"
            );
        }
        assert!(dest.join(".env").exists(), "and the rest still lands");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-481 AC-2: the exclude answer comes from the REPO's patterns alone.
    /// A `.env` line in an operator's global gitignore must not carve `.env`
    /// out of every repo's seed — that is the file this feature exists to
    /// carry, and node git config is not a second home for the setting (NG-1).
    #[test]
    fn a_global_gitignore_cannot_carve_entries_out_of_the_seed() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-glob-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let global = tmp.join("global-ignore");
        std::fs::write(&global, ".env\n").unwrap();

        // `excluded_by` neutralises core.excludesFile, so this global rule is
        // invisible to it even while git is told to read the file.
        //
        // The config file is written BEFORE the env points at it, so the var
        // never names a path that does not yet exist — a concurrent git that
        // read it mid-setup used to see a missing file. The real protection is
        // that every FIXTURE git now ignores this variable (`hermetic_git`);
        // this only narrows the source. `set_var` across threads is a data race
        // regardless, which is why the readers, not the writer, are the fix.
        std::fs::write(
            tmp.join("gitconfig"),
            format!("[core]\n\texcludesFile = {}\n", global.display()),
        )
        .unwrap();
        let prev = std::env::var("GIT_CONFIG_GLOBAL").ok();
        unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", tmp.join("gitconfig")) };

        let entries = vec![".env".to_string(), "build.log".to_string()];
        let skip = excluded_by(&["*.log".to_string()], &entries).expect("excludes");

        match prev {
            Some(v) => unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", v) },
            None => unsafe { std::env::remove_var("GIT_CONFIG_GLOBAL") },
        }

        assert!(skip.contains("build.log"), "the repo's own pattern applies");
        assert!(
            !skip.contains(".env"),
            "a node's global ignore must not decide what a repo seeds"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-481 AC-3/AC-5: no primary checkout under the node's roots is the
    /// logged-skip case, and it must actually be reachable — nothing else
    /// pinned that `primary_checkout_in` can return `None`.
    #[test]
    fn a_missing_primary_checkout_is_none_and_a_linked_worktree_is_never_the_source() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-481-src-{}", uuid::Uuid::now_v7().simple()));
        let root = tmp.join("roots");
        std::fs::create_dir_all(&root).unwrap();
        let roots = vec![root.to_string_lossy().to_string()];
        let remote = "https://github.com/o/r.git";

        assert_eq!(
            primary_checkout_in(&roots, remote),
            None,
            "an empty root is the AC-3 skip, not a panic"
        );

        // A repo of the RIGHT remote, but reached as a linked worktree: as bare
        // as the tree being seeded, so choosing it would seed nothing.
        let primary = tmp.join("elsewhere");
        std::fs::create_dir_all(&primary).unwrap();
        git_in(&primary, &["init", "-b", "main"]);
        git_in(&primary, &["remote", "add", "origin", remote]);
        std::fs::write(primary.join("README.md"), "# demo\n").unwrap();
        commit_all(&primary, "init");
        git_in(
            &primary,
            &[
                "worktree",
                "add",
                &root.join("linked").to_string_lossy(),
                "-b",
                "side",
            ],
        );
        assert_eq!(
            primary_checkout_in(&roots, remote),
            None,
            "a linked worktree is never the seed source"
        );

        // The same repo as a real checkout under the root IS found.
        git_in(
            &tmp,
            &[
                "clone",
                "-q",
                &primary.to_string_lossy(),
                &root.join("real").to_string_lossy(),
            ],
        );
        git_in(&root.join("real"), &["remote", "set-url", "origin", remote]);
        assert_eq!(
            primary_checkout_in(&roots, remote),
            Some(root.join("real")),
            "a primary checkout of the right remote is the source"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── MAIN-480: the build worktree's lifecycle ────────────────────────────

    /// Only build trees outlive their run; review keeps a stable PATH but is
    /// rebuilt every pass, and per-job dirs are per-job (NG-2).
    #[test]
    fn only_build_worktrees_are_the_persistent_kind() {
        assert!(is_build_dirname(&build_dirname("ws-1", "MAIN-42")));
        assert!(!is_build_dirname(&review_dirname("ws-1", 7)));
        assert!(!is_build_dirname(&job_dirname("0193-abc")));
    }

    /// The shape a real build pass leaves behind: a worktree of the mirror with
    /// the card's BRANCH checked out, optionally pushed to origin. Every
    /// lifecycle test runs against this, because the detached tree the node
    /// creates only exists until the skill's first `checkout -b`.
    fn build_tree_after_a_pass(tmp: &Path, key: &str, push: bool) -> (PathBuf, PathBuf, String) {
        let remote = tmp.join("remote");
        scratch_remote(&remote);
        let wt_base = tmp.join("worktrees");
        let dirname = build_dirname("ws-1", key);
        let own = wt_base.join(&dirname);
        let cache = ensure_mirror_in(
            &tmp.join("cache"),
            &remote.to_string_lossy(),
            None,
            &own,
            &mut Vec::new(),
        )
        .expect("mirror clone");
        let wt = add_job_worktree_in(&wt_base, &cache, "main", &dirname, true).expect("worktree");
        // What the skill does: its own branch, a commit, and a push if the pass
        // got that far.
        let branch = format!("{}-work", key.to_lowercase());
        git_in(&wt, &["checkout", "-b", &branch]);
        std::fs::write(wt.join("feature.rs"), "fn feature() {}\n").unwrap();
        commit_all(&wt, "the pass's work");
        if push {
            // By URL, not by the `origin` remote: a linked worktree of a
            // `--mirror` clone inherits `remote.origin.mirror`, which refuses a
            // refspec. The destination is the same repository either way.
            git_in(&wt, &["push", &remote.to_string_lossy(), &branch]);
        }
        (cache, wt, branch)
    }

    /// MAIN-480 AC-1, the defect this ticket exists to prevent: the tree must
    /// survive the NEXT pass's mirror fetch.
    ///
    /// It did not. The skill leaves the tree on the card's branch, a worktree
    /// of a `--mirror` clone shares `refs/heads/*`, and `fetch --prune` is then
    /// refused — whereupon `fetch_mirror`'s self-heal recognised this run's own
    /// path and DELETED the tree. Freeing the branch first is the fix.
    #[test]
    fn the_worktree_survives_the_next_passs_fetch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-480-fetch-{}", uuid::Uuid::now_v7().simple()));
        let (cache, wt, branch) = build_tree_after_a_pass(&tmp, "MAIN-42", true);

        // Exactly what `run` does at the start of the next pass.
        assert_eq!(detach_worktree(&wt).as_deref(), Some(branch.as_str()));
        fetch_mirror(&cache, None, &wt).expect("the fetch must not be refused");

        assert!(
            wt.exists(),
            "the card's worktree must outlive the fetch — deleting it here is the bug"
        );
        assert!(wt.join("feature.rs").exists(), "with its contents intact");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-480 AC-2, the clean half: a pass that pushed holds nothing origin
    /// lacks, so it resets — DETACHED at the card's own pushed head, never at
    /// the default branch, and never by moving the card's branch ref.
    #[test]
    fn a_pushed_tree_resets_detached_to_its_own_pushed_head() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-480-clean-{}", uuid::Uuid::now_v7().simple()));
        let (cache, wt, branch) = build_tree_after_a_pass(&tmp, "MAIN-43", true);
        let pass_head = git_in(&wt, &["rev-parse", "HEAD"]);

        // The warm layer, plus a stray the sweep must take.
        std::fs::write(wt.join(".gitignore"), "warm/\n").unwrap();
        commit_all(&wt, "ignore warm");
        git_in(
            &wt,
            &["push", &tmp.join("remote").to_string_lossy(), &branch],
        );
        let pushed_tip = git_in(&wt, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(wt.join("warm")).unwrap();
        std::fs::write(wt.join("warm/target-cache"), "expensive\n").unwrap();
        std::fs::write(wt.join("scratch.txt"), "stray\n").unwrap();

        detach_worktree(&wt);
        fetch_mirror(&cache, None, &wt).expect("fetch");
        let pushed = pushed_head(&cache, Some(&branch), "main");
        assert_eq!(
            pushed,
            format!("refs/heads/{branch}"),
            "the card's own branch is the head to start from, not the default branch"
        );
        assert_eq!(classify_tree(&wt, &pushed), TreeState::Clean);

        let swept = reset_clean_worktree(&wt, &pushed).expect("reset");
        assert_eq!(swept, vec!["scratch.txt".to_string()]);
        assert!(
            wt.join("warm/target-cache").exists(),
            "ignored files are the warm layer — `clean -fd` must never take them"
        );
        assert_eq!(
            git_in(&wt, &["rev-parse", "HEAD"]),
            pushed_tip,
            "reset to the card's pushed head"
        );
        assert_ne!(pass_head, pushed_tip, "the fixture actually moved the head");
        assert_eq!(
            git_in(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD",
            "and left DETACHED, or the next fetch is wedged again"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-480 AC-2: a run that committed and died before pushing leaves the
    /// only copy of that work here. With the branch attached — the real shape —
    /// the old probe answered "nothing unpushed" and reset it away.
    #[test]
    fn an_unpushed_commit_on_the_cards_branch_is_interrupted() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-480-unpush-{}", uuid::Uuid::now_v7().simple()));
        let (cache, wt, branch) = build_tree_after_a_pass(&tmp, "MAIN-44", false);

        detach_worktree(&wt);
        fetch_mirror(&cache, None, &wt).expect("fetch");
        // Never pushed, so the fetch's --prune took the local branch with it:
        // the head to compare against falls back to the default branch, and the
        // commit is visibly absent from origin.
        let pushed = pushed_head(&cache, Some(&branch), "main");
        assert_eq!(pushed, "refs/heads/main");
        assert_eq!(
            classify_tree(&wt, &pushed),
            TreeState::Interrupted {
                uncommitted: false,
                unpushed: true
            },
            "a committed-but-unpushed pass is the case worth an hour of work"
        );
        assert!(wt.join("feature.rs").exists(), "and it is left untouched");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-480 AC-2: uncommitted work on the card's branch is interrupted too.
    #[test]
    fn an_uncommitted_tree_is_interrupted_and_says_so() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-480-dirty-{}", uuid::Uuid::now_v7().simple()));
        let (cache, wt, branch) = build_tree_after_a_pass(&tmp, "MAIN-45", true);
        std::fs::write(wt.join("feature.rs"), "fn feature() { /* half-done */ }\n").unwrap();

        detach_worktree(&wt);
        fetch_mirror(&cache, None, &wt).expect("fetch");
        let state = classify_tree(&wt, &pushed_head(&cache, Some(&branch), "main"));
        assert_eq!(
            state,
            TreeState::Interrupted {
                uncommitted: true,
                unpushed: false
            }
        );
        assert!(state.note().contains("resuming interrupted work"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-480 AC-3: the base comes from the MIRROR's own HEAD, so a primary
    /// clone parked on a feature branch cannot become the base every build
    /// starts from.
    #[test]
    fn the_base_is_the_repositorys_default_branch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-480-base-{}", uuid::Uuid::now_v7().simple()));
        let remote = tmp.join("remote");
        scratch_remote(&remote);
        // The repository's default stays `main`. `someones-feature` exists and
        // is what `resolve_repo` would have handed down, having read it off a
        // primary clone that happened to be parked there — the exact shape that
        // poisoned the base for every build worktree.
        git_in(&remote, &["branch", "someones-feature"]);
        let cache = ensure_mirror_in(
            &tmp.join("cache"),
            &remote.to_string_lossy(),
            None,
            &tmp.join("worktrees/none"),
            &mut Vec::new(),
        )
        .expect("mirror clone");
        assert_eq!(default_branch_name(&cache).as_deref(), Some("main"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── MAIN-482: where a build run may and may not launch ─────────────────

    /// A cache worktree, a primary clone and a human's worktree, as real
    /// directories — `build_launch_refusal` canonicalizes, so paths that exist
    /// are what the guard actually sees on a node.
    struct Layout {
        tmp: PathBuf,
        wt_base: PathBuf,
        cache_worktree: PathBuf,
        checkouts: Vec<PathBuf>,
    }

    fn layout(tag: &str) -> Layout {
        let tmp =
            std::env::temp_dir().join(format!("nook-482-{tag}-{}", uuid::Uuid::now_v7().simple()));
        let wt_base = tmp.join("clone-cache/cp/worktrees");
        let cache_worktree = wt_base.join(build_dirname("ws-1", "MAIN-482"));
        let primary = tmp.join("workspace/nook-os");
        let human = tmp.join("workspace/nook-os-feature");
        for d in [&cache_worktree, &primary, &human] {
            std::fs::create_dir_all(d).unwrap();
        }
        Layout {
            tmp,
            wt_base,
            cache_worktree,
            checkouts: vec![primary, human],
        }
    }

    /// AC-1: the primary clone is the human's escape hatch for parallel work
    /// while the loop runs (NG-2). A build run pointed at it is refused before
    /// the agent starts, not merely discouraged in prose.
    #[test]
    fn a_build_run_is_refused_in_a_checkout_outside_the_clone_cache() {
        let l = layout("outside");
        let primary = l.checkouts[0].clone();

        let refusal = build_launch_refusal(&primary, &l.wt_base, &l.checkouts, None, "main");

        assert_eq!(refusal, Some(LaunchRefusal::OutsideCache));
        let said = refusal.unwrap().message(&primary);
        assert!(
            said.contains(&primary.display().to_string()) && said.contains("clone cache"),
            "the transcript must name the directory and the rule: {said}"
        );
        let _ = std::fs::remove_dir_all(&l.tmp);
    }

    /// AC-1's second half: a path can be inside the cache AND be a checkout
    /// this node reports — an operator who pointed a workspace root at the
    /// cache. Being reported to the control plane is what makes a directory
    /// somebody's, so containment alone is not enough.
    #[test]
    fn a_build_run_is_refused_in_a_directory_the_node_reports_as_a_checkout() {
        let l = layout("known");
        let mut checkouts = l.checkouts.clone();
        checkouts.push(l.cache_worktree.clone());

        assert_eq!(
            build_launch_refusal(&l.cache_worktree, &l.wt_base, &checkouts, None, "main"),
            Some(LaunchRefusal::KnownCheckout)
        );
        let _ = std::fs::remove_dir_all(&l.tmp);
    }

    /// AC-2: attached to the repository's default branch is the one HEAD a
    /// build pass may not start from, and the refusal names the branch.
    #[test]
    fn a_build_run_is_refused_on_the_default_branch() {
        let l = layout("default");

        let refusal = build_launch_refusal(
            &l.cache_worktree,
            &l.wt_base,
            &l.checkouts,
            Some("main"),
            "main",
        );

        assert_eq!(refusal, Some(LaunchRefusal::OnDefaultBranch("main".into())));
        assert!(
            refusal.unwrap().message(&l.cache_worktree).contains("main"),
            "which branch was refused is the whole of the message"
        );
        // …and it is the REPOSITORY's default, not the string `main`: a repo
        // whose default is `trunk` refuses `trunk` and permits `main`.
        assert_eq!(
            build_launch_refusal(
                &l.cache_worktree,
                &l.wt_base,
                &l.checkouts,
                Some("main"),
                "trunk"
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&l.tmp);
    }

    /// AC-2: both legitimate start states pass — detached at the pushed head
    /// (a clean pass) and attached to the card's own branch (resuming work an
    /// interrupted pass left behind).
    #[test]
    fn a_detached_cache_worktree_and_the_cards_own_branch_both_launch() {
        let l = layout("ok");

        assert_eq!(
            build_launch_refusal(&l.cache_worktree, &l.wt_base, &l.checkouts, None, "main"),
            None,
            "detached at the pushed head is how every clean pass starts"
        );
        assert_eq!(
            build_launch_refusal(
                &l.cache_worktree,
                &l.wt_base,
                &l.checkouts,
                Some("main-482-guards"),
                "main"
            ),
            None,
            "attached to the card's own branch is how a resumed pass starts"
        );
        let _ = std::fs::remove_dir_all(&l.tmp);
    }

    /// The cache base itself is not a worktree of the cache. A build run whose
    /// path resolved to it would be operating on the directory that holds
    /// every card's tree.
    #[test]
    fn the_worktree_base_itself_is_not_a_place_to_build() {
        let l = layout("base");

        assert_eq!(
            build_launch_refusal(&l.wt_base, &l.wt_base, &l.checkouts, None, "main"),
            Some(LaunchRefusal::OutsideCache)
        );
        let _ = std::fs::remove_dir_all(&l.tmp);
    }

    /// MAIN-480 AC-1: reconcile is what deleted a build tree on every node
    /// restart. It must now spare build trees — whose normal state between
    /// passes is "no running job" — while still sweeping every other kind.
    #[test]
    fn reconcile_spares_build_worktrees_and_still_sweeps_the_rest() {
        let tmp =
            std::env::temp_dir().join(format!("nook-480-recon-{}", uuid::Uuid::now_v7().simple()));
        let wt_base = tmp.join("worktrees");
        let build = wt_base.join(build_dirname("ws-1", "MAIN-45"));
        let review = wt_base.join(review_dirname("ws-1", 11));
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&review).unwrap();

        reconcile_in(&tmp);

        assert!(
            build.exists(),
            "a build tree between passes has no running job — that is not an orphan"
        );
        assert!(!review.exists(), "every other kind is swept as before");
        assert_eq!(
            held_build_worktrees_in(&wt_base, &HashSet::new()),
            vec![build.to_string_lossy().to_string()],
            "what it keeps, it reports — the control plane decides if it is still wanted"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-537: the report is what the control plane removes from, and a tree
    /// whose run has not yet reported `LoopWorktreeReady` is recorded by no card
    /// — so reporting it while the run is live offers up a working directory
    /// with a build in it. The node is the only side that knows.
    #[test]
    fn a_tree_this_node_is_building_in_is_not_reported() {
        let tmp =
            std::env::temp_dir().join(format!("nook-537-held-{}", uuid::Uuid::now_v7().simple()));
        let wt_base = tmp.join("worktrees");
        let live = build_dirname("ws-1", "MAIN-537");
        let idle = build_dirname("ws-1", "MAIN-505");
        std::fs::create_dir_all(wt_base.join(&live)).unwrap();
        std::fs::create_dir_all(wt_base.join(&idle)).unwrap();

        let running: HashSet<String> = [live].into_iter().collect();

        assert_eq!(
            held_build_worktrees_in(&wt_base, &running),
            vec![wt_base.join(&idle).to_string_lossy().to_string()],
            "only the tree no pass is using is offered for collection"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── MAIN-493: reclaiming a concluded run's build output ─────────────────

    /// Give a build tree the shape the reclaim has to tell apart: ignored build
    /// output, ignored config that is NOT output, and tracked source.
    fn build_tree_with_output(wt: &Path) {
        std::fs::write(wt.join(".gitignore"), "/target\n/.cache/\n.env\n").unwrap();
        commit_all(wt, "ignore the build output");
        for dir in ["target/debug/.fingerprint", ".cache/cargo-target/debug"] {
            std::fs::create_dir_all(wt.join(dir)).unwrap();
            std::fs::write(wt.join(dir).join("artifact.rlib"), vec![7u8; 4096]).unwrap();
        }
        std::fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
    }

    /// MAIN-493 AC-1: the output goes, and everything MAIN-480 was actually
    /// protecting — the branch, the commits, the tracked source, the warm
    /// `.env` — stays exactly where it was.
    #[test]
    fn a_concluded_run_loses_its_build_output_and_keeps_its_git_state() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-493-drop-{}", uuid::Uuid::now_v7().simple()));
        let (_cache, wt, branch) = build_tree_after_a_pass(&tmp, "MAIN-42", true);
        build_tree_with_output(&wt);
        let head = git_in(&wt, &["rev-parse", "HEAD"]);

        let settings = WorktreeSettings {
            reclaim: vec!["target".into(), ".cache/cargo-target".into()],
            ..WorktreeSettings::default()
        };
        let got = reclaim_build_output(&wt, &settings);

        assert_eq!(got.refused, Vec::<String>::new());
        assert_eq!(got.removed, vec!["target", ".cache/cargo-target"]);
        assert_eq!(got.bytes, 8192, "what it freed, counted before it went");
        assert!(!wt.join("target").exists());
        assert!(!wt.join(".cache/cargo-target").exists());

        assert!(
            wt.join("feature.rs").exists(),
            "tracked source is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join(".env")).unwrap(),
            "SECRET=1\n",
            "an ignored file that is not build output is not build output"
        );
        assert_eq!(
            git_in(&wt, &["rev-parse", "HEAD"]),
            head,
            "the commit stays"
        );
        assert_eq!(
            git_in(&wt, &["symbolic-ref", "--short", "HEAD"]),
            branch,
            "and so does the branch — MAIN-480 is not being reversed (NG-1)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-493 AC-2, the one that must not regress: concluding one run leaves
    /// every OTHER card's worktree, and its in-progress `target/`, alone.
    ///
    /// This is what keys the cleanup on a conclusion rather than a sweep. A
    /// pass that listed the worktrees directory and deleted the output of the
    /// ones with no running job would race a build that started between the
    /// listing and the delete — one was observed being created mid-cleanup on
    /// 2026-08-09 (MAIN-210).
    #[test]
    fn concluding_one_run_never_touches_another_cards_worktree() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-493-two-{}", uuid::Uuid::now_v7().simple()));
        let (_c1, mine, _b1) = build_tree_after_a_pass(&tmp.join("a"), "MAIN-47", false);
        let (_c2, theirs, _b2) = build_tree_after_a_pass(&tmp.join("b"), "MAIN-48", false);
        build_tree_with_output(&mine);
        build_tree_with_output(&theirs);

        reclaim_build_output(&mine, &WorktreeSettings::default());

        assert!(!mine.join("target").exists(), "this run's output goes");
        assert!(
            theirs
                .join("target/debug/.fingerprint/artifact.rlib")
                .exists(),
            "a build running next door keeps every byte of its target/"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-493 AC-1: the guard that makes a wrong entry cost nothing. A path
    /// this repo does not IGNORE is source — whatever it is called — and the
    /// refusal is named rather than silent.
    #[test]
    fn reclaim_refuses_a_path_the_repo_does_not_ignore() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-493-src-{}", uuid::Uuid::now_v7().simple()));
        let (_cache, wt, _b) = build_tree_after_a_pass(&tmp, "MAIN-43", false);
        build_tree_with_output(&wt);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/main.rs"), "fn main() {}\n").unwrap();
        commit_all(
            &wt,
            "tracked source in a directory somebody named in reclaim",
        );

        let settings = WorktreeSettings {
            reclaim: vec!["src".into(), "target".into()],
            ..WorktreeSettings::default()
        };
        let got = reclaim_build_output(&wt, &settings);

        assert!(wt.join("src/main.rs").exists(), "source survives a typo");
        assert_eq!(got.removed, vec!["target"], "the real entry still runs");
        assert_eq!(got.refused.len(), 1);
        assert!(
            got.refused[0].starts_with("src ("),
            "and says which and why"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-493 AC-1: a declared path may not leave the worktree, by `..` or by
    /// a symlink — `target` pointed at a shared cache would otherwise hand
    /// `remove_dir_all` the whole of it.
    #[test]
    fn reclaim_cannot_escape_the_worktree() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-493-esc-{}", uuid::Uuid::now_v7().simple()));
        let (_cache, wt, _b) = build_tree_after_a_pass(&tmp, "MAIN-44", false);
        build_tree_with_output(&wt);
        let outside = tmp.join("shared-cache");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("everyone-elses.rlib"), vec![1u8; 16]).unwrap();

        assert_eq!(normalized_reclaim_entry("../shared-cache"), None);
        assert_eq!(normalized_reclaim_entry("/etc"), Some("etc".into()));
        assert_eq!(
            normalized_reclaim_entry("  ./target/ "),
            Some("target".into())
        );

        #[cfg(unix)]
        {
            std::fs::remove_dir_all(wt.join("target")).unwrap();
            std::os::unix::fs::symlink(&outside, wt.join("target")).unwrap();
            let settings = WorktreeSettings {
                reclaim: vec!["../shared-cache".into(), "target".into()],
                ..WorktreeSettings::default()
            };
            assert_eq!(reclaim_build_output(&wt, &settings), Reclaimed::default());
            assert!(
                outside.join("everyone-elses.rlib").exists(),
                "a symlinked target is not this tree's to delete"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-493 AC-4: every absence is a no-op — a worktree that is gone, a
    /// tree that never built, and a second reclaim of the first one's work.
    #[test]
    fn reclaiming_twice_or_nothing_is_never_an_error() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-493-idem-{}", uuid::Uuid::now_v7().simple()));
        let (_cache, wt, _b) = build_tree_after_a_pass(&tmp, "MAIN-46", false);
        build_tree_with_output(&wt);
        let settings = WorktreeSettings {
            reclaim: vec!["target".into()],
            ..WorktreeSettings::default()
        };

        assert!(!reclaim_build_output(&wt, &settings).removed.is_empty());
        assert_eq!(
            reclaim_build_output(&wt, &settings),
            Reclaimed::default(),
            "the second pass over an already-reclaimed tree finds nothing to do"
        );
        assert_eq!(
            reclaim_build_output(&tmp.join("never-existed"), &settings),
            Reclaimed::default(),
            "and a worktree that is gone is not an error either"
        );
        assert_eq!(
            reclaim_note(&Reclaimed::default()),
            None,
            "and says nothing"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-493: a repo opts out by declaring an empty list, and that must not
    /// shell out to git at all — an empty `reclaim` is the answer, not a query.
    #[test]
    fn an_empty_reclaim_list_reclaims_nothing() {
        let settings = WorktreeSettings {
            reclaim: vec![],
            ..WorktreeSettings::default()
        };
        assert_eq!(
            reclaim_build_output(Path::new("/nonexistent-worktree"), &settings),
            Reclaimed::default()
        );
    }

    /// The transcript line is the operator's only view of what a pass gave
    /// back, so both halves of a partial reclaim have to reach it.
    #[test]
    fn the_reclaim_note_reports_what_went_and_what_did_not() {
        let note = reclaim_note(&Reclaimed {
            removed: vec!["target".into()],
            bytes: 3 * 1024 * 1024 * 1024,
            refused: vec![".cache/cargo-target (Permission denied)".into()],
        })
        .expect("something happened");
        assert_eq!(
            note,
            "reclaimed 3.0 GiB of build output (target); could not reclaim \
             .cache/cargo-target (Permission denied)"
        );
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    // ── MAIN-538: what THIS repo declares, and what a refusal costs ─────────

    /// MAIN-538 AC-2 and AC-4: the safety argument applied to the entries this
    /// repo actually ships, not only to invented ones.
    ///
    /// `reclaim_build_output`'s guards make a wrong entry a no-op, which is
    /// what lets the list be edited without fear — but "a no-op" is a silent
    /// outcome, so nothing would have told us that a `reclaim` entry had
    /// stopped matching. This reads the real `.nook.toml` and asserts every
    /// declared path is one git IGNORES, which is exactly the condition under
    /// which the delete is reachable at all.
    #[test]
    fn this_repos_declared_reclaim_names_only_ignored_build_output() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/nook-node sits two below the repo root");
        let declared = worktree_settings(root).reclaim;
        assert!(
            declared.len() > 1,
            "this repo reclaims more than target/ (AC-1), got {declared:?}"
        );

        let normalized: Vec<String> = declared
            .iter()
            .map(|r| {
                normalized_reclaim_entry(r)
                    .unwrap_or_else(|| panic!("{r} does not name a path inside the worktree"))
            })
            .collect();
        assert!(
            normalized.iter().any(|r| r == "frontend/node_modules"),
            "the pnpm half is declared (AC-3), got {normalized:?}"
        );

        // AC-2: `exclude` answers "too big to copy", `reclaim` answers
        // "regenerable". `.nook-secrets/` is the case that separates them — it
        // is excluded from seeding and holds the fleet's Claude session, which
        // no rebuild reproduces.
        for kept in [
            ".nook-secrets",
            ".cache/cargo-registry",
            ".cache/web-node-modules",
            "frontend/.pnpm-store",
        ] {
            assert!(
                !normalized.iter().any(|r| r == kept),
                "{kept} is deliberately NOT reclaimed"
            );
        }

        if !git_available()
            || crate::gitops::run_git(&["rev-parse", "--git-dir"], Some(root), None).is_err()
        {
            eprintln!("skipping the ignore half: no usable git checkout at the repo root");
            return;
        }
        // Asked with a trailing slash, which is how git is told to treat a path
        // it cannot see as a directory. `node_modules/` is a directory-ONLY
        // pattern, so `check-ignore frontend/node_modules` answers "not
        // ignored" purely because this checkout has not installed the frontend.
        // Production never asks that question — `reclaimable_dir` filters to
        // existing directories before `ignored_by_repo` runs.
        let as_dirs: Vec<String> = normalized.iter().map(|r| format!("{r}/")).collect();
        let ignored = ignored_by_repo(root, &as_dirs);
        let unignored: Vec<&String> = as_dirs.iter().filter(|r| !ignored.contains(*r)).collect();
        assert!(
            unignored.is_empty(),
            "these declared paths are not ignored here, so the reclaim would refuse them and free nothing: {unignored:?}"
        );
    }

    /// Whether a read-only directory actually refuses an unlink on this run.
    ///
    /// It does not for root, which is why this exists: `./test.sh`'s default
    /// path is `docker compose exec` into the control-plane container, and that
    /// image sets no `USER`, so the suite runs as uid 0 and
    /// `remove_dir_all` walks straight through `0o500`.
    ///
    /// Every step is best-effort on purpose. As root the probe's own
    /// `remove_dir_all` SUCCEEDS, so the scaffold is already gone by the time
    /// there is anything to restore — an `unwrap` on that restore panicked
    /// before the caller could reach its skip, which is the bug this shape
    /// fixes rather than the behaviour it tests.
    #[cfg(unix)]
    fn read_only_dirs_refuse_unlink(tmp: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        let probe = tmp.join("root-probe");
        let _ = std::fs::remove_dir_all(&probe);
        if std::fs::create_dir_all(probe.join("inner")).is_err()
            || std::fs::write(probe.join("inner/x"), b"x").is_err()
            || std::fs::set_permissions(probe.join("inner"), std::fs::Permissions::from_mode(0o500))
                .is_err()
        {
            return false;
        }
        let refused = std::fs::remove_dir_all(&probe).is_err();
        let _ =
            std::fs::set_permissions(probe.join("inner"), std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&probe);
        refused
    }

    /// MAIN-538 AC-5 and AC-6: one path that cannot be removed must not become
    /// a silent skip, and must not take the entries behind it down with it.
    ///
    /// This is how the root-owned `.cache/cargo-target` problem surfaced at
    /// all. The containers write parts of the target dirs as root, so a
    /// host-side delete half-failing is the EXPECTED shape here rather than an
    /// exotic one — and the run has already concluded, so the only thing left
    /// to get right is the report.
    #[cfg(unix)]
    #[test]
    fn a_refused_entry_is_named_and_does_not_stop_the_ones_behind_it() {
        use std::os::unix::fs::PermissionsExt;

        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-538-part-{}", uuid::Uuid::now_v7().simple()));
        let (_cache, wt, _b) = build_tree_after_a_pass(&tmp, "MAIN-538", false);
        build_tree_with_output(&wt);

        // Root ignores permission bits (`CAP_DAC_OVERRIDE`), so uid 0 cannot
        // stage this failure at all — and uid 0 is the DEFAULT path here:
        // `./test.sh` runs the suite with `docker compose exec` into the
        // control-plane container, which sets no `USER` and no compose `user:`.
        // Probed BEFORE the subject is built, so the skip cannot strand a
        // half-sealed tree, and probed without `unwrap` because as root the
        // probe deletes its own scaffold and there is nothing left to restore.
        if !read_only_dirs_refuse_unlink(&tmp) {
            eprintln!("skipping: running as root, permission bits do not bind");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }

        // Sealed at the INNER directory, not the outer one: `remove_dir_all`
        // recurses, so a read-only outer directory still lets it unlink the
        // artifact inside and fail only on the final rmdir — a half-delete
        // that would make "was anything preserved?" untestable. Read-only
        // `inner` refuses the unlink itself, so the failure frees nothing.
        let sealed = wt.join("sealed");
        std::fs::create_dir_all(sealed.join("inner")).unwrap();
        std::fs::write(sealed.join("inner/artifact.rlib"), vec![7u8; 4096]).unwrap();
        std::fs::write(wt.join(".gitignore"), "/target\n/.cache/\n/sealed/\n.env\n").unwrap();
        commit_all(&wt, "ignore the sealed directory too");
        std::fs::set_permissions(sealed.join("inner"), std::fs::Permissions::from_mode(0o500))
            .unwrap();

        let settings = WorktreeSettings {
            reclaim: vec!["sealed".into(), "target".into()],
            ..WorktreeSettings::default()
        };
        let got = reclaim_build_output(&wt, &settings);

        // Best-effort: the assertions below, not the cleanup, are what fail a
        // broken run.
        let _ =
            std::fs::set_permissions(sealed.join("inner"), std::fs::Permissions::from_mode(0o700));

        assert_eq!(
            got.removed,
            vec!["target"],
            "the entry behind the refusal still runs — a refusal is not an abort"
        );
        assert!(!wt.join("target").exists());
        assert_eq!(got.refused.len(), 1);
        assert!(
            got.refused[0].starts_with("sealed (") && got.refused[0].len() > "sealed ()".len(),
            "the path is named and so is the reason, got {:?}",
            got.refused[0]
        );
        assert!(
            sealed.join("inner/artifact.rlib").exists(),
            "and what could not be removed is still there, not half-reported gone"
        );
        assert_eq!(
            got.bytes, 4096,
            "only what actually went is counted as freed"
        );
        assert!(
            reclaim_note(&got).is_some_and(|n| n.contains("could not reclaim sealed")),
            "and the transcript carries it — a silent skip is the failure mode"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-465 AC-2: the delivered API URL wins; absent or blank falls back
    /// to this node's own configured server, byte-identical to before.
    #[test]
    fn the_runs_server_is_the_advertised_api_or_the_nodes_own() {
        assert_eq!(
            run_server(
                Some("https://api.example.test"),
                "http://control-plane:8080"
            ),
            "https://api.example.test"
        );
        assert_eq!(
            run_server(None, "http://control-plane:8080"),
            "http://control-plane:8080"
        );
        assert_eq!(
            run_server(Some("  "), "http://control-plane:8080"),
            "http://control-plane:8080"
        );
    }

    /// AC-2: every advertised kind either maps to a skill or is a RECORDED gap.
    ///
    /// The first version of this test could not fail. It asserted
    /// `skill_for(k).starts_with("nook-")` against a function whose fallback
    /// was `"nook-spec"` — so every input passed, including `epic-run` and
    /// `build`, which were silently resolving to the spec skill while both
    /// their skills existed on disk. A guard that reports green while the drift
    /// it names is present is worse than no guard: the next person reads the
    /// test name and stops looking.
    ///
    /// Now the mapping returns `Option`, and a kind must be in one list or the
    /// other. Adding a kind to `KNOWN_LOOP_KINDS` without either fails here.
    #[test]
    fn every_advertised_kind_is_mapped_or_recorded_as_unmapped() {
        assert_eq!(skill_for("spec"), Some("nook-spec"));
        assert_eq!(skill_for("decompose"), Some("nook-epic"));
        assert_eq!(skill_for("review"), Some("nook-review"));
        assert_eq!(skill_for("epic-run"), Some("nook-epic-runner"));
        assert_eq!(skill_for("build"), Some("nook-build"));
        assert_eq!(skill_for("investigate"), Some("nook-investigate"));

        for k in crate::capabilities::KNOWN_LOOP_KINDS {
            let mapped = skill_for(k).is_some();
            let recorded = UNMAPPED_KINDS.iter().any(|(n, _)| n == k);
            assert!(
                mapped ^ recorded,
                "{k} must be either mapped or recorded as unmapped, never both \
                 and never neither — mapped={mapped} recorded={recorded}"
            );
        }
    }

    /// The recorded gaps are real gaps: each names an owner, and none of them
    /// is a kind that actually has a mapping.
    #[test]
    fn the_unmapped_kinds_are_advertised_and_owned() {
        for (kind, owner) in UNMAPPED_KINDS {
            assert!(
                crate::capabilities::KNOWN_LOOP_KINDS.contains(kind),
                "{kind} is recorded as unmapped but is not advertised at all"
            );
            assert!(
                owner.contains("MAIN-"),
                "{kind}'s exemption must name the card that owns it, got {owner:?}"
            );
            assert!(skill_for(kind).is_none(), "{kind} is mapped after all");
        }
    }

    /// The warm layer's whole contract: the same PR always names the same
    /// agent session and the same working directory, run after run — that is
    /// what lets `--resume` find the earlier conversation. Confirmed broken the
    /// other way live: per-job paths meant two runs of one PR produced two
    /// unrelated sessions.
    #[test]
    fn a_pull_requests_session_and_worktree_are_stable_across_runs() {
        let a = review_session_id("ws-1", 348);
        let b = review_session_id("ws-1", 348);
        assert_eq!(a, b, "same PR, same session, whatever run asks");
        assert_ne!(
            a,
            review_session_id("ws-1", 349),
            "another PR is another reviewer"
        );
        assert_ne!(
            a,
            review_session_id("ws-2", 348),
            "another repo's #348 is unrelated"
        );
        assert_eq!(review_dirname("ws-1", 348), review_dirname("ws-1", 348));
        assert_ne!(review_dirname("ws-1", 348), review_dirname("ws-1", 349));
    }

    /// MAIN-460 AC-1/AC-2: the same card always names the same session and the
    /// same working directory — the warm layer's contract, `review`'s twin.
    #[test]
    fn a_cards_session_and_worktree_are_stable_across_runs() {
        let a = build_session_id("ws-1", "MAIN-42");
        assert_eq!(a, build_session_id("ws-1", "MAIN-42"));
        assert_ne!(
            a,
            build_session_id("ws-1", "MAIN-43"),
            "another card is another builder"
        );
        assert_ne!(
            a,
            build_session_id("ws-2", "MAIN-42"),
            "another repo's MAIN-42 is unrelated"
        );
        assert_eq!(
            build_dirname("ws-1", "MAIN-42"),
            build_dirname("ws-1", "MAIN-42")
        );
        assert_ne!(
            build_dirname("ws-1", "MAIN-42"),
            build_dirname("ws-1", "MAIN-43")
        );
    }

    /// One decision names both stable facts, and only for the kinds that have
    /// them: review by PR, build by card, everything else per-job (`None`).
    #[test]
    fn warm_identity_matches_kind_and_inputs() {
        // A review run warms its PR whatever the kind string says — the PR
        // number IS the signal, exactly as the dirname selection always read.
        let review = warm_identity("review", Some(7), Some("ws-1"), "");
        assert_eq!(
            review,
            Some((review_dirname("ws-1", 7), review_session_id("ws-1", 7)))
        );

        let build = warm_identity("build", None, Some("ws-1"), "MAIN-42");
        assert_eq!(
            build,
            Some((
                build_dirname("ws-1", "MAIN-42"),
                build_session_id("ws-1", "MAIN-42")
            ))
        );

        // Cold kinds and incomplete identities stay per-job.
        assert_eq!(warm_identity("spec", None, Some("ws-1"), "MAIN-42"), None);
        assert_eq!(warm_identity("decompose", None, Some("ws-1"), "E-1"), None);
        assert_eq!(warm_identity("build", None, Some("ws-1"), ""), None);
        assert_eq!(warm_identity("build", None, None, "MAIN-42"), None);
    }

    /// MAIN-455: every kind may build its cache, review included — the inverse
    /// of what this asserted under MAIN-406, whose bar existed for a sweep that
    /// could name any repo on a shared machine. Barring it did not make the
    /// operator safer; it made every review fail with "no clone cache",
    /// because clone-on-demand lands a working tree and a job reads a bare
    /// mirror.
    #[test]
    fn every_kind_may_build_its_clone_cache() {
        for kind in ["review", "spec", "decompose"] {
            assert!(may_create_cache(kind), "{kind} must be able to cache");
        }
    }

    /// MAIN-331 AC-3: a read-only run holds no forge credential, and the fleet
    /// fallback does not put one back.
    ///
    /// The fallback is the half worth asserting. The control plane sends `None`
    /// for this kind, which on its own means nothing here — every kind arrives
    /// with `None` when its workspace configured no identity, and the next line
    /// used to reach for `NOOK_GH_TOKEN`. So the machine's own token is set for
    /// the duration of the check, which is the state a real operator node is in.
    #[test]
    fn an_investigate_run_is_handed_no_forge_credential() {
        // Serialized against the other env-reading tests in this module by
        // being the only one that writes this variable; `fleet_gh_token`'s own
        // rules are asserted in `config`.
        std::env::set_var("NOOK_GH_TOKEN", "ghp_fleet");
        assert_eq!(
            forge_token("investigate", None),
            None,
            "the fleet token must not reinstate what the control plane withheld"
        );
        assert_eq!(
            forge_token("investigate", Some("ghp_workspace".into())),
            None,
            "nor may one delivered by mistake be exported"
        );
        assert_eq!(
            forge_token("build", None).as_deref(),
            Some("ghp_fleet"),
            "every other kind keeps the fallback"
        );
        assert_eq!(
            forge_token("review", Some("ghp_workspace".into())).as_deref(),
            Some("ghp_workspace"),
            "and the workspace's own identity still outranks the fleet's"
        );
        std::env::remove_var("NOOK_GH_TOKEN");
    }

    /// AC-4 / MAIN-143 AC-5: what counts as a credential.
    ///
    /// The predicate moved to `config::fleet_gh_token` when MAIN-407 gave the
    /// fleet its own variable, so the emptiness rule is asserted there, beside
    /// the search order it now has to survive. What is checked HERE is the only
    /// part this module still owns: that the preflight consults that one
    /// accessor rather than reading the environment itself, which is what keeps
    /// it from disagreeing with the token a session is handed.
    #[test]
    fn the_preflight_reads_the_shared_accessor() {
        let src = include_str!("loop_job.rs");
        let f = src
            .split("fn gh_is_authenticated")
            .nth(1)
            .expect("gh_is_authenticated exists");
        let body = &f[..f.find("\n}\n").expect("its body ends")];
        assert!(
            body.contains("config::fleet_gh_token"),
            "the preflight must go through the shared accessor, not its own env read"
        );
        assert!(
            !body.contains("std::env::var"),
            "a second env lookup here is how the preflight and the session export drift"
        );
    }

    /// MAIN-468 AC-1/AC-3: the preflight validates the credential the RUN will
    /// use. The prod failure: a valid workspace token in the job, a revoked
    /// login in the node's gh, every review run refused at preflight for hours.
    /// The probe seam stands in for `gh auth status`, authorizing ONLY the
    /// vault token exactly as GitHub would have.
    #[test]
    fn a_delivered_token_is_what_gets_validated_not_the_nodes_login() {
        let probed = std::cell::RefCell::new(Vec::new());
        let ok = gh_preflight(Some("vault-token"), Some("revoked-fleet".into()), |t| {
            probed.borrow_mut().push(t.map(str::to_string));
            if t == Some("vault-token") {
                GhProbe::Authorized
            } else {
                GhProbe::Refused
            }
        });
        assert!(
            ok.is_ok(),
            "a valid carried token must pass whatever the node holds: {ok:?}"
        );
        assert_eq!(
            *probed.borrow(),
            vec![Some("vault-token".to_string())],
            "exactly the carried token is probed, in the check's env"
        );
    }

    /// AC-2: a carried token GitHub refuses is ITS OWN failure — no falling
    /// back to the node's reach, because the run would use the carried token.
    #[test]
    fn a_rejected_workspace_token_fails_without_falling_back() {
        let err = gh_preflight(Some("revoked"), Some("fleet".into()), |_| GhProbe::Refused)
            .expect_err("a rejected carried token is a refusal");
        assert!(err.contains("workspace token was rejected"), "{err}");
    }

    /// AC-2/AC-3: no carried token, no fleet env, gh logged out — still
    /// refused, with the existing phrase kept and the missing job token named.
    #[test]
    fn no_token_on_an_unauthenticated_node_keeps_the_existing_refusal() {
        let err = gh_preflight(None, None, |t| {
            assert_eq!(t, None, "no token means nothing in the probe's env");
            GhProbe::Refused
        })
        .expect_err("an unauthenticated node with no job token is refused");
        assert!(
            err.contains("gh is installed but not authenticated"),
            "{err}"
        );
        assert!(
            err.contains("carries none"),
            "names the missing side: {err}"
        );

        let missing = gh_preflight(None, None, |_| GhProbe::NoGh).expect_err("no gh at all");
        assert_eq!(missing, "no GitHub token (NOOK_GH_TOKEN) and no gh on PATH");
    }

    /// A blank carried token is "carries none", and the fleet env then answers
    /// without any probe — the short-circuit the node relied on before.
    #[test]
    fn blank_tokens_fall_back_and_the_fleet_env_short_circuits() {
        let ok = gh_preflight(Some("  "), Some("fleet".into()), |_| {
            panic!("the fleet env answers before any probe")
        });
        assert!(ok.is_ok());
        let ok = gh_preflight(None, Some("fleet".into()), |_| panic!("no probe needed"));
        assert!(ok.is_ok());
    }

    /// AC-3: a missing cache is a NAMED failure, not a hang and not a clone.
    #[test]
    fn a_missing_cache_names_the_path_and_the_repo_instead_of_cloning() {
        let empty = std::env::temp_dir().join(format!("nook-406-{}", std::process::id()));
        let err = existing_mirror_in(
            &empty,
            "git@example.test:acme/api.git",
            None,
            &empty.join("worktrees").join("review-x-pr1"),
            &mut Vec::new(),
        )
        .expect_err("an absent mirror must not be created");
        assert!(err.contains("no clone cache"), "{err}");
        assert!(err.contains("acme/api"), "names the repo: {err}");
        assert!(
            !empty.exists(),
            "it must not have created anything at {}",
            empty.display()
        );
    }

    #[test]
    fn exit_status_decides_success_honestly() {
        // Only a recorded zero is success…
        assert!(exit_is_ok(Some(0)));
        // …a non-zero code is a failure…
        assert!(!exit_is_ok(Some(1)));
        assert!(!exit_is_ok(Some(137)));
        // …and an ABSENT status is abnormal death (the shell always writes $? on
        // a normal end), so it is a failure too — not a false completion.
        assert!(!exit_is_ok(None));
    }

    #[test]
    fn read_exit_status_parses_and_consumes_the_sentinel() {
        let dir = std::env::temp_dir().join(format!("nook-exit-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s");
        std::fs::write(&f, "3\n").unwrap();
        assert_eq!(read_exit_status(&f), Some(3));
        // Consumed: a second read finds nothing.
        assert_eq!(read_exit_status(&f), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// AC-5 worktree lifecycle, network-free: mirror-clone a local repo, add two
    /// worktrees off the one cache for two distinct job ids (they must be
    /// distinct, both present, non-interfering), then remove both and confirm
    /// they are gone.
    #[test]
    fn two_jobs_get_independent_worktrees_off_one_cache() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-loopjob-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&tmp).unwrap();

        // A local repo we treat as the remote — no network involved.
        let remote = tmp.join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        let git = |args: &[&str]| {
            let out = hermetic_git()
                .args(args)
                .current_dir(&remote)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        std::fs::write(remote.join("README.md"), "# demo\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "init",
        ]);

        let base = tmp.join("cache");
        let wt_base = tmp.join("worktrees");
        let repo_url = remote.to_string_lossy().to_string();

        // A local path needs no key — the `None` arm this fix deliberately kept.
        let cache = ensure_mirror_in(
            &base,
            &repo_url,
            None,
            &wt_base.join("none"),
            &mut Vec::new(),
        )
        .expect("mirror clone");
        assert!(cache.join("HEAD").exists(), "mirror has a HEAD");

        let w1 =
            add_job_worktree_in(&wt_base, &cache, "main", "job-aaa", false).expect("worktree 1");
        let w2 =
            add_job_worktree_in(&wt_base, &cache, "main", "job-bbb", false).expect("worktree 2");

        assert_ne!(w1, w2, "distinct job ids get distinct dirs");
        assert!(
            w1.is_dir() && w1.join("README.md").exists(),
            "wt1 checked out"
        );
        assert!(
            w2.is_dir() && w2.join("README.md").exists(),
            "wt2 checked out"
        );

        remove_job_worktree(&cache, &w1).expect("remove 1");
        remove_job_worktree(&cache, &w2).expect("remove 2");
        assert!(!w1.exists(), "wt1 gone");
        assert!(!w2.exists(), "wt2 gone");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `git` for the FIXTURES, pinned to read NO ambient config.
    ///
    /// `cargo test` runs these in parallel, and a sibling test
    /// (`a_global_gitignore_cannot_carve_entries_out_of_the_seed`) repoints
    /// `GIT_CONFIG_GLOBAL` process-wide for the duration of its own run. A
    /// fixture `git add` that happened to execute in that window read a config
    /// path meant for another test — often mid-create or mid-delete — and died
    /// with `fatal: unknown error occurred while reading the configuration
    /// files`. It looked engine-specific only because it surfaced on the SQLite
    /// leg; it is pure test-process races (MAIN-6xx). Clearing both scopes here
    /// makes every fixture git independent of whatever a concurrent test set.
    fn hermetic_git() -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = hermetic_git().args(args).current_dir(dir).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A commit-bearing repo at `dir`, standing in for the remote. No network.
    fn scratch_remote(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
        commit_all(dir, "init");
    }

    fn commit_all(dir: &Path, msg: &str) {
        git_in(dir, &["add", "."]);
        git_in(
            dir,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                msg,
            ],
        );
    }

    /// The refusal is recognised by OUR quoted path, whatever git version
    /// produced it — this pins the matching itself, since the live-repro test
    /// below skips on a git too old to refuse.
    #[test]
    fn the_refusal_predicate_matches_only_our_own_worktree() {
        let own = Path::new("/x/worktrees/review-ws-pr1");
        let msg = |p: &str| {
            format!("fatal: refusing to fetch into branch 'refs/heads/main' checked out at '{p}'")
        };
        assert!(fetch_refused_by(&msg("/x/worktrees/review-ws-pr1"), own));
        assert!(
            !fetch_refused_by(&msg("/x/worktrees/review-ws-pr10"), own),
            "a sibling PR's worktree is not ours, even when ours is its prefix"
        );
        assert!(
            !fetch_refused_by("Permission denied (publickey)", own),
            "an unrelated fetch failure must not trigger removal"
        );
    }

    /// MAIN-466 AC-1: a review worktree holds NO ref. Attached, it pins the
    /// workspace branch in the mirror from a stable path that outlives the run,
    /// and every fetch after the branch moves is refused — the prod wedge.
    #[test]
    fn a_review_worktree_is_detached() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-466-det-{}", uuid::Uuid::now_v7().simple()));
        let remote = tmp.join("remote");
        scratch_remote(&remote);
        let wt_base = tmp.join("worktrees");
        let cache = ensure_mirror_in(
            &tmp.join("cache"),
            &remote.to_string_lossy(),
            None,
            &wt_base.join("none"),
            &mut Vec::new(),
        )
        .expect("mirror clone");

        let wt = add_job_worktree_in(&wt_base, &cache, "main", &review_dirname("ws-1", 7), true)
            .expect("review worktree");
        assert_eq!(
            git_in(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD",
            "a review worktree must not pin a branch"
        );
        assert!(
            wt.join("README.md").exists(),
            "detached is still a real checkout of the branch tip"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// MAIN-466 AC-2: a wedged node heals on its next run. A pre-fix worktree
    /// has the branch checked out by name; the branch moves on the remote; the
    /// mirror fetch is refused. The run recognises its OWN worktree in the
    /// refusal, removes it, and the retried fetch lands the new tip — while a
    /// refusal naming someone ELSE's worktree removes nothing.
    #[test]
    fn a_wedged_fetch_removes_its_own_worktree_and_retries() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-466-fix-{}", uuid::Uuid::now_v7().simple()));
        let remote = tmp.join("remote");
        scratch_remote(&remote);
        let wt_base = tmp.join("worktrees");
        let own = wt_base.join(review_dirname("ws-1", 9));
        let cache = ensure_mirror_in(
            &tmp.join("cache"),
            &remote.to_string_lossy(),
            None,
            &own,
            &mut Vec::new(),
        )
        .expect("mirror clone");

        // The pre-detach shape: the branch checked out BY NAME at the stable path.
        let wedge =
            add_job_worktree_in(&wt_base, &cache, "main", &review_dirname("ws-1", 9), false)
                .expect("attached worktree");
        assert_eq!(wedge, own);

        // main moves on the remote — the moment prod wedged.
        std::fs::write(remote.join("more.txt"), "merged\n").unwrap();
        commit_all(&remote, "a merge lands");

        let refused = match fetch_mirror(&cache, None, &wt_base.join("not-ours")) {
            // git < 2.35 fast-forwards straight through a linked worktree — the
            // wedge cannot exist there, so there is nothing to heal. The node
            // images (bookworm) carry 2.39+, which refuses; that is where this
            // path is exercised.
            Ok(()) => {
                eprintln!("skipping: this git does not protect linked worktrees");
                let _ = std::fs::remove_dir_all(&tmp);
                return;
            }
            Err(e) => e,
        };
        assert!(refused.contains("refusing to fetch into"), "{refused}");
        assert!(own.exists(), "another run's worktree is not ours to remove");

        fetch_mirror(&cache, None, &own).expect("the owning run's fetch recovers");
        assert!(!own.exists(), "the wedging worktree is gone");
        assert_eq!(
            git_in(&cache, &["rev-parse", "refs/heads/main"]),
            git_in(&remote, &["rev-parse", "refs/heads/main"]),
            "the mirror caught up to the moved branch"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── MAIN-629: the mirror follows the workspace's remote ─────────────────

    /// A workspace whose remote MOVED, network-free: two local repos of the
    /// same `acme/api`, so both URLs slug to the one mirror directory exactly
    /// as an HTTPS and an SSH spelling of one GitHub repo do. `old` carries a
    /// `legacy` branch `new` has never heard of — the object that proves, later,
    /// that the mirror was kept rather than re-cloned.
    fn two_remotes_of_one_repo(tmp: &Path) -> (PathBuf, PathBuf, String) {
        let old = tmp.join("old").join("acme").join("api");
        scratch_remote(&old);
        git_in(&old, &["checkout", "-q", "-b", "legacy"]);
        std::fs::write(old.join("legacy.txt"), "only ever on the old remote\n").unwrap();
        commit_all(&old, "a branch the new remote never had");
        let legacy = git_in(&old, &["rev-parse", "HEAD"]);
        git_in(&old, &["checkout", "-q", "main"]);

        let new = tmp.join("new").join("acme").join("api");
        git_in(
            tmp,
            &[
                "clone",
                "-q",
                "--single-branch",
                "--branch",
                "main",
                &old.to_string_lossy(),
                &new.to_string_lossy(),
            ],
        );
        std::fs::write(new.join("moved.txt"), "landed after the move\n").unwrap();
        commit_all(&new, "work that only the new remote has");
        (old, new, legacy)
    }

    /// AC-1 and AC-2: the run repoints the mirror at the URL the job carries and
    /// fetches through it — without re-cloning, so the objects it already held
    /// are still there.
    #[test]
    fn a_stale_mirror_is_repointed_at_the_url_the_job_carries() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-629-move-{}", uuid::Uuid::now_v7().simple()));
        let (old, new, legacy) = two_remotes_of_one_repo(&tmp);
        let base = tmp.join("cache");
        let own = tmp
            .join("worktrees")
            .join(build_dirname("ws-1", "MAIN-629"));

        let cache = ensure_mirror_in(&base, &old.to_string_lossy(), None, &own, &mut Vec::new())
            .expect("mirror");
        git_in(&cache, &["cat-file", "-e", &legacy]);
        // A re-clone would build a new directory; this file could not survive it.
        std::fs::write(cache.join("nook-same-mirror"), "").unwrap();

        let again = ensure_mirror_in(&base, &new.to_string_lossy(), None, &own, &mut Vec::new())
            .expect("the moved remote must fetch, not die on the old URL");

        assert_eq!(again, cache, "the same mirror serves both spellings");
        assert_eq!(
            git_in(&cache, &["remote", "get-url", "origin"]),
            new.to_string_lossy(),
            "AC-1: origin follows the job's URL"
        );
        assert!(
            cache.join("nook-same-mirror").exists(),
            "AC-2: the mirror was adopted, not re-cloned"
        );
        git_in(&cache, &["cat-file", "-e", &legacy]);
        assert_eq!(
            git_in(&cache, &["rev-parse", "refs/heads/main"]),
            git_in(&new, &["rev-parse", "refs/heads/main"]),
            "and the fetch went through the new remote"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-1 in the shape prod actually wore: a mirror pinned to HTTPS while the
    /// workspace moved to SSH for a deploy key. Reconciliation only, because the
    /// fetch that follows is the part that needs a network and a credential.
    #[test]
    fn an_https_mirror_follows_the_workspace_to_ssh() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-629-ssh-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "https://github.com/acme/api.git");
        reconcile_mirror_remote(&cache, "git@github.com:acme/api.git", &mut Vec::new())
            .expect("reconcile");
        assert_eq!(
            git_in(&cache, &["remote", "get-url", "origin"]),
            "git@github.com:acme/api.git"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A bare repo standing in for a mirror pinned to `url`.
    fn mirror_with_origin(tmp: &Path, url: &str) -> PathBuf {
        let cache = tmp.join("acme__api.git");
        std::fs::create_dir_all(&cache).unwrap();
        git_in(&cache, &["init", "-q", "--bare"]);
        git_in(&cache, &["remote", "add", "origin", url]);
        cache
    }

    /// The rule `gh` leaves behind when it authenticates over HTTPS in a repo.
    fn add_rewrite(cache: &Path, base: &str, prefix: &str) {
        git_in(cache, &["config", &format!("url.{base}.insteadOf"), prefix]);
    }

    /// Every `url.*.insteadOf` the mirror's own config still carries, as
    /// `base -> prefix`. Read hermetically, so the developer's global config
    /// cannot put a rule in an assertion about the mirror's.
    fn rules_left(cache: &Path) -> Vec<String> {
        let out = hermetic_git()
            .args(["config", "--local", "--get-regexp", r"^url\..*\.insteadof$"])
            .current_dir(cache)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// MAIN-646 AC-1: the comparison reads the STORED remote, never the one
    /// `git remote get-url` reports.
    ///
    /// The two spellings here are one remote, so nothing should be written.
    /// Under the resolved read they were not: the rule reported HTTPS, the
    /// mirror looked moved, and `set-url` replaced the stored words with the
    /// job's — a spurious repoint that then read back as HTTPS again, which is
    /// how this was misdiagnosed three times as an agent clobbering the config.
    #[test]
    fn a_rewritten_remote_is_not_mistaken_for_a_moved_one() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-646-raw-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "git@github.com:acme/api");
        add_rewrite(&cache, "https://github.com/", "git@github.com:");
        assert_eq!(
            git_in(&cache, &["remote", "get-url", "origin"]),
            "https://github.com/acme/api",
            "the fixture must actually reproduce the rewrite"
        );

        reconcile_mirror_remote(&cache, "git@github.com:acme/api.git", &mut Vec::new())
            .expect("reconcile");

        assert_eq!(
            git_in(&cache, &["config", "--get", "remote.origin.url"]),
            "git@github.com:acme/api",
            "the stored remote already named this repo — a repoint would have replaced its words"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-2 and AC-4: the rule `gh` writes is removed before the fetch, and the
    /// run says so rather than silently mutating a config a person may have set.
    #[test]
    fn a_rewrite_rule_that_redirects_the_remote_is_removed_and_reported() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-646-drop-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "git@github.com:acme/api.git");
        add_rewrite(&cache, "https://github.com/", "git@github.com:");

        let mut notes = Vec::new();
        reconcile_mirror_remote(&cache, "git@github.com:acme/api.git", &mut notes)
            .expect("reconcile");

        assert!(
            rules_left(&cache).is_empty(),
            "AC-2: the redirecting rule must be gone: {:?}",
            rules_left(&cache)
        );
        assert_eq!(
            git_in(&cache, &["remote", "get-url", "origin"]),
            "git@github.com:acme/api.git",
            "AC-2: and the effective URL is the ssh form the workspace chose"
        );
        let said = notes.join("\n");
        assert!(
            said.contains("url.https://github.com/.insteadof")
                && said.contains("git@github.com:")
                && said.contains("https://github.com/acme/api.git"),
            "AC-4: the transcript must name the rule and where it was sending the fetch: {said}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-3: only a rule that actually redirects THIS mirror's remote is
    /// touched. An operator's corporate mirror, a rule for another host, and a
    /// rule that merely respells the same remote all survive.
    #[test]
    fn only_a_rule_redirecting_this_remote_is_removed() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        struct Case {
            what: &'static str,
            stored: &'static str,
            /// `(base, prefix)` — the two halves of `url.<base>.insteadOf`.
            rules: &'static [(&'static str, &'static str)],
            survivors: usize,
        }
        let cases = [
            Case {
                what: "gh's rule against the ssh remote it redirects",
                stored: "git@github.com:acme/api.git",
                rules: &[("https://github.com/", "git@github.com:")],
                survivors: 0,
            },
            Case {
                what: "a corporate mirror for another host",
                stored: "git@github.com:acme/api.git",
                rules: &[("git@git.corp.example:", "https://git.corp.example/")],
                survivors: 1,
            },
            Case {
                what: "the same rewrite aimed at a repo this mirror is not",
                stored: "git@github.com:acme/api.git",
                rules: &[("https://github.com/", "git@gitlab.com:")],
                survivors: 1,
            },
            Case {
                what: "a respelling of the same remote is not a redirect",
                stored: "git@github.com:acme/api.git",
                rules: &[("ssh://git@github.com/", "git@github.com:")],
                survivors: 1,
            },
            Case {
                what: "the corporate mirror survives alongside gh's",
                stored: "git@github.com:acme/api.git",
                rules: &[
                    ("https://github.com/", "git@github.com:"),
                    ("git@git.corp.example:", "https://git.corp.example/"),
                ],
                survivors: 1,
            },
            Case {
                // git applies the longest matching prefix and no other, so the
                // shorter redirecting rule is not in force and is not ours to
                // delete.
                what: "a longer harmless prefix shadows a shorter redirecting one",
                stored: "git@github.com:acme/api.git",
                rules: &[
                    ("https://github.com/", "git@github.com:"),
                    ("ssh://git@github.com/acme/", "git@github.com:acme/"),
                ],
                survivors: 2,
            },
            Case {
                // Equal-length prefixes: git keeps the one it saw FIRST, so
                // the redirecting rule is the one in force and the one to go.
                // Reading the tie the other way leaves it in place — the mirror
                // still fetches over HTTPS, and MAIN-646 survives its own fix.
                what: "an equal-length tie is broken the way git breaks it",
                stored: "git@github.com:acme/api.git",
                rules: &[
                    ("https://github.com/", "git@github.com:"),
                    ("ssh://git@github.com/", "git@github.com:"),
                ],
                survivors: 1,
            },
            Case {
                // The same tie, file order reversed: now the harmless rule is
                // the one git applies, and the redirecting one it never reaches
                // is not ours to delete.
                what: "and the same tie in the other file order touches nothing",
                stored: "git@github.com:acme/api.git",
                rules: &[
                    ("ssh://git@github.com/", "git@github.com:"),
                    ("https://github.com/", "git@github.com:"),
                ],
                survivors: 2,
            },
        ];
        let tmp =
            std::env::temp_dir().join(format!("nook-646-only-{}", uuid::Uuid::now_v7().simple()));
        for (i, c) in cases.iter().enumerate() {
            let cache = mirror_with_origin(&tmp.join(i.to_string()), c.stored);
            for (base, prefix) in c.rules {
                add_rewrite(&cache, base, prefix);
            }
            reconcile_mirror_remote(&cache, c.stored, &mut Vec::new()).expect("reconcile");
            assert_eq!(
                rules_left(&cache).len(),
                c.survivors,
                "{}: {:?}",
                c.what,
                rules_left(&cache)
            );
            // The invariant every case shares, and the one the job depends on:
            // whatever git resolves afterwards has to be the remote the
            // workspace chose. Asserted through git itself rather than through
            // this module's model of it, so a wrong model fails here.
            assert_eq!(
                normalized_remote(&git_in(&cache, &["remote", "get-url", "origin"])),
                normalized_remote(c.stored),
                "{}: git must still resolve origin to the workspace's remote",
                c.what
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-5: `gh` writes the rule again the next time it authenticates, so
    /// reconciliation runs before EVERY fetch. Two rounds with the rule
    /// reintroduced between them, each asserting the fetch reached the remote
    /// the workspace named — which it cannot have done through the rule, whose
    /// target does not exist.
    #[test]
    fn a_reintroduced_rewrite_rule_is_removed_again_before_the_next_fetch() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-646-again-{}", uuid::Uuid::now_v7().simple()));
        let remote = tmp.join("acme").join("api");
        scratch_remote(&remote);
        let url = remote.to_string_lossy().to_string();
        let base = tmp.join("cache");
        let own = base.join("worktrees").join("none");
        let cache = ensure_mirror_in(&base, &url, None, &own, &mut Vec::new()).expect("mirror");
        let nowhere = tmp.join("nowhere").to_string_lossy().to_string();

        for round in 0..2 {
            std::fs::write(remote.join(format!("round{round}.txt")), "moved on\n").unwrap();
            commit_all(&remote, &format!("round {round}"));

            add_rewrite(&cache, &nowhere, &url);
            assert_eq!(
                git_in(&cache, &["remote", "get-url", "origin"]),
                nowhere,
                "round {round}: the fixture must actually reproduce the rewrite"
            );

            let mut notes = Vec::new();
            ensure_mirror_in(&base, &url, None, &own, &mut notes)
                .unwrap_or_else(|e| panic!("round {round} must still fetch: {e}"));

            assert!(
                rules_left(&cache).is_empty(),
                "round {round}: the rule must be gone again"
            );
            assert_eq!(
                git_in(&cache, &["rev-parse", "refs/heads/main"]),
                git_in(&remote, &["rev-parse", "refs/heads/main"]),
                "round {round}: the fetch went to the remote the workspace named"
            );
            assert_eq!(notes.len(), 1, "round {round}: and said so once");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-6: MAIN-629's message is what made this findable, so it keeps both
    /// URLs — and now names the rule standing between them, which is the clause
    /// that stops "the two URLs agree" from being a dead end.
    #[test]
    fn a_credential_failure_names_the_rewrite_rule_in_force() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-646-said-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "git@github.com:acme/api.git");
        add_rewrite(&cache, "https://github.com/", "git@github.com:");

        // The one test here that cannot go through `hermetic_git`: AC-6's whole
        // point is naming a rule `--local` could NOT remove, so
        // `explain_fetch_failure` reads every scope on purpose. A developer
        // whose own global config rewrites this remote with a longer prefix
        // would see their rule named — a true message about a different config,
        // not a defect. Skipped rather than pinned, because pinning means
        // repointing `GIT_CONFIG_GLOBAL` process-wide, which is exactly the
        // cross-test race `hermetic_git` was written to end.
        let mut ambient = rewrite_rules(&cache, &["--global"]);
        ambient.extend(rewrite_rules(&cache, &["--system"]));
        if RewriteRule::effective(&ambient, "git@github.com:acme/api.git").is_some() {
            eprintln!("skipping: this machine's own git config rewrites git@github.com:");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }

        let said = explain_fetch_failure(
            &cache,
            "git@github.com:acme/api.git",
            "fatal: could not read Username for 'https://github.com': No such device or address"
                .to_string(),
        );

        assert!(
            said.contains("git@github.com:acme/api.git"),
            "the stored remote and the job's URL, which are the same here: {said}"
        );
        assert!(
            said.contains("url.https://github.com/.insteadof")
                && said.contains("https://github.com/acme/api.git"),
            "and the rule, with where it sends the fetch: {said}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A key holding the same value twice is cleared by ONE `--unset-all`, so
    /// the list it was read from has to drop both entries at once.
    ///
    /// Asking git again for a value already gone exits 5 with an empty stderr,
    /// which `run_git` turns into `Err("")` — a job failing with a bare
    /// `clone cache failed: ` on a mirror this very pass had just made correct,
    /// and a strike spent on a condition that no longer exists.
    #[test]
    fn a_duplicated_rule_value_is_removed_once_and_does_not_fail_the_job() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-646-dup-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "git@github.com:acme/api.git");
        // `--add`, not the plain set `add_rewrite` uses: a set REPLACES, which
        // is why this state needs asking for.
        for _ in 0..2 {
            git_in(
                &cache,
                &[
                    "config",
                    "--add",
                    "url.https://github.com/.insteadOf",
                    "git@github.com:",
                ],
            );
        }
        assert_eq!(rules_left(&cache).len(), 2, "the fixture must hold both");

        let mut notes = Vec::new();
        reconcile_mirror_remote(&cache, "git@github.com:acme/api.git", &mut notes)
            .expect("a duplicated value must not fail the job");

        assert!(rules_left(&cache).is_empty(), "both values are gone");
        assert_eq!(notes.len(), 1, "and one removal is one line: {notes:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-7: a public repo on HTTPS is untouched. `gh`'s rule rewrites the ssh
    /// spelling, which such a workspace does not use, so there is nothing to
    /// remove and nothing to repoint — the fetch keeps working exactly as it
    /// did, over whichever protocol it was already using.
    #[test]
    fn a_public_https_workspace_keeps_its_config() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-646-pub-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "https://github.com/acme/api.git");
        add_rewrite(&cache, "https://github.com/", "git@github.com:");

        let mut notes = Vec::new();
        reconcile_mirror_remote(&cache, "https://github.com/acme/api.git", &mut notes)
            .expect("reconcile");

        assert_eq!(
            rules_left(&cache).len(),
            1,
            "the rule does not touch this remote, so it is not ours to remove"
        );
        assert_eq!(
            git_in(&cache, &["config", "--get", "remote.origin.url"]),
            "https://github.com/acme/api.git",
            "and nothing was repointed"
        );
        assert!(notes.is_empty(), "nor anything to say: {notes:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A value is matched as a regex by `git config --unset-all`, and an
    /// unescaped `git@github.com:` is a pattern whose dots match any character
    /// — so the escaping is what keeps the removal to the rule it named.
    #[test]
    fn a_value_pattern_matches_only_its_own_literal() {
        assert_eq!(
            exact_value_pattern("git@github.com:"),
            r"^git@github\.com:$"
        );
        assert_eq!(
            exact_value_pattern("https://git.corp/a+b?/"),
            r"^https://git\.corp/a\+b\?/$"
        );
    }

    /// AC-3: a spelling change is not a move. Each pair would fetch from the
    /// same place, so the mirror's config keeps the words it already had —
    /// asserted on the stored URL, which a write would have replaced.
    #[test]
    fn an_equivalent_spelling_does_not_rewrite_the_mirror() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let equivalent = [
            (
                "https://github.com/acme/api.git",
                "https://github.com/acme/api",
            ),
            (
                "https://github.com/acme/api/",
                "https://github.com/acme/api.git",
            ),
            (
                "git@github.com:acme/api.git",
                "ssh://git@github.com/acme/api",
            ),
            (
                "https://GitHub.com/acme/api.git",
                "https://github.com/acme/api.git",
            ),
            ("git@github.com:acme/api", "git@github.com:acme/api.git"),
        ];
        let tmp =
            std::env::temp_dir().join(format!("nook-629-same-{}", uuid::Uuid::now_v7().simple()));
        for (i, (pinned, carried)) in equivalent.iter().enumerate() {
            let cache = mirror_with_origin(&tmp.join(i.to_string()), pinned);
            reconcile_mirror_remote(&cache, carried, &mut Vec::new()).expect("reconcile");
            assert_eq!(
                git_in(&cache, &["remote", "get-url", "origin"]),
                *pinned,
                "{pinned} and {carried} are one remote — nothing should have been written"
            );
        }

        // The boundary: a change of transport, of host or of repo IS a move.
        for (a, b) in [
            (
                "https://github.com/acme/api.git",
                "git@github.com:acme/api.git",
            ),
            ("git@github.com:acme/api.git", "git@gitlab.com:acme/api.git"),
            ("git@github.com:acme/api.git", "git@github.com:acme/web.git"),
            (
                "https://github.com/acme/api.git",
                "https://github.com/Acme/api.git",
            ),
        ] {
            assert_ne!(normalized_remote(a), normalized_remote(b), "{a} vs {b}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-4: git says only what it could not read. The run says which remote it
    /// was reading and which one the job expected.
    #[test]
    fn a_credential_failure_names_both_remotes() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-629-cred-{}", uuid::Uuid::now_v7().simple()));
        let cache = mirror_with_origin(&tmp, "https://github.com/acme/api.git");
        let git_said =
            "fatal: could not read Username for 'https://github.com': No such device or address";

        let explained =
            explain_fetch_failure(&cache, "git@github.com:acme/api.git", git_said.to_string());
        assert!(explained.contains(git_said), "git's own words survive");
        assert!(
            explained.contains("https://github.com/acme/api.git"),
            "names the mirror's remote: {explained}"
        );
        assert!(
            explained.contains("git@github.com:acme/api.git"),
            "and the one the job carries: {explained}"
        );

        let unrelated = "fatal: not a git repository".to_string();
        assert_eq!(
            explain_fetch_failure(&cache, "git@github.com:acme/api.git", unrelated.clone()),
            unrelated,
            "a failure that is not about credentials is passed through untouched"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AC-5: a review run never creates a mirror, but it adopts one — so it
    /// reconciles the remote on the same path every other kind does.
    #[test]
    fn a_review_run_reconciles_the_mirror_it_may_not_create() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let tmp =
            std::env::temp_dir().join(format!("nook-629-review-{}", uuid::Uuid::now_v7().simple()));
        let (old, new, _) = two_remotes_of_one_repo(&tmp);
        let base = tmp.join("cache");
        let own = tmp.join("worktrees").join(review_dirname("ws-1", 7));
        ensure_mirror_in(&base, &old.to_string_lossy(), None, &own, &mut Vec::new())
            .expect("mirror");

        let cache = existing_mirror_in(&base, &new.to_string_lossy(), None, &own, &mut Vec::new())
            .expect("the review run adopts the mirror at the moved URL");
        assert_eq!(
            git_in(&cache, &["remote", "get-url", "origin"]),
            new.to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod clone_cache_key_tests {

    /// The clone cache must USE the workspace's key, not the node's own.
    ///
    /// The prod failure: a loop job on a private repo died at "preparing
    /// workspace" with `Permission denied (publickey)` while the credential was
    /// correctly pinned. `workspace_id` gave the job's SESSION a working git via
    /// the shim (MAIN-367), but the bare mirror is built here — in the node
    /// process, before any session exists — and was passing `None`, which
    /// `run_git` resolves to the node's own generated key.
    ///
    /// Asserts on the mechanism rather than on a real private clone, which a
    /// test cannot have: given key material, `run_git` is handed a path, and
    /// `git_ssh_command` builds a `GIT_SSH_COMMAND` naming THAT file. If the key
    /// stopped reaching git, the `-i <path>` would go with it.
    #[test]
    fn supplied_key_material_becomes_the_ssh_identity() {
        let held = crate::gitops::TransientKey::write("-----BEGIN OPENSSH PRIVATE KEY-----\nx\n")
            .expect("transient key");
        let cmd = crate::ssh::git_ssh_command(Some(held.path.as_path()))
            .expect("a GIT_SSH_COMMAND for an explicit key");
        assert!(
            cmd.contains(&held.path.to_string_lossy().to_string()),
            "the supplied key is not the identity git would use: {cmd}"
        );
    }

    /// And the file does not outlive the command that needed it — the property
    /// that lets a shared operator node clone a private repo without becoming a
    /// place where private keys accumulate.
    #[test]
    fn the_transient_key_is_removed_on_drop() {
        let path = {
            let held = crate::gitops::TransientKey::write("material").expect("transient key");
            assert!(held.path.exists(), "the key was not written");
            held.path.clone()
        };
        assert!(!path.exists(), "the key outlived its guard");
    }
}
