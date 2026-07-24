import { describe, expect, it } from "vitest";
import { applyPrefix, applySurround } from "./Markdown";

// The editing transforms are what MAIN-16 must keep byte-identical to the old
// textarea. They are pure, so we assert directly on the resulting document
// string — the same values the toolbar buttons and ⌘B/⌘I/⌘E shortcuts feed into
// a CodeMirror transaction.

describe("applySurround", () => {
  it("wraps a selection and keeps the inner text selected", () => {
    // "hello [world]" → select "world" at [6,11)
    const r = applySurround("hello world", 6, 11, "**");
    expect(r.doc).toBe("hello **world**");
    // The selection now covers "world" (between the markers).
    expect(r.doc.slice(r.from, r.to)).toBe("world");
  });

  it("inserts the markers at an empty caret with the caret between them", () => {
    const r = applySurround("ab", 1, 1, "**");
    expect(r.doc).toBe("a****b");
    expect(r.from).toBe(3);
    expect(r.to).toBe(3); // collapsed, sitting between the two `**`
  });

  it("supports distinct open/close markers", () => {
    const r = applySurround("x", 0, 1, "`", "`");
    expect(r.doc).toBe("`x`");
  });
});

describe("applyPrefix", () => {
  it("prefixes every line the selection touches", () => {
    const doc = "one\ntwo\nthree";
    // Selection spans from inside line 1 to inside line 3.
    const r = applyPrefix(doc, 1, 10, "- ");
    expect(r.doc).toBe("- one\n- two\n- three");
    // The whole affected block is selected afterwards.
    expect(r.doc.slice(r.from, r.to)).toBe("- one\n- two\n- three");
  });

  it("toggles the prefix off when a line already has it", () => {
    const doc = "- one\n- two";
    const r = applyPrefix(doc, 0, doc.length, "- ");
    expect(r.doc).toBe("one\ntwo");
  });

  it("applies a task-list prefix across a single caret line", () => {
    const r = applyPrefix("buy milk", 3, 3, "- [ ] ");
    expect(r.doc).toBe("- [ ] buy milk");
  });

  it("prefixes a quote on the last line with no trailing newline", () => {
    const r = applyPrefix("hi", 2, 2, "> ");
    expect(r.doc).toBe("> hi");
  });
});
