//! Long-running git operations: clone and worktree-add. Blocking; run these
//! under `spawn_blocking`. Results feed the generic `OpResult` protocol
//! message.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct OpOutcome {
    pub ok: bool,
    pub path: Option<String>,
    pub message: String,
}

fn fail(message: impl Into<String>) -> OpOutcome {
    OpOutcome {
        ok: false,
        path: None,
        message: message.into(),
    }
}

/// Run a git command that reaches a REMOTE, authenticated with `ssh_key_material`.
///
/// **The one place key material becomes an authenticated git command.** Three
/// call sites used to hand-roll the same two lines — write the material to a
/// transient file, take its path, pass it down — and a fourth, the loop job's
/// bare-mirror clone, forgot them entirely. That fourth is why a loop job on a
/// private repo died at "preparing workspace" with `Permission denied
/// (publickey)` while the credential was pinned correctly and the same repo had
/// already cloned onto the same machine.
///
/// Collapsing them here is the point: a new remote command reaches for THIS and
/// gets the key handling by construction, instead of remembering to repeat it.
///
/// `None` means this machine's own reach, which is right for a public repo or a
/// local path — [`crate::ssh::git_ssh_command`] then resolves the key chosen at
/// `nook setup`, else the node's generated key. Callers do not re-implement that
/// fallback; `push_current` used to and got it subtly wrong, handing git a
/// configured path without checking it existed.
pub(crate) fn run_git_remote(
    args: &[&str],
    cwd: Option<&Path>,
    ssh_key_material: Option<&str>,
) -> Result<String, String> {
    // Bound rather than inlined: `TransientKey`'s Drop removes the file, so the
    // guard has to outlive the git command — including on its error paths.
    let held = ssh_key_material.and_then(TransientKey::write);
    run_git(args, cwd, held.as_ref().map(|k| k.path.as_path()))
}

/// Run a git command. Prefer [`run_git_remote`] for anything touching a remote —
/// it owns the key handling, so there is no key argument here to forget.
pub(crate) fn run_git(
    args: &[&str],
    cwd: Option<&Path>,
    ssh_key: Option<&Path>,
) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(ssh) = crate::ssh::git_ssh_command(ssh_key) {
        cmd.env("GIT_SSH_COMMAND", ssh);
    }
    match cmd.output() {
        Err(e) => Err(format!("git failed to start: {e}")),
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('.')
        && !s.contains('/')
        && !s.contains('\\')
}

/// Derive the qualified checkout path "owner/repo" from a remote URL:
/// `git@github.com:acme/services.git` → `acme/services`.
///
/// Repos are cloned into `<root>/<owner>/<repo>` so two orgs can each own a
/// "services" (or "api", or "web") without colliding — and so a workspace's
/// name says who it belongs to. Falls back to the bare repo name when no
/// owner is present in the URL.
pub fn repo_path_from_url(url: &str) -> Option<String> {
    let trimmed = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    // Normalize scp-style (git@host:owner/repo) to a slash-separated tail.
    let after_host = match trimmed.split_once(':') {
        // scp-style has no "//" right after the colon; a URL scheme does.
        Some((_, rest)) if !rest.starts_with("//") => rest.to_string(),
        _ => {
            let no_scheme = trimmed
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(trimmed);
            // drop host (and any credentials) — keep the path
            match no_scheme.split_once('/') {
                Some((_, path)) => path.to_string(),
                None => return None,
            }
        }
    };

    let parts: Vec<&str> = after_host.split('/').filter(|p| !p.is_empty()).collect();
    let repo = parts.last()?.trim();
    if !safe_segment(repo) {
        return None;
    }
    // The owner is the segment directly above the repo (handles nested
    // GitLab-style groups by taking the closest one).
    match parts.len() {
        0 => None,
        1 => Some(repo.to_string()),
        _ => {
            let owner = parts[parts.len() - 2].trim();
            if safe_segment(owner) {
                Some(format!("{owner}/{repo}"))
            } else {
                Some(repo.to_string())
            }
        }
    }
}

/// Write a control-plane-supplied private key to a transient 0600 file.
/// Deleted by the caller (see `TransientKey::drop`).
pub struct TransientKey {
    pub path: PathBuf,
}

impl TransientKey {
    pub fn write(key_material: &str) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("nook-keys");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("k{}", uuid::Uuid::now_v7().simple()));
        let mut material = key_material.trim_end().to_string();
        material.push('\n');
        std::fs::write(&path, material).ok()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok()?;
        Some(Self { path })
    }
}

impl Drop for TransientKey {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn clone_repo(
    workspace_root: &str,
    url: &str,
    dest_name: Option<&str>,
    ssh_key_material: Option<&str>,
) -> OpOutcome {
    let root = crate::config::expand_path(workspace_root);
    // Checkouts live at <root>/<owner>/<repo> so repos with the same name in
    // different orgs don't collide. An explicit dest_name may itself be
    // qualified ("owner/repo"); every segment is validated.
    let name = match dest_name
        .map(str::to_string)
        .or_else(|| repo_path_from_url(url))
    {
        Some(n) if n.split('/').all(safe_segment) => n,
        _ => return fail("could not derive a safe directory name from the URL"),
    };
    let dest = Path::new(&root).join(&name);
    if dest.exists() {
        // Idempotent: a git checkout already sitting at this repo's deterministic
        // path IS this repo's clone (the path is derived from the URL), so report
        // success with the path rather than a collision. Clone-on-demand re-issues
        // on every pass until the checkout is recorded, and each re-issue has to
        // converge, not fail — a hard error would strand the workspace forever.
        // A directory that is NOT a git checkout is a real collision and still fails.
        if dest.join(".git").exists() {
            return OpOutcome {
                ok: true,
                path: Some(dest.to_string_lossy().to_string()),
                message: format!("already cloned at {}", dest.display()),
            };
        }
        return fail(format!("{} already exists", dest.display()));
    }
    // Creates the owner directory too.
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return fail(format!("cannot create {}", parent.display()));
        }
    }

    match run_git_remote(
        &["clone", url, &dest.to_string_lossy()],
        None,
        ssh_key_material,
    ) {
        Ok(_) => OpOutcome {
            ok: true,
            path: Some(dest.to_string_lossy().to_string()),
            message: format!("cloned into {}", dest.display()),
        },
        Err(e) => fail(explain_git_error("clone", &e, ssh_key_material.is_some())),
    }
}

/// Delete a checkout directory — primary clone or linked worktree.
///
/// Deliberately paranoid: this is the only operation that removes user files,
/// so the path must sit inside one of the node's configured workspace roots
/// AND look like a git checkout. A worktree is removed through git so the
/// primary repo's metadata stays consistent.
pub fn remove_checkout(path: &str, workspace_roots: &[String]) -> OpOutcome {
    let dir = Path::new(path);
    // Resolve symlinks/.. before comparing against the roots.
    let Ok(canonical) = dir.canonicalize() else {
        return fail(format!("{path} does not exist"));
    };
    let inside_root = workspace_roots.iter().any(|root| {
        Path::new(&crate::config::expand_path(root))
            .canonicalize()
            .is_ok_and(|r| canonical.starts_with(&r) && canonical != r)
    });
    if !inside_root {
        return fail(format!(
            "refusing to delete {path}: outside this node's workspace roots"
        ));
    }
    let git_marker = canonical.join(".git");
    if !git_marker.exists() {
        return fail(format!("refusing to delete {path}: not a git checkout"));
    }

    // Linked worktree (.git is a file) → let git unregister it properly.
    if git_marker.is_file() {
        return remove_worktree(&canonical.to_string_lossy());
    }
    match std::fs::remove_dir_all(&canonical) {
        Ok(()) => OpOutcome {
            ok: true,
            path: Some(path.to_string()),
            message: format!("removed checkout {path}"),
        },
        Err(e) => fail(format!("could not remove {path}: {e}")),
    }
}

/// Turn git's terse transport errors into something the operator can act on.
/// Auth failures are the common case and the fix depends on which key the
/// node is using, so say exactly which one was presented.
/// Turn git's auth refusal into something actionable, naming the key that was
/// actually offered. `what` is the operation, so the message reads as the thing
/// the user tried ("clone failed" / "push failed").
fn explain_git_error(what: &str, stderr: &str, used_tenant_credential: bool) -> String {
    let lower = stderr.to_lowercase();
    let auth_failed = lower.contains("permission denied")
        || lower.contains("could not read from remote repository")
        || lower.contains("authentication failed");
    if !auth_failed {
        return format!("{what} failed: {stderr}");
    }

    let which = if used_tenant_credential {
        "the git credential from the vault".to_string()
    } else if let Some(cfg) = crate::config::NodeConfig::load()
        .ok()
        .and_then(|c| c.ssh_key_path)
    {
        format!("this node's configured key ({cfg})")
    } else {
        "this node's own generated key".to_string()
    };

    let key_hint = crate::ssh::public_key_for(
        crate::config::NodeConfig::load()
            .ok()
            .and_then(|c| c.ssh_key_path)
            .as_deref(),
    )
    .map(|k| format!("\n\nPublic key presented:\n{k}"))
    .unwrap_or_default();

    format!(
        "authentication rejected by the git host — {which} does not have access \
         to this repository.\n\nFix it one of these ways:\n\
         • Add the public key below as a deploy key (repo → Settings → Deploy keys)\n\
         • Run `nook setup` on this node and choose an existing SSH key that has access\n\
         • Add a git credential in NookOS (Settings → Git credentials) for this tenant\
         {key_hint}\n\ngit said: {stderr}"
    )
}

pub fn add_worktree(repo_path: &str, branch: &str) -> OpOutcome {
    let repo = Path::new(repo_path);
    if !repo.join(".git").exists() {
        return fail("not a git checkout");
    }
    let sanitized: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let dest = repo
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{repo_name}__{sanitized}"));
    if dest.exists() {
        return fail(format!("{} already exists", dest.display()));
    }

    // Existing branch first; fall back to creating it.
    let dest_str = dest.to_string_lossy().to_string();
    if let Err(attach_err) = run_git(&["worktree", "add", &dest_str, branch], Some(repo), None) {
        if let Err(create_err) = run_git(
            &["worktree", "add", "-b", branch, &dest_str],
            Some(repo),
            None,
        ) {
            // Both failed. When the reason is that the branch is already checked
            // out in ANOTHER worktree (attach can't move it, `-b` can't recreate
            // it), neither can win — check out its tip DETACHED at the new path
            // instead (MAIN-225 AC-5). A genuinely unrelated failure (dest exists,
            // not a git repo, disk error) is NOT a collision and still fails as
            // before; the dest/repo cases were already rejected above.
            if is_branch_collision(&attach_err) {
                return match run_git(
                    &["worktree", "add", "--detach", &dest_str, branch],
                    Some(repo),
                    None,
                ) {
                    Ok(_) => OpOutcome {
                        ok: true,
                        path: Some(dest_str.clone()),
                        message: format!(
                            "worktree at {dest_str}, detached at '{branch}' \
                             (that branch is checked out in another worktree)"
                        ),
                    },
                    Err(e) => fail(format!("worktree add failed: {e}")),
                };
            }
            return fail(format!("worktree add failed: {create_err}"));
        }
    }
    OpOutcome {
        ok: true,
        path: Some(dest_str.clone()),
        message: format!("worktree for '{branch}' at {dest_str}"),
    }
}

/// Git refuses `worktree add <dest> <branch>` when `<branch>` is already checked
/// out in another worktree, with a message naming that state. Detecting it lets
/// [`add_worktree`] fall back to a detached checkout (MAIN-225 AC-5) rather than
/// surfacing a bare "worktree add failed".
fn is_branch_collision(git_stderr: &str) -> bool {
    let s = git_stderr.to_lowercase();
    s.contains("already checked out") || s.contains("already used by worktree")
}

/// Stage everything and commit. The UI's "commit" button, which is aimed at
/// the common case: you looked at the diff, you want it saved.
///
/// Staging is deliberately all-or-nothing here — a partial-staging UI is a
/// different feature, and pretending to offer one from a button labelled
/// "commit" would quietly leave work behind.
/// The success arm both commit paths share: report the short sha, which is what
/// a person actually checks, falling back to git's own words if it is missing.
fn committed(checkout_path: &str, dir: &Path, out: String) -> OpOutcome {
    let sha = run_git(&["rev-parse", "--short", "HEAD"], Some(dir), None)
        .unwrap_or_default()
        .trim()
        .to_string();
    OpOutcome {
        ok: true,
        path: Some(checkout_path.to_string()),
        message: if sha.is_empty() {
            out.trim().to_string()
        } else {
            format!("committed {sha}")
        },
    }
}

/// Reject a path a caller has no business staging (MAIN-325).
///
/// These arrive from a browser, and `git add` is happy to be pointed at things:
/// an absolute path, or one that climbs out with `..`, would stage a file from
/// outside the checkout. A leading `-` is worse — git would read it as a FLAG
/// rather than a path. Git rejects most of this itself, but "most" is not a
/// security argument, and the `--` separator only fixes the last case.
///
/// Empty is rejected too: `git add ""` is an error whose message says nothing
/// useful about where the empty string came from.
pub(crate) fn staging_path_ok(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('-')
        && !p.starts_with('/')
        && !Path::new(p).is_absolute()
        && !p.split(['/', '\\']).any(|seg| seg == "..")
}

/// Stage `paths` (or everything, when `None`) and commit.
pub fn commit_paths(checkout_path: &str, message: &str, paths: Option<&[String]>) -> OpOutcome {
    let dir = Path::new(checkout_path);
    if !dir.join(".git").exists() {
        return fail("not a git checkout");
    }
    if message.trim().is_empty() {
        return fail("a commit needs a message");
    }
    // The selection is checked BEFORE any git runs. A request naming a path
    // outside the checkout is refused on its own terms, not because some later
    // git invocation happened to dislike it — and the refusal says which path.
    match paths {
        // A selection that names nothing would fall through to `git add -A` and
        // commit the whole tree — the opposite of what a partial commit asks
        // for. Refuse rather than guess.
        Some([]) => return fail("no files selected to commit"),
        Some(list) => {
            if let Some(bad) = list.iter().find(|p| !staging_path_ok(p)) {
                return fail(format!(
                    "refusing to stage {bad:?} — not a path inside the checkout"
                ));
            }
        }
        None => {}
    }

    // Nothing staged AND nothing to stage means there is nothing to commit;
    // say so plainly rather than letting git's "nothing to commit" reach the
    // user as a failed operation.
    match run_git(&["status", "--porcelain"], Some(dir), None) {
        Ok(out) if out.trim().is_empty() => {
            return fail("nothing to commit — the working tree is clean");
        }
        Err(e) => return fail(format!("git status failed: {e}")),
        _ => {}
    }

    match paths {
        Some(list) => {
            // `--` first: after it git cannot mistake any of these for a flag.
            let mut args: Vec<&str> = vec!["add", "--"];
            args.extend(list.iter().map(String::as_str));
            if let Err(e) = run_git(&args, Some(dir), None) {
                return fail(format!("git add failed: {e}"));
            }
        }
        None => {
            if let Err(e) = run_git(&["add", "-A"], Some(dir), None) {
                return fail(format!("git add failed: {e}"));
            }
        }
    }
    // Plain `commit`, never `-a`: with a selection, `-a` would sweep up every
    // other modified file alongside the ones that were named.
    match run_git(&["commit", "-m", message], Some(dir), None) {
        Ok(out) => committed(checkout_path, dir, out),
        Err(e) => fail(format!("commit failed: {e}")),
    }
}

/// Push the current branch, setting upstream on first push.
///
/// Uses the same SSH identity as clone, so a repo you could clone is a repo you
/// can push to — and when it isn't, the error explains which key was offered
/// rather than leaving you with git's bare "permission denied".
pub fn push_current(checkout_path: &str, ssh_key_material: Option<&str>) -> OpOutcome {
    let dir = Path::new(checkout_path);
    if !dir.join(".git").exists() {
        return fail("not a git checkout");
    }
    let branch = match run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(dir), None) {
        Ok(b) => b.trim().to_string(),
        Err(e) => return fail(format!("could not read the current branch: {e}")),
    };
    if branch == "HEAD" {
        return fail("detached HEAD — check out a branch before pushing");
    }

    // -u so the first push on a fresh branch doesn't need the caller to know
    // that "no upstream" is a different command.
    match run_git_remote(
        &["push", "-u", "origin", &branch],
        Some(dir),
        ssh_key_material,
    ) {
        Ok(_) => OpOutcome {
            ok: true,
            path: Some(checkout_path.to_string()),
            message: format!("pushed {branch} to origin"),
        },
        Err(e) => fail(explain_git_error("push", &e, ssh_key_material.is_some())),
    }
}

pub fn remove_worktree(worktree_path: &str) -> OpOutcome {
    let dir = Path::new(worktree_path);
    if !dir.join(".git").exists() {
        return fail("not a git checkout");
    }
    // `git worktree remove` run from inside the worktree; --force tolerates a
    // dirty tree (the task is done, we're cleaning up).
    match run_git(
        &["worktree", "remove", "--force", worktree_path],
        Some(dir),
        None,
    ) {
        Ok(_) => OpOutcome {
            ok: true,
            path: Some(worktree_path.to_string()),
            message: format!("removed worktree {worktree_path}"),
        },
        Err(e) => fail(format!("worktree remove failed: {e}")),
    }
}

/// Create a brand-new empty git project (`git init` + README + first commit).
pub fn init_project(workspace_root: &str, name: &str) -> OpOutcome {
    let root = crate::config::expand_path(workspace_root);
    if name.contains('/') || name.starts_with('.') || name.trim().is_empty() {
        return fail("invalid project name");
    }
    let dest = Path::new(&root).join(name);
    if dest.exists() {
        return fail(format!("{} already exists", dest.display()));
    }
    if std::fs::create_dir_all(&dest).is_err() {
        return fail(format!("cannot create {}", dest.display()));
    }
    if std::fs::write(dest.join("README.md"), format!("# {name}\n")).is_err() {
        return fail("cannot write README");
    }
    let steps: [&[&str]; 4] = [
        &["init", "-b", "main"],
        &["add", "."],
        &[
            "-c",
            "user.email=nook@nookos.local",
            "-c",
            "user.name=NookOS",
            "commit",
            "-m",
            "initial commit",
        ],
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    ];
    for args in steps {
        if let Err(e) = run_git(args, Some(&dest), None) {
            // symbolic-ref may already be correct; only fail on the essentials.
            if args[0] == "init" || args[0] == "commit" {
                return fail(format!("git {} failed: {e}", args[0]));
            }
        }
    }
    OpOutcome {
        ok: true,
        path: Some(dest.to_string_lossy().to_string()),
        message: format!("created project {}", dest.display()),
    }
}

/// Write a synced workspace file (e.g. .env) with owner-only permissions.
pub fn write_workspace_file(checkout_path: &str, name: &str, content: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if name.contains('/') || name.contains("..") {
        return Err("invalid file name".into());
    }
    let dir = Path::new(checkout_path);
    if !dir.is_dir() {
        return Err(format!("checkout {checkout_path} does not exist"));
    }
    let path = dir.join(name);
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Read a workspace file back out of a checkout, for adopting a repo's
/// existing `.env` into the vault. Same name guard as the write path: only a
/// plain file name directly inside the checkout.
pub fn read_workspace_file(checkout_path: &str, name: &str) -> Result<Vec<u8>, String> {
    if name.contains('/') || name.contains("..") {
        return Err("invalid file name".into());
    }
    let path = Path::new(checkout_path).join(name);
    if !path.is_file() {
        return Err(format!("no {name} in {checkout_path}"));
    }
    std::fs::read(&path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{add_worktree, commit_paths, repo_path_from_url, staging_path_ok};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A unique scratch dir (no `tempfile` dependency). Best-effort cleanup via
    /// `Scratch`'s Drop; a leak lands in the system temp dir, reclaimed by the OS.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Scratch {
            let p = std::env::temp_dir().join(format!(
                "nook-wt-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// A repo at `<base>/repo` with one commit on the default branch.
    fn init_repo(base: &Path) -> PathBuf {
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@example.test"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f"), "x").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "c"]);
        repo
    }

    #[test]
    fn add_worktree_detaches_when_branch_is_checked_out_elsewhere() {
        let base = Scratch::new();
        let repo = init_repo(&base.0);
        // `feature` exists AND is checked out in another worktree, so neither the
        // attach nor the `-b` create can use it (MAIN-225 AC-5).
        git(&repo, &["branch", "feature"]);
        let other = base.0.join("other-checkout");
        git(
            &repo,
            &["worktree", "add", other.to_str().unwrap(), "feature"],
        );

        let out = add_worktree(repo.to_str().unwrap(), "feature");
        assert!(out.ok, "expected a detached success, got: {}", out.message);
        assert!(
            out.message.contains("detached"),
            "message names the detached fallback: {}",
            out.message
        );
        let made = out.path.expect("a worktree path");
        assert!(Path::new(&made).exists(), "the detached worktree exists");
    }

    #[test]
    fn add_worktree_fails_on_a_non_git_path() {
        let base = Scratch::new();
        let plain = base.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let out = add_worktree(plain.to_str().unwrap(), "feature");
        assert!(!out.ok, "a non-git path is not a checkout");
    }

    #[test]
    fn add_worktree_fails_when_the_destination_already_exists() {
        let base = Scratch::new();
        let repo = init_repo(&base.0);
        // add_worktree derives dest = <repo.parent>/<repo_name>__<branch>.
        let dest = base.0.join("repo__feature");
        std::fs::create_dir_all(&dest).unwrap();
        let out = add_worktree(repo.to_str().unwrap(), "feature");
        assert!(
            !out.ok,
            "an existing destination is refused before any git op"
        );
        assert!(out.message.contains("already exists"), "{}", out.message);
    }

    #[test]
    fn derives_owner_and_repo_across_url_shapes() {
        for url in [
            "git@github.com:acme/services.git",
            "https://github.com/acme/services.git",
            "https://github.com/acme/services",
            "ssh://git@github.com/acme/services.git",
            "https://user:pass@github.com/acme/services.git",
            "git@github.com:acme/services/",
        ] {
            assert_eq!(
                repo_path_from_url(url).as_deref(),
                Some("acme/services"),
                "{url}"
            );
        }
    }

    #[test]
    fn nested_groups_use_the_closest_owner() {
        assert_eq!(
            repo_path_from_url("https://gitlab.com/team/sub/group/api.git").as_deref(),
            Some("group/api")
        );
    }

    #[test]
    fn same_repo_name_in_two_orgs_does_not_collide() {
        assert_ne!(
            repo_path_from_url("git@github.com:acme/services.git"),
            repo_path_from_url("git@github.com:globex/services.git"),
        );
    }

    #[test]
    fn rejects_path_traversal_and_keeps_bare_name_fallback() {
        assert_eq!(
            repo_path_from_url("git@github.com:owner/..").as_deref(),
            None
        );
        // A local path has no real owner, so the segment before the repo is
        // taken as one. "git" is a directory here rather than an account —
        // harmless, since this only ever names a checkout directory.
        assert_eq!(
            repo_path_from_url("/srv/git/solo.git").as_deref(),
            Some("git/solo")
        );
    }

    // ── MAIN-325: selective staging ─────────────────────────────────────────

    #[test]
    fn an_ordinary_repo_relative_path_is_stageable() {
        for p in [
            "src/main.rs",
            "README.md",
            "a b/c.txt",
            "deep/nested/dir/file",
            "..hidden",
            "src/..foo",
        ] {
            assert!(staging_path_ok(p), "{p} should be stageable");
        }
    }

    #[test]
    fn a_path_that_leaves_the_checkout_is_refused() {
        // These arrive from a browser. `git add` would happily follow them out
        // of the checkout, and the file it staged would be somebody's ssh key.
        for p in ["../outside", "a/../../outside", "/etc/passwd", "..", "a/.."] {
            assert!(!staging_path_ok(p), "{p} must be refused");
        }
    }

    #[test]
    fn a_path_that_would_be_read_as_a_flag_is_refused() {
        // The one `--` cannot save us from is a caller who gets to choose the
        // argument BEFORE the separator — so refuse the shape outright.
        for p in ["--all", "-A", "-"] {
            assert!(!staging_path_ok(p), "{p} must be refused");
        }
    }

    #[test]
    fn an_empty_path_is_refused() {
        assert!(!staging_path_ok(""));
    }

    /// The PATHS git still reports as dirty. Paths, not the raw text: asking
    /// whether the output "contains" a filename says yes for `unwanted` when
    /// the file left behind is `wanted`, and a test that passes on a substring
    /// is not testing what it claims.
    fn dirty_paths(repo: &Path) -> Vec<String> {
        String::from_utf8(
            Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(repo)
                .output()
                .expect("git status")
                .stdout,
        )
        .expect("utf8")
        .lines()
        .filter(|l| l.len() > 3)
        .map(|l| l[3..].to_string())
        .collect()
    }

    #[test]
    fn a_selection_commits_only_what_it_named() {
        // The whole point of AC-1: two files changed, one committed. The other
        // must still be sitting there afterwards. `git commit -a` — or falling
        // back to `add -A` — would take both and silently include work the
        // author did not mean to ship.
        let base = Scratch::new();
        let repo = init_repo(&base.0);
        std::fs::write(repo.join("wanted"), "a").unwrap();
        std::fs::write(repo.join("unwanted"), "b").unwrap();

        let out = commit_paths(
            &repo.to_string_lossy(),
            "just the one",
            Some(&["wanted".to_string()]),
        );
        assert!(out.ok, "{}", out.message);
        assert!(out.message.starts_with("committed "), "{}", out.message);

        assert_eq!(
            dirty_paths(&repo),
            vec!["unwanted".to_string()],
            "only the file that was NOT named should still be dirty"
        );
    }

    #[test]
    fn no_selection_still_commits_everything() {
        // The `None` path is what every existing caller sends, and it must keep
        // meaning what it meant before selective staging existed.
        let base = Scratch::new();
        let repo = init_repo(&base.0);
        std::fs::write(repo.join("one"), "a").unwrap();
        std::fs::write(repo.join("two"), "b").unwrap();

        let out = commit_paths(&repo.to_string_lossy(), "everything", None);
        assert!(out.ok, "{}", out.message);
        assert!(dirty_paths(&repo).is_empty(), "the tree should be clean");
    }

    #[test]
    fn an_empty_selection_is_refused_rather_than_committing_everything() {
        // The dangerous default: `Some([])` falling through to `git add -A`
        // would commit the whole tree when the user asked for nothing.
        let dir = std::env::temp_dir().join(format!("nook325-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join(".git")).expect("fixture");
        let out = commit_paths(&dir.to_string_lossy(), "m", Some(&[]));
        assert!(!out.ok);
        assert!(out.message.contains("no files selected"), "{}", out.message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_traversing_selection_is_refused_before_git_runs() {
        let dir = std::env::temp_dir().join(format!("nook325-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join(".git")).expect("fixture");
        let out = commit_paths(
            &dir.to_string_lossy(),
            "m",
            Some(&["../../etc/passwd".to_string()]),
        );
        assert!(!out.ok);
        assert!(out.message.contains("refusing to stage"), "{}", out.message);
        std::fs::remove_dir_all(&dir).ok();
    }
}
