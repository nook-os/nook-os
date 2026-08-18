//! Source-inspecting guards, in the shape `nook-node`'s `sandbox::guards` set.
//!
//! Two claims here are about the SHAPE of the tree rather than about behaviour
//! any runtime test can observe, so they are checked by reading files:
//!
//! - AC-4: the control plane must never gain a Kubernetes client. A test that
//!   exercised the control plane would pass perfectly well the day somebody
//!   added the dependency — the failure is that the permission exists at all,
//!   not that anything misbehaves.
//! - AC-5: the dependency stays confined to this crate, so what the control
//!   plane, the web build and the desktop bundle compile is unchanged.
//!
//! Both read the manifests, because a manifest is where the decision is made.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// The crates whose presence in a manifest means "this thing talks to
/// Kubernetes", including the wrapper this crate is.
const KUBERNETES_CRATES: &[&str] = &[
    "kube",
    "kube-client",
    "kube-core",
    "kube-runtime",
    "k8s-openapi",
    "nook-k8s",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> sits two levels under the workspace root")
        .to_path_buf()
}

fn manifest(crate_name: &str) -> String {
    let path = workspace_root()
        .join("crates")
        .join(crate_name)
        .join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Which in-workspace crates a manifest names as dependencies. Line-based
/// rather than a TOML parse, because every dependency in this tree is written
/// `name = ...` on its own line and a parser would be a second dependency for
/// nothing.
fn nook_dependencies(manifest: &str) -> Vec<String> {
    manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split('=').next())
        .map(str::trim)
        .filter(|name| name.starts_with("nook-"))
        .map(str::to_string)
        .collect()
}

/// Every workspace crate the named one compiles, transitively.
fn compile_closure(root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([root.to_string()]);
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        for dep in nook_dependencies(&manifest(&name)) {
            queue.push_back(dep);
        }
    }
    seen
}

/// AC-4. The control plane holds every tenant's database and every node's
/// credential; a Kubernetes permission there is a blast radius nobody asked
/// for. Executors reach clusters, the control plane places work.
#[test]
fn the_control_plane_never_gains_a_kubernetes_client() {
    let closure = compile_closure("nook-control");
    assert!(
        closure.contains("nook-db") && closure.len() > 3,
        "the closure walk found only {closure:?} — it is reading the manifest \
         wrongly and is asserting nothing"
    );
    for crate_name in &closure {
        let manifest = manifest(crate_name);
        for kube in KUBERNETES_CRATES {
            assert!(
                !names_dependency(&manifest, kube),
                "{crate_name} depends on `{kube}`, and the control plane \
                 compiles {crate_name} — MAIN-339 AC-4: the Kubernetes client \
                 lives in an executor, never in the control plane. An executor \
                 reaching a cluster is scoped to its own namespace by RBAC; the \
                 control plane holds every tenant's data, and a cluster \
                 permission there is a blast radius nobody asked for."
            );
        }
    }
}

/// AC-5. One manifest names the Kubernetes crates, so adding them changed
/// nothing the control plane, the web build or the desktop bundle compiles.
///
/// The next crate to name them will be `nook-node`, when MAIN-623 grows the Pod
/// driver — and that DOES change the desktop bundle, which ships the `nook`
/// binary. This guard is where that trade gets looked at rather than absorbed
/// silently.
#[test]
fn only_this_crate_names_the_kubernetes_dependency() {
    let crates = workspace_root().join("crates");
    let entries =
        std::fs::read_dir(&crates).unwrap_or_else(|e| panic!("{}: {e}", crates.display()));
    let mut checked = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "nook-k8s" {
            continue;
        }
        let path = entry.path().join("Cargo.toml");
        let Ok(manifest) = std::fs::read_to_string(&path) else {
            continue;
        };
        checked += 1;
        for kube in KUBERNETES_CRATES {
            assert!(
                !names_dependency(&manifest, kube),
                "{name} depends on `{kube}`. MAIN-339 AC-5 confined the \
                 Kubernetes client to nook-k8s so that adding it changed \
                 nothing the control plane, the web build or the desktop \
                 bundle compiles. The desktop bundle builds `nook` and \
                 `nook-control`: if this crate is one of them, that bundle now \
                 carries k8s-openapi. Widen this guard deliberately, with the \
                 card that needs it."
            );
        }
    }
    assert!(
        checked > 10,
        "the guard read {checked} manifests — it is looking in the wrong place"
    );
}

/// The manifest names `dep` as a dependency of its own, rather than merely
/// mentioning it in prose. Comment lines are skipped and the name must be the
/// whole key, so `kube` does not match `kubernetes-something`.
fn names_dependency(manifest: &str, dep: &str) -> bool {
    manifest.lines().any(|line| {
        let line = line.trim();
        !line.starts_with('#')
            && line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == dep || key.trim() == format!("\"{dep}\""))
    })
}

/// AC-3's other half: "never a panic". Every failure in this crate is a typed
/// error, and the way that stays true is that the source contains no shortcut
/// that could panic instead — an `unwrap`, an `expect`, a slice index.
///
/// Test code is exempt and stripped: a test asserting a fixture loaded SHOULD
/// panic, that is what a failing assertion is.
#[test]
fn no_panicking_shortcuts_outside_tests() {
    for file in ["config.rs", "error.rs", "pods.rs", "lib.rs"] {
        let src = shipped_source(file);
        for shortcut in [
            ".unwrap()",
            ".expect(",
            "panic!(",
            "unreachable!(",
            "todo!(",
        ] {
            assert!(
                !src.contains(shortcut),
                "{file} uses `{shortcut}` outside a test. MAIN-339 AC-3: a \
                 Kubernetes failure reaches the caller as a typed error, never \
                 as a panic — an executor that panics on a 403 takes its whole \
                 agent down instead of refusing one job."
            );
        }
    }
}

/// A source file with its `#[cfg(test)]` tail removed, `sandbox::guards`' own
/// `source` helper.
fn shipped_source(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    let whole =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    match whole.find("\n#[cfg(test)]") {
        Some(i) => whole[..i].to_string(),
        None => whole,
    }
}
