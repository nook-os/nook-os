//! MAIN-265: the visibility rule is written once, and stays that way.
//!
//! MAIN-261's `visibility_agreement` proves the SQL sites agree with the Rust
//! oracle. That is a comparison of the sites it KNOWS about — it cannot notice a
//! fifth query added next month with a fresh hand-written predicate, because
//! nothing would drive that query through the matrix. Agreement guards drift in
//! the copies; it does not guard against a new copy appearing.
//!
//! So this file guards the shape instead: `visibility <> 'private'` may appear in
//! exactly ONE source file, `services/tasks.rs`, which holds the two definitions
//! every query is expected to call —
//!
//!   - [`visible_sql`]      — given a viewer, what may they see?
//!   - [`public_only_sql`]  — no viewer at all; drop every private card.
//!
//! Paste the predicate into a new query and this test names the file. It is a
//! grep, deliberately: the property AC-2 asks for is literally "grep-provable",
//! and a test that greps is the only kind that keeps being true after the person
//! who wrote it has gone.
//!
//! No database. Reads the source tree, so it runs on every engine and in any
//! environment.

use std::path::{Path, PathBuf};

/// The one file allowed to spell the predicate out.
const HOME: &str = "services/tasks.rs";

/// The literal forms of "is not private" a query could carry. `!=` is included
/// even though the codebase writes `<>` — a copy typed the other way is exactly
/// the copy that would slip past a check looking only for the house style.
const SPELLINGS: [&str; 2] = ["visibility <> 'private'", "visibility != 'private'"];

fn src_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_visibility_predicate_is_written_in_exactly_one_file() {
    let root = src_root();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(files.len() > 10, "expected a source tree, found {files:?}");

    let mut offenders = Vec::new();
    let mut found_at_home = false;

    for path in &files {
        let body = std::fs::read_to_string(path).expect("read source file");
        if !SPELLINGS.iter().any(|s| body.contains(s)) {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .expect("path under src")
            .to_string_lossy()
            .replace('\\', "/");
        if rel == HOME {
            found_at_home = true;
        } else {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "the task-visibility predicate is hand-written outside {HOME}: {offenders:?}\n\
         Call `services::tasks::visible_sql(alias, viewer)` instead — or \
         `public_only_sql(alias)` if the query has no viewer to scope by. A copy \
         is what MAIN-265 removed, and a copy that drifts leaks a private card \
         to a stranger without ever failing loudly."
    );
    // If the definitions themselves ever move or are renamed away, this test
    // must not keep passing by proving nothing.
    assert!(
        found_at_home,
        "no visibility predicate found in {HOME} — the definitions moved, and \
         this guard is now asserting the absence of something that no longer \
         exists anywhere. Point HOME at wherever `visible_sql` lives."
    );
}

/// The other half of "one definition": the four call sites actually call it.
///
/// Without this, deleting `AND {visible}` from a query would leave the grep above
/// perfectly happy — the predicate is still written once, in a helper nobody
/// uses — while that query silently returned private cards to everyone.
/// `visibility_agreement` would catch it for the sites it drives; this catches
/// the wiring directly, and names the site.
#[test]
fn every_known_site_calls_the_shared_definition() {
    let root = src_root();
    let repo = std::fs::read_to_string(root.join("repo/tasks.rs")).expect("repo/tasks.rs");
    // The overview join moved to the cross-cutting read model (MAIN-304); the
    // query — and its call to the shared predicate — went with it.
    let overview =
        std::fs::read_to_string(root.join("repo/read_model.rs")).expect("repo/read_model.rs");

    // `visible_sql` is called once per viewer-scoped site: pick_tasks,
    // epic_children, related_tasks and board_health in the repository, and the
    // overview join.
    let repo_calls = repo.matches("visible_sql(").count();
    assert_eq!(
        repo_calls, 4,
        "expected pick_tasks, epic_children, related_tasks and board_health to \
         call visible_sql — found {repo_calls} call(s) in repo/tasks.rs"
    );
    assert!(
        overview.contains("visible_sql("),
        "the Mission Control overview join no longer calls visible_sql — if that \
         query moved again, point this at its new home rather than deleting the \
         assertion"
    );
    assert!(
        repo.contains("public_only_sql("),
        "operator_visible_titles no longer calls public_only_sql"
    );
}
