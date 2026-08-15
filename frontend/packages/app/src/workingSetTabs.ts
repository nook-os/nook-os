// The synced working set, as the tab strip consumes it (MAIN-417).
//
// Separate from `liveTabs.ts` rather than replacing it, because the two answer
// different questions and both are still wanted: the NAVIGATOR asks "what is
// running" (MAIN-414), and the STRIP asks "what do I have open". Folding them
// together would either put exited sessions in the pane or drop them from the
// strip, and AC-4 needs the second one to keep them.
import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@nookos/api";
import type { SessionTab } from "./sessionTabsStore";
import { useWorkspaceNames } from "./workspaces";
import {
  EMPTY_WORKING_SET,
  WORKING_SET_KEY,
  closeSession,
  deriveWorkingSetTabs,
  openSession,
  parseWorkingSet,
  reorderWorkingSet,
  strandedIds,
  togglePinned,
  type WorkingSet,
} from "./workingSet";

export interface WorkingSetTabs {
  tabs: SessionTab[];
  /** False until both the set and the session list have arrived. Deciding
   *  before then is how you flash an empty strip at somebody who has ten tabs. */
  loaded: boolean;
  open(id: string): void;
  close(id: string): void;
  togglePin(id: string): void;
  reorder(id: string, targetId: string, after: boolean, visible: SessionTab[]): void;
}

export function useWorkingSet(): WorkingSetTabs {
  const queryClient = useQueryClient();

  // EVERY session, not `active=true`: a session that exited keeps its tab
  // until dismissed (AC-4), and it cannot be rendered from a list that
  // excludes it.
  const { data: sessions } = useQuery({
    queryKey: ["sessions", "all-for-tabs"],
    queryFn: async () => (await api.GET("/api/v1/sessions")).data ?? [],
  });
  const { data: settings } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });

  const stored = React.useMemo(
    () => parseWorkingSet(settings?.find((s) => s.key === WORKING_SET_KEY)?.value),
    [settings],
  );
  // The local copy is what makes opening and closing feel immediate; the write
  // below is what makes it true on the next machine.
  const [local, setLocal] = React.useState<WorkingSet | null>(null);
  const set = local ?? stored;

  const write = React.useCallback(
    (next: WorkingSet, prev: WorkingSet) => {
      // The set operations return the SAME object when nothing changed —
      // opening what is already open, closing what is not. Viewing a session
      // runs `open` on every mount, so writing regardless would PUT on every
      // navigation.
      if (next === prev) return;
      setLocal(next);
      void api
        .PUT("/api/v1/settings/{key}", {
          params: { path: { key: WORKING_SET_KEY } },
          body: { scope: "user", value: next },
        })
        .then(() => queryClient.invalidateQueries({ queryKey: ["settings"] }))
        // A strip that failed to save is still the strip you have in front of
        // you; nothing was destroyed, so this is not worth a dialog.
        .catch(() => {});
    },
    [queryClient],
  );

  // Ids the server no longer has: nothing can open, restart or dismiss those
  // tabs, so they go. Guarded on the query having ARRIVED — pruning against a
  // pending list would empty the strip on every page load.
  const stranded = strandedIds(set, sessions).join(",");
  React.useEffect(() => {
    if (!stranded) return;
    const gone = stranded.split(",");
    setLocal((cur) => {
      const base = cur ?? set;
      const next = gone.reduce(closeSession, base);
      if (next === base) return cur;
      void api
        .PUT("/api/v1/settings/{key}", {
          params: { path: { key: WORKING_SET_KEY } },
          body: { scope: "user", value: next },
        })
        .catch(() => {});
      return next;
    });
    // `set` is intentionally out of the dependency list: this reacts to the
    // stranded ids changing, and including the object would re-run it on every
    // render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stranded]);

  // Names for the ids the sessions carry, read one row at a time (MAIN-606) —
  // the collection is paged, so indexing "every workspace" is not on offer.
  const workspaceNames = useWorkspaceNames((sessions ?? []).map((s) => s.workspace_id));
  const names = Object.fromEntries(workspaceNames);
  const nodeNames = Object.fromEntries((nodes ?? []).map((n) => [n.id, n.name]));
  const tabs = deriveWorkingSetTabs(set, sessions ?? [], names, nodeNames);

  return {
    tabs,
    loaded: sessions !== undefined && settings !== undefined,
    open: (id) => write(openSession(set, id), set),
    close: (id) => write(closeSession(set, id), set),
    togglePin: (id) => write(togglePinned(set, id), set),
    reorder: (id, targetId, after, visible) =>
      write(reorderWorkingSet(set, id, targetId, after, visible), set),
  };
}

export { EMPTY_WORKING_SET };
