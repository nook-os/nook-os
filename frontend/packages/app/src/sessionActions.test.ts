// MAIN-416: the two rules behind rename and stop.
import { describe, expect, it } from "vitest";
import { checkSessionName, renameIsANoop, stopPrompt } from "./sessionActions";

describe("checkSessionName", () => {
  it("accepts a name and hands back the trimmed form the server would store", () => {
    expect(checkSessionName("  api shell  ")).toEqual({ ok: true, name: "api shell" });
  });

  it("refuses an empty name BEFORE the request (AC-5)", () => {
    // The server trims then refuses empty; saying so here means the message
    // lands while the field is still on screen, instead of arriving as a 400
    // after the dialog has closed.
    for (const bad of ["", "   ", "\t\n"]) {
      const out = checkSessionName(bad);
      expect(out.ok).toBe(false);
      if (!out.ok) expect(out.reason).toMatch(/cannot be empty/);
    }
  });

  it("treats a cancelled dialog as its own outcome, not as an error", () => {
    // `askText` resolves null on cancel. Reporting "cannot be empty" at
    // somebody who pressed Escape would be a lie about what they did.
    const out = checkSessionName(null);
    expect(out.ok).toBe(false);
    if (!out.ok) expect(out.reason).toBe("cancelled");
  });
});

describe("renameIsANoop", () => {
  it("skips a rename that changes nothing, whitespace included", () => {
    expect(renameIsANoop("api", "api")).toBe(true);
    expect(renameIsANoop("api", "  api  ")).toBe(true);
    expect(renameIsANoop("api", "api 2")).toBe(false);
  });
});

describe("stopPrompt", () => {
  it("is destructive and confirmed", () => {
    const p = stopPrompt({ name: "alpha" });
    expect(p.danger).toBe(true);
    expect(p.title).toContain("alpha");
    expect(p.confirmLabel).toBeTruthy();
  });

  it("promises the session SURVIVES — it is not a kill", () => {
    // The words matter as much as the action: somebody who reads this and
    // expects to lose the session has been told the wrong thing.
    expect(stopPrompt({ name: "alpha" }).description).toMatch(/open it again/);
  });

  it("tells a managed session it will NOT be replaced", () => {
    // The question anyone who knows the reconciler will ask, answered before
    // they have to guess (MAIN-415 AC-3).
    expect(stopPrompt({ name: "alpha", managed: true }).description).toMatch(
      /NOT start a replacement/,
    );
  });
});
