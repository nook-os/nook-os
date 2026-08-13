// `previewText` reduces a spec body to something a card can show. MAIN-571 adds
// a length cap to it, so what is asserted here is both halves: the stripping
// rules are unchanged (NG-4), and the cap cuts on a word boundary.
import { describe, expect, it } from "vitest";

import { previewText, priorityRank } from "./taskmeta";

describe("previewText stripping (unchanged by MAIN-571)", () => {
  it("drops frontmatter, headings, checkboxes, fences and emphasis", () => {
    const md = [
      "---",
      "key: MAIN-1",
      "---",
      "## Problem",
      "The **row** uses `flex` with no basis.",
      "```rust",
      "let x = 1;",
      "```",
      "- [ ] AC-1 — fix it",
    ].join("\n");
    expect(previewText(md)).toBe("Problem The row uses flex with no basis. let x = 1; AC-1 — fix it");
  });

  it("is empty for an absent description", () => {
    expect(previewText(null)).toBe("");
    expect(previewText(undefined)).toBe("");
    expect(previewText("")).toBe("");
  });
});

describe("previewText length cap (MAIN-571 AC-4)", () => {
  it("returns a short description untouched", () => {
    expect(previewText("A short one.", 120)).toBe("A short one.");
  });

  it("leaves a description of exactly the cap alone", () => {
    const exact = "x".repeat(40);
    expect(previewText(exact, 40)).toBe(exact);
  });

  it("cuts on a word boundary and appends an ellipsis", () => {
    const out = previewText("alpha beta gamma delta epsilon", 18);
    expect(out).toBe("alpha beta gamma…");
    // The words that survive are whole ones — no "gam…".
    expect(out.slice(0, -1).split(" ").at(-1)).toBe("gamma");
  });

  it("cuts mid-word when a single word is longer than the whole cap", () => {
    // A cap a lone long token could opt out of would not be a cap.
    expect(previewText("supercalifragilistic", 8)).toBe("supercal…");
  });

  it("keeps a capped preview at the cap however long the body is", () => {
    const long = `${"word ".repeat(4000)}`;
    const out = previewText(long, 120);
    expect(out.length).toBeLessThanOrEqual(121);
    expect(out.endsWith("…")).toBe(true);
  });

  it("caps the STRIPPED text, so markup does not eat the budget", () => {
    // The cap is on what a person reads, not on the markdown that produced it.
    expect(previewText("## **Heading** here", 100)).toBe("Heading here");
  });

  it("is uncapped when no maximum is given — the kanban card's contract (NG-2)", () => {
    const body = "word ".repeat(200).trim();
    expect(previewText(body)).toBe(body);
  });
});

describe("priorityRank", () => {
  it("sorts unset last, not first", () => {
    expect(priorityRank(0)).toBeGreaterThan(priorityRank(4));
    expect(priorityRank(null)).toBeGreaterThan(priorityRank(1));
  });
});
