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
 *  caller invalidates the ticket's job list so the panel shows it.
 *
 *  `seed` (MAIN-231) is the human's opening idea — what they actually want out
 *  of this run. Blank is the same as absent: the field is omitted entirely so a
 *  run started from the compact panel's plain button is byte-identical to what
 *  it sent before the Loop workspace existed. */
export async function createLoopJob(
  kind: "spec" | "decompose",
  targetTaskId: string,
  seed?: string,
) {
  const trimmed = seed?.trim();
  return api.POST("/api/v1/jobs", {
    body: {
      kind,
      target_task_id: targetTaskId,
      ...(trimmed ? { seed: trimmed } : {}),
    },
  });
}

/** Send an unsolicited steering message to a live job (MAIN-231). The server
 *  appends it to the transcript, pushes it into the run's session, and resumes a
 *  job paused on a human — so the caller only has to invalidate. */
export async function postJobMessage(jobId: string, body: string) {
  return api.POST("/api/v1/jobs/{id}/messages", {
    params: { path: { id: jobId } },
    body: { body: body.trim() },
  });
}

/**
 * What the workspace's bottom bar is FOR, given the job it is looking at. The
 * three states are genuinely different jobs of work, so the composer is not one
 * box that changes placeholder — it is this decision, made in one pure place:
 *
 * - `seed` — no job yet. The box is the opening idea (AC-2), not a bare Play
 *   button: you are telling the agent what you want before it starts.
 * - `steer` — a job is in flight or paused on a human. The box posts steering
 *   messages (AC-3); a paused job resumes when one lands.
 * - `readonly` — the job reached a terminal state. There is no session left to
 *   talk to, and the server refuses messages, so the UI must not offer a box
 *   that can only fail (AC-5).
 */
export type ComposerMode = "seed" | "steer" | "readonly";

export function composerMode(job: LoopJob | null | undefined): ComposerMode {
  if (!job) return "seed";
  if (isActiveJob(job)) return "steer";
  return "readonly";
}

/**
 * Terminal escape sequences, stripped so a PTY chunk is readable prose.
 *
 * Agent transcript lines are recorded verbatim (MAIN-161 NG-2) — cursor moves,
 * colours and all. That is right for the record and unreadable on a page, so
 * the *view* strips them. The stored line is never touched.
 */
// eslint-disable-next-line no-control-regex
const ANSI = /(?:\x1B\[[0-?]*[ -/]*[@-~])|(?:\x1B\][^\x07\x1B]*(?:\x07|\x1B\\))|[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]/g;

export function stripAnsi(s: string): string {
  return s.replace(ANSI, "");
}

/**
 * Does this transcript entry look like a drafted issue rather than narration?
 *
 * The skills print their draft into the session before asking for a go-ahead,
 * so it arrives as an ordinary transcript line. There is no marker on the wire
 * saying "this is a draft" — the shape IS the marker: the issue template's own
 * headings. Recognising them is what lets the page render a draft as markdown
 * while leaving raw terminal noise as preformatted text (AC-4).
 */
export function looksLikeDraft(content: string): boolean {
  const text = stripAnsi(content);
  return (
    /^\s*##\s+Acceptance Criteria\s*$/m.test(text) ||
    (/^\s*##\s+Problem\s*$/m.test(text) && /^\s*##\s+Non-goals\s*$/m.test(text))
  );
}

/** Board keys (`MAIN-42`) named anywhere in a job's transcript — what the run
 *  filed, so the page can link back to it (AC-4). Deduped, in first-mention
 *  order, with the job's own target excluded: a spec job always names the
 *  ticket it is speccing, and offering that as "what it filed" would be a lie. */
export function filedKeys(
  transcript: { content: string }[] | null | undefined,
  exclude?: string | null,
): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const line of transcript ?? []) {
    for (const m of stripAnsi(line.content).matchAll(/\b[A-Z][A-Z0-9]{1,9}-\d+\b/g)) {
      const key = m[0];
      if (key === exclude || seen.has(key)) continue;
      seen.add(key);
      out.push(key);
    }
  }
  return out;
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
