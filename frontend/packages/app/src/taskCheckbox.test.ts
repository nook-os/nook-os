import { describe, expect, it } from "vitest";
import { countTaskCheckboxes, toggleTaskCheckbox } from "./taskCheckbox";

const BODY = [
  "## Acceptance Criteria",
  "",
  "- [ ] AC-1 — first thing",
  "- [x] AC-2 — already done",
  "- [ ] AC-3 — first thing", // deliberately same text as AC-1 (AC-6)
  "",
  "Some prose with a literal [ ] that is NOT a checkbox.",
].join("\n");

describe("toggleTaskCheckbox", () => {
  it("counts only real task-list checkboxes, not `[ ]` in prose", () => {
    expect(countTaskCheckboxes(BODY)).toBe(3);
  });

  it("checks the Nth unchecked box, leaving the others untouched", () => {
    const out = toggleTaskCheckbox(BODY, 0);
    expect(out).toContain("- [x] AC-1 — first thing");
    expect(out).toContain("- [x] AC-2 — already done"); // unchanged
    expect(out).toContain("- [ ] AC-3 — first thing"); // unchanged
    expect(out).toContain("literal [ ] that is NOT"); // prose untouched
  });

  it("unchecks a checked box", () => {
    const out = toggleTaskCheckbox(BODY, 1);
    expect(out).toContain("- [ ] AC-2 — already done");
    expect(out).toContain("- [ ] AC-1 — first thing"); // unchanged
  });

  it("flips the RIGHT line even when two have identical text (AC-6)", () => {
    // AC-1 and AC-3 share text; toggling index 2 must hit AC-3, not AC-1.
    const out = toggleTaskCheckbox(BODY, 2);
    const lines = out.split("\n");
    expect(lines[2]).toBe("- [ ] AC-1 — first thing"); // AC-1 unchanged
    expect(lines[4]).toBe("- [x] AC-3 — first thing"); // AC-3 flipped
  });

  it("supports *, + bullets and leading indentation", () => {
    const src = "  * [ ] indented star\n+ [x] plus";
    expect(toggleTaskCheckbox(src, 0)).toBe("  * [x] indented star\n+ [x] plus");
    expect(toggleTaskCheckbox(src, 1)).toBe("  * [ ] indented star\n+ [ ] plus");
  });

  it("is a no-op for an out-of-range index", () => {
    expect(toggleTaskCheckbox(BODY, 9)).toBe(BODY);
  });
});
