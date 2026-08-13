// What a backlog row promises that only the STYLESHEET can keep (MAIN-571).
//
// jsdom applies no CSS and runs no layout engine, so `BoardBacklog.test.tsx`
// can prove every row emits the same cells but not that those cells are the
// same WIDTH — and equal width is the whole point: a track that sizes to its
// content is a track whose neighbours move when the content changes. So the
// source is what is read here, the same method as `WorkspaceRunsStyles.test.ts`.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const css = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "../../ui/src/global.css"),
  "utf8",
);

/** The backlog block with its prose stripped: a comment saying what the block
 *  does NOT do would otherwise satisfy an assertion about what it does. */
function backlogBlock(): string {
  const start = css.indexOf("/* Backlog tab: a dense prioritized list, not columns (MAIN-82). */");
  const end = css.indexOf("/* Bulk-selection shell (MAIN-123)");
  expect(start).toBeGreaterThan(-1);
  expect(end).toBeGreaterThan(start);
  return css.slice(start, end).replace(/\/\*[\s\S]*?\*\//g, "");
}

/** One rule's declarations, by exact selector. */
function rule(selector: string): string {
  const block = backlogBlock();
  const at = block.indexOf(`\n${selector} {`);
  expect(at, `no rule for ${selector}`).toBeGreaterThan(-1);
  return block.slice(at, block.indexOf("}", at));
}

describe("the backlog row's stylesheet (MAIN-571)", () => {
  it("declares a width for every reserved cell (AC-1)", () => {
    // The defect was that NO element in the row declared one.
    const declared = backlogBlock();
    for (const v of [
      "--nook-backlog-prio-w",
      "--nook-backlog-type-w",
      "--nook-backlog-key-w",
      "--nook-backlog-status-w",
      "--nook-backlog-chevron-w",
    ]) {
      expect(declared, `${v} is never defined`).toMatch(new RegExp(`${v}:\\s*\\d+px;`));
    }
  });

  it("fixes each optional cell's basis so an empty one still occupies it (AC-2)", () => {
    // `0 0 <width>`: no grow and NO SHRINK. A cell allowed to shrink is a cell
    // whose row realigns as soon as a busy neighbour needs the space.
    expect(rule(".backlog-row-prio")).toContain("flex: 0 0 var(--nook-backlog-prio-w);");
    expect(rule(".backlog-row-type")).toContain("flex: 0 0 var(--nook-backlog-type-w);");
    expect(rule(".backlog-row-status")).toContain("flex: 0 0 var(--nook-backlog-status-w);");
    expect(rule(".backlog-row-key")).toContain("flex: 0 0 var(--nook-backlog-key-w);");
  });

  it("zeroes the key's inherited right margin, which sits outside the basis", () => {
    expect(rule(".backlog-row-key")).toContain("margin-right: 0;");
  });

  it("gives the preview a zero basis, so a long one cannot push the title (AC-5)", () => {
    const preview = rule(".backlog-row-preview");
    expect(preview).toContain("flex: 1 1 0;");
    // Subordinate: smaller than the row's 12px, and `faint` in the markup.
    expect(preview).toMatch(/font-size:\s*11\.5px;/);
    // One line, clipped — never a paragraph that wraps the row to two.
    expect(preview).toContain("white-space: nowrap;");
    expect(preview).toContain("text-overflow: ellipsis;");
  });

  it("lets the meta cell give way before the title does (AC-5/6)", () => {
    const meta = rule(".backlog-row-meta");
    const title = rule(".backlog-row-title");
    expect(meta).toContain("flex: 0 4 auto;");
    expect(title).toContain("flex: 0 1 auto;");
    // min-width:0 on both is what makes shrinking end in an ellipsis rather
    // than in the row overflowing at a narrow viewport (AC-6).
    expect(title).toContain("min-width: 0;");
    expect(meta).toContain("min-width: 0;");
  });

  it("indents an epic child and changes nothing else about it (AC-3)", () => {
    // One track definition, two sections: the ONLY rule that separates an epic
    // child from a top-level row is this padding.
    const indent = rule(".backlog-epic-body .backlog-row");
    expect(indent).toContain("padding-left: 22px;");
    expect(indent.replace("padding-left: 22px;", "")).not.toMatch(/[a-z-]+:/);
  });

  it("sizes the chevron and its spacer from one number (AC-3)", () => {
    expect(rule(".backlog-epic-collapse")).toContain("width: var(--nook-backlog-chevron-w);");
    expect(rule(".backlog-epic-chevron-spacer")).toContain(
      "flex: 0 0 var(--nook-backlog-chevron-w);",
    );
  });
});
