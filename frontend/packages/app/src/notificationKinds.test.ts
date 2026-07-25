import { describe, expect, it } from "vitest";
import { type NotificationKind } from "@nookos/api";
import {
  decode,
  encode,
  groupsOf,
  groupLabel,
  type KindsState,
} from "./notificationKinds";

/** A catalog shaped like the real one (MAIN-91): three groups, mixed sizes. */
const CATALOG: NotificationKind[] = [
  { id: "node.connected", label: "Node connected", description: "", group: "node." },
  { id: "node.disconnected", label: "Node disconnected", description: "", group: "node." },
  { id: "git.clone_finished", label: "Clone finished", description: "", group: "git." },
  { id: "task.claimed", label: "Task claimed", description: "", group: "task." },
  { id: "task.comment.created", label: "New comment", description: "", group: "task." },
  { id: "task.label.added", label: "Escalation label", description: "", group: "task." },
];

const ids = (s: KindsState) => [...s.checked].sort();

describe("groupsOf (MAIN-92 AC-1)", () => {
  it("groups by prefix in first-seen order with readable labels", () => {
    const gs = groupsOf(CATALOG);
    expect(gs.map((g) => g.prefix)).toEqual(["node.", "git.", "task."]);
    expect(gs.map((g) => g.label)).toEqual(["Nodes", "Git", "Tasks"]);
    expect(gs[2].kinds.map((k) => k.id)).toEqual([
      "task.claimed",
      "task.comment.created",
      "task.label.added",
    ]);
  });

  it("falls back to a capitalised stem for an unknown group", () => {
    expect(groupLabel("deploy.")).toBe("Deploy");
    expect(groupLabel("task.")).toBe("Tasks");
  });
});

describe("decode: kinds → checkboxes (MAIN-92 AC-1/AC-2)", () => {
  it("empty array is the everything state, not all-checked", () => {
    const s = decode([], CATALOG);
    expect(s.everything).toBe(true);
    expect(s.checked.size).toBe(0);
    expect(s.chips).toEqual([]);
  });

  it("a group prefix checks every kind under it", () => {
    const s = decode(["task."], CATALOG);
    expect(s.everything).toBe(false);
    expect(ids(s)).toEqual(["task.claimed", "task.comment.created", "task.label.added"]);
  });

  it("a full kind id checks exactly that one", () => {
    expect(ids(decode(["task.claimed"], CATALOG))).toEqual(["task.claimed"]);
  });

  it("a prefix matching no catalogued kind becomes a chip, not a check (AC-4)", () => {
    const s = decode(["custom."], CATALOG);
    expect(s.checked.size).toBe(0);
    expect(s.chips).toEqual(["custom."]);
  });

  it("mixes a whole group, a single kind, and an unknown chip", () => {
    const s = decode(["node.", "task.claimed", "custom."], CATALOG);
    expect(ids(s)).toEqual(["node.connected", "node.disconnected", "task.claimed"]);
    expect(s.chips).toEqual(["custom."]);
  });
});

describe("encode: checkboxes → minimal kinds (MAIN-92 AC-3)", () => {
  it("everything encodes to the empty array", () => {
    expect(encode({ everything: true, checked: new Set(), chips: [] }, CATALOG)).toEqual([]);
  });

  it("a fully-ticked group collapses to its prefix", () => {
    const checked = new Set(["task.claimed", "task.comment.created", "task.label.added"]);
    expect(encode({ everything: false, checked, chips: [] }, CATALOG)).toEqual(["task."]);
  });

  it("a partially-ticked group emits individual ids", () => {
    const checked = new Set(["task.claimed", "task.label.added"]);
    expect(encode({ everything: false, checked, chips: [] }, CATALOG)).toEqual([
      "task.claimed",
      "task.label.added",
    ]);
  });

  it("all boxes checked is NOT everything — it stores every group prefix (AC-2)", () => {
    const checked = new Set(CATALOG.map((k) => k.id));
    const out = encode({ everything: false, checked, chips: [] }, CATALOG);
    expect(out).toEqual(["node.", "git.", "task."]);
    expect(out).not.toEqual([]); // visually/semantically distinct from "everything"
  });

  it("chips ride along verbatim", () => {
    const checked = new Set(["task.claimed"]);
    expect(encode({ everything: false, checked, chips: ["custom."] }, CATALOG)).toEqual([
      "task.claimed",
      "custom.",
    ]);
  });
});

describe("round-trip: encode∘decode is a fixed point (MAIN-92 AC-3)", () => {
  const cases: string[][] = [
    [],
    ["task."],
    ["task.claimed"],
    ["task.claimed", "task.label.added"],
    ["node.", "task.claimed"],
    ["custom."],
    ["node.", "task.claimed", "custom."],
    ["node.", "git.", "task."], // all groups
  ];
  for (const kinds of cases) {
    it(`round-trips ${JSON.stringify(kinds)}`, () => {
      expect(encode(decode(kinds, CATALOG), CATALOG)).toEqual(kinds);
    });
  }
});
