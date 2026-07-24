// Obsidian-style inline live-preview for the CodeMirror markdown editor.
//
// The whole point of splitting this from C1 (MAIN-16): the stored document is
// the artifact agents parse, so live-preview must be *display only*. Everything
// here is CodeMirror decorations over the real characters — it never dispatches
// a document change, never re-serializes, never hides a character from storage.
// Toggle it off and you are looking at the exact same bytes.
//
// The two ideas:
//  - STYLE the content (a heading looks like a heading, **bold** looks bold) by
//    laying mark decorations over the parsed ranges.
//  - HIDE the syntax markers (`**`, `#`, `> `, `- `) with replace decorations,
//    but REVEAL them the moment the caret enters their span — so you can always
//    edit the real markdown (AC-3). The markers are in the document the whole
//    time; we are only choosing whether to draw them.
import { syntaxTree } from "@codemirror/language";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { Strikethrough, TaskList } from "@lezer/markdown";
import type { Range } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";

// The markdown language (GFM strikethrough + task lists) that produces the
// syntax tree the decorations read. Tables/images/footnotes are deliberately
// not enabled for inline rendering (NG-1) — they stay as source and still
// render in the read-only Markdown view.
export const markdownLive = markdown({
  base: markdownLanguage,
  extensions: [Strikethrough, TaskList],
});

/** A non-interactive checkbox, shown in place of a `[ ]`/`[x]` task marker.
 *  Toggling happens by editing the source (AC-6), not by clicking here. */
class TaskBox extends WidgetType {
  constructor(readonly checked: boolean) {
    super();
  }
  eq(other: TaskBox) {
    return other.checked === this.checked;
  }
  toDOM() {
    const span = document.createElement("span");
    span.className = `cm-md-task${this.checked ? " done" : ""}`;
    span.textContent = this.checked ? "☑" : "☐";
    return span;
  }
}

/** A rendered bullet, shown in place of a `-`/`*`/`+` list marker. */
class Bullet extends WidgetType {
  eq() {
    return true;
  }
  toDOM() {
    const span = document.createElement("span");
    span.className = "cm-md-bullet";
    span.textContent = "•";
    return span;
  }
}

const hidden = Decoration.replace({});
const bullet = Decoration.replace({ widget: new Bullet() });
const mark = (cls: string) => Decoration.mark({ class: cls });
const lineMark = (cls: string) => Decoration.line({ class: cls });

const STRONG = mark("cm-md-strong");
const EMPH = mark("cm-md-emph");
const CODE = mark("cm-md-code");
const STRIKE = mark("cm-md-strike");
const LINK = mark("cm-md-link");

const HEADING: Record<string, ReturnType<typeof mark>> = {
  ATXHeading1: mark("cm-md-h1"),
  ATXHeading2: mark("cm-md-h2"),
  ATXHeading3: mark("cm-md-h3"),
  ATXHeading4: mark("cm-md-h4"),
  ATXHeading5: mark("cm-md-h5"),
  ATXHeading6: mark("cm-md-h6"),
};

const INLINE_WRAP: Record<string, ReturnType<typeof mark>> = {
  StrongEmphasis: STRONG,
  Emphasis: EMPH,
  InlineCode: CODE,
  Strikethrough: STRIKE,
  Link: LINK,
};

// Marker node names → how their reveal region is computed. "inline" reveals when
// the caret is anywhere in the parent inline node; "line" reveals when the caret
// is on the marker's line (headings, quotes, list/task markers).
const INLINE_MARKS = new Set(["EmphasisMark", "CodeMark", "StrikethroughMark", "LinkMark"]);
const LINE_MARKS = new Set(["HeaderMark", "QuoteMark"]);

function buildDecorations(view: EditorView): DecorationSet {
  const decos: Range<Decoration>[] = [];
  const { state } = view;
  const sel = state.selection;
  const touches = (from: number, to: number) =>
    sel.ranges.some((r) => r.from <= to && r.to >= from);
  const lineTouched = (pos: number) => {
    const line = state.doc.lineAt(pos);
    return touches(line.from, line.to);
  };

  for (const { from, to } of view.visibleRanges) {
    syntaxTree(state).iterate({
      from,
      to,
      enter: (node) => {
        const name = node.name;

        // Style the content spans (kept whether or not the marks are revealed —
        // a heading stays big while you edit its `#`).
        const wrap = INLINE_WRAP[name] ?? HEADING[name];
        if (wrap) {
          if (node.to > node.from) decos.push(wrap.range(node.from, node.to));
          return;
        }

        // Fenced code: a mono block on every line it spans.
        if (name === "FencedCode") {
          let pos = node.from;
          while (pos <= node.to) {
            const line = state.doc.lineAt(pos);
            decos.push(lineMark("cm-md-codeblock").range(line.from));
            if (line.to + 1 > node.to) break;
            pos = line.to + 1;
          }
          return;
        }

        // Bullet list marker → •, unless the caret is on the line.
        if (name === "ListMark") {
          const text = state.doc.sliceString(node.from, node.to);
          if (/^[-*+]$/.test(text) && !lineTouched(node.from)) {
            decos.push(bullet.range(node.from, node.to));
          }
          return;
        }

        // Task marker `[ ]`/`[x]` → a checkbox, unless the caret is on the line.
        if (name === "TaskMarker") {
          if (!lineTouched(node.from)) {
            const checked = /[xX]/.test(state.doc.sliceString(node.from, node.to));
            decos.push(
              Decoration.replace({ widget: new TaskBox(checked) }).range(node.from, node.to),
            );
          }
          return;
        }

        // Syntax markers: hide them unless revealed (AC-3).
        if (INLINE_MARKS.has(name)) {
          const parent = node.node.parent;
          const reveal = parent ? touches(parent.from, parent.to) : lineTouched(node.from);
          if (!reveal && node.to > node.from) decos.push(hidden.range(node.from, node.to));
          return;
        }
        if (LINE_MARKS.has(name)) {
          // Header/quote marks include the trailing space; hide it all so the
          // text sits where the rendered element would.
          if (!lineTouched(node.from) && node.to > node.from) {
            decos.push(hidden.range(node.from, node.to));
          }
          return;
        }
      },
    });
  }

  // Decoration.set requires ascending order; sort by from, then by the marker's
  // startSide (replace before mark at the same position).
  decos.sort((a, b) => a.from - b.from || a.value.startSide - b.value.startSide);
  return Decoration.set(decos);
}

/** The live-preview decoration layer. Rebuilds on document, selection, and
 *  viewport changes — selection matters because revealing markers at the caret
 *  is a function of where the caret is. */
export const livePreviewDecorations = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }
    update(u: ViewUpdate) {
      if (u.docChanged || u.selectionSet || u.viewportChanged) {
        this.decorations = buildDecorations(u.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

/** The full live-preview extension: the markdown language for the tree, plus
 *  the decoration layer. Source mode simply omits this. */
export function livePreview() {
  return [markdownLive, livePreviewDecorations];
}
