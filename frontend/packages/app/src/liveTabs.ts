// Which sessions are your tabs, and in what order (MAIN-321).
//
// Extracted from `SessionTabs` because a SECOND caller now needs the same
// answer: `/sessions` opens the first session rather than a list, and "first"
// has to mean the leftmost tab. Two copies of this rule would drift, and the
// symptom would be the nav landing on a session that is visibly not the first
// tab — the kind of wrong that looks like a bug in the tab strip.
//
// No behaviour of its own: it is the queries and the filter `SessionTabs`
// already ran, moved somewhere both callers can reach.
import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { useWorkspaceContext } from "./context";
import { deriveTabs, useSessionTabPrefs, type SessionTab } from "./sessionTabsStore";

export interface LiveTabs {
  tabs: SessionTab[];
  /** False until the session list has actually arrived. The difference between
   *  "you have no sessions" and "we have not asked yet" decides whether the
   *  caller redirects, waits, or shows an empty state — and getting it wrong
   *  means flashing "no sessions" at somebody who has ten. */
  loaded: boolean;
}

export interface LiveTabsOptions {
  /** Ignore the workspace context and return every workspace's sessions.
   *
   *  For the session navigator (MAIN-414), which exists to find a session you
   *  have NOT got open — including one in a workspace you are not scoped to.
   *  The strip keeps the scoping; this is an opt-in for the one caller whose
   *  whole job is the wider view. */
  allWorkspaces?: boolean;
}

export function useLiveTabs(opts: LiveTabsOptions = {}): LiveTabs {
  const prefs = useSessionTabPrefs((s) => s.prefs);
  const prune = useSessionTabPrefs((s) => s.prune);
  const scoped = useWorkspaceContext((s) => s.selectedWorkspaceId);
  const selectedWorkspaceId = opts.allWorkspaces ? null : scoped;

  // Unscoped by workspace on purpose — the workspace context filters the strip
  // below, but the QUERY must see everything or a session on another machine
  // could not have a tab. The live bus invalidates `["sessions"]` on any
  // session event, so this updates without a reload.
  const { data: sessions } = useQuery({
    queryKey: ["sessions", "tabs"],
    queryFn: async () =>
      (await api.GET("/api/v1/sessions", { params: { query: { active: true } } })).data ?? [],
  });
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });
  // Machine names for the per-tab badge (MAIN-323 AC-2). The session rows carry
  // only `node_id`, and "one repo across four VMs" is unreadable without this.
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });

  // Whose sessions belong in a tab strip. The control plane already scopes a
  // plain member to the sessions they created, so this only bites an
  // owner/admin — whose list is the WHOLE tenant, and whose tab strip would
  // otherwise fill with their team's terminals. An unattributed session (no
  // creator: started by a node or a job) is kept rather than hidden: it is not
  // somebody else's, and silently dropping it is how work becomes invisible.
  const mineId = me?.user?.id;
  const mine = (sessions ?? []).filter(
    (s) => !mineId || !s.created_by || s.created_by === mineId,
  );
  const names = Object.fromEntries((workspaces ?? []).map((w) => [w.id, w.name]));
  const nodeNames = Object.fromEntries((nodes ?? []).map((n) => [n.id, n.name]));
  const tabs = deriveTabs(mine, names, prefs, selectedWorkspaceId, nodeNames);

  // Prefs outlive the sessions they name, so drop the dead ones. Keyed on the
  // full live list, not the visible strip, or switching workspace context would
  // read as "those sessions are gone" and discard another context's order.
  // Passing `undefined` while the query is pending is load-bearing: an empty
  // list would read as "every session is gone" and wipe the user's pin/order on
  // each page load. The store refuses to prune on that.
  const liveIds = sessions ? mine.map((s) => s.id).join(",") : undefined;
  useEffect(() => {
    prune(liveIds === undefined ? undefined : liveIds ? liveIds.split(",") : []);
  }, [liveIds, prune]);

  return { tabs, loaded: sessions !== undefined };
}
