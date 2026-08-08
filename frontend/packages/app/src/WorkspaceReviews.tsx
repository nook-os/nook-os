// A repo's review runs, read the way a spec run is read (MAIN-455 AC-5).
//
// A review used to be a tmux session on a machine: attachable, and gone the
// moment it died, so "what did the reviewer actually do" had no answer unless
// you happened to be watching. It is a headless run now, and a run keeps a
// transcript — the same `loop_job_transcript` a spec keeps, rendered through
// the same `ChatView`. There is deliberately no second transcript mechanism.
import React, { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, type LoopJobTranscriptEntry } from "@nookos/api";
import { ChatView, Empty, Panel, Pill } from "@nookos/ui";
import { transcriptMessages } from "./LoopPanel";
import { jobStateMeta } from "./loop";

type Run = {
  id: string;
  state: string;
  review_pr_number?: number | null;
  review_head_sha?: string | null;
  created_at: string;
};

/** What a run is ABOUT, in the words the panel can show without a lookup. */
/** The loop's state tones, in the design system's words. */
export function pillTone(
  tone: "info" | "warn" | "err" | "ok" | "muted",
): "ok" | "warn" | "err" | "info" | "dim" {
  return tone === "muted" ? "dim" : tone;
}

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

// Both queries below repaint from the live `job_changed` event, which
// `live.ts` turns into an invalidation of `["job"]` and `["workspace-reviews"]`
// — the same mechanism the Loop panel rides, rather than a poll of its own.
export function WorkspaceReviews({ workspaceId }: { workspaceId: string }) {
  const [openId, setOpenId] = useState<string | null>(null);

  const { data: runs } = useQuery({
    queryKey: ["workspace-reviews", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/reviews", {
          params: { path: { id: workspaceId } },
        })
      ).data as Run[] | undefined) ?? [],
  });

  const open = openId ?? runs?.[0]?.id ?? null;
  const { data: detail } = useQuery({
    queryKey: ["job", open],
    enabled: !!open,
    queryFn: async () =>
      (
        await api.GET("/api/v1/jobs/{id}", { params: { path: { id: open as string } } })
      ).data as { transcript?: LoopJobTranscriptEntry[] } | undefined,
  });

  if (!runs) return null;
  if (runs.length === 0) {
    return (
      <Panel title="Reviews">
        <Empty>
          No review has run for this repo yet. The control plane raises one per
          open pull request, and again when a pull request is pushed to.
        </Empty>
      </Panel>
    );
  }

  return (
    <Panel title="Reviews">
      <div className="reviews-split">
        <ul className="reviews-runs">
          {runs.map((r) => {
            // `jobStateMeta` speaks the loop's tone vocabulary, which has a
            // `muted` the Pill spells `dim`. Mapped here rather than widened in
            // the shared component, whose set is the design system's.
            const tone = pillTone(jobStateMeta(r.state).tone);
            return (
              <li key={r.id}>
                <button
                  className={`reviews-run${r.id === open ? " is-open" : ""}`}
                  onClick={() => setOpenId(r.id)}
                  data-testid="review-run"
                >
                  <span className="mono">{runLabel(r)}</span>
                  <Pill tone={tone}>{r.state}</Pill>
                  <span className="faint small mono">{shortHead(r.review_head_sha)}</span>
                </button>
              </li>
            );
          })}
        </ul>
        <div className="reviews-transcript" data-testid="review-transcript">
          {detail?.transcript?.length ? (
            <ChatView
              messages={transcriptMessages(detail.transcript)}
              // Read-only on purpose: a review run is the control plane's work,
              // not a conversation somebody steers. A spec run's composer is
              // how a human shapes the draft; there is no equivalent here, so
              // the box is disabled rather than wired to a no-op that looks
              // like it might do something.
              onSend={() => {}}
              disabled
              placeholder="A review run is not steered from here."
            />
          ) : (
            <Empty>This run has not said anything yet.</Empty>
          )}
        </div>
      </div>
    </Panel>
  );
}
