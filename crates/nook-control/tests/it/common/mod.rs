//! Fixtures shared across the suites in this binary.
//!
//! Compiled ONCE now that there is one integration-test binary (MAIN-657), so
//! anything unused here is dead code for the whole crate — hence the `allow`
//! inside each submodule rather than a `#[cfg]` maze over the items.
pub mod build_ports;

/// The one lock every `set_var`/`remove_var` in this binary takes (MAIN-657).
///
/// The environment is process-global and the suites now share a process, so a
/// per-file `Mutex` isolates nothing: `runtime_auth_codex` clearing the codex
/// variables to assert a descriptor is absent runs concurrently with
/// `runtime_auth_sessionless` writing the claude ones and `workspace_gh_token`
/// pointing the API base at its stub. Hold this across the mutation AND across
/// whatever has to observe it.
///
/// Poisoning is recovered from rather than propagated: a panic in one suite's
/// assertions must fail that suite, not every other test that touches the
/// environment afterwards.
pub fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
