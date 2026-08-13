// The Board page's Health tab (MAIN-570).
//
// Four exact rules over board state, each naming the cards that break it. The
// board can say one thing and behave as another — a card archived while still in
// Todo reads as live work but no listing shows it and the loop can never pick it
// up — and until now none of those states was queryable at all, so they were
// found by a human fetching cards one at a time.
//
// **Read-only, deliberately (NG-3).** Nothing here archives, unarchives, labels
// or closes. A non-zero row is a link into the Backlog tab filtered to exactly
// its cards, where the bulk toolbar already does the fixing.
import type { BoardHealth as HealthReport, BoardHealthCheckKind } from "@nookos/api";
import { Empty } from "@nookos/ui";

/// What each check is called, and what it means. Presentation only — the set and
/// its order come from the report itself, so the server stays the one definition
/// of which checks exist.
export const HEALTH_LABEL: Record<BoardHealthCheckKind, string> = {
  archived_not_done: "Archived while unfinished",
  done_agent_ready: "Done, still agent-ready",
  epics_closeable: "Epics ready to close",
  epics_empty: "Epics with no children",
};

export const HEALTH_WHY: Record<BoardHealthCheckKind, string> = {
  archived_not_done:
    "off every listing and unpickable by the loop — this work is simply lost",
  done_agent_ready: "finished, and still labelled ready for an agent to build",
  epics_closeable: "every child that is left sits in Done or Canceled",
  epics_empty: "nothing has ever been filed under them",
};

/// A `health` URL value, if it names a real check. Anything else — a typo, a
/// stale link, a check that has since been removed — is `null`, so a bad URL
/// reads as "no health filter" rather than as a backlog filtered to nothing.
/// Pure and unit-tested.
export function parseHealthCheck(value: string | null): BoardHealthCheckKind | null {
  // `hasOwnProperty`, not `in`: `in` walks the prototype chain, so `?health=
  // toString` would have parsed as a real check and filtered the backlog to
  // nothing while looking like it had worked.
  return value !== null && Object.prototype.hasOwnProperty.call(HEALTH_LABEL, value)
    ? (value as BoardHealthCheckKind)
    : null;
}

/// How many keys a row shows before it stops. A display cap only — the ids the
/// Backlog tab filters by are the whole list, never this slice — and the
/// remainder is counted out loud rather than dropped silently.
const KEYS_SHOWN = 12;

export function BoardHealthTab({
  report,
  onPick,
}: {
  report: HealthReport | undefined;
  /// Show this check's cards on the Backlog tab.
  onPick: (check: BoardHealthCheckKind) => void;
}) {
  if (!report) return <Empty>Loading…</Empty>;

  return (
    <div className="board-health">
      <div className="board-health-intro faint small">
        Board state that reads as one thing and behaves as another. Each check is
        an exact rule; pick one to see its cards in the Backlog.
      </div>
      {report.checks.map((c) => {
        const shown = c.tasks.slice(0, KEYS_SHOWN);
        const rest = c.tasks.length - shown.length;
        return (
          <button
            key={c.check}
            type="button"
            className={`board-health-row${c.count === 0 ? " zero" : ""}`}
            disabled={c.count === 0}
            onClick={() => onPick(c.check)}
            title={
              c.count === 0
                ? "nothing to look at"
                : "show these cards on the Backlog tab"
            }
          >
            <span className="board-health-count">{c.count}</span>
            <span className="board-health-text">
              <span className="board-health-label">{HEALTH_LABEL[c.check]}</span>
              <span className="board-health-why faint">{HEALTH_WHY[c.check]}</span>
            </span>
            <span className="board-health-keys">
              {/* A clean check still gets a row, so an all-zero board reads as
                  healthy rather than as a page that failed to render (AC-7). */}
              {c.count === 0 ? (
                <span className="faint">none</span>
              ) : (
                <>
                  {shown.map((t) => (
                    <span key={t.id} className="task-chip">
                      {t.key ?? t.id.slice(0, 8)}
                    </span>
                  ))}
                  {rest > 0 && <span className="faint">+{rest} more</span>}
                </>
              )}
            </span>
          </button>
        );
      })}
    </div>
  );
}
