// What one run offers, derived once (MAIN-559).
//
// Three surfaces ask the same question — the row's right-click menu, the row's
// `…` button, and the detail header's primary action — and a run whose state
// moved between them answering differently is the bug AC-5 names. So the answer
// is a PURE FUNCTION of the run, computed at the moment it is needed, and the
// three surfaces are three renderings of one list rather than three branches.
//
// Nothing here calls anything. An action is a name, a refusal and an id; who
// fires it, and what confirmation it needs, belongs to the surface.

/** The subset of a run row this derivation reads. Structural rather than the
 *  `RunRow` import, so the panel can depend on this file and not the reverse. */
export interface RunActionTarget {
  id: string;
  kind: "build" | "review";
  state: string;
  /** The run's name in the list — `MAIN-42`, `PR #341`. Used in the words. */
  label: string;
}

export type RunActionId =
  | "open"
  | "cancel"
  | "rerun"
  | "copy-id"
  | "copy-link"
  | "view-task"
  | "view-pr";

export interface RunAction {
  id: RunActionId;
  label: string;
  /** Destructive, and rendered as such. */
  danger?: boolean;
  /** Why this action cannot fire, in the words to show — and the whole of AC-6.
   *  An action the API would refuse is still OFFERED, carrying its reason:
   *  absence is indistinguishable from an oversight, and a person who expects
   *  Re-run to be there and does not find it learns nothing. */
  refusal?: string;
}

/**
 * Why `/rerun` will refuse this run, said before the call is made.
 *
 * `services/jobs.rs::rerun` returns *"only a failed or canceled JOB can be
 * re-run"*; this says RUN, because that is the noun this whole surface uses —
 * MAIN-488 made "run" the word a reader sees, and `job` is the internal name
 * for the same row. So it is deliberately not byte-identical, and NOTHING
 * enforces the tie: no test can, across the language boundary. What is pinned
 * is the RULE — `canRerunRun` mirrors the server's `matches!(failed | canceled)`
 * and is table-tested — because that is the half a drift would actually break.
 * If the server's rule changes, this file is the place that has to follow it.
 */
export const RERUN_REFUSAL = "only a failed or canceled run can be re-run";

/** What Cancel refuses with once the run it names has already ended (AC-5). */
export const CANCEL_ENDED_REFUSAL = "this run has already finished — there is nothing to cancel";

/** What Cancel says while this client's own cancel is still in flight (AC-4). */
export const CANCEL_PENDING_REFUSAL = "already canceling this run";

/** The states `jobs::is_terminal` names — a run there is finished and has no
 *  outgoing transition, which is what makes Cancel meaningless on it. */
const TERMINAL = ["completed", "failed", "canceled"];

export function isTerminalRun(state: string): boolean {
  return TERMINAL.includes(state);
}

/** Cancel is legal out of ANY live state (`legal_transition`), and out of no
 *  terminal one. */
export function canCancelRun(state: string): boolean {
  return !isTerminalRun(state);
}

/** Re-run is narrower than terminal: the server takes a failed or canceled run
 *  and refuses a completed one. `completed` is therefore the state that gets an
 *  offered-but-refusing Re-run rather than none (AC-6). */
export function canRerunRun(state: string): boolean {
  return state === "failed" || state === "canceled";
}

export interface RunActionContext {
  /** A cancel this client has sent and not yet seen answered (AC-4). */
  pending?: boolean;
  /** The card a build run is about, when the panel knows it. */
  taskHref?: string | null;
  /** The pull request a review run is about, when the panel knows it. */
  prHref?: string | null;
}

/**
 * Every action this run offers, in menu order (AC-2).
 *
 * Active runs get Cancel and never Re-run; terminal runs get Re-run and never
 * Cancel. Open, Copy run ID and Copy link are unconditional — they read the run
 * rather than change it, and there is no state in which reading is wrong.
 */
export function runActions(
  run: RunActionTarget,
  ctx: RunActionContext = {},
): RunAction[] {
  const out: RunAction[] = [{ id: "open", label: "Open" }];

  if (canCancelRun(run.state)) {
    out.push({
      id: "cancel",
      label: "Cancel run",
      danger: true,
      refusal: ctx.pending ? CANCEL_PENDING_REFUSAL : undefined,
    });
  } else {
    out.push({
      id: "rerun",
      label: "Re-run",
      refusal: canRerunRun(run.state) ? undefined : RERUN_REFUSAL,
    });
  }

  out.push({ id: "copy-id", label: "Copy run ID" });
  out.push({ id: "copy-link", label: "Copy link" });

  // Only once the run has ended: while it is live the transcript beside it is
  // the thing to be looking at, and a link away from it is an invitation to
  // stop watching.
  if (isTerminalRun(run.state)) {
    // The hrefs decide WHETHER the action is offered and nothing more: the
    // destination is re-derived from the run when the action fires. Carrying it
    // on the action too would be one fact in two places, and the copy nobody
    // read is the one that would rot.
    if (run.kind === "build" && ctx.taskHref) {
      out.push({ id: "view-task", label: `View ${run.label}` });
    }
    if (run.kind === "review" && ctx.prHref) {
      out.push({ id: "view-pr", label: `View ${run.label}` });
    }
  }

  return out;
}

/**
 * The ONE action a run's header shows as a button (AC-7), which is the same
 * action the menu leads with — Cancel while it is live, Re-run once it is not.
 * Everything else stays in the overflow.
 */
export function primaryRunAction(actions: RunAction[]): RunAction | null {
  return actions.find((a) => a.id === "cancel" || a.id === "rerun") ?? null;
}

/** The overflow's contents: everything the header is not already showing. */
export function overflowRunActions(actions: RunAction[]): RunAction[] {
  const primary = primaryRunAction(actions);
  return actions.filter((a) => a !== primary);
}

/**
 * What Cancel does when it is chosen for a run that has moved on since the menu
 * was built (AC-5) — the last line of defence behind the live re-derivation,
 * covering the gap between choosing an action and the request leaving.
 *
 * Null means go ahead; a string is the refusal to show instead of calling.
 */
export function cancelRefusal(state: string, pending: boolean): string | null {
  if (!canCancelRun(state)) return CANCEL_ENDED_REFUSAL;
  if (pending) return CANCEL_PENDING_REFUSAL;
  return null;
}

/** Re-run's twin of the above. */
export function rerunRefusal(state: string, pending: boolean): string | null {
  if (!canRerunRun(state)) return RERUN_REFUSAL;
  if (pending) return "already re-running this run";
  return null;
}

/** The confirmation a cancel must carry (AC-3): the run by name, and what
 *  actually stops. Named here so the dialog's words are testable without a
 *  dialog. */
export function cancelPrompt(run: RunActionTarget): { title: string; description: string } {
  return {
    title: `Cancel ${run.kind} run ${run.label}?`,
    description:
      "The agent working on this run will be stopped where it is, and the run " +
      "ends as canceled. Anything it has already pushed stays pushed.",
  };
}
