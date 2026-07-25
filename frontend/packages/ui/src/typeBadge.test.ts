// The one issue-type → presentation mapping (MAIN-60 AC-2), asserted directly.
// If a type's tone or label drifts here, the detail selector, the board card,
// and the filter all drift with it — which is exactly why the mapping is one
// table and this test guards it.
import { describe, expect, it } from "vitest";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { TYPE_META, TypeBadge, typeMeta } from "./components";

describe("TYPE_META / typeMeta (MAIN-60 AC-2)", () => {
  it("maps each of the five types to its tone and label", () => {
    const byValue = Object.fromEntries(TYPE_META.map((t) => [t.value, t]));
    expect(Object.keys(byValue).sort()).toEqual(["bug", "chore", "epic", "story", "task"]);
    expect(byValue.bug.tone).toBe("err");
    expect(byValue.epic.tone).toBe("accent");
    expect(byValue.story.tone).toBe("info");
    expect(byValue.task.tone).toBe("dim");
    expect(byValue.chore.tone).toBe("dim");
  });

  it("defaults an absent or unknown type to `task` (the server default)", () => {
    expect(typeMeta(null).value).toBe("task");
    expect(typeMeta(undefined).value).toBe("task");
    expect(typeMeta("nonsense").value).toBe("task");
    expect(typeMeta("bug").value).toBe("bug");
  });
});

describe("TypeBadge", () => {
  it("renders the tone class and, unless compact, the label", () => {
    const full = renderToStaticMarkup(
      React.createElement(TypeBadge, { type: "bug" }),
    );
    expect(full).toContain("type-badge err");
    expect(full).toContain("Bug");

    const compact = renderToStaticMarkup(
      React.createElement(TypeBadge, { type: "bug", compact: true }),
    );
    expect(compact).toContain("type-badge err");
    // Compact is icon-only — the label text is not rendered.
    expect(compact).not.toContain("type-badge-label");
  });
});
