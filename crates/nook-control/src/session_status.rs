//! What a session's status MEANS, in one place (MAIN-415).
//!
//! There were twenty-four copies of `('starting', 'running', 'detached')` in
//! this crate — some SQL, some `matches!`, all spelling out the same idea by
//! hand. That was survivable while there was only one idea to spell. Adding
//! `stopped` ends that: a stopped session **satisfies a workspace declaration**
//! (so the reconciler must not replace it) and **does not occupy the machine**
//! (so it holds no ports and counts against no capacity). Those are opposite
//! answers from the same list, and a copy updated by reflex gets one of them
//! silently wrong — the reconciler quietly restarting a session you stopped, or
//! a node's capacity permanently over-counted.
//!
//! So there are two predicates here and never a bare list at a call site. The
//! name at the call site says which question is being asked, which is the part
//! a reviewer can actually check.

/// Occupying the machine RIGHT NOW: a tmux session exists, ports are held, a
/// terminal can attach. `stopped` is deliberately absent — that is the whole
/// point of stopping one.
pub const LIVE: [&str; 3] = ["starting", "running", "detached"];

/// Satisfying a workspace's session declaration: the row is what the workspace
/// asked for, whether or not it is running at this moment. `stopped` counts,
/// because a session you stopped on purpose is still the session the
/// declaration asked for — and if it did not count, the reconciler would start
/// a replacement within a poll interval and Stop would silently undo itself.
pub const DECLARED: [&str; 4] = ["starting", "running", "detached", "stopped"];

/// Ended and not resumable by opening it: it died, or it was never able to run.
pub const DEAD: [&str; 2] = ["exited", "error"];

/// Intentional, resumable, costing nothing. Distinct from [`DEAD`] so a UI can
/// say which happened (AC-6): `exited` is "it died", `stopped` is "you stopped
/// it, open it to get it back".
pub const STOPPED: &str = "stopped";

pub fn is_live(status: &str) -> bool {
    LIVE.contains(&status)
}

pub fn is_declared(status: &str) -> bool {
    DECLARED.contains(&status)
}

pub fn is_stopped(status: &str) -> bool {
    status == STOPPED
}

/// `'starting', 'running', 'detached'` — the body of an `IN (…)` list, as a
/// MACRO so it also works where the SQL must stay a `&'static str`.
///
/// Some call sites `format!` their SQL already and can take [`LIVE_SQL`]; others
/// hand a `&'static str` to a struct field and cannot. A macro expands inside
/// `concat!`, so both kinds of site name the same one definition instead of the
/// static ones keeping a private copy — which is the whole point of this module.
#[macro_export]
macro_rules! live_sql {
    () => {
        "'starting', 'running', 'detached'"
    };
}

/// The [`DECLARED`] set, same reasoning as [`live_sql!`].
#[macro_export]
macro_rules! declared_sql {
    () => {
        "'starting', 'running', 'detached', 'stopped'"
    };
}

/// `'starting', 'running', 'detached'` for the `format!`-ing call sites.
pub const LIVE_SQL: &str = crate::live_sql!();

/// `'starting', 'running', 'detached', 'stopped'` — see [`DECLARED`].
pub const DECLARED_SQL: &str = crate::declared_sql!();

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL and the Rust list are two spellings of one set, and nothing else
    /// checks that they agree — a stale `LIVE_SQL` would put the database and
    /// the code on different definitions of "live", which is exactly the class
    /// of bug this module exists to remove.
    fn sql_set(sql: &str) -> Vec<String> {
        sql.split(',')
            .map(|s| s.trim().trim_matches('\'').to_string())
            .collect()
    }

    #[test]
    fn the_sql_lists_say_what_the_rust_lists_say() {
        assert_eq!(sql_set(LIVE_SQL), LIVE.to_vec());
        assert_eq!(sql_set(DECLARED_SQL), DECLARED.to_vec());
    }

    #[test]
    fn stopped_satisfies_a_declaration_but_does_not_occupy_the_machine() {
        assert!(!is_live(STOPPED), "a stopped session holds nothing");
        assert!(is_declared(STOPPED), "…but it is still what was asked for");
    }

    #[test]
    fn stopped_is_not_dead() {
        // AC-6. If these ever collapse into one bucket, "it crashed" and "you
        // stopped it" become the same sentence on screen.
        assert!(!DEAD.contains(&STOPPED));
        assert!(!is_declared("exited") && !is_declared("error"));
    }

    #[test]
    fn every_live_status_also_satisfies_a_declaration() {
        for s in LIVE {
            assert!(is_declared(s), "{s} is live but does not count as declared");
        }
    }
}
