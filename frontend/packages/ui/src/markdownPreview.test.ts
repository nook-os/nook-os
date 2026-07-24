import { afterEach, describe, expect, it } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { livePreview } from "./markdownPreview";
import { loadMarkdownMode } from "./Markdown";

// The C-split invariant (AC-2): live-preview is a *display* over the real
// characters. Turning it on, and editing through it, must never change a single
// byte of the stored document — that document is what agents parse.

const SAMPLE = [
  "## Acceptance Criteria",
  "",
  "Some **bold** and _italic_ and `code` and ~~strike~~.",
  "",
  "- [ ] **AC-1** first",
  "- [x] done",
  "",
  "> a quote",
  "",
  "```",
  "code fence",
  "```",
].join("\n");

function mountLive(doc: string) {
  const parent = document.createElement("div");
  document.body.appendChild(parent);
  const view = new EditorView({
    parent,
    state: EditorState.create({ doc, extensions: [livePreview()] }),
  });
  return {
    view,
    destroy: () => {
      view.destroy();
      parent.remove();
    },
  };
}

describe("live-preview keeps the document byte-identical", () => {
  it("does not rewrite the document when live-preview is on", () => {
    const { view, destroy } = mountLive(SAMPLE);
    expect(view.state.doc.toString()).toBe(SAMPLE);
    destroy();
  });

  it("appends exactly what was typed — no re-serialization", () => {
    const { view, destroy } = mountLive(SAMPLE);
    const insert = "\n- [ ] **AC-9** something";
    view.dispatch({ changes: { from: view.state.doc.length, insert } });
    expect(view.state.doc.toString()).toBe(SAMPLE + insert);
    destroy();
  });

  it("edits inside a styled span touch only those characters", () => {
    const { view, destroy } = mountLive("a **bold** b");
    // Replace "bold" with "BOLD" (positions 4..8) — the ** markers stay put.
    view.dispatch({ changes: { from: 4, to: 8, insert: "BOLD" } });
    expect(view.state.doc.toString()).toBe("a **BOLD** b");
    destroy();
  });
});

describe("markdown editor mode preference", () => {
  afterEach(() => localStorage.clear());

  it("defaults to live", () => {
    expect(loadMarkdownMode()).toBe("live");
  });

  it("persists a source preference across mounts", () => {
    localStorage.setItem("nook.md-editor-mode", "source");
    expect(loadMarkdownMode()).toBe("source");
  });

  it("treats an unknown stored value as live", () => {
    localStorage.setItem("nook.md-editor-mode", "wysiwyg");
    expect(loadMarkdownMode()).toBe("live");
  });
});
