import { describe, expect, it } from "vitest";
import {
  parseFilter,
  serializeFilter,
  writeFilter,
  showsUnderArchive,
  isBacklogTask,
  type BoardFilter,
} from "./pages/Board";

const roundTrip = (f: BoardFilter) => parseFilter(serializeFilter(f));

describe("board filter URL round-trip (MAIN-15 AC-1)", () => {
  const cases: BoardFilter[] = [
    {
      label: [],
      not_label: [],
      type: [],
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
