// Pure derivations for Mission Control (MAIN-226), kept out of the component so
// grouping, ghosting, lamp filtering, and the clone-only worktree rule are
// unit-testable without a DOM.
import type {
  Overview,
  OverviewCheckout,
  OverviewWorkspace,
  Session,
} from "@nookos/api";

export interface NodeGroup {
  nodeId: string;
  nodeName: string;
  nodeStatus: string;
  checkouts: OverviewCheckout[];
}

/** One workspace's checkouts grouped by the node they live on, order preserved
 *  (the endpoint already sorts by node name then path). */
export function groupCheckoutsByNode(
  checkouts: OverviewCheckout[],
): NodeGroup[] {
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

/** A checkout that has vanished from disk (MAIN-220 tombstone) — a "ghost".
 *  Hidden by default behind the ghosts toggle; ghosted (not hidden) when shown. */
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
    // The ticket is what people actually search for — "where is MAIN-42
    // running" is the question this page exists to answer (MAIN-230 AC-4).
    for (const t of c.tasks ?? []) hay.push(t.key, t.title);
  }
  for (const s of w.unbound_sessions) hay.push(s.name, s.runtime);
  return hay.join(" ").toLowerCase().includes(needle);
}

// ── The annunciator deck ─────────────────────────────────────────────────────

/** Orientation counters for the deck's left side. Nodes are the distinct
 *  machines hosting a visible checkout. */
export interface DeckStats {
  nodesOnline: number;
  nodesTotal: number;
  repos: number;
  checkouts: number;
  sessions: number;
}

export function deckStats(ov: Overview | undefined): DeckStats {
  const empty = {
    nodesOnline: 0,
    nodesTotal: 0,
    repos: 0,
    checkouts: 0,
    sessions: 0,
  };
  if (!ov) return empty;
  const nodes = new Map<string, string>();
  let checkouts = 0;
  let sessions = ov.loose_sessions.length;
  for (const w of ov.workspaces) {
    sessions += w.unbound_sessions.length;
    for (const c of w.checkouts) {
      nodes.set(c.node_id, c.node_status);
      checkouts += 1;
      sessions += c.sessions.length;
    }
  }
  return {
    nodesOnline: [...nodes.values()].filter((s) => s === "online").length,
    nodesTotal: nodes.size,
    repos: ov.workspaces.length,
    checkouts,
    sessions,
  };
}

/** The annunciator lamps. Each lights only when its count is non-zero; clicking
 *  a lit lamp filters the tree to the rows it counted. */
export type Lamp = "dirty" | "missing" | "offline";

export interface ExceptionCounts {
  dirty: number;
  missing: number;
  offline: number;
}

export function exceptionCounts(ov: Overview | undefined): ExceptionCounts {
  const out = { dirty: 0, missing: 0, offline: 0 };
  if (!ov) return out;
  const offlineNodes = new Set<string>();
  for (const w of ov.workspaces) {
    for (const c of w.checkouts) {
      if (isMissing(c)) out.missing += 1;
      else if (c.dirty) out.dirty += 1;
      if (c.node_status !== "online") offlineNodes.add(c.node_id);
    }
  }
  out.offline = offlineNodes.size;
  return out;
}

/** Does this checkout belong under the given lamp? */
export function lampMatches(c: OverviewCheckout, lamp: Lamp): boolean {
  switch (lamp) {
    case "dirty":
      return c.dirty && !isMissing(c);
    case "missing":
      return isMissing(c);
    case "offline":
      return c.node_status !== "online";
  }
}

// ── Visibility: filter → lamp → ghosts ───────────────────────────────────────

/** What the tree renders for one workspace after the free-text filter, the
 *  active lamp, and the ghosts toggle. `hiddenGhosts` is the count the repo
 *  header hints at while the toggle is off. */
export interface VisibleRepo {
  workspace: OverviewWorkspace;
  checkouts: OverviewCheckout[];
  hiddenGhosts: number;
}

export function visibleRepos(
  ov: Overview | undefined,
  q: string,
  lamp: Lamp | null,
  showGhosts: boolean,
): VisibleRepo[] {
  if (!ov) return [];
  const out: VisibleRepo[] = [];
  for (const w of ov.workspaces) {
    if (!matchesQuery(w, q)) continue;
    let checkouts = w.checkouts;
    if (lamp) checkouts = checkouts.filter((c) => lampMatches(c, lamp));
    const ghosts = checkouts.filter(isMissing).length;
    // The missing lamp is an explicit request to see ghosts; otherwise the
    // toggle decides.
    if (!showGhosts && lamp !== "missing") {
      checkouts = checkouts.filter((c) => !isMissing(c));
    }
    if (lamp) {
      // A lamp narrows to matching rows only.
      if (checkouts.length === 0) continue;
    } else if (
      checkouts.length === 0 &&
      w.unbound_sessions.length === 0 &&
      ghosts === 0
    ) {
      continue;
    }
    out.push({
      workspace: w,
      checkouts,
      hiddenGhosts: showGhosts || lamp === "missing" ? 0 : ghosts,
    });
  }
  return out;
}

/** Repo-header rollup, so a collapsed repo still reports what it holds. */
export function repoRollup(w: OverviewWorkspace): {
  sessions: number;
  checkouts: number;
  dirty: number;
  missing: number;
} {
  let sessions = w.unbound_sessions.length;
  let dirty = 0;
  let missing = 0;
  for (const c of w.checkouts) {
    sessions += c.sessions.length;
    if (isMissing(c)) missing += 1;
    else if (c.dirty) dirty += 1;
  }
  return { sessions, checkouts: w.checkouts.length, dirty, missing };
}

// ── Live overlay ─────────────────────────────────────────────────────────────

/** The overview REST payload with the websocket deltas laid over it: node and
 *  session statuses come from the live store when present, and a session that
 *  died since the fetch drops out (the endpoint only ever returns active ones).
 *  Everything downstream — deck, lamps, tree — reads the overlaid truth. */
export function overlayLive(
  ov: Overview | undefined,
  nodeStatus: Record<string, string>,
  sessionStatus: Record<string, string>,
): Overview | undefined {
  if (!ov) return ov;
  const alive = (s: Session): boolean => {
    const st = sessionStatus[s.id] ?? s.status;
    return st !== "exited" && st !== "error" && st !== "killed";
  };
  const overlay = (s: Session): Session => ({
    ...s,
    status: sessionStatus[s.id] ?? s.status,
  });
  return {
    workspaces: ov.workspaces.map((w) => ({
      ...w,
      checkouts: w.checkouts.map((c) => ({
        ...c,
        node_status: nodeStatus[c.node_id] ?? c.node_status,
        sessions: c.sessions.filter(alive).map(overlay),
      })),
      unbound_sessions: w.unbound_sessions.filter(alive).map(overlay),
    })),
    loose_sessions: ov.loose_sessions.filter(alive).map(overlay),
  };
}

// ── Alternate views ──────────────────────────────────────────────────────────

/** How the fleet is organized on screen. All views share the deck, the filter,
 *  the lamps and the ghosts toggle — only the grouping changes. */
export type MissionView = "tree" | "grid" | "machines" | "matrix" | "canvas";

const VIEW_KEY = "nook.mission.view.v1";

export function loadView(): MissionView {
  try {
    const v = window.localStorage.getItem(VIEW_KEY);
    return v === "grid" || v === "machines" || v === "matrix" || v === "canvas"
      ? v
      : "tree";
  } catch {
    return "tree";
  }
}

export function saveView(v: MissionView): void {
  try {
    window.localStorage.setItem(VIEW_KEY, v);
  } catch {
    // Storage unavailable: the choice just won't persist.
  }
}

/** Machine-first regrouping of the (already filtered) repos: one group per
 *  node, holding every checkout on it with its owning workspace. Node order is
 *  first-appearance, matching the tree's ordering. */
export interface MachineGroup {
  nodeId: string;
  nodeName: string;
  nodeStatus: string;
  entries: { workspace: OverviewWorkspace; checkout: OverviewCheckout }[];
}

export function machineGroups(repos: VisibleRepo[]): MachineGroup[] {
  const order: string[] = [];
  const by: Record<string, MachineGroup> = {};
  for (const { workspace, checkouts } of repos) {
    for (const c of checkouts) {
      if (!by[c.node_id]) {
        by[c.node_id] = {
          nodeId: c.node_id,
          nodeName: c.node_name,
          nodeStatus: c.node_status,
          entries: [],
        };
        order.push(c.node_id);
      }
      by[c.node_id].entries.push({ workspace, checkout: c });
    }
  }
  return order.map((id) => by[id]);
}

/** The repos × machines board: one column per node, one row per repo, each
 *  cell the checkouts of that repo on that node. */
export interface MatrixData {
  nodes: { id: string; name: string; status: string }[];
  rows: {
    workspace: OverviewWorkspace;
    cells: Record<string, OverviewCheckout[]>;
  }[];
}

export function matrixData(repos: VisibleRepo[]): MatrixData {
  const nodeOrder: string[] = [];
  const nodes: Record<string, { id: string; name: string; status: string }> =
    {};
  const rows: MatrixData["rows"] = [];
  for (const { workspace, checkouts } of repos) {
    const cells: Record<string, OverviewCheckout[]> = {};
    for (const c of checkouts) {
      if (!nodes[c.node_id]) {
        nodes[c.node_id] = {
          id: c.node_id,
          name: c.node_name,
          status: c.node_status,
        };
        nodeOrder.push(c.node_id);
      }
      (cells[c.node_id] ??= []).push(c);
    }
    rows.push({ workspace, cells });
  }
  return { nodes: nodeOrder.map((id) => nodes[id]), rows };
}

// ── Canvas positions ─────────────────────────────────────────────────────────

const CANVAS_KEY = "nook.mission.canvas.v1";

export type CanvasPositions = Record<string, { x: number; y: number }>;

/** Where each machine card sits on the canvas, surviving reloads. */
export function loadCanvasPositions(): CanvasPositions {
  try {
    const raw = window.localStorage.getItem(CANVAS_KEY);
    const v = raw ? (JSON.parse(raw) as unknown) : {};
    if (v && typeof v === "object" && !Array.isArray(v)) {
      const out: CanvasPositions = {};
      for (const [k, p] of Object.entries(v as Record<string, unknown>)) {
        if (
          p &&
          typeof p === "object" &&
          typeof (p as { x: unknown }).x === "number" &&
          typeof (p as { y: unknown }).y === "number"
        ) {
          out[k] = { x: (p as { x: number }).x, y: (p as { y: number }).y };
        }
      }
      return out;
    }
    return {};
  } catch {
    return {};
  }
}

export function saveCanvasPositions(p: CanvasPositions): void {
  try {
    window.localStorage.setItem(CANVAS_KEY, JSON.stringify(p));
  } catch {
    // Storage unavailable: the arrangement just won't persist.
  }
}

/** Staggered starting spot for the i-th card a person hasn't placed yet. */
export function defaultCanvasPosition(i: number): { x: number; y: number } {
  return { x: 20 + (i % 3) * 380, y: 20 + Math.floor(i / 3) * 260 };
}

// ── Collapse persistence ─────────────────────────────────────────────────────

const COLLAPSE_KEY = "nook.mission.collapsed.v1";

/** Repo ids the person collapsed, surviving reloads (localStorage). Fails open
 *  — nothing collapsed — on any storage weirdness. */
export function loadCollapsed(): Set<string> {
  try {
    const raw = window.localStorage.getItem(COLLAPSE_KEY);
    const ids = raw ? (JSON.parse(raw) as unknown) : [];
    return new Set(
      Array.isArray(ids) ? ids.filter((v) => typeof v === "string") : [],
    );
  } catch {
    return new Set();
  }
}

export function saveCollapsed(ids: Set<string>): void {
  try {
    window.localStorage.setItem(COLLAPSE_KEY, JSON.stringify([...ids]));
  } catch {
    // Storage unavailable: collapse state just won't persist.
  }
}

// ── Context bits ─────────────────────────────────────────────────────────────

/** Compact relative age: "now", "5m", "2h", "3d". Empty for missing input, so
 *  a fixture without timestamps renders nothing rather than crashing. */
export function age(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.floor((now - t) / 1000));
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** A stable identity color per repo, hashed from its slug — the same hue in
 *  every view, so "the teal one" means the same repo everywhere. Muted
 *  saturation/lightness to sit inside the console palette. */
export function repoTint(slug: string): string {
  let h = 0;
  for (let i = 0; i < slug.length; i++) h = (h * 31 + slug.charCodeAt(i)) >>> 0;
  return `hsl(${h % 360} 45% 55%)`;
}
