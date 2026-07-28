// MAIN-188 AC-6: the exported markdown format is the contract. These pin the
// exact bytes for all three scopes — body, title+body, all-with-comments —
// including zero-comments and multi-comment chronological ordering.
import { describe, expect, it } from "vitest";
import {
  formatAll,
  formatBody,
  formatTitleBody,
  type CardComment,
  type CardTask,
} from "./taskMarkdown";

const task: CardTask = {
  key: "MAIN-42",
  title: "Copy card as markdown",
  description: "Export a card as clean **markdown**.\n\n- one\n- two",
  type: "task",
  url: "https://nook.example/board?task=MAIN-42",
  labels: [{ name: "ux" }, { name: "board" }],
};
const meta = { priorityLabel: "high", columnName: "In Progress" };

describe("formatBody", () => {
  it("is the description verbatim", () => {
    expect(formatBody(task)).toBe("Export a card as clean **markdown**.\n\n- one\n- two");
  });
  it("is empty when there is no description", () => {
    expect(formatBody({ ...task, description: null })).toBe("");
  });
});

describe("formatTitleBody", () => {
  it("is `# KEY — Title`, a blank line, then the body", () => {
    expect(formatTitleBody(task)).toBe(
      "# MAIN-42 — Copy card as markdown\n\nExport a card as clean **markdown**.\n\n- one\n- two",
    );
  });
});

describe("formatAll", () => {
  it("title+body, metadata blockquote, then `## Comments (0)` when empty", () => {
    expect(formatAll(task, meta, [])).toBe(
      "# MAIN-42 — Copy card as markdown\n\n" +
        "Export a card as clean **markdown**.\n\n- one\n- two\n\n" +
        "> task · high · In Progress · ux, board · https://nook.example/board?task=MAIN-42\n\n" +
        "## Comments (0)",
    );
  });

  it("renders comments oldest-first with author + ISO timestamp, regardless of input order", () => {
    const comments: CardComment[] = [
      { author_name: "Bob", created_at: "2026-07-28T12:00:00Z", body_md: "second" },
      { author_name: "Alice", created_at: "2026-07-28T09:00:00Z", body_md: "first" },
    ];
    expect(formatAll(task, meta, comments)).toBe(
      "# MAIN-42 — Copy card as markdown\n\n" +
        "Export a card as clean **markdown**.\n\n- one\n- two\n\n" +
        "> task · high · In Progress · ux, board · https://nook.example/board?task=MAIN-42\n\n" +
        "## Comments (2)\n\n" +
        "**Alice** · 2026-07-28T09:00:00Z\n\nfirst\n\n" +
        "**Bob** · 2026-07-28T12:00:00Z\n\nsecond",
    );
  });

  it("falls back to sensible values for no labels / missing url / unknown author", () => {
    const bare: CardTask = { key: "MAIN-7", title: "Bare", description: "", type: "epic", url: null, labels: [] };
    const comments: CardComment[] = [
      { author_name: null, created_at: "2026-07-28T10:00:00Z", body_md: "hi" },
    ];
    expect(formatAll(bare, { priorityLabel: "none", columnName: "Triage" }, comments)).toBe(
      "# MAIN-7 — Bare\n\n\n\n" +
        "> epic · none · Triage · no labels · —\n\n" +
        "## Comments (1)\n\n" +
        "**unknown** · 2026-07-28T10:00:00Z\n\nhi",
    );
  });
});
