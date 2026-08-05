// MAIN-417: the strip is a synced per-user working set again.
//
// The rules that matter are the ones MAIN-322 got wrong by removing local
// membership: closing must drop a tab WITHOUT ending anything, a dead session
// must keep its tab, and the set must never seed itself from what happens to be
// running.
import { describe, expect, it } from "vitest";
import {
  EMPTY_WORKING_SET,
  closeSession,
  deriveWorkingSetTabs,
  openSession,
  parseWorkingSet,
  reorderWorkingSet,
  strandedIds,
  togglePinned,
  type WorkingSet,
} from "./workingSet";
import type { LiveSession } from "./sessionTabsStore";

const session = (id: string, extra: Partial<LiveSession> = {}): LiveSession => ({
  id,
  name: id,
  runtime: "bash",
  node_id: "n1",
  ...extra,
});

const set = (o: Partial<WorkingSet> = {}): WorkingSet => ({
  ...EMPTY_WORKING_SET,
  ...o,
});

describe("the set is membership", () => {
  it("starts empty and is never seeded from the live sessions", () => {
    // AC-5 / NG-4. A blank strip after the upgrade is the accepted cost; the
    // alternative is the derived strip this card exists to replace.
    expect(parseWorkingSet(undefined)).toEqual(EMPTY_WORKING_SET);
    expect(
      deriveWorkingSetTabs(EMPTY_WORKING_SET, [session("a"), session("b")], {}),
    ).toEqual([]);
  });

  it("opens and closes without touching anything else", () => {
    const one = openSession(EMPTY_WORKING_SET, "a");
    expect(one.open).toEqual(["a"]);
    const two = openSession(one, "b");
    expect(two.open).toEqual(["a", "b"]);
    expect(closeSession(two, "a").open).toEqual(["b"]);
  });

  it("opening what is already open does not reorder the strip", () => {
    // Clicking the tab you are looking at goes through here.
    const s = set({ open: ["a", "b"] });
    expect(openSession(s, "a")).toBe(s);
  });

  it("closing drops the id from pin and order too, so the row cannot grow forever", () => {
    const s = set({ open: ["a", "b"], pinned: ["a"], order: ["a", "b"] });
    expect(closeSession(s, "a")).toEqual({ open: ["b"], pinned: [], order: ["b"] });
  });

  it("closing something that is not open is a no-op", () => {
    const s = set({ open: ["a"] });
    expect(closeSession(s, "zzz")).toBe(s);
  });
});

describe("presentation comes from the sessions", () => {
  const sessions = [
    session("a", { name: "alpha", runtime: "claude", workspace_id: "w1" }),
    session("b", { name: "beta" }),
  ];

  it("hydrates name, runtime, workspace and machine", () => {
    const tabs = deriveWorkingSetTabs(
      set({ open: ["a", "b"] }),
      sessions,
      { w1: "api" },
      { n1: "azul" },
    );
    expect(tabs.map((t) => [t.name, t.runtime, t.workspaceName, t.nodeName])).toEqual([
      ["alpha", "claude", "api", "azul"],
      ["beta", "bash", undefined, "azul"],
    ]);
  });

  it("KEEPS the tab of a session that exited", () => {
    // AC-4, and the reason the strip reads every session rather than the live
    // ones: a dead tab is how you find the thing to restart.
    const dead = [session("a", { name: "alpha" })]; // present in the list, whatever its status
    expect(deriveWorkingSetTabs(set({ open: ["a"] }), dead, {}).map((t) => t.id)).toEqual([
      "a",
    ]);
  });

  it("drops a tab for a session the server no longer has at all", () => {
    // Not a status question — there is nothing left to render or restart.
    expect(deriveWorkingSetTabs(set({ open: ["gone"] }), sessions, {})).toEqual([]);
  });

  it("shows every workspace's sessions — the strip is not workspace-scoped", () => {
    const tabs = deriveWorkingSetTabs(set({ open: ["a", "b"] }), sessions, {});
    expect(tabs).toHaveLength(2);
  });
});

describe("order and pin, now synced rather than per-browser", () => {
  const sessions = [session("a"), session("b"), session("c")];

  it("sorts pinned first", () => {
    const tabs = deriveWorkingSetTabs(
      set({ open: ["a", "b", "c"], pinned: ["c"] }),
      sessions,
      {},
    );
    expect(tabs.map((t) => t.id)).toEqual(["c", "a", "b"]);
  });

  it("honours a saved drag order and appends what was never dragged", () => {
    const tabs = deriveWorkingSetTabs(
      set({ open: ["a", "b", "c"], order: ["c", "a"] }),
      sessions,
      {},
    );
    expect(tabs.map((t) => t.id)).toEqual(["c", "a", "b"]);
  });

  it("toggles a pin both ways", () => {
    const on = togglePinned(set({ open: ["a"] }), "a");
    expect(on.pinned).toEqual(["a"]);
    expect(togglePinned(on, "a").pinned).toEqual([]);
  });

  it("records a drag as an order the next machine will read", () => {
    // AC-6: the same drag used to land in localStorage and stay there.
    const s = set({ open: ["a", "b", "c"] });
    const visible = deriveWorkingSetTabs(s, sessions, {});
    const moved = reorderWorkingSet(s, "c", "a", false, visible);
    expect(moved.order.slice(0, 3)).toEqual(["c", "a", "b"]);
    expect(deriveWorkingSetTabs(moved, sessions, {}).map((t) => t.id)).toEqual([
      "c",
      "a",
      "b",
    ]);
  });

  it("leaves the set alone when a drag is rejected", () => {
    const s = set({ open: ["a", "b"], pinned: ["a"] });
    const visible = deriveWorkingSetTabs(s, sessions, {});
    // Cross-pin-group moves are refused by MAIN-322's rule, which this reuses.
    expect(reorderWorkingSet(s, "b", "a", false, visible)).toBe(s);
  });
});

describe("strandedIds", () => {
  it("names only the ids the server has forgotten", () => {
    expect(strandedIds(set({ open: ["a", "gone"] }), [session("a")])).toEqual(["gone"]);
  });

  it("names NOTHING while the session list is still pending", () => {
    // Load-bearing: an undefined list read as "everything is gone" would empty
    // the strip on every page load, and the write-back would make it permanent.
    expect(strandedIds(set({ open: ["a", "b"] }), undefined)).toEqual([]);
  });
});

describe("parseWorkingSet", () => {
  it("round-trips what the hook stores", () => {
    const s = { open: ["a"], pinned: ["a"], order: ["a"] };
    expect(parseWorkingSet(JSON.parse(JSON.stringify(s)))).toEqual(s);
  });

  it("falls back field by field rather than losing the whole strip", () => {
    expect(parseWorkingSet({ open: ["a"], pinned: "nope" })).toEqual({
      open: ["a"],
      pinned: [],
      order: [],
    });
  });

  it("ignores non-string ids instead of putting them in the strip", () => {
    expect(parseWorkingSet({ open: ["a", 7, null] }).open).toEqual(["a"]);
  });
});
