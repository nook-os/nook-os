// The workspace context: the thing Muddy-OS made hard. Selecting a
// workspace is sticky (survives reloads) and scopes Sessions, Board, and
// Activity until cleared.
import { create } from "zustand";
import { persist } from "zustand/middleware";

interface WorkspaceContextState {
  selectedWorkspaceId: string | null;
  select(id: string | null): void;
}

export const useWorkspaceContext = create<WorkspaceContextState>()(
  persist(
    (set) => ({
      selectedWorkspaceId: null,
      select: (id) => set({ selectedWorkspaceId: id }),
    }),
    { name: "nookos-workspace-context" },
  ),
);
