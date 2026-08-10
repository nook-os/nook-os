// What the chat channel drawer (MAIN-498) promises that only the STYLESHEET can
// keep. jsdom applies no CSS and evaluates no media query, so `Chat.test.tsx`
// can prove which controls exist and what they do, but not that the drawer is
// 340px wide, not that it stops short of the shell's bottom bar, and not that
// the rail above the breakpoint is untouched. The source is where those live,
// so the source is what is read here — same method as `sectionedPageStyles`.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// Not `new URL("./global.css", import.meta.url)`: Vite rewrites that literal
// into an asset URL, which `readFileSync` then refuses as a non-file scheme.
const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "global.css"), "utf8");

/** The compact block's rules, prose stripped — a comment saying what the block
 *  does not do would otherwise read as it doing it. Ends at the media query's
 *  MATCHING BRACE, so a sub-block added inside it cannot silently shorten what
 *  the absence assertions below are reading. */
function compactBlock(): string {
  const marker = css.indexOf("── compact: the channel list becomes a drawer (MAIN-498)");
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

/** The whole sheet with every `@media` block cut out, outer brace to matching
 *  brace — i.e. exactly what applies at EVERY width.
 *
 *  This is what the AC-7 guard has to read. Slicing "everything before the
 *  compact block" was the obvious thing and the wrong one: the drawer's
 *  keyframe is emitted AFTER that block, so the slice never saw it, and a
 *  `.chat-drawer-*` rule appended below would have walked straight past the
 *  guard that exists to catch it. Comments go first, so prose naming a
 *  selector cannot read as a rule declaring one. */
function everyWidthOf(source: string): string {
  const bare = source.replace(/\/\*[\s\S]*?\*\//g, "");
  let out = "";
  let i = 0;
  for (;;) {
    const at = bare.indexOf("@media", i);
    if (at === -1) return out + bare.slice(i);
    out += bare.slice(i, at);
    let depth = 0;
    let j = bare.indexOf("{", at);
    for (; j < bare.length; j++) {
      if (bare[j] === "{") depth++;
      else if (bare[j] === "}" && --depth === 0) {
        j++;
        break;
      }
    }
    i = j;
  }
}

const everyWidth = () => everyWidthOf(css);

describe("the chat channel drawer's stylesheet (MAIN-498)", () => {
  it("takes the list out of the grid and gives the room the full width (AC-1)", () => {
    const block = compactBlock();
    expect(block).toContain("@media (max-width: 640px)");
    expect(block).toMatch(/\.chat-page \{[^}]*grid-template-columns: minmax\(0, 1fr\);/);
    expect(block).toMatch(/\.chat-page > \.chat-channels \{[^}]*position: absolute;/);
  });

  it("is MAIN-418's drawer width, not a second one (AC-4)", () => {
    expect(compactBlock()).toMatch(
      /\.chat-page > \.chat-channels \{[^}]*width: min\(86vw, 340px\);/,
    );
    // The session drawer it is matching, so the two cannot drift apart silently.
    expect(css).toMatch(/\.session-nav \{[^}]*width: min\(86vw, 340px\) !important;/);
  });

  it("stops at the page's edges rather than covering the shell (AC-5)", () => {
    // `fixed` — what `.git-drawer` uses — spans the viewport and would sit over
    // the bottom bar the compact shell puts under `.nook-main`. `absolute`
    // inside a positioned `.chat-page` cannot: `.nook-main` clips it.
    const block = compactBlock();
    expect(block).toMatch(/\.chat-page \{[^}]*position: relative;/);
    expect(block).not.toMatch(/position: fixed/);
    expect(block).toMatch(/\.chat-drawer-scrim \{[^}]*position: absolute;/);
    expect(css).toMatch(/\.nook-main \{[^}]*overflow: hidden;/);
  });

  it("scrolls the drawer and the log, never the document (AC-6)", () => {
    // A `1fr` track floors at its content, which is how a message list gives
    // the DOCUMENT a horizontal scrollbar; the `minmax(0, …)` above is the fix
    // and these two are the belt to its braces.
    const block = compactBlock();
    expect(block).toMatch(/\.chat-page \{[^}]*max-width: 100vw;/);
    expect(block).toMatch(/\.chat-page \{[^}]*overflow-x: hidden;/);
    expect(css).toMatch(/\.chat-channels \{[^}]*overflow-y: auto;/);
  });

  it("leaves the 200px rail exactly as it was above the breakpoint (AC-7)", () => {
    // The rail's own rule applies at every width and still says 200px…
    const always = everyWidth();
    expect(always).toMatch(/\.chat-page \{[^}]*grid-template-columns: 200px minmax\(0, 1fr\);/);
    // …and nothing that makes the list a drawer — position, z-index, the
    // width — is stated outside a media query, wherever in the sheet it is
    // written.
    expect(always).not.toContain(".chat-drawer-scrim");
    expect(always).not.toContain(".chat-drawer-toggle");
    expect(always).not.toMatch(/\.chat-page > \.chat-channels/);
  });

  it("reads the whole sheet for AC-7, not just the part above the block", () => {
    // The guard above is only worth having if it reads the TAIL too. The
    // keyframe below the compact block is the standing proof that rules do get
    // written there, and a `.chat-drawer-*` rule appended after it is exactly
    // what would otherwise walk past the guard.
    const always = everyWidth();
    expect(always.indexOf("@keyframes chat-drawer-in")).toBeGreaterThan(
      always.indexOf("grid-template-columns: 200px"),
    );
    expect(always).not.toContain(".chat-drawer-toggle");
    expect(everyWidthOf(css + "\n.chat-drawer-toggle { display: block; }")).toContain(
      ".chat-drawer-toggle",
    );
  });

  it("slides in from the edge it docks to", () => {
    // `git-drawer-in` starts at `translateX(100%)` — the right edge. A left
    // drawer replaying it would slide OUT of view, which is why this is a
    // second keyframe and not a reuse of that one (NG-4).
    expect(compactBlock()).toMatch(/animation: chat-drawer-in 0\.16s ease-out;/);
    expect(css).toMatch(/@keyframes chat-drawer-in \{\s*from \{\s*transform: translateX\(-100%\);/);
    // The keyframe itself is outside every media query, as `git-drawer-in` is —
    // inert until something plays it, and the only thing that plays it is
    // inside the compact block.
    expect(everyWidth()).not.toMatch(/animation:[^;]*chat-drawer-in/);
  });
});
