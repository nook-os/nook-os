// MAIN-414: the navigator's rules, which a screenshot cannot check.
import { describe, expect, it } from "vitest";
import {
  COMPACT_WIDTH,
  DEFAULT_NAV_PREFS,
  DEFAULT_PANE_WIDTH,
  MAX_PANE_WIDTH,
  MIN_CONTENT_WIDTH,
  MIN_PANE_WIDTH,
  filterFolders,
  navFolders,
  paneMode,
  parseNavPrefs,
} from "./sessionNav";
import { ADHOC_GROUP, groupTabs, hueOf } from "./tabGroups";
import type { SessionTab } from "./sessionTabsStore";

const tab = (
  id: string,
  extra: Partial<SessionTab> = {},
): SessionTab => ({
  id,
  name: id,
  runtime: "bash",
  ...extra,
});

describe("navFolders", () => {
  it("gives the SAME keys, labels and hues the tab strip does", () => {
    // The reason this is a test and not a comment: the pane and the strip
    // showing one repo as two differently-named, differently-coloured things
    // is the failure mode a second grouping implementation produces, and it
    // looks like a rendering bug rather than a duplication bug.
    const tabs = [
      tab("a", { workspaceId: "w1", workspaceName: "api" }),
      tab("b", { workspaceId: "w2", workspaceName: "web" }),
      tab("c", { workspaceId: "w1", workspaceName: "api" }),
      tab("t"),
    ];
    const folders = navFolders(tabs);
    const groups = groupTabs(tabs, []);

    expect(folders.map((f) => f.key)).toEqual(groups.map((g) => g.key));
    expect(folders.map((f) => f.label)).toEqual(groups.map((g) => g.label));
    expect(folders.map((f) => f.hue)).toEqual(groups.map((g) => g.hue));
    expect(folders.map((f) => f.sessions.map((s) => s.id))).toEqual(
      groups.map((g) => g.tabs.map((t) => t.id)),
    );
  });

  it("keeps workspace-less terminals in the strip's own ad-hoc folder", () => {
    const folders = navFolders([tab("t")]);
    expect(folders[0].key).toBe(ADHOC_GROUP);
    expect(folders[0].label).toBe("Terminals");
    expect(folders[0].hue).toBe(hueOf(ADHOC_GROUP));
  });
});

describe("filterFolders", () => {
  const tabs = [
    tab("api-claude", { workspaceId: "w1", workspaceName: "api", runtime: "claude" }),
    tab("api-shell", { workspaceId: "w1", workspaceName: "api", runtime: "bash" }),
    tab("web-claude", { workspaceId: "w2", workspaceName: "web", runtime: "claude" }),
    tab("scratch", { nodeName: "azul" }),
  ];
  const folders = navFolders(tabs);

  it("returns everything for an empty term", () => {
    expect(filterFolders(folders, "   ")).toBe(folders);
  });

  it("matches the session name and drops the folders left empty", () => {
    const out = filterFolders(folders, "scratch");
    expect(out.map((f) => f.label)).toEqual(["Terminals"]);
    expect(out[0].sessions.map((s) => s.id)).toEqual(["scratch"]);
  });

  it("matches the runtime across folders, preserving the structure", () => {
    const out = filterFolders(folders, "claude");
    // Both folders survive, in their original order, each holding only the
    // session that matched — you never lose your place in the tree.
    expect(out.map((f) => f.label)).toEqual(["api", "web"]);
    expect(out.map((f) => f.sessions.map((s) => s.id))).toEqual([
      ["api-claude"],
      ["web-claude"],
    ]);
  });

  it("matches the workspace, which keeps that folder whole", () => {
    const out = filterFolders(folders, "api");
    expect(out).toHaveLength(1);
    expect(out[0].sessions.map((s) => s.id)).toEqual(["api-claude", "api-shell"]);
  });

  it("requires EVERY word, so two terms narrow rather than widen", () => {
    const out = filterFolders(folders, "api claude");
    expect(out.map((f) => f.sessions.map((s) => s.id))).toEqual([["api-claude"]]);
  });

  it("hides every folder when nothing matches", () => {
    expect(filterFolders(folders, "nothing-like-this")).toEqual([]);
  });
});

describe("paneMode", () => {
  const narrow = MIN_CONTENT_WIDTH + 100; // pane of 260 leaves 480 — too tight
  const wide = MIN_CONTENT_WIDTH + 600;

  it("pushes when there is room for both", () => {
    expect(
      paneMode({ pinned: false, viewportWidth: wide, paneWidth: DEFAULT_PANE_WIDTH }),
    ).toBe("push");
  });

  it("overlays when pushing would squeeze the terminal", () => {
    expect(
      paneMode({ pinned: false, viewportWidth: narrow, paneWidth: DEFAULT_PANE_WIDTH }),
    ).toBe("overlay");
  });

  it("PINNED pushes at the very width where unpinned overlays", () => {
    // The pin's whole meaning. A width threshold that overrode it would take
    // back a decision only the person looking at the screen can make.
    const at = { viewportWidth: narrow, paneWidth: DEFAULT_PANE_WIDTH };
    expect(paneMode({ ...at, pinned: false })).toBe("overlay");
    expect(paneMode({ ...at, pinned: true })).toBe("push");
  });

  it("pinned pushes however narrow it gets — until a phone", () => {
    // Above compact the pin still wins at any width (MAIN-414 AC-5).
    expect(
      paneMode({ pinned: true, viewportWidth: COMPACT_WIDTH + 1, paneWidth: MAX_PANE_WIDTH }),
    ).toBe("push");
  });

  it("is ALWAYS a drawer on a phone, pinned or not (MAIN-418 AC-1)", () => {
    // At 375px a pushed 260px pane leaves ~115px of terminal: not a smaller
    // desktop layout, a broken one. The pin keeps its meaning everywhere it
    // can be honoured, and this is the one width where it cannot.
    for (const w of [320, 375, 414, COMPACT_WIDTH]) {
      expect(paneMode({ pinned: true, viewportWidth: w, paneWidth: 260 })).toBe("overlay");
      expect(paneMode({ pinned: false, viewportWidth: w, paneWidth: 260 })).toBe("overlay");
    }
  });
});

describe("parseNavPrefs", () => {
  it("round-trips what the pane stores", () => {
    // What actually crosses the wire: the object is JSON in a settings row and
    // comes back parsed, so this is the real round trip.
    const prefs = { width: 340, collapsed: true, pinned: true };
    expect(parseNavPrefs(JSON.parse(JSON.stringify(prefs)))).toEqual(prefs);
  });

  it("defaults a missing or unusable value rather than blanking the pane", () => {
    expect(parseNavPrefs(undefined)).toEqual(DEFAULT_NAV_PREFS);
    expect(parseNavPrefs(null)).toEqual(DEFAULT_NAV_PREFS);
    expect(parseNavPrefs("nonsense")).toEqual(DEFAULT_NAV_PREFS);
  });

  it("falls back FIELD BY FIELD, so one bad value does not cost the others", () => {
    expect(parseNavPrefs({ width: "wide", collapsed: true, pinned: true })).toEqual({
      width: DEFAULT_PANE_WIDTH,
      collapsed: true,
      pinned: true,
    });
  });

  it("clamps a width that would make the pane unusable", () => {
    expect(parseNavPrefs({ width: 4 }).width).toBe(MIN_PANE_WIDTH);
    expect(parseNavPrefs({ width: 9000 }).width).toBe(MAX_PANE_WIDTH);
  });
});
