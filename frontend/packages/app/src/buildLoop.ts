// The per-workspace build loop, as a screen has to read it (MAIN-387).
//
// MAIN-385 shipped the switch, the pin and the sweep; everything about them was
// reachable only from `nook builds loop` and curl. The hard part of putting
// them on screen is not the controls — it is the SILENCE. A repo whose switch
// is on and whose board has ready cards can still be doing nothing for four
// separate reasons, and until now none of them was visible anywhere.
//
// This module is that judgement, kept out of the components so it is testable
// as a function of its inputs. Everything here reads the SAME records the
// control plane decided from: the settings row, the run list, the newest run's
// own state, and the tenant switch. Nothing is inferred from a sentence.
import { useQuery } from "@tanstack/react-query";
import { api, type LoopJob, type Schemas, type WorkspaceLocation } from "@nookos/api";

export type BuildLoopSettings = Schemas["BuildLoopSettings"];

/** A run in one of these states is holding this repo's ceiling. The same four
 *  `live_build_states` counts, spelled the same way — what counts as in flight
 *  is one question with one answer. */
export const LIVE_RUN_STATES = ["queued", "claimed", "running", "waiting_on_human"] as const;

export function isLiveRun(state: string): boolean {
  return (LIVE_RUN_STATES as readonly string[]).includes(state);
}

/** One row of `GET /workspaces/{id}/builds`, as this module reads it. */
export interface BuildRunRow {
  id: string;
  state: string;
  task_key?: string | null;
  queued_reason?: string | null;
  created_at: string;
}

/** `run_reconcile::FAILURE_BACKOFF`, in the client's units.
 *
 *  Duplicated rather than served, because there is no endpoint that reports the
 *  hold and NG-1 forbids adding one. The hold is derivable from records the
 *  client already has — a run that concluded nothing, and when it did — so the
 *  only thing this constant is trusted for is the length, which is a compiled-in
 *  constant on the server too. If it ever moves, this reads five minutes late,
 *  not wrong about which card is held. */
export const FAILURE_BACKOFF_MS = 5 * 60_000;

/** Did this run conclude NOTHING — the state that starts the hold?
 *
 *  `run_heads`' own test, and all three arms matter: `failed` and `canceled`
 *  are terminal without an outcome by definition, and a `completed` run with no
 *  `build_outcome` is an agent that ended its pass early, which the wakeup rule
 *  treats identically. */
export function concludedNothing(job: {
  state: string;
  build_outcome?: string | null;
}): boolean {
  if (job.state === "failed" || job.state === "canceled") return true;
  return job.state === "completed" && !job.build_outcome;
}

/** Why this repo is raising nothing right now. */
export type BuildLoopWhy =
  /** The settings row has not arrived; nothing may be claimed about the repo. */
  | { kind: "loading" }
  /** `loops.enabled` is off for the tenant — MAIN-239's failure mode, and the
   *  one that outranks everything below because no other switch can undo it. */
  | { kind: "tenant-off" }
  | { kind: "switch-off" }
  /** Concurrency 0 — this repo's own kill switch, which is not the same
   *  statement as the switch being off and must not read as one. */
  | { kind: "ceiling-zero" }
  /** A run exists and cannot be placed. The control plane's own sentence. */
  | { kind: "queued"; reason: string; run: BuildRunRow }
  | { kind: "at-concurrency"; live: number; concurrency: number }
  /** The newest run concluded nothing, so its card is held until `until`. */
  | { kind: "backoff"; until: number; taskKey: string | null }
  | { kind: "no-work" };

export interface BuildLoopWhyInput {
  /** `undefined` while the tenant setting is still in flight — "not loaded"
   *  must never render as "off", which would flash MAIN-239's diagnosis over
   *  every healthy repo on every page load. */
  tenantLoops: boolean | undefined;
  settings: BuildLoopSettings | null | undefined;
  runs: BuildRunRow[] | undefined;
  /** The newest run's full record, once fetched. The listing carries neither
   *  `updated_at` nor `build_outcome`, and the hold needs both. */
  newest?: { state: string; build_outcome?: string | null; updated_at: string } | null;
  now?: number;
}

export function buildLoopWhy({
  tenantLoops,
  settings,
  runs,
  newest,
  now = Date.now(),
}: BuildLoopWhyInput): BuildLoopWhy {
  if (!settings) return { kind: "loading" };
  if (tenantLoops === false) return { kind: "tenant-off" };
  if (!settings.enabled) return { kind: "switch-off" };
  if (settings.concurrency === 0) return { kind: "ceiling-zero" };

  const live = (runs ?? []).filter((r) => isLiveRun(r.state));
  // A queued run naming its own gate outranks the count: with a ceiling of one
  // and one unplaceable run, "at concurrency" is true and useless — the reason
  // that run is stuck is the thing somebody has to go and fix.
  const stuck = live.find((r) => r.state === "queued" && r.queued_reason);
  if (stuck) return { kind: "queued", reason: stuck.queued_reason as string, run: stuck };
  if (live.length >= settings.concurrency) {
    return { kind: "at-concurrency", live: live.length, concurrency: settings.concurrency };
  }
  if (newest && concludedNothing(newest)) {
    const until = Date.parse(newest.updated_at) + FAILURE_BACKOFF_MS;
    if (Number.isFinite(until) && until > now) {
      return { kind: "backoff", until, taskKey: (runs ?? [])[0]?.task_key ?? null };
    }
  }
  return { kind: "no-work" };
}

/** The default clock format. Split out so a test can pass a stable one — the
 *  sentence is what is under test, not the host's locale. */
const clock = (t: number) => new Date(t).toLocaleTimeString();

/** The reason in words. `tenant-off` is deliberately absent: it gets a notice
 *  with a link rather than a line of prose (AC-5), because it is the only cause
 *  here whose fix is on another page. */
export function whyWords(why: BuildLoopWhy, fmt: (t: number) => string = clock): string {
  switch (why.kind) {
    case "loading":
      return "reading this repo's build loop…";
    case "tenant-off":
      return "loops are off for this tenant";
    case "switch-off":
      return "off — this repo's cards are built only when somebody asks";
    case "ceiling-zero":
      return "concurrency is 0 — no build run is raised for this repo";
    case "queued":
      return why.reason;
    case "at-concurrency":
      return why.concurrency === 1
        ? "at concurrency — one build run at a time"
        : `at concurrency — ${why.live} of ${why.concurrency} build runs in flight`;
    case "backoff":
      return `backing off until ${fmt(why.until)} — ${
        why.taskKey ?? "the last card"
      }'s run concluded nothing, and a card is retried five minutes after that`;
    case "no-work":
      return "no work available — nothing on this board is labelled agent-ready";
  }
}

/** The pin in the words the panel and Mission Control both show. `Auto` is the
 *  ordinary case and says what it MEANS: placement over the enabler's own
 *  eligible nodes, not "unset". */
export function pinLabel(settings: BuildLoopSettings | null | undefined): string {
  if (!settings) return "—";
  if (!settings.node_id) return "Auto";
  // A pin whose node is gone still has an id. Saying "Auto" there would claim
  // the run is placeable anywhere, which is the opposite of what it does.
  return settings.node_name ?? "a node that no longer exists";
}

/** What a build run CONCLUDED, in words (MAIN-458's three outcomes). Named one
 *  by one rather than prettified: an outcome this build has never heard of is
 *  shown verbatim rather than dressed up as one of the three. */
export function buildOutcomeWords(outcome: string | null | undefined): string | null {
  if (!outcome) return null;
  switch (outcome) {
    case "pr_opened":
      return "PR opened";
    case "blocked":
      return "blocked — handed back to a human";
    case "nothing_to_do":
      return "nothing to do";
    default:
      return outcome;
  }
}

/**
 * The branch a build run is working on.
 *
 * Nothing stamps `tasks.branch` for a loop run — only the human `start-work`
 * path does — so the branch is read where it actually lives: the CHECKOUT the
 * run recorded (MAIN-480 AC-4), whose `git_branch` discovery keeps current. The
 * card's own column still wins when it has one, because a human who started the
 * work said so explicitly.
 */
export function branchOf(
  task: { branch?: string | null; worktree_path?: string | null } | null | undefined,
  locations: WorkspaceLocation[] | undefined,
): string | null {
  if (!task) return null;
  if (task.branch) return task.branch;
  if (!task.worktree_path) return null;
  return (locations ?? []).find((l) => l.path === task.worktree_path)?.git_branch ?? null;
}

/** This repo's build-loop settings. Its own key, shared by every surface that
 *  shows the switch, so flipping it in Mission Control repaints the panel. */
export const buildLoopSettingsKey = (workspaceId: string) =>
  ["build-loop-settings", workspaceId] as const;

export function useBuildLoopSettings(workspaceId: string) {
  return useQuery({
    queryKey: buildLoopSettingsKey(workspaceId),
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/build-loop-settings", {
          params: { path: { id: workspaceId } },
        })
      ).data as BuildLoopSettings | undefined) ?? null,
  });
}

/** This repo's build runs. The key `WorkspaceRuns` and `BuildLoop` already
 *  read, so the three share one fetch and one live invalidation. */
export function useWorkspaceBuilds(workspaceId: string) {
  return useQuery({
    queryKey: ["workspace-builds", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/builds", {
          params: { path: { id: workspaceId } },
        })
      ).data as BuildRunRow[] | undefined) ?? [],
    // No poll of its own, deliberately: `job_changed` already invalidates this
    // key (see `live.ts`), and Mission Control mounts one of these PER REPO —
    // a ten-second poll each would make this page's dominant traffic a question
    // the websocket has already answered.
  });
}

/** The tenant's loop master switch (MAIN-239), on the `["settings"]` key the
 *  Settings page writes — so turning it on there repaints this without a
 *  reload. `undefined` until the query answers. */
export function useTenantLoopsEnabled(): boolean | undefined {
  const { data } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });
  if (!data) return undefined;
  return data.find((x) => x.key === "loops.enabled" && x.scope === "tenant")?.value === true;
}

/** What a build run PRODUCED, joined from the three records that hold it: the
 *  job (node, card, outcome), the card (key, PR) and the repo's checkouts (the
 *  branch). No endpoint answers this in one read and NG-1 forbids adding one —
 *  but every key here is one another surface already fetches, so the joins are
 *  usually free. */
export interface BuildRunFacts {
  job: LoopJob | null;
  taskId: string | null;
  taskKey: string | null;
  branch: string | null;
  prUrl: string | null;
  nodeName: string | null;
  outcome: string | null;
}

export function useBuildRunFacts(
  jobId: string | null | undefined,
  workspaceId: string | null | undefined,
): BuildRunFacts {
  const { data: job } = useQuery({
    queryKey: ["job", jobId],
    enabled: !!jobId,
    queryFn: async () =>
      ((await api.GET("/api/v1/jobs/{id}", { params: { path: { id: jobId as string } } }))
        .data as LoopJob | undefined) ?? null,
  });
  const taskId = job?.target_task_id ?? null;

  // `TaskDetail`, on the SAME key the ticket modal reads it under — so opening
  // a card and then a run costs one fetch, not two. The card itself is
  // `detail.task`; the wrapper carries its comments and relations.
  const { data: detail } = useQuery({
    queryKey: ["task", taskId],
    enabled: !!taskId,
    queryFn: async () =>
      (await api.GET("/api/v1/tasks/{id}", { params: { path: { id: taskId as string } } }))
        .data ?? null,
  });
  const task = detail?.task ?? null;
  const { data: ws } = useQuery({
    queryKey: ["workspaces", workspaceId],
    enabled: !!workspaceId,
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}", {
          params: { path: { id: workspaceId as string } },
        })
      ).data ?? null,
  });
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    // A spec run's panel mounts this too and has no executor to name; the key
    // is shared, so a page that already listed nodes still pays nothing.
    enabled: !!jobId,
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });

  return {
    job: job ?? null,
    taskId,
    taskKey: task?.key ?? null,
    branch: branchOf(task, ws?.locations),
    prUrl: task?.pr_url ?? null,
    nodeName:
      (nodes ?? []).find((n) => n.id === job?.executor_node_id)?.name ??
      (job?.executor_node_id ? "an unknown node" : null),
    outcome: job?.build_outcome ?? null,
  };
}
