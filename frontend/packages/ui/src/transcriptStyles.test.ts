// The two promises the transcript variant makes that only the STYLESHEET can
// keep (MAIN-499 NG-1, AC-8), asserted against the stylesheet itself.
//
// jsdom applies no CSS and evaluates no media query, so a rendered-DOM test can
// prove neither "chat's density is untouched" nor "nothing new animates" — the
// component emits the same nodes either way. The source is where those two live,
// so the source is what is read here.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// Not `new URL("./global.css", import.meta.url)`: Vite rewrites that literal
// into an asset URL, which `readFileSync` then refuses as a non-file scheme.
const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "global.css"), "utf8");

/** The variant's own rules: everything between its banner and the next one,
 *  with the prose stripped — a comment saying what the block does not do would
 *  otherwise read as it doing it. */
function transcriptBlock(): string {
  const marker = css.indexOf("── The transcript variant (MAIN-499)");
  expect(marker).toBeGreaterThan(-1);
  // From the banner's opening `/*`, so the banner itself strips as a comment.
  const from = css.lastIndexOf("/*", marker);
  const to = css.indexOf("/* ──", marker);
  expect(to).toBeGreaterThan(from);
  return css.slice(from, to).replace(/\/\*[\s\S]*?\*\//g, "");
}

describe("the transcript variant's stylesheet (MAIN-499)", () => {
  it("scopes every chat rule it adds to .transcript, so chat's density is untouched", () => {
    // NG-1: chat's `padding: 1px 8px` and `gap: 2px` were tuned deliberately.
    // A rule in this block reaching `.chat-log` or `.chat-msg` unqualified would
    // retune them for team chat as well.
    const selectors = transcriptBlock()
      .split("\n")
      .filter((l) => l.trim().endsWith("{"))
      .map((l) => l.slice(0, l.indexOf("{")).trim());
    expect(selectors.length).toBeGreaterThan(0);
    for (const sel of selectors) {
      if (/\.chat-(log|msg)\b/.test(sel)) expect(sel).toContain(".transcript");
    }
  });

  it("adds no motion of its own, and leaves the reduced-motion rule standing", () => {
    // AC-8: the only moving part on either variant is the typing indicator,
    // which the OS preference still switches off.
    expect(transcriptBlock()).not.toMatch(/animation|transition|@keyframes/);
    expect(css).toContain("@media (prefers-reduced-motion: reduce)");
    expect(css).toMatch(/\.chat-typing-dots i \{ animation: none; opacity: 0\.7; \}/);
  });
});
