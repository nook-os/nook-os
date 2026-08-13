// What narrows the runs list, proved without mounting anything (MAIN-558).
//
// The panel's own suite covers the controls and the live list; this covers the
// RULES — which fields search matches, that an absent one is a non-match and
// never a throw, that the six dimensions compose, and that a URL round-trips
// exactly. Pure, so each of those is a sentence rather than a render.
import { describe, expect, it } from "vitest";
import {
  activeRunsFilterCount,
  clearRunsFilters,
  EMPTY_RUNS_FILTER,
  filterRuns,
  KIND_CHOICES,
  parseKind,
  parseRunsFilter,
  raisedBounds,
  raisedLabel,
  RAISED_PRESETS,
  RUN_STATES,
  runSearchFields,
  runsFilterChips,
  serializeRunsFilter,
  writeRunsFilter,
  type FilterableRun,
  type RunsFilter,
} from "./runsFilter";

const NOW = Date.parse("2026-08-13T12:00:00Z");

/** A build row with everything a build can carry. */
const buildRow = (over: Partial<FilterableRun> = {}): FilterableRun => ({
  id: "019f8000-0000-7000-8000-00000000b001",
  kind: "build",
  state: "running",
  label: "MAIN-512",
  createdAt: "2026-08-13T11:00:00Z",
  taskRef: "MAIN-512",
  commitSha: "abcdef1234567890abcdef1234567890abcdef12",
  branch: "main-512-a-slug",
  initiator: "Ryan Hein",
  ...over,
});

/** A review row: no card, no branch — the absences AC-2 is about. */
const reviewRow = (over: Partial<FilterableRun> = {}): FilterableRun => ({
  id: "019f8000-0000-7000-8000-00000000r001",
  kind: "review",
  state: "completed",
  label: "PR #439",
  createdAt: "2026-08-13T10:00:00Z",
  prNumber: 439,
  commitSha: "999888777666555444333222111000fedcba9876",
  initiator: "the converger",
  ...over,
});

const filter = (over: Partial<RunsFilter> = {}): RunsFilter => ({
  ...EMPTY_RUNS_FILTER,
  ...over,
});

/** The whole list, narrowed — the shape every composition case below reads. */
const narrow = (rows: FilterableRun[], f: Partial<RunsFilter>) =>
  filterRuns(rows, filter(f), NOW).map((r) => r.label);

describe("search (AC-2)", () => {
  const rows = [buildRow(), reviewRow()];

  it("finds a build by its card key", () => {
    expect(narrow(rows, { q: "MAIN-512" })).toEqual(["MAIN-512"]);
    expect(narrow(rows, { q: "main-512" })).toEqual(["MAIN-512"]);
  });

  it("finds a review by the PR both ways it is written", () => {
    // `PR #439` is what the row says; `439` is what somebody types.
    expect(narrow(rows, { q: "PR #439" })).toEqual(["PR #439"]);
    expect(narrow(rows, { q: "439" })).toEqual(["PR #439"]);
  });

  it("finds a run by its id", () => {
    expect(narrow(rows, { q: "00000000b001" })).toEqual(["MAIN-512"]);
  });

  it("finds a commit by the short sha AND the full one", () => {
    expect(narrow(rows, { q: "abcdef1" })).toEqual(["MAIN-512"]);
    expect(narrow(rows, { q: "abcdef1234567890abcdef1234567890abcdef12" })).toEqual(["MAIN-512"]);
  });

  it("finds a run by branch and by initiator", () => {
    expect(narrow(rows, { q: "a-slug" })).toEqual(["MAIN-512"]);
    expect(narrow(rows, { q: "converger" })).toEqual(["PR #439"]);
  });

  it("matches nothing it does not match, rather than everything", () => {
    expect(narrow(rows, { q: "MAIN-999" })).toEqual([]);
    expect(narrow(rows, { q: "deadbeef" })).toEqual([]);
  });

  it("requires every word, so two terms narrow rather than widen", () => {
    expect(narrow(rows, { q: "MAIN-512 Ryan" })).toEqual(["MAIN-512"]);
    expect(narrow(rows, { q: "MAIN-512 converger" })).toEqual([]);
  });

  it("treats a field absent on this kind as a non-match, never an error", () => {
    // A review has no card and no branch; a fresh build has no commit. Every
    // one of those is `undefined` on the row, and searching for it must return
    // an empty list rather than throw on a `.toLowerCase()` of nothing.
    const bare = reviewRow({ commitSha: null, branch: null, initiator: undefined });
    expect(() => filterRuns([bare], filter({ q: "anything" }), NOW)).not.toThrow();
    expect(narrow([bare], { q: "anything" })).toEqual([]);
    // And the row is still findable by what it DOES carry.
    expect(narrow([bare], { q: "439" })).toEqual(["PR #439"]);
  });

  it("names exactly the fields a run is findable by", () => {
    expect(runSearchFields(reviewRow({ commitSha: null }))).toEqual([
      "PR #439",
      "439",
      "019f8000-0000-7000-8000-00000000r001",
      "the converger",
    ]);
  });

  it("is no filter at all when it is empty or blank", () => {
    expect(narrow(rows, { q: "" })).toEqual(["MAIN-512", "PR #439"]);
    expect(narrow(rows, { q: "   " })).toEqual(["MAIN-512", "PR #439"]);
  });
});

describe("the dimensions compose (AC-6)", () => {
  const rows = [
    buildRow({ label: "MAIN-1", state: "queued", initiator: "Ryan Hein" }),
    buildRow({ id: "b2", label: "MAIN-2", state: "completed", initiator: "Dana" }),
    reviewRow({ label: "PR #1", state: "queued", prNumber: 1, initiator: "Ryan Hein" }),
  ];

  it("takes the rows every dimension accepts", () => {
    expect(narrow(rows, { kind: "build", states: ["queued"] })).toEqual(["MAIN-1"]);
    expect(narrow(rows, { states: ["queued"], initiator: "Ryan Hein" })).toEqual([
      "MAIN-1",
      "PR #1",
    ]);
    expect(narrow(rows, { kind: "review", states: ["completed"] })).toEqual([]);
  });

  it("ORs the states, so two of them is one question", () => {
    expect(narrow(rows, { states: ["queued", "completed"] })).toEqual([
      "MAIN-1",
      "MAIN-2",
      "PR #1",
    ]);
  });

  it("compares the kind against a value rather than switching on it", () => {
    // A third kind is a `KIND_CHOICES` entry and a row builder: nothing in the
    // filtering knows the names of the two that exist today.
    const spec = buildRow({ id: "s1", kind: "spec", label: "MAIN-3" });
    expect(narrow([...rows, spec], { kind: "spec" as never })).toEqual(["MAIN-3"]);
  });

  it("leaves the other dimensions alone when one changes", () => {
    const f = filter({ kind: "build", q: "MAIN", states: ["queued"] });
    expect({ ...f, kind: "review" }).toMatchObject({ q: "MAIN", states: ["queued"] });
    expect({ ...f, q: "" }).toMatchObject({ kind: "build", states: ["queued"] });
  });
});

describe("the raised range (AC-3)", () => {
  const rows = [
    buildRow({ label: "recent", createdAt: "2026-08-13T11:30:00Z" }),
    buildRow({ id: "b2", label: "yesterday", createdAt: "2026-08-12T09:00:00Z" }),
    buildRow({ id: "b3", label: "old", createdAt: "2026-06-01T09:00:00Z" }),
  ];

  it("narrows to a relative preset, measured from one instant", () => {
    expect(narrow(rows, { since: "1h" })).toEqual(["recent"]);
    expect(narrow(rows, { since: "24h" })).toEqual(["recent"]);
    expect(narrow(rows, { since: "7d" })).toEqual(["recent", "yesterday"]);
    expect(narrow(rows, { since: "30d" })).toEqual(["recent", "yesterday"]);
  });

  it("includes the whole of the last day a date range names", () => {
    // A run raised at 11:30 on the 13th is inside "up to the 13th": the bound
    // is the start of the day AFTER, not that day's midnight.
    const { from, to } = raisedBounds(filter({ from: "2026-08-12", to: "2026-08-13" }), NOW);
    expect(from).toBe(new Date(2026, 7, 12).getTime());
    expect(to).toBe(new Date(2026, 7, 14).getTime());
  });

  it("takes an open-ended range from either side", () => {
    expect(raisedBounds(filter({ from: "2026-08-12" }), NOW).to).toBe(Infinity);
    expect(raisedBounds(filter({ to: "2026-08-12" }), NOW).from).toBe(-Infinity);
  });

  it("keeps a row whose timestamp will not parse", () => {
    // A broken date costs a row its age, not its place in the list.
    const broken = buildRow({ id: "b4", label: "broken", createdAt: "not a date" });
    expect(narrow([broken], { since: "1h" })).toEqual(["broken"]);
  });

  it("reads as one sentence per shape, and as nothing when unset", () => {
    expect(raisedLabel(filter({ since: "24h" }))).toBe("last 24 hours");
    expect(raisedLabel(filter({ from: "2026-08-01", to: "2026-08-02" }))).toBe(
      "2026-08-01 → 2026-08-02",
    );
    expect(raisedLabel(filter({ from: "2026-08-01" }))).toBe("from 2026-08-01");
    expect(raisedLabel(filter({ to: "2026-08-02" }))).toBe("to 2026-08-02");
    expect(raisedLabel(filter())).toBe("");
  });
});

describe("chips and the count (AC-4, AC-5)", () => {
  it("is one chip per active filter, and the count is the chips", () => {
    const f = filter({ states: ["queued", "running"], initiator: "Dana", since: "24h" });
    expect(runsFilterChips(f).map((c) => c.label)).toEqual([
      "queued",
      "running",
      "Dana",
      "last 24 hours",
    ]);
    expect(activeRunsFilterCount(f)).toBe(4);
  });

  it("does not chip the kind or the search, which show themselves", () => {
    // Both are always-visible controls carrying their own value; a chip
    // repeating them would be one state rendered twice.
    expect(runsFilterChips(filter({ kind: "build", q: "MAIN-512" }))).toEqual([]);
    expect(activeRunsFilterCount(filter({ kind: "build", q: "MAIN-512" }))).toBe(0);
  });

  it("removes JUST the one chip, leaving its neighbours", () => {
    const f = filter({ states: ["queued", "running"], branch: "main-1-x" });
    const [queued] = runsFilterChips(f);
    expect(queued.next.states).toEqual(["running"]);
    expect(queued.next.branch).toBe("main-1-x");
  });

  it("shows a range as ONE chip that clears every shape of it", () => {
    const dated = runsFilterChips(filter({ from: "2026-08-01", to: "2026-08-02" }));
    expect(dated).toHaveLength(1);
    expect(dated[0].next).toMatchObject({ since: "", from: "", to: "" });
  });

  it("takes the loop's word for a state when one is offered", () => {
    const f = filter({ states: ["waiting_on_human"] });
    expect(runsFilterChips(f, (s) => s.replace(/_/g, " "))[0].label).toBe("waiting on human");
  });

  it("clears every chipped dimension and nothing else", () => {
    const f = filter({
      kind: "build",
      q: "MAIN-512",
      states: ["queued"],
      initiator: "Dana",
      branch: "b",
      from: "2026-08-01",
    });
    const cleared = clearRunsFilters(f);
    expect(runsFilterChips(cleared)).toEqual([]);
    // Kind and search survive: they are not what the chip row is showing, and
    // clearing something invisible is how a control lies.
    expect(cleared).toMatchObject({ kind: "build", q: "MAIN-512" });
  });
});

describe("the URL is the state (AC-7)", () => {
  const cases: RunsFilter[] = [
    EMPTY_RUNS_FILTER,
    filter({ kind: "review" }),
    filter({ q: "MAIN-512 alice" }),
    filter({ states: ["queued", "failed"] }),
    filter({ initiator: "Ryan Hein", branch: "main-558-x" }),
    filter({ since: "7d" }),
    filter({ from: "2026-08-01", to: "2026-08-02" }),
    filter({
      kind: "build",
      q: "abcdef1",
      states: ["running"],
      initiator: "Dana",
      branch: "b",
      since: "1h",
    }),
  ];

  it("round-trips every shape a filter can take", () => {
    for (const f of cases) expect(parseRunsFilter(serializeRunsFilter(f))).toEqual(f);
  });

  it("writes a default as ABSENCE, so a link carries only what was chosen", () => {
    expect(serializeRunsFilter(EMPTY_RUNS_FILTER).toString()).toBe("");
    expect(serializeRunsFilter(filter({ kind: "all", q: "x" })).toString()).toBe("q=x");
  });

  it("leaves the rest of the URL alone", () => {
    const params = new URLSearchParams("section=runs&run=job-1&kind=build&q=old");
    const next = writeRunsFilter(params, filter({ q: "new" }));
    expect(next.get("section")).toBe("runs");
    expect(next.get("run")).toBe("job-1");
    expect(next.get("kind")).toBeNull();
    expect(next.get("q")).toBe("new");
  });

  it("drops what it cannot honour rather than filtering by it", () => {
    const junk = parseRunsFilter(
      new URLSearchParams("kind=specs&state=queued,martian&since=eventually&from=yesterday"),
    );
    expect(junk).toEqual(filter({ states: ["queued"] }));
  });

  it("lets the dates win a URL carrying a preset too, so the range stays one", () => {
    const both = parseRunsFilter(new URLSearchParams("since=24h&from=2026-08-01"));
    expect(both).toMatchObject({ since: "", from: "2026-08-01" });
  });
});

describe("the vocabularies this filter offers", () => {
  it("names the three kind segments, wired to the parse", () => {
    expect(KIND_CHOICES.map((c) => c.label)).toEqual(["All", "Builds", "Reviews"]);
    for (const c of KIND_CHOICES)
      expect(parseKind(c.value === "all" ? null : c.value)).toBe(c.value);
    expect(parseKind("specs")).toBe("all");
  });

  it("names the seven states the card names", () => {
    expect([...RUN_STATES]).toEqual([
      "queued",
      "claimed",
      "running",
      "waiting_on_human",
      "completed",
      "failed",
      "canceled",
    ]);
  });

  it("names four relative ranges, each a real span", () => {
    expect(RAISED_PRESETS.map((p) => p.value)).toEqual(["1h", "24h", "7d", "30d"]);
    expect(RAISED_PRESETS.every((p) => p.ms > 0)).toBe(true);
  });
});
