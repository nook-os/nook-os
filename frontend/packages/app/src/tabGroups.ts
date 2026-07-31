// Chrome-style tab groups for the session strip (MAIN-323).
//
// Flat tabs stop scaling at about one repo times four machines: twelve
// identically-shaped chips, and nothing says which repo or which VM any of them
// is. Grouping by workspace answers the first question and the machine badge
// answers the second.
//
// Pure, and separate from `SessionTabs.tsx`, because the two rules worth
// getting right cannot be seen in a screenshot: which group a tab belongs to
// when it has no workspace, and what a collapsed group does with the session
// you are currently looking at.

import type { SessionTab } from "./sessionTabsStore";

/** The group key for tabs that belong to no workspace — ad-hoc `$HOME`
 *  terminals. A real key rather than `undefined` so it can be collapsed and
 *  remembered like any other group. */
export const ADHOC_GROUP = "@adhoc";

export interface TabGroup {
  /** Workspace id, or [`ADHOC_GROUP`]. Stable, so collapse state keys on it. */
  key: string;
  label: string;
  /** 0–359. Derived from the key, never stored: the same repo is the same
   *  colour on every machine and after every reload, with no palette to keep in
   *  sync and no colour to run out of. */
  hue: number;
  tabs: SessionTab[];
  collapsed: boolean;
}

/** A stable hue from a string. FNV-1a — small, and it scatters adjacent ids
 *  (`…a1`, `…a2`) instead of giving them neighbouring colours, which matters
 *  because workspace ids are sequential UUIDv7s. */
export function hueOf(key: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return Math.abs(h) % 360;
}

/**
 * Group the visible strip by workspace, preserving tab order within a group.
 *
 * Group order follows the FIRST appearance of each group in the tab list, so
 * the strip's existing order — pinned first, then the user's drag order — still
 * decides what sits leftmost. Sorting groups by name instead would silently
 * override a pin.
 *
 * **A collapsed group containing the active session is expanded anyway.** That
 * is not a nicety: the strip is how you know where you are, and hiding the tab
 * you are looking at leaves the terminal below it unexplained. Chrome does the
 * same thing.
 */
export function groupTabs(
  tabs: SessionTab[],
  collapsed: string[],
  activeId?: string,
): TabGroup[] {
  const shut = new Set(collapsed);
  const byKey = new Map<string, TabGroup>();

  for (const t of tabs) {
    const key = t.workspaceId ?? ADHOC_GROUP;
    let g = byKey.get(key);
    if (!g) {
      g = {
        key,
        label:
          key === ADHOC_GROUP ? "Terminals" : (t.workspaceName ?? "workspace"),
        hue: hueOf(key),
        tabs: [],
        collapsed: shut.has(key),
      };
      byKey.set(key, g);
    }
    g.tabs.push(t);
    if (activeId && t.id === activeId) g.collapsed = false;
  }
  return [...byKey.values()];
}

/**
 * The tabs actually on screen, in strip order.
 *
 * Keyboard switching has to walk exactly this list: `Ctrl+Tab` landing on a tab
 * inside a collapsed group would move the terminal to a session the strip is
 * not showing.
 */
export function visibleTabs(groups: TabGroup[]): SessionTab[] {
  return groups.flatMap((g) => (g.collapsed ? [] : g.tabs));
}
