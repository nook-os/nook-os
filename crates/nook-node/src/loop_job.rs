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

/// Ensure a fresh bare mirror of `repo_url` under `base`, using the node's own
/// SSH auth (`run_git` falls back to the node key when no explicit key is
/// passed). Returns the mirror path.
fn ensure_mirror_in(base: &Path, repo_url: &str) -> Result<PathBuf, String> {
    let cache = base.join(format!("{}.git", repo_slug(repo_url)));
    if cache.join("HEAD").exists() {
        crate::gitops::run_git(&["fetch", "--prune"], Some(&cache), None)?;
    } else {
        std::fs::create_dir_all(base)
            .map_err(|e| format!("cannot create {}: {e}", base.display()))?;
        crate::gitops::run_git(
            &["clone", "--mirror", repo_url, &cache.to_string_lossy()],
            None,
            None,
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

/// Run one loop job to completion. Blocking; call under `spawn_blocking`.
pub fn run(cfg: NodeConfig, out: Sender<NodeToControl>, job: LoopJob) {
    let LoopJob {
        job_id,
        kind,
        target_task_key,
        repo_url,
        branch,
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
    let cache = match ensure_mirror_in(&base, &repo_url) {
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
    let tmux_name = job_tmux_name(&job_id);
    note(
        &out,
        &job_id,
        format!(
            "launching claude in {} to run /{skill} {target_task_key}",
            worktree.display()
        ),
    );

    let (ok, message) = drive_session(
        &out,
        &job_id,
        &tmux_name,
        &worktree,
        skill,
        &target_task_key,
    );

    if let Err(e) = remove_job_worktree(&cache, &worktree) {
        note(&out, &job_id, format!("worktree cleanup: {e}"));
    }
    unregister(&dirname);
    finished(&out, &job_id, ok, message);
}

fn unregister(dirname: &str) {
    if let Ok(mut s) = running_jobs().lock() {
        s.remove(dirname);
    }
}

/// Launch the runtime, stream its PTY output verbatim, drive the skill, and wait
/// for the session to end (or time out). Returns `(ok, message)` for
/// `JobFinished`.
fn drive_session(
    out: &Sender<NodeToControl>,
    job_id: &str,
    tmux_name: &str,
    worktree: &Path,
    skill: &str,
    target: &str,
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
    std::thread::sleep(SKILL_STARTUP_DELAY);
    let line = format!("/{skill} {target}");
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
            Some(code) if code != 0 => format!("agent exited with status {code}"),
            _ => "loop session ended".to_string(),
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

/// A job succeeded unless the runtime wrote a non-zero exit status (AC-4). An
/// absent status is not proof of failure (the timeout and node-disconnect paths
/// cover the crashes that leave none), so it counts as a clean end.
fn exit_is_ok(status: Option<i32>) -> bool {
    matches!(status, None | Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_decides_success_honestly() {
        // A written non-zero code is a failure (AC-4)…
        assert!(!exit_is_ok(Some(1)));
        assert!(!exit_is_ok(Some(137)));
        // …zero is success, and an absent/unknowable status is not proof of
        // failure (killed / never written — covered by timeout & disconnect).
        assert!(exit_is_ok(Some(0)));
        assert!(exit_is_ok(None));
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

        let cache = ensure_mirror_in(&base, &repo_url).expect("mirror clone");
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
