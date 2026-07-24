import { describe, expect, it } from "vitest";
import {
  parseFilter,
  serializeFilter,
  writeFilter,
  showsUnderArchive,
  type BoardFilter,
} from "./pages/Board";

const roundTrip = (f: BoardFilter) => parseFilter(serializeFilter(f));

describe("board filter URL round-trip (MAIN-15 AC-1)", () => {
  const cases: BoardFilter[] = [
    {
      label: [],
      not_label: [],
      assignee: "any",
      priority: null,
      blocked: null,
      workspace: null,
      showArchived: false,
    },
    {
      label: ["agent-ready", "urgent"],
      not_label: ["blocked"],
      assignee: "me",
      priority: 2,
      blocked: false,
      workspace: "019f840f-2d80-7163-b4b1-8b1e12d7e0d3",
      showArchived: true,
    },
    {
      label: [],
      not_label: [],
      assignee: "none",
      priority: 0,
      blocked: true,
      workspace: null,
      showArchived: false,
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
});

describe("archive visibility (MAIN-15 AC-5)", () => {
  it("hides archived tasks unless the toggle is on", () => {
    expect(showsUnderArchive(false, undefined)).toBe(true); // live
    expect(showsUnderArchive(false, null)).toBe(true); // live
    expect(showsUnderArchive(false, "2026-01-01T00:00:00Z")).toBe(false); // archived, hidden
    expect(showsUnderArchive(true, "2026-01-01T00:00:00Z")).toBe(true); // archived, shown
  });
});
