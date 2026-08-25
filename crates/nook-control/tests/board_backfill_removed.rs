//! MAIN-640: the one-shot workspace-board backfill stays deleted.
//!
//! It was written in Rust rather than as a SQL migration precisely so it could
//! be removed once every environment had booted it at least once — an applied
//! migration cannot be, because a checkout missing an already-applied version
//! is tolerated in dev and fatal in production. Removal was the plan; this is
//! what stops the plan being undone by a stale reference.
//!
//! The cross-tenant reads it needed went with it, so a resurrected call fails
//! to COMPILE — the strongest form of this check. This grep is the other half,
//! for a catch-up rebuilt from scratch, which would compile perfectly well and
//! quietly put the boot-time query back.
//!
//! **What it does NOT cover: prose.** The needles are call sites and the exact
//! log text, deliberately — a bare `backfill` needle would hit dozens of
//! unrelated migration comments and be deleted by the first person it
//! inconvenienced. So a comment still describing the removed mechanism passes
//! this test cleanly (two did, and the MAIN-640 review found them by reading).
//! Do not read a green run here as "nothing in the tree mentions the backfill".
//!
//! No database. Reads the source tree, so it runs on every engine and in any
//! environment.

use std::path::{Path, PathBuf};

/// Spellings the removed path would come back under. The two repository reads
/// existed for the backfill alone and take no tenant, so either name
/// reappearing means the unscoped boot query is back whatever its caller is
/// called.
const REMOVED: [&str; 4] = [
    "boards::backfill",
    "boards_across_tenants",
    "workspaces_across_tenants",
    "board backfill",
];

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
fn nothing_in_the_control_plane_reaches_for_the_backfill() {
    let root = src_root();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(files.len() > 10, "expected a source tree, found {files:?}");

    let mut offenders = Vec::new();
    for path in &files {
        let body = std::fs::read_to_string(path).expect("read source file");
        let rel = path
            .strip_prefix(&root)
            .expect("path under src")
            .to_string_lossy()
            .replace('\\', "/");
        for needle in REMOVED {
            if body.contains(needle) {
                offenders.push(format!("{rel}: {needle}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the workspace-board backfill is back: {offenders:?}\n\
         MAIN-640 removed it because it is a one-shot catch-up every environment \
         has already run, and every boot after that was a pointless cross-tenant \
         query. Board creation on workspace creation is the ongoing mechanism — \
         `services::boards::create_with_columns`, called from POST /workspaces."
    );
}

/// AC-5 directly: boot no longer emits the backfill's log line, which is the
/// only thing an operator could have observed it by. Asserted against `main.rs`
/// alone rather than folded into the sweep above, because it is the boot
/// sequence specifically that must have nothing left to say here.
#[test]
fn boot_logs_no_backfill_line() {
    let main = std::fs::read_to_string(src_root().join("main.rs")).expect("main.rs");
    for line in [
        "board backfill created missing workspace boards",
        "board backfill failed",
        "board backfill SKIPPED",
    ] {
        assert!(
            !main.contains(line),
            "the control plane's boot sequence logs {line:?} again — MAIN-640 \
             removed the backfill, and that line is how an operator could tell \
             it had run"
        );
    }
}
