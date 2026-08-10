// Ticket-anchored loop jobs (MAIN-128) — the shared, view-agnostic half.
//
// A "loop job" is a spec draft (fill in a ticket) or a decompose run (break an
// epic into sub-tickets) that an executor picks up somewhere in the fleet. It
// belongs to ONE ticket, so it is surfaced ON that ticket — the Loop panel in
// the detail modal, and the entry action in the three-dots menu / on the board.
// This module holds the query keys, the fetchers, and the pure action logic the
// panel and the menu both read, so the two can never disagree about whether a
// new job may be started or why not.
import { api, type LoopJob, type LoopJobTranscriptEntry } from "@nookos/api";

/** A job in one of these states is still in flight — its ticket already has a
 *  loop running, so a second one must not be started on top of it (AC-1). */
export const ACTIVE_JOB_STATES = [
  "queued",
  "claimed",
  "running",
  "waiting_on_human",
] as const;

type ActiveJob = LoopJob & { state: (typeof ACTIVE_JOB_STATES)[number] };

export function isActiveJob(job: LoopJob | undefined | null): job is ActiveJob {
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
 * - `continue` — the job COMPLETED. Not a dead end: the box is still there, but
 *   sending it starts a follow-up run so the conversation keeps going. A finished
 *   spec is a good place to say "also add X".
 * - `readonly` — the job FAILED or was CANCELED. There is no session to talk to
 *   and nothing to continue, so the UI must not offer a box that can only fail
 *   (AC-5); it points at starting a fresh run instead.
 */
export type ComposerMode = "seed" | "steer" | "continue" | "readonly";

export function composerMode(job: LoopJob | null | undefined): ComposerMode {
  if (!job) return "seed";
  if (isActiveJob(job)) return "steer";
  if (job.state === "completed") return "continue";
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
 * Should this transcript entry render as markdown rather than preformatted text?
 *
 * The skills print real markdown into the session — a full drafted issue, an
 * interview (Q&A with **bold** options and bulleted choices), research notes, a
 * fenced ```code``` block. There is no marker on the wire saying "this is
 * markdown" — the shape IS the marker. Recognising markdown structure is what
 * lets the page render those beautifully instead of as a wall of literal `##`
 * and backticks, while short tool markers ("· Bash") and raw terminal output —
 * which carry none of that structure — stay preformatted (AC-4).
 *
 * A full drafted spec (the original signal) still matches: it opens with `##`
 * headings, so `hasHeading` covers it.
 */
export function looksLikeMarkdown(content: string): boolean {
  const text = stripAnsi(content);
  const hasHeading = /^\s{0,3}#{1,6}\s+\S/m.test(text);
  const hasFence = /^\s*```/m.test(text);
  const constructs =
    (/\*\*[^*\n]+\*\*/.test(text) ? 1 : 0) + // **bold**
    (/^\s*[-*]\s+\S/m.test(text) ? 1 : 0) + // - bullet list
    (/^\s*\d+\.\s+\S/m.test(text) ? 1 : 0) + // 1. numbered list
    (/`[^`\n]+`/.test(text) ? 1 : 0); // `inline code`
  // A heading or a fence is a strong signal on its own; otherwise require two
  // constructs so an accidental single `- ` or `**` in raw output stays raw.
  return hasHeading || hasFence || constructs >= 2;
}

/** A transcript entry as the fold leaves it. `steps` is present on — and only
 *  on — an entry the fold produced: the marker lines it stands for, kept so the
 *  view can hand them back (MAIN-499 AC-5). Discarding them made `· 37 steps —
 *  Bash ×37` a dead end whose detail only a whole-transcript export could
 *  recover. */
export type FoldedTranscriptEntry = LoopJobTranscriptEntry & { steps?: string[] };

/** The minimum the fold and the chat mapping actually read.
 *
 *  A loop run's transcript entry satisfies it, and so does a CHAT SESSION's
 *  message (MAIN-502) — which is the point: a session and a run produce the
 *  same `· Bash` markers and the same prose, so they must fold and render the
 *  same way. Naming the shape rather than the DTO is what lets the second
 *  consumer reuse this instead of growing a second copy that drifts. */
export interface TranscriptLine {
  id: string;
  /** `system` | `agent` | `human` — the author, and the grouping key. */
  source: string;
  content: string;
  at: string;
}

/**
 * Fold a run of consecutive agent tool-markers (`· Bash`, `· Read`, …) into ONE
 * activity entry, and drop the empty tool-result placeholders between them — so
 * the transcript reads "· 7 steps — Bash ×5 · Read ×2" instead of a ladder of
 * identical lines. This mirrors how Claude Code collapses tool activity into a
 * single working line rather than printing one per call. Substantive turns — the
 * agent's prose, a human message, a system note — pass through untouched.
 */
export function foldToolActivity<T extends TranscriptLine>(
  transcript: T[],
): (T & { steps?: string[] })[] {
  const isMarker = (e: T) =>
    e.source === "agent" && /^·\s+\S/.test(stripAnsi(e.content));
  const isBlank = (e: T) => stripAnsi(e.content).trim() === "";
  const out: (T & { steps?: string[] })[] = [];
  // `steps` are the marker lines themselves — the record the label summarises;
  // `tools` are just their first words, which is all the label needs.
  let run: { first: T; tools: string[]; steps: string[] } | null = null;
  const flush = () => {
    if (!run) return;
    const counts = new Map<string, number>();
    for (const t of run.tools) counts.set(t, (counts.get(t) ?? 0) + 1);
    const summary = [...counts]
      .map(([t, n]) => (n > 1 ? `${t} ×${n}` : t))
      .join(" · ");
    const label =
      run.tools.length === 1 ? `· ${summary}` : `· ${run.tools.length} steps — ${summary}`;
    out.push({ ...run.first, content: label, steps: run.steps });
    run = null;
  };
  for (const e of transcript) {
    if (isBlank(e)) continue; // empty tool-result placeholder — nothing to show
    if (isMarker(e)) {
      const line = stripAnsi(e.content);
      const tool = line.replace(/^·\s+/, "").split(/\s+/)[0];
      if (!run) run = { first: e, tools: [], steps: [] };
      run.tools.push(tool);
      // Stripped, because that is what a reader can read (MAIN-161 NG-2 keeps
      // the escapes on the stored line, which the export still carries).
      run.steps.push(line);
    } else {
      flush();
      out.push(e);
    }
  }
  flush();
  return out;
}

/**
 * Board keys named anywhere in a job's transcript — what the run filed, so the
 * page can link back to it (AC-4). Deduped, in first-mention order.
 *
 * Matching is anchored to `selfKey`'s OWN board prefix, and that is the whole
 * design. "Uppercase word, dash, digits" is not a board key — it is also
 * `AC-1`, `NG-2`, `UTF-8`, `SHA-256`. A drafted spec contains `AC-N` and `NG-N`
 * lines *by definition*, so the unanchored form turned every draft into a
 * header full of links to tickets that do not exist. Only keys on this ticket's
 * board can be things this run filed, so only those are offered.
 *
 * `selfKey` is also the exclusion: a spec job always names the ticket it is
 * speccing, and offering that as "what it filed" would be a lie. Without a
 * usable `selfKey` there is no prefix to trust, so nothing is offered — an
 * empty header beats a wrong link.
 */
export function filedKeys(
  transcript: { content: string }[] | null | undefined,
  selfKey?: string | null,
): string[] {
  const prefix = /^([A-Z][A-Z0-9]{0,9})-\d+$/.exec(selfKey ?? "")?.[1];
  if (!prefix) return [];
  const re = new RegExp(`\\b${prefix}-\\d+\\b`, "g");
  const seen = new Set<string>();
  const out: string[] = [];
  for (const line of transcript ?? []) {
    for (const m of stripAnsi(line.content).matchAll(re)) {
      const key = m[0];
      if (key === selfKey || seen.has(key)) continue;
      seen.add(key);
      out.push(key);
    }
  }
  return out;
}

/** What the activity indicator should say for a job in `state`, or null for no
 *  indicator (MAIN-237 AC-4, MAIN-240 AC-2).
 *
 *  It lives HERE, with the loop's other view-agnostic decisions, because three
 *  surfaces show that indicator — the ticket's panel, the Loop workspace and a
 *  workspace's Runs list — and the third of them reaching into the first's
 *  module for it (MAIN-499 AC-7) said the panel owned a rule that is the loop's.
 *
 *  Only a job that could still be producing output gets one. A job paused on a
 *  human is NOT working — it is waiting on you, which the interaction surface
 *  says far better — and a finished job is finished. Showing "working…" in
 *  either case is the specific lie this is meant to prevent: it is the operator's
 *  only cue that the agent is alive.
 *
 *  `turn` is the real signal the streaming adapter reports (`job_turn`), and it
 *  OUTRANKS the inference — that is the whole point of MAIN-240. State can only
 *  ever say "this job is running", which stays true in the gap between turns
 *  when the agent is sitting idle waiting to be steered; the adapter can say
 *  which of those two it actually is.
 *
 *  It is deliberately allowed to silence the indicator and never to raise one:
 *
 *  - `undefined` — no adapter reported. The tmux fallback path (NG-1) never
 *    will, so it keeps the inferred label exactly as before.
 *  - `active: false` — a real "not working right now". Believed, so a job
 *    between turns stops claiming to work.
 *  - `active: true` — agrees with the inference; nothing to add.
 *
 *  `queued` and `claimed` ignore `turn` entirely: their labels describe the
 *  executor, not the agent, and no turn can be in flight before a process
 *  exists. Only `running` is turn-gated. */
export function agentActivityLabel(
  state: string,
  turn?: { active: boolean },
): string | null {
  switch (state) {
    case "queued":
      return "waiting for an executor…";
    case "claimed":
      return "the operator agent is starting…";
    case "running":
      if (turn && !turn.active) return null;
      return "the operator agent is working…";
    default:
      return null;
  }
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

/**
 * Why a queued run is not moving (MAIN-297).
 *
 * A run that cannot be placed used to sit on "waiting for an executor…"
 * indefinitely. The two causes that actually happen each have their fix on a
 * DIFFERENT page — Settings→Loops, and Nodes→authorize — neither of which is
 * reachable, or even guessable, from the stuck run.
 *
 * The distinction the backend cannot make for us: when `loops.enabled` is off
 * the dispatcher never polls, so `select_executor` never runs and
 * `queued_reason` is simply NULL. "No reason recorded" is therefore not
 * evidence of anything — which is exactly why the page looked like a dead end.
 * The switch has to be read separately and it WINS: while loops are off nothing
 * will place this run whatever a stale reason from before the switch was
 * flipped might still say.
 */
export type StuckCause =
  | { kind: "loops-off" }
  | { kind: "no-executor"; detail: string }
  | { kind: "waiting"; detail: string | null };

export function stuckCause(
  job: LoopJob | null | undefined,
  /** `undefined` while the settings query is still in flight. */
  loopsEnabled: boolean | undefined,
): StuckCause | null {
  // Only a QUEUED job is stuck. A claimed or running one has an executor, and
  // saying "loops are off" over a run that is already going would be a lie.
  if (!job || job.state !== "queued") return null;

  // Only an explicit `false`. Treating "not loaded yet" as off would flash the
  // wrong diagnosis — and its Turn-on button — on every page load.
  if (loopsEnabled === false) return { kind: "loops-off" };

  const reason = job.queued_reason ?? null;
  // The backend's own phrasing (`services/jobs.rs::no_executor_reason`) always
  // opens with this, and it already distinguishes "no node of yours is online"
  // from "not authorized" — so the detail is passed through rather than
  // re-worded here, where it would drift from the source.
  if (reason && reason.includes("no eligible executor")) {
    return { kind: "no-executor", detail: reason };
  }
  // Anything else stays an honest "waiting", carrying whatever the backend did
  // say (AC-3): an undiagnosed cause must not be dressed up as one of the two
  // we know how to fix.
  return { kind: "waiting", detail: reason };
}
