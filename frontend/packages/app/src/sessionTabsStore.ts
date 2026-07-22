// Named `sessionTabsStore`, not `sessiontabs`, because `SessionTabs.tsx` sits
// beside it. Two files whose names differ only in case are distinct on Linux
// and the SAME FILE on macOS and Windows, where the import resolved to the
// wrong one and the frontend would not build at all. CI on Linux was happy;
// every Mac was not.
// VS-Code-style session tabs: every session you visit opens a tab; tabs
// persist across reloads (localStorage) and closing a tab only stops viewing —
// the tmux session keeps running (like closing a file tab in VS Code).
import { create } from "zustand";

export interface SessionTab {
  id: string;
  name: string;
  runtime: string;
  /** Owning workspace — tabs are filtered by the active workspace context.
   *  Optional only for tabs persisted before this field existed; they show in
   *  every context until revisited (which backfills it). */
  workspaceId?: string;
  workspaceName?: string;
  /** Pinned tabs sort first and survive "close others" / "close all". */
  pinned?: boolean;
}

const KEY = "nook.session-tabs";

function load(): SessionTab[] {
  try {
    const raw = localStorage.getItem(KEY);
    if (raw) return JSON.parse(raw) as SessionTab[];
  } catch {
    // corrupted store — start fresh
  }
  return [];
}

function save(tabs: SessionTab[]) {
  try {
    localStorage.setItem(KEY, JSON.stringify(tabs));
  } catch {
    // storage full/unavailable — tabs just won't persist
  }
}

interface SessionTabsState {
  tabs: SessionTab[];
  /** Add (or refresh) a tab; visiting a session calls this. */
  open(tab: SessionTab): void;
  close(id: string): void;
  /** Close every tab except `id` (pinned tabs stay). */
  closeOthers(id: string): void;
  /** Close tabs after `id` in the given visible order (pinned tabs stay). */
  closeToTheRight(id: string, visible: string[]): void;
  /** Close all tabs in `ids` that aren't pinned. */
  closeAll(ids: string[]): void;
  togglePin(id: string): void;
  rename(id: string, name: string): void;
}

export const useSessionTabs = create<SessionTabsState>((set) => ({
  tabs: load(),
  open: (tab) =>
    set((s) => {
      const exists = s.tabs.some((t) => t.id === tab.id);
      const tabs = exists
        ? s.tabs.map((t) => (t.id === tab.id ? { ...t, ...tab } : t))
        : [...s.tabs, tab];
      save(tabs);
      return { tabs };
    }),
  close: (id) =>
    set((s) => {
      const tabs = s.tabs.filter((t) => t.id !== id);
      save(tabs);
      return { tabs };
    }),
  closeOthers: (id) =>
    set((s) => {
      const tabs = s.tabs.filter((t) => t.id === id || t.pinned);
      save(tabs);
      return { tabs };
    }),
  closeToTheRight: (id, visible) =>
    set((s) => {
      const cut = visible.indexOf(id);
      if (cut < 0) return s;
      const doomed = new Set(visible.slice(cut + 1));
      const tabs = s.tabs.filter((t) => !doomed.has(t.id) || t.pinned);
      save(tabs);
      return { tabs };
    }),
  closeAll: (ids) =>
    set((s) => {
      const doomed = new Set(ids);
      const tabs = s.tabs.filter((t) => !doomed.has(t.id) || t.pinned);
      save(tabs);
      return { tabs };
    }),
  togglePin: (id) =>
    set((s) => {
      const tabs = s.tabs.map((t) =>
        t.id === id ? { ...t, pinned: !t.pinned } : t,
      );
      save(tabs);
      return { tabs };
    }),
  rename: (id, name) =>
    set((s) => {
      const tabs = s.tabs.map((t) => (t.id === id ? { ...t, name } : t));
      save(tabs);
      return { tabs };
    }),
}));
