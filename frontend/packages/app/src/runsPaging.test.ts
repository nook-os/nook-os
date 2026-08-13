// The merge rule two paged walks share (MAIN-560 AC-1/AC-3/AC-4).
//
// What is worth pinning is the frontier: a row is renderable only once BOTH
// walks have gone at least as far back as it, because otherwise the next page
// inserts rows above rows already on screen.
import { describe, expect, it } from "vitest";
import { compareRuns, walksHaveMore, walksToAdvance, withinFrontier } from "./runsPaging";

const at = (id: string, createdAt: string) => ({ id, createdAt });
/** A walk, described the way the panel describes one: how far it has read, and
 *  whether that is the end. */
const walk = (rows: { id: string; createdAt: string }[], done = false) => ({
  oldest: rows[rows.length - 1] ?? null,
  done,
});
/** Both walks' rows in the one merged order, which is what `withinFrontier`
 *  cuts. */
const merge = (...rows: { id: string; createdAt: string }[][]) =>
  rows.flat().sort(compareRuns);
const ids = (rows: { id: string }[]) => rows.map((r) => r.id);

describe("the merged order", () => {
  it("is newest first, with the id breaking a tie", () => {
    const older = at("a", "2026-08-08T09:00:00Z");
    const newer = at("b", "2026-08-08T10:00:00Z");
    expect(compareRuns(newer, older)).toBeLessThan(0);
    expect(compareRuns(at("a", "2026-08-08T09:00:00Z"), at("b", "2026-08-08T09:00:00Z"))).toBeLessThan(0);
  });

  it("falls back to the id rather than returning NaN for a broken date", () => {
    // NaN is not "these are equal" — `Array.sort` reads it as no opinion at
    // all, and one row with a bad timestamp would scramble its neighbours.
    const cmp = compareRuns(at("a", "not-a-date"), at("b", "2026-08-08T09:00:00Z"));
    expect(Number.isNaN(cmp)).toBe(false);
    expect(cmp).toBeLessThan(0);
  });
});

describe("the frontier", () => {
  it("withholds rows the other walk has not reached yet", () => {
    // Builds have been walked back to 09:00 and are not finished, so the
    // reviews from 08:00 and 07:00 cannot be placed: an unfetched build could
    // belong between them.
    const builds = [at("b1", "2026-08-08T11:00:00Z"), at("b2", "2026-08-08T09:00:00Z")];
    const reviews = [at("r1", "2026-08-08T10:00:00Z"), at("r2", "2026-08-08T08:00:00Z")];
    const merged = withinFrontier(merge(builds, reviews), [walk(builds), walk(reviews, true)]);
    expect(ids(merged)).toEqual(["b1", "r1", "b2"]);
  });

  it("shows everything once every walk has ended", () => {
    const builds = [at("b1", "2026-08-08T11:00:00Z"), at("b2", "2026-08-08T09:00:00Z")];
    const reviews = [at("r1", "2026-08-08T10:00:00Z"), at("r2", "2026-08-08T08:00:00Z")];
    const merged = withinFrontier(merge(builds, reviews), [
      walk(builds, true),
      walk(reviews, true),
    ]);
    expect(ids(merged)).toEqual(["b1", "r1", "b2", "r2"]);
  });

  it("shows nothing while a walk has not answered at all", () => {
    const builds = [at("b1", "2026-08-08T11:00:00Z")];
    expect(withinFrontier(merge(builds), [walk(builds, true), walk([])])).toEqual([]);
  });

  it("never repeats a run that arrived in two pages", () => {
    const twice = [at("b1", "2026-08-08T11:00:00Z"), at("b1", "2026-08-08T11:00:00Z")];
    const merged = withinFrontier(merge(twice), [walk(twice, true), walk([], true)]);
    expect(ids(merged)).toEqual(["b1"]);
  });
});

describe("which walk a load advances", () => {
  it("is the one holding the frontier back, and not the one already past it", () => {
    const walks = [
      walk([at("b1", "2026-08-08T11:00:00Z"), at("b2", "2026-08-08T09:00:00Z")]),
      walk([at("r1", "2026-08-08T10:00:00Z"), at("r2", "2026-08-08T02:00:00Z")]),
    ];
    // Builds stop at 09:00 and reviews at 02:00, so builds are the constraint:
    // fetching reviews would spend a request to reveal no row.
    expect(walksToAdvance(walks)).toEqual([0]);
  });

  it("leaves a finished walk alone", () => {
    const walks = [
      walk([at("b1", "2026-08-08T11:00:00Z")], true),
      walk([at("r1", "2026-08-08T10:00:00Z")]),
    ];
    expect(walksToAdvance(walks)).toEqual([1]);
  });

  it("asks a walk that has answered nothing", () => {
    expect(walksToAdvance([walk([], true), walk([])])).toEqual([1]);
  });
});

describe("the end of history", () => {
  it("is reached only when neither walk has a cursor left", () => {
    expect(walksHaveMore([walk([], true), walk([], true)])).toBe(false);
    expect(walksHaveMore([walk([], true), walk([at("r1", "2026-08-08T10:00:00Z")])])).toBe(true);
  });
});
