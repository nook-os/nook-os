import { describe, expect, it } from "vitest";
import { reorderTabs, type SessionTab } from "./sessionTabsStore";

const tab = (id: string, pinned = false): SessionTab => ({
  id,
  name: id,
  runtime: "bash",
  pinned,
});

const ids = (tabs: SessionTab[]) => tabs.map((t) => t.id);

describe("reorderTabs", () => {
  it("moves a tab before a target within the same group", () => {
    const tabs = [tab("a"), tab("b"), tab("c")];
    // Drag c to before a → c, a, b
    expect(ids(reorderTabs(tabs, "c", "a", false))).toEqual(["c", "a", "b"]);
  });

  it("moves a tab after a target within the same group", () => {
    const tabs = [tab("a"), tab("b"), tab("c")];
    // Drag a to after b → b, a, c
    expect(ids(reorderTabs(tabs, "a", "b", true))).toEqual(["b", "a", "c"]);
  });

  it("reorders among pinned tabs without disturbing unpinned ones", () => {
    const tabs = [tab("p1", true), tab("p2", true), tab("u1"), tab("u2")];
    // Drag p2 before p1 → p2, p1, u1, u2
    expect(ids(reorderTabs(tabs, "p2", "p1", false))).toEqual([
      "p2",
      "p1",
      "u1",
      "u2",
    ]);
  });

  it("rejects a cross-group move (unpinned onto pinned)", () => {
    const tabs = [tab("p1", true), tab("u1"), tab("u2")];
    // Dropping an unpinned tab onto a pinned one must not cross the boundary.
    const out = reorderTabs(tabs, "u1", "p1", false);
    expect(out).toBe(tabs); // unchanged reference — signals "rejected"
  });

  it("rejects a cross-group move (pinned onto unpinned)", () => {
    const tabs = [tab("p1", true), tab("u1")];
    expect(reorderTabs(tabs, "p1", "u1", true)).toBe(tabs);
  });

  it("is a no-op for a self-drop or an unknown id", () => {
    const tabs = [tab("a"), tab("b")];
    expect(reorderTabs(tabs, "a", "a", false)).toBe(tabs);
    expect(reorderTabs(tabs, "ghost", "a", false)).toBe(tabs);
    expect(reorderTabs(tabs, "a", "ghost", false)).toBe(tabs);
  });
});
