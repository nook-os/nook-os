// A repo's review runs (MAIN-455 AC-5) over the shared runs panel.
import React from "react";
import { api } from "@nookos/api";
import { WorkspaceRuns, type RunRow } from "./WorkspaceRuns";
export { pillTone } from "./WorkspaceRuns";

type Run = {
  id: string;
  state: string;
  review_pr_number?: number | null;
  review_head_sha?: string | null;
  created_at: string;
};

/** What a run is ABOUT, in the words the panel can show without a lookup. */
export function runLabel(run: Run): string {
  return run.review_pr_number ? `PR #${run.review_pr_number}` : "review";
}

/** The head a run was raised for, short enough to sit in a row.
 *
 *  Shown because it is the whole wakeup rule made visible: two runs of the same
 *  PR differ by this and nothing else, and without it a list of five runs for
 *  one PR looks like the loop spinning rather than five pushes. */
export function shortHead(sha?: string | null): string {
  return sha ? sha.slice(0, 7) : "";
}

export function WorkspaceReviews({ workspaceId }: { workspaceId: string }) {
  return (
    <WorkspaceRuns
      title="Reviews"
      queryKey={["workspace-reviews", workspaceId]}
      empty={
        "No review has run for this repo yet. The control plane raises one per " +
        "open pull request, and again when a pull request is pushed to."
      }
      testid="review-run"
      transcriptTestid="review-transcript"
      fetchRows={async () => {
        const rows =
          ((
            await api.GET("/api/v1/workspaces/{id}/reviews", {
              params: { path: { id: workspaceId } },
            })
          ).data as Run[] | undefined) ?? [];
        return rows.map(
          (r): RunRow => ({
            id: r.id,
            state: r.state,
            label: runLabel(r),
            meta: shortHead(r.review_head_sha),
          }),
        );
      }}
    />
  );
}
