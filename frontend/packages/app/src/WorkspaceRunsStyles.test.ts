// What the runs browser (MAIN-556) promises that only the STYLESHEET can keep.
//
// jsdom applies no CSS and runs no layout engine, so a rendered test can prove
// every row has the same cells but not that they are the same HEIGHT, not that
// the state column is reserved, and not that the toolbar sits outside the box
// that scrolls. Those live in the source, so the source is what is read here —
// the same method as `ui/src/sectionedPageStyles.test.ts`.
//
// It reads across packages on purpose: `RUNS_MIN_PANE_PX` and
// `--nook-runs-min-pane` are one number written twice, in the only two places
// that can act on it, and a test that read either alone would let them drift.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { RUNS_MIN_PANE_PX } from "./WorkspaceRuns";

const css = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "../../ui/src/global.css"),
  "utf8",
);

/** The runs block with its prose stripped: a comment saying what the block does
 *  NOT do would otherwise satisfy an assertion about what it does. */
function runsBlock(): string {
  const start = css.indexOf("/* A repo's review runs beside the one being read (MAIN-455).");
  const end = css.indexOf("/* The copy/export row above a run transcript (MAIN-471). */");
  expect(start).toBeGreaterThan(-1);
  expect(end).toBeGreaterThan(start);
  return css.slice(start, end).replace(/\/\*[\s\S]*?\*\//g, "");
}

/** One rule's declarations, by exact selector. */
function rule(selector: string): string {
  const block = runsBlock();
  const at = block.indexOf(`\n${selector} {`);
  expect(at, `no rule for ${selector}`).toBeGreaterThan(-1);
  return block.slice(at, block.indexOf("}", at));
}

describe("the runs browser's stylesheet (MAIN-556)", () => {
  it("floors the browser column at the width the code says it is designed for (AC-8)", () => {
    // One number, two files. If somebody widens the state column and moves the
    // CSS token without moving the constant, this is what goes red.
    expect(rule(".reviews-split")).toContain(`--nook-runs-min-pane: ${RUNS_MIN_PANE_PX}px;`);
    expect(rule(".reviews-split")).toContain(
      "grid-template-columns: minmax(var(--nook-runs-min-pane), 300px) minmax(0, 1fr);",
    );
  });

  it("gives every row two FIXED line tracks, which is what makes them one height (AC-1)", () => {
    // `repeat(2, <length>)`, not `auto`: an auto track grows to its content,
    // which is exactly how the old wrapping row ended up two heights.
    const row = rule(".runs-row");
    expect(row).toContain("grid-template-rows: repeat(2, var(--nook-runs-line-h));");
    expect(row).not.toMatch(/grid-template-rows:[^;]*auto/);
    expect(row).toContain("display: grid;");
    // A 20px pill inside an 18px track is how a "fixed" height starts drifting.
    expect(rule(".runs-row .pill")).toContain("height: var(--nook-runs-line-h);");
  });

  it("reserves the state column so narrowness can never reach the state (AC-2)", () => {
    const row = rule(".runs-row");
    const tracks = row.slice(row.indexOf("grid-template-columns:"));
    expect(tracks).toContain("var(--nook-runs-kind-col)");
    expect(tracks).toContain("minmax(0, 1fr)");
    expect(tracks).toContain("var(--nook-runs-state-col)");
    // The identifier is the ONLY flexible track: exactly one `fr` on the line.
    expect(tracks.slice(0, tracks.indexOf(";")).match(/fr\b/g)).toHaveLength(1);
    expect(rule(".runs-browser")).toMatch(/--nook-runs-state-col: \d+px;/);
  });

  it("truncates the secondary line and the identifier, and never the state (AC-3)", () => {
    for (const cell of [".runs-row-meta", ".runs-row-id"]) {
      expect(rule(cell)).toContain("text-overflow: ellipsis;");
      expect(rule(cell)).toContain("overflow: hidden;");
    }
    expect(rule(".runs-row-state")).not.toContain("text-overflow");
    expect(rule(".runs-row-state")).not.toContain("overflow: hidden");
    // Below the designed minimum the SECONDARY line is what goes — and only it.
    const collapse = runsBlock().slice(
      runsBlock().indexOf("@container runs-browser (max-width: 283px)"),
    );
    expect(collapse.slice(0, collapse.indexOf("}\n}") + 3)).toBe(
      "@container runs-browser (max-width: 283px) {\n  .runs-row-meta {\n    display: none;\n  }\n}",
    );
    // 283 is "below --nook-runs-min-pane": a container query cannot read a
    // custom property, so the two are checked against each other here.
    expect(RUNS_MIN_PANE_PX - 1).toBe(283);
    expect(rule(".runs-browser")).toContain("container-type: inline-size;");
  });

  it("puts the cells on shared columns rather than per-row flex (AC-4)", () => {
    // Explicit grid areas: a row cannot place its badge anywhere but column 1
    // however much or little it has to say.
    expect(rule(".runs-row-kind")).toContain("grid-area: 1 / 1;");
    expect(rule(".runs-row-id")).toContain("grid-area: 1 / 2;");
    expect(rule(".runs-row-state")).toContain("grid-area: 1 / 3;");
    expect(rule(".runs-row-meta")).toContain("grid-area: 2 / 1 / 3 / 3;");
    expect(rule(".runs-row-time")).toContain("grid-area: 2 / 3;");
  });

  it("reserves a track for the row's actions button, and hides it until asked (MAIN-559)", () => {
    // A TRACK, not an overlay: revealing the button on hover must not reflow
    // the row, which is the whole promise MAIN-556 made about this list.
    const tracks = rule(".runs-row");
    expect(tracks).toContain("var(--nook-runs-menu-col)");
    expect(rule(".runs-browser")).toMatch(/--nook-runs-menu-col: \d+px;/);
    expect(rule(".runs-row-menu")).toContain("grid-area: 1 / 4 / 3 / 5;");
    // `visibility`, not `opacity`: a transparent button still takes clicks, and
    // an invisible thing that can be clicked is a trap.
    expect(rule(".runs-row-menu")).toContain("visibility: hidden;");
    const block = runsBlock();
    for (const on of [
      ".runs-row:hover .runs-row-menu",
      ".runs-row:focus-within .runs-row-menu",
      ".runs-row.is-open .runs-row-menu",
    ]) {
      expect(block).toContain(on);
    }
  });

  it("marks the selected row with a background AND an edge (AC-5)", () => {
    const open = rule(".runs-row.is-open");
    expect(open).toContain("border-left-color: var(--nook-accent);");
    expect(open).toContain("background: var(--nook-bg-raised);");
    // The transparent edge is reserved on every row, so selecting one does not
    // shift its text sideways by 2px.
    expect(rule(".runs-row")).toContain("border-left: 2px solid transparent;");
  });

  it("scrolls the list and nothing else, so the toolbar cannot scroll away (AC-6)", () => {
    // Three regions as two grid tracks plus the transcript beside them. The
    // toolbar is `auto`, the list is the `1fr` that can shrink — and the list
    // is the only box in here with an overflow of its own.
    expect(rule(".runs-browser")).toContain("grid-template-rows: auto minmax(0, 1fr);");
    expect(rule(".runs-list")).toContain("overflow-y: auto;");
    expect(rule(".runs-toolbar")).not.toContain("overflow");
    expect(rule(".runs-browser")).not.toMatch(/overflow-y:\s*(auto|scroll)/);
  });

  it("stacks the two columns below the shared breakpoint rather than overflowing", () => {
    // The 260px floor is wider than half a phone. MAIN-187's scale, not a
    // fresh number.
    const block = runsBlock();
    expect(block).toContain("@media (max-width: 1024px)");
    expect(block.slice(block.indexOf("@media"))).toContain(
      "grid-template-columns: minmax(0, 1fr);",
    );
  });
});
