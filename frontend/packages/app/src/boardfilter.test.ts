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
  activeChips,
  isFilterActive,
  searchTypeParam,
  matchedEpicHeaders,
  exactKeyMatch,
  BACKLOG_TYPES,
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
      epic: null,
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
      epic: "019fa216-3c52-7a13-b7b6-b60f91802850",
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
      epic: null,
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

describe("specific-person assignee + epic filter (MAIN-111)", () => {
  const uuid = "019fa216-3c52-7a13-b7b6-b60f91802850";

  it("parses a uuid assignee, keeps me/none, and coerces garbage to any (AC-2)", () => {
    expect(parseFilter(new URLSearchParams(`assignee=${uuid}`)).assignee).toBe(uuid);
    expect(parseFilter(new URLSearchParams("assignee=me")).assignee).toBe("me");
    expect(parseFilter(new URLSearchParams("assignee=none")).assignee).toBe("none");
    // Not a uuid, not me/none → any, rather than passing junk to the server.
    expect(parseFilter(new URLSearchParams("assignee=nonsense")).assignee).toBe("any");
    expect(parseFilter(new URLSearchParams("")).assignee).toBe("any");
  });

  it("round-trips a uuid assignee and an epic through the URL (AC-2/AC-4)", () => {
    const f = parseFilter(new URLSearchParams(`assignee=${uuid}&epic=${uuid}`));
    expect(f.assignee).toBe(uuid);
    expect(f.epic).toBe(uuid);
    const round = parseFilter(serializeFilter(f));
    expect(round.assignee).toBe(uuid);
    expect(round.epic).toBe(uuid);
    // Absent keys stay absent, so a default board URL is unchanged.
    expect(serializeFilter({ ...f, assignee: "any", epic: null }).has("assignee")).toBe(false);
    expect(serializeFilter({ ...f, epic: null }).has("epic")).toBe(false);
    // A non-uuid epic is dropped (AC-6: no crash, treated as no filter).
    expect(parseFilter(new URLSearchParams("epic=nope")).epic).toBeNull();
  });

  it("renders a person chip by display name and an epic chip by key (AC-5)", () => {
    const base: BoardFilter = {
      label: [],
      not_label: [],
      type: [],
      visibility: [],
      assignee: uuid,
      priority: null,
      blocked: null,
      epic: uuid,
      workspace: null,
      showArchived: false,
      q: "",
      view: "board",
    };
    const members = [{ id: uuid, name: "Alex Rivera" }];
    const epics = [{ id: uuid, key: "MAIN-7" }];
    const chips = activeChips(base, [], members, epics);
    expect(chips.find((c) => c.key === "assignee")?.label).toBe("Alex Rivera");
    expect(chips.find((c) => c.key === "epic")?.label).toBe("MAIN-7");
    // Removing each clears just that filter.
    expect(chips.find((c) => c.key === "assignee")?.next.assignee).toBe("any");
    expect(chips.find((c) => c.key === "epic")?.next.epic).toBeNull();
  });

  it("falls back to a label (not a crash) for an unknown user or epic (AC-6)", () => {
    const base: BoardFilter = {
      label: [],
      not_label: [],
      type: [],
      visibility: [],
      assignee: uuid,
      priority: null,
      blocked: null,
      epic: uuid,
      workspace: null,
      showArchived: false,
      q: "",
      view: "board",
    };
    // No members/epics resolve the ids — the chips still render and are active.
    const chips = activeChips(base, [], [], []);
    expect(chips.find((c) => c.key === "assignee")?.label).toBe("unknown user");
    expect(chips.find((c) => c.key === "epic")?.label).toBe("unknown epic");
    expect(isFilterActive(base)).toBe(true);
  });
});

describe("active-filter chips (MAIN-110 AC-2/AC-3/AC-4)", () => {
  const empty: BoardFilter = {
    label: [],
    not_label: [],
    type: [],
    visibility: [],
    assignee: "any",
    priority: null,
    blocked: null,
    epic: null,
    workspace: null,
    showArchived: false,
    q: "",
    view: "board",
  };
  const ws = [{ id: "ws1", name: "nook-os" }];

  it("an empty filter has no chips and is not active", () => {
    expect(activeChips(empty, ws)).toEqual([]);
    expect(isFilterActive(empty)).toBe(false);
  });

  it("search alone is active but is NOT a chip (excluded from the count)", () => {
    const f = { ...empty, q: "postmark" };
    expect(activeChips(f, ws)).toEqual([]); // search has its own box
    expect(isFilterActive(f)).toBe(true);
  });

  it("a workspace-only filter counts as active (fixes the old gap) — AC-4", () => {
    const f = { ...empty, workspace: "ws1" };
    const chips = activeChips(f, ws);
    expect(chips.map((c) => c.label)).toEqual(["nook-os"]); // resolved name
    expect(isFilterActive(f)).toBe(true);
  });

  it("an archived-only filter counts as active — AC-4", () => {
    const f = { ...empty, showArchived: true };
    expect(activeChips(f, ws).map((c) => c.key)).toEqual(["archived"]);
    expect(isFilterActive(f)).toBe(true);
  });

  it("an excluded label chip is negated and distinct from an included one", () => {
    const f = { ...empty, label: ["urgent"], not_label: ["blocked"] };
    const chips = activeChips(f, ws);
    const inc = chips.find((c) => c.label === "urgent")!;
    const exc = chips.find((c) => c.label === "blocked")!;
    expect(inc.negated).toBeFalsy();
    expect(exc.negated).toBe(true);
  });

  it("removing a chip clears only that one filter", () => {
    const f = {
      ...empty,
      label: ["a", "b"],
      type: ["bug"],
      assignee: "me" as const,
      workspace: "ws1",
    };
    const chips = activeChips(f, ws);
    // one chip per value: 2 labels + 1 type + assignee + workspace = 5
    expect(chips).toHaveLength(5);
    const removeA = chips.find((c) => c.key === "label:a")!;
    expect(removeA.next.label).toEqual(["b"]); // only "a" gone
    expect(removeA.next.type).toEqual(["bug"]); // everything else intact
    const removeWs = chips.find((c) => c.key === "ws")!;
    expect(removeWs.next.workspace).toBeNull();
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

const EMPTY: BoardFilter = {
  label: [],
  not_label: [],
  type: [],
  visibility: [],
  assignee: "any",
  priority: null,
  blocked: null,
  epic: null,
  workspace: null,
  showArchived: false,
  q: "",
  view: "board",
};

describe("backlog search includes epics (MAIN-181 AC-1)", () => {
  it("asks for ALL types incl. epic on the backlog tab with no explicit type", () => {
    const t = searchTypeParam({ ...EMPTY, view: "backlog", q: "auth" });
    expect(t).toEqual([...BACKLOG_TYPES]);
    expect(t).toContain("epic");
  });

  it("respects an explicit type filter verbatim (any tab)", () => {
    expect(searchTypeParam({ ...EMPTY, view: "backlog", type: ["bug"] })).toEqual(["bug"]);
    expect(searchTypeParam({ ...EMPTY, view: "board", type: ["story", "epic"] })).toEqual([
      "story",
      "epic",
    ]);
  });

  it("omits the type param on the kanban tab (epics never render there)", () => {
    expect(searchTypeParam({ ...EMPTY, view: "board", q: "auth" })).toBeUndefined();
  });
});

describe("grouping survives search: a matching child shows its epic header (MAIN-181 AC-2)", () => {
  it("pulls in the epic header for a matched child even when the epic didn't match", () => {
    const epic = mk({ id: "e1", type: "epic", key: "MAIN-1" });
    const child = mk({ id: "c1", parent_task_id: "e1", key: "MAIN-2", title: "matched child" });
    const other = mk({ id: "e2", type: "epic", key: "MAIN-9" }); // unrelated epic

    // The search matched only the child (not its epic, not the other epic).
    const headers = matchedEpicHeaders([child], [epic, other, child]);
    expect(headers.map((t) => t.id)).toEqual(["e1"]);

    // Feeding those headers into groupByEpic renders the child under its header.
    const g = groupByEpic([child, ...headers], new Map([["col-backlog", "backlog"]]));
    const section = g.epics.find((s) => s.epic.id === "e1");
    expect(section).toBeTruthy();
    expect(section!.children.map((c) => c.id)).toEqual(["c1"]);
  });

  it("does not re-add an epic that itself matched (already visible)", () => {
    const epic = mk({ id: "e1", type: "epic" });
    const child = mk({ id: "c1", parent_task_id: "e1" });
    expect(matchedEpicHeaders([epic, child], [epic, child])).toEqual([]);
  });
});

describe("exact-key search hit (MAIN-181 AC-3)", () => {
  const tasks = [
    mk({ id: "t34", key: "MAIN-34", title: "the one" }),
    mk({ id: "t340", key: "MAIN-340", title: "a longer key" }),
  ];

  it("matches a full key case-insensitively", () => {
    expect(exactKeyMatch(tasks, "MAIN-34")).toBe("t34");
    expect(exactKeyMatch(tasks, "main-34")).toBe("t34");
    expect(exactKeyMatch(tasks, "  MAIN-34 ")).toBe("t34");
  });

  it("does not treat a partial/other query as an exact hit", () => {
    expect(exactKeyMatch(tasks, "MAIN-3")).toBeNull(); // partial
    expect(exactKeyMatch(tasks, "the one")).toBeNull(); // title, not a key
    expect(exactKeyMatch(tasks, "")).toBeNull();
  });
});
