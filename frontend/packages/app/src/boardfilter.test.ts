import { describe, expect, it } from "vitest";
import { type TaskItem } from "@nookos/api";
import {
  parseFilter,
  serializeFilter,
  writeFilter,
  showsUnderArchive,
  isBacklogTask,
  groupByEpic,
  epicOptions,
  type BoardFilter,
} from "./pages/Board";

/** A minimal TaskItem for the grouping tests — only the fields the pure
 *  functions read; the rest is filled to satisfy the type. */
function mk(p: Partial<TaskItem> & { id: string }): TaskItem {
  return {
    title: p.id,
    type: "task",
    priority: 0,
    position: 0,
    created_at: "2026-01-01T00:00:00Z",
    column_id: "col-backlog",
    ...p,
  } as TaskItem;
}

const roundTrip = (f: BoardFilter) => parseFilter(serializeFilter(f));

describe("board filter URL round-trip (MAIN-15 AC-1)", () => {
  const cases: BoardFilter[] = [
    {
      label: [],
      not_label: [],
      type: [],
      visibility: [],
      assignee: "any",
      priority: null,
      blocked: null,
      workspace: null,
      showArchived: false,
      q: "",
      view: "board",
    },
    {
      label: ["agent-ready", "urgent"],
      not_label: ["blocked"],
      type: ["bug", "epic"],
      visibility: ["private", "org"],
      assignee: "me",
      priority: 2,
      blocked: false,
      workspace: "019f840f-2d80-7163-b4b1-8b1e12d7e0d3",
      showArchived: true,
      q: "postmark",
      view: "backlog",
    },
    {
      label: [],
      not_label: [],
      type: ["chore"],
      visibility: ["team"],
      assignee: "none",
      priority: 0,
      blocked: true,
      workspace: null,
      showArchived: false,
      q: "MAIN-42",
      view: "board",
    },
  ];

  it("serialize → parse is the identity", () => {
    for (const f of cases) expect(roundTrip(f)).toEqual(f);
  });

  it("an empty filter serializes to an empty query string", () => {
    expect(serializeFilter(cases[0]).toString()).toBe("");
  });

  it("preserves the `task` param and only touches filter keys", () => {
    const params = new URLSearchParams("task=NOOK-42&label=old");
    const next = writeFilter(params, cases[1]);
    expect(next.get("task")).toBe("NOOK-42"); // untouched
    expect(next.get("label")).toBe("agent-ready,urgent"); // rewritten
  });

  it("writes the multi-select type filter as a comma list, and parses it back (MAIN-60 AC-4)", () => {
    expect(serializeFilter(cases[1]).get("type")).toBe("bug,epic");
    expect(parseFilter(new URLSearchParams("type=bug,epic")).type).toEqual([
      "bug",
      "epic",
    ]);
    // No type filter → the key is absent, so a default board URL is unchanged.
    expect(serializeFilter(cases[0]).has("type")).toBe(false);
  });

  it("writes the multi-select visibility filter as a comma list, and parses it back (MAIN-103)", () => {
    expect(serializeFilter(cases[1]).get("vis")).toBe("private,org");
    expect(parseFilter(new URLSearchParams("vis=private,org")).visibility).toEqual([
      "private",
      "org",
    ]);
    // No visibility filter → the key is absent, so a default board URL is unchanged.
    expect(serializeFilter(cases[0]).has("vis")).toBe(false);
  });

  it("round-trips the Board/Backlog tab via ?view (MAIN-82 AC-4)", () => {
    // The default `board` tab writes no key, so a plain board URL is unchanged.
    expect(serializeFilter(cases[0]).has("view")).toBe(false);
    // The backlog tab is addressable and survives a round-trip.
    expect(serializeFilter(cases[1]).get("view")).toBe("backlog");
    expect(parseFilter(new URLSearchParams("view=backlog")).view).toBe("backlog");
    // Anything else falls back to the board tab.
    expect(parseFilter(new URLSearchParams("view=nonsense")).view).toBe("board");
    expect(parseFilter(new URLSearchParams("")).view).toBe("board");
  });
});

describe("epic grouping in the Backlog tab (MAIN-83 AC-1)", () => {
  const colType = new Map<string, string | undefined>([
    ["col-backlog", "backlog"],
    ["col-todo", "unstarted"],
    ["col-done", "completed"],
  ]);

  it("groups children under their epic, counts done/total, and buckets parentless backlog tasks", () => {
    const tasks = [
      mk({ id: "E1", type: "epic", priority: 2 }),
      mk({ id: "E2", type: "epic", priority: 3 }), // empty epic
      mk({ id: "C1", parent_task_id: "E1", column_id: "col-backlog" }),
      mk({ id: "C2", parent_task_id: "E1", column_id: "col-done" }), // done
      mk({ id: "N1", column_id: "col-backlog" }), // parentless backlog → No epic
      mk({ id: "B1", column_id: "col-todo" }), // parentless on-board → shown nowhere here
    ];
    const g = groupByEpic(tasks, colType);

    // Epics in pick order (E1 priority 2 before E2 priority 3).
    expect(g.epics.map((s) => s.epic.id)).toEqual(["E1", "E2"]);
    const e1 = g.epics[0];
    expect(e1.children.map((c) => c.id).sort()).toEqual(["C1", "C2"]);
    expect(e1.done).toBe(1); // only C2 is in a completed column
    expect(e1.total).toBe(2);
    expect(g.epics[1].total).toBe(0); // empty epic still gets a section

    // The No-epic bucket holds ONLY the parentless backlog task, not the
    // parentless on-board one (that lives on the kanban).
    expect(g.noEpic.map((t) => t.id)).toEqual(["N1"]);
  });
});

describe("epic picker options (MAIN-83 AC-4/AC-5)", () => {
  it("offers every epic except the task itself, and only epics", () => {
    const tasks = [
      mk({ id: "E1", type: "epic" }),
      mk({ id: "E2", type: "epic" }),
      mk({ id: "T1", type: "task" }),
    ];
    expect(epicOptions(tasks, "T1").map((e) => e.id)).toEqual(["E1", "E2"]);
    // An epic cannot be filed under itself.
    expect(epicOptions(tasks, "E1").map((e) => e.id)).toEqual(["E2"]);
    // Non-epics are never options.
    expect(epicOptions(tasks, "E1").some((e) => e.id === "T1")).toBe(false);
  });
});

describe("which tab a task belongs to (MAIN-82 AC-1/AC-5)", () => {
  it("puts backlog-column tasks and epics in the Backlog tab, everything else on the Board", () => {
    // A backlog-column task → Backlog.
    expect(isBacklogTask("backlog", "task")).toBe(true);
    // An epic → Backlog, regardless of the column it sits in.
    expect(isBacklogTask("started", "epic")).toBe(true);
    expect(isBacklogTask("backlog", "epic")).toBe(true);
    // A normal workflow task → the kanban Board tab.
    expect(isBacklogTask("unstarted", "task")).toBe(false);
    expect(isBacklogTask("review", "bug")).toBe(false);
    // Unknown/absent types default to the Board tab (only backlog/epic leave it).
    expect(isBacklogTask(undefined, undefined)).toBe(false);
  });
});

describe("archive visibility (MAIN-15 AC-5)", () => {
  it("hides archived tasks unless the toggle is on", () => {
    expect(showsUnderArchive(false, undefined)).toBe(true); // live
    expect(showsUnderArchive(false, null)).toBe(true); // live
    expect(showsUnderArchive(false, "2026-01-01T00:00:00Z")).toBe(false); // archived, hidden
    expect(showsUnderArchive(true, "2026-01-01T00:00:00Z")).toBe(true); // archived, shown
  });
});
