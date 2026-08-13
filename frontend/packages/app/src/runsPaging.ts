// Paging a list that is TWO cursor walks merged (MAIN-560 AC-1/AC-3).
//
// The runs list is one list on screen and two endpoints on the wire (MAIN-488),
// each paged on its own keyset cursor (MAIN-557). Sorting both fetched sets
// together is not enough to render them: the oldest build fetched and the
// oldest review fetched sit at different instants, and every row older than the
// NEWER of those two may still have unfetched neighbours from the other walk.
// Rendering them anyway is what makes the next page insert rows ABOVE rows
// already on screen — the reordering AC-3 forbids, and the reason this is not
// simply `mergeRuns(everything fetched)`.
//
// So the merged list is cut at that FRONTIER, and "load more" advances whichever
// walk is holding it back. Rows past the frontier are already in hand and appear
// the moment it moves, so nothing is refetched to reveal them.
//
// Pure, and free of both react-query and the row shape: the merge itself stays
// `mergeRuns`, and this module only says how far down it may be trusted.

/** All this module needs of a row: its place in the one order the list has. */
export type RunOrder = { id: string; createdAt: string };

/** One endpoint's walk, as far as it has been read. */
export type RunWalk = {
  /** The oldest row fetched so far — null before the first page lands. */
  oldest: RunOrder | null;
  /** The walk reached the end: its last page carried no cursor. */
  done: boolean;
};

/**
 * The merged order: newest first, the id breaking a tie so two runs raised in
 * the same instant keep the same place across repaints.
 *
 * A timestamp that will not parse falls back to the id rather than poisoning
 * the comparison — `NaN !== 0` would otherwise make this return NaN, which
 * `Array.sort` reads as "no opinion at all", and one row with a broken date
 * would scramble its neighbours.
 */
export function compareRuns(a: RunOrder, b: RunOrder): number {
  const delta = Date.parse(b.createdAt) - Date.parse(a.createdAt);
  return Number.isNaN(delta) || delta === 0 ? a.id.localeCompare(b.id) : delta;
}

/**
 * The oldest row every walk can vouch for, plus whether one of them cannot
 * vouch for anything yet.
 *
 * `bound` is the NEWEST of the walks' oldest fetched rows — past it, one walk
 * has read rows and another has not, so the order there is not yet known. A
 * finished walk contributes no bound: it has no unfetched rows left to surprise
 * anyone with. `blocked` is a walk that is neither finished nor holding a single
 * row, which is only true while its first page is in flight.
 */
function frontierOf(walks: RunWalk[]): { bound: RunOrder | null; blocked: boolean } {
  let bound: RunOrder | null = null;
  let blocked = false;
  for (const w of walks) {
    if (w.done) continue;
    if (!w.oldest) {
      blocked = true;
      continue;
    }
    if (!bound || compareRuns(w.oldest, bound) < 0) bound = w.oldest;
  }
  return { bound, blocked };
}

/**
 * How much of an already-merged list the walks can vouch for (AC-1, AC-3).
 *
 * De-duplicated by id on the way through: a refetch of a walk in progress can
 * in principle hand back a row a neighbouring page already carried, and one run
 * appearing twice is the failure AC-3 names — cheaper to make impossible here
 * than to reason about at each call site.
 */
export function withinFrontier<T extends RunOrder>(merged: T[], walks: RunWalk[]): T[] {
  const { bound, blocked } = frontierOf(walks);
  if (blocked) return [];
  const seen = new Set<string>();
  return merged.filter((r) => {
    if (seen.has(r.id)) return false;
    seen.add(r.id);
    return !bound || compareRuns(r, bound) <= 0;
  });
}

/** Which walks "load more" must advance, by index: the one holding the frontier
 *  back, and any that has not answered at all. Not simply "every unfinished
 *  walk" — one whose rows already reach past the frontier has nothing to
 *  contribute until the other catches up, and fetching it would spend a request
 *  to reveal no row. */
export function walksToAdvance(walks: RunWalk[]): number[] {
  const { bound } = frontierOf(walks);
  return walks.flatMap((w, i) => {
    if (w.done) return [];
    if (!w.oldest) return [i];
    return bound && w.oldest.id === bound.id ? [i] : [];
  });
}

/** There is more history to reach — which is the difference between the list
 *  simply stopping and it stating its end (AC-4). */
export function walksHaveMore(walks: RunWalk[]): boolean {
  return walks.some((w) => !w.done);
}
