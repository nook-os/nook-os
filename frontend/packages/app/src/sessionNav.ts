// The session navigator's rules (MAIN-414), kept out of the component so the
// three that can actually be wrong are testable without a browser: what a
// folder IS, what a search leaves standing, and when the pane pushes rather
// than covers the terminal.
//
// Folders are not grouped here — `tabGroups.groupTabs` is called and its answer
// is adopted. That is the point: the pane and the tab strip must never disagree
// about which workspace a session belongs to, what a workspace-less terminal is
// called, or what colour any of it is. Two implementations of "group by
// workspace" would drift, and the symptom would be a repo that is amber in the
// strip and green in the pane.

import type { SessionTab } from "./sessionTabsStore";
import { groupTabs } from "./tabGroups";

/** One workspace's sessions in the tree. `key` and `hue` are `tabGroups`'. */
export interface NavFolder {
  key: string;
  label: string;
  hue: number;
  sessions: SessionTab[];
}

/** The tree, in the strip's own group order. */
export function navFolders(tabs: SessionTab[]): NavFolder[] {
  return groupTabs(tabs, []).map((g) => ({
    key: g.key,
    label: g.label,
    hue: g.hue,
    sessions: g.tabs,
  }));
}

/**
 * Narrow the tree in place: sessions that do not match drop out, folders left
 * with nothing drop out, and everything else keeps its position.
 *
 * Every whitespace-separated word must hit somewhere — the same rule as
 * `matchSections`, so "claude api" narrows to a claude session in the api repo
 * rather than widening to everything that is either.
 *
 * A session's haystack includes ITS FOLDER's label, which is what makes typing
 * a workspace name keep that folder whole instead of emptying it.
 */
export function filterFolders(folders: NavFolder[], term: string): NavFolder[] {
  const words = term.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return folders;
  return folders
    .map((f) => ({
      ...f,
      sessions: f.sessions.filter((s) => {
        const hay = [s.name, s.runtime, f.label, s.nodeName ?? ""]
          .join(" ")
          .toLowerCase();
        return words.every((w) => hay.includes(w));
      }),
    }))
    .filter((f) => f.sessions.length > 0);
}

/** Whether the pane displaces the terminal or floats over it. */
export type PaneMode = "push" | "overlay";

/** Below this much room for the terminal, pushing would squeeze it into
 *  uselessness — so an unpinned pane floats instead. */
export const MIN_CONTENT_WIDTH = 640;

export const MIN_PANE_WIDTH = 180;
export const MAX_PANE_WIDTH = 480;
export const DEFAULT_PANE_WIDTH = 260;

/**
 * **Pinned means pushed, at any width.** That is the whole point of the pin:
 * "keep it beside the terminal even when things are tight" is a decision only
 * the person looking at the screen can make, and a width threshold that
 * overrode it would silently take it back.
 */
export function paneMode(opts: {
  pinned: boolean;
  viewportWidth: number;
  paneWidth: number;
}): PaneMode {
  if (opts.pinned) return "push";
  return opts.viewportWidth - opts.paneWidth < MIN_CONTENT_WIDTH
    ? "overlay"
    : "push";
}

/** The user-scoped setting this pane persists under. One key holding all three
 *  values, so opening the app costs one settings read and not three. */
export const NAV_PREFS_KEY = "sessions.navigator";

export interface NavPrefs {
  width: number;
  collapsed: boolean;
  pinned: boolean;
}

export const DEFAULT_NAV_PREFS: NavPrefs = {
  width: DEFAULT_PANE_WIDTH,
  collapsed: false,
  pinned: false,
};

export function clampWidth(w: number): number {
  if (!Number.isFinite(w)) return DEFAULT_PANE_WIDTH;
  return Math.min(MAX_PANE_WIDTH, Math.max(MIN_PANE_WIDTH, Math.round(w)));
}

/** Read a stored value into prefs, field by field.
 *
 *  Every field falls back independently rather than the object falling back as
 *  a whole: a settings row written by an older build (or hand-edited) should
 *  cost you the one value it got wrong, not your pane width as well. */
export function parseNavPrefs(value: unknown): NavPrefs {
  if (!value || typeof value !== "object") return { ...DEFAULT_NAV_PREFS };
  const v = value as Record<string, unknown>;
  return {
    width:
      typeof v.width === "number"
        ? clampWidth(v.width)
        : DEFAULT_NAV_PREFS.width,
    collapsed:
      typeof v.collapsed === "boolean"
        ? v.collapsed
        : DEFAULT_NAV_PREFS.collapsed,
    pinned:
      typeof v.pinned === "boolean" ? v.pinned : DEFAULT_NAV_PREFS.pinned,
  };
}
