// The per-repo BUILD ceiling, editable.
//
// MAIN-461 shipped the column, both halves of `/build-loop`, and the read-only
// Builds panel — but nothing ever called the write half, so the only way to
// change the number was `nook builds scale`. This is that missing half.
//
// Deliberately shaped like `ReviewLoop`, its twin: same raw null-is-unset
// contract, same clear-back-to-unset control. It is a SEPARATE component rather
// than one generic control because the two differ exactly where it matters —
// reviews have `/review-loop-status`, which plans through the reconciler's own
// `plan_now`, so it can state a desired number. Builds have no such endpoint.
// What is shown here is COUNTED from the runs list, and it says so rather than
// implying a planner it does not have.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { Pill } from "@nookos/ui";

/** The ceiling in words. Split out for the same reason `reviewLoopSummary` is:
 *  `null` and `0` are different states that a number alone cannot show, and the
 *  distinction is worth testing without a DOM. */
export function buildLoopSummary(max: number | null): { state: string; detail: string } {
  if (max === null) {
    return { state: "unset (default 1)", detail: "one build run at a time" };
  }
  if (max === 0) {
    return { state: "0 (off)", detail: "no build run is raised for this repo" };
  }
  return {
    state: `max ${max}`,
    detail: max === 1 ? "one build run at a time" : `up to ${max} build runs at once`,
  };
}

/** The server's own words for a refused write — its 400 names the field that
 *  was just typed into, which beats any sentence guessed here. */
function refusalText(error: unknown): string {
  const e = error as { error?: string } | undefined;
  return e?.error ?? JSON.stringify(error);
}

export function BuildLoop({ workspaceId }: { workspaceId: string }) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);

  const { data: decl } = useQuery({
    queryKey: ["build-loop", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/build-loop", {
          params: { path: { id: workspaceId } },
        })
      ).data ?? null,
  });

  // Counted, not planned — see the header. Shares the Builds panel's key so the
  // two never disagree about how many runs are live.
  const { data: runs } = useQuery({
    queryKey: ["workspace-builds", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/builds", {
          params: { path: { id: workspaceId } },
        })
      ).data as { state: string }[] | undefined) ?? [],
    refetchInterval: 10000,
  });

  // `undefined` is "not fetched yet" and `null` is "no declaration". Collapsing
  // them would render the loading state as "unset (default 1)" — a claim about
  // the repo made before anything about the repo is known.
  const loaded = decl !== undefined;
  const max = decl ? ((decl as { max_replicas?: number | null }).max_replicas ?? null) : null;
  const summary = buildLoopSummary(max);
  const editing = draft !== null;
  const running = (runs ?? []).filter((r) => r.state === "running").length;

  const save = async (next: number | null) => {
    setBusy(true);
    setRefusal(null);
    const { error } = await api.PUT("/api/v1/workspaces/{id}/build-loop", {
      params: { path: { id: workspaceId } },
      body: { max_replicas: next },
    });
    setBusy(false);
    if (error) {
      setRefusal(refusalText(error));
      return;
    }
    setDraft(null);
    queryClient.invalidateQueries({ queryKey: ["build-loop", workspaceId] });
  };

  return (
    <div className="loop-scale" data-testid="build-loop">
      <div className="loop-scale-head">
        <b>Build loop</b>
        {/* At the ceiling is normal and healthy, not a warning — unlike the
            review loop's shortfall, which means capacity it wanted and lacks.
            Counted from the runs list, so the tooltip says so rather than
            letting the number read as a planner's desired figure. */}
        <span data-testid="build-loop-running">
          <Pill
            tone={max === 0 ? "warn" : "ok"}
            title="counted from this repo's run list, not planned"
          >
            {running} running
          </Pill>
        </span>
      </div>

      {!loaded ? (
        <div className="loop-scale-state faint">reading the declaration…</div>
      ) : !editing ? (
        <div className="loop-scale-state">
          <span className="mono" data-testid="build-loop-state">
            {summary.state}
          </span>
          <span className="faint"> · {summary.detail}</span>
        </div>
      ) : (
        <div className="loop-scale-edit">
          <input
            className="input small"
            type="number"
            min={0}
            aria-label="build loop maximum"
            value={draft}
            disabled={busy}
            onChange={(e) => setDraft(e.target.value)}
          />
          <button
            className="btn primary small"
            disabled={busy}
            onClick={() => save(draft.trim() === "" ? null : Number(draft))}
          >
            save
          </button>
          <button className="btn small" disabled={busy} onClick={() => setDraft(null)}>
            cancel
          </button>
        </div>
      )}

      {refusal && (
        <div className="small" data-testid="build-loop-refusal" style={{ color: "var(--nook-err)" }}>
          {refusal}
        </div>
      )}

      {!editing && (
        <div className="loop-scale-actions">
          <button
            className="btn small"
            disabled={busy}
            onClick={() => setDraft(max === null ? "" : String(max))}
          >
            set a maximum
          </button>
          {/* Clearing is not the same as typing 1: this is the only control that
              can put the column back to the state a fresh workspace is in. */}
          {max !== null && (
            <button
              className="btn small"
              disabled={busy}
              title="back to unset — the default ceiling"
              onClick={() => save(null)}
            >
              clear
            </button>
          )}
        </div>
      )}
    </div>
  );
}
