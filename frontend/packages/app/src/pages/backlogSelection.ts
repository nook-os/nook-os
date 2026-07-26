// Bulk-selection state for the Backlog tab (MAIN-123). A checkbox on every
// selectable row feeds a shared set of task ids; a toolbar shows the count and
// later tickets hang bulk actions off it. The store holds ONLY the selection;
// the collapse state stays in BoardBacklog (localStorage-backed) but its toggle
// rule lives here as a pure helper so collapse is one definition and the AC-4
// "no desync" guarantee is unit-testable.
import { create } from "zustand";
import type { BacklogGroups } from "./Board";

/// The ids eligible for selection given the current groups and collapse state
/// (MAIN-123 AC-2/AC-5): every epic child whose parent epic is NOT collapsed,
/// PLUS every "No epic" row. Epics themselves are containers and never
/// selectable, so they are excluded. A collapsed epic's children are hidden, so
/// they are not selectable either — select-all must not reach rows the user
/// can't see. Pure so select-all and the "all selected?" check share one rule.
export function selectableRowIds(
  groups: BacklogGroups,
  collapsed: Set<string>,
): string[] {
  const ids: string[] = [];
  for (const section of groups.epics) {
    if (collapsed.has(section.epic.id)) continue; // hidden children aren't selectable
    for (const child of section.children) ids.push(child.id);
  }
  for (const t of groups.noEpic) ids.push(t.id);
  return ids;
}

/// Toggle `id` in a collapse set, returning a NEW set (never mutating the
/// input). This is the single source of truth for the collapse toggle: the
/// chevron icon/title and the body render both read the resulting set, so they
/// can never disagree (MAIN-123 AC-4).
export function nextCollapsed(collapsed: Set<string>, id: string): Set<string> {
  const next = new Set(collapsed);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
}

interface BacklogSelectionState {
  /** The selected task ids. Every mutation replaces this with a NEW Set so
   *  zustand subscribers see a changed reference and re-render. */
  selected: Set<string>;
  /** Add/remove a single id (the per-row checkbox). */
  toggle(id: string): void;
  /** Replace the whole set (select-all / select-none). */
  setSelection(ids: string[]): void;
  /** Empty the selection (the toolbar's "clear", filter change, tab leave). */
  clear(): void;
}

export const useBacklogSelection = create<BacklogSelectionState>((set) => ({
  selected: new Set<string>(),
  toggle: (id) =>
    set((s) => {
      const selected = new Set(s.selected);
      if (selected.has(id)) selected.delete(id);
      else selected.add(id);
      return { selected };
    }),
  setSelection: (ids) => set({ selected: new Set(ids) }),
  clear: () =>
    set((s) => (s.selected.size === 0 ? s : { selected: new Set<string>() })),
}));
