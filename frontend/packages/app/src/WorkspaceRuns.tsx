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
import React, { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import { api, type LoopJobTranscriptEntry } from "@nookos/api";
import { ChatView, Empty, Panel, Pill } from "@nookos/ui";
import { transcriptMessages } from "./LoopPanel";
import { agentActivityLabel, foldToolActivity, jobStateMeta } from "./loop";
import { fileSlug, TranscriptActions } from "./transcriptExport";

/** What raised a run. The word a row shows, and the value the URL carries. */
export type RunKind = "build" | "review";

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
};

/** The loop's state tones, in the design system's words. */
export function pillTone(
  tone: "info" | "warn" | "err" | "ok" | "muted",
): "ok" | "warn" | "err" | "info" | "dim" {
  return tone === "muted" ? "dim" : tone;
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
  };
}

/** Both kinds in one list, newest first (AC-3). The id breaks a tie so two runs
 *  raised in the same instant keep a stable order across repaints. */
export function mergeRuns(builds: BuildRun[], reviews: ReviewRun[]): RunRow[] {
  return [...builds.map(buildRow), ...reviews.map(reviewRow)].sort((a, b) => {
    const delta = Date.parse(b.createdAt) - Date.parse(a.createdAt);
    return delta !== 0 ? delta : a.id.localeCompare(b.id);
  });
}

/** The kind filter's value, where "all" is the absence of a filter. */
export type KindFilter = RunKind | "all";

export function parseKind(raw: string | null): KindFilter {
  return raw === "build" || raw === "review" ? raw : "all";
}

export function filterRuns(rows: RunRow[], kind: KindFilter, state: string): RunRow[] {
  return rows.filter(
    (r) => (kind === "all" || r.kind === kind) && (state === "all" || r.state === state),
  );
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

// Both queries below repaint from the live `job_changed` event, which
// `live.ts` turns into an invalidation of `["job"]` plus these two row keys
// — the same mechanism the Loop panel rides, rather than a poll of its own.
// The keys are unchanged by the merge, so the event still reaches both halves
// and `BuildLoop` still shares the builds cache entry.
export function WorkspaceRuns({
  workspaceId,
  workspaceName,
}: {
  workspaceId: string;
  /** The workspace's name, for the export filename (MAIN-471 AC-2). */
  workspaceName?: string;
}) {
  const [params, setParams] = useSearchParams();
  const [openId, setOpenId] = useState<string | null>(null);
  // The kind lives in the URL because an old link arrives carrying one; the
  // state filter is a glance at the list you are already looking at, so it
  // stays local rather than growing the URL a second knob nobody links to.
  const [stateFilter, setStateFilter] = useState("all");
  const kind = parseKind(params.get("kind"));

  const { data: builds } = useQuery({
    queryKey: ["workspace-builds", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/builds", {
          params: { path: { id: workspaceId } },
        })
      ).data as BuildRun[] | undefined) ?? [],
  });
  const { data: reviews } = useQuery({
    queryKey: ["workspace-reviews", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/reviews", {
          params: { path: { id: workspaceId } },
        })
      ).data as ReviewRun[] | undefined) ?? [],
  });

  const runs = builds && reviews ? mergeRuns(builds, reviews) : null;
  // Offered from what actually ran, so the control never lists a state this
  // repo has no run in and the loop's vocabulary is not copied here.
  const states = [...new Set((runs ?? []).map((r) => r.state))].sort();
  // A state nothing is in any more is no filter at all. Held rather than reset
  // because the list is LIVE: a run leaving `running` must not silently leave
  // the list empty under a control still reading "running".
  const stateOf = states.includes(stateFilter) ? stateFilter : "all";
  const visible = runs ? filterRuns(runs, kind, stateOf) : [];
  // The open run if the filter still shows it, else the newest one that is —
  // so narrowing the list never leaves the transcript of a hidden run beside it.
  const openRun = visible.find((r) => r.id === openId) ?? visible[0] ?? null;
  const open = openRun?.id ?? null;

  const { data: detail } = useQuery({
    queryKey: ["job", open],
    enabled: !!open,
    queryFn: async () =>
      (
        await api.GET("/api/v1/jobs/{id}", { params: { path: { id: open as string } } })
      ).data as { transcript?: LoopJobTranscriptEntry[] } | undefined,
  });

  if (!runs) return null;
  if (runs.length === 0) {
    // ONE empty state for the repo, not one per kind (AC-9). No filters either:
    // there is nothing here to narrow.
    return (
      <Panel title="Runs">
        <Empty>
          No run has happened in this repo yet. The control plane raises a review
          per open pull request — and again when one is pushed to — and a build
          when a card is enqueued.
        </Empty>
      </Panel>
    );
  }

  const pickKind = (next: KindFilter) => {
    const p = new URLSearchParams(params);
    // Absence IS "all": the default belongs in the code, not in every URL.
    if (next === "all") p.delete("kind");
    else p.set("kind", next);
    setParams(p, { replace: true });
  };

  return (
    <Panel
      title="Runs"
      actions={
        <>
          <select
            className="input small"
            aria-label="filter by kind"
            value={kind}
            onChange={(e) => pickKind(e.target.value as KindFilter)}
          >
            <option value="all">all kinds</option>
            <option value="build">build</option>
            <option value="review">review</option>
          </select>
          <select
            className="input small"
            aria-label="filter by state"
            value={stateOf}
            onChange={(e) => setStateFilter(e.target.value)}
          >
            <option value="all">all states</option>
            {states.map((s) => (
              <option key={s} value={s}>
                {jobStateMeta(s).label}
              </option>
            ))}
          </select>
        </>
      }
    >
      <div className="reviews-split">
        <ul className="reviews-runs">
          {visible.length === 0 ? (
            <li>
              <Empty>No run matches this filter.</Empty>
            </li>
          ) : (
            visible.map((r) => {
              // `jobStateMeta` speaks the loop's tone vocabulary, which has a
              // `muted` the Pill spells `dim`. Mapped here rather than widened
              // in the shared component, whose set is the design system's.
              const tone = pillTone(jobStateMeta(r.state).tone);
              return (
                <li key={r.id}>
                  <button
                    className={`reviews-run${r.id === open ? " is-open" : ""}`}
                    onClick={() => setOpenId(r.id)}
                    data-testid="run-row"
                    data-kind={r.kind}
                    data-reason-kind={r.reasonKind}
                  >
                    {/* The kind is a WORD, not a colour: colour in this row is
                        already the state's, and a second palette next to it
                        would make two claims a reader has to tell apart. */}
                    <Pill tone="dim">{r.kind}</Pill>
                    <span className="mono run-label">{r.label}</span>
                    <Pill tone={tone}>{r.state}</Pill>
                    <span className="faint small mono">{r.meta}</span>
                    {/* A second line rather than the meta slot: the reason is a
                        SENTENCE — it names a node, or the label to set — and
                        squeezing it into a one-word annotation would truncate
                        the half that says what to do about it. */}
                    {r.reason ? (
                      <span className="faint small run-reason" data-testid="run-reason">
                        {r.reason}
                      </span>
                    ) : null}
                  </button>
                </li>
              );
            })
          )}
        </ul>
        <div className="reviews-transcript" data-testid="run-transcript">
          {detail?.transcript?.length ? (
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
