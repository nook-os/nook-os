//! Workspace discovery: scan configured roots (depth ≤ 2) for git
//! repositories. Repositories are self-describing — name, remote, branch,
//! dirtiness all come from git itself.

use nook_proto::DiscoveredWorkspace;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A checkout's origin remote, if it has one.
pub fn remote_of(dir: &Path) -> Option<String> {
    git(dir, &["config", "--get", "remote.origin.url"])
}

fn inspect(dir: &Path) -> Option<DiscoveredWorkspace> {
    let git_marker = dir.join(".git");
    if !git_marker.exists() {
        return None;
    }
    // A primary checkout has a `.git` directory; a linked worktree has a
    // `.git` file pointing back at the primary repo.
    let worktree = git_marker.is_file();
    let remote = git(dir, &["config", "--get", "remote.origin.url"]);
    // Name a workspace after its remote ("owner/repo") — that's its real
    // identity, and it keeps two orgs' "services" repos distinguishable. Only
    // remote-less checkouts fall back to the directory name.
    let name = remote
        .as_deref()
        .and_then(crate::gitops::repo_path_from_url)
        .unwrap_or_else(|| {
            dir.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
    Some(DiscoveredWorkspace {
        path: dir.to_string_lossy().to_string(),
        name,
        git_remote_url: remote,
        branch: git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        dirty: git(dir, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty()),
        worktree,
    })
}

const MAX_DIFF_BYTES: usize = 200_000;

/// Branch + porcelain status + working-tree diff for a checkout. Everything
/// comes from git itself; the diff is truncated so a giant refactor can't
/// blow up the WebSocket frame.
pub fn git_status(path: &str) -> (Option<String>, Vec<nook_types::GitFileStatus>, String) {
    let dir = Path::new(path);
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let files = git(dir, &["status", "--porcelain"])
        .map(|out| {
            out.lines()
                .filter(|l| l.len() > 3)
                .map(|l| nook_types::GitFileStatus {
                    status: l[..2].to_string(),
                    path: l[3..].to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    // Untracked files don't appear in `git diff`; HEAD may not exist yet.
    let mut diff = git(dir, &["diff", "HEAD"])
        .or_else(|| git(dir, &["diff"]))
        .unwrap_or_default();
    if diff.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n… diff truncated …\n");
    }
    (branch, files, diff)
}

pub fn scan(roots: &[String]) -> Vec<DiscoveredWorkspace> {
    let mut found = Vec::new();
    for root in roots {
        let root = crate::config::expand_path(root);
        let root = Path::new(&root);
        // The root itself may be a repository.
        if let Some(ws) = inspect(root) {
            found.push(ws);
            continue;
        }
        // Depth 1 and 2.
        let Ok(level1) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in level1.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            if let Some(ws) = inspect(&p) {
                found.push(ws);
                continue;
            }
            let Ok(level2) = std::fs::read_dir(&p) else {
                continue;
            };
            for entry2 in level2.flatten() {
                let p2 = entry2.path();
                if p2.is_dir() {
                    if let Some(ws) = inspect(&p2) {
                        found.push(ws);
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}
