import { describe, expect, it } from "vitest";
import type { TaskItem } from "@nookos/api";
import type { BacklogGroups, EpicSection } from "./Board";
import {
  nextCollapsed,
  selectableRowIds,
  summarizeBulk,
  useBacklogSelection,
} from "./backlogSelection";
import type { BulkTaskResponse } from "@nookos/api";

// Minimal fixtures — rows only need the fields the helpers touch.
const task = (id: string, type = "task"): TaskItem =>
  ({ id, key: id.toUpperCase(), type }) as unknown as TaskItem;

const section = (epicId: string, childIds: string[]): EpicSection => ({
  epic: task(epicId, "epic"),
  children: childIds.map((c) => task(c)),
  done: 0,
  total: childIds.length,
});

const groups: BacklogGroups = {
  epics: [section("e1", ["a", "b"]), section("e2", ["c"])],
  noEpic: [task("n1"), task("n2")],
};

describe("selectableRowIds", () => {
  it("returns every noEpic row plus the children of non-collapsed epics", () => {
    expect(selectableRowIds(groups, new Set()).sort()).toEqual([
      "a",
      "b",
      "c",
      "n1",
      "n2",
    ]);
  });

  it("excludes the children of a collapsed epic (they're hidden)", () => {
    // e1 collapsed → a, b drop out; e2's child and the noEpic rows remain.
    expect(selectableRowIds(groups, new Set(["e1"])).sort()).toEqual([
      "c",
      "n1",
      "n2",
    ]);
  });

  it("never includes the epic ids themselves (containers aren't selectable)", () => {
    const ids = selectableRowIds(groups, new Set());
    expect(ids).not.toContain("e1");
    expect(ids).not.toContain("e2");
  });
});

describe("nextCollapsed", () => {
  it("toggles an id into and back out of the set", () => {
    const a = nextCollapsed(new Set(), "e1");
    expect(a.has("e1")).toBe(true);
    const b = nextCollapsed(a, "e1");
    expect(b.has("e1")).toBe(false);
  });

  it("returns a new set and does not mutate the input", () => {
    const input = new Set(["e1"]);
    const out = nextCollapsed(input, "e2");
    expect(out).not.toBe(input); // fresh reference
    expect([...input]).toEqual(["e1"]); // input untouched
    expect(out.has("e1")).toBe(true);
    expect(out.has("e2")).toBe(true);
  });
});

describe("summarizeBulk", () => {
  const res = (
    results: { id: string; status: string; reason?: string | null }[],
  ): BulkTaskResponse => ({
    results,
    updated: results.filter((r) => r.status === "ok").length,
    skipped: results.filter((r) => r.status === "skipped").length,
  });

  it("on full success reports only the updated count and keeps nothing selected", () => {
    const out = summarizeBulk(res([
      { id: "a", status: "ok" },
      { id: "b", status: "ok" },
    ]));
    expect(out.message).toBe("2 updated");
    expect(out.skippedIds).toEqual([]);
  });

  it("on partial success keeps the skipped ids and dedupes distinct reasons", () => {
    const out = summarizeBulk(res([
      { id: "a", status: "ok" },
      { id: "b", status: "ok" },
      { id: "e1", status: "skipped", reason: "epics are containers" },
      { id: "e2", status: "skipped", reason: "epics are containers" },
    ]));
    expect(out.skippedIds).toEqual(["e1", "e2"]);
    expect(out.message).toBe("2 updated, 2 skipped: epics are containers");
  });

  it("joins several distinct skip reasons", () => {
    const out = summarizeBulk(res([
      { id: "a", status: "skipped", reason: "epics are containers" },
      { id: "b", status: "skipped", reason: "not found" },
    ]));
    expect(out.message).toBe("0 updated, 2 skipped: epics are containers; not found");
  });

  it("omits the reason clause when skipped rows carry no reason", () => {
    const out = summarizeBulk(res([
      { id: "a", status: "ok" },
      { id: "b", status: "skipped" },
    ]));
    expect(out.message).toBe("1 updated, 1 skipped");
  });
});

describe("useBacklogSelection", () => {
  const store = useBacklogSelection;

  it("toggle adds an id then removes it, each time a new Set", () => {
    store.getState().clear();
    const before = store.getState().selected;
    store.getState().toggle("a");
    expect(store.getState().selected.has("a")).toBe(true);
    expect(store.getState().selected).not.toBe(before); // immutable
    store.getState().toggle("a");
    expect(store.getState().selected.has("a")).toBe(false);
  });

  it("setSelection replaces the whole set", () => {
    store.getState().clear();
    store.getState().toggle("a");
    store.getState().setSelection(["x", "y"]);
    expect([...store.getState().selected].sort()).toEqual(["x", "y"]);
  });

  it("clear empties the selection", () => {
    store.getState().setSelection(["x", "y"]);
    store.getState().clear();
    expect(store.getState().selected.size).toBe(0);
  });
});
