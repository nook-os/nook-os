// The runs list's designed states (MAIN-560 AC-5) — every one of them together,
// because what they have to be is DIFFERENT from each other.
//
// All five used to render as an empty list, which reads as one sentence: "no
// runs exist". "Nothing matched what you typed", "nothing matched the filters
// you set", "the request failed" and "the run you had open is gone" are four
// other things, and a reader who cannot tell them apart draws the wrong
// conclusion about their repo every time.
//
// Presentational and pure: the phase is decided by `runsPhase` from values a
// test can hand it, and every way out is a callback. Nothing here fetches.
import React from "react";
import { Empty } from "@nookos/ui";

/** One narrowing that is currently on, named for a reader (AC-6) — in the
 *  words the chip row uses (MAIN-558), so the sentence explaining an empty list
 *  and the controls above it cannot describe the same filter differently. */
export type RunFilterChip = { key: string; label: string };

/** Which of the list's states is showing. `rows` is the ordinary one. */
export type RunsPhase = "loading" | "error" | "empty" | "no-search" | "no-filters" | "rows";

/**
 * The state the list is in, from what it knows about itself.
 *
 * Rows win as soon as there are any — the same rule `dataListPhase` uses — so
 * appending a page never flashes a state over a list somebody is reading, and a
 * background refetch that fails leaves the rows it already has rather than
 * replacing them with an error. A failed load with nothing to show is the
 * error; a failed load with rows is the sentinel's business at the bottom.
 *
 * Search outranks filters when both are on and nothing matched. It has to
 * outrank something — the two states are deliberately distinct — and the search
 * is what a reader was typing into a moment ago, so it is the narrowing they
 * are holding in mind. The state names the filters as well, so the answer is
 * never hidden behind clearing the search first.
 */
export function runsPhase(o: {
  /** The FIRST page is in flight. */
  loading: boolean;
  error: boolean;
  /** Rows after every narrowing — what is actually on screen. */
  shown: number;
  search: string;
  filters: RunFilterChip[];
}): RunsPhase {
  if (o.shown > 0) return "rows";
  if (o.error) return "error";
  if (o.loading) return "loading";
  if (o.search.trim()) return "no-search";
  if (o.filters.length > 0) return "no-filters";
  return "empty";
}

/** The filters, as one phrase a sentence can end with. */
export function filtersSentence(filters: RunFilterChip[]): string {
  return filters.map((f) => f.label).join(", ");
}

/**
 * Whatever the list has instead of rows.
 *
 * One component rather than five exports: these states exist to be told apart,
 * and the way to keep their words distinct is to have them written next to each
 * other.
 */
export function RunsState({
  phase,
  search,
  filters,
  error,
  onRetry,
  onClearSearch,
  onClearFilters,
}: {
  /** `rows` renders nothing: the list itself is what that phase looks like. */
  phase: RunsPhase;
  search: string;
  filters: RunFilterChip[];
  /** What the server said, when it said anything (AC-5's failed state). */
  error?: string | null;
  onRetry(): void;
  onClearSearch(): void;
  onClearFilters(): void;
}) {
  if (phase === "rows") return null;

  if (phase === "loading") {
    return (
      <Empty>
        <span data-testid="runs-loading">Loading runs…</span>
      </Empty>
    );
  }

  if (phase === "error") {
    return (
      <Empty>
        <div className="runs-state" data-testid="runs-load-failed">
          <p className="runs-state-line">The runs could not be loaded.</p>
          {/* The server's own sentence, as everywhere else in this panel
              (MAIN-559 AC-4) — "something went wrong" would throw away the one
              part of this a reader can act on. */}
          {error ? <p className="runs-state-detail">{error}</p> : null}
          {/* Explicit and user-driven (NG-4): nothing here retries on its own,
              because a control plane that is down does not want a browser
              asking again every second until the tab is closed. */}
          <button type="button" className="btn small" data-testid="runs-retry" onClick={onRetry}>
            try again
          </button>
        </div>
      </Empty>
    );
  }

  if (phase === "no-search") {
    return (
      <Empty>
        <div className="runs-state" data-testid="runs-no-search">
          <p className="runs-state-line">
            No run matches <span className="mono bright">{search.trim()}</span>.
          </p>
          {filters.length > 0 ? (
            <p className="runs-state-detail" data-testid="runs-no-search-filters">
              Filters are narrowing this list too: {filtersSentence(filters)}.
            </p>
          ) : null}
          <div className="runs-state-actions">
            <button
              type="button"
              className="btn small"
              data-testid="runs-clear-search"
              onClick={onClearSearch}
            >
              clear search
            </button>
            {filters.length > 0 ? (
              <button
                type="button"
                className="btn small"
                data-testid="runs-clear-filters"
                onClick={onClearFilters}
              >
                clear filters
              </button>
            ) : null}
          </div>
        </div>
      </Empty>
    );
  }

  if (phase === "no-filters") {
    return (
      <Empty>
        <div className="runs-state" data-testid="runs-no-filters">
          {/* WHICH filters, by name (AC-6). "No run matches this filter" was
              true and useless: it left a reader to work out which of the
              controls above was responsible, and at a glance it reads the same
              as a repo that has never run anything. */}
          <p className="runs-state-line">
            This repo has runs. None match these filters: {filtersSentence(filters)}.
          </p>
          <div className="runs-state-actions">
            <button
              type="button"
              className="btn small"
              data-testid="runs-clear-filters"
              onClick={onClearFilters}
            >
              clear filters
            </button>
          </div>
        </div>
      </Empty>
    );
  }

  return (
    <Empty>
      <span data-testid="runs-empty">
        No run has happened in this repo yet. The control plane raises a review per open
        pull request — and again when one is pushed to — and a build when a card is enqueued.
      </span>
    </Empty>
  );
}

/**
 * The fifth state, and the only one that is not about the list (AC-5).
 *
 * `?run=` names a run this repo cannot show — deleted, or belonging to somebody
 * whose runs this reader may not see. It renders where the transcript would be,
 * because that is the pane whose blankness was the lie: a shared link to a run
 * that has since gone used to open on an empty reading pane beside a perfectly
 * healthy list.
 */
export function RunGone({ onShowNewest }: { onShowNewest(): void }) {
  return (
    <Empty>
      <div className="runs-state" data-testid="run-gone">
        <p className="runs-state-line">That run is gone, or you are not allowed to see it.</p>
        <div className="runs-state-actions">
          <button
            type="button"
            className="btn small"
            data-testid="run-gone-newest"
            onClick={onShowNewest}
          >
            show the newest run
          </button>
        </div>
      </div>
    </Empty>
  );
}
