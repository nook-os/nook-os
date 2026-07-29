// Pure derivations for Mission Control (MAIN-226), kept out of the component so
// grouping, ghosting, and the clone-only worktree rule are unit-testable without
// a DOM.
import type { Overview, OverviewCheckout, OverviewWorkspace } from "@nookos/api";

export interface NodeGroup {
  nodeId: string;
  nodeName: string;
  nodeStatus: string;
  checkouts: OverviewCheckout[];
}

/** One workspace's checkouts grouped by the node they live on, order preserved
 *  (the endpoint already sorts by node name then path). */
export function groupCheckoutsByNode(checkouts: OverviewCheckout[]): NodeGroup[] {
  const order: string[] = [];
  const by: Record<string, NodeGroup> = {};
  for (const c of checkouts) {
    if (!by[c.node_id]) {
      by[c.node_id] = {
        nodeId: c.node_id,
        nodeName: c.node_name,
        nodeStatus: c.node_status,
        checkouts: [],
      };
      order.push(c.node_id);
    }
    by[c.node_id].checkouts.push(c);
  }
  return order.map((id) => by[id]);
}

/** A checkout that has vanished from disk (MAIN-220 tombstone) — the UI ghosts
 *  it rather than hiding it. */
export const isMissing = (c: OverviewCheckout): boolean => !!c.missing_at;

/** "+ worktree" is offered on a present primary clone only (AC-4). */
export const canAddWorktree = (c: OverviewCheckout): boolean =>
  c.kind === "clone" && !isMissing(c);

/** "terminal here" is offered on any present checkout (AC-4). */
export const canOpenTerminal = (c: OverviewCheckout): boolean => !isMissing(c);

/** Case-insensitive free-text match over a workspace's repo / node / branch /
 *  session text (AC-5). An empty query keeps everything. */
export function matchesQuery(w: OverviewWorkspace, q: string): boolean {
  const needle = q.trim().toLowerCase();
  if (!needle) return true;
  const hay: string[] = [
    w.name,
    w.slug,
    w.git_remote_url ?? "",
    w.git_remote_normalized ?? "",
  ];
  for (const c of w.checkouts) {
    hay.push(c.node_name, c.path, c.branch ?? "", c.kind);
    for (const s of c.sessions) hay.push(s.name, s.runtime);
  }
  for (const s of w.unbound_sessions) hay.push(s.name, s.runtime);
  return hay.join(" ").toLowerCase().includes(needle);
}

/** The visible workspaces after applying the filter. */
export function filterOverview(ov: Overview | undefined, q: string): OverviewWorkspace[] {
  if (!ov) return [];
  return ov.workspaces.filter((w) => matchesQuery(w, q));
}
