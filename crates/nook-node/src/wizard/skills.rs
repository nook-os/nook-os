//! `nook skills install` — teach an agent to drive the fleet.
//!
//! The skill is embedded rather than read from the repo, because the whole
//! point is that nobody had to clone anything. `skills/install.sh` needed the
//! working tree; this needs the binary that is already on the machine.
//!
//! The same writer serves `nook teach`: the control plane sends a name and a
//! document, and this end decides which agents are actually installed and
//! writes into each. Detection lives here rather than there because only the
//! machine knows what is on it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The skills that ship inside the binary and install as first-class citizens —
/// the fleet-driving `nookos` skill plus the loop skills (spec, build, review,
/// epic, and the two merge authorities). Embedded rather than read from a repo:
/// the whole point of `nook skills install` is that nobody had to clone
/// anything.
const EMBEDDED: &[(&str, &str)] = &[
    ("nookos", include_str!("../../../../skills/nookos/SKILL.md")),
    (
        "nook-spec",
        include_str!("../../../../skills/nook-spec/SKILL.md"),
    ),
    (
        "nook-build",
        include_str!("../../../../skills/nook-build/SKILL.md"),
    ),
    (
        "nook-review",
        include_str!("../../../../skills/nook-review/SKILL.md"),
    ),
    (
        "nook-epic",
        include_str!("../../../../skills/nook-epic/SKILL.md"),
    ),
    // The MERGE authority for an epic's children. It was the one loop skill
    // never embedded, so a node had four fifths of the loop and the pass that
    // lands work was the missing fifth (MAIN-344).
    (
        "nook-epic-runner",
        include_str!("../../../../skills/nook-epic-runner/SKILL.md"),
    ),
    // The OTHER merge authority, and the difference is what it does with
    // trouble: the epic runner halts the run, which is right for a supervised
    // twenty-minute pass and wrong for an unattended night, where one bad PR at
    // 1am would leave everything after it unmerged until morning. Yolo skips,
    // records the cause, and keeps going (MAIN-419).
    (
        "nook-yolo",
        include_str!("../../../../skills/nook-yolo/SKILL.md"),
    ),
    // The READ-ONLY one (MAIN-331). It ships with the rest because the run that
    // needs it is seeded by an inbound support email, on whichever machine has
    // capacity — there is no moment at which somebody could install it by hand.
    (
        "nook-investigate",
        include_str!("../../../../skills/nook-investigate/SKILL.md"),
    ),
];

/// Where a given agent keeps its skills.
struct Target {
    name: &'static str,
    /// Directories to write `<name>/SKILL.md` under.
    roots: Vec<PathBuf>,
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME is not set")?,
    ))
}

/// Discover which agents are actually present.
///
/// Detection, not assumption: writing a skill into a directory an agent does
/// not read is litter, and creating `~/.something/skills` for a tool that is
/// not installed is worse — it looks like configuration someone chose.
fn detect() -> Result<Vec<Target>> {
    Ok(detect_in(
        &home()?,
        std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .as_deref(),
    ))
}

/// The detection, with home AND the relocated Claude config dir supplied rather
/// than read from the environment, so it is testable without mutating
/// process-globals that parallel tests share.
///
/// `claude_cfg` was read from `CLAUDE_CONFIG_DIR` here until MAIN-355. Half a
/// parameter is no parameter: two tests exercising the relocated and the
/// default case had to `set_var`/`remove_var` around each other, and under the
/// parallel runner whichever lost the race read the other's value. It is the
/// same reason `home` is a parameter, applied to the second global.
fn detect_in(h: &Path, claude_cfg: Option<&Path>) -> Vec<Target> {
    let mut found = Vec::new();

    // Hermes keeps a global set AND a private copy per profile — profiles hold
    // copies rather than symlinks, so "install it for all my agents" means
    // writing to every one of them.
    if h.join(".hermes").is_dir() {
        let mut roots = vec![h.join(".hermes/skills")];
        if let Ok(entries) = std::fs::read_dir(h.join(".hermes/profiles")) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    roots.push(e.path().join("skills"));
                }
            }
        }
        found.push(Target {
            name: "Hermes",
            roots,
        });
    }

    // `CLAUDE_CONFIG_DIR` RELOCATES the whole config directory, and skills live
    // inside it. The operator node sets it (MAIN-238) so the fleet's identity is
    // separate from anyone's personal login — which meant every skill was being
    // written to `~/.claude/skills` while the agent read somewhere else
    // entirely. Verified on the live dev operator: `~/.claude/skills/nookos`
    // present, `$CLAUDE_CONFIG_DIR/skills` absent (MAIN-344).
    //
    // BOTH roots, not a swap: writing to the relocated dir is what fixes the
    // fleet, and keeping the default means a machine that later unsets the
    // variable does not silently lose its skills. Writing a second copy costs
    // one small file.
    if h.join(".claude").is_dir() || claude_cfg.is_some() {
        let mut roots = Vec::new();
        if let Some(dir) = claude_cfg {
            roots.push(dir.join("skills"));
        }
        if h.join(".claude").is_dir() {
            roots.push(h.join(".claude/skills"));
        }
        found.push(Target {
            name: "Claude Code",
            roots,
        });
    }

    // Codex uses the same `skills/<name>/SKILL.md` layout as Claude Code — its
    // built-ins live under `~/.codex/skills/.system`, user skills directly
    // under `~/.codex/skills`. It was simply never in this list, so a machine
    // with codex installed had its skill quietly skipped while hermes and
    // claude got theirs.
    if h.join(".codex").is_dir() {
        found.push(Target {
            name: "Codex",
            roots: vec![h.join(".codex/skills")],
        });
    }

    // OpenClaw: this is the conventional location, but it is unverified — I
    // could not find an installation to check against. Detect-only, so a wrong
    // guess costs nothing: if the directory is absent we simply say so and
    // point at --dir rather than inventing a home for it.
    if h.join(".openclaw").is_dir() {
        found.push(Target {
            name: "OpenClaw",
            roots: vec![h.join(".openclaw/skills")],
        });
    }

    found
}

/// Write every embedded skill into `root` as `<name>/SKILL.md`, one file each.
fn write_embedded(root: &Path) -> Result<Vec<PathBuf>> {
    EMBEDDED
        .iter()
        .map(|(name, content)| write_named(root, name, content))
        .collect()
}

/// Whether writing `content` to `path` would change anything — false when the
/// file already holds exactly it. This is the sha-skip (MAIN-105 AC-4): the
/// store hashes this same string, so content equality IS sha equality, and a
/// reconnect that replays the whole managed set rewrites nothing already current.
fn needs_write(path: &Path, content: &str) -> bool {
    !std::fs::read_to_string(path).is_ok_and(|existing| existing == content)
}

fn write_named(root: &Path, name: &str, content: &str) -> Result<PathBuf> {
    let dir = root.join(name);
    let path = dir.join("SKILL.md");
    if !needs_write(&path, content) {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// What a node did with a skill the control plane taught it.
#[derive(Debug, Default)]
pub struct Installed {
    pub agents: Vec<String>,
    pub paths: Vec<String>,
}

/// Write a taught skill into every agent on this machine.
///
/// The name is re-validated here even though the control plane already checked
/// it. This is the end that turns a wire string into a path, and it should not
/// be relying on the other end having been careful.
/// Write the embedded set into every detected agent, quietly, for a node that
/// is about to start running loop jobs (MAIN-344).
///
/// Called on every `nook run`, not once at join. A node joined by an older
/// binary keeps whatever set that binary shipped — which is how the dev
/// operator ended up with only `nookos` while four loop skills had been
/// embedded for cards — and a job that types `/nook-spec` at an agent which has
/// never heard of it is a silent no-op. Running it at boot means an image
/// upgrade IS the delivery mechanism.
///
/// Idempotent through the same sha-skip as every other write here, and
/// non-fatal: a node that cannot write a skill file should still come up and
/// say so, not refuse to run.
pub fn install_embedded_quietly() -> Vec<String> {
    let mut written = Vec::new();
    let Ok(targets) = detect() else {
        return written;
    };
    for t in targets {
        for root in &t.roots {
            match write_embedded(root) {
                Ok(paths) => written.extend(paths.into_iter().map(|p| p.display().to_string())),
                Err(e) => tracing::warn!(agent = t.name, root = %root.display(), error = %e,
                    "could not install the embedded skills"),
            }
        }
    }
    written
}

/// Whether a skill of this name is installed for any detected agent — the
/// preflight a loop job runs before typing `/<skill>` at an agent (MAIN-344
/// AC-5). Typing a command nothing resolves is a silent no-op, and a silent
/// no-op is the failure mode this whole card exists to remove.
pub fn is_installed(name: &str) -> bool {
    detect()
        .map(|ts| {
            ts.iter()
                .flat_map(|t| t.roots.iter())
                .any(|r| r.join(name).join("SKILL.md").is_file())
        })
        .unwrap_or(false)
}

pub fn install_taught(name: &str, content: &str) -> Result<Installed> {
    let name = safe_name(name)?;
    let mut out = Installed::default();
    for t in detect()? {
        for root in &t.roots {
            let p = write_named(root, name, content)?;
            out.paths.push(p.display().to_string());
        }
        out.agents.push(t.name.to_string());
    }
    Ok(out)
}

/// Remove a taught skill from every agent on this machine.
pub fn forget_taught(name: &str) -> Result<Vec<String>> {
    let name = safe_name(name)?;
    let mut removed = Vec::new();
    for t in detect()? {
        for root in &t.roots {
            let dir = root.join(name);
            // Only remove what looks like a skill directory. A `SKILL.md` is
            // the thing we wrote; a directory of somebody's own work that
            // happens to share the name is not ours to delete.
            if dir.join("SKILL.md").is_file() && std::fs::remove_dir_all(&dir).is_ok() {
                removed.push(dir.display().to_string());
            }
        }
    }
    Ok(removed)
}

/// The name check, borrowed from the crate that defines the message carrying
/// it. Deliberately not a second implementation: a name the control plane
/// accepts and this end refuses is a skill that reports as taught and exists on
/// no machine, and that divergence would only ever show up in production.
pub fn safe_name(name: &str) -> Result<&str> {
    nook_proto::valid_skill_name(name).map_err(|e| anyhow::anyhow!(e))
}

/// `dir` overrides detection entirely — the escape hatch for an agent we have
/// not special-cased.
pub fn install(dir: Option<PathBuf>, quiet: bool) -> Result<()> {
    if let Some(d) = dir {
        for p in write_embedded(&d)? {
            println!("✓ {}", p.display());
        }
        return Ok(());
    }

    let targets = detect()?;
    if targets.is_empty() {
        println!("No agent installations found.");
        println!();
        println!("Looked for ~/.hermes, ~/.claude, ~/.codex and ~/.openclaw. If your agent keeps");
        println!("skills somewhere else, point at it directly:");
        println!();
        println!("    nook skills install --dir ~/path/to/skills");
        return Ok(());
    }

    let mut count = 0;
    for t in &targets {
        for root in &t.roots {
            for p in write_embedded(root)? {
                count += 1;
                if !quiet {
                    println!("✓ {} → {}", t.name, p.display());
                }
            }
        }
    }
    println!(
        "\nInstalled {} NookOS skills ({count} file(s)) across {} agent(s).",
        EMBEDDED.len(),
        targets.len()
    );
    println!(
        "Your agents can now spec, build, review, drive epics, and run sessions across the fleet."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded copy has to be the real skill, not an empty file — a
    /// `include_str!` pointed at the wrong path still compiles if the file
    /// exists, and an agent handed a stub fails in a way nobody traces back
    /// here.
    #[test]
    fn the_embedded_skills_look_like_the_real_ones() {
        // All eight ship, in order, and each is a real document rather than an
        // empty file — an `include_str!` at the wrong path still compiles if the
        // file exists, and an agent handed a stub fails in a way nobody traces
        // back here.
        //
        // `nook-epic-runner` joined the set in MAIN-344: it was the one loop
        // skill never embedded, so a node had four fifths of the loop and the
        // pass that actually lands work was the missing fifth. `nook-yolo`
        // joined in MAIN-419 — the same merge authority scoped to the whole
        // board and told to skip rather than halt, which is what makes an
        // unattended night possible. `nook-investigate` joined in MAIN-331,
        // the read-only one.
        let names: Vec<&str> = EMBEDDED.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "nookos",
                "nook-spec",
                "nook-build",
                "nook-review",
                "nook-epic",
                "nook-epic-runner",
                "nook-yolo",
                "nook-investigate"
            ]
        );
        for (name, content) in EMBEDDED {
            assert!(
                content.len() > 500,
                "{name} suspiciously short: {}",
                content.len()
            );
            assert!(
                content.starts_with("---") || content.starts_with('#'),
                "{name} needs frontmatter or a heading"
            );
        }
    }

    /// The name RULES are tested in `nook-proto`, where they live. What this
    /// pins is that the path-making end applies them at all — the check that
    /// would be missing if someone inlined `root.join(name)` later.
    #[test]
    fn a_name_that_would_escape_the_skills_directory_is_refused() {
        for bad in [
            "..",
            ".",
            "../../etc",
            "a/b",
            "/etc/passwd",
            "",
            "has space",
        ] {
            assert!(safe_name(bad).is_err(), "must refuse {bad:?}");
            assert!(install_taught(bad, "x").is_err(), "must refuse {bad:?}");
            assert!(forget_taught(bad).is_err(), "must refuse {bad:?}");
        }
        assert_eq!(safe_name("code-review").unwrap(), "code-review");
    }

    /// Codex is detected and writes to `~/.codex/skills`, the same layout as
    /// Claude Code. This is the regression the fix is for: codex was installed,
    /// its directory present, and the skill was silently skipped because the
    /// list never named it.
    #[test]
    fn codex_is_detected_alongside_the_others() {
        let h = std::env::temp_dir().join(format!("nook-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        for d in [".claude", ".codex/skills/.system", ".hermes"] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }

        let found = detect_in(&h, None);
        let names: Vec<&str> = found.iter().map(|t| t.name).collect();
        assert!(names.contains(&"Codex"), "codex not detected: {names:?}");
        assert!(names.contains(&"Claude Code"), "{names:?}");
        assert!(names.contains(&"Hermes"), "{names:?}");

        let codex = found.iter().find(|t| t.name == "Codex").unwrap();
        assert_eq!(codex.roots, vec![h.join(".codex/skills")]);

        // Absent codex → not detected (no litter for a tool that isn't here).
        let bare = h.join("bare");
        std::fs::create_dir_all(bare.join(".claude")).unwrap();
        assert!(!detect_in(&bare, None).iter().any(|t| t.name == "Codex"));

        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn writing_creates_a_named_subdirectory_per_embedded_skill() {
        let dir = std::env::temp_dir().join(format!("nook-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = write_embedded(&dir).unwrap();
        // One `<name>/SKILL.md` per embedded skill.
        assert_eq!(paths.len(), EMBEDDED.len());
        for (name, content) in EMBEDDED {
            let p = dir.join(name).join("SKILL.md");
            assert!(paths.contains(&p), "{name} was not written");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), *content);
        }
        // Idempotent: installing twice must not fail or duplicate.
        assert_eq!(write_embedded(&dir).unwrap(), paths);

        // A taught skill lands under its own name, so two skills cannot
        // overwrite each other.
        let a = write_named(&dir, "alpha", "A").unwrap();
        let b = write_named(&dir, "beta", "B").unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "B");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Removing a taught skill must not remove somebody's own directory that
    /// happens to share the name — the marker is the `SKILL.md` we wrote.
    #[test]
    fn forgetting_only_removes_directories_holding_a_skill() {
        let base = std::env::temp_dir().join(format!("nook-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ours = base.join("taught");
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("SKILL.md"), "x").unwrap();
        let theirs = base.join("handmade");
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(theirs.join("notes.txt"), "mine").unwrap();

        assert!(ours.join("SKILL.md").is_file(), "ours is removable");
        assert!(!theirs.join("SKILL.md").exists(), "no SKILL.md → not ours");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The sha-skip (AC-4): a file that already holds the pushed content does not
    /// need rewriting, so connect-replay of the managed set is free.
    #[test]
    fn needs_write_is_the_sha_skip() {
        let base = std::env::temp_dir().join(format!("nook-skills-{}", uuid::Uuid::now_v7()));
        let path = base.join("SKILL.md");
        // Nonexistent → must write.
        assert!(needs_write(&path, "hello"));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&path, "hello").unwrap();
        // Same content → skip; different content → write.
        assert!(!needs_write(&path, "hello"), "identical content must skip");
        assert!(needs_write(&path, "changed"), "new content must write");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── MAIN-344 ────────────────────────────────────────────────────────────

    #[test]
    fn claude_config_dir_is_where_the_skills_go() {
        // The bug this pins: the operator node sets CLAUDE_CONFIG_DIR so the
        // fleet's identity is separate from anyone's (MAIN-238), and every
        // skill was being written to `~/.claude/skills` while the agent read
        // the relocated directory. Verified on the live dev operator before
        // being fixed here.
        let tmp = std::env::temp_dir().join(format!("nook344-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        let relocated = tmp.join("elsewhere");

        let targets = detect_in(&tmp, Some(&relocated));

        let claude = targets
            .iter()
            .find(|t| t.name == "Claude Code")
            .expect("claude detected");
        assert!(
            claude.roots.contains(&relocated.join("skills")),
            "the relocated config dir must be a root: {:?}",
            claude.roots
        );
        // And the default is KEPT, so unsetting the variable later does not
        // silently strand a machine with no skills.
        assert!(claude.roots.contains(&tmp.join(".claude/skills")));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_relocated_config_dir_counts_even_with_no_dot_claude() {
        // The operator container is exactly this shape: CLAUDE_CONFIG_DIR set,
        // and no `~/.claude` at all. Detection keyed only on the directory
        // would find no claude here and install nothing.
        let tmp = std::env::temp_dir().join(format!("nook344b-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let relocated = tmp.join("nook-claude");

        let targets = detect_in(&tmp, Some(&relocated));

        assert!(
            targets.iter().any(|t| t.name == "Claude Code"),
            "a relocated config dir is still Claude Code"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
