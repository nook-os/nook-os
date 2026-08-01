// The finder IS the feature — "where do I change the chime" gets typed, not
// scanned — so its matching rules get pinned independently of any page.
import { describe, expect, it } from "vitest";
import { matchSections, type PageSection } from "./SectionedPage";

const s = (id: string, title: string, group: string, keywords: string[] = [], badge?: string): PageSection => ({
  id,
  title,
  group,
  keywords,
  badge,
  render: () => null,
});

const SECTIONS = [
  s("appearance", "Appearance", "You", ["theme", "colors"]),
  s("notifications", "Notifications", "You", ["chime", "desktop", "sound"]),
  s("automation", "Automation", "Team", ["loops", "reconcile"], "team"),
  s("skills", "Taught skills", "Team", ["teach", "SKILL.md"], "fleet"),
];

describe("matchSections", () => {
  it("returns everything for an empty term", () => {
    expect(matchSections(SECTIONS, "")).toHaveLength(4);
    expect(matchSections(SECTIONS, "   ")).toHaveLength(4);
  });

  it("matches keywords, which is the whole point — the words people type", () => {
    // "chime" appears in no title. It is what somebody looking for the sound
    // toggle actually types, and it must land them there.
    expect(matchSections(SECTIONS, "chime").map((x) => x.id)).toEqual(["notifications"]);
    expect(matchSections(SECTIONS, "reconcile").map((x) => x.id)).toEqual(["automation"]);
  });

  it("matches titles and groups too, case-insensitively", () => {
    expect(matchSections(SECTIONS, "APPEAR").map((x) => x.id)).toEqual(["appearance"]);
    expect(matchSections(SECTIONS, "you").map((x) => x.id)).toEqual([
      "appearance",
      "notifications",
    ]);
  });

  it("matches the blast-radius badge, so 'team' finds team-wide settings", () => {
    expect(matchSections(SECTIONS, "fleet").map((x) => x.id)).toEqual(["skills"]);
  });

  it("every word must match somewhere — more words narrow, never widen", () => {
    // "team loops" is one section; a widening OR would return both Team rows.
    expect(matchSections(SECTIONS, "team loops").map((x) => x.id)).toEqual(["automation"]);
    expect(matchSections(SECTIONS, "team zebra")).toHaveLength(0);
  });

  it("preserves registry order, which is what keeps groups stable", () => {
    expect(matchSections(SECTIONS, "t").map((x) => x.id)).toEqual(
      SECTIONS.filter((x) =>
        [x.title, x.group, x.badge ?? "", ...(x.keywords ?? [])].join(" ").toLowerCase().includes("t"),
      ).map((x) => x.id),
    );
  });
});
