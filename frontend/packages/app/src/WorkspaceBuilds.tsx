// A repo's build runs (MAIN-461 AC-2): card key + outcome + state over the
// same runs panel Reviews uses — generalized, not forked.
import React from "react";
import { api } from "@nookos/api";
import { WorkspaceRuns, type RunRow } from "./WorkspaceRuns";

type Run = {
  id: string;
  state: string;
  task_key?: string | null;
  created_at: string;
  // `loop_jobs.build_outcome`, the exact column MAIN-458 (PR #361) commits —
  // the name is pinned there, not guessed here. Optional so the panel lights
  // up the moment the listing starts sending it; until then rows show
  // key + state.
  build_outcome?: string | null;
};

export function WorkspaceBuilds({ workspaceId }: { workspaceId: string }) {
  return (
    <WorkspaceRuns
      title="Builds"
      queryKey={["workspace-builds", workspaceId]}
      empty={
        "No build has run for this repo yet. Enqueue one from a card, or set " +
        "a ceiling with `nook builds scale`."
      }
      testid="build-run"
      transcriptTestid="build-transcript"
      fetchRows={async () => {
        const rows =
          ((
            await api.GET("/api/v1/workspaces/{id}/builds", {
              params: { path: { id: workspaceId } },
            })
          ).data as Run[] | undefined) ?? [];
        return rows.map(
          (r): RunRow => ({
            id: r.id,
            state: r.state,
            label: r.task_key ?? "build",
            meta: r.build_outcome ?? "",
          }),
        );
      }}
    />
  );
}
