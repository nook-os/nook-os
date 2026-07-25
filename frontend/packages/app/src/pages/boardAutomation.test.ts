import { describe, expect, it } from "vitest";

import {
  addAction,
  cleanAutomation,
  defaultAction,
  removeAction,
  updateAction,
  type Automation,
} from "./BoardAutomation";

describe("board automation editor state", () => {
  it("adds an action with a blank config for its kind", () => {
    const a = addAction({}, "review", "notify");
    expect(a.review).toEqual([{ kind: "notify", title: "", body: "" }]);
    const b = addAction(a, "review", "add_board_label");
    expect(b.review[1]).toEqual({ kind: "add_board_label", label: "" });
  });

  it("patches a field, and resets shape when the kind changes", () => {
    let a: Automation = addAction({}, "completed", "add_board_label");
    a = updateAction(a, "completed", 0, { label: "agent-ready" });
    expect(a.completed[0]).toEqual({ kind: "add_board_label", label: "agent-ready" });
    // Switching kind drops the now-irrelevant `label` and installs notify fields.
    a = updateAction(a, "completed", 0, { kind: "notify" });
    expect(a.completed[0]).toEqual(defaultAction("notify"));
  });

  it("removes the action at an index", () => {
    let a: Automation = addAction({}, "started", "notify");
    a = addAction(a, "started", "add_board_label");
    a = removeAction(a, "started", 0);
    expect(a.started).toHaveLength(1);
    expect(a.started[0].kind).toBe("add_board_label");
  });

  it("cleans for storage: drops empty column lists and trims fields", () => {
    let a: Automation = addAction({}, "review", "notify");
    a = updateAction(a, "review", 0, { title: "  {key} in review  ", body: "  " });
    a = addAction(a, "completed", "remove_board_label"); // empty column stays until filled
    a = updateAction(a, "completed", 0, { label: "  agent-ready " });
    a = addAction(a, "started", "notify"); // will be emptied below
    a = removeAction(a, "started", 0);

    const out = cleanAutomation(a);
    expect(out.review).toEqual([{ kind: "notify", title: "{key} in review" }]);
    expect(out.completed).toEqual([{ kind: "remove_board_label", label: "agent-ready" }]);
    // A column type with no actions is omitted entirely.
    expect(out.started).toBeUndefined();
  });

  it("keeps a blank label so the server can reject it visibly", () => {
    let a: Automation = addAction({}, "completed", "add_board_label");
    a = updateAction(a, "completed", 0, { label: "   " });
    const out = cleanAutomation(a);
    expect(out.completed).toEqual([{ kind: "add_board_label", label: "" }]);
  });
});
