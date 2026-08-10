// The finder IS the feature — "where do I change the chime" gets typed, not
// scanned — so its matching rules get pinned independently of any page.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { SectionedPage, matchSections, type PageSection } from "./SectionedPage";

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

// The compact strip (MAIN-497). jsdom evaluates no media query, so what a
// rendered test can prove is the STRUCTURE the strip needs — one box holding
// every item, with the finder outside it — plus the behaviour no stylesheet can
// carry: scrolling the active item into view. The rules that only exist in CSS
// are pinned next to the stylesheet, in `sectionedPageStyles.test.ts`.
describe("SectionedPage's nav", () => {
  const scrolled: Element[] = [];
  beforeEach(() => {
    scrolled.length = 0;
    // jsdom has no layout and so no `scrollIntoView`; this both supplies it and
    // records which element asked.
    Element.prototype.scrollIntoView = vi.fn(function (this: Element) {
      scrolled.push(this);
    });
  });
  afterEach(cleanup);

  const show = (section?: string) =>
    render(
      <MemoryRouter initialEntries={[section ? `/settings?section=${section}` : "/settings"]}>
        <SectionedPage sections={SECTIONS} />
      </MemoryRouter>,
    );

  it("puts every item in ONE box, with the finder above it — the strip at compact, the rail above", () => {
    // AC-1/AC-4: `.spage-list` is the element the compact rule turns into a
    // scrolling row, and the finder is its sibling rather than its first item,
    // so it stays full-width above the strip instead of scrolling off the side.
    const { container } = show();
    const nav = container.querySelector(".spage-nav")!;
    const kids = [...nav.children].map((c) => c.className);
    expect(kids).toEqual(["input spage-find", "spage-list"]);

    const list = nav.querySelector(".spage-list")!;
    expect(list.querySelectorAll(".spage-item")).toHaveLength(SECTIONS.length);
    expect(container.querySelectorAll(".spage-item").length).toBe(
      list.querySelectorAll(".spage-item").length,
    );
    // AC-7: the groups come with it. A strip that silently drops its headings
    // is a second, lesser navigation model, not the same one reflowed.
    expect([...list.querySelectorAll(".spage-group")].map((g) => g.textContent)).toEqual([
      "You",
      "Team",
    ]);
  });

  it("keeps each badge inline on its own item", () => {
    // AC-3: the badge is how a section says it needs attention, and small is
    // exactly where scanning is hardest.
    const { container } = show();
    const badged = [...container.querySelectorAll(".spage-item")].filter((i) =>
      i.querySelector(".spage-badge"),
    );
    expect(
      badged.map((i) => [
        i.querySelector(".spage-item-title")!.textContent,
        i.querySelector(".spage-badge")!.textContent,
      ]),
    ).toEqual([
      ["Automation", "team"],
      ["Taught skills", "fleet"],
    ]);
  });

  it("scrolls the active item into view on mount, even when it is last in the list", () => {
    // AC-2: the strip scrolls, so a section late in the list would otherwise sit
    // off-screen with nothing saying it exists.
    const { container } = show("skills");
    expect(scrolled).toHaveLength(1);
    expect(scrolled[0]).toBe(
      [...container.querySelectorAll(".spage-item")].find((i) => i.classList.contains("active")),
    );
    expect(scrolled[0].textContent).toContain("Taught skills");
  });

  it("scrolls again whenever the active item changes, however it changed", () => {
    // A click is one way; narrowing the finder until the pick no longer matches
    // is the other, and it moves the active item without anyone touching it.
    show();
    expect(scrolled.map((e) => e.textContent)).toEqual(["Appearance"]);

    fireEvent.click(screen.getByText("Automation"));
    expect(scrolled.map((e) => e.textContent)).toEqual(["Appearance", "Automationteam"]);

    fireEvent.change(screen.getByLabelText("find a section"), { target: { value: "chime" } });
    expect(scrolled.map((e) => e.textContent)).toEqual([
      "Appearance",
      "Automationteam",
      "Notifications",
    ]);
  });

  it("filters the strip through matchSections and nothing else", () => {
    // AC-4: the finder above the strip is the same finder, unchanged.
    const { container } = show();
    fireEvent.change(screen.getByLabelText("find a section"), { target: { value: "team" } });
    const titles = [...container.querySelectorAll(".spage-item-title")].map((t) => t.textContent);
    expect(titles).toEqual(matchSections(SECTIONS, "team").map((x) => x.title));
    expect(titles).toEqual(["Automation", "Taught skills"]);
  });
});
