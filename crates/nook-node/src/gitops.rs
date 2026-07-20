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

/// Derive "repo" from ".../repo.git" or ".../repo".
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let name = trimmed
        .rsplit(['/', ':'])
        .next()?
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
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
    let name = match dest_name
        .map(str::to_string)
        .or_else(|| repo_name_from_url(url))
    {
        Some(n) if !n.contains('/') && !n.starts_with('.') => n,
        _ => return fail("could not derive a safe directory name from the URL"),
    };
    let dest = Path::new(&root).join(&name);
    if dest.exists() {
        return fail(format!("{} already exists", dest.display()));
    }
    if std::fs::create_dir_all(&root).is_err() {
        return fail(format!("cannot create workspace root {root}"));
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
        Err(e) => fail(format!("clone failed: {e}")),
    }
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
