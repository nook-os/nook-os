// Ticket-anchored loop jobs (MAIN-128) — the shared, view-agnostic half.
//
// A "loop job" is a spec draft (fill in a ticket) or a decompose run (break an
// epic into sub-tickets) that an executor picks up somewhere in the fleet. It
// belongs to ONE ticket, so it is surfaced ON that ticket — the Loop panel in
// the detail modal, and the entry action in the three-dots menu / on the board.
// This module holds the query keys, the fetchers, and the pure action logic the
// panel and the menu both read, so the two can never disagree about whether a
// new job may be started or why not.
import { api, type LoopJob } from "@nookos/api";

/** A job in one of these states is still in flight — its ticket already has a
 *  loop running, so a second one must not be started on top of it (AC-1). */
export const ACTIVE_JOB_STATES = [
  "queued",
  "claimed",
  "running",
  "waiting_on_human",
] as const;

export function isActiveJob(job: LoopJob | undefined | null): job is LoopJob {
  return !!job && (ACTIVE_JOB_STATES as readonly string[]).includes(job.state);
}

/** The query key both surfaces read the ticket's jobs under. The live
 *  `job_changed` event invalidates exactly this (see `live.ts`). */
export const taskJobsKey = (taskId: string) => ["task", taskId, "jobs"] as const;
/** A single job's detail (with transcript). Invalidated per-job on the event. */
export const jobKey = (jobId: string) => ["job", jobId] as const;

export async function fetchTaskJobs(taskId: string): Promise<LoopJob[]> {
  return (
    (await api.GET("/api/v1/tasks/{task_id}/jobs", {
      params: { path: { task_id: taskId } },
    })).data ?? []
  );
}

/**
 * What the entry action should offer for a ticket, given its jobs (newest
 * first, as the API returns them). Pure so both the panel header and the menu
 * compute the same label and the same disabled reason from the same input, and
 * so it is trivially unit-testable.
 *
 * - An epic runs the DECOMPOSER; any other ticket DRAFTS A SPEC (AC-1).
 * - Disabled while a job is already active on the ticket, with the reason:
 *   a `queued` job that named WHY it can't be placed surfaces that
 *   (`queued_reason`, e.g. "no eligible executor…"); otherwise the generic
 *   "a loop job is already running on this ticket".
 */
export function loopAction(
  taskType: string | null | undefined,
  jobs: LoopJob[] | undefined,
): {
  kind: "spec" | "decompose";
  label: string;
  disabled: boolean;
  reason: string | null;
  latest: LoopJob | null;
} {
  const isEpic = taskType === "epic";
  const kind = isEpic ? "decompose" : "spec";
  const label = isEpic ? "Run decomposer" : "Draft a spec";
  const latest = jobs && jobs.length > 0 ? jobs[0] : null;

  let disabled = false;
  let reason: string | null = null;
  if (isActiveJob(latest)) {
    disabled = true;
    reason =
      latest.state === "queued" && latest.queued_reason
        ? latest.queued_reason
        : "a loop job is already running on this ticket";
  }
  return { kind, label, disabled, reason, latest };
}

/** Create a loop job on a ticket. The response is the fresh job's detail; the
 *  caller invalidates the ticket's job list so the panel shows it. */
export async function createLoopJob(kind: "spec" | "decompose", targetTaskId: string) {
  return api.POST("/api/v1/jobs", {
    body: { kind, target_task_id: targetTaskId },
  });
}

/** How a job's state reads in the panel: a human label and a colour tone. */
export function jobStateMeta(state: string): {
  label: string;
  tone: "info" | "warn" | "err" | "ok" | "muted";
} {
  switch (state) {
    case "queued":
      return { label: "queued", tone: "muted" };
    case "claimed":
      return { label: "claimed", tone: "info" };
    case "running":
      return { label: "running", tone: "info" };
    case "waiting_on_human":
      return { label: "waiting on human", tone: "warn" };
    case "completed":
      return { label: "done", tone: "ok" };
    case "failed":
      return { label: "failed", tone: "err" };
    case "canceled":
      return { label: "canceled", tone: "muted" };
    default:
      return { label: state, tone: "muted" };
  }
}
