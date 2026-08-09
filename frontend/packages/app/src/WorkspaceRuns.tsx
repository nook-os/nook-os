// One repo's managed runs — review or build — read the way a spec run is read
// (MAIN-455 AC-5, generalized by MAIN-461 AC-2 instead of forked).
//
// A run keeps a transcript — the same `loop_job_transcript` a spec keeps,
// rendered through the same `ChatView`. There is deliberately no second
// transcript mechanism, and no second copy of this panel: Reviews and Builds
// are the same surface with different words and a different row query.
import React, { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, type LoopJobTranscriptEntry } from "@nookos/api";
import { ChatView, Empty, Panel, Pill } from "@nookos/ui";
import { transcriptMessages } from "./LoopPanel";
import { foldToolActivity, jobStateMeta } from "./loop";
import { fileSlug, TranscriptActions } from "./transcriptExport";

/** What every run row can say about itself, whatever kind produced it. */
export type RunRow = {
  id: string;
  state: string;
  /** The item's name in the list: "PR #12", "MAIN-42". */
  label: string;
  /** The right-hand annotation: a short head sha, an outcome. */
  meta: string;
};

/** The loop's state tones, in the design system's words. */
export function pillTone(
  tone: "info" | "warn" | "err" | "ok" | "muted",
): "ok" | "warn" | "err" | "info" | "dim" {
  return tone === "muted" ? "dim" : tone;
}

// Both queries below repaint from the live `job_changed` event, which
// `live.ts` turns into an invalidation of `["job"]` plus this panel's row key
// — the same mechanism the Loop panel rides, rather than a poll of its own.
export function WorkspaceRuns({
  title,
  queryKey,
  fetchRows,
  empty,
  testid,
  transcriptTestid,
  filePrefix,
}: {
  title: string;
  queryKey: readonly unknown[];
  fetchRows: () => Promise<RunRow[]>;
  empty: string;
  testid: string;
  transcriptTestid: string;
  /** The workspace's name, for the export filename (MAIN-471 AC-2). */
  filePrefix?: string;
}) {
  const [openId, setOpenId] = useState<string | null>(null);

  const { data: runs } = useQuery({ queryKey, queryFn: fetchRows });

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
      <Panel title={title}>
        <Empty>{empty}</Empty>
      </Panel>
    );
  }

  return (
    <Panel title={title}>
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
                  data-testid={testid}
                >
                  <span className="mono">{r.label}</span>
                  <Pill tone={tone}>{r.state}</Pill>
                  <span className="faint small mono">{r.meta}</span>
                </button>
              </li>
            );
          })}
        </ul>
        <div className="reviews-transcript" data-testid={transcriptTestid}>
          {detail?.transcript?.length ? (
            <>
              <div className="reviews-transcript-actions">
                <TranscriptActions
                  // The FULL transcript, not the folded view (AC-3): the fold
                  // is how the panel reads, never what an incident paste
                  // carries.
                  lines={detail.transcript}
                  filename={`${fileSlug(
                    [filePrefix, runs.find((r) => r.id === open)?.label ?? "run"]
                      .filter(Boolean)
                      .join("-"),
                  ).toLowerCase()}-${(open ?? "").slice(0, 8)}.md`}
                />
              </div>
              <ChatView
                // Folded like the Loop page folds it, so a ladder of `· Bash`
                // lines reads as one activity entry there and here alike.
                messages={transcriptMessages(foldToolActivity(detail.transcript))}
                // Read-only on purpose: a managed run is the control plane's
                // work, not a conversation somebody steers. The composer is
                // HIDDEN, not disabled — there is nothing here to say anything
                // TO, and an inert box under every finished run was clutter that
                // read as broken.
                onSend={() => {}}
                hideComposer
              />
            </>
          ) : (
            <Empty>This run has not said anything yet.</Empty>
          )}
        </div>
      </div>
    </Panel>
  );
}
