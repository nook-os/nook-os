// Live state pushed over /ws/ui. REST (TanStack Query) owns durable state;
// this store holds the deltas and pokes queries to refetch.
import { create } from "zustand";
import type { QueryClient } from "@tanstack/react-query";
import { connectUiSocket, type EventItem, type UiEvent } from "@nookos/api";
import { notifyEvent } from "./notify";
import { runJobFollowUp, useJobs } from "./jobs";
import { resyncSealedSecrets } from "./secretkeys";
import { useAppPassword } from "./apppassword";
import { useToasts } from "./Notifications";
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

export interface AgentState {
  /** `running` | `waiting`. `idle` is represented by absence. */
  state: string;
  /** tmux window the agent runs in, so the right terminal chip lights up. */
  window: number | null;
  /** Client receipt time (ms), for the staleness fallback above. */
  at: number;
}

interface LiveState {
  connected: boolean;
  nodeStatus: Record<string, string>;
  nodeResources: Record<string, unknown>;
  sessionStatus: Record<string, string>;
  /** Live agent activity per session (running/waiting). Absence means idle. */
  agentState: Record<string, AgentState>;
  activity: EventItem[];
  seedActivity(events: EventItem[]): void;
  seedAgentStates(items: { session_id: string; window?: number | null; state: string }[]): void;
}

export const useLive = create<LiveState>(() => ({
  connected: false,
  nodeStatus: {},
  nodeResources: {},
  sessionStatus: {},
  agentState: {},
  activity: [],
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
    } else if (event.type === "node_resources") {
      useLive.setState((s) => ({
        nodeResources: {
          ...s.nodeResources,
          [event.data.node_id]: event.data.resources,
        },
      }));
    } else if (event.type === "session_status") {
      useLive.setState((s) => ({
        sessionStatus: {
          ...s.sessionStatus,
          [event.data.session_id]: event.data.status,
        },
      }));
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
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
      queryClient.invalidateQueries({ queryKey: ["notifications"] });
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
