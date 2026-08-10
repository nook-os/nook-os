//! Holding an agent update back until this node's loop jobs finish (MAIN-505).
//!
//! A terminal session survives a restart because tmux is the buffer of record
//! and outlives this process. A streaming loop job does not: since MAIN-240 the
//! agent is spawned with piped stdio (`job_adapter`), so THIS process is the
//! buffer, and exiting under one is data loss rather than a reconnectable
//! interruption. So a version mismatch no longer means "update now" — it means
//! "stop taking work, then update when the machine is quiet".
//!
//! Deferring without cordoning never converges on a busy node: fresh work keeps
//! arriving and the quiet moment never comes. The two are one state here for
//! exactly that reason.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use nook_types::NodeCordon;

/// How long a deferral waits before it is called out, from
/// `NOOK_UPDATE_DRAIN_MINUTES`. Six hours: longer than any real build pass —
/// the streaming adapter imposes no timeout of its own — so reaching it means
/// something is genuinely wedged rather than merely slow.
pub const DEFAULT_DRAIN_MINUTES: u64 = 6 * 60;

/// What happens at the deadline: **nothing changes**, and that is the decision
/// (AC-4). Updating anyway would reintroduce the exact orphaning this exists to
/// prevent — later, and less predictably, because it would land on whichever
/// run happened to be slowest. So the node stays cordoned and says so loudly
/// instead, which an operator can act on: cancelling the stuck run
/// (`POST /api/v1/jobs/{id}/cancel`) makes the cordon converge on its own.
fn drain_deadline() -> Duration {
    parse_drain_minutes(&std::env::var("NOOK_UPDATE_DRAIN_MINUTES").unwrap_or_default())
}

/// Minutes from `NOOK_UPDATE_DRAIN_MINUTES`; junk falls back to the default
/// rather than to zero, because an instantly-overdue cordon would cry wolf on
/// every ordinary deploy.
pub fn parse_drain_minutes(raw: &str) -> Duration {
    let raw = raw.trim();
    let mins = if raw.is_empty() {
        DEFAULT_DRAIN_MINUTES
    } else {
        raw.parse().unwrap_or(DEFAULT_DRAIN_MINUTES)
    };
    Duration::from_secs(mins * 60)
}

/// An update this node has been told to install and is holding back.
#[derive(Debug, Clone)]
struct Deferral {
    /// The version the control plane expects — installed once the node is quiet.
    expected: String,
    /// When work stopped being accepted. Kept across a re-advertised version:
    /// a second deploy retargets the same deferral, it does not start a new one.
    since: DateTime<Utc>,
    jobs: u32,
    overdue: bool,
    /// The install has been started and this process expects to exit.
    ///
    /// The deferral is NOT dropped at that point, and that is the fix for the
    /// split state: `selfupdate::run` is non-fatal, so an install that fails on
    /// a download or a permission error leaves the node running. Clearing when
    /// the install STARTS would uncordon it locally while the control plane
    /// still had the cordon and nothing left to re-send it — the node would
    /// take no loop work until the socket happened to drop. So the lift is
    /// [`install_failed`]'s, and it is reported.
    installing: bool,
}

impl Deferral {
    fn cordon(&self) -> NodeCordon {
        if self.installing {
            return NodeCordon {
                reason: format!(
                    "installing the agent update to {} — this node restarts when it lands",
                    self.expected
                ),
                jobs_in_flight: 0,
                since: self.since,
                overdue: self.overdue,
                installing: true,
            };
        }
        let mut reason = format!(
            "deferring the agent update to {} until {} loop job{} finish{}",
            self.expected,
            self.jobs,
            if self.jobs == 1 { "" } else { "s" },
            if self.jobs == 1 { "es" } else { "" }
        );
        if self.overdue {
            reason.push_str(
                " — still blocked past the drain deadline. Cancel the run, or widen \
                 NOOK_UPDATE_DRAIN_MINUTES if this is normal here",
            );
        }
        NodeCordon {
            reason,
            jobs_in_flight: self.jobs,
            since: self.since,
            overdue: self.overdue,
            installing: false,
        }
    }
}

fn state() -> &'static Mutex<Option<Deferral>> {
    static S: OnceLock<Mutex<Option<Deferral>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// What this node is refusing new loop work for, if anything.
pub fn current() -> Option<NodeCordon> {
    state()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(Deferral::cordon))
}

/// Start (or retarget) a deferral, and describe the cordon it raises.
///
/// `jobs` is the count that made it necessary; the drain tick keeps it current.
pub fn defer_update(expected: &str, jobs: u32) -> NodeCordon {
    defer_at(Utc::now(), expected, jobs)
}

fn defer_at(now: DateTime<Utc>, expected: &str, jobs: u32) -> NodeCordon {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let since = guard.as_ref().map(|d| d.since).unwrap_or(now);
    let overdue = guard.as_ref().is_some_and(|d| d.overdue);
    let d = Deferral {
        expected: expected.to_string(),
        since,
        jobs,
        overdue,
        installing: false,
    };
    let cordon = d.cordon();
    *guard = Some(d);
    cordon
}

/// Drop any deferral — the control plane no longer expects a different version.
pub fn clear() {
    if let Ok(mut guard) = state().lock() {
        *guard = None;
    }
}

/// The install this node cordoned itself for did not happen, so lift the
/// cordon; `true` when there was one, meaning the caller must report the lift.
///
/// The only mid-connection lift there is. Every other clear rides a connect,
/// where `Register` re-asserts the state anyway — this one does not, which is
/// exactly why it has to say so rather than rely on the next reconnect.
pub fn install_failed() -> bool {
    state()
        .lock()
        .map(|mut guard| guard.take().is_some())
        .unwrap_or(false)
}

/// What one drain tick decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// No update is being held back here.
    Idle,
    /// Loop jobs are still running. `changed` is true when the cordon moved and
    /// the control plane should be told again.
    Waiting { cordon: NodeCordon, changed: bool },
    /// Nothing is holding it any more — install the update. Returned ONCE per
    /// deferral: the node stays cordoned across the install, so later ticks
    /// report `Waiting` rather than downloading the binary again every tick.
    Proceed { cordon: NodeCordon },
}

/// Reconsider a held-back update against what is running right now.
///
/// Called on a timer rather than off a job's own end, because the deadline
/// needs a clock anyway and a deferral must also survive the reconnect that a
/// control-plane deploy causes. The cost is a lock and a set length.
pub fn tick(in_flight: u32) -> Tick {
    tick_at(Utc::now(), in_flight, drain_deadline())
}

fn tick_at(now: DateTime<Utc>, in_flight: u32, deadline: Duration) -> Tick {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(d) = guard.as_mut() else {
        return Tick::Idle;
    };
    if d.installing {
        // Already installing: this process is expected to exit, and until it
        // does the cordon holds. Nothing to reconsider and nothing to re-report.
        return Tick::Waiting {
            cordon: d.cordon(),
            changed: false,
        };
    }
    if in_flight == 0 {
        d.jobs = 0;
        d.installing = true;
        return Tick::Proceed { cordon: d.cordon() };
    }
    let elapsed = (now - d.since).to_std().unwrap_or_default();
    let overdue = elapsed >= deadline;
    let changed = d.jobs != in_flight || d.overdue != overdue;
    d.jobs = in_flight;
    d.overdue = overdue;
    Tick::Waiting {
        cordon: d.cordon(),
        changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test drives the same process-global, so they take one lock rather
    /// than racing each other's `clear()`.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        let g = L
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear();
        g
    }

    const HOUR: Duration = Duration::from_secs(3600);

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("valid instant")
    }

    /// AC-6's first negative: an idle node has nothing to defer, so a tick on
    /// one that never deferred says so rather than inventing a cordon.
    #[test]
    fn an_idle_node_is_never_cordoned() {
        let _g = serial();
        assert_eq!(tick_at(at(0), 0, HOUR), Tick::Idle);
        assert_eq!(
            tick_at(at(0), 3, HOUR),
            Tick::Idle,
            "jobs alone cordon nothing"
        );
        assert!(current().is_none());
    }

    /// AC-1/AC-2: the deferral holds while work runs, and it is visible as a
    /// cordon with a reason naming the version and the count.
    #[test]
    fn a_busy_node_defers_and_says_why() {
        let _g = serial();
        let c = defer_at(at(0), "0.6.7", 2);
        assert!(c.reason.contains("0.6.7"), "{}", c.reason);
        assert!(c.reason.contains("2 loop jobs"), "{}", c.reason);
        assert_eq!(c.jobs_in_flight, 2);
        assert!(!c.overdue);

        let Tick::Waiting { cordon, changed } = tick_at(at(60), 2, HOUR) else {
            panic!("still holding two jobs");
        };
        assert!(!changed, "nothing moved, so nothing to re-report");
        assert_eq!(cordon.jobs_in_flight, 2);
    }

    /// AC-6's second negative, and the one that matters most: a cordon that
    /// never lifts is worse than no cordon. The jobs finish, and the next tick
    /// updates — no second `RegisterAck`, no operator.
    #[test]
    fn the_update_proceeds_once_the_last_job_ends() {
        let _g = serial();
        defer_at(at(0), "0.6.7", 2);
        assert!(matches!(tick_at(at(30), 1, HOUR), Tick::Waiting { .. }));
        let Tick::Proceed { cordon } = tick_at(at(90), 0, HOUR) else {
            panic!("nothing is running any more");
        };
        assert!(cordon.installing);
        assert_eq!(cordon.jobs_in_flight, 0);
        assert!(cordon.reason.contains("installing"), "{}", cordon.reason);
    }

    /// The install is the window this cordon must NOT open. Between the last run
    /// ending and the process exiting, a job accepted here would be orphaned by
    /// that exit — the very failure the card is about — so the node stays
    /// cordoned, and `Proceed` is returned once rather than every tick (which
    /// would re-download the binary every ten seconds).
    #[test]
    fn the_node_stays_cordoned_across_the_install() {
        let _g = serial();
        defer_at(at(0), "0.6.7", 1);
        assert!(matches!(tick_at(at(60), 0, HOUR), Tick::Proceed { .. }));
        assert!(
            current().is_some_and(|c| c.installing),
            "still refusing work while the binary is being replaced"
        );
        assert!(
            matches!(
                tick_at(at(70), 0, HOUR),
                Tick::Waiting { changed: false, .. }
            ),
            "installing already — never a second Proceed"
        );
    }

    /// The regression this pins: `selfupdate::run` is non-fatal, so an install
    /// that fails leaves the node running. Clearing when the install STARTED
    /// left the node locally uncordoned while the control plane still had the
    /// cordon and nothing left to re-send it — the machine took no loop work
    /// until its socket happened to drop. The lift is here, and it reports.
    #[test]
    fn a_failed_install_lifts_the_cordon_and_says_so() {
        let _g = serial();
        defer_at(at(0), "0.6.7", 1);
        assert!(matches!(tick_at(at(60), 0, HOUR), Tick::Proceed { .. }));

        assert!(
            install_failed(),
            "there was a cordon, so the caller reports it"
        );
        assert!(current().is_none(), "and the node takes work again");
        assert_eq!(
            tick_at(at(70), 0, HOUR),
            Tick::Idle,
            "the deferral is gone; a retry waits for the next RegisterAck"
        );
        assert!(
            !install_failed(),
            "nothing to lift twice — a second call must not ask for another report"
        );
    }

    /// A count that moves is worth re-reporting; one that does not is silence.
    #[test]
    fn only_a_moved_cordon_is_re_reported() {
        let _g = serial();
        defer_at(at(0), "0.6.7", 3);
        let Tick::Waiting { changed, cordon } = tick_at(at(10), 2, HOUR) else {
            panic!("two left");
        };
        assert!(changed);
        assert_eq!(cordon.jobs_in_flight, 2);
        assert!(matches!(
            tick_at(at(20), 2, HOUR),
            Tick::Waiting { changed: false, .. }
        ));
    }

    /// AC-4: past the deadline the node STAYS cordoned and escalates. The one
    /// thing it must not do is update — that is the orphaning this prevents.
    #[test]
    fn the_deadline_escalates_rather_than_updating_anyway() {
        let _g = serial();
        defer_at(at(0), "0.6.7", 1);
        let Tick::Waiting { cordon, changed } = tick_at(at(3600), 1, HOUR) else {
            panic!("a job is still running, so it must never proceed");
        };
        assert!(changed, "crossing the deadline is worth saying out loud");
        assert!(cordon.overdue);
        assert!(
            cordon.reason.contains("drain deadline"),
            "{}",
            cordon.reason
        );
        // And it stays that way rather than flapping.
        assert!(matches!(
            tick_at(at(7200), 1, HOUR),
            Tick::Waiting { changed: false, .. }
        ));
    }

    /// A second deploy retargets the deferral. It does not restart the clock —
    /// the cordon has been refusing work since the first one.
    #[test]
    fn a_re_advertised_version_keeps_the_original_start() {
        let _g = serial();
        defer_at(at(0), "0.6.7", 1);
        let c = defer_at(at(600), "0.6.8", 1);
        assert_eq!(c.since, at(0));
        assert!(c.reason.contains("0.6.8"), "{}", c.reason);
    }

    /// Junk must not disable the wait: a zero deadline would report every
    /// ordinary deploy as an escalation the moment it deferred.
    #[test]
    fn a_bad_deadline_falls_back_rather_than_expiring_at_once() {
        let default = Duration::from_secs(DEFAULT_DRAIN_MINUTES * 60);
        assert_eq!(parse_drain_minutes(""), default);
        assert_eq!(parse_drain_minutes("  "), default);
        assert_eq!(parse_drain_minutes("soon"), default);
        assert_eq!(parse_drain_minutes(" 90 "), Duration::from_secs(90 * 60));
    }
}
