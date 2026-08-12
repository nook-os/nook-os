// The contract AC-2 makes, asserted against the source itself: NO command is
// implemented, transformed or special-cased in frontend code.
//
// The composer posts a name and some text and renders what comes back, so the
// browser holds no command set of its own and cannot drift from the backend's.
// That is a property of the whole tree rather than of any one function, which
// is why it is read here rather than exercised — a "quick client-side command"
// added later fails this file before it can ship.
//
// Fixtures and tests are exempt: naming a command is exactly what a test of the
// palette must do.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const FRONTEND = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

/** Every hand-written source file in the UI layer — where a client-side command
 *  would have to live. `generated/` is the Rust-owned wire schema: it describes
 *  the server's shapes, including the command DTOs, and describing them is the
 *  opposite of implementing them. */
function sources(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      if (entry !== "node_modules" && entry !== "generated") sources(full, out);
    } else if (/\.tsx?$/.test(entry) && !/\.test\.tsx?$/.test(entry)) {
      out.push(full);
    }
  }
  return out;
}

const FILES = sources(join(FRONTEND, "packages", "ui", "src")).concat(
  sources(join(FRONTEND, "packages", "app", "src")),
);

/** Which files a pattern appears in, sorted — `readdirSync` walks in the
 *  filesystem's order, not the alphabet's, so an unsorted list would compare
 *  against an order no runner guarantees. */
function hits(pattern: RegExp): string[] {
  return FILES.filter((f) => pattern.test(readFileSync(f, "utf8")))
    .map((f) => relative(FRONTEND, f))
    .sort();
}

describe("the command set is the server's (MAIN-529 AC-2)", () => {
  it("scans a UI layer that is actually there", () => {
    // A walk that silently found nothing would pass every assertion below.
    expect(FILES.length).toBeGreaterThan(50);
  });

  it("implements no command's BEHAVIOUR", () => {
    // Each of these is something only an implementation would need: the word
    // one command is named after, the glyph another appends, and the sentence
    // the third answers with. All three live in `nook-chat`, and a client that
    // grew its own copy would show up here.
    expect(hits(/\bshrug\b/i)).toEqual([]);
    expect(hits(/¯\\_\(ツ\)_\/¯/)).toEqual([]);
    expect(hits(/commands you can use here/i)).toEqual([]);
  });

  it("branches on no command NAME", () => {
    // A typed command as a string literal — `startsWith("/me")`, a lookup keyed
    // by "/help" — is the shape a special case takes. The two allowed hits are
    // the docs ROUTE, which is a page in this app and not a chat command.
    expect(hits(/["']\/(help|me|shrug)["']/)).toEqual(
      ["packages/app/src/documentTitle.ts", "packages/app/src/layout.tsx"].sort(),
    );
  });
});
