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
}

/// Worktree directory names of jobs running on this node right now, so
/// `reconcile` can tell a live worktree apart from an orphan left by a crash.
fn running_jobs() -> &'static Mutex<HashSet<String>> {
    static JOBS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn note(out: &Sender<NodeToControl>, job_id: &str, content: impl Into<String>) {
    let _ = out.blocking_send(NodeToControl::JobTranscript {
        job_id: job_id.to_string(),
        source: "system".into(),
        content: content.into(),
    });
}

fn finished(out: &Sender<NodeToControl>, job_id: &str, ok: bool, message: impl Into<String>) {
    let _ = out.blocking_send(NodeToControl::JobFinished {
        job_id: job_id.to_string(),
        ok,
        message: message.into(),
    });
}

/// The node-local clone cache root, per control plane so two control planes on
/// one machine never share a mirror (mirrors MAIN-58's per-cp isolation).
fn cache_base(server: &str) -> PathBuf {
    PathBuf::from(crate::config::expand_path("~/.nook/clone-cache"))
        .join(crate::config::cp_slug(server))
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
fn ensure_mirror_in(base: &Path, repo_url: &str, ssh_key: Option<&str>) -> Result<PathBuf, String> {
    let cache = base.join(format!("{}.git", repo_slug(repo_url)));
    if cache.join("HEAD").exists() {
        // The fetch needs the key too: a mirror that cloned once still pulls
        // from the same private remote on every later job.
        crate::gitops::run_git_remote(&["fetch", "--prune"], Some(&cache), ssh_key)?;
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

/// Add a per-job worktree off `cache`, into `<wt_base>/<job>` so concurrent jobs
/// on the same workspace get distinct trees.
///
/// Three attempts, because two worktrees cannot check out the same branch: the
/// branch as-is (the lone-job case) → the branch tip detached (a second
/// concurrent job on the *same* branch, which git refuses to check out twice) →
/// creating the branch if it isn't present locally (mirroring
/// `gitops::add_worktree`). Either way the tree is based on `branch`.
fn add_job_worktree_in(
    wt_base: &Path,
    cache: &Path,
    branch: &str,
    job_id: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(wt_base)
        .map_err(|e| format!("cannot create {}: {e}", wt_base.display()))?;
    let dest = wt_base.join(sanitize(job_id));
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
    }
    let dest_str = dest.to_string_lossy().to_string();
    let attempts: [&[&str]; 3] = [
        &["worktree", "add", &dest_str, branch],
        &["worktree", "add", "--detach", &dest_str, branch],
        &["worktree", "add", "-b", branch, &dest_str],
    ];
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

/// Best-effort cleanup of orphaned job worktrees on (re)connect (AC-4/AC-6):
/// prune each known mirror's worktree admin, then delete any worktree dir whose
/// job is no longer running. Never fatal.
pub fn reconcile(cfg: &NodeConfig) {
    let base = cache_base(&cfg.server);
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
            if p.is_dir() && !running.contains(name) {
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

/// Run one loop job to completion. Blocking; call under `spawn_blocking`.
pub fn run(cfg: NodeConfig, out: Sender<NodeToControl>, job: LoopJob) {
    let LoopJob {
        job_id,
        kind,
        target_task_key,
        repo_url,
        branch,
        seed,
        workspace_id,
        ssh_key,
        nook_token,
    } = job;
    let dirname = job_dirname(&job_id);
    if let Ok(mut s) = running_jobs().lock() {
        s.insert(dirname.clone());
    }

    let base = cache_base(&cfg.server);
    let wt_base = base.join("worktrees");

    note(
        &out,
        &job_id,
        format!("preparing workspace from {repo_url} @ {branch}"),
    );
    let cache = match ensure_mirror_in(&base, &repo_url, ssh_key.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            finished(&out, &job_id, false, format!("clone cache failed: {e}"));
            unregister(&dirname);
            return;
        }
    };
    let worktree = match add_job_worktree_in(&wt_base, &cache, &branch, &job_id) {
        Ok(w) => w,
        Err(e) => {
            finished(&out, &job_id, false, format!("worktree setup failed: {e}"));
            unregister(&dirname);
            return;
        }
    };

    // `decompose` walks an epic into sub-tickets; everything else is a spec pass.
    let skill = if kind == "decompose" {
        "nook-epic"
    } else {
        "nook-spec"
    };
    // AC-5: a skill the agent has never heard of makes `/nook-spec` ordinary
    // prose. The agent reads it, does nothing in particular, and the job
    // "succeeds" having produced no ticket — the exact silent no-op this card
    // exists to remove. Refuse before launching anything.
    if !crate::wizard::skills::is_installed(skill) {
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
        unregister(&dirname);
        return;
    }

    let tmux_name = job_tmux_name(&job_id);
    note(
        &out,
        &job_id,
        format!(
            "launching claude in {} to run /{skill} {target_task_key}",
            worktree.display()
        ),
    );

    // Which execution strategy this runtime gets (MAIN-240). Claude speaks
    // stream-json, so it runs headless and the transcript comes from real
    // events; anything else keeps the tmux/PTY path untouched (NG-1).
    let (ok, message) = match crate::job_adapter::adapter_for(RUNTIME) {
        crate::job_adapter::Adapter::Streaming => drive_streaming(
            &out,
            &job_id,
            &worktree,
            skill,
            &target_task_key,
            seed.as_deref(),
            AgentIdentity {
                token: nook_token.as_deref(),
                server: &cfg.server,
                workspace_id: workspace_id.as_deref(),
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
            workspace_id.as_deref(),
        ),
    };

    if let Err(e) = remove_job_worktree(&cache, &worktree) {
        note(&out, &job_id, format!("worktree cleanup: {e}"));
    }
    unregister(&dirname);
    finished(&out, &job_id, ok, message);
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
}

fn drive_streaming(
    out: &Sender<NodeToControl>,
    job_id: &str,
    worktree: &Path,
    skill: &str,
    target: &str,
    seed: Option<&str>,
    identity: AgentIdentity<'_>,
) -> (bool, String) {
    use crate::job_adapter::{self, Event, StreamingSession, TurnState};

    let args = job_adapter::claude_stream_args(job_id);
    let mut env: Vec<(&str, &str)> = vec![
        ("NOOK_JOB_ID", job_id),
        // The agent runs headless with `--dangerously-skip-permissions`
        // (job_adapter), which Claude Code refuses under root "for security
        // reasons" — and the node runs as root. But a per-job worktree on a
        // confined node genuinely IS a sandbox, and `IS_SANDBOX=1` is exactly
        // how Claude Code sanctions the flag there. Without it the agent exits 1
        // on launch and the run fails before it does anything.
        ("IS_SANDBOX", "1"),
    ];
    if let Some(s) = seed.filter(|s| !s.trim().is_empty()) {
        env.push(("NOOK_JOB_SEED", s));
    }
    // The agent's own identity, in the JOB's tenant. `AuthConfig::load` reads a
    // FILE, so without this `nook` inside the agent acts as whoever last ran
    // `nook login` on this machine — on a shared operator node, one human in one
    // tenant, which is how a job for another tenant's workspace listed the wrong
    // boards and drafted against the wrong one.
    if let Some(t) = identity.token.filter(|t| !t.trim().is_empty()) {
        env.push(("NOOK_TOKEN", t));
        env.push(("NOOK_SERVER", identity.server));
    }
    // The streaming adapter spawns the agent directly and never touches tmux,
    // so it never inherited what `tmux.rs` exports. `nook get workspace git-ssh`
    // needs this to name the repo it is authenticating for; without it, git
    // inside the agent silently falls back to the node's own key.
    if let Some(w) = identity.workspace_id.filter(|w| !w.trim().is_empty()) {
        env.push(("NOOK_WORKSPACE_ID", w));
    }

    let mut session = match StreamingSession::spawn(RUNTIME, &args, worktree, &env) {
        Ok(s) => s,
        Err(e) => return (false, e),
    };
    register_stream(job_id, &session);

    let Some(stdout) = session.take_stdout() else {
        session.kill();
        unregister_stream(job_id);
        return (false, "the agent produced no stdout".into());
    };

    // The opening turn is the skill command — the same line the tmux path typed,
    // now sent as structured input. A multi-line seed survives intact here,
    // which the typed path could not manage.
    let mut opening = format!("/{skill} {target}");
    if let Some(s) = seed.filter(|s| !s.trim().is_empty()) {
        opening.push(' ');
        opening.push_str(s);
    }
    if let Err(e) = session.send(&opening) {
        session.kill();
        unregister_stream(job_id);
        return (false, format!("could not send the skill command: {e}"));
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
        Event::Ignored => {}
    });

    // A stream that died mid-turn would otherwise leave the UI showing
    // "working" forever.
    if turn.active() {
        report_turn(out, job_id, false);
    }

    let code = session.wait();
    unregister_stream(job_id);

    match outcome {
        Some((ok, message)) => (ok, message),
        // The stream ended without a result record: fall back to the exit code
        // and the tail, the same crash-honesty rule the tmux path uses (AC-4 of
        // MAIN-161) rather than reporting a success nobody observed.
        None => {
            let tail = session.tail_text();
            let reason = match code {
                Some(0) => "the agent exited without a result record".to_string(),
                Some(c) => format!("the agent exited with status {c}"),
                None => "the agent died without an exit status".to_string(),
            };
            (
                false,
                if tail.is_empty() {
                    reason
                } else {
                    format!("{reason}\n{tail}")
                },
            )
        }
    }
}

/// Tell the control plane whether a turn is in flight (AC-2). A real signal
/// off real events — the thing screen-scraping could only guess at.
fn report_turn(out: &Sender<NodeToControl>, job_id: &str, active: bool) {
    let _ = out.blocking_send(NodeToControl::JobTurn {
        job_id: job_id.to_string(),
        active,
    });
}

fn unregister(dirname: &str) {
    if let Ok(mut s) = running_jobs().lock() {
        s.remove(dirname);
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
    workspace_id: Option<&str>,
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
    let mut line = format!("/{skill} {target}");
    if let Some(seed) = seed.map(one_line).filter(|s| !s.is_empty()) {
        line.push(' ');
        line.push_str(&seed);
    }
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
            let out = std::process::Command::new("git")
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
        let cache = ensure_mirror_in(&base, &repo_url, None).expect("mirror clone");
        assert!(cache.join("HEAD").exists(), "mirror has a HEAD");

        let w1 = add_job_worktree_in(&wt_base, &cache, "main", "job-aaa").expect("worktree 1");
        let w2 = add_job_worktree_in(&wt_base, &cache, "main", "job-bbb").expect("worktree 2");

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
