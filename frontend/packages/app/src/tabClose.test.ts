// What closing a tab means, per session type (MAIN-324).
//
// These are the assertions that matter more than the pixels: the two branches
// are a destructive kill and an edit to shared workspace state, and picking the
// wrong one is either somebody's terminal gone without warning or a "close"
// that visibly undoes itself on the next reconcile pass.
import { describe, expect, it } from "vitest";
import { closePlan, scaledDown } from "./tabClose";
import type { SessionTab } from "./sessionTabsStore";

const tab = (over: Partial<SessionTab> = {}): SessionTab => ({
  id: "s1",
  name: "worktree",
  runtime: "bash",
  ...over,
});

describe("closePlan — ad-hoc", () => {
  it("kills, and says nothing will restart it (AC-1)", () => {
    const p = closePlan(tab());
    expect(p.kind).toBe("kill");
    expect(p.description).toMatch(/nothing restarts it/i);
    if (p.kind === "kill") expect(p.confirmLabel).toMatch(/end/i);
  });

  it("treats an ABSENT managed flag as ad-hoc", () => {
    // The safe default, and deliberate: an unknown session gets a confirm
    // before a kill rather than a silent edit to a workspace declaration that
    // may not even exist. A stale client reading an older API is the realistic
    // way this happens.
    expect(closePlan(tab({ managed: undefined })).kind).toBe("kill");
  });

  it("kills even inside a workspace — membership is not ownership", () => {
    // The whole reason `managed` is a stored column (MAIN-318): a hand-started
    // terminal in a managed workspace looks exactly like a replica, and if this
    // branched on `workspaceId` it would scale the workspace down because
    // somebody closed their own shell.
    const p = closePlan(tab({ workspaceId: "w1", workspaceName: "nook" }));
    expect(p.kind).toBe("kill");
  });
});

describe("closePlan — managed", () => {
  it("offers scale-down, never a kill, and says why (AC-2)", () => {
    const p = closePlan(tab({ managed: true, workspaceId: "w1", workspaceName: "nook" }));
    expect(p.kind).toBe("scale-down");
    if (p.kind !== "scale-down") return;
    expect(p.workspaceId).toBe("w1");
    expect(p.title).toContain("nook");
    // The sentence that stops somebody hunting for a kill button.
    expect(p.description).toMatch(/would start another/i);
    expect(p.confirmLabel).not.toMatch(/kill/i);
  });

  it("explains rather than killing when the workspace is unknown", () => {
    // A managed session with no workspace on the tab is an inconsistency, not a
    // user error. Falling back to a kill would produce the exact behaviour AC-2
    // forbids — a close that respawns.
    const p = closePlan(tab({ managed: true }));
    expect(p.kind).toBe("explain");
    expect(p.description).toMatch(/starts another/i);
  });
});

describe("scaledDown", () => {
  it("takes one off a count", () => {
    expect(scaledDown({ kind: "count", count: 3 }, 3)).toEqual({ kind: "count", count: 2 });
  });

  it("turns `single` into zero — wanting none is expressible on purpose", () => {
    expect(scaledDown({ kind: "single" }, 1)).toEqual({ kind: "count", count: 0 });
  });

  it("pins `all` to one below what is RUNNING", () => {
    // `all` has no number to decrement — it means one per matching node — so
    // the only honest scale-down is a count below the current reality.
    expect(scaledDown({ kind: "all" }, 4)).toEqual({ kind: "count", count: 3 });
  });

  it("refuses to go below zero, and refuses with no declaration", () => {
    expect(scaledDown({ kind: "count", count: 0 }, 0)).toBeNull();
    expect(scaledDown({ kind: "all" }, 0)).toBeNull();
    expect(scaledDown(undefined, 2)).toBeNull();
  });
});
