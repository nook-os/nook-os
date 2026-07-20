// Live state pushed over /ws/ui. REST (TanStack Query) owns durable state;
// this store holds the deltas and pokes queries to refetch.
import { create } from "zustand";
import type { QueryClient } from "@tanstack/react-query";
import { connectUiSocket, type EventItem, type UiEvent } from "@nookos/api";

const ACTIVITY_BUFFER = 250;

interface LiveState {
  connected: boolean;
  nodeStatus: Record<string, string>;
  nodeResources: Record<string, unknown>;
  sessionStatus: Record<string, string>;
  activity: EventItem[];
  seedActivity(events: EventItem[]): void;
}

export const useLive = create<LiveState>(() => ({
  connected: false,
  nodeStatus: {},
  nodeResources: {},
  sessionStatus: {},
  activity: [],
  seedActivity(events) {
    useLive.setState((s) => {
      const known = new Set(s.activity.map((e) => e.id));
      const merged = [...s.activity, ...events.filter((e) => !known.has(e.id))];
      merged.sort((a, b) => (a.occurred_at < b.occurred_at ? 1 : -1));
      return { activity: merged.slice(0, ACTIVITY_BUFFER) };
    });
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
    }
  };

  connectUiSocket(
    (event) => {
      if (!useLive.getState().connected) useLive.setState({ connected: true });
      handle(event);
    },
    () => {
      // Reconnected after a gap: refetch everything that could have moved.
      queryClient.invalidateQueries();
    },
  );
}
