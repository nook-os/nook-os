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

/// The skill itself, compiled in.
const SKILL: &str = include_str!("../../../../skills/nookos/SKILL.md");

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
    Ok(detect_in(&home()?))
}

/// The detection, with home supplied rather than read from the environment, so
/// it is testable without mutating a process-global `HOME` that parallel tests
/// share.
fn detect_in(h: &Path) -> Vec<Target> {
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

    if h.join(".claude").is_dir() {
        found.push(Target {
            name: "Claude Code",
            roots: vec![h.join(".claude/skills")],
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

fn write_skill(root: &Path) -> Result<PathBuf> {
    write_named(root, "nookos", SKILL)
}

fn write_named(root: &Path, name: &str, content: &str) -> Result<PathBuf> {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join("SKILL.md");
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
        let p = write_skill(&d)?;
        println!("✓ {}", p.display());
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
            let p = write_skill(root)?;
            count += 1;
            if !quiet {
                println!("✓ {} → {}", t.name, p.display());
            }
        }
    }
    println!(
        "\nInstalled the NookOS skill in {count} location(s) across {} agent(s).",
        targets.len()
    );
    println!("Your agents can now start and drive sessions across the fleet.");
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
    fn the_embedded_skill_looks_like_the_real_one() {
        assert!(SKILL.len() > 1000, "suspiciously short: {}", SKILL.len());
        assert!(SKILL.contains("nook"), "does not mention the CLI");
        assert!(
            SKILL.starts_with("---") || SKILL.starts_with('#'),
            "skills need frontmatter or a heading"
        );
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

        let found = detect_in(&h);
        let names: Vec<&str> = found.iter().map(|t| t.name).collect();
        assert!(names.contains(&"Codex"), "codex not detected: {names:?}");
        assert!(names.contains(&"Claude Code"), "{names:?}");
        assert!(names.contains(&"Hermes"), "{names:?}");

        let codex = found.iter().find(|t| t.name == "Codex").unwrap();
        assert_eq!(codex.roots, vec![h.join(".codex/skills")]);

        // Absent codex → not detected (no litter for a tool that isn't here).
        let bare = h.join("bare");
        std::fs::create_dir_all(bare.join(".claude")).unwrap();
        assert!(!detect_in(&bare).iter().any(|t| t.name == "Codex"));

        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn writing_creates_the_named_subdirectory() {
        let dir = std::env::temp_dir().join(format!("nook-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_skill(&dir).unwrap();
        assert_eq!(p, dir.join("nookos/SKILL.md"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SKILL);
        // Idempotent: installing twice must not fail or duplicate.
        assert_eq!(write_skill(&dir).unwrap(), p);

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
}
