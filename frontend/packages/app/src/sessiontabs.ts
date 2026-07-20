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
}));
