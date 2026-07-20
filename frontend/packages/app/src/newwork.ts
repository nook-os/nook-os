// Single source of "start new work". Any page opens the one modal through
// this store — no duplicate forms, no source tabs. The intent (clone / new /
// existing) is inferred from a single input.
import { create } from "zustand";

export interface NewWorkSeed {
  taskId?: string;
  /** Preselect an existing workspace (fills the input with its name). */
  workspaceId?: string;
  nodeId?: string;
  /** Pre-tick "new worktree branch". */
  worktree?: boolean;
}

interface NewWorkState {
  open: boolean;
  seed: NewWorkSeed;
  show(seed?: NewWorkSeed): void;
  hide(): void;
}

export const useNewWork = create<NewWorkState>((set) => ({
  open: false,
  seed: {},
  show: (seed = {}) => set({ open: true, seed }),
  hide: () => set({ open: false, seed: {} }),
}));
