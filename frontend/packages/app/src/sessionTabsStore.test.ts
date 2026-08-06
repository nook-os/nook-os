import { beforeEach, describe, expect, it } from "vitest";
import {
  deriveTabs,
  reorderTabs,
  type LiveSession,
  type SessionTab,
  type TabPrefs,
  useSessionTabPrefs,
} from "./sessionTabsStore";

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

// ── MAIN-322: the strip is the live session list ────────────────────────────

const live = (
  id: string,
  workspace_id: string | null = null,
  node_id = "n1",
): LiveSession => ({
  id,
  name: `s-${id}`,
  runtime: "bash",
  workspace_id,
  node_id,
});

const noPrefs: TabPrefs = { pinned: [], collapsed: [], order: [] };

describe("deriveTabs — membership comes from the sessions, not from prefs", () => {
  it("shows every live session with no local state at all", () => {
    // The point of the card: a browser that has never opened any of these
    // still shows all of them, because there is no open-set to have missed.
    const tabs = deriveTabs([live("a"), live("b"), live("c")], {}, noPrefs);
    expect(ids(tabs)).toEqual(["a", "b", "c"]);
  });

  it("cannot resurrect a session that is gone, however many prefs name it", () => {
    // The inverse, and the reason membership must not be a pref: an ended
    // session leaves the strip even though it is still pinned and ordered.
    const prefs: TabPrefs = { pinned: ["ghost"], collapsed: [], order: ["ghost", "a"] };
    expect(ids(deriveTabs([live("a")], {}, prefs))).toEqual(["a"]);
  });

  it("shows the same tabs on any machine, whatever the local prefs are", () => {
    // AC-2: two browsers with different local histories differ at most in the
    // ORDER of the strip — never in which sessions are on it.
    const sessions = [live("a"), live("b"), live("c")];
    const machineA = deriveTabs(sessions, {}, noPrefs);
    const machineB = deriveTabs(sessions, {}, { pinned: ["c"], collapsed: [], order: ["b", "a"] });
    expect(ids(machineB)).toEqual(["c", "b", "a"]); // arranged differently…
    expect(ids(machineA).slice().sort()).toEqual(ids(machineB).slice().sort()); // …same tabs
  });

  it("hydrates the workspace name and leaves ad-hoc terminals unlabeled", () => {
    const tabs = deriveTabs([live("a", "w1"), live("b")], { w1: "nook-os" }, noPrefs);
    expect(tabs[0].workspaceName).toBe("nook-os");
    expect(tabs[0].workspaceId).toBe("w1");
    expect(tabs[1].workspaceName).toBeUndefined();
    expect(tabs[1].workspaceId).toBeUndefined();
  });
});

describe("deriveTabs — view prefs order the strip", () => {
  it("sorts pinned tabs first", () => {
    const prefs: TabPrefs = { pinned: ["c"], collapsed: [], order: [] };
    expect(ids(deriveTabs([live("a"), live("b"), live("c")], {}, prefs))).toEqual([
      "c",
      "a",
      "b",
    ]);
    expect(deriveTabs([live("a"), live("c")], {}, prefs)[0].pinned).toBe(true);
  });

  it("honours a saved drag order, and appends sessions never dragged", () => {
    // A session started after the user arranged their strip must land at the
    // end rather than in the middle of the arrangement.
    const prefs: TabPrefs = { pinned: [], collapsed: [], order: ["c", "a"] };
    expect(ids(deriveTabs([live("a"), live("b"), live("c")], {}, prefs))).toEqual([
      "c",
      "a",
      "b",
    ]);
  });
});

describe("deriveTabs — no workspace scoping", () => {
  // It used to take a workspace context and filter on it. MAIN-417 made the
  // strip a set you curate yourself, so that was filtering your own choices a
  // second time, and the only caller left was passing an opt-out to turn it
  // off. Pinned here as the rule rather than left as an absence: a future
  // reader adding "just scope it to the current workspace" should have to
  // delete a test that says why not.
  it("keeps every workspace's sessions, and the ad-hoc ones", () => {
    const sessions = [live("a", "w1"), live("b", "w2"), live("c")];
    expect(ids(deriveTabs(sessions, {}, noPrefs))).toEqual(["a", "b", "c"]);
  });
});

describe("prune — prefs are dropped only for sessions that are really gone", () => {
  beforeEach(() => {
    localStorage.clear();
    useSessionTabPrefs.setState({
      prefs: { pinned: ["a"], collapsed: ["w1"], order: ["a", "b"] },
    });
  });

  it("drops the ids of sessions that ended, keeping the live ones", () => {
    useSessionTabPrefs.getState().prune(["a"]);
    expect(useSessionTabPrefs.getState().prefs).toEqual({
      pinned: ["a"],
      // `collapsed` holds WORKSPACE ids and must survive: a workspace whose
      // last session just ended still exists, and forgetting that its group
      // was collapsed every time would make the setting feel random.
      collapsed: ["w1"],
      order: ["a"],
    });
  });

  it("does NOTHING when the live list is unknown", () => {
    // The session query is pending (or failed) on every page load. Treating
    // that as an empty list would wipe the user's pins and order each time the
    // app starts — the prefs would survive nothing but a warm cache.
    useSessionTabPrefs.getState().prune(undefined);
    expect(useSessionTabPrefs.getState().prefs).toEqual({
      pinned: ["a"],
      collapsed: ["w1"],
      order: ["a", "b"],
    });
  });

  it("does clear prefs when every session really is gone", () => {
    useSessionTabPrefs.getState().prune([]);
    expect(useSessionTabPrefs.getState().prefs).toEqual({
      pinned: [],
      collapsed: ["w1"],
      order: [],
    });
  });
});

describe("toggleCollapsed — MAIN-323 AC-3", () => {
  beforeEach(() => {
    localStorage.clear();
    useSessionTabPrefs.setState({ prefs: { pinned: [], collapsed: [], order: [] } });
  });

  it("collapses, expands, and persists", () => {
    const { toggleCollapsed } = useSessionTabPrefs.getState();
    toggleCollapsed("w1");
    expect(useSessionTabPrefs.getState().prefs.collapsed).toEqual(["w1"]);
    // It survives a reload: the store writes through to localStorage.
    expect(JSON.parse(localStorage.getItem(Object.keys(localStorage)[0])!).collapsed).toEqual([
      "w1",
    ]);
    toggleCollapsed("w1");
    expect(useSessionTabPrefs.getState().prefs.collapsed).toEqual([]);
  });

  it("does not disturb pin or order", () => {
    useSessionTabPrefs.setState({ prefs: { pinned: ["a"], collapsed: [], order: ["a"] } });
    useSessionTabPrefs.getState().toggleCollapsed("w1");
    const p = useSessionTabPrefs.getState().prefs;
    expect(p.pinned).toEqual(["a"]);
    expect(p.order).toEqual(["a"]);
  });
});
