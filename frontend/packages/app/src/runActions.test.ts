// What a run offers, and why (MAIN-559 AC-9).
//
// The action set is the part of this card that can be wrong without anything
// looking wrong: a Cancel on a finished run reads exactly like a Cancel on a
// live one until it is pressed. So it is pinned here, as a table over states and
// kinds, rather than only through a mounted menu — the menu is one renderer of
// this list and there are three.
import { describe, expect, it } from "vitest";

import {
  CANCEL_ENDED_REFUSAL,
  CANCEL_PENDING_REFUSAL,
  cancelPrompt,
  cancelRefusal,
  canCancelRun,
  canRerunRun,
  isTerminalRun,
  overflowRunActions,
  primaryRunAction,
  RERUN_REFUSAL,
  rerunRefusal,
  runActions,
  type RunActionTarget,
} from "./runActions";

const buildRun = (state: string): RunActionTarget => ({
  id: "job-b1",
  kind: "build",
  state,
  label: "MAIN-42",
});

const reviewRun = (state: string): RunActionTarget => ({
  id: "job-r1",
  kind: "review",
  state,
  label: "PR #341",
});

const ids = (t: RunActionTarget, ctx = {}) => runActions(t, ctx).map((a) => a.id);

const ACTIVE = ["queued", "claimed", "running", "waiting_on_human"];
const TERMINAL = ["completed", "failed", "canceled"];

describe("which states are which", () => {
  it("calls terminal exactly what the control plane calls terminal", () => {
    // `jobs::is_terminal`. A fourth state appearing here without appearing
    // there is what would let Cancel be offered on a run nothing can cancel.
    for (const s of TERMINAL) expect(isTerminalRun(s)).toBe(true);
    for (const s of ACTIVE) expect(isTerminalRun(s)).toBe(false);
  });

  it("permits cancel out of every live state and no dead one", () => {
    for (const s of ACTIVE) expect(canCancelRun(s)).toBe(true);
    for (const s of TERMINAL) expect(canCancelRun(s)).toBe(false);
  });

  it("permits re-run only where `/rerun` does — which is narrower than terminal", () => {
    // The server takes a failed or canceled run and refuses a completed one, so
    // `completed` is terminal AND not re-runnable. Reading "terminal" as
    // "re-runnable" is the mistake this exists to catch.
    expect(canRerunRun("failed")).toBe(true);
    expect(canRerunRun("canceled")).toBe(true);
    expect(canRerunRun("completed")).toBe(false);
    for (const s of ACTIVE) expect(canRerunRun(s)).toBe(false);
  });
});

describe("the action set per state and kind (AC-2)", () => {
  it("offers cancel, and never re-run, while a run is live", () => {
    for (const s of ACTIVE) {
      expect(ids(buildRun(s))).toEqual(["open", "cancel", "copy-id", "copy-link"]);
      expect(ids(reviewRun(s))).toEqual(["open", "cancel", "copy-id", "copy-link"]);
    }
  });

  it("offers re-run, and never cancel, once a run has ended", () => {
    for (const s of TERMINAL) {
      expect(ids(buildRun(s))).toEqual(["open", "rerun", "copy-id", "copy-link"]);
      expect(ids(buildRun(s))).not.toContain("cancel");
    }
  });

  it("adds the related card or PR only when a terminal run has one", () => {
    expect(ids(buildRun("failed"), { taskHref: "/loop/t-1" })).toContain("view-task");
    expect(ids(reviewRun("failed"), { prHref: "https://x/pull/1" })).toContain("view-pr");
    // No join, no action — a link to nowhere is worse than no link.
    expect(ids(buildRun("failed"), { taskHref: null })).not.toContain("view-task");
    // And never while it is still going: the transcript beside it is the thing
    // to be watching.
    expect(ids(buildRun("running"), { taskHref: "/loop/t-1" })).not.toContain("view-task");
    // A build's join is its card, a review's is its PR; neither takes the
    // other's.
    expect(ids(buildRun("failed"), { prHref: "https://x/pull/1" })).not.toContain("view-pr");
    expect(ids(reviewRun("failed"), { taskHref: "/loop/t-1" })).not.toContain("view-task");
  });

  it("names the related thing after the run, not after its type", () => {
    const view = runActions(buildRun("failed"), { taskHref: "/loop/t-1" }).find(
      (a) => a.id === "view-task",
    );
    expect(view?.label).toBe("View MAIN-42");
  });

  it("marks cancel destructive so it is not one click among four", () => {
    expect(runActions(buildRun("running")).find((a) => a.id === "cancel")?.danger).toBe(true);
    expect(runActions(buildRun("failed")).find((a) => a.id === "rerun")?.danger).toBeFalsy();
  });
});

describe("refusals are shown, not hidden (AC-6)", () => {
  it("offers re-run on a completed run carrying the reason it would be refused", () => {
    const rerun = runActions(buildRun("completed")).find((a) => a.id === "rerun");
    // Present — absence would be indistinguishable from an oversight.
    expect(rerun).toBeTruthy();
    // The SENTENCE, not the constant: `refusal === RERUN_REFUSAL` agrees with
    // itself whatever either says, so it could not tell a reader the wording
    // had moved. The server's own noun is `job`; this surface says `run`
    // (MAIN-488), and that difference is a decision, not drift.
    expect(rerun?.refusal).toBe("only a failed or canceled run can be re-run");
    expect(RERUN_REFUSAL).toBe("only a failed or canceled run can be re-run");
    // While a run that CAN be re-run carries none, so the menu row is live.
    expect(runActions(buildRun("failed")).find((a) => a.id === "rerun")?.refusal).toBeUndefined();
  });

  it("refuses a second cancel while the first is still in flight (AC-4)", () => {
    const cancel = runActions(buildRun("running"), { pending: true }).find(
      (a) => a.id === "cancel",
    );
    expect(cancel?.refusal).toBe(CANCEL_PENDING_REFUSAL);
  });
});

describe("firing an action that has gone stale (AC-5)", () => {
  it("refuses a cancel for a run that has since finished", () => {
    expect(cancelRefusal("running", false)).toBeNull();
    for (const s of TERMINAL) expect(cancelRefusal(s, false)).toBe(CANCEL_ENDED_REFUSAL);
    expect(cancelRefusal("running", true)).toBe(CANCEL_PENDING_REFUSAL);
  });

  it("refuses a re-run for a run that is no longer re-runnable", () => {
    expect(rerunRefusal("failed", false)).toBeNull();
    expect(rerunRefusal("completed", false)).toBe(RERUN_REFUSAL);
    expect(rerunRefusal("running", false)).toBe(RERUN_REFUSAL);
  });
});

describe("the header's one button and its overflow (AC-7)", () => {
  it("promotes the action the state permits and leaves the rest behind it", () => {
    const live = runActions(buildRun("running"));
    expect(primaryRunAction(live)?.id).toBe("cancel");
    expect(overflowRunActions(live).map((a) => a.id)).toEqual(["open", "copy-id", "copy-link"]);

    const done = runActions(buildRun("failed"));
    expect(primaryRunAction(done)?.id).toBe("rerun");
    expect(overflowRunActions(done)).not.toContain(primaryRunAction(done));
  });
});

describe("the cancel confirmation (AC-3)", () => {
  it("names the run and says plainly that the agent is stopped", () => {
    const { title, description } = cancelPrompt(buildRun("running"));
    expect(title).toContain("MAIN-42");
    expect(title).toContain("build");
    expect(description).toMatch(/agent .*will be stopped/);
    expect(cancelPrompt(reviewRun("running")).title).toContain("PR #341");
  });
});
