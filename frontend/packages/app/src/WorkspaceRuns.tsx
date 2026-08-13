// One repo's managed runs — build and review together (MAIN-488).
//
// Reviews and Builds were two sections, two tabs and two wrappers over this one
// component, differing in the words on the panel and the row query. The split
// answered no question a reader has, so the kind is a BADGE and a FILTER here
// rather than a second surface.
//
// The two listings stay two endpoints and are merged on the client (NG-1) —
// there is no runs endpoint, and inventing one would mean paginating across
// two tables for a list that is short by construction.
//
// A run keeps a transcript — the same `loop_job_transcript` a spec keeps,
// rendered through the same `ChatView`. There is deliberately no second
// transcript mechanism.
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useInfiniteQuery, useQuery, useQueryClient } from "@tanstack/react-query";
import { useLocation, useNavigate, useSearchParams } from "react-router-dom";
import { Filter, GitBranch, MoreHorizontal, Search } from "lucide-react";
import { api, type LoopJobTranscriptEntry, type Page } from "@nookos/api";
import { ChatView, Empty, Panel, useAnchoredMenu } from "@nookos/ui";
import { useAgentCommands } from "./agentCommands";
import { BuildOutcome } from "./BuilderStrip";
import { useBuildRunFacts } from "./buildLoop";
import { ContextMenuRegion, useContextMenuApi, type ContextMenuItem } from "./contextMenu";
import { askConfirm } from "./dialogs";
import { transcriptMessages } from "./LoopPanel";
import { agentActivityLabel, foldToolActivity, jobStateMeta } from "./loop";
import {
  CANCEL_ENDED_REFUSAL,
  cancelPrompt,
  cancelRefusal,
  isTerminalRun,
  overflowRunActions,
  primaryRunAction,
  rerunRefusal,
  runActions,
  type RunAction,
  type RunActionId,
} from "./runActions";
import {
  compareRuns,
  walksHaveMore,
  walksToAdvance,
  withinFrontier,
  type RunWalk,
} from "./runsPaging";
import {
  activeRunsFilterCount,
  clearRunsFilters,
  filterRuns,
  KIND_CHOICES,
  parseRunsFilter,
  RAISED_PRESETS,
  RUN_STATES,
  runsFilterChips,
  writeRunsFilter,
  type KindFilter,
  type RunKind,
  type RunsFilter,
  type RunsFilterChip,
} from "./runsFilter";
import {
  RunGone,
  RunsState,
  runsPhase,
  type RunFilterChip,
  type RunsPhase,
} from "./RunsStates";
// The queue panel's duration words, not a second set of them: "5m" there and
// "5 min" here would be two vocabularies for one idea, and the one thing a
// reader must never have to do is work out whether they mean the same.
import { shortAge } from "./QueuePanel";
import { fileSlug, TranscriptActions } from "./transcriptExport";

/** What every run row can say about itself, whatever kind produced it. */
export type RunRow = {
  id: string;
  kind: RunKind;
  state: string;
  /** The item's name in the list: "PR #12", "MAIN-42". */
  label: string;
  /** The right-hand annotation: a short head sha, an outcome. */
  meta: string;
  /** Why a `queued` run is still waiting (MAIN-494), in the sentence the
   *  control plane wrote. Empty on a run that is not waiting. */
  reason?: string;
  /** The same gate typed — carried onto the row as an attribute, so a client
   *  reading this list branches on a value rather than on the sentence. */
  reasonKind?: string;
  /** When the run was raised. The list's only ordering, and the one field the
   *  two row shapes below have to agree on. */
  createdAt: string;
  /**
   * The card a build run is about, in whatever form the listing sends — which
   * is the KEY (`MAIN-42`), because `WorkspaceBuildRun` carries no uuid.
   *
   * Named `taskRef` rather than `taskId` on purpose: `/loop/:id` resolves keys
   * and uuids alike server-side (MAIN-209), so the route does not care which
   * this is, and a field called `taskId` holding a key is how a reader ends up
   * looking for an id that was never on the wire.
   */
  taskRef?: string | null;
  /** The pull request a review run is about, as a number: the URL is built from
   *  the workspace's remote, which a row does not carry. */
  prNumber?: number | null;
  /** The head a review run was raised for, short. Line 1 of the detail header
   *  shows it where a build shows its branch (AC-7). */
  commit?: string;
  /**
   * The three fields R1 (MAIN-557) added to the listings, carried so search can
   * match on them (MAIN-558 AC-2). Read by `filterRuns` and rendered by
   * NOTHING: the row's shape is R2's and this card does not touch it (NG-4).
   *
   * The commit is the FULL sha, unlike `commit` above, because AC-2 wants a
   * short prefix and the whole thing to find the same row — and a substring
   * search over the full one does both, where a search over the short one
   * cannot do the second.
   */
  commitSha?: string | null;
  branch?: string | null;
  initiator?: string | null;
};

type BuildRun = {
  id: string;
  state: string;
  task_key?: string | null;
  created_at: string;
  // `loop_jobs.build_outcome`, the exact column MAIN-458 (PR #361) commits —
  // the name is pinned there, not guessed here. Optional so the panel lights
  // up the moment the listing starts sending it; until then rows show
  // key + state.
  build_outcome?: string | null;
  queued_reason?: string | null;
  queued_reason_kind?: { kind: string } | null;
  // MAIN-557's three joins. Optional because they genuinely are: a loop-raised
  // build records no branch, and a fresh build has no commit to name.
  branch?: string | null;
  initiator?: string | null;
  commit_sha?: string | null;
};

type ReviewRun = {
  id: string;
  state: string;
  review_pr_number?: number | null;
  review_head_sha?: string | null;
  created_at: string;
  // `loop_jobs.review_verdict_source` (MAIN-516, MAIN-542): the control plane's
  // own `changes_requested` for a PR that conflicts with its base, or that the
  // merge queue ejected — no run, no agent, no findings. The row is a real
  // review row in every other respect, so without this it reads as a review
  // that happened.
  review_verdict_source?: string | null;
  // A review run's branch is always null today (MAIN-557 says why); it is read
  // here anyway, so the day the forge starts reporting one it is searchable
  // without a second change.
  branch?: string | null;
  initiator?: string | null;
};

/** The loop's state tones, in the design system's words. */
export function pillTone(
  tone: "info" | "warn" | "err" | "ok" | "muted",
): "ok" | "warn" | "err" | "info" | "dim" {
  return tone === "muted" ? "dim" : tone;
}

/**
 * The greyscale half of a state badge (MAIN-556 AC-5).
 *
 * The pill's colour is the whole signal today, and on this theme `warn` and the
 * accent are the same amber — so a greyscale screen, a projector, or a reader
 * who cannot separate the red from the green sees one pill repeated down the
 * list. Every state therefore also has a SHAPE. Distinct characters rather than
 * one dot at several opacities, because opacity is still colour.
 *
 * Named state by state rather than derived from the tone: `queued` and
 * `canceled` share a tone and are not the same thing to look at.
 */
export function stateGlyph(state: string): string {
  switch (state) {
    case "queued":
      return "…";
    case "claimed":
      return "◐";
    case "running":
      return "▸";
    case "waiting_on_human":
      return "?";
    case "completed":
      return "✓";
    case "failed":
      return "✕";
    case "canceled":
      return "⊘";
    case CANCELING:
      return "◌";
    default:
      return "•";
  }
}

/**
 * The state a row shows while THIS client's cancel is in flight (AC-4).
 *
 * Not a server state and deliberately not added to `jobStateMeta`, which is the
 * loop's own vocabulary: no run is ever stored in `canceling`. It is a
 * transition this client knows about because it started it, and the honest
 * rendering of "I have asked, and have not been told yet" is a word in the
 * state column rather than a spinner somewhere else.
 */
export const CANCELING = "canceling";

export function shownState(state: string, pending?: string): string {
  return pending === "cancel" && !isTerminalRun(state) ? CANCELING : state;
}

export function runStateMeta(state: string): ReturnType<typeof jobStateMeta> {
  return state === CANCELING ? { label: CANCELING, tone: "warn" } : jobStateMeta(state);
}

/** What a review run is ABOUT, in the words the panel can show without a
 *  lookup. */
export function runLabel(run: ReviewRun): string {
  return run.review_pr_number ? `PR #${run.review_pr_number}` : "review";
}

/** The head a run was raised for, short enough to sit in a row.
 *
 *  Shown because it is the whole wakeup rule made visible: two runs of the same
 *  PR differ by this and nothing else, and without it a list of five runs for
 *  one PR looks like the loop spinning rather than five pushes. */
export function shortHead(sha?: string | null): string {
  return sha ? sha.slice(0, 7) : "";
}

/**
 * The sentence a waiting run explains itself with (MAIN-494 AC-5/AC-6).
 *
 * The TEXT verbatim, whether or not a typed gate came with it — the control
 * plane writes the two together and the sentence IS the rendering, so a row
 * from before the typed column renders identically to one after it. Nothing
 * here parses the sentence into a cause: a near-match would be a confident lie
 * about why something waited.
 *
 * Only while `queued`. A claimed run's reason is cleared at the claim, and a
 * stale one beside `running` would read as the run being stuck.
 */
export function queuedReason(state: string, reason?: string | null): string {
  return state === "queued" && reason ? reason : "";
}

export function buildRow(r: BuildRun): RunRow {
  return {
    id: r.id,
    kind: "build",
    state: r.state,
    label: r.task_key ?? "build",
    meta: r.build_outcome ?? "",
    reason: queuedReason(r.state, r.queued_reason),
    reasonKind: r.queued_reason_kind?.kind,
    createdAt: r.created_at,
    // The key is the whole join. `WorkspaceBuildRun` (nook-types) sends
    // `id, state, task_key, queued_reason, queued_reason_kind, created_at` and
    // nothing else, so reading a `target_task_id` off it would be a field that
    // is `undefined` on every row — which is not a missing link, it is a link
    // that silently never appears.
    taskRef: r.task_key ?? null,
    commitSha: r.commit_sha ?? null,
    branch: r.branch ?? null,
    initiator: r.initiator ?? null,
  };
}

/** What each verdict the CONTROL PLANE concluded says it is. Named one by one
 *  rather than prettified from the column: an unknown source is a source this
 *  build does not understand, and the honest rendering of that is to say
 *  nothing extra rather than to invent a phrase for it. */
const CONTROL_PLANE_VERDICTS: Record<string, string> = {
  conflict: "conflict, not reviewed",
  queue_ejection: "queue ejection, not reviewed",
};

/** The right-hand annotation for a review row: the head, and — for a verdict no
 *  agent produced — what it actually is (MAIN-516 AC-6, MAIN-542 AC-4). */
export function reviewMeta(r: ReviewRun): string {
  const head = shortHead(r.review_head_sha);
  const cause = CONTROL_PLANE_VERDICTS[r.review_verdict_source ?? ""];
  if (!cause) return head;
  return head ? `${head} · ${cause}` : cause;
}

export function reviewRow(r: ReviewRun): RunRow {
  return {
    id: r.id,
    kind: "review",
    state: r.state,
    label: runLabel(r),
    meta: reviewMeta(r),
    createdAt: r.created_at,
    prNumber: r.review_pr_number ?? null,
    commit: shortHead(r.review_head_sha),
    commitSha: r.review_head_sha ?? null,
    branch: r.branch ?? null,
    initiator: r.initiator ?? null,
  };
}

/** Both kinds in one list, newest first (AC-3). The id breaks a tie so two runs
 *  raised in the same instant keep a stable order across repaints — the order
 *  itself is `compareRuns`, shared with the paged merge so a page boundary
 *  cannot sort differently from the list it lands in (MAIN-560). */
export function mergeRuns(builds: BuildRun[], reviews: ReviewRun[]): RunRow[] {
  return [...builds.map(buildRow), ...reviews.map(reviewRow)].sort(compareRuns);
}

/**
 * What is narrowing the list, for a reader who has ended up with no rows at all
 * (MAIN-560 AC-6).
 *
 * The chip row's dimensions in the chip row's own words, PLUS the kind — which
 * MAIN-558 deliberately leaves off the chips because its segment always shows
 * its own value, but which is still a reason the list is empty and so still has
 * to be named here. The search is not in this list: it has a state of its own,
 * quoting the term.
 */
export function narrowingRunFilters(
  kind: KindFilter,
  chips: RunsFilterChip[],
): RunFilterChip[] {
  const named = KIND_CHOICES.find((c) => c.value === kind);
  return [
    ...(kind === "all" || !named ? [] : [{ key: "kind", label: named.label }]),
    ...chips.map((c) => ({ key: c.key, label: c.label })),
  ];
}

/**
 * The values a repo's runs actually carry for one searchable field, as the
 * options its filter offers (MAIN-558 AC-3).
 *
 * From the rows rather than from a registry, so the control never lists a
 * person or a branch this repo has no run for. `current` is added back in case
 * the rows moved under a filter that is already set: a URL naming an initiator
 * must still show that initiator selected, and its chip must still be
 * removable, even after the last run of theirs left the list.
 */
export function fieldValues(
  rows: RunRow[] | null,
  field: "initiator" | "branch",
  current: string,
): string[] {
  const seen = new Set((rows ?? []).map((r) => r[field]).filter((v): v is string => !!v));
  if (current) seen.add(current);
  return [...seen].sort();
}

/**
 * The narrowest pane the runs browser is DESIGNED for (MAIN-556 AC-8).
 *
 * Derived, not chosen: it is the sum of the row grid's reserved tracks — the
 * kind badge, the state column wide enough for the loop's longest state word,
 * the gaps and the list's padding — plus a floor for the identifier, which is
 * the one flexible track. At exactly this width every part of line 1 still
 * fits without any of it truncating.
 *
 * Below it the SECONDARY line is what gives way (a container query in
 * `global.css` hides it); the identifier and the whole state word survive,
 * because those two are what a scan is for. Rows do not get taller when it
 * goes: the grid's two rows are fixed tracks, so the row keeps its height and
 * the list keeps its rhythm.
 *
 * `--nook-runs-min-pane` in `global.css` is the same number in the place that
 * can act on it; `WorkspaceRunsStyles.test.ts` fails if the two drift.
 *
 * MAIN-559 moved it: the row reserves a fourth track for its `…` button, and
 * this number is the sum of what the row reserves. It grew by that track and
 * its gap, not by a fresh guess.
 */
export const RUNS_MIN_PANE_PX = 284;

/** How long ago a run was raised, or "" for a timestamp that will not parse —
 *  a row with a broken date should lose its age, not its whole line. */
export function runAge(createdAt: string, now: number): string {
  const at = Date.parse(createdAt);
  return Number.isNaN(at) ? "" : shortAge(Math.max(0, now - at));
}

/** Line 2's full text, for the `title` a truncated cell needs (AC-3). */
export function rowSecondary(row: RunRow): string {
  return [row.meta, row.reason].filter(Boolean).join(" · ");
}

/**
 * The pull request a review run is about, as a URL a browser can open (AC-2).
 *
 * `https://github.com/{owner}/{repo}/pull/{n}` — the same literal the control
 * plane builds every time it names a PR (`merge_reconcile::pr_web_url`,
 * `pr_hygiene`, `jobs`). Mirrored here because no field carries the URL and
 * NG-4 forbids adding one.
 *
 * Null for anything this cannot read a github.com owner/repo out of, and that
 * is the point: the action is then absent rather than pointing somewhere that
 * does not exist. A guessed path for some other forge would be a link that
 * looks right and 404s.
 */
export function prWebUrl(remote: string | null | undefined, number?: number | null): string | null {
  if (!remote || !number) return null;
  const m = /^(?:https?:\/\/|ssh:\/\/git@|git@)github\.com[/:]([^/]+)\/(.+?)(?:\.git)?\/?$/.exec(
    remote.trim(),
  );
  return m ? `https://github.com/${m[1]}/${m[2]}/pull/${number}` : null;
}

/** This run's own address, for Copy link (AC-2) — the section and the run, on
 *  top of whatever the URL already carries. Relative: the caller puts the
 *  origin in front, because only it knows which one. */
export function runHref(pathname: string, search: string, id: string): string {
  const p = new URLSearchParams(search);
  p.set("section", RUNS_SECTION);
  p.set("run", id);
  return `${pathname}?${p.toString()}`;
}

/** When the run was raised, on the reader's clock (AC-7). The exact instant
 *  stays on the `title`, as it does on a row. */
export function runStarted(createdAt: string): string {
  const at = new Date(createdAt);
  return Number.isNaN(at.getTime()) ? "" : at.toLocaleTimeString();
}

/**
 * What went wrong, in the server's own words (AC-4).
 *
 * The control plane's error body is `{"error": "..."}` and that sentence is the
 * whole point of surfacing anything: "only a failed or canceled job can be
 * re-run" tells a reader what to do, and a generic replacement does not. The
 * status line is the fallback for a failure with no body at all.
 */
export function apiFailureText(error: unknown): string {
  const said = (error as { error?: unknown } | null | undefined)?.error;
  if (typeof said === "string" && said.trim()) return said;
  return "the request failed";
}

/** The section id this panel lives under, and the two ids it replaced. */
export const RUNS_SECTION = "runs";
const LEGACY_SECTIONS: Record<string, RunKind> = { builds: "build", reviews: "review" };

/**
 * Land an old `?section=builds` / `?section=reviews` link on the Runs section
 * with that kind pre-selected (AC-6).
 *
 * A redirect rather than a drop: those links are in card comments, in docs and
 * in browser history, and without one they resolve to no section at all — the
 * page silently shows Checkouts, which reads as the link being wrong.
 */
export function useLegacyRunsSectionRedirect(): void {
  const [params, setParams] = useSearchParams();
  const kind = LEGACY_SECTIONS[params.get("section") ?? ""];
  useEffect(() => {
    if (!kind) return;
    const next = new URLSearchParams(params);
    next.set("section", RUNS_SECTION);
    next.set("kind", kind);
    // Replace: the old URL is not a step somebody wants to back-button onto,
    // and it would bounce straight back here.
    setParams(next, { replace: true });
    // Keyed on the kind alone: `params` is a fresh object every render, and
    // once the redirect lands there is no legacy section left to match.
  }, [kind]);
}

/** How many runs one page of either walk asks for — the server's own default,
 *  named here because the frontier below reasons about page boundaries. */
const RUNS_PAGE_SIZE = 50;

/** What the panel needs of the two walks: the merged rows, and everything the
 *  bottom of the list has to say about them. */
type PagedRuns = {
  runs: RunRow[];
  /** The FIRST page of either walk is in flight and nothing is renderable. */
  loading: boolean;
  /** What the server said, in its own words — null while both walks are well. */
  error: string | null;
  /** More history to reach, so the list has not ended (AC-4). */
  hasMore: boolean;
  loadingMore: boolean;
  loadMore(): void;
  retry(): void;
};

/**
 * This repo's runs, both kinds, walked to wherever the reader has scrolled
 * (AC-1).
 *
 * Two infinite queries rather than one, because the wire is two endpoints
 * (MAIN-488) each with its own cursor (MAIN-557). `mergeRuns` puts them in one
 * order, as it always has; `runsPaging` says how far down that order the two
 * walks can vouch for, which is what stops a later page inserting rows above
 * rows already on screen.
 *
 * The keys keep the `["workspace-builds"|"workspace-reviews", workspaceId]`
 * prefix `live.ts` invalidates, so a `job_changed` frame still repaints this
 * without knowing it has become paged. Invalidating an infinite query refetches
 * every page it holds, in order, deriving each cursor from the page before it —
 * which is exactly what keeps a run raised mid-scroll from duplicating a row or
 * opening a gap (AC-3).
 *
 * `"paged"` on the end is not decoration: `useWorkspaceBuilds` and `BuildLoop`
 * read the bare key expecting a plain `BuildRun[]`, and an infinite query's
 * cache entry is pages. Two entries, one prefix, one invalidation — and this
 * panel already mounts the un-paged one through `useBuildRunFacts`, so the
 * split costs no request it was not already making.
 */
function usePagedRuns(workspaceId: string): PagedRuns {
  const builds = useInfiniteQuery({
    queryKey: ["workspace-builds", workspaceId, "paged"],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{id}/builds", {
        params: {
          path: { id: workspaceId },
          query: { after: pageParam, limit: RUNS_PAGE_SIZE },
        },
      });
      // THROWN, not defaulted. `openapi-fetch` hands a failure back as a
      // data-less `{ error }`, and the `?? []` this replaces is precisely how a
      // control plane that was down rendered as "this repo has never run
      // anything" (AC-5).
      if (error) throw new Error(apiFailureText(error));
      return (data ?? { rows: [], next_cursor: null }) as Page<BuildRun>;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const reviews = useInfiniteQuery({
    queryKey: ["workspace-reviews", workspaceId, "paged"],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{id}/reviews", {
        params: {
          path: { id: workspaceId },
          query: { after: pageParam, limit: RUNS_PAGE_SIZE },
        },
      });
      if (error) throw new Error(apiFailureText(error));
      return (data ?? { rows: [], next_cursor: null }) as Page<ReviewRun>;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });

  const buildRows = useMemo(
    () => (builds.data?.pages ?? []).flatMap((p) => p.rows),
    [builds.data],
  );
  const reviewRows = useMemo(
    () => (reviews.data?.pages ?? []).flatMap((p) => p.rows),
    [reviews.data],
  );

  /** How far each walk has been read. A walk that has not ANSWERED is not a
   *  walk that has ENDED: `hasNextPage` is false before the first page lands,
   *  and taking that for the end would let the frontier vouch for rows nothing
   *  has read. */
  const walks: RunWalk[] = useMemo(() => {
    const tail = (rows: { id: string; created_at: string }[]) => {
      const last = rows[rows.length - 1];
      return last ? { id: last.id, createdAt: last.created_at } : null;
    };
    return [
      { oldest: tail(buildRows), done: builds.isSuccess && !builds.hasNextPage },
      { oldest: tail(reviewRows), done: reviews.isSuccess && !reviews.hasNextPage },
    ];
  }, [
    buildRows,
    reviewRows,
    builds.isSuccess,
    builds.hasNextPage,
    reviews.isSuccess,
    reviews.hasNextPage,
  ]);

  // ONE merge: `mergeRuns` decides the order and the row shape, and
  // `withinFrontier` only cuts it. A second merge here would be a second order
  // to keep in step with this one.
  const runs = useMemo(
    () => withinFrontier(mergeRuns(buildRows, reviewRows), walks),
    [buildRows, reviewRows, walks],
  );
  const failure = builds.error ?? reviews.error;

  return {
    runs,
    loading: builds.isPending || reviews.isPending,
    error: failure ? failure.message : null,
    hasMore: walksHaveMore(walks),
    loadingMore: builds.isFetchingNextPage || reviews.isFetchingNextPage,
    loadMore: () => {
      for (const i of walksToAdvance(walks)) {
        void (i === 0 ? builds : reviews).fetchNextPage();
      }
    },
    // Both, unconditionally: the reader asked for the list, not for whichever
    // half of it happened to fail.
    retry: () => {
      void builds.refetch();
      void reviews.refetch();
    },
  };
}

export function WorkspaceRuns({
  workspaceId,
  workspaceName,
}: {
  workspaceId: string;
  /** The workspace's name, for the export filename (MAIN-471 AC-2). */
  workspaceName?: string;
}) {
  const [params, setParams] = useSearchParams();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const location = useLocation();
  const { openAt, refresh } = useContextMenuApi();
  // WHICH run is open lives in the URL now that a row can be copied as a link
  // (AC-2): a "Copy link" whose link did not select the run would be a lie, and
  // the only way to make it true is for the URL to be what selects.
  const openId = params.get("run");
  // Where the KEYBOARD is in the list, which is not the same as which run is
  // open (AC-9): arrowing moves this, Enter is what opens. Null until somebody
  // arrows, so tabbing in lands on the run the pane is already showing rather
  // than at the top of a list they have scrolled away from.
  const [cursorId, setCursorId] = useState<string | null>(null);
  // Requests this client has sent and not yet seen answered, by run (AC-4).
  // A map rather than one slot: cancelling two runs is two independent waits,
  // and a single slot would make the second one disable the first.
  const [busy, setBusy] = useState<Record<string, "cancel" | "rerun">>({});
  // The last refusal, in the server's own words (AC-4). Held until dismissed or
  // superseded: an error that vanished on the next repaint would be an error
  // nobody read.
  const [failure, setFailure] = useState<string | null>(null);
  // EVERY dimension lives in the URL (AC-7), not just the kind an old link
  // arrives carrying: a filtered view that cannot be refreshed or pasted to
  // somebody else exists only in the tab it was made in. It is also what makes
  // AC-8 free — a live repaint cannot reset a filter that was never state.
  const filter = parseRunsFilter(params);
  const kind = filter.kind;
  const listRef = useRef<HTMLDivElement>(null);
  /** The box that actually scrolls — the observer's root, and the node whose
   *  `scrollTop` a page must not disturb (MAIN-560 AC-2). `listRef` is the
   *  options container inside it, which is what focus is looked up through. */
  const scrollRef = useRef<HTMLDivElement>(null);
  /** The bottom of the list — what the observer watches to page it (AC-1).
   *
   *  State rather than a ref, because the observer has to be attached when this
   *  node ARRIVES: it is mounted a render after the first page lands, and a
   *  ref's mutation is not something an effect can depend on. With a ref this
   *  read `null` on the one render whose dependencies changed, and then never
   *  ran again — an infinite scroll that never scrolled. */
  const [moreNode, setMoreNode] = useState<HTMLDivElement | null>(null);

  const paged = usePagedRuns(workspaceId);

  // Shared key with `useBuildRunFacts`, so a panel that already resolved this
  // workspace pays nothing for it here. The remote is the only field wanted:
  // it is what a review row's PR link is built from.
  const { data: workspace } = useQuery({
    queryKey: ["workspaces", workspaceId],
    queryFn: async () =>
      (await api.GET("/api/v1/workspaces/{id}", { params: { path: { id: workspaceId } } }))
        .data ?? null,
  });

  const runs = paged.runs;
  // Read ONCE per render, so every row is judged against the same instant: a
  // relative range re-read per row could admit one and reject its neighbour.
  const now = Date.now();
  const visible = filterRuns(runs, filter, now);
  const initiators = fieldValues(runs, "initiator", filter.initiator);
  const branches = fieldValues(runs, "branch", filter.branch);
  // The loop's own word for a state on the chip, not the stored value: a chip
  // reading `waiting_on_human` beside a pill reading "waiting on human" would
  // be two vocabularies for one idea.
  const chips = runsFilterChips(filter, (st) => jobStateMeta(st).label);
  // The open run if the filter still shows it, else the newest one that is —
  // so narrowing the list never leaves the transcript of a hidden run beside it.
  const openRun = visible.find((r) => r.id === openId) ?? visible[0] ?? null;
  const open = openRun?.id ?? null;
  // The open run's commands (MAIN-530 AC-6): the same list, from the same
  // endpoint, that the loop page and the ticket's panel read. The composer
  // below stays hidden — a managed run is the control plane's work, not a
  // conversation somebody steers (MAIN-488), and opening it is not this card's
  // change — so this panel serves the surface without offering a box to type
  // prose into.
  const { commands, onCommand } = useAgentCommands("run", open);

  // What a BUILD run's branch is (AC-7). Shared queries again — `BuildOutcome`
  // below mounts the same hook, so the header and the strip cannot name
  // different branches for one run.
  const facts = useBuildRunFacts(openRun?.kind === "build" ? open : null, workspaceId);

  // ── The live handle (AC-5) ────────────────────────────────────────────────
  // A menu is built when it is READ, not when it was opened, and everything it
  // reads comes through here. Held in a ref because an open menu is portalled
  // out of this subtree: it does not re-render when the list repaints, so a
  // closure captured at right-click time would go on describing a run that has
  // since finished. This is the race AC-5 names.
  const live = useRef({
    runs: [] as RunRow[],
    busy: {} as Record<string, "cancel" | "rerun">,
    remote: null as string | null,
    act: (async (_id: string, _action: RunActionId) => {}) as (
      id: string,
      action: RunActionId,
    ) => void | Promise<void>,
  });

  /**
   * Narrow the list (AC-6, AC-7). One writer for all six dimensions, so they
   * cannot drift into six ways of not clearing each other, and so a change to
   * one leaves the run selection and every other dimension exactly as they were.
   *
   * `replace` for the same reason `chooseRun` is: narrowing a list in place is
   * not a navigation, and typing six characters into the search box must not
   * cost six presses of the back button to undo.
   */
  const setFilter = (next: RunsFilter) => {
    setParams(writeRunsFilter(new URLSearchParams(params), next), { replace: true });
  };

  /** Open a run: the URL is what selects, so this is a `replace` like the
   *  filters — reading down a list is not a stack of navigations. */
  const chooseRun = (id: string) => {
    const p = new URLSearchParams(params);
    p.set("run", id);
    setParams(p, { replace: true });
    setCursorId(id);
  };

  /**
   * Put focus back on a run's row (AC-8).
   *
   * By id and through the DOM rather than by index, because the list re-sorts
   * under an action: a re-run inserts a new row above this one, and an index
   * remembered before the call would land on a different run. Falls back to the
   * first row, and never to a node that has gone.
   */
  const focusRun = (id: string) => {
    const rows = [...(listRef.current?.querySelectorAll<HTMLElement>("[data-run-id]") ?? [])];
    (rows.find((n) => n.dataset.runId === id) ?? rows[0])?.focus();
  };

  const invalidate = () =>
    Promise.all([
      qc.invalidateQueries({ queryKey: ["workspace-builds", workspaceId] }),
      qc.invalidateQueries({ queryKey: ["workspace-reviews", workspaceId] }),
      qc.invalidateQueries({ queryKey: ["job"] }),
    ]);

  const send = async (id: string, action: "cancel" | "rerun") => {
    setFailure(null);
    setBusy((b) => ({ ...b, [id]: action }));
    try {
      const path = action === "cancel" ? "/api/v1/jobs/{id}/cancel" : "/api/v1/jobs/{id}/rerun";
      const { error } = await api.POST(path, { params: { path: { id } } });
      if (error) throw new Error(apiFailureText(error));
      await invalidate();
    } catch (e) {
      // VERBATIM (AC-4). The server refuses in a sentence naming the reason;
      // replacing it with "something went wrong" throws that away.
      setFailure(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy((b) => {
        const next = { ...b };
        delete next[id];
        return next;
      });
      // The selection is untouched by any of this — only focus moves, and it
      // moves back to the row that was acted on.
      focusRun(id);
    }
  };

  const doCancel = async (id: string) => {
    const row = live.current.runs.find((r) => r.id === id);
    if (!row) return;
    const stale = cancelRefusal(row.state, !!live.current.busy[id]);
    if (stale) {
      setFailure(stale);
      focusRun(id);
      return;
    }
    const { title, description } = cancelPrompt(row);
    const go = await askConfirm({
      title,
      description,
      confirmLabel: "cancel run",
      danger: true,
    });
    if (!go) {
      focusRun(id);
      return;
    }
    // Asked AGAIN after the dialog: it was open for as long as somebody took to
    // read it, and a run that finished in that time must not be cancelled on
    // the strength of a question about a state it has left.
    const now = live.current.runs.find((r) => r.id === id);
    const after = now ? cancelRefusal(now.state, !!live.current.busy[id]) : CANCEL_ENDED_REFUSAL;
    if (after) {
      setFailure(after);
      focusRun(id);
      return;
    }
    await send(id, "cancel");
  };

  const doRerun = async (id: string) => {
    const row = live.current.runs.find((r) => r.id === id);
    if (!row) return;
    const stale = rerunRefusal(row.state, !!live.current.busy[id]);
    if (stale) {
      setFailure(stale);
      focusRun(id);
      return;
    }
    await send(id, "rerun");
  };

  const copy = (text: string) => {
    // Best effort: a browser that refuses the clipboard is not an error worth
    // interrupting a list for, and there is nothing to retry.
    void navigator.clipboard?.writeText?.(text);
  };

  const runAction = async (id: string, action: RunActionId) => {
    const row = live.current.runs.find((r) => r.id === id);
    if (!row) return;
    switch (action) {
      case "open":
        chooseRun(id);
        focusRun(id);
        return;
      case "copy-id":
        copy(id);
        return;
      case "copy-link":
        copy(window.location.origin + runHref(location.pathname, location.search, id));
        return;
      case "view-task":
        if (row.taskRef) navigate(`/loop/${row.taskRef}`);
        return;
      case "view-pr": {
        const url = prWebUrl(live.current.remote, row.prNumber);
        if (url) window.open(url, "_blank", "noreferrer");
        return;
      }
      case "cancel":
        await doCancel(id);
        return;
      case "rerun":
        await doRerun(id);
        return;
    }
  };

  live.current = {
    runs: runs ?? [],
    busy,
    remote: workspace?.git_remote_url ?? null,
    act: runAction,
  };

  /**
   * One run's menu, as a LIVE source (AC-5).
   *
   * Returns a function, not a list: the context menu calls it every time it
   * renders, so a run that finishes under an open menu drops Cancel from it
   * rather than leaving an action that would now be refused. `only: "overflow"`
   * is the detail header's menu, which is the same list minus the action the
   * header is already showing as a button (AC-7).
   *
   * Stable across renders, and reads everything through `live` — so the two
   * routes into a menu, the right-click region and the `…` button, hand over
   * the very same source and cannot be one fresh and one stale.
   */
  const menuFor = useCallback(
    (id: string, only?: "overflow") =>
      (): ContextMenuItem[] => {
        const l = live.current;
        const row = l.runs.find((r) => r.id === id);
        if (!row) return [];
        const all = runActions(row, {
          pending: !!l.busy[id],
          taskHref: row.taskRef ? `/loop/${row.taskRef}` : null,
          prHref: prWebUrl(l.remote, row.prNumber),
        });
        return (only === "overflow" ? overflowRunActions(all) : all).map((a) => ({
          label: a.label,
          danger: a.danger,
          // An action the server would refuse is shown, disabled, WITH the
          // reason (AC-6): leaving it out is indistinguishable from an
          // oversight, and teaches nobody why it is not there.
          disabled: !!a.refusal,
          hint: a.refusal,
          onSelect: () => void l.act(id, a.id),
        }));
      },
    [],
  );

  const openMenuAt = (id: string, anchor: HTMLElement, only?: "overflow") => {
    const r = anchor.getBoundingClientRect();
    openAt(r.left, r.bottom, menuFor(id, only));
  };

  // Tell an OPEN menu that what it describes has moved (AC-5). The list
  // repainting is not something a portalled menu hears about, so the panel that
  // does hear it says so.
  const liveSignature = [
    (runs ?? []).map((r) => `${r.id}:${r.state}`).join(","),
    Object.entries(busy)
      .map(([k, v]) => `${k}:${v}`)
      .join(","),
  ].join("|");
  useEffect(() => {
    refresh();
  }, [liveSignature, refresh]);

  const { data: detail } = useQuery({
    queryKey: ["job", open],
    enabled: !!open,
    queryFn: async () =>
      (
        await api.GET("/api/v1/jobs/{id}", { params: { path: { id: open as string } } })
      ).data as
        | { kind?: string; transcript?: LoopJobTranscriptEntry[] }
        | undefined,
  });

  // The run the URL names, when this list does not hold it. ASKED about rather
  // than concluded from the rows on screen: once the list is paged, a run can
  // be perfectly real and simply not walked to yet, so "not among the rows" is
  // emphatically not "gone" (MAIN-560 AC-5). The server is the only thing that
  // knows.
  const unlisted = openId && !runs.some((r) => r.id === openId) ? openId : null;
  const probe = useQuery({
    queryKey: ["job", unlisted],
    enabled: !!unlisted,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/jobs/{id}", {
        params: { path: { id: unlisted as string } },
      });
      if (error) throw new Error(apiFailureText(error));
      return data ?? null;
    },
  });
  const goneRun = !!unlisted && probe.isError;

  /** Auto-page as the bottom of the list comes into view (AC-1).
   *
   *  Held off while a page is already in flight and while one has FAILED: an
   *  observer that re-fired on its own error is the infinite retry NG-4
   *  forbids, and the failed row below offers the reader the retry instead. */
  const loadMoreLive = useRef(paged.loadMore);
  loadMoreLive.current = paged.loadMore;
  const autoPage = paged.hasMore && !paged.loadingMore && !paged.error;
  useEffect(() => {
    // Guarded rather than polyfilled: jsdom has no observer, and the button
    // inside the same row is the route that does not need one.
    if (!moreNode || !autoPage || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) loadMoreLive.current();
      },
      { root: scrollRef.current },
    );
    io.observe(moreNode);
    return () => io.disconnect();
  }, [moreNode, autoPage]);

  // What is narrowing the list, when the answer to "why is this empty" has to
  // be more than "it is" (AC-6).
  const narrowing = narrowingRunFilters(kind, chips);
  const phase: RunsPhase = runsPhase({
    loading: paged.loading,
    error: !!paged.error,
    shown: visible.length,
    search: filter.q,
    filters: narrowing,
  });

  /** Back to the unfiltered list in one act (AC-6). Wider than the chip row's
   *  `Clear all`, which keeps the kind and the search on purpose (MAIN-558
   *  AC-5): a reader looking at an empty list wants the list back, and the kind
   *  is one of the reasons it is empty. The search keeps its own control. */
  const clearFilters = () => setFilter({ ...clearRunsFilters(filter), kind: "all" });
  const clearSearch = () => setFilter({ ...filter, q: "" });

  /** Out of the gone state: drop the run the URL names and let the ordinary
   *  fallback take over. Not a jump to a chosen id — the newest run is whatever
   *  the list says it is, and naming one here would go stale as it repaints. */
  const showNewestRun = () => {
    const p = new URLSearchParams(params);
    p.delete("run");
    setParams(p, { replace: true });
  };

  const listState = (
    <RunsState
      phase={phase}
      search={filter.q}
      filters={narrowing}
      error={paged.error}
      onRetry={paged.retry}
      onClearSearch={clearSearch}
      onClearFilters={clearFilters}
    />
  );

  // Three states have no list to sit beside and no filter worth offering: the
  // repo has never run anything, the load failed outright, or nothing has
  // arrived yet. They take the whole panel, as the empty state always did —
  // there is nothing here to narrow (MAIN-556 AC-9).
  if (phase === "loading" || phase === "error" || phase === "empty") {
    return <Panel title="Runs">{listState}</Panel>;
  }

  const pickKind = (next: KindFilter) => setFilter({ ...filter, kind: next });

  // Roving focus inside the segmented control, which is what makes a
  // radiogroup one tab stop instead of three.
  const onKindKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const step =
      e.key === "ArrowRight" || e.key === "ArrowDown"
        ? 1
        : e.key === "ArrowLeft" || e.key === "ArrowUp"
          ? -1
          : 0;
    if (!step) return;
    e.preventDefault();
    const at = KIND_CHOICES.findIndex((c) => c.value === kind);
    const next = (at + step + KIND_CHOICES.length) % KIND_CHOICES.length;
    pickKind(KIND_CHOICES[next].value);
    e.currentTarget.querySelectorAll<HTMLElement>('[role="radio"]')[next]?.focus();
  };

  const cursorAt = Math.max(
    0,
    visible.findIndex((r) => r.id === (cursorId ?? open)),
  );

  const onListKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    // The `…` button is inside a row and owns its own keys: without this,
    // Enter on it would open the menu AND open the run underneath.
    if (e.target instanceof HTMLElement && e.target.closest(".runs-row-menu")) return;
    const step = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
    if (step) {
      e.preventDefault();
      const next = Math.min(visible.length - 1, Math.max(0, cursorAt + step));
      setCursorId(visible[next]?.id ?? null);
      listRef.current?.querySelectorAll<HTMLElement>('[role="option"]')[next]?.focus();
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      // A row is a <button>, so the browser would click it for us — but then
      // "move the cursor, then open it" and "open whatever has focus" would be
      // two paths to one act, and only one of them would ever be tested.
      e.preventDefault();
      const row = visible[cursorAt];
      if (row) chooseRun(row.id);
    }
  };

  return (
    <Panel title="Runs">
      <div className="reviews-split">
        {/* Three regions (AC-6): a toolbar that cannot scroll away because it
            is a grid track of its own, the list that scrolls inside the second
            track, and the transcript beside them. */}
        <div className="runs-browser">
          <div className="runs-toolbar">
            {/* Full width and first (AC-1): search is what somebody reaches for
                when they know WHICH run they want, and it filters as it is
                typed — there is no submit, because a list that only narrows
                when asked twice is a list nobody searches. */}
            <div className="runs-search">
              <Search size={12} aria-hidden="true" />
              <input
                className="input small runs-search-input"
                type="search"
                aria-label="search runs"
                placeholder="Search runs…"
                value={filter.q}
                onChange={(e) => setFilter({ ...filter, q: e.target.value })}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setFilter({ ...filter, q: "" });
                }}
              />
            </div>
            <div className="runs-controls">
              <div
                className="runs-kinds"
                role="radiogroup"
                aria-label="filter by kind"
                onKeyDown={onKindKey}
              >
                {KIND_CHOICES.map((c) => (
                  <button
                    key={c.value}
                    type="button"
                    role="radio"
                    aria-checked={kind === c.value}
                    tabIndex={kind === c.value ? 0 : -1}
                    className={`runs-kind${kind === c.value ? " is-on" : ""}`}
                    onClick={() => pickKind(c.value)}
                  >
                    {c.label}
                  </button>
                ))}
              </div>
              <RunsFilters
                value={filter}
                onChange={setFilter}
                initiators={initiators}
                branches={branches}
              />
            </div>
            {/* Beneath the toolbar's controls and inside its region (AC-5), so
                a chip appearing never pushes the list into a grid track of its
                own — the browser's second track is the only box that scrolls
                and it must stay the list. */}
            {chips.length > 0 && (
              <div className="runs-chips" data-testid="run-filter-chips">
                {chips.map((c) => (
                  <button
                    key={c.key}
                    type="button"
                    className="task-chip filter-chip on"
                    aria-label={`remove filter: ${c.label}`}
                    title="remove this filter"
                    onClick={() => setFilter(c.next)}
                  >
                    {c.label}
                    <span className="filter-chip-x" aria-hidden="true">
                      ×
                    </span>
                  </button>
                ))}
                {/* Only past one (AC-5): with a single chip it would do exactly
                    what the chip beside it already does, under a second name. */}
                {chips.length > 1 && (
                  <button
                    type="button"
                    className="btn small"
                    data-testid="run-filters-clear"
                    onClick={() => setFilter(clearRunsFilters(filter))}
                  >
                    Clear all
                  </button>
                )}
              </div>
            )}
          </div>
          {failure && (
            <div className="runs-failure" role="alert" data-testid="run-failure">
              <span className="runs-failure-text">{failure}</span>
              <button
                type="button"
                className="btn small"
                onClick={() => setFailure(null)}
                aria-label="dismiss this error"
              >
                dismiss
              </button>
            </div>
          )}
          <div className="runs-list" ref={scrollRef}>
            {visible.length === 0 ? (
              listState
            ) : (
              <div
                className="runs-options"
                role="listbox"
                aria-label="runs"
                ref={listRef}
                onKeyDown={onListKey}
              >
                {visible.map((r, i) => {
                  // `jobStateMeta` speaks the loop's tone vocabulary, which has
                  // a `muted` the design system spells `dim`. Mapped here
                  // rather than widened in the shared component, whose set is
                  // the design system's.
                  // While this client's cancel is in flight the row says so,
                  // in the state column, because that is where a reader looks
                  // for what a run is doing (AC-4).
                  const shown = shownState(r.state, busy[r.id]);
                  const meta = runStateMeta(shown);
                  const tone = pillTone(meta.tone);
                  const secondary = rowSecondary(r);
                  return (
                    // A region per row, not one for the list: the menu has to
                    // know WHICH run was right-clicked, and the nearest-region
                    // rule is what answers that without an event to inspect.
                    // `display: contents` keeps the wrapper out of the grid.
                    <ContextMenuRegion
                      key={r.id}
                      items={menuFor(r.id)}
                      style={{ display: "contents" }}
                    >
                      <div
                        role="option"
                        aria-selected={r.id === open}
                        tabIndex={i === cursorAt ? 0 : -1}
                        className={`runs-row${r.id === open ? " is-open" : ""}`}
                        onClick={() => chooseRun(r.id)}
                        data-testid="run-row"
                        data-run-id={r.id}
                        data-kind={r.kind}
                        data-reason-kind={r.reasonKind}
                        data-pending={busy[r.id]}
                      >
                        {/* The kind is a WORD, not a colour: colour in this row
                            is already the state's, and a second palette next to
                            it would make two claims a reader has to tell apart.
                            `role="img"` so the badge announces as one named thing
                            — an aria-label on a bare span is not owed to anyone.  */}
                        <span
                          className="pill dim runs-row-kind"
                          role="img"
                          aria-label={`kind: ${r.kind}`}
                        >
                          {r.kind}
                        </span>
                        <span className="mono runs-row-id" title={r.label}>
                          {r.label}
                        </span>
                        <span
                          className={`pill ${tone} runs-row-state`}
                          role="img"
                          aria-label={`state: ${meta.label}`}
                        >
                          <span className="runs-row-glyph">{stateGlyph(shown)}</span>
                          {meta.label}
                        </span>
                        {/* Line 2 is ONE cell, whatever it holds: the outcome, the
                            waiting sentence, or both. It is the row's only
                            truncating text and the first thing a narrow pane
                            takes away (AC-3, AC-8) — the row's height is the
                            grid's, so losing it costs no rhythm. */}
                        <span className="runs-row-meta" title={secondary || undefined}>
                          {r.meta ? <span className="mono">{r.meta}</span> : null}
                          {r.reason ? <span data-testid="run-reason">{r.reason}</span> : null}
                        </span>
                        <span className="runs-row-time" title={r.createdAt}>
                          {runAge(r.createdAt, now)}
                        </span>
                        {/* The same menu right-click opens, on a control that can
                            be seen and clicked (AC-1). Right-click is never the
                            only route to an action. It holds a reserved grid
                            track, so revealing it on hover moves nothing — the
                            row's shape is the same whether it is showing or not
                            (MAIN-556 AC-1). `tabIndex={-1}` keeps the list one
                            tab stop; Shift+F10 on the row is the keyboard's
                            route (AC-8). */}
                        <button
                          type="button"
                          className="runs-row-menu"
                          tabIndex={-1}
                          aria-haspopup="menu"
                          aria-label={`actions for ${r.label}`}
                          data-testid="run-actions"
                          onClick={(e) => {
                            e.stopPropagation();
                            chooseRun(r.id);
                            openMenuAt(r.id, e.currentTarget);
                          }}
                        >
                          <MoreHorizontal size={12} />
                        </button>
                      </div>
                    </ContextMenuRegion>
                  );
                })}
              </div>
            )}
            {/* The bottom of the history, which always says something (AC-2,
                AC-4). Inside the scrolling box on purpose: it is what an
                observer watches, and it is where a reader who has scrolled to
                the end is looking. */}
            {visible.length > 0 &&
              (paged.error ? (
                <div className="runs-more is-failed" role="alert" data-testid="runs-more-failed">
                  <span className="runs-more-text">{paged.error}</span>
                  <button
                    type="button"
                    className="btn small"
                    data-testid="runs-retry"
                    onClick={paged.retry}
                  >
                    try again
                  </button>
                </div>
              ) : paged.hasMore ? (
                <div className="runs-more" ref={setMoreNode} data-testid="runs-more">
                  {paged.loadingMore ? (
                    // A ROW, not an overlay: it is appended below what is
                    // already rendered, so nothing above it moves and the scroll
                    // position is the one the reader left (AC-2).
                    <span className="runs-more-text" role="status" data-testid="runs-loading-more">
                      loading more runs…
                    </span>
                  ) : (
                    // The observer above clicks this by scrolling; the button is
                    // what a keyboard, a screen reader, and a browser with no
                    // IntersectionObserver have instead.
                    <button
                      type="button"
                      className="runs-more-btn"
                      data-testid="runs-load-more"
                      onClick={() => paged.loadMore()}
                    >
                      load more
                    </button>
                  )}
                </div>
              ) : (
                // STATED (AC-4). A list that merely stops is indistinguishable
                // from one that has given up loading.
                <div className="runs-end" data-testid="runs-end">
                  That is every run this repo has kept.
                </div>
              ))}
          </div>
        </div>
        <div className="reviews-transcript" data-testid="run-transcript">
          {/* The run the URL names is gone, so this pane says so rather than
              quietly showing a different run's transcript under a header naming
              it — which is how a stale shared link used to read as the loop
              having run something nobody asked for (AC-5). */}
          {goneRun && <RunGone onShowNewest={showNewestRun} />}
          {/* Above the transcript, not inside it (MAIN-387 AC-7): the branch and
              the PR are what the open run PRODUCED, and reading a log to find
              them is the thing this replaces. */}
          {!goneRun && openRun && (
            <RunHeader
              run={openRun}
              pending={busy[openRun.id]}
              // A build's branch, a review's head: the same slot, filled from
              // whichever the kind actually has (AC-7).
              gitRef={openRun.kind === "build" ? facts.branch : openRun.commit || null}
              now={now}
              actions={runActions(openRun, {
                pending: !!busy[openRun.id],
                taskHref: openRun.taskRef ? `/loop/${openRun.taskRef}` : null,
                prHref: prWebUrl(workspace?.git_remote_url, openRun.prNumber),
              })}
              onAction={(a) => void runAction(openRun.id, a)}
              onOverflow={(el) => openMenuAt(openRun.id, el, "overflow")}
            />
          )}
          {!goneRun && open && (
            <BuildOutcome
              job={{ id: open, kind: openRun?.kind ?? "" }}
              workspaceId={workspaceId}
              // The header above owns the branch now; repeating it here would
              // be one fact rendered twice, one line apart.
              showBranch={false}
            />
          )}
          {goneRun ? null : detail?.transcript?.length ? (
            <>
              <div className="reviews-transcript-actions">
                <TranscriptActions
                  // The FULL transcript, not the folded view (AC-3): the fold
                  // is how the panel reads, never what an incident paste
                  // carries.
                  lines={detail.transcript}
                  filename={`${fileSlug(
                    [workspaceName, openRun?.label ?? "run"].filter(Boolean).join("-"),
                  ).toLowerCase()}-${(open ?? "").slice(0, 8)}.md`}
                />
              </div>
              <ChatView
                // A run's transcript, read as a transcript (MAIN-499): grouped
                // turns, and the folded activity as its own expandable kind.
                variant="transcript"
                // Folded like the Loop page folds it, so a ladder of `· Bash`
                // lines reads as one activity entry there and here alike.
                messages={transcriptMessages(foldToolActivity(detail.transcript))}
                // A live run says so, in the same words and with the same
                // indicator the ticket's loop view uses (AC-6). Silent for a
                // finished run — `agentActivityLabel` returns null there, which
                // is the whole reason this is not a `state === "running"` test.
                typing={openRun ? agentActivityLabel(openRun.state) : null}
                // Read-only on purpose: a managed run is the control plane's
                // work, not a conversation somebody steers. The composer is
                // HIDDEN, not disabled — there is nothing here to say anything
                // TO, and an inert box under every finished run was clutter that
                // read as broken.
                onSend={() => {}}
                hideComposer
                commands={commands}
                onCommand={onCommand}
                conversationId={open ?? undefined}
              />
            </>
          ) : (
            <Empty>This run has not said anything yet.</Empty>
          )}
        </div>
      </div>
    </Panel>
  );
}

/**
 * Everything that is not the kind or the search text, behind one button
 * (MAIN-558 AC-3).
 *
 * A popover rather than four more controls in the toolbar: the browser column
 * is 284px at its designed minimum (`RUNS_MIN_PANE_PX`) and four dropdowns do
 * not fit across it — but more than that, state, initiator, branch and a date
 * range are the questions somebody asks OCCASIONALLY, and a toolbar that shows
 * every occasional control is what MAIN-556 took the detached dropdowns out of.
 * What is always visible instead is the COUNT (AC-4) and the chips (AC-5), so a
 * closed popover never hides that a filter is on.
 *
 * The same shape, and the same classes, as the board's filter strip (MAIN-110):
 * one popover, `task-chip` toggles inside it, a `filter-badge` on the trigger.
 */
function RunsFilters({
  value,
  onChange,
  initiators,
  branches,
}: {
  value: RunsFilter;
  onChange(next: RunsFilter): void;
  /** Offered from what actually ran — see `fieldValues`. */
  initiators: string[];
  branches: string[];
}) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  // Anchored out of the browser column, which scrolls and would clip it.
  const { hostRef, portal } = useAnchoredMenu(open, close, { height: 360, width: 300 });
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  const count = activeRunsFilterCount(value);
  const toggleState = (s: string) =>
    onChange({
      ...value,
      states: value.states.includes(s)
        ? value.states.filter((x) => x !== s)
        : [...value.states, s],
    });
  // The range is ONE dimension in two shapes, so choosing either clears the
  // other — see `RunsFilter.since`.
  const pickSince = (since: string) => onChange({ ...value, since, from: "", to: "" });
  const pickDay = (edge: "from" | "to", day: string) =>
    onChange({ ...value, [edge]: day, since: "" });

  return (
    <>
      <div ref={hostRef} className="runs-filters-host">
        <button
          type="button"
          className="btn small"
          aria-expanded={open}
          aria-haspopup="dialog"
          data-testid="run-filters"
          onClick={() => setOpen((o) => !o)}
        >
          <Filter size={11} /> Filters
          {count > 0 && (
            <span className="filter-badge" data-testid="run-filter-count">
              {count}
            </span>
          )}
        </button>
      </div>
      {portal(
        <div
          className="filters-popover"
          role="dialog"
          aria-label="run filters"
          onMouseDown={(e) => e.stopPropagation()}
        >
          <div className="filters-group">
            <span className="faint small">state</span>
            {/* Several at once (AC-3), OR&apos;d: "what is queued or running"
                is one question, and asking it twice is not the same thing. */}
            <div className="filters-chips">
              {RUN_STATES.map((s) => {
                const on = value.states.includes(s);
                return (
                  <button
                    key={s}
                    type="button"
                    className={`task-chip${on ? " on" : ""}`}
                    aria-pressed={on}
                    onClick={() => toggleState(s)}
                  >
                    {jobStateMeta(s).label}
                  </button>
                );
              })}
            </div>
          </div>

          <div className="filters-group filters-row">
            <label className="filters-field">
              <span className="faint small">initiator</span>
              <select
                className="task-select"
                value={value.initiator}
                onChange={(e) => onChange({ ...value, initiator: e.target.value })}
              >
                <option value="">anyone</option>
                {initiators.map((i) => (
                  <option key={i} value={i}>
                    {i}
                  </option>
                ))}
              </select>
            </label>
            <label className="filters-field">
              <span className="faint small">branch</span>
              <select
                className="task-select"
                value={value.branch}
                onChange={(e) => onChange({ ...value, branch: e.target.value })}
              >
                <option value="">any branch</option>
                {branches.map((b) => (
                  <option key={b} value={b}>
                    {b}
                  </option>
                ))}
              </select>
            </label>
          </div>

          <div className="filters-group">
            <span className="faint small">raised</span>
            <div className="filters-chips">
              {RAISED_PRESETS.map((p) => (
                <button
                  key={p.value}
                  type="button"
                  className={`task-chip${value.since === p.value ? " on" : ""}`}
                  aria-pressed={value.since === p.value}
                  onClick={() => pickSince(value.since === p.value ? "" : p.value)}
                >
                  {p.label}
                </button>
              ))}
            </div>
            <div className="filters-row">
              <label className="filters-field">
                <span className="faint small">raised after</span>
                <input
                  className="input small"
                  type="date"
                  value={value.from}
                  onChange={(e) => pickDay("from", e.target.value)}
                />
              </label>
              <label className="filters-field">
                <span className="faint small">raised before</span>
                <input
                  className="input small"
                  type="date"
                  value={value.to}
                  onChange={(e) => pickDay("to", e.target.value)}
                />
              </label>
            </div>
          </div>
        </div>,
        "filters-popover-host",
      )}
    </>
  );
}

/**
 * The open run's header (AC-7): what it is, what state it is in, what it is
 * working on, when it started — and the ONE action that state permits, with
 * everything else behind an overflow.
 *
 * Compact on purpose. It sits above a transcript, which is the thing somebody
 * came here to read; a header that grew a row of buttons would push it down the
 * screen to say what a row already said. Copy and Export stay where they were,
 * below this and beside the transcript they act on.
 */
function RunHeader({
  run,
  pending,
  gitRef,
  now,
  actions,
  onAction,
  onOverflow,
}: {
  run: RunRow;
  pending?: "cancel" | "rerun";
  /** The branch a build is on, or the head a review was raised for. */
  gitRef: string | null;
  now: number;
  actions: RunAction[];
  onAction(action: RunActionId): void;
  onOverflow(anchor: HTMLElement): void;
}) {
  const shown = shownState(run.state, pending);
  const meta = runStateMeta(shown);
  const primary = primaryRunAction(actions);
  const elapsed = runAge(run.createdAt, now);
  const started = runStarted(run.createdAt);
  return (
    <div className="run-header" data-testid="run-header">
      <span className="pill dim runs-row-kind" role="img" aria-label={`kind: ${run.kind}`}>
        {run.kind}
      </span>
      <span className="mono bright run-header-id" title={run.label}>
        {run.label}
      </span>
      <span className={`pill ${pillTone(meta.tone)}`} role="img" aria-label={`state: ${meta.label}`}>
        <span className="runs-row-glyph">{stateGlyph(shown)}</span>
        {meta.label}
      </span>
      {gitRef && (
        <span className="mono faint small" data-testid="run-header-ref" title={gitRef}>
          <GitBranch size={11} /> {gitRef}
        </span>
      )}
      {started && (
        <span className="faint small" data-testid="run-header-started" title={run.createdAt}>
          started {started}
        </span>
      )}
      {elapsed && (
        <span
          className="faint small"
          data-testid="run-header-elapsed"
          title={`${elapsed} since this run was raised`}
        >
          {elapsed}
        </span>
      )}
      <span className="run-header-actions">
        {primary && (
          <button
            type="button"
            className={`btn small${primary.danger ? "" : " primary"}`}
            // Disabled with its reason on the button rather than absent, for
            // the same reason the menu keeps it (AC-6).
            disabled={!!primary.refusal}
            title={primary.refusal ?? undefined}
            data-testid="run-primary-action"
            onClick={() => onAction(primary.id)}
          >
            {primary.label}
          </button>
        )}
        <button
          type="button"
          className="btn small"
          aria-haspopup="menu"
          aria-label={`more actions for ${run.label}`}
          data-testid="run-header-overflow"
          onClick={(e) => onOverflow(e.currentTarget)}
        >
          <MoreHorizontal size={12} />
        </button>
      </span>
    </div>
  );
}
