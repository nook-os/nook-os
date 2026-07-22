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

fn run_git(args: &[&str], cwd: Option<&Path>, ssh_key: Option<&Path>) -> Result<String, String> {
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
        return fail(format!("{} already exists", dest.display()));
    }
    // Creates the owner directory too.
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return fail(format!("cannot create {}", parent.display()));
        }
    }

    // Tenant credential (if provided) lives on disk only for the duration of
    // the clone.
    let transient = ssh_key_material.and_then(TransientKey::write);
    let key_path = transient.as_ref().map(|t| t.path.as_path());

    match run_git(&["clone", url, &dest.to_string_lossy()], None, key_path) {
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
    let existing = run_git(&["worktree", "add", &dest_str, branch], Some(repo), None);
    let result = match existing {
        Ok(out) => Ok(out),
        Err(_) => run_git(
            &["worktree", "add", "-b", branch, &dest_str],
            Some(repo),
            None,
        ),
    };
    match result {
        Ok(_) => OpOutcome {
            ok: true,
            path: Some(dest_str.clone()),
            message: format!("worktree for '{branch}' at {dest_str}"),
        },
        Err(e) => fail(format!("worktree add failed: {e}")),
    }
}

/// Stage everything and commit. The UI's "commit" button, which is aimed at
/// the common case: you looked at the diff, you want it saved.
///
/// Staging is deliberately all-or-nothing here — a partial-staging UI is a
/// different feature, and pretending to offer one from a button labelled
/// "commit" would quietly leave work behind.
pub fn commit_all(checkout_path: &str, message: &str) -> OpOutcome {
    let dir = Path::new(checkout_path);
    if !dir.join(".git").exists() {
        return fail("not a git checkout");
    }
    if message.trim().is_empty() {
        return fail("a commit needs a message");
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

    if let Err(e) = run_git(&["add", "-A"], Some(dir), None) {
        return fail(format!("git add failed: {e}"));
    }
    match run_git(&["commit", "-m", message], Some(dir), None) {
        Ok(out) => {
            // "abc1234 message" — the short sha is what a person checks.
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

    let transient = ssh_key_material.and_then(TransientKey::write);
    let key_path = transient.as_ref().map(|k| k.path.clone()).or_else(|| {
        crate::config::NodeConfig::load()
            .ok()
            .and_then(|c| c.ssh_key_path)
            .map(std::path::PathBuf::from)
    });

    // -u so the first push on a fresh branch doesn't need the caller to know
    // that "no upstream" is a different command.
    match run_git(
        &["push", "-u", "origin", &branch],
        Some(dir),
        key_path.as_deref(),
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
    use super::repo_path_from_url;

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
}
