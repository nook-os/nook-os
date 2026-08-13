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
// `plan_now`, so it can state a desired number. Builds have no planner: what is
// shown here is COUNTED from the runs list, and it says so rather than implying
// one.
//
// What builds DO have now is `/build-loop-status` (MAIN-495), and it answers a
// different question — not how many runs are wanted, but whether the number
// somebody typed can be honoured by the machines they own. A ceiling of three
// against one node's two slots changes nothing observable, so the third run
// queues forever and the loop looks broken. That is what the note below says
// out loud. It is ADVISORY: the write still saves any valid number, because
// fleet capacity changes without warning and a refusal correct at write time is
// wrong an hour later.
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

/** One node the status says delivers nothing, and the ground it failed on. */
type BuildBlocker = {
  node_id: string;
  node_name: string;
  reason: { kind: string; label?: string; runtime?: string; job_kind?: string };
};

/** `/build-loop-status`, as this panel reads it. */
export type BuildStatus = {
  desired: number;
  running: number;
  shortfall: number;
  capacity: number;
  eligible_nodes: number;
  blocked: BuildBlocker[];
};

/** The note beside the ceiling, or `null` when the declaration and the fleet
 *  agree and there is nothing to say.
 *
 *  Zero eligible nodes is its OWN sentence rather than "can deliver 0": a zero
 *  reads as a limit somebody set, and sends a person looking for the number to
 *  raise instead of for the machine to label. A ceiling of 0 says nothing at
 *  all — builds are off for this repo, so what the fleet could have delivered
 *  is not a question anyone asked. */
export function buildCapacityNote(status: BuildStatus | null | undefined): string | null {
  if (!status || status.desired === 0) return null;
  if (status.eligible_nodes === 0) return "no node of yours accepts build work";
  if (status.desired > status.capacity) {
    return `${status.desired} requested \u00b7 your nodes can deliver ${status.capacity}`;
  }
  return null;
}

/** A blocker in the words of the thing a person would go and change. */
export function blockerWords(reason: BuildBlocker["reason"]): string {
  switch (reason.kind) {
    case "shared_operator":
      return "the shared operator never builds";
    case "not_yours":
      return "not your machine";
    case "offline":
      return "offline";
    case "runtime_not_authorized":
      return `${reason.runtime} is not signed in`;
    case "kind_not_accepted":
      return `does not take ${reason.job_kind} work`;
    case "no_role_label":
      return `no ${reason.label} label`;
    default:
      // A ground this build predates. Naming the node is still worth more than
      // hiding it, and the server's vocabulary can grow without this lying.
      return "not eligible";
  }
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
      ).data?.rows as { state: string }[] | undefined) ?? [],
    refetchInterval: 10000,
  });

  const { data: status } = useQuery({
    queryKey: ["build-loop-status", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/build-loop-status", {
          params: { path: { id: workspaceId } },
        })
      ).data as BuildStatus | undefined) ?? null,
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
  const note = buildCapacityNote(status);

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
    queryClient.invalidateQueries({ queryKey: ["build-loop-status", workspaceId] });
  };

  return (
    <div className="loop-scale" data-testid="build-loop">
      <div className="loop-scale-head">
        <b>Build loop</b>
        {/* At the ceiling is normal and healthy, not a warning — unlike the
            review loop's shortfall, which means capacity it wanted and lacks.
            Counted from the runs list, so the tooltip says so rather than
            letting the number read as a planner's desired figure.
            Deliberately NOT the status's own `running`, which counts every run
            holding a node slot — a claimed one included — because that is the
            number `capacity` is about and this one is not. */}
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

      {/* Beside the ceiling, never in place of it (AC-5). The value is saved
          and the panel still reports it; this only says what the fleet will do
          with it. */}
      {note && (
        <div className="small" data-testid="build-loop-capacity" style={{ color: "var(--nook-warn)" }}>
          {note}
          {/* One line per node rather than one joined line: a person with
              several machines got a single unwrapped run-on, and there is no
              cap here to truncate it — naming every one is the point. */}
          {status && status.blocked.length > 0 && (
            <div className="faint" data-testid="build-loop-blocked">
              {status.blocked.map((b) => (
                <div key={b.node_id}>
                  {b.node_name || b.node_id}: {blockerWords(b.reason)}
                </div>
              ))}
            </div>
          )}
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
