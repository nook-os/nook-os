// What the compact section strip (MAIN-497) promises that only the STYLESHEET
// can keep. jsdom applies no CSS and evaluates no media query, so a rendered
// test can prove the strip's structure but not that it is a row, not that the
// rail above 1024px is untouched, and not that the page cannot scroll sideways.
// The source is where those live, so the source is what is read here — same
// method as `transcriptStyles.test.ts`.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// Not `new URL("./global.css", import.meta.url)`: Vite rewrites that literal
// into an asset URL, which `readFileSync` then refuses as a non-file scheme.
const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "global.css"), "utf8");

/** The compact block's rules, prose stripped — a comment saying what the block
 *  does not do would otherwise read as it doing it.
 *
 *  The end is the media query's MATCHING BRACE, not the next banner comment.
 *  Slicing to the next `/* ──` fails open: add one sub-banner inside the block
 *  and the slice quietly ends early, so the absence assertions below would keep
 *  passing while reading less and less of what they claim to guard. */
function compactBlock(): string {
  const marker = css.indexOf("── medium: the rail rotates into a strip (MAIN-497)");
  expect(marker).toBeGreaterThan(-1);
  const open = css.indexOf("{", css.indexOf("@media", marker));
  let depth = 0;
  let end = -1;
  for (let i = open; i < css.length; i++) {
    if (css[i] === "{") depth++;
    else if (css[i] === "}" && --depth === 0) {
      end = i + 1;
      break;
    }
  }
  expect(end).toBeGreaterThan(open);
  return css.slice(css.lastIndexOf("/*", marker), end).replace(/\/\*[\s\S]*?\*\//g, "");
}

describe("the sectioned page's stylesheet (MAIN-497)", () => {
  it("rotates the nav into a scrolling row at the shared medium breakpoint", () => {
    // AC-1. The breakpoint is MAIN-187's named scale, not a fresh number.
    const block = compactBlock();
    expect(block).toContain("@media (max-width: 1024px)");
    expect(block).toMatch(/\.spage-list \{[^}]*flex-direction: row;/);
    expect(block).toMatch(/\.spage-list \{[^}]*overflow-x: auto;/);
  });

  it("leaves the 220px rail exactly as it was above the breakpoint", () => {
    // AC-8: the desktop rule is outside every media query, and `.spage-list` is
    // `contents` there — no box, so the rail lays out as it did before the
    // element existed.
    expect(css).toMatch(/\.spage \{[^}]*grid-template-columns: 220px minmax\(0, 1fr\);/);
    expect(css).toContain(".spage-list { display: contents; }");
    expect(css).toMatch(/\.spage-nav \{[^}]*flex-direction: column;[^}]*overflow-y: auto;/);
  });

  it("scrolls the strip and never the document", () => {
    // AC-6: a `1fr` track floors at its content, and a row of section names is
    // wider than a 375px phone — which is precisely how the page ends up with a
    // horizontal scrollbar. The wrap clips, the track may shrink, the strip is
    // the only thing that scrolls sideways.
    expect(css).toMatch(/\.spage-wrap \{[^}]*overflow: hidden;/);
    const block = compactBlock();
    expect(block).toMatch(/\.spage \{[^}]*grid-template-columns: minmax\(0, 1fr\);/);
    expect(block).toMatch(/\.spage-nav \{[^}]*min-width: 0;/);
    expect(block).toMatch(/overscroll-behavior-x: contain;/);
  });

  it("gives the strip's items the 44px touch floor the session navigator uses", () => {
    // AC-5: one minimum across the app's touch surfaces, not two that disagree.
    expect(compactBlock()).toMatch(/\.spage-item \{[^}]*min-height: 44px;/);
  });

  it("keeps the finder's PHONE rules on the phone breakpoint", () => {
    // AC-7. The item floor is AC-5's and belongs to the strip, so it holds
    // wherever the strip does. A 16px input is a defence against iOS zoom and a
    // 44px one is sized for a thumb — neither is a reason at 1024px, where the
    // pointer is a mouse and both would just break the console's density. This
    // is the same split MAIN-418 made, and it is asserted so the two cannot be
    // merged back together by accident.
    const nested = compactBlock().match(/@media \(max-width: 640px\) \{[\s\S]*?\n {2}\}/);
    expect(nested).not.toBeNull();
    expect(nested![0]).toMatch(/\.spage-find \{[^}]*min-height: 44px;[^}]*font-size: 16px;/);
    // …and nowhere else in the block, which is the half that would regress.
    const outsideNest = compactBlock().replace(nested![0], "");
    expect(outsideNest).not.toMatch(/font-size: 16px/);
    expect(outsideNest).not.toMatch(/\.spage-find/);
  });

  it("reflows and does not restyle — no drawer, no overlay, no colours of its own", () => {
    // AC-7 and NG-4. The rail's items keep their theme; what changes is where
    // the box sits and which way it runs.
    const block = compactBlock();
    expect(block).not.toMatch(/position: (fixed|absolute)/);
    expect(block).not.toMatch(/z-index|box-shadow|--nook-accent|background:/);
  });
});
