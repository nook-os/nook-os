// Live state pushed over /ws/ui. REST (TanStack Query) owns durable state;
// this store holds the deltas and pokes queries to refetch.
import { create } from "zustand";
import type { QueryClient } from "@tanstack/react-query";
import { connectUiSocket, type EventItem, type UiEvent } from "@nookos/api";
import { notifyEvent } from "./notify";
import { runJobFollowUp, useJobs } from "./jobs";
import { resyncSealedSecrets } from "./secretkeys";
import { useAppPassword } from "./apppassword";
import { NOTIFICATIONS_KEY, useToasts } from "./Notifications";
import { chimeFor } from "./notify";
import type { Notification } from "@nookos/api";
import { api } from "@nookos/api";

const ACTIVITY_BUFFER = 250;

/** How long a `running`/`waiting` mark survives with no fresh report before the
 *  UI treats it as idle. A crashed agent never fires `Stop`, so without this a
 *  spinner would spin forever. Kept AT LEAST as long as the server's TTL
 *  (`AGENT_STATE_TTL` = 15 min in `crates/nook-control/src/ws/registry.rs`): if
 *  the client faded a mark first, a reload would re-seed it from
 *  `GET /sessions/agent-states` — which the server still serves until its own
 *  sweep — and the spinner would flicker back. Being >= the server means the
 *  client never drops a mark the server would still hand back. */
export const AGENT_STATE_STALE_MS = 15 * 60 * 1000;

/** The agent mark to show for a session, or `undefined` when the session is
 *  dead — a session that has `exited`/`error`/`killed` must show no agent mark,
 *  so the last state its agent reported does not linger as a spinner. Pure and
 *  shared so the term-chip (`SessionWindows`) and the top tab (`SessionTabs`)
 *  cannot disagree about a dead session. */
export function liveAgentMark(
  status: string | undefined,
  agent: AgentState | undefined,
): AgentState | undefined {
  const dead = status === "exited" || status === "error" || status === "killed";
  return dead ? undefined : agent;
}

/** A control-plane device authorization, as the UI follows it. */
export interface RuntimeAuthFlow {
  flowId: string;
  runtime: string;
  state: "starting" | "prompt" | "delivered" | "failed";
  userCode?: string;
  verificationUri?: string;
  error?: string;
}

export interface AgentState {
  /** `running` | `waiting`. `idle` is represented by absence. */
  state: string;
  /** tmux window the agent runs in, so the right terminal chip lights up. */
  window: number | null;
  /** Client receipt time (ms), for the staleness fallback above. */
  at: number;
}

/** Whether a loop job's agent is mid-turn right now (MAIN-240 AC-2).
 *
 *  ABSENCE IS NOT `active: false` — it means no adapter ever reported, which is
 *  the ordinary case for the tmux fallback path. The two have to stay
 *  distinguishable, because a job with no real signal must keep the old
 *  state-inferred indicator while a job with one must be believed over the
 *  inference. Storing `false` explicitly is what makes "the agent finished its
 *  turn and is idle" sayable at all. */
export interface JobTurn {
  active: boolean;
  /** Client receipt time (ms). Not currently used to expire the mark — see the
   *  note in the `job_turn` handler for why job state is the better backstop. */
  at: number;
}

interface LiveState {
  connected: boolean;
  nodeStatus: Record<string, string>;
  nodeResources: Record<string, unknown>;
  sessionStatus: Record<string, string>;
  /** Live agent activity per session (running/waiting). Absence means idle. */
  agentState: Record<string, AgentState>;
  /** Live turn state per loop job id. Absence means "no adapter reported". */
  jobTurn: Record<string, JobTurn>;
  activity: EventItem[];
  /** The device-flow authorization in progress, if any (MAIN-650). One at a
   *  time: it is a person at a browser approving a code, not a queue. */
  runtimeAuth: RuntimeAuthFlow | null;
  seedActivity(events: EventItem[]): void;
  seedAgentStates(items: { session_id: string; window?: number | null; state: string }[]): void;
}

export const useLive = create<LiveState>(() => ({
  connected: false,
  nodeStatus: {},
  nodeResources: {},
  sessionStatus: {},
  agentState: {},
  jobTurn: {},
  activity: [],
  runtimeAuth: null,
  seedActivity(events) {
    useLive.setState((s) => {
      const known = new Set(s.activity.map((e) => e.id));
      const merged = [...s.activity, ...events.filter((e) => !known.has(e.id))];
      merged.sort((a, b) => (a.occurred_at < b.occurred_at ? 1 : -1));
      return { activity: merged.slice(0, ACTIVITY_BUFFER) };
    });
  },
  // Seed the agent-state map on load (and on reconnect) from
  // GET /sessions/agent-states, so a tab already spinning when you open the app
  // shows it without waiting for the next hook to fire.
  seedAgentStates(items) {
    const now = Date.now();
    const next: Record<string, AgentState> = {};
    for (const it of items) {
      if (it.state === "idle") continue;
      next[it.session_id] = { state: it.state, window: it.window ?? null, at: now };
    }
    useLive.setState({ agentState: next });
  },
}));

let started = false;

export function startLive(queryClient: QueryClient) {
  if (started) return;
  started = true;

  const handle = (event: UiEvent) => {
    if (event.type === "node_status") {
      useLive.setState((s) => ({
        nodeStatus: { ...s.nodeStatus, [event.data.node_id]: event.data.status },
      }));
      queryClient.invalidateQueries({ queryKey: ["nodes"] });
      queryClient.invalidateQueries({ queryKey: ["workspaces"] });
      // Mission Control groups by node and shows node status per checkout.
      queryClient.invalidateQueries({ queryKey: ["overview"] });
    } else if (event.type === "node_resources") {
      useLive.setState((s) => ({
        nodeResources: {
          ...s.nodeResources,
          [event.data.node_id]: event.data.resources,
        },
      }));
    } else if (event.type === "runtime_auth_prompt") {
      // The code and link the person has to act on. Held in the store rather
      // than pushed as a toast: it stays on screen until the flow ends, because
      // a code that vanishes is a code nobody can type.
      useLive.setState({
        runtimeAuth: {
          flowId: event.data.flow_id,
          runtime: event.data.runtime,
          state: "prompt",
          userCode: event.data.user_code,
          verificationUri: event.data.verification_uri,
        },
      });
    } else if (event.type === "runtime_auth_delivered") {
      useLive.setState((s) => ({
        runtimeAuth: s.runtimeAuth
          ? { ...s.runtimeAuth, state: "delivered", userCode: undefined }
          : null,
      }));
      // The node re-probes and pushes a fresh profile set after a delivery, so
      // the panel's state comes from the refetch rather than from this event.
      queryClient.invalidateQueries({ queryKey: ["nodes"] });
    } else if (event.type === "runtime_auth_failed") {
      useLive.setState((s) => ({
        runtimeAuth: s.runtimeAuth
          ? {
              ...s.runtimeAuth,
              state: "failed",
              userCode: undefined,
              error: String(
                (event.data as { error?: string }).error ?? "authorization failed",
              ),
            }
          : null,
      }));
    } else if (event.type === "session_status") {
      useLive.setState((s) => ({
        sessionStatus: {
          ...s.sessionStatus,
          [event.data.session_id]: event.data.status,
        },
      }));
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      // Mission Control shows sessions under the checkout they run in.
      queryClient.invalidateQueries({ queryKey: ["overview"] });
    } else if (event.type === "session_agent_state") {
      // What the agent in a session is doing right now. `idle` is the absence
      // of a mark, so remove the entry rather than storing it — that keeps the
      // "is anything running" check a simple key lookup.
      const { session_id, window, state } = event.data;
      useLive.setState((s) => {
        const agentState = { ...s.agentState };
        if (state === "idle") delete agentState[session_id];
        else agentState[session_id] = { state, window: window ?? null, at: Date.now() };
        return { agentState };
      });
    } else if (event.type === "notification") {
      // Toast it now, and refresh the inbox so the bell's count is right even
      // if nobody looks until tomorrow.
      const n = event.data.notification as Notification;
      useToasts.getState().push(n);
      queryClient.invalidateQueries({ queryKey: NOTIFICATIONS_KEY });
      // The chime and desktop notification stay: they reach you when the tab
      // is not focused, which a toast cannot.
      chimeFor(n.level, n.title, n.body, n.link ?? "");
    } else if (event.type === "task_changed") {
      // Agents change tasks constantly — claiming, commenting, moving — and a
      // board that only refetched on a timer would show a human work that was
      // taken seconds ago. Invalidating rather than patching state from the
      // event keeps one source of truth: the event says "stale", the query
      // says what is true.
      queryClient.invalidateQueries({ queryKey: ["boards"] });
      // The whole prefix, not one id: a task modal opened by human key is
      // cached under `["task", "NOOK-42"]`, so invalidating by uuid would miss
      // exactly the view somebody is looking at.
      queryClient.invalidateQueries({ queryKey: ["task"] });
      queryClient.invalidateQueries({ queryKey: ["tasks"] });
    } else if (event.type === "interaction_changed") {
      // A durable interaction was raised, answered, or canceled (MAIN-159).
      // Same "what you have is stale" contract as `task_changed`: refetch the
      // pending list, and — when a ticket is named — that ticket's own pending
      // interactions and the ticket itself.
      queryClient.invalidateQueries({ queryKey: ["interactions", "pending"] });
      if (event.data.task_id) {
        queryClient.invalidateQueries({
          queryKey: ["interactions", "task", event.data.task_id],
        });
        queryClient.invalidateQueries({ queryKey: ["task", event.data.task_id] });
      }
    } else if (event.type === "job_changed") {
      // A loop job's transcript grew or its state changed (MAIN-128). Same
      // "what you have is stale" contract as `task_changed`: the event carries
      // the TARGET TICKET id, so refetch that ticket's job list and — because a
      // transcript line or a state change is a per-job delta — every job cached
      // under it. The panel reads `["task", id, "jobs"]` for the list and
      // `["job", jobId]` for a job's detail; invalidating the `["job"]` prefix
      // covers whichever job the panel is currently showing without knowing its id.
      // `task_id` is null for a review run — there is no ticket to invalidate,
      // and the job + reviews prefixes below are its whole live surface.
      if (event.data.task_id) {
        queryClient.invalidateQueries({ queryKey: ["task", event.data.task_id, "jobs"] });
      }
      queryClient.invalidateQueries({ queryKey: ["job"] });
      // A REVIEW run has no ticket (MAIN-455), so the branch above cannot reach
      // its list — the event's `task_id` is null for one. The workspace's review
      // list is keyed by workspace, and invalidating the prefix repaints it
      // without this needing to know which repo the run belonged to.
      queryClient.invalidateQueries({ queryKey: ["workspace-reviews"] });
      // Builds ride the same nudge (MAIN-461 AC-3): a build HAS a task id, so
      // the ticketed branch above already repaints its card — this repaints
      // the workspace's build rows.
      queryClient.invalidateQueries({ queryKey: ["workspace-builds"] });
      // Two keys, one panel since MAIN-488: the Runs panel reads both listings
      // and merges them, so both prefixes still have to be nudged.
    } else if (event.type === "session_message") {
      // A chat session's conversation grew, or a permission was answered
      // (MAIN-502). Same "what you have is stale" contract as `job_changed`:
      // the page refetches the conversation rather than being handed a message
      // to splice in, so a device that missed part of the stream converges on
      // the same history instead of a half-one.
      queryClient.invalidateQueries({
        queryKey: ["session-messages", event.data.session_id],
      });
    } else if (event.type === "job_turn") {
      // A loop job's agent started or stopped a turn (MAIN-240 AC-2). Unlike
      // every neighbour in this switch, this does NOT invalidate a query: a turn
      // is live state with no row to go and read, so the event carries the fact
      // itself and the store IS the source of truth for it. Refetching here
      // would be pure cost for an answer the server does not have.
      //
      // Kept even when it says `false`, because absence has to keep meaning "no
      // adapter reported" — that is what lets the tmux path fall back to the
      // old inferred indicator while the streaming path is believed exactly.
      //
      // No staleness timer, unlike `agentState` above, and the difference is
      // deliberate. A stream that dies mid-turn already reports `false` as its
      // pump unwinds (`loop_job.rs`), and the case that cannot — the whole node
      // dying — is covered better by job state than by a clock: the reaper
      // fails the stranded job, and a job that is not running shows no working
      // indicator whatever this map says. A timer would only add a second way
      // to be wrong.
      const { job_id, active } = event.data;
      useLive.setState((s) => ({
        jobTurn: { ...s.jobTurn, [job_id]: { active, at: Date.now() } },
      }));
    } else if (event.type === "activity") {
      useLive.setState((s) => ({
        activity: [event.data.event, ...s.activity].slice(0, ACTIVITY_BUFFER),
      }));
      // Git/workspace happenings (clone finished, worktree added, discovery)
      // should refresh workspace lists live.
      const kind = event.data.event.kind;
      if (kind.startsWith("workspace.") || kind.startsWith("git.")) {
        queryClient.invalidateQueries({ queryKey: ["workspaces"] });
      }
      // Sessions too. A session you started yourself refreshes the list from
      // its own mutation, but one started somewhere else — by an agent, from
      // the CLI, on another machine — only ever arrived as an activity event,
      // so it sat invisible until something unrelated forced a refetch.
      if (kind.startsWith("session.")) {
        queryClient.invalidateQueries({ queryKey: ["sessions"] });
      }
      // Background jobs report completion through activity events.
      const payload = (event.data.event.payload ?? {}) as Record<string, unknown>;
      if (kind === "git.clone_finished" && typeof payload.job_id === "string") {
        const ok = payload.ok !== false;
        useJobs
          .getState()
          .finish(
            payload.job_id,
            ok,
            typeof payload.message === "string" ? payload.message : undefined,
          );
        // "Start work" on a clone still means start work — the session is
        // created once the repo has actually landed.
        runJobFollowUp(payload.job_id, ok);
      }
      // A new checkout can't receive sealed secrets from the server, so push
      // them from here while we still hold the passphrase.
      if (
        kind === "git.clone_finished" ||
        kind === "workspace.worktree_added" ||
        kind === "workspace.discovered" ||
        kind === "workspace.checkout_added"
      ) {
        const wsId = event.data.event.workspace_id;
        if (wsId && useAppPassword.getState().passphrase) {
          void resyncSealedSecrets(wsId, api as never);
        }
      }
      // Desktop notification + chime for things worth looking up for.
      notifyEvent(event.data.event);
    }
  };

  // Pull the current agent-state snapshot so tabs already running when the app
  // opens (or after a socket drop) show their mark without waiting for the next
  // hook. The push stream keeps it current after this.
  const seedAgentStates = async () => {
    const { data } = await api.GET("/api/v1/sessions/agent-states");
    if (data) useLive.getState().seedAgentStates(data);
  };

  connectUiSocket(handle, {
    // "Live" means the socket is open, not that an event has arrived — a quiet
    // fleet is still connected.
    onOpen: () => {
      useLive.setState({ connected: true });
      void seedAgentStates();
    },
    onClose: () => useLive.setState({ connected: false }),
    onReconnect: () => {
      // Reconnected after a gap: refetch everything that could have moved.
      queryClient.invalidateQueries();
    },
  });

  // Fade a mark whose agent went away without ever reporting idle (a crash, a
  // killed machine). The server sweeps its own copy on the same clock; this is
  // the client mirror so a spinner does not outlive the thing it tracks.
  setInterval(() => {
    const now = Date.now();
    useLive.setState((s) => {
      const stale = Object.entries(s.agentState).filter(
        ([, v]) => now - v.at > AGENT_STATE_STALE_MS,
      );
      if (stale.length === 0) return {};
      const agentState = { ...s.agentState };
      for (const [id] of stale) delete agentState[id];
      return { agentState };
    });
  }, 60 * 1000);
}
