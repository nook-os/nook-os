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
// What builds DO have now is `/build-loop/status` (MAIN-495), and it answers a
// different question — not how many runs are wanted, but whether the number
// somebody typed can be honoured by the machines they own. A ceiling of three
// against one node's two slots changes nothing observable, so the third run
// queues forever and the loop looks broken. That is what the note below says
// out loud. It is ADVISORY: the write still saves any valid number, because
// fleet capacity changes without warning and a refusal correct at write time is
// wrong an hour later.
//
// Every read and write goes through `buildRuns`' hooks (MAIN-641 AC-7): the
// declaration is one route now, and one module owning its key is what stops two
// panels disagreeing about what they just wrote.
import React, { useState } from "react";
import { Pill } from "@nookos/ui";
import {
  useBuildLoop,
  useBuildLoopStatus,
  useSetBuildLoop,
  useWorkspaceBuilds,
  type BuildBlocker,
  type BuildLoopStatus,
} from "./buildRuns";

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

/** The note beside the ceiling, or `null` when the declaration and the fleet
 *  agree and there is nothing to say.
 *
 *  Zero eligible nodes is its OWN sentence rather than "can deliver 0": a zero
 *  reads as a limit somebody set, and sends a person looking for the number to
 *  raise instead of for the machine to label. A ceiling of 0 says nothing at
 *  all — builds are off for this repo, so what the fleet could have delivered
 *  is not a question anyone asked. */
export function buildCapacityNote(status: BuildLoopStatus | null | undefined): string | null {
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

export function BuildLoop({ workspaceId }: { workspaceId: string }) {
  const [draft, setDraft] = useState<string | null>(null);

  const { data: decl } = useBuildLoop(workspaceId);
  // Counted, not planned — see the header. The Builds panel's own hook, so the
  // two never disagree about how many runs are live.
  const { data: runs } = useWorkspaceBuilds(workspaceId);
  const { data: status } = useBuildLoopStatus(workspaceId);
  const { save: patch, busy, refusal } = useSetBuildLoop(workspaceId);

  // `undefined` is "not fetched yet" and `null` is "no declaration". Collapsing
  // them would render the loading state as "unset (default 1)" — a claim about
  // the repo made before anything about the repo is known.
  const loaded = decl !== undefined;
  const max = decl?.concurrency ?? null;
  const summary = buildLoopSummary(max);
  const editing = draft !== null;
  const running = (runs ?? []).filter((r) => r.state === "running").length;
  const note = buildCapacityNote(status);

  // Only the ceiling: a PATCH naming one field leaves the switch and the pin
  // exactly as they were, which is why the two controls can share one route.
  const save = async (next: number | null) => {
    if (await patch({ concurrency: next })) setDraft(null);
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
