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

/// Is this directory inside a git repository?
///
/// The subtlety is what to do when git refuses to answer. `rev-parse` exits
/// non-zero for "this is not a repository" AND for "detected dubious
/// ownership" — the latter happens whenever the checkout is owned by a
/// different user than the node process, which is the normal state of affairs
/// in a container with a bind mount. Treating that as "no repository" would
/// hide the git panel on a real repository, which is the failure this whole
/// change exists to avoid.
///
/// So only git's own words for the negative case count. Anything else — git
/// missing, permissions, ownership — answers `true` and lets the panel show
/// whatever it can, which is the behaviour that existed before.
fn is_git_repo(dir: &Path) -> bool {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--git-dir"])
        .output()
    else {
        return true; // no git binary — not our place to say
    };
    if out.status.success() {
        return true;
    }
    !says_no_repository(&String::from_utf8_lossy(&out.stderr))
}

/// Does this `git rev-parse` failure mean "there is no repository here"?
///
/// Split out so the distinction can be tested against git's real wording
/// without arranging a second uid to produce an ownership refusal.
fn says_no_repository(stderr: &str) -> bool {
    stderr.to_lowercase().contains("not a git repository")
}

/// What the node can say about a checkout.
pub struct GitSnapshot {
    /// Is this directory inside a git repository at all?
    ///
    /// Worth its own field because every other value here is empty in two very
    /// different situations — a clean repository and a directory that is not a
    /// repository — and the UI has to tell them apart. Without it, "+ New empty
    /// project" produces a git panel showing a blank diff and the word "clean",
    /// which is a confident answer to a question that does not apply.
    pub is_repo: bool,
    pub branch: Option<String>,
    pub files: Vec<nook_types::GitFileStatus>,
    pub diff: String,
}

/// Branch + porcelain status + working-tree diff for a checkout. Everything
/// comes from git itself; the diff is truncated so a giant refactor can't
/// blow up the WebSocket frame.
pub fn git_status(path: &str) -> GitSnapshot {
    let dir = Path::new(path);
    let is_repo = is_git_repo(dir);
    if !is_repo {
        return GitSnapshot {
            is_repo: false,
            branch: None,
            files: Vec::new(),
            diff: String::new(),
        };
    }
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
    GitSnapshot {
        is_repo: true,
        branch,
        files,
        diff,
    }
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

#[cfg(test)]
mod git_status_tests {
    use super::*;

    /// A directory that is not a repository must say so, rather than looking
    /// like a clean one — they are otherwise byte-identical.
    #[test]
    fn a_plain_directory_is_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("nook-notrepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let snap = git_status(&dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!snap.is_repo, "a plain directory must not report as a repo");
        assert_eq!(snap.branch, None);
        assert!(snap.files.is_empty());
    }

    /// A repository must report as one, so the check is not just always-false.
    /// Made here rather than using the crate's own checkout, because that one
    /// is bind-mounted and git refuses it for "dubious ownership" — which is
    /// the case the next test is about.
    #[test]
    fn a_real_repository_is_a_repo() {
        let dir = std::env::temp_dir().join(format!("nook-isrepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("init")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no usable git here; the negative test still holds
        }
        let snap = git_status(&dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(snap.is_repo, "a `git init` directory must report as a repo");
    }

    /// Git refusing to answer is NOT "there is no repository".
    ///
    /// A checkout owned by another user makes `rev-parse` fail with "dubious
    /// ownership" — the ordinary state of a bind mount in a container, and
    /// exactly what happens to this repo inside the dev container. Reading
    /// that as "no repo" would hide the git panel on real work.
    #[test]
    fn only_gits_own_words_mean_there_is_no_repository() {
        assert!(says_no_repository(
            "fatal: not a git repository (or any of the parent directories): .git"
        ));
        assert!(says_no_repository(
            "fatal: not a git repository: '/tmp/x/.git'"
        ));

        for refusal in [
            "fatal: detected dubious ownership in repository at '/app'",
            "fatal: could not read Username for 'https://github.com'",
            "error: cannot open .git/FETCH_HEAD: Permission denied",
            "fatal: unsafe repository ('/app' is owned by someone else)",
        ] {
            assert!(
                !says_no_repository(refusal),
                "`{refusal}` is git declining to answer, not git saying there is \
                 no repository — treating it as the latter hides the git panel \
                 on a real checkout"
            );
        }
    }
}
