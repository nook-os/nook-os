//! `nook skills install` — teach an agent to drive the fleet.
//!
//! The skill is embedded rather than read from the repo, because the whole
//! point is that nobody had to clone anything. `skills/install.sh` needed the
//! working tree; this needs the binary that is already on the machine.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The skill itself, compiled in.
const SKILL: &str = include_str!("../../../../skills/nookos/SKILL.md");

/// Where a given agent keeps its skills.
struct Target {
    name: &'static str,
    /// Directories to write `nookos/SKILL.md` under, relative to $HOME.
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
    let h = home()?;
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

    Ok(found)
}

fn write_skill(root: &Path) -> Result<PathBuf> {
    let dir = root.join("nookos");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
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
        println!("Looked for ~/.hermes, ~/.claude and ~/.openclaw. If your agent keeps");
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

    #[test]
    fn writing_creates_the_nookos_subdirectory() {
        let dir = std::env::temp_dir().join(format!("nook-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_skill(&dir).unwrap();
        assert_eq!(p, dir.join("nookos/SKILL.md"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SKILL);
        // Idempotent: installing twice must not fail or duplicate.
        assert_eq!(write_skill(&dir).unwrap(), p);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
