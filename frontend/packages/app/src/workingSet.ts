// The tab strip's membership, as a synced per-user working set (MAIN-417).
//
// MAIN-322 made the strip a VIEW of the live session list, to fix a real bug:
// the open set lived in `localStorage`, so the same account showed different
// tabs on every machine. That fix was right about SYNC and wrong about
// MEMBERSHIP — with no local list to drop from, closing a tab could only end
// the session, which is how "close" came to mean "kill" (MAIN-324).
//
// This keeps the sync and gives the membership back. The set is stored against
// the PERSON, so it follows you between machines; it is stored as a user-scoped
// SETTING, so it is per-tenant without anyone writing per-tenant code — a
// settings row belongs to `(tenant, user)` already, and `GET /settings` only
// returns the active tenant's. Client work and personal repos cannot mix
// because they are not in the same row.
//
// Membership is the set. Presentation — name, runtime, machine, status — comes
// from the session list, which is why a session that DIED keeps its tab: it is
// still in your set, it is just no longer running.

import type { LiveSession, SessionTab } from "./sessionTabsStore";
import { reorderTabs } from "./sessionTabsStore";

/** The user-scoped setting this lives in. One key; the row is already scoped
 *  to the active tenant, which is the whole of AC-1's per-tenant requirement
 *  and NG-2's no-cross-tenant one. */
export const WORKING_SET_KEY = "sessions.workingset";

export interface WorkingSet {
  /** Session ids with a tab, in no particular order — `order` decides that. */
  open: string[];
  /** Pinned ids (MAIN-322's view pref, now synced rather than per-browser). */
  pinned: string[];
  /** Drag order. Ids not mentioned sort after those that are. */
  order: string[];
}

/** AC-5: the set starts EMPTY and is never seeded from the live sessions
 *  (NG-4). A blank strip on first load after the upgrade is the accepted cost —
 *  the navigator is how you fill it. */
export const EMPTY_WORKING_SET: WorkingSet = { open: [], pinned: [], order: [] };

/** Read a stored value, field by field.
 *
 *  Each array falls back on its own rather than the object falling back whole:
 *  a row written by an older build should cost you the one field it got wrong,
 *  not your entire strip. */
export function parseWorkingSet(value: unknown): WorkingSet {
  if (!value || typeof value !== "object") return { ...EMPTY_WORKING_SET };
  const v = value as Record<string, unknown>;
  const ids = (x: unknown) =>
    Array.isArray(x) ? x.filter((i): i is string => typeof i === "string") : [];
  return { open: ids(v.open), pinned: ids(v.pinned), order: ids(v.order) };
}

/** Add a session to the set. Idempotent: opening what is already open is what
 *  clicking a tab you are looking at does, and it must not reorder the strip. */
export function openSession(set: WorkingSet, id: string): WorkingSet {
  if (set.open.includes(id)) return set;
  return { ...set, open: [...set.open, id] };
}

/**
 * Remove a session from the set. **This ends nothing** (NG-1) — it is the
 * whole point of the card that closing a tab and killing a session became
 * different actions again.
 *
 * The id is dropped from `pinned` and `order` too, so a stored set cannot grow
 * without bound as sessions come and go.
 */
export function closeSession(set: WorkingSet, id: string): WorkingSet {
  if (!set.open.includes(id)) return set;
  return {
    open: set.open.filter((x) => x !== id),
    pinned: set.pinned.filter((x) => x !== id),
    order: set.order.filter((x) => x !== id),
  };
}

export function togglePinned(set: WorkingSet, id: string): WorkingSet {
  return {
    ...set,
    pinned: set.pinned.includes(id)
      ? set.pinned.filter((x) => x !== id)
      : [...set.pinned, id],
  };
}

/** Drag-reorder within a pin group, given the strip as displayed. Delegates to
 *  MAIN-322's rule so pinned-stays-ahead keeps meaning what it meant. */
export function reorderWorkingSet(
  set: WorkingSet,
  id: string,
  targetId: string,
  after: boolean,
  visible: SessionTab[],
): WorkingSet {
  const moved = reorderTabs(visible, id, targetId, after);
  if (moved === visible) return set;
  const movedIds = moved.map((t) => t.id);
  const seen = new Set(movedIds);
  return { ...set, order: [...movedIds, ...set.order.filter((x) => !seen.has(x))] };
}

/**
 * The strip: the working set, joined against every session the server knows
 * about — not just the live ones.
 *
 * A session that `exited` or `error`ed KEEPS its tab (AC-4), because it is
 * still in your set; only a session the server no longer has at all drops out,
 * since there is nothing left to show or restart. That is the one case where
 * membership is not purely the set's business: a tab pointing at a deleted row
 * could never be opened or dismissed into anything.
 */
export function deriveWorkingSetTabs(
  set: WorkingSet,
  sessions: LiveSession[],
  workspaceNames: Record<string, string>,
  nodeNames: Record<string, string> = {},
): SessionTab[] {
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const pinned = new Set(set.pinned);
  const rank = new Map(set.order.map((id, i) => [id, i]));
  return set.open
    .map((id, i) => ({ id, at: rank.get(id) ?? set.order.length + i }))
    .filter((x) => byId.has(x.id))
    .map((x) => {
      const s = byId.get(x.id)!;
      return {
        tab: {
          id: s.id,
          name: s.name,
          runtime: s.runtime,
          workspaceId: s.workspace_id ?? undefined,
          workspaceName: s.workspace_id ? workspaceNames[s.workspace_id] : undefined,
          nodeName: nodeNames[s.node_id],
          pinned: pinned.has(s.id),
          managed: s.managed ?? false,
        } satisfies SessionTab,
        at: x.at,
      };
    })
    .sort((a, b) => Number(!!b.tab.pinned) - Number(!!a.tab.pinned) || a.at - b.at)
    .map((r) => r.tab);
}

/** Ids in the set the server no longer knows about — nothing can be done with
 *  their tabs, so they are dropped on the next write rather than kept forever.
 *
 *  Separate from `deriveWorkingSetTabs` on purpose: deriving must never write,
 *  and a pending session query (`sessions` undefined) must never read as "every
 *  session is gone" and empty somebody's strip on page load. */
export function strandedIds(set: WorkingSet, sessions: LiveSession[] | undefined): string[] {
  if (!sessions) return [];
  const known = new Set(sessions.map((s) => s.id));
  return set.open.filter((id) => !known.has(id));
}
