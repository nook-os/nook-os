// The one visibility → presentation mapping (MAIN-103), asserted directly.
// If a visibility's tone or label drifts here, the detail selector, the board
// card, the context menu and the filter all drift with it — which is exactly
// why the mapping is one table and this test guards it. Mirrors the type badge
// test.
import { describe, expect, it } from "vitest";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { VISIBILITY_META, VisibilityBadge, visibilityMeta } from "./components";

describe("VISIBILITY_META / visibilityMeta (MAIN-103)", () => {
  it("gives each of the three visibilities meta with a tone, label, tooltip and icon", () => {
    const byValue = Object.fromEntries(VISIBILITY_META.map((v) => [v.value, v]));
    expect(Object.keys(byValue).sort()).toEqual(["org", "private", "team"]);
    for (const v of VISIBILITY_META) {
      expect(v.label.length).toBeGreaterThan(0);
      expect(v.tooltip.length).toBeGreaterThan(0);
      expect(v.Icon).toBeTruthy();
    }
    expect(byValue.private.tone).toBe("warn");
    expect(byValue.team.tone).toBe("dim");
    expect(byValue.org.tone).toBe("info");
  });

  it("defaults an absent or unknown visibility to `team` (the server default)", () => {
    expect(visibilityMeta(null).value).toBe("team");
    expect(visibilityMeta(undefined).value).toBe("team");
    expect(visibilityMeta("nonsense").value).toBe("team");
    expect(visibilityMeta("private").value).toBe("private");
  });
});

describe("VisibilityBadge", () => {
  it("renders the tone class and, unless compact, the label", () => {
    const full = renderToStaticMarkup(
      React.createElement(VisibilityBadge, { visibility: "private" }),
    );
    expect(full).toContain("type-badge warn");
    expect(full).toContain("Private");

    const compact = renderToStaticMarkup(
      React.createElement(VisibilityBadge, { visibility: "private", compact: true }),
    );
    expect(compact).toContain("type-badge warn");
    // Compact is icon-only — the label text is not rendered.
    expect(compact).not.toContain("type-badge-label");
  });
});
