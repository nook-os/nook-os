// Named `sessionTabsStore`, not `sessiontabs`, because `SessionTabs.tsx` sits
// beside it. Two files whose names differ only in case are distinct on Linux
// and the SAME FILE on macOS and Windows, where the import resolved to the
// wrong one and the frontend would not build at all. CI on Linux was happy;
// every Mac was not.
//
// MAIN-322: the tab strip IS the live session list, not a per-browser open-set.
// It used to be the latter — every session you visited was appended to a
// localStorage array, so your tabs were whatever *this* browser happened to
// have opened, and the same account on a second machine showed a different
// strip. Now the set of tabs is derived from `GET /api/v1/sessions?active=true`,
// which is the same answer on every machine.
//
// What stays client-side is only what the ticket allows: view-only prefs that
// change the ORDER of the strip, never its membership — which tabs are pinned
// and the drag-chosen order. A pref for a session that no longer exists cannot
// bring a tab back, because membership is not a pref.
import { create } from "zustand";
import {
  activeControlPlaneKey,
  sessionTabPrefsKey,
  sessionTabsKey,
} from "./desktop";

export interface SessionTab {
  id: string;
  name: string;
  runtime: string;
  /** Owning workspace — tabs are filtered by the active workspace context.
   *  Absent for an ad-hoc terminal, which belongs to no workspace and so shows
   *  in every context. */
  workspaceId?: string;
  workspaceName?: string;
  /** The machine it runs on (MAIN-323 AC-2). One repo across four VMs is only
   *  legible inside a single group if each tab says which VM it is. */
  nodeName?: string;
  /** Pinned tabs sort first and are a view-only pref (MAIN-322). */
  pinned?: boolean;
  /** Reconciler-owned, from `Session.managed` (MAIN-318). Decides what closing
   *  the tab MEANS (MAIN-324): killing a managed session does not close it, the
   *  next reconcile pass starts another. Never inferred here — a hand-started
   *  terminal in a managed workspace, on an eligible node, with the spec's
   *  runtime, is indistinguishable from a replica by inspection. */
  managed?: boolean;
}

/** The fields of a live session the strip needs. A subset of the API's
 *  `Session`, so the derivation can be tested without a control plane. */
export interface LiveSession {
  id: string;
  name: string;
  runtime: string;
  workspace_id?: string | null;
  node_id: string;
  /** Whether the reconciler owns this session (MAIN-318). Optional so the
   *  derivation can be tested with a bare fixture; absent reads as ad-hoc,
   *  which is the safe default — it makes closing ask before killing rather
   *  than silently editing a workspace declaration. */
  managed?: boolean;
}

/** The only client-side state left: how the strip is ORDERED, never what it
 *  contains. Both arrays hold session ids. */
export interface TabPrefs {
  pinned: string[];
  /** Workspace ids whose group is collapsed (MAIN-323 AC-3). Beside pin and
   *  order because it is the same KIND of thing — a view pref that changes how
   *  the strip is arranged, never which sessions are on it. */
  collapsed: string[];
  order: string[];
}

const PREFS_KEY = sessionTabPrefsKey(activeControlPlaneKey());

/** Move `id` to just before/after `targetId`, but only WITHIN a pin group.
 *
 *  Pure so the ordering rule is testable without the store or localStorage. The
 *  tab strip renders pinned-first via a stable sort, so the array order is the
 *  within-group order; moving among same-group items here is exactly what the
 *  visible strip shows. A cross-group move (pinned ↔ unpinned) is rejected —
 *  returned unchanged — because pinned tabs stay grouped ahead (AC-3), and a
 *  self-drop or an unknown id is a no-op. */
export function reorderTabs(
  tabs: SessionTab[],
  id: string,
  targetId: string,
  after: boolean,
): SessionTab[] {
  if (id === targetId) return tabs;
  const dragged = tabs.find((t) => t.id === id);
  const target = tabs.find((t) => t.id === targetId);
  if (!dragged || !target) return tabs;
  if (!!dragged.pinned !== !!target.pinned) return tabs; // cross-group: rejected
  const without = tabs.filter((t) => t.id !== id);
  const at = without.findIndex((t) => t.id === targetId) + (after ? 1 : 0);
  return [...without.slice(0, at), dragged, ...without.slice(at)];
}

/** The visible strip: live sessions, in the user's chosen order.
 *
 *  Pure, and the whole rule in one place — membership comes from `sessions`
 *  alone (AC-1/AC-2), and `prefs` may only reorder what is already there
 *  (AC-4). A session absent from the live list has no tab no matter what the
 *  prefs say, which is what makes "the tabs ARE the sessions" true rather than
 *  merely usually true.
 *
 *  NOT scoped by workspace. It was, until MAIN-417 made the strip a working set
 *  you curate yourself — at which point filtering your own chosen set by
 *  workspace was filtering it twice, and the only surviving caller was passing
 *  an opt-out to switch it off. */
export function deriveTabs(
  sessions: LiveSession[],
  workspaceNames: Record<string, string>,
  prefs: TabPrefs,
  nodeNames: Record<string, string> = {},
): SessionTab[] {
  const pinned = new Set(prefs.pinned);
  const rank = new Map(prefs.order.map((id, i) => [id, i]));
  return sessions
    .map((s, i) => ({
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
      // A session the user has never dragged sorts after every one they have,
      // in the order the server listed it — so a new session appends rather
      // than landing in the middle of a hand-arranged strip.
      at: rank.get(s.id) ?? prefs.order.length + i,
    }))
    .sort((a, b) => Number(!!b.tab.pinned) - Number(!!a.tab.pinned) || a.at - b.at)
    .map((r) => r.tab);
}

function load(): TabPrefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (raw) {
      const p = JSON.parse(raw) as Partial<TabPrefs>;
      return {
        pinned: Array.isArray(p.pinned) ? p.pinned : [],
        collapsed: Array.isArray(p.collapsed) ? p.collapsed : [],
        order: Array.isArray(p.order) ? p.order : [],
      };
    }
  } catch {
    // corrupted prefs — start fresh
  }
  return { pinned: [], collapsed: [], order: [] };
}

function save(prefs: TabPrefs) {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // storage full/unavailable — prefs just won't persist
  }
}

// Retire the old open-set for real, rather than leaving it to rot: with the
// strip sourced from the API nothing will ever read this key again, and a stale
// list of session ids from months ago is only a confusing thing to find in
// devtools.
try {
  localStorage.removeItem(sessionTabsKey(activeControlPlaneKey()));
} catch {
  // ignore
}

interface SessionTabPrefsState {
  prefs: TabPrefs;
  togglePin(id: string): void;
  /** Collapse or expand one workspace's group (MAIN-323 AC-3). */
  toggleCollapsed(workspaceKey: string): void;
  /** Drag-reorder: move `id` before/after `targetId` within its pin group,
   *  given the strip as currently displayed. */
  reorder(id: string, targetId: string, after: boolean, visible: SessionTab[]): void;
  /** Drop prefs for sessions that no longer exist, so the stored ids cannot
   *  grow without bound as sessions come and go.
   *
   *  `undefined` means "the live list is not known yet" and is a no-op — a
   *  pending or failed session query must not read as "every session is gone"
   *  and wipe the user's pin/order on page load. */
  prune(liveIds: string[] | undefined): void;
}

export const useSessionTabPrefs = create<SessionTabPrefsState>((set) => ({
  prefs: load(),
  togglePin: (id) =>
    set((s) => {
      const pinned = s.prefs.pinned.includes(id)
        ? s.prefs.pinned.filter((x) => x !== id)
        : [...s.prefs.pinned, id];
      const prefs = { ...s.prefs, pinned };
      save(prefs);
      return { prefs };
    }),
  toggleCollapsed: (workspaceKey) =>
    set((s) => {
      const collapsed = s.prefs.collapsed.includes(workspaceKey)
        ? s.prefs.collapsed.filter((x) => x !== workspaceKey)
        : [...s.prefs.collapsed, workspaceKey];
      const prefs = { ...s.prefs, collapsed };
      save(prefs);
      return { prefs };
    }),
  reorder: (id, targetId, after, visible) =>
    set((s) => {
      const moved = reorderTabs(visible, id, targetId, after);
      if (moved === visible) return s; // rejected/no-op — don't churn storage
      // The dragged strip is the new truth for the tabs it contains; ids it
      // does not mention (another workspace's context) keep their old rank.
      const shown = new Set(moved.map((t) => t.id));
      const prefs = {
        ...s.prefs,
        order: [...moved.map((t) => t.id), ...s.prefs.order.filter((x) => !shown.has(x))],
      };
      save(prefs);
      return { prefs };
    }),
  prune: (liveIds) =>
    set((s) => {
      if (!liveIds) return s; // list unknown — never prune on a guess
      const live = new Set(liveIds);
      const pinned = s.prefs.pinned.filter((id) => live.has(id));
      const order = s.prefs.order.filter((id) => live.has(id));
      if (pinned.length === s.prefs.pinned.length && order.length === s.prefs.order.length) {
        return s;
      }
      // `collapsed` holds WORKSPACE ids and is deliberately not pruned here: a
      // workspace with no live sessions still exists, and forgetting that its
      // group was collapsed every time its last session ends would make the
      // setting feel random.
      const prefs = { ...s.prefs, pinned, order };
      save(prefs);
      return { prefs };
    }),
}));
