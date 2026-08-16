//! The files a from-scratch project is created with (MAIN-619).
//!
//! Here, and not beside the code that writes them, because two crates have to
//! agree on the same bytes and share only this one: nook-node writes them in
//! `gitops::init_project`, and the control plane's `repo_settings::parse` is
//! what has to accept the `.nook.toml` we generate. A copy in each would let us
//! ship a scaffold our own parser rejects.

/// AC-4. Declares the zero-config default as a real declaration, which is what
/// lifts the one-session-per-node cap an undeclared workspace lives under.
pub const NOOK_TOML: &str = r#"# What this repo tells nook about itself.
#
# `[[ports]]` declares the listeners a session in this repo needs. Nook leases
# a free number per entry from the node's range and exports it as `env`, so N
# checkouts run side by side instead of fighting over one port. An app binds
# `$NOOK_PORT` — never a literal.
#
# Unknown sections are ignored, so `[worktree]` and `[sandbox]` can be added
# later without touching this one.

[[ports]]
name = "web"
env = "NOOK_PORT"
protocol = "tcp"
required = false
browsable = true
"#;

/// AC-6. Language-neutral on purpose: picking the stack is the human's, so this
/// covers the four ecosystems' build output and the secrets file every one of
/// them has.
pub const GITIGNORE: &str = ".env\n.env.local\n.DS_Store\nnode_modules/\ntarget/\ndist/\n";

/// AC-7. The one-line title it has always been, with the typed description as
/// its body when there is one — never an empty heading.
pub fn readme_md(name: &str, description: Option<&str>) -> String {
    match trimmed(description) {
        Some(d) => format!("# {name}\n\n{d}\n"),
        None => format!("# {name}\n"),
    }
}

/// AC-5. Fixed template, `{name}` substituted, plus AC-7's optional section.
pub fn claude_md(name: &str, description: Option<&str>) -> String {
    let about = match trimmed(description) {
        Some(d) => format!("\n## What this project is\n\n{d}\n"),
        None => String::new(),
    };
    format!(
        "# {name}\n\
         {about}\n\
         ## Ports\n\
         \n\
         Bind `$NOOK_PORT` — never a hardcoded literal. Every listener this repo needs\n\
         is declared in `.nook.toml`, and nook leases a free number per listener per\n\
         session, so two checkouts run side by side.\n\
         \n\
         ## This repo\n\
         \n\
         Created from scratch in nook. It is local-only — there is no git remote, so\n\
         there is nothing to push and no PRs to open. Add a remote when you want one.\n\
         \n\
         ## Working here\n\
         \n\
         - Small, focused commits.\n\
         - Run the tests before calling something done.\n\
         - Write down what the code cannot say; skip comments that restate the line below.\n"
    )
}

/// Whitespace-only is the same as absent: a description field somebody tabbed
/// through must not produce a heading with nothing under it (AC-7).
fn trimmed(description: Option<&str>) -> Option<&str> {
    description.map(str::trim).filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_becomes_a_section_and_a_readme_body() {
        let claude = claude_md("greeting-lab", Some("A place to try greetings."));
        assert!(claude.starts_with(
            "# greeting-lab\n\n## What this project is\n\nA place to try greetings.\n\n## Ports\n"
        ));
        assert_eq!(
            readme_md("greeting-lab", Some("A place to try greetings.")),
            "# greeting-lab\n\nA place to try greetings.\n"
        );
    }

    #[test]
    fn no_description_leaves_no_empty_heading() {
        let claude = claude_md("greeting-lab", None);
        assert!(!claude.contains("What this project is"), "{claude}");
        assert!(
            claude.starts_with("# greeting-lab\n\n## Ports\n"),
            "{claude}"
        );
        assert_eq!(readme_md("greeting-lab", None), "# greeting-lab\n");
    }

    /// A field somebody tabbed through carries spaces, and a heading with
    /// nothing under it is exactly what AC-7 forbids.
    #[test]
    fn a_blank_description_reads_as_absent() {
        assert!(!claude_md("x", Some("   \n ")).contains("What this project is"));
        assert_eq!(readme_md("x", Some("  ")), "# x\n");
    }

    #[test]
    fn the_template_is_the_one_the_card_fixed() {
        let claude = claude_md("greeting-lab", None);
        assert!(claude.contains("Bind `$NOOK_PORT` — never a hardcoded literal."));
        assert!(claude.contains("It is local-only — there is no git remote, so"));
        assert!(claude.ends_with("skip comments that restate the line below.\n"));
        assert!(NOOK_TOML.contains("env = \"NOOK_PORT\""));
        assert!(NOOK_TOML.contains("browsable = true"));
        for line in [
            ".env",
            ".env.local",
            ".DS_Store",
            "node_modules/",
            "target/",
            "dist/",
        ] {
            assert!(GITIGNORE.lines().any(|l| l == line), "{line} missing");
        }
    }
}
