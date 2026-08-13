// What narrows the runs list, derived once (MAIN-558).
//
// Search, kind, state, initiator, branch and a raised-range are SIX INDEPENDENT
// DIMENSIONS that compose (AC-6): each is a predicate over one row, and the
// list is the rows every predicate accepts. Written that way rather than as one
// condition so a seventh dimension is an entry in `DIMENSIONS`, and so the kind
// is compared against a value rather than switched on — a third run kind is
// then a row builder and a `KIND_CHOICES` entry, and nothing in here moves.
//
// Nothing here renders and nothing here reads a URL of its own: the panel owns
// the `useSearchParams` and hands the parsed filter down, which is what lets
// every rule below be a unit test rather than a mounted component.
//
// The board's filter strip (MAIN-110) is the shape this follows — parse from
// params, write back onto params, one chip per active filter, a count. It is
// deliberately NOT shared code: that filter's dimensions are a task's, these
// are a run's, and the only thing the two have in common is the pattern.

/** What raised a run. The word a row shows, and the value the URL carries. */
export type RunKind = "build" | "review";

/** The kind filter's value, where "all" is the absence of a filter. */
export type KindFilter = RunKind | "all";

/** The segments of the kind selector, in the order they are shown (MAIN-556
 *  AC-7). Plural words, because each names a set of rows rather than one row's
 *  kind — the badge in a row is the singular.
 *
 *  This list IS the kind dimension: adding a kind here and building its rows is
 *  the whole of adding one (AC-6). */
export const KIND_CHOICES: { value: KindFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "build", label: "Builds" },
  { value: "review", label: "Reviews" },
];

export function parseKind(raw: string | null): KindFilter {
  return KIND_CHOICES.some((c) => c.value === raw && c.value !== "all")
    ? (raw as KindFilter)
    : "all";
}

/**
 * The subset of a run row the filtering reads.
 *
 * Structural rather than the `RunRow` import, so the panel depends on this file
 * and not the reverse — the same arrangement `runActions.ts` (MAIN-559) makes
 * for the same reason. Every optional field is optional because it is genuinely
 * absent on one of the kinds: a review has no card, a fresh build has no commit
 * (AC-2). Absence is a non-match, never an error.
 */
export interface FilterableRun {
  id: string;
  kind: string;
  state: string;
  /** The run's name in the list — `MAIN-42`, `PR #341`. */
  label: string;
  createdAt: string;
  taskRef?: string | null;
  prNumber?: number | null;
  /** The FULL sha, so a short prefix and the whole thing both match (AC-2). */
  commitSha?: string | null;
  branch?: string | null;
  initiator?: string | null;
}

/**
 * The states a run can be filtered to (AC-3), in lifecycle order.
 *
 * The LOOP's own vocabulary, not a second one: the value is the state the
 * control plane stores and the label is `jobStateMeta`'s, resolved by the
 * panel. A fixed list rather than one derived from the rows on screen, because
 * a live list must not withdraw the filter somebody is reading under — and
 * because a state with no runs answering it is a legitimate, honest empty.
 */
export const RUN_STATES = [
  "queued",
  "claimed",
  "running",
  "waiting_on_human",
  "completed",
  "failed",
  "canceled",
] as const;

/** The relative halves of the raised-range (AC-3), as a span in milliseconds.
 *  Presets rather than a free number: "how far back" is a decision with four
 *  useful answers, and a spinner asking for minutes is a worse question. */
export const RAISED_PRESETS: { value: string; label: string; ms: number }[] = [
  { value: "1h", label: "last hour", ms: 60 * 60 * 1000 },
  { value: "24h", label: "last 24 hours", ms: 24 * 60 * 60 * 1000 },
  { value: "7d", label: "last 7 days", ms: 7 * 24 * 60 * 60 * 1000 },
  { value: "30d", label: "last 30 days", ms: 30 * 24 * 60 * 60 * 1000 },
];

export interface RunsFilter {
  /** Which kinds show. `all` is the absence of the filter. */
  kind: KindFilter;
  /** The search box's text (AC-1). Never a chip: it has its own visible field. */
  q: string;
  /** States to include, OR'd. Empty is every state (AC-3). */
  states: string[];
  /** An exact initiator display name, or "" for any. */
  initiator: string;
  /** An exact branch name, or "" for any. */
  branch: string;
  /**
   * The raised-range, in ONE of two forms — a relative preset, or a pair of
   * calendar dates (AC-3). One dimension, so setting either clears the other:
   * "the last 24 hours" and "the 3rd to the 5th" are two ways to say the same
   * kind of thing, and a list obeying both at once answers neither question.
   */
  since: string;
  /** Inclusive `YYYY-MM-DD` bounds, or "". Local days, because a date picked in
   *  a date field means the reader's day and not UTC's. */
  from: string;
  to: string;
}

export const EMPTY_RUNS_FILTER: RunsFilter = {
  kind: "all",
  q: "",
  states: [],
  initiator: "",
  branch: "",
  since: "",
  from: "",
  to: "",
};

/** Every query parameter this filter owns. `writeRunsFilter` clears exactly
 *  these and leaves the rest of the URL — `section`, `run` — untouched. */
export const RUNS_FILTER_KEYS = [
  "kind",
  "q",
  "state",
  "initiator",
  "branch",
  "since",
  "from",
  "to",
] as const;

/** A `YYYY-MM-DD` a date input produces. Anything else in the URL is not a date
 *  and is dropped rather than fed to `Date` to become an invalid instant. */
const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;

export function parseRunsFilter(params: URLSearchParams): RunsFilter {
  const date = (k: string) => {
    const v = params.get(k) ?? "";
    return DATE_RE.test(v) ? v : "";
  };
  const since = params.get("since") ?? "";
  const from = date("from");
  const to = date("to");
  return {
    kind: parseKind(params.get("kind")),
    q: params.get("q") ?? "",
    states: (params.get("state") ?? "")
      .split(",")
      .map((s) => s.trim())
      // A state this build does not know is not a filter it can honour, and
      // keeping it would empty the list with a chip nobody can explain.
      .filter((s): s is string => (RUN_STATES as readonly string[]).includes(s)),
    initiator: params.get("initiator") ?? "",
    branch: params.get("branch") ?? "",
    // The dates win a URL carrying both: they are the more specific statement,
    // and one of the two has to lose for the dimension to stay one dimension.
    since: from || to ? "" : RAISED_PRESETS.some((p) => p.value === since) ? since : "",
    from,
    to,
  };
}

/** Apply a filter onto a URLSearchParams, clearing only this filter's keys.
 *  A default is written as ABSENCE, so a link carries what was chosen and not
 *  a restatement of what the code already believes. */
export function writeRunsFilter(next: URLSearchParams, f: RunsFilter): URLSearchParams {
  for (const k of RUNS_FILTER_KEYS) next.delete(k);
  if (f.kind !== "all") next.set("kind", f.kind);
  if (f.q) next.set("q", f.q);
  if (f.states.length) next.set("state", f.states.join(","));
  if (f.initiator) next.set("initiator", f.initiator);
  if (f.branch) next.set("branch", f.branch);
  if (f.since) next.set("since", f.since);
  if (f.from) next.set("from", f.from);
  if (f.to) next.set("to", f.to);
  return next;
}

/** Serialize to a fresh URLSearchParams — the half of the round-trip that
 *  `parseRunsFilter` inverts (AC-7). */
export function serializeRunsFilter(f: RunsFilter): URLSearchParams {
  return writeRunsFilter(new URLSearchParams(), f);
}

/**
 * Every string a run can be FOUND by (AC-2).
 *
 * The list is the whole of what search matches, so adding a searchable field is
 * a line here. A field absent on this kind contributes nothing and is not an
 * error — which is why this filters rather than indexes by name.
 *
 * The label carries the display form of both identifiers (`PR #439`, `MAIN-42`)
 * and the bare PR number is added beside it, so `439` and `PR #439` both find
 * the same row without search having to know either spelling.
 */
export function runSearchFields(row: FilterableRun): string[] {
  return [
    row.label,
    row.taskRef,
    row.prNumber == null ? null : String(row.prNumber),
    row.id,
    row.commitSha,
    row.branch,
    row.initiator,
  ].filter((v): v is string => typeof v === "string" && v.length > 0);
}

/**
 * Whether a row answers the search text.
 *
 * Every whitespace-separated word must hit SOME field, so `MAIN-42 alice`
 * narrows rather than widens — the same rule the section finder and the board
 * search use, because a reader should not have to learn a third one.
 *
 * Substring, not prefix: it is what makes a short sha find the full one it is
 * the head of, and `439` find `PR #439`.
 */
export function matchesSearch(row: FilterableRun, q: string): boolean {
  const words = q.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const hay = runSearchFields(row).map((s) => s.toLowerCase());
  return words.every((w) => hay.some((h) => h.includes(w)));
}

/** Local midnight of a `YYYY-MM-DD`, as an instant. `plusDays` walks whole
 *  CALENDAR days rather than adding 24h, so a range does not gain or lose an
 *  hour across a daylight-saving boundary. */
function dayStart(day: string, plusDays = 0): number {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(y, m - 1, d + plusDays).getTime();
}

/**
 * The instants a filter's raised-range admits, as `[from, to)`.
 *
 * `to` is the START OF THE DAY AFTER the chosen one, because a date range a
 * person picks is inclusive of both ends and a run raised at 16:20 on the last
 * day is inside "up to that day".
 */
export function raisedBounds(f: RunsFilter, now: number): { from: number; to: number } {
  const preset = RAISED_PRESETS.find((p) => p.value === f.since);
  if (preset) return { from: now - preset.ms, to: Infinity };
  return {
    from: f.from ? dayStart(f.from) : -Infinity,
    to: f.to ? dayStart(f.to, 1) : Infinity,
  };
}

/** Whether a row was raised inside the range. A timestamp that will not parse
 *  is KEPT: a broken date should cost a row its age, not its place in the list
 *  — the same call `runAge` makes. */
function matchesRaised(row: FilterableRun, f: RunsFilter, now: number): boolean {
  if (!f.since && !f.from && !f.to) return true;
  const at = Date.parse(row.createdAt);
  if (Number.isNaN(at)) return true;
  const { from, to } = raisedBounds(f, now);
  return at >= from && at < to;
}

/**
 * The independent dimensions, composed (AC-6).
 *
 * A list rather than one boolean expression: each entry is separately readable
 * and separately testable, and the composition — every dimension must accept —
 * is stated once instead of being spelled out again per dimension.
 */
const DIMENSIONS: ((row: FilterableRun, f: RunsFilter, now: number) => boolean)[] = [
  (row, f) => f.kind === "all" || row.kind === f.kind,
  (row, f) => f.states.length === 0 || f.states.includes(row.state),
  (row, f) => !f.initiator || row.initiator === f.initiator,
  (row, f) => !f.branch || row.branch === f.branch,
  matchesRaised,
  (row, f) => matchesSearch(row, f.q),
];

export function filterRuns<T extends FilterableRun>(
  rows: T[],
  f: RunsFilter,
  now: number,
): T[] {
  return rows.filter((row) => DIMENSIONS.every((d) => d(row, f, now)));
}

/** One active filter, as a removable chip (AC-5): what it says, and the filter
 *  that results from removing JUST this one. */
export interface RunsFilterChip {
  key: string;
  label: string;
  next: RunsFilter;
}

/** How a raised-range reads on its chip. One sentence per shape, so a chip
 *  never says "from  to " with a hole where an unset bound was. */
export function raisedLabel(f: RunsFilter): string {
  const preset = RAISED_PRESETS.find((p) => p.value === f.since);
  if (preset) return preset.label;
  if (f.from && f.to) return `${f.from} → ${f.to}`;
  if (f.from) return `from ${f.from}`;
  if (f.to) return `to ${f.to}`;
  return "";
}

/**
 * Every active filter as its own chip, in a stable order (AC-5).
 *
 * KIND and SEARCH are deliberately not chips, and so are not counted (AC-4):
 * both are controls that are always visible and always show their own value, so
 * a chip repeating them would be one state rendered twice — and removing the
 * chip for a kind would have to mean "All", which the segment already says.
 *
 * The raised-range is ONE chip whichever shape it took, because it is one
 * dimension: removing it clears the preset and both dates together.
 *
 * `stateLabel` is passed in rather than looked up: the loop's word for a state
 * is `jobStateMeta`'s, and importing that here would make this module depend on
 * the loop's vocabulary to answer a question about counting.
 */
export function runsFilterChips(
  f: RunsFilter,
  stateLabel: (state: string) => string = (state) => state,
): RunsFilterChip[] {
  const chips: RunsFilterChip[] = [];
  // Lifecycle order, not the order they were clicked in: the chip row is a
  // reading of what is on, and two people who chose the same two states should
  // be looking at the same row.
  for (const s of RUN_STATES.filter((x) => f.states.includes(x)))
    chips.push({
      key: `state:${s}`,
      label: stateLabel(s),
      next: { ...f, states: f.states.filter((x) => x !== s) },
    });
  if (f.initiator)
    chips.push({ key: "initiator", label: f.initiator, next: { ...f, initiator: "" } });
  if (f.branch) chips.push({ key: "branch", label: f.branch, next: { ...f, branch: "" } });
  const raised = raisedLabel(f);
  if (raised)
    chips.push({ key: "raised", label: raised, next: { ...f, since: "", from: "", to: "" } });
  return chips;
}

/** What the filter button's badge shows (AC-4) — the chips, counted, because
 *  the badge and the chip row must never disagree about how many filters are
 *  on. */
export function activeRunsFilterCount(f: RunsFilter): number {
  return runsFilterChips(f).length;
}

/** The filter with every CHIPPED dimension cleared — what `Clear all` does
 *  (AC-5). Kind and search survive it, because they are not what the chip row
 *  is showing and clearing something invisible is how a control lies. */
export function clearRunsFilters(f: RunsFilter): RunsFilter {
  return { ...EMPTY_RUNS_FILTER, kind: f.kind, q: f.q };
}
